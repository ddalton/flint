#!/bin/bash
# Wrap-up deadline (user: "try to wrap up in 50 mins"): stop the gate at 00:43Z.
while [ "$(date +%s)" -lt 1791074580 ]; do sleep 5; done
pkill -f "[l]eangate.py"; sleep 1; pkill -f "/data/gt/.*/release/tlcge[n]"
echo "STOPPED $(date -u +%FT%TZ): wrap-up deadline 00:43Z (user); the world that was running is in its .out" >> /data/out/RESULTS.txt
echo "GATEDONE (stopped)" >> /data/out/RESULTS.txt
aws s3 cp /data/out/ s3://flint-tlc-gate-20261004/out/ --recursive --quiet
