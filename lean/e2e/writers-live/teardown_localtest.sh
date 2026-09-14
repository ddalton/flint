#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016
# teardown_localtest.sh — teardown.sh against a fake aws and a fake trove (curl),
# no AWS, no trove. The refusals and the "an error is not a zero" rules, each
# with the arm that must pass beside it:
#   no subcommand / no --yes deletes nothing; a non-drill bucket name and a
#   versioned bucket are refused; rb is confirmed by head-bucket 404;
#   pull verifies every SHA256SUMS (a byte changed in the bucket fails), an
#   empty evidence prefix fails, a failing listing fails;
#   policy: absent is success, AccessDenied is NOT absence;
#   project: delete, wait for the row, second delete 404; "Failure" fails;
#   an unreachable trove fails; two projects of one name are refused;
#   zeroset: all-zero passes; a volume, a cluster instance, a foreign instance
#   (unless --ignore-foreign-instances), a trove orphan each fail; AccessDenied
#   on list-buckets and an unreachable trove fail as NOT a zero.
#
#   bash teardown_localtest.sh [--keep]
set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TD="$HERE/teardown.sh"
T=$(mktemp -d "${TMPDIR:-/tmp}/teardown-localtest.XXXXXX")
KEEP=0
[ "${1:-}" = --keep ] && KEEP=1
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); printf '  PASS  %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n' "$1"; [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/        /' | head -30; }
trap '[ $KEEP = 1 ] && echo "kept $T" || rm -rf "$T"' EXIT

# expect <name> <want-exit> <must-grep|-> <must-not-grep|-> -- cmd...
expect() {
    local name=$1 want=$2 yes=$3 no=$4 out rc
    shift 5
    out=$("$@" 2>&1)
    rc=$?
    if [ "$rc" != "$want" ]; then bad "$name" "exit $rc, wanted $want
$out"; return; fi
    if [ "$yes" != - ] && ! printf '%s' "$out" | grep -q -- "$yes"; then bad "$name" "missing '$yes'
$out"; return; fi
    if [ "$no" != - ] && printf '%s' "$out" | grep -q -- "$no"; then bad "$name" "unexpected '$no'
$out"; return; fi
    ok "$name"
}

F="$T/fake"
mkdir -p "$T/shim" "$F/s3" "$F/state"
cat >"$T/shim/aws" <<'EOF'
#!/usr/bin/env bash
F=$FAKE
echo "$*" >>"$F/aws-calls.log"
[ "$1" = --region ] && shift 2
denied() { echo "An error occurred (AccessDenied) when calling the $1 operation: Access Denied" >&2; exit 254; }
jfile() { if [ -f "$F/state/$1" ]; then cat "$F/state/$1"; else printf '%s' "$2"; fi; }
case "$1 $2" in
    "sts get-caller-identity") echo '{"Account": "123456789012", "Arn": "arn:aws:sts::123456789012:assumed-role/AWSReservedSSO_AdministratorAccess/x"}' ;;
    "s3 ls")
        [ -e "$F/state/ls-fails" ] && denied ListObjectsV2
        d="$F/s3/${3#s3://}"; n=0; b=0
        if [ -d "$d" ]; then n=$(find "$d" -type f | wc -l | tr -d ' '); b=$(find "$d" -type f -exec cat {} + | wc -c | tr -d ' '); fi
        printf '\nTotal Objects: %s\n   Total Size: %s\n' "$n" "$b" ;;
    "s3 sync") mkdir -p "$4"; if [ -d "$F/s3/${3#s3://}" ]; then cp -R "$F/s3/${3#s3://}/." "$4/"; fi ;;  # an empty prefix syncs nothing, exit 0
    "s3 rb") rm -rf "$F/s3/${3#s3://}" ;;
    "s3api get-bucket-versioning") jfile "versioning-$4" "None"; echo ;;
    "s3api get-bucket-location") echo us-west-1 ;;
    "s3api head-bucket")
        if [ -d "$F/s3/$4" ]; then echo '{}'; else echo "An error occurred (404) when calling the HeadBucket operation: Not Found" >&2; exit 254; fi ;;
    "s3api list-buckets")
        [ -e "$F/state/list-buckets-denied" ] && denied ListBuckets
        jfile list-buckets '{"Buckets": [{"Name": "trove-other"}], "Owner": {"ID": "x"}}' ;;
    "iam get-role-policy")
        [ -e "$F/state/iam-denied" ] && denied GetRolePolicy
        if [ -f "$F/state/policy" ]; then cat "$F/state/policy"; else echo "An error occurred (NoSuchEntity) when calling the GetRolePolicy operation" >&2; exit 254; fi ;;
    "iam delete-role-policy") rm -f "$F/state/policy" ;;
    "iam list-role-policies") jfile role-policies '{"PolicyNames": ["trove-base"]}' ;;
    "ec2 describe-security-groups")
        case "$*" in *Values=default*) echo '{"SecurityGroups": [{"GroupId": "sg-default"}]}' ;; *) jfile sgs '{"SecurityGroups": []}' ;; esac ;;
    "ec2 describe-instances") jfile instances '{"Reservations": []}' ;;
    "ec2 describe-spot-instance-requests") jfile spot '{"SpotInstanceRequests": []}' ;;
    "ec2 describe-volumes") jfile volumes '{"Volumes": []}' ;;
    "cloudtrail describe-trails") echo '{"trailList": []}' ;;
    *) echo "fake aws: unhandled $*" >&2; exit 99 ;;
