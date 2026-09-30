# flint-lite NFS proxy — many hubs, one port

Status: **DESIGN, NO CODE** (2026-09-27).

This is the alternative to `flint-lite-multivolume-design.md`. That
design gets to one port by making one hub serve N workspaces, which
splits the tier, the state DB, the lease, the hydrator, credentials and
the idle ladder per volume. This one leaves **one hub per share
exactly as it is** and puts a proxy in front that speaks NFSv4.1 on one
port and routes each request to the right hub.

```
 agents (any cluster)                    hub cluster
 ┌────────────┐                ┌──────────────────────────────────────┐
 │ mount      │  one LB :2049  │  flint-nfs-proxy   ┌──> hub ws-a :2049│
 │ proxy:/ws-a├───────────────>│  (1 Deployment)    ├──> hub ws-b :2049│
 │ proxy:/ws-b│                │  pseudo-root /     └──> hub ws-c :2049│
 └────────────┘                └──────────────────────────────────────┘
                     hubs unchanged; headless Services (§7a)
```

PVs differ only in `path`, exactly as the multi-volume design promised:

```yaml
spec:
  nfs:
    server: nfs.flint.example.com   # the proxy's one address
    path: /ws-agent42               # the only field that varies
  mountOptions: ["nfsvers=4.2", "hard", "proto=tcp"]
```

## 1. Why it has to be a COMPOUND router

Three cheaper shapes were considered and none works:

- **L4 / per-connection routing.** The Linux client keeps **one
  `nfs_client` — one TCP connection and one session — per server
  address**, shared by every mount from that node. `proxy:/ws-a` and
  `proxy:/ws-b` on one node travel on the same connection, often in the
  same slot table. A connection carries no single destination.
- **Referrals (`NFS4ERR_MOVED` + `fs_locations`).** The client follows
  the referral to the hub's own address — N endpoints again, which is
  the thing being removed.
- **One export, N subdirectories.** Works on the wire today, but it is
  one bucket, one lease and one failure domain for everything: the
  multi-volume problem without the multi-volume fixes.

So the proxy terminates NFSv4.1 sessions from clients and routes **each
COMPOUND** to one hub.

## 2. What the hubs already give us

The design is small because four properties are already true of every
lite hub:

