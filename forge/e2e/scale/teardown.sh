#!/usr/bin/env bash
# Take the scale rig's cloud footprint down and VERIFY it is gone: the
# trove project (all spot instances), every orphaned multipart upload,
# the bucket, the scoped IAM user and its key, the local keyfile.
#
#   BUCKET=... KEYFILE=... CLUSTER=<trove project> ./forge/e2e/scale/teardown.sh
#
# Needs the admin profile (the drill's rolesanywhere profile can neither
# delete a bucket nor an IAM user) and the trove backend that created
# the cluster. Every step is idempotent; the verification at the end is
# the point — a teardown that is not checked is a bill.
set -uo pipefail
REGION=${REGION:-us-west-1}
: "${BUCKET:?set BUCKET}"; : "${CLUSTER:?set CLUSTER to the trove project name}"
IAM_USER=${IAM_USER:-$BUCKET}
export AWS_PROFILE=${ADMIN_PROFILE:-trove-admin} AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION
TROVE=${TROVE_BASE_URL:-https://localhost:8080/api/v1}
# The admin identity, verified BEFORE anything is judged. With an expired
# SSO token every head-bucket and get-user below fails, and the first
# run of this script on runby (2026-09-05) read those failures as
# "absent", shredded the keyfile, and left the bucket and the user
# standing: an auth failure is not an absence. A judgement needs a
# credential that can see.
aws sts get-caller-identity --query Arn --output text >/dev/null 2>&1 \
    || { echo "the admin profile '$AWS_PROFILE' cannot authenticate — run: aws sso login --profile $AWS_PROFILE --use-device-code — nothing judged, nothing removed" >&2; exit 3; }
api() { curl -sk --noproxy '*' --max-time 60 -X "$1" "$TROVE$2" -H 'Authorization: Bearer trove-dummy-token' -H 'Content-Type: application/json' ${3:+-d "$3"}; }

echo "── cluster $CLUSTER ──"
pid=$(api GET /projects | jq -r --arg n "$CLUSTER" '.data[] | select(.name==$n) | .id')
if [ -n "$pid" ]; then api POST /projects/delete "{\"projectId\":$pid}" >/dev/null; echo "  project $pid: delete requested"
else echo "  no such project (already gone)"; fi

echo "── bucket $BUCKET ──"
# 404 is absence; anything else that is not success is "cannot tell".
hb=$(aws s3api head-bucket --bucket "$BUCKET" 2>&1); hb_rc=$?
if [ "$hb_rc" != 0 ] && ! printf '%s' "$hb" | grep -q -E '\(404\)|Not Found'; then
    echo "  cannot tell whether the bucket exists: $hb" >&2; exit 3
fi
if [ "$hb_rc" = 0 ]; then
    n=0
    while read -r uid k; do
        [ -n "$uid" ] || continue
        aws s3api abort-multipart-upload --bucket "$BUCKET" --key "$k" --upload-id "$uid" && n=$((n+1))
    done < <(aws s3api list-multipart-uploads --bucket "$BUCKET" --query 'Uploads[].[UploadId,Key]' --output text 2>/dev/null | grep -v '^None')
    echo "  aborted $n multipart upload(s)"
    aws s3 rm "s3://$BUCKET" --recursive --quiet
    aws s3api delete-bucket --bucket "$BUCKET" && echo "  deleted"
else echo "  absent"; fi

echo "── iam user $IAM_USER ──"
gu=$(aws iam get-user --user-name "$IAM_USER" 2>&1); gu_rc=$?
if [ "$gu_rc" != 0 ] && ! printf '%s' "$gu" | grep -q NoSuchEntity; then
    echo "  cannot tell whether the user exists: $gu" >&2; exit 3
fi
if [ "$gu_rc" = 0 ]; then
    for k in $(aws iam list-access-keys --user-name "$IAM_USER" --query 'AccessKeyMetadata[].AccessKeyId' --output text); do
        aws iam delete-access-key --user-name "$IAM_USER" --access-key-id "$k" && echo "  key ${k:0:4}… deleted"
    done
    for p in $(aws iam list-user-policies --user-name "$IAM_USER" --query 'PolicyNames[]' --output text); do
        aws iam delete-user-policy --user-name "$IAM_USER" --policy-name "$p"
    done
    aws iam delete-user --user-name "$IAM_USER" && echo "  deleted"
else echo "  absent"; fi
# The keyfile goes only once the user it belongs to is confirmed gone:
# a key on disk is the only way left to empty the bucket if the admin
# side fails halfway.
if [ -n "${KEYFILE:-}" ] && [ -f "$KEYFILE" ]; then
    if aws iam get-user --user-name "$IAM_USER" 2>&1 | grep -q NoSuchEntity; then
        rm -P "$KEYFILE" 2>/dev/null || shred -u "$KEYFILE" 2>/dev/null || rm -f "$KEYFILE"
        echo "  keyfile removed"
    else
        echo "  keyfile KEPT: the user is not confirmed gone"
    fi
fi

echo "── verify (instances take ~5 min to terminate) ──"
t0=$(date +%s)
while :; do
    live=$(aws ec2 describe-instances --filters Name=instance-state-name,Values=pending,running,stopping,stopped,shutting-down \
        --query 'length(Reservations[].Instances[])' --output text)
    [ "$live" = 0 ] && break
    [ $(( $(date +%s) - t0 )) -gt 720 ] && { echo "  STILL $live non-terminated instance(s) after 12 min"; break; }
    sleep 20
done
printf '  %-28s %s\n' "non-terminated instances" "$live"
printf '  %-28s %s\n' "volumes"      "$(aws ec2 describe-volumes --query 'length(Volumes)' --output text)"
printf '  %-28s %s\n' "spot requests open/active" "$(aws ec2 describe-spot-instance-requests --filters Name=state,Values=open,active --query 'length(SpotInstanceRequests)' --output text)"
printf '  %-28s %s\n' "elastic IPs"  "$(aws ec2 describe-addresses --query 'length(Addresses)' --output text)"
printf '  %-28s %s\n' "SGs trove-$CLUSTER*" "$(aws ec2 describe-security-groups --filters "Name=group-name,Values=trove-$CLUSTER*" --query 'length(SecurityGroups)' --output text)"
printf '  %-28s %s\n' "owned snapshots" "$(aws ec2 describe-snapshots --owner-ids self --query 'length(Snapshots)' --output text)"
printf '  %-28s %s\n' "bucket present" "$(aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1 && echo YES || echo no)"
printf '  %-28s %s\n' "iam user present" "$(aws iam get-user --user-name "$IAM_USER" >/dev/null 2>&1 && echo YES || echo no)"
printf '  %-28s %s\n' "trove orphans" "$(api GET /aws/orphans | jq -c '{matched, ghostRows, purgedRows}' 2>/dev/null)"
