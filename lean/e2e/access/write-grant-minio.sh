#!/usr/bin/env bash
# The broker's WRITE grant against a real policy evaluator (per-user access
# design §4.3 as written; D14 superseded 2026-10-05), on one host with MinIO
# in Docker — read-grant-minio.sh's twin, for `write_session_policy`, the
# policy flint-s3-broker attaches to a READ-WRITE exchange on the sts
# backend (s3csi/SECURITY.md §4.14).
#
# What it proves, and the control each claim needs:
#
#   A  a WRITER SYNCER runs whole on keys narrowed by the exact policy
#      (written out by the broker's unit test): checkout of an empty
#      prefix, a barrier that creates (a 20 MiB file among them, so the
#      multipart verbs are exercised), a barrier that edits and deletes —
#      and its log carries no denial. A reader on the parent's keys sees
#      every write land. This is where D14's worry — an action the policy
#      misses fails every writer at once — would show.
#   B  the write keys cannot PUT, GET, DELETE or LIST ANOTHER prefix.
#      Control: the parent user, bucket-wide, can.
#   C  the mount-s3 shape: what Mountpoint documents it needs (ListBucket,
#      GetObject, PutObject, AbortMultipartUpload, DeleteObject), each done
#      with the aws CLI on the write keys INSIDE the prefix (allowed) and
#      a multipart upload started OUTSIDE it (denied). mount-s3 itself is
#      not run here (FUSE); the kind rig's broker is `static`, so this is
#      the one place the write policy meets an evaluator.
#   D  the keys are not the operator's: GetBucketVersioning and
#      ListMultipartUploads on the bucket are denied. Control: the parent
#      can do both.
#
# POLICY_OVERRIDE=<file> runs a hand-weakened policy instead — the drill's
# own positive control: with PutObject removed, A must FAIL.
#
# MinIO's evaluator is not AWS's. What this shows is that the policy's
# statements say what the design says; AWS STS and Ceph RGW are not run.
#
# Needs: docker, aws CLI, a flint-sync built with `--features "s3 http"`
# (SYNC=), and the spdk-csi-driver crate's test build (it writes the
# policy; CARGO_TARGET_DIR is honoured).
set -uo pipefail

REPO=${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}
SYNC=${SYNC:-$REPO/lean/syncer/target/debug/flint-sync}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
PORT=${PORT:-19001}
NAME=flint-write-grant-minio
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
# "denied" in a CLI or syncer log: MinIO answers AccessDenied; the store
# layer renders it "not authorized: <verb>: 403 AccessDenied".
denied() { grep -qiE "not authorized|AccessDenied|403" "$1"; }

cleanup() { docker rm -f $NAME >/dev/null 2>&1; }
[ "${KEEP:-}" = 1 ] || trap cleanup EXIT

[ -x "$SYNC" ] || { echo "no flint-sync at $SYNC (build lean/syncer with --features \"s3 http\", or set SYNC=)"; exit 1; }
echo "work dir: $WORK"
cleanup
docker run -d --name $NAME -p $PORT:9000 --tmpfs /data \
    -e MINIO_ROOT_USER=$ROOT_AK -e MINIO_ROOT_PASSWORD=$ROOT_SK \
    cgr.dev/chainguard/minio:latest server /data >/dev/null || { echo "docker run failed"; exit 1; }
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

# The policy, as the broker builds it — or POLICY_OVERRIDE for the control.
POLICY=$WORK/write-policy.json
if [ -n "${POLICY_OVERRIDE:-}" ]; then
    cp "$POLICY_OVERRIDE" "$POLICY"
    echo "POLICY OVERRIDDEN from $POLICY_OVERRIDE — this run is a control, not a result"
else
    (cd "$REPO/spdk-csi-driver" && CARGO_INCREMENTAL=0 FLINT_S3B_WRITE_WRITE_POLICY=$POLICY \
        FLINT_S3B_WRITE_POLICY_TARGET=$BUCKET/$PREFIX \
        cargo test -q --lib the_write_session_policy_writes_the_prefix_and_nothing_else >"$WORK/policy-test.log" 2>&1)
fi
[ -s "$POLICY" ] || { echo "no policy written (see $WORK/policy-test.log)"; exit 1; }
echo "policy: $(cat "$POLICY")"

# Another prefix, which the write keys must not reach.
echo "elsewhere" >"$WORK/secret.txt"
awsc $PARENT_AK $PARENT_SK "" s3api put-object --bucket $BUCKET --key $OTHER/secret.txt --body "$WORK/secret.txt" >/dev/null

# The write grant: AssumeRole on the parent's keys WITH the broker's policy.
CREDS=$(awsc $PARENT_AK $PARENT_SK "" sts assume-role --role-arn arn:xxx:xxx:xxx:xxxx \
    --role-session-name writer --policy "file://$POLICY" --duration-seconds 900 --output json 2>"$WORK/assume.err")