| Property | Where | What it buys |
|---|---|---|
| Every filehandle carries the hub's `instance_id` **in the clear at bytes [1..9]**, all four formats (v1/v2/v3 and v4 kernel handles) | `nfs/v4/filehandle.rs:12-35`, `nfs/v4/fh_kernel.rs:12-14` | A routing key in every PUTFH. **The proxy never rewrites a filehandle.** |
| `instance_id` is stable per volume (`stable_nfs_instance_id`, a hash of the volume id) | `rwx_nfs.rs:53` | The key survives hub restarts, so a routing table stays valid. |
| Clientids, stateids and locks are **persisted**; only sessions are dropped on restart | `state.db`, see NFSv4 persistence (2026-06-12) | A hub restart looks to its clients like `BADSESSION` → `CREATE_SESSION` on the same clientid. No reclaim. **Measured** in census Part 2, with one exception: `CLOSE` of an open held across the restart with a lock returns `BAD_STATEID` (defect D2). |
| With one export, `PUTROOTFH` returns the export root directly | `nfs/v4/operations/fileops.rs:2031` | A hub's root **is** the workspace root; the proxy owns the only pseudo-root. |
| Delegations are gated off (`FLINT_NFS_DELEGATIONS` unset; lite's render does not set it) and standalone hands out no pNFS layouts | `nfs/v4/state` `delegations_enabled()` | **No callbacks to relay.** The back channel can be accepted and left idle in v1. |

Checked but **not** true, and the reason for the two hub changes in §5:
stateids are `other[0..8] = counter, other[8..12] = client_id as u32`
(`nfs/v4/state/stateid.rs:704`) — two hubs mint colliding stateids —
and `fsid_major` is the backing file's `st_dev`
(`fileops.rs:151`), which two hubs can share.

## 3. Namespace and routing

**The pseudo-root belongs to the proxy.** `/` lists one entry per
workspace. The proxy answers `PUTROOTFH`, `LOOKUP`, `GETATTR`,
`ACCESS`, `READDIR`, `SECINFO_NO_NAME` and `GETFH` on it itself, and
does **not** advertise `xattr_support`, matching the hubs. Its
filehandle uses a marker byte no hub mints, and its `fsid` is distinct
from every hub's.

**Routing key = the current filehandle.** The proxy walks each COMPOUND
op by op and tracks which target the current FH belongs to:

| Op | Target after it |
|---|---|
| `PUTROOTFH` | proxy (pseudo-root) |
| `PUTFH fh` | hub named by `fh[1..9]`; unknown instance → `NFS4ERR_STALE` |
| `LOOKUP name` while the current FH is the pseudo-root | **crossing**: hub `name`; the proxy sends `PUTROOTFH` to that hub in place of the pseudo-root FH and the `LOOKUP`. Linux never sends `PUTROOTFH, LOOKUP` together: it sends **`PUTFH(pseudo-root), LOOKUP`** (census s1, s4) |
| `LOOKUPP` at a hub's root | proxy (pseudo-root) — answered by the proxy |
| `SAVEFH` / `RESTOREFH` | the saved target travels with the saved FH |
| any other op | same target as the current FH |

**One compound goes to at most one hub.** The rule:

- a proxy-only **prefix** (`PUTROOTFH`, `LOOKUP` on `/`) followed by
  ops for **one** hub is supported — that is how every mount and
  submount crossing looks;
- a two-FH op (`RENAME`, `LINK`, `COPY`, `CLONE`) whose saved and
  current FH are on different hubs returns **`NFS4ERR_XDEV`** at that
  op. With distinct fsids (§5 H1), Linux refuses a cross-workspace
  rename locally with `EXDEV` before anything is sent, so this arm is
  a backstop;
- anything else that needs a second target returns an error at that op
  and **is logged and counted**. The first drill must show this counter
  at zero for the Linux client; if it is not, the split is added then,
  not guessed at now.

**Forwarding is byte-for-byte.** The proxy decodes with the crate's own
`CompoundRequest::decode` to route, and forwards the **original op
bytes**. The decoder needs one addition: record each op's byte range
(`decoder.remaining()` before and after, `compound.rs:1165`). The
backend COMPOUND is a new header + a synthesised `SEQUENCE` (+ a
synthesised `PUTROOTFH` for a crossing) + the original op slices. The
reply is spliced the other way: the hub's results after its `SEQUENCE`
are copied opaque behind the proxy's own `SEQUENCE` result and any
synthesised prefix results (`PUTROOTFH`/`LOOKUP` → `NFS4_OK`), with
`numres` and the status fixed up. **The proxy never decodes a READ,
WRITE, GETATTR or READDIR body.** An op the decoder cannot parse ends
routing: the rest of the compound goes opaque to the current target,
which runs the same decoder and fails the same way. (Against knfsd,
every `ls -la` sent `LISTXATTRS`, which the hub's decoder does not
know. That is because knfsd advertises `xattr_support` and the hub
does not. Neither the proxy's pseudo-root nor the hubs advertise it,
so re-capture against a real hub to confirm the client never sends
it; census s1–s4.)

## 4. Sessions, identity and state

### Identity: one backend client per (client, hub)

A client's open owners and lock owners are only unique **within its
clientid** (Linux mints owners from per-client counters). Putting every
client on one backend clientid would merge them. So the proxy acts as a
**separate NFS client toward each hub, one per downstream client**:

- backend `co_ownerid = "flint-proxy/" ‖ downstream co_ownerid`,
  backend verifier = **the downstream verifier**.
- A client reboot (new verifier, RFC 8881 §18.35.5 case 5) therefore
  reaches every hub as the same case 5 the next time the proxy
  re-registers — the hub discards that client's old state exactly as it
  would have for a direct mount. **This must be tested as convergence,
  not only safety:** the reboot has to reach the hub, not just avoid
  corrupting the proxy.
- The existing `co_ownerid` collision defects (many-clusters drill,
  defects 1–3) are **inherited unchanged, not widened**: the backend
  sees the same owner bytes it would have seen directly.

Backend clients are created lazily, on the first compound for that hub,
and the proxy sends `RECLAIM_COMPLETE` on them itself.

### Downstream sessions: reuse the hub's code

The proxy is a server toward clients: `EXCHANGE_ID`, `CREATE_SESSION`,
`SEQUENCE`, `BIND_CONN_TO_SESSION`, `DESTROY_*`, `RECLAIM_COMPLETE`.
It uses the crate's own `ClientManager` / session code and a SQLite
`StateBackend`, not a rewrite. That also means any fix to those
(e.g. the nconnect case-4 defect) lands in both.

### Slots and exactly-once

Each downstream slot keeps `(last seqid, hub, backend slot, backend
seqid)`. A retransmission of downstream `(slot, seq)` is re-sent to the
same hub with the **same** backend `(slot, seqid)`, so **the hub's reply
cache answers it** — the proxy stores no reply bodies. Proxy-only
compounds (pseudo-root ops) are idempotent and are simply recomputed.
Successive requests on one downstream slot can go to different hubs, so
backend seqids are tracked per hub, never copied from the client.

### Leases

- The proxy advertises and enforces **the hubs' own lease, not a
  shorter one.** (This first said "shorter than the hub's by a margin";
  the step-3 keepalive drill disproved it.) The Linux client takes its
  renewal period from the LAST fsinfo it ran (`nfs4_set_lease_period`
  in `nfs4_do_fsinfo`), and a workspace submount's fsinfo is answered by
  the hub, unmodified. So a client renews on the hub's schedule, and a
  shorter proxy lease reaped live clients between renewals: `SEQUENCE:
  session not found`, then a recovery loop on a hard mount. What keeps
  a hub from expiring a client the proxy still holds is the keepalive
  below, not a margin.
- While a downstream client keeps renewing, the proxy sends a bare
  `SEQUENCE` to every hub it holds a backend client on for that client,
  about once per third of the hub lease. When the downstream lease
  lapses, the proxy stops, and the hub's own expiry and courtesy release
  run as they do today.
- `sr_status_flags` from every hub the client uses are **OR-ed into the
  next downstream `SEQUENCE` reply**. A revocation must not be dropped
  on the way through.

### Restarts and state loss

| Event | Direct mount today | Through the proxy |
|---|---|---|
| **Hub restarts** (state persisted) | `BADSESSION` → `CREATE_SESSION`, no reclaim | Hub returns `BADSESSION` to the proxy; proxy re-`CREATE_SESSION`s on the same backend clientid and retries. **The client sees nothing but latency.** |
| **Proxy restarts** | — | The client sees `BADSESSION`, re-`CREATE_SESSION`s on its clientid. That works because the proxy's client table is **persisted** (small SQLite DB on a PVC, written only at `EXCHANGE_ID` confirm and `DESTROY_CLIENTID`). Backend clients are re-attached with the same owner and verifier, so the hubs return the **same** clientid with state intact — no reclaim anywhere. |
| **Hub loses a client's state** (hibernate deleted the PVC, `state.db` quarantined) | Remount, not resume (hibernate already means this) | Hub returns `STALE_CLIENTID`; proxy re-registers. **The proxy must NOT set `SEQ4_STATUS_ADMIN_STATE_REVOKED`.** The census measured Linux 6.12 treating that flag client-wide: revoking one export's state lost byte-range locks in the *other* export on the same session (census Part 3). Behind the proxy, that means every workspace the node mounts. Instead: (1) **hibernation requires zero live leases** (the HIB-1 fix), so hibernation never destroys state a client holds; (2) for the remaining losses, return per-op errors on that hub's stateids only, with no SEQUENCE flag. Whether Linux keeps that recovery per-state is **unmeasured** and is the step-3 drill. |
| **Hub parked** (idle ladder, replicas 0) | `hard` mount hangs; nothing can wake it (an NFS client cannot write an annotation) | Proxy gets connection refused, stamps `chert.us/requested-at` on the FlintShare (the hub-gateway's `/wake` code), and returns **`NFS4ERR_DELAY`** until the hub is Ready. **This fixes the agent-mount hazard** for proxied clients. |

The last row is an improvement over direct mounts: the proxy is the
first NFS-side component that can wake a hub. It also holds the hub's
`activeLeases` above zero for exactly as long as a downstream client
is live, which is the signal `suspendWithSessions: false` reads.

## 5. Hub changes — two, small, gated off by default

Both are env-gated. With them off, a hub is byte-identical to today, so
no tier or state_backend test changes.

- **H1 — fsid from the volume.** `FLINT_NFS_FSID_FROM_VOLUME=1` reports
  `fsid = (server_id, 0)` for objects on the export's own device,
  instead of `st_dev` (`fileops.rs:151`). `server_id` is the random,
  non-zero u64 persisted in `state.db`, already the FH `instance_id`
  and `status.serverId`. So it is stable across restarts and builds
  (no hash involved), and changes exactly when the volume is new. The
  pseudo-root keeps `(0, 0)`, which is also the proxy's pseudo-root
  fsid. Scsi volumes and any mount nested in the export keep their
  own. Without it, two workspaces can share an fsid:
  Linux may then share a superblock between the two mounts, merge
  `statfs`, and send cross-workspace renames to the wire. The
  alternative (the proxy rewriting `FATTR4_FSID` inside every `GETATTR`
  and `READDIR` reply) means decoding every attribute reply on the hot
  path. It is rejected.
- **H2 — stateids carry a hub tag.** `TEST_STATEID` and `FREE_STATEID`
  carry stateids and **no filehandle**, so the current FH cannot route
  them. Today two hubs mint the same `other` for their first client's
  first open. A learned stateid→hub map does not fix it: one downstream
  client can hold the **same** `other` on two hubs. Fan-out is wrong
  too, because `FREE_STATEID` on the wrong hub frees real state.
  **Fix (census 2026-09-27): put a hub tag in `other[8..12]`**, where
  `allocate` now writes `client_id as u32`. Production code writes
  that field but never reads it. The counter keeps 64 bits (the rejected
  alternative, a tag in the counter's top bits, wraps a persisted
  32-bit counter in about 50 days at 1,000 opens/s). The tag is
  **assigned** by the operator (unique u32, recorded in the share's
  status and passed as env), never hashed. **It never changes for the
  life of the volume:** stateids restored from `state.db` carry it, so a
  tag that changed across a hub restart would strand them. It is tied
  to the share, not the pod. Only a new volume (hibernation deletes the
  PVC) may get a new one. The operator writes `status.stateidTag` once
  and never rewrites it; that set-once write is what the test pins. The delegation mint's
  anti-reuse epoch moves out of `[8..12]`. Only `allocate` and
  `allocate_delegation` mint client-visible stateids; the `0xFC`
  lock-table keys and the breaker marker never reach the wire.
  `LAYOUTRETURN` FSID/ALL and `DELEGPURGE` are also FH-less, but
  carry no live state in lite, so the proxy answers them itself.
- **Decoder byte ranges** (§3). This is additive and changes no
  behaviour.

For comparison, multi-volume changes the tier, state DB schema, lease,
hydrator, credentials, idle ladder and operator model.

## 6. Operator and chart

- `flint-lite-operator-chart`: `nfsProxy.enabled` (off by default),
  following the `gateway.enabled` pattern. It adds one Deployment, one
  `LoadBalancer` Service on 2049, and a small PVC for the client table.
  It ships in the operator image as another `command`, like
  `flint-hub-gateway`.
- **Where each identifier lives (decided 2026-09-27).** Everything the
  proxy routes by comes from **FlintShare status**, so the proxy's whole
  routing table is derived from a watch (reuse
  `lite_gateway/resolve.rs`) and rebuilt from scratch on restart:

  | Key | Source of truth | Hub | Proxy |
  |---|---|---|---|
  | workspace name → hub Service | the CR (label `chert.us/volume-id`, defaulting to the CR name) | — | in memory, from the watch |
  | `instance_id` (FH bytes 1..9) → hub | **already there:** a lite hub's FH `instance_id` is its persistent `server_id` (`state.db`), which `/status` publishes and the operator already copies to `status.serverId` | as today | in memory, from the watch |
  | stateid tag (H2) → hub | the operator **assigns** it once per share, into `status.stateidTag` | reads it from env and stamps every stateid | in memory, from the watch |
  | fsid (H1) | the same persistent `server_id` | reports it as the export's fsid | stores nothing; passes attributes through |

  The proxy computes nothing from a hash of the volume id
  (`stable_nfs_instance_id` uses `DefaultHasher`, which is not stable
  across Rust releases); it reads what the hub reports. **The only
  state the proxy persists is the client table (§4).**
- The hub NetworkPolicy auto-admits the proxy on 2049, using the same
  mechanism the gateway peer uses.
- RBAC: `get,list,watch,patch` on flintshares, the same as the gateway.

## 6a. Istio ambient, two ingress paths, and client mTLS

Decided with the owner 2026-09-27:
- Istio runs in **ambient** mode.
- Clients reach the proxy both **directly** (a `LoadBalancer`) **and**
  through the **Istio ingress gateway**. Both are supported.
- The owner controls the client node images, so **client mTLS is
  available**.

Nothing in flint knows about Istio today.

### The mesh cannot identify an NFS client, so the client proves who it is

The kernel NFS client opens its socket in the mounter's network
namespace. For an `nfs:` PV that is kubelet, in the node's namespace,
which ztunnel does not capture as a workload. Remote-cluster clients
are outside the mesh altogether. **Client identity therefore comes from
RPC-with-TLS (RFC 9289), terminated at the proxy:**

- **Client:** Linux ≥ 6.5 with `CONFIG_TLS`, `ktls-utils` (`tlshd`)
  enabled on the node, and the mount option `xprtsec=mtls`:
  `mountOptions: ["nfsvers=4.2", "xprtsec=mtls"]`.
- **Proxy:** answers the `AUTH_TLS` NULL probe with `STARTTLS`, runs
  the TLS handshake on the same connection (rustls), and requires a
  client certificate chaining to the configured CA. The certificate's
  URI SAN names the client, and the per-workspace allowlist (§7 (a))
  keys on it. A connection that does not upgrade is refused on the
  external listener.
- The same check runs whichever path the connection took. **The
  identity is end to end, so it does not depend on the gateway, NAT,
  or ztunnel preserving the source address.** The address allowlist
  becomes defence in depth, not the identity.

RPC-with-TLS is **STARTTLS-shaped**: the first bytes on the wire are a
plaintext RPC NULL call, not a TLS ClientHello. So nothing in between
can route it by SNI or terminate it. Both ingress paths must carry it
as **opaque TCP**.

### Path A — direct `LoadBalancer`

`LoadBalancer` Service on 2049, `externalTrafficPolicy: Local`, and
`loadBalancerSourceRanges` as the coarse outer fence.

### Path B — Istio ingress gateway

A Gateway API `TCPRoute` (experimental-channel CRD), or the classic
`Gateway` + `VirtualService` `tcp` route. The chart renders whichever
one is selected. The gateway only forwards bytes. The coarse
address fence moves to the gateway: an `AuthorizationPolicy` on the
gateway with `remoteIpBlocks`, which needs the gateway's own Service at
`externalTrafficPolicy: Local`. The proxy sees the gateway as the
peer, and that is fine, because the client certificate still arrives
intact.

### The proxy's inbound: bypass ztunnel, keep its outbound in the mesh

- Annotate the proxy pod `ambient.istio.io/bypass-inbound-capture:
  "true"`. Its outbound traffic to the hubs still goes through ztunnel
  (HBONE) under the proxy's own SPIFFE identity. The reason is not
  security, because the payload is already TLS. It is
  **availability**: a client connection that passes through ztunnel is
  reset when that node's ztunnel restarts or upgrades, and on the
  proxy's node that means **every client at once**. The NFS client
  recovers, but the proxy should not add that event.
- **Fallback** if the bypass does not interoperate with the gateway
  (see the checks below): keep the proxy enrolled with `PERMISSIVE`
  `PeerAuthentication`, so plaintext NFS-over-TLS from non-mesh
  clients is accepted, and accept the ztunnel-restart resets.

### Proxy → hubs: identity in the mesh

- Hub namespace enrolled in ambient, `PeerAuthentication` `STRICT`, and
  an L4 `AuthorizationPolicy` on the hubs: `principals:
  ["<trust-domain>/ns/<ns>/sa/flint-nfs-proxy"]`, port 2049. ztunnel
  enforces it with no waypoint. This replaces the "NetworkPolicy
  admits only the proxy" rule in §6.
- **No waypoint on the hub Services.** NFS gains nothing from L7, and a
  waypoint adds a hop to every RPC. If the namespace has one, set
  `istio.io/use-waypoint: none` on the hub Services.
- **Hubs serve no one directly.** STRICT rejects any kubelet mount
  straight to a hub. In-cluster consumers mount the proxy's ClusterIP
  Service with the same `xprtsec=mtls`.
- The hub's probes are TCP connects to 2049. In ambient, kubelet probe
  traffic is exempted from ztunnel, so the probe should still reach the
  hub itself. **Verify it.** A probe that ztunnel answers would be
  vacuous.
- A parked hub (replicas 0) seen through ztunnel is an accepted
  connection that then closes, not a refusal. The wake path (§4)
  treats every "no upstream" shape alike.
- **Cost:** every byte from proxy to hub crosses two ztunnels (HBONE
  encrypt and decrypt). Include it in the A/B perf leg (§8).

### cert-manager

- **Proxy server certificate:** a `Certificate` → Secret. **Client CA
  set:** a trust-manager `Bundle`. The proxy **watches both files and
  hot-reloads** without a restart. Existing connections keep the
  credentials they handshook with. TLS validity is checked at
  handshake, and the NFS client reconnects rarely.
- **Client certificates:** a per-client-cluster `Certificate`
  (cert-manager in that cluster, from an issuer chaining to the CA the
  proxy trusts) → Secret → a small **DaemonSet
  (`flint-nfs-client-identity`)** that copies key and certificate to a
  host path, `/etc/flint/nfs-tls/`, and points `tlshd` at it. The
  identity granularity is **the cluster**, which is what the allowlist
  expresses ("cluster A may use `ws-a*`"). A per-node key (the node
  generates its key, and the DaemonSet submits a CSR) is a later
  refinement: revocation per node, and the key never leaves the node.
- The DaemonSet also checks the node's prerequisites (kernel version,
  `tlshd` running) and reports them. A mount with `xprtsec=mtls` on a
  node without `tlshd` fails at mount time, and the DaemonSet's
  readiness should say why before any mount does.
- If the mesh uses istio-csr, the hub-side identities are
  cert-manager-issued too, with nothing for flint to do.

### Checks before code (a kind rig with Istio ambient)

1. Does `bypass-inbound-capture` exist in the Istio version in use, and
   does the ingress gateway then send plain TCP to the proxy rather
   than HBONE? If not, use the PERMISSIVE fallback.
2. Is the source address preserved on path A with the bypass?
3. Does the hub's kubelet TCP probe bypass ztunnel? Control: kill the
   hub process while keeping the pod, and the probe must fail.
4. Does a ztunnel restart reset connections that go through it? This
   decides whether the bypass matters.
5. Does `tlshd` pick up a rotated certificate without a restart?
6. `xprtsec=mtls` needs the **node** kernel. kind uses the host kernel,
   so run on the Linux build box (Docker Desktop's VM kernel is not the
   target).

**Answered 2026-09-29** (`tests/lima/nfs-proxy-census/step6a-istio.sh`,
kind on the box, Istio 1.31.1 ambient, Gateway API v1.6.2, host kernel
6.12 with `tlshd`; 17/17, `results-kind-istio-6a/`):

1. **Yes.** The annotation exists in 1.31 and ztunnel still enrolls the
   pod for its outbound. An `xprtsec=mtls` mount through the Istio
   Gateway (TCP listener + `TCPRoute`) works, which is only possible if
   the gateway hands the proxy plain TCP. **Needs Gateway API ≥ v1.6**:
   Istio 1.31 ignores older TCPRoute CRDs, with nothing but a warning in
   istiod's log (run 1: the route never attached and the mount timed
   out, which the rig at first misread as a bypass failure).
