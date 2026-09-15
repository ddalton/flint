#!/usr/bin/env bash
# The AWS half of the access drill (aws-access.sh): one bucket and the
# identities the three broker arms need, made and destroyed together.
#
#   ./aws-access-iam.sh up      # bucket + users + role; key JSONs under WORK
#   ./aws-access-iam.sh down    # purge + delete everything `up` made, then verify
#   ./aws-access-iam.sh env     # print the exports aws-access.sh reads
#
# What it makes (DATE-suffixed, us-west-1 unless S3_REGION):
#   bucket  flint-access-drill-DATE   versioning on, public access blocked
#   user    flint-acc-rw-DATE         s3:* on the bucket — the rig's key and
#                                     the static backend's one key
#   user    flint-acc-ro-DATE         Get/List on the bucket — the static
#                                     backend's READ key
#   user    flint-acc-sts-DATE        sts:AssumeRole on the role, nothing else —
#                                     the STS stand-in's own key
#   role    flint-acc-role-DATE       s3:* on the WHOLE bucket, trusted by the
#                                     sts user: the too-wide role a session
#                                     policy has to narrow
#
# Needs the trove-admin profile (rolesanywhere can create neither buckets
# nor IAM). Key JSONs are written mode 600 under WORK, never in the repo.
set -uo pipefail
PROFILE=${PROFILE:-trove-admin}
S3_REGION=${S3_REGION:-us-west-1}
DATE=${DATE:-$(date +%Y%m%d)}
WORK=${WORK:-/private/tmp/claude-503/flint-access-drill}
B=flint-access-drill-$DATE
RW=flint-acc-rw-$DATE
RO=flint-acc-ro-$DATE
STS=flint-acc-sts-$DATE
ROLE=flint-acc-role-$DATE
aws_() { env -u AWS_ACCESS_KEY_ID -u AWS_SECRET_ACCESS_KEY -u AWS_SESSION_TOKEN aws --profile "$PROFILE" "$@"; }
mkdir -p "$WORK" && chmod 700 "$WORK"

