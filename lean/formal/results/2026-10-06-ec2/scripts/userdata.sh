#!/bin/bash
shutdown -h +420
exec > /var/log/flint-userdata.log 2>&1
export HOME=/root AWS_DEFAULT_REGION=us-west-1
dnf -y install gcc tar gzip python3
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
mkdir -p /data /opt/flint && cd /opt && aws s3 cp s3://flint-tlc-lean-20261006/payload/ /opt/ --recursive && tar -xzf /opt/tree.tgz -C /opt/flint
nohup bash /opt/runner.sh > /var/log/flint-runner.log 2>&1 &
