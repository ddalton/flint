# NFSv4.2 server-side copy and sparse-file handling

Status: **Phase 0 landed.** Phases 1–3 are specified below and not started.

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

## Phase 1 — conformance on the live path — NOT STARTED

| # | Change | Site | Error |
|---|---|---|---|
| 1.1 | Consume `ca_source_server<netloc4>`; NOTSUPP when non-empty | `compound.rs` COPY decoder | NOTSUPP |
| 1.2 | `cr_synchronous = TRUE` unconditionally on success | `perfops.rs`, `dispatcher.rs` | — |
| 1.3 | Unconditional `sync_all()` before replying; keep FILE_SYNC4 | `perfops.rs` | — |
| 1.4 | Range validation for both ops (end-of-range, `> i64::MAX` before the `off_t` cast) | shared helper | INVAL |
| 1.5 | src==dst overlap check on resolved `(dev, ino)` | both handlers | INVAL |
| 1.6 | change-counter bump for COPY and CLONE | `perfops.rs` | — |
| 1.7 | SEEK: `eof = (offset >= size)`; ENXIO → NXIO | `perfops.rs` | NXIO |
| 1.8 | ENOSPC → NoSpc; fix `count: 0` on partial failure | `perfops.rs` | NOSPC |
| 1.9 | SPACE_USED for striped files; delete the dead second attribute encoder | `fileops.rs` | — |

Notes:

- **1.1 first.** The COPY decoder stops before `ca_source_server`, so the
  array's length word is read as the next opcode. Zero is reserved →
  `Operation::Unsupported` → the decode loop breaks and truncates the
  compound. Separately, a **non-empty** `ca_source_server` — an
  inter-server copy request — is silently performed *locally* and reported
  OK. That is the F15 class exactly. Land this before any wire capture:
  you cannot read `ca_synchronous` off a trace with confidence while the
  server's own view of COPY4args is known to be short.
- **1.3** makes the hardcoded `wr_committed = FILE_SYNC4` true by
  construction with no new `CopyResult` fields and no dependence on the
  unmeasured `ca_synchronous` bit.
- **1.5** compares `(dev, ino)`, not paths: the filehandle layer follows a
  rename-alias table, so one inode is legitimately reachable through
  different handle bytes.
- **1.7's `eof` half** is unconditionally wrong today and needs no RFC
  arbitration — `sr_eof` is hardcoded false, so it can never be true on a
  successful lseek. `Nfs4Status::NxIo` is declared and used nowhere.

## Phase 2 — measurement — NOT STARTED, cheap, re-orders Phase 3

**Nobody has established whether a Linux client falls back cleanly when
COPY returns NFS4ERR_NOTSUPP.** It is unmeasured in both directions: no
pcap, drill, or log line in the tree shows a COPY ever arriving at flint,
and pynfs COPY5 is SKIP in every artifact.

The in-tree precedent is READ_PLUS (F15, observed live) and it is
**weaker than it looks**: READ_PLUS falls back to plain READ, mandatory in
every minor version, so the client always has somewhere to go. COPY's
fallback is `copy_file_range(2)` erroring into userspace.

One capture on the **shipped RWX mount** answers all of it:

```
tcpdump -i any -s0 -w /tmp/copy.pcap port 2049 &
strace -f -e trace=copy_file_range,ioctl,read,write,lseek cp --reflink=never big.bin copy1
strace -f -e trace=ioctl cp --reflink=always big.bin copy2
```

Read the XDR by hand (`tshark -O nfs -V`). **Do not `grep -c` the
capture** — this repo has already shipped a drill that scored `grep -c CB_`
on binary XDR and structurally could never match.

It settles: whether opcode 60 is emitted at all; `ca_synchronous`
TRUE/FALSE (three findings lean on that one bit in *opposite* directions);
whether `ca_source_server` is on the wire; whether NOTSUPP clears the
capability mount-wide or is re-asked per call (a free guard vs a per-call
tax); and whether the client emits opcode 71 at all given
`fattr4_clone_blksize` is absent from SUPPORTED_ATTRS.

Also: find out **why** ALLOC1-3/COPY5 are skipped in all 25 pynfs
artifacts, and convert the aggregate gate to a named-code gate where
PASS→SKIP is a failure.

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
