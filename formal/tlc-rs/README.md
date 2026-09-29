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

### Liveness on disk

The graph is no longer held in memory. When a worker expands a state it
appends one record to its own graph log: the state's trace index and key,
the fairness actions enabled there, the property predicates' bits
(P and Q of each `P ~> Q` instance, evaluated then, so the state is never
needed again), and its edges out with their fairness masks. A check
(partial, while failing fast, or final) loads the logs in two passes into
a compact graph: keys sorted give dense ids, edges become one array of
4-byte targets, masks go through a palette of the few distinct ones
(~34 bytes a node, ~8 an edge, against whole states before). A
counterexample's states are rebuilt from the trace logs. The loader
refuses a graph in which a state was recorded twice.

Liveness runs now checkpoint: the graph logs' lengths go into the
checkpoint, and recovery **cuts the logs back to them**. That is the trap
flint-27 saw TLC fall into: TLC's disk graph had been written past its
checkpoint and its recovery failed reading it. Control: FlintTierSessionLive
killed 25 s in with each log ~10 MB past the checkpoint; recovered, exact
(2,603,207, holds); with the cut disabled, the loader stops with "the
graph logs record the state ... twice".

| FlintTierSessionLive, 8 workers | before | now |
|---|---|---|
| peak RSS | 4.2 GB | **749 MB** |
| time | 100 s (the Mac swapping) | 27.7 s |

flint-27 on the box, LeanP1LiveHoldsSmall (2 workers): tlc-rs holds with
10,676,334 distinct states, depth 36, in 441 s at 1.6 GB (the graph of
10.7M nodes loaded in 29 s); TLC (`-lncheck final`, `-Xmx8g`) the same
verdict and count in 877 s. The build before this one hit its 6 GB guard
within 51 s.

All 92 property entries re-run (`results/liveness-disk-sweep-2026-09-28.jsonl`):
71 compared with TLC, 0 differences (ForgeSyncLive's 1,781,559 is the
spec as changed on 2026-09-28; TLC re-run on it: 1,781,559). Liveness
mutation runs, summed: 3.5 s (TLC 93.5 s). A lasso may differ from the
earlier build's by which of equally short paths it takes (node ids now
follow the key order).

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

## The official examples (tlaplus/Examples)

