#!/bin/bash
# ssmrun.sh '<shell>' — run on the drill instance, print stdout
export AWS_PROFILE=trove-admin AWS_REGION=us-west-1
CID=$(aws ssm send-command --instance-ids i-021a7ca627ca0c38d --document-name AWS-RunShellScript --parameters "$(python3 -c 'import json,sys;print(json.dumps({"commands":[sys.argv[1]]}))' "$1")" --query Command.CommandId --output text) || exit 1
for i in $(seq 1 60); do st=$(aws ssm get-command-invocation --command-id $CID --instance-id i-021a7ca627ca0c38d --query Status --output text 2>/dev/null); case $st in Success|Failed|TimedOut|Cancelled) break;; esac; sleep 3; done
aws ssm get-command-invocation --command-id $CID --instance-id i-021a7ca627ca0c38d --query '[StandardOutputContent,StandardErrorContent]' --output text
