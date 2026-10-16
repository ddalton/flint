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
| `nfs.kerberos.minSec` | `krb5i` | Every RWX volume refuses `sec=sys` mounts. | `minSec: ""` (needs `attachedNodesOnly: false` and `requireMachCred: false` too) |
| `nfs.kerberos.attachedNodesOnly` | `true` | A volume's server serves only the nodes it is attached to ([below](#only-the-nodes-a-volume-is-attached-to)). | `attachedNodesOnly: false` |
| `nfs.kerberos.requireMachCred` | `true` | Every client's sessions are bound to the machine that made it ([below](#sessions-are-bound-to-their-machine)). | `requireMachCred: false` |
| `nfs.enforcePermissions` | `enforce` | POSIX permissions deny, checked against each caller's mapped identity. | `warn` (log only) or `off` |
| `nfs.kerberos.perVolumeKeys` | `true` | Each new volume's server has a principal of its own ([below](#each-volume-has-its-own-server-key)). | `perVolumeKeys: false` |

Without these, Kerberos would only protect the wire. A `sec=sys` mount
would still claim any uid, any node with a machine key would reach every
volume, and permission checks would only log.

**Every RWX volume becomes Kerberos only.** The floor applies to every NFS
server, so a StorageClass that mounts with `sec=sys` stops mounting once
Kerberos is on. Move those volumes to `sec=krb5i` classes first, or keep
Kerberos off until you can.

`minSec: ""` with `attachedNodesOnly` or `requireMachCred` on fails to
render: a `sec=sys` mount carries no node identity and cannot ask for
SP4_MACH_CRED, so every one would be refused anyway.

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
| `ldap` | `FLINT_NFS_GSS_LDAP_*` (off by default): users' identities from an LDAP directory ([below](#identities-from-a-directory)). |
| `nfs.enforcePermissions` | `FLINT_NFS_ENFORCE_PERMISSIONS` for every NFS server: `enforce`, `warn` or `off`. The default is `enforce` with Kerberos on, and the server's `warn` with it off. |
| `attachedNodesOnly` | `FLINT_NFS_ATTACHED_NODES_ONLY` (default `true`), and the servers' read-only service account. |
| `requireMachCred` | `FLINT_NFS_REQUIRE_MACH_CRED` (default `true`): an EXCHANGE_ID that does not ask for SP4_MACH_CRED is refused. A value other than true or false stops the server. |

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

