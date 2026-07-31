# Plan: remove the dead FFLv4 layout encoder

**Status**: EXECUTED 2026-07-31. Kept as the record — §"What the plan got
wrong" is the part worth reading.
**Written against**: `18e8bbd`. **Executed at**: `dda33cd` (no line drift —
`git diff 18e8bbd..dda33cd` touches only `pnfs/mds/callback.rs`).
**Scope**: deletion + comment repair only. No behaviour change.

## Why

`CHANGELOG.md:510` already records the decision at v1.0.0: pNFS Flex
Files "is not implemented and is deferred indefinitely" because
replication lives at the SPDK NVMe-oF RAID layer below the protocol,
and FFL would duplicate it with client-side write amplification plus a
separate rebuild scanner. ADR 0005
(`docs/decisions/0005-pnfs-durable-ds-replication-cost.md`) then
measured what that lower layer costs: ~0% write throughput
(0.98–0.99×), *faster* reads (1.23–2.50×), 2× capacity, and an N=4 r2
aggregate (1993 MiB/s) matching ADR 0004's r1 number (1978 MiB/s)
within noise. Nothing since has changed the calculus.

What remained in the tree was a half-built `encode_fflv4_layout` with
five green unit tests and no callers. It was **inert** — three
independent reasons it could not execute:

1. No non-test callers.
2. `FATTR4_FS_LAYOUT_TYPES` emits a hardcoded one-element array `[1]`
   (`fileops.rs:1289-1290`), gated only on `pnfs_enabled` — flexfiles
   is never advertised. Two further sites emit the same array
   (`fileops.rs:1459`, `:1901`); the third is *ungated*
   (`encode_single_attribute` takes no `pnfs_enabled`).
3. LAYOUTGET does branch on the client-requested `layout_type`
   (`dispatcher.rs:2126-2133`: `4 => LayoutType::FlexFiles`), but then
   discards the distinction — the response hardcodes
   `LAYOUT_TYPE_NFSV4_1_FILES` and unconditionally calls
   `encode_file_layout_striped`. So a type-4 LAYOUTGET is answered
   `NFS4_OK` with a body tagged type 1. That is a real wire defect, one
   advertisement flag away from reachable; this deletion neither
   created nor fixed it. **Fixed separately 2026-07-31** — see
   residuals.

So this was not a fire. The cost of carrying it was future-tense and
concentrated in two lines — `docs/plans/pnfs-production-readiness.md:273`
and `tests/lima/STATUS.md:768`, both of which said:

> Re-enable FFLv4 layout encoding for mirrors. ~3 days.

"Re-enable" reads like flipping a switch. Whoever acted on it would have
inherited code stale against every invariant the live files-layout path
has learned since:

- **Path-keyed filehandles** — it called
  `generate_pnfs_filehandle(instance_id, filename, i)`, the legacy
  name-hash scheme, whose own successor's doc comment
  (`filehandle_pnfs.rs`) records why it is wrong: a recreated same-name
  file collides with its predecessor's DS stripe files. The live path
  uses `generate_pnfs_filehandle_from_id(0, file_id, j)`.
- **No stripe rotation** — the live path derives per-file rotation from
  `file_id` so sub-8-MiB files don't all land on DS[0]; ADR 0004's
  small-file byte-spread criterion (48.4%/51.6%) depends on it. The FFL
  encoder emitted plain array order. (RFC 8435 has no
  `nfl_first_stripe_index` equivalent — position is implicit in array
  order — so for *striping* the rotation would have to be done by
  permuting `ffm_data_servers`. For *mirroring*, which is what Phase C
  actually wants, the question does not arise: one `ff_data_server4`
  per mirror, nothing to rotate.)
- **`ffds_stateid` is all zeros** ("anonymous stateid for now").
- **Invented credential semantics** — `ffds_user`/`ffds_group` were
  empty strings commented "empty = use client creds". Not a
  spec-defined sentinel; those fields drive RFC 8435's coupling model.
  After the AUTH_NONE⇒DENIED finding (`docs/pnfs-operator-runbook.md:21`)
  and the F44/F45/F46 identity-domain wave (`CHANGELOG.md:56`), a second
  client↔DS identity domain is the most expensive known bug class here.

