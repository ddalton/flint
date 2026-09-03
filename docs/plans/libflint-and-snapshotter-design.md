---
title: libflint and the containerd snapshotter — the userspace client tier and the model-image endgame
status: designed
type: design-impl-spec
tags: [libflint, snapshotter, pnfs, block-layout, spdk, containerd, erofs, registry]
created: 2026-08-09
governs:
  - libflint/ (new — Rust core crate, C ABI cdylib, bindings)
  - flint-snapshotter/ (new — containerd proxy snapshotter binary)
  - spdk-csi-driver/src/main.rs (coordinates-publish branch in node_publish_volume; ControllerPublish token mint; NodeGetVolumeStats pNFS branch)
  - spdk-csi-driver/src/pnfs_csi.rs (coordinates ctx keys, sc_params)
  - registry-driver/ (new — CNCF Distribution StorageDriver on libflint)
  - conformance/ (new — adapter conformance kit + RFC 9561 §2.2.4 client obligations suite)
---

# libflint and the containerd snapshotter

## 1. Summary

Two coupled deliverables, both standing on the block-layout substrate
(`docs/plans/pnfs-block-layout-design.md`, phases 1-4 prerequisite — phase 4 *is* this doc's
anchor adapter):

**libflint** is the highest-performance client tier: a userspace NFSv4.1 **metadata** client
plus SPDK's NVMe-oF TCP initiator as the off-the-shelf data client. The block layout is what
makes this small — it shrinks the userspace client to a metadata problem (block doc §4b). We
write session/slot machinery, LAYOUTGET/LAYOUTCOMMIT/GETDEVICEINFO, an extent cache, and a
reservation-conflict state machine. We write **zero data-path code**: the data plane is
`spdk_nvme_connect` + qpair polling, upstream, unmodified. This is the only tier that escapes
the ~5-6 GB/s per-node kernel `nfs_client` wall (measured, runbh) and the ~700-900 MiB/s
sunrpc per-connection ceiling (measured, runbi) — because it is per-process, not per-node.

**The containerd snapshotter** is the endgame for model-image distribution: in-cluster pulls
become mounts, and the registry degenerates into an index/auth layer over flint's
content-addressed store. The bytes live once per cluster (scoped in §10 — lane 3 pays a
per-image assembly copy until extent clone is proven; §15 for what "once" costs in
durability); a "pull" resolves a digest and
attaches a device or hands coordinates to libflint. The unpack tax (gunzip at low hundreds of
MB/s on every pulling node — `docs/oci-registry-pnfs-architecture.md` §5) moves to a
once-per-push EROFS conversion.

**NO FUSE anywhere in either deliverable.** The FUSE mount-pod tier exists separately as the
generic fallback for POSIX-shaped workloads and is specified here only enough to bound it (§7).
Everything that needs byte-range locks, mmap, or shared mutable files routes there or to the
file-layout class — that guardrail is what keeps libflint's correctness surface small.

## 2. libflint layer cake

```
adapters        registry StorageDriver · fsspec/PyArrow · S3 sidecar · Hadoop FS · Go io/fs
bindings        Go cgo · Python pyo3 · Java JNI/FFM · C++ header
stable C ABI    libflint.so — flat, versioned, symbol-hidden internals
Rust core       session engine · extent cache · SPDK initiator glue   (NOT public API)
```

The Rust core is not a public crate surface; the contract is the C ABI. Rationale: the SPDK
ABI churns per release (`libspdk.so` SO_VER 8.0 at v26.05, bumped each release —
`~/github/spdk/shared_lib/Makefile`), so we statically link the initiator closure inside
`libflint.so` with a version script hiding everything but our exports.

**SPDK embedding — the brief confirms "off the shelf, no hugepages," with three nuances that
are load-bearing, not caveats-in-passing:**

- **No hugepages, no root, no VFIO — confirmed at code level.** `no_huge` is a first-class
  `spdk_env_opts` field (`~/github/spdk/include/spdk/env.h:66`); it expands to
  `--no-huge --legacy-mem --iova-mode=va` (`lib/env_dpdk/init.c:479-481`) and the `-s` heap is
  ordinary anonymous memory — unprivileged pod, no hugetlbfs, no CAP_SYS_ADMIN. **But
  `mem_size` MUST be set or init fails** (`init.c:462-468`) — the exact rule flint's chart
  already documents for the target side (`values.yaml:652`). Env config:
  `no_huge=true, mem_size≈64-256MB, core_mask="0x1", no_pci=true`. Only qpair internals draw
  from the SPDK heap; **I/O payloads are arbitrary application memory** on TCP — contig
  payloads go straight into the socket iov (`lib/nvme/nvme_tcp.c:834-848`), zero vtophys calls
  in the whole TCP transport.
- **App-thread qpair model, no reactor.** The NVMe driver is passive — spawns no threads
  (`doc/nvme.md:19-21`). One qpair = one thread at a time, unenforced (`doc/nvme.md:155-160`);
  the design shape is one qpair per I/O thread, or `spdk_nvme_poll_group` for many volumes on
  one thread. Housekeeping: one plain pthread at ~1 Hz calling
  `spdk_nvme_ctrlr_process_admin_completions` for keep-alive — this is exactly what upstream's
  own `spdk_nvme_perf` does (`app/spdk_nvme_perf/perf.c:3341`), so foreign pthreads are a
  proven model.
- **Polling discipline: TCP qpairs are poll-only in v26.05.** `spdk_nvme_qpair_get_fd` exists
  but only PCIe implements it (`nvme_tcp.c:3000` has a TODO, no op). No epoll integration for
  data qpairs — every binding owns a polling thread. Idle polling is cheap (socket-level readv
  attempts), but it is a budget line, not free.

**RDMA caveat that shapes the ABI now:** under RDMA (later phase, Azure only — EFA has no RC
verbs) payloads must come from SPDK-registered memory (`lib/rdma_utils/rdma_utils.c:64-108`);
the TCP "any buffer" luxury does not carry. The buffer API hands out SPDK-heap buffers from
day one, so the ABI survives the transport flip. Licensing is clean: the entire
`spdk_nvme spdk_env_dpdk sock_*` static closure is BSD-3-Clause; flint's six carried SPDK
patches are all target-side, none touch `lib/nvme`.

Rust bindings reality: no viable in-tree or external crate (openebs/spdk-rs binds the full app
framework and tracks a fork). We build our own `flint-spdk-sys`: bindgen over a curated shim
header (~40 functions, env + nvme only), static link with `-Wl,--whole-archive` (constructor
registration — forget it and the posix sock module silently vanishes, `doc/pkgconfig.md:23-51`).

## 3. The semantic contract ("S3-plus")

libflint's API contract is object-store semantics **plus** the five things S3 cannot give:

