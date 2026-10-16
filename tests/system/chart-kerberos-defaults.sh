#!/usr/bin/env bash
# The chart's Kerberos defaults (docs/nfs-kerberos.md, "Secure by default"):
# nfs.kerberos.enabled alone gives every NFS server the krb5i floor,
# enforced permissions, attached-nodes-only and required SP4_MACH_CRED,
# and each new volume a server principal of its own; each relaxes only
# when set explicitly; Kerberos off changes nothing. pnfs.kerberos gives
# each MDS shard its principal and the same server env (the names in
# tests/system/krb5-server-env-names.txt), without attached-nodes-only.
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
env_of() { awk -v n="$1" '$0 ~ "- name: "n"$" {getline; sub(/.*value: /, ""); gsub(/"/, ""); print; found=1; exit} END {if (!found) print "-"}' "${2:-$TMP/out.yaml}"; }
# One rendered object (<kind> <name>) into $TMP/doc.yaml; false when absent.
doc_of() {
  awk -v k="kind: $1" -v n="  name: $2" '
    /^---/ { if (hit) exit; buf = ""; kind = 0; name = 0; next }
    { buf = buf $0 "\n" }
    $0 == k { kind = 1 }
    $0 == n && !name { name = 1 }
    kind && name && !hit { hit = 1 }
    END { if (hit) printf "%s", buf; else exit 1 }' "$TMP/out.yaml" >"$TMP/doc.yaml"
}
check() { # <case> <what> <command...>: passes when the command does
  local c=$1 w=$2; shift 2
  if "$@" >/dev/null 2>&1; then echo "✓ $c: $w"; else echo "✗ $c: $w"; fails=$((fails + 1)); fi
}
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
  expect "Kerberos off" NFS_REQUIRE_MACH_CRED -
fi

if renders "Kerberos off, enforce set" --set nfs.enforcePermissions=enforce; then
  expect "Kerberos off, enforce set" NFS_ENFORCE_PERMISSIONS enforce
fi

if renders "Kerberos on" "${KRB5[@]}"; then
  expect "Kerberos on" NFS_MIN_SEC krb5i
  expect "Kerberos on" NFS_ENFORCE_PERMISSIONS enforce
  expect "Kerberos on" NFS_ATTACHED_NODES_ONLY true
  expect "Kerberos on" NFS_SERVER_SERVICE_ACCOUNT flint-nfs-server
  expect "Kerberos on" NFS_REQUIRE_MACH_CRED true
  if grep -q 'volumeattachments' "$TMP/out.yaml"; then
    echo "✓ Kerberos on: the servers may read VolumeAttachments"
  else
    echo "✗ Kerberos on: no RBAC for VolumeAttachments"; fails=$((fails + 1))
  fi
fi

if renders "Kerberos on" "${KRB5[@]}"; then
  expect "Kerberos on" NFS_KRB5_REALM CORP.EXAMPLE
  expect "Kerberos on" NFS_KRB5_SERVER_NAME flint-nfs.flint-system.svc.cluster.local
  if grep -q 'kerberosprincipals' "$TMP/out.yaml"; then
    echo "✓ Kerberos on: the controller may make each volume's principal"
  else
    echo "✗ Kerberos on: no RBAC for per-volume KerberosPrincipals"; fails=$((fails + 1))
  fi
fi

if renders "perVolumeKeys=false" "${KRB5[@]}" --set nfs.kerberos.perVolumeKeys=false; then
  expect "perVolumeKeys=false" NFS_KRB5_REALM -
  expect "perVolumeKeys=false" NFS_KRB5_KEYTAB_SECRET flint-nfs-krb5-keytab
  if grep -q 'kerberosprincipals' "$TMP/out.yaml"; then
    echo "✓ perVolumeKeys=false: the controller may still delete earlier volumes' principals"
  else
    echo "✗ perVolumeKeys=false: no RBAC to delete earlier volumes' principals"; fails=$((fails + 1))
  fi
fi

if renders "warn" "${KRB5[@]}" --set nfs.enforcePermissions=warn; then
  expect "warn" NFS_ENFORCE_PERMISSIONS warn
fi

