# NFSv4.2 server-side copy and sparse-file handling

Status: **Phases 0 and 1 landed** (1.9 partially — see below). Phase 2 is
the wire capture and needs a cluster. Phase 3 is mostly dropped, with
reasons.

Scope: COPY (60), CLONE (71), ALLOCATE (59), DEALLOCATE (62), SEEK (69),
READ_PLUS (68).

## The reachability fact everything else hangs off

The **standalone / RWX** server mounts `vers=4.2` (`src/main.rs`), so all
of these operations are live on a shipped path today.

The **pNFS MDS** mount is hard-coded `minorversion=1` (`src/main.rs`), and
there is no StorageClass override — `mount_flags` appears nowhere in
`main.rs`, and there is no other pNFS mount site.

That looked like it made the pNFS side safe. It did not, because **the
server did not gate 4.2 opcodes on the minor version at all**: the
COMPOUND decoder read them by opcode number, the dispatcher matched them
by opcode number, and the only minor-version check rejected `> 2`. The
pNFS mount's safety was therefore a *Linux-client convention*, and a
single hand-mount against the MDS Service port reached every 4.2 handler.

Phase 0.6 makes it a server property. Rank findings accordingly: Tier A is
live on a shipped mount, Tier B was one client-convention away.

## Phase 0 — landed

| # | Change | Fixes |
|---|---|---|
| 0.1 | `try_reflink_clone` (FICLONE, destructive) → `try_reflink_range` (FICLONERANGE, non-destructive) | A1 |
| 0.2 | Deleted CLONE's `(0,0,0)` special case; one path, one reading of `count == 0`, via `resolve_range_len` | A2, A8(c) |
| 0.3 | pNFS guard on all five ops, split by where each resolves its path | B1 |
| 0.4 | `FakePnfs` test double + `create_test_dispatcher_pnfs` | C1 |
| 0.5 | Retro-coverage for the shipped READ/WRITE stub guard | — |
| 0.6 | Opcode↔minorversion gate → `NFS4ERR_OP_ILLEGAL` | — |

### A1 — CLONE destroyed the destination before it knew it could clone

`try_reflink_clone` opened the destination `.truncate(true)` **before**
the FICLONE ioctl, and returned `Err` on failure with the destination
already emptied. `mkfs.ext4` is a shipped filesystem option and ext4 has
no reflink, so **every** whole-file CLONE on ext4 emptied the destination
and rebuilt it non-atomically via `std::fs::copy`. If that rebuild then
failed (ENOSPC, EACCES) the client was told CLONE **failed** and the file
was gone — an error naming the opposite of what happened.

FICLONERANGE writes nothing on failure and grows the destination only
when the cloned range reaches past its end, which is the correct
byte-range CLONE semantic. `std::fs::copy` is gone entirely: it is
whole-file, it truncates, and it carries the source's permission bits,
none of which belong to any NFS operation.

### 0.3 — why the guard is in two different places

| Ops | Guard site | Key |
|---|---|---|
| ALLOCATE, DEALLOCATE, SEEK | `dispatcher.rs`, beside LINK's guard | `pnfs_current_fh_key` |
| COPY, CLONE | inside `perfops`, after the handler resolves its own paths | the stateid-resolved source and destination |

This is forced, not stylistic. Under RFC 7862 §15.2 the COPY source is
SAVED_FH and the destination is CURRENT_FH, and both handlers bind the
compound context as `_ctx` and never read it. **A `current_fh`-keyed
guard structurally cannot see a COPY's source**, for any client. The
other three do read `current_fh`, so a dispatcher-side guard there reads
bit-identically what the handler will read.

The predicate is `is_pnfs_managed(file_key)` — **per file, not per role**.
An MDS deliberately keeps never-layouted files fully accessible, a
decision recorded in both `pnfs/handler_trait.rs` and the READ/WRITE
guard. A role-level guard would make COPY stricter than WRITE on the
same file.

Error is `NFS4ERR_NOTSUPP`, matching what this server already answers for
LINK, RENAME and READ_PLUS on a striped file.

## Phase 1 — conformance on the live path — LANDED

Every rule below was taken from the RFC 7862 text, read directly rather
than recalled. Two things that changed the plan on contact:

**COPY and CLONE do not share a same-file rule.** §15.2.3: "SAVED_FH and
CURRENT_FH must be different files. If SAVED_FH and CURRENT_FH refer to
the same file, the operation MUST fail with NFS4ERR_INVAL" — no overlap
qualifier. §15.13.3 forbids it only when "the source and target ranges
overlap". A same-file, non-overlapping CLONE is legal; a same-file COPY
never is. They *do* share the source-range sentence verbatim, which is why
`resolve_range_len` is shared and the same-file checks are not.

