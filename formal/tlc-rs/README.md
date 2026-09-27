# tlc-rs — a Rust explicit-state checker for the safety subset of TLA+

An experiment asking whether it is worth replacing TLC (Java) for the gates
in `scripts/check-tla.sh` and `lean/formal/check.sh`. Measured 2026-09-26
on the 8-core (4 performance + 4 efficiency), 8 GiB Mac, against TLC v1.7.4
run the way the gates run it.

    cargo build --release --offline
    target/release/tlc-rs [-workers N] [-engine interp|closure] [-var-order v1,v2,..] [-config X.cfg]
                          [-metadir DIR] [-checkpoint MIN] [-recover DIR] [-queue-mem MB] X.tla
    target/release/tlc-rs -codegen DIR [-config X.cfg] X.tla   # then: cd DIR && cargo build --release

## What it supports

TLA+ expressions (sets, functions, records, sequences, EXCEPT with `@`,
CHOOSE, CASE, lazy LET, LET RECURSIVE, top-level RECURSIVE, LAMBDA in
SelectSeq, `:>`/`@@`, `Permutations`), `Init`/`Next` or `SPECIFICATION`
(temporal conjuncts are skipped when extracting Init and Next), INVARIANT,
CONSTRAINT, CHECK_DEADLOCK, SYMMETRY, VIEW, EXTENDS of sibling modules,
counterexample traces, a disk-backed queue, and checkpoint/`-recover`.

Temporal `PROPERTY`s in the shapes the gates use: `[][A]_v` (checked on
every transition), `P ~> Q`, `[](P => <>Q)`, `<>[]P`, `[]<>P`, under
`\A`, with `WF`/`SF` fairness from the spec (also `SpecLive == Spec /\
Fairness`), and `ENABLED` in state predicates.

It **refuses** a cfg rather than silently checking less: other temporal
shapes, `ACTION_CONSTRAINT`, and `INSTANCE` (so `Core!Spec` refinement).

## Liveness

After the search, on the reachable graph (every state has a stuttering
self-loop): a strongly connected component is fair under WF_v(A) iff A is
disabled at one of its nodes or one of its edges is an A-step; under
SF_v(A), if a node enables A and no edge is an A-step, the enabling nodes
are dropped and components recomputed (Emerson-Lei). `P ~> Q` fails iff a
reachable P /\ ~Q node reaches, through ~Q nodes, a fair component of ~Q
nodes; `<>[]P` iff a fair component contains a ~P node; `[]<>P` iff a fair
component lies within ~P. Counterexamples print as a lasso. Enabledness and
A-step labels are computed during the parallel search, on each actual
transition (not a SYMMETRY representative); an action's unassigned primed
variables are free, as in TLA+.

Against TLC on every property-bearing gate entry both finish
(`results/liveness-vs-tlc-*.jsonl`): **72 compared, 0 differences** —
verdict, violation kind (temporal vs action), and the exact distinct count
on every strict run. Strict runs are much faster (FlintTierSessionLive 29 s
vs 398 s; ForgeSyncLive 37 s vs 191 s); liveness *mutation* runs are
slower (~20 s vs ~4 s), because TLC checks liveness periodically during the
search and stops at the first violation, and tlc-rs checks once, at the
end.

## Three engines, one checker

The BFS, fingerprints, SYMMETRY/VIEW keys and traces are shared; how
formulas are evaluated is an `Engine`:

| engine | how | build cost |
|---|---|---|
| `interp` | tree-walking interpreter over resolved IR (by-reference path reads, fused guards) | none |
| `closure` | the IR compiled once to nested Rust closures | none |
| generated | `-codegen` emits a Cargo crate: locals are Rust `&Value`s, quantifiers are loops, the action tree is nested blocks | rustc: median 7 s, 70 s for LeanSubtree |

All three agree on all 167 decidable gate entries
(`results/engine-sweep-2026-09-26.jsonl`). The closure compiler buys
nothing over the interpreter (104.6 s vs 101.2 s on FlintTierSession): the
interpreter was already specialized. Generated Rust buys 1.3–1.5x.

## Correctness against TLC

Every gate entry was run through it (`results/gate-sweep-2026-09-26.jsonl`,
before SYMMETRY/VIEW existed): 167 agree with the gate's expected verdict,
0 disagree. On those 167, TLC was also run
(`results/tlc-vs-tlcrs-2026-09-26.jsonl`): all 36 strict runs report the
**identical distinct-state count**, and every mutation run is violated by
both.

### SYMMETRY + VIEW: what exactness took

