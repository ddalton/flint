# Kerberos for flint lite (FlintShare hubs): the plan

Status: plan, 2026-10-17. Nothing here is built. The radar's krb5p column
is the lite hub, and three of its Security cells wait on this: "the lite
operator sets no map yet, so on a hub every Kerberos user is nobody until
one is set by hand" (Rights), "no CRD field, Helm value or other in-tree
tooling to set it" (Misconfiguration), and attached-nodes-only "exists for
flint's RWX volume servers, not for the hub" (Tenancy).

## What is already there

The hub is `flint-pnfs-mds` in standalone mode (`lite_operator/render.rs`,
`mode: standalone`, no data servers), and that binary already does
Kerberos when its env says so: `MdsSecEnv::from_env` validates the floor,
`FLINT_NFS_REQUIRE_MACH_CRED` and `KRB5_KTNAME` at startup and stops on a
bad value (`pnfs/mds/krb5.rs`); it serves through
`nfs::server_v4::serve_tcp`, so GSS (`nfs::gss_front`), the floor, the
allowlist, the idmap, LDAP, root squash and enforced permissions all apply
(pNFS Phase 1, PR #22). What is missing is everything around it:

- **The operator** renders a fixed env (`RUST_LOG`, `POD_NAME`,
  `FLINT_FH_KERNEL`, `FLINT_NFS_ENFORCE_PERMISSIONS` when asked, the proxy
  and tier settings). No principal, no keytab, no floor, no map. Its only
  security field is `spec.security.enforcePermissions` (off by default).
- **The single-hub chart** (`flint-lite-chart/templates/hub.yaml`) renders
  the same pod independently; a golden parity test (`render.rs`) keeps the
  two in step.
- **Clients** mount a hub through a static `nfs:` PV (kubelet's mount, its
  `mountOptions`), by hand, or through the nfs-proxy. No flint code runs on
  a client node for a direct mount, so nothing writes the `/etc/hosts` line
  that tells `rpc.gssd` which principal an IP belongs to (the RWX node
  plugin's `nfs_krb5::prepare_node_mount`).
- **The nfs-proxy** refuses RPCSEC_GSS (`AUTH_TOOWEAK`,
  `nfs_proxy/server.rs`); its client identity is RPC-with-TLS mTLS. A GSS
  context is bound to the server principal, so Kerberos through the proxy
  means the proxy ends GSS itself (the proxy design's "v2").

## Ground rules

- **Per share, off by default:** `spec.security.kerberos` on a FlintShare.
  Present means Kerberos with the RWX defaults: floor `krb5i`,
  SP4_MACH_CRED required, permissions **enforced** (a new Kerberos share
  has no installed base to break, unlike `enforcePermissions` alone), root
  squash as `nfs.rootSquash` (off).
- **Fail closed:** an unknown floor, a missing realm, or Kerberos together
  with the nfs-proxy is a status condition (`Ready=False`, reason
  `KerberosInvalid` / `KerberosBehindProxy`) and no Deployment, never a hub
  that silently serves `sec=sys`.
- **The same server code and helpers as RWX and pNFS:**
  `nfs_krb5::ServerSecurity` + `server_pod_parts` for env, volume and
  mount; `ensure_volume_principal`'s server-side apply for the
  KerberosPrincipal. Not `FLINT_NFS_ATTACHED_NODES_ONLY` (fatal on a
  standalone hub by design) and not `FLINT_PNFS_LAYOUTS_FOR_GSS` (no
  layouts).
- **Owned by the FlintShare, not the Deployment:** hibernation deletes the
  Deployment, Service and ConfigMap; the principal and its keytab must
  survive a park/wake, and the Service's DNS name (so the principal) stays
  the same across a wake although its ClusterIP does not.

## L1: the operator

CRD (`crd.rs`, `SecuritySpec`):

```yaml
spec:
  security:
    kerberos:
      realmRef: {name: corp, namespace: krb5-system}   # a krb5-operator KerberosRealm
      minSec: krb5i            # krb5 | krb5i (krb5p cannot be a floor: Linux does state ops over krb5i)
      requireMachCred: true
      allowedPrincipals: []    # default host/*@R, nfs/*@R plus the idmap's principals
      idmap: []                # "alice@CORP=1000:1000[:g,g]"
      anonIdentity: "65534:65534"
      machineIdentity: "0:0"
      rootSquash: false
      ldap: {...}              # as nfs.kerberos.ldap; bind password from a Secret in the share's namespace
      serverNames: []          # extra names the hub answers to (an advertiseAddress host is added)
```

Reconcile:
- Server-side apply `KerberosPrincipal <share>-krb5` with
  `nfs/<service>.<ns>.svc.<clusterDomain>`, plus `nfs/<host>` for
  `service.advertiseAddress`'s host when it is a DNS name and for each
  `serverNames` entry; keytab Secret `<share>-krb5`; owner reference to the
  FlintShare. Delete with the share.
- The hub pod gets `server_pod_parts` (keytab volume, `KRB5_KTNAME`, the
  floor, allowlist, idmap, anon/machine identity, root squash,
  SP4_MACH_CRED, LDAP) and `FLINT_NFS_ENFORCE_PERMISSIONS=enforce` unless
  `enforcePermissions: false` is set explicitly.
- Wait for the keytab Secret before rendering the Deployment (condition
  `KerberosKeytabPending`), so the hub never starts without its key.
- `status.kerberos`: `serverName` (the name clients must mount by),
  `principals`, `realm`.
- Refuse Kerberos with `nfsProxy` (above).
- Operator chart: RBAC for `kerberosprincipals.krb5.ddalton.dev`
  (get/list/watch/create/patch/delete), `.owns()` it so a deleted
  principal is re-applied; values `kerberos.clusterDomain`.

Tests: render units (env, volumes, principal JSON, both names, no
attached-nodes-only, enforce default), CRD CEL for `minSec`, reconcile
conditions (proxy, missing Secret), the parity test's golden updated.

## L2: the single-hub chart

`flint-lite-chart`: `kerberos.*` values with the same meaning, a
`KerberosPrincipal` template, the same env and mounts in `hub.yaml`;
the parity test covers a Kerberos share. Chart test legs for the render
guards (floor values, proxy refusal).

## L3: client nodes

A direct mount needs, on each client node: a machine keytab
(`KerberosNodeKeytab`, as for RWX), `rpc.gssd` running, a krb5.conf
mapping `.svc.<clusterDomain>` to the realm (krb5-operator writes it), and
the server **name**: the kubelet mounts from the host's resolver, which
does not resolve `*.svc.cluster.local`, and a PV naming the ClusterIP
makes `rpc.gssd` reverse-resolve an IP no principal carries.

- `flint-nfs-client-chart` (the DaemonSet that already runs on client
  nodes for the proxy's mTLS) gains `kerberos.enabled`: renders the
  `KerberosNodeKeytab` and a hosts sync that writes
  `<ClusterIP> <serverName>` for every Kerberos FlintShare it can read
  (marker `# flint-lite-krb5`, as `# flint-pnfs-ds-krb5`), refreshed on
  the share's status (a wake's new ClusterIP).
- PVs name the hub by `status.kerberos.serverName`; docs give the PV.
- Cross-cluster (`advertiseAddress`): the name must be real DNS the remote
  nodes resolve; the principal carries it (L1). No hosts sync there.

## L4: tests

- **kind `LITE=1 KRB5=1`:** krb5-operator + KDC as the pNFS suite does;
  a FlintShare with `security.kerberos`; a client pod on a static PV with
  `sec=krb5i`: mounts and writes; a `sec=sys` PV is refused by the floor;
  alice (not allowed) refused; bob (idmap) gets his uid and is refused a
  root-owned directory (enforce); hibernate and wake, and the mount works
  after (the hosts sync followed the new ClusterIP). Data, not mount
  status, after the wake (the shared-superblock lesson from Phase 2).
- **Proxy refusal:** a FlintShare with Kerberos and the proxy stays
  `Ready=False`, no Deployment.
- **VM rig:** the hub binary with the lite env against the real KDC and a
  Linux client (`tests/krb5`), as Phase 1 did for the MDS.

## Later, not in this plan

- **Kerberos through the nfs-proxy** (the proxy ends GSS with its own
  principal and speaks AUTH_SYS or its own GSS to the hubs): the proxy
  design's v2.
- **Per-node scoping on a hub** (the hub's analogue of attached-nodes-only):
  a hub serves clients from other clusters, so VolumeAttachments do not
  describe its clients; the per-share principal allowlist is the control.
- **B12 prefix reuse** (Tenancy) is a separate defect.

## Expected radar effect (to be rescored after the build, not before)

- Misconfiguration: a CRD field and chart values with secure defaults and
  fail-closed validation, where today there is none.
- Rights: the map and LDAP from the CRD, enforced by default.
- Tenancy: a per-share principal allowlist instead of reachability.
- Day-2 and consumer cost rise (keytabs, rpc.gssd, hosts sync on client
  nodes), as the column already assumes.

## Order and size

L1 → L2 → L3 → L4; L1 and L2 are about a week together, L3 two to three
days, L4 two days.