**SEEK's ENXIO is two different answers.** Linux returns ENXIO both for
"the offset is past EOF" and for "there is no more content of that type
before EOF". §15.11.3 gives them OPPOSITE results — the first MUST be
NFS4ERR_NXIO, the second is NFS4_OK with `sr_eof` TRUE. The old code
collapsed both into `Ok(eof, size)`, so an out-of-range SEEK reported
success.

| # | Change | Error |
|---|---|---|
| 1.1 | `ca_source_server<netloc4>` consumed arm-by-arm; non-empty refused | NOTSUPP |
| 1.2 | `cr_synchronous` reports what the server DID | — |
| 1.3 | Unconditional `sync_all()` before replying | — |
| 1.4 | Shared source-range validation, `checked_add` | INVAL |
| 1.5 | COPY: same file at all. CLONE: same file AND overlapping | INVAL |
| 1.6 | change-counter bump for COPY and CLONE, keyed off the fd | — |
| 1.7 | SEEK `sr_eof`; past-EOF → NXIO; unknown `sa_what` → UNION_NOTSUPP | NXIO / UNION_NOTSUPP |
| 1.8 | ENOSPC → NOSPC; `off_t` representability guard | NOSPC / INVAL |
| 1.9 | **Partial** — dead encoders deleted; SPACE_USED value NOT changed | — |

`Nfs4Status::NxIo` and `Nfs4Status::UnionNotsupp` had both existed with
zero uses. SEEK is now the only producer of either.

**1.9 is deliberately half-done.** The two dead attribute encoders
(`encode_attributes`, `encode_single_attribute` — 493 lines, zero callers)
are gone; they disagreed with the live encoder on `space_used` itself,
which is how the next reader gets it wrong. The *value* is unchanged:
fixing it properly needs the per-file pNFS predicate threaded into the
attribute encoder, and doing it at role level would over-report for
never-layouted files — the same "per file, not per role" principle as the
guards. Phase 2.3 measures A3 before that fix is written.

### Not covered by any test, stated plainly

- **1.3, the unconditional fsync.** No unit test can observe whether
  `sync_all` was called. It is unverified by construction, not by
  omission.
- **1.7's `sr_eof` and NXIO behaviour** is Linux-gated and this
  development host is darwin, so those two arms were never *executed*
  here and their mutations were never run. They compile and are logically
  derived from the quoted RFC text; that is all that can be claimed until
  CI (ubuntu) or a cluster runs them.

Notes on the non-obvious ones:

- **1.1** was landed first for a reason: you cannot read `ca_synchronous`
  off a wire capture with any confidence while the server's own view of
  COPY4args is known to be short. An unknown `netloc_type4` discriminant
  is now BADXDR rather than a skip — the discriminant determines the arm's
  width, and a decoder that cannot determine the width cannot honestly
  claim to have consumed it.
- **1.3** makes the hardcoded `wr_committed = FILE_SYNC4` true by
  construction, with no new `CopyResult` fields and no dependence on the
  unmeasured `ca_synchronous` bit — correct whichever value the client
  sends. Plumbing `committed` + `verifier` through `CopyResult` stays
  deferred until the fsync cost is measured.
- **1.4** rejects rather than saturates. The old `len() - src_offset` on
  u64 wrapped in release (no `[profile]` section in the workspace); a
  `saturating_sub` would have hidden that rather than fixed it.
- **1.5** compares `(dev, ino)`, not paths: the filehandle layer follows a
  rename-alias table, so one inode is legitimately reachable through
  different handle bytes.
- **1.6** bumps from the destination **fd** inside the blocking closure,
  not `bump_path` — a raced rename must not steer it.

## Phase 2 — measurement — RUN 2026-08-01, and it found a livelock

**No cluster was needed.** The lima rig (Ubuntu 24.04, kernel 6.8.0-136,
nfs-utils 2.6.4) is a real Linux NFS client. The one change required was
running the **server** inside the VM too — the Makefile runs it on the
macOS host, where SEEK/ALLOCATE/DEALLOCATE are `#[cfg(target_os =
"linux")]` stubs that answer NOTSUPP unconditionally, so a capture against
that rig measures the platform, not the code. `cargo zigbuild --target
aarch64-unknown-linux-musl --bin flint-nfs-server`, `limactl copy`, run it
under `systemd-run` (backgrounding via `limactl shell ... &` does not
survive the shell exiting).

