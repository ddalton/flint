# Strict mode — design and implementation plan (v2, post-review)

**Phase A: the group-committed S3 journal. Phase B: the rebuildable hub.**

Status: REVIEWED. v1 of this plan went through a 12-agent adversarial review
(6 dimensions, each verified by a dedicated skeptic) on 2026-08-24: 23 findings
CONFIRMED (8 critical), 1 refuted. Every confirmed finding is folded into this
v2 — the review record is §7. The headline lesson: the components compose, but
the *interactions* (watermark × flush skips, fence × GC, replay × checkpoint
staleness, hydrate × replayed tails) were wrong in v1 in ways that lost acked
data. The Phase 0 formal model is therefore non-negotiable and its scope is
wider than v1 stated.

Companion: `docs/flint-strict-architecture.pdf` (62dbbd6) — **now stale in
specific claims** (F29 row, "listener-up in tens of seconds", "every strict ack
round-trips the fencing arbiter"); owes a revision pass once this v2 settles.

Relationship to other plans:

- `docs/plans/nfs-server-hardening-plan.md` — B3 (incarnation verifier) is a
  prerequisite of Phase B. B1/B2 reclaim correctness matters for PVC-mode
  restarts; in cache mode the state DB does not survive, so Phase B stands on
  **forced-lost degraded grace** (§3.2 step 5), not on reclaim-table replay.
- `docs/plans/flint-lean-plan.md` — the journal is `tier::` functionality; the
  lean gateway is its natural group-commit point later.
- `docs/plans/s3-tier-l2-design-review.md` — tier design of record; not
  reopened here.

Out of scope: full-pNFS/DS (A1); warm standby (v2 sketch only); multi-writer
strict.

---

## 0. What strict mode is

A per-path opt-in that upgrades the ack contract: on a strict path, a positive
reply to any operation that claims durability means the bytes are durable **in
S3**, not just on the PVC. The mechanism is a group-committed journal of small
guarded objects under `.flint/journal/`. The checkpoint (today's flush +
manifest barrier) gains per-file journal accounting and a GC scheme. Phase B
composes DR import with journal replay so the hub can be rebuilt from the
bucket alone.

Non-strict paths keep today's contract and data path. (Note the honest limit
found in review: during an S3 brownout, strict parking must not starve the
connection pipeline shared with non-strict traffic — that is invariant I9,
not an automatic property.)

---

## 1. Invariants (the contract — model ALL of these)

- **I1 (ack ⇒ journaled, full inventory).** On a strict path, the server
  replies success to an operation that makes a durability claim only after
  (a) the local fsync that is today's bar, and (b) journal records covering
  every byte/effect the operation made durable-claimable are durable in S3.
  The inventory is every such op, not just WRITE/COMMIT:
  - `WRITE` with `stable != UNSTABLE4` — parks; its records are its own range.
  - `COMMIT` — parks; its records come from the strict lane's per-file
    dirty-interval tracker (§2.1), because Linux sends COMMIT(0,0) and the
    server's COMMIT is range-blind (`ioops.rs:2103` whole-file sync_all).
    **COMMIT is never refused for size** — the tracker spills to as many
    records/batches as needed.
  - `COPY` / `CLONE` on a strict destination — **refused with
    NFS4ERR_NOTSUPP in v1** (errno-enforced, death-list doctrine). The
    client's generic fallback (read+write loop) routes the bytes through the
    journaled WRITE lane. Rationale: the server's COPY fsyncs and replies
    `wr_committed = FILE_SYNC4` with a live verifier (`perfops.rs:896`,
    `compound.rs:2618` — which is COPY's encoder, not WRITE's), and
    FILE_SYNC data is exempt from verifier-driven resend, so an unjournaled
    COPY is an unrecoverable acked-loss hole. A by-reference Copy record is a
    v2 lever.
  - `ALLOCATE` / `DEALLOCATE` — journal a `Zero { path, offset, len }`
    record (cheap; a hole punch is a content mutation a rebuild would
    otherwise resurrect).
  - `SETATTR` truncate — parks; `Truncate` record.
