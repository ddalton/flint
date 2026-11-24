#!/bin/bash
# c27 on EC2 2026-10-30: m7i.48xlarge spot (x86: tlapm), us-west-1, 2 TB gp3 root.  Hard cap: shutdown in
# 720 min, and the instance terminates on shutdown.  Installs tla-mc 0.2.1 (compiled checkers need cargo),
# pulls in/ from the bucket, runs in/run.sh (ONE script).  BK is filled in by launch.sh.
shutdown -h +720
sysctl -w vm.max_map_count=1048576
export HOME=/root
apt-get update -y && apt-get install -y build-essential pkg-config python3 time
mkdir -p /work/out /work/formal /work/proof /work/tlaps && cd /work
snap install aws-cli --classic
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
cargo install tla-mc --version 0.2.1 --locked > /work/out/install.log 2>&1
BK=flint-c27-20261030
aws s3 cp s3://$BK/in/ /work/ --recursive
tar -xzf /work/formal.tgz -C /work/formal && tar -xzf /work/proof.tgz -C /work/proof && tar -xzf /work/tlaps.tgz -C /work/tlaps
{ date -u +%FT%TZ; nproc; free -g; df -h /work; lsblk; /root/.cargo/bin/tla-mc -version; ls /work/tlaps/tlapm/bin; } > /work/out/setup.txt 2>&1
aws s3 sync /work/out s3://$BK/out
bash /work/run.sh $BK > /work/out/run.log 2>&1
