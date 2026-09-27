# Lean simplification (P1–P5 and lock-free commit): verification and final recommendation

I only read files. Nothing was edited, built or run.
- **[V]** means I checked it at the file:line given.
- **[I]** means I inferred it.

Line numbers are from the **working tree**, not HEAD. It has 19 uncommitted lean files (+6025/−3625), including `sync.rs`, `state.rs`, `sentinel.rs`, `untracked.rs` and `manifest.rs`.

## Verdict summary

| Proposal | Analyst's verdict | My verdict | What changed | Read path (the ~10x) |
|---|---|---|---|---|
| **P1 as written** (keep every generation) | reject; replace with "baseline is the merge base" | **Reject** | Confirmed. Chunks keep no history (`manifest.rs:917-923`). | Risk: adds a sequential hop to the manifest load (R9) |
| **P1-lite** (baseline = merge base; what the tree is owed is derived) | adopt-with-changes, after P2 | **Adopt, after P2**, with 4 additions (M2, M3, M4, M6 below) | The journal count was wrong: `keys` and `recent_uuids` are never read. The queue has two readers the analysis missed. It leaves one stuck state (a delete that never resolves). | Neutral for checkout. Better convergence once the owed fetches go through the parallel fetcher. |
| **P2** (the gateway commits directly) | adopt-with-changes, gated on a model run | **Adopt. This is the keystone. Gated on a LeanCore experiment.** | Storage figure wrong: chunk grace is 3600 s, not 600 s (`manifest.rs:801`). Missed a new read-path risk (M1). | Improves at rest. One new risk: frequent pointer moves plus the chunk reaper at large N. Mitigated by a retire-age rule. |
| **P3a** (rename copies to a fresh handle) | adopt now, on its own | **Downgrade: optional after P2, fallback if P2 fails** | Most of what it removes is also removed by P2 deleting repairs. Under P2 a citation move is already safe (see Interactions). | Neutral. A rename's source becomes a new 404 source for checkout. |
| **P3b** (a repair re-uploads) | reject | **Reject** | Confirmed | — |
| **P4** as stored (tombstones as versions) | reject | **Reject** | Its "semantic flip" risk is overstated. Today's delete-vs-edit already ends with the delete winning (M3). | Risk: manifest size grows with every path ever deleted (R9) |
| **P4 ⊥ row + delete the tombstone map** | adopt, inside P5 | **Adopt**, after P2 | — | Improves: one sequential GET fewer per manifest load |
| **P5 stage A** (outcomes as data, TraceCore) | adopt now | **Adopt now, first** | Strengthened: I found a live model-vs-code divergence it would close (M5) | Neutral; improves if sync fetches in parallel |
| **P5 stage B** (one narrow merge) | after P2 | **After P2 + P1-lite** | — | Neutral |
| **Lock-free commit** | enabled by P2 and P3 | **Defer. Last, if ever.** | It needs P2, not P3. It gives up the FIFO fairness you explicitly asked for. | Neutral |

## Verification

**Read-path baseline**
- **CONFIRMED.** The door-drill numbers match (`door-drill-2026-09-12.md:120, 320`). Host engine `small`: lean 5.29–5.93 s against uncached passthrough 63.25–69.24 s. As deployed: 22.86–24.90 s.
- **MISSING nuance.** Passthrough `--cache`, cold, took **32.11–34.91 s** (`:120`). Against a cached FUSE mount, lean's host-engine lead is about 6x, not 10x.
- **CONFIRMED.** Chunks are fetched sequentially (`manifest.rs:342-350`). The tombstones object costs one extra GET (`:393-417`).
- **CONFIRMED.** No per-file fsync (`barrier.rs:2574`). The comment at `checkout.rs:602` still says "create+write+fsync+rename"; it is stale.

**P1**
- **CONFIRMED.** Generations are not kept: `manifest.rs:917` ("Deliberately no history window"), `:789` (`KEEP_GENERATIONS`, legacy layout only).
- **CONFIRMED.** The rationale for splitting baseline from merge base is adoption (`state.rs:59-66`).
- **CONFIRMED.** `installed_etag` is an etag (`barrier.rs:2896-2904`). The model spells the same test as `jSeq = seq` (`LeanCore.tla:509-510`).
- **WRONG.** The claim that "keys and recent_uuids stay (AdoptOwn)" is false. Both are written (`barrier.rs:1266-1278`, `state.rs:638-649`) and never read; AdoptOwn is a leftover of the slot era. So P1-lite plus P2 leaves only `flush_uuid` and `carrier` in the journal, not four fields.
- **CONFIRMED.** The content-identity rule exists (`sync.rs:251-270`). It uses one HEAD and falls back to the manifest's CRC. "No HEAD needed under handles" is **[I]** but plausible.

