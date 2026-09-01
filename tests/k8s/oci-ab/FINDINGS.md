# oci-ab campaign findings

## F-OCIAB-1 (2026-09-01, runbw, flint 1.43.0): the WRITING client's cache
## reads back NULs for a small file another client reads correctly

**Severity: shipped-defect class (silent zeros, client-side, durable).**
Falsifies the image-serving design's rung-0 claim ("stock Distribution on an
RWX pNFS PVC works unmodified") for the PUSH path until fixed. Serving reads
from other clients are unaffected by the observed instance.

**Observed:** CNCF Distribution v3.1.1 (`registry:3`, filesystem driver) on a
flint-pnfs RWX PVC, chart/images 1.43.0, kernel client `vers=4.2, nconnect=4`
(mount line in evidence). Every concurrent blob upload's completing PUT
answers 500: "error resolving upload: parsing time \x00×20". The upload's
`_uploads/<uuid>/startedat` file (exactly 20 ASCII bytes written by the
registry itself ~40 ms earlier) reads back as **20 NUL bytes through the
writing pod's own mount — durably, minutes later** — while a fresh pod on a
different node reads the **correct bytes** from the same PVC:

    registry pod:  size=20, od: \0 ×20
    fresh client:  size=20, od: "2026-09-01T16:57:58Z"

So the server holds the right data; the writing client holds a poisoned zero
page that nothing ever invalidates.

**Repro shape:** NOT a bare write-then-read (50 sequential + 20 concurrent
probes in the same pod all read correctly). The failing pattern is
Distribution's upload flow: POST writes `startedat`, then PATCH/PUT handlers
re-read it within milliseconds while large layer-data writers stream in
parallel on the same mount. Reproduced 2/2 pushes (first: many concurrent
uploads, most zeroed; second: two uploads, one zeroed one correct).

**Working hypothesis** (unverified): a read of the just-written range races
the write's visibility, instantiates a zero page at the client, and the
server's change attribute does not advance (or not in an order the client
observes) past the value cached with the zero page — so revalidation keeps
the poisoned page forever. Same family as the 1.43.0 "LAYOUTCOMMIT moves the
change attribute so DS writes are visible" fix, but on the writing client's
own read-back; possibly the sub-stripe/MDS write lane.

**Evidence preserved** (cluster is ephemeral): scratchpad `oci-ab/evidence/`
— registry-flint.log (the 500s + upload timeline), mds.log (157 lines in the
failure second), ds-0/1/2.log, mount-options.txt. Failing upload uuid for
correlation: `01a05de7-f92e-7ac3-bc9a-4106967dd091` at 16:57:58Z; good
sibling `...7dd6` same second.

## F-OCIAB-2 (2026-09-01, runbw, flint 1.43.0): page-aligned ZERO HOLES in a
## large read, cached permanently by one client while another reads correctly

**Same defect family as F-OCIAB-1, read-side face — and the pair now points
at one mechanism.** Layer blob `9e54f8…` (25,958,969 bytes, written to the
RWX PVC by an `aws s3 sync` pod seconds earlier):

    S3 object:                sha 9e54f8… (correct)
    fresh busybox pod, PVC:   sha 9e54f8… (correct, full size)
    registry pod (serving):   sha 0914e3… — full size, but a 16 KiB zero
                              hole at offset 15,728,640 and three 4 KiB
                              zero holes near the tail (page-aligned)
    same pod after restart:   sha 9e54f8… (correct)

The corrupt view was STABLE across reads (nerdctl pull failed digest
verification; curl re-fetch hashed identically wrong) and healed only by a
remount. The serving pod's mount was created AFTER the sync completed, so
the zero pages were captured during its own first reads — a read racing the
just-written ranges' visibility returns zeros, the client caches them, and
nothing afterwards invalidates the pages.

