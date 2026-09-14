#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016  # `check && ok || bad` (ok never fails); perl/awk bodies in single quotes
# shipper_localtest.sh — shipper.sh start|status|flush|stop on this machine,
# with no AWS and no cluster: a fake /var/log/pods tree, a fake cgroup tree,
# fake aws / chronyc / dmesg / journalctl, and GNU tail + stdbuf (gtail/gstdbuf
# on macOS). Checks the log copy byte for byte through append, rotation with
# writes to the renamed file, pod deletion, a kill -9'd tail resumed, a tail
# dead when its file vanished (LOSS recorded), rotated .gz handling, the
# namespace filter, the catch-up at stop, a restart that resumes without
# duplicating, the cgroup / chrony / dmesg / journal lines, the flush's
# SHA256SUMS against what was "uploaded", every upload under
# _rig/evidence/<node>/, a failed listing that must not print a count, and the
# /mnt/nvme device guard. What it cannot check here: /proc (Linux-only
# catch-up precision), real kubelet rotation timing, real SSM.
#
#   bash shipper_localtest.sh [--keep]
set -u

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SHIPPER="$HERE/shipper.sh"
T=$(mktemp -d "${TMPDIR:-/tmp}/shipper-localtest.XXXXXX")
KEEP=0
[ "${1:-}" = --keep ] && KEEP=1
PASS=0
FAIL=0

ok() { PASS=$((PASS + 1)); printf '  PASS  %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n' "$1"; [ -n "${2:-}" ] && printf '        %s\n' "$2"; }
check() { # name cmd...
    local name=$1
    shift
    if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name" "$*"; fi
}
wait_for() { # tenths cmd...
    local n=$1 i=0
    shift
    while [ $i -lt "$n" ]; do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
        i=$((i + 1))
    done
    "$@" >/dev/null 2>&1
}

