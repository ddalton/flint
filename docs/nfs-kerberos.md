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
  --set nfs.kerberos.realm=CORP.EXAMPLE --set nfs.kerberos.realmRef=corp
```

Then use the `flint-krb5` StorageClass (`mountOptions: [sec=krb5i]`), or
set `sec=` in any class's `mountOptions`.

## Secure by default

Turning Kerberos on turns on everything that makes it hold:

| Setting | Default with Kerberos | What it does | To relax it |
|---|---|---|---|
| `nfs.kerberos.minSec` | `krb5i` | Every RWX volume refuses `sec=sys` mounts. | `minSec: ""` (needs `attachedNodesOnly: false` too) |
| `nfs.kerberos.attachedNodesOnly` | `true` | A volume's server serves only the nodes it is attached to ([below](#only-the-nodes-a-volume-is-attached-to)). | `attachedNodesOnly: false` |
| `nfs.enforcePermissions` | `enforce` | POSIX permissions deny, checked against each caller's mapped identity. | `warn` (log only) or `off` |
| `nfs.kerberos.perVolumeKeys` | `true` | Each new volume's server has a principal of its own ([below](#each-volume-has-its-own-server-key)). | `perVolumeKeys: false` |

Without these, Kerberos would only protect the wire. A `sec=sys` mount
would still claim any uid, any node with a machine key would reach every
volume, and permission checks would only log.

**Every RWX volume becomes Kerberos only.** The floor applies to every NFS
server, so a StorageClass that mounts with `sec=sys` stops mounting once
Kerberos is on. Move those volumes to `sec=krb5i` classes first, or keep
Kerberos off until you can.

`minSec: ""` with `attachedNodesOnly` on fails to render: a `sec=sys`
mount carries no node identity, so every one would be refused anyway.

## What the chart sets up

| Piece | What it is |
|---|---|
| `KerberosPrincipal flint-nfs-server` | `nfs/flint-nfs.<nfs namespace>.svc.<clusterDomain>`, the shared server principal. Its keytab is in Secret `flint-nfs-krb5-keytab`. Only servers of volumes made with `perVolumeKeys` off (or before it existed) mount it. |
| `KerberosPrincipal flint-nfs-<volume>` | Made by the controller for each new volume (`perVolumeKeys`): `nfs/<volume>.<shared server name>`, keytab in Secret `flint-nfs-krb5-<volume>`, which only that volume's server pod mounts (`KRB5_KTNAME`). Deleted with the volume. The chart gives the controller a Role for these in the NFS namespace. |
| `KerberosNodeKeytab <release>-nfs-clients` | `host/` and `nfs/<node>` for every selected node, installed into `/etc/krb5.keytab` by the operator's DaemonSet, along with the realm's `krb5.conf`. That `krb5.conf` maps `.svc.<clusterDomain>` to the realm, which `rpc.gssd` needs in order to pick the server's realm. |
| node plugin | Before a `sec=krb5*` mount, writes `<volume ClusterIP> <that volume's server name>` into the node's `/etc/hosts`. |
| `minSec` | `FLINT_NFS_MIN_SEC` for every NFS server. `krb5i` (the default) refuses `sec=sys` mounts. |
| `allowedPrincipals` | `FLINT_NFS_GSS_PRINCIPALS`: who may establish a GSS context. The default is the nodes' machine principals (`host/*@<realm>`, `nfs/*@<realm>`) plus every principal `idmap` names. |
| `idmap`, `machineIdentity`, `anonIdentity` | `FLINT_NFS_GSS_IDMAP` and `FLINT_NFS_GSS_ANON`: which uid, gid and groups a Kerberos caller is. |
| `nfs.enforcePermissions` | `FLINT_NFS_ENFORCE_PERMISSIONS` for every NFS server: `enforce`, `warn` or `off`. The default is `enforce` with Kerberos on, and the server's `warn` with it off. |
| `attachedNodesOnly` | `FLINT_NFS_ATTACHED_NODES_ONLY` (default `true`), and the servers' read-only service account. |

## Who may connect

