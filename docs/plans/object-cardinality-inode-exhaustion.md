# Object cardinality vs. node/PVC inodes — the three front ends

> **Status: findings + fix specification, NO code.** Produced 2026-08-28
> from a code read at `85184494`. **Nothing here has been executed.**
> Every claim below cites the path that carries it; §"What is not
> verified" is the honest boundary of that read. The two fixes in §3.1
> and §3.2 are specified to the line but not written.
>
> The question that produced it: *does the number of objects in the S3
> bucket adversely affect the inode limit on the underlying node?*

## The one-line answer

**Yes for lite (1:1 with bucket cardinality, on the PVC, permanently),
partly for lean (on the NODE, via emptyDir), and no for passthrough.**
But the capacity arithmetic is the boring half. **The half that
matters is that lite's response to running out of inodes is to serve a
silently truncated namespace and then publish it** — §2.

## 1. Where each front end spends inodes

| Front end | Local inodes | Where they land | Tracks bucket cardinality? | Reclaimed? |
| --- | --- | --- | --- | --- |
| **flint-lite** | 1 per object (+1 per dir/symlink) | the hub's **PVC** | **Yes, 1:1** | **Never** |
| **flint-lean** | 1 per file in the workspace | the agent pod's **emptyDir = node ephemeral fs** | Per-prefix, capped at checkout only | On pod delete |
| **flint-passthrough** | ~0 | nowhere (FUSE) | **No** | n/a |

### 1.1 lite — the local tree is the bucket, and eviction never frees an inode

Two facts compose into the whole finding.

**(a) Every object becomes a local inode.** `tier/import.rs` materializes
each object under the prefix as a **0-byte evicted stub** — a real
inode + dirent + `user.flint.tier.evicted` xattr whose durable marker
points at the object (`place_stub`, `import.rs:499`). Both lanes do it:
the manifest lane (DR restore) and the sweep lane (adopting a
pre-existing bucket). `importOnStart: true` is the **chart default**
(`flint-lite-chart/values.yaml:87`), so a fresh hub pointed at an
N-object bucket walks all N before the listener binds.

Steady state reaches the same place by a different road: a client
creates a file, flush publishes one object, so live-namespace size and
bucket cardinality are equal by construction. Import is the fast path
to the limit, not the only one.

**(b) Eviction reclaims bytes and never inodes.** Eviction is
marker-before-truncate, *in place*; hydration restores "in place, into
the marker inode" (`tier/hydrate.rs:1-11`). `grep -c 'remove_file\|unlink'
spdk-csi-driver/src/tier/evict.rs` returns **0**. So the entire
watermark/eviction machinery — the thing that lets a small PVC serve a
large bucket — has **no effect whatsoever on inode pressure**. Inode
consumption is monotonic in the number of distinct paths ever present.

**(c) The space model sees inodes and does not use them.** `tier/space.rs`
reads `f_files`/`f_ffree`/`f_favail` and stores them (`space.rs:368-370`),
and reports them through the `FILES_*` attributes so `df -i` on the
client is truthful. **No admission path consults them.** `admit_create`
(`space.rs:255`) gates on `CREATE_COST = 64 * 1024` (`space.rs:41`) — 64
KiB of *byte* headroom, a proxy for "inode + dirent + first block". With
free bytes and zero free inodes it admits, and the kernel refuses
underneath. The watermark never trips, so eviction never fires; and per
(b), firing would not have helped.

**The collision is with the chart's own sizing advice**
(`flint-lite-chart/values.yaml:44-48`):

> `# with tier.enabled, durably flushed data lives in S3, so size for the`
> `# WORKING SET, not the dataset.`
> `size: 20Gi`

Sizing the PVC for the working set also sizes the **inode table** for
the working set — while the inode demand is the **dataset**. The CSI
driver runs bare `mkfs.ext4 -F` with no `-N`/`-i` (`main.rs:3940`), so
mke2fs defaults apply (16 KiB/inode for volumes in the 512 MiB – 4 TiB
band):

| PVC (ext4, defaults) | Inodes | Objects it holds |
| --- | --- | --- |
| **20 GiB (chart default)** | **1,310,720** | ~1.3M |
| 50 GiB | 3,276,800 | ~3.2M |
| 100 GiB | 6,553,600 | ~6.5M |
| 20 GiB **xfs** (`imaxpct` 25) | ~20M | not a practical limit |