2. **Yes.** A rule needing the certificate AND the host's address
   matched on path A (externalTrafficPolicy Local). Through the gateway
   the peer is the gateway, and the address rule does not match: the
   certificate is what names the client there.
3. **Yes.** With TCP 2049 REJECTed inside the hub pod's netns the pod
   went NotReady, and Ready again once the rule was gone: the probe
   reaches the hub, not ztunnel.
4. **Yes, the bypass matters.** A ztunnel restart on the proxy's node
   reconnected the client (kernel connect_count 2→4) with the proxy's
   inbound captured, and did not (2→2) with the bypass. I/O continued in
   both cases: the proxy re-dials its hubs, whose connections do go
   through ztunnel.
5. **Yes.** Certificate files rewritten in place, no restart: the next
   handshake presented the new certificate.
6. Ran on the box's kernel throughout.

Also checked: the hubs' AuthorizationPolicy (chart `nfsProxy.istio`)
refuses a pod that has the proxy's LABELS but another ServiceAccount (no
NFS reply to a NULL call), and serves the proxy's ServiceAccount (the
control); the operator reaches every hub's status port through the mesh.
In ambient the hub NetworkPolicy must admit HBONE (15008) from the proxy
and the operator, which the chart now renders. Not checked: STRICT alone
against a non-mesh client (the NetworkPolicy refuses it first).