if renders "attachedNodesOnly=false" "${KRB5[@]}" --set nfs.kerberos.attachedNodesOnly=false; then
  expect "attachedNodesOnly=false" NFS_ATTACHED_NODES_ONLY -
  expect "attachedNodesOnly=false" NFS_MIN_SEC krb5i
  expect "attachedNodesOnly=false" NFS_REQUIRE_MACH_CRED true
fi

if renders "requireMachCred=false" "${KRB5[@]}" --set nfs.kerberos.requireMachCred=false; then
  expect "requireMachCred=false" NFS_REQUIRE_MACH_CRED -
  expect "requireMachCred=false" NFS_ATTACHED_NODES_ONLY true
  expect "requireMachCred=false" NFS_MIN_SEC krb5i
fi

if renders "krb5 floor" "${KRB5[@]}" --set nfs.kerberos.minSec=krb5; then
  expect "krb5 floor" NFS_MIN_SEC krb5
  expect "krb5 floor" NFS_REQUIRE_MACH_CRED true
fi

if renders "no floor" "${KRB5[@]}" --set nfs.kerberos.minSec= --set nfs.kerberos.attachedNodesOnly=false \
    --set nfs.kerberos.requireMachCred=false; then
  expect "no floor" NFS_MIN_SEC -
  expect "no floor" NFS_ATTACHED_NODES_ONLY -
  expect "no floor" NFS_REQUIRE_MACH_CRED -
fi

# No floor with attachedNodesOnly on would refuse every sec=sys mount: the
# chart says so instead of rendering it.
if render "${KRB5[@]}" --set nfs.kerberos.minSec= --set nfs.kerberos.requireMachCred=false; then
  echo "✗ no floor, attachedNodesOnly on: rendered"; fails=$((fails + 1))
elif grep -q 'attachedNodesOnly' "$TMP/err.txt"; then
  echo "✓ no floor, attachedNodesOnly on: refused"
else
  echo "✗ no floor, attachedNodesOnly on: failed for another reason"; head -3 "$TMP/err.txt"; fails=$((fails + 1))
fi

# Likewise requireMachCred: a sec=sys mount cannot ask for SP4_MACH_CRED.
if render "${KRB5[@]}" --set nfs.kerberos.minSec= --set nfs.kerberos.attachedNodesOnly=false; then
  echo "✗ no floor, requireMachCred on: rendered"; fails=$((fails + 1))
elif grep -q 'requireMachCred' "$TMP/err.txt"; then
  echo "✓ no floor, requireMachCred on: refused"
else
  echo "✗ no floor, requireMachCred on: failed for another reason"; head -3 "$TMP/err.txt"; fails=$((fails + 1))
fi

# The directory (nfs.kerberos.ldap): off by default, and off without
# Kerberos even when enabled.
if renders "Kerberos on, no directory" "${KRB5[@]}"; then
  expect "Kerberos on, no directory" NFS_GSS_LDAP_URL -
fi
LDAP=(--set nfs.kerberos.ldap.enabled=true --set nfs.kerberos.ldap.url=ldaps://ldap.corp.example
  --set 'nfs.kerberos.ldap.userBase=ou=People\,dc=corp\,dc=example'
  --set 'nfs.kerberos.ldap.bindDN=cn=nfs\,dc=corp\,dc=example'
  --set nfs.kerberos.ldap.bindPasswordSecret.name=flint-ldap-bind
  --set nfs.kerberos.ldap.ca.configMap=corp-ca)
if renders "directory, Kerberos off" "${LDAP[@]}"; then
  expect "directory, Kerberos off" NFS_GSS_LDAP_URL -
