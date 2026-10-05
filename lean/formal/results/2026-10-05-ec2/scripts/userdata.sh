#!/bin/bash
shutdown -h +600
exec > /var/log/flint-userdata.log 2>&1
export BUCKET=flint-tlc-run-20261005 AWS_DEFAULT_REGION=us-west-1 HOME=/root
dnf -y install gcc tar gzip python3
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
mkdir -p /data /opt/flint && aws s3 cp s3://flint-tlc-run-20261005/payload/payload.tgz /opt/payload.tgz && tar -xzf /opt/payload.tgz -C /opt/flint
nohup bash /opt/flint/runner.sh > /var/log/flint-runner.log 2>&1 &
