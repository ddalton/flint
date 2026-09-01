#!/bin/bash
set -u
export AWS_PROFILE=rolesanywhere
REMOTE='for i in $(seq 1 30); do R=$(curl -s -m 3 -XPOST localhost:9081/api/disks -d "{}" -H "Content-Type: application/json"); [ -n "$R" ] && break; sleep 5; done
echo "DISKS: $(echo "$R" | head -c 250)"
curl -s -m 90 -XPOST localhost:9081/api/disks/initialize_blobstore -d "{\"pci_address\":\"0000:00:1f.0\"}" -H "Content-Type: application/json" | head -c 250
echo'
B64=$(printf '%s' "$REMOTE" | base64 | tr -d '\n')
aws ec2 describe-instances \
  --filters Name=tag:Name,Values='trove/runbw/runbw-aws-*' Name=instance-state-name,Values=running \
  --query 'Reservations[].Instances[].[Tags[?Key==`Name`]|[0].Value,InstanceId]' --output text | sort |
while read -r name iid; do
  echo "=== $name ($iid)"
  cid=$(aws ssm send-command --instance-ids "$iid" --document-name AWS-RunShellScript \
        --parameters commands="echo $B64 | base64 -d | bash" \
        --query Command.CommandId --output text) || continue
  st=Pending
  for i in $(seq 1 80); do
    st=$(aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query Status --output text 2>/dev/null || echo Pending)
    case $st in Success|Failed|TimedOut|Cancelled) break;; *) sleep 3;; esac
  done
  echo "status: $st"
  aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query StandardOutputContent --output text | head -5
done