**P2**
- **CONFIRMED.** `cas_manifest` requires a lease epoch (`workspace.rs` `require_current_epoch`, around 1239-1293). It is not a UI commit path.
- **CONFIRMED.** LeanSubtree cannot express P2:
  - the ASSUMEs at `LeanSubtree.tla:4173` and `:4185`;
  - `foreignQ` is empty when `InboxEnabled` is FALSE (`:3009-3016`);
  - `LeanDirectMergeInsufficient.cfg` sets `WriterQueue=FALSE, ImmutableObjects=FALSE`, and `check.sh:162` still expects it to violate.
- **CONFIRMED, and the experiment is bigger than stated.** LeanCore has no inbox or queue switch at all (constants at `LeanCore.tla:115-145`). The experiment needs new gateway actions plus a new rule constant; there is nothing to flip.
- **CONFIRMED.** The sweep spares cited, named, and own-flush handles, with a 600 s grace (`untracked.rs:231-243`, `inbox.rs:629`). Its own comment says the grace is "churn control, not a safety margin". Safety comes from the re-read under the lease, which the gateway does not do.
- **WRONG number.** The chunk orphan grace is `ORPHAN_GRACE_SECS = 3600` (`manifest.rs:801`). Worst-case transient chunk storage at 1 Hz autosave is therefore about 3.5 GiB, not 0.6 GiB. That is still storage the user accepts, but it also makes each reaper LIST longer.
- **CONFIRMED, and it strengthens P2.** Today's step 6 deletes retired handles immediately, "UNCONDITIONALLY, in one batch" (`barrier.rs:1822-1830`). R8 already rests entirely on re-resolve. P2's deferred collection with a grace G is a strict improvement, not just a mitigation.
- **Incomplete.** LeanCore guards `UIWrite` and `UIRename` on `~WindowOpen`, but not `UIDelete` (`LeanCore.tla:259, 278, 297-305`).

**P3**
- **CONFIRMED.**
  - A rename is a citation move (`workspace.rs:1026-1031`; the destination takes `key: Some(src.key.clone())`).
  - A repair re-cites by name (`barrier.rs:1392-1431`).
  - The step-6 "never cited again" claim holds only through the `still_cited`/`named` exceptions (`:1822-1858`).
  - `copy_object` and MPU part-copy exist (`flint-store/src/lib.rs:793`; `s3.rs:37-40, 231`, which has uncommitted changes).
- **Overturned (double counting).** P3's removals (KeepsAt, MovedElsewhere, PendingAdoption, `still_cited`) all exist only because repairs exist. P2 deletes repairs. See Interactions.

**P4**
- **CONFIRMED.** Upserts are inserted unconditionally, and `overridden` is computed only over `theirs.entries` (`manifest.rs:1051-1070`). Editing a path a peer deleted, where this tree never integrated the delete, silently resurrects it with no record.
- **PARTLY WRONG.** "Modify wins in both directions" is not the end state **[I, read, not run]**. When my delete meets their edit:
  - the first barrier outranks the delete;
  - `merge`'s `foreign` loop does not exclude `mine_deletes` (`manifest.rs:1053-1056`), so their edit is queued;
  - consume sees the local file absent, which counts as dirty (`barrier.rs:2368`);
  - `consume_preserve_dirty` preserves their bytes and sets the baseline to their version (`:948-985`);
  - the next barrier's delete then applies against a matching base.

  So today, **the delete wins, with a preserved copy, one barrier late**. `PROTOCOL.md:51` ("Theirs wins a delete/modify race") describes only the first barrier. A single "mine wins, theirs preserved" rule therefore matches today's end state in that direction.
- **CONFIRMED.** Checkout never uses tombstones but loads them (no match in `checkout.rs`; `manifest.rs:393-417`).