| Capability | Mechanism | Status |
|---|---|---|
| Atomic RENAME | NFSv4 RENAME via MDS — one syscall server-side (`fileops.rs:2919-3097`) | serves today |
| Append | OPEN + WRITE at EOF; Distribution `Writer(append)` contract | serves today |
| Sub-file reads | extent reads at any offset; `Reader(offset)` / `cat_ranges` | phase-4 |
| Single-writer opens | OPEN share deny, **enforced server-side** (`stateid.rs:301-332`; both OPEN paths `ioops.rs:650-669`, `809-825`; WRITE-path bypass closed `ioops.rs:1212-1225`) | serves today |
| Whole-file advisory locks | LOCK offset 0, len 2⁶⁴-1 — LOCK/LOCKT/LOCKU are real (`lockops.rs:502-759`) | serves today |

Atomic RENAME is the Iceberg/registry differentiator vs S3A: S3A's rename is copy-then-delete,
non-atomic, and the reason the entire S3A committer machinery exists; Iceberg's HadoopCatalog
commits by renaming the metadata file and is unsafe without atomic rename. flint's is a real
POSIX rename in one namespace (§8.4).

**REFUSED**: byte-range locks, mmap, shared mutable files, delegations. The MDS never grants
delegations anyway — the OPEN encoder hardcodes OPEN_DELEGATE_NONE (`compound.rs:2208`;
rationale `ioops.rs:887-896`) — so libflint implements nothing delegation-shaped. Workloads
needing the refused list route to the FUSE tier or the file-layout class (§7).

Locks are metadata-plane: one MDS RTT, zero data-path cost — a lock confers no data-path
authority; writes travel via an extent grant **or the MDS fallback lane** (block doc §8 —
a real, steady-state write path, not a rounding error; the lock story must hold on both,
§13). Cross-tier coherence is real:
kernel flock() on an NFSv4 mount becomes a server-visible whole-file lock (nfs(5), clients
>2.6.12), arbitrated by the same MDS LOCK path (`dispatcher.rs:1733`). **Honest caveat:**
kernel clients mounted `local_lock=flock|all` keep flock local and are invisible to this
coherence — that goes in the compatibility matrix. Two more server-dialect facts the contract
must absorb (per the MDS survey): **no blocking-lock queueing** — conflicts return
NFS4ERR_DENIED immediately with LOCK4denied (`compound.rs:1989-2010`), so adapters poll; and
**lock loss at lease expiry is silent** — the courtesy sweep releases locks with no callback
and `sr_status_flags` is always 0 (`dispatcher.rs:797-801`), so libflint detects state loss
only via BADSESSION/BAD_STATEID and must treat every lock as lease-bounded.

**THE LOCK-FENCE COUPLING.** On the block class, lock loss at lease expiry is a fencing
problem: the (ex-)holder has raw write reach until the NVMe fence lands. Today the lease sweep
releases locks/opens but **does not sweep pNFS layouts at all** — nothing calls
`LayoutManager::return_all_for_client` from the expiry path (verified; layouts die only via
LAYOUTRETURN or recall). Block doc §8 already closes this for grants: lease expiry must become
an *event*, and for block-class volumes the order is **fence-first** — (a) fence the client's
NVMe host (reservation preempt per block doc §5), (b) only then release its locks/opens,
(c) only then let grant GC proceed. Release-before-fence would hand the lock to a successor
while the ex-holder still has raw device reach — that ordering is the hazard, not a variant.
Lock acquisition attempts landing in the (a)→(b) window get NFS4ERR_DENIED like any other
conflict: the dead client's locks stay on the books until the fence lands. The file class
keeps today's release-at-sweep — a file-class lock holder has no raw device reach.
Consequence: **block-class locking ships gated on fence proof — the relevant
`FenceReaches*` constant, per consumer class (§13 splits it); the file class gets locks
first**, ungated. **The gate is a server work item, not a vibe:** LOCK serves today with zero
layout-class awareness (`lockops.rs:502-759`), so the moment a block-class volume exists any
kernel client can take the "gated" lock. The MDS grows a per-class refusal — LOCK on a
block-class filehandle returns NFS4ERR_NOTSUPP while the gate holds — a named deliverable in
block doc phase 1 alongside the allocator, and the error lands in the compatibility matrix
so adapters can document it. Verification split for the coupling itself: §13.

## 4. Metadata client scope

What libflint speaks, against the server dialect actually implemented in this repo:

- **Session establishment**: EXCHANGE_ID (SP4_NONE — SP4_SSV is refused with ENCR_ALG_UNSUPP,
  `session.rs:265-268`; stable co_ownerid AND stable RPC cred across
  EXCHANGE_ID→CREATE_SESSION or CLID_INUSE, `session.rs:409-421`), CREATE_SESSION with
  CONN_BACK_CHAN, DESTROY_SESSION/DESTROY_CLIENTID. `eir_sequenceid` starts at 1 — a client
  sending csa_sequence=0 gets SEQ_MISORDERED (`session.rs:1313-1315`).
- **SEQUENCE discipline**: full 128-slot table available (`sr_target` always
  ca_maxrequests−1); retransmit = same slot+seqid; RETRY_UNCACHED_REP means wait, never
  re-execute (`session.rs:812-825`). Every SEQUENCE renews the lease — SEQUENCE is the *only*
  renewal path (`session.rs:836`). **Size I/O to the negotiated maxima, not 1 MiB**: the
  server caps fore channels at exactly 1 MiB and Linux-style COMPOUND overhead subtraction
  applies (`session.rs:20-44`).
- **Namespace + I/O fallback**: PUTROOTFH/PUTFH/LOOKUP/GETFH/GETATTR/ACCESS/READDIR,
  OPEN (CLAIM_NULL and CLAIM_FH only — all the server *serves*, `ioops.rs:84-91`; every
  claim type 0-6 decodes cleanly, `compound.rs:1276-1314`, but reclaim claims hit the grace
  gate below and delegation claims are unsupported — no BADXDR on any of them), CLOSE,
  SETATTR, CREATE/REMOVE/RENAME, READ/WRITE/COMMIT. The NFS READ/WRITE lane is the one place
  "metadata-only client" leaks data-path NFS back in: sub-blksize tails and manifest-sized
  writes route via MDS by design (block doc §8 fallback lane). For blob workloads tails are
  rare but not zero — metered, not hidden (F68 lesson).
- **pNFS**: LAYOUTGET/LAYOUTRETURN/LAYOUTCOMMIT, GETDEVICEINFO with honest maxcount and
  TOOSMALL handling (the server ignores maxcount today, `dispatcher.rs:2555-2559`; phase 1
  makes it real), RECLAIM_COMPLETE — **non-negotiable**, the server refuses OPEN during grace
  until it arrives (`dispatcher.rs:1227-1262`).
