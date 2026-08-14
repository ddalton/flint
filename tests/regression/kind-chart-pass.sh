#!/usr/bin/env bash
# kind chart pass — the pnfs-block chart surface, validated against a REAL
# Kubernetes API server.
#
# WHAT THIS TIER CAN AND CANNOT PROVE, stated first because the limit is
# the reason the script is shaped this way:
#
#   * The Docker VM kernel is 5.10-linuxkit, far below the pnfs-block
#     client floor (6.11 + CONFIG_PNFS_BLOCK). No kind node can ever
#     stage a block volume, so there is no data path to test here.
#   * The kernel-floor REFUSAL is already proven on real hardware — a
#     stock Ubuntu 24.04 VM (6.8) refused before touching the endpoint,
#     and sailed past the same gate with FLINT_PNFS_BLOCK_KERNEL_OVERRIDE=1
#     (design §11). Re-staging that on kind would prove nothing new.
#   * What kind DOES add, and what nothing else covers: the chart's
#     pnfs-block surface is accepted by a real API server. `helm template`
#     only proves the templates produce text; a server-side dry-run
#     proves Kubernetes agrees the objects are legal — field names, enum
#     values, API versions, required fields — without pulling a single
#     image or running a single flint pod.
#
# So: this is a CHART pass, not a data-plane pass. It needs ~1GB of
# Docker memory and no flint images.
#
# Usage:  tests/regression/kind-chart-pass.sh
#         KEEP=1 tests/regression/kind-chart-pass.sh   # leave the cluster up
set -uo pipefail

CLUSTER="${CLUSTER:-flint-chart-pass}"
CHART="$(cd "$(dirname "$0")/../.." && pwd)/flint-csi-driver-chart"
NS=flint-system
KUBECONFIG_FILE="$(mktemp -t flint-kind-kubeconfig.XXXXXX)"
export KUBECONFIG="$KUBECONFIG_FILE"

pass() { echo "✓ $*"; }
fail() { echo "✗ $*"; exit 1; }

cleanup() {
  if [ "${KEEP:-0}" != "1" ]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    rm -f "$KUBECONFIG_FILE"
  else
    echo "· cluster kept: kind get kubeconfig --name $CLUSTER"
  fi
}
trap cleanup EXIT

# ── 0. preflight ─────────────────────────────────────────────────────
for t in kind kubectl helm docker; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"
KVER=$(docker info --format '{{.KernelVersion}}' 2>/dev/null)
echo "· docker VM kernel: $KVER (pnfs-block floor is 6.11 — negative path only, by design)"

# ── 1. a one-node cluster; no images, no workloads ───────────────────
if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1 \
    || fail "kind create cluster failed (docker memory? $(docker info --format '{{.MemTotal}}' 2>/dev/null) bytes)"
fi
kind export kubeconfig --name "$CLUSTER" >/dev/null 2>&1 || fail "kind export kubeconfig"
kubectl create namespace "$NS" >/dev/null 2>&1 || true
SRV=$(kubectl version -o json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["serverVersion"]["gitVersion"])' 2>/dev/null)
pass "kind cluster up ($SRV)"

# ── helpers ──────────────────────────────────────────────────────────
# Render, then hand the result to the REAL API server. --dry-run=server
# runs admission and full schema validation; it is the whole point of
# doing this on kind rather than with `helm template` alone.

# CONTRACT: these set the global RENDER and return a status. They do NOT
# run inside `$( )` and they do NOT call `fail` — a `fail` inside command
# substitution writes its message into the VARIABLE and its `exit` only
# leaves the subshell, so the caller sails on with the error as its data.
# (That trap ate two runs of this very script, one turn after it was
# written down in lessons-learnt. Hence the contract rather than care.)
RENDER=""

# The chart ships a VolumeSnapshotClass, whose CRDs come from the
# external-snapshotter and are a genuine cluster PREREQUISITE (values.yaml
# says as much) — not something the chart installs. A bare kind cluster
# has no such CRD, so a server-side dry-run rightly rejects it. Turning it
# off here keeps the pass focused on the pnfs-block surface instead of
# re-litigating a documented dependency; the API server still validates
# every other object.
BASE=(--set snapshotClass.enabled=false)

