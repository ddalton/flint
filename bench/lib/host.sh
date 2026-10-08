# Sourced helpers for the driver scripts: run commands in a node's HOST
# namespaces through the bench samplers (hostPID + privileged, sampler.yaml),
# and find the node's instance-store NVMe.
BENCH_DIR="${BENCH_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
DISK_MODEL="${DISK_MODEL:-Amazon EC2 NVMe Instance Storage}"

step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

samplers_up() {
  kubectl apply -f "$BENCH_DIR/sampler.yaml" >/dev/null
  kubectl -n flint-bench rollout status ds/bench-sampler --timeout=300s >/dev/null
}

nodes() { kubectl get nodes -o jsonpath='{.items[*].metadata.name}'; }

sampler_on() {  # <node>
  kubectl -n flint-bench get pods -l app=bench-sampler \
    --field-selector "spec.nodeName=$1" -o jsonpath='{.items[0].metadata.name}'
}

hostexec() {  # <node> <shell command...>  -- runs in the host's mount/net/pid/ipc/uts namespaces
  local n=$1; shift
  kubectl -n flint-bench exec "$(sampler_on "$n")" -- nsenter -t 1 -m -u -n -i -p -- sh -c "$*"
}

# The instance store, as the kernel sees it NOW: "<dev> <pci-bdf> <by-id-path>"
# (empty when no kernel block device carries the model, e.g. while a
# userspace NVMe driver holds the controller).
instance_store() {  # <node>
  hostexec "$1" '
    for b in /sys/block/nvme*n1; do
      m=$(sed "s/ *$//" "$b/device/model" 2>/dev/null)
      [ "$m" = "'"$DISK_MODEL"'" ] || continue
      d=$(basename "$b"); c=$(basename "$(readlink -f "$b/device")")
      bdf=$(cat "/sys/class/nvme/$c/address")
      id=$(ls /dev/disk/by-id/ | grep -E "^nvme-Amazon_EC2_NVMe_Instance_Storage_[^_]+$" | head -1)
      echo "/dev/$d $bdf /dev/disk/by-id/$id"
    done'
}

# The nodes' roots are 8 GiB and the competitors' images are 1-3 GiB each:
# on 2026-10-08 Longhorn's pull on top of Mayastor's leftovers put all three
# nodes into DiskPressure, the kubelet evicted the sampler mid-install, and
# the run died. Drop every image no container uses (ctr: the nodes have no
# crictl) and print what the root holds.
prune_images() {
  local n
  for n in $(nodes); do
    hostexec "$n" '
      used=$(ctr -n k8s.io containers ls 2>/dev/null | awk "NR>1{print \$2}" | sort -u); k=0
      for img in $(ctr -n k8s.io images ls -q 2>/dev/null); do
        echo "$used" | grep -qx "$img" && continue
        case "$img" in *pause*) continue ;; esac
        ctr -n k8s.io images rm --sync "$img" >/dev/null 2>&1 && k=$((k+1))
      done
      echo "'"$n"': pruned $k image refs; root: $(df -h / | tail -1)"' >&2
  done
}

# helm with private cache/config/data dirs: on the Mac the default dirs are
# root-owned and `helm repo add` / OCI pulls fail there.
helm_private() {
  local h="${HELM_PRIVATE_HOME:-${TMPDIR:-/tmp}/bench-helm}"
  mkdir -p "$h"/{cache,config,data}
  HELM_CACHE_HOME="$h/cache" HELM_CONFIG_HOME="$h/config" HELM_DATA_HOME="$h/data" helm "$@"
}

# Refuse to install a driver onto a disk the last one left dirty.
require_clean_disks() {
  local n dev bdf byid sig
  for n in $(nodes); do
    read -r dev bdf byid <<<"$(instance_store "$n")"
    [ -n "${dev:-}" ] || fail "$n: no kernel block device for the instance store (run wipe-disks.sh)"
    sig=$(hostexec "$n" "wipefs -n $dev 2>/dev/null | tail -n +2")
    [ -z "$sig" ] || fail "$n: $dev carries signatures (run wipe-disks.sh): $sig"
  done
}
