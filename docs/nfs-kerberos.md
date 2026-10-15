# Kerberos for RWX NFS volumes

RWX volumes can mount with `sec=krb5`, `krb5i` or `krb5p`, with the keys
coming from [krb5-operator](https://github.com/ddalton/krb5-operator) (0.3
or later). The operator runs the KDC (a `KerberosRealm`), and the chart
asks it for keys:

```sh
kubectl apply --server-side \
  -f https://raw.githubusercontent.com/ddalton/krb5-operator/v0.3.0/deploy/crds.yaml \
  -f https://raw.githubusercontent.com/ddalton/krb5-operator/v0.3.0/deploy/operator.yaml
kubectl apply -f - <<EOF
apiVersion: krb5.ddalton.dev/v1alpha1
kind: KerberosRealm
metadata: {name: corp, namespace: flint-system}
spec: {realm: CORP.EXAMPLE}
EOF
helm upgrade --install flint-csi flint-csi-driver-chart -n flint-system \
  --set nfs.kerberos.enabled=true \
  --set nfs.kerberos.realm=CORP.EXAMPLE --set nfs.kerberos.realmRef=corp \
  --set nfs.kerberos.minSec=krb5i
```

Then use the `flint-krb5` StorageClass (`mountOptions: [sec=krb5i]`), or
set `sec=` in any class's `mountOptions`.

## What the chart sets up

| Piece | What it is |
|---|---|
| `KerberosPrincipal flint-nfs-server` | `nfs/flint-nfs.<nfs namespace>.svc.<clusterDomain>`, the one principal every NFS server pod uses. Its keytab is in Secret `flint-nfs-krb5-keytab`, which each server pod mounts (`KRB5_KTNAME`). |
| `KerberosNodeKeytab <release>-nfs-clients` | `host/` and `nfs/<node>` for every selected node, installed into `/etc/krb5.keytab` by the operator's DaemonSet, along with the realm's `krb5.conf`. That `krb5.conf` maps `.svc.<clusterDomain>` to the realm, which `rpc.gssd` needs in order to pick the server's realm. |
| node plugin | Before a `sec=krb5*` mount, writes `<volume ClusterIP> <server name>` into the node's `/etc/hosts`. |
| `minSec` | `FLINT_NFS_MIN_SEC` for every NFS server. `krb5i` refuses `sec=sys` mounts. |

## Why the hosts entry

A node mounts a volume by its Service's ClusterIP. `rpc.gssd`, on the node,
works out which principal to ask for from that address, using the node's
own resolver. That resolver does not know cluster DNS. Measured against a
real `rpc.gssd` (nfs-utils 2.6.4):

- **By IP alone:** the mount fails with `unable to resolve <ip> to
  hostname`.
- **By a cluster name** (resolved inside the node plugin): the mount fails,
  because `gssd` forward-resolves the name itself (`get_full_hostname`).
- **With `<ip> <name>` in `/etc/hosts`:** it works. `gssd` reverse-resolves
  the IP to the name and asks for `nfs/<name>`.

Every volume's IP maps to the same name, so one server principal serves
every volume. The node plugin owns the lines that end in
`# flint-nfs-krb5`. It drops the lines of volumes no longer mounted with
Kerberos and leaves the rest of the file alone.

## Node prerequisites

- **`nfs-utils` and `rpc.gssd`:** the node needs `nfs-utils` (`nfs-common`
  on Debian and Ubuntu) and `rpc.gssd`. The `rpc-gssd` unit only starts
  when `/etc/krb5.keytab` exists, so start or restart it once the
  KerberosNodeKeytab is Ready.
- **Kernel modules:** the kernel needs `auth_rpcgss` and `rpcsec_gss_krb5`.

## What Kerberos gives here

The server maps no principal to a uid, because requests under Kerberos
carry none. So Kerberos authenticates the node and protects the wire
(`krb5i` for integrity, `krb5p` for privacy), but it does not give per-user
file permissions. Keep ownership-sensitive workloads, such as postgres, on
`sec=sys`. A `minSec` floor makes every volume Kerberos. `krb5p` cannot be
a floor (see `tests/krb5/README.md`), but a client can mount with
`sec=krb5p` over a `krb5i` floor.

pNFS volumes are not covered.

## Tests

- **Unit tests:** `nfs_krb5.rs` covers flavor detection, `/proc/mounts`
  parsing, the hosts file rewritten in place and only our lines, and the
  server pod's keytab and floor.
- **kind:** `KRB5=1 tests/system/kind-nfs-only.sh` uses kind nodes with
  `rpc.gssd`, krb5-operator, and `tests-nfs-krb5/`. There, an RWX volume
  mounts with `sec=krb5i` on two nodes and the pods share files, while a
  `sec=sys` mount is refused by the floor. CI runs it after the nfs-only
  suite.
