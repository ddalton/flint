#!/usr/bin/env bash
# The broker's read grant against a real policy evaluator (per-user access
# design §4.3, falsifiers F3 and F8), on one host with MinIO in Docker.
#
# What it proves, and the control each claim needs:
#
#   A  a READER SYNCER runs whole on keys narrowed by the exact session
#      policy flint-s3-broker attaches (`read_session_policy`, written out by
#      its unit test): `run` starts, checks out, and pulls a writer's later
#      boundary — so the policy is not missing a read the reader needs.
#   B  those keys cannot write under the prefix: PUT and DELETE are denied
#      (F3). Control: the same requests on the parent user's keys succeed.
#   C  those keys cannot read or list ANOTHER prefix (F8). Control: the
#      parent user, whose own policy is bucket-wide ("a too-wide role"), can.
#   D  the credential holds even when flint does not: a syncer started in
#      READ-WRITE mode on the read keys fails its barrier, and the bucket's
#      manifest pointer is unchanged (D1 — the credential is the enforcement).
#
# MinIO's evaluator is not AWS's. What this shows is that the policy's
# statements say what the design says; AWS STS and Ceph RGW are phase F.
#
# Needs: docker, aws CLI, a flint-sync built with `--features "s3 http"`, and
# the spdk-csi-driver crate's test build (it writes the policy).
set -uo pipefail

REPO=/Users/ddalton/github/flint
SYNC=${SYNC:-$REPO/lean/syncer/target/debug/flint-sync}
WORK=${WORK:-$(mktemp -d)}
PORT=${PORT:-19000}
NAME=flint-read-grant-minio
EP=http://127.0.0.1:$PORT
BUCKET=flint-ws
PREFIX=team/proj1
OTHER=team/other
ROOT_AK=minioadmin
ROOT_SK=minioadmin-secret
PARENT_AK=parentuser
PARENT_SK=parentuser-secret

pass=0
fail=0
ok() { echo "PASS $*"; pass=$((pass + 1)); }
bad() { echo "FAIL $*"; fail=$((fail + 1)); }

# A clean AWS CLI: no profile, no config file, only the keys given.
awsc() {
    local ak=$1 sk=$2 tok=$3
    shift 3
    env -u AWS_PROFILE AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null \
        AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" AWS_SESSION_TOKEN="$tok" \
        AWS_DEFAULT_REGION=us-east-1 aws --endpoint-url "$EP" "$@"
}

# flint-sync on the given keys.
syncer() {
    local ak=$1 sk=$2 tok=$3 root=$4
    shift 4
    env -u AWS_PROFILE AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null \
        AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" AWS_SESSION_TOKEN="$tok" AWS_REGION=us-east-1 \
        FLINT_SYNC_BUCKET=$BUCKET FLINT_SYNC_PREFIX=$PREFIX FLINT_SYNC_ROOT="$root" \
        FLINT_SYNC_ENDPOINT=$EP "$@"
}

cleanup() { docker rm -f $NAME >/dev/null 2>&1; }
[ "${KEEP:-}" = 1 ] || trap cleanup EXIT

echo "work dir: $WORK"
cleanup
docker run -d --name $NAME -p $PORT:9000 \
    -e MINIO_ROOT_USER=$ROOT_AK -e MINIO_ROOT_PASSWORD=$ROOT_SK \
    quay.io/minio/minio:latest server /data >/dev/null || { echo "docker run failed"; exit 1; }
for _ in $(seq 1 60); do
    curl -fsS "$EP/minio/health/ready" >/dev/null 2>&1 && break
    sleep 1
done
docker exec $NAME minio --version | head -1

# The parent user: readwrite on EVERYTHING — the too-wide role a session
# policy must narrow. AssumeRole is called with its keys.
docker exec $NAME mc alias set local http://127.0.0.1:9000 $ROOT_AK $ROOT_SK >/dev/null
docker exec $NAME mc admin user add local $PARENT_AK $PARENT_SK >/dev/null
docker exec $NAME mc admin policy attach local readwrite --user $PARENT_AK >/dev/null
awsc $ROOT_AK $ROOT_SK "" s3api create-bucket --bucket $BUCKET >/dev/null
awsc $ROOT_AK $ROOT_SK "" s3api put-bucket-versioning --bucket $BUCKET \
    --versioning-configuration Status=Enabled

# The policy, as the broker builds it — or POLICY_OVERRIDE, a hand-weakened
# one, for the check's own positive control (B or C must then FAIL).
POLICY=$WORK/read-policy.json
if [ -n "${POLICY_OVERRIDE:-}" ]; then
    cp "$POLICY_OVERRIDE" "$POLICY"
    echo "POLICY OVERRIDDEN from $POLICY_OVERRIDE — this run is a control, not a result"