- **I2 (exactly-once keys, total order, contiguity).** Journal keys live in
  ONE shared sequence namespace `.flint/journal/<seq20>` (zero-padded),
  written with `If-None-Match: *`; epoch and holder id ride in the batch
  header. **A writer is single-flight and never abandons a seq**: it retries
  the SAME key until success or self-fence. (A DELAYed op's effects staying
  in a later-landing batch is safe: records are absolute and later keys win.)
  The namespace is therefore dense — replay treats any gap as an error (I6).
- **I3 (fence handshake, three arms, heartbeat-composed).** A successor that
  has deposed the epoch discovers the tail by LIST, replays, then claims
  `tail+1` with a FENCE batch. **After its FENCE lands at f, it LISTs > f;
  anything found means a pipelining predecessor — replay it and re-fence at
  the new tail, looping until the post-FENCE LIST is empty.** On any journal
  PUT 412, the writer GETs the colliding key and switches on the epoch in its
  header:
  - **higher than mine** ⇒ fence yourself permanently (`fence_publishing` +
    fail strict acks);
  - **equal to mine** ⇒ it is my own earlier PUT whose response was lost
    (epochs are single-holder by CAS construction) — verify holder_id and
    header CRC match, treat the seq as written, advance;
  - **lower than mine** ⇒ replay it and retry at the next seq.
  Honesty note (review): under sustained load the incumbent can win every
  seq race, so **termination is bounded by the epoch heartbeat, not by the
  handshake alone** — the model must compose both (§4), and the strict lane
  tightens the bound via I5a.