Past the limit, `open(O_CREAT)`/`mkdir` return ENOSPC **while `df` shows
the PVC nearly empty**.

### 1.2 lean — a NODE problem, and the cap is narrower than it reads

The checkout is a full local materialization on an **`emptyDir` with
`medium: None`** (`lean_operator/inject.rs:88`) — the node's ephemeral
filesystem, shared with the kubelet, the container runtime, and every
other pod on that node.

- `maxFiles` defaults to **250,000** (`lean_operator/crd.rs:258`), and
  the webhook does stamp it (`inject.rs:144` → `FLINT_SYNC_MAX_FILES`),
  so the knob is live.
- **But it is checked in exactly one place** — `checkout.rs:92`, against
  the manifest, before the first byte. It bounds what lean **reads in**,
  not what the workspace can **grow to**. An agent that generates
  millions of small files post-checkout is bounded by nothing.
- `sizeLimit` (default 20 GiB, `crd.rs:272`) **caps bytes, not inodes**,
  and zero-byte files consume none of it. kubelet enforces it by
  periodically sizing the volume; there is no inode term.
- There is no per-node accounting anywhere. On a 100 GiB ext4 node fs
  (~6.5M inodes), ~24 full-cap workspaces exhaust it — and kubelet's
  `nodefs.inodesFree<5%` hard-eviction threshold trips first, evicting
  **unrelated** pods.

Cardinality here is per-*subtree-prefix*, not per-bucket: a 50M-object
bucket is fine as long as each workspace's prefix stays under the cap.

The cap was derived without this axis in view.
`docs/plans/flint-lean-0b-measurements.md:117` reads "**Bytes**: no new
constraint below the emptyDir budget" — that analysis measured wall
clock, RSS and manifest size. Inodes are not in it.

### 1.3 passthrough — structurally immune

Mountpoint for S3 is a FUSE client with no local materialization; its
inode table is in the mounter's **RAM**, populated lazily by what is
actually accessed. The two injected volumes are empty `emptyDir`s
(mount target + state) and `mounter_args` never passes `--cache`
(`passthrough/inject.rs:251-296`). Bucket cardinality costs S3 LIST
latency on `readdir` and mounter RSS proportional to the *accessed*
path set — not node inodes.

One theoretical hole, not worth acting on: `spec.mountOptions` is raw
argv passthrough, so `--cache <dir>` would put mountpoint's block cache
(one file per ~1 MiB block, bounded by `--max-cache-size` in *bytes*)
on the container's writable layer — the node's overlayfs. Same shape as
§1.2, opt-in.

## 2. The concerns, ranked — the failure mode, not the limit

### 2.1 Inode exhaustion produces a silent partial namespace, not a stop

`stage_stub`/`place_stub` failures are counted and stepped over —
`rep.failed += 1` then `return`/`continue` (`import.rs:555` manifest
lane, `import.rs:864` sweep lane). The caller **logs the count and gates
nothing** (`pnfs/mds/server.rs:1150-1166`): `imported = true`, the
listener binds, and the hub serves a tree missing every file that ran
out of inodes.

What makes this a defect rather than a nit is the code **directly
above** it. The adjacent `refused` arm — unreadable manifest — calls
`orch.fence_publishing(why)` (`server.rs:1148`), with a comment
spelling out exactly this danger:

> "a transient GET failure on the manifest was enough to publish an
> EMPTY tree over a real one … after which `rpo::evaluate` reports clean
> and the idle ladder reclaims the disk."

`fence_publishing` (`flush.rs:878`) is process-wide and is the first
check in `tick()`, so it is a real stop. **A partial import reaches the
same end state through a door with no guard on it.** Someone already
reasoned carefully about "serve a truncated tree, then publish it back";
inode exhaustion arrives at it from an angle that arm does not cover.

### 2.2 The retry is discarded, and growing the PVC does not recover it

`import.rs:878-882` clears the sweep note on any pass that walked the
whole listing, on an explicitly stated assumption:

> "A sweep that failed objects still completed its pass — those keys are
> individually broken, and re-running would fail them again; what must
> survive is an INTERRUPTION, which never reaches here."

