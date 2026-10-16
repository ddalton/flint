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
| `KerberosPrincipal flint-nfs-server` | `nfs/flint-nfs.<nfs namespace>.svc.<clusterDomain>`, the one principal every NFS server pod uses. Its keytab is in Secret `flint-nfs-krb5-keytab`, which each server pod mounts (`KRB5_KTNAME`). |
| `KerberosNodeKeytab <release>-nfs-clients` | `host/` and `nfs/<node>` for every selected node, installed into `/etc/krb5.keytab` by the operator's DaemonSet, along with the realm's `krb5.conf`. That `krb5.conf` maps `.svc.<clusterDomain>` to the realm, which `rpc.gssd` needs in order to pick the server's realm. |
| node plugin | Before a `sec=krb5*` mount, writes `<volume ClusterIP> <server name>` into the node's `/etc/hosts`. |
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

pNFS volumes are not covered ([below](#pnfs)).

## pNFS

None of the above protects a pNFS volume today. The MDS shares most of
the code, but nothing turns it on, and the data servers do no RPC
authentication at all. Rust paths are under `spdk-csi-driver/src/`, chart
templates under `flint-csi-driver-chart/templates/`, as of this commit.

### What the MDS already shares

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

### The data servers: no RPC authentication

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

### The MDS to DS control path

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

### Layouts

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

### Bottom line

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
  servers' settings (`helm` only; CI runs it).
- **Session binding:** unit tests in `dispatcher.rs` (`mach_cred`) and,
  against the real kernel client in the VM rig,
  `tests/krb5/run-mach-cred.sh` (connection killed, server restarted,
  session id stolen by a user with a valid ticket, and an SP4_NONE
  control).
- **Unit tests:** `nfs_krb5.rs` covers flavor detection, `/proc/mounts`
  parsing, the hosts file rewritten in place and only our lines, and the
  server pod's keytab and floor.
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
  0600 file. CI runs it after the nfs-only suite.

## Tested on

Only two kinds of host have run this:

- **kind, Debian-based nodes.** `KRB5=1 tests/system/kind-nfs-only.sh`
  builds `tests/system/krb5/kind-node.Dockerfile`: `kindest/node:v1.34.0`
  (`kind-nfs-only.sh:49`) plus `nfs-common` and `krb5-user` from apt.
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
until someone starts it. The kind suite does that by hand after the
"Kerberos keys" step (`tests/system/kind-nfs-only.sh:199-210`,
`systemctl restart rpc-gssd` at `:207`). `systemctl try-restart` would
not do: it restarts only a running unit, and this one never started.

[krb5-operator PR #14](https://github.com/ddalton/krb5-operator/pull/14)
(stacked on its PR #12; not merged, not built, not released) adds
`KerberosNodeKeytab.spec.restartServices: [rpc-gssd]`, off by default.
The node agent asks the host's systemd to start each unit when the agent
starts, and to restart it whenever the agent rewrites the keytab. It uses
systemd's private socket (`/run/systemd/private`, hostPath type `Socket`),
so it needs no `hostPID`, no capabilities and no `privileged`. Its README
also gives a host path unit (`PathChanged=/etc/krb5.keytab`) for clusters
that would rather not grant that. flint pins
`KRB5_OPERATOR_VERSION=0.3.0` (`kind-nfs-only.sh:51`), so the suite
keeps the manual restart until an operator release has the field.

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
