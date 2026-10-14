# Lean: model-to-code action items after M5 (2026-10-14)

The M5 proofs (`lean/formal/results/2026-10-07-tlaps-m5/`, parts C and D)
show that the restated claims NR5 and AN5 hold in the MODEL, for every path
count, writer count and bound. That makes the code more robust only through
two follow-ups. Neither is done.

1. **Add the missing gate worlds**, so the gate can see the behaviour the
   proofs had to handle.
2. **Confirm the code matches the two modelling fixes** the proofs depend on.

Status: OPEN. Owner: unassigned.

---

## 1. Add the missing gate worlds

Every world in the LeanP1 gate today has `MaxRemovals <= 1`, and none
combines a copy, a restart and the retire age. In those worlds the original
`Inv_NoRegress` and `Inv_AckedNamed` cannot fail, and neither can the
restated NR5 and AN5. Every counterexample found during M5 lives outside the
gate:

| trace | what it needs | what it broke |
|---|---|---|
| E6, E8 (NOTES.txt) | 3 removals; the retire age | the original Inv_NoRegress, Inv_AckedNamed |
| E21 | 2 committing writers, 4 barriers, 2 removals | NR3 (before udel recorded a commit's deletes) |
| E27, E31, E33 | an edit over the tree's own pending upload | NR3 (model ghost: Edit's base) |
| E29 | a conflict naming the baseline's successor | NR3 (the claim's reading of a conflict) |
| E38, E39 | a copy (upload + restart + re-upload), 2-3 UI saves, rename, the retire age, a failed fetch | AN3 |

To do:

- [ ] **Restate the claims in `LeanP1.tla`.** Replace `Inv_NoRegress` and
  `Inv_AckedNamed` with NR5 and AN5 (today they are stated only in
  `MCLeanP1M5R.tla` / `MCLeanP1M5D.tla`). This edits the gate's input; it
  needs the owner's approval first.
- [ ] **Add BFS gate worlds:** `Rem3` (3 removals), `Rem3Age` (3 removals and
  the retire age), `ScopeHolds` with 3 removals. These are E35/E41's worlds:
  7.4M to 77M distinct, minutes to about 25 minutes each with tla-mc
  `-compile`.
- [ ] **Add the scripted traces to the gate as regression checks:**
  `MCLeanP1M5H*.tla` (E27, E29, E31, E33, E38, E39). Each must still reach its
  end (`HReach`). Each must still break the OLD claim, and must pass the new
  one. BFS cannot reach these depths (28-32 steps in large worlds), so the
  scripts are the only gate evidence for them.
- [ ] **Keep the known-bad controls in the gate:** MN2 must break NR5 and MA1
  must break AN5. Today: MN2 breaks NR5 at depth 13; MA1 breaks AN5 at depth
  10 and KD2 at depth 2.
- [ ] **Optional, needs EC2 (ask first, pure spot):**
  - P3Age (3 paths, 3 removals, the retire age) for AN5 and NR5 on today's
    model. E20 checked it exhaustively for NR3 and AN3 on the older model:
    1,090,176,224 distinct.
  - W2Rem3 with the retire age. That world was never decided; without the
    age its AN3 run was vacuous.

## 2. Confirm the code matches the two modelling fixes

### Fix 1: Edit's base (E27, E31, E33; `LeanP1.tla` `Edit`)

The model's `base` of an edit is the tree's newest version at the path:
- the tree's bytes, if present;
- else, while a barrier uploads that path, the snapshot it publishes;
- else the baseline.

`base` is a GHOST. The claims read it, through `Derives`; the protocol
never does. The merge base is the baseline, in the model (`Foreign`) and in
the code: `lean/syncer/src/barrier.rs:2032` `merge_base()`, `:1013-1022`,
`manifest.rs:1243` `merge()`. So the fix changes what the claims count as
"derived from", not what the code does. What has to be confirmed is that the
ghost tells the truth about provenance:

- [x] **Edit over a file the agent still has:** its bytes derive from the
  file's current bytes, not from the baseline. This is true by
  construction: the editor writes the file.
- [x] **A file deleted and re-created while its upload is pending (between
  the scan and the finish):** CHECKED 2026-10-14. Step 7 makes the published
  snapshot the baseline (`barrier.rs:1324`), so the re-created file's merge
  base is that snapshot. It publishes as the snapshot's successor with no
  record, which is the model's "else the snapshot it publishes".
- [x] **A restart between the CAS and step 7:** CHECKED 2026-10-14.
  - If the agent left the path alone, the next consume finds the tree's
    bytes equal the document's, and the baseline follows
    (`barrier.rs:145-146` names this case; the convergence code is at
    `:290-310`). This is the model's `Converges`.
  - If the agent re-edited it, the path is dirty and publishes over the
    tree's own snapshot. The merge base (the old baseline) differs from
    theirs, so `merge` lists the path as `overridden`
    (`manifest.rs:1274-1282`). The baseline doesn't name the snapshot, so
    it isn't "integrated", and step 6 preserves a copy and records
    `commit-surfaced-foreign` (`barrier.rs:1188-1209`). This is the
    model's contested install: a conflict naming the snapshot.
  - If the agent deleted it, the delete applies over theirs and
    `commit-deleted-over-theirs` preserves the snapshot
    (`barrier.rs:1227-1241`, DeleteWinsPreserved).
  - Nothing is silent. The cost is one spurious conflict record naming the
    tree's own version.
- [ ] **Docs:** `lean/PROTOCOL.md:54` still says Edit mints "h derived from
  `baseline[p]`". Update it to the new rule.

### Fix 2: a commit's deletes are acknowledged (E21; `LeanP1.tla` `Install`, `udel`)

In the model, `udel` is the set of acknowledged deletes, from the gateway and
from a commit alike. It only grows. AN5's proof uses it: a delete is what
accounts for an acked version that is no longer cited. NR5 uses it too.

- [ ] **What a writer's commit acknowledges:** confirm the commit
  acknowledges each delete it performs, as the gateway's reply does. The
  code writes a tombstone for both (`manifest.rs:1288-1305` for a commit,
  `gateway/src/workspace.rs:874-913` for the gateway). But a tombstone is
  the model's `tomb`, not `udel`: it is cleared when the path is cited
  again, and pruned after `TOMBSTONE_KEEP_SEQS` (`manifest.rs:98`).
  Nothing durable corresponds to `udel`. The commit's only acknowledgement
  is `report.deleted` (`barrier.rs:1256`) and the trace. Decide whether
  that is acknowledgement enough. If it is, the model's reading is right.
  If it is not, AN5's delete clause rests on a record the code does not
  keep.
- [ ] **Docs:** `lean/PROTOCOL.md:32` maps `udel` to "the gateway's answers"
  for UI deletes only. Add the commit's deletes.

---

Evidence: `lean/formal/results/2026-10-07-tlaps-m5/NOTES.txt` (E27-E41,
M5C PROVED, M5D), `RESTATEMENT-PROPOSAL.txt`, and the `m5c/` and `m5d/`
proof modules.
