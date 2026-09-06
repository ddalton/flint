# flint forge — compaction tiers (X18): geometric folds, one bitmap on the base, no index in the bucket

Status: **DESIGN 2026-09-05; phases 1–2 BUILT 2026-09-06** (`forge/syncer/src/fold.rs`,
the wiring in `server.rs`, `restore.rs`, `batch.rs`, `sweep.rs`, `gitcmd.rs`; 9 planner
property tests and 8 integration tests against real git and the memory store, the
reflog control among them — see §12 at the end for what was built, what was
measured and what is still owed). The design of record for
`docs/plans/flint-forge-simplification-2026-09-05.md` §7 row X18
("compaction has no tiers"). Three candidates were drafted
independently and each was refuted twice (durability + sweep; git
semantics + cost); §11 lists what was weighed. Every number below is
either measured (with its log named), simulated (with the script in
the scratchpad reproduced in §8), or marked **unverified** and carried
into §9 as a gate. The candidate chosen is the one that changes no
bucket rule, no snapshot field and no sweep predicate; what it gives
up is stated in §0 with the one number that could still reverse it.

Reading order for the impatient: §0 (the decision), §3 (the fold
rule), §8 (the pre-registered numbers), §9 (the gates).

## 0. The decision

| | chosen | why the others lose | judged by |
|---|---|---|---|
| **C — geometric tiers of plain packs; a single-pack `.bitmap` on the base only; no multi-pack index anywhere** | **yes**, with grafts from A, B and every refutation (§11) | — | P9: bytes uploaded ≤ 2.0 GB for 384 MiB pushed (today 12.84 GB); worst push ≤ 5 s (today 816 s). P7 at the worst tier state ≤ 1.5× solo wall. P5 unchanged. |
| A — index and bitmap rebuilt LOCALLY at restore and at folds | no | the index is rewritten at every fold that touches a covered pack, and with the restore writing it over ALL packs that is every fold — under load the bitmap is absent most of the time and one core is spent rewriting it (2.6 µs/object measured: 13 s per rewrite at 5 M objects, minutes at walgit's 73 M); its fold's CAS (`packs = local_packs() − superseded`) named packs never uploaded and could unname the base when a rebuild reproduced the base's own name — both fixable, both fixed here (§3.6); what is kept from A is the layout (nothing new in the bucket) and, as the §10 phase-5 contingency, the local cold-prefix index | the same numbers; A is the contingency if P7's number fails |
| B — the index and its bitmap as content-named objects the snapshot names | no | a second object class whose bitmap key is NOT a content address (same midx checksum, different bitmap bytes after a ref-only change — measured on 2.50.1), a fold CAS that named packs it had not uploaded, a restore repair arm and two model theorems for a file git can derive, ~32 B/object re-uploaded per fold, and a midx checksum that depends on pack mtime; at fleet scale (≤ 200 k objects) its restore advantage over a local rebuild is < 10 % of the restore | — |

What C gives up, stated so it can be wrong: between base rebuilds a
clone's objects outside the base are not bitmapped and not reused
verbatim; with `core.bigFileThreshold` bounding the delta search and
the base rebuilt at 50 % tier growth, the pre-registered ceiling is
1.5× the solo clone at the worst moment (§8 F19). If P7 refutes that,
the local cold-prefix index of §10 phase 5 is owed — and still nothing
enters the bucket.

## 1. The question, stated so it can be wrong

The bucket's rules (design §3, §10) are: every object under
`objects/pack/` is immutable and content-named, written with an
unconditional PUT; ONE object, `snapshot`, is the pointer, rewritten
by If-Match on its etag; the sweep's only reference predicate is
"named by the snapshot whose etag this sweep read" (`sweep.rs:22-26`,
matched by stem at `sweep.rs:112`). git's incremental compaction
(`repack --geometric`) wants `objects/pack/multi-pack-index` — a
MUTABLE file at a FIXED name whose bitmap is named by the index's
checksum — which is a second kind of object with a second reference
relation (the index names packs).

The question is whether forge needs that file at all. The claim of
this document: **it does not.** A repository of N packs with a
single-pack `.bitmap` on the largest one is served by stock git with
the base streamed verbatim (`pack-reused`) and the rest walked; a
tier fold is `pack-objects --stdin-packs` (git ≥ 2.32) into a side
directory, and the base is `pack-objects --all --write-bitmap-index`
— neither is `git repack`, neither needs an index, and both produce
immutable content-named packs the existing sweep already understands.

Wrong if: (a) git 2.45 in the shipped image does not use a
single-pack bitmap when other packs are present (gate G1); (b) the
walk over the un-bitmapped tiers makes a clone at the worst tier state
more than 1.5× a bitmapped clone after `core.bigFileThreshold` (F19);
(c) the amortised bytes per pushed byte are not ≤ 10× on every shape
in §8 (F14).

## 2. The bucket after the change, and the snapshot

Unchanged in kind. Under `<prefix>/git/objects/pack/`:

```
pack-<sha>.pack / .idx / .rev      every tier: push packs, fold packs, the base — immutable, content-named, unconditional PUT (packio.rs:56-118)
pack-<sha>.bitmap                  beside EXACTLY ONE named pack, the base (the only pack written with --write-bitmap-index)
```

No `multi-pack-index`, no `multi-pack-index-<sum>.bitmap`, no
`.keep`, no `.mtimes`, ever. `pack_siblings` (`gitcmd.rs:325-336`)
is unchanged: `.idx` always, `.bitmap` and `.rev` when present.

The pointer, `<prefix>/git/snapshot`: **no new field, `SNAPSHOT_VERSION`
stays 1** (`snapshot.rs:27`, whose rule is "bumped only for a change an
older syncer could MISREAD"). `packs` (`snapshot.rs:41-44`) remains the
whole reference set — base + tiers + push packs not yet folded. Tiers
are not recorded: they are a function of the named packs' byte sizes,
recomputed at every plan from the local `.pack` sizes and available to
a restore from the LIST (`restore.rs:175-187` already carries `size`).
An older syncer reads this snapshot as today's: it would resume full
repacks at 24 packs — cost, not integrity (§7.11).

The base is identified LOCALLY by a marker git itself honours: a
`.keep` file beside it containing `flint-forge base\n`. It is never
uploaded (`pack_siblings` does not list `.keep`); a restore recreates
it beside the named pack whose `.bitmap` the LIST carries (§4). In the
bucket the base is "the named pack with a `.bitmap` sibling" — at most
one after the first base rebuild under this design (§7.13 covers the
transient and the legacy case).

Local files, all under `<repo>/flint-forge/` (`lib.rs:195`, the same
emptyDir as `objects/pack`, so a rename is a rename):

```
fold/                  the fold task's scratch: pack-<sha>.{pack,idx,rev[,bitmap]} before the commit; wiped at start
fold-retained.json     [{name, unlink_after_unix}] — superseded packs kept on disk for readers (§3.5)
fold-ledger.json       [{stems: [...], unnamed_unix}] — what the ledger sweep deletes past the grace (§5.1)
```

Derived files: `objects/info/packs` is written FROM THE SNAPSHOT'S
LIST, not from `update-server-info`'s directory scan (§3.5 step 6);
`info/refs` and `HEAD` unchanged. Bundles unchanged.

## 3. The fold rule

### 3.1 The cut — git's split over bytes

`fold::plan(packs: &[(name, bytes, is_base)], factor) -> Option<Plan>`,
a pure function of ~40 lines, property-tested against the simulation
in §8. It is `split_pack_geometry` from `builtin/repack.c` (v2.45)
with the pack WEIGHT changed from object count to `.pack` bytes:

1. Input: the packs the CURRENT snapshot names (`sc.cell().snap.packs`,
   never `local_packs()`), minus the base, with their local sizes.
   A queued push's pack (`.keep`'d by receive-pack, unnamed) and a
   refused push's pack (X2, unnamed until the next batch names it) are
   therefore never inputs.
2. Sort ascending. Walk from the largest down while
   `pack[i] ≥ factor × pack[i−1]`; the first break is the split (+1).
   Then, with `total = Σ pack[..split]`, extend the split while
   `pack[split] ≤ factor × total`.
3. `Plan::Fold { inputs: S = pack[..split] }` if `|S| ≥ 2`; else none.

Factor 2 (`fold_factor`, `FLINT_FORGE_FOLD_FACTOR`; 0 = off, which keeps
the shipped `maybe_repack` as the control arm until §8 has run). Bytes,
not git's object counts: forge pays for bytes, and the rig's README
records why counts fail on a blob-shaped repository. Simulated over
five shapes (§8), factor 2 beats 3 and 4 on every one (P9 800 pushes:
7.6× vs 9.4× vs 10.6×).

Cadence: with uniform pushes git's rule folds at every second push;
the plan runs once per BATCH (after `deliver()`, `server.rs:409`) and
once per maintenance tick (`server.rs:228`), so under load the fold
rate is the batch rate, not the push rate. `fold_min_bytes` (default 0)
with `fold_max_packs` (default 64: fold regardless when the tier count
reaches it) is the knob if P2's request count says so; the simulation
shows a 32 MiB floor trims 10–50 % of the bytes and 40–99 % of the
folds, and without the pack cap would leave ~1,000 packs unfolded under
32 KiB pushes.

### 3.2 The base rebuild

`Plan::Base` when, and only when:

- there is no base and `Σ named ≥ base_min_bytes` (64 MiB; a fresh
  repository gets its bitmap early instead of after ~64 pushes), or
- `Σ non-base named packs ≥ base_tier_percent × base` (50 %, git's own
  split condition applied to the base),

and `base_rebuild_min_secs` (3600) have elapsed since the last one, and
the emptyDir has ≥ 1.2× the named pack bytes free (§7.9). The base
rebuild rolls EVERY named pack, by reachability:

```
git pack-objects --all --indexed-objects --write-bitmap-index --delta-base-offset --non-empty -q <state_dir>/fold/pack
```

stdin `Stdio::null()` — `--all` implies `--revs` and reads stdin to EOF
(`gitcmd.rs:76` already passes null when there is no input; an open
stdin hangs it). No `--reflog`: §7.7. Unreachable objects are dropped
here and nowhere else — today's `repack -a -d` semantics; X15's
retention window is `--keep-unreachable`, compatible with the bitmap
(verified on 2.50.1), not built here.

The percent is the knob that trades base-rebuild bytes against clone
CPU: at 50 % a base rebuild costs 3 bytes per byte pushed (1.5 B per
B/2); at 25 % it costs 5, and the simulation shows it worse on every
shape (P9 800: 10.5× vs 7.6×). It stays at 50 unless F19 fails.

### 3.3 The tier fold

```
git -c pack.window=0 pack-objects --stdin-packs --delta-base-offset --non-empty -q <state_dir>/fold/pack
```

with S's names (`pack-<sha>.pack`, one per line) on stdin. Existing
deltas are reused and exact duplicates (the copies `index-pack
--fix-thin` appended to each thin push) are packed once; `pack.window=0`
keeps the fold from re-searching deltas over every non-delta object,
which on the blob shape is 2.12 CPU-s per 64 MiB against 0.16 s with
the window off, for a pack 0.002 % larger (measured on 2.50.1; gate G6
repeats it on the source shape). A tier fold carries every object of
its inputs, reachable or not (`pack-objects --stdin-packs` is exactly
"the objects of the listed packs"), so a fold never drops anything and
the base rebuild is the only place reachability is applied. A tier
fold writes NO bitmap: `--stdin-packs --write-bitmap-index` silently
writes none (verified on 2.50.1, gate G1 on 2.45), which is why the
base is a different command.

### 3.4 Who runs what — the fold is beside the loop, the commit is on it

**The task** (`fold::spawn`, `tokio::spawn`, at most one in flight,
`Syncer.fold: Option<InFlight>`):

1. Inputs frozen ON THE LOOP before spawning: S (or "base"), the cell's
   etag and `packs`, the base marker. Freezing S before `pack-objects`
   reads the refs is the ordering the model's `FoldInputsAfterStart`
   mutation exists for (§5.3).
2. `pack-objects` on a blocking thread (`spawn_blocking`; git's own
   threads are bounded by `pack.threads`, set to `max(1, ncpu − 1)` for
   the task so `index-pack` for a concurrent push keeps a core).
3. Upload every sibling of F with `packio::upload_file` (unconditional,
   content-named) at fanout 1, checking `hold.fenced()` before each
   PUT. The task ticks ITS OWN `AtomicU64`, never the hold's: the
   renewer renews a `Pushing` phase only while `hold.progress()` moved
   (`lease.rs:294`, `status.rs:62-63`), and a fold ticking that counter
   would keep a WEDGED batch's holder renewing for the fold's whole
   upload — the inverse of the lie `Inv_NoSkipOverMovement` guards.
   While the phase is `Serving` the renewer is unconditional and the
   fold needs no sensor.
4. Stall detection on the fold's own counter: no bytes for
   `fold_stall_secs` (300) ⇒ the task is aborted, its scratch removed,
   the plan re-run at the next tick. No wall-clock bound: a 50 GiB base
   rebuild at 76 MiB/s is 11 min of upload after 10–15 min of
   `pack-objects`, and a bound that a large repository cannot meet is a
   livelock, not a limit (sweep rule 3's lesson).
5. Result: `FoldResult { f, inputs: S, is_base, cell_etag }` on a
   oneshot the loop selects on.

**The commit** (`fold::commit`, on the loop; a `select!` arm on the
oneshot, plus a check after each batch and on the tick; O(1) S3):

1. `sc.check_fence()`.
2. If `F ∈ cell.snap.packs` (a rebuild reproduced an existing name — a
   base rebuild over an unchanged reachable set reproduces the base's
   own name, 3/3 on 2.50.1; a tier fold whose other inputs were exact
   duplicates reproduces one input's): discard the scratch, upload
   nothing, and let `S := S \ {F}`. Never unlink F.
3. Otherwise rename F's siblings from the scratch into `objects/pack`
   in the order `.keep` (base only), `.pack`, `.rev`, `.bitmap`,
   `.idx` LAST — the X1 gate (`gitcmd.rs:282-295`): `local_packs`
   lists a pack only with its index.
4. ONE CAS with `next = cell.snap.clone(); next.packs = sorted((packs
   \ S) ∪ {F})`, carrying `pending_exported_commit` and
   `pending_bundle` exactly as `batch.rs:272-280` does (one shared
   helper `Snapshot::carry_pending`), through `snapshot::cas`. Never
   from `local_packs()`: a fresh listing can name a pack no batch
   uploaded (a refused push's, `batch.rs:197`), and a snapshot naming an
   un-uploaded pack is a refused restore (exit 78, final). 412 ⇒
   `sc.fence(…)` (the arm at `restore.rs:241-246`, kept). Any OTHER
   error ⇒ re-read the snapshot once: if it names F the write landed
   and the cell is adopted; if not, the error is returned as fatal and
   the process exits and restores — never "deferred" with a stale cell,
   which would let the next batch upload F on a push's path.
5. Move `S \ {F}` to the retained set (`fold-retained.json`,
   `unlink_after = now + fold_retain_secs`); no file is unlinked here.
   For a base rebuild the old base's `.keep` is removed and the new
   base's was written in step 3.
6. `publish_derived`: `objects/info/packs` generated from `next.packs`
   (the format is `P <name>\n` per pack and a blank line), then
   `info/refs`. Today the file is republished only by a batch
   (`batch.rs:307-310, 510-524`), so an idle repository's bucket would
   list swept packs after a tick-driven fold; and `update-server-info`
   lists every pack in the directory, retained ones included, which is
   why the list comes from the snapshot.
7. Append `S \ {F}`'s stems and `now` to `fold-ledger.json`.

Cost on a push's path: ≤ 5 renames, one CAS, two small PUTs, two JSON
writes — about three S3 round trips (~50 ms on EC2). Nothing of the
fold's BYTES is ever on it. The 816 s was the loop inside a 6 GiB
upload; under this design the loop is never inside one.

### 3.5 The batch beside a fold, and readers beside a commit

A batch never sees the scratch. Its step 4 listing (`batch.rs:218`)
and step 5 (`batch.rs:270`) go through ONE new wrapper,
`Syncer::listed_packs() = git.local_packs()? − retained`, and the plan
uses the same. So: F is named at the commit before it is ever listed
(the rename in step 3 precedes the CAS in step 4, both on the loop),
the batch after a commit uploads nothing of F (it is `known`), and a
retained pack is never re-named by a batch (which would re-upload it
under rule 4 — the collision the refuters found in every candidate's
"keep the old packs a while" fix).

Readers: an `upload-pack` in the git container that opened the old
pack set can die on an unlinked pack ("packfile … cannot be accessed")
— git's own `repack -d` race, present today once per 24 pushes and
under this design once per fold. Retained packs stay on disk for
`fold_retain_secs` (900; a 40 GiB clone at 76 MiB/s is 9 min) and are
unlinked on the tick. **Unverified: whether 2.45's `upload-pack` fails
at all on this race** — gate G5 settles it and sets the default (0 if
the control never fails; the knob stays either way).

The hook keeps waiting exactly as today; the only fold work a push can
queue behind is a commit already on the loop.

### 3.6 The amortised bound, stated correctly

Under git's rule over bytes, a byte pushed is rewritten once per tier
level it climbs before the base absorbs it, and the base is rewritten
once per `base_tier_percent` of growth. The sum is not a binary
counter (that would be log2 N rewrites) — the "≤ 2 × Σ smaller" rule
merges more aggressively and grows the top tier by ~1.62× per top
merge. The simulation (§8, factor 2, base at 50 %) gives, in bytes
uploaded per byte pushed:

| shape | this design | today (threshold 24) |
|---|---|---|
| P9: 6 GiB base + 48 × 8 MiB (the compare's window; no base rebuild inside it) | **4.5×** (1,744 MiB; 24 folds, largest 336 MiB) | 34.5× (2 whole-repository uploads; measured 33× = 12.84 GB) |
| P9 steady: 6 GiB + 800 × 8 MiB (one base rebuild, at push 384) | **7.6×** | 49.5× |
| RUN1: 0 → 875 × 8 MiB (12 base rebuilds, 64 MiB → 5.7 GiB) | **7.5×** | 19.3× unstalled (5.6× measured, stall-limited) |
| agent fleet: 1 GiB base + 10,000 × 32 KiB | **9.3×** | 1,572× |
| rig blob 512 MiB + 100 × 2 MiB | **4.8×** | 13.6× |
| rig blob 96 MiB + 30 × 2 MiB (the shipped rig) | 5.7× | 3.4× (measured) — **worse**; the rig must be re-sized |

The ceiling is a constant times a logarithm in N/p, where today's is
linear in the repository; the design does not make amplification 1×
and says so. The worst single UPLOAD is still the repository (the base
rebuild), once per 50 % growth and once per hour, off the path; the
worst single WAIT is a commit.

## 4. Restore, with X14 in view

Order (`restore.rs:41-170`, amended):

1. `init_bare` (unchanged config except §6), then **wipe
   `<state_dir>/fold/` and any `objects/pack/multi-pack-index*`** (a
   hand-run `repack --write-midx` in the pod; a stale index naming a
   deleted pack fails `fsck --connectivity-only` with exit 32 while
   the objects are all present — measured on 2.50.1 by two refuters).
2. Snapshot, LIST, the missing-pack revalidate loop (`restore.rs:60-93`)
   — unchanged; a fold under the previous holder moves the list exactly
   as a repack did.
3. **Reconcile packs, the twin of the refs rule the module header
   promises (`restore.rs:3-8`):** every local `pack-*.pack` with an
   `.idx` that the snapshot does not name is unlinked (idx first) unless
   `fold-retained.json` still retains it. A pack without an index (a
   push mid-migration) is left alone. This is what makes every fold
   crash window benign (§7.1-7.3) and brings the code to what
   `ForgeSync.tla`'s `Restore` already assumes (`localPacks' =
   usable`, line 405).
4. Fetch plan as today (`restore.rs:104-123`, by stem, one fan-out), the
   units sorted by size DESCENDING so the base's chunks start first and
   the tail is not one stream. The base's `.bitmap` and `.rev` travel by
   stem as today; the restore is clone-ready with no local bitmap
   build — the point where this beats A.
5. Write the base marker: the named pack whose `.bitmap` the LIST
   carries gets `.keep` = `flint-forge base`; if more than one carries a
   bitmap (a legacy `repack -b` pack plus a new base, or a hand repack),
   the largest is the base and the other bitmap is left in place (git
   selects one; §7.13).
6. Refs, HEAD unchanged. `fsck --connectivity-only --no-reflogs`
   (§7.7).
7. Serve. `/status.repo` gains `base`, `tierPacks`, `tierBytes`,
   `retained`, `fold: {stage, bytes, of}`.

**What X14 gets from this layout:** nothing it must have, two things it
may use. Refs are in the snapshot, independent of packs and of any
index — there is no index object that must land before git can open a
pack, so a refs-first server has no new precondition. And the LIST
gives every tier's size: the newest objects (every tip) live in the
smallest packs, so a refs-first restore may fetch smallest-first and
install refs whose tips have landed while the base streams; that is
X14's code and X14's number, and this design neither helps nor hinders
it beyond making the sizes visible. Restore time stays proportional to
the named bytes: 138 s for the 10 GiB drill repository (P5, compare
log), unchanged by tiers because `fetch_all`'s fan-out is across chunks
(`packio.rs:208-216`).

**A mid-restore crash** leaves what it leaves today — `.part` files
removed on error, complete files skipped on retry (`restore.rs:110`),
temporaries renamed only after every chunk landed — plus a scratch
directory the next start wipes and a retained file the next start
honours. There is no index object, so there is no state with an index
and a missing pack.

## 5. The sweep and the model

### 5.1 The four rules against a fold that supersedes packs

Code logic unchanged; the CALLERS change. Under this design there are
two sweeps:

**The ledger sweep** (new, `fold::sweep_ledger`, on the tick, capped at
64 requests per tick so the loop is held ≤ ~1 s): for each ledger entry
older than the grace, read the snapshot once, skip any stem it names
(none should be; X15's retained copies will change this — below), HEAD
each key and require age ≥ grace by the store's clock (rule 2), DELETE.
It is O(|S|) per fold, not O(orphans in the bucket).

**The full LIST sweep** (today's `sweep::sweep`, the four rules verbatim)
for what a crashed incarnation or a deposed straggler left: at claim
(after the restore, before serving) and at most once per
`sweep_every_secs` (3600) on the tick, **never while a fold is in
flight** (an assertion at `sweep.rs:57` and at
`abort_orphaned_uploads`, `sweep.rs:43` — whose premise "between
batches nothing of ours is in flight" is false during a fold's upload;
a tick-driven sweep without the guard would abort the holder's own
base rebuild every hour and the repository would never fold again),
and with a prefilter: a candidate whose LISTED age (`ListedObject.
last_modified_unix`, `crates/flint-store/src/lib.rs:340`, filled from
S3 at `s3.rs:518`) is under the grace is skipped before any HEAD. The
prefilter can only under-delete: an object can be made YOUNGER after
the LIST (a re-upload) but never older, so listed-age ≥ grace is
necessary for HEAD-age ≥ grace, and rule 2 is still enforced at the
delete. Without this the sweep the refuters costed — every orphan
HEADed serially under the writer lock, ~21,600 files inside one grace
at 1 push/s — would hold the loop for minutes.

Why the refuters' point about rule 3 is accepted: under this design
the fold pack F is uploaded before the CAS that names it and no sweep
of THIS process can run between the two (the ledger sweep deletes
only what a commit unnamed; the full sweep never runs with a fold in
flight; a straggler's sweep aborts on the rotated etag, `sweep.rs:68-78`).
So the grace protects none of the fold's own bytes and a control at
grace 0 cannot fail — the vacuity trap the module header names
(`ForgeSync.tla:73-76`). What the grace DOES protect: a bundle a client
is holding a URL for (`sweep.rs:90-95`), a batch's packs between its
upload and its CAS against a sweep on another task, and any future
reader that fetches WITHOUT rotating first — X14's pre-claim fetch is
exactly that reader, and this is the composition fact X14 must design
against: "named by the snapshot whose etag this sweep read" protects
only readers who rotated first.

Rule 4 (a re-upload refreshes the age) is exercised by a crash-retry
fold: the fold's name is deterministic for the same inputs (four runs,
threads 1 and 8, one name on 2.50.1; gate G1 on 2.45), so the retry
re-PUTs the same key.

**The reference predicate** stays exactly "named by the snapshot whose
etag this sweep read", matched by stem. Every object this design writes
is a pack sibling; the assignment's question — an index naming packs
the snapshot must also name — does not arise, and that absence is the
strongest argument for C on this item. With X15 the reference set
becomes the union over the live snapshot and every retained copy
(`LeanChunkGC.tla`'s `AllRefs`); for the ledger sweep that means an
entry's delete-after is `max(grace, X15's window)` and its snapshot
read covers the retained copies. Unreachable objects are then kept
across base rebuilds with `--keep-unreachable` for the same window —
X15's design, not this one.

`Phase::Sweeping` is retired: both sweeps and the fold run with the
phase `Serving`; readiness (`Facts::serving`, `status.rs:105-107`)
never flaps for hygiene.

### 5.2 f10's invariant, widened

`forge/e2e/f10-sweep.sh:80` ("every pack object in the bucket is either
named by the snapshot or younger than the grace") holds unchanged; its
`bucket_packs()` (line 21) keeps its `.pack$` filter because nothing new
appears. Add the second half: every pack the snapshot names is present
with its `.idx`, and exactly one named pack has a `.bitmap`.

### 5.3 The model — what `formal/ForgeSync.tla` needs

Today the module identifies push, commit and pack ("push p creates
commit p in pack p", header line 67; `Durable(p)` requires `p ∈
snap.packs`, line 778; `Inv_LandedPackComplete` line 791; `Restore`'s
`usable` test lines 392-395) and has no delete action at all
(`SweepDone`, line 370, only clears `uploads`). A fold breaks the
identification and a sweep that deletes is what the fold makes
load-bearing. The mutation, in order:

1. `holds ∈ [PackIds → SUBSET Pushes]`, `PackIds = Pushes ∪ FoldIds`
   (`FoldIds` a constant set, bounded by `MaxFolds`). A push pack holds
   its push; `holds[f] = UNION {holds[q] : q ∈ S}` for a tier fold;
   for a base rebuild `holds[f] = snap.history` at plan time (linear
   history: every landed push is reachable).
2. `Durable(p) == p ∈ snap.history ∧ ∃ q ∈ snap.packs : p ∈ holds[q] ∧
   q ∈ packObj ∧ q ∈ idxObj`; `Inv_LandedPackComplete` restated the
   same way; `Restore`'s check becomes "every named pack fetched with
   its index, and the ref's push held by some usable pack". New
   `Inv_NamedIsUploaded == ∀ q ∈ snap.packs : q ∈ packObj ∧ q ∈ idxObj`
   — true of the shipped protocol (every CAS names only what it
   uploaded or a prior CAS named) and the invariant the fold's CAS
   formula preserves.
3. Actions, a `Crash` possible between any two: `FoldPlan(s, S)` (S ⊆
   belief[s].packs, |S| ≥ 2, no fold in flight, `st[s] ∈ {serving,
   pushing}` — the task runs beside a batch); `FoldUpload(s)`
   (`packObj ∪ {f}`, `idxObj ∪ {f}` via `uploads`, so the claim-time
   abort ends a straggler's fold with `NoSuchUpload`); `FoldCommit(s)`
   (`st[s] = serving ∧ batch[s].stage = none`; CAS on `belief[s].etag`:
   `snap.packs' = (snap.packs \ S) ∪ {f}`, `localPacks' = (localPacks
   \ S) ∪ {f}`, belief updated; mismatch ⇒ `Fall`, with `stragglerLand`
   as in `BatchCas`). The fold sets neither `realMoved` nor
   `sensorMoved`: it is not the batch's movement.
4. `SweepDelete(s)`: deletes any `q ∈ packObj \ snap.packs` (age is
   abstract — every orphan is deletable), enabled only when `st[s] =
   serving`, no batch stage, and no fold uploaded-but-uncommitted. Its
   list-then-read-then-etag-check collapses to one action, as the
   header's faithfulness note already does for read-then-CAS pairs.
5. The renewer's twin witness: at `RenewTick`'s renewing branch,
   `renewOverWedge' = renewOverWedge ∨ (MustProgress(s) ∧ sensorMoved[s]
   ∧ ¬realMoved[s])`; `Inv_NoRenewOverWedge == ¬renewOverWedge` joins
   `Inv`. In the shipped module every ticking step also sets
   `realMoved`, so it holds.
6. Mutations, each registered in `scripts/check-tla.sh` beside lines
   1157-1162 as a required-fail `mutation_run`, each a real ordering in
   the code:
   - `FoldCasBeforeUpload` — `FoldCommit` enabled before `FoldUpload`;
     a crash between loses `Inv_NamedIsUploaded` and, at the next
     restore, `Inv_NoUnrestorable`. The fold twin of `CasBeforePacks`.
   - `FoldCasFromDisk` — `FoldCommit` names `localPacks \ S ∪ {f}` (A's
     and B's formula) instead of `belief.packs \ S ∪ {f}`; a refused
     push's pack that landed since the last batch (`IdxLand`, then
     `BatchStart`'s refuse arm) is named without an upload; loses
     `Inv_NamedIsUploaded`.
   - `FoldInputsAfterStart` — S taken after the base rebuild's
     reachability was read, so a push landed in between is in S
     (unnamed after the commit) and not in `holds[f]`; loses
     `Inv_AckedIsDurable`.
   - `FoldCommitMidBatch` — `FoldCommit` enabled while `batch[s].stage
     ≠ none`; the batch's earlier listing re-names S and omits f, and a
     `SweepDelete` between the two CASes deletes S; loses
     `Inv_NoUnrestorable`.
   - `SweepDuringFold` — `SweepDelete` enabled with f uploaded and
     uncommitted; f is deleted, then named; loses
     `Inv_NamedIsUploaded`.
   - `FoldTicksBatchSensor` — `FoldUpload` sets `sensorMoved` while a
     batch is at a stage that ticks nothing; loses
     `Inv_NoRenewOverWedge`.
   Documented NON-runs (a mutation that cannot lose proves nothing):
   `RacyGrace`/`NoRevalidate` against the fold (no sweep of this
   process can precede the fold's CAS — §5.1), and `FoldInputUnnamed`
   (an unnamed pack as a fold input is cost and a racing upload, never
   a loss).
7. `lean/formal/LeanChunkGC.tla`: no change. A fold is a publisher that
   replaces the live reference set, retained pointers = 0 until X15,
   which is the shape the module already checks under
   `GraceCoversPublish` (line 212).

## 6. Git requirements

**Shipped version.** Both server images are `FROM alpine:3.20`
(`spdk-csi-driver/docker/Dockerfile.forge-git:35`, whose comment says
"alpine:3.20 ships 2.45", asserted ≥ 2.43 at lines 48-49;
`Dockerfile.forge-syncer.prebuilt:17`). Alpine 3.20's package is
2.45.4-r0 (**unverified in this session** — from a refuter's package
lookup; gate G1 runs inside the image). Every experiment cited here ran
on 2.50.1 (this Mac). Every option used predates the 2.43 floor, so
`restore::GIT_FLOOR` (`restore.rs:27`) is unchanged:
`pack-objects --stdin-packs` (2.32), `--write-bitmap-index` on a single
pack (old), `.rev` by default (`pack.writeReverseIndex`, default true
since 2.41), `core.bigFileThreshold` (old), single-pack verbatim reuse
(`pack.allowPackReuse`, default true). Multi-pack reuse
(`pack.allowPackReuse=multi`, 2.45) needs a midx and is the phase-5
contingency only.

**The exact commands** (all via `Git::run`, `-C <repo>`,
`GIT_CONFIG_NOSYSTEM=1`, `HOME=/nonexistent`, `gitcmd.rs:63-93`):

| what | command | stdin |
|---|---|---|
| tier fold | `git -c pack.window=0 -c pack.threads=<ncpu−1> pack-objects --stdin-packs --delta-base-offset --non-empty -q <state_dir>/fold/pack` | `pack-<sha>.pack` per line |
| base rebuild | `git -c pack.threads=<ncpu−1> pack-objects --all --indexed-objects --write-bitmap-index --delta-base-offset --non-empty -q <state_dir>/fold/pack` | null |
| before a base rebuild | `git reflog expire --expire=now --all` | — |
| the proof at restore | `git fsck --connectivity-only --no-reflogs --no-progress` | — |
| never | `git repack` (any form); `git multi-pack-index write` | — |

**Why not `git repack`** — stated as the one reason that holds, not
the four the drafts gave: repack has no output directory, so its new
pack lands in `objects/pack` where `local_packs()` lists it and the next
batch uploads it ON A PUSH'S PATH — the mechanism the 816 s came from.
(The other reasons are avoidable: `--no-write-bitmap-index` gets past
"Incremental repacks are incompatible with bitmap indexes"; a `.keep`
on the base keeps geometric from swallowing it; `--keep-pack` exists.
Leg G's `--geometric=2 -d --write-midx` measured the size of the prize
and is retired: the product is the probe.)

**`init_bare` (`gitcmd.rs:150-210`) changes:** drop
`repack.writeBitmaps=true` (line 193; `pack-objects` ignores it, the
flag decides, and the syncer never runs `repack`); add
`core.bigFileThreshold=1m` **pending gate G3** (objects above it skip
the delta search in every `pack-objects`, including `upload-pack`'s —
17.0 → 0.61 CPU-s for a 256 MiB blob tier on 2.50.1 — at the cost of
delta compression for text files above 1 MiB, which G3 bounds on the
source shape); `pack.useBitmapBoundaryTraversal` is NOT set — it is
about negated tips (a fetch's haves), not a clone, and is an experiment
(§9 G3b), not a default. `core.logAllRefUpdates=true` (line 206) stays;
§7.7 says what it costs.

**What `pack_siblings` uploads** (`gitcmd.rs:325-336`, unchanged):
`.pack`, `.idx` always; `.bitmap`, `.rev` when present. Never `.keep`
(local liveness and the base marker), never `.mtimes` (cruft packs —
forge writes none), never `.promisor`, never `multi-pack-index*`. A new
`siblings_in(dir, pack)` serves the scratch directory.

**What `upload-pack` needs to serve a clone fast:** the base's
`.bitmap` beside the base, which the restore fetches by stem. Verified
on 2.50.1 in the real `upload-pack` shape (OIDs on stdin under `--revs`,
no `--all`): a 50-object bitmapped base beside two push packs reports
`pack-reused 50 (from 1)` of 60; with a fold pack beside the base
(this design's steady state) reuse holds; with `pack.useBitmaps=false`
it is 0. The bitmap is used while other packs exist — git does not
drop a single-pack bitmap because the pack count grew. **Unverified on
2.45: gate G1.** What is lost against a midx bitmap: reachability for
the tiers is a walk, and tier packs are not reused verbatim — §8 F19 is
the number.

**Bundle cuts** (`bundle.rs:213-235`, `git bundle create <path>
<branch>`): the same `pack-objects` path — the base streams verbatim,
the tiers are processed; no option changes. A bundle's create+upload is
on the loop today (a 1 GiB bundle is 5.8 s plus the PUT); that is the
hourly cost §8 accepted, not X18's.

## 7. Failure cases

1. **Crash between the fold's upload and its CAS.** The bucket holds F
   unnamed; S is still named and present (nothing deletes a named
   pack). Restart: the scratch is wiped (F was never renamed in), the
   restore fetches nothing new, the plan recomputes the same S, the
   retry re-PUTs the same name (rule 4 refreshes its age) or, if the
   name differs, the old F is a second orphan — the ledger never saw
   either, so the full sweep takes them past the grace.
2. **Crash between the rename into `objects/pack` and the CAS.** F and
   S both local with indexes; the snapshot names S only. Restart: the
   reconcile step (§4.3) unlinks F (unnamed, not retained); the plan
   re-runs. Without the reconcile the next batch would name F beside S
   and UPLOAD it on a push's path (`batch.rs:237`, an unnamed local
   pack is uploaded) — the reconcile is what makes this window benign.
3. **Crash between the CAS and the retained-set write.** The snapshot
   names F; S is local and unnamed. Restart: the reconcile unlinks S
   (unnamed, not in the retained file); the full sweep deletes S from
   the bucket past the grace (the ledger entry was never written).
   Correct, and the only cost is readers in the git container losing
   the retention.
4. **A deposed writer's fold landing late.** Its multipart F is aborted
   at the successor's claim (`sweep.rs:43-55`, `server.rs:137`), so its
   Complete fails `NoSuchUpload` and the task errors; a whole-PUT F (<
   64 MiB) lands content-named and unnamed by the successor's snapshot,
   swept past the grace. Its commit's CAS is If-Match on a rotated
   etag → 412 → fence (`Inv_NoStragglerLandAfterRestore`, unchanged).
   Its local renames and retained set are on a pod that is exiting.
5. **A client's pack git names identically to an existing one.** A
   name is the pack's trailer checksum: same name ⇒ same bytes ⇒ same
   objects. (a) Identical to a named pack: `index-pack` finalises onto
   the existing file, `known.contains` (`batch.rs:237`) skips the
   upload — as today. (b) Identical to a pack an in-flight fold is
   rolling: the commit moves it to retained after its CAS; the push's
   objects are in F; its batch names nothing new. (c) Identical to a
   RETAINED pack: `listed_packs()` excludes it, so the batch would not
   name it and the objects are in F already — but `index-pack`'s
   finalise onto a file the tick is about to unlink is a race
   (`.keep` first, then `.pack`… the unlink happens between): **gate
   G5** includes this shape; the safe rule is that the unlink skips any
   stem that currently carries a `.keep` (receive-pack's, or ours) and
   retries next tick.
6. **F's name equals an input's.** §3.4 step 2: F is not renamed, not
   uploaded, never unlinked; `S \ {F}` is unnamed. The A refuter's
   catastrophe (the base unnamed and unlinked by its own rebuild)
   cannot happen because the CAS formula is `(packs \ S) ∪ {F}` and the
   unlink set is `S \ {F}`.
7. **The reflog.** `core.logAllRefUpdates=true` logs every move; a
   rewind (`allow_non_fast_forward`) leaves the old tip reflog-only;
   today's `repack -a -d` passes `--reflog` and keeps it, so nothing in
   forge has ever dropped a reflogged object. A base rebuild without
   `--reflog` drops it, and on a WARM restart (emptyDir kept, `logs/`
   kept) `fsck --connectivity-only` walks reflogs and exits 2 ("invalid
   reflog entry") → `Refused` → exit 78 → final — a restorable
   repository refused, intermittently (a pod REPLACEMENT has no reflog
   and passes). Measured by two refuters on 2.50.1. The design does
   both halves: `reflog expire --expire=now --all` before every base
   rebuild (the local repository stays self-consistent for any
   reflog-walking command), and the proof is `--no-reflogs` (what it
   proves is the snapshot's refs, which have no reflog). Retention
   belongs in the bucket (X15), not in an emptyDir that differs by
   restart kind — the simplification note's "the bare repository has
   no reflog" was wrong about the code and right about the design.
8. **A fold that stalls in its upload.** The loop keeps serving; the
   stall detector (§3.4) aborts it after `fold_stall_secs` of no bytes;
   its parts are aborted by the next full sweep with no fold in flight
   (or the successor's claim); the plan re-runs. `/status.fold` shows
   the stage and the bytes.
9. **Disk.** A base rebuild holds ~2× the repository in the emptyDir
   until its commit, plus retained packs for 15 min, plus every push
   that lands meanwhile — today's `repack -a -d` envelope, with a
   higher peak because pushes are not blocked. The plan checks
   `statvfs` free ≥ 1.2× the named pack bytes before choosing a base
   rebuild and logs a deferral otherwise; the operator's
   ephemeral-storage request (`render.rs:296`, an `emptyDir` with no
   `sizeLimit` today) is sized at 2× from gate G7's measurement.
10. **A refused push's pack (X2).** Named by the next batch as today
    (`batch.rs:270` names every listed pack), it becomes a tier and
    folds; its unreachable objects are dropped at the next base
    rebuild. A cost this design inherits and does not fix; X2's local
    delete, when built, must not unlink a pack the snapshot names.
11. **An older syncer on this bucket** (rollback, or a fleet mid-roll):
    the layout is today's, so it restores and serves; it resumes
    `repack -a -d -b` at 24 packs — whose `-a` rolls the base and its
    bitmap into one pack — and the amplification returns until the
    roll completes. No refusal protects against it because there is
    nothing to misread. On a warm restart the older syncer inherits
    `fold-retained.json` it does not read and unnamed retained packs it
    would name and re-upload at its next batch (cost, not integrity).
12. **A hand-run `git repack` in the pod.** `-a -d` rewrites everything
    into one pack and deletes named packs locally: the next batch names
    the new pack and uploads it on a push's path, and the bucket still
    holds the old ones (named, then unnamed by that batch, then swept).
    Same class as today's "second writer of `objects/pack`"; the rule
    stands, and the base's `.keep` at least keeps `--geometric` from
    rolling it.
13. **Two bitmaps at once.** The commit's transient (new base renamed
    in, old base not yet unlinked — now for the retention window, not a
    moment) has two `.bitmap` files locally; git uses one, silently,
    and 2.50.1 takes the newer pack (`rev-list --test-bitmap`: "Located
    via pack '<new base>'"; a clone-shaped run reused the new base).
    **Unverified on 2.45: gate G1.** A legacy `.bitmap` from the old
    syncer's `repack -b` coexists the same way until its pack is
    folded. Neither is a correctness case.
14. **A push larger than the base** (the drill's 40 GiB push into a
    1 GiB repository). The tier fold's cut excludes only the base, so
    the huge push pack sits at the top of the tier progression and
    trips the base rule at once (Σ tiers ≥ 50 % of the base): one base
    rebuild of 41 GiB with a bitmap, once — today's shape at 25 packs,
    now capped by `base_rebuild_min_secs`.
15. **A restore that finds a `.bitmap` whose `.pack` is missing**:
    refused at `restore.rs:78-86` as any missing named pack (the stem
    is named). A `.bitmap` whose stem is unnamed is neither fetched nor
    kept. The "index names a pack the snapshot lacks" case has no
    referent under this design; its local cousin (a stray midx) is
    wiped at start (§4.1).

## 8. Measurement, pre-registered

The control arm everywhere is the SHIPPED rule (`fold_factor=0`,
`repack_threshold=24`, kept in the binary until these have run, then
deleted). Arms differ only in that variable. The simulation the
expectations come from is git's `split_pack_geometry` over bytes,
factor 2, the base excluded from tier folds and rebuilt at 50 % tier
growth (or at 64 MiB with no base) — 60 lines of Python, reproduced
in the rig as `amplify.py`'s prediction so the oracle is the same code.

### 8.1 Rigs and numbers

1. **`forge/e2e/repack/run-repack.sh`, re-sized.** At the shipped
   `BLOB_SEED_MB=96 PUSHES=30` the treatment is WORSE than the control
   (5.7× vs 3.4×: 30 pushes into 96 MiB is the regime where a full
   repack every 24 is cheap). Settings that separate the arms: blob
   `BLOB_SEED_MB=512 PUSHES=100 BLOB_PUSH_MB=2` — treatment ≤ 5.5×
   (simulated 4.84×, 968 MiB, 50 folds, largest 136 MiB, 0 base
   rebuilds), control ≥ 12× (simulated 13.6×, four whole-repository
   uploads of up to 704 MiB); source `PUSHES=100` — treatment ≤ 25× of
   content (the control's 5.1× tree-rewrite floor times ≈ 4.5× of fold
   rewriting), control ≥ 40×. `amplify.py` gains: no push in the
   treatment uploads more than the largest planned fold + its own pack;
   the run total is within 25 % of the simulated number; a spike equal
   to the repository outside a base rebuild is a FAIL. Leg G becomes
   the oracle that byte weights rewrite no more than git's count
   weights over the run on both shapes (open question 1 of the C
   draft) and is otherwise retired.
2. **`forge/e2e/walgit/run-compare.sh` with `cw-summary.sh`**, against
   `results/compare-20260905-220006.log` and
   `work-20260905-220006/cw-summary.txt` (a provisioning gate — the
   user's go, per the standing rule):
   - P9 (48 × 8 MiB, ~6 GiB repository): BytesUploaded ≤ 2.0 GB
     (simulated 1.83 GB = 4.5×) against 12,835,673,358 B (33×);
     per-push max ≤ 5 s against 816.41 s; median ≤ 1.0 s (0.98 today);
     the syncer's log shows 0 base rebuilds and a largest fold ≤ 400
     MiB. Stated honestly: the 48-push window contains no base
     rebuild; the like-for-like steady state on this shape is 7.6×, and
     a P9 at ≥ 400 pushes (or a ≤ 512 MiB seed) is the leg that shows
     it.
   - P2 (32 pushers, 60 s), with X20 also built: ≥ 5 pushes/s and
     median ≤ 10 s against 1.1/s and 78.64 s; BytesUploaded ≤ 100 MB
     against 5.44 GB; AllRequests per acknowledged push ≤ 12 (7.9
     today, 5.6 walgit) — the fold's PUTs and CAS are the only new
     requests, and X20's window is isolated by a P2 arm at
     `FLINT_FORGE_BATCH_WINDOW_MS=0`.
   - P7 (8 concurrent clones of 1 GiB), run TWICE: right after a forced
     base rebuild (expect solo 18.6 s and 3.5× as today) and at the
     worst tier state (tiers driven to 49 % of the base by pushes of
     the SAME shape as the branch): ≤ 1.5× solo wall and ≤ 2× solo
     server CPU-s at the worst state. This is the number that decides
     whether §10 phase 5 is owed.
   - P5 (cold start): unchanged (138 s ± one heartbeat), and `rev-list
     --test-bitmap <tip>` on the restored repository says "Located via
     pack '<base>'" — the bitmap travelled.
   - RUN1-style hour: ≤ 8× content (simulated 7.45× with 12 base
     rebuilds from empty) against 39.3 GB for ~7 GB (5.6×,
     stall-limited; 19.3× unstalled) — and the honest headline for
     that regime is that NO push waited, not the bytes: from empty the
     base is rebuilt at every 50 % of growth whatever the scheme.
3. **`forge/e2e/latency/run-latency.sh`, a new leg**: a 64 MiB push
   issued while a forced ≥ 1 GiB base rebuild is uploading, arms
   interleaved; ≤ 1.5× the solo median (2.55 s on the wire, P1) —
   bandwidth and CPU contention are the only effects a fold may have on
   a push.
4. **Fold CPU** on the blob and source shapes: `pack-objects
   --stdin-packs` with `pack.window=0` vs default, user+sys and output
   size (gate G6).
5. **Clone CPU vs tier bytes** (gate G3): `upload-pack` CPU-s for a
   full clone at tier fractions 0 %, 10 %, 25 %, 49 % of a 1 GiB base
   on both shapes, `core.bigFileThreshold` at 512m and 1m.

### 8.2 Falsifiers, design §13 style (numbered after falsifier 12)

13. **A fold is never on a push's path.** Over P9 + P2 + the latency
    leg no push waits more than 5 s while a fold is in flight (the log
    stamps fold start, upload done, commit). Wrong if any does.
    Control: the shipped binary, where the push after a repack waits
    the whole upload (816 s class); and a treatment build with F
    written straight into `objects/pack` — a batch uploads it and one
    push waits ≥ 30 s.
14. **Amortised bytes are logarithmic.** The rig's two re-sized shapes
    and P9 within 25 % of the simulated numbers (§8.1). Wrong if the
    blob treatment exceeds 5.5× or the source 25× at 100 pushes, or if
    P9 exceeds 2.0 GB. Control: factor 0 reproduces 13.6× / ≥ 40× /
    33×.
15. **The base is rebuilt rarely and off the path.** P9's 48 pushes: 0
    base rebuilds, largest fold ≤ 400 MiB; the hour: ≤ 1 base rebuild
    per 50 % of growth and ≤ 1 per hour. Wrong if a base rebuild
    fires inside P9's window or two fire inside one hour. Control:
    today's rule — 2 whole-repository uploads inside 48 pushes.
16. **A fold crash loses nothing.** `kill -9` the syncer at five points
    (inside `pack-objects`; F uploaded, not renamed; F renamed, not
    CAS'd; CAS'd, retained file not written; retained, ledger not
    written), 20 runs each: after restart `fsck --strict` passes, the
    snapshot names a superset of what the refs need, and F or S is
    gone from the bucket past the grace. Wrong if any restart refuses
    or any acknowledged push's objects are absent. Control: a
    treatment build whose commit CASes BEFORE the upload, killed
    between — the next restore exits 78; and the model's
    `FoldCasBeforeUpload` loses `Inv_NamedIsUploaded`.
17. **Exactly one bitmap, on the base, always used.** After every base
    rebuild and every restore: one `.bitmap` locally and in the bucket,
    beside a named pack that carries the `.keep` marker; `rev-list
    --test-bitmap <default tip>` says "Located via pack '<base>'"; a
    tier fold carries none. Wrong if zero or two named packs carry
    one, or the test-bitmap says "doesn't have an indexed bitmap" on a
    tip inside the base. Control: the base rule disabled so a tier
    fold's S includes the base — zero bitmaps (verified on 2.50.1: a
    geometric roll-up that swallows the bitmapped pack leaves none).
18. **The sweep is off the push path and complete.** 200 folds at 1
    push/s with `orphan_grace_secs=60`: the ledger sweep's lock hold
    per tick ≤ 1 s; past the grace every superseded pack is gone and
    every named pack present (f10's invariant, widened, §5.2); the
    full sweep never runs with a fold in flight (an assertion, and the
    log). Wrong if a tick holds the loop > 5 s or a named pack is
    missing. Control: the full LIST sweep placed at the commit (the C
    draft's shape) for 30 min at 1 push/s — lock holds that grow with
    the orphan population.
19. **Clone cost at the worst tier state is bounded.** P7 at 49 % tier
    bytes with `core.bigFileThreshold=1m`: ≤ 1.5× solo wall, ≤ 2× solo
    server CPU-s, on both shapes. Wrong if either bound is exceeded —
    and then §10 phase 5 is owed. Control: the blob shape at git's
    default threshold (512m) — ≥ 4× (66 ms per MiB of non-delta tier
    per clone, measured on 2.50.1).
20. **A warm restart after a rewind and a base rebuild serves.**
    Force-push a branch back one commit, drive a base rebuild, `kill
    -9` the syncer with the emptyDir kept: the restart reports Serving.
    Wrong if it exits 78. Control: the proof without `--no-reflogs` and
    the rebuild without the `reflog expire` — exit 78 with "invalid
    reflog entry".
21. **The fold ticks no batch sensor.** The memory double stalls a
    push's PUT while a fold uploads: the token goes quiet within
    `QUIET_POLLS` heartbeats and a challenger claims. Wrong if renewals
    continue for the fold's duration. Control: the fold ticking
    `hold.progress_handle()` — renewals continue; and the model's
    `FoldTicksBatchSensor` loses `Inv_NoRenewOverWedge`.
22. **Retention protects readers, or is not needed.** 8 concurrent
    clones of the 1 GiB repository looping while 50 fold commits land,
    `fold_retain_secs=900`: every clone completes and `fsck --strict`
    is clean and byte-identical to a control clone taken with folds
    off. Control: `fold_retain_secs=0` — if no clone ever fails on
    2.45, the default becomes 0 (gate G5) and the falsifier is recorded
    as "not needed on this git".

## 9. Gates — experiments that run before code

| gate | question | settles | how |
|---|---|---|---|
| **G1** | do the git facts hold on the SHIPPED 2.45, not the Mac's 2.50.1? | whether §6 is true of the image | inside `docker run --rm <flint-forge-git image> sh` (Docker Desktop: check `docker version --format {{.Server.Version}}` first and ask the peer session — the standing rule): (a) `pack-objects --stdin-packs` writes `.pack/.idx/.rev` to a base outside `objects/pack` with "reused N" = N; (b) `--stdin-packs --write-bitmap-index` writes no bitmap, rc 0; (c) the base rebuild in a side dir writes `.bitmap`; (d) a single-pack bitmap beside two push packs and beside a fold pack is used in the `upload-pack` shape (`pack-reused` > 0 with `GIT_TRACE2_PERF`); (e) two `.bitmap` files are silent and one is chosen; (f) two folds of the same S give one name; (g) `git --version` = 2.45.x. Any (a)-(e) failing changes §6 before a line of code |
| **G2** | what does the control cost at the re-sized rig settings? | the control numbers in F14 | `run-repack.sh` with the SHIPPED binary at `BLOB_SEED_MB=512 PUSHES=100` and source `PUSHES=100`; expect ≈ 13.6× and ≥ 40× |
| **G3** | how much does a clone cost per tier byte, and does `core.bigFileThreshold=1m` bound it without inflating source egress? | the default of `bigFileThreshold`; whether 50 % is the right `base_tier_percent`; F19's prediction | §8.1 item 5; keep 1m if the blob-shape clone CPU falls ≥ 10× and the source-shape clone's egress grows ≤ 5 %; otherwise 4m, 16m. **G3b**: a one-commit fetch whose have is in the tiers, `pack.useBitmapBoundaryTraversal` on/off, 5 reps interleaved — set it only if ≥ 10 % less server CPU |
| **G4** | the reflog leg (§7.7) on 2.45 | that `--no-reflogs` + `reflog expire` is sufficient and necessary | rewind, base rebuild in a side dir, delete S, `fsck --connectivity-only` with and without `--no-reflogs`; expect rc 2 / rc 0 |
| **G5** | does a 2.45 `upload-pack` fail when a pack it scheduled is unlinked mid-clone, and does `index-pack` finalising onto a pack being unlinked reappear it? | `fold_retain_secs` default (900 or 0); §7.5(c) | 8 looping clones of 1 GiB during 50 commits at retention 0; a push of a pack byte-identical to a retained one while the tick unlinks it |
| **G6** | fold CPU and size with `pack.window=0` on the source shape | whether tier folds run with the window off | `pack-objects --stdin-packs` both ways over a 100-push source repository; keep window 0 if the pack is ≤ 5 % larger |
| **G7** | the emptyDir peak during a base rebuild with P2's pushers running | the operator's ephemeral-storage request (§7.9) | the 10 GiB drill repository; `du objects/ flint-forge/` every second through a forced base rebuild |
| **G8** | the wire re-match (§8.1 item 2) | F13-F15, F19 on real S3 | provisioning gate — the user's go |

## 10. Phases and the order of work

**Phase 1 — the ground (no fold yet).**
- `forge/syncer/src/lib.rs:399-434` — `Syncer::listed_packs()` (=
  `local_packs()` − retained) and the retained set loaded from
  `fold-retained.json`; `batch.rs:218` and `:270` use it.
- `forge/syncer/src/restore.rs:60` — before the LIST: wipe
  `<state_dir>/fold/` and `objects/pack/multi-pack-index*`; after the
  fetch (`:122`): the pack reconcile (§4.3) and the base `.keep`
  marker; `:104-121` sort units by size descending; `:167` the proof
  becomes `--no-reflogs` (`gitcmd.rs:399-409`).
- `forge/syncer/src/batch.rs:510-524` — `publish_derived` writes
  `objects/info/packs` from `snap.packs`; made `pub(crate)`; the
  pending-carry lines `:272-280` become `Snapshot::carry_pending`.
- `forge/syncer/src/gitcmd.rs:193` — drop `repack.writeBitmaps`; add
  `core.bigFileThreshold` (value from G3); `siblings_in(dir, pack)`.
- Tests (`forge/syncer/src/tests.rs`): `a_restore_prunes_packs_the_snapshot_does_not_name`
  (control: today's restore keeps them and the next batch names them),
  `info_packs_lists_exactly_the_snapshots_packs`,
  `a_retained_pack_is_never_named_by_a_batch`.

**Phase 2 — the fold.**
- `forge/syncer/src/fold.rs` (NEW): `plan` (pure; property tests:
  the progression holds after a fold, a fold never increases the pack
  count, kept and base packs never enter S, the simulation's fold
  sequence at P9's shape — 24 folds, largest 336 MiB — reproduces);
  `spawn` (the task, §3.4); `commit`; `sweep_ledger`; `unlink_retained`
  (on the tick; skips any stem carrying a `.keep`).
- `forge/syncer/src/gitcmd.rs:392-395` — `repack()` replaced by
  `pack_fold(inputs, out_base)` and `pack_base(out_base)`
  (`pack_new_objects`, `:354-387`, generalised with an output base);
  `reflog_expire_all()`.
- `forge/syncer/src/server.rs:414-424` — `maybe_repack` behind
  `fold_factor == 0` (the control) else `fold::commit_if_ready` then
  `fold::maybe_spawn`; a `select!` arm on the fold's oneshot; the tick
  (`:228`) runs `commit_if_ready`, `unlink_retained`, `sweep_ledger`
  (capped), `maybe_spawn`, and the full sweep when due and no fold is
  in flight; the SIGTERM arm (`:203-213`) aborts the task and removes
  the scratch.
- `forge/syncer/src/sweep.rs:43,57` — the in-flight guard; the listed-
  age prefilter before the HEAD at `:121`; the header paragraph.
- `forge/syncer/src/lib.rs:150-156, 203` — `fold_factor`,
  `base_tier_percent`, `base_min_bytes`, `base_rebuild_min_secs`,
  `fold_retain_secs`, `fold_stall_secs`, `sweep_every_secs`,
  `fold_min_bytes`, `fold_max_packs`; `Syncer.fold`;
  `forge/syncer/src/bin/flint_forge_syncer.rs:129` the env names
  (`FLINT_FORGE_FOLD_FACTOR`, `…_BASE_TIER_PERCENT`, `…_BASE_MIN_MIB`,
  `…_BASE_REBUILD_MIN_SECS`, `…_FOLD_RETAIN_SECS`, `…_FOLD_STALL_SECS`,
  `…_SWEEP_EVERY_SECS`); `FLINT_FORGE_REPACK_THRESHOLD` read for the
  control and logged as ignored after phase 4.
- `forge/syncer/src/status.rs:25-37, 79-96, 151` — `Phase::Sweeping`
  retired; `Facts` gains `base`, `tier_packs`, `tier_bytes`,
  `retained`, `fold`.
- `spdk-csi-driver/src/forge_operator/render.rs:333-480` — plumb the
  env; the ephemeral-storage request from G7; `crd.rs:391-393` the
  doc comment ("rewritten WHOLE by `repack -a`") rewritten for tiers.
- Tests: `tests.rs:934-963`'s
  `the_repack_publishes_the_new_pack_and_the_sweep_takes_the_old_ones`
  becomes `a_fold_publishes_the_rolled_pack_and_the_sweep_takes_its_inputs`;
  new: `a_batch_during_a_fold_names_the_old_packs_and_never_the_fold_pack_before_its_cas`
  (the memory double delays the fold's PUT; control removes the
  rename-before-CAS order), `a_fold_whose_name_is_an_inputs_never_unlinks_it`,
  `a_fold_cas_names_the_snapshots_packs_not_the_directory` (a refused
  push's pack on disk; control: the `local_packs()` formula names it
  and the cold restore refuses), `a_base_rebuild_after_a_rewind_survives_a_warm_restart`
  (control: no `--no-reflogs`), `the_fold_never_ticks_the_holds_counter`
  (virtual time; control ticks it and the renewer keeps renewing a
  stalled batch), `the_full_sweep_refuses_while_a_fold_is_in_flight`,
  `the_ledger_sweep_deletes_only_past_the_grace_and_only_unnamed`.

**Phase 3 — the model.** `formal/ForgeSync.tla` per §5.3; six new
`.cfg` files; `scripts/check-tla.sh:1157-1162` gains six
`mutation_run` lines and the strict run's description names
`Inv_NamedIsUploaded` and `Inv_NoRenewOverWedge`. The strict run's
state space grows by the fold stages, bounded by `MaxFolds = 1` and
`FoldIds = {f1}` in `ForgeSync.cfg`.

**Phase 4 — measurement, then the control's removal.** `run-repack.sh`
+ `amplify.py` (§8.1 item 1); the latency leg (item 3); G8's re-match
(items 2); then delete `maybe_repack` (`restore.rs:208-250`),
`Git::repack`, `repack_threshold`, and update design §3, §5, §8 item
1, §10, §13 (falsifiers 13-22) and the simplification note's X18 row.

**Phase 5 — contingency, only if F19 fails.** A LOCAL multi-pack
index over the cold prefix (base + tiers above `base/8`), written
after Serving at restore and after a base rebuild or a fold that
touches a covered pack — A's mechanism at the cadence of the top tier,
never in the bucket, deleted before the proof at every start (§4.1).
Its cost is 2.6 µs/object per rewrite off the readiness path; its
gate is the linux.git timing on the x86 build node. Nothing in §2 or
§5 changes for it.

## 11. Refutations weighed

Accepted (and where the design answers each):
- A/B: a fold CAS built from `local_packs()` names packs never uploaded (a refused push's) — the CAS is `(snap.packs \ S) ∪ {F}` (§3.4 step 4; `FoldCasFromDisk`).
- A: a rebuild can reproduce an input's name and the "delete superseded" rm would unlink the base — F ∈ S is guarded and F is never unlinked (§3.4 step 2, §7.6).
- A/B: a reachability rebuild without `--reflog` makes a warm restart's `fsck` refuse — `reflog expire` before the rebuild and `--no-reflogs` on the proof (§7.7, F20, G4).
- A/B/C: a fold ticking the hold's counter renews a wedged batch — the fold ticks its own counter (§3.4 step 3, F21, `FoldTicksBatchSensor`).
- A/C: `abort_orphaned_uploads`'s premise is false during a fold — both sweeps are guarded by "no fold in flight" (§5.1, `SweepDuringFold`).
- C: the LIST sweep at the commit scales with fold cadence × grace and sits on the push path — the ledger sweep, the prefilter, the hourly full sweep (§5.1, F18).
- C: the grace-0 control for the fold cannot fail — recorded as a documented non-run; what the grace protects is stated (§5.1).
- C: the fold's CAS must carry `pending_bundle` / `pending_exported_commit` — `carry_pending` (§3.4 step 4).
- A/C: `objects/info/packs` is stale after a tick-driven fold — generated from the snapshot's list at every commit (§3.4 step 6).
- C: a non-412 CAS error must not be "deferred" — re-read once, adopt or exit (§3.4 step 4).
- C: "the base is the bitmapped pack" and "S excludes the largest" diverge when a push is larger than the base — the base is the `.keep`'d pack and a tier fold never includes it (§2, §7.14).
- A/B/C: the amortised arithmetic — every draft's bound was wrong; the simulation is the number (§3.6, §8).
- A/B/C: the 30-push 96 MiB rig cannot separate the arms — re-sized (§8.1).
- C: the fold's `--stdin-packs` is not I/O-bound — `pack.window=0`, `pack.threads` bounded (§3.3, G6).
- C: the un-bitmapped tiers cost 66 ms/MiB of blob per clone — `core.bigFileThreshold`, F19, the phase-5 contingency (§0, §6, §8).
- A: a wall-clock fold timeout is a livelock on a large repository — a progress-gated stall detector (§3.4 step 4).
- B/C: local unlink of superseded packs under readers is git's `repack -d` race at 24× the frequency — retention with the batch subtracting it (§3.5, G5, F22).
- B: `pack.useBitmapBoundaryTraversal` is about negated tips — not set; an experiment (G3b).
- A: `.bitmap` on a pack and a midx bitmap coexist silently — moot without a midx; noted for phase 5.
- B: git 2.45 vs 2.50.1 differ in the stale-midx clone path — every git fact is gated on the image (G1).
- C: `kill_on_drop` on the fold's child — the task's abort kills `pack-objects` (phase 2).
- C: the model's `Restore` already assumes the prune — the reconcile brings the code to the model (§4.3).

Rejected (and why):
- B's "A beats nothing here: the bitmap walk at restore" — at fleet scale A's local write is < 10 % of a restore; A was rejected on the rewrite CADENCE under load, not on restore time.
- B's "C cannot use `repack --geometric` with a bitmap present" — `--no-write-bitmap-index` passes; C does not use `repack` for the reason that holds (no output directory, §6).
- B's "a midx that maps objects to an absent pack makes a clone fail" — the fsck half reproduces, the clone half did not on 2.50.1; irrelevant without a midx.
- A's `RacyGrace` and `NoRevalidate` mutations — vacuous against a sweep that runs after the fold's CAS on the same loop; not registered.
- The A draft's smallest-first restore order as an X14 gain — true of the layout, but X14's code decides; the restore here sorts largest-first for the tail and says so (§4).
- A's `fold.json` written before the rename — under C nothing is resumed; the reconcile at start and a deterministic re-fold replace the resume file (§7.1-7.3).
- B's fold threshold of 8 packs and its base cap at 3600 s — the cap is kept (`base_rebuild_min_secs`), the threshold is git's rule (§3.1).

## 12. Built — 2026-09-06, the same session

**Phases 1 and 2 are code** (X20 beside them, the batch collector
draining what queued at window 0 — three tests on virtual time):

- `fold.rs`: `plan` (git's split over bytes, the base rule, the cap
  and the floor), `maybe_spawn`/`spawn` (the task: `pack_fold` or
  `reflog expire` + `pack_base` into `<state_dir>/fold/`, then every
  sibling uploaded at fanout 1 on the fold's OWN counter, a fence check
  before each PUT), `check_stall`, `abort`, `commit` (§3.4 steps 1–7
  as written: the reproduced-name guard, renames with the index last,
  ONE CAS from `(snap.packs \ S) ∪ {F}` carrying the pending bundle
  and export, a non-412 error re-read once then adopted or fatal,
  retention, the ledger, `objects/info/packs` from the snapshot),
  `unlink_retained` (skips any stem carrying a `.keep`), `sweep_ledger`
  (capped, the snapshot read once, HEAD-age at the delete),
  `load_state` (the retained set, the ledger, the scratch wiped, a
  stray midx removed), the base marker helpers, `facts` for `/status`.
- `server.rs`: the fold reports on an mpsc the loop owns (no borrow of
  the syncer in the select); the post-batch hook plans when
  `fold_factor > 0` and runs the shipped `maybe_repack` + sweep when it
  is 0 (the control); the tick runs the stall check, retention's
  unlinks, the ledger sweep (64 requests), the plan, and the full LIST
  sweep when due and no fold is in flight; the start-up runs one full
  sweep after the restore; SIGTERM aborts the task.
- `restore.rs`: `fold::load_state` first; the fetch units largest
  first; the pack reconcile (unnamed and not retained ⇒ unlinked, index
  first); the base marker from the LIST's `.bitmap`; the proof with
  `--no-reflogs`. `PackObject` carries the listing's age.
- `batch.rs`: `listed_packs()` at steps 4 and 5; `carry_pending`;
  `publish_derived` writes `objects/info/packs` from the snapshot.
- `sweep.rs`: both sweeps refuse with a fold in flight; the listed-age
  prefilter before the HEAD.
- `gitcmd.rs`: `pack_fold` (window 0, threads bounded), `pack_base`,
  `reflog_expire_all`, `siblings_in`; `core.bigFileThreshold=1m` in
  place of `repack.writeBitmaps`; `repack()` kept for the control.
- knobs (`lib.rs`, the binary's env): `FLINT_FORGE_FOLD_FACTOR` (2; 0 =
  control), `BASE_TIER_PERCENT` 50, `BASE_MIN_MIB` 64,
  `BASE_REBUILD_MIN_SECS` 3600, `FOLD_RETAIN_SECS` 900,
  `FOLD_STALL_SECS` 300, `SWEEP_EVERY_SECS` 3600, `FOLD_MIN_MIB` 0,
  `FOLD_MAX_PACKS` 64.
- `/status.repo` gains `base`, `tierPacks`, `retained`, `fold {stage,
  bytes, inputs, base}`.

**Tests** (`fold::plan_tests`, `tests.rs` "compaction tiers"): the
planner's properties (a progression is left alone — the control for
every other case; equal packs fold; the split extends while the heavy
half is dominated; the base is never an input; the base rule fires at
the percent and only when allowed; the floor for a fresh repository;
the cap; factor 0 plans nothing; 48 uniform pushes fold at every second
push with ≤ 8 tiers and a largest roll-up of 256 MiB); a fold end to
end with the ledger sweep, retention's unlink and a cold restore; the
CAS names the snapshot's packs and not a stray on disk (the control by
construction: the stray is not in the bucket, and a cold restore of the
result passes); a base rebuild that reproduces the base's own name
unlinks and unnames nothing and spends no CAS; **the reflog trap with
its control on git 2.50.1** — a rewind, a rebuild WITHOUT the expiry,
its inputs unlinked: the plain proof refuses ("invalid reflog entry",
rc 2) and `--no-reflogs` passes on the same state; then the design's
path and a warm restart that serves; the fold's own counter moves and
the hold's does not; both sweeps refuse mid-fold and run after the
commit; the ledger sweep inside and past the grace; the restore's
reconcile keeps a retained pack and a named one and unlinks the stray;
a batch after a fold never re-names a retained pack, and
`objects/info/packs` is the snapshot's list. 106 tests in the crate,
clippy clean.

**Gate G4 is settled on 2.50.1 by the test above** (the design's
prediction held: rc 2 / rc 0 / rc 0 after `reflog expire`); one rig
lesson on the way: the rig's staging leaves loose copies a push never
does, and with them the trap is masked — `prune-packed` first.

**Not built here:** phase 3 (the model's `holds` relation and six
mutations), the batch-beside-a-fold test that needs a PUT delay in the
memory double, the operator's ephemeral-storage request (G7), the
`.keep`-onto-retained race (§7.5 c, G5). The control rule stays in the
binary until G8 has run.

**Measured locally** (`forge/e2e/repack/run-repack.sh`, the `tiers-*`
arms added; MinIO on the laptop, 256 MiB blob seed and 100 pushes of
2 MiB — the 512 MiB the design named was more than the machine's free
disk that night; `forge/e2e/results/repack-tiers-2026-09-06.log`):
blob shape, shipped rule 8.5× (four whole-repository uploads of up to
450 MiB) against tiers **7.8×** (55 folds, 2 base rebuilds of which
the first is the fresh repository's base creation, largest upload
405 MiB); source shape, shipped rule 83× against tiers **32×** (62
folds, no base, largest upload 0.7 MiB). Both tiers arms are above the
ceilings §8.1 pre-registered (5.5× and 25×) because this rig's window
holds the first base creation and one rebuild, which the simulation
excluded; F14 as written is FAILED at this rig size and the rule is
kept for the 512 MiB run.

**Measured on the wire — G8, runca, 2026-09-06** (`forge/e2e/walgit/README.md`
"The re-match"; the simplification note §9.2): P9 at 48 pushes, wall
1,021 → 36 s and the worst push 816 → **0.83 s** (F13 and F15 hold:
no push waited, no base rebuild inside the window); P9 at 300 pushes,
worst push 5.8 s (F13's 5 s bound missed by 0.8 s, once); P2 1.1 →
**14.1 pushes/s** with a 1.86 s median (the ≥ 5/s and ≤ 10 s
pre-registered, met); P7 solo 1.24× at the mid-ladder state and 0.93×
after the base rebuild (F19's 1.5× holds); P5 unchanged in kind
(51 s to refs); 97 folds and 1 base rebuild committed, 2 folds failed
inside an S3 cut and were retried, 0 stalls. **Bytes over run A:
47.8 GB for ≈ 6.8 GB pushed (7.0×) against walgit's 16.5 GB (2.4×)** —
F14's P9 bound (≤ 2.0 GB) is not met: the 48-push window carried
3.08 GB, of which most is the tail of a fold that began before it;
the 300-push window, tier folds only, carried 12.27 GB for 2.52 GB
pushed — **4.9×**, the simulation's 4.5–4.8× for this regime — against
walgit's 4.41 GB (1.75×).

**Three defects the wire run found, each a rule in this document:**

1. `base_rebuild_min_secs` is not persisted: `last_base_rebuild_unix`
   is process memory, so the pod that P5 restarted rebuilt a 12 GiB
   base at once. Fix: derive the cadence from the base pack's age in
   the LIST (`ListedObject.last_modified_unix`) at restore.
2. The cap (`fold_max_packs`) folds EVERY tier when it trips (§3.1
   "fold regardless"); at P2's 14 pushes/s it tripped once per batch
   and rewrote the 300 MiB P9 tiers for tiny pushes, 885 MB in a
   minute. Fix: the cap folds the light half only (git's split with
   the count as the weight), never a pack above `fold_min_bytes` × the
   factor.
3. Pushes the size of the repository climb the ladder at full price:
   ten 1 GiB pushes cost about 20 GB of folds (2.3, 4.3, 6.7, 2.0 and
   3.1 GiB roll-ups). The simulation's shapes had no such push. Fix
   candidates, to be simulated first: a `fold_min_bytes` floor that
   exempts packs above a fraction of the base from tier folds until
   the base rebuild takes them; or a size-aware factor.
