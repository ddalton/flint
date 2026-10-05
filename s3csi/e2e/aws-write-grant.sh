#!/usr/bin/env bash
# The broker's WRITE grant against AWS STS and S3 THEMSELVES: the MinIO
# write-grant drill (lean/e2e/access/write-grant-minio.sh) re-run where
# the evaluator is AWS's. AssumeRole on a role that is s3:* on the whole
# bucket, WITH the exact `write_session_policy` flint-s3-broker attaches
# to a read-write exchange on the sts backend (s3csi/SECURITY.md §4.14),
# then the same four sections:
#
#   A  a WRITER SYNCER runs whole on the narrowed keys — checkout, a
#      barrier that creates (a 20 MiB file among them: multipart), a
#      barrier that edits and deletes — and its logs carry no denial. A
#      reader on the role's own keys sees every write land.
#   B  the write keys cannot PUT, GET, DELETE or LIST ANOTHER prefix.
#      Control: the unnarrowed keys can.
#   C  the mount-s3 action set, inside the prefix (allowed) and a
#      multipart upload started outside it (denied).
#   D  GetBucketVersioning and ListMultipartUploads — the lean operator's
#      verbs — are denied. Control: the unnarrowed keys do both.
#
# Also recorded: STS's PackedPolicySize for the policy (a session policy
# is capped at 2048 plaintext characters and a packed size AWS reports as
# a percentage of its limit), and the AssumedRoleUser.
#
# Runs against what `aws-access-iam.sh up` made — eval "$(aws-access-iam.sh env)":
#   BUCKET S3_REGION; STS_KEY_FILE (a user allowed sts:AssumeRole on the
#   role and nothing else); S3_KEY_FILE (the rw user, s3:* on the bucket:
#   the reader and every control arm); ROLE_ARN (s3:* on the whole
#   bucket — the too-wide role a session policy must narrow).
# POLICY_IN=<file>  the policy as the broker unit test writes it:
#   FLINT_S3B_WRITE_WRITE_POLICY=<file> FLINT_S3B_WRITE_POLICY_TARGET=$BUCKET/$PREFIX \
#     cargo test -q --lib the_write_session_policy_writes_the_prefix_and_nothing_else
# POLICY_OVERRIDE=<file>  a hand-weakened policy instead — the drill's own
#   positive control: with PutObject removed, A must FAIL.
# SYNC=<flint-sync built with --features "s3 http">.
#
# Cost: STS and IAM are free; the bucket holds ~20 MiB for the run.
set -uo pipefail

: "${BUCKET:?}" "${S3_REGION:?}" "${STS_KEY_FILE:?}" "${S3_KEY_FILE:?}" "${ROLE_ARN:?}" "${SYNC:?}"
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
PREFIX=${PREFIX:-team/proj1}
OTHER=${OTHER:-team/other}

pass=0
fail=0
ok() { echo "PASS $*"; pass=$((pass + 1)); }
bad() { echo "FAIL $*"; fail=$((fail + 1)); }

key() { python3 -c 'import json,sys; k=json.load(open(sys.argv[1]))["AccessKey"]; print(k[sys.argv[2]])' "$1" "$2"; }
P_AK=$(key "$S3_KEY_FILE" AccessKeyId); P_SK=$(key "$S3_KEY_FILE" SecretAccessKey)
S_AK=$(key "$STS_KEY_FILE" AccessKeyId); S_SK=$(key "$STS_KEY_FILE" SecretAccessKey)

# A clean AWS CLI: no profile, no config file, only the keys given.
awsc() {
    local ak=$1 sk=$2 tok=$3
    shift 3
    env -u AWS_PROFILE AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null \
        AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" ${tok:+AWS_SESSION_TOKEN="$tok"} \
        AWS_DEFAULT_REGION="$S3_REGION" aws "$@"
}
syncer() {
    local ak=$1 sk=$2 tok=$3 root=$4
    shift 4
    env -u AWS_PROFILE AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null \
        AWS_ACCESS_KEY_ID="$ak" AWS_SECRET_ACCESS_KEY="$sk" ${tok:+AWS_SESSION_TOKEN="$tok"} AWS_REGION="$S3_REGION" \
        FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PREFIX" FLINT_SYNC_ROOT="$root" "$@"
}
denied() { grep -qiE "not authorized|AccessDenied|403" "$1"; }
sha() { python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }

