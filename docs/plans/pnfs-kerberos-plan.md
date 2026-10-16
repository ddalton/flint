# Kerberos for pNFS (MDS and data servers): the plan

Status: plan, 2026-10-16. Nothing here is built. The gaps it closes are in
[nfs-kerberos.md, "pNFS"](../nfs-kerberos.md#pnfs). It assumes the RWX
Kerberos work is merged (PRs #15 per-volume keys, #18 required
SP4_MACH_CRED, #19 LDAP identities). Line numbers are `origin/main` at
`1b581372`.

## Ground rules

- **One switch, off by default:** `pnfs.kerberos.enabled`. It reuses
  `nfs.kerberos.{realm, realmRef, realmNamespace, clusterDomain,
  allowedPrincipals, idmap, machineIdentity, anonIdentity, nodeKeytab,
  ldap}`. `KerberosNodeKeytab` moves into a helper so it renders when either
  `nfs.kerberos` or `pnfs.kerberos` is on. With the switch off, nothing in
  the pNFS pods' spec changes.
- **One server env helper:** `flint.krb5ServerEnv` in `_helpers.tpl`, the
  same env and volumes as `nfs_krb5::server_pod_parts` (floor, allowlist,
  idmap, anon, root squash, enforce, mach-cred, LDAP env and CA). A unit
  test dumps the names `server_pod_parts` sets; a
  `chart-kerberos-defaults.sh` leg diffs them against the helper.
- **Regression guards first, in their own PR:** `helm template` golden
  files for `pnfs.enabled=true` with Kerberos off, and a golden-bytes test
  of `pnfs/ds/server.rs::assemble_reply` (`:1684`) for a fixed
  `[SEQUENCE, PUTFH, READ]` reply under AUTH_SYS. Every phase keeps both
  identical when its feature is off.

## Phase 0: experiments before building (about 1.5 days)

| # | Question | Experiment | Where |
|---|---|---|---|
| E1 | With a `sec=krb5i` pNFS mount and today's DS (no GSS), does I/O fall back to the MDS cleanly (F66 proxy, `pnfs/mds/operations/mod.rs:1342`, `:1388`), or stall? Does the device flap back every ~120 s? | MDS (keytab) and two DSes as musl binaries in the `tests/krb5` VM, DSes on loopback aliases 127.0.0.2/.3:2049 (each needs its own IP for its own name). Mount `sec=krb5i`, `dd` + fsx; watch the F68a meter, DS byte counters, `dmesg`. | Lima VM |
| E1b | **Possible regression today:** with `rpc.gssd` running (any node with RWX Kerberos on), does a `sec=sys` pNFS mount still reach the DSes, and how fast? The kernel may create the DS `nfs_client` with krb5i and fall back to AUTH_UNIX on EACCES. | Same rig, `sec=sys`, gssd running vs stopped: DS byte counters, time to first byte. | Lima VM |
| E2 | Which principal `rpc.gssd` asks for per DS (reverse lookup of the uaddr IP); machine credential for EXCHANGE_ID, the user's for READ/WRITE? Does DS EXCHANGE_ID ask for SP4_MACH_CRED, and is SP4_NONE accepted? One name for several multipath IPs? | Same rig plus `nfs/ds{1,2}.test` in the KDC and hosts lines. Read the KDC TGS-REQ log, `rpc.gssd -vvv`, `tcpdump`. No DS code needed. | Lima VM |
| E4 | Which DS error for a refused filehandle recovers cleanly (BADHANDLE/STALE → new LAYOUTGET, or ACCESS → MDS) without a livelock? | Scratch DS patch refusing one file_id; `tests/lima/pnfs/fallback-drill.sh`-style phases. | Lima pNFS rig |
| E5 | Does TokenReview of a pod-bound projected token return `authentication.kubernetes.io/pod-name` on kind v1.34? Does tonic 0.10 `serve_with_incoming` work with tokio-rustls 0.26? | `kubectl create token --bound-object-kind Pod` + TokenReview; a compile spike. | kind / laptop |

E1/E1b decide Phase 1's layout behaviour, E2 Phase 3's naming and SP4,
E4 Phase 4's error code, E5 gates Phase 5.

### Phase 0 results (2026-10-16): E1, E1b, E2 run

On a KVM Ubuntu 24.04 VM on the build box (kernel 6.8, nfs-utils 2.6.4,
MIT KDC 1.20), `tests/krb5/pnfs-e1.sh` with `rpcflav.py`: the MDS at
127.0.0.1:20490 (`nfs/flintsrv.flint.test` keytab), two DSes advertised as
127.0.0.2:20491 and 127.0.0.3:20492 (`ds1`/`ds2.flint.test` in
`/etc/hosts`), 64 MiB written with fsync, caches dropped, read back and
compared. Evidence: `tests/krb5/results/pnfs-phase0-2026-10-16/`.

| Arm | Read-back | Bytes allocated on | RPCs to each DS | Read |
|---|---|---|---|---|
| A `sec=sys`, gssd on | intact | DSes, 32 MiB each; MDS export 16 KiB | AUTH_SYS | 0.2 s |
| B `sec=sys`, gssd off | intact | same | AUTH_SYS | 0.2 s |
| C `sec=krb5i`, no DS principals | intact | same | **AUTH_SYS** (~590), AUTH_NONE NULL (~99) | 2.3 s |
| D `sec=krb5i`, DS principals exist | intact | same | **AUTH_SYS** (~585) after **~980 failed GSS context NULLs** | 1.5 s |

- **E1b: no regression.** An AUTH_SYS pNFS mount on a node running
  `rpc.gssd` behaves exactly as without it.
- **E2:** `rpc.gssd` names a DS by reverse lookup of the uaddr IP
  (`nfs/ds1.flint.test`), asking with the machine credential
  (`host/<node>`, uid 0 for root's I/O). Without the principal the KDC
  says "Server not found" (C); with it, gssd gets a ticket and our DS's
  non-GSS reply to the context INIT fails (D).
- **E1: the kernel does NOT fall back to the MDS.** When it cannot get a
  GSS context with a DS it **downgrades DS I/O to AUTH_SYS**, silently,
  data intact, reads 7–11× slower from the retries. So today a
  `sec=krb5i` pNFS mount sends its data unauthenticated and unsigned,
  and the DS takes it.

**Consequences for the plan:**
1. Phase 1 must ship with option **B** (no layouts for GSS sessions, so
   all I/O goes through the MDS under Kerberos), not A. Option A would
   put metadata under Kerberos while the data goes as AUTH_SYS — worse
   than nothing, because it looks protected. The F67 cost of B (files
   written in Phase 1 stay on the MDS) is accepted until Phase 3, or
   addressed by migrating them when Phase 3 lands.
2. Phase 3's DS must enforce the floor (refuse AUTH_SYS/NONE when a floor
   is set), and E1 must be rerun against a DS that refuses: whether the
   client then falls back to the MDS or fails the I/O is still unknown.

**E5 (TokenReview half), kind v1.34:** a token from `kubectl create token
<sa> --audience flint-pnfs-mds --bound-object-kind Pod ...` reviews as
`system:serviceaccount:<ns>:<sa>` with `extra` carrying
`authentication.kubernetes.io/pod-name`, `pod-uid`, `node-name` and
`node-uid`; the same token reviewed for audience `flint-pnfs-ds` is
refused ("token audiences [flint-pnfs-mds] is invalid"). So Phase 5's
per-DS identity (pod name == `device_id`) needs no per-pod certificate.
Caveat: the token still reviewed `authenticated: true` 3 s after its pod
was deleted (the apiserver caches token results briefly), so revocation
is not instant; Phase 5 must not rely on it. The tonic/rustls half of E5
is a compile spike, left to Phase 5.

## Phase 1: the MDS gets Kerberos

Gives the floor, allowlist, idmap, enforced permissions, root squash and
SP4_MACH_CRED for pNFS metadata. Data goes through the MDS until Phase 3
(E1 measures it).

Rust:
- `nfs_mds_main.rs` `async_main` (after `config.validate()`, ~`:181`):
  `SecPolicy::validate_env()` and `require_mach_cred_from_env()`, exit
  non-zero on a bad value (as `nfs/server_v4.rs:177`).
- `pnfs/mds/server.rs:389-397` `new_with_pnfs(...)`: add
  `.with_require_mach_cred(...)`, or #18's env does nothing on the MDS.
- `main.rs:2420` ControllerPublish, pNFS branch: put
  `nfs_krb5::SERVER_NAME_CONTEXT` =
  `flint-pnfs-mds-<shard>.<ns>.svc.<domain>` (shard from
  `pnfs_csi::parse_shard_suffix`, none = 0; base from new controller env
  `PNFS_KRB5_MDS_NAME_BASE`, set only when enabled).
- `main.rs` NodePublish, pNFS branch, between `build_pnfs_mount_opts`
  (`:4593-4594`) and `mount` (`:4599`): a strict
  `nfs_krb5::prepare_pnfs_node_mount` that errors when the name is missing
  (never the RWX shared name). No-op for `sec=sys`.

Chart:
- New `templates/pnfs-kerberos.yaml`: a `KerberosPrincipal
  flint-pnfs-mds-<i>` per shard (`nfs/flint-pnfs-mds-<i>.<ns>.svc.<domain>`,
  Secret `flint-pnfs-mds-<i>-krb5`); optional StorageClass
  `flint-pnfs-krb5` with `mountOptions: [sec=krb5i]`.
- `pnfs-mds.yaml:273-309`: `flint.krb5ServerEnv` + `KRB5_KTNAME`, keytab
  volume per shard (`:320`, `:337`). **No `FLINT_NFS_ATTACHED_NODES_ONLY`
  yet**: the MDS has no watcher and would refuse every node.
- `values.yaml`: `pnfs.kerberos.{enabled:false, minSec:krb5i,
  requireMachCred:true, enforcePermissions:enforce, storageClass}`; render
  guards as RWX.

Layouts for GSS callers: **B** (decided by E1, Phase 0 results above):
answer LAYOUTUNAVAIL to GSS sessions so every byte goes through the MDS
under Kerberos. The F67 orphan guard (`pnfs/mds/layout.rs:1701-1718`)
then keeps every Phase-1 file on the MDS; Phase 3 decides whether to
migrate them. Option A (change nothing) is rejected: E1 showed the client
then sends the data to the DSes as AUTH_SYS.

Tests: unit (strict hosts entry, publish context name, MDS refuses a bad
floor, env-name parity); chart legs; **a new `PNFS=1 KRB5=1` kind suite**
(`tests-pnfs-krb5/`: krb5i mount on two nodes shares files, sys refused,
alice refused, bob's idmap) — about half of this phase, reused by the rest;
`tests/krb5/run-pnfs-krb5.sh` from E1.

Risk low (all gated). Migration hazard to document: a node with a live
`sec=sys` pNFS mount reuses that AUTH_SYS `nfs_client` for new mounts of
the same MDS IP, which the floor refuses — drain pNFS mounts per node
before enabling. Size ~600 lines, 3–5 days.

## Phase 2: attached nodes only, per volume, on the MDS

**Built** (`nfs::node_authz::{VolumeNodes, watch_pnfs}`, the dispatcher's
`unattached_volume_refusal` and the PUTFH/RESTOREFH checks; chart
`pnfs.kerberos.attachedNodesOnly`, default on). As built, it differs from
the sketch below in three ways: the name operations (LOOKUP, OPEN by name,
CREATE, REMOVE, RENAME, LINK) are checked *before* they run, against the
name when the directory is the export root, so nothing is created or
removed at the root for another volume; the watch keeps every shard's
pNFS volumes (PVs are not filtered by `mds-ip`, which is a Service IP the
MDS does not know), so the node set is the union over all shards; root
READDIR is not filtered. Tested in `tests-pnfs-krb5/` (another node's
volume refused, a detach revokes).


- `node_authz::global()` on the MDS holds the **union** of nodes attached
  to any pNFS volume of this shard, so the existing session gate
  (`dispatcher.rs:1044-1048`, `:5051-5096`) works unchanged (the kernel
  shares one `nfs_client` per node per MDS IP).
- New `node_authz::VolumeNodes` (volume key → nodes; refuse all before
  the first list) and `watch_pnfs(...)`: VolumeAttachments
  (`status.attached`, our attacher) + PVs with `pnfs.chert.us/mds-ip` of
  this shard; key = `pnfs.chert.us/volume-file` (`pnfs_csi.rs:42`). Spawned
  from `nfs_mds_main.rs` (as `nfs_main.rs:219-226`).
- Per-volume gate in `CompoundDispatcher` after any op that sets the
  current FH (PUTFH `:1431`, LOOKUP, LOOKUPP, RESTOREFH `:1456`, OPEN
  `:1661`, CREATE): volume = first path component under the export (as
  `:3096-3101`); caller = the session's machine principal; refuse
  `NFS4ERR_ACCESS` and clear the FH. Root allowed. LAYOUTGET is covered
  through the FH. Nothing computed when off. Optional: filter root READDIR.
- Rejected: controller pushing attach/detach to the MDS (needs MDS
  persistence, and rides the token-only control path until Phase 5).
- Chart: MDS ServiceAccount when witness or attachedNodesOnly; ClusterRole
  read on volumeattachments, persistentvolumes, nodes;
  `pnfs.kerberos.attachedNodesOnly: true`.
- Gap until Phase 4: layouts already granted keep working at the DSes
  after a detach (optionally recall that node's layouts on detach).
- Size ~700 lines, ~1 week.

## Phase 3: Kerberos on the data servers

- **Share the GSS code:** extract `gss_front` from
  `handle_rpcsec_gss_call` (`nfs/server_v4.rs:1052-1180`: INIT,
  CONTINUE_INIT, DESTROY, DATA validate/floor/verifier/sequence/unseal),
  `floor_refusal` from `:789-799`, make `frame_reply` (`:975`)
  `pub(crate)`. `serve_tcp` becomes a caller (pure refactor). The DS embeds
  its own `RpcSecGssManager`. Rejected: moving the DS onto `serve_tcp`
  (pipeline/splice don't fit); forking the GSS code
  (`pnfs/mds/server.rs:1847-1854` shows how forks drift).
- DS (`pnfs/ds/server.rs`): `gss: Option<Arc<RpcSecGssManager>>`, `Some`
  only with `KRB5_KTNAME` (+ `validate_env()`); `dispatch_minimal_nfs`
  (`:325-361`) routes GSS through `gss_front`; `assemble_reply` (`:1684`)
  frames with `frame_reply` under GSS; **splice off under GSS**
  (`pnfs/ds/io.rs:469-476`); EXCHANGE_ID decodes `state_protect4_a`
  properly (`:438-445` reads only the discriminant) and records the RPC
  principal (`:456-461`); SECINFO_NO_NAME advertises what the floor
  accepts, not a hard-coded `krb5`/none (`:791-811`).
- DS union gate (attached nodes' machine principals only) — do it: the
  default allowlist `nfs/*@REALM` also matches server principals.
- DS SP4_MACH_CRED deferred to 3b (needs BIND_CONN_TO_SESSION); document
  the residual.
- Principals: krb5-operator `hostPrincipals` (`statefulSet:
  flint-pnfs-ds, services: [nfs]`) →
  `nfs/flint-pnfs-ds-N.flint-pnfs-ds-headless.<ns>.svc.<domain>`; each pod
  mounts only its own key (`subPathExpr: $(POD_NAME).keytab`; a rekey
  needs a restart).
- Hosts lines for every DS address (multipath too): node plugin
  `nfs_krb5::ds_hosts` with its own marker `# flint-pnfs-ds-krb5` (must not
  end with the RWX marker, or RWX pruning drops it); built from Services
  labelled `component: pnfs-ds` → `<pod>.flint-pnfs-ds-headless...`
  (`pnfs-ds.yaml:116-133`); written before `mount`, then reconciled;
  stripped when off. Node SA gets services get/list/watch.
- Tests: refactor covered by existing rpcsec_gss/gss_framing/gssneg; DS GSS
  unit tests (INIT, krb5i MIC, krb5p seal, TOOWEAK, golden AUTH_SYS bytes,
  no splice under GSS); VM: DS bytes > 0 and MDS-lane ≈ 0 under krb5i/p;
  `tests/lima/pnfs/{smoke,ds-lane-differential,splice-differential,pynfs}`
  off as the byte-identical guard; kind with 3 DSes + multipath.
- Size ~1,500 lines, 2–3 weeks.

## Phase 4: the DS checks the layout (MAC'd filehandle)

- **MAC'd filehandle, not a pushed stateid table:** a files-layout client
  sends the open/lock/delegation stateid to the DS (RFC 8881 §13.9.1), so
  a table would mirror every OPEN to every DS; delegation stateids are
  unpersisted. The pin test (`pnfs/ds/server.rs:2060-2186`) stays green.
- DS FH version `4`, 33 bytes: `ver|key_id(4)|file_id(8)|stripe(4)|mac16`,
  `mac = HMAC-SHA256(key, ver‖key_id‖file_id‖stripe‖node)[..16]`, `node` =
  the LAYOUTGET session's machine host (empty under AUTH_SYS). Key Secret
  `flint-pnfs-fh-key` (chart `lookup`, as `pnfs-security.yaml:18-36`),
  current + previous for rotation; per-DS HKDF keys after Phase 5.
- MDS mints v4 in `encode_file_layout_striped` (`dispatcher.rs:4749-4767`)
  when keyed; legacy pins (`:4779-4784`) get LAYOUTUNAVAIL when enforcing.
- DS PUTFH (`:695-704`) with `FLINT_PNFS_DS_FH_AUTH=enforce`: only v4
  that verifies for the session owner's host; canonicalise to v2 for the
  io handler; refuse v2 and MDS path handles (`io.rs:689-717`); error per
  E4 (expect BADHANDLE).
- Works without Kerberos (closes "know or guess a file id"); opt-in
  `pnfs.dsAuth.enabled`. Size ~800 lines, 1–1.5 weeks.

## Phase 5: TLS and identities on MDS↔DS gRPC

- No cert-manager, no per-pod certs: chart `genCA` + `genSignedCert`, kept
  with `lookup` (Secret `flint-pnfs-control-tls`, `existingSecret`
  override). One MDS cert (all shard SANs), one shared DS server cert;
  clients verify a fixed server name (the MDS dials DSes by IP).
- Per-DS identity: pod-bound projected ServiceAccount tokens, TokenReview
  (cached to expiry). MDS requires SA `flint-pnfs-ds` **and** pod name ==
  `RegisterRequest.device_id` on Register/Heartbeat/Capacity/Unregister,
  SA `flint-csi-controller` on volume/block RPCs; DS requires SA
  `flint-pnfs-mds` on `DsControl`. The bearer token stays as the fallback
  without Kubernetes (Lima rig).
- `serve_with_incoming` + tokio-rustls 0.26 on the servers
  (`pnfs/mds/server.rs:1794-1835`, `pnfs/ds/server.rs:1353-1358`),
  `connect_with_connector` on the clients
  (`pnfs/ds/registration.rs:47-58`, `pnfs/mds/operations/mod.rs:1143`,
  `:1254-1270`, the controller's `PnfsShards`); `pnfs/grpc.rs:84-96`.
  RBAC `tokenreviews: create`; the DS gets its own ServiceAccount.
- Independent of 1–4 and could go first: without it, Kerberos on 2049 is
  bypassed by registering a rogue DS. Size ~1,000 lines, 1.5–2 weeks.

## Order and where tests run

Phase 1 → 2 → 3; 4 works alone under AUTH_SYS (node binding needs 3); 5
is independent. A Linux box with kind runs the unit, chart, RWX `KRB5=1`
and new `PNFS=1 KRB5=1` suites; the Lima VM gives wire evidence (gssd
`-vvv`, KDC logs, tcpdump), the experiments, and the `tests/lima/pnfs`
differentials that guard AUTH_SYS.

## Unsettled until measured

1. ~~DS credentials and flavor (E1b, E2)~~ — settled: no AUTH_SYS
   regression; DS I/O uses the mount's flavor, else downgrades to AUTH_SYS.
2. ~~DS principal naming (E2)~~ — settled: reverse lookup of the IP.
3. ~~Fallback when DS GSS fails (E1)~~ — settled: downgrade to AUTH_SYS,
   not the MDS. Still open: what the client does when the DS REFUSES
   AUTH_SYS (rerun E1 in Phase 3).
4. SP4_NONE accepted from a DS after asking SP4_MACH_CRED (E2).
5. Kernel recovery from a refused DS filehandle (E4).
6. ~~TokenReview `pod-name` extra~~ — settled (E5); tonic + rustls still a spike.
7. Whether the MDS honours `FLINT_NFS_ENFORCE_PERMISSIONS` and root squash
   through the shared ioops (grep + unit test in Phase 1).