**P5**
- **CONFIRMED.**
  - `TraceLean.tla:22` extends LeanSubtree.
  - `ndjson2tla.py:301` pins `ImmutableObjects: False`.
  - The trace directory was last changed in `843a668c` (2026-09-18).
  - There is no proptest or quickcheck.
- **CONFIRMED, and worse than stated (M5).** `delete_applies` (`barrier.rs:2830-2831`) re-derives `theirs_unchanged` (`manifest.rs:1071-1078`), and does so against a **different base** from the one the merge uses (M5).

## Interactions

1. **P2 is the keystone.** It deletes adoption, and with it:
   - repairs and pending adoption;
   - the KeepsAt class (L-122 ×3, L-119);
   - the reason baseline and merge base must be kept apart (`state.rs:59-66`);
   - the only reason tombstones carry information (H1e).
2. **P2 makes most of P3 unnecessary.** Once repairs are gone, a handle is cited only if it was freshly minted or carried forward from the current document, and a gateway rename moves `doc[p]` to `q` in **one** CAS. So `Inv_NoRecite` holds with citation moves. The collector's rule is then simply `Handles(old) \ Handles(new)`, which is what `barrier.rs:1854-1856` already computes, minus `named`. P3a adds only `Inv_HandleAtItsPath`, a nicer proof shape, and pays for it with O(bytes) renames, an async verb and more mints in the model. **P3a is P2's fallback, not its complement.** If the P2 experiment fails, P3a alone still removes the aliasing half of the KeepsAt class.
3. **P1-lite requires P2.** Without it, an adopted but uncited version makes `baseline ≠ doc` point the wrong way (README item 3).
4. **P4's stored half has nothing left to pay for after P2**, so delete tombstones rather than promote them. "Rename = copy + tombstone" is P3a plus P4-as-stored; neither is needed, because P2's single CAS is atomic. The ⊥ row belongs in P5's merge table.
5. **The lock-free commit needs P2** (no re-cite) plus a condemned set, **not P3**. The P3 analyst says P3a alone leaves a same-path re-cite; the P2 analyst says P2 alone is enough. I agree with both. It also gives up the FIFO ticket, which you asked for ("one should not face starvation").
6. **The all-at-once combination breaks the read constraints.** Keeping generations (P1 as written), plus tombstone versions (P4 as stored), plus copies (P3a) would:
   - grow the manifest with deletion history, adding sequential chunk GETs to every checkout (R9);
   - add per-seq snapshots and a retention floor tied to writer liveness.

   The recommended set (P2 + P1-lite + P5 + tombstone deletion) adds **no** bytes to entries and **no** hop to the manifest load.
7. **One rule serves P1, P2 and R8 together:** a retire-age grace G for both **handles and chunks** (M1). It also gives readers a bounded history, so a reader that lags less than G can diff chunk lists (the P1 read win) without any history design.

## Missing

- **M1 (read path, P2 × chunk reaper).** The chunk reaper judges age from when a chunk was **written**, not from when it was superseded (the HEAD-age check at `manifest.rs:1001-1006`), and it runs in every writer publish (`barrier.rs:1990`).
  - Under P2, every UI write moves the pointer. A long-written chunk that one of those writes supersedes becomes reapable at the next publish.
  - A checkout's sequential manifest load (about 6 s at 1M entries) then hits a 404 and restarts. It gives up after `LOAD_ATTEMPTS=3` (`manifest.rs:308, 463`), and the checkout fails.
  - Today the pointer moves only per writer barrier, so this is rare.
  - Fix: judge a chunk's age from its retirement (the same retire-age list as handles), or fetch chunks in parallel. Neither analysis saw this.
- **M2 (P1-lite).** The foreign queue has two readers besides consume:
  - the H10 ack carrier (`sentinel.rs:784-795`, "queued deletes");
  - the D5 ticker (`sentinel.rs:943-955`, `waiting = queue non-empty || installed_foreign`). The ticker "never issues a request of its own".

  P1-lite needs a persisted derived fact, "owed set empty as of seq X", written when the owed set is applied. That is a cache, and it must be modelled as a sensor that can lie.
- **M3 (P1-lite gets stuck).** Take base = baseline = v1, theirs v2, and the tree deleted p.
  - The merge outranks the delete.
  - owed(p) is false, because the tree is dirty.
  - Nothing preserves v2, and every barrier repeats this.
  - Result: the tree lacks p while the document cites v2, forever, with no record. Today `consume_preserve_dirty` resolves it.
  - P1-lite needs an explicit ⊥-vs-version row, which is the P4/P5 policy decision.