# Render, then hand the result to the REAL API server. --dry-run=server
# runs admission and full schema validation; it is the whole point of
# doing this on kind rather than with `helm template` alone.
render_ok() {   # name, helm args… → RENDER set, 0 on success
  local name="$1"; shift
  local tmp rc st
  RENDER=$(helm template flint "$CHART" --namespace "$NS" "${BASE[@]}" "$@" 2>&1)
  st=$?
  if [ "$st" -ne 0 ]; then
    echo "✗ $name: helm template failed:" >&2
    printf '%s\n' "$RENDER" | tail -3 >&2
    return 1
  fi
  tmp=$(mktemp -t flint-chart.XXXXXX.yaml)
  printf '%s\n' "$RENDER" > "$tmp"
  rc=$(kubectl apply --dry-run=server -f "$tmp" 2>&1)
  st=$?
  rm -f "$tmp"
  if [ "$st" -ne 0 ]; then
    echo "✗ $name: the API server REFUSED the rendered chart:" >&2
    printf '%s\n' "$rc" | grep -iE 'error|invalid|unable' | head -5 >&2
    return 1
  fi
  return 0
}

render_must_fail() {  # name, helm args… → RENDER holds the error text
  local name="$1"; shift
  RENDER=$(helm template flint "$CHART" --namespace "$NS" "${BASE[@]}" "$@" 2>&1)
  if [ $? -eq 0 ]; then
    echo "✗ $name: the chart RENDERED when it should have refused — a required value is not required" >&2
    return 1
  fi
  return 0
}

has() { case "$2" in *"$1"*) return 0 ;; *) return 1 ;; esac; }

# ── 2. the baseline: pnfs off entirely ───────────────────────────────
render_ok "pnfs-off" || fail "baseline chart did not validate"
OUT="$RENDER"
has "flint-csi-node" "$OUT" || fail "baseline render has no csi-node DaemonSet"
has "FLINT_PNFS_BLOCK_LAYOUT" "$OUT" && fail "block-layout env leaked into a chart with pnfs disabled"
pass "baseline (pnfs off) renders and the API server accepts it"

# ── 3. pnfs on, block off — the common production shape ──────────────
render_ok "pnfs-on-block-off" --set pnfs.enabled=true --set pnfs.server.enabled=true \
  || fail "pnfs-on chart did not validate"
OUT="$RENDER"
has "flint-pnfs-mds" "$OUT" || fail "no MDS rendered with pnfs.server.enabled"
has "FLINT_PNFS_BLOCK_LAYOUT" "$OUT" && fail "block-layout env present without blockLayout.enabled"
has "blockExport:" "$OUT" && fail "blockExport config present without blockExport.enabled"
pass "pnfs on / block off: MDS present, block surface absent"

# ── 4. THE GUARDS: blockExport without its required values MUST refuse ─
BLOCK_ON=(--set pnfs.enabled=true --set pnfs.server.enabled=true
          --set pnfs.blockLayout.enabled=true
          --set pnfs.server.mds.blockExport.enabled=true)

render_must_fail "blockExport-no-lvstore" "${BLOCK_ON[@]}" \
  --set pnfs.server.mds.blockExport.traddr=10.0.0.9 || exit 1
ERR="$RENDER"
# Assert on the KEY, not on one phrasing: the refusal must tell an
# operator which value to set. The wording moved when blockExport
# became per-shard (it now names the shard and both places the value
# can live) and this leg rightly failed on the change.
has "lvstore" "$ERR" || fail "the missing-lvstore refusal does not name lvstore"
render_must_fail "blockExport-no-traddr" "${BLOCK_ON[@]}" \
  --set pnfs.server.mds.blockExport.lvstore=lvs_flint || exit 1
ERR="$RENDER"
has "traddr" "$ERR" || fail "the missing-traddr refusal does not name traddr"
pass "blockExport refuses to render without lvstore and traddr, and names each"

# ── 5. the full block surface, accepted by the API server ────────────
FULL=("${BLOCK_ON[@]}"
      --set pnfs.server.mds.blockExport.lvstore=lvs_flint
      --set pnfs.server.mds.blockExport.traddr=10.0.0.9
      --set pnfs.blockLayout.storageClass.create=true)
render_ok "block-full" "${FULL[@]}" || fail "the full block surface did not validate"
OUT="$RENDER"

# 5a. the StorageClass the class gate keys on.
has "layout: pnfs-block" "$OUT" || fail "no pnfs-block StorageClass rendered"
has "flint-pnfs-block" "$OUT"   || fail "the block SC has the wrong name"

# 5b. the controller must be told, or it refuses `layout: pnfs-block`
#     classes loudly at CreateVolume.
has "FLINT_PNFS_BLOCK_LAYOUT" "$OUT" || fail "controller not given FLINT_PNFS_BLOCK_LAYOUT"

