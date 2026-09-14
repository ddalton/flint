#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016  # `check && ok || bad` (ok never fails); perl/awk bodies in single quotes
# podwatch_localtest.sh — podwatch.sh start|status|flush|stop with a fake kubectl
# (pretty-printed watch streams, a List snapshot, a stream that ends and must
# be restarted) and a fake aws. Checks: one compact line per event and per pod,
# the tenant-pod annotation kept and last-applied-configuration dropped, the
# restart of an ended watch, flush of evidence/cp AND history with SHA256SUMS
# that verify, no upload outside _rig/evidence/cp/ and _rig/history/.
#
#   bash podwatch_localtest.sh [--keep]
set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/podwatch-localtest.XXXXXX")
KEEP=0
[ "${1:-}" = --keep ] && KEEP=1
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); printf '  PASS  %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n' "$1"; [ -n "${2:-}" ] && printf '        %s\n' "$2"; }
check() { local n=$1; shift; if "$@" >/dev/null 2>&1; then ok "$n"; else bad "$n" "$*"; fi; }
wait_for() { local n=$1 i=0; shift; while [ $i -lt "$n" ]; do "$@" >/dev/null 2>&1 && return 0; sleep 0.1; i=$((i + 1)); done; "$@" >/dev/null 2>&1; }
cleanup() {
    for f in "$T"/state/pids/*.pid; do [ -f "$f" ] && { kill -KILL "-$(cat "$f")" 2>/dev/null; kill -KILL "$(cat "$f")" 2>/dev/null; }; done
    if [ $KEEP = 1 ]; then echo "kept $T"; else rm -rf "$T"; fi
}
trap cleanup EXIT

mkdir -p "$T/shim" "$T/bucket" "$T/evid" "$T/state" "$T/history/a1/current"
if ! command -v setsid >/dev/null 2>&1; then
    printf '#!/usr/bin/perl\nuse POSIX ();\nPOSIX::setsid() or die "setsid: $!";\nexec { $ARGV[0] } @ARGV or die;\n' >"$T/shim/setsid"
fi
cat >"$T/shim/aws" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$FAKE_BUCKET_ROOT/../aws-calls.log"
map() { local u=${1#s3://}; printf '%s/%s' "$FAKE_BUCKET_ROOT" "$u"; }
case "$2" in
    sync) src=$3; dst=$(map "$4"); mkdir -p "$dst"; (cd "$src" && find . -type f ! -name SHA256SUMS | while read -r f; do mkdir -p "$dst/$(dirname "$f")"; cp "$f" "$dst/$f"; done) ;;
    cp) dst=$(map "$4"); mkdir -p "$(dirname "$dst")"; cp "$3" "$dst" ;;
    ls) d=$(map "$3"); echo "Total Objects: $(find "$d" -type f | wc -l | tr -d ' ')"; echo "Total Size: $(find "$d" -type f -exec cat {} + | wc -c | tr -d ' ')" ;;
esac
EOF
cat >"$T/shim/kubectl" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$FAKE_BUCKET_ROOT/../kubectl-calls.log"
pod() { # name ns uid tenant
cat <<JSON
{
  "apiVersion": "v1", "kind": "Pod",
  "metadata": {"name": "$1", "namespace": "$2", "uid": "$3",
    "annotations": {"chert.us/tenant-pod": "$4", "kubectl.kubernetes.io/last-applied-configuration": "{\"huge\": true}"},
    "labels": {"app": "x"}, "ownerReferences": [{"kind": "Pod", "name": "owner"}]},
  "spec": {"nodeName": "node-1"},
  "status": {"phase": "Running", "containerStatuses": [
    {"name": "worker", "restartCount": 2, "ready": true, "state": {"running": {}},
     "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}}]}
}
JSON
}
case "$*" in
    version*) exit 0 ;;
    "get events -A -w -o json")
        n=$(cat "$FAKE_BUCKET_ROOT/../events-runs" 2>/dev/null || echo 0); echo $((n + 1)) >"$FAKE_BUCKET_ROOT/../events-runs"
        printf '{\n "kind": "Event",\n "metadata": {"name": "e%s", "managedFields": [{"x": 1}]},\n "reason": "Killing", "message": "Stopping container worker"\n}\n' "$n"
        printf '{"kind": "Event", "metadata": {"name": "f%s"}, "reason": "OOMKilling"}\n' "$n"
        [ "$n" -ge 1 ] && exec sleep 3600
        exit 0 ;;  # the first watch ends: the loop must restart it
    "get pods -A -w -o json") pod s3w-aaaa flint-workers uid-w1 wl-a1/agents-x; exec sleep 3600 ;;
    "get pods -A -o json") printf '{"kind": "List", "items": [\n'; pod s3w-bbbb flint-workers uid-w2 wl-a1/agents-y; printf ',\n'; pod agents-y wl-a1 uid-a2 ""; printf ']}\n' ;;
esac
EOF
chmod +x "$T/shim/"*
echo '{"seq": 1}' >"$T/history/a1/current/1-e.json"
echo '{"ts_ms":1,"name":"current"}' >"$T/history/a1/index.jsonl"

export PATH="$T/shim:$PATH" FAKE_BUCKET_ROOT="$T/bucket" BUCKET=flint-lean-writers-test
export EVID="$T/evid" STATE="$T/state" HISTORY="$T/history" PODS_SECS=0.5 UPLOAD_SECS=3600 ALLOW_ROOTFS=1
export AWS_BIN="$T/shim/aws" KUBECTL_BIN="$T/shim/kubectl" KUBECONFIG="$T/kc"

echo "podwatch.sh"
bash "$HERE/podwatch.sh" start >"$T/start.out" 2>&1
check "start exits 0" test $? -eq 0
check "status: every loop alive" bash "$HERE/podwatch.sh" status
check "an ended events watch is restarted (4 events from 2 runs)" wait_for 80 sh -c "[ \$(wc -l <'$EVID/events.jsonl') -ge 4 ]"
python3 - "$EVID/events.jsonl" <<'PY' && ok "events: one compact line each, managedFields stripped, observed_ms added" || bad "events lines"
import json, sys
ls = [json.loads(l) for l in open(sys.argv[1])]
assert {l["metadata"]["name"] for l in ls} >= {"e0", "f0", "e1", "f1"}, ls
assert all("managedFields" not in l["metadata"] and l["observed_ms"] > 0 for l in ls)
PY
check "pods.jsonl snapshot lines" wait_for 50 test -s "$EVID/pods.jsonl"
check "pods-watch.jsonl lines" wait_for 50 test -s "$EVID/pods-watch.jsonl"
python3 - "$EVID/pods.jsonl" "$EVID/pods-watch.jsonl" <<'PY' && ok "pods: a List expanded, src snap/watch, uid, node, restarts, OOMKilled lastState, tenant-pod kept, last-applied dropped" || bad "pod lines"
import json, sys
snap = [json.loads(l) for l in open(sys.argv[1])]
watch = [json.loads(l) for l in open(sys.argv[2])]
w2 = next(l for l in snap if l["uid"] == "uid-w2")
assert w2["src"] == "snap" and w2["ns"] == "flint-workers" and w2["node"] == "node-1" and w2["phase"] == "Running"
assert w2["restarts"] == 2 and w2["lastState"]["worker"]["terminated"]["reason"] == "OOMKilled"
assert w2["annotations"] == {"chert.us/tenant-pod": "wl-a1/agents-y"}, w2["annotations"]
assert any(l["uid"] == "uid-a2" for l in snap)
w1 = watch[0]
assert w1["src"] == "watch" and w1["uid"] == "uid-w1" and w1["annotations"]["chert.us/tenant-pod"] == "wl-a1/agents-x"
PY
bash "$HERE/podwatch.sh" flush >"$T/flush.out" 2>&1
check "flush exits 0" test $? -eq 0
check "flush printed counts for both prefixes" sh -c "grep -q '_rig/evidence/cp/ objects=' '$T/flush.out' && grep -q '_rig/history/ objects=' '$T/flush.out'"
check "evidence/cp SHA256SUMS verifies" sh -c "cd '$T/bucket/$BUCKET/_rig/evidence/cp' && shasum -a 256 -c SHA256SUMS"
check "history SHA256SUMS verifies and carries <leg>/ paths" sh -c "cd '$T/bucket/$BUCKET/_rig/history' && shasum -a 256 -c SHA256SUMS && grep -q ' a1/index.jsonl' SHA256SUMS"
python3 - "$T/aws-calls.log" "s3://$BUCKET/_rig/evidence/cp/" "s3://$BUCKET/_rig/history/" <<'PY' && ok "every aws call targets _rig/evidence/cp/ or _rig/history/" || bad "an aws call elsewhere" "$(cat "$T/aws-calls.log")"
import sys
allowed = sys.argv[2:]
for l in open(sys.argv[1]):
    a = l.split()
    target = a[3] if a[1] in ("sync", "cp") else a[2]
    assert any(target.startswith(p) for p in allowed), l
PY
pgids=$(cat "$STATE"/pids/*.pid | tr '\n' ' ')
bash "$HERE/podwatch.sh" stop >"$T/stop.out" 2>&1
check "stop exits 0" test $? -eq 0
if bash "$HERE/podwatch.sh" status >"$T/status.out" 2>&1; then bad "status after stop exits 1"; else ok "status after stop exits 1"; fi
check "no loop alive after stop" sh -c "! grep -q ': alive (pid' '$T/status.out'"
left=$(ps -A -o pgid=,pid=,command= | awk -v g=" $pgids " 'index(g, " " $1 " ")')
if [ -z "$left" ]; then ok "no process left in any loop's process group (kubectl watches, sleeps)"; else bad "processes left in loop groups" "$left"; fi
echo
echo "podwatch localtest: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