- **M4.** Checkout's S3-wins arm (`checkout.rs:578-589`) is a repair source: it adopts bytes no manifest cites. It must be **deleted**, not merely made rare, for P2's no-re-cite rule and P1-lite's "baseline holds only cited versions" to hold.
- **M5 (a live divergence, [I] unrun).** `void_stale_repairs` is handed `baseline.inst_base` (`barrier.rs:1637`). `merge_onto` substitutes `own_base` when `installed_etag` matches (`:2896-2904`). LeanCore's `KeepsAt` uses `Foreign`, i.e. `MergeBase`, the own-base-aware one (`LeanCore.tla:555-559`, `:509-511`).
  - In the restart branch the code can decide "delete not applied, repair yields" where the model says the repair proceeds.
  - This is a fourth instance of the declared-vs-applied class, and the model does not describe what the code does.
  - It needs a test or model world, and it is the concrete reason to do P5 stage A first.
- **M6.** `keys` and `recent_uuids` are dead journal fields (see Verification).
- **M7.** "10x over FUSE" is ambiguous. Host engine against uncached FUSE is 10.7–13.1x. Against cached FUSE, cold, it is about 6x. As deployed it was 2.7–3.0x (loop image, pre-`1037d55a`). Warm re-reads are unmeasured for lean. The constraint needs one named, deployed measurement.
- **M8.** The gateway's read door does a full `manifest::load` plus an inbox load for every `get_file` (`workspace.rs:910-917`), and it skips CRC verification. Under P2 the gateway is a publisher and can cache the manifest keyed by pointer etag. That is a UI-read win no analysis claimed.
- **M9.** Under P2, UI→tree latency becomes two ticks: pull-only queues the change, and the next consume applies it. The idle path already scans every tick (`barrier.rs` around 1173) and loads the full manifest on any pointer move (`:1453`). That cost is bounded by the tick rate, so it is neutral, but same-tick pull-then-consume must be specified.

## Recommendation (ordered)

**0. Take the baseline measurement first.**
- Run the door-drill `small` leg (20k × 8 KiB) as deployed on HEAD (plain-directory tree):
  - lean;
  - passthrough without cache;
  - passthrough `--cache`, cold and warm.
- Record the `reresolved` count and any manifest-load restarts.
- Add a 1M-entry manifest-load timing for R9.
- Run three pairs and quote ranges. Everything later is judged against this.

**1. P5 stage A now (code only, no protocol change).**
- Make `merge` return per-path outcomes.
- Make `void_stale_repairs` read them, which closes M5.
- Add a test pinning today's delete-vs-edit end state (M3 and the P4 correction).
- Start the `TraceCore.tla` skeleton.
- Kill criterion: none; it is cheap. If the M5 test shows an acked write not cited, that is a defect to fix in any case.

**2. LeanCore sandbox for P2, in `lean/formal/pending/`.** Leave LeanSubtree alone while r34 runs.
- Model `GPut`/`GCas` (split), `GWrite`/`GDelete`/`GRename` as single CAS steps, acks after the CAS, the queue on, repairs and S3-wins **deleted**, and a `retiredYoung` ghost.
- Must hold: `LeanCoreGatewayHolds`, for 1 path × 2 writers and for 2-path rename.
- Must fire:
  - `GatewayNoQueue`, the direct-merge refutation on the handles shape. It is the positive control; if it does not fire, the holds result is vacuous.
  - `GatewayCollectWithRepair`
  - `GatewaySweepNoGrace`
  - `GatewayRebaseBlind`
  - `GatewayRenameTwoCAS`
- Kill criterion: `GatewayHolds` violates `Inv_AckedNamed` or `Prop_NoSilentRevert`, and fixing it needs more than one new rule. Then abandon P2 and fall back to P3a + P5.

**3. P1-lite in the same sandbox, once P2 holds.**
- Remove `instBase`, `queue` and `jSeq`.
- Add a guarded `ApplyOwed`, a `ContentConverged` step, a `Restart` action, and the M3 row.
- Must fire:
  - the L-123 regress with the guard removed (MaxGen ≥ 3);
  - a P1-without-P2 world, which is the README item-3 control;
  - `Owed ⊆ Scope` with the scope filter removed;
  - `LeanCoreForeignFlat`.