## 7. What this does not solve, and what it costs

- **One failure domain for the door.** Every proxied mount goes through
  one Deployment. A proxy restart costs every client one
  `CREATE_SESSION` (no reclaim, per §4), but on a spot reclaim the time
  to serve again is the cost. **Measure it before promising anything.**
  v1 is a single replica. Two replicas need the client table shared
  between them, which is v2.
- **All data bytes cross the proxy twice** (in and out). The proxy
  node's NIC and CPU cap aggregate throughput across all workspaces.
  Metadata-heavy agent workloads pay one extra in-cluster hop per
  RPC. **Measure small-file and throughput A/B against a direct mount**
  before deciding it is acceptable.
- **One trust domain per port.** Anyone who can open TCP to the port
  can `READDIR /`, `LOOKUP` any workspace, and present AUTH_SYS uid 0,
  which the hub honours (`no_root_squash`, `nfs/v4/authz.rs:40`). No
  mount privilege is needed for this: a userspace NFS client in an
  unprivileged pod is enough. **This is a partial regression.** Today,
  per-share NetworkPolicy can separate *in-pod* mounts inside the hub's
  cluster, because pod selectors see the pod. Kubelet-mounted `nfs:`
  PVs (whose source is the node IP) and remote clusters (SNAT) were
  never separable. **The goal is ONE external port, so the check has to
  live INSIDE the proxy**, keyed on who the connection is:
  (a) **source-address allowlist per workspace.** The LB must preserve
  the client address (`externalTrafficPolicy: Local`, or an NLB with IP
  targets). A remote cluster usually egresses through a NAT gateway,
  so an address identifies a *cluster*. The proxy filters `READDIR /`,
  refuses `LOOKUP`, and **refuses `PUTFH` for a hub the connection is
  not allowed**. Without the `PUTFH` check, a leaked filehandle
  bypasses the `LOOKUP` check.
  (b) **RPC-with-TLS mTLS** on the same port (RFC 9289, Linux
  `xprtsec=mtls` + `tlshd`), terminated at the proxy, mapping each
  client certificate to the workspaces it may see. **This is the
  primary identity (§6a)**; (a) is defence in depth. Identity is per node or cluster, not per pod.
  flint does not implement it today.
  (c) **krb5 terminated at the proxy.** The hub already enforces a krb5
  floor (`nfs/sec_policy.rs`), but GSS cannot pass through a proxy.
  Either way, the hubs' NetworkPolicy admits **only the proxy**, so the
  proxy cannot be bypassed.