esac
EOF
cat >"$T/shim/curl" <<'EOF'
#!/usr/bin/env bash
F=$FAKE
[ -e "$F/state/trove-down" ] && exit 7
method=GET out=/dev/null body="" url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -X) method=$2; shift ;;
        -o) out=$2; shift ;;
        -d) body=$2; shift ;;
        -H|-w) shift ;;
        -sk) ;;
        *) url=$1 ;;
    esac
    shift
done
echo "$method $url $body" >>"$F/curl-calls.log"
path=${url#*/api/v1}
code=200
case "$method $path" in
    "GET /projects"|"GET /projects?id="*)
        python3 - "$F/state/projects.json" "$F/state" >"$out" <<'PY'
import json, os, sys
rows = json.load(open(sys.argv[1]))
if os.path.exists(os.path.join(sys.argv[2], "delete-ghost")):  # a trove that hides a row it has not deleted
    rows = [r for r in rows if r["status"] != "Deleting"]
print(json.dumps({"data": rows, "totalCount": len(rows)}))
PY
        # a deleting project disappears after two polls (or turns to Failure)
        python3 - "$F/state/projects.json" "$F/state" <<'PY'
import json, os, sys
rows = json.load(open(sys.argv[1]))
for r in list(rows):
    if r["status"] == "Deleting" and not r.get("stuck"):
        r["polls"] = r.get("polls", 0) + 1
        if r["polls"] >= 2:
            if os.path.exists(os.path.join(sys.argv[2], "delete-fails")):
                r["status"] = "Failure"
            else:
                rows.remove(r)
json.dump(rows, open(sys.argv[1], "w"))
PY
        ;;
    "POST /projects/delete")
        code=$(python3 - "$F/state/projects.json" "$body" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))
pid = json.loads(sys.argv[2])["projectId"]
for r in rows:
    if r["id"] == pid and r.get("stuck"):  # never deleted: every delete is accepted again
        r["status"] = "Deleting"
        json.dump(rows, open(sys.argv[1], "w"))
        print(200)
        sys.exit(0)
hit = [r for r in rows if r["id"] == pid]
if not hit:
    print(404)
else:
    hit[0]["status"] = "Deleting"
    json.dump(rows, open(sys.argv[1], "w"))
    print(200)
PY
)
        : >"$out" ;;
    "GET /aws/orphans")
        if [ -f "$F/state/orphans.json" ]; then cp "$F/state/orphans.json" "$out"; else echo '{"matched": 0, "orphansKnownCluster": [], "orphansDeletedCluster": [], "ghostRows": [], "purgedRows": 0, "at": "2026-09-13T12:00:00Z"}' >"$out"; fi ;;
    *) code=404; : >"$out" ;;