- Kill criteria:
  - `NoRegress` or crash recovery needs a persisted per-path intent. That rebuilds the journal, so keep the queue with the L-123 fix.
  - M3 cannot be resolved without stored state.

**4. Retire-age G for handles and chunks.**
- Model it as a ghost.
- Measure a deployed `small` checkout, at 20k and at a large N, with UI saves at a person's pace against files in the checkout set.
- **1 Hz autosave leg DROPPED (2026-09-25).** The contract is now that an editor autosaves to a draft and commits only when the person saves (`lean/gateway/README.md`, "What a write is"), so commit-per-second is not a supported load.
- Kill criterion for any read regression: slower than the step-0 range across three pairs, or `reresolved > 0` at rest.

**5. Implement P2, P1-lite, the deletions, P4's ⊥ row, and tombstone removal.**
- Delete repairs, the S3-wins arm, and the dead journal fields.
- Re-run step 0's legs. (The autosave leg is dropped; see step 4.)

**6. P5 stage B.**
- Build an exhaustive table over `{Nil, v1, v2, v3}` per path.
- Generate the table's expected outcomes by evaluating the TLA operator with TLC, never from the Rust.
- Record handle-level TraceCore traces of the shipped shape.

**7. Optional.**
- P3a, only if you want `Inv_HandleAtItsPath` and accept O(bytes) renames.
- The lock-free commit, only if FIFO fairness can be dropped or given a separate liveness fallback.

## Simplified protocol

| Today (rules) | After P2 + P1-lite + P5 + tombstone removal |
|---|---|
| UIWrite / UIRename / UIDelete append to the cell. They are judged later: saw / supersede (clock-ordered) / stale / answered / refused, plus the window guard. | **Publish(party):** load D, `result = Merge(baseline, mine, D)`, CAS on D's etag, retry by re-merging. The gateway is a party: it PUTs a fresh handle, publishes, and **acks after the CAS**. If-Match gives a synchronous 412. |
| Install merges against `inst_base`, or `own_base` when `installed_etag` matches. | One merge per path over Handles ∪ {⊥}, as a table: `m=b → t`; `t=b → m`; `m=t → m`; otherwise mine wins, theirs is preserved with a copy and a record. The ⊥-vs-version row is your choice. |
| R7 surfacing is a per-path re-check of the baseline key. | Surfacing is the merge's conflict row (base = baseline). |
| Repairs (RepairCandidate, KeepsAt with 3 conjuncts, MovedElsewhere, SupersededByUI, Entombed, PendingAdoption) | **None.** No party cites a handle it did not mint or carry forward from D (`Inv_NoRecite`). |
| Collect spares cited-elsewhere and entry-named handles and deletes at once. Sweep spares named/own/grace. | **Retire** = Handles(D) \ Handles(result). Deleted only after retire-age G. Chunks follow the same rule. The sweep takes handles that are uncited, not in the retire list, older than the grace, and not its own in flight. |
| The foreign queue, `installed_foreign`, `installed_etag`, the `inst_base` merge base, sync's hidden overlay, and the L-123 prune | **Owed(p)** = p ∈ scope ∧ doc[p] ≠ baseline[p] ∧ the tree is clean at p. Applied only while baseline[p] = from, through the parallel fetcher, with a local CRC and a fresh-stat recheck. **Converged:** dirty bytes whose CRC and size equal doc[p] are adopted. |
| The tombstone map (H1e, 10k seqs), the tombstones GET on every load | Gone. A delete is ⊥ against a complete baseline. |
| Declared removals, refusals, answered records | A gateway CAS edit set. Shape rules L-120/L-121 are checked at the CAS. Conflicts go to a bucket-side log. |
| Reader tick: inbox + pointer (2 GETs) | Pointer only (1 GET). The cell keeps only `boundary_request`/`sync_request` and the advisory window. |
| Journal: flush_uuid, keys, recent_uuids, installed_etag, installed_foreign, declared_deletes, carrier | flush_uuid, carrier, plus a derived "owed-empty@seq" cache (M2) |
| Checkout (unchanged) | Unchanged: one If-Match GET per file, sharded fan-out, local CRC, one syncfs, no lease. |
| Kept in both | Writer lease and FIFO ticket, the two-scan delete guard, ScopeIntent, and invariants `Inv_CitationsLive`, `Inv_OneName`, `Inv_AckedNamed`, `Prop_NoSilentRevert`, `Inv_OneHolder`. New invariants: `Inv_NoRecite`, `Owed ⊆ Scope`, `NoRegress`. |