[ -x "$SYNC" ] || { echo "no flint-sync at $SYNC"; exit 1; }
echo "work dir: $WORK; bucket $BUCKET ($S3_REGION); role $ROLE_ARN"
awsc $P_AK $P_SK "" sts get-caller-identity --query Arn --output text >"$WORK/parent.arn" 2>&1 || { echo "the rw user's keys do not work: $(cat "$WORK/parent.arn")"; exit 1; }
echo "unnarrowed keys: $(cat "$WORK/parent.arn")"

# The policy.
POLICY=$WORK/write-policy.json
if [ -n "${POLICY_OVERRIDE:-}" ]; then
    cp "$POLICY_OVERRIDE" "$POLICY"
    echo "POLICY OVERRIDDEN from $POLICY_OVERRIDE — this run is a control, not a result"
else
    cp "${POLICY_IN:?set POLICY_IN=<the policy file the broker unit test writes>}" "$POLICY"
fi
grep -q "\"arn:aws:s3:::$BUCKET/$PREFIX/\*\"" "$POLICY" || { echo "the policy is not for $BUCKET/$PREFIX: $(cat "$POLICY")"; exit 1; }
echo "policy ($(wc -c <"$POLICY" | tr -d ' ') bytes): $(cat "$POLICY")"

# Start clean, and plant the other prefix's object.
awsc $P_AK $P_SK "" s3 rm "s3://$BUCKET/$PREFIX/" --recursive --quiet >/dev/null 2>&1 || true
awsc $P_AK $P_SK "" s3 rm "s3://$BUCKET/$OTHER/" --recursive --quiet >/dev/null 2>&1 || true
echo "elsewhere" >"$WORK/secret.txt"
awsc $P_AK $P_SK "" s3api put-object --bucket "$BUCKET" --key "$OTHER/secret.txt" --body "$WORK/secret.txt" >/dev/null || { echo "could not plant $OTHER/secret.txt"; exit 1; }

# The write grant: AssumeRole on the sts user's keys WITH the broker's policy.
CREDS=$(awsc $S_AK $S_SK "" sts assume-role --role-arn "$ROLE_ARN" --role-session-name writer \
    --policy "file://$POLICY" --duration-seconds 900 --output json 2>"$WORK/assume.err")
if [ -z "$CREDS" ]; then
    echo "AssumeRole with the policy FAILED:"; cat "$WORK/assume.err"
    bad "A0 AWS STS did not accept the broker's write policy"
    echo "RESULT pass=$pass fail=$fail"; exit 1
