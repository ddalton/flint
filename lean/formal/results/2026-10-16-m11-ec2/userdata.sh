#!/bin/bash
# M11 + M12 proofs and the AK1 part 2 TLC candidates on EC2 2026-10-16: c7i.48xlarge
# (x86: tlapm) spot, us-west-1.  Hard cap: shutdown in 180 min, and the instance
# terminates on shutdown.
shutdown -h +180
sysctl -w vm.max_map_count=1048576
export HOME=/root
apt-get update -y && apt-get install -y build-essential pkg-config python3 time
mkdir -p /work && cd /work
snap install aws-cli --classic
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
cargo install tla-mc --version 0.1.2 --locked > /work/install.log 2>&1
ln -sf /root/.cargo/bin/tla-mc /usr/local/bin/tla-mc
BK=flint-tlc-m11-20261016
aws s3 cp s3://$BK/in/ /work/ --recursive
mkdir -p /work/formal /work/proof /work/tlaps && tar -xzf /work/formal.tgz -C /work/formal && tar -xzf /work/proof.tgz -C /work/proof && tar -xzf /work/tlaps.tgz -C /work/tlaps
mkdir -p /work/out; { cp /work/install.log /work/out/; df -h /work; lsblk; nproc; free -g; ls /work/tlaps/tlapm/bin; } > /work/out/setup.txt 2>&1
aws s3 sync /work/out s3://$BK/out
bash /work/run.sh $BK > /work/run.log 2>&1
