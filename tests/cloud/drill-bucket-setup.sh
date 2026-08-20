#!/bin/bash
# ---------------------------------------------------------------------------
# Bucket + scoped credentials for the flint-lite cluster drill.
#
#   aws sso login --profile trove-admin --use-device-code    # you, first
#   tests/cloud/drill-bucket-setup.sh [bucket-name]
#
# WHY THIS EXISTS SEPARATELY FROM TROVE
#
# Trove has no S3 code at all: no bucket creation, no IRSA, and the node
# instance profile carries SSM only. The `rolesanywhere` identity trove
# provisions with is OBJECT-scoped — its policy has zero `s3:` actions,
# so it cannot create a bucket even though it can be handed one.
#
# And the hub cannot use a `rolesanywhere` session key either: those
# expire in about an hour, and an expired credential mid-drill wedges the
# hub (it self-fences when it cannot renew the epoch, exits 70, and takes
# the export down). So the hub needs a LONG-LIVED key scoped to one
# bucket, which is what this mints.
#
# Idempotent: re-running reuses an existing bucket and user, and mints a
# fresh key only if none is usable.
# ---------------------------------------------------------------------------
set -euo pipefail

BUCKET="${1:-flint-lite-drill-$(date -u +%Y%m%d)}"
REGION="${REGION:-us-west-1}"
USER_NAME="${USER_NAME:-flint-drill-hub}"
ADMIN="${ADMIN_PROFILE:-trove-admin}"

a() { aws --profile "$ADMIN" --region "$REGION" "$@"; }
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

# --- 0. the admin identity must be live ------------------------------------
a sts get-caller-identity --query Arn --output text >/dev/null 2>&1 || fail \
"profile '$ADMIN' has no valid session. Run:
    aws sso login --profile $ADMIN --use-device-code
The device-code flow is the one that works here; the browser flow is not."
pass "admin identity: $(a sts get-caller-identity --query Arn --output text)"

# --- 1. bucket, versioned --------------------------------------------------
# Versioning is NOT optional. The tier's delete-marker recovery assumes
# it, and the CRD documents the bucket as needing it — the hub never
# turns it on itself.
say "bucket $BUCKET ($REGION)"
if a s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
    pass "already exists"
else
    a s3api create-bucket --bucket "$BUCKET" \
        --create-bucket-configuration LocationConstraint="$REGION" >/dev/null
    pass "created"
fi
a s3api put-bucket-versioning --bucket "$BUCKET" \
    --versioning-configuration Status=Enabled
VER=$(a s3api get-bucket-versioning --bucket "$BUCKET" --query Status --output text)
[ "$VER" = "Enabled" ] || fail "versioning is '$VER', not Enabled"
pass "versioning Enabled"

# Belt and braces: a drill fills this bucket and nobody wants to discover
# it three months later. Abort incomplete multipart uploads after a day.
a s3api put-bucket-lifecycle-configuration --bucket "$BUCKET" \
    --lifecycle-configuration '{"Rules":[{
        "ID":"abort-incomplete-mpu","Status":"Enabled","Filter":{"Prefix":""},
        "AbortIncompleteMultipartUpload":{"DaysAfterInitiation":1}}]}' 2>/dev/null \
    && pass "lifecycle: incomplete multipart uploads abort after 1 day" \
    || echo "  ! lifecycle rule not applied (non-fatal)"

# --- 2. an IAM user scoped to exactly this bucket ---------------------------
say "IAM user $USER_NAME, scoped to $BUCKET only"
a iam get-user --user-name "$USER_NAME" >/dev/null 2>&1 \
    || a iam create-user --user-name "$USER_NAME" >/dev/null
pass "user present"

# The verb list is the hub's actual S3 surface, and each line earns its
# place: conditional PUT/GET for the epoch cell, the multipart trio for
# large publishes, ListMultipartUploads + AbortMultipartUpload because a
# takeover sweeps the previous holder's in-flight assemblies, and the
# *Version verbs because delete-marker recovery reads history.
POLICY=$(cat <<JSON
{"Version":"2012-10-17","Statement":[
 {"Effect":"Allow",
  "Action":["s3:ListBucket","s3:ListBucketVersions","s3:ListBucketMultipartUploads","s3:GetBucketVersioning"],
  "Resource":"arn:aws:s3:::$BUCKET"},
 {"Effect":"Allow",
  "Action":["s3:GetObject","s3:GetObjectVersion","s3:PutObject","s3:DeleteObject",
            "s3:DeleteObjectVersion","s3:AbortMultipartUpload","s3:ListMultipartUploadParts"],
  "Resource":"arn:aws:s3:::$BUCKET/*"}]}
JSON
)
a iam put-user-policy --user-name "$USER_NAME" \
    --policy-name "flint-drill-$BUCKET" --policy-document "$POLICY"