**Hypothesis (unverified, checkable):** the 1.43.0 counter-validated attr
cache (the small-file wire-parity work) can serve a pre-write change
attribute for a window after writes land via the DS/LAYOUTCOMMIT path, so a
client that cached zero pages inside that window never sees a change-attr
move and never revalidates. That single mechanism explains both faces:
F-OCIAB-1 (writer's own 20-byte read-back, NULs forever) and F-OCIAB-2
(reader's holes, permanent until remount). Check: attr-cache
invalidation ordering vs LAYOUTCOMMIT/fallback-lane writes.

## LOG-EVIDENCE PASS (2026-09-01, against the preserved logs) — both
## original hypotheses are damaged; the timeline points at a third

Run against `evidence/mds-1718.log.gz` (5,696 lines, 16:50:14–17:20:12,
covering the whole incident). **Three vacuity traps caught first — two
proposed discriminators cannot be used:**
- The MDS logs at **INFO**: there are ZERO debug-level lines in 30 minutes
  (the two "DEBUG" string hits are the words inside `WARN ... STARTING WITH
  DEBUG LOGGING` / `MDS SERVER BINARY VERSION: DEBUG BUILD`). So "no `🔁
  READ through MDS for striped` debug line" proves nothing.
- `READ`/`WRITE` op lines never appear at INFO **anywhere in the log**, not
  even during the known-heavy 16:55–16:57 push. So "no MDS-side READs in the
  window" proves nothing either. (The single `READ` regex hit is the
  substring inside "ALREADY".)
- Informative negatives that DO hold: WARN is emitted (385 lines), and there
  are **zero** `⛔ refused/failed fast` warns — so no read took the refuse arm.

**REFUTED — MDS restart/reload window:** the MDS ran continuously; every
single minute from 16:50 to 17:20 carries log lines, one startup banner
only (16:50:14, when its PVC bound). No reload gap exists to blame.

**REFUTED for the corrupted read — the fail-open `Serve` (sparse-stub) arm:**
the failing read **was granted a layout**. Blob `9e54f8…` timeline:

    17:17:45.900  📌 Pinned placement + 📄 LAYOUTGET granted   (writer: s3-sync temp file)
    17:17:46.311  Placement re-keyed for rename                (temp `data.d799be87` → `data`)
    17:18:07.251  📄 LAYOUTGET granted                          (reader: the corrupted read, +21s)

A granted LAYOUTGET means the placement was found and the client read the
DSes directly — the MDS stub path was not in play for those bytes.

**NEW LEAD — cross-client visibility of DS-written stripes after a rename:**
the corrupted file was written by ONE client under a temp name, renamed, and
read 21 s later by a DIFFERENT client that held its own layout, and some
stripe ranges came back as zeros (hole at **15,728,640 = exactly 15 MiB**,
plus 3×4 KiB near the tail). Supporting: **zero LAYOUTCOMMIT lines appear
anywhere in the 30-minute log** — either the op is never sent by these
clients or it is never logged; that must be settled in code before the lead
is trusted, since LAYOUTCOMMIT is precisely what publishes DS-written data
to other clients (and 1.43.0's changelog names it: "LAYOUTCOMMIT moves the
change attribute so DS writes are visible"). DS logs show normal window
activity (1.5–1.7k lines each) with no errors, short reads, or sparse
warnings — consistent with DSes serving what they hold.

**CODE PASS — the LAYOUTCOMMIT observation above is ALSO vacuous
(verified):** in `handle_layoutcommit` (`nfs/v4/dispatcher.rs:4012`) the only
`info!` is inside the `if scsi` branch (~:4113, gated on
`layout_class_for(&key) == LayoutClass::Scsi`). The registry PVC is
files-class, whose path emits `debug!` only ("clamping…", "extended … → …")
around its `set_len`. At INFO a files-class LAYOUTCOMMIT is *completely
silent* whether it ran a thousand times or never. Logs cannot answer it;
don't re-spend time there. (Credit: flint-87 raised it; verified here.)

**And LAYOUTCOMMIT almost certainly DID run, by a size argument that needs
no logs:** the MDS stub is the size authority for a striped file — DS writes
never advance it, `set_len` in the files-class LAYOUTCOMMIT path does. Every
reader, including the corrupted one, saw the full 25,958,969 bytes. A stub
still at its birth size would have shown an empty file, not a 25 MB file
with four holes. So the size was published *before* the corrupted read.

## ~~★ ROOT CAUSE (evidence-complete)~~ — **REFUTED, TWICE OVER. DO NOT
## READ THIS AS THE ANSWER.** Refuted first by the settled-fleet
## discriminator below (both MDS lanes metered zero during a reproducing
## run — this was correlation), and superseded by the settled root cause
## at the END of this file (the LAYOUTGET stripe width). Kept only as the
## record of a wrong turn: F68's silent MDS data lane meets a
## FAIL-OPEN fallback, and a striped file's sparse STUB is served as data

**The smoking gun was in the log all along: the F68a meter.** It fired 12
times, and its timing lands exactly on both corruption events:

    16:55:44 16:56:14 16:57:14 16:57:44 16:58:14 16:58:44   ← F-OCIAB-1 window
                                                              (the failing PUTs / NUL read-back)
    17:01:44 17:04:14 17:06:44 17:07:14
    17:18:14 17:19:14                                       ← F-OCIAB-2 window
                                                              (the holey 25MB read at 17:18:07)

Text: `🚨 F68a: client DATA is flowing through the MDS with 3 Active DS(es)
— a healthy pNFS client does this ~never (F68 signature)`. The meter is not
a guess: `dispatcher.rs:1837-1842` calls `m.served_read(res.data.len())`
only when the MDS **locally served** a READ with `Nfs4Status::Ok` (and the
mirror at :1961 for WRITE). So client data really was crossing the MDS in
both windows.

**What a locally-served READ returns on a striped file:**
`stub_io_disposition` (`dispatcher.rs:2743`) is **fail-open** — it returns
`D::Serve` on *four* paths before it ever consults placement (no
pnfs_handler :2747, no current_fh :2751, `resolve_handle` error :2755, empty
file_key :2764), plus whenever `fallback_io_disposition` says `Serve`. And
`Serve` on a striped file means the MDS reads **its own stub**, which is
sparse by construction — the bytes live on the DSes. Result: **ZEROS
returned with NFS4_OK**, no error, no short read, clean DS logs.

That is the complete signature, both faces:
- F-OCIAB-1: a 20-byte `startedat` read back as 20 NULs — a stub read.
- F-OCIAB-2: page-aligned holes inside a 25 MB blob — partial stub reads
  interleaved with real DS reads.

**CORRECTION — my "REFUTED: Serve arm" verdict above was WRONG.** I refuted
it because the reader had been granted a layout; but a layout grant does not
mean *all* I/O goes to the DSes — a client can hold a layout and still send
data ops down the MDS lane, and F68a proves it did exactly that in both
windows. Credit: flint-87 proposed this mechanism first; I dismissed it on
bad grounds. What survives from my side is the no-writer argument, which now
*corroborates* the mechanism rather than competing with it: the DS stripes
were complete and healthy the whole time, which is precisely why the server
data was intact, why a fresh mount read correctly, and why a remount healed
— the zeros came from a *different file* (the stub), not from damaged data.

**The write mirror, in the code's own words — and it implies silent data
LOSS, not only corruption.** The WRITE meter at `dispatcher.rs:1961` is
commented "locally-landed WRITE on an MDS = client data crossing the MDS
**(and a stub going dense)**". So the true statement of the defect is a
**lane split**: when a data op takes the MDS lane on a striped file it
touches the *stub*, while that file's real bytes live on the *stripes*.
- read via MDS lane, data on DSes → **zeros returned with OK** (observed,
  both faces).
- write via MDS lane, readers using layouts → the bytes land in the stub
  and are **invisible to every layout reader** — silent write loss, the
  mirror image, equally unerrored. Not observed here only because our
  failing reads happened to be the visible half; the same trigger produces
  it.

**Refining the fix (framing from flint-87, worth keeping):** the four
pre-placement fail-opens are the *more dangerous* half. A `Serve` verdict
from `fallback_io_disposition` at least consulted the placement table and
got an answer; the other four return `Serve` **before asking**, on
conditions that are transport hiccups (no current_fh, a `resolve_handle`
error) rather than statements about the file. On an ordinary file that is a
correct conservative default; on a striped file it silently substitutes a
sparse stub for the data. **Those two cases want different defaults and
today they share one** — that, not the placement-consulting arm, is the
change to make first.

**Severity upgrade:** F68 has been catalogued as a *performance* anomaly
(client data needlessly crossing the MDS). On a striped file it is a
**correctness** bug: the fallback serves a sparse stub as if it were data.
Fix direction — for any file that has a placement, `Serve` is never a
correct disposition; the striped-file fallback must refuse (or proxy to the
DSes), and the four pre-placement fail-open returns must not apply to
striped files. Whatever still triggers the MDS lane (F68c, open) then costs
throughput instead of silent corruption.

## Superseded reasoning, kept for the record: "the corruption is READ-SIDE"

The writer was an ephemeral `kubectl run --rm` sync pod, and it was **deleted
before the corrupted read**:

    17:17:45-47   writer writes temp file, renames, LAYOUTCOMMIT publishes size
    ~17:17:50     writer pod deleted  ← no client holds this file open after here
    17:18:07      registry pod (FRESH mount, restarted after the sync) reads → 4 holes
    later         fresh busybox pod reads the same path → full size, CORRECT hash

Between the bad read and the good read **no writer existed**. Nothing could
have filled server-side holes in that interval (a later `set_len` extends,
it never fills content). Therefore the DS bytes were already complete at
17:18:07, and the zeros were manufactured by the *reading* client. This
refutes the "DS files genuinely had holes / write-side durability" reading
of the evidence: the write side is exonerated by the absence of any writer
in the healing interval.

**Remaining suspects, all read-side:** the client's own first-touch read of
freshly-renamed stripes instantiating zero pages that are then cached with a
change attribute that never moves again (my original shape, but on the
layout/read path rather than the attr cache proper); or a
per-mount/per-open placement-to-stripe resolution that transiently reads the
wrong or an unwritten range. Both are read-path, both are consistent with
"remount heals" and with the DS logs being clean.

**Next test (cheapest decisive), adapted from flint-87's proposal:** after
the writer finishes and renames, read the file from a THIRD mount *before*
any other reader touches it, and hash it. Given the argument above the
prediction is that it hashes CORRECT — which localizes the bug to whatever
the corrupted reader did differently (fresh mount, first touch, concurrent
readers). Then vary one factor at a time: fresh-mount-vs-warm, immediate-vs-
delayed, single-vs-concurrent readers.

## ⚠ FIELD VERIFICATION OF THE FIX (runbx, 2026-09-01): RED reproduced,
## **GREEN FAILED** — commit 351314d3 does NOT close F-OCIAB-1 in the field

One cluster, one variable (the flint-pnfs image), the registry workload that
broke runbw. Evidence: `evidence/runbx-*`.

| leg | MDS+DS image | result |
|---|---|---|
| RED | `flint-pnfs:1.43.0` (stock) | **corruption reproduced** — `crane copy` dies with `parsing time "\x00"×20`, i.e. the registry's 20-byte `startedat` read back as 20 NULs. Identical signature to runbw. |
| GREEN | `flint-pnfs:f68-351314d3` (the fix) | **corruption reproduced identically**, 27 registry 500s |

**The fixed binary was definitely running** (this is the provenance check that
makes the result trustworthy): pod image digest
`sha256:a2b6d882…` matches the pushed build, and the MDS emitted **125 INFO
`📥 LAYOUTCOMMIT` lines** — the log line the fix ADDED, which on stock 1.43.0
is `debug!`-only and therefore invisible at INFO. New binary, confirmed by a
behaviour only it has.

**And the fix's gate never engaged: 0 `⛔` refusals.** The files were striped
(212 `LAYOUTGET granted` on `_uploads/` paths, LAYOUTCOMMITs firing on them),
so this is not "the gate saw a plain file". The corrupt op simply never took
the Delay/FailFast path the fix installs.

**★ THE DISCRIMINATOR (settled-fleet re-run, 18:43): BOTH MDS LANES READ
ZERO — which refutes the root cause this document previously asserted.**

    18:43:45  📊 F68a last 30s: served r 0op/0MiB w 0op/0MiB ·
              proxy r 0op/0MiB w 0op/0MiB · layoutget +219/-0 · layoutreturn 219
    refusals: 0        corruption: REPRODUCED (same 20-NUL signature)

`served_read`/`served_write` are called only in the Serve fall-through, and
the Proxy arm reports separately; both are zero. **No client data crossed the
MDS while the corruption occurred.** Therefore:
- the partially-dense-stub / Serve hypothesis (below) is DEAD;
- the proxy short-read hypothesis (`assemble_fallback_read` zero-filling) is
  DEAD;
- **and the "F68a is the smoking gun" root cause asserted earlier in this
  document does NOT survive.** On runbw the F68a WARNs fired during the
  corruption; here the identical signature occurs with the meter at zero on
  both lanes. That was correlation, not causation.

Confound caught and removed before reporting: the first GREEN push ran ~30 s
after the rolling image swap while DSes were still re-registering (two
variables). The run above is the redo on a settled fleet (3 active/3, zero
recent rejections) and reproduces identically.

**Status of the fix, precisely:** PARTIAL, and aimed at a path this workload
never takes. It closes a real unresolvable-handle hole (red-proven in its own
unit test), but the failing I/O goes client→DS directly under 219 layouts, so
nothing it guards sits on that path. Not failed; not sufficient; not
implicated.

**DS-SIDE EVIDENCE (runbx, fixed image, corruption reproducing) — the WRITE
side is clean, which kills the rotation hypothesis too.** Full numbers in
`evidence/runbx-ds-facts.txt`; debug-level MDS in `evidence/runbx-debug-mds.log.gz`.
- Stripe files live at `/data/{file_id:016x}.stripeN`, slot == DS index.
- **Zero byte-empty stripe files** on all three DSes ⇒ no "write never
  landed" case.
- **Rotation is exact: over 320 small single-unit stripe files,
  `stripe_index == file_id % 3` for 320/320, zero mismatches**, odd and even
  file_ids alike. The F66 precedent (one party dropping the rotation) does
  NOT reproduce here — there is no odd/even asymmetry because there are no
  mismatches at all.
- ⚠ **Withdrawn inference, recorded so it is not rediscovered as a finding:**
  `file_id 00c5635dd23ae781` has no stripe on any DS and its rotation points
  at ds-0, which looked decisive — but the surrounding lines show it belongs
  to a REMOVEd file ("Placement forgotten for deleted file" + "Stripe cleanup
  enqueued"). An id with no stripes *because it was deliberately cleaned up*
  is not evidence of a reader pointed at an unwritten id.

**✗ LIFECYCLE THREAD CLOSED (flint-87's code pass, independently verified
here) — every proposed mechanism is now eliminated.** The debug line's
wording ("Stripe cleanup enqueued for '<path>'") is misleading: cleanup is
**identity-keyed, not path-keyed**. `enqueue_stripe_cleanup`
(`pnfs/mds/layout.rs:1470`) builds its targets from
`placement.stripe_rel_path(slot)` — the retired placement's own
`{file_id:016x}.stripe{slot}` — and uses the `file_key` argument only in the
log message (:1487). And `allocate_file_id()` (:308) is
`Uuid::new_v4().as_u64_pair()`, so a recreated file at the same path gets an
id that has never existed and cannot name its predecessor's stripes. A late
async drain can therefore only delete stripes of a pin that is genuinely
gone. The mirror question — can a re-keyed placement outlive its stripes? —
fails for the same reason: rename moves the path key while `file_id` (hence
the stripe paths) is untouched, which is why rename is pure metadata for v2
pins. The one construction that WOULD break this is real but inert:
`generate_file_id(filename)` hashes the name, and its own doc warns of
exactly this collision — every caller is inside `#[cfg(test)] mod tests`
(`nfs/v4/filehandle_pnfs.rs:125+`); nothing in production mints ids that way.

**⇒ STATE OF THE HUNT: write side clean (0 empty stripes, rotation 320/320),
MDS lane never touched (served 0 / proxy 0), lifecycle clean by construction.
Every mechanism either session proposed is eliminated.** The next step needs
DATA, not another hypothesis: a wire-level capture of what the reading client
actually sent and received for the failing 20-byte read (which DS, which
offset, what came back). Nothing readable in this tree settles it. That is a
worthwhile use of a future cluster hour when someone has that specific
question — not a reason to hold one open now.

**(CLOSED) Earlier surviving thread — path-keyed lifecycle across rename/remove.** The debug
log shows the registry does not simply write `startedat`: it writes
`startedat.<uuid>.tmp`, RENAMEs it onto `startedat` ("Placement re-keyed for
rename"), and REMOVEs `startedat` during cleanup, which enqueues an
**async, path-keyed "Stripe cleanup"**. So one path is created, renamed onto,
read, and deleted repeatedly inside a single push, while stripe deletion runs
asynchronously behind a path key and placements are re-keyed by path. The
open question is lifecycle identity: can a cleanup enqueued for one
incarnation of a path remove stripes a later incarnation now owns, or can a
re-keyed placement outlive the stripes it names? Needs no cluster to reason
about.

**(WEAKENED by the 320/320 rotation result) Earlier hypothesis — sparse-layout stripe mapping (flint-87's
original, restored):** flint serves file layouts with `NFL4_UFLG_DENSE = 0`,
so each DS file uses LOGICAL offsets and the ranges owned by other DSes are
real filesystem holes in it. A reader that maps a byte range to the wrong DS
therefore gets **a legitimate sparse hole — zeros, no error, no short read**,
while the DS that actually holds the data is fine and the DS logs stay clean.
This fits every observation without the stub or the MDS lane: server data
intact, fresh mount correct, remount heals, page-aligned zeros. The 20-byte
`startedat` is the sharpest case — all of it lives in stripe 0, so a
writer/reader disagreement about rotation (`file_id % N`, deviceid, or stripe
count) is sufficient. **Blocked on instrumentation:** LAYOUTGET grant lines
carry only path + iomode + layout count, so writer-vs-reader grant comparison
needs a debug-level MDS or a new log line.

**(SUPERSEDED by the discriminator above) Leading hypothesis — the arm the fix deliberately left alone.**
`fallback_io_disposition_impl` (`pnfs/mds/operations/mod.rs:229`, core at
:264+) already carries a sparse-stub guard: it returns `FailFast` only when
`meta.len > 0 && meta.blocks == 0` — a *wholly* sparse stub — and otherwise
returns **`Serve`**. That guard cannot see a **partially** dense stub: a file
whose stub holds some blocks but is missing the ranges being read passes it
and is Served. That is precisely F-OCIAB-2's shape (a 25 MB file with four
holes) and is consistent with F-OCIAB-1 if the 20-byte stub is dense-but-stale
or zero-length. The fix hardened the four *pre-placement* fail-opens; this is
the *placement-consulting* arm, which flint-87 explicitly scoped out on the
reasoning that it "at least consulted the placement table and got an answer".
The field says the answer it gets can still be wrong.

**Status:** the fix is a real improvement (it closes a genuine unresolvable-
handle hole, proven by its own red test) but it is **not sufficient** — the
shipped field defect survives it. Not a regression; an incomplete fix.

## KNOWN-LATENT ADJACENT HOLE (not this defect — recorded so it isn't lost)

**`assemble_fallback_read` zero-fills a DS chunk that comes back SHORT or
EMPTY with `ok = true`.** Verified at `pnfs/mds/operations/mod.rs:1270`:
`if data.is_empty() { continue; // hole — stays zero }` (:1279-1280) and
`let n = data.len().min(want - start)` (:1286), which copies only what
arrived and leaves the remainder of `out` at its initialized zero. Errors,
timeouts and `ok = false` all propagate correctly — this is strictly the
"succeeded but returned less than asked" case.

**Not implicated in F-OCIAB-1/2** (the proxy counter was zero on every
reproducing run), so it is neither this campaign's defect nor the
delegations campaign's to fix. But it is **the same zeros-with-NFS4_OK shape
this investigation spent hours chasing**, on the proxy path rather than the
stub path, and it will produce an identical, equally silent signature the
first time a DS answers short. Small and independently testable: a unit test
that hands the assembler a short chunk and asserts a refusal rather than a
zero-filled buffer. Raised by flint-87, verified here, flagged to Dilip as
its own change.

## THE TRANSFERABLE LESSON — four vacuity traps in one investigation

Every one of them was a check that would have "confirmed" something by an
**absence** that could never have been present:
1. "no `🔁 READ through MDS` debug line" — MDS ran at INFO; zero debug-level
   lines exist in 30 minutes.
2. "no MDS-side READ/WRITE op lines in the window" — those ops are never
   logged at INFO *anywhere*, including known-heavy windows.
3. "zero LAYOUTCOMMIT lines" — the only `info!` on that path is inside the
   `if scsi` branch; the files-class path is `debug!`-only.
4. "compare the two LAYOUTGET grants field by field" — grant lines carry
   only path + iomode + layout count; no deviceid, stripe count, or
   first_stripe_index exists to compare.

**Rule earned: before proposing any test that confirms by ABSENCE, check the
emitter and the log level — could the thing you are looking for ever have
appeared?** Two of these traps were proposed by one session and two by the
other; taking either at face value would have closed the investigation on a
false negative. The sibling of the drill rule this repo already keeps ("a
leg that would pass if the feature were broken proves nothing"), for logs.

**Instrumentation owed for the rebuild** (this class is invisible at INFO):
run the MDS with the log LEVEL at debug (the "DEBUG BUILD" banner is about
the binary, not the level — they are different things, and this log had
zero debug-level lines); and the files-class LAYOUTCOMMIT arguably deserves
an `info!` of its own, since it is the operation that publishes data
visibility and its silence cost two hours of log archaeology here.

**Discriminator the debugging pass must answer first:** is the mechanism
change-attr-specific or cache-wide? The delegation workstream's grant/fence
sites read (dev,ino) identity through the same stat_cache — identity is safe
under a stale change attribute, but not under a cache-wide staleness; the
answer decides whether those call sites need a re-audit (flint-87 session,
2026-09-01).

**Blast radius beyond the registry (recorded from the delegation session):**
if the hypothesis holds, this is a coherence bug in exactly the invalidation
ordering delegations amplify — a delegation holder by definition never
revalidates, so a missed change signal becomes PERMANENT staleness rather
than an acregmax-bounded one. This finding is therefore a gating input for
the delegations slice-4 default-on argument, not just a neighboring bug
(the feature ships dark and stays dark regardless; nothing landed in slices
0-3 can grant delegations, and 1.43.0 predates all of it — so runbw's repro
is pure released-code behavior).

**Consequence:** the phase-1 A/B is BLOCKED at rung 0/A5 — a registry
serving corrupt bytes invalidates any perf comparison. That is itself the
campaign's primary result so far: 1.43.0 does not yet safely serve the
registry-on-flint workload; the design's rung-0 "works unmodified" claim is
falsified until this is fixed and the rig re-run green.

**Evidence:** preserved durably at `tests/k8s/oci-ab/evidence/` (gzipped
MDS/DS logs incl. mds-1718.log.gz covering the corruption window, registry
logs, mount-options.txt, and the SSM driver scripts). Rig-side repro is
cheap: push or sync content at the registry, read it back through the
writing/serving client under concurrent I/O.

**Campaign workaround for F-OCIAB-1 only** (defect documented, not hidden): push to registry-s3,
`aws s3 sync` the bucket's `docker/` tree onto the PVC (Distribution's S3 and
filesystem layouts mirror each other), restart registry-flint, serve
read-only. Arms A1/A5 measure the READ path, which is the campaign's
subject; the write-path defect is this finding.

---

# ★★ ROOT CAUSE — SETTLED (2026-09-01, flint-29, from the evidence already
# in hand): a bounded LAYOUTGET advertises the WRONG STRIPE WIDTH, which
# re-rotates the whole stripe map and points the client at a stripe file
# that does not exist

**This supersedes every hypothesis above.** It was found by re-reading
`evidence/runbx-debug-mds.log.gz` — the debug capture taken while the
corruption was reproducing — for the one thing the earlier passes said was
missing ("writer-vs-reader grant comparison needs a debug-level MDS"). The
capture had it all along; nobody had mined the encode lines.

## The mechanism

`generate_stripe_layout` (`pnfs/mds/layout.rs`) returned **two incompatible
shapes** under one name:

| request | returned | `segments.len()` means |
|---|---|---|
| `length == u64::MAX` (whole file) | one segment **per device** | the stripe width |
| bounded `length` | one segment **per stripe unit of the request** | a unit count |

`encode_file_layout_striped` (`nfs/v4/dispatcher.rs`) derived the wire stripe
width **and** the `nfl_fh_list` from `segments.len()`.

So a 4 KiB READ LAYOUTGET on a striped file — one stripe unit — produced a
layout advertising **width 1**, with
`nfl_first_stripe_index = wire_first_stripe_index(file_id, 1) = file_id % 1 = 0`
and a single filehandle naming `.stripe0`. GETDEVICEINFO meanwhile still
advertised all 3 DSes. The client therefore mapped unit 0 to **slot 0**
while the bytes lived on slot `file_id % 3`: an absent stripe file, read as
a legitimate sparse hole ⇒ **zeros returned with NFS4_OK**, no error, no
short read, clean DS logs. The client cached the zero page and nothing ever
invalidated it.

## The evidence, per grant, no interpretation required

From the 47 striped-layout encodes in `runbx-debug-mds.log.gz`:

- **41 grants at width 3** — every one advertises `fsi == file_id % 3`. Correct.
- **6 grants at width 1** — every one advertises `fsi = 0`. Four of the six
  mismatch `file_id % 3`; the other two had `file_id % 3 == 0` and agreed
  **by coincidence**.
- `file_id 00d4db4de5ab4bff` received **both** a width-3 grant with `fsi=2`
  **and** a width-1 grant with `fsi=0` — the same file, two grants, two
  contradictory stripe maps. That is precisely the invariant the encoder's
  own comment declares load-bearing: *"every LAYOUTGET ever issued for the
  file has to agree, or readers reassemble the stripes in a different order
  than the writer laid them down."*
- Every bounded grant in the log is `iomode=Read, length=4096`, and the only
  files that got one are **`startedat`** and three digest files — exactly the
  files observed reading back as NULs. Every write
  (`startedat.<uuid>.tmp`) was `iomode=ReadWrite, length=u64::MAX`: full
  width, correctly rotated.

That write/read asymmetry is the whole defect in one line: **writers took
the whole-file path and laid the bytes down correctly; readers took the
bounded path and were told a different map.**

**The layouts were also protocol-malformed, which widens the blast radius
beyond the 4 KiB case.** GETDEVICEINFO resolves the composite deviceid from
the pinned placement, so `nflda_stripe_indices` always carried N=3 entries.
RFC 8881 §13.4.2 requires `nfl_fh_list` to hold **0, 1, or exactly N**
filehandles. The bounded path emitted one FH per stripe unit of the request:
- a 1-unit request → 1 FH — legal, but "use this FH for every stripe",
  and that FH names `.stripe0`, so the client addressed the wrong DS *and*
  the wrong stripe file;
- a 2-unit request → **2 FHs against 3 stripe indices — illegal**, an
  undefined-behaviour layout whose interpretation is the client's choice.
That is the shape behind F-OCIAB-2's *multiple* page-aligned holes in one
25 MB blob, as opposed to F-OCIAB-1's single wholly-zeroed small file.

## Why every earlier elimination was right and still points here

Nothing above has to be retracted — each eliminated result is *predicted* by
this mechanism:

- `served_read`/`served_write` and the proxy counters all **zero**: correct,
  the I/O genuinely went client→DS direct. The MDS lane was never involved.
- **write side clean, rotation 320/320**: correct, writes always used
  whole-file RW layouts. ⚠ **And this is a FIFTH vacuity trap — the one
  that cost the most time.** That 320/320 was measured on DS *stripe
  files*, i.e. entirely write-side state. Writes never took the bounded
  path, so the sample could only ever agree; a perfect score there was
  guaranteed whether or not readers were being handed a divergent map.
  It was read as "the rotation hypothesis is dead" when it could not
  have discriminated. Same family as the four log traps, but on
  filesystem state instead of logs: *we checked the side that could only
  agree.* The discriminating artifact — a per-grant comparison of
  advertised width and rotation — was sitting in the debug capture
  taken the same afternoon. (Credit: flint-87 identified this about
  their own evidence.)
- **DS logs clean, zero empty stripe files**: correct, the DS honestly served
  a hole from a file that legitimately does not exist.
- **a fresh pod reads correctly; remount heals**: correct, a different grant
  sequence yields the right map.
- **1-in-3 files fine**: the `file_id % 3 == 0` coincidence.
- **the F68 fix neither helped nor hurt**: correct, it guards a path this
  workload never took.

## It is F66, one layer up

F66 was a rotation divergence between the fallback proxy and the wire,
disguised by the even-`file_id` cases that agreed by coincidence. This is the
same divergence between the **encoder and the placement**, disguised by the
`file_id % N == 0` cases. F66's fix made the proxy call the shared formula;
nobody checked what the *encoder* was deriving `N` from.

## Why the test suite missed it — two independent blind spots

1. `encoded_first_stripe_index` (the encoder's own test helper) **hand-builds
   a device-shaped segment list**. The encoder tests therefore never saw the
   bounded shape at all.
2. `test_layout_segments_for_striping` requested **24 MiB over 3 × 8 MiB
   DSes**, where unit-count == device-count. `stripe_size_pinned_per_file`
   picked 16 MiB/8 MiB and 2 MiB/1 MiB over 2 DSes — the same coincidence.
   Every test in the file chose a length that made the two shapes
   indistinguishable.

**The composition `generate_stripe_layout(bounded)` → `encode_file_layout_striped`
was never executed by any test.** That is where the bug lived.

## The fix

Stripe width now comes from the pinned placement, exactly as `stripe_unit`,
`device_id_bin` and `file_id` already did:

- `Layout.stripe_width` (`pnfs/mds/operations/mod.rs`) — set from
  `placement.device_ids.len()`.
- `encode_file_layout_striped` takes it explicitly and uses it for the
  rotation and the fh list; `segments` no longer reaches the wire at all. It
  warns loudly if the two ever disagree again.
- `generate_stripe_layout` always returns the pinned device set. The
  per-unit decomposition had no correct consumer: the wire's layout range
  comes from the REQUEST, and rotation-aware chunking (the proxy) uses
  `FilePlacement::split_at_stripe_bounds`, which rotates.
- `generate_roundrobin_layout` likewise. It returned a **1-segment list
  unconditionally**, so under `policy: roundrobin` this same encode bug fired
  on *every* file, not merely on bounded reads. Reachable from config today.

Two further consequences of the old shape, fixed by the same change:
`recall_layouts_for_device` matches on `seg.device_id`, so a bounded-path
layout was **invisible to the recalls of every DS but one**; and the
per-device layout counts double-counted a DS appearing in several units.

## Verification

- **Red control**: restoring the old bounded decomposition fails the new test
  on shape `(0, 4096)` — *"advertised stripe width 1 — the file is striped
  over 3 DSes"*. Removed after proving red.
- New `every_request_shape_yields_the_pinned_device_set` asserts every
  request shape (sub-unit, single byte, unaligned, whole-file) yields the
  same pattern, and that unit 0 sits on the rotated slot, never slot 0.
- New `wire_stripe_width_follows_the_placement_not_the_segment_count` hands
  the encoder the exact poisoned input (1 segment, width 3, a `file_id`
  with `% 3 == 2`) and pins rotation + fh count to the placement.
- Two existing tests corrected: `test_layout_segments_for_striping` (its
  "all segments use different devices" assertion counted the vector it had
  just built and would have passed with all three segments on one DS) and
  `stripe_size_pinned_per_file` (read the stripe size back out of segment
  lengths; now reads the placement).
- Full lib suite **2278 passed / 0 failed** on macOS. Linux gate pending.
- **Field verification still outstanding** — this is a code-and-log result,
  not yet a cluster result. The rig that reproduced it (`drive-ab.sh` +
  `f68-verify.sh`) is the gate.

## Correction: the `assemble_fallback_read` "hole" is NOT a small fix

Recorded above as a known-latent adjacent hole with a suggested unit test.
That framing was wrong and is withdrawn. The zero-fill is **deliberate,
documented, and pinned by four tests** as the sparse contract — 
`assemble_absent_stripe_is_all_zeros_not_error` asserts that an absent stripe
file reads as zeros by design, "the sparse semantics tar --sparse depends
on", and a short chunk is the *normal* case for a sparse tail. Making the
assembler refuse a short chunk would break sparse-file reads and fail those
tests. The genuine gap is narrower: `ReadStripeResponse` carries no stripe
EOF/extent, so the assembler cannot distinguish a legitimate sparse tail
from a DS that returned less than it held. Closing it needs a protocol field
on both sides — a real change, not a small one. Not implicated in
F-OCIAB-1/2 either way (proxy counters zero on every reproducing run).

## The gate: `stripe-width-gate.py`

The analysis that found this is now a script, so it is a check rather than
an anecdote. Point it at a **debug-level** MDS log:

    ./stripe-width-gate.py evidence/runbx-debug-mds.log.gz     # exit 1, FAIL
    ./stripe-width-gate.py <green-run.log.gz> --expect-width=3 # exit 0 = PASS

It asserts three things per LAYOUTGET — every grant at the pinned width,
`fsi == file_id % width`, and no file ever given two different widths — and
one anti-vacuity condition:

**INCONCLUSIVE (exit 2) is not PASS.** The defect only appears on *bounded*
grants, so a log containing only whole-file grants cannot exonerate a
server; it exits 2 and says so. Likewise, at INFO none of these lines exist
at all, and it reports blindness rather than success. A GREEN run must show
bounded grants **present** and all at full width — otherwise the workload
simply stopped asking the question.

Its own red control: run against `evidence/runbx-debug-mds.log.gz`, the
capture taken while the corruption was reproducing. It must report 6
width-1 grants (all `offset=0 length=4096 iomode=Read`) and 6 files holding
contradictory stripe maps. **Every single width-1 grant was for a file that
also held a width-3 grant** — there is no case where the narrow map was the
file's only map.

## Field verification protocol (not yet run)

One cluster, one variable, RED first:

| leg | image | required outcome |
|---|---|---|
| RED | `flint-pnfs:1.43.0` | `crane copy` fails with `parsing time "\x00"×20`; gate exits 1 |
| GREEN | build of this fix | push clean, blob digests verify; gate exits **0**, not 2 |

Three conditions that make the result mean something, each learned the hard
way in this campaign:
1. **Settle before pushing.** runbx's first GREEN was void because the push
   started ~30 s after the image swap while DSes were still re-registering —
   two variables. Wait for 3/3 active and zero recent rejections.
2. **Prove the binary.** At debug, a fixed MDS prints
   `Number of DSes in stripe: 3` for *every* grant, including the bounded
   `iomode=Read, length=4096` ones. On 1.43.0 those print `1`. That is a
   positive marker on the defect's own path, not an absence.
3. **Prove the path was taken.** The gate's exit-2 arm exists for this: a
   GREEN with no bounded grants proves nothing.

## ✅ FIELD VERIFICATION — runby, 2026-09-01: RED reproduced, GREEN PASSES

One cluster (5 × i4i.xlarge, all spot incl. CP), one variable (the
flint-pnfs image), the registry workload that broke runbw and runbx.

| leg | image | crane copy | `parsing time "\x00"×20` | gate |
|---|---|---|---|---|
| RED | `flint-pnfs:1.43.0` | never completed | **98** | **exit 1** |
| GREEN | `flint-pnfs:stripe-width-7fd7917b` | completed in 16–21 s, 136 blobs | **0** | **exit 0** |

RED's gate output, from a cluster provisioned an hour earlier — the
mechanism predicted from code reading, reproducing independently:

    widths seen   : {3: 41, 1: 10}
    ✗ 10 encodes NOT at width 3 — every one  offset=0 length=4096 iomode=Read
    ✗ 8 files given CONTRADICTORY stripe maps — widths [1, 3]

GREEN, same workload, after the image swap:

    encode blocks : 548 (541 pairable, 7 braided by concurrency)
    widths seen   : {3: 548}
    bounded grants: 95
    PASS — all 541 grants at width 3 with fsi == file_id % 3

**The four conditions that make this mean something**, each one bought
with a wasted run earlier in this campaign:
1. **RED reproduced here, today.** The control is live; GREEN's silence
   is informative rather than merely quiet.
2. **Provenance by digest, not by behaviour.** MDS and all three DSes
   report `sha256:910a0546…`, byte-identical to the pushed image.
   (runbx proved its binary by a log line, which worked but only because
   that line was reachable at INFO — a property four of our other
   discriminators lacked.)
3. **The path was exercised.** 95 bounded grants in GREEN. A clean log
   with no bounded grants is the gate's exit-2, not a pass.
4. **Settled fleet.** 3/3 DSes re-registered before any I/O, so the
   image is the only variable — runbx's first GREEN was void for
   exactly this reason.

### Three rig faults caught during the run, none of them the server

Recorded because each would have produced a confident wrong answer:

- **`du -sh` reported 2.1 MB for a 600 MB push.** The MDS stub is sparse
  by construction — the bytes live on the DSes — so `du` reports
  allocated blocks near zero while `ls -la` shows true sizes. Nearly read
  as "the workload never wrote anything". **Never size a pNFS file with `du`.**
- **Debug logging + a 600 MB push overran the container log**, rotating
  the LAYOUTGET lines away before they could be read. The first GREEN
  gate run returned INCONCLUSIVE for this reason and was *correct* to.
  Fix: `kubectl logs -f` streamed to a file **during** the push, never
  read back afterwards. 495k lines captured that way.
- **The gate itself produced a FALSE FAIL** — 3 rotation mismatches in
  526 grants. The MDS is concurrent and tracing writes lines
  independently, so two encode blocks braid together *within the same
  microsecond*; a forward scan then pairs one grant's
  `first_stripe_index` with another's `file_id`. Verified by reading the
  raw log: two `🔧 Encoding` blocks interleaved line-by-line. The gate now
  pairs only blocks that run open→close without a second open
  intervening, reports braided blocks (7 of 548 here) and never counts
  them as failures. **A gate that cries wolf under concurrency is exactly
  as useless as one that passes everything, and concurrency is when this
  defect matters most.**

### Also observed, NOT chased, NOT explained

`rm -rf /var/lib/registry/docker` fails with `Directory not empty` on
`_uploads`, repeatably, and survives a fresh mount. Could be readdir
cache, silly-rename residue, or a real unlink/readdir inconsistency.
Sidestepped by recreating the PVC. **Not investigated — recorded so it
is not mistaken for settled.**

## PHASE-1 A/B ON runby (2026-09-01): one narrow result, and the two
## reasons the headline question is still unanswered

**What was measured, and it is real:** eager pull, single client, cold on
BOTH sides, flint-backed registry vs S3-backed registry, everything else
held identical (same image, same snapshotter, same node, interleaved and
rotated per rep).

    paired per-rep ready_ms (A1 flint / A0 s3)
      rep1  20505 / 21132  = 0.970
      rep2  20170 / 21169  = 0.953
      rep3  20366 / 21034  = 0.968
      rep4  20383 / 20623  = 0.988
      median 0.969  → flint ~3.1% faster to ready

8/8 reps valid, zero voids, every rep digest-verified by G-INTEG. The
direction is consistent across all four pairs, but **3% is not a result
the design should lean on**: the design's claimed advantage is capacity
and request rate under a BOOT STORM, not bandwidth to one puller. A
single-client eager pull is the case where the backend matters least, and
the number is consistent with "the storage backend is not the bottleneck
for one client" — which says nothing either way about the storm case.

**The rig WITHHELD the headline, correctly.** `score` refuses to publish
while the substrate gate is not PASS, and the gate returned
`INCONCLUSIVE:gate-could-not-ask-the-question`: 84 encode blocks, all at
width 3, and **zero bounded grants**. A pull-only workload issues only
whole-file (u64::MAX) LAYOUTGETs; the bounded 4 KiB reads that carry the
corruption defect come from the registry's UPLOAD path. So this run cannot
exonerate the substrate — though G-INTEG's per-rep digest check is direct
evidence the bytes served were correct for this workload.
⇒ **Rig note:** a pull-only run can never certify via the width gate. Either
include a push in the run, or treat G-INTEG as the integrity authority for
pull-only workloads and say so explicitly.

### ✗ THE HEADLINE QUESTION IS STILL UNMEASURED — registry referrers

A3/A5 (the lazy/snapshotter arms, and the whole point of the snapshotter
rungs) could not be measured. SOCI discovers its index through the OCI
**referrers API**, and this Distribution 3.1.1 deployment answers
`GET /v2/<name>/referrers/<digest>` with a bare `404 page not found` —
over plain HTTP and over trusted TLS alike, with the subject manifest
present (200) and the registry advertising only `registry/2.0`. The index
itself is present and well-formed under SOCI's fallback tag
(`artifactType: application/vnd.amazon.soci.index.v1+json`), but SOCI's
read path does not fall back to it. Result: SOCI silently serves an
ordinary eager pull.

**This is a design constraint, not a rig bug.** The image-serving design's
rung-0/1 posture assumes stock Distribution; the snapshotter rungs
additionally require a registry that serves referrers. That requirement is
not currently recorded in `docs/plans/oci-image-serving-design.md` and
should be, because it changes what "works unmodified" means.

TLS was set up properly along the way and is worth keeping: CA + server
cert with IP SANs for both registry ClusterIPs, mounted into both
Distribution pods, CA installed in the node trust store
(`/etc/pki/ca-trust/source/anchors` + `update-ca-trust`), `certs.d`
pointing at `https://` with no `skip_verify`. That closed a real error
(`server gave HTTP response to HTTPS client` on SOCI's index fetch) — it
was necessary but not sufficient.

### THREE measurement defects found, each of which produced confident
### wrong numbers before being caught

1. **SOCI was never wired into containerd.** `node-soci-setup.sh` writes
   `/etc/containerd/oci-ab.toml` while the node imports
   `/etc/containerd/conf.d/*.toml` — a directory that did not exist. The
   nodes run containerd **2.2.5, config version 3**; the script targets
   "1.7 classic" and also uses 1.x plugin key names. 10 reps voided
   `G-PULL:pull-rc=1` before this was found.
2. **`prune -af` orphans the nerdctl bridge.** It removes the network but
   leaves `nerdctl0` holding 10.4.0.1/24, so rep 1 passes and every later
   rep voids `G-RUN`. Deleting the orphan does not fix it — the run
   recreates it and the poststop hook fails identically. Fixed with
   `--net host` on every arm.
3. **★ G-COLD cooled the CLIENT but never the BACKEND.** The registry pod
   holds blobs in its page cache after the first pull, so later reps were
   served from RAM and the storage layer was never read: **five flint
   pulls of a ~400 MB image produced TWO `LAYOUTGET granted` lines.** The
   arm was comparing the registry's RAM against S3's network fetch and
   calling it flint-vs-S3. With `cold_backend` restarting the serving
   registry per arm, the same run produced **42**. A 3.8% "result" was
   quoted off the warm-backend run before this was caught.

All three share one shape, and it is the campaign's signature: **every rep
individually valid, zero voids, tight variance, and the thing under test
not actually exercised.** No attribution/integrity/coldness/load guard can
see it. The question that catches it is not "did the reps pass?" but
**"did the workload actually reach the thing I am measuring?"**
