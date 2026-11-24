#!/bin/bash
# c33 on EC2 2026-10-31: m7i.16xlarge spot (x86: tlapm), us-east-1f, 300 GB gp3 root; the bucket is in us-west-1.  Hard cap: shutdown in 480 min,
# and the instance terminates on shutdown.  Installs tla-mc 0.2.1 (--locked; the compiled checkers need cargo), pulls
# in/ from the bucket, runs in/run.sh (ONE script).  BK is filled in by launch.sh.
shutdown -h +480
sysctl -w vm.max_map_count=1048576
export HOME=/root AWS_DEFAULT_REGION=us-west-1
apt-get update -y && apt-get install -y build-essential pkg-config python3 time
mkdir -p /work/out /work/formal && cd /work
# (10-30: `snap install aws-cli` failed at early boot; the AWS CLI v2 installer instead)
apt-get install -y unzip curl && (cd /tmp && curl -sSo awscliv2.zip https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip && unzip -q awscliv2.zip && ./aws/install)
command -v aws || { echo "NO AWS CLI" > /work/out/FATAL; shutdown -h +5; exit 1; }
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
cargo install tla-mc --version 0.2.1 --locked > /work/out/install.log 2>&1
BK=__BUCKET__
aws s3 cp s3://$BK/in/ /work/ --recursive
tar -xzf /work/formal.tgz -C /work/formal
{ date -u +%FT%TZ; nproc; free -g; df -h /work; lsblk; /root/.cargo/bin/tla-mc -version; rustc -V; cargo -V; ldd --version | head -1; } > /work/out/setup.txt 2>&1
aws s3 sync /work/out s3://$BK/out
bash /work/run.sh $BK > /work/out/run.log 2>&1