else
    (cd $REPO/spdk-csi-driver && CARGO_INCREMENTAL=0 FLINT_S3B_WRITE_READ_POLICY=$POLICY \
        FLINT_S3B_READ_POLICY_TARGET=$BUCKET/$PREFIX \
        cargo test -q --lib the_read_session_policy_reads_the_prefix_and_nothing_else >$WORK/policy-test.log 2>&1)
fi
[ -s "$POLICY" ] || { echo "no policy written (see $WORK/policy-test.log)"; exit 1; }
echo "policy: $(cat "$POLICY")"

# A writer publishes the first boundary, on the parent's keys.
W=$WORK/writer
mkdir -p "$W"
syncer $PARENT_AK $PARENT_SK "" "$W" "$SYNC" checkout >$WORK/w-checkout.log 2>&1
mkdir -p "$W/src"
echo "first" >"$W/src/a.txt"
echo "readme" >"$W/README.md"
syncer $PARENT_AK $PARENT_SK "" "$W" "$SYNC" barrier >$WORK/w-barrier1.log 2>&1 ||
    { echo "writer's first barrier failed"; tail -5 $WORK/w-barrier1.log; exit 1; }
# Another prefix, which the reader must not see.
awsc $PARENT_AK $PARENT_SK "" s3api put-object --bucket $BUCKET --key $OTHER/secret.txt --body "$W/README.md" >/dev/null

# The read grant: AssumeRole on the parent's keys WITH the broker's policy.
CREDS=$(awsc $PARENT_AK $PARENT_SK "" sts assume-role --role-arn arn:xxx:xxx:xxx:xxxx \
    --role-session-name reader --policy "file://$POLICY" --duration-seconds 900 --output json 2>$WORK/assume.err)
if [ -z "$CREDS" ]; then
    echo "AssumeRole with the policy failed:"
    cat $WORK/assume.err
    exit 1
fi
R_AK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["AccessKeyId"])')
R_SK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["SecretAccessKey"])')
R_TOK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["SessionToken"])')
ok "AssumeRole accepted the broker's read policy"

# ── A: a reader syncer runs whole on the read keys ──────────────────────
R=$WORK/reader
mkdir -p "$R"
syncer "$R_AK" "$R_SK" "$R_TOK" "$R" env FLINT_SYNC_ACCESS=read FLINT_SYNC_FLOOR_SECS=2 \
    FLINT_SYNC_SENTINEL_POLL_SECS=1 "$SYNC" run >$WORK/r-run.log 2>&1 &
RPID=$!
for _ in $(seq 1 60); do [ -f "$R/src/a.txt" ] && break; sleep 1; done
if [ "$(cat "$R/src/a.txt" 2>/dev/null)" = "first" ]; then
    ok "A1 reader checked out on the read keys"
else
    bad "A1 reader checkout (see $WORK/r-run.log)"
fi
# A later boundary by the writer arrives at a floor tick. A delete is
# published once the path is absent from two consecutive scans, so the
# writer runs two barriers.
echo "second" >"$W/src/a.txt"
echo "new" >"$W/src/b.txt"
rm "$W/README.md"
syncer $PARENT_AK $PARENT_SK "" "$W" "$SYNC" barrier >$WORK/w-barrier2.log 2>&1 || bad "writer's second barrier"
syncer $PARENT_AK $PARENT_SK "" "$W" "$SYNC" barrier >$WORK/w-barrier3.log 2>&1 || bad "writer's third barrier"
for _ in $(seq 1 60); do
    [ "$(cat "$R/src/a.txt" 2>/dev/null)" = "second" ] && [ -f "$R/src/b.txt" ] && [ ! -e "$R/README.md" ] && break
    sleep 1
done
if [ "$(cat "$R/src/a.txt" 2>/dev/null)" = "second" ] && [ -f "$R/src/b.txt" ] && [ ! -e "$R/README.md" ]; then
    ok "A2 reader pulled the writer's edit, add and delete on the read keys"
else
    bad "A2 reader did not converge (see $WORK/r-run.log)"
fi
# A denial reads "store: not authorized: <verb>: 403 AccessDenied" (D1's
# log below is the specimen, so this grep is checked against a real one).
if grep -qiE "not authorized|AccessDenied|403" $WORK/r-run.log; then
    bad "A3 the reader was denied something it asked for:"
    grep -iE "not authorized|AccessDenied|403" $WORK/r-run.log | head -5
else
    ok "A3 the reader was denied nothing it asked for"
fi
kill -TERM $RPID 2>/dev/null
wait $RPID
echo "reader exit on SIGTERM: $?"

# ── B: the read keys cannot write under the prefix (F3) ─────────────────
echo "x" >$WORK/x.txt
if awsc "$R_AK" "$R_SK" "$R_TOK" s3api put-object --bucket $BUCKET --key $PREFIX/src/evil.txt --body $WORK/x.txt >$WORK/b1.out 2>&1; then
    bad "B1 PUT under the prefix on the read keys SUCCEEDED"
