# Measuring flint-lite's POSIX fidelity — the two missing arms

> **Status: design, NO code.** Written 2026-08-28 against `740e45db`.
> Every claim cites the path that carries it. Nothing here has been run.

## 0. Why this exists

`make test-pjdfstest` runs the industry-standard POSIX conformance suite
(8,798 assertions, 238 files) against a knfsd control arm, with genuinely
good differential craft. **It does not measure flint-lite.**

The server it starts is configured by `tests/lima/pnfs/lite-pynfs.yaml`,
which has **no `tier:` section**, and `pjdfstest-differential.sh` sets no
`FLINT_TIER_CAPTURE`. `capture::enabled()` (`tier/capture.rs:377`) is true
only when the config carries an enabled tier section — the sole production
call site is `pnfs/mds/server.rs:692` — or when that env var is set.
Neither holds. So all 8,798 assertions ran with the tier **off**.

Two consequences:

1. **`LINK` was never refused.** The refusal is gated on
   `capture::enabled()` (`dispatcher.rs:2168`). pjdfstest's `link/` tests
   exercised a code path flint-lite does not have.
2. **No object was ever written.** Nothing flushed, evicted, hydrated or
   imported, so no round-trip gap is in scope of any recorded number.

The recorded ceilings (`tests/lima/pjdfstest-baseline.json`) — 645
default-mode, 239 enforcing, against a 149-failure knfsd control — are a
conformance number for the hub **as an NFS server**. That is the *floor*
for flint-lite, not a measurement of it.

There is precedent for exactly this error one level up. `lite-pynfs.yaml`'s
own header records that every pynfs number had been taken against
`flint-nfs-server` while flint-lite runs `flint-pnfs-mds --standalone`, and
that three recovery mechanisms had drifted far enough apart to ship as
defects (locks not persisted, F33 never armed, state.db crash-loop; fixed
in 1.37.0). The suite was re-pointed at the right **binary**. It was never
re-pointed at the right **posture**.

## 1. What the two arms are, and why two

**Arm C — pjdfstest with the tier live.** Measures mount semantics while
the tier is running: dirty capture on the pre-ack path, the write gate,
the LINK refusal. Cheap: a config change plus guards.

**Arm D — round-trip fidelity.** Measures what survives
flush → evict → hydrate, and flush → destroy → import. This needs a new
harness, because **pjdfstest structurally cannot do it**: each test
creates, asserts and removes its files inside one test body, so there is
no seam where a flush barrier could land between the write and the
assertion. Running pjdfstest with the tier on measures a mount under a
tier; it never measures a round trip.

`tests/lima/pnfs/tier-drill.sh` already exercises the round trip end to
end (flush barrier, forced eviction, PVC destruction, import, warm fill)
in ~57 assertions. It answers *does DR complete*. Arm D answers *what did
DR lose*. Those are different questions and the second has never been
asked.

## 2. Arm C — `pjdfstest` with the tier on

`tests/lima/pnfs/pjdfstest-tiered-differential.sh`, or a `TIER=1` mode on
the existing script.

**Config.** Reuse the tier block from `tier-drill.sh:102-114` verbatim
(it is known-good against MinIO) on top of `lite-pynfs.yaml`. Set
`watermarkPct: 99` so eviction does **not** fire — arm C is about a live
tier, not about eviction; D1 owns that.

**Arms.** Three, not two: knfsd control, flint tier-off, flint tier-on.
The interesting number is the **tier-on minus tier-off** delta. Differencing
tier-on directly against knfsd would re-charge flint for the 645 it already
has a ceiling for.

**Expected new failures, which are correct and must not read as
regressions.** `link/*.t` should newly fail. Record them under their own
baseline key with their own ceiling:

```json
"flint_only_max_tiered": <n>,
"tiered_expected_deltas": { "link": "LINK is refused while the tier is on (dispatcher.rs:2168)" }
```

A tiered failure in a file **not** on that list is a regression. A file on
that list that *stops* failing means the refusal regressed — assert both
directions.

**Anti-vacuity — the leg that matters.** Without it, arm C silently
degenerates into a second copy of arm A and reports a clean sheet by
measuring nothing. Two guards, both hard VOIDs:

- **The falsifiability leg: `link/00.t` MUST fail in the tier-on arm.**
  If LINK succeeded, capture was not enabled and the whole arm is void.
  This is the one assertion that can distinguish "tier on" from "tier off"
  from the client side alone.
- **Objects must exist under the prefix after the run** (`mc ls --recursive`,
  count > 0). Proves the flusher ran, not merely that capture was flipped.

**Known risk, to size before committing.** With the tier on, every mutating
op takes a durable pre-ack dirty mark (`dispatcher.rs:726-739`, which
refuses the ack if the mark fails). pjdfstest performs on the order of
10^4 namespace ops. Runtime and state.db churn may blow up. Time a single
`prove -r /opt/pjdfstest/tests/chmod` first and extrapolate before running
the full 238.

## 3. Arm D — the round-trip fidelity differential

`tests/lima/pnfs/posix-roundtrip-differential.sh`. New harness. Four phases.

