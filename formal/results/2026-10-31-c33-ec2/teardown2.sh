#!/bin/bash
# teardown2.sh (the instance is in us-east-1, the bucket in us-west-1): pull the bucket's out/ -> terminate the instance -> cancel its spot request -> delete the inline
# policy -> remove the bucket -> verify the zero set (no flint-c33 instance or volume, no open spot request) in
# us-east-1, us-east-2, us-west-1, us-west-2.  Safe to run twice.
export AWS_PROFILE=trove-admin
D=$(cd "$(dirname "$0")" && pwd); BK=flint-c33-20261031; ROLE=TroveSSMInstanceProfile
read -r _ _ _ IID SIR _ < <(sed 's/\t/ /g' $D/launch.out) || true
mkdir -p $D/out && aws s3 sync s3://$BK/out $D/out --region us-west-1 --quiet || echo "WARN: no out/ to pull"
[ -n "${IID:-}" ] && aws ec2 terminate-instances --region us-east-1 --instance-ids $IID --query 'TerminatingInstances[0].CurrentState.Name' --output text
[ -n "${SIR:-}" ] && aws ec2 cancel-spot-instance-requests --region us-east-1 --spot-instance-request-ids $SIR --output text >/dev/null 2>&1
aws iam delete-role-policy --role-name $ROLE --policy-name $BK-bucket 2>/dev/null && echo "policy $BK-bucket deleted"
aws s3 rb s3://$BK --force --region us-west-1 2>/dev/null && echo "bucket $BK removed"
sleep 60
echo "ZERO SET CHECK $(date -u +%FT%TZ)"
for r in us-east-1 us-east-2 us-west-1 us-west-2; do
  i=$(aws ec2 describe-instances --region $r --filters Name=tag:Name,Values=flint-c33 Name=instance-state-name,Values=pending,running,stopping,stopped,shutting-down --query 'length(Reservations[].Instances[])' --output text)
  v=$(aws ec2 describe-volumes --region $r --filters Name=tag:Name,Values=flint-c33 --query 'length(Volumes)' --output text)
  s=$(aws ec2 describe-spot-instance-requests --region $r --filters Name=state,Values=open,active --query 'length(SpotInstanceRequests)' --output text)
  echo "$r instances=$i volumes=$v open-spot-requests(all)=$s"
done
aws iam list-role-policies --role-name $ROLE --query 'PolicyNames' --output text | tr '\t' '\n' | grep -c "$BK" | sed 's/^/inline policies for the bucket: /'
aws s3 ls s3://$BK --region us-west-1 >/dev/null 2>&1 && echo "BUCKET STILL EXISTS" || echo "bucket gone"