esac
printf '%s' "$code"
EOF
chmod +x "$T/shim/"*
export FAKE="$F" AWS_BIN="$T/shim/aws" CURL_BIN="$T/shim/curl" POLL_SECS=0.05 PROJECT_WAIT_SECS=20
B=flint-lean-writers-20260913
export BUCKET=$B

echo "teardown.sh: no accidental action"
expect "no subcommand prints usage and exits 2" 2 "NEVER from a driver" - -- bash "$TD"
check_no_calls() { [ ! -s "$F/aws-calls.log" ] && [ ! -s "$F/curl-calls.log" ]; }
if check_no_calls; then ok "no subcommand made no aws or trove call"; else bad "no subcommand made calls"; fi

echo "bucket"
mkdir -p "$F/s3/$B/_rig/evidence/n1" "$F/s3/$B-logs/2026"
echo x >"$F/s3/$B/_rig/evidence/n1/a"
echo y >"$F/s3/$B-logs/2026/log1"
expect "without --yes: counts printed, exit 2, nothing deleted" 2 "nothing deleted" - -- env LOGBUCKET="$B-logs" bash "$TD" bucket
if [ -d "$F/s3/$B" ] && ! grep -q " rb " "$F/aws-calls.log"; then ok "no rb was called without --yes"; else bad "rb without --yes"; fi
expect "a bucket not named flint-lean-writers-* is refused even with --yes" 1 "refusing" - -- env BUCKET=trove-state bash "$TD" bucket --yes
expect "a log bucket not named flint-lean-writers-* is refused" 1 "refusing" - -- env LOGBUCKET=my-logs bash "$TD" bucket --yes
echo Enabled >"$F/state/versioning-$B"
expect "a versioned bucket is refused" 1 "versioning Enabled" - -- bash "$TD" bucket --yes
rm -f "$F/state/versioning-$B"
touch "$F/state/ls-fails"
expect "a bucket that cannot be counted is not deleted" 1 "not deleting what cannot be counted" - -- bash "$TD" bucket --yes
rm -f "$F/state/ls-fails"
if [ -d "$F/s3/$B" ]; then ok "the uncountable bucket still exists"; else bad "the uncountable bucket was deleted"; fi

echo "pull"
mkdir -p "$F/s3/$B/_rig/evidence/n1/pods" "$F/s3/$B/_rig/history/a1" "$F/s3/$B/_rig/collect/A1"
printf 'one\n' >"$F/s3/$B/_rig/evidence/n1/pods/0.log"
printf 'two\n' >"$F/s3/$B/_rig/evidence/n1/chrony.jsonl"
(cd "$F/s3/$B/_rig/evidence/n1" && shasum -a 256 pods/0.log chrony.jsonl a >SHA256SUMS)
printf 'idx\n' >"$F/s3/$B/_rig/history/a1/index.jsonl"
(cd "$F/s3/$B/_rig/history" && shasum -a 256 a1/index.jsonl >SHA256SUMS)
printf 'tar\n' >"$F/s3/$B/_rig/collect/A1/a3.tar.gz"
expect "pull: every SHA256SUMS verifies, uncovered collect files reported" 0 "pull: OK" "sha256 FAILED" -- bash "$TD" pull "$T/pulled"
expect "pull again reports the uncovered collect tarball" 0 "not covered by any SHA256SUMS under collect/" - -- bash "$TD" pull "$T/pulled2"
printf 'ONE\n' >"$F/s3/$B/_rig/evidence/n1/pods/0.log"
expect "pull: a byte changed after the manifest fails" 1 "sha256 FAILED" "pull: OK" -- bash "$TD" pull "$T/pulled3"
printf 'one\n' >"$F/s3/$B/_rig/evidence/n1/pods/0.log"
rm -rf "$F/s3/$B/_rig/history"
expect "pull: an EMPTY prefix fails" 1 "_rig/history/ is EMPTY" - -- bash "$TD" pull "$T/pulled4"
expect "pull: --allow-empty accepts it" 0 "pull: OK" - -- bash "$TD" pull "$T/pulled5" --allow-empty
touch "$F/state/ls-fails"
expect "pull: a failing listing fails (not 0 objects)" 1 "listing s3://$B/_rig/collect/ failed" "objects=0" -- bash "$TD" pull "$T/pulled6"
rm -f "$F/state/ls-fails"

