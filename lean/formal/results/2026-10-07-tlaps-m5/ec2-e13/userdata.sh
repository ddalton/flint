#!/bin/bash
shutdown -h +150
sysctl -w vm.max_map_count=1048576
export HOME=/root
mkdir -p /work && cd /work
snap install aws-cli --classic
apt-get update -y && apt-get install -y build-essential pkg-config
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
cargo install tla-mc --version 0.1.1 --locked > /work/install.log 2>&1
ln -sf /root/.cargo/bin/tla-mc /usr/local/bin/tla-mc
BK=flint-tlc-m5-20261012
aws s3 cp s3://$BK/in/ /work/ --recursive
mkdir -p /work/out; cp /work/install.log /work/out/
aws s3 cp /work/install.log s3://$BK/out/install.log
bash /work/runner.sh $BK > /work/runner.log 2>&1