fi
if renders "directory" "${KRB5[@]}" "${LDAP[@]}"; then
  expect "directory" NFS_GSS_LDAP_URL ldaps://ldap.corp.example
  expect "directory" NFS_GSS_LDAP_REALMS CORP.EXAMPLE
  expect "directory" NFS_GSS_LDAP_USER_BASE "ou=People,dc=corp,dc=example"
  expect "directory" NFS_GSS_LDAP_INSECURE -
  expect "directory" NFS_GSS_LDAP_BIND_PASSWORD_SECRET flint-ldap-bind
  expect "directory" NFS_GSS_LDAP_BIND_PASSWORD_KEY password
  expect "directory" NFS_GSS_LDAP_CA_CONFIGMAP corp-ca
  expect "directory" NFS_GSS_LDAP_TTL 300
  expect "directory" NFS_GSS_LDAP_ALLOW_ROOT -
  # The password is named, never a value in a manifest.
  expect "directory" NFS_GSS_LDAP_BIND_PASSWORD -
  expect "directory" FLINT_NFS_GSS_LDAP_BIND_PASSWORD -
  # The static allowlist is unchanged: the directory's users are admitted
  # by the servers, not by a longer list.
  expect "directory" NFS_GSS_PRINCIPALS "host/*@CORP.EXAMPLE,nfs/*@CORP.EXAMPLE"
fi
for bad in "nfs.kerberos.ldap.userBase=" "nfs.kerberos.ldap.url=http://ldap" \
  "nfs.kerberos.ldap.bindPasswordSecret.name=" "nfs.kerberos.ldap.ca.secret=corp-ca"; do
  if render "${KRB5[@]}" "${LDAP[@]}" --set "$bad"; then
    echo "✗ directory with $bad: rendered"; fails=$((fails + 1))
  elif grep -q 'nfs.kerberos.ldap' "$TMP/err.txt"; then
    echo "✓ directory with $bad: refused"
  else
    echo "✗ directory with $bad: failed for another reason"; head -3 "$TMP/err.txt"; fails=$((fails + 1))
  fi
done

# ── pNFS (pnfs.kerberos) ────────────────────────────────────────────
PNFS=(--set pnfs.enabled=true --set pnfs.server.enabled=true --set pnfs.server.mds.count=2)
PK=(--set pnfs.kerberos.enabled=true --set nfs.kerberos.realm=CORP.EXAMPLE --set nfs.kerberos.realmRef=corp)

if renders "pNFS, Kerberos off" "${PNFS[@]}"; then
  check "pNFS, Kerberos off" "no MDS principal" bash -c "! grep -q 'kind: KerberosPrincipal' '$TMP/out.yaml'"
  check "pNFS, Kerberos off" "no keytab, no floor anywhere" bash -c "! grep -qE 'KRB5_KTNAME|FLINT_NFS_MIN_SEC|PNFS_KRB5_MDS_NAME_BASE|flint-pnfs-krb5|host-etc-hosts' '$TMP/out.yaml'"
  check "pNFS, Kerberos off" "no KerberosNodeKeytab" bash -c "! grep -q 'kind: KerberosNodeKeytab' '$TMP/out.yaml'"
fi