Every exhaustive-search model in [tlaplus/Examples](https://github.com/tlaplus/Examples)
(commit c9e45d0) whose manifest records a TLC runtime of at most 10
minutes was run through tlc-rs (`results/examples_run.py`,
`results/examples-2026-09-28.jsonl`): 165 models. tlc-rs accepts 29 and
refuses 136 (exit 2, nothing checked).

**All 29 it accepts agree with TLC's recorded result**: the verdict (26
hold, 3 safety violations), and every recorded distinct-state count and
depth — among them dijkstra-mutex's 4-processor model (33.5M states),
acp, bosco, GermanProtocol, ReadersWriters, SingleLaneBridge, the
Specifying Systems chapters. The one depth that differed, btree/kvstore
(9 against the manifest's 11), is TLC's multi-worker depth: TLC reports 9
at 1 worker, and the same 2,641 states and 28,585 generated.

What the 136 need is almost all syntax, not checking: named `ASSUME
Name == ...` and proof syntax (`LEMMA ... BY ... DEF`, `<1>` steps), which
TLC ignores; higher-order operator parameters (`op(_, _)`); recursive
function definitions (`f[x \in S] == ...`); unbounded `CHOOSE x : P`;
`INSTANCE` in a `LET`; a few operators (`\prec`, `^^`, `&`); some
temporal shapes; and the Community Modules.

### Closing the gaps the examples showed (2026-09-28)

- **Theorems and proofs are skipped**, as TLC skips them: `THEOREM` /
  `LEMMA` / `PROPOSITION` / `COROLLARY` with any proof (`BY`, `DEF`,
  `OBVIOUS`, `<1>` steps), and `USE` / `HIDE`, up to the next line that
  starts a module-level unit. `ASSUME Name == e` is an assumption and a
  definition of Name. Labels (`Name::`) are skipped. `TLAPS` and the proof
  libraries extending it are built-in empty modules.
- **Only the module is TLA+**: prose before `---- MODULE` and after the
  closing `====` is ignored, as by SANY.
- **Function definitions** `f[x \in S, ...] == e`, at module level and in
  LET, recursive or not. As in TLC they are lazy: an application `f[a]`
  evaluates `e` for that argument only (an operator `f!app`), with the
  domain checked; `f` alone is the whole function. (Building the whole
  function per application made ElevatorSafetyMedium, 18.0M states, not
  finish in 5 minutes; now 132 s, TLC's recorded runtime 3 minutes.)
- **Operator parameters** `F(op(_, _), x) == ...`, called with an operator
  or a `LAMBDA`; `SelectSeq` with any operator.
- **User-defined infix operators** (`\prec`, `^^`, `++`, `\oplus`, ...) with
  TLA+'s precedences.
- **Unbounded `CHOOSE x : P`** parses; as in TLC it is an error only if
  evaluated (a cfg normally overrides such a definition).
- **Membership in sets that cannot be enumerated**: `x \in S` where S
  involves Nat, Int, STRING or Seq(T) is decided from S's structure —
  `{y \in T : P}`, `A \ B`, `\cup`, `\cap`, `[D -> R]`, record sets, and
  set-valued definitions — never building S (`Capacity \in [Jug -> {n \in
  Nat : n > 0}]`, `N \in Nat \ {0}`).
- **cfg**: `<-[M]` and `= [M]v`, overrides scoped to a module, are accepted
  (modules are flattened).

### More gaps, and membership as TLC decides it (2026-09-29)

- **`<>P`** (P a state predicate): a fair behavior from an initial state
  that never reaches P, checked on the liveness graph as `Init ~> P`.
  `ENABLED A` inside a temporal formula is a state predicate
  (`<>(ENABLED Termination)`, `[]((~ENABLED Next) => Done)`). A toy spec
  gives TLC's verdict in all three cases (holds under WF; fails without it,
  by stuttering; fails for a P never reached).
- **Operator arguments by name, as TLC substitutes them.** An argument that
  is a state variable (`XAct(0, x, x')`, `Send(p, d, memInt, memInt')`) is
  substituted into the body, so `xNext = xInit` there assigns x'; an
  action argument (`NoStutter(NoHistoryChange(l0(self)))`) is expanded
  where the body uses it, in the caller's scope. A cfg override of an
  operator (`Send <- MCSend`) applies in actions too.
- **`x <-[M] e` is scoped**: it overrides M's definition under every name an
  INSTANCE gave it (`V!Ballot` in MCPaxos's refinement property `V!Spec`),
  not the same name in the root module.
- **Membership is decided from the set's structure, as TLC does**: `x \in
  {y \in S : P}`, `SUBSET S`, `[D -> R]`, record sets, `\cup`/`\cap`/`\`,
  never building the set to test one element — unless the set is a
  constant, finite definition, which is evaluated once and looked up.
  MCQuicksort (`UV \in DomainPartitions`, a filter over `SUBSET SUBSET
  (1..4)`) went from over 300 s to 2 s; MCBinarySearch (`seq \in
  SortedSeqs`, 488,280 sequences) stays at 1.6 s because SortedSeqs is
  constant. A value that is not a function is in no function or record
  set (FALSE, not an error: `NoVal \in [adr : Adr, ...] \cup {NoVal}`).
- **No behavior spec**: a cfg with neither SPECIFICATION nor INIT/NEXT
  checks the assumptions and explores nothing (TLC's 0 states). A false
  ASSUME is a verdict, exit 10 (TLC's), not "unsupported".
- **Parser**: `<<A>>_v`; `{<<a, b>> \in S : P}`; a theorem's `ASSUME NEW`
  and a proof's definitions are skipped with it (a unit ends only at a line
  no further right than the theorem); an infix operator as an argument
  (`FoldFunctionOnSet(+, 0, f, S)`); `(+)` and `(-)`, other spellings of
  `\oplus` and `\ominus`.
- **Modules**: `-lib DIR` (or `TLCRS_LIB`, a path list) is searched for a
  module not beside the spec — where TLC's classpath finds the
  [Community Modules](https://github.com/tlaplus/CommunityModules). Bags,
  a standard module TLC defines in TLA+, is built in (its source from
  tla2tools.jar, `modules/Bags.tla`).
- The liveness violation prints TLC's `Error: Temporal properties were
  violated.` before naming the property (the lean gate matches that line).
- **More parser**: positional subexpressions `Inv!2` (the second conjunct or
  disjunct of Inv's definition, also `D!k!j`); `THEOREM Name == e` records
  its statement as `Name!:` (TLC checks `ASSUME QuorumNonEmpty!:`; a false
  statement fails the ASSUME, checked); `[Next]_I!vars`; `INSTANCE` in a LET
  (without WITH) is hoisted to module level. An empty search reports depth 0.

**Where that leaves the examples** (`results/examples-2026-09-29.jsonl`,
this commit's tlc-rs, `-lib` pointing at CommunityModules 9aae8ea, 4 workers,
10 minutes a model): of the 165 models, tlc-rs now accepts **112, and all 112
agree with TLC's recorded result** (100 hold, 12 safety violations; every
recorded distinct count and depth). MultiPaxos_MC (8 minutes in TLC) ran out
the 10 minutes. The 50 it refuses and 2 it errors on, by cause: temporal
shapes the liveness checker does not take (refinement properties carrying
WF/SF — `ABCSpec`, `EWD998Spec` —, `[]<><<A>>_v`, a temporal IF, `=>`
between quantified temporal formulas); parser gaps (parameterized `P(x) ==
INSTANCE`, `\cdot`, unbounded `\A x :`, LET RECURSIVE); Community Modules
whose TLA+ definitions TLC replaces with Java (Graphs and UndirectedGraphs
over `Seq(S)`, Bitwise) or that tlc-rs lacks (TLCExt, Randomization);
`TLCGet`, `RandomElement`, `ACTION_CONSTRAINT`.

**The gate sweep had skipped a third of the gate.** `results/sweep.py` read
only one-line entries and only the variable `$M`: the 138 lean entries whose
expected violation sits on a continuation line were never run, `$C` stayed
literal and `$M2` became `LeanSubtree2`. Fixed (439 entries, all resolved),
and a lean mutation's expectation (`"Invariant X is violated"`, a substring
of the output) is matched as the lean gate matches it. On all 439
(`results/gate-sweep-2026-09-29.jsonl`, 60 s a model): **404 agree, 0
disagree, 0 unsupported**; the other 35 are the large worlds, which need more
than 60 s (being run to completion on the box).

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

### Scaling across cores

Measured at 1 and 4 workers on the Mac's 4 performance cores, where total
CPU time against wall time separates the two ways to lose: CPU time that
grows is contention, idle time is imbalance.

- **Imbalance.** A spilled level was handed out in blocks of 4,096 states,
  so its last few blocks ran on single workers while the rest waited:
  LeanScopedSyncHolds' big levels ran 63-66% busy (per level:
  `TLCRS_LEVEL_PROFILE=1`). Blocks are now at most 256 states, sized to
  give each worker ~16 per level.
- **Sharing.** States held in memory as value trees share sub-values with
  their parents, built on other cores, and every expansion bumps the same
  reference counts across cores; a fully spilled run, whose states are
  decoded privately, was *faster* (5.34 s against 6.15 s, CPU 18.8 s
  against 23.8 s). So a level is now always kept serialized, in memory up
  to `-queue-mem` (default 256 MB, now real bytes) and then on disk, and a
  worker decodes its own copy.

| world | workers | before | now |
|---|---|---|---|
| LeanScopedSyncHolds (SYMMETRY+VIEW) | 4 | 6.07 s (2.7x over 1) | **4.72 s (3.6x)** |
| LeanChunkGC | 4 / 8 | 4.82 / 4.31 s | 4.63 / 3.84 s |
| FlintTierSession | 4 | 37.5 s | 35.8 s |

The cost is serializing each state once: ~6% at 1 worker on
LeanScopedSyncHolds, nothing measurable on FlintTierSession. What remains
at 4 workers is hardware: on FlintTierSession every function slows by the
same third (sampled profiles at 1 and 4 workers have the same shape), as
the performance cores share L2 and memory bandwidth over a 10.7M-state
working set; TLC scales no better there (196 s at 1 worker, 65.5 s at 8).
Beyond 4 workers the Mac adds efficiency cores, which add little.

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
Liveness runs checkpoint and recover too (see *Liveness on disk*).

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
