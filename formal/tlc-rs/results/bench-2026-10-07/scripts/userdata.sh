#!/bin/bash
shutdown -h +180
sysctl -w vm.max_map_count=16777216
exec > /var/log/flint-userdata.log 2>&1
export HOME=/root AWS_DEFAULT_REGION=us-west-1
dnf -y install gcc tar gzip python3
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
mkdir -p /data /opt/flint && aws s3 cp s3://flint-tlc-bench-20261007/payload/ /opt/ --recursive && tar -xzf /opt/tree.tgz -C /opt/flint
systemd-run --unit=flint-bench --property=KillMode=process bash /opt/runner.sh