- **I4 (per-file watermark — never a global tail).** Each published file's
  manifest entry (GenRecord) carries `contained_seq`: the highest journal seq
  whose effects on THAT file are contained in its checkpointed generation.
  `journal_watermark` = min of `contained_seq` over all strict files with
  outstanding journal effects (a never-yet-published strict file contributes
  its first record's seq − 1). Rationale: the existing flusher skips hot
  files every tick (60 s floor + 10 s quiescence guard, `flush.rs:918-931`)
  and publishes the barrier anyway — a frozen global tail therefore *lies*
  for exactly the hot-small-state workload strict mode targets, and GC would
  delete the only durable copy of acked writes. To keep the watermark
  advancing, **strict files bypass the quiescence guard but keep the 60 s
  floor**: a continuously-hot strict file checkpoints at floor cadence, so
  GC lag is bounded in minutes, not unbounded.
- **I5 (GC floors).** Journal keys may be deleted only when ALL hold:
  seq ≤ the watermark of the previous manifest (lag-one); the key is older
  than the configured max-rebuild horizon (default 1 h) — bounded grace for
  bucket-alone passive readers; the key is at least one full epoch lease
  behind the live tail; and the GC'er has, this round, re-read the epoch
  object from the store and found itself still the holder (one GET —
  "observed" means store-observed, not local-guard-observed). Versioned
  buckets: bootstrap creates a lifecycle rule scoped to
  `<prefix>.flint/journal/` (Expiration + NoncurrentVersionExpiration +
  ExpiredObjectDeleteMarker) — **the v1 "existing tag-filtered reaper" does
  not exist**; journal objects are exempt from A9's recovery-window
  semantics for data objects.
- **I5a (ack validity is lease-bounded).** A strict ack is valid only if,
  between the batch PUT completing and the parked replies being woken, the
  epoch guard is un-fenced AND the last successful heartbeat CAS is within
  lease/2. **The strict lane pauses acks on the FIRST failed renew** (strict
  is the lane contractually allowed to DELAY), collapsing the
  deposed-but-still-acking window to one heartbeat. Together with I5's lease
  floor this closes the paused-predecessor hole: a resumed predecessor whose
  PUT lands in a GC-freed key cannot ack, because its heartbeat is stale.
- **I6 (recovery order + replay guards).** Load checkpoint (import) → replay
  → protocol grace → listener; replay is pre-listener. Replay starts at
  `max(journal_watermark, applied_seq) + 1`, where `applied_seq` is a small
  fsynced marker file in the working set updated after each batch PUT and
  before ack — so a same-hub restart whose PVC is already at the tail
  replays nothing (v1's "idempotent replay" was false for Rename over a
  recreated source). **Replay verifies contiguity**: the first live key must
  be exactly its start seq and every key must increment by one; on any gap
  or 404 it re-GETs the latest manifest and restarts from the (higher)
  watermark, looping until a contiguous pass — this is what protects a slow
  or bucket-alone rebuild against a live incumbent's GC. A record applies to
  a file only if that file's manifest `contained_seq` < the record's seq
  (Rename requires it for both `from` and `to`); this guards replay over a
  checkpoint that is ahead of the watermark, since whole-file objects are
  read at flush time and legitimately embed post-watermark effects.
- **I7 (namespace ops journal too, with binding rules).** On strict
  subtrees, REMOVE/RENAME/CREATE/SETATTR emit journal records. **A record's
  path is bound at apply time, in the op handler, under the same op that
  performs the mutation** — never at window close (v4 filehandles are
  rename-stable, so a window-close resolution races logrotate-style renames
  and replays acked writes into the wrong file). **A RENAME crossing a
  strict-subtree boundary in either direction is refused with NFS4ERR_XDEV**
  (mv falls back to copy+unlink, which journals the content through the
  WRITE lane) — otherwise a rename-into-strict from an unflushed non-strict
  source is acked but unreconstructible at rebuild.
- **I8 (sessions and locks are NOT journaled).** Client state rebuilds
  through protocol grace. In cache mode this is the **forced-lost** path:
  every cache-mode rebuild passes `state_lost = true` (a fresh volume's
  innocently-empty state DB otherwise reports `restored_clean`, grace never
  arms, and a rival lock is granted over a pre-crash holder — §3.2).
- **I9 (bounded admission).** The strict lane has a byte budget (default
  128 MiB parked + queued) and a per-connection semaphore (default 16, well
  under the 64 pipeline permits — `pipeline.rs:135`); at either bound, NEW
  strict ops are refused with NFS4ERR_DELAY **at admission, before parking
  and before read-back** (the A4 gate refusal shape). Read-back is deferred
  to PUT time: a stalled lane holds (fd, offset, len) descriptors, never
  payloads. This is what keeps an S3 brownout from stalling the whole
  connection (permit exhaustion = dead air for non-strict traffic sharing
  the mount) or OOMing the hub.
- **I10 (batch construction).** Records within a batch are ordered by apply
  order. Write-range coalescing never crosses a Truncate/Rename/Remove
  record for the same file — the namespace record closes that file's
  coalescing window. Read-back runs at PUT time via the retained fd, and is
  serialized per file against writers (a strict-lane per-file write lock for
  the duration of that file's pread) — an unserialized pread against a
  concurrent pwrite can return a torn mix on Linux.

---

## 2. Phase A — the strict journal

### 2.1 Data path

```
strict op acks locally (pwrite+fsync — unchanged, ioops.rs)
  → admission check (I9): lane saturated ⇒ NFS4ERR_DELAY now
  → park the reply; enqueue descriptors (fd, path-bound-at-apply, offset, len)
      WRITE(stable): its own range        COMMIT: drain the file's
      dirty-interval tracker (see below)
  → group-commit window closes (5–25 ms adaptive — widen toward 25 ms when
      batches run near-empty, so the sustained PUT rate is a design ceiling,
      not an accident; or close early at the 8 MiB batch threshold)
  → at PUT time: per-file write-lock, read back descriptor ranges via fd,
      build apply-ordered records (I10)
  → ONE PUT: .flint/journal/<seq>, If-None-Match:*, crc64-nvme in header
      (single-flight; on 412 apply the three-arm rule, I3)
  → fsync the applied_seq marker (I6)
  → validity check (I5a): guard un-fenced, heartbeat fresh ⇒ wake replies
```

**The dirty-interval tracker (new component, missed by v1).** COMMIT carries
no usable range, so the strict lane records per-file dirty intervals for
UNSTABLE writes at the WRITE apply site (next to the existing
`capture::note_at` call, `ioops.rs:1987`), drained and reset by COMMIT's
batch. It has its own overflow policy: bounded intervals that **spill to
multiple Write records across multiple batches** — never a refusal (the
client cannot un-send acked UNSTABLE data; a refused COMMIT is a permanent
fsync failure) and never a borrow of `capture`'s epoch (whose
MAX_INTERVALS=256 → Whole collapse would turn a 4 KiB commit into a
whole-file record).

**Sizes.** Records are chunked at 8 MiB; an op needing more spills across
records and batches, and its ack waits for all of them. Nothing is refused
for size (v1's 8 MiB batch cap vs 32 MiB record cap was contradictory and
livelocked 8–32 MiB writes); the cost model documents the amplification and
the policy guidance stays "target hot small state". Head-of-line honesty: a
large in-flight record delays co-parked small commits — the latency exit
criterion (§2.6) is stated for the small-record workload, with the
large-record case measured and published separately.

**Failure ladder.** PUT retry with backoff inside a deadline (default 8 s) →
parked ops return NFS4ERR_DELAY. First failed epoch renew ⇒ lane pauses acks
(I5a). S3 outage = strict paths loudly slow and the lane refusing at
admission (I9) — non-strict traffic keeps flowing; that property has its own
drill oracle now (SJ4).

### 2.2 Batch format

Header: `magic, version, epoch, seq, holder_id (claim identity), record
count, crc64-nvme`. Records (§1 I1/I7 vocabulary): `Write`, `Zero`,
`Truncate`, `Remove`, `Rename`, `Create`, `Stamp` — apply-ordered, paths
bound at apply time, absolute offsets. Binary framing for the record section
(base64-in-JSON costs ~33%); JSON header is fine.

### 2.3 Policy surface

`FLINT_STRICT_PATHS` — comma-separated globs against export-relative paths,
fed by `share.spec.strictPaths`. Empty = off (default). Evaluated per file at
open/first-write, cached. Boundary semantics: COPY/CLONE → NOTSUPP,
cross-boundary RENAME → XDEV (I1/I7), both errno-enforced with tests.

### 2.4 Watermark + GC

- `GenRecord`/manifest entries gain `contained_seq` (I4), stamped at each
  successful publish from the journal tail observed at that file's
  epoch-take. `WriterState` computes `journal_watermark` = min (I4).
- Strict files bypass the quiescence guard, keep the 60 s floor (I4) — the
  watermark advances at floor cadence; a hot strict file costs one
  whole-file (or interval-part) upload per floor period.
- GC task after a successful barrier applies ALL I5 floors (lag-one +
  max-rebuild horizon + lease floor + store-side epoch re-read).
- Bootstrap creates the journal-scoped lifecycle rule (I5) beside the
  existing MPU-abort rule (`s3.rs:763`); GC's own deletes are plain
  DeleteObject — lifecycle reaps the noncurrent versions and delete markers.

### 2.5 Code seams (corrected against HEAD — v1's were wrong)

| Piece | Where | Est. lines |
| --- | --- | --- |
| `tier/journal.rs` — writer, group commit, batch, three-arm fence, replay (contiguity + per-file guards) | new module | ~2,000 |
| Dirty-interval tracker (strict lane, per-file, spill policy) | new, beside `tier/capture.rs` | ~400 |
| Parking hooks: WRITE/COMMIT in `nfs/v4/operations/ioops.rs` (the real ack sites — `compound.rs:2618` is COPY's encoder); COPY/CLONE NOTSUPP + ALLOCATE/DEALLOCATE Zero records in `perfops.rs`; I7 records at the `fileops.rs` call sites (NOT `metadata_sync.rs`, which is parent-dir fsync only) | listed files | ~700 |
| Admission bounds (I9: semaphore + byte budget, A4-shaped refusal) | ioops.rs + journal.rs | ~150 |
| Policy (globs, env + CRD plumb, boundary errnos) | journal.rs + operator/chart | ~200 |
| Watermark (`contained_seq`) + GC floors + lifecycle rule | `tier/manifest.rs`, `tier/flush.rs`, `tier/store/s3.rs` bootstrap | ~600 |

Phase A total ≈ 4,000 lines + drills: **~6–8 engineer-weeks** (v1 said 4–6;
the tracker, the admission bounds, the corrected seams, and the GC/lifecycle
work are the growth).

### 2.6 Phase A exit criteria

- Formal model green (§4) before the fence handshake is coded.
- SJ1–SJ4 green **with their failing controls** (§5).
- Full suite battery unchanged on non-strict paths (pynfs 171/0/91 floor,
  pynfs-4.2 4/4, nfstest posix 459/2, lock 5296/0 — private port 20495).
- Latency: small-record strict ack p50 ≤ ~30 ms against real S3, p99
  published; large-record head-of-line measured separately.
- Cost model (restated): worst case is set by the window adaptation ceiling
  — at the 25 ms window that is ≤ 40 PUT/s ≈ $520/mo request cost; the
  5 ms floor exists for latency under light load, and adaptation widens
  under sustained load so the ceiling holds. Add: versioned-storage line
  (bounded by the lifecycle rule's expiry days), and SSE-KMS without Bucket
  Keys adds ~$0.03/1k PUTs — bootstrap hard-warns on strict shares.
- Memory: sustained stable-write load during S3 SlowDown plateaus hub RSS
  (SJ4 sub-leg).

---

## 3. Phase B — the rebuildable hub

### 3.1 What changes

The PVC demotes to cache. Chart gains `workingSet: pvc | cache` (default
stays `pvc`). Cache mode requires a byte budget: **`SpaceConfig` grows an
explicit capacity override, chart-fed from the ephemeral volume's sizeLimit**
— today the space machinery is statvfs of the export root
(`space.rs:349`), which on an ephemeral volume measures the NODE, so no
admission/watermark/NOSPC guard would ever fire before kubelet evicts the
pod mid-serve (and F55's postgres guard would be dead). Warm fill after a
cache-mode rebuild defaults off (demand paging into a bounded cache, not a
bulk fill).

### 3.2 Rebuild ladder (corrected)

1. Claim identity check (import's adopt arm + the lean plan's P1 claim).
2. Depose epoch (existing CAS takeover; MPU-abort sweep unchanged).
3. Import checkpoint — lazy for non-strict content (demand hydration;
   cold-read fan-out + warm fill shipped). **Strict files that have journal
   tail records are FULLY hydrated pre-listener, then replayed into, then
   marked dirty.** v1's "tails come from the journal, not hydration" cannot
   compose with the shipped hydrate contract: a lazily-imported file is an
   evicted stub pinned to the checkpoint's etag, and `restore_once` truncates
   any non-empty stub and restores checkpoint bytes CRC-pinned
   (`hydrate.rs:806-1013`) — replayed tails written into a stub would be
   destroyed by the first client READ. (An overlay/range-merge hydrate
   contract is the v2 optimization; it is real new `hydrate.rs` surface with
   its own estimate, not a footnote.)
4. Journal replay > `max(watermark, applied_seq)` with contiguity + per-file
   guards (I6). FENCE batch claims the tail; post-FENCE LIST loop (I3).
5. Protocol grace, **forced-lost**: cache mode passes `state_lost = true`
   into the state bring-up regardless of how innocent the fresh DB looks —
   otherwise `anything_reclaimable` computes false, `restored_clean()` stays
   true, the new-lock refusal branch (`lockops.rs:1047`) never arms, and a
   rival client is granted a lock over a pre-crash holder's range during
   grace. B3's verifier makes un-COMMITted UNSTABLE data resend; reclaims
   proceed by client resend, not by reclaim-table restore (the tables died
   with the volume).
6. Listener up. Same ClusterIP ⇒ clients never remount.

**Timing honesty (re-costed):** listener-up = metadata import + journal tail
discovery + **sum of strict-file sizes at hydrate fan-out rate** + replay.
For the intended strict policy (hot small state: MiBs, not GiBs) this stays
tens of seconds; it degrades with large strict files, and the drill leg
publishes measured numbers instead of the v1 blanket claim.

### 3.3 What it deletes from the failure track — corrected

- F10 force-detach (~6 min): row deleted in cache mode — no PVC attach.
- F8/F13: recovery no longer trusts the disk that just failed.
- Epoch split-brain residual: closed for strict paths by I5a + I3 (and
  honestly bounded by the heartbeat, not "every ack round-trips the
  arbiter").
- **F29 stays.** It is a CONSUMER-node CSI staging wedge; the hub's working
  set mode does not touch it. (v1 claimed the class disappears — wrong.)
- Not touched: the NFS protocol surface (B-track), the CL load track,
  non-strict RPO.

### 3.4 Work breakdown

| Piece | Est. |
| --- | --- |
| Cache-mode bootstrap: import + strict-file pre-hydration + replay + forced-lost grace, no-PVC startup | ~1,200 lines |
| `SpaceConfig` byte-budget override + warm-fill bounding | ~200 |
| Fence rework (FENCE batch + post-FENCE LIST loop; preStop keeps the 17 s path) | ~200 |
| Operator: failover choreography, `workingSet` CRD/chart, events | ~400 |
| Drills SJ5–SJ8 + battery | — |

**~5–6 engineer-weeks** after Phase A (v1 said 4). Warm standby stays a
later ~2 weeks.

---

## 4. Phase 0 — the formal model gate (~1.5–2 weeks, before Phase A code)

New module `FlintStrictJournal` beside the existing twelve. **The model
composes the CAS heartbeat with the journal handshake** — v1 scoped the
handshake alone, and the review showed termination is a property of the
composition. State space must include: seq claims with holes (abandoned
PUTs), error-after-commit (a PUT that lands but whose response is lost —
the equal-epoch arm), GC with all I5 floors, a paused-then-resumed
predecessor, replay guards over an ahead-of-watermark checkpoint,
path-aliasing rename interleavings (write-then-rename and rename-then-write
in one window), and a file that is dirty at EVERY flush tick (the quiesce
case that falsified v1's watermark).

Properties:

- **No acked-lost** under crash at any step, including the paused
  predecessor and the always-hot file.
- **Fence termination** in heartbeat-bounded time for the composition.
- **GC safety**: no permitted rebuild (deposing OR bucket-alone passive)
  reads a journal with an undetected gap.
- **Watermark honesty** per-file: checkpoint entry ⊇ effects ≤ that file's
  `contained_seq`, under skips and failed flushes.
- **Replay soundness**: replay with I6 guards over any reachable
  (checkpoint, journal, local-state) triple reproduces the acked history.

---

## 5. Drill legs (anti-vacuity mandatory; oracles upgraded per review)

- **SJ1 crash-mid-window:** kill -9 during the window under a client-side
  acked-op log; oracle is CONTENT EQUALITY, not presence — including the
  torn-read interleaving (stable WRITE racing an UNSTABLE write to the same
  range) and the write/truncate/write sandwich in one window. Control:
  strict off must show acked loss.
- **SJ2 two-writer fence race:** successor vs incumbent under strict load;
  verify total seq order, zero acked-lost, incumbent stops acking within
  the HEARTBEAT-derived bound (stated explicitly, not "the handshake
  deadline"); store proxy swallows one PUT response to force the
  equal-epoch arm. Control: disable the three-arm rule in a test build.
- **SJ3 GC vs rebuild:** both arms — a deposing rebuild AND a bucket-alone
  passive reader replaying while the live incumbent barriers + GCs; the
  contiguity check must catch the deletion and restart from the newer
  manifest. Control: replay with the gap check disabled must silently lose
  the tail.
- **SJ4 S3 outage/SlowDown:** iptables DROP mid-traffic. Oracles: nothing
  acked without a journal PUT (proxy log); **non-strict ops on the same
  connection complete at normal latency during the DROP** (control: remove
  the I9 lane bound — they must then stall); hub RSS plateaus.
- **SJ5 (B) PVC destroyed:** rebuild; sqlite and git workloads resume
  through grace. Arms added: a rival client attempting a CONFLICTING lock
  during the rebuilt hub's grace is REFUSED (forced-lost arm); a lazily
  imported strict file is READ before it is written (hydrate-vs-tail arm).
  Control: pre-B1/B2 binary must fail reclaim.
- **SJ6 (B) rebuild-under-load timing:** publish listener-up and first-read
  latencies, including the strict-file pre-hydration term.
- **SJ7 (B) resurrection:** REMOVE on a strict path, crash before barrier,
  rebuild — file stays dead. Arm added: rename-into-strict refused (XDEV)
  and the mv fallback journaled.
- **SJ8 rename-aliasing:** logrotate-style RENAME against a strict writer
  mid-window; replay must place the acked bytes in the file the client
  observed. Control: bind paths at window close in a test build — must fail.

Rig: dedicated `flint-drill` qemu VM; suites on private port 20495.

---

## 6. Open questions for the user

1. Policy surface: CRD globs only for v1? (Plan assumes yes.)
2. DELAY posture during S3 outage: park behind DELAY + I9 admission refusal
   (proposed) vs a config to degrade to non-strict with a loud event.
3. GC max-rebuild horizon default (proposed 1 h) and the lifecycle rule's
   noncurrent-expiry days (proposed 7).
4. Phase B chart default stays `pvc` — confirm cache mode ships opt-in.

(v1's Q2 — over-cap refusal — is resolved by design: chunked records, no
size refusal.)

---

## 7. Review record (2026-08-24, 12 agents, 6 dimensions × verify)

23 CONFIRMED (8 critical), 1 REFUTED. One line each; all folded above.

Critical: GC-freed keys defeat If-None-Match fencing (→ I5 floors + I5a);
Rename replay over recreated source corrupts (→ applied_seq + per-file
guards, I6); global watermark lies for flush-skipped hot files — found
independently by three dimensions (→ per-file contained_seq, I4); seq-hole
lets a successor fence below acked batches (→ contiguity + post-FENCE LIST,
I2/I3); COMMIT has no range source (→ dirty-interval tracker, §2.1); COPY/
CLONE/DEALLOCATE ack durability outside the journal (→ I1 inventory);
rebuild hydrate destroys replayed tails (→ pre-hydrate strict files, §3.2);
cache-mode boots innocently-empty state.db, grace collapses (→ forced-lost,
I8/§3.2).

Major: unserialized read-back can journal torn bytes (→ I10); v1 seam table
named the wrong code sites (→ §2.5); no equal-epoch arm in the 412 rule (→
I3); fence termination is heartbeat-bounded + GC TOCTOU (→ I3 note, I5, I5a);
path-keyed records vs rename aliasing (→ I7 binding, SJ8); parked replies
exhaust the 64-permit pipeline (→ I9); rename-into-strict unreconstructible
(→ XDEV, I7); lag-one GC insufficient for slow/passive rebuilds (→ horizon
floor + replay contiguity, I5/I6); batch/record cap contradiction (→ chunked
records, §2.1); no parked-bytes bound → OOM (→ I9); tag-filtered reaper does
not exist + cost model omissions (→ lifecycle rule, §2.4/§2.6); cache-mode
statvfs measures the node + F29 claim wrong (→ §3.1/§3.3). Minor: versioned
GC debris (→ lifecycle rule).

Refuted (correctly): "cache mode deletes the RWO interlock so a partitioned
predecessor keeps serving" — the hub exits on depose
(`on_deposed` → `process::exit(70)`), so the RWO-attach interlock was never
the fence.