if [ -z "$CREDS" ]; then
    echo "AssumeRole with the policy failed:"
    cat "$WORK/assume.err"
    exit 1
fi
W_AK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["AccessKeyId"])')
W_SK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["SecretAccessKey"])')
W_TOK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["SessionToken"])')
ok "AssumeRole accepted the broker's write policy"

# ── A: a writer syncer runs whole on the write keys ─────────────────────
W=$WORK/writer
mkdir -p "$W"
syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" checkout >"$WORK/w-checkout.log" 2>&1 \
    && ok "A1 checkout of the empty prefix on the write keys" \
    || { bad "A1 checkout failed: $(tail -1 "$WORK/w-checkout.log")"; }
mkdir -p "$W/src"
echo "first" >"$W/src/a.txt"
echo "readme" >"$W/README.md"
# Over the part size (crates/flint-store S3_MIN_PART), so this barrier
# creates, uploads parts and completes a multipart upload under the grant.
head -c 20971520 /dev/urandom >"$W/src/big.bin"
BIG_SUM=$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$W/src/big.bin")
if syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" barrier >"$WORK/w-barrier1.log" 2>&1; then
    ok "A2 the first barrier (create ×3, one over the part size) published on the write keys"
else
    bad "A2 the first barrier failed: $(tail -2 "$WORK/w-barrier1.log" | tr '\n' ' ')"
fi
echo "second" >"$W/src/a.txt"
rm "$W/README.md"
# A delete is published once the path is absent from two consecutive scans.
syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" barrier >"$WORK/w-barrier2.log" 2>&1 || bad "A3 the second barrier failed: $(tail -1 "$WORK/w-barrier2.log")"
syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" barrier >"$WORK/w-barrier3.log" 2>&1 || bad "A3 the third barrier failed: $(tail -1 "$WORK/w-barrier3.log")"
if denied "$WORK/w-checkout.log" || denied "$WORK/w-barrier1.log" || denied "$WORK/w-barrier2.log" || denied "$WORK/w-barrier3.log"; then
    bad "A4 the writer was DENIED something it asked for — an action the policy misses (D14's worry):"
    cat "$WORK"/w-*.log | grep -iE "not authorized|AccessDenied|403" | head -5
else
    ok "A4 the writer was denied nothing it asked for"
fi
# A reader on the parent's keys sees the writes: edit, delete, and the big
# file byte-identical.
R=$WORK/reader
mkdir -p "$R"
syncer $PARENT_AK $PARENT_SK "" "$R" "$SYNC" checkout >"$WORK/r-checkout.log" 2>&1
if [ "$(cat "$R/src/a.txt" 2>/dev/null)" = "second" ] && [ ! -e "$R/README.md" ] && [ -f "$R/src/big.bin" ] &&
    [ "$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$R/src/big.bin")" = "$BIG_SUM" ]; then
    ok "A5 a reader checked out the writer's edit, delete and the 20 MiB file byte-identical"
else
    bad "A5 the reader did not see the writes (a.txt='$(cat "$R/src/a.txt" 2>/dev/null)', README $([ -e "$R/README.md" ] && echo present || echo absent), big.bin $([ -f "$R/src/big.bin" ] && echo present || echo absent); see $WORK/r-checkout.log)"
fi