With a directory (`nfs.kerberos.ldap`), a user principal the directory
has may also connect, whether the list names it or not
([below](#identities-from-a-directory)).

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
  never an unchecked caller. A malformed map makes everyone nobody, and so
  does no map at all (a server run without `FLINT_NFS_GSS_IDMAP` and
  without a directory, by hand or in a rig). Until 2026-10-16 such a
  server gave Kerberos calls no uid, and the permission check then
  evaluated nothing (`tests/krb5/run-authz-gss.sh`).
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
knfsd does. The Linux client asks for it whenever `rpc.gssd` runs, and
by default the server refuses a client that does not ask.

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

**Every client must ask** (`nfs.kerberos.requireMachCred: true`, the
default with Kerberos). A client that does not ask for SP4_MACH_CRED
would get SP4_NONE, and an SP4_NONE session is bound to whatever
connection uses it first, so its id, seen on the wire, would be the
session. With `requireMachCred` the server refuses such an EXCHANGE_ID
with NFS4ERR_ACCESS, as it refuses a node the volume is not attached to,
and the client cannot mount. That covers a node without `rpc.gssd`, a
client that does not implement SP4_MACH_CRED, and one downgraded on
purpose. A client that asks without integrity gets NFS4ERR_INVAL, falls
back to SP4_NONE, and is refused then.

The Linux kernel asks whenever `rpc.gssd` is running, since it then makes
its client and does its state management over `krb5i`, whatever the
mount's `sec=`. So with `requireMachCred`:

| `minSec` | Mounts that work (with `rpc.gssd`) | Notes |
|---|---|---|
| `krb5i` (default) | `sec=krb5i`, `sec=krb5p` | The recommended floor: the data is signed too. |
| `krb5` | also `sec=krb5` | The client is still made over `krb5i` and bound to its machine; only the data calls go unsigned. |
| `""` | | Does not render: a `sec=sys` mount cannot ask. |

A non-Linux client must ask for SP4_MACH_CRED, over `krb5i` or `krb5p`,
or be served by an install with `requireMachCred: false`, where its
sessions are not bound.

## Identities from a directory

`idmap` names a few principals. For everyone else, `nfs.kerberos.ldap`
looks users up in an LDAP directory with RFC 2307 attributes, as idmapd
or SSSD do for knfsd. Each NFS server does the lookup itself and caches
the answers.

```yaml
nfs:
  kerberos:
    ldap:
      enabled: true
      url: ldaps://ldap.corp.example
      userBase: ou=People,dc=corp,dc=example
      groupBase: ou=Groups,dc=corp,dc=example
      bindDN: cn=flint-nfs,ou=Services,dc=corp,dc=example
      bindPasswordSecret: {name: flint-ldap-bind, key: password}
      ca: {configMap: corp-ca, key: ca.crt}
```

The bind password Secret and the CA ConfigMap (or Secret) must be in the
NFS namespace. The servers read the password from the Secret, so it is
never in a pod spec or in the controller's env.

- **Order:** an `idmap` rule wins, the machine principals included (they
  are `idmap` rules, `machineIdentity`). Otherwise the directory decides,
  and a principal it does not have is `anonIdentity`, as before.
- **Who is looked up:** only user principals (`name@REALM`, one
  component) of `realms` (default: `realm`). `host/`, `nfs/` and other
  service principals never are.
- **Matching:** `alice@CORP.EXAMPLE` is the entry with `uid=alice` that
  matches `userFilter` (default `(objectClass=posixAccount)`). With
  `principalAttribute: krbPrincipalName`, it is the entry whose
  `krbPrincipalName` is `alice@CORP.EXAMPLE` (FreeIPA, or a KDC with an LDAP
  backend). The entry must name the principal exactly. LDAP compares
  `uid` without case and Kerberos with it, so `Alice@CORP.EXAMPLE` is not
  `alice`. Two matching entries are refused, never guessed between.
- **The identity:** `uidNumber` and `gidNumber`, plus the `gidNumber` of
  every `posixGroup` under `groupBase` that lists the user by `memberUid`
  (with `groupMembership: member`, by its DN, as RFC 2307bis does).
- **Never root:** an entry with `uidNumber` or `gidNumber` 0 is refused,
  and a group with gid 0 is left out, unless `allowRoot: true`.
  `nfs.rootSquash` still applies after that.
- **Who may connect:** a user the directory has may connect, even when
  `allowedPrincipals` does not name it. A user it does not have is decided
  by the list, as before. To admit fewer users, narrow `userFilter`, for
  example to a group's members
  (`(&(objectClass=posixAccount)(memberOf=cn=nfs,ou=Groups,dc=corp,dc=example))`).
- **Caching:** a user found is cached for `cacheTTL` (300 s), and a user
  not found for `negativeCacheTTL` (60 s). The principal is looked up
  when its context is established (RPCSEC_GSS_INIT), so the context's
  calls normally hit the cache.
- **When the directory is down:** a cached user keeps working until its
  TTL runs out. Anyone else is refused, never made anonymous. A new
  context is refused at RPCSEC_GSS_INIT. A call on an existing context
  whose user's TTL has run out gets `RPCSEC_GSS_CREDPROBLEM`, so the client
  re-establishes the context, and that is refused until the directory
  answers. Machine principals are `idmap` rules, so the nodes keep
  working.
- **Slow directories:** a lookup, including its wait for a slot, takes at
  most `timeout` (5 s), and at most `maxConcurrent` (4) run at once in
  each server. A lookup that times out counts as the directory being down.
- **Transport:** `ldaps://`, or `ldap://` upgraded with StartTLS, with the
  server's certificate checked against `ca` (or the image's system roots).
  `insecure: true` allows plain `ldap://`, which sends the bind password
  in the clear. Use it only in test rigs.
- **Malformed settings** (on the server, e.g. a bad URL) refuse every user
  the `idmap` does not name, rather than failing open. The chart refuses
  to render a missing `url` or `userBase`, a `bindDN` without its Secret,
  and both a CA ConfigMap and a CA Secret.

**What changes about "who may connect".** Without a directory, the
allowlist is a short list of names, and its strength is control over who
can create those principals in the realm. With one, the set of admitted
names is every user in the directory (narrowed by `userFilter`). The name
is still the link between a ticket and an identity: whoever can create
the principal `alice@CORP.EXAMPLE` in the KDC is alice, uid and all. So
the realm must be one whose user principals belong to the directory's
users. Keep the KerberosRealm's `allowedNamespaces` to trusted namespaces,
as before, but note that a namespace allowed to create a principal named
after a directory user can now become that user. Before, it could only
become `anonIdentity`. With `principalAttribute: krbPrincipalName`, the
directory names each user's principal itself, which is the stronger
binding when the KDC and the directory are the same system. Machine
principals do not change: they are still `idmap` rules, and the directory
never makes root.

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
  KerberosNodeKeytab is Ready. With krb5-operator 0.4 or later the node
  agent can do it: `nfs.kerberos.nodeKeytab.restartServices: [rpc-gssd]`
  has it start the unit once the keys are in and restart it when they
  change. Off by default, because the agent then mounts the host's
  systemd socket (`/run/systemd/private`) and could start any unit.
  Nodes must run systemd.
- **krb5-operator 0.4 or later is recommended:** each node's keys are in a
  Secret of its own that only that node's agent mounts, so root on one
  node cannot read another node's keys (0.3 kept every node's keys in
  one Secret that every agent mounted).
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

