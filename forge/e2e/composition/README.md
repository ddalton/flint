# Composition drills (C1–C7)

Every other drill in `forge/e2e/` tests forge against itself. These
test what happens when **two products meet on one bucket** — forge and
lean, or forge and a read-write passthrough mount.

They exist because the rule they check is stated as an invariant but
implemented as a convention:

> One prefix has exactly one writer. Each prefix has one epoch lease
> and one pointer object, and writes are serialised by conditional PUT
> against that pointer.

That is true *within* a product. Across products the arbitration units
are disjoint — `<prefix>/git/epoch` for forge, `<prefix>/.flint/lean/epoch`
for lean — so nothing contends, nothing 412s, and nothing is logged.

## Running

```sh
bash forge/e2e/composition/run-all.sh          # all seven
bash forge/e2e/composition/c3-foreign-write.sh # one
DOWN=1 bash forge/e2e/composition/run-all.sh   # and stop MinIO after
```

Needs Docker, `git`, and the two binaries:

```sh
cargo build --manifest-path forge/syncer/Cargo.toml --features s3
cargo build --manifest-path lean/sidecar/Cargo.toml --features s3 --bin flint-sync
```

## C6 and C7 are not composition drills

`c6-undo.sh` and `c7-prewarm.sh` are the odd ones out: they test forge against itself, on a
real S3 API rather than the memory double, because what it checks is a
SWEEP — and a sweep is the one thing a double makes too easy to believe.
The question is X15's: after a branch is force-pushed back, is the
previous state still recoverable from the bucket?

Its three anti-vacuity devices are the point of it. A **decoy** orphan
under the pack prefix, which the sweep must take, so "the protected
pack survived" cannot be read off a sweep that never ran. A **control
arm** with the window at 0 — the code before X15 — which must LOSE the
pack. And a wait for a sweep **past the rebuild**, because the sweep
that ran at start-up says nothing about packs a later base rebuild
unnamed. Three shapes of this drill were green before those were added,
and all three were measuring nothing.

`c7-prewarm.sh` asks X14's question: does a server that warmed while it
waited take over without paying for the repository? Two arms on one
binary — the control is `PREWARM=0 LOG_MAX_ENTRIES=0`, which is the
code before this — each with a holder, a challenger on its own disk,
six pushes of ~64 MB and 4,000 small files, and then ONE more push
after the challenger is already warm, because a standby is never caught
up at the moment it is needed.

Its own anti-vacuity devices: the rig refuses a verdict unless the
control actually fetched most of the repository (`430 MiB`, or
INCONCLUSIVE); unless the challenger's own `warm:` line appeared; and
unless the successor's repository, in BOTH arms, clones with the
holder's tip, the blob's bytes and a passing `fsck` — fast must not be
allowed to mean empty. The wake is measured at both edges through
`/status` at 50 ms (`watchphase.py`), because the whole wall clock
carries up to one claim poll of quantisation and on a loopback store
that noise is larger than the transfer.

    arm    wake_ms  claim2srv  restore_ms  files   MiB      proof
    cold   1457     861        845         21      430.0    Full
    warm    732     222        232          0        0.0    Nothing

What the rig can and cannot decide: it decides the mechanism (bytes
fetched, the proof's kind, the catch-up through the log) and the
claim-to-serving span. It cannot decide the whole wake — 430 MiB moves
at loopback speed here, so the transfer is 0.6 s and the poll is 1 s.
The number that decides the wake is the cluster's, where the 40 GiB
drill repository restored in 139 s.

## Why MinIO and not the memory double

`export::run_barrier` **execs** the shipped `flint-sync` binary
(`export.rs:254`). A second process cannot reach an in-process
`MemoryStore`, so the composition is only observable against a real
endpoint. `rig_gate` re-proves on every run that the store honours
`If-None-Match: *`, `If-Match`, and leaves bytes alone on a refusal —
without that, a drill could report corruption that is the rig's own.

## Reading the output

A leg is phrased so **PASS means the composition rule held**. A FAIL is
a finding about the product, not a broken drill. Each drill asserts its
own preconditions first (`precondition:` lines); if one of those fails,
the legs after it prove nothing and should be ignored.

## What they substitute

`c4` and `c5` stand in for a read-write passthrough mount with a plain
S3 PUT/DELETE. Mountpoint does not run on macOS, and what is under test
is whether the export notices an object that changed behind it — which
does not depend on what changed it. A real mount would additionally
pick its own part size and etag shape; that is not measured here.
