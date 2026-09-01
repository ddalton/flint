#!/bin/bash
# rung-1 pilot (oci-image-serving-design.md §9.3) — host driver.
# Stages payloads into the shared lima VM (flint-nfs-client) and drives
# tests/lima/oci-pilot/pilot-vm.sh. Shared-VM rules: nothing outside
# /var/tmp/oci-pilot in the VM, private port 22049, no reboot/resize.
set -eu

VM=flint-nfs-client
BASE=/var/tmp/oci-pilot
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
SCRATCH="${PILOT_SCRATCH:?set PILOT_SCRATCH to a host scratch dir}"
IMAGE=python:3.12
BIN="$REPO/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/flint-nfs-server"

vsh() { limactl shell "$VM" -- bash -c "$*"; }

case "${1:-}" in
stage)
  mkdir -p "$SCRATCH"
  echo "[host] pulling $IMAGE (linux/arm64) and exporting a flattened rootfs tar" >&2
  docker pull --platform linux/arm64 "$IMAGE" >&2
  cid=$(docker create --platform linux/arm64 "$IMAGE")
  docker export "$cid" | gzip -6 > "$SCRATCH/image.tar.gz"
  docker rm "$cid" >&2
  ls -la "$SCRATCH/image.tar.gz" >&2
  vsh "mkdir -p $BASE/stage"
  limactl copy "$SCRATCH/image.tar.gz" "$VM:$BASE/stage/image.tar.gz"
  limactl copy "$BIN" "$VM:$BASE/flint-nfs-server"
  limactl copy "$HERE/pilot-vm.sh" "$VM:$BASE/pilot-vm.sh"
  vsh "chmod +x $BASE/flint-nfs-server $BASE/pilot-vm.sh"
  echo "[host] staged" >&2
  ;;
prep)
  vsh "command -v mkfs.erofs >/dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y erofs-utils" >&2
  vsh "bash $BASE/pilot-vm.sh prep"
  ;;
run)
  out="$HERE/results-$(date +%Y%m%d-%H%M%S).json"
  vsh "REPS=${REPS:-5} bash $BASE/pilot-vm.sh run" > "$out"
  echo "[host] results: $out" >&2
  python3 - "$out" <<'PYEOF'
import json, sys, statistics as st
d = json.load(open(sys.argv[1]))
reps = [r for r in d["reps"] if not r.get("failed")]
def med(arm, k):
    v = [r[k] for r in reps if r["arm"] == arm and k in r]
    return st.median(v) if v else None
p1, p2, p3 = med("P1","ready_ms"), med("P2","ready_ms"), med("P3","ready_ms")
# G1: faults went remote — ops consistent with bytes at <=1MiB rsize, and a
# working-set floor of 15MB (EROFS reads are lz4-COMPRESSED bytes; the first
# scoring used 20MB from an uncompressed model and failed a 19.2MB truth).
g1 = all(r["nfs_read_ops"] >= r["nfs_read_bytes"] / 2**20 and
         r["nfs_read_bytes"] >= 15*2**20
         for r in reps if r["arm"] == "P3")
# G2: falsifiability — a warm rerun must show ~zero NEW remote READs and be
# strictly faster. (No 5x total-ready collapse: ready includes the fixed
# losetup+mount cost, which never collapses.)
g2 = all(r["warm_read_ops"] < 8 and r["warm_ready_ms"] < r["ready_ms"]
         for r in reps if r["arm"] == "P3")
g3 = d["guards"][0]["g3_eio_count"] == 0
g4 = d["guards"][1]["g4_ok"]
fio_nfs_p99 = {f["bs"]: f["p99_us"] for f in d["fio"] if f["backing"] == "nfs"}
fault_ok = all(v < 5000 for v in fio_nfs_p99.values())
ratio = (p3/p2) if (p2 and p3) else None
verdict = {
  "medians_ms": {"P1": p1, "P2": p2, "P3": p3},
  "p3_over_p2": round(ratio,2) if ratio else None,
  "format_win_p1_over_p2": round(p1/p2,2) if (p1 and p2) else None,
  "fio_nfs_p99_us": fio_nfs_p99,
  "criteria": {
    "p3_fault_p99_lt_5ms": fault_ok,
    "p3_ready_le_1.5x_p2": bool(ratio and ratio <= 1.5),
    "g1_faults_went_remote": g1, "g2_falsifiability": g2,
    "g3_zero_eio": g3, "g4_digest_identity": g4,
  },
}
verdict["pass"] = all(verdict["criteria"].values())
print(json.dumps(verdict, indent=2))
PYEOF
  ;;
clean)
  vsh "bash $BASE/pilot-vm.sh clean" || true
  ;;
*) echo "usage: $0 stage|prep|run|clean" >&2; exit 2 ;;
esac