echo "bucket --yes"
expect "rb both buckets, head-bucket 404 confirms" 0 "s3://$B-logs: gone" "FAIL" -- env LOGBUCKET="$B-logs" bash "$TD" bucket --yes
if [ ! -d "$F/s3/$B" ] && [ ! -d "$F/s3/$B-logs" ]; then ok "both buckets gone"; else bad "buckets remain"; fi

echo "policy"
echo '{"RoleName": "TroveSSMInstanceProfile", "PolicyName": "flint-lean-writers-20260913", "PolicyDocument": {"Statement": [{"Resource": "arn:aws:s3:::flint-lean-writers-20260913/*"}]}}' >"$F/state/policy"
export POLICY=flint-lean-writers-20260913
expect "without --yes: the document printed, nothing deleted" 2 "arn:aws:s3:::flint-lean-writers-20260913" - -- bash "$TD" policy
if [ -f "$F/state/policy" ]; then ok "policy still present without --yes"; else bad "policy deleted without --yes"; fi
expect "--yes deletes and confirms NoSuchEntity" 0 "deleted (get-role-policy answers NoSuchEntity)" - -- bash "$TD" policy --yes
expect "already absent is success" 0 "already absent" - -- bash "$TD" policy --yes
touch "$F/state/iam-denied"
expect "AccessDenied is NOT absence" 1 "NOT evidence of absence" "already absent" -- bash "$TD" policy --yes
rm -f "$F/state/iam-denied"

echo "project"
echo '[{"id": 7, "name": "runcv", "status": "Running", "totalServersCount": 4}, {"id": 8, "name": "ozone1", "status": "Running", "totalServersCount": 1}]' >"$F/state/projects.json"
touch "$F/state/trove-down"
expect "an unreachable trove fails (not a 404)" 1 "did not answer" - -- bash "$TD" project runcv --yes
rm -f "$F/state/trove-down"
expect "without --yes nothing is posted" 2 "nothing deleted" - -- bash "$TD" project runcv
if ! grep -q "^POST" "$F/curl-calls.log" 2>/dev/null; then ok "no POST without --yes"; else bad "POST without --yes"; fi
expect "delete, wait for the row to go, second delete 404" 0 "second delete answers 404" - -- bash "$TD" project runcv --yes
if python3 -c 'import json,sys; rows=json.load(open(sys.argv[1])); sys.exit(0 if [r["id"] for r in rows]==[8] else 1)' "$F/state/projects.json"; then ok "only the named project was deleted"; else bad "wrong projects deleted" "$(cat "$F/state/projects.json")"; fi
expect "a name trove no longer lists is reported, exit 0" 0 "no project named 'runcv'" - -- bash "$TD" project runcv --yes
touch "$F/state/delete-fails"
expect "a delete trove marks Failure fails" 1 "Failure" - -- bash "$TD" project ozone1 --yes
rm -f "$F/state/delete-fails"
echo '[{"id": 11, "name": "ghost", "status": "Running", "stuck": true}]' >"$F/state/projects.json"
touch "$F/state/delete-ghost"
expect "a row that vanished from the list but still accepts a delete (200) fails" 1 "not 404" "deleted (a second delete" -- bash "$TD" project ghost --yes
rm -f "$F/state/delete-ghost"
echo '[{"id": 12, "name": "stuck", "status": "Running", "stuck": true}]' >"$F/state/projects.json"
expect "a row that never goes away times out (wall clock), not a hang" 1 "still present" - -- env PROJECT_WAIT_SECS=2 POLL_SECS=0.2 bash "$TD" project stuck --yes
echo '[{"id": 9, "name": "dup", "status": "Running"}, {"id": 10, "name": "dup", "status": "Running"}]' >"$F/state/projects.json"
expect "two projects of one name are refused" 1 "refusing to guess" - -- bash "$TD" project dup --yes