# 5c. the MDS must report its own node — the roller's join key for
#     refusing to roll a tgt with live block initiators (design §11).
#     Absent, the roller falls back to matching the listener address
#     against Node objects; present, it is exact.
has "FLINT_NODE_NAME" "$OUT" || fail "MDS missing FLINT_NODE_NAME (the roller's export-node join key)"
has "spec.nodeName" "$OUT"   || fail "FLINT_NODE_NAME is not wired to the downward API"

# 5d. the PTPL directory: a hostPath that must OUTLIVE tgt restarts, and
#     deliberately not under /var/tmp (systemd-tmpfiles ages that out,
#     and an aged ptpl file is a silently weakened fence).
has "/var/lib/flint-pnfs-ptpl" "$OUT" || fail "the PTPL hostPath is not mounted"
has "path: /var/tmp/flint-pnfs-ptpl" "$OUT" \
  && fail "the PTPL dir is under /var/tmp — systemd-tmpfiles will age the fence out"

# 5e. the §4a udev rule: without the eui link the kernel client silently
#     degrades every I/O to MDS proxying, which looks like it works.
has "udev" "$OUT" || fail "no udev rule surface on the node DaemonSet"

# 5f. RBAC the roller needs: it resolves an export's listener address
#     against Node objects when an older MDS cannot name its own node.
has "nodes" "$OUT" || fail "RBAC does not grant nodes (the roller's fallback resolution)"
pass "full block surface renders, and the API server accepts every object"

# ── 5g. TWO BLOCK-EXPORT SHARDS, which is the whole point of replicas:2 ─
#
# A two-copy volume lives on two targets driven by two MDS shards, and
# blockExport is per NODE — lvols are node-local and lvstore names are
# minted per node. While the chart carried ONE shared MDS ConfigMap,
# `lvstore`/`traddr`/`nodeSelector` were single-valued, so this topology
# could not be expressed at all and the only way to run it was
# hand-written config outside the chart. Found on a real cluster.
TWO=("${BLOCK_ON[@]}"
     --set pnfs.server.mds.count=2
     --set pnfs.server.mds.blockExport.shards[0].lvstore=lvs_a
     --set pnfs.server.mds.blockExport.shards[0].traddr=10.0.0.1
     --set pnfs.server.mds.blockExport.shards[0].nodeSelector.kubernetes\\.io/hostname=node-a
     --set pnfs.server.mds.blockExport.shards[1].lvstore=lvs_b
     --set pnfs.server.mds.blockExport.shards[1].traddr=10.0.0.2
     --set pnfs.server.mds.blockExport.shards[1].nodeSelector.kubernetes\\.io/hostname=node-b)
render_ok "two-block-shards" "${TWO[@]}" || fail "the two-shard block surface did not validate"
OUT="$RENDER"
has "lvs_a" "$OUT" || fail "shard 0's lvstore is missing"
has "lvs_b" "$OUT" || fail "shard 1's lvstore is missing — the shards share one ConfigMap again"
has "flint-pnfs-mds-config-1" "$OUT" || fail "shard 1 has no ConfigMap of its own"
has "node-b" "$OUT" || fail "shard 1 is not pinned to its own node"
pass "two block-export shards render with their own lvstore, ConfigMap and node pin"

# A shard entry nothing will deploy is a volume whose second copy has
# nowhere to live — that must refuse, not be silently dropped.
render_must_fail "shards-exceed-count" "${TWO[@]}" --set pnfs.server.mds.count=1 || exit 1
ERR="$RENDER"
has "cannot host a second copy" "$ERR" \
  || fail "an over-long shards list did not refuse with the reason: $ERR"
pass "more shard entries than shards refuses, and says why"

# ── 6. the master switch really is one switch ────────────────────────
render_ok "block-sc-only" --set pnfs.enabled=true --set pnfs.server.enabled=true \
  --set pnfs.blockLayout.enabled=true \
  --set pnfs.server.mds.blockExport.enabled=true \
  --set pnfs.server.mds.blockExport.lvstore=lvs_flint \
  --set pnfs.server.mds.blockExport.traddr=10.0.0.9 \
  || fail "block-sc-only did not validate"
OUT="$RENDER"
has "flint-pnfs-block" "$OUT" && fail "the block StorageClass rendered without storageClass.create=true"
pass "the block StorageClass is opt-in separately from the class itself"

echo
echo "✅ kind chart pass — the pnfs-block chart surface renders, refuses what it must,"
echo "   and is accepted by a real Kubernetes API server ($SRV). The data path is NOT"
echo "   covered here and cannot be: this VM's kernel is $KVER, below the 6.11 floor."