# ── B: the write keys cannot touch another prefix ───────────────────────
echo "x" >"$WORK/x.txt"
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api put-object --bucket $BUCKET --key $OTHER/evil.txt --body "$WORK/x.txt" >"$WORK/b1.out" 2>&1; then
    bad "B1 PUT to another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b1.out" && ok "B1 PUT to another prefix: AccessDenied" || { bad "B1 PUT failed but not AccessDenied"; cat "$WORK/b1.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-object --bucket $BUCKET --key $OTHER/secret.txt "$WORK/b2.body" >"$WORK/b2.out" 2>&1; then
    bad "B2 GET of another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b2.out" && ok "B2 GET of another prefix: AccessDenied" || { bad "B2 GET failed but not AccessDenied"; cat "$WORK/b2.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api delete-object --bucket $BUCKET --key $OTHER/secret.txt >"$WORK/b3.out" 2>&1; then
    bad "B3 DELETE in another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b3.out" && ok "B3 DELETE in another prefix: AccessDenied" || { bad "B3 DELETE failed but not AccessDenied"; cat "$WORK/b3.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-objects-v2 --bucket $BUCKET --prefix $OTHER/ >"$WORK/b4.out" 2>&1; then
    bad "B4 LIST of another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b4.out" && ok "B4 LIST of another prefix: AccessDenied" || { bad "B4 LIST failed but not AccessDenied"; cat "$WORK/b4.out"; }
fi
if awsc $PARENT_AK $PARENT_SK "" s3api get-object --bucket $BUCKET --key $OTHER/secret.txt "$WORK/b-ctl.body" >/dev/null 2>&1 &&
    awsc $PARENT_AK $PARENT_SK "" s3api put-object --bucket $BUCKET --key $OTHER/control.txt --body "$WORK/x.txt" >/dev/null 2>&1; then
    ok "B-control the parent's keys read and write the other prefix (the narrowing is the session policy's)"
else
    bad "B-control the parent could not reach the other prefix: B proves nothing"
fi

# ── C: the mount-s3 shape, inside and outside the prefix ────────────────
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-objects-v2 --bucket $BUCKET --prefix $PREFIX/ --output text >"$WORK/c1.out" 2>&1 && grep -q big.bin "$WORK/c1.out"; then
    ok "C1 ListBucket under the prefix lists the writer's objects"
else
    bad "C1 ListBucket under the prefix failed or saw nothing: $(head -c 200 "$WORK/c1.out")"
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api put-object --bucket $BUCKET --key $PREFIX/mount-s3.txt --body "$WORK/x.txt" >/dev/null 2>"$WORK/c2.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-object --bucket $BUCKET --key $PREFIX/mount-s3.txt "$WORK/c2.body" >/dev/null 2>>"$WORK/c2.err" &&
    [ "$(cat "$WORK/c2.body")" = "x" ] &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api delete-object --bucket $BUCKET --key $PREFIX/mount-s3.txt >/dev/null 2>>"$WORK/c2.err"; then
    ok "C2 PutObject, GetObject and DeleteObject under the prefix on the write keys"
else
    bad "C2 put/get/delete under the prefix failed: $(tail -1 "$WORK/c2.err")"
fi
UP=$(awsc "$W_AK" "$W_SK" "$W_TOK" s3api create-multipart-upload --bucket $BUCKET --key $PREFIX/mpu.bin --query UploadId --output text 2>"$WORK/c3.err")
if [ -n "$UP" ] && awsc "$W_AK" "$W_SK" "$W_TOK" s3api upload-part --bucket $BUCKET --key $PREFIX/mpu.bin --upload-id "$UP" --part-number 1 --body "$WORK/x.txt" >/dev/null 2>>"$WORK/c3.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-parts --bucket $BUCKET --key $PREFIX/mpu.bin --upload-id "$UP" >/dev/null 2>>"$WORK/c3.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api abort-multipart-upload --bucket $BUCKET --key $PREFIX/mpu.bin --upload-id "$UP" >/dev/null 2>>"$WORK/c3.err"; then
    ok "C3 a multipart upload under the prefix: created, a part uploaded, parts listed, aborted"
else
    bad "C3 multipart under the prefix failed: $(tail -1 "$WORK/c3.err")"
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api create-multipart-upload --bucket $BUCKET --key $OTHER/mpu.bin >"$WORK/c4.out" 2>&1; then
    bad "C4 a multipart upload STARTED in another prefix on the write keys"
    UP2=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["UploadId"])' "$WORK/c4.out" 2>/dev/null)
    [ -n "$UP2" ] && awsc $PARENT_AK $PARENT_SK "" s3api abort-multipart-upload --bucket $BUCKET --key $OTHER/mpu.bin --upload-id "$UP2" >/dev/null 2>&1
else
    denied "$WORK/c4.out" && ok "C4 a multipart upload in another prefix: AccessDenied" || { bad "C4 failed but not AccessDenied"; cat "$WORK/c4.out"; }
fi

# ── D: the grant is not the operator's credential ───────────────────────
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-bucket-versioning --bucket $BUCKET >"$WORK/d1.out" 2>&1; then
    bad "D1 GetBucketVersioning on the write keys SUCCEEDED (an operator verb in a grant)"
else
    denied "$WORK/d1.out" && ok "D1 GetBucketVersioning: AccessDenied" || { bad "D1 failed but not AccessDenied"; cat "$WORK/d1.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-multipart-uploads --bucket $BUCKET >"$WORK/d2.out" 2>&1; then
    bad "D2 ListMultipartUploads on the write keys SUCCEEDED (the MPU sweep is the operator's)"
else
    denied "$WORK/d2.out" && ok "D2 ListMultipartUploads: AccessDenied" || { bad "D2 failed but not AccessDenied"; cat "$WORK/d2.out"; }
fi
if awsc $PARENT_AK $PARENT_SK "" s3api get-bucket-versioning --bucket $BUCKET >/dev/null 2>&1 &&
    awsc $PARENT_AK $PARENT_SK "" s3api list-multipart-uploads --bucket $BUCKET >/dev/null 2>&1; then
    ok "D-control the parent's keys do both (the denial is the session policy's)"
else
    bad "D-control the parent could not: D proves nothing"
fi

echo "RESULT pass=$pass fail=$fail"
[ $fail -eq 0 ]