- **AUTH_SYS only in v1.** The proxy copies the caller's AUTH_SYS
  credential into each backend call. An RPCSEC_GSS context is bound to
  the server principal and cannot be passed through. krb5 means the
  proxy terminates GSS itself, which is v2.
- **One client for every workspace on a node.** Behind the proxy, all
  workspaces a node mounts share one NFS client and one session
  (census s2). Any client-wide event now spans workspaces. The one
  measured so far: Linux's recovery after
  `SEQ4_STATUS_ADMIN_STATE_REVOKED` loses locks in every export on the
  session, not only the revoked one (census Part 3). §4 avoids ever
  sending it. Direct mounts never had this coupling.
- **Delegations stay off behind the proxy.** Enabling them means
  relaying `CB_RECALL` from hub to client over the back channel, which
  is v2.

## 7a. Scale target: 1,000 active, 10,000–20,000 total

Stated by the owner 2026-09-27:
- each project holds **1,000–10,000 files**;
- about **1,000 active** projects within a year;
- **90–95% of all projects inactive**, so **10,000–20,000 in total**;
- storage class **flint-spdk**.

What is measured today (`docs/plans/flint-lite-fleet-rig-results.md`):
- 3,000 shares with 300 live, on 4 `i4i.xlarge` spot workers. The
  operator settled at about 50 MiB.
- A fully parked fleet of 3,000 settled at about 0.24 API writes/s.
- **The live hubs were stubs** (no `state.db`, no tier, no S3, no real
  PVC I/O), so per-hub constants have never been measured.

### Active side — 1,000 hubs

| Limit | At 1,000 | Status / lever |
|---|---|---|
| Volume attach limit | none: flint-spdk reports `max_volumes_per_node: 0` (`spdk-csi-driver/src/main.rs:5761`); lvols live on node NVMe, reached over NVMe-oF | not a limit |
| SPDK NVMe-oF target subsystem cap | about 100 active volumes per node | SPDK's default is 1,024 per target. **Check flint's configured value.** |
| Pods per node (110, minus about 10 system pods) | 10–12 nodes minimum | Node shape of the 16-vCPU class, given the 100m hub CPU request |
| Hub CPU/memory requests (100m / 128Mi) | 100 cores / 125 GiB reserved | **Real RSS and CPU unknown**; set the requests from the rig |
| Operator live-hub polling | 3.3x the tested live count | Needs the rig run |
| Proxy (§7) | one replica carries every byte for 1,000 projects | **Multi-replica proxy (shared client table) becomes required** |
| Spot reclaim | about 70–100 hubs per node lost at once | Same under multi-volume, so it does not decide between them |