A valid ticket proves who a caller is, not that it may use the volume.
Without a list, every principal of the realm that can get a ticket for
`nfs/<server name>` could connect: users, services, anything.

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
  while this one is proven by a ticket. With Kerberos on, the checks
  deny by default (`nfs.enforcePermissions: enforce`).
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

## Only the nodes a volume is attached to

By default (`nfs.kerberos.attachedNodesOnly: true`), each volume's NFS
server serves only the nodes the volume is attached to. It watches the volume's
VolumeAttachments and those nodes' hostnames, through a read-only service
account.

- **Making clients and sessions:** EXCHANGE_ID and CREATE_SESSION come
  only from an attached node's machine principal (`host/`, `nfs/`,
  `root/<node>`). Users and AUTH_SYS callers make none.
- **Using a session:** SEQUENCE and BIND_CONN_TO_SESSION need the
  session's client to be an attached node's. A machine principal using it
  must be one too, while a user rides it; that is how a user's calls
  arrive.
- **Detach:** when a node is detached, its sessions fail at its next call
  and its next EXCHANGE_ID is refused.
- **Before the first list:** every node is refused.

Every RWX mount must then be Kerberos.

## Sessions are bound to their machine

Session ids are random, but under `krb5i` they cross the wire in the
clear. So the server binds each Kerberos client's sessions to the machine
that made them, using SP4_MACH_CRED state protection (RFC 8881 §18.35), as
knfsd does. The Linux client asks for it on every `krb5i` or `krb5p` mount.

- **Granted only under integrity:** the server grants SP4_MACH_CRED only to
  an EXCHANGE_ID sent with `krb5i` or `krb5p`. Over AUTH_SYS or plain
  `krb5`, the client gets NFS4ERR_INVAL and falls back to SP4_NONE.
- **The machine's own ops:** BIND_CONN_TO_SESSION, CREATE_SESSION,
  DESTROY_SESSION and DESTROY_CLIENTID need the principal that made the
  client. Anyone else gets NFS4ERR_WRONG_CRED.
- **Connections:** a session is used only on connections bound to it. A
  SEQUENCE on any other connection gets NFS4ERR_CONN_NOT_BOUND_TO_SESSION
  before its slot is touched. The client then binds the connection with
  its machine credential (BIND_CONN_TO_SESSION), which it does after every
  reconnect. A sniffed session id is no use to anyone else: they would
  need the node's key.
- **The client record:** while the client's lease runs, no other principal
  can take over its client owner, even with no state held (that is
  NFS4ERR_CLID_INUSE).
- **Restarts:** the protection is stored with the client record, so it
  survives a server restart.

A user rides the node's session on the node's connection, which is how
the kernel sends every user's calls. Root on the node holds the machine
key, so it can do anything the node can.

A client that does not ask for SP4_MACH_CRED is not protected this way.
The Linux kernel asks whenever `rpc.gssd` is running, since it then does
its state management over `krb5i`, whatever the mount's `sec=`. Without
`rpc.gssd` it uses AUTH_SYS, which the default floor refuses.

Not done yet:
- reading identities from a directory (LDAP or SSSD, as idmapd does) in
  place of the explicit list;

## Each volume has its own server key

With one shared server principal, every NFS server pod holds the same
key. Anyone who gets into one server pod could then decrypt the tickets
nodes present to every other volume's server, and pass for any of them.
So by default (`perVolumeKeys: true`) each new volume's server gets a
principal of its own:

- **Its name:** `nfs/<volume>.<shared server name>@<realm>`, for example
  `nfs/pvc-0a1b….flint-nfs.flint-system.svc.cluster.local`. A volume id
  that is not a DNS label is replaced by `v-<16 hex digits>` of its
  SHA-256. The name stays under `.svc.<clusterDomain>`, which the nodes'
  `krb5.conf` already maps to the realm.
- **Its key:** the controller creates `KerberosPrincipal flint-nfs-<volume>`
  before it starts the volume's server pod. krb5-operator writes the
  keytab to Secret `flint-nfs-krb5-<volume>`, and the pod waits for it.
  That pod mounts this Secret only, not the shared keytab.