fi
W_AK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["AccessKeyId"])')
W_SK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["SecretAccessKey"])')
W_TOK=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["Credentials"]["SessionToken"])')
PACKED=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("PackedPolicySize","?"))')
ASSUMED=$(echo "$CREDS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["AssumedRoleUser"]["Arn"])')
ok "A0 AWS STS accepted the broker's write policy (PackedPolicySize ${PACKED}% of the limit; $ASSUMED)"
[ "$PACKED" != "?" ] && [ "$PACKED" -le 50 ] && ok "A0b the packed policy is at most half the cap ($PACKED%): room for a longer bucket and prefix" || bad "A0b PackedPolicySize $PACKED — the policy is near STS's cap; a long bucket/prefix would be refused"

# ── A: a writer syncer runs whole on the write keys ─────────────────────
W=$WORK/writer
mkdir -p "$W"
syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" checkout >"$WORK/w-checkout.log" 2>&1 \
    && ok "A1 checkout of the empty prefix on the write keys" \
    || bad "A1 checkout failed: $(tail -1 "$WORK/w-checkout.log")"
mkdir -p "$W/src"
echo "first" >"$W/src/a.txt"
echo "readme" >"$W/README.md"
head -c 20971520 /dev/urandom >"$W/src/big.bin"
BIG_SUM=$(sha "$W/src/big.bin")
if syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" barrier >"$WORK/w-barrier1.log" 2>&1; then
    ok "A2 the first barrier (create ×3, one over the part size) published on the write keys"
else
    bad "A2 the first barrier failed: $(tail -2 "$WORK/w-barrier1.log" | tr '\n' ' ')"
fi
echo "second" >"$W/src/a.txt"
rm "$W/README.md"
syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" barrier >"$WORK/w-barrier2.log" 2>&1 || bad "A3 the second barrier failed: $(tail -1 "$WORK/w-barrier2.log")"
syncer "$W_AK" "$W_SK" "$W_TOK" "$W" "$SYNC" barrier >"$WORK/w-barrier3.log" 2>&1 || bad "A3 the third barrier failed: $(tail -1 "$WORK/w-barrier3.log")"
if denied "$WORK/w-checkout.log" || denied "$WORK/w-barrier1.log" || denied "$WORK/w-barrier2.log" || denied "$WORK/w-barrier3.log"; then
    bad "A4 the writer was DENIED something it asked for — an action the policy misses:"
    cat "$WORK"/w-*.log | grep -iE "not authorized|AccessDenied|403" | head -5
else
    ok "A4 the writer was denied nothing it asked for"
fi
R=$WORK/reader
mkdir -p "$R"
syncer $P_AK $P_SK "" "$R" "$SYNC" checkout >"$WORK/r-checkout.log" 2>&1
if [ "$(cat "$R/src/a.txt" 2>/dev/null)" = "second" ] && [ ! -e "$R/README.md" ] && [ -f "$R/src/big.bin" ] && [ "$(sha "$R/src/big.bin")" = "$BIG_SUM" ]; then
    ok "A5 a reader on the unnarrowed keys checked out the edit, the delete and the 20 MiB file byte-identical"
else
    bad "A5 the reader did not see the writes (a.txt='$(cat "$R/src/a.txt" 2>/dev/null)', README $([ -e "$R/README.md" ] && echo present || echo absent), big.bin $([ -f "$R/src/big.bin" ] && echo present || echo absent); see $WORK/r-checkout.log)"
fi

# ── B: the write keys cannot touch another prefix ───────────────────────
echo "x" >"$WORK/x.txt"
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api put-object --bucket "$BUCKET" --key "$OTHER/evil.txt" --body "$WORK/x.txt" >"$WORK/b1.out" 2>&1; then
    bad "B1 PUT to another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b1.out" && ok "B1 PUT to another prefix: AccessDenied" || { bad "B1 PUT failed but not AccessDenied"; cat "$WORK/b1.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-object --bucket "$BUCKET" --key "$OTHER/secret.txt" "$WORK/b2.body" >"$WORK/b2.out" 2>&1; then
    bad "B2 GET of another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b2.out" && ok "B2 GET of another prefix: AccessDenied" || { bad "B2 GET failed but not AccessDenied"; cat "$WORK/b2.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api delete-object --bucket "$BUCKET" --key "$OTHER/secret.txt" >"$WORK/b3.out" 2>&1; then
    bad "B3 DELETE in another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b3.out" && ok "B3 DELETE in another prefix: AccessDenied" || { bad "B3 DELETE failed but not AccessDenied"; cat "$WORK/b3.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-objects-v2 --bucket "$BUCKET" --prefix "$OTHER/" >"$WORK/b4.out" 2>&1; then
    bad "B4 LIST of another prefix on the write keys SUCCEEDED"
else
    denied "$WORK/b4.out" && ok "B4 LIST of another prefix: AccessDenied" || { bad "B4 LIST failed but not AccessDenied"; cat "$WORK/b4.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-objects-v2 --bucket "$BUCKET" >"$WORK/b5.out" 2>&1; then
    bad "B5 LIST of the whole bucket on the write keys SUCCEEDED"
else
    denied "$WORK/b5.out" && ok "B5 LIST of the whole bucket (no prefix): AccessDenied" || { bad "B5 failed but not AccessDenied"; cat "$WORK/b5.out"; }
fi
if awsc $P_AK $P_SK "" s3api get-object --bucket "$BUCKET" --key "$OTHER/secret.txt" "$WORK/b-ctl.body" >/dev/null 2>&1 &&
    awsc $P_AK $P_SK "" s3api put-object --bucket "$BUCKET" --key "$OTHER/control.txt" --body "$WORK/x.txt" >/dev/null 2>&1; then
    ok "B-control the unnarrowed keys read and write the other prefix (the narrowing is the session policy's)"
else
    bad "B-control the unnarrowed keys could not reach the other prefix: B proves nothing"
fi

# ── C: the mount-s3 shape, inside and outside the prefix ────────────────
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-objects-v2 --bucket "$BUCKET" --prefix "$PREFIX/" --output text >"$WORK/c1.out" 2>&1 && grep -q big.bin "$WORK/c1.out"; then
    ok "C1 ListBucket under the prefix lists the writer's objects"
else
    bad "C1 ListBucket under the prefix failed or saw nothing: $(head -c 200 "$WORK/c1.out")"
fi
# HeadBucket carries no prefix, so the prefix-conditioned ListBucket
# statement does not reach it: denied. mount-s3 does not need it — it is
# given --region and probes with ListObjectsV2 under its --prefix (C1),
# which is how the read grant, under the same condition, mounted on real
# nodes (aws-access.sh arm A, EC2 campaign 3).
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api head-bucket --bucket "$BUCKET" >"$WORK/c1b.out" 2>&1; then
    bad "C1b HeadBucket (no prefix) SUCCEEDED on the write keys — the listing condition did not hold"
else
    grep -q "403" "$WORK/c1b.out" && ok "C1b HeadBucket without a prefix: 403 (the listing condition holds; mount-s3 probes with ListObjectsV2 under its prefix, C1)" || { bad "C1b HeadBucket failed but not 403"; cat "$WORK/c1b.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api put-object --bucket "$BUCKET" --key "$PREFIX/mount-s3.txt" --body "$WORK/x.txt" >/dev/null 2>"$WORK/c2.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api head-object --bucket "$BUCKET" --key "$PREFIX/mount-s3.txt" >/dev/null 2>>"$WORK/c2.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-object --bucket "$BUCKET" --key "$PREFIX/mount-s3.txt" "$WORK/c2.body" >/dev/null 2>>"$WORK/c2.err" &&
    [ "$(cat "$WORK/c2.body")" = "x" ] &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api delete-object --bucket "$BUCKET" --key "$PREFIX/mount-s3.txt" >/dev/null 2>>"$WORK/c2.err"; then
    ok "C2 PutObject, HeadObject, GetObject and DeleteObject under the prefix on the write keys"
else
    bad "C2 put/head/get/delete under the prefix failed: $(tail -1 "$WORK/c2.err")"
fi
UP=$(awsc "$W_AK" "$W_SK" "$W_TOK" s3api create-multipart-upload --bucket "$BUCKET" --key "$PREFIX/mpu.bin" --query UploadId --output text 2>"$WORK/c3.err")
if [ -n "$UP" ] && awsc "$W_AK" "$W_SK" "$W_TOK" s3api upload-part --bucket "$BUCKET" --key "$PREFIX/mpu.bin" --upload-id "$UP" --part-number 1 --body "$WORK/x.txt" >/dev/null 2>>"$WORK/c3.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-parts --bucket "$BUCKET" --key "$PREFIX/mpu.bin" --upload-id "$UP" >/dev/null 2>>"$WORK/c3.err" &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api abort-multipart-upload --bucket "$BUCKET" --key "$PREFIX/mpu.bin" --upload-id "$UP" >/dev/null 2>>"$WORK/c3.err"; then
    ok "C3 a multipart upload under the prefix: created, a part uploaded, parts listed, aborted"
else
    bad "C3 multipart under the prefix failed: $(tail -1 "$WORK/c3.err")"
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api create-multipart-upload --bucket "$BUCKET" --key "$OTHER/mpu.bin" >"$WORK/c4.out" 2>&1; then
    bad "C4 a multipart upload STARTED in another prefix on the write keys"
    UP2=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["UploadId"])' "$WORK/c4.out" 2>/dev/null)
    [ -n "$UP2" ] && awsc $P_AK $P_SK "" s3api abort-multipart-upload --bucket "$BUCKET" --key "$OTHER/mpu.bin" --upload-id "$UP2" >/dev/null 2>&1
else
    denied "$WORK/c4.out" && ok "C4 a multipart upload in another prefix: AccessDenied" || { bad "C4 failed but not AccessDenied"; cat "$WORK/c4.out"; }
fi
# Versioned bucket: C2's delete of mount-s3.txt left a marker over the
# version it put. ListBucketVersions under the prefix, GetObjectVersion
# of the earlier version and DeleteObjectVersion of the marker are in
# the grant. (The syncer's own objects are content-keyed under files/
# and never overwritten, so they are not the fixture here.)
VER=$(awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-object-versions --bucket "$BUCKET" --prefix "$PREFIX/mount-s3.txt" --query 'Versions[0].VersionId' --output text 2>"$WORK/c5.err")
MARK=$(awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-object-versions --bucket "$BUCKET" --prefix "$PREFIX/mount-s3.txt" --query 'DeleteMarkers[0].VersionId' --output text 2>>"$WORK/c5.err")
if [ -n "$VER" ] && [ "$VER" != None ] && awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-object --bucket "$BUCKET" --key "$PREFIX/mount-s3.txt" --version-id "$VER" "$WORK/c5.body" >/dev/null 2>>"$WORK/c5.err" &&
    [ "$(cat "$WORK/c5.body")" = "x" ] && [ -n "$MARK" ] && [ "$MARK" != None ] &&
    awsc "$W_AK" "$W_SK" "$W_TOK" s3api delete-object --bucket "$BUCKET" --key "$PREFIX/mount-s3.txt" --version-id "$MARK" >/dev/null 2>>"$WORK/c5.err"; then
    ok "C5 ListBucketVersions under the prefix, GetObjectVersion of the deleted object's version, DeleteObjectVersion of its marker"
else
    bad "C5 the versioned verbs under the prefix failed (version=$VER marker=$MARK): $(tail -1 "$WORK/c5.err")"
fi

# ── D: the grant is not the operator's credential ───────────────────────
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-bucket-versioning --bucket "$BUCKET" >"$WORK/d1.out" 2>&1; then
    bad "D1 GetBucketVersioning on the write keys SUCCEEDED (an operator verb in a grant)"
else
    denied "$WORK/d1.out" && ok "D1 GetBucketVersioning: AccessDenied" || { bad "D1 failed but not AccessDenied"; cat "$WORK/d1.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api list-multipart-uploads --bucket "$BUCKET" >"$WORK/d2.out" 2>&1; then
    bad "D2 ListMultipartUploads on the write keys SUCCEEDED (the MPU sweep is the operator's)"
else
    denied "$WORK/d2.out" && ok "D2 ListMultipartUploads: AccessDenied" || { bad "D2 failed but not AccessDenied"; cat "$WORK/d2.out"; }
fi
if awsc "$W_AK" "$W_SK" "$W_TOK" s3api get-bucket-lifecycle-configuration --bucket "$BUCKET" >"$WORK/d3.out" 2>&1; then
    bad "D3 GetLifecycleConfiguration on the write keys SUCCEEDED"
else
    denied "$WORK/d3.out" && ok "D3 GetLifecycleConfiguration: AccessDenied" || { bad "D3 failed but not AccessDenied (a bucket with no lifecycle answers NoSuchLifecycleConfiguration to an ALLOWED caller)"; cat "$WORK/d3.out"; }
fi
if awsc $P_AK $P_SK "" s3api get-bucket-versioning --bucket "$BUCKET" >/dev/null 2>&1 &&
    awsc $P_AK $P_SK "" s3api list-multipart-uploads --bucket "$BUCKET" >/dev/null 2>&1; then
    ok "D-control the unnarrowed keys do both (the denial is the session policy's)"
else
    bad "D-control the unnarrowed keys could not: D proves nothing"
fi

# Leave the bucket as found (the IAM script purges versions at `down`).
awsc $P_AK $P_SK "" s3 rm "s3://$BUCKET/$PREFIX/" --recursive --quiet >/dev/null 2>&1 || true
echo "RESULT pass=$pass fail=$fail"
[ $fail -eq 0 ]
