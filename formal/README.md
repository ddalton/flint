# Formal model — the replica-lifecycle / writer-set machine

`FlintReplication.tla` models the durability core every flint orchestrator
mutates: leg lifecycle states, the writer set, epoch cuts, raid superblock
generations, the F36c freshness gate, and the failure taxonomy the P4 work
made explicit (crash-stop vs **silent omission** vs verified death).

Run the gate: `scripts/check-tla.sh` (fetches tla2tools.jar on first use).
It requires **both**:

1. `FlintReplication.cfg` — the shipped design (`GateStrict=TRUE`): all
   invariants plus post-storm convergence hold. ~25k distinct states,
   seconds on a laptop.
2. `FlintReplicationF36c.cfg` — the pre-F36c bug (`GateStrict=FALSE`):
   TLC **must find** an `Inv_NoSilentLoss` violation. The 6-state
   counterexample is the split-lineage loss in its purest form: assemble
   from `l1` alone, ack a write, crash, assemble from `l2` alone —
   the acked write is gone with nothing surfaced. This run is the model's
   own regression test; if it stops failing, the model lost its teeth.

## What maps to what

| model | code |
|---|---|
| `writerSet` + stamp/add/remove actions | `replica_sync.rs`: `set_writer_set` / `mark_in_sync` / `mark_stale` / `prune_writers_for_replacement` |
| `Assemble` gate disjunction | `freshness_gate.rs` (`Proceed` / `Defer` / `ServeWithRisk`) |
| `Replace` guard `legUp = "dead"` | `replica_replace.rs::node_gone` (the C2 justification) |
| `raidGen` / `legGen` / `NewestOf` | SPDK raid1 superblock examine (newest incarnation serves) |
| WF on `RaidDeconfigure` | P4 dead-target timeouts (TCP_USER_TIMEOUT + fast_io_fail) |
| WF on `ConfirmDead` | the replace-after / node-gone threshold |
| `Inv_NoSilentLoss` | PacificA's commit invariant; the ledger oracle's zero-loss check is its runtime shadow |

## What the model already caught

Its **first TLC run** disproved the author's belief (recorded in several
code comments) that replacement is the *only* writer-set exit — the code's
`mark_stale` also removes, and `set_writer_set` stamps wholesale at
assembly. The model now mirrors the real maintenance rules, and the
crash-before-stale-mark race they leave is shown to be covered by the
superblock-generation belt — for exactly as long as at least one
newer-generation leg attaches, which is why the record-level gate exists
for the lone-returned-leg case. That interplay was previously argued in
comments; now it is checked.

## Deliberate scope limits (tranche 2 candidates)

- The F48 zombie / two concurrent assemblies (needs per-process views of
  the record — the natural next extension, and S2's design question).
- Hot-rejoin's esnap window internals (covered by the crash-sweep sim
  harness in `hot_rejoin.rs` tests instead — cancellation at every RPC
  boundary, invariants asserted after recovery).
- Epoch chains deeper than one cut; the stale-only-survivor catastrophe
  path; identity domains (killed at compile time by the newtypes).

The model checks the design, not the Rust. The campaign drills and the
ledger oracle remain the check on the axioms (P4 exists because a
transport assumption, not a design step, was wrong).