### What the wire said

**COPY livelocked a real Linux client.** One `copy_file_range()` of 1 MiB
produced **264,601 COPY RPCs** and the syscall never returned; the server
performed a full 1 MiB server-side copy on every iteration. Every reply
was individually well-formed and said NFS4_OK with `length: 1048576`,
`committed: FILE_SYNC4`, `synchronous: Yes`.

The cause was one line in the encoder:

```rust
encoder.encode_fixed_opaque(&[0u8; 8]); // wr_writeverf (sync copy: unused)
```

Linux issues COPY and COMMIT in **one compound** and compares COPY's
`wr_writeverf` against COMMIT's verifier. Zeros never match, so the client
read every successful copy as a server reboot and reissued the identical
COPY forever. Phase 1 had explicitly DEFERRED verifier plumbing as
cosmetic; the measurement overturned that.

| | COPY RPCs for one 1 MiB copy | syscall |
|---|---|---|
| before | 264,601 | never returned |
| after | 2 (one call, one reply) | returned 1048576 in 5.2 s |

Verified byte-identical afterwards on all three client paths:
`copy_file_range` (opcode 60), `cp --reflink=always` (opcode 71 via
FICLONE, `ioctl(...) = 0`), and plain `cp`.

### The deciding question, answered differently than expected

The question was "does the Linux client fall back cleanly when COPY
returns NOTSUPP?" It never arose, because **COPY does not return
NOTSUPP** — the client reaches a working COPY and CLONE on the shipped
`vers=4.2` mount. So:

- **COPY and CLONE are not theoretical.** A stock `cp --reflink=always`
  emits opcode 71, and `copy_file_range(2)` emits opcode 60. Both are
  reachable from ordinary user commands, which retroactively raises the
  severity of the Phase 0 CLONE truncate bug: that path is one `cp` away.
- The NOTSUPP fallback question **remains open for the pNFS striped-file
  guards**, which is where NOTSUPP is actually returned. Not yet measured.
- `ca_synchronous` arrives **TRUE** from Linux, and `ca_source_server` is
  **absent** (empty array) — settling the three findings that leaned on
  that bit in opposite directions. The empty array is exactly the case
  whose length word the old decoder ate as the next opcode.

### Method note

Two harness mistakes worth remembering. `cp --reflink=never` emits **zero**
`copy_file_range` calls — that flag forces a plain copy, so that arm
measured nothing; use `copy_file_range(2)` directly. And `tshark -c N`
limits packets **read**, not packets **matched**, so `-Y filter -c 6`
silently returns nothing on a large capture.

### 2.2 — answered, and now gated

`testserver.py`'s `--minorversion` **defaults to 1** (`:77`), and it skips
any test whose declared version range excludes it (`:193`). `st_sparse`
and `st_copy` are 4.2-only. The harness never passed the flag, so all four
were skipped **by construction** across 25 archived runs — the aggregate
"91 skipped" reported a clean bill of health for operations that never
executed.

Run with `--minorversion=2` against flint on 2026-08-01:

```
ALLOC1   st_sparse.testAllocateSupported    : PASS
ALLOC2   st_sparse.testAllocateStateidZero  : PASS
ALLOC3   st_sparse.testAllocateStateidOne   : PASS
COPY5    st_copy.testZeroLengthCopy         : PASS
```

They were never failing. Nobody asked.

Now `make test-nfs-42` plus `scripts/check-pynfs42.py`, which gates on
**named codes and treats SKIP as FAILURE** — a test that stops running is
precisely the regression this catches, and an aggregate count cannot tell
"passed" from "never ran". Those four are the entire 4.2 surface this
pynfs has; a gate naming COPY1..COPY4 would fail for the wrong reason.

### Still to run

- **2.3** `du -sh` vs `ls -l` on a striped file, to confirm or kill A3
  before the SPACE_USED fix is written. Needs the pNFS rig (MDS + 2 DSes),
  which `tests/lima/pnfs/pynfs.sh` already stands up in the same VM.
- **The NOTSUPP fallback question is still open** — but only for the pNFS
  striped-file guards, which is now the only place flint returns NOTSUPP
  for these ops. On the standalone mount the client never sees a refusal.

## Phase 3 — mostly dropped, and why

- **DEALLOCATE fanout `[defer]`** — needs a new MDS→DS RPC, and its gate
  is strictly harder than truncate's: truncate's is a scalar `min`
  because cuts are totally ordered; punched ranges are not, so it needs a
  persisted mergeable interval set layered onto a gate that is currently
  proven in TLA+.
