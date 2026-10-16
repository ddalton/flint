#!/usr/bin/env bash
# The chart's Kerberos defaults (docs/nfs-kerberos.md, "Secure by default"):
# nfs.kerberos.enabled alone gives every NFS server the krb5i floor,
# enforced permissions and attached-nodes-only; each relaxes only when set
# explicitly; Kerberos off changes nothing.
#
# helm only, no cluster. Runs in CI before the kind suites.
#
# Usage:  tests/system/chart-kerberos-defaults.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$ROOT/flint-csi-driver-chart"
TMP="$(mktemp -d -t flint-krb5-defaults.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
command -v helm >/dev/null || { echo "MISSING TOOL: helm — this test cannot run and is NOT passing"; exit 2; }

fails=0
KRB5=(--set nfs.kerberos.enabled=true --set nfs.kerberos.realm=CORP.EXAMPLE --set nfs.kerberos.realmRef=corp)

render() { helm template t "$CHART" -n flint-system "$@" >"$TMP/out.yaml" 2>"$TMP/err.txt"; }
# The controller's value for an env var, or "-" when it is not set.
env_of() { awk -v n="$1" '$0 ~ "- name: "n"$" {getline; sub(/.*value: /, ""); gsub(/"/, ""); print; found=1; exit} END {if (!found) print "-"}' "$TMP/out.yaml"; }
expect() { # <case> <env var> <want>
  local got; got=$(env_of "$2")
  if [ "$got" = "$3" ]; then echo "✓ $1: $2=$3"; else echo "✗ $1: $2 is $got, want $3"; fails=$((fails + 1)); fi
}
renders() { # <case> <helm args...>
  local c=$1; shift
  if render "$@"; then return 0; fi
  echo "✗ $c: helm template failed"; head -3 "$TMP/err.txt"; fails=$((fails + 1)); return 1
}

if renders "Kerberos off"; then
  expect "Kerberos off" NFS_MIN_SEC -
  expect "Kerberos off" NFS_ENFORCE_PERMISSIONS -
  expect "Kerberos off" NFS_ATTACHED_NODES_ONLY -
fi

if renders "Kerberos off, enforce set" --set nfs.enforcePermissions=enforce; then
  expect "Kerberos off, enforce set" NFS_ENFORCE_PERMISSIONS enforce
fi

if renders "Kerberos on" "${KRB5[@]}"; then
  expect "Kerberos on" NFS_MIN_SEC krb5i
  expect "Kerberos on" NFS_ENFORCE_PERMISSIONS enforce
  expect "Kerberos on" NFS_ATTACHED_NODES_ONLY true
  expect "Kerberos on" NFS_SERVER_SERVICE_ACCOUNT flint-nfs-server
  if grep -q 'volumeattachments' "$TMP/out.yaml"; then
    echo "✓ Kerberos on: the servers may read VolumeAttachments"
  else
    echo "✗ Kerberos on: no RBAC for VolumeAttachments"; fails=$((fails + 1))
  fi
fi

if renders "warn" "${KRB5[@]}" --set nfs.enforcePermissions=warn; then
  expect "warn" NFS_ENFORCE_PERMISSIONS warn
fi

if renders "attachedNodesOnly=false" "${KRB5[@]}" --set nfs.kerberos.attachedNodesOnly=false; then
  expect "attachedNodesOnly=false" NFS_ATTACHED_NODES_ONLY -
  expect "attachedNodesOnly=false" NFS_MIN_SEC krb5i
fi

if renders "no floor" "${KRB5[@]}" --set nfs.kerberos.minSec= --set nfs.kerberos.attachedNodesOnly=false; then
  expect "no floor" NFS_MIN_SEC -
  expect "no floor" NFS_ATTACHED_NODES_ONLY -
fi

# No floor with attachedNodesOnly on would refuse every sec=sys mount: the
# chart says so instead of rendering it.
if render "${KRB5[@]}" --set nfs.kerberos.minSec=; then
  echo "✗ no floor, attachedNodesOnly on: rendered"; fails=$((fails + 1))
elif grep -q 'attachedNodesOnly' "$TMP/err.txt"; then
  echo "✓ no floor, attachedNodesOnly on: refused"
else
  echo "✗ no floor, attachedNodesOnly on: failed for another reason"; head -3 "$TMP/err.txt"; fails=$((fails + 1))
fi

[ "$fails" -eq 0 ] && echo PASS || { echo "FAIL: $fails"; exit 1; }
