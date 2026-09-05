#!/usr/bin/env bash
# Back the workers' emptyDirs with the local NVMe.
#
# A trove instance boots from the AMI's 8 GiB root, and that root is
# where every emptyDir lives (/var/lib/kubelet/pods) and where every
# container's writable layer lives. A forge repository's cache is an
# emptyDir (design §5, decision 9), so on a stock trove node the
# largest repository forge can hold is a few GiB — smaller than the
# restore this drill needs. The i4i.xlarge's 872 GB NVMe sits unused
# unless flint's SPDK tier claims it, which this rig never does.
#
# So, per worker, over SSM: format the NVMe, copy the kubelet's pod
# directory onto it, bind it over the original, and restart every pod on
# the node so that nothing keeps a view of the hidden directory. The
# root disk keeps images and system state; only pod volumes move.
#
# Guards, because the root disk is the other NVMe on this instance
# type and /dev/nvmeXn1 names are NOT stable across identical nodes
# (memory: runbo, 2026-08-14): the device is found by PCI address,
# refused below 100 GB, refused if it carries a signature that is not
# this script's own label, refused if mounted anywhere.
#
#   KUBECONFIG=/tmp/trove-aws-kc-<name> ./forge/e2e/scale/prep-nodes.sh
set -euo pipefail
REGION=${REGION:-us-west-1}
export AWS_PROFILE=${AWS_PROFILE:-rolesanywhere}
PCI=${PCI:-0000:00:1f.0}           # the data NVMe on i4i.*; 0000:00:04.0 is the 8 GiB root
MIN_BYTES=$((100 * 1000 * 1000 * 1000))

script=$(cat <<'EOS'
set -e
ctl=$(ls -d /sys/bus/pci/devices/__PCI__/nvme/nvme* 2>/dev/null | head -1)
[ -n "$ctl" ] || { echo "no nvme controller at __PCI__"; exit 3; }
dev=/dev/$(basename "$ctl")n1
[ -b "$dev" ] || { echo "no block device $dev"; exit 3; }
size=$(lsblk -bno SIZE "$dev" | head -1)
[ "$size" -gt __MIN__ ] || { echo "$dev is $size bytes, under 100 GB: refusing (is this the root disk?)"; exit 3; }
if findmnt -no SOURCE /var/lib/kubelet/pods >/dev/null 2>&1; then
  echo "already backed: $(findmnt -no SOURCE,FSTYPE,SIZE,AVAIL /var/lib/kubelet/pods)"; exit 0
fi
if lsblk -no MOUNTPOINT "$dev" | grep -q .; then echo "$dev is mounted elsewhere"; lsblk "$dev"; exit 3; fi
label=$(blkid -s LABEL -o value "$dev" 2>/dev/null || true)
if [ -n "$(blkid -o value "$dev" 2>/dev/null)" ] && [ "$label" != flint-scale ]; then
  echo "$dev carries a signature that is not ours: $(blkid "$dev")"; exit 3
fi
[ "$label" = flint-scale ] || mkfs.ext4 -F -q -L flint-scale "$dev"
mkdir -p /mnt/pods && mount "$dev" /mnt/pods
systemctl stop kubelet
# Projected/secret volumes are tmpfs mounts under the pod directory;
# copying THROUGH them would carry live tokens as plain files. Unmount
# them first (running containers keep their own mount-namespace views).
findmnt -rn -o TARGET | grep '^/var/lib/kubelet/pods/' | sort -r | xargs -r umount 2>/dev/null || true
cp -a /var/lib/kubelet/pods/. /mnt/pods/
mount --bind /mnt/pods /var/lib/kubelet/pods
systemctl start kubelet
echo "done: $(findmnt -no SOURCE,FSTYPE,SIZE,AVAIL /var/lib/kubelet/pods)"
EOS
)
script=${script//__PCI__/$PCI}
script=${script//__MIN__/$MIN_BYTES}

params=$(mktemp)
printf '%s\n' "$script" | jq -Rn '{commands: [inputs]}' > "$params"

workers=$(kubectl get nodes -o json | jq -r '.items[] | select(.metadata.labels["node-role.kubernetes.io/control-plane"] == null) | .metadata.name')
[ -n "$workers" ] || { echo "no worker nodes"; exit 2; }
for node in $workers; do
    ip=$(kubectl get node "$node" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
    iid=$(aws ec2 describe-instances --region "$REGION" \
        --filters "Name=private-ip-address,Values=$ip" "Name=instance-state-name,Values=running" \
        --query 'Reservations[0].Instances[0].InstanceId' --output text)
    [ -n "$iid" ] && [ "$iid" != None ] || { echo "$node ($ip): no running instance found"; exit 2; }
    echo "── $node = $iid ($ip) ──"
    cid=$(aws ssm send-command --region "$REGION" --instance-ids "$iid" \
        --document-name AWS-RunShellScript --comment "flint scale rig: NVMe under /var/lib/kubelet/pods" \
        --parameters "file://$params" --query Command.CommandId --output text)
    status=Pending
    for _ in $(seq 1 90); do
        status=$(aws ssm get-command-invocation --region "$REGION" --command-id "$cid" --instance-id "$iid" \
            --query Status --output text 2>/dev/null || echo Pending)
        case "$status" in Success|Failed|TimedOut|Cancelled) break;; esac
        sleep 2
    done
    aws ssm get-command-invocation --region "$REGION" --command-id "$cid" --instance-id "$iid" \
        --query '[StandardOutputContent,StandardErrorContent]' --output text | sed 's/^/    /'
    [ "$status" = Success ] || { echo "SSM on $node ended $status"; exit 1; }
    # Everything scheduled here before the move still sees the hidden
    # directory; recreate it so no pod keeps a stale view.
    kubectl delete pods -A --field-selector "spec.nodeName=$node" --wait=false >/dev/null 2>&1 || true
    sleep 5
    # Not `kubectl wait`: it keeps watching the pods it started with, and
    # once those are the deleted ones it sits past its own --timeout
    # (12 minutes on runbw). Poll the live set instead.
    for _ in $(seq 1 48); do
        notready=$(kubectl get pods -A --field-selector "spec.nodeName=$node" -o json 2>/dev/null \
            | jq '[.items[] | select(.metadata.deletionTimestamp == null)
                   | select(([.status.conditions[]? | select(.type=="Ready" and .status=="True")] | length) == 0)] | length')
        [ "${notready:-1}" = 0 ] && break
        sleep 5
    done
    [ "${notready:-1}" = 0 ] && echo "    pods on $node are back" || echo "    WARNING: $notready pod(s) on $node not Ready after 4 min"
done
rm -f "$params"
echo "workers prepared: $(echo "$workers" | tr '\n' ' ')"