### 3.1 Build a POSIX-hostile tree

Deliberately include the declined list *and* the things nobody has checked:

| Class | Cases |
| --- | --- |
| Regular files | 0 bytes, 1 byte, part-boundary, multi-part |
| **Sparse** | holes at start / middle / end; `st_blocks` far below `st_size` |
| Dirs | nested to depth; **one path near `PATH_MAX`** (pins the known EIO bug); **empty dirs** (no object exists for one — do they round-trip at all?) |
| Symlinks | relative, absolute, dangling, to-dir, long target |
| **Hard links** | 2 names, 3 names, link-then-unlink-one |
| Special | FIFO, socket, char dev, block dev (stand-ins today — record what *actually* happens) |
| Modes | full 07777 sweep incl. setuid/setgid/sticky |
| Ownership | several uid/gid pairs |
| Times | far past, far future, **nanosecond-precision** values |
| xattrs | `user.*`, if settable at all |
| **Names** | valid UTF-8, **invalid UTF-8 bytes**, embedded newline, leading/trailing space, control chars, **NFC vs NFD pair**, 255-byte name, a literal `.flint` directory |

The hard-link subtree must be built **while the tier is off**, then the hub
restarted with the tier on. That is the only way to construct it (LINK is
refused), and it is precisely the untested **ingestion** path: the tier has
no `nlink` awareness anywhere (`grep -rn nlink spdk-csi-driver/src/tier/`
returns nothing relevant), so the refusal guards creation and nothing
guards arrival.

### 3.2 Fingerprint it — the oracle

Walk the tree; per path record type, mode, uid, gid, size, **`st_blocks`**
(the only way to see sparseness), mtime **with nanoseconds**, ctime,
`nlink`, symlink target, xattr set, content hash. Emit sorted JSON.

Two rules that decide whether this harness keeps its teeth:

- **`st_ino` is compared as an equivalence class, never as an absolute.**
  What matters is *which paths share an inode*. Inode numbers legitimately
  change across a DR restore; a fingerprint that compared them directly
  would fail everywhere for a non-reason, someone would delete the field,
  and hard-link detection would leave with it.
- **The oracle is a standalone script (python3 in the VM). It must not
  import or link any flint code.** An instrument that shares logic with the
  thing it measures reports on itself. This repo has already produced five
  bugs of that shape.

### 3.3 Round-trip it — three legs, because they have different fidelity

- **D1 — evict/hydrate.** Flush barrier; drop `watermarkPct` to force
  eviction of everything; read every file back to trigger hydration;
  re-fingerprint.
  *Guard:* assert eviction actually happened (`tier_evicted` row count > 0,
  or `st_blocks` observed at 0) — otherwise D1 tests nothing and passes.
- **D2 — manifest DR.** Flush barrier; destroy export tree **and**
  state.db; restart against the surviving bucket with `importOnStart`;
  re-fingerprint.
- **D3 — sweep DR.** Same as D2, but **delete `.flint/manifest` first**,
  forcing the bucket-sweep lane.
  *Guard:* assert the manifest object is absent at restart, else D3 is a
  duplicate of D2.

**D3 is the leg to expect the worst from, and nothing covers it today.**
The manifest is the sole carrier of mode/uid/gid/mtime, directories and
symlink targets (`tier/manifest.rs:6-14`). The sweep lane has no manifest
by construction. The two lanes therefore cannot have equal fidelity, and
the size of that gap has never been measured.

### 3.4 Diff, classified

Three buckets, not pass/fail:

- **EXPECTED-LOSS** — matches the documented declined list
  (`tier/manifest.rs:19-33`). Allowed, but **every instance is enumerated
  in a baseline file**, so a new loss cannot hide inside "hard links don't
  round-trip".
- **UNEXPECTED-LOSS** — fail.
- **SILENT vs LOUD** — did anything count or log it? A loss the manifest
  counted (`beyond_rpo`, `skipped_special`) is categorically different from
  one nothing noticed. Report the split; the silent column is the one that
  should trend to zero.

### 3.5 Anti-vacuity for D

A fidelity differential that cannot fail is worse than none: it certifies.

- **Falsifiability leg (mandatory before trusting any green run).** Delete
  or corrupt one object in the bucket, re-run D2. **It must go RED.** If it
  stays green, the fingerprint is not reading what it claims to read.
- Assert the tree is non-empty and the fingerprint entry count matches what
  the builder created, **before** diffing. An empty tree diffs clean
  against an empty tree.
- Assert every builder case actually got created — a `mknod` that silently
  failed removes that row from both sides and diffs clean.

## 4. Order

1. Arm C's config + the two VOID guards. Cheapest, and the `link/00.t`
   falsifiability leg is reusable evidence that "tier on" is real.
2. Arm D's fingerprint tool + its falsifiability leg — **before** any D
   leg is trusted.
3. D2, then D3 (the expected-worst), then D1.
4. Record ceilings only after inspecting the diffs by hand. A ceiling
   adopted from an uninspected run launders whatever it contains.