**ENOSPC-on-inodes is neither individually-broken nor deterministic.**
It is a transient, tree-wide resource condition in which *every key
after the table fills* fails. The note clears anyway. On the next start
tier state is no longer fresh (so no import — `state_is_fresh`,
`import.rs:92`) and the note is gone (so no sweep). **Grow the PVC
afterwards and the missing objects stay invisible**: they exist in the
bucket, and nothing will ever look at them again.

That one comment is the whole bug. It names its assumption, and inodes
are the counterexample.

### 2.3 It can propagate into the bucket's DR manifest

The hub's manifest writer rebuilds the whole document from its local
walk and cannot merge (recorded at `lean/sidecar/src/manifest.rs:5-8`,
which exists *because* of that property). A truncated local tree
therefore yields a truncated manifest, and **G13 is still open**
(`docs/plans/nfs-server-hardening-plan.md:265`):

> "anti-shrink barrier at manifest publish (refuse/alarm an entry-count
> collapse without matching tombstones — the dev-drift bug shrank 37→4
> silently)"

Same collapse, new cause.

**Scoped honestly:** there is **no manifest-diff GC** in lite — the only
`store.delete` calls in `flush.rs` are tombstone-driven (`:766`) and
re-key-driven (`:1479`) — so this does **not** immediately delete
objects. The damage is (i) the DR manifest, so a later restore restores
the truncated namespace, and (ii) RPO reporting clean over it. Bad, but
it is not live data deletion and should not be described as such.

## 3. Fixes

### 3.1 F1 — fence publishing on a resource-shaped partial import (primary)

**The cheapest change that converts §2.1 and §2.3 from silent to loud.**
It is not the `files_avail` admission gate (§3.3): the gate stops one
more stub from being created, whereas F1 stops a truncated tree from
being *believed*.

1. Distinguish resource failures from per-key failures. Add to
   `ImportReport` (`import.rs:66-82`):
   ```rust
   /// Placements that failed on a TREE-WIDE resource condition
   /// (ENOSPC/EDQUOT — including inode exhaustion, which reports
   /// ENOSPC with bytes still free). Never a per-key defect: the next
   /// key will fail too, and a later retry may well succeed.
   pub failed_resource: usize,
   ```
   Classify at the two `std::fs` error sites (`import.rs:555` and the
   `stage_stub` returning `None` path feeding `:864`) with
   `e.raw_os_error() == Some(libc::ENOSPC) || == Some(libc::EDQUOT)`.
   **Use `raw_os_error`, not `ErrorKind::StorageFull`** — the crate is
   edition 2021 with no pinned toolchain (`spdk-csi-driver/Cargo.toml:4`,
   no `rust-toolchain.toml`), so do not depend on a recently-stabilized
   `ErrorKind` variant.
2. In `pnfs/mds/server.rs`, inside `if let Some(rep) = outcome.report {`
   (`:1150`), after the existing `info!`:
   ```rust
   if rep.failed_resource > 0 {
       self.status.set_import_refused(format!(
           "{} stub(s) failed on ENOSPC/EDQUOT — the export does not \
            describe the bucket", rep.failed_resource));
       orch.fence_publishing("import incomplete: out of space/inodes");
   }
   ```
   This reuses the mechanism the arm 30 lines above already trusts. The
   `error!` text there is the model for the operator-facing message: say
   what is wrong, what is *not* restored, and what to do (grow the PVC —
   `df -i`, not `df` — then restart).

### 3.2 F2 — keep the sweep note when the failures are resource-shaped

At `import.rs:878-882`, replace the unconditional `clear_note` with:

```rust
// A pass that failed objects still completed — UNLESS the failures
// were tree-wide resource refusals. Those are not "individually
// broken keys": every key after the table filled failed, and a
// retry after the volume grows is exactly what recovers them.
if rep.failed_resource == 0 {
    clear_note(note_path.as_deref());
    rep.completed = true;
}
```

Leave the note and `completed = false`, so the next start re-runs the
sweep. Together with F1 the operator's recovery becomes what they would
expect: grow the PVC, restart, namespace restored, fence lifts.

The same classification should gate `sweep_owed`/`completed` in
`SweepReport`. Note the manifest lane's own intent note is already
handled correctly for interruption — F2 is about the *sweep* note only.