And the five tests asserted this encoder's own output, so CI reported
"FFLv4 encoding works" for a path that had never been on a wire and that
`fileops.rs:1282-1284` records as having previously made the kernel
silently fall back to MDS-direct I/O. That is the instrument reporting
on itself — the same shape as the five harness bugs from the F65 drill.
(Dating that argument honestly: per `rust-ci.yml`, CI ran no Rust tests
at all until 2026-07-30, so the misleading green signal was ~1 day old.)

## What the plan got wrong

An adversarial re-verification before execution found four rationales
that were false in the direction a reader would act on. Corrected above;
recorded here because the *pattern* matters more than the instances.

1. **"LAYOUTGET never branches on `layout_type`."** It does
   (`dispatcher.rs:2126-2133`). The inertness argument rests on the
   *reply* being hardcoded, not the request being ignored.
2. **"deviceid built from `DefaultHasher` instead of
   `composite_device_id()`."** `pnfs/mds/layout.rs:199-214` uses the
   *identical* `DefaultHasher` → `to_be_bytes()` → both-halves
   construction. Switching could not have fixed hash instability.
3. **"never registered in `stripe_groups`, so GETDEVICEINFO could not
   resolve it."** Non-sequitur. `pnfs/mds/device.rs:367-381` builds the
   same bytes, and `mds/operations/mod.rs:394` resolves via
   `device_registry.get_by_binary_id` with a layout-type gate that
   already admits FlexFiles. Per-DS deviceids resolve fine.
   `stripe_groups` exists for the FILES layout's *composite* id — a
   thing an `ff_data_server4` slot does not have.
4. **Three `fileops.rs` line cites were off** (1288→1289, 1273→1282).

Two mechanical hazards, also caught before execution:

- **No application order** was stated for three absolute line ranges in
  one file. Applied top-to-bottom, the test range lands 74 lines past
  EOF; and the Edit-5 single-line deletion misapplies onto a whitespace
  line inside `encode_file_layout`, where it **compiles, silently
  no-ops, and reports success**.
- **The stated escape hatch — "re-anchor by the quoted text" — does not
  work.** The quoted blocks are de-indented against the file's 4-space
  indent, one line has a trailing space, and the Edit-5 quote matches
  three sites.

Executed by computing all ranges against the unmodified files and
deleting in a single pass, with each range's first and last line
asserted against expected text before any write.

## What was done

| # | file | action |
|---|---|---|
| 1 | `nfs/v4/dispatcher.rs` | deleted `2642-2756` — `encode_fflv4_layout` plus the orphaned FILE-layout doc comment that Rust was attaching to it |
| 2 | `nfs/v4/dispatcher.rs` | deleted `2759` — a stale `#[allow(dead_code)]` sitting on `encode_file_layout_striped`, which is *live* (called at `:2203`) |
| 3 | `nfs/v4/dispatcher.rs` | deleted `3164-3464` — the five `test_encode_fflv4_*` tests. Stopped at 3464: the next test, `striped_layout_rotates_first_stripe_index_per_file`, guards the live files-layout path |
| 4 | `pnfs/protocol.rs` | deleted the whole `layout_type` module (`16-22`), not just `LAYOUT4_FLEX_FILES`. All four constants were dead, `layout_type::` had zero hits tree-wide, and the module being `pub` meant dead-code analysis would never say so |
| 5 | `nfs/v4/filehandle_pnfs.rs` | deleted `generate_pnfs_filehandle` — the legacy name-hash wrapper. Edit 1 removed its only non-test caller; `pub` in a `pub mod`, so it would have gone dead **silently**. Its two tests exercised the live `parse_pnfs_filehandle` / `filehandle_to_ds_path`, so they were re-pointed at `generate_pnfs_filehandle_from_id` rather than deleted |
| 6 | `pnfs-production-readiness.md:273`, `tests/lima/STATUS.md:768` | rewrote both: **written fresh, not re-enabled**, with the list of what a fresh encoder must satisfy |

