# tlc-rs — a Rust explicit-state checker for the safety subset of TLA+

An experiment asking whether it is worth replacing TLC (Java) for the gates
in `scripts/check-tla.sh` and `lean/formal/check.sh`. Measured 2026-09-26
on the 8-core (4 performance + 4 efficiency), 8 GiB Mac, against TLC v1.7.4
run the way the gates run it.

    cargo build --release --offline
    target/release/tlc-rs [-workers N] [-engine interp|closure] [-var-order v1,v2,..] [-print-var-order] [-config X.cfg]
                          [-metadir DIR] [-checkpoint MIN] [-recover DIR] [-queue-mem MB] [-fpmem MB] X.tla
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

`INSTANCE` (named or bare, with `WITH` substitutions), so a refinement
`PROPERTY Core!Spec` is checked: its `Init` on every initial state, its
`[][Next]_vars` on every step, through the mapping.

It **refuses** a cfg rather than silently checking less: other temporal
shapes and `ACTION_CONSTRAINT`.

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
vs 398 s; ForgeSyncLive 37 s vs 191 s).

**Fail fast.** As TLC does, the properties are also checked during the
search, on the graph of the states expanded so far, each time it has
doubled (from 1,000 states): every cycle there is a cycle of the final
graph and every node in it has its final successors and enabledness, so a
counterexample found there is real; states not yet expanded are left out
(they would look like dead ends stuttering forever). The final check still
runs. Liveness mutation runs, summed: 57.5 s checking only at the end,
**7.3 s** failing fast, TLC 93.5 s (e.g. FlintCompositionNoWitness 11.4 s
-> 0.09 s, TLC 3.8 s). Re-run on all 92 property entries
(`results/liveness-failfast-sweep-2026-09-28.jsonl`): 69 compared with TLC,
0 differences.

## INSTANCE and refinement

`I == INSTANCE M WITH x <- e, ...` is expanded when the spec is loaded
(`instance.rs`): each definition D of M (and of what M extends) becomes
`I!D`, with M's own references renamed, each substituted constant or
variable x replaced by a new definition `I!__sub_x == e` in the
instantiating module (so no name M binds can capture one in e), and one
not named in WITH left as the same name outside (TLA+'s implicit
substitution). `x'` is then `e'`: e evaluated in the next state, the way
TLC's `OPCODE_prime` evaluates its operand against the next state.

A `PROPERTY I!Spec` of the shape `Init /\ [][Next]_vars` checks Init on
every initial state and `[Next]_vars` on every step. What made it
practical: each substituted state function is evaluated at most once per
evaluation context (`Expr::Memo`), as TLC binds a substitution to a
`LazyValue`; before that, every `w'[s]` inside `Core!Next` re-derived the
whole mapped state, and LeanRefineQueue took 255 s instead of 41.6 s.

Against TLC (8 workers each, this Mac):

| cfg | TLC | tlc-rs |
|---|---|---|
| LeanRefineQueue | 606,916 distinct, holds, 155 s | 606,916, holds, 41.6 s (3.7x) |
| LeanRefineProbe | 1,965,383 distinct, holds, 903 s | 1,965,383, holds, 195 s (4.6x) |
| mapping `ui <- 0` (a UI write is no core step) | action property violated, 2-state trace | the same |
| mapping `seq <- manSeq + 1` | violated by the initial state | the same |

The two mutations were made on copies, to show the check can fail through
the path it claims (the unmutated gate entries hold).

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
   (`Context.getVariableDecls`), not declaration order. tlc-rs derives it
   (`varorder.rs`): it rebuilds the root module's SANY context — SANY's 72
   built-in operators first (every context starts as a copy of them),
   then each EXTENDS module's non-local symbols in the order they entered
   its context, then the module's own declarations in source order, a
   named INSTANCE adding `I!D` in the instanced module's table order and
   then `I` — simulates `java.util.Hashtable` exactly (capacity 11, load
   0.75, rehash to 2n+1, new entries at the head of their bucket,
   `elements()` from the last bucket down, `String.hashCode` keys), and
   reverses the result, as `ModuleNode.getVariableDecls` does. Checked
   against TLC's own order on all 23 root modules of the gates (including
   LeanRefine's INSTANCE and the pending ForgeSyncRewind): 23/23. The
   context was compared entry by entry with SANY's own
   (`results/CtxDump.java`). `-var-order` still overrides;
   `-print-var-order` prints the derived one.

With all four, the single-worker count is exactly TLC's 246,151, and none
of TLC's 246,151 dumped states is merged by tlc-rs. With the derived
order (no `-var-order`), all 9 strict SYMMETRY+VIEW gate entries with a
1-worker TLC count match it exactly (LeanSubtreeTakeover 566,405 ...
LeanBarrierLeaseSyncOverlayHolds 455,989), and so does the pending
ForgeSyncRewindProbeResurrected (3,172,931); in declaration order
ScopedSync gave 241,719. Under SYMMETRY+VIEW the
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
  shards), in memory up to `-fpmem` (default 1024 MB); past it a shard
  sorts its table and merges it into its own sorted file (a new file,
  renamed over the old), keeping in memory only every 128th fingerprint
  (for a one-block read) and a Bloom filter (~10 bits, 4 probes) so a new
  fingerprint rarely touches the disk — ~1.3 bytes of RAM per state on
  disk, so a billion states need ~1.3 GB of RAM and 8 GB of disk;
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

The INSTANCE changes were re-run over the whole gate
(`results/instance-sweep-2026-09-28.jsonl`): 257 entries as before, 0
regressions (3 newly supported).

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

The fingerprint set on disk (`-fpmem 8`, so nearly every fingerprint is
in a file): FlintTierSession 10,698,103 distinct, exact, in 40.3 s against
33.7 s in memory (44.9 s before the Bloom filters: every new state had
cost two reads finding nothing); killed and recovered with the set on
disk, exact again (the checkpoint hard-links the 1,024 shard files);
LeanScopedSyncHolds (SYMMETRY+VIEW, 1 worker) 246,151. The whole gate with
every run's set on disk (`TLCRS_FPMEM_MB=1`,
`results/fpset-disk-sweep-2026-09-28.jsonl`): 255 entries as before, 0
regressions.

On the Linux box (x86, NVMe, 1 niced worker beside other runs) it found
what the Mac could not: 1,024 open shard files exceed Linux's default soft
limit of 1,024 descriptors ("Too many open files"; macOS's default is
~1M). tlc-rs now raises its soft limit toward the hard one at start, as
the JVM does for TLC. Then: FlintTierSession with the set on disk
10,698,103, exact (280 s against 254 s in memory); killed with `kill -9`
after a checkpoint and recovered, exact again. **MCLeanP1Like with the set
spilling (`-fpmem 64`, 4,096 shard spills): 26,592,522 distinct, TLC's
exact count, in 52 min on 1 niced worker of the loaded box (TLC: 61 min on
6 workers), 873 MB peak RSS.**

A queue fix from the same round, found by flint-27: under SYMMETRY a
level must be read back in the order it was written, since which state
stands for an orbit depends on which comes first. After one batch of a
level had spilled, a later, smaller batch (always the last, partial one)
could still fit under `-queue-mem` and was then read *before* the blocks
on disk. ForgeSyncRewindProbeResurrected at 1 worker gave 3,189,392
against TLC's 3,172,931 by default, and TLC's count with `-queue-mem
1024`. Now a level that has spilled keeps spilling: 3,172,931 at the
default and fully spilled; LeanScopedSyncHolds 246,151 at 64, 1 and 0 MB.

Not yet: checkpoints only at level boundaries (TLC's can land
mid-level).

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
