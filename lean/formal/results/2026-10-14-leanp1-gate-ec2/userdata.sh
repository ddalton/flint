#!/bin/bash
# The LeanP1 gate on EC2 2026-10-14: c8g.48xlarge spot, us-west-1 (the only
# region with a 192-vCPU spot quota).  Hard cap: shutdown in 180 min, and the
# instance terminates on shutdown.
shutdown -h +180
sysctl -w vm.max_map_count=1048576
export HOME=/root
apt-get update -y && apt-get install -y build-essential pkg-config python3
mkdir -p /work && cd /work
snap install aws-cli --classic
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
cargo install tla-mc --version 0.1.2 --locked > /work/install.log 2>&1
ln -sf /root/.cargo/bin/tla-mc /usr/local/bin/tla-mc
BK=flint-tlc-gate-20261014
aws s3 cp s3://$BK/in/ /work/ --recursive
mkdir -p /work/formal && tar -xzf /work/formal.tgz -C /work/formal
mkdir -p /work/out; { cp /work/install.log /work/out/; df -h /work; lsblk; nproc; free -g; } > /work/out/setup.txt 2>&1
aws s3 sync /work/out s3://$BK/out
bash /work/runner.sh $BK > /work/runner.log 2>&1
