#!/bin/bash
# ssmrun.sh <instance-id> — remote script on stdin, b64-shipped (no shorthand
# parsing hazards), polls to completion, prints stdout then stderr tail.
set -u
export AWS_PROFILE=rolesanywhere
iid=$1
b64=$(base64 | tr -d '\n')
cid=$(aws ssm send-command --instance-ids "$iid" --document-name AWS-RunShellScript \
      --timeout-seconds 900 --parameters commands="echo $b64 | base64 -d | bash" \
      --query Command.CommandId --output text) || exit 1
st=Pending
for i in $(seq 1 150); do
  st=$(aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query Status --output text 2>/dev/null || echo Pending)
  case $st in Success|Failed|TimedOut|Cancelled) break;; *) sleep 4;; esac
done
echo "[ssmrun $iid: $st]"
aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query StandardOutputContent --output text
[ "$st" = Success ] || aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query StandardErrorContent --output text | tail -8
[ "$st" = Success ]