**flint-csi-node rolls.** Rolling the node DaemonSet gives EIO to every
flint-PVC consumer on the rolled node, and each one must be restarted
(see the v1.14 topology release record). At about 100 hubs per node,
a routine driver upgrade becomes a mass outage unless the **operator
detects the roll and restarts every hub on the rolled node**. This is
required before 1,000.

### Inactive side — 9,000–19,000 shares

- **Hibernate, do not suspend.** A suspended share keeps its PVC.
  9,000–19,000 lvols at even 1 GiB is 9–19 TiB of NVMe held for nothing.
  At 1,000–10,000 files, a wake from S3 is cheap. So the ladder's
  default for an inactive project is hibernate (PVC deleted, S3 the only
  copy), with suspend only as a short first rung. Inactive projects
  rarely hold mounts, so hibernate's "remount, not resume" (§4's state-loss
  row) rarely fires.
- **Hibernate becomes load-bearing, so recheck its precondition
  first.** The HIB-1 review (2026-08-22) found `hibernatable()` reads
  only `rpo_clean && epoch.held` (`lite_operator/hubstatus.rs:301`),
  with no lease or activity term, while suspend reads both. The code
  has moved since, and whether that is still open has **not** been
  re-derived.
- **Object count.** A parked share keeps a CR, a Deployment (replicas
  0), a Service and a ConfigMap. That is 40,000–80,000 objects, against
  3,000 parked tested.
  - **Headless hub Services are required, not optional.** More than
    about 4,000 ClusterIPs overflows a GKE-default /20 Service range.
    The proxy is what makes headless safe: clients mount the proxy's
    address, so a hub no longer needs a stable ClusterIP to keep a
    mount valid across suspend and wake.
  - **Proposed:** a hibernated share keeps only its CR. The operator
    deletes the Deployment, Service and ConfigMap and re-renders them
    on wake. That is about 4x fewer parked objects. It is an operator
    change, not something that exists today.
  - **The CR itself stays: delete what is derived, keep what is
    declared.** Only the CR holds these, and nothing left behind can
    recover them:
    - where the data is (endpoint, bucket, prefix, credentials ref).
      The S3 `<prefix>.flint/owner` object cannot be found without
      them.
    - the prefix claim that `AdmitTable` arbitrates overlaps from
      (`conflict.rs`).
    - the proxy's answer for a hibernated workspace (wake, not
      `ENOENT`), and its allowlist.
    - RBAC and GitOps: deleting a Git-declared CR fights Argo/Flux.
    Removing the CR too means moving the source of truth to a project
    catalog (an S3 roster with conditional writes, or the project
    service's DB), with the CR created on wake. That is only worth it
    if CR count itself binds.
  - **CR count may bind in the operator's memory.** The fleet rig
    measured `managedFields` at 52.4 MB for 3,000 shares (about 17 KB
    per share). Linear extrapolation gives about 350 MB at 20,000,
    above today's 256Mi limit. Measure it at 20,000, and check a
    watcher-side strip of `managedFields` as the lever.

### What this means for multi-volume

With flint-spdk, **no hard limit at this target requires
multi-volume.** The case for it becomes **density**: fixed per-hub
overhead × 1,000, still unmeasured. There is also the **blast radius
of a node-DaemonSet roll**. Multi-volume does not shrink that per
node, but it cuts the number of restarts. Decide after the rig. The
proxy is kept under either outcome: it routes by `instance_id`, a
multi-volume hub would answer for several, and several hubs behind one
port are still wanted for capacity and failure isolation.

## 8. Build order, and the drills with their controls

0. **Census first**: every stateid `other` layout (H2), and a wire
   capture of the Linux client's COMPOUND shapes for mount, submount
   crossing, rename, `ls -la` at `/`, and state recovery. This confirms
   the one-target rule covers them.
1. H1 + H2 + decoder byte ranges, gated, with a positive control for
   each gate: flip it and watch its test fail. **Hub side DONE
   2026-09-27:**
   - **Mutation controls.** Each test fails with its load-bearing line
     mutated out (H2 twice: the tag in `allocate`, and the delegation
     epoch's move).
   - **Lib suite** 2554/0/6.
   - **Live on a real hub** (`tests/lima/nfs-proxy-census/step1-live.sh`,
     `results-box-6.12-step1/`):
     - GETATTR fsid = the persistent server id, where the control
       arm shows `st_dev` (44);
     - OPEN stateids end in the tag (`…000a11ce`), where the control
       arm shows the client id (`…00000002`, the census collision);
     - a malformed `FLINT_NFS_STATEID_TAG` refuses to start.
   - **Remaining:** the operator assigning the tag (`status.stateidTag`,
     set once) and passing both env vars. That lands with step 4's
     `nfsProxy.enabled`, since nothing sets them before a proxy exists.
2. Proxy core: downstream sessions (reused), pseudo-root, routing,
   backend clients, slot mapping, splice. **The authorization hook is
   part of the core, not a later add-on:** every connection carries an
   identity (the source address in step 2, the certificate in 2b), and
   the router consults an allowlist at exactly three points: `READDIR /`
   (filtered), the crossing `LOOKUP` (`NFS4ERR_NOENT`, so a refused
   workspace is indistinguishable from an absent one), and `PUTFH` of a
   hub filehandle (`NFS4ERR_STALE`, the leaked-filehandle bypass of
   §7). Drill control: the same client without the allowlist entry must
   see the workspace.
   **Core DONE 2026-09-28** (`src/nfs_proxy/`, bin `flint-nfs-proxy`):
   `route.rs` (the router and the three allowlist points), `wire.rs`
   (the codec toward the hubs and the reply splice), `pseudo.rs` (the
   root, with READDIR cookies stable across creates and deletes),
   `backend.rs` (xid-multiplexed hub connections, one backend client per
   (client, hub), one backend session per (downstream session, hub),
   slot s = slot s), `table.rs` (the rows and per-connection views from
   a static config), `server.rs` (sessions through the crate's own
   dispatcher; retransmissions re-sent with the same backend seqid).
   Mutation controls bite on every load-bearing check. **End to end on a
   real kernel, 14/14** with its allowlist control failing exactly the
   two allowlist checks (census Part 4). Found on the way: the hub's
   LOOKUPP result carried LOOKUP's opcode (fixed), and D3 (census Part
   4; fixed 2026-09-29). **Left for step 3:** DESTROY_SESSION/_CLIENTID on the
   backends, keepalive, the wake on a refused connection (today:
   `NFS4ERR_DELAY`), a retransmission across a hub re-establish.
2b. RPC-with-TLS mTLS at the proxy (§6a): the `AUTH_TLS` NULL probe,
   STARTTLS, rustls with a required client certificate, the URI SAN as
   the identity, hot reload of the cert-manager files. A connection
   that does not upgrade is refused on the external listener.
   **DONE 2026-09-29** (`nfs_proxy/tls.rs`; `tls:` in the config, the
   chart's `nfsProxy.tls`). The first record on a connection must be the
   `AUTH_TLS` NULL probe: it is answered `STARTTLS` and rustls takes the
   socket (TLS 1.3, ALPN `sunrpc`, a client certificate REQUIRED and
   chained to `clientCa`); anything else is answered `AUTH_TOOWEAK`, a
   plain NULL ping excepted. The URI SANs are the connection's identity:
   an identity rule names `clients`, `sources` or both (then both must
   hold). The three files are re-read every `reloadSecs`; a file that
   fails to parse keeps the previous configuration. Also fixed on the
   way: `AUTH_TLS` did not decode anywhere in the crate, so the probe
   went UNANSWERED (hubs too) and an `xprtsec=` mount waited out a
   timeout instead of learning there is no TLS.
   **Identity binding** (found while writing the drill): session and
   client ids are counters, so any connection could name another
   client's session. On a TLS connection the downstream owner is
   prefixed `id:<sha256(URIs)[..8]>/`, and a session or clientid of
   another identity is answered as absent (BADSESSION /
   STALE_CLIENTID). Plaintext connections stay unbound: an address is
   not an identity.
   **Drills on a real kernel, 19/19** (`step2b-mtls.sh`, Linux 6.12,
   ktls-utils 1.0 `tlshd`, `results-box-6.12-step2b/`): clients a and b
   each see only their workspaces; another CA's certificate and a
   plaintext mount are refused (the latter by `AUTH_TOOWEAK`, seen in the
   log); a rotated server certificate is SERVED without a restart (a
   mount of an address only the new certificate names fails before the
   rotation and succeeds after), and a mount made before it keeps
   reading; a marker written through the mount never appears in the
   clear on the proxy's port (control: it does on the hub's). The pre-2b
   proxy fails 13/19 (`run-prefix-known-bad.txt`; the 6 it passes are
   refusals that pass when nothing mounts, each paired with a positive
   control). In-process: 7 tests over a real hub; mutation controls
   (rules ignore `clients`, no TOOWEAK, client certificate optional,
   binding off, SEQUENCE unbound) each fail a test.
   **Found by run 1: one node, one identity.** While an NFS client to
   the proxy exists, Linux trunks a new mount of the same server
   (same `server_owner`) onto that client's connection, so a second
   certificate on the same node is never presented: b's connection came
   up as b and b's mount listed a's workspaces over a's connection
   (`run-1-trunking-finding.txt`). Per-cluster (or per-node)
   certificates, as §6a has them, are the only shape that works.
   The §6a Istio checks are answered (§6a, 2026-09-29).
   **Client DaemonSet BUILT 2026-09-29:** `flint-nfs-client-identity`
   (`src/nfs_client_identity.rs`, in the operator image) and the
   `flint-nfs-client-chart` (install in each CLIENT cluster). Per node it
   keeps `/etc/flint/nfs-tls/` equal to a cert-manager Secret — only a
   pair that validates (key matches, within validity, a URI SAN), each
   file atomically, the key 0600 — optionally points `tlshd.conf` at it
   (restarting tlshd only when that edit changed something; a renewal
   needs no restart, check 5), checks kernel ≥ 6.5 and a running tlshd,
   and reports readiness (`--check`) and, optionally, a node label
   `chert.us/nfs-tls-ready` for nodeAffinity. `configureTlshd` makes the
   pod privileged (host PID, edits the host's conf through /proc/1/root,
   `systemctl restart tlshd` via nsenter); false needs only the hostPath.
   **Drill 22/22** (`client-identity-drill.sh`,
   `results-box-6.12-client-identity/`): on the host with the real kernel
   and tlshd, an mTLS mount fails before the agent (the control) and
   works after; a renewal reaches the next handshake without a tlshd
   restart; a mismatched pair leaves the host's files and the mounts
   alone and says why; a stopped tlshd reads not ready. On kind (no
   tlshd on the nodes) the chart runs a pod per node, installs the files,
   reads NotReady with the reason, labels every node false, and a Secret
   update reaches the nodes. Run 1 found a race (the first pass after a
   restart saw systemd's forked `(tlshd)` and read not ready for one
   interval); fixed. Not built: publishing the chart (`release.sh` checks
   the image carries the binary, but pushes no flint-nfs-client chart).
3. Restarts and wake: lease keepalive, status-flag OR, the table in
   §4, and `NFS4ERR_DELAY` + wake.
   **DONE 2026-09-28 (static-table mode):** a one-slot control session
   per backend client (RECLAIM_COMPLETE, the root, the keepalive — never
   a client's slot), the keepalive, DESTROY_SESSION/_CLIENTID propagated,
   a wake hook (logs in static mode; the FlintShare stamp is step 4),
   retransmissions tied to the backend session they were sent on, and a
   hub that reaped the backend client is registered with again. **Drills
   on a real kernel, 11/11** (`step3-drills.sh`, `results-box-6.12-step3/`):
   hub restart and proxy restart under an fsync'd writer (no error, no
   gaps), an idle lock holder keeps its lock past three hub leases (a
   direct-mount contender is refused), a stopped hub makes the reader
   wait and finish, umount destroys the backend clients. **Control:**
   keepalive off → the contender acquires the lock.
   **Two defects the drills found, both fixed test-first:** (1) the
   proxy lease must EQUAL the hubs' (§4 Leases); (2) a backend client
   the hub had reaped surfaced as `NFS4ERR_DELAY` forever — on a hard
   mount that wedged every process touching the mount, sshd's session
   setup included, and the box needed a forced reboot. DELAY is now
   answered only when the hub is unreachable; anything else is an
   error the client sees. **Still open:** the status-flag drill and the
   per-op revocation drill (§4 hub-loses-state row), the wake stamp
   (step 4).
4. Chart/operator wiring, with **headless hub Services** (§7a), and
   the hub lockdown that makes the proxy unbypassable: NetworkPolicy
   (and, under ambient, the L4 `AuthorizationPolicy`) admitting only
   the proxy's ServiceAccount. Drill control: a pod beside the proxy,
   under a different ServiceAccount, must fail to connect to a hub.
   **DONE 2026-09-28** (`--nfs-proxy` / chart `nfsProxy.enabled`):
   - the operator assigns each share a random, fleet-unique
     `status.stateidTag` ONCE, written by its own field manager
     (`flint-lite-operator/stateid-tag`) so the main status apply —
     server-side, forced, and rebuilt every pass — can never remove it;
     tags assigned in-process are remembered by uid, so a lagging store
     cannot cause a second pick;
   - hubs get H1 + their tag as env and headless NFS Services;
   - **with the proxy on, every hub runs its status listener and is
     polled**: `serverId` is the routing key and `/status` its only
     source (the first kind run found every share Ready with no
     `serverId` — monitoring is off by default and the ladder polled
     only shares with an idle policy); the file API stays the spec's;
   - the proxy's kube mode derives its rows from a FlintShare watch
     (routable once a share has an address, `serverId` and tag; parked
     shares stay listed; a collision drops the later share, not the
     table) and wakes by stamping `chert.us/requested-at` (not for an
     admin's `Suspended`); a root whose instance id disagrees with the
     table is DELAY (a woken share's new `serverId` not yet published);
   - the chart: the proxy Deployment (1 replica, Recreate), a
     LoadBalancer on 2049 (`externalTrafficPolicy: Local`), the client
     table PVC, RBAC, and the hub NetworkPolicy admitting the proxy on
     2049; it refuses to install without identities. The binary ships in
     the operator image, and `release.sh` refuses to push the chart if
     the image lacks any binary the chart execs.
   **Kind drill 14/14** (`step4-kind.sh`, `results-kind-step4/`): tags
   distinct and owned by the tag manager alone, unchanged across an
   operator and a hub restart; `/` lists exactly the allowed workspaces;
   bytes land in the right hub; an idle-suspended workspace is woken by
   the proxy and the write completes (14 s); a stranger pod cannot reach
   a hub's 2049 while a pod with the proxy's labels can (kindnet
   enforces NetworkPolicy — this is a real result, not a vacuous one).
   **Not done:** the ambient `AuthorizationPolicy` (the NetworkPolicy
   peer is label-based, not ServiceAccount-based) and the §6a Istio
   checks.
5. **Scale prerequisites (§7a):** hibernate as the inactive default,
   with **zero live leases as a hibernation precondition** (the HIB-1
   fix, and now required by §4); hub defects **D1** (`FREE_STATEID` →
   `LOCKS_HELD` after the last `LOCKU`) and **D2** (`CLOSE` →
   `BAD_STATEID` after a restart) fixed test-first; operator-driven hub restart after a
   flint-csi-node roll; hibernated share = CR only.
6. **Real-hub rig** (never run: the 3,000-share rig used stubs): 10–30
   real hubs behind the proxy at 1,000–10,000 files each, measuring
   RSS/CPU per hub, wake time from hibernate, proxy throughput, and the
   operator at about 1,000 live + 10,000 parked. **Its numbers decide
   multi-volume.**
7. Multi-replica proxy (shared client table) before about 1,000 active.

Drills. Each one needs an arm that fails when the mechanism is removed:

- **Two hubs, colliding stateids**: one client, one open on each hub,
  `TEST_STATEID` on both. With H2 off this **must fail** — that is the
  control that proves the test reaches the collision.
- **Replay through the proxy**: kill the client→proxy connection after
  a non-idempotent op (`CREATE`, `RENAME`) reaches the hub. The
  retransmission must be answered from the hub's reply cache. Check the
  hub's op counter, not the client's exit code.
- **Hub restart under a writer; proxy restart under a writer**: no
  reclaim, and the file has no gaps.
- **Client reboot propagates**: new verifier → the hub discards the old
  state (convergence), and the other hubs discard theirs on next use.
- **Parked hub**: a remote `hard` mount touches a suspended workspace →
  the hub wakes, and the read completes. The control is the same
  touch against a direct mount, which hangs.
- **Cross-workspace rename** → `EXDEV` locally. With H1 off, it reaches
  the proxy's `XDEV` arm.
- **Mixed-target refusal counter** stays at 0 across the whole Linux
  suite (`nfstest`, the kind rig).
- **A/B perf** against a direct mount: small-file, metadata storm,
  sequential throughput. Report ranges over three pairs.

## 9. Open questions to settle before code

- Whether Linux's no-grace recovery after `SEQ4_STATUS_ADMIN_STATE_REVOKED`
  recovers opens on the affected workspace **without** disturbing the
  others. **Answered by the census (Part 3): NO.** Linux 6.12 lost
  locks in the other export too. So §4 never sets the flag. The open
  question is now whether per-op stateid errors *without* the flag stay
  per-state. That is the step-3 drill.
- Whether the hub accepts a `CREATE_SESSION` without a back channel,
  and behaves correctly with one. With delegations off it should never
  use it; confirm that.
- The hub's lease time and the proxy margin, as concrete numbers.
- ~~Whether any real client compound crosses targets in a way §3
  refuses.~~ **Answered by the census:** none did. Crossings are
  `PUTFH(pseudo-root), LOOKUP`; a cross-workspace rename never leaves
  the client when the fsids differ; `LOOKUPP` did not appear. See
  `flint-lite-nfs-proxy-census-2026-09-27.md`.