## Open questions for the user

1. **Which "10x" must hold?** The host engine against uncached FUSE is 10.7–13.1x. Against cached FUSE, cold, it is about 6x. As deployed it was about 3x before the plain-directory tree. The step-0 re-measurement decides what "must not regress" means.
2. **Your file against a peer's delete: which wins?** Today the delete ends up winning with a preserved copy, one barrier late, in one direction, and the edit silently wins in the other. One explicit rule is needed either way.
3. **A UI rename or delete of a path the agent has dirty.** Today it is refused. Under P2 it lands in the document, the agent's publish then wins, and a conflict record is written. Do you accept that?
4. **Rename cost (P3a).** Do you want copy-on-rename, which means O(bytes) and multi-GB renames going async, for a cleaner proof? Under P2 it is not needed for safety.
5. **Fairness.** Is the FIFO guarantee still required? If yes, the lock-free commit stays off the table.
6. **Retention G** for retired handles and chunks (for example 1 h at 3.5 GiB worst case per busy workspace). Acceptable?
7. **Sole-writer (mirror) workspaces.** Should the gateway refuse UI writes to them under P2?
## Requirements the P2 sandbox model must state and check (user, 2026-09-24)

P2's purpose is that **a UI save stays interactive**. These are requirements
of the model, each with a world that must hold and a known-bad world that
must break. A holds result without its broken twin doesn't count.

**G1. A UI save never waits on the lease.**
- **Structural:** no gateway action (`GPut`, `GCas`, and the rename and
  delete variants) has `holder`, the lease cell, or the commit window in its
  enabling condition. Today's `UIWrite`/`UIRename` guard `~WindowOpen`
  (`LeanCore.tla:259, 278`) is exactly what G1 removes. The review of the
  sandbox must show every gateway guard.
- **Behavioural:** a `Stall(s)` action freezes the lease holder forever, still
  holding the lease: no action of `s` is fair and the lease never expires in
  the world.
  - Property (liveness): `UISaveStarted(p) ~> UISaveAcked(p)`, with weak
    fairness on the gateway's actions only.
  - **Must hold:** `GatewayStalledHolder` (a UI save started while the
    holder is stalled).
  - **Must fail:** `GatewayWaitsOnLease`, where the gateway CAS keeps
    today's `~WindowOpen` guard. The liveness property must be violated
    there: that's the positive control.

**G2. Under heavy saving, the pressure moves to the writers, never the
reverse.**
- The gateway's CAS retries only against a pointer that moved. Writers are
  serialized by the lease, so at most one writer CAS races it. A writer that
  loses re-merges and retries, within its bounded attempt count; after that
  the barrier is refused and retried later. That cost is paid by a writer's
  barrier, never by the UI.
- **Liveness under saving:** `UISaveStarted ~> UISaveAcked` must still hold
  while writers keep committing, given fairness on the gateway alone.
  - If TLC shows a gateway starved by writer CASes, the design needs an
    explicit yield rule, for example a writer that sees a pending UI save
    re-merges it before its own CAS. That rule then becomes a protocol rule
    with its own known-bad world.
- **Safety unchanged under the pressure:** `Inv_AckedNamed`,
  `Inv_CitationsLive` and `Prop_NoSilentRevert` hold when writer barriers
  are refused mid-flight.
- **Measured, not modelled:** the writer CAS loss rate and barrier refusals
  under UI saving. The 1 Hz autosave version of this was DROPPED
  2026-09-25: autosave goes to drafts (see step 4), so saves arrive at a
  person's pace.

**G3. A stalled holder never hurts a UI save's durability or its handle.**
The gateway uploads its fresh handle before its CAS. An *active* sweeper must
not take it in that window (the critic's `GatewaySweepNoGrace` must break
without the grace G). A stalled holder does nothing, so G3 is about the next
holder after the lease expires: that is modelled as a separate world, with
expiry and deposal enabled.

What stays blocked, knowingly: writers behind a stalled holder wait for lease
expiry, as today (M3). The requirement puts that cost on writers only.