else
    grep -q AccessDenied $WORK/b1.out && ok "B1 PUT under the prefix: AccessDenied" || { bad "B1 PUT failed but not AccessDenied"; cat $WORK/b1.out; }
fi
if awsc "$R_AK" "$R_SK" "$R_TOK" s3api delete-object --bucket $BUCKET --key $PREFIX/src/a.txt >$WORK/b2.out 2>&1; then
    bad "B2 DELETE under the prefix on the read keys SUCCEEDED"
else
    grep -q AccessDenied $WORK/b2.out && ok "B2 DELETE under the prefix: AccessDenied" || { bad "B2 DELETE failed but not AccessDenied"; cat $WORK/b2.out; }
fi
if awsc $PARENT_AK $PARENT_SK "" s3api put-object --bucket $BUCKET --key $PREFIX/control.txt --body $WORK/x.txt >/dev/null 2>&1 &&
    awsc $PARENT_AK $PARENT_SK "" s3api delete-object --bucket $BUCKET --key $PREFIX/control.txt >/dev/null 2>&1; then
    ok "B-control the parent's keys PUT and DELETE the same prefix"
else
    bad "B-control the parent could not write: B proves nothing"
fi

# ── C: the read keys cannot read or list another prefix (F8) ────────────
if awsc "$R_AK" "$R_SK" "$R_TOK" s3api get-object --bucket $BUCKET --key $OTHER/secret.txt $WORK/c1.body >$WORK/c1.out 2>&1; then
    bad "C1 GET of another prefix on the read keys SUCCEEDED"
else
    grep -q AccessDenied $WORK/c1.out && ok "C1 GET of another prefix: AccessDenied" || { bad "C1 GET failed but not AccessDenied"; cat $WORK/c1.out; }
fi
if awsc "$R_AK" "$R_SK" "$R_TOK" s3api list-objects-v2 --bucket $BUCKET --prefix $OTHER/ >$WORK/c2.out 2>&1; then
    bad "C2 LIST of another prefix on the read keys SUCCEEDED"
else
    grep -q AccessDenied $WORK/c2.out && ok "C2 LIST of another prefix: AccessDenied" || { bad "C2 LIST failed but not AccessDenied"; cat $WORK/c2.out; }
fi
if awsc "$R_AK" "$R_SK" "$R_TOK" s3api list-objects-v2 --bucket $BUCKET --prefix $PREFIX/ >/dev/null 2>&1; then
    ok "C3 LIST of the workspace's own prefix is allowed"
else
    bad "C3 LIST of the own prefix denied"
fi
if awsc $PARENT_AK $PARENT_SK "" s3api get-object --bucket $BUCKET --key $OTHER/secret.txt $WORK/c-ctl.body >/dev/null 2>&1; then
    ok "C-control the parent's keys GET the other prefix (the narrowing is the session policy's)"
else
    bad "C-control the parent could not read the other prefix: C proves nothing"
fi

# ── D: a READ-WRITE syncer on the read keys publishes nothing ───────────
ptr() { awsc $PARENT_AK $PARENT_SK "" s3api head-object --bucket $BUCKET --key $PREFIX/.flint/lean/current --query ETag --output text 2>/dev/null; }
before=$(ptr)
D=$WORK/misconfigured
mkdir -p "$D"
syncer "$R_AK" "$R_SK" "$R_TOK" "$D" "$SYNC" checkout >$WORK/d-checkout.log 2>&1
echo "should never land" >"$D/src/landed.txt"
if syncer "$R_AK" "$R_SK" "$R_TOK" "$D" "$SYNC" barrier >$WORK/d-barrier.log 2>&1; then
    bad "D1 a read-write barrier on the read keys EXITED 0"
# The conformance probe (`conformance.rs`) runs first and is denied by the
# same keys, so its line carries a 403 too. Filtered out, or this leg would
# have its specimen supplied by the probe and pass however the barrier failed.
elif grep -v 'could not ask this store' $WORK/d-barrier.log |
    grep -qiE "not authorized|AccessDenied|403"; then
    ok "D1 a read-write barrier on the read keys was denied: $(tail -1 $WORK/d-barrier.log)"
else
    bad "D1 the barrier failed, but not with a denial — A3's grep has no specimen: $(tail -1 $WORK/d-barrier.log)"
fi
after=$(ptr)
if [ -n "$before" ] && [ "$before" = "$after" ] &&
    ! awsc $PARENT_AK $PARENT_SK "" s3api list-objects-v2 --bucket $BUCKET --prefix $PREFIX/ --output text | grep -q landed; then
    ok "D2 the manifest pointer is unchanged ($before) and nothing named landed.txt exists"
else
    bad "D2 pointer before=$before after=$after, or landed.txt is in the bucket"
fi

echo "RESULT pass=$pass fail=$fail"
[ $fail -eq 0 ]
