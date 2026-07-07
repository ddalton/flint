# pNFS CSI RWX isolation + Spark committer — fix plan

**Date**: 2026-07-07
**Status**: Fix 1 IMPLEMENTED + rig-validated (csi-e2e green: isolation,
subtree mount, striping, delete-reclaims, re-create-empty) with the
directory-rename placement sweep; Fix 2 documented in the operator
runbook (no repo code); Fix 3 open.

**Capacity decision (recorded 2026-07-07):** directory model for ALL
new pNFS CSI volumes — effectively option (c), but the "regression"
framing dissolved on verification: P0-4's gate (`enospc-drill.sh`)
asserts DS-statvfs registration + POOL-level bounded ENOSPC; the
sparse file's `set_len` never enforced anything (a consumer could
always write past it into pool space). So per-volume enforcement is
not lost — it never existed. Legacy file volumes keep their exact old
semantics (same-size re-create ok, resize refused, root mount via
volume-mode=file back-compat; NodePublish branches on the new
`pnfs.flint.io/volume-mode` context key so pre-upgrade PVs are
untouched). Per-volume quota remains future work (option b).

**Bonus finding while implementing:** `note_rename` re-keyed only the
exact placement key, so renaming a DIRECTORY left child pins at the
old path — fresh readers would mint new pins and read holes (exactly
the shape of Spark's rename-based commit). Fixed: prefix sweep
(`rename_placements_under`) + legacy-pin guard extension
(`has_legacy_placements_under`).
**Motivation**: The two blockers found by the Spark-on-pNFS dry-run
(`docs/plans/pnfs-spark-flight-benchmark.md`, "Phase 0 dry-run results",
Findings 1 & 2). Raw-read DS scaling is proven (1.81× at N=4); these
fixes are what stand between that and the real Parquet-Spark headline
with a `flint-pnfs` PVC per executor.

The two are independent: **Fix 1** is flint driver + MDS code (P1,
blocks the PVC packaging); **Fix 2** is Spark configuration only, no
flint code (P2, only affects Parquet write-back).

---

## Fix 1 — per-volume isolation for pNFS CSI volumes (P1)

### Root cause (confirmed in code)

The pNFS volume model is a **sparse sized file**, and NodePublish mounts
the **shared export root**:

- `spdk-csi-driver/src/pnfs/grpc.rs` `create_volume` (~L412–455):
  `OpenOptions::new().create_new(true)…open(<export>/<volume_id>)` then
  `f.set_len(size_bytes)` — one sparse file per PVC.
- `delete_volume` (~L458–480): `std::fs::remove_file(<export>/<volume_id>)`.
- `spdk-csi-driver/src/main.rs` `node_publish_volume` (~L3083–3094):
  `let nfs_source = format!("{}:/", mds_ip)` — mounts `MDS:/` (the export
  **root**) at the pod target. In-code comment: *"The kernel mounts the
  export root, not a per-volume path … volume_file is informational."*

Consequence (observed): every PVC — even a fresh one — sees the whole
export: other volumes' `pvc-<uuid>` sparse files and any leftover files.
There is no per-PVC directory to own, and RWX filesystem isolation /
cross-mount write-visibility can't be relied on. A single sized file is
the wrong primitive for an RWX POSIX share (you can't RWX-share a
loop-mounted image across nodes).

### Why it was built this way (not an accident)

The single-sized-file model was a deliberate choice, for two reasons —
which is exactly why the fix has to be careful:

1. **Capacity truth (P0-4, commit `de7efa5`).** The volume is a
   `set_len(size_bytes)` sparse file *specifically so CSI can report a
   real per-PVC capacity*, and so the DS maps `ENOSPC/EDQUOT/EROFS` to
   the right NFS errors instead of generic EIO. There is a dedicated
   gate — `tests/lima/pnfs/enospc-drill.sh` (64 MB DS: "capacity
   registers true, overfill fails bounded, heartbeat cleanup frees the
   space"). A directory has no intrinsic size, so this model was how
   per-volume capacity semantics were earned.
2. **pNFS files-layout striping is per-file, and it was staged as an
   MVP.** The CSI path landed in three PRs on 2026-05-02 (PR 1 MDS
   verbs, PR 2 driver module, PR 3 the `main.rs` mount). PR 3 took the
   simplest route — mount the export root — and *explicitly deferred*
   per-volume isolation (the "volume_file is informational" comment).
   The intended consumer was expected to use its one file by name.

So the existing model is effectively **block-ish: one big striped file
= one volume**. It fits a single-file / RWO consumer with strict
capacity. It does **not** fit an RWX filesystem consumer (Spark reading
a directory of Parquet parts) — that is the mismatch, and it is the
tension the fix must resolve rather than paper over.

### Target design — directory-per-volume (RWX-correct)

Model each pNFS PVC as a **directory subtree** `<export>/<volume_id>/`
that pods mount as an isolated shared POSIX namespace. This is the
natural RWX shape (many pods on many nodes mount the same subtree; NFS
handles concurrency) and matches the existing `<export>/<volume_id>`
intent in the module doc.

### Changes

1. **MDS `create_volume` (`pnfs/grpc.rs`)** — replace the
   create-file+`set_len` block with `std::fs::create_dir_all(
   <export>/<volume_id>)`. Keep the existing `volume_id` guard
   (non-empty, no `/`, no NUL). Idempotency: a re-create of an existing
   dir returns success (drop the file-size-mismatch check, or keep a
   hidden `.flint-size` marker if capacity signalling is wanted).
2. **NodePublish (`main.rs` pNFS branch, ~L3094)** — mount the
   per-volume subpath instead of the root:
   `let nfs_source = format!("{}:/{}", mds_ip, volume_file);`
   (`volume_file` is the `volume_id`). NFSv4.1 sub-path mounts work here
   — verified manually: `mount -t nfs4 MDS:/` plus subdirectories
   resolve and stripe correctly. Keep the existing mount options
   (`minorversion=1,nconnect=…,rsize=wsize=1M,noresvport`). Fallback if a
   sub-path export ever misbehaves: mount `MDS:/` to a per-pod staging
   dir and `mount --bind <staging>/<volume_id> <target>`.
3. **MDS `delete_volume` (`pnfs/grpc.rs`)** — `remove_dir_all(
   <export>/<volume_id>)` instead of `remove_file`, and reclaim **all**
   DS stripes under the `<volume_id>/…` prefix (today it reclaims one
   file keyed on the exact path; with a directory the placement keys
   become `<volume_id>/<name>`, so reclaim must sweep the prefix).
4. **Capacity — a first-class decision, not a footnote (this is the
   crux).** The sparse-file `set_len` was the only per-volume size
   signal, and it backs a *shipped, tested* feature (P0-4, capacity
   truth + bounded ENOSPC). A directory has none, so the naive
   file→dir change **silently regresses P0-4**. Three ways to handle
   it, in preference order:
   - **(a) Access-mode-aware model (recommended).** Branch on the PVC's
     access mode in `create_volume`: `ReadWriteMany` → directory subtree
     (Spark's case, capacity best-effort at the DS/LVS pool level);
     `ReadWriteOnce` → keep the sized-file model unchanged (preserves
     P0-4 and the enospc gate for RWO). This resolves the tension
     without giving anything up, at the cost of two code paths.
   - **(b) Directory + project quota.** Directory subtree for all, with
     an XFS/ext4 project quota (or NFS quota) sized to the request so
     ENOSPC is still bounded per-volume. Preserves capacity truth for
     RWX too; more infra (quota setup on the DS/export FS).
   - **(c) Directory + explicitly supersede P0-4 for RWX.** Report
     requested capacity to CSI but enforce only at the pool level;
     document that pNFS RWX capacity is best-effort and update/retire
     the enospc gate's per-volume expectation for RWX. Simplest code,
     but a deliberate, documented feature regression — only acceptable
     under the "ephemeral scratch tier" framing.
5. **Migration / orphans** — this changes the on-disk layout
   (file → dir) for the RWX path. Simplest: land before pNFS CSI is
   advertised (no real users yet). Add a one-time sweep that removes
   stale `pvc-*` sparse files from the export root, and document that
   existing file-based volumes are not auto-migrated.

### Regressions & risk (what this change can break)

The **data path does not regress** — striping, LAYOUTGET, and DS
placement are unchanged; files just live one level deeper. The risk is
concentrated in three areas:

| Area | Risk | Mitigation |
|---|---|---|
| **Capacity truth / ENOSPC** (P0-4, `enospc-drill.sh`) | **High** — the naive dir model drops per-volume size + bounded ENOSPC | Pick capacity option (a)/(b)/(c) above *before* coding; don't let the gate silently change meaning |
| **`csi-e2e.sh`** asserts CreateVolume makes a *file*, DeleteVolume removes it, and the `volume-file`/`size-bytes` contract | **Medium** — test breaks by construction | Rewrite the test for the dir model (assert isolation + reclaim), preserving intent — not just made green |
| **DS stripe reclaim** goes one-file → sweep-all-under-`<volume_id>/` | **Medium** — leak or over-reclaim if the prefix match is wrong | Unit-test reclaim with two volumes sharing a name prefix; assert no cross-volume deletion |
| **Sub-path mount** (`MDS:/<volume_id>`) idempotency / NodeUnstage | **Low** — verified manually, but staging/unmount paths need care | Cover in csi-e2e + an unmount/re-mount cycle |
| **RWO / single-file consumers** of the current model | **Low** — no advertised users; confirm access modes in use | Option (a) keeps RWO on the old model entirely |

### Tests (gate)

- **Fresh PVC is empty** — a new `flint-pnfs` PVC mounts an empty dir
  (no other volumes' files visible). Directly refutes the observed bug.
- **Two PVCs are isolated** — pod A writes to PVC-1; a pod on PVC-2
  cannot see A's files.
- **RWX cross-node** — two pods on different nodes, same PVC: writer's
  files (and checksums) are visible to the reader (the test that
  "passed" before only because of the shared root — must still pass, now
  for the right reason).
- **DeleteVolume reclaims** — dir gone and DS stripes freed; re-create
  of the same name is clean; reclaim of volume `foo` never touches
  `foobar` (prefix-match correctness).
- **Capacity gate** — `enospc-drill.sh` must still hold for whichever
  capacity option is chosen: per-volume bounded ENOSPC for RWO (option
  a) or RWX-with-quota (b); or, under option (c), the gate is
  consciously updated to pool-level and the change is documented.
- Keep the existing pynfs / recall / fsx gates green.

### Effort

The mount/isolation change itself is small and localized: 3 edit sites
(create/delete in `grpc.rs`, mount source in `main.rs`) + reclaim-by-
prefix. **The capacity decision (option a/b/c) is what sets the real
scope** — option (c) stays small; option (a) adds a second code path +
test matrix; option (b) adds quota plumbing on the export FS. No
protocol/layout changes in any case. Decide capacity first, then code.

---

## Fix 2 — Spark `file://` output committer on the pNFS mount (P2)

### Root cause

Parquet write fails: `java.io.IOException: Mkdirs failed to create
…/_temporary/0/_temporary/attempt_…/`. Reproducible single-threaded
(`coalesce(1)`) and as root — not permissions or a race. Hadoop's
`ChecksumFileSystem` (the default `LocalFileSystem` for `file://`)
creates `.crc` sidecars and does staged `mkdirs` that the NFS client
rejects (close-to-open visibility of just-created dirs). The **read
path is unaffected** — only rename/stage-based writes break.

### Fix — updated after testing on the cluster (2026-07-07)

The failing op is Java `File.mkdirs()` itself hitting the NFS client's
**cached negative lookup** (create dir → re-`stat` → stale "absent" →
false). Two things were tried/measured:

- **`spark.hadoop.fs.file.impl=RawLocalFileSystem` — did NOT work.**
  Same `Mkdirs failed` error; the checksum layer wasn't the cause.
- **`lookupcache=none` (or `actimeo=0`) mount option — works** (shell
  `mkdir -p` and the committer path succeed), but makes metadata-heavy
  reads (16 GB CSV scan) unusably slow. Not viable as a general default.

There is also a **hard blocker beyond the committer** (Finding 4 / Fix
3 below): long Parquet filenames exceed the MDS's 85-byte filehandle
path limit → the write fails with EIO *and* leaves un-deletable
`Remote I/O error` debris. So even a working committer isn't enough
until the filehandle limit is fixed.

**Working recipe (validated):** write Parquet to a **flint RWO block
PVC** (ext4/NVMe — the committer works there), then `cp` the finished
parts onto the pNFS mount **with short filenames** (`p0000.parquet`,
skip `.crc`). 120 parts / 3.36 GB landed clean and re-read from a fresh
mount; the Parquet scan then ran at ~546 MB/s (storage-bound, ~29× the
CSV scan). This is the harness default until Fix 3 lands.

Longer-term native-write fix: a **no-rename / direct output committer**
(`spark.sql.sources.commitProtocolClass` /
`spark.sql.parquet.output.committer.class`) **plus** the Fix 3
filehandle change so long names are accepted.

### Tests

- Parquet on pNFS (via the recipe above) is re-read from a fresh mount
  (row counts match) — done.
- Fold the recipe / chosen committer into the Phase 1–2 Spark harness
  defaults.

---

## Fix 3 — MDS metadata filehandle is path-based, ~85-byte limit (P2)

### Root cause (confirmed in code)

`nfs/v4/filehandle.rs::generate_handle` builds the MDS metadata
filehandle by embedding the **literal path** plus a redundant 32-byte
SHA-256 of it: `1 (version) + 8 (instance_id) + 32 (hash) + 2 (len) =
43` fixed bytes, against `Nfs4FileHandle::MAX_SIZE = 128` (the RFC 8881
`NFS4_FHSIZE`). That leaves only **85 bytes for the path**. Long names
(Spark's `part-00000-<uuid>-c000.snappy.parquet` + `.crc` + dir prefix)
overflow → `OPEN/LOOKUP: Path too long for file handle` →
`COMPOUND status Io` → client sees **EIO**, and half-written stripe
files become un-deletable (`Remote I/O error`).

Spec-compliant (128 is the max) but self-inflicted: the spec says
filehandles are *opaque*, not that they must carry the path. Flint's
**DS-side** pNFS v2 FH (`filehandle_pnfs.rs`) is already **id-based**
(21 bytes: version + instance_id + 8-byte file_id + stripe_index) with
no path limit.

### Fix

Make the MDS metadata FH **id-based like the DS FH** — an opaque
fixed-size `(instance_id, file_id)` handle, with an id↔path table on
the MDS (the placement store already keys on an allocated `file_id`).
Path length then stops mattering entirely. Keep the 128-byte envelope.

### Why it's coupled to Fix 1

The directory-per-volume RWX model makes paths *deeper*
(`<volume_id>/<app-name>`), so the 85-byte ceiling bites harder. Fix 3
should land with (or before) Fix 1, or RWX volumes with realistic app
filenames will hit EIO immediately.

### Tests

- Create/read/delete a file whose export-relative path is > 85 bytes
  (e.g. a Spark Parquet part name) — succeeds.
- Long-name write leaves no un-deletable debris on failure paths.
- Existing pynfs / smoke / recall gates stay green.

### Effort

Trivial — one Spark conf block; ~1 test run to confirm. No repo code.

---

## Sequencing

1. **Fix 2 first** (config, minutes) — unblocks CSV→Parquet conversion
   and the storage-bound Parquet-Spark scan, which is the immediate
   benchmark need. Works today via the direct-MDS-mount harness even
   without Fix 1.
2. **Fix 1 next** (driver code) — makes `flint-pnfs` a usable CSI
   StorageClass so executors can consume PVCs instead of the privileged
   direct-mount sidecar. Required before advertising pNFS CSI, not
   strictly required for the benchmark numbers.

## Non-goals

- Hard per-volume capacity enforcement (quota) — best-effort for now.
- Snapshots/clones on pNFS (unsupported in no-SPDK pNFS mode).
- Loop-mounted single-file (block/RWO) pNFS volumes — the RWX directory
  model is what the Spark use case needs; a block variant is a separate
  proposal if ever wanted.