- **Backchannel on the fore connection — no listening socket, ever.** CREATE_SESSION's
  CONN_BACK_CHAN binds the very connection it arrived on (`session.rs:585-591`); libflint's
  read loop demuxes RPC CALL vs REPLY and answers CB_SEQUENCE + CB_LAYOUTRECALL inline,
  never head-of-line-blocking a fore request (audit R2, `back_channel.rs:113-121`). Offer
  back-channel ca_maxoperations ≥ 2 (a recall is two ops). **Correction to the "backchannel
  is mandatory" claim in one research brief:** the server survey shows a client MAY offer no
  backchannel (cb_program=0) — recalls classify NoChannel and the server revokes unilaterally
  (`callback.rs:541-546`), safe iff the client treats every layout as instantly revocable.
  We implement the backchannel anyway (it is ~200 lines of demux and buys graceful truncate
  coordination), but the degraded mode is a legal fallback, not a protocol violation.
- **Stateid practice**: track one open-stateid per (owner, fh); send seqid=0 for I/O; use
  returned seqids only for CLOSE/OPEN_DOWNGRADE ordering (`stateid.rs:621-696`).
- **Reservation conflict**: NVMe status 0x83 ⇒ commit what's committable, LAYOUTRETURN,
  unregister (RFC 9561 §2.2.4) — libflint owns this state machine, and it is conformance-
  tested (§13).

**NOT implemented, on purpose**: no cache-coherence protocol (the server's change_info is
synthetic — OPEN reports 0/1, RENAME 1/2 (`ioops.rs:688-695`, `fileops.rs:3074-3079`); a
client that built cache consistency on those counters would be building on sand); no POSIX
vfs semantics; no delegations; no byte-range lock API.

## 5. K8s delivery — the PVC stays the control plane

The PVC remains the unit of provisioning, authorization, and lifecycle. What changes is the
publish verb: for coordinates-class consumers, NodePublish writes a file instead of mounting.

**Coordinates-publish.** A new first-priority branch in `node_publish_volume` (before the
pNFS mds-ip check at `main.rs:3969`), keyed on an explicit discriminator
(`pnfs.chert.us/layout: block` + a publish-mode key — presence-based classification is the
§7 landmine of the block doc; new SC keys must join `sc_params::ALL` or provisioning
hard-fails, `pnfs_csi.rs:63-70`). It writes `volume.json` into `target_path`:

```json
{
  "version": 1,
  "mds": { "endpoints": ["10.0.1.5:2049"], "shard": "m2" },
  "volume": { "handle": "pvc-abc~m2", "layoutClass": "block",
              "sizeBytes": 107374182400, "stripeSize": 1048576, "stripeWidth": 1 },
  "identity": { "hostnqn": "nqn.2024-11.com.flint:pod:<ns>.<pod-uid>" },
  "auth": { "token": "<short-lived, node-scoped>", "expiresAt": "..." },
  "pod": { "name": "...", "namespace": "...", "uid": "..." }
}
```

Deliberately absent: device addresses. Targets, NGUIDs, and extents come from
GETDEVICEINFO/LAYOUTGET on the wire, so fencing and grant state never go stale in a file.
The pod keys are free: `podInfoOnMount: true` is already set (`csidriver.yaml:12`) and the
driver currently drops every key but `ephemeral` — we start reading them. A directory of
JSON has none of the dead-NFS-mount D-state probe hazards that unpublish is currently
armored against (`main.rs:4549-4558`) — itself an argument for this shape. NodeUnpublish
classification uses an on-disk marker (the ephemeral-marker precedent, `main.rs:4448-4457`)
because unpublish receives no volume_context.

**ControllerPublish is the add_host authorization moment.** The existing pNFS no-op
(`main.rs:2159-2166`) is the block doc's "free per-node hook": ControllerPublish knows
(volume, node, hostnqn, access mode, readonly) — hostnqn is a pure function of node name
(`identity.rs:458-460`) — and its publish_context reaches NodePublish. Here it grows real
work: `nvmf_subsystem_add_host` for the grant (idempotency precedent `hot_rejoin.rs:1513-1528`)
and minting the scoped token. ControllerUnpublish needs an explicit pNFS-class branch for
remove_host — today it is an *accidental* no-op via an error path (`main.rs:2440-2447`).
**Token caveats (all three bite):** publish_context is world-visible in the VolumeAttachment
to anyone with VA read — the csi-node role itself has it (`rbac.yaml:133-137`) — so tokens are
short-lived and node-bound, never long-lived bearer; the VA snapshot is replayed stale on
restage (F41, `main.rs:2957-2959`), so libflint exchanges the coordinates token for a session
at the MDS rather than the node plugin re-minting (which would also break the
`MinimalNodeService` design invariant that the node side never dials the MDS,
`main.rs:2844-2852`). **And "short-lived" forces the refresh question now, not later:**
`volume.json` is written once at publish; kubelet never re-publishes an already-published
volume on its own, and the node plugin must not re-mint — so without a refresh path, any
consumer that outlives the TTL and then needs a *new* session (container restart days
later, MDS failover, session loss after a partition) reads an expired token and is dead
forever. The answer is `tokenRequests` + `requiresRepublish: true` in the CSIDriver,
**adopted now rather than deferred**: kubelet re-runs NodePublish periodically with a fresh
serviceaccount token — refresh, per-pod cryptographic binding, and the end of tokens
transiting the world-visible VolumeAttachment at all, one chart change.

