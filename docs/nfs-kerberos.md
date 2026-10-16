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
| `allowedPrincipals` | `FLINT_NFS_GSS_PRINCIPALS`: who may establish a GSS context. The default is the nodes' machine principals (`host/*@<realm>`, `nfs/*@<realm>`) plus every principal `idmap` names. |
| `idmap`, `machineIdentity`, `anonIdentity` | `FLINT_NFS_GSS_IDMAP` and `FLINT_NFS_GSS_ANON`: which uid, gid and groups a Kerberos caller is. |
| `nfs.enforcePermissions` | `FLINT_NFS_ENFORCE_PERMISSIONS` for every NFS server (`warn` by default, `enforce`, `off`). |

## Who may connect

A valid ticket proves who a caller is, not that it may use the volume.
flint's server maps no principal to a uid, and a call that carries no uid
is not permission-checked. So without a list, every principal of the realm
that can get a ticket for `nfs/<server name>` has the whole volume: users,
services, anything.

Servers like knfsd, NetApp and Isilon handle this in three layers:

1. **Map principals to identities.** idmapd or SSSD, or the server's
   name-mapping rules, turn `user@REALM` into a uid. A principal that maps
   to nothing becomes `nobody`, never full access.
2. **Treat machine principals specially.** `nfs/`, `host/` and
   `root/<host>` are a client machine (knfsd maps them to root, and
   `root_squash` then applies).
3. **Restrict each export** to the security flavors (`sec=`) and client
   hosts it allows.

The allowlist is the part of that which decides who connects. Only the
listed principals may establish a context; anyone else's `RPCSEC_GSS_INIT`
fails (logged as `<principal> is not in FLINT_NFS_GSS_PRINCIPALS`), and
the client gets EACCES.

Patterns look like `<component>[/<component>...]@<REALM>`. `*` matches
within one component, the number of components must match, and the realm
is always spelled out, because a wildcard realm would admit a cross-realm
trust. A malformed list refuses everyone rather than failing open.

The list names principals, so it is only as good as control over who can
create them. Anyone allowed to make a `KerberosPrincipal` in the realm can
make a `host/whatever` principal. Keep the realm's `allowedNamespaces` to
trusted namespaces (the default, `Same`, allows only the realm's own).

## Who a caller is

`nfs.kerberos.idmap` maps principals to unix identities, the way knfsd's
idmapping does:

```yaml
nfs:
  enforcePermissions: enforce
  kerberos:
    idmap:
    - principal: alice@CORP.EXAMPLE
      uid: 1000
      gid: 1000
      groups: [100]
```

- **Matching:** rules are tried in order and the first match wins. The
  user rules come first, then the nodes' machine principals
  (`machineIdentity`, `0:0`). The kernel client uses the machine principal
  for root and for lease traffic. The server does not squash root, so
  that is root, as an AUTH_SYS root is.
- **Unmatched principals** are `anonIdentity` (`65534:65534`, nobody),
  never an unchecked caller. A malformed map makes everyone nobody.
- **What a mapped caller gets:** a Kerberos caller is permission-checked
  as its identity and owns what it creates. This is the first identity
  the server can trust. An AUTH_SYS uid is whatever the client claims,
  while this one is proven by a ticket. With `nfs.enforcePermissions:
  enforce`, the checks deny (by default they only log).
- **Users on the node:** a process that is not root needs its own ticket
  on the node, in `/tmp/krb5cc_<uid>`, where `rpc.gssd` looks.
- **What a user with no usable credential sees:** a user with no ticket, or
  whose principal the allowlist refuses, gets "Permission denied" on files
  and "Stale file handle" on the mount's root directory. `rpc.gssd` hands
  the kernel EACCES; the Linux client (6.8) reports the root directory as
  ESTALE. knfsd gives exactly the same two errors (measured side by side).
  It is the client's report, not the server's: without a ticket, no
  request reaches the server at all.

- **Root:** with `nfs.rootSquash: true`, root on a client (the machine
  principal, or an AUTH_SYS uid 0) is the anonymous identity, as knfsd's
  `root_squash`. Root containers and the kubelet then cannot chown, or
  read what only root may read, on RWX volumes.

Not done yet:
- reading identities from a directory (LDAP or SSSD, as idmapd does) in
  place of the explicit list;
- restricting a volume to the nodes Kubernetes published it to.

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
carry none. So Kerberos authenticates the node (and, with the allowlist,
only nodes connect) and protects the wire
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
  `sec=sys` mount is refused by the floor. In the same run, a user
  principal with a real ticket (alice, uid 1000 on a node) is refused by
  the allowlist, while root on that node (the machine principal) still
  reads the volume. With permissions enforced, bob (mapped to 2000:2000)
  creates a file that root sees owned by 2000:2000, and is denied root's
  0600 file. CI runs it after the nfs-only suite.
