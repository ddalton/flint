# The trace check re-based onto LeanP1 (P2 + P1-lite) — 2026-09-25

The code after step 5, slice 4, checked against `LeanP1.tla` (md5
`80052897bb3495c329aef483e4513eaf`) by `trace/trace-check-core.sh`.
`summary.txt` is the run.

**40/40**: 15 traces accepted, 19 mutations rejected (18 by the model, 1 by
the converter's handle-immutability check), and 6 controls rejected (4 by
the model, 2 stop UNSAFE).

- **Model:** `LeanP1.tla` is LeanP2 without the merge base, queue, journal,
  L-126 overlay or L-123 prune, plus:
  - the derived owed set;
  - content convergence;
  - M3 (a delete over theirs applies, and theirs is recorded);
  - the consume's cheap path as state (`synced`, `behind`), with
    `Inv_ShortcutSound`: an idle writer at the pointer it left, with nothing
    marked owed, owes nothing.
- **New scenario:** `ui_save_lands_inside_a_barrier`, a UI save that lands
  between a writer's consume and its merge. It is the only way a trace
  reaches `MergeMarksOwed`. With the rule off, the check stops with
  `Inv_ShortcutSound` violated. It mirrors the code's M6/M6b mutants,
  which lost the save for good.
- **Converter fix:** a gateway event that arrives inside a barrier now
  closes that barrier's consume first. The consume's events are complete by
  then, and without this the model saw the save before the consume.
- **Scenario renamed:** `outranked_delete_publish` became
  `delete_over_a_peers_edit` under M3.
- **Not exercised by any trace:** `ContentConverges` (no scenario restarts
  between a CAS and step 7), `CollectorSparesCited`, `GatewayJudgesRead`
  and `GatewayIgnoresLease`. `SweepUnderLease` and `GatewaySweepGrace` only
  restrict the model. The LeanP1 gate (`../2026-09-25-leanp1-gate/`, 26
  worlds) covers them.