if renders "pNFS Kerberos on" "${PNFS[@]}" "${PK[@]}"; then
  expect "pNFS Kerberos on" PNFS_KRB5_MDS_NAME_BASE flint-system.svc.cluster.local
  expect "pNFS Kerberos on" FLINT_NFS_KRB5_HOSTS_FILE /host/etc/hosts
  # RWX Kerberos stays off: the RWX servers' settings are not set.
  expect "pNFS Kerberos on" NFS_MIN_SEC -
  expect "pNFS Kerberos on" FLINT_NFS_KRB5_SERVER_NAME -
  for i in 0 1; do
    c="pNFS Kerberos on, shard $i"
    check "$c" "a KerberosPrincipal" doc_of KerberosPrincipal "flint-pnfs-mds-$i"
    check "$c" "nfs/flint-pnfs-mds-$i.flint-system.svc.cluster.local@CORP.EXAMPLE" \
      grep -q "nfs/flint-pnfs-mds-$i.flint-system.svc.cluster.local@CORP.EXAMPLE" "$TMP/doc.yaml"
    check "$c" "its own keytab Secret" grep -q "secretName: flint-pnfs-mds-$i-krb5" "$TMP/doc.yaml"
    # The shard's Service has the name its principal is for.
    check "$c" "a Service named for it" doc_of Service "flint-pnfs-mds-$i"
    if [ "$i" = 0 ]; then dep=flint-pnfs-mds; else dep=flint-pnfs-mds-$i; fi
    if doc_of Deployment "$dep"; then
      for kv in KRB5_KTNAME=/etc/flint-krb5/krb5.keytab FLINT_NFS_MIN_SEC=krb5i \
        FLINT_NFS_REQUIRE_MACH_CRED=true FLINT_NFS_ENFORCE_PERMISSIONS=enforce \
        FLINT_NFS_GSS_ANON=65534:65534 FLINT_NFS_ATTACHED_NODES_ONLY=- FLINT_NFS_ROOT_SQUASH=- \
        FLINT_PNFS_LAYOUTS_FOR_GSS=-; do
        got=$(env_of "${kv%%=*}" "$TMP/doc.yaml")
        if [ "$got" = "${kv#*=}" ]; then echo "✓ $c: ${kv%%=*}=${kv#*=}"
        else echo "✗ $c: ${kv%%=*} is $got, want ${kv#*=}"; fails=$((fails + 1)); fi
      done
      check "$c" "mounts its own keytab, and no other shard's" bash -c \
        "grep -q 'secretName: flint-pnfs-mds-$i-krb5' '$TMP/doc.yaml' && [ \$(grep -c 'secretName: flint-pnfs-mds-.*-krb5' '$TMP/doc.yaml') = 1 ]"
      check "$c" "the keytab mounted where KRB5_KTNAME points" grep -q 'mountPath: /etc/flint-krb5$' "$TMP/doc.yaml"
    else
      echo "✗ $c: no Deployment $dep"; fails=$((fails + 1))
    fi
  done
  if doc_of StorageClass flint-pnfs-krb5; then
    check "pNFS Kerberos on" "StorageClass mounts sec=krb5i" grep -q -- '- sec=krb5i' "$TMP/doc.yaml"
    check "pNFS Kerberos on" "StorageClass is a pNFS class" grep -q 'layout: pnfs$' "$TMP/doc.yaml"
  else
    echo "✗ pNFS Kerberos on: no flint-pnfs-krb5 StorageClass"; fails=$((fails + 1))
  fi
  check "pNFS Kerberos on" "one KerberosNodeKeytab" bash -c "[ \$(grep -c 'kind: KerberosNodeKeytab' '$TMP/out.yaml') = 1 ]"
  check "pNFS Kerberos on" "the node maps hosts" grep -q 'mountPath: /host/etc/hosts' "$TMP/out.yaml"
fi

if renders "pNFS and RWX Kerberos on" "${PNFS[@]}" "${PK[@]}" --set nfs.kerberos.enabled=true; then
  check "pNFS and RWX Kerberos on" "one KerberosNodeKeytab, not two" bash -c "[ \$(grep -c 'kind: KerberosNodeKeytab' '$TMP/out.yaml') = 1 ]"
  expect "pNFS and RWX Kerberos on" FLINT_NFS_KRB5_SERVER_NAME flint-nfs.flint-system.svc.cluster.local
  check "pNFS and RWX Kerberos on" "the RWX servers' principal too" doc_of KerberosPrincipal flint-nfs-server
fi

if renders "pNFS Kerberos, StorageClass off" "${PNFS[@]}" "${PK[@]}" --set pnfs.kerberos.storageClass.create=false; then
  check "pNFS Kerberos, StorageClass off" "no flint-pnfs-krb5" bash -c "! grep -q 'flint-pnfs-krb5' '$TMP/out.yaml'"
fi

if renders "pNFS Kerberos, relaxed" "${PNFS[@]}" "${PK[@]}" --set pnfs.kerberos.minSec= \
    --set pnfs.kerberos.requireMachCred=false --set pnfs.kerberos.enforcePermissions=warn --set nfs.rootSquash=true; then
  doc_of Deployment flint-pnfs-mds
  for kv in FLINT_NFS_MIN_SEC=- FLINT_NFS_REQUIRE_MACH_CRED=- FLINT_NFS_ENFORCE_PERMISSIONS=warn FLINT_NFS_ROOT_SQUASH=true; do
    got=$(env_of "${kv%%=*}" "$TMP/doc.yaml")
    if [ "$got" = "${kv#*=}" ]; then echo "✓ pNFS Kerberos, relaxed: ${kv%%=*}=${kv#*=}"
    else echo "✗ pNFS Kerberos, relaxed: ${kv%%=*} is $got, want ${kv#*=}"; fails=$((fails + 1)); fi
  done
fi

