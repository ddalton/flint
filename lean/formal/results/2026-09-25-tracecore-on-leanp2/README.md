# The trace check re-based onto LeanP2 — 2026-09-25

The code after P2 step 5, slices 1-3 (UI verbs commit; no cell entries),
checked against `LeanP2.tla` (md5 `7be0698441b34aca7a4e8a3248886eb8`) by
`trace/trace-check-core.sh`. `summary.txt` is the run.

**38/38**: 14 traces accepted, 18 mutations rejected (17 by the model, 1 by
the converter's handle-immutability check), 6 controls rejected (5 by the
model, 1 stops UNSAFE: the two-CAS rename cites one handle at two names).

What changed from the LeanCore version (archived in
`../2026-09-25-tracecore-on-leancore/`, last run 44/44):

- **Model:** `LeanP2.tla`. It is LeanCore's writer loop without the cell,
  plus P2R's gateway, restart, sync and re-upload copies. It differs from the
  sandbox where the code does: a save is judged against the version it read
  (a moved document refuses it), and the gateway deletes nothing (the orphan
  sweep takes what a save, delete or rename stopped citing).
- **Mapping:** `conf_hitl_write` → `ui_put` + `ui_commit` (GPut, then GCas
  at the seq logged); delete and rename → one gateway step at the seq
  logged. A re-upload of bytes PUT once maps to a copy handle. Any cell
  event (repair, removal, a consume from the inbox) is refused.
- **Harness:** `conf_start` carries the seed's keys; every `conf_hitl_*`
  carries the seq its commit installed.
- **New scenario** `agent_edit_over_a_ui_delete`: under P2 the old route to
  the delete-override record (a writer adopting the UI write) is gone, and
  `control-delete-override-unrecorded` was ACCEPTED on the old trace. This
  scenario takes the user's rule's route and the control rejects again.
- **Runner fix:** a mutation whose anchor was missing stopped the generator,
  and every mutation after it went unchecked with only a stderr line. The
  generator's failure now fails the run. In the first run it hid 9 of 17.

Rules no trace can exercise (the model checker's, in `gen-leanp2.sh`):
SweepUnderLease and GatewaySweepGrace restrict the model; no scenario syncs
(QueueYieldsToSync), has a save refused (GatewayJudgesRead) or saves while a
writer holds the lease (GatewayIgnoresLease); and CollectorSparesCited is no
longer reached. `ui_rename` exercised it while a rename went through a writer.