bucket_policy() { # actions-json
    printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":%s,"Resource":["arn:aws:s3:::%s","arn:aws:s3:::%s/*"]}]}' "$1" "$B" "$B"
}

case "${1:-}" in
up)
    set -e
    acct=$(aws_ sts get-caller-identity --query Account --output text)
    echo "account $acct, region $S3_REGION, bucket $B"
    aws_ s3api create-bucket --bucket "$B" --region "$S3_REGION" \
        --create-bucket-configuration LocationConstraint="$S3_REGION" >/dev/null
    aws_ s3api put-public-access-block --bucket "$B" --public-access-block-configuration \
        BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true
    aws_ s3api put-bucket-versioning --bucket "$B" --versioning-configuration Status=Enabled
    for u in "$RW" "$RO" "$STS"; do aws_ iam create-user --user-name "$u" >/dev/null; done
    aws_ iam put-user-policy --user-name "$RW" --policy-name bucket-rw --policy-document "$(bucket_policy '["s3:*"]')"
    aws_ iam put-user-policy --user-name "$RO" --policy-name bucket-ro \
        --policy-document "$(bucket_policy '["s3:GetObject","s3:GetObjectVersion","s3:GetObjectAttributes","s3:ListBucket","s3:ListBucketVersions"]')"
    trust=$(printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"AWS":"arn:aws:iam::%s:user/%s"},"Action":"sts:AssumeRole"}]}' "$acct" "$STS")
    # The trust names a user created a moment ago; IAM can refuse it as an
    # invalid principal until the user has propagated.
    for i in 1 2 3 4 5 6; do
        aws_ iam create-role --role-name "$ROLE" --assume-role-policy-document "$trust" >/dev/null 2>&1 && break
        sleep 10
    done
    aws_ iam get-role --role-name "$ROLE" >/dev/null
    aws_ iam put-role-policy --role-name "$ROLE" --policy-name bucket-wide --policy-document "$(bucket_policy '["s3:*"]')"
    aws_ iam put-user-policy --user-name "$STS" --policy-name assume-drill-role \
        --policy-document "$(printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"sts:AssumeRole","Resource":"arn:aws:iam::%s:role/%s"}]}' "$acct" "$ROLE")"
    for u in "$RW" "$RO" "$STS"; do
        umask 077
        aws_ iam create-access-key --user-name "$u" > "$WORK/key-$u.json"
    done
    echo "arn:aws:iam::$acct:role/$ROLE" > "$WORK/role-arn"
    echo "up: keys in $WORK (mode 600). IAM is eventually consistent: allow ~15 s before first use."
    ;;
env)
    echo "export BUCKET=$B S3_REGION=$S3_REGION"
    echo "export S3_KEY_FILE=$WORK/key-$RW.json RO_KEY_FILE=$WORK/key-$RO.json STS_KEY_FILE=$WORK/key-$STS.json"
    echo "export ROLE_ARN=$(cat "$WORK/role-arn" 2>/dev/null)"
    ;;
down)
    # Every version and delete marker, then the bucket.
    if aws_ s3api head-bucket --bucket "$B" >/dev/null 2>&1; then
        while :; do
            aws_ s3api list-object-versions --bucket "$B" --max-items 1000 --output json > "$WORK/versions.json" || break
            n=$(python3 - "$WORK/versions.json" "$WORK/delete.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
objs = [{"Key": v["Key"], "VersionId": v["VersionId"]} for v in (d.get("Versions") or []) + (d.get("DeleteMarkers") or [])]
json.dump({"Objects": objs, "Quiet": True}, open(sys.argv[2], "w"))
print(len(objs))
PY
)
            [ "$n" -gt 0 ] || break
            aws_ s3api delete-objects --bucket "$B" --delete "file://$WORK/delete.json" >/dev/null
            echo "purged $n versions"
        done
        aws_ s3api delete-bucket --bucket "$B" && echo "bucket $B deleted"
    fi
    for u in "$RW" "$RO" "$STS"; do
        for k in $(aws_ iam list-access-keys --user-name "$u" --query 'AccessKeyMetadata[].AccessKeyId' --output text 2>/dev/null); do
            aws_ iam delete-access-key --user-name "$u" --access-key-id "$k"
        done
        for p in $(aws_ iam list-user-policies --user-name "$u" --query 'PolicyNames[]' --output text 2>/dev/null); do
            aws_ iam delete-user-policy --user-name "$u" --policy-name "$p"
        done
        aws_ iam delete-user --user-name "$u" 2>/dev/null && echo "user $u deleted"
        rm -f "$WORK/key-$u.json"
    done
    for p in $(aws_ iam list-role-policies --role-name "$ROLE" --query 'PolicyNames[]' --output text 2>/dev/null); do
        aws_ iam delete-role-policy --role-name "$ROLE" --policy-name "$p"
    done
    aws_ iam delete-role --role-name "$ROLE" 2>/dev/null && echo "role $ROLE deleted"
    # The zero set, by name: every probe must say it is gone.
    left=0
    aws_ s3api head-bucket --bucket "$B" >/dev/null 2>&1 && { echo "STILL THERE: bucket $B"; left=1; }
    for u in "$RW" "$RO" "$STS"; do
        aws_ iam get-user --user-name "$u" >/dev/null 2>&1 && { echo "STILL THERE: user $u"; left=1; }
    done
    aws_ iam get-role --role-name "$ROLE" >/dev/null 2>&1 && { echo "STILL THERE: role $ROLE"; left=1; }
    [ $left -eq 0 ] && echo "zero set: bucket, 3 users, role all gone"
    exit $left
    ;;
*) echo "usage: aws-access-iam.sh up|env|down" >&2; exit 2 ;;
esac
