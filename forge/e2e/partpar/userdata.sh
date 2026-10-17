#!/bin/bash
# EC2 userdata for the part-parallelism measurement (run.sh). Spot, us-west-1,
# Ubuntu 24.04; the instance terminates on shutdown, and shuts down in 150
# minutes whatever happens. BK and TAG are substituted at launch.
shutdown -h +150
export HOME=/root
BK=__BUCKET__
TAG=__TAG__
apt-get update -y && apt-get install -y time
snap install aws-cli --classic
# The instance store: the NVMe disk with no partitions and nothing mounted.
DEV=$(lsblk -dpno NAME,TYPE,MOUNTPOINT | awk '$2=="disk" && $3=="" {print $1}' | while read d; do
  [ -z "$(lsblk -no PARTTYPE "$d" | tr -d ' \n')" ] && [ "$(lsblk -no NAME "$d" | wc -l)" = 1 ] && echo "$d"; done | head -1)
mkfs.ext4 -q -F "$DEV" && mkdir -p /data && mount "$DEV" /data
aws s3 cp s3://$BK/in/upload_bench /data/upload_bench
aws s3 cp s3://$BK/in/run.sh /data/run.sh
bash /data/run.sh $BK $TAG > /data/run.stdout 2>&1
