#!/bin/bash
# launch.sh: c27 on EC2 (run ONLY on the user's go).  AWS_PROFILE=trove-admin, us-west-1, as the 10-15/16 runs:
# a fresh bucket, an inline policy for it on role TroveSSMInstanceProfile, in/ uploaded, one m7i.48xlarge
# one-time spot instance, terminate on shutdown, 2 TB gp3 (16k IOPS, 1000 MB/s) DeleteOnTermination.
# Writes launch.out (bucket, instance id, spot request id) for teardown.sh.
set -euo pipefail
export AWS_PROFILE=trove-admin AWS_REGION=us-west-1
D=$(cd "$(dirname "$0")" && pwd); BK=flint-c27-20261030; ROLE=TroveSSMInstanceProfile
AMI=ami-03f6db1ed30590f8a          # Ubuntu 24.04 amd64 gp3, us-west-1 (SSM parameter, 10-30)
AZ=${AZ:-us-west-1a}               # m7i.48xlarge spot was cheapest there at 18:10Z
aws s3 mb s3://$BK --region us-west-1
aws iam put-role-policy --role-name $ROLE --policy-name $BK-bucket --policy-document "{
  \"Version\": \"2012-10-17\", \"Statement\": [
    {\"Effect\": \"Allow\", \"Action\": [\"s3:GetObject\", \"s3:PutObject\", \"s3:DeleteObject\"], \"Resource\": \"arn:aws:s3:::$BK/*\"},
    {\"Effect\": \"Allow\", \"Action\": [\"s3:ListBucket\"], \"Resource\": \"arn:aws:s3:::$BK\"}]}"
aws s3 cp $D/in/ s3://$BK/in/ --recursive
sed "s/__BUCKET__/$BK/" $D/userdata.sh > $D/userdata.final.sh
# (10-30: the Mac uplink dropped a 46 MB upload with no error) every in/ file must be in the bucket at its size
for f in $D/in/*; do k=in/$(basename $f); n=$(aws s3api head-object --bucket $BK --key $k --query ContentLength --output text 2>/dev/null || echo 0)
  [ "$n" = "$(stat -f %z $f)" ] || { echo "MISSING OR SHORT in S3: $k ($n bytes); not launching"; exit 1; }; done
sleep 10   # the inline policy propagates
OUT=$(aws ec2 run-instances --image-id $AMI --instance-type m7i.48xlarge \
  --placement AvailabilityZone=$AZ \
  --instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}' \
  --instance-initiated-shutdown-behavior terminate \
  --iam-instance-profile Name=$ROLE \
  --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":2000,"VolumeType":"gp3","Iops":16000,"Throughput":1000,"DeleteOnTermination":true}}]' \
  --user-data file://$D/userdata.final.sh \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=flint-c27}]' 'ResourceType=volume,Tags=[{Key=Name,Value=flint-c27}]' \
  --query 'Instances[0].[InstanceId,SpotInstanceRequestId,LaunchTime]' --output text)
echo "bucket $BK instance+spot+launch $OUT az $AZ" | tee $D/launch.out