Net: 417 lines out of `dispatcher.rs`, 8 out of `protocol.rs`, 18 out of
`filehandle_pnfs.rs`.

### Deliberately KEPT

`pnfs/mds/operations/mod.rs:483` and `:523-603` —
`process_fflv4_layout_return`, decoding `FfLayoutReturn4` on the
LAYOUTRETURN path. This is live-compiled FFL surface. It is currently
unreachable (no client can send `layout_type=4` while all three
FS_LAYOUT_TYPES sites advertise `[1]`), but it is the *return* direction
and independent of the encoder. The original plan's exit criterion —
`grep fflv4 → expect zero hits` — would have failed on a correct
execution because of it, inviting whoever ran it to conclude the
deletion had failed, or to delete live code to force the grep to zero.

### Dropped

The `FATTR4_FS_LAYOUT_TYPES` comment rewrite at `fileops.rs:1274-1286`.
The plan asserted the existing comment was wrong to call the
`ff_layout4` body shape "mirrors-of-DSes" — arguing that `ffl_mirrors`
is the replica axis and `ffm_data_servers` the stripe axis, so "1 mirror
with N data servers" is correct for unmirrored striping. That reading is
plausible but **not settled in-repo**: no RFC 8435 XDR is vendored here,
Linux's `ff_layout_alloc_lseg()` rejects `ds_count != 1`, and `git log -1
cdbbe21` records the opposite conclusion from an actual kernel —
"structural bug (mirrors-of-DSes vs DSes-of-mirrors) … the kernel
silently discarded the body … without ever issuing GETDEVICEINFO".
Replacing a comment that matches the observed kernel behaviour with one
that contradicts it, on the strength of a spec reading nobody has tested,
is a worse comment. Left alone. Note it also propagates: `fileops.rs:1454-1466`
and `:1897-1904` back-reference this block by name.

(The comment's second clause — "a per-DS filehandle that we don't yet
generate" — *is* verifiably false today; `dispatcher.rs` mints one v2
identity FH per slot. Fixing that one clause is safe whenever someone
touches this comment for another reason.)

## Verification

The plan's original block was red before the change and could not
distinguish success from failure: `cargo build | grep -i "warn\|error"`
has a permanent 4-line floor (a multi-target collision between
`tests/nfs_client_test.rs` and `[[bin]] nfs-test-client` at
`Cargo.toml:26-27`, plus three `build.rs` `cargo:warning=` lines), bare
`cargo test` fails on a pre-existing `src/pnfs/mod.rs:49` doctest (E0277
— which is why `rust-ci.yml` runs `--lib` only), and `cargo build` never
compiles `#[cfg(test)]`, so it would not have typechecked the ~300
deleted test lines at all.

What was actually run, and what it showed:

```
cd spdk-csi-driver

cargo check --lib                  # before: 1 warning (encode_fflv4_layout never used)
                                   # after:  0 rustc warnings           ✓

cargo check --lib --tests          # clean                              ✓

cargo test --lib --no-fail-fast    # before: 1019 passed, 0 failed, 3 ignored
                                   # after:  1014 passed, 0 failed, 3 ignored   ✓
                                   # (−5 = the FFL tests; the two filehandle
                                   #  tests were re-pointed, not removed)

grep -rn "encode_fflv4_layout\|LAYOUT4_FLEX_FILES\|generate_pnfs_filehandle(" \
     --include='*.rs' src          # 0 hits                             ✓
grep -rn "layout_type::" --include='*.rs' src   # 0 hits                ✓
grep -rn "process_fflv4_layout_return" --include='*.rs' src
                                   # exactly 2 — KEPT ON PURPOSE        ✓
```

The dead-code warning going 1 → 0 is the load-bearing signal: it is the
one check that would have caught Edit 2 misapplying onto a whitespace
line, and the one the original plan's `cargo build` could never read.

No drill or live-cluster gate was needed. The code was unreachable, so
there was no runtime behaviour to re-validate — that is the whole
argument for deleting it.

## Residuals — out of scope, decide separately

