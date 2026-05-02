#!/usr/bin/env bash
# Shared helpers for SPDK CSI regression scenarios.
#
# These tests assume a Kubernetes cluster with the Flint SPDK CSI driver
# already installed (via the helm chart in flint-csi-driver-chart/). The
# `kubeconfig` is read from $KUBECONFIG or ~/.kube/config; override with
# REGRESSION_KUBECONFIG=/path/to/kubeconfig if you keep a dedicated one.
#
# Each scenario sources this file, calls `regression::setup`, runs its
# steps, and calls `regression::teardown` on exit. Failures are surfaced
# with a non-zero exit and a clear "FAIL: <reason>" line.

set -euo pipefail

# Configuration (override via environment).
: "${REGRESSION_KUBECONFIG:=${KUBECONFIG:-$HOME/.kube/config}}"
: "${REGRESSION_NAMESPACE:=flint-regression}"
: "${REGRESSION_STORAGE_CLASS:=flint}"          # default SC from helm chart values.yaml
: "${REGRESSION_PROVISIONER:=flint.csi.storage.io}"
: "${REGRESSION_TIMEOUT:=180}"                   # seconds for any single wait
: "${REGRESSION_BASELINE_DIR:=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../baseline/spdk-e2e" && pwd)}"

export KUBECONFIG="$REGRESSION_KUBECONFIG"

# Per-scenario state — set by regression::setup, used by helpers + cleanup.
SCENARIO=""
SCENARIO_LOG=""

# ---------------------------------------------------------------------------
# Pretty-printers. Output goes to stderr so command substitution doesn't
# capture it; the scenario's structured output goes to stdout / SCENARIO_LOG.
# ---------------------------------------------------------------------------