cleanup() {
    if [ -f "$T/state/pids/logs.pid" ] || ls "$T/state/pids/"*.pid >/dev/null 2>&1; then
        for f in "$T"/state/pids/*.pid; do [ -f "$f" ] && kill -KILL "-$(cat "$f")" 2>/dev/null; kill -KILL "$(cat "$f")" 2>/dev/null; done
    fi
    python3 - "$T/state/tails" <<'PY' 2>/dev/null
import json, os, signal, sys
d = sys.argv[1]
for f in os.listdir(d) if os.path.isdir(d) else []:
    try:
        pid = json.load(open(os.path.join(d, f))).get("pid")
        if pid:
            os.kill(pid, signal.SIGKILL)
    except Exception:
        pass
PY
    if [ $KEEP = 1 ]; then echo "kept $T"; else rm -rf "$T"; fi
}
trap cleanup EXIT

# ------------------------------------------------------------- the shims --

mkdir -p "$T/shim" "$T/bucket" "$T/pods" "$T/cg" "$T/evid" "$T/state"
tail_bin=$(command -v gtail || true)
[ -z "$tail_bin" ] && tail --version 2>/dev/null | grep -q GNU && tail_bin=$(command -v tail)
[ -n "$tail_bin" ] || { echo "SKIP: needs GNU tail (gtail)"; exit 2; }
stdbuf_bin=$(command -v gstdbuf || command -v stdbuf || true)
ln -s "$tail_bin" "$T/shim/tail"
if ! command -v setsid >/dev/null 2>&1; then
    cat >"$T/shim/setsid" <<'EOF'
#!/usr/bin/perl
use POSIX ();
POSIX::setsid() or die "setsid: $!";
exec { $ARGV[0] } @ARGV or die "exec $ARGV[0]: $!";
EOF
    chmod +x "$T/shim/setsid"
fi

cat >"$T/shim/aws" <<'EOF'
#!/usr/bin/env bash
# fake aws: s3 sync | s3 cp | s3 ls --recursive --summarize, over $FAKE_BUCKET_ROOT
echo "$*" >>"$FAKE_BUCKET_ROOT/../aws-calls.log"
map() { local u=${1#s3://}; printf '%s/%s' "$FAKE_BUCKET_ROOT" "$u"; }
[ "$1" = s3 ] || exit 64
case "$2" in
    sync)
        src=$3; dst=$(map "$4"); mkdir -p "$dst"
        excl=""
        shift 4
        while [ $# -gt 0 ]; do [ "$1" = --exclude ] && excl=$2; shift; done
        (cd "$src" && find . -type f ! -name "${excl:-__none__}" | while read -r f; do
            mkdir -p "$dst/$(dirname "$f")"
            cmp -s "$f" "$dst/$f" || cp "$f" "$dst/$f"
        done) ;;
    cp) dst=$(map "$4"); mkdir -p "$(dirname "$dst")"; cp "$3" "$dst" ;;
    ls)
        [ -e "$FAKE_BUCKET_ROOT/../aws_fail_ls" ] && { echo "An error occurred (AccessDenied)" >&2; exit 254; }
        d=$(map "$3")
        n=$(find "$d" -type f 2>/dev/null | wc -l | tr -d ' ')
        b=$(find "$d" -type f -exec cat {} + 2>/dev/null | wc -c | tr -d ' ')
        echo ""; echo "Total Objects: $n"; echo "   Total Size: $b" ;;
    *) exit 64 ;;
esac
EOF
cat >"$T/shim/chronyc" <<'EOF'
#!/bin/sh
# System time +0.000250 s: chrony's text form would say "0.000250000 seconds slow of NTP time"
echo "A9FEA97B,169.254.169.123,3,1789300000.123456789,0.000250000,-0.000012000,0.000020000,-12.345,-0.001,0.012,0.000300000,0.000200000,64.1,Normal"
EOF
cat >"$T/shim/dmesg" <<'EOF'
#!/bin/sh
echo "2026-09-13T10:00:00,000000+00:00 Out of memory: Killed process 4242 (flint-sync) args=$*"
exec sleep 3600
EOF
cat >"$T/shim/journalctl" <<'EOF'
#!/bin/sh
echo "2026-09-13T10:00:00.000000+0000 node kubelet[1]: fake journal line args=$*"
exec sleep 3600
EOF
chmod +x "$T/shim/"*

export PATH="$T/shim:$PATH"
export FAKE_BUCKET_ROOT="$T/bucket"
export BUCKET=flint-lean-writers-test NODE=ip-10-0-0-1.us-west-1.compute.internal
export EVID="$T/evid" STATE="$T/state" PODS_ROOT="$T/pods" CGROUP_ROOT="$T/cg"
export DISCOVER_SECS=0.3 CGROUP_SECS=0.5 CHRONY_SECS=0.5 UPLOAD_SECS=3600 CATCHUP_SECS=20
export AWS_BIN="$T/shim/aws" CHRONYC_BIN="$T/shim/chronyc" DMESG_BIN="$T/shim/dmesg" JOURNALCTL_BIN="$T/shim/journalctl"
export TAIL_BIN=tail STDBUF_BIN="${stdbuf_bin:-stdbuf}"
export ALLOW_ROOTFS=1

line() { printf '2026-09-13T10:00:%02d.%09dZ stderr F {"ts_ms":%s,"ev":"x","n":%s}\n' $(($2 % 60)) "$2" "$2" "$2" >>"$1"; }
lines() { local f=$1 from=$2 to=$3 i; i=$from; while [ "$i" -le "$to" ]; do line "$f" "$i"; i=$((i + 1)); done; }
reg_field() { # out-path-suffix field
    python3 - "$STATE/tails" "$1" "$2" <<'PY'
import json, os, sys
d, suffix, field = sys.argv[1:]
for f in sorted(os.listdir(d)):
    e = json.load(open(os.path.join(d, f)))
    if (e.get("out") or "").endswith(suffix):
        v = e.get(field)
        print("" if v is None else v)
        break
PY
}

echo "shipper.sh guards"
out=$(ALLOW_ROOTFS=0 EVID=/nonexistent-flint-evid STATE=/nonexistent-flint-state bash "$SHIPPER" start 2>&1)
rc=$?
if [ $rc -ne 0 ] && printf '%s' "$out" | grep -q "root filesystem"; then ok "EVID on the root filesystem's device is refused"; else bad "EVID on the root filesystem's device is refused" "rc=$rc $out"; fi
out=$(NODE="a/b" bash "$SHIPPER" start 2>&1)
rc=$?
if [ $rc -ne 0 ] && printf '%s' "$out" | grep -q NODE; then ok "a NODE with a slash (an S3 key escape) is refused"; else bad "a NODE with a slash is refused" "rc=$rc $out"; fi
check "nothing was started by a refused start" test ! -e "$STATE/pids/logs.pid"

# ------------------------------------------------------------ the fixture --

W="$PODS_ROOT/flint-workers_s3w-aaaa_uid-w1/worker"
A="$PODS_ROOT/wl-a1_agents-x_uid-a1/agent"
C="$PODS_ROOT/flint-system_csi-node-q_uid-c1/plugin"
D="$PODS_ROOT/default_unrelated_uid-d1/app"
mkdir -p "$W" "$A" "$C" "$D"
lines "$W/0.log" 1 3
lines "$A/0.log" 1 2
lines "$D/0.log" 1 2
POD="kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod12345678_1234_1234_1234_123456789abc.slice"
CT="$POD/cri-containerd-$(printf 'ab%.0s' $(seq 1 32)).scope"
for d in "$POD" "$CT"; do
    mkdir -p "$CGROUP_ROOT/$d"
    echo 104857600 >"$CGROUP_ROOT/$d/memory.current"
    echo 209715200 >"$CGROUP_ROOT/$d/memory.peak"
    echo 2147483648 >"$CGROUP_ROOT/$d/memory.max"
    printf 'usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n' >"$CGROUP_ROOT/$d/cpu.stat"
    printf 'low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n' >"$CGROUP_ROOT/$d/memory.events"
done
mkdir -p "$CGROUP_ROOT/system.slice/containerd.service"
echo 1 >"$CGROUP_ROOT/system.slice/containerd.service/memory.current"

echo "start"
bash "$SHIPPER" start >"$T/start.out" 2>&1
check "start exits 0" test $? -eq 0
WO="$EVID/pods/flint-workers_s3w-aaaa_uid-w1/worker/0.log"
AO="$EVID/pods/wl-a1_agents-x_uid-a1/agent/0.log"
check "status: every loop alive" bash "$SHIPPER" status
if wait_for 50 cmp -s "$W/0.log" "$WO"; then ok "a worker log is copied from byte 0"; else bad "a worker log is copied from byte 0"; fi
check "an agent (wl-*) log is copied" wait_for 50 cmp -s "$A/0.log" "$AO"
check "the output path keeps <ns>_<pod>_<uid>/<container>/<N>.log" test -f "$WO"

lines "$W/0.log" 4 400
check "appended lines follow, byte-exact" wait_for 50 cmp -s "$W/0.log" "$WO"

echo "rotation"
mv "$W/0.log" "$W/0.log.20260913-101010"
lines "$W/0.log.20260913-101010" 401 410 # containerd still writing to the renamed inode before its reopen
lines "$W/0.log" 411 420
new_ino=$(python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_ino)' "$W/0.log")
WO2="$EVID/pods/flint-workers_s3w-aaaa_uid-w1/worker/0.log.i$new_ino"
check "writes to the renamed file after the rename reach the ORIGINAL copy" wait_for 50 cmp -s "$W/0.log.20260913-101010" "$WO"
check "the new N.log gets its own copy <N>.log.i<inode>, byte-exact" wait_for 50 cmp -s "$W/0.log" "$WO2"

echo "pod deletion"
lines "$A/0.log" 3 300
cp "$A/0.log" "$T/agent-final"
rm -rf "$PODS_ROOT/wl-a1_agents-x_uid-a1"
check "lines written just before the pod dir is removed are all copied" wait_for 50 cmp -s "$T/agent-final" "$AO"
check "the vanished file's tail is still held open (not lost)" wait_for 30 sh -c "bash '$SHIPPER' status | grep -q 'vanished-held-open=1'"

echo "a kill -9'd tail resumes"
pid=$(reg_field "/0.log.i$new_ino" pid)
kill -9 "$pid" 2>/dev/null
lines "$W/0.log" 421 500
check "the copy converges byte-exact (resumed at the copied size, nothing twice)" wait_for 80 cmp -s "$W/0.log" "$WO2"
launches=$(python3 - "$STATE/tails" "/0.log.i$new_ino" <<'PY'
import json, os, sys
for f in os.listdir(sys.argv[1]):
    e = json.load(open(os.path.join(sys.argv[1], f)))
    if (e.get("out") or "").endswith(sys.argv[2]):
        print(" ".join(str(l["offset"]) for l in e["launches"]))
PY
)
case "$launches" in "0 "[1-9]*) ok "registry shows a relaunch at a non-zero offset ($launches)" ;; *) bad "registry shows a relaunch at a non-zero offset" "$launches" ;; esac

echo "a tail dead when its file vanishes is a recorded LOSS"
X="$PODS_ROOT/flint-workers_s3w-bbbb_uid-x1/worker"
mkdir -p "$X"
lines "$X/0.log" 1 5
XO="$EVID/pods/flint-workers_s3w-bbbb_uid-x1/worker/0.log"
wait_for 50 cmp -s "$X/0.log" "$XO"
logs_pid=$(cat "$STATE/pids/logs.pid")
kill -STOP "$logs_pid"
xpid=$(reg_field "s3w-bbbb_uid-x1/worker/0.log" pid)
kill -9 "$xpid"
lines "$X/0.log" 6 9
rm -rf "$PODS_ROOT/flint-workers_s3w-bbbb_uid-x1"
kill -CONT "$logs_pid"
check "status counts lost=1 and names the copy" wait_for 50 sh -c "bash '$SHIPPER' status | grep -q 'lost=1'"
check "lost_after is the copied size" test "$(reg_field "s3w-bbbb_uid-x1/worker/0.log" lost_after)" = "$(wc -c <"$XO" | tr -d ' ')"

echo "rotated .gz"
gzip -c "$W/0.log.20260913-101010" >"$W/0.log.20260913-101010.gz"
mkdir -p "$C"
printf 'old rotated plugin log\n' | gzip -c >"$C/0.log.20260913-090000.gz"
lines "$C/0.log" 1 3
check "a .gz whose uncompressed name was never tailed is copied" wait_for 50 cmp -s "$C/0.log.20260913-090000.gz" "$EVID/pods/flint-system_csi-node-q_uid-c1/plugin/0.log.20260913-090000.gz"
sleep 1
check "a .gz of a rotation already tailed is NOT copied again" test ! -e "$EVID/pods/flint-workers_s3w-aaaa_uid-w1/worker/0.log.20260913-101010.gz"
check "a namespace outside NS_RE is never copied" test ! -e "$EVID/pods/default_unrelated_uid-d1"

echo "cgroups, chrony, dmesg, journal"
check "cgroups.jsonl written" wait_for 50 test -s "$EVID/cgroups.jsonl"
python3 - "$EVID/cgroups.jsonl" <<'PY' && ok "cgroup lines: pod uid (dashes restored), container id, peak, max, cpu, oom_kill; no non-kubepods cgroup" || bad "cgroup lines"
import json, sys
ls = [json.loads(l) for l in open(sys.argv[1])]
pod = [l for l in ls if l["container_id"] is None]
ct = [l for l in ls if l["container_id"]]
assert pod and ct, ls[:3]
assert all(l["pod_uid"] == "12345678-1234-1234-1234-123456789abc" for l in ls), ls[:2]
l = ct[0]
assert l["container_id"] == "ab" * 32 and l["memory_peak"] == 209715200 and l["memory_max"] == 2147483648
assert l["memory_current"] == 104857600 and l["cpu_usage_usec"] == 123456 and l["oom_kill"] == 1 and l["ts_ms"] > 0
assert not any("system.slice" in l["path"] for l in ls)
PY
check "chrony.jsonl written" wait_for 50 test -s "$EVID/chrony.jsonl"
python3 - "$EVID/chrony.jsonl" <<'PY' && ok "chrony: field 5 +0.000250 s (slow) is offset_ms -0.25 (node behind true time)" || bad "chrony sign"
import json, sys
l = json.loads(open(sys.argv[1]).readline())
assert abs(l["offset_ms"] - (-0.25)) < 1e-9, l
assert l["system_time_s"] == 0.00025 and l["leap"] == "Normal" and l["synced"] is True and l["stratum"] == 3
assert l["ref_name"] == "169.254.169.123" and abs(l["last_offset_s"] + 0.000012) < 1e-12
PY
check "dmesg.log has the OOM line, started with -w" grep -q "Out of memory.*args=-w" "$EVID/dmesg.log"
check "journal.log has the kubelet line, with a cursor file" grep -q "cursor-file=$STATE/journal.cursor" "$EVID/journal.log"

echo "flush"
bash "$SHIPPER" flush >"$T/flush.out" 2>&1
rc=$?
check "flush exits 0" test $rc -eq 0
DEST="$T/bucket/$BUCKET/_rig/evidence/$NODE"
if grep -q "objects=[1-9][0-9]* bytes=[1-9]" "$T/flush.out"; then ok "flush prints the object count and total bytes"; else bad "flush prints counts" "$(cat "$T/flush.out")"; fi
check "SHA256SUMS in the bucket verifies every uploaded file" sh -c "cd '$DEST' && shasum -a 256 -c SHA256SUMS"
check "the shipper's own logs and tail registry ride along under _shipper/" sh -c "test -s '$DEST/_shipper/logs/logs.log' && ls '$DEST/_shipper/tails/'*.json >/dev/null && grep -q ' _shipper/tails/' '$DEST/SHA256SUMS'"
check "the uploaded copy of the rotated inode equals the renamed source file" cmp -s "$W/0.log.20260913-101010" "$DEST/pods/flint-workers_s3w-aaaa_uid-w1/worker/0.log"
python3 - "$T/aws-calls.log" "s3://$BUCKET/_rig/evidence/$NODE/" <<'PY' && ok "every aws write targets s3://BUCKET/_rig/evidence/NODE/ only" || bad "an aws call outside _rig/evidence/NODE/" "$(cat "$T/aws-calls.log")"
import sys
want = sys.argv[2]
for l in open(sys.argv[1]):
    a = l.split()
    if a[1] == "sync":
        assert a[3] == want, l
    elif a[1] == "cp":
        assert a[3].startswith(want) and a[3] == want + "SHA256SUMS", l
    elif a[1] == "ls":
        assert a[2] == want, l
PY
out=$(NODE=some-other-hostname bash "$SHIPPER" flush 2>&1)
rc=$?
if [ $rc -eq 0 ] && printf '%s' "$out" | grep -q "WARNING: NODE=some-other-hostname differs" && [ ! -e "$T/bucket/$BUCKET/_rig/evidence/some-other-hostname" ]; then
    ok "a flush under another NODE warns and uploads under the recorded name only"
else
    bad "a flush under another NODE warns and uploads under the recorded name only" "rc=$rc $out"
fi
touch "$T/aws_fail_ls"
out=$(bash "$SHIPPER" flush 2>&1)
rc=$?
if [ $rc -ne 0 ] && printf '%s' "$out" | grep -q UNKNOWN && ! printf '%s' "$out" | grep -q "objects="; then ok "a failed listing exits non-zero and prints no count"; else bad "a failed listing exits non-zero and prints no count" "rc=$rc $out"; fi
rm -f "$T/aws_fail_ls"

echo "stop"
# a burst landing in the same instant as stop (tails poll once a second): only the catch-up saves it
python3 -c 'import sys; sys.stdout.write("".join("2026-09-13T10:01:00.%09dZ stderr F {\"ts_ms\":%d,\"ev\":\"burst\"}\n" % (i, i) for i in range(60000)))' >"$T/burst"
cat "$T/burst" >>"$W/0.log"
bash "$SHIPPER" stop >"$T/stop.out" 2>&1
rc=$?
check "stop exits 0" test $rc -eq 0
check "the burst written just before stop is fully copied" cmp -s "$W/0.log" "$WO2"
if bash "$SHIPPER" status >"$T/status.out" 2>&1; then bad "status after stop reports dead loops (exit 1)"; else ok "status after stop reports dead loops (exit 1)"; fi
check "no loop alive after stop" sh -c "! grep -q ': alive (pid' '$T/status.out'"
check "no tail alive after stop" sh -c "grep -q ' alive=0 ' '$T/status.out'"
check "the final flush uploaded the complete copy" cmp -s "$W/0.log" "$DEST/pods/flint-workers_s3w-aaaa_uid-w1/worker/0.log.i$new_ino"
check "the final SHA256SUMS verifies" sh -c "cd '$DEST' && shasum -a 256 -c SHA256SUMS"

echo "restart resumes without duplicating"
lines "$W/0.log" 5001 5010
bash "$SHIPPER" start >"$T/start2.out" 2>&1
check "the copy converges after the restart (bytes written while stopped included, none twice)" wait_for 80 cmp -s "$W/0.log" "$WO2"
check "the first worker copy is untouched by the restart" cmp -s "$W/0.log.20260913-101010" "$WO"
bash "$SHIPPER" stop >"$T/stop2.out" 2>&1
check "second stop exits 0" test $? -eq 0

echo
echo "shipper localtest: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