**Per-pod NQN.** libflint runs pod-networked; RFC 9561 fencing is per Host Identifier, so a
per-pod NQN buys pod-granular fencing instead of cutting a whole node — and keeps fencing a
libflint pod from also fencing the node's kernel client. **Hard constraint:** `converge_hosts`
only fences NQNs matching `FLINT_HOST_NQN_PREFIX` (`nvmeof_export.rs:499`) — the pod family
`nqn.2024-11.com.flint:pod:…` must be added to the match or per-pod identities are invisible
to the existing fence. NQN is client-asserted: this buys granularity, not authentication
(block doc §7's stance stands; DH-HMAC-CHAP/TLS exist in SPDK v26.05 if ever wanted).
**Who admits the pod NQN — assigned, not implied:** ControllerPublish is per (volume, node)
and knows only the node identity; the pod NQN exists only at NodePublish time, and the node
plugin must not dial the target or the MDS (MinimalNodeService, above). Left there, the
identity the data plane actually uses is never on any allow-list and the pod's
`nvme connect` is refused — the fence-side prefix fix above is only half. So
ControllerPublish's `add_host` admits the *node* family, and the **MDS admits the pod**: at
coordinates-token exchange it performs `nvmf_subsystem_add_host` for the pod NQN bound into
the token — the token carries the pod UID, so an arbitrary pod on the node cannot claim a
sibling's NQN — and removes it at session teardown or lease expiry. `converge_hosts`
reconciliation must treat MDS-granted pod NQNs as part of the desired set, keyed off live
grant state rather than the static per-node list, or the reconcile loop races freshly
admitted pods off the allow-list. RWX falls out: one ControllerPublish per node, N pod
admissions at the MDS.

**The hybrid publish.** One publish can carry both a kernel mount *and* coordinates: hot
blobs via the kernel client's page cache, cold reads and writes via the library, one PVC.
This is a publish-mode value (`mount`, `coordinates`, `hybrid`), not a new class. **But it
is a cache-coherence commitment, and §4 just refused the machinery that would back it**:
the server's change_info is synthetic (OPEN 0/1, RENAME 1/2), and the kernel client's
dentry/readdir cache revalidates on exactly those counters — the kernel client *is* the
"client that built cache consistency on sand." A blob written via the library and RENAMEd
into place can stay invisible on the mount side indefinitely, or a stale negative dentry
can linger — the same cached-negative-dentry class as the Spark committer wall (§8.4). So
hybrid ships with a written contract, not a vibe: (a) **immutable CAS namespaces only** —
write-once blobs under digest names; no path both sides write, ever; (b) the visibility
rule — a library write + RENAME-into-place becomes mount-visible within the mount's
directory attribute timeout — is honest only over a **real per-directory change
attribute**, so fixing the synthetic change_info is a named prerequisite of offering
hybrid, not an aspiration; (c) **file class only** — on the block class a kernel client
holding cached pages over extents the library rewrites has no invalidation path at all.
Hybrid gets a conformance line in §13; until (b) lands it is spec'd, not offered.

**Stats and expansion.** NodeGetVolumeStats grows a coordinates/pNFS branch reporting
capacity from publish-time context (`size-bytes`) with used-bytes omitted — CSI permits
omission, and the alternative (the node plugin querying the MDS) would violate the same
MinimalNodeService invariant this section just leaned on; if used-bytes ever matters, the
controller pushes allocator stats into a volume attribute the node merely reads —
**fixing a live defect: every pNFS
volume is misreported `abnormal: true` on every kubelet stats poll today** because the stats
path requires a `disk.chert.us/node-name` attribute pNFS PVs don't have
(`main.rs:4779-4791`, `driver.rs:1650`). Expansion is observed by the library via GETATTR
size polling — no publish-time replumbing.

**NetworkPolicy delta.** A userspace client originates 2049 and 4420 traffic from POD IPs;
today's 2049 NetworkPolicy admits `nodeCIDRs` only (`pnfs-security.yaml:63-68`) — its premise
is "mounts originate from node kernels." The policy grows a pod-selector arm for
coordinates-class consumers — **on 2049 only**. Precision about what inverts: the block
doc §7 caveat is about the *target* — spdk-tgt lives in the hostNetwork csi-node DaemonSet
and NetworkPolicy cannot select host-network pods — so 4420 ingress stays
security-groups/host-firewall no matter how the client is networked, until a pod-networked
target fleet exists. What client-side pod networking buys is enforceable client-*egress*
policy and the pod-selector ingress arm on the (pod-networked) MDS's 2049.

**The discovery mini-spec.** Genericity by protocol, not by owning every integration: a
well-known env var (`FLINT_VOLUME_DIR`) pointing at the directory containing `volume.json`,
plus the schema above, is the entire integration contract. The conformance kit (§8) is its
executable form — community adapters self-certify against it.

## 6. Caching — the honest ranking

The library forfeits the kernel page cache. That is not a footnote; it is the number-one
performance liability of any userspace client, and we rank it above every protocol
optimization in this doc:

1. **Hot-dominated serving is RAM-cache-bound.** A registry serving the same hot blobs is won
   or lost on cache hits, not wire speed. The kernel-mount tier gets this for free; libflint
   only matches or exceeds it **once its own cache exists**.
2. Therefore the **hot-blob LRU (RAM tier + optional local-NVMe tier) is a NON-OPTIONAL line
   item of the registry adapter**, sized into its estimate (§14), not a fast-follow.
3. Content-addressed data makes the cache trivial to keep correct: no invalidation protocol,
   eviction only. This is the one place the registry workload is *easier* than generic NFS.
4. Cold/streaming reads (model weights, large sequential blobs) don't want a cache at all —
   the runbk finding that a fully-cached server-side RAM read was *slower* than O_DIRECT
   (measured, runbk: 1470 vs 3173 MiB/s) does not transfer to the client side, but its lesson
   does: caching streaming reads is a loss; the LRU admits by re-reference, not by read.

## 7. The FUSE mount-pod tier (bounding only)

Specified here only to bound it; it is a separate deliverable. Per-volume mount pods
(the JuiceFS pattern), explicitly **NOT in csi-node** — the spdk-tgt-in-csi-node roll
landmine (DS rolls restarting the data plane under mounted PVCs) is the precedent we do not
repeat with a FUSE daemon. FUSE-over-io-uring where the kernel offers it. Same coordinates
contract (§5) — the mount pod is just another libflint consumer. Everything that needs
byte-range locks, mmap, multi-writer, or POSIX vfs semantics routes here or to the
file-layout class. That routing rule is the guardrail that keeps libflint's refused list (§3)
refused, and its correctness surface small.

## 8. Adapter roadmap — priority by users-per-line

The conformance kit rides alongside all of these: golden tests for the §3 contract
(rename atomicity under crash, deny-conflict behavior, append semantics, offset-read
correctness), runnable against any adapter. It is the drills analog for client code.

1. **Registry StorageDriver (anchor — block doc phase 4).** Distribution v3 interface, pinned
   at registry:3 (`RedirectURL` replaced 2.x's `URLFor`). The mapping is almost embarrassingly
   direct: `Writer` → OPEN(CREATE, SHARE_DENY_BOTH) — two racing upload writers get
   NFS4ERR_SHARE_DENIED instead of interleaving, enforced today (`stateid.rs:301-332`);
   `Commit` → LAYOUTCOMMIT + CLOSE; `Cancel` → CLOSE + REMOVE; `Move` (upload-session →
   `blobs/sha256/xx/<digest>/data`) → atomic RENAME; `Reader(offset)` → extent pread;
   `RedirectURL` → `""`. The Move target IS the CAS path the snapshotter reads — the two
   deliverables share one namespace by construction.
2. **fsspec + PyArrow.** `AsyncFileSystem` with `_cat_ranges` as the perf contract (maps 1:1
   onto extent reads); `PyFileSystem(FSSpecHandler(fs))` buys Arrow/Parquet/Dataset for free.
   Model to copy: obstore — native pyo3 API with buffer-protocol zero-copy, fsspec as the
   thin compatibility layer, not the primary API.
3. **Localhost S3 sidecar gateway.** The middle tier: loopback copies, no kernel storage
   stack, zero app changes; fills the vacuum MinIO gateway's removal left. Floor: SigV4
   (header + presigned — Velero requires presigned validation), Put/Get(+Range)/Head/Delete/
   ListObjectsV2, full multipart (parts 1-10000, 5 MiB min, out-of-order, re-uploadable),
   DeleteObjects. **The multipart traps are real:** the ETag is not an MD5
   (md5-of-part-md5s + `-N` — the s3proxy #338 breakage class); CompleteMultipartUpload's
   200-with-error-in-body must be implemented or SDK retries corrupt state; and
   complete-by-concatenation costs 3x IO (s3proxy #292: client timeouts at 25 GB) — concat
   vs reserve-and-copy-last-part is a real design decision, since part sizes are unknown
   until receipt. flint's differentiator: assemble under a temp name, atomic-RENAME into
   place, under a deny-both OPEN — an atomic exclusive multipart-complete no FS-backed
   gateway can promise.
4. **Hadoop FileSystem (Spark/Iceberg).** Rename atomicity is the whole pitch: HadoopCatalog
   is safe again, no lock manager, no committer machinery. Reconciling with
   `docs/plans/pnfs-spark-flight-benchmark.md`: Findings 2/3 (the committer wall —
   `File.mkdirs()` failing on cached negative dentries) are *kernel-client* behavior, and a
   libflint-backed FileSystem makes that failure class structurally impossible; Blocker #1
   and Finding 4 are FIXED (`docs/plans/pnfs-csi-rwx-and-committer-fixes.md`) and must not
   be re-cited as live. **Honest paragraph the pitch owes:** extent RW grants are exclusive —
   Spark's many-executor concurrent-write commit pattern serializes via recall/regrant on
   the block class; the file layout remains the answer for shared-write-heavy stages. Java
   binding (JNI/FFM over the C ABI) is its own deliverable line.
5. **Go io/fs.** Read-only, small, mostly falls out of the cgo binding.

## 9. The snapshotter — three lanes

Lane detection at Prepare time by layer media type and labels (§10); one proxy plugin, three
serving shapes:

- **Lane 1 — small layers, materialized normally.** App code, configs: pulled and unpacked as
  usual (or via containerd's own EROFS differ where present). Not worth optimizing; keeping
  it boring keeps the snapshotter honest.
- **Lane 2 — model layers, NEVER materialized.** Prepare returns a stub + coordinates; the
  inference server reads weights by digest through libflint. Zero kernel storage stack, zero
  HTTP, lazy to the byte, and the access pattern (large sequential reads of content-addressed
  blobs) is the block layout's declared best case. This is the GDS-shaped lane: 4 KiB-aligned
  registered-buffer reads into pinned host memory (§15 for the honest GDS bracket).
  ModelPack's `weight.v1.raw` layers are the ideal input — no tar handling at all.
  Two bills this lane pays explicitly. **Authorization**: an image pull traverses no PVC,
  no VolumeAttachment, no ControllerPublish — there is no §5 token mint anywhere in the
  flow, and the snapshotter is node-side, so self-minting would violate the same
  MinimalNodeService posture §5 defends. The snapshotter therefore returns coordinates
  *without* a token; the consuming pod exchanges its own identity — its `tokenRequests`
  serviceaccount token (§5) or a PVC binding — at the MDS for the session. The PVC stays
  the unit of provisioning and lifecycle; for lane 2 the *pod identity* is the unit of
  authorization, and §5's token machinery is what makes that non-circular. **Opt-in**:
  media type asserts "this layer is weights," not "this consumer speaks libflint" — a
  ModelPack image on a libflint-unaware runtime would find a stub where its weights should
  be and fail at inference time in an app-specific way. So lane 2 is opt-in per workload
  (pod annotation / RuntimeClass), and weight layers whose consumer has not opted in fall
  back to lane 3 automatically. The stub itself is a fail-fast marker — a sentinel file
  naming the digest and the coordinates path, never zero bytes of silence — so a runtime
  that reaches it anyway fails loudly at open, not mysteriously downstream.
- **Lane 3 — unmodified apps: EROFS images off flint NVMe/TCP namespaces.** No FUSE, no
  unpack, page-cache-backed reads on the consumer node; the overlaybd shape on our fabric,
  minus TCMU — the device is a real NVMe-oF namespace, not a userspace-backed loopback.
  `Mounts` returns erofs + overlay stacks (precedent: containerd's own core EROFS
  snapshotter, experimental since 2.1 — PR #10705).

**HONEST CORRECTION, carried from the design conversation and kept prominent:** a container
rootfs CANNOT be served from userspace — containers are kernel process trees; their root
filesystems are kernel mounts, full stop. Lane 3 is not a defeat or a fallback; it is the
*good* kernel path: the thinnest possible kernel surface between a process tree and flint
bytes.

**KERNEL FLOOR ADVANTAGE — both lanes dodge the blocklayout floor entirely.** The pNFS block
client floor is mainline ≥ 6.11 + CONFIG_PNFS_BLOCK, with silent-MDS-degradation below it
(block doc §4a). The snapshotter needs none of that: lane 3 needs nvme-tcp + EROFS, plus
loop-offset/dm-linear for intra-namespace layer addressing (§10 — counted in the floor,
not smuggled past it) — and the EROFS floor is **Linux 5.4** for plain ro block-device
mounts (fs/erofs mainlined 5.4; the 5.19 fscache mode is cited only to reject it), the
oldest, most conservative EROFS path there is — **provided ingest pins the mkfs feature
profile to the 5.4-readable set**: chunk-based layout, dedupe, fragments, and non-lz4
compression all raise the on-disk floor, so the converter flags are fixed in the ingest
spec, never left to erofs-utils defaults, or the floor claim and the tooling claim drift
apart in practice. Lane 2 needs **no kernel path at all**. Nydus removed FUSE but kept a
userspace data daemon feeding cache misses; flint removes both, because the backing store
already *is* a block device — a miss is just a block read. That sentence is the sharpest
differentiator this design owns.

Also free: lane 2 dodges the udev NGUID landmine (block doc §4a) completely — libflint
resolves namespaces by NGUID from Identify data, never via `/dev/disk/by-id` symlinks.

## 10. Snapshotter mechanics + ingest

**Proxy snapshotter gRPC.** Registered via `[proxy_plugins]` (type `snapshot`, unix socket);
implements snapshots.v1 (Prepare/View/Mounts/Commit/Remove/Stat/Update/List/Usage/Cleanup).
Lazy-pull protocol per containerd's remote-snapshotter doc: Prepare arrives with
`containerd.io/snapshot.ref` = target ChainID plus the CRI labels (`cri.layer-digest`,
`cri.image-ref`, `cri.image-layers`); if we can serve the layer, return **ErrAlreadyExists**;
the client Stats the ChainID and skips download + unpack. Idempotent under re-Prepare after a
GC'd lease (return ErrAlreadyExists again). **Version gates, stated plainly:** CRI requires
`disable_snapshot_annotations = false` (default true — the classic gotcha); 1.7.x **and
2.0.x** work with the classic keys unchanged — 2.0's CRI still pulls via the classic local
path, zero transfer-service involvement (verified against release/2.0
`internal/cri/server/images/image_pull.go`, which has no transfer imports, and its
`config.go`, which has no `use_local_image_pull` key at all). The flip is **2.1+**: CRI
pulls route through the transfer service by default, so 2.1+ needs either
`[proxy_plugins.<name>.exports] enable_remote_snapshot_annotations = "true"` (PR #11195) or
`use_local_image_pull = true` (key introduced in 2.1).

**Digest → artifact resolution.** The `cri.layer-digest` label + the Distribution CAS layout
(`blobs/sha256/xx/<digest>/data`) means the snapshotter addresses any layer with zero registry
round-trips once it holds PVC coordinates. Lane detection: media-type allowlist — ModelPack
(`application/vnd.cncf.model.weight.v1.*` — the strongest standard), Docker Model Runner
(`vnd.docker.ai.*`), plus annotation/image-ref fallbacks for KServe modelcars, which are
indistinguishable plain images. Multi-arch: ChainIDs are per-platform diffID chains — store
lookup keys on (digest, platform).

**GC/leases.** containerd GC deletes *snapshot records* via chain refs; the snapshotter's
Remove/Cleanup is "detach namespace + drop grant + forget" — **never "delete blob."** The
authoritative bytes are pinned by the registry CAS, not containerd's to collect. Content-store
tooling that re-resolves absent blobs (`ctr image export`) will fetch lazily or fail; stated,
accepted. **And the containerd side is only half the GC story**: the registry has its own GC
(Distribution's garbage-collect of untagged manifests), and nothing in that tool knows a
blob's extents currently back a live lane-3 rootfs or an in-flight lane-2 read. Image
updates make the collision routine, not exotic — retagging `:latest` orphans the old digest
while pods still run it, and an unpinned collect turns their root filesystems into EIO. So
the cross-domain pin is a first-class design element: every lane-2/3 attachment registers a
per-digest pin at the MDS (the party both domains already talk to); registry GC consults
pins and skips; pins ride the same grant/lease expiry as everything else, so a dead node
cannot pin forever. Image-update lifecycle, stated plainly: the old digest survives until
its last attachment drops.

**Ingest — the registry StorageDriver IS the ingest path.** Push writes layers into the
content-addressed store through the §8.1 driver; tar→EROFS conversion happens once at push
(erofs-utils ≥1.7 `--tar`; containerd's EROFS differ is the production evidence the
conversion is shippable). The unpack tax moves from N pulls to 1 push. Dedup becomes
cluster-wide: one copy per cluster versus one per-node content store. **In-cluster publish**
(a training job publishing a checkpoint) is a direct libflint write + manifest registration —
the bytes never traverse HTTP or the registry process at all. Future flag, direction not
promise: clone-publish via refcounted extents would make publish O(metadata) — but block doc
§12 refuses CoW/CLONE for the block class today and the snapshotter is read-only over blobs,
not clone-based; **unproven, and this doc does not spend it.**

**Session budget.** Namespace-per-layer would explode the block doc §5 session ceiling
(sessions = volumes × client-nodes, with layers as the multiplier). Lane 3 therefore mounts
per-*image* (or per-model-volume) namespaces with per-layer EROFS blobs inside, not
per-layer subsystems. **The CAS→namespace shape, named so its bill is visible:** a
per-image namespace is *assembled* — the MDS copies the image's layer blobs out of the CAS
into a fresh lvol at first in-cluster pull — because block doc §12 refuses refcounted-extent
clone and this doc does not spend it (above), and because exposing the whole CAS volume as
one shared namespace would show every consumer node every blob in the registry, which is
acceptable for model stores (§12) but not for lane 3's arbitrary apps. Assembly is a
server-side copy per image, once per cluster, not per node — so the §1 "bytes live once"
headline is **scoped**: it holds exactly for the CAS, lanes 1-2, and blob storage; lane 3
pays one assembly copy per image (shared layers duplicated across the images sharing them)
until clone is proven. Addressing per-layer blobs inside one namespace is loop-offset or
dm-linear on the consumer node — kernel surface the §9 floor now carries explicitly. And
the ceiling multiplier is worse than layers: libflint's per-pod model turns the block doc's
premise (sessions = volumes × client-*nodes*) into volumes × client-*pods*, and per-pod
NQNs turn add_host/remove_host into pod-churn-rate events against the `converge_hosts`
reconcile loop. The phase-2-style ceiling measurement therefore covers pod-multiplied
session counts **and** allow-list reconcile churn under pod creation/deletion storms —
before the rollout shape or the per-pod-NQN default is fixed.

## 11. Performance expectations, honestly bracketed

- **Pull cold-start collapses**: resolve + mount vs download + gunzip. The gzip decompress
  step alone (low hundreds of MB/s, per-node, `docs/oci-registry-pnfs-architecture.md` §5)
  usually dominates pull wall-clock; lanes 2/3 delete it entirely. This is the headline
  number — **to be measured, not asserted**: expected order-of-magnitude on multi-GB model
  images, claimed only after the A/B.
- **External push ~unchanged**: wire- and compression-bound; the EROFS conversion adds
  push-side latency the puller no longer pays. Net neutral to slightly worse at push, and we
  say so.
- **In-cluster publish potentially order-of-magnitude**: direct write vs
  compress→HTTP→registry→decompress. "Potentially" is load-bearing — the write-side fleet
  record is much thinner than the read side (every campaign wall we've measured — runbh,
  runbi, runbd — is a read wall), so write claims get the widest error bars in this doc.
- **Hot serving ranking**: RAM cache first (§6), wire second. A libflint registry without its
  LRU loses to a kernel-mount registry on hot traffic; with it, it wins on cold + concurrent.
- **The S3 lazy-pull arm is the external comparison, and it gets the headline mechanism
  for free** (added 2026-09-01): SOCI/nydus/stargz over an S3-backed registry delete the
  same download+gunzip tax — AWS measured 4.1–4.9x cold-start on GB-scale images with
  SOCI on ECR/S3. What the backend changes is the demand-fault path and the storm shape:
  SOCI against in-region ECR measures ~4.6 ms per range fetch with a warm connection
  pool and 62–290 ms cold (arXiv 2607.06868), with a userspace daemon on every miss;
  lane 3's miss is a sub-ms kernel block read on the NVMe-oF namespace with no daemon
  (§9's differentiator), and an N-node boot storm of one image is served from the DS's
  cache and fleet bandwidth instead of N full-wire downloads. S3 keeps durability, zero
  ops and per-request costs measured at ~5% of a pull's compute savings — so the honest
  posture is both: the S3/external registry remains the origin and re-fetch lane (§15),
  flint is the in-cluster serving tier. The cold-start A/B therefore runs THREE arms —
  baseline pull, lazy-on-S3, lazy-on-flint — because the lazy format and the backend are
  separable wins, and only the third arm attributes anything to flint.
- **Discipline**: every ratio ships from a same-hardware A/B with one variable moved —
  a ratio between legs differing in more than one way is not an attribution (runbl's lesson,
  learned the expensive way). Results land as ADR 0006+ with pass criteria declared before
  the run, single-AZ only (budget the bytes, not the hours).

## 12. Trust model

Node-level, identical to the block class — block doc §6 and the runbook's "Trust model
(production checklist)" are the normative references, not re-argued here. A layout-holding
client has raw write reach over the whole namespace (RFC 8154 §3, extents-are-permissions
§2.4.6 — inherited by RFC 9561, which adds §2.2 client fencing); reachability is the
boundary; multi-tenant clusters keep the file layout. Deltas this doc adds:

- **Snapshotter workers mount raw namespaces** — read-only exposure via `no_auto_visible` +
  per-namespace `nvmf_ns_add_host`. A reader still sees the *whole* namespace — which the
  §10 shape keeps proportionate: a lane-3 per-image namespace exposes that image's own
  layers (bytes the pod would see anyway), and whole-volume visibility on shared model
  stores is exactly the workload where it is acceptable; the doc says so rather than
  pretending otherwise. Write reach stays registry-side. For lane-2-only consumers the
  reservation story must be *consistent*, not hand-waved: the block doc §5 chose RTYPE=4h
  (Exclusive Access – Registrants Only), which denies **reads** to non-registrants — an
  unregistered lane-2 reader would eat 0x83 on its first pread. Read-shared namespaces
  therefore carry **RTYPE=3h (Write Exclusive – Registrants Only)** instead: unregistered
  readers pass, writers must register and stay fenceable — a scoped amendment to block doc
  §5, whose 4h choice stands for single-writer volumes. What lane-2 consumers actually shed
  is the §2.2.4 conflict state machine — they never register, so they cannot be preempted —
  and *that* is the deleted risk surface, stated precisely.
- **Per-pod NQN upgrades fencing granularity for lane 2** (§5) — granularity, not authn.
- **Scoped tokens in coordinates** (§5): short-lived, node-bound, exchanged for a session at
  the MDS; the world-visible VolumeAttachment is treated as hostile storage.

## 13. Verification

- **FlintExtents already covers the coupling.** The lease→fence→release chain is
  action-structural in the module block doc §9 specs: ClientCrash leaves grants that only
  lease-expiry+fence clears; Free is guarded by RecallBeforeReuse plus quarantine;
  `staleWrite` fires on `client ∈ fenced` — the fenced write is the crown-jewel hazard, and
  a release-before-fence counterexample is already `FlintExtentsLostFence` /
  `FlintExtentsReuseUnderGrant`'s counterexample.
- **The lock-release-after-fence position: subsumed on the device path, NOT on the MDS
  lane.** Device-path half: by the corpus's own doctrine, an arm needs a run that fails
  *only* without it; a `LockReleaseAfterFence` arm's counterexample is LostFence's
  counterexample — dead weight by the formal/README.md standard ("a mutation that cannot
  lose proves nothing"), because a device-path write travels via an extent grant the
  existing gen/fenced machinery sees. But the corpus's own scope condition — "no fast path
  that bypasses extent grants" — is **already violated by the fallback lane**: sub-blksize
  tails and manifest-sized writes route via the MDS by design (§4, block doc §8), carry no
  extent grant, and FlintExtents has no lock state — two successive lock holders writing
  tails via the MDS lane is a corruption the model cannot represent (`staleWrite` fires on
  grant/fence state, and neither holds). Nor is the MDS lane structurally safe in code: the
  WRITE validator still admits the anonymous stateid (`ioops.rs:1212-1227`), so an expired
  ex-holder's tail write is not guaranteed to bounce on BAD_STATEID. So the interaction
  becomes model state after all: a **`LockHolderFallbackWrite` arm with its own
  must-violate run** — one that LostFence's counterexample never exercises, satisfying the
  no-dead-arm doctrine — plus a conformance-suite must-fail (expired client replays a tail
  write via the MDS lane; it must be rejected, which forces the anonymous-stateid escape
  closed for block-class writes).
- **NO new TLA module.** The library and the snapshotter are clients; the corpus models
  server-side safety. But FlintTruncate's scope-limit ("whether a conforming client would
  issue the offending read is a client-behaviour question the model does not settle") existed
  because the client was the kernel's. **libflint is our client** — so the client-behaviour
  half becomes a test suite: RFC 9561 §2.2.4 obligations (on 0x83: commit, return, unregister;
  release locks at lease loss) get a conformance suite, and **the fencing rig re-runs with a
  libflint leg before block-class layouts are granted to the userspace path.** Sequencing
  honesty, because the naive version is circular: §14 puts all libflint work after block
  phases 1-3, so the phase-2 rig can only ever have kernel-client evidence — a single
  `FenceReaches` boolean cannot express "proven for the kernel, unproven for the library."
  The constant **splits**: `FenceReachesKernel` flips on phase-2 evidence and releases
  kernel-class quarantine (the block doc's release condition amended to require only the
  classes actually holding layouts); `FenceReachesLib` stays FALSE — enforced by the MDS
  refusing block-class LAYOUTGET to libflint sessions at grant time, not by convention —
  until the rig re-run with the libflint leg, a named tranche-A exit criterion (§14), not an
  unscheduled hope. Quarantine is not weakened anywhere in this.
- **The conformance kit (§8) is the drills analog for adapters** — the executable form of the
  §3/§5 contracts, run by us in CI and by community adapters for self-certification. The §5
  hybrid contract is a kit line, not prose: library write + RENAME must become mount-visible
  within the directory attribute timeout — a test that **must fail today** against the
  synthetic change_info, and whose flip to green is what turns hybrid from spec'd to offered.

## 14. Phasing + sizing

Strictly after block-layout phases 1-3 (allocator + wire, fencing + stock-kernel validation,
chart class + roll-safety); phase 4 of that doc — the registry StorageDriver — is this doc's
anchor adapter. Each phase ships standalone value; none is gated on the next.

| Tranche | Contents | Sizing |
|---|---|---|
| A | libflint core (session engine, extent cache, flint-spdk-sys, C ABI) + registry driver + **the hot-blob LRU** + **the fencing-rig re-run with the libflint leg** (flips `FenceReachesLib`, §13 — a tranche exit criterion, and block-class layouts stay refused to libflint sessions until it passes) | registry driver 4-8 weeks on top of core; the cache is inside that line, not after it |
| B | fsspec/pyo3 + S3 sidecar gateway | each a small fraction of core — adapters reuse one engine |
| C | Hadoop FS + Go io/fs; conformance kit hardening | Java binding is the long pole |
| D | FUSE mount-pod tier | separate deliverable, bounded by §7 |
| E | Snapshotter | own tranche; known shapes throughout (proxy plugin, EROFS packing, digest resolution, lane detection) — the one design choice that *was* research-shaped, the CAS→namespace assembly, is pinned in §10 (assembly copy, not clone), which is precisely why the tranche goes last: it spends proven pieces |

The session engine is the work; the XDR is the easy half. No viable OSS userspace v4.1
session client exists to borrow (libnfs's shipped client is 4.0-style — SETCLIENTID, no
sessions, no slots, no pNFS).

## 15. Risks and open questions

- **SPDK embedding residuals**: idle-polling CPU cost per qpair at fleet scale (unmeasured);
  poll-group sharing across bindings' thread models (Go LockOSThread + cgo transition cost
  ~100 ns — drain completions into a ring, never per-completion callbacks); ABI churn managed
  by static-link + symbol hiding but never *tested* across an SPDK upgrade yet.
- **The MDS-side policy question the survey surfaced**: should the MDS refuse block-class
  LAYOUTGET to a session with no bound backchannel? Today it grants an unrecallable layout
  **silently** — `handle_layoutget` never consults backchannel state; the only WARNs fire
  later, at recall time (`callback.rs:179`) or at CREATE_SESSION for an undersized *bound*
  backchannel (`session.rs:634`). Nothing flags the hazard until a recall is actually
  attempted, which strengthens the case. Leaning refuse-for-block-class; decide in phase 1.
- **The once-per-cluster store is a once-per-cluster loss** (§1's headline, read as a
  threat) — **AMENDED 2026-09-01: the premise is stale; block doc §12 replication has
  SHIPPED.** `pnfs.chert.us/replicas: 2` (2 is the ceiling — the composition machine
  reasons about ONE peer) places a mirrored raid1 leg on a second target, behind the
  witness-arbitrated seat, the degrade barrier and the sparse-aware rebuild; the chart
  needs per-shard `blockExport.shards` entries plus the composition witness, and refuses
  a replicas: 2 volume loudly without them. Mirror, placement, witness and promotion
  arbitration are proven on real hardware (runbo/runbq), so a storage-node loss no
  longer costs the bytes — but state it the way the campaign record does:
  **replicas: 2 is durability, not serving-through-failover.** The composer's MDS shard
  is node-pinned with the volume's geometry and extent-allocator rows on its own RWO
  PVC, so after a composer death the survivor holds in-sync bytes it cannot yet serve;
  control-plane failover is a named future tranche, and the chart still calls the
  surface experimental. Consequences for this design: put the CAS and lane-3 namespaces
  on replicas: 2 — WORM image blobs are the friendly case for the machine (no churn to
  rebuild, and lane-3's read-only EROFS consumers cannot hit the ext4 emergency-ro that
  forces filesystem pods to restart after a failover window) — and the two mitigations
  stand with sharpened jobs: the external registry of record stays a re-fetch lane for
  AVAILABILITY now, not durability (new attaches of a dead composer's volumes stall
  until recovery), and lane-3/registry GA gates on the control-plane-failover tranche
  or an explicitly accepted stall-until-recovery posture in the runbook — no longer on
  replication existing at all.
- **EROFS ingest maturity**: tarerofs was marked experimental at erofs-utils 1.7; containerd's
  differ is production evidence, but our push-side conversion needs its own fuzz + diffID
  round-trip verification (the original-tar sha256 remains the verification key), plus a
  floor-compatibility check that the emitted on-disk features stay inside the §9 pinned
  5.4-readable profile.
- **Model media-type conventions still settling**: ModelPack is the strongest standard but
  KServe modelcars are indistinguishable plain images — lane detection will need a config
  escape hatch indefinitely.
- **S3 sidecar semantic gaps**: multipart ETag semantics, no versioning/object-lock
  (documented, Velero works without), list consistency during rename — the gateway ships a
  published deviation list, not a compatibility claim.
- **GDS is forward-only**: over NVMe/TCP payloads land in host memory; "GDS-shaped" means the
  4 KiB-aligned registered-buffer discipline (which the ≤4 KiB wire blksize already imposes)
  plus one cudaMemcpyAsync. True zero-copy to GPU arrives only with the RDMA transport
  (Azure HBv3 rig) — **one paragraph of direction, zero promises.**
- **Multi-arch**: per-platform ChainIDs and lane detection are handled (§10), but multi-arch
  EROFS conversion at push doubles ingest work per image; unmeasured.
- **containerd version gates**: there is no 2.0.x lazy-pull hole — 2.0's CRI never adopted
  the transfer service; that default flipped in 2.1, the same release that added the exports
  pass-through and the `use_local_image_pull` escape hatch (§10). The honest support
  statement is 1.7.x **and 2.0.x** via classic annotations, 2.1+ via the §10 config.
- **Write-side record is thin** (§11): every headline ratio in this doc that involves writes
  is a hypothesis until the A/Bs land as ADRs.

---

Cross-references: `docs/plans/pnfs-block-layout-design.md` — §4b (libflint's identity), §5
(fencing/add_host/ControllerPublish hook), §6 (trust, RWX honesty), §7 (SC/chart landmines),
§8 (allocator, quarantine, fallback lane), §9 (FlintExtents cfg matrix), §10 (RDMA), §11
(phasing), §12 (risks; CoW/CLONE refusal). `docs/oci-registry-pnfs-architecture.md` — §1 (the
client-identity assumption this doc inverts), §4 (CAS layout the driver and snapshotter
share), §5 (decompression honesty check). `docs/pnfs-operator-runbook.md` — "Trust model
(production checklist)", "Rename-committer apps on pNFS", "Known residuals".
`formal/README.md` — no-dead-arm doctrine, FlintTruncate scope-limit lesson (the model/test
split §13 leans on). `docs/plans/pnfs-spark-flight-benchmark.md` — committer-wall findings
the Hadoop adapter answers; `docs/plans/pnfs-csi-rwx-and-committer-fixes.md` — the fixes that
obsolete its Blocker #1/Finding 4. `docs/plans/mds-performance-plan.md` — "Tier 1 measured
results" (the mdsbench budget adapter metadata churn is judged against).
`docs/plans/mds-sharding-plan.md` — shard-local allocator constraint. `docs/decisions/` —
ADR 0006+ is where every §11 ratio lands, pass criteria pre-declared.