### 3.3 F3 — `admit_create` should gate on `files_avail` (secondary)

`space.rs:255`. The gauge already exists and is already refreshed
(`space.rs:368-370`); it is simply never read. Adding
`files_avail >= 1` (with a small reserve, mirroring `reserve_bytes`)
turns a kernel ENOSPC into the modelled `NFS4ERR_NOSPC` refusal the
module exists to deliver — the F55 posture. Also worth a
`NospcInodeRefusals` counter distinct from `NospcCreateRefusals`, or
the metric cannot tell an operator which resource ran out.

### 3.4 F4 — documentation and sizing (secondary)

`flint-lite-chart/values.yaml:44-48` should carry an inode term
alongside the byte guidance, e.g. *"on ext4 the PVC also needs ~16 KiB
of volume per bucket object for the stub inode; above ~1M objects
prefer a `fsType: xfs` StorageClass, which allocates inodes
dynamically."*

`lean_operator/crd.rs` should say in the `sizeLimit` doc comment that
it bounds **bytes only**, and that node inode headroom is
`maxFiles × pods-per-node` — a number nothing currently computes.

### 3.5 F5 — G13 (already tracked, not new)

The anti-shrink barrier at manifest publish would independently catch
§2.3 regardless of cause. Recorded here as a second motivating case,
not as new scope.

## 4. What would prove any of this

**`df -i` appears nowhere** in `tests/`, `scripts/`, `lean/e2e/` or
`passthrough/e2e/`. No drill has ever observed inode pressure. Minimum
legs, each with an anti-vacuity guard:

- **L1 (lite, the whole chain).** `mkfs.ext4 -N 20000` a small loopback
  volume, point a hub at a bucket holding ~50k objects, `importOnStart:
  true`. **Assert red before the fix:** listener binds; `rep.failed > 0`
  in the log; `is_publish_fenced() == false`; a manifest barrier
  publishes an entry count below the bucket's. *Anti-vacuity:* the
  bucket must hold more objects than the volume has free inodes — assert
  `objects > f_favail` at leg start, or the leg passes by not filling.
- **L2 (lite, the discarded retry).** After L1, grow the volume
  (`resize2fs`), restart, assert the namespace is **still** truncated
  and the sweep note is absent. This is §2.2 and it is the leg that
  distinguishes "capacity limit" from "permanent data invisibility".
- **L3 (lean, node-level).** N workspaces × `maxFiles` on one kind node;
  watch `nodefs.inodesFree` and assert which pod kubelet evicts. Expect
  it not to be the one that consumed the inodes.
- **L4 (xattr spill, arithmetic only so far).** `mkfs.ext4` a loopback,
  create stubs carrying `user.flint.tier.evicted` with a realistic
  `{generation}:{etag}` value, compare `df` before/after. If the ~63-byte
  payload plus the 23-byte name exceeds ext4's inline budget in a
  256-byte inode, every stub also burns a 4 KiB external xattr block —
  8 GiB on a 2M-object bucket. **Unmeasured.**

## 5. What is NOT verified

Stated plainly so nobody quotes this document as a measurement.

1. **None of it was executed.** Every claim is a code read at
   `85184494`. Each link (§1.1a–c, §2.1, §2.2, §2.3) is individually
   solid and cited; **the chain has never been observed firing.**
2. **The 16 KiB/inode figure** is the mke2fs default behind *this
   driver's* `mkfs.ext4 -F` (`main.rs:3940`). A lite hub on EBS, PD or
   Ceph gets its filesystem from **that** CSI driver, whose mkfs flags
   were not checked. Most use k8s `mount-utils` defaults (the same), but
   an xfs StorageClass removes the problem entirely — confirm before
   sizing anything on this number.
3. **§4 L4 (xattr spill)** is arithmetic against a remembered ext4
   inline-xattr budget, not a measurement. Hypothesis only.
4. **kubelet eviction ranking under `nodefs.inodesFree`** is quoted from
   general knowledge of the eviction manager, not from a run on this
   fleet. L3 is what settles it.
5. **passthrough** was read, not exercised, for cache behaviour. The
   conclusion "no local materialization" rests on `mounter_args`
   (`passthrough/inject.rs:251-296`) never emitting `--cache` and on the
   two injected volumes being empty `emptyDir`s.