pNFS volumes are covered by `pnfs.kerberos`, with the limits [below](#pnfs).

## pNFS

`pnfs.kerberos.enabled` (off by default) gives the pNFS metadata server
(MDS) and the data servers (DSes) Kerberos: Phases 1 to 3 of the
[plan](plans/pnfs-kerberos-plan.md). A Kerberos client gets layouts, and
its data is striped across the DSes and signed (`krb5i`) or sealed
(`krb5p`) on every hop. With `pnfs.kerberos.dataServers: false` the DSes
do no Kerberos, a Kerberos client is given no layouts, and **all of its
I/O goes through the MDS** (Phase 1).

```sh
helm upgrade --install flint-csi flint-csi-driver-chart -n flint-system \
  --set pnfs.enabled=true --set pnfs.server.enabled=true \
  --set pnfs.kerberos.enabled=true \
  --set nfs.kerberos.realm=CORP.EXAMPLE --set nfs.kerberos.realmRef=corp
```

Then use the `flint-pnfs-krb5` StorageClass (the `flint-pnfs` class's
parameters, `mountOptions: [sec=krb5i]`). It needs the in-chart MDS
(`pnfs.server.enabled`) and krb5-operator, as above.

**Settings.** `pnfs.kerberos` has its own `minSec` (`krb5i`),
`requireMachCred` (`true`), `enforcePermissions` (`enforce`),
`attachedNodesOnly` (`true`), `dataServers` (`true`) and `storageClass`, with the same meaning and
render guards as RWX. Everything
else is `nfs.kerberos`'s: `realm`, `realmRef`, `realmNamespace`,
`clusterDomain`, `allowedPrincipals`, `idmap`, `machineIdentity`,
`anonIdentity`, `ldap`, `nodeKeytab`, `deletionPolicy`, and
`nfs.rootSquash`. `nfs.kerberos.enabled` itself may stay off. The MDS
runs in the release namespace, so the realm must allow that namespace,
and an LDAP bind password Secret and CA must be there too.

| Piece | What it is |
|---|---|
| `KerberosPrincipal flint-pnfs-mds-<i>` | One per MDS shard: `nfs/flint-pnfs-mds-<i>.<namespace>.svc.<clusterDomain>`, named after the shard's own Service. Its keytab is in Secret `flint-pnfs-mds-<i>-krb5`, which only that shard's pod mounts (`KRB5_KTNAME`). |
| MDS pods | The same server env as the RWX servers (floor, allowlist, idmap, anonymous identity, root squash, enforced permissions, SP4_MACH_CRED, the directory), from one chart helper. A bad floor or `FLINT_NFS_REQUIRE_MACH_CRED` stops the MDS at startup. |
| ControllerPublishVolume | Puts the volume's shard name in the publish context (`nfs.chert.us/krb5-server-name`): shard from the volume id's `~m<i>` suffix, none being shard 0. |
| node plugin | Before a `sec=krb5*` pNFS mount, maps the PV's MDS IP to that name in `/etc/hosts`, so `rpc.gssd` asks for the shard's principal. Without a name it refuses the mount; it never falls back to the RWX servers' name. Shard 0 has the same name whether the PV carries its per-shard Service's IP or the legacy `flint-pnfs-mds` alias's. |
| `KerberosNodeKeytab` | The nodes' keys, as for RWX; rendered once when both are on. |
| `KerberosPrincipal flint-pnfs-ds` | With `dataServers`: `hostPrincipals` for StatefulSet `flint-pnfs-ds`, a key per pod into Secret `flint-pnfs-ds-krb5`, each pod mounting only its own; the DS pods get the MDS's floor, allowlist and directory env. |
| node plugin, data servers | With `dataServers`: before a `sec=krb5*` pNFS mount, maps every DS Service address (multipath included) to that DS's principal name in `/etc/hosts` (`FLINT_PNFS_KRB5_DS`). |
| ServiceAccount `flint-pnfs-mds` | With `attachedNodesOnly`: the MDS reads PersistentVolumes, VolumeAttachments and Nodes (a read-only ClusterRole). |

**What it protects.** Every pNFS call from a Kerberos mount is
authenticated and signed (`krb5i`) or sealed (`krb5p`), metadata and data:
the floor refuses `sec=sys` mounts, the allowlist decides who connects, a
caller is its `idmap`/directory identity with permissions enforced, and
sessions are bound to their machine (SP4_MACH_CRED).

**Only the nodes a volume is attached to** (`pnfs.kerberos.attachedNodesOnly`,
the default; Phase 2). One MDS shard serves every pNFS volume it holds, each
a top-level name in its export (the PV's `pnfs.chert.us/volume-file`), so
the RWX servers' per-server check is not enough here. The MDS watches the
pNFS PersistentVolumes and their VolumeAttachments and keeps, per volume,
the nodes it is attached to, by their machine principals' hostnames:

- **Clients and sessions,** as for RWX: only a node attached to some pNFS
  volume makes an NFS client or session, and a node attached to none
  loses its sessions at its next call.
- **Per volume:** a PUTFH or RESTOREFH into a volume, and a LOOKUP, OPEN by
  name, CREATE, REMOVE, RENAME or LINK of a name in the export root or in a
  volume, are refused with NFS4ERR_ACCESS unless the session's node is
  attached to that volume. The export root itself is open to every
  attached node (it is what a mount starts from), but every name in it is
  a volume and only that volume's nodes reach it; a name that is no pNFS
  volume reaches nobody. A user's calls ride their node's session and
  reach what the node does. LAYOUTGET, READ and WRITE need a filehandle
  first, so they are covered.
- **A detach** takes effect at the node's next call into the volume: its
  reads, writes and opens there fail with EACCES (the MDS logs `refused:
  volume <v> is not attached to the node of this session`), while its
  other volumes keep working. A fresh mount of the volume may still
  succeed when the node has another volume of the same MDS mounted: the
  kernel shares one superblock per export and walks the path from its
  dentry cache, without a LOOKUP. Nothing under it can be read.
- **The detach itself** happens only after kubelet has unmounted the
  volume on that node (NodeUnpublish, NodeUnstage); on a Ready node
  Kubernetes never times that out. A pod's volume therefore stays
  reachable from its node until its last pod there is gone and unmounted.
- Until the first lists arrive, every node and volume is refused. With
  several shards, the node set is the union over every shard's volumes,
  and each shard only ever looks up its own.
- READDIR of the export root still lists every volume's name to an
  attached node (names only; nothing under them).

**The data servers** (`pnfs.kerberos.dataServers`, the default; Phase 3).
Each DS pod has its own principal,
`nfs/flint-pnfs-ds-<i>.flint-pnfs-ds-headless.<namespace>.svc.<clusterDomain>`:
one `KerberosPrincipal flint-pnfs-ds` with `hostPrincipals` mints a key
per pod of the StatefulSet into Secret `flint-pnfs-ds-krb5`, and each pod
mounts only its own (`subPathExpr: $(POD_NAME).keytab`; a rekey needs a
restart). The DS runs RPCSEC_GSS through the same front end as the MDS
(`nfs/gss_front.rs`: context, floor, call verifier, sequence window,
unseal; the reply signed or sealed), with the MDS's floor and allowlist;
without a keytab it refuses RPCSEC_GSS calls (it used to answer them
unauthenticated), and with the floor it refuses AUTH_SYS. Splice is off
under GSS (the reply body is signed or sealed in memory), so a `krb5i`
read costs a copy and a MIC, a `krb5p` read a seal. SECINFO_NO_NAME
answers what the floor accepts. The Linux client names a DS by the reverse
lookup of the address GETDEVICEINFO gives it, so the node plugin maps every
DS address (each DS's Service and its multipath Services, resolved through
the cluster DNS) to that DS's principal name in `/etc/hosts`, with its own
marker (`# flint-pnfs-ds-krb5`), before every `sec=krb5*` pNFS mount
(`FLINT_PNFS_KRB5_DS`). The MDS then hands Kerberos callers layouts
(`FLINT_PNFS_LAYOUTS_FOR_GSS=true`), but only while every registered DS
reported, at registration, a keytab and a floor of krb5 or stronger: a DS
that would take the client's AUTH_SYS fallback (no floor), or one from
before the report, withholds layouts from every Kerberos caller, and their
I/O goes through the MDS (`tests/krb5/results/pnfs-ds-posture-2026-10-16/`).

**Without the data servers: no layouts for Kerberos callers.** Measured
(plan, "Phase 0 results"):
a Linux client with a `sec=krb5i` mount that cannot get a GSS context with
a DS sends that DS its I/O as AUTH_SYS, silently, and our DSes take it.
Handing such a client a layout would put its data on the wire
unauthenticated while the mount looks protected. So with a keytab
configured (`KRB5_KTNAME`), the MDS answers LAYOUTGET from an RPCSEC_GSS
call with NFS4ERR_LAYOUTUNAVAIL, and the client does its I/O through the
MDS. This is what `pnfs.kerberos.dataServers: false` gives. The
decision is per call, by the call's credential flavor; AUTH_SYS
calls still get layouts (but the floor refuses `sec=sys` mounts). A
LAYOUTCOMMIT from a Kerberos caller, which holds no layout, is refused
with NFS4ERR_BADLAYOUT. `pnfs-block` (SCSI layout) volumes are not
affected: their data goes over NVMe-oF, which Kerberos does not cover.
`FLINT_PNFS_LAYOUTS_FOR_GSS=true` on the MDS hands layouts to Kerberos
callers again; it is for when the DSes do Kerberos (Phase 3), not before.

**What it does not protect yet.**

- **A DS checks the caller, not the layout.** With `attachedNodesOnly`
  a DS serves only nodes attached to SOME pNFS volume (it watches the same
  PVs and VolumeAttachments as the MDS, through ServiceAccount
  `flint-pnfs-ds`: clients and sessions from those nodes' machine keys, a
  session dropped when its node detaches from everything). But it does not
  know which volume a stripe belongs to: such a node, or an allowed user on
  it, can read or write any stripe file it can name, and a layout granted
  before a detach keeps working while the node has another pNFS volume.
  The plan's Phase 4 (a MAC'd DS filehandle) closes this.
- **DS sessions are not bound to the machine.** The DS answers SP4_NONE
  to an SP4_MACH_CRED request (Phase 3b).
- **The control path** between the MDS and the DSes (gRPC: registration,
  heartbeats, the F66 relay of a client's I/O to a file the MDS cannot
  layout, truncates) has no TLS (Phase 5); keep `pnfs.networkPolicy` on.
- **Files written under Phase 1 stay on the MDS.** A file a Kerberos client
  wrote while the DSes did no Kerberos (`dataServers: false`) lives in the
  MDS's export, and the F67 guard never pins a file that already has bytes:
  its layout requests are refused and its I/O keeps going through the MDS.

**Turning it on (migration).** The floor applies to every pNFS volume of
the release, so a `sec=sys` pNFS mount stops working. Worse, a node with a
live `sec=sys` pNFS mount reuses that AUTH_SYS client for new mounts of
the same MDS IP, which the floor refuses: **drain every `sec=sys` pNFS
mount from a node** (move its pods, or unmount) before enabling. A PV
keeps the mount options it was made with (`sec=sys` is the pNFS default),
so existing volumes need `sec=krb5i` in their `spec.mountOptions`; new
ones come from `flint-pnfs-krb5`. Volumes attached before the
upgrade have no MDS name in their VolumeAttachment: a `sec=krb5*` mount
of one fails until it is re-attached (detach and attach, e.g. by
rescheduling its pods off the node and back).

### Before Phase 1: what it would take

This analysis was written before Phase 1, against the code as it was then
(Rust paths under `spdk-csi-driver/src/`, chart templates under
`flint-csi-driver-chart/templates/`; line numbers are of that commit). It
is kept as the reasoning behind the phases: Phase 1 above is work item 1
of its bottom line (the MDS wiring) plus the no-layouts rule, which
replaces its "expected, not measured" fallback through the MDS with a
measured one. Items 2 to 5 are still open.

#### What the MDS already shares

The MDS serves NFS through the same function as the RWX server,
`nfs::server_v4::serve_tcp` (`pnfs/mds/server.rs:1855-1862`, calling
`nfs/server_v4.rs:381`), with a `CompoundDispatcher`
(`pnfs/mds/server.rs:389-390`) and its own `RpcSecGssManager`
(`pnfs/mds/server.rs:638-639`). So the per-call machinery is the same:

| Protection | In the shared path | What the MDS lacks |
|---|---|---|
| Floor | `serve_tcp` checks `sec_policy::active()` for AUTH_SYS/NONE (`nfs/server_v4.rs:789-798`) and GSS calls (`:1107-1121`). | `FLINT_NFS_MIN_SEC` is not set on the MDS (`pnfs-mds.yaml:273-308` has no Kerberos env). The MDS never calls `SecPolicy::validate_env()` (only `NfsServer::new` does, `nfs/server_v4.rs:177`), so a bad value does not stop it starting. `active()` still fails closed to `krb5p` (`nfs/sec_policy.rs:210-221`). |
| Keytab | `RpcSecGssManager::new(KRB5_KTNAME)`. | No `KRB5_KTNAME`, no principal, no keytab mount. With no keytab every `RPCSEC_GSS_INIT` is refused (`nfs/rpcsec_gss.rs:376`), so a `sec=krb5*` pNFS mount fails at the MDS today. The mount options let one ask for it (`pnfs_csi.rs:1588-1607`; the default is `sec=sys`, `:1591`). |
| Allowlist | Loaded inside `RpcSecGssManager::new` (`nfs/rpcsec_gss.rs:383`), enforced at INIT (`:484`). | Env only. |
| idmap, root squash | `serve_tcp` (`nfs/server_v4.rs:881-891`). | Env only. |
| mach_cred | In the dispatcher (`nfs/v4/dispatcher.rs:1105`, `:1176-1190`, `:1256`), so the MDS gets it once a `krb5i` EXCHANGE_ID reaches it. | Nothing. |
| Attached nodes only | The check is in the dispatcher (`nfs/v4/dispatcher.rs:1044-1048`). | The watch that fills the node set is spawned only by `nfs_main.rs:219-226`, never by `nfs_mds_main.rs`. With the env set, the MDS would refuse every node forever. The set is also per PersistentVolume, and one MDS (per shard) serves every pNFS volume, each as a directory of one export at one ClusterIP (`main.rs:4574-4577`). It would have to become a per-filehandle check (at PUTFH, against the volume the handle is in), not a per-client one. |
| Hosts entry | `nfs_krb5::prepare_node_mount` is called only on the RWX path (`main.rs:4687`). The pNFS mount (`main.rs:4593-4599`) does not call it. | `rpc.gssd` could not name the MDS. |
| Per-volume keys | Not on main yet (flint PR #15, open). | Not applicable as such. `rpc.gssd` picks the principal from the server's address, and every pNFS volume shares the MDS ClusterIP. At most there is one key per MDS shard. |

**The MDS is wiring, not new protocol.** It needs a `KerberosPrincipal`,
the keytab and the same env (`nfs_krb5::server_pod_parts`) in
`pnfs-mds.yaml`, `SecPolicy::validate_env()` at MDS startup, a hosts
entry for the MDS ClusterIP before the pNFS mount, and an attached-nodes
check that works per volume.

#### The data servers: no RPC authentication

The DS has its own minimal NFS loop (`pnfs/ds/server.rs:231-361`), not
`serve_tcp`.

- **The credential is never read.** `dispatch_minimal_nfs` checks only
  program and version (`pnfs/ds/server.rs:333-361`). There is no
  RPCSEC_GSS: a GSS NULL (context INIT) gets a plain success with no
  `rpc_gss_init_res`, and every reply carries an AUTH_NONE verifier
  (`pnfs/ds/server.rs:1693-1696`). AUTH_NONE, AUTH_SYS and a GSS
  credential are all served alike. `docs/pnfs-operator-runbook.md`
  ("Trust model") says so: the trust boundary is the network.
- **SECINFO_NO_NAME advertises `krb5`** (service none) that the DS
  cannot do (`pnfs/ds/server.rs:791-811`).
- **No stateid check.** The DS decodes the stateid and drops it
  (`let _stateid`, `pnfs/ds/server.rs:707`). The DS test pins this on
  purpose (`pnfs/ds/server.rs:2060-2078`): a `[PUTFH, READ]` compound
  with AUTH_NULL, no SEQUENCE and a stateid the DS never saw returns the
  data (`:2157-2170`).
- **The filehandle is not authenticated.** A pNFS DS handle is a version
  byte, instance id, file id and stripe index, with no MAC
  (`nfs/v4/filehandle_pnfs.rs:37-57`).
- **No state protection.** The DS answers SP4_NONE whatever the client
  asks for (`pnfs/ds/server.rs:438-439`, `:513-514`).

Today a peer that can reach a DS on 2049 and knows or guesses a file id
can read and write the stripe. Only the NetworkPolicy
(`pnfs.networkPolicy`, **off by default**, `values.yaml:647-648`) limits
who can reach it.

What a Linux client would do with a `sec=krb5i` pNFS mount: for a files
layout the kernel opens DS connections with the MDS mount's auth flavor,
so it would ask `rpc.gssd` for a context with each DS. That would fail
against this DS. The client would then mark the device unavailable and
send I/O through the MDS. This is expected from the kernel source, **not
measured**: data would likely still flow, but through the MDS, so pNFS
would be quietly off. The MDS can proxy READs from the DSes
(F66, `pnfs/ds/server.rs:1496-1497`), and whether every write path works this way
is unknown.

For the DS to carry the same protections, it needs:

1. **RPCSEC_GSS in the DS loop**, with the floor: either move the DS onto
   `serve_tcp`'s GSS layer (`handle_rpcsec_gss_call`, `nfs/server_v4.rs:1052`)
   or embed `RpcSecGssManager`. The DS's splice fast path assumes
   `rpc_gss_svc_none` (`pnfs/ds/io.rs:469-476`). Under `krb5i` the READ
   reply needs a MIC over the payload, and under `krb5p` it must be
   sealed, so splice turns off, as it does on the RWX lane.
2. **A principal per DS.** The client names the DS from the address in
   GETDEVICEINFO: a per-DS Service name (`pnfs-ds.yaml:202-203`,
   `FLINT_DS_ADVERTISE_ADDR`), resolved by the MDS to a ClusterIP
   (`pnfs/protocol.rs:687-696`, `endpoint_to_uaddr`). Multipath DSes add more names
   (`FLINT_DS_EXTRA_ADVERTISE_ADDRS`). `rpc.gssd` reverse-resolves each IP
   on the node, so every node needs a hosts line for every DS address,
   multipath ones too, *before* the kernel's first LAYOUTGET. That set
   changes as DSes register, so the node plugin would have to learn it
   from the MDS. Each DS would need `nfs/<ds-name>` in its own keytab. One
   shared name for all DSes would be simpler, but a single DS pod's key
   would then pass for every DS.
3. **Authorization, not only authentication.** With Kerberos the DS
   would know which node is calling, but still not whether that node
   holds a layout for the file. The allowlist and attached-nodes-only
   (restricted to machine principals and the volume's nodes) can bound
   that. Real per-file authorization needs the DS to check the layout
   stateid or the filehandle, through a MAC the MDS mints and the DS
   verifies, or a stateid table the MDS pushes. The pin test above says
   that work must also accept the delegation stateids the MDS mints.
4. **idmap does not matter on the DS** as long as it does no permission
   checks: it trusts the MDS's layout grant. A user's I/O does reach the
   DS with the user's own ticket, so the allowlist must admit users there
   as on the MDS.
5. **mach_cred on the DS**, if the floor is `krb5i`. Otherwise a sniffed
   DS session id can be used from another connection, as before
   SP4_MACH_CRED on the RWX server.

#### The MDS to DS control path

The control path is tonic gRPC over **plain TCP**, with no TLS. That
covers the MDS's `MdsControl` on 50051 (`pnfs/mds/server.rs:1824-1826`),
the DS's `DsControl` (`pnfs/ds/server.rs:1353-1358`) and the DS
registration client (`pnfs/ds/registration.rs:41-58`). Authentication is
an optional shared Bearer token, `FLINT_PNFS_CONTROL_TOKEN`, checked by
an interceptor (`pnfs/mds/server.rs:1794-1820`, `pnfs/grpc.rs:84-96`). It
is on by default (`values.yaml:632-633`) and is one random secret for
every party (`pnfs-security.yaml`). Since nothing encrypts the
connection, anyone who can see pod traffic can read the token and then
register a DS, so that new files stripe onto it. Kerberos does not touch
this path. Fixing it means mTLS (a per-DS identity, with the MDS
checking it against the device id at registration) or at least TLS. It
is separate from Kerberos, but without it Kerberos on 2049 can be
bypassed by registering a rogue DS.

#### Layouts

- **Files layout** (`LAYOUT4_NFSV4_1_FILES`) is the only NFS layout
  served. `FS_LAYOUT_TYPES` advertises FILES or SCSI
  (`nfs/v4/operations/fileops.rs:1209-1224`), and no `ff_layout4` body is
  generated (`:1199`). FlexFiles is accepted only on LAYOUTRETURN
  (`nfs/v4/dispatcher.rs:4225`, `pnfs/mds/operations/mod.rs:584`). A
  files layout carries device ids and filehandles, and no credential.
  Its security is entirely the DS's.
- **FlexFiles**, if it were adopted, carries `ffds_user`/`ffds_group`
  per mirror (RFC 8435 §5.1). In the loosely coupled model the client
  sends those as synthetic AUTH_SYS credentials to the DS, which is
  incompatible with a Kerberos floor on the DS. Only tightly coupled
  storage, where the client uses its own RPCSEC_GSS credential and the
  DS knows the MDS's stateids, fits. So FlexFiles is no shortcut here.
- **GETDEVICEINFO** carries universal addresses (IPv4 and port), not
  names (`pnfs/protocol.rs:687-696`, `endpoint_to_uaddr`). That is why the node needs the
  hosts lines in item 2 above. Kerberos service principals are DNS names.
- **SCSI layout (`pnfs-block`)** data goes over NVMe-oF, not NFS, so
  Kerberos does not apply. That path is guarded by the per-subsystem
  host-NQN allow-list (`docs/pnfs-operator-runbook.md`, "Port 4420").

#### Bottom line

Work items, in order of how much they buy:

1. **MDS, configuration only:** principal, keytab and Kerberos env in
   `pnfs-mds.yaml`; `validate_env()` at MDS startup; the hosts entry for
   the MDS ClusterIP on the pNFS mount path; a pNFS StorageClass with
   `sec=krb5i`. This gives the floor, allowlist, idmap and mach_cred for
   metadata. The data would still fall back to the MDS (expected, not
   measured).
2. **Attached-nodes-only on the MDS:** a per-volume check at
   PUTFH/LAYOUTGET, fed by every pNFS PV's VolumeAttachments, in place of
   the per-server client gate.
3. **DS Kerberos:** a GSS layer and floor in the DS loop, `nfs/<ds>`
   keys per DS, and hosts lines for every DS address, multipath included,
   kept current from the MDS. Splice is off under `krb5i`/`krb5p`.
4. **DS authorization:** verify the layout stateid or a MAC'd filehandle
   on the DS, with the delegation-stateid caveat. Without this, item 3
   authenticates nodes but does not bind I/O to layouts.
5. **Control path:** TLS or mTLS on 50051 and the DS control port, with
   a per-DS identity checked at registration.
6. **Per-volume keys:** not meaningful for pNFS (one address per MDS
   shard). Per-shard and per-DS keys are what the protocol allows.

Until 1 to 5 exist, the only protection for pNFS data is the network:
turn on `pnfs.networkPolicy` (`docs/pnfs-operator-runbook.md`, "Trust
model").

## Tests

- **Chart defaults:** `tests/system/chart-kerberos-defaults.sh` renders
  the chart with Kerberos off, on, and with each opt-out, and checks the
  servers' settings, including the directory's (the password only ever a
  Secret reference) and the settings it refuses to render (`helm` only;
  CI runs it).
- **Directory, not yet in kind:** the kind suite has no LDAP server yet.
  The plan is an OpenLDAP pod in `tests-nfs-krb5/` with a `posixAccount`
  carol (3000:3000) in a `posixGroup`, a principal carol with a ticket on
  a node, and these checks: carol connects without being in the
  allowlist, a file she creates is owned by 3000:3000, a principal the
  directory lacks is refused, and with the directory scaled to zero a
  new principal is refused while carol works until her TTL runs out.
- **pNFS:** `chart-kerberos-defaults.sh` also renders `pnfs.kerberos`
  (off: no change; on: a principal and keytab per shard, the server env
  by the names in `tests/system/krb5-server-env-names.txt`, which a unit
  test also holds `server_pod_parts` to, attached-nodes-only with the
  MDS's service account and read-only ClusterRole, the StorageClass, one
  node keytab; and the guards). `node_authz` unit tests: a volume admits
  only its own nodes, before the first list none, and the fold from PVs
  and VolumeAttachments. Unit tests: the
  dispatcher's `gss_callers_get_no_files_layout_when_withheld`,
  `pnfs::mds::krb5` (the MDS env, refusals to start) and `nfs_krb5`
  (the shard name, the strict hosts entry). In the VM rig,
  `tests/krb5/pnfs-e1.sh` (results in `tests/krb5/results/pnfs-phase1-2026-10-16/`):
  under `sec=krb5i` no RPC reaches a DS and the data is on the MDS, while
  `sec=sys` still stripes. On kind, see below.
- **Session binding:** unit tests in `dispatcher.rs` (`mach_cred`) and,
  against the real kernel client in the VM rig,
  `tests/krb5/run-mach-cred.sh` (connection killed, server restarted,
  session id stolen by a user with a valid ticket, all with
  `FLINT_NFS_REQUIRE_MACH_CRED` on; a `sec=krb5` mount still bound; an
  SP4_NONE client refused; and a knob-off SP4_NONE control).
- **Unit tests:** `nfs_krb5.rs` covers flavor detection, `/proc/mounts`
  parsing, the hosts file rewritten in place and only our lines (each IP
  with its own name), the server pod's keytab, floor and directory
  settings (the bind password as a Secret reference), the per-volume
  server name, and which volumes get a principal of their own.
  `gss_ldap.rs` covers the RFC 2307 parsing, exact
  matching, root refusal, caching, the outage and timeout behaviour, and
  bounded concurrency, against an in-memory directory. `gss_idmap.rs`
  covers the order (`idmap`, directory, anonymous) and who may connect.
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
- **kind, pNFS:** `KRB5=1 PNFS=1 tests/system/kind-nfs-only.sh` also
  builds the `flint-pnfs` image, installs the in-chart MDS (one shard) and
  two DSes with `pnfs.kerberos.enabled` (and `nfs.kerberos.enabled` off),
  loads `nfs_layout_nfsv41_files` on the host, and runs
  `tests-pnfs-krb5/`: a `flint-pnfs-krb5` volume mounts with `sec=krb5i`
  on two nodes and the pods share files; the shard's `KerberosPrincipal`
  is `nfs/flint-pnfs-mds-0.flint-system.svc.cluster.local`, its keytab
  Secret is the only one the MDS pod mounts and holds that principal
  alone, the VolumeAttachments name it, and each node maps the PV's MDS
  IP to it; 8 MiB written under `krb5i` are allocated in the MDS's export
  and not on the DSes, with the MDS's F68a meter counting refused
  LAYOUTGETs and no grants; a `sec=sys` pNFS mount is refused by the
  `krb5i` floor; alice is refused by the allowlist; with the data servers
  doing Kerberos, each DS's keytab holds its own principal alone, 8 MiB
  written under `krb5i` are striped across both DSes (about 4 MiB each,
  the MDS keeping a sparse stub), every call they log is RPCSEC_GSS and
  none is refused by their floor; a second volume used
  only on one node cannot be mounted from the other (which holds a session
  for the first volume), the MDS logging the refusal, and after its pod is
  deleted, the volume leaves the node's `volumesInUse` and is detached, and
  the first node can no longer read it (the MDS refusing it), while both
  keep the first volume. `PNFS=1` alone runs
  `tests-pnfs-only/`, the AUTH_SYS control: the same write is striped on
  both DSes and the MDS keeps a sparse stub. Not in CI yet.

## Tested on

Only two kinds of host have run this:

- **kind, Debian-based nodes.** `KRB5=1 tests/system/kind-nfs-only.sh`
  builds `tests/system/krb5/kind-node.Dockerfile`: `kindest/node:v1.34.0`
  (`kind-nfs-only.sh:61`) plus `nfs-common` and `krb5-user` from apt.
  CI runs it on `ubuntu-latest` (`.github/workflows/system-chainsaw.yml:63`,
  `:123-131`). A kind node uses the host's kernel, so the client kernel
  is the runner's (or Docker Desktop's VM's), and systemd runs inside the
  node container.
- **Lima, Ubuntu 24.04 aarch64** (`tests/krb5/results/authz-gss-2026-09-05.log:61`),
  for every drill in `tests/krb5/` (`setup-mount.sh`, `run-mach-cred.sh`,
  ...): kernel 6.8, `nfs-utils` 2.6.4, MIT KDC.

Nothing has run on RHEL, Fedora, Rocky, Alma, SUSE, Amazon Linux,
Bottlerocket, Flatcar, Talos, COS or any SELinux-enforcing host. Below is
what we know or expect to differ. **None of it is measured.**

### Restarting `rpc-gssd`

The `rpc-gssd` unit starts only when `/etc/krb5.keytab` exists, so a
node that boots before its KerberosNodeKeytab is Ready has no `rpc.gssd`
until someone starts it. `systemctl try-restart` would not do: it
restarts only a running unit, and this one never started.

krb5-operator 0.4.0 adds `KerberosNodeKeytab.spec.restartServices`, which
the chart passes as `nfs.kerberos.nodeKeytab.restartServices` (off by
default; see [Node prerequisites](#node-prerequisites)). The node agent
asks the host's systemd to start each unit when the agent starts, and to
restart it whenever the agent rewrites the keytab. It uses systemd's
private socket (`/run/systemd/private`, hostPath type `Socket`), so it
needs no `hostPID`, no capabilities and no `privileged`. The operator's
README also gives a host path unit (`PathChanged=/etc/krb5.keytab`) for
clusters that would rather not grant that. The kind suite sets
`[rpc-gssd]` and only checks that the unit is active.

**Non-systemd hosts** (OpenRC, runit, or hosts with no init you can
reach) have no `rpc-gssd` unit to restart. `rpc.gssd` has to be started
however that host starts it, once the keytab is there. Minimal,
immutable OSes may not ship `rpc.gssd` at all. Without it the kernel
does no Kerberos, and the default floor refuses the mount.

### SELinux (RHEL, Fedora, and others, enforcing)

Two components write host files:

- **`/etc/hosts`, by the node plugin.** The `flint-csi-driver` container
  in `flint-csi-node` is `privileged: true` (`flint-csi-driver-chart/templates/node.yaml:154`,
  `:443`, `:639`, per profile). It mounts the host's `/etc/hosts` as a
  single-file `hostPath` (`node.yaml:749-752`, at `/host/etc/hosts`,
  `_helpers.tpl:203-208`). A privileged container runs as `spc_t`
  (unconfined), so the write should be allowed. A `container_t` process
  could not write `net_conf_t`. The plugin **writes the file in place**:
  `flock`, truncate, write, `fsync` on the same inode (`nfs_krb5.rs:94-121`),
  because a bind-mounted file cannot be renamed over. The file keeps its
  `net_conf_t` label (expected; not checked on an SELinux host).
- **`/etc/krb5.keytab` (and the `krb5.conf` drop-in), by krb5-operator's
  node agent.** In 0.3.0 the agent is not privileged. It runs as root
  with every capability dropped and SELinux type `spc_t`
  (krb5-operator `src/node.rs:316-326`), and mounts the host's directory
  (`/etc`), not the file. It **replaces the file by rename**: it writes
  `.krb5.keytab.krb5-operator.tmp` beside it and renames it over the old
  one (`src/agent.rs:77-97`). A new file takes its label from policy for
  the creating domain, the directory and the *new file's name*. The
  temporary file's name matches no named type transition for
  `krb5.keytab`, so it is expected to get the directory's `etc_t`, and the
  rename keeps that. The result is `/etc/krb5.keytab` labeled `etc_t`
  where policy expects `krb5_keytab_t`. We expect `rpc.gssd` (`gssd_t`)
  can still read `etc_t`, so this is more likely a keytab other confined
  domains can read (the file's mode, 0600 root, still holds) than a
  broken mount. A confined reader whose policy allows only
  `krb5_keytab_t` would be refused, though, and `restorecon` or an
  autorelabel would silently change the label later. Neither is
  verified. Run `restorecon -v /etc/krb5.keytab` after the agent writes
  it. The fix in the operator is to label the temporary file
  (`setfscreatecon` to `krb5_keytab_t`, or the label of the file it
  replaces) or to rewrite the file in place when it exists.

### gssproxy (RHEL, Fedora)

- **Server side: does not apply.** On these distros knfsd's
  `rpc.svcgssd` is replaced by gssproxy (`use-gss-proxy`). flint's NFS
  server is a userspace process with its own keytab, not knfsd, so
  nothing on the host serves its contexts.
- **Client side: may apply.** The client still uses `rpc.gssd`, which on
  these distros can be told to get credentials from gssproxy
  (`GSS_USE_PROXY=yes` in `/etc/sysconfig/nfs` on older releases,
  `[gssd] use-gss-proxy=1` in `/etc/nfs.conf`). gssproxy's `nfs-client`
  service then reads its own configuration
  (`/etc/gssproxy/24-nfs-client.conf`: `mechs = krb5`, a client keytab
  that is usually `/etc/krb5.keytab`). After the keytab changes,
  gssproxy, not only `rpc-gssd`, may need a restart or reload to see it.
  With krb5-operator's `restartServices` (above), name `gssproxy` as
  well as `rpc-gssd`. This comes
  from the nfs-utils and gssproxy documentation, not from a test. Whether
  `use-gss-proxy` is on by default for `rpc.gssd`, and whether gssproxy
  rereads a replaced keytab by itself, are unknown here.