# The MDS's server env, by name, is the RWX servers' (nfs_krb5::server_pod_parts):
# every optional setting on, the names must be exactly the list.
if renders "pNFS Kerberos, every setting" "${PNFS[@]}" "${PK[@]}" "${LDAP[@]}" --set nfs.rootSquash=true \
    --set nfs.kerberos.ldap.insecure=true --set nfs.kerberos.ldap.allowRoot=true \
    --set nfs.kerberos.ldap.groupBase=ou=Groups --set nfs.kerberos.ldap.principalAttribute=krbPrincipalName; then
  doc_of Deployment flint-pnfs-mds
  grep -oE -- '- name: (KRB5_|FLINT_NFS_)[A-Z0-9_]+$' "$TMP/doc.yaml" | sed 's/- name: //' | sort >"$TMP/got.txt"
  grep -vE '^(#|$)' "$ROOT/tests/system/krb5-server-env-names.txt" | sort >"$TMP/want.txt"
  if diff "$TMP/want.txt" "$TMP/got.txt" >"$TMP/names.diff"; then
    echo "✓ pNFS Kerberos, every setting: the MDS env names are krb5-server-env-names.txt"
  else
    echo "✗ pNFS Kerberos, every setting: the MDS env names differ from krb5-server-env-names.txt"
    cat "$TMP/names.diff"; fails=$((fails + 1))
  fi
  # The password is a Secret reference, never a value.
  check "pNFS Kerberos, every setting" "bind password from its Secret" bash -c \
    "grep -A3 'name: FLINT_NFS_GSS_LDAP_BIND_PASSWORD$' '$TMP/doc.yaml' | grep -q 'secretKeyRef'"
  check "pNFS Kerberos, every setting" "the CA as a file" grep -q 'mountPath: /etc/flint-ldap-ca$' "$TMP/doc.yaml"
fi

refused() { # <case> <error text> <helm args...>
  local c=$1 want=$2; shift 2
  if render "$@"; then
    echo "✗ $c: rendered"; fails=$((fails + 1))
  elif grep -q -- "$want" "$TMP/err.txt"; then
    echo "✓ $c: refused"
  else
    echo "✗ $c: failed for another reason"; head -3 "$TMP/err.txt"; fails=$((fails + 1))
  fi
}
refused "pNFS Kerberos, no realm" "nfs.kerberos.realm" "${PNFS[@]}" --set pnfs.kerberos.enabled=true --set nfs.kerberos.realmRef=corp
refused "pNFS Kerberos, no realmRef" "nfs.kerberos.realmRef" "${PNFS[@]}" --set pnfs.kerberos.enabled=true --set nfs.kerberos.realm=CORP.EXAMPLE
refused "pNFS Kerberos, no floor, requireMachCred on" "requireMachCred" "${PNFS[@]}" "${PK[@]}" --set pnfs.kerberos.minSec=
refused "pNFS Kerberos, external MDS" "pnfs.server.enabled" --set pnfs.enabled=true --set pnfs.mdsEndpoint=mds:50051 "${PK[@]}"
refused "pNFS Kerberos, bad sec" "pnfs.kerberos.storageClass.sec" "${PNFS[@]}" "${PK[@]}" --set pnfs.kerberos.storageClass.sec=sys


# The node agent starts no host unit unless asked (restartServices).
if renders "Kerberos on" "${KRB5[@]}"; then
  if grep -q 'restartServices' "$TMP/out.yaml"; then
    echo "✗ Kerberos on: the node agent may start host units by default"; fails=$((fails + 1))
  else
    echo "✓ Kerberos on: no restartServices by default"
  fi
fi
if renders "restartServices" "${KRB5[@]}" --set 'nfs.kerberos.nodeKeytab.restartServices[0]=rpc-gssd'; then
  if grep -A1 'restartServices:' "$TMP/out.yaml" | grep -q -- '- rpc-gssd'; then
    echo "✓ restartServices: [rpc-gssd] reaches the KerberosNodeKeytab"
  else
    echo "✗ restartServices: not on the KerberosNodeKeytab"; fails=$((fails + 1))
  fi
fi

[ "$fails" -eq 0 ] && echo PASS || { echo "FAIL: $fails"; exit 1; }