Each alone matched TLC at once. Together they did not (LeanScopedSyncHolds:
241,719 vs TLC's 246,151), because TLC fingerprints the VIEW of the *least
permuted state*, and "least" is defined by TLC internals. Read from the
v1.7.4 source and confirmed against TLC's own tokens:

1. TLC checks the **group generated** by the SYMMETRY set
   (`MVPerms.permutationSubgroup`), not the set.
2. Strings, record fields and model values are ordered by **`UniqueString`
   token**, not text: cfg values first (the cfg is read before the spec),
   then identifiers as SANY lexes them, then string literals (interned only
   at semantic analysis). tlc-rs interns names in that order, so its value
   order *is* TLC's `compareTo` order — 164/164 names checked pairwise
   against TLC's real tokens.
3. Functions and records compare by domain size then interleaved (key,
   value); a model value is below every other kind.
4. Variables are compared in **TLC's variable order, which is a
   `java.util.Hashtable` iteration order** over SANY's symbol table
   (`Context.getVariableDecls`), not declaration order. tlc-rs cannot
   derive it yet; pass it with `-var-order` (the first state of a TLC
   `-dump` lists it).

With all four, the single-worker count is exactly TLC's 246,151, and none
of TLC's 246,151 dumped states is merged by tlc-rs. Under SYMMETRY+VIEW the
count depends on exploration order at more than one worker, **in TLC too**
(246,151 / 246,247 at 8 workers), so only 1-worker counts are reproducible.

## Speed

| spec | workers | TLC | tlc-rs interp | tlc-rs generated |
|---|---|---|---|---|
| LeanChunkGC (3,080,892 distinct) | 1 | 34.2 s | 14.3 s (2.4x) | 10.2 s (3.4x) |
| LeanChunkGC | 8 | 14.0 s | 4.9 s (2.9x) | — |
| FlintTierSession (10,698,103) | 1 | 196 s | 101 s (1.9x) | 66.9 s (2.9x) |
| FlintTierSession | 8 | 65.5 s | 36.8 s (1.8x) | — |
| LeanScopedSyncHolds, SYMMETRY+VIEW (246,151) | 1 | 34.4 s | 14.6 s (2.4x) | 11.4 s (3.0x) |
| LeanScopedSyncHolds, each at its best | 6 / 4 | 9.7 s | 6.0 s | 5.4 s (1.8x) |
| the 167 decided gate runs, summed | 8 | 308 s | 82 s (3.8x) | — |
| of which the 152 runs TLC finishes in < 2 s | 8 | 131 s | 3.4 s (~39x) | — |

tlc-rs stops scaling at the 4 performance cores (ScopedSync: 14.6 / 9.1 /
6.0 / 7.1 s at 1 / 2 / 4 / 8 workers); TLC keeps gaining to 6.

## Memory, disk, and checkpoints

TLC's layout (`store.rs`):

- the seen set holds **8-byte fingerprints** only (open addressing, 1024
  shards);
- each worker appends (fingerprint, parent) records to its own **trace
  log**, buffered (512 MB in all) and spilled to a file, and a
  counterexample walks parents back through it;
- a BFS level stays in memory up to `-queue-mem` (default 64 MB,
  estimated); past it, blocks of 4096 states are serialized to a level
  file and read back by whichever worker takes the block;
- every `-checkpoint` minutes (default 30, as TLC; 0 = never), at a level
  boundary, the fingerprints, the level about to be expanded (its file is
  hard-linked, not copied) and the trace logs' lengths are written to
  `ckpt/`, swapped in atomically. `-recover DIR` resumes; it refuses (exit
  2) if the spec or cfg changed since.

The metadir defaults to `states/<spec>-tlcrs-<time>-<pid>` beside the spec
and is created only when something spills; a run that finishes removes
the files it wrote (only those). A killed run leaves them, as TLC does.
Liveness properties still keep their graph in memory, so with them there
are no checkpoints and `-recover` is refused.

| | before | now (default) | now, queue fully on disk |
|---|---|---|---|
| FlintTierSession, 8 workers: peak RSS | 2.39 GB | 0.98 GB | 0.81 GB (0.53 GB with trace on disk) |
| time | 37.9 s | 38.0 s | 38.3 s |

TLC on the same run: 1.2 GB. Serializing is cheap next to evaluating
actions, so spilling costs ~1%.

Checked: the 301-entry gate sweep matches the previous sweep (260
comparable, 0 differences); FlintTierSession killed (`kill -9`) at 20 s and
recovered gives exactly the uninterrupted 41,286,785 generated / 10,698,103
distinct / depth 31; LeanScopedSyncHolds (SYMMETRY+VIEW, 1 worker) killed,
recovered with the queue and trace on disk: 246,151, TLC's count; a
mutation run killed and recovered prints its full 27-state counterexample,
23 levels of it read from pre-crash trace records.

**MCLeanP1Like** (flint-27's LeanP1 world, md5s identical to theirs), which
needed an estimated ~10 GB before this: **26,592,522 distinct, no error —
TLC's exact count** — in 657 s at 8 workers on this Mac, **1.46 GB peak
RSS**, queue on disk (TLC: 1 h 01 min, 6 workers, `-Xmx24g`, on the Linux
box; not the same machine). Generated counts differ (112,597,310 vs TLC's
115,302,321); the two count generated states differently, and distinct is
the comparison. Sweep: `results/disk-sweep-2026-09-26.jsonl`.

Not yet: TLC's fingerprint set also spills to disk (tlc-rs needs ~14 B
of RAM per distinct state: ~14 GB at 1B states); checkpoints only at level
boundaries (TLC's can land mid-level).

## Where the time went (and what fixed it)

1. Fingerprint every successor from scratch → per-variable hashes, reusing
   the parent's hash where the value is the same `Arc`; already-seen
   successors are never allocated (19.2 → 16.0 s on LeanChunkGC).
2. `hst[h] = "up"` cloned and dropped `hst` → a by-reference evaluator for
   paths into the state or constants (FlintTierSession 120 → 103 s).
3. SYMMETRY+VIEW (ScopedSync 27.1 → 14.6 s): keys computed incrementally
   (per-parent memo of each variable's permuted comparison and hash; a
   successor redoes only the variables it changed); permuted values
   compared and hashed **without building the copy** wherever element
   order is known in place, including `[Paths -> X]` via the inverse
   permutation; a VIEW that is a tuple is keyed part by part, and a part
   proven equivariant is evaluated once on the state and its value's
   permuted hash taken, instead of evaluating it on a permuted copy.
4. Codegen of lazy LET: one closure per definition, not the body inlined
   at each reference (LeanSubtree's generated `next`: 135 MB → 428 KB).
5. Measured and dropped: a per-allocation memo of hashes and permuted
   images (20.2 → 22.4 s); PGO (+4–7%).
6. mimalloc, fat LTO, one codegen unit, `target-cpu=native`, `panic=abort`.