echo "zeroset"
expect "all zero: ZERO SET yes, identity printed first" 0 "ZERO SET: yes" "FAIL" -- bash "$TD" zeroset runcv
expect "the identity line is printed" 0 "caller identity (profile trove-admin)" - -- bash "$TD" zeroset runcv
echo '{"Volumes": [{"VolumeId": "vol-1", "Size": 8}]}' >"$F/state/volumes"
expect "an available volume fails" 1 "FAIL  available EBS volumes: 1" "ZERO SET: yes" -- bash "$TD" zeroset runcv
rm -f "$F/state/volumes"
echo '{"Reservations": [{"Instances": [{"InstanceId": "i-1", "InstanceType": "i4i.large", "State": {"Name": "running"}, "Tags": [{"Key": "trove:cluster", "Value": "runcv"}]}]}]}' >"$F/state/instances"
expect "a cluster instance fails even with --ignore-foreign-instances" 1 "FAIL  non-terminated instances of cluster runcv: 1" - -- bash "$TD" zeroset runcv --ignore-foreign-instances
echo '{"Reservations": [{"Instances": [{"InstanceId": "i-2", "InstanceType": "t3.small", "State": {"Name": "running"}, "Tags": [{"Key": "Name", "Value": "someone-else"}]}]}]}' >"$F/state/instances"
expect "a foreign instance fails by default" 1 "FAIL  non-terminated instances in us-west-1 (all): 1" - -- bash "$TD" zeroset runcv
expect "a foreign instance is reported, not failed, with --ignore-foreign-instances" 0 "NOTE  non-terminated instances in us-west-1 (all): 1" "ZERO SET: NO" -- bash "$TD" zeroset runcv --ignore-foreign-instances
rm -f "$F/state/instances"
echo '{"Buckets": [{"Name": "flint-lean-writers-20260913"}], "Owner": {"ID": "x"}}' >"$F/state/list-buckets"
expect "a leftover drill bucket fails" 1 "FAIL  buckets flint-lean-writers-\*: 1" - -- bash "$TD" zeroset runcv
rm -f "$F/state/list-buckets"
touch "$F/state/list-buckets-denied"
expect "AccessDenied on list-buckets is NOT a zero" 1 "query failed (NOT a zero)" "ZERO SET: yes" -- bash "$TD" zeroset runcv
rm -f "$F/state/list-buckets-denied"
echo '{"PolicyNames": ["trove-base", "flint-lean-writers-20260913"]}' >"$F/state/role-policies"
expect "the drill's inline policy still on the role fails" 1 "FAIL  drill inline policy" - -- bash "$TD" zeroset runcv
rm -f "$F/state/role-policies"
echo '{"SecurityGroups": [{"GroupId": "sg-9", "GroupName": "trove-runcv-abc"}]}' >"$F/state/sgs"
expect "a leftover trove-<cluster>-* security group fails" 1 "FAIL  security groups trove-runcv-\*: 1" - -- bash "$TD" zeroset runcv
rm -f "$F/state/sgs"
echo '{"matched": 1, "orphansKnownCluster": [], "orphansDeletedCluster": [{"instanceId": "i-3"}], "ghostRows": [5], "purgedRows": 0, "at": "x"}' >"$F/state/orphans.json"
expect "trove orphans (matched, ghostRows, orphans) fail" 1 "FAIL  trove reconcile: ghostRows: 1" - -- bash "$TD" zeroset runcv
rm -f "$F/state/orphans.json"
touch "$F/state/trove-down"
expect "an unreachable trove is NOT a zero" 1 "trove did not answer /aws/orphans (NOT a zero)" "ZERO SET: yes" -- bash "$TD" zeroset runcv
rm -f "$F/state/trove-down"

echo
echo "teardown localtest: $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ]
