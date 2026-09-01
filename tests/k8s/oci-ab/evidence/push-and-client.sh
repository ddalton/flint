#!/bin/bash
set -eu
export KUBECONFIG=/tmp/trove-aws-kc-runbw
export AWS_PROFILE=rolesanywhere
AB=/Users/ddalton/github/flint/tests/k8s/oci-ab
IMG=python:3.12

rf=$(kubectl get svc registry-flint -o jsonpath='{.spec.clusterIP}'):5000
rs=$(kubectl get svc registry-s3 -o jsonpath='{.spec.clusterIP}'):5000
cn=$(kubectl get node -l oci-ab/role=client -o jsonpath='{.items[0].metadata.name}')
# no cloud-controller-manager => no providerID; map via the EC2 Name tag
iid=$(aws ec2 describe-instances \
  --filters "Name=tag:Name,Values=trove/runbw/$cn" Name=instance-state-name,Values=running \
  --query 'Reservations[0].Instances[0].InstanceId' --output text)
echo "client: $cn ($iid) rf=$rf rs=$rs"
[ -n "$iid" ] && [ "$iid" != "None" ] || { echo "no instance id"; exit 1; }

ssm() { # $1 = command string; prints stdout, fails loudly
  local cid st
  cid=$(aws ssm send-command --instance-ids "$iid" --document-name AWS-RunShellScript \
        --timeout-seconds 900 --parameters commands="$1" \
        --query Command.CommandId --output text)
  st=Pending
  for i in $(seq 1 150); do
    st=$(aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query Status --output text 2>/dev/null || echo Pending)
    case $st in Success|Failed|TimedOut|Cancelled) break;; *) sleep 5;; esac
  done
  echo "--- ssm status: $st"
  aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query StandardOutputContent --output text
  if [ "$st" != Success ]; then
    aws ssm get-command-invocation --command-id "$cid" --instance-id "$iid" --query StandardErrorContent --output text | tail -10
    return 1
  fi
}

echo "== node setup (soci + nerdctl + containerd wiring) =="
b64=$(base64 < "$AB/node-soci-setup.sh" | tr -d '\n')
ssm "echo $b64 | base64 -d > /tmp/nss.sh && sudo REG_FLINT=$rf REG_S3=$rs bash /tmp/nss.sh" | tail -6

echo "== pull from Docker Hub on the node; push to BOTH registries =="
ssm "sudo nerdctl pull --quiet $IMG && sudo nerdctl tag $IMG $rf/$IMG && sudo nerdctl tag $IMG $rs/$IMG && sudo nerdctl --hosts-dir /etc/containerd/certs.d push $rf/$IMG >/dev/null && sudo nerdctl --hosts-dir /etc/containerd/certs.d push $rs/$IMG >/dev/null && echo PUSHED && sudo nerdctl image ls | grep python | head -3" | tail -5

echo "== digest identity across registries (G4) =="
ssm "for r in $rf $rs; do curl -s http://\$r/v2/python/manifests/3.12 -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json' -o /dev/null -w \"\$r %{header_docker-content-digest}\n\" 2>/dev/null || curl -sI http://\$r/v2/python/manifests/3.12 -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json' | grep -i docker-content-digest | sed \"s|^|\$r |\"; done"

echo "== SOCI index: build once, push to both =="
ssm "sudo soci create $rf/$IMG && sudo soci push --user '' $rf/$IMG 2>/dev/null || sudo soci push $rf/$IMG; sudo soci push $rs/$IMG && echo SOCI-PUSHED" | tail -3
echo "push-and-client done"