- **SEEK extent map `[defer]`** — a `min` over N DSes with one offline is
  not a `min`, it is a refusal.
- **DS-local COPY `[drop]`** — inexpressible. `first_stripe_index =
  file_id % segments.len()` over UUIDv4-derived file_ids means two files'
  rotations agree with probability 1/N. Making it expressible needs a
  rotation-aware pin allocator — new coupling between COPY and placement
  minting, for a feature no client has been shown to use.
- **MDS reads through the DSes `[drop]`** — funnels N parallel client→DS
  paths through one metadata node over 2N hops. Slower than the fallback
  it replaces.
- **TLA+ modules for DEALLOCATE/COPY `[drop]`** — a `PunchFansOut` arm
  would model a delivery `DsControl` cannot perform: the "model the
  implementation, not the design" violation the craft rules exist to
  prevent.

## Known-unsafe, unchanged by Phase 0

1. **No Linux client has been shown to be happy with any of these
   refusals.** Only Phase 2 settles it. **ALLOCATE is the one op with
   prior wire evidence** (PG16's `posix_fallocate`, captured per-op on
   runw) and the one whose refusal costs measurably — sequence it first
   in any drill even though it ranks below COPY and SEEK by damage.
2. **The pNFS key helpers are fragile against a symlinked export path.**
   `FileHandleManager::new` canonicalizes the export path; `resolve_handle`
   deliberately does not canonicalize what it returns; both
   `pnfs_current_fh_key` and `stub_io_disposition` strip one from the
   other. If the two disagree, `strip_prefix` misses, the key becomes an
   absolute path, and **every pNFS guard silently degrades to "not
   striped"** — the failure mode is open, not closed. Production export
   paths have no symlink component so this is latent, but it was found by
   a test going red on macOS, not by design. Fixing it means changing key
   computation, which is also the placement-record key — not a Phase 0
   change.
3. **Byte-range locks are consulted by no I/O path at all.** RFC 7862's
   stated client-side mitigation for concurrent writes during a COPY does
   not exist on this server. Do **not** add lock enforcement to COPY
   alone — making COPY stricter than WRITE produces an inconsistency that
   looks like a guarantee and is not one.
4. **The per-file guard is TOCTOU-racy against a concurrent LAYOUTGET.**
   COPY's whole transfer runs inside one `spawn_blocking`, so a guard
   evaluated at entry is stale for the duration. Two latent conditions in
   series, so low — but the refusal is not airtight.
5. **COPY with `ca_count == 0` snapshots the source length once** and
   returns OK with a short `wr_count` if the source shrinks mid-copy.

## Test notes

Every guarantee added in Phase 0 is mutation-tested: 15 mutations, each
breaking exactly one guarantee, all killed. Three traps shaped the tests
and will shape Phase 1's:

- **Platform vacuity.** The real bodies of ALLOCATE, DEALLOCATE and SEEK
  are `#[cfg(target_os = "linux")]`; every other target returns NOTSUPP
  unconditionally. On darwin, `assert_eq!(status, NotSupp)` passes against
  completely unguarded code. Refusal tests for those three carry the Linux
  cfg **and an arm B** asserting the unpinned control does *not* answer
  NOTSUPP — verified by widening the cfg and watching arm B fail on
  darwin for exactly that reason.
- **The repo's own COPY/CLONE test template is inverted.**
  `test_copy`/`test_clone` set `current_fh = src, saved_fh = dst`,
  backwards from RFC 7862, and pass only because nothing reads `ctx`.
  A guard test written from that template pins the wrong file and ships a
  broken guard green. `the_copy_guard_does_not_read_the_current_filehandle`
  is the trap arm that catches it: it sets `current_fh` to an unrelated
  *unpinned* file, so a dispatcher-side implementation goes red while
  every other COPY test stays green.
- **`!= Ok` is green both ways.** All five perfops arms validate the
  stateid before touching a file, so a dummy stateid yields BadStateId
  before *and* after any guard. Every test allocates a real Open stateid
  bound to the target and asserts the exact status.

Harness bug found and fixed while doing this, worth remembering: the
mutation runner restored sources with `shutil.copy2`, which **preserves
mtime**, so cargo saw the restored file as older than the artifact built
from the mutated source and silently re-ran the **mutated** test binary.
It presented as three tests failing in the full suite while passing in
isolation. Always `os.utime` after a restore.
