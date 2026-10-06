#!/bin/bash
shutdown -h +240
exec > /var/log/flint-userdata.log 2>&1
export AWS_DEFAULT_REGION=us-west-1 HOME=/root
dnf -y install gcc tar gzip python3
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
mkdir -p /data /opt/flint && aws s3 cp s3://flint-tlc-rewind-20261005/payload/ /opt/payload/ --recursive && tar -xzf /opt/payload/tree.tgz -C /opt/flint && cp /opt/payload/jobs.py /opt/payload/runner.sh /opt/flint/
nohup bash /opt/flint/runner.sh > /var/log/flint-runner.log 2>&1 &