- ~~**`dispatcher.rs` answers a type-4 LAYOUTGET with a type-1 body.**~~
  **FIXED 2026-07-31.** LAYOUTGET and GETDEVICEINFO — the two operations
  that *emit* a layout-typed body — now go through one
  `layout_type_served()` guard returning `NFS4ERR_UNKNOWN_LAYOUTTYPE`
  (10062) for anything but type 1, replacing the generic `NFS4ERR_NOTSUPP`
  they returned for types 2 and 3. GETDEVICEINFO was the worse of the
  two: it echoed the requested type back over a files-layout device
  address, so a type-4 caller got a body explicitly labelled FFLv4.
  LAYOUTRETURN deliberately stays lenient — it emits nothing, so there is
  nothing to mislabel, and accepting type 4 is what lets a client hand
  back a layout this server granted before `cdbbe21`.
- **`encode_file_layout` (`dispatcher.rs` ~2863-2916 pre-deletion) is
  genuinely dead** — `#[allow(dead_code)]`, zero callers. Files-layout
  code, so deleting it is a different judgement than deleting FFL. It
  also holds the only *wire* readers of `LayoutSegment.stripe_index` and
  `.pattern_offset`; deleting it makes two `pub` fields write-only with
  no warning, and drops the tree's only comment stating `nfl_util4` is
  32-bit. Don't confuse it with `pnfs/protocol.rs:634 pub fn
  encode_file_layout`, which *is* exercised by a test at `:772`.
- ~~**`striped_layout_rotates_first_stripe_index_per_file` does not test
  what its name implies.**~~ **FIXED 2026-07-31.** Replaced by three
  tests: the identity-keyed arm asserted against the exact documented
  mapping (`file_id % N`, for stripe widths that are not powers of two),
  the legacy `file_id == 0` arm kept separately and asserted to *differ*
  from it, and the invariant the placement drill actually caught — same
  `file_id`, different filehandle, same rotation, so a reader arriving
  after a RENAME reassembles the stripes in the order the writer laid
  them down.

  All four guards were mutation-tested. The rename test was written with
  two filehandles first; the mutation run showed it passing against a
  server whose rotation followed the FH, because with N=4 a single pair
  collides 25% of the time. It now sweeps six filehandles. A test that
  cannot fail is the same instrument-reports-on-itself shape this whole
  plan is about — it just took a mutation run to see it.

- **A duplicate `pnfs_error` module in `pnfs/protocol.rs` had six of
  seven error codes wrong** — found while reaching for the right
  constant for the fix above. `NFS4ERR_UNKNOWN_LAYOUTTYPE` was 10052,
  which is `NFS4ERR_BADSESSION`: a client told that tears down its
  session. `BADLAYOUT` and `RECALLCONFLICT` were both 10051, which is
  `BAD_SESSION_DIGEST`. Zero references, `pub`, so no warning would ever
  have fired — the third dead-`pub`-module trap in this one change.
  **Deleted 2026-07-31** rather than corrected; `nfs::v4::protocol::Nfs4Status`
  is the definition the wire encoder actually uses.
- **`filehandle_pnfs.rs` is mislabelled RFC 8435** at `nfs/v4/mod.rs:22`,
  while its live output feeds the RFC 8881 §13 FILES path.

## If Phase C ever happens

The encoder was ~110 lines and every fix has an existing primitive to
copy (v2 FH generator, `LayoutState.stateid` at `layout.rs:363`).
Re-writing it is not the expensive part of Phase C. The expensive parts
are partial-write divergence across mirrors, per-mirror LAYOUTCOMMIT
status, the re-mirror coordinator (~2 of the ~4 weeks — re-implementing
at the file layer what ADR 0005 measured the raid layer doing in ~9s for
2 GiB, invisible to clients), and topology-aware placement. Plus the
cost that is in no estimate: owning two replication systems with
independent failure semantics that can disagree about the same bytes —
the shape the formal-models workstream keeps finding bugs in.

Deleting ~440 lines did not make that work meaningfully harder. It
removed a green CI signal that said the hard part was already done.