pass "inline policy scoped to arn:aws:s3:::$BUCKET"

# --- 3. an access key ------------------------------------------------------
# IAM allows two keys per user; a drill that reruns this should not
# accumulate them. Old keys are deleted because their secrets are not
# retrievable after creation — keeping them buys nothing.
say "access key"
for K in $(a iam list-access-keys --user-name "$USER_NAME" \
           --query 'AccessKeyMetadata[].AccessKeyId' --output text); do
    a iam delete-access-key --user-name "$USER_NAME" --access-key-id "$K"
    echo "  · removed prior key $K (its secret was unrecoverable anyway)"
done
read -r KEY_ID KEY_SECRET < <(a iam create-access-key --user-name "$USER_NAME" \
    --query 'AccessKey.[AccessKeyId,SecretAccessKey]' --output text)
[ -n "${KEY_ID:-}" ] && [ -n "${KEY_SECRET:-}" ] || fail "key creation returned nothing"
pass "minted $KEY_ID"

# --- 4. prove the key actually works on this bucket ------------------------
# IAM is eventually consistent, so a fresh key can 403 for a few seconds.
# Never hand back a credential that has not completed a round trip.
#
# The probe body is a REAL FILE, not /dev/null: the v2 CLI rejects a
# character device with "Blob values must be a path to a file" — a
# client-side parameter error that never reaches S3 and so retries
# forever. That cost a bucket-setup run, silently, because the error
# went to /dev/null too. Hence also: keep the last error and print it.
say "verifying the scoped key round-trips"
PROBE=$(mktemp); printf 'flint drill preflight\n' > "$PROBE"
trap 'rm -f "$PROBE"' EXIT
scoped() { env -u AWS_PROFILE -u AWS_SESSION_TOKEN \
    AWS_ACCESS_KEY_ID="$KEY_ID" AWS_SECRET_ACCESS_KEY="$KEY_SECRET" \
    aws --region "$REGION" "$@"; }

OK=""; ERR=""
for i in $(seq 1 12); do
    if ERR=$(scoped s3api put-object --bucket "$BUCKET" \
             --key ".flint-drill-preflight" --body "$PROBE" 2>&1 >/dev/null); then
        OK=1; break
    fi
    sleep 5
done
[ -n "$OK" ] || fail "the new key still cannot write to $BUCKET after 60s.
Last error was:
    $ERR"

# Read it back too. A PUT that 200s and a GET that 404s is a bucket in a
# state worth knowing about before the drill, not during it.
scoped s3api get-object --bucket "$BUCKET" --key ".flint-drill-preflight" \
    /dev/stdout >/dev/null 2>&1 || fail "wrote the probe but could not read it back"
scoped s3api delete-object --bucket "$BUCKET" --key ".flint-drill-preflight" >/dev/null 2>&1 || true
pass "wrote, read and deleted a probe object with the scoped key"

# And prove it is actually SCOPED — a key that works everywhere is not
# the thing we asked for.
if scoped s3 ls >/dev/null 2>&1; then
    echo "  ! WARNING: this key can list ALL buckets — the policy is wider than intended"
else
    pass "cannot list other buckets (scoped, as intended)"
fi

cat <<EOF

══════════════════════════════════════════════════════════════════
 Ready. Export these for the drill:
══════════════════════════════════════════════════════════════════

export BUCKET=$BUCKET
export REGION=$REGION
export HUB_KEY_ID=$KEY_ID
export HUB_KEY_SECRET=$KEY_SECRET

  # fish:
  set -gx BUCKET $BUCKET
  set -gx REGION $REGION
  set -gx HUB_KEY_ID $KEY_ID
  set -gx HUB_KEY_SECRET $KEY_SECRET

The secret is shown ONCE and is not retrievable afterwards. Losing it
means re-running this script, which mints a new key.

TEARDOWN when the drill is done (nothing does this for you):
  aws --profile $ADMIN --region $REGION s3 rm s3://$BUCKET --recursive
  aws --profile $ADMIN --region $REGION s3api delete-bucket --bucket $BUCKET
  aws --profile $ADMIN iam delete-user-policy --user-name $USER_NAME \\
      --policy-name flint-drill-$BUCKET
  aws --profile $ADMIN iam delete-access-key --user-name $USER_NAME \\
      --access-key-id $KEY_ID
  aws --profile $ADMIN iam delete-user --user-name $USER_NAME
EOF