regression::log()  { printf '  %s\n' "$*" >&2; }
regression::step() { printf '\n▶ %s\n' "$*" >&2; }
regression::ok()   { printf '✓ %s\n' "$*" >&2; }
regression::fail() { printf '\n✗ FAIL: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Setup / teardown.
# ---------------------------------------------------------------------------

# Pre-flight: cluster reachable, driver registered, default SC exists.
regression::preflight() {
    kubectl version --request-timeout=5s >/dev/null 2>&1 \
        || regression::fail "cluster not reachable via $KUBECONFIG"

    kubectl get csidriver "$REGRESSION_PROVISIONER" >/dev/null 2>&1 \
        || regression::fail "CSIDriver $REGRESSION_PROVISIONER not registered (helm install missing?)"

    kubectl get storageclass "$REGRESSION_STORAGE_CLASS" >/dev/null 2>&1 \
        || regression::fail "StorageClass $REGRESSION_STORAGE_CLASS not found"
}

# Create a clean namespace for this scenario. Idempotent — wipes any
# leftovers from a prior failed run.
regression::setup() {
    SCENARIO="$1"
    SCENARIO_LOG="/tmp/regression-${SCENARIO}-$$.log"
    : > "$SCENARIO_LOG"

    regression::step "scenario: $SCENARIO (log: $SCENARIO_LOG)"
    regression::preflight

    # If a previous run left the namespace around, wipe it. The
    # `--wait=true` ensures finalizers complete before we recreate.
    if kubectl get ns "$REGRESSION_NAMESPACE" >/dev/null 2>&1; then
        regression::log "wiping leftover namespace $REGRESSION_NAMESPACE"
        kubectl delete ns "$REGRESSION_NAMESPACE" --wait=true --timeout="${REGRESSION_TIMEOUT}s" \
            >>"$SCENARIO_LOG" 2>&1
    fi
    kubectl create ns "$REGRESSION_NAMESPACE" >>"$SCENARIO_LOG" 2>&1
}

# Trap-friendly teardown. Records the cluster state on failure to help
# triage, then deletes the namespace. The 'on success' branch also asserts
# no orphan PV/PVC are left behind — that's part of the contract from
# inventory §6.
regression::teardown() {
    local exit_code=$?
    regression::step "teardown ($([ $exit_code -eq 0 ] && echo PASS || echo FAIL))"

    if [ $exit_code -ne 0 ]; then
        regression::log "capturing diagnostic state to $SCENARIO_LOG"
        {
            echo "=== get all in $REGRESSION_NAMESPACE ==="
            kubectl get all,pvc,pv,events -n "$REGRESSION_NAMESPACE" -o wide || true
            echo "=== driver controller logs (last 200 lines) ==="
            kubectl logs -n flint-system -l app=flint-csi-controller --tail=200 || true
            echo "=== driver node logs (last 200 lines) ==="
            kubectl logs -n flint-system -l app=flint-csi-node --tail=200 || true
        } >>"$SCENARIO_LOG" 2>&1
    fi

    # Delete namespace. PVCs in it should cascade-delete; PVs (cluster-
    # scoped) we check separately.
    kubectl delete ns "$REGRESSION_NAMESPACE" --wait=true --timeout="${REGRESSION_TIMEOUT}s" \
        >>"$SCENARIO_LOG" 2>&1 || true

    # Orphan PV check — Backend B (NFS pod) creates a PV in flint-system,
    # not the test namespace, so check by storage class. A clean run leaves
    # no PV bound to a PVC that no longer exists.
    local orphans
    orphans=$(kubectl get pv -o json \
        | jq -r '.items[] | select(.spec.storageClassName=="'"$REGRESSION_STORAGE_CLASS"'") | select(.status.phase=="Released" or .spec.claimRef==null) | .metadata.name' 2>/dev/null \
        | grep -v '^$' || true)
    if [ -n "$orphans" ] && [ $exit_code -eq 0 ]; then
        regression::fail "orphan PVs left after cleanup: $orphans"
    fi

    exit $exit_code
}

# ---------------------------------------------------------------------------
# Resource helpers.
# ---------------------------------------------------------------------------

# Apply a manifest from stdin into the test namespace. Always tags
# resources with `regression-scenario=$SCENARIO` for debuggability.
regression::apply() {
    sed "s|@@NS@@|$REGRESSION_NAMESPACE|g; s|@@SC@@|$REGRESSION_STORAGE_CLASS|g" \
        | kubectl apply -n "$REGRESSION_NAMESPACE" -f - >>"$SCENARIO_LOG" 2>&1
}

# Wait for a PVC to be Bound. Fails with a clear error and event dump
# if it doesn't bind within REGRESSION_TIMEOUT.
regression::wait_pvc_bound() {
    local pvc="$1"
    regression::log "waiting for PVC $pvc to be Bound"
    if ! kubectl wait --for=jsonpath='{.status.phase}'=Bound \
            -n "$REGRESSION_NAMESPACE" pvc/"$pvc" \
            --timeout="${REGRESSION_TIMEOUT}s" >>"$SCENARIO_LOG" 2>&1; then
        kubectl describe pvc/"$pvc" -n "$REGRESSION_NAMESPACE" >>"$SCENARIO_LOG" 2>&1 || true
        regression::fail "PVC $pvc never bound (see $SCENARIO_LOG)"
    fi
}

regression::wait_pod_ready() {
    local pod="$1"
    regression::log "waiting for pod $pod to be Ready"
    if ! kubectl wait --for=condition=Ready \
            -n "$REGRESSION_NAMESPACE" pod/"$pod" \
            --timeout="${REGRESSION_TIMEOUT}s" >>"$SCENARIO_LOG" 2>&1; then
        kubectl describe pod/"$pod" -n "$REGRESSION_NAMESPACE" >>"$SCENARIO_LOG" 2>&1 || true
        regression::fail "pod $pod never became Ready (see $SCENARIO_LOG)"
    fi
}

# Wait for a pod to fully terminate (gone from the API).
regression::wait_pod_gone() {
    local pod="$1"
    regression::log "waiting for pod $pod to terminate"
    kubectl wait --for=delete pod/"$pod" \
        -n "$REGRESSION_NAMESPACE" --timeout="${REGRESSION_TIMEOUT}s" \
        >>"$SCENARIO_LOG" 2>&1 || true
}

# ---------------------------------------------------------------------------
# Contract assertions — these are the load-bearing checks. Each maps to
# a specific item in docs/spdk-driver-inventory.md.
# ---------------------------------------------------------------------------

# Inventory §6 — assert specific volume_context keys appear on a PV.
# Usage: regression::assert_pv_attribute <pv-name> <key> [<expected-value-prefix>]
# If <expected-value-prefix> is given, asserts the value starts with it.
regression::assert_pv_attribute() {
    local pv="$1" key="$2" prefix="${3:-}"
    local val
    val=$(kubectl get pv "$pv" -o jsonpath="{.spec.csi.volumeAttributes['$key']}" 2>/dev/null || echo "")
    if [ -z "$val" ]; then
        regression::fail "PV $pv missing volume_context key '$key' (inventory §6)"
    fi
    if [ -n "$prefix" ] && [[ "$val" != "$prefix"* ]]; then
        regression::fail "PV $pv key '$key' = '$val', expected prefix '$prefix'"
    fi
    regression::log "  PV $pv has $key = $val"
}

# Resolve the PV name backing a PVC.
regression::pv_for_pvc() {
    local pvc="$1"
    kubectl get pvc "$pvc" -n "$REGRESSION_NAMESPACE" -o jsonpath='{.spec.volumeName}'
}

# Run a command inside a pod and return its stdout. Stderr goes to the log.
regression::pod_exec() {
    local pod="$1"; shift
    kubectl exec -n "$REGRESSION_NAMESPACE" "$pod" -- "$@" 2>>"$SCENARIO_LOG"
}

# Compare a recorded baseline (a single line of text) against a current
# value. Used for "the expected key set" style checks. If REGRESSION_RECORD=1
# is set, writes the value to the baseline instead of comparing.
regression::baseline() {
    local key="$1" current="$2"
    local file="$REGRESSION_BASELINE_DIR/${SCENARIO}.${key}"
    mkdir -p "$REGRESSION_BASELINE_DIR"
    if [ "${REGRESSION_RECORD:-0}" = "1" ]; then
        printf '%s\n' "$current" > "$file"
        regression::log "  recorded baseline ${SCENARIO}.${key}: $current"
        return 0
    fi
    if [ ! -f "$file" ]; then
        regression::fail "no baseline at $file (run with REGRESSION_RECORD=1 first)"
    fi
    local expected
    expected=$(cat "$file")
    if [ "$expected" != "$current" ]; then
        regression::fail "baseline drift on ${SCENARIO}.${key}: expected '$expected', got '$current'"
    fi
    regression::log "  baseline match: ${SCENARIO}.${key}"
}
