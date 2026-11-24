#!/bin/bash
# launch2.sh: c33's second launch (21:22Z).  launch.sh made the bucket (us-west-1), the inline policy and uploaded in/,
# then run-instances in us-west-1 was refused MaxSpotInstanceCountExceeded (the region's 192-vCPU spot quota is all
# c31's).  This: re-upload in/run.sh (fpmem 4096) and userdata, check every in/ file in S3 at its size, then one
# m7i.16xlarge one-time spot instance in us-east-1f (64 vCPU quota there), terminate on shutdown, 300 GB gp3.
set -euo pipefail
export AWS_PROFILE=trove-admin
D=$(cd "$(dirname "$0")" && pwd); BK=flint-c33-20261031; ROLE=TroveSSMInstanceProfile
AMI=ami-0d6dd9e8265d48906          # Ubuntu 24.04 amd64 gp3, us-east-1 (SSM parameter, 10-31)
AZ=${AZ:-us-east-1f}               # m7i.16xlarge spot $0.6265/h there at 21:30Z
aws s3 cp $D/in/run.sh s3://$BK/in/run.sh --region us-west-1
sed "s/__BUCKET__/$BK/" $D/userdata.sh > $D/userdata.final.sh
for f in $D/in/*; do k=in/$(basename $f); n=$(aws s3api head-object --region us-west-1 --bucket $BK --key $k --query ContentLength --output text 2>/dev/null || echo 0)
  [ "$n" = "$(stat -f %z $f)" ] || { echo "MISSING OR SHORT in S3: $k ($n bytes); not launching"; exit 1; }; done
[ "$(aws s3 cp s3://$BK/in/run.sh - --region us-west-1 | md5 -q)" = "$(md5 -q $D/in/run.sh)" ] || { echo "run.sh in S3 differs; not launching"; exit 1; }
OUT=$(aws ec2 run-instances --region us-east-1 --image-id $AMI --instance-type m7i.16xlarge \
  --placement AvailabilityZone=$AZ \
  --instance-market-options 'MarketType=spot,SpotOptions={SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}' \
  --instance-initiated-shutdown-behavior terminate \
  --iam-instance-profile Name=$ROLE \
  --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":300,"VolumeType":"gp3","DeleteOnTermination":true}}]' \
  --user-data file://$D/userdata.final.sh \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=flint-c33}]' 'ResourceType=volume,Tags=[{Key=Name,Value=flint-c33}]' \
  --query 'Instances[0].[InstanceId,SpotInstanceRequestId,LaunchTime]' --output text)
echo "bucket $BK instance+spot+launch $OUT az $AZ region us-east-1" | tee $D/launch.out
