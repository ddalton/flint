#!/bin/bash
# E20 on EC2 (c8g.48xlarge, us-west-1: the only region with a 192-vCPU spot
# quota, and it offers no c8gd): /work on the 2 TB gp3 root (1000 MB/s, 16k IOPS).
shutdown -h +260
sysctl -w vm.max_map_count=1048576
export HOME=/root
apt-get update -y && apt-get install -y build-essential pkg-config
mkdir -p /work && cd /work
snap install aws-cli --classic
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
cargo install tla-mc --version 0.1.2 --locked > /work/install.log 2>&1
ln -sf /root/.cargo/bin/tla-mc /usr/local/bin/tla-mc
BK=flint-tlc-m5-20261013
aws s3 cp s3://$BK/in/ /work/ --recursive
mkdir -p /work/out; { cp /work/install.log /work/out/; df -h /work; lsblk; } > /work/out/setup.txt 2>&1
aws s3 sync /work/out s3://$BK/out
bash /work/runner.sh $BK > /work/runner.log 2>&1