- **The node's side:** the server pod carries the name in its
  `chert.us/krb5-server-name` annotation. ControllerPublishVolume passes
  it to the node (`nfs.chert.us/krb5-server-name`), and the node maps the
  volume's ClusterIP to it in `/etc/hosts`, so `rpc.gssd` asks for that
  principal.
- **Deletion:** deleting the volume deletes its `KerberosPrincipal`, and
  krb5-operator removes it from the KDC unless `deletionPolicy: Retain`.
  This happens whether or not `perVolumeKeys` is still on.
- **Existing volumes:** CreateVolume marks a new volume
  (`nfs.chert.us/krb5-volume-key` in its volume context). Volumes made
  before keep the shared principal, even when their server pod is
  recreated, since their nodes may still hold tickets for it. Recreate a
  volume to move it to its own key.

Root on a node that mounts the volume still holds tickets for the
server. A per-volume key limits what a broken server pod gives away; it
does not protect against a node.

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

Each volume's IP maps to its own server's name (or, for volumes with the
shared principal, to the shared name). The node plugin owns the lines that end in
`# flint-nfs-krb5`. It drops the lines of volumes no longer mounted with
Kerberos and leaves the rest of the file alone.

## Node prerequisites

- **`nfs-utils` and `rpc.gssd`:** the node needs `nfs-utils` (`nfs-common`
  on Debian and Ubuntu) and `rpc.gssd`. The `rpc-gssd` unit only starts
  when `/etc/krb5.keytab` exists, so start or restart it once the
  KerberosNodeKeytab is Ready.
- **Kernel modules:** the kernel needs `auth_rpcgss` and `rpcsec_gss_krb5`.

## What Kerberos gives here

Kerberos authenticates every caller, protects the wire (`krb5i` for
integrity, `krb5p` for privacy), and gives each caller the identity
`idmap` proves for it, with permissions enforced against that identity.

The cost is on the nodes. Root in a pod uses the node's machine key, but a
pod that runs as another uid needs that uid's own ticket on the node
([Who a caller is](#who-a-caller-is)), or its calls are refused. With the
default floor every RWX volume is Kerberos, so plan this for every non-root
workload, such as postgres, before turning Kerberos on.

`krb5p` cannot be a floor (see `tests/krb5/README.md`), but a client can
mount with `sec=krb5p` over the `krb5i` floor.

pNFS volumes are not covered.

## Tests

- **Chart defaults:** `tests/system/chart-kerberos-defaults.sh` renders
  the chart with Kerberos off, on, and with each opt-out, and checks the
  servers' settings (`helm` only; CI runs it).
- **Session binding:** unit tests in `dispatcher.rs` (`mach_cred`) and,
  against the real kernel client in the VM rig,
  `tests/krb5/run-mach-cred.sh` (connection killed, server restarted,
  session id stolen by a user with a valid ticket, and an SP4_NONE
  control).
- **Unit tests:** `nfs_krb5.rs` covers flavor detection, `/proc/mounts`
  parsing, the hosts file rewritten in place and only our lines (each IP
  with its own name), the server pod's keytab and floor, the per-volume
  server name, and which volumes get a principal of their own.
- **kind:** `KRB5=1 tests/system/kind-nfs-only.sh` uses kind nodes with
  `rpc.gssd`, krb5-operator, and `tests-nfs-krb5/`. There, an RWX volume
  mounts with `sec=krb5i` on two nodes and the pods share files, while a
  `sec=sys` mount is refused by the floor. The suite sets none of the
  floor, enforcement or attached-nodes-only: it runs on the chart's
  defaults. In the same run, a user
  principal with a real ticket (alice, uid 1000 on a node) is refused by
  the allowlist, while root on that node (the machine principal) still
  reads the volume. With permissions enforced, bob (mapped to 2000:2000)
  creates a file that root sees owned by 2000:2000, and is denied root's
  0600 file. The volume's server holds only its own
  `nfs/<pv>.flint-nfs.…` key, the node maps its ClusterIP to that name,
  and deleting the volume deletes its `KerberosPrincipal`. CI runs it
  after the nfs-only suite.
