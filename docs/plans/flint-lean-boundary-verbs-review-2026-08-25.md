# ultracode review — docs/plans/flint-lean-boundary-verbs-plan.md

**Run:** `wf_4bf110db-4fe` (resumed) · 6 reviewers · 67 raw findings · dedup → 56 distinct · top 8 adversarially verified

Reviewer dimensions: crash/takeover, plan-vs-code drift, formal-model coverage, security/DoS, ops-fleet knob wiring, coherence/residuals.


**Read the tiers literally.** Tier 1 findings survived an adversarial verifier whose default stance was that the finding is wrong. Tier 2 was refuted — recorded so it is not re-found. Tier 3 was never routed to a verifier (the run caps verification at 8); those are leads with reviewer-claimed evidence only.


---

# Tier 1 — CONFIRMED (survived adversarial verification)


## [C1] CRITICAL — The preStop drain skips its final barrier/citation whenever a sync sentinel ack was settled — everything since the last boundary is forfeited

**Plan section:** §2.4.4 (D10 rule 1) / §10 status table row "3 — run-loop wiring"  ·  **Reviewer:** coherence-residuals  ·  **Independently reported by 2 reviewers**  ·  **Verifier confidence:** high


D10 says the drain "runs fused and cite-everything in every mode: all pending citations installed by CAS, all dirty files uploaded and cited, regardless of boundaries — then lease release", and §10 marks it done ("the preStop drain cites everything staged in place"). The shipped drain only runs the declared barrier/citation when NO settled ack carries a seq: `if !acks.iter().any(|a| a.seq.is_some())`. `honor_sync` always returns `seq: Some(report.seq)` (the manifest it synced against, publishing nothing). So a pending `.flint/sync` record standing at SIGTERM — reachable any time a consumed sync sits inside `sentinelMinIntervalSecs`, or indefinitely while the publish budget is exhausted — makes the drain honor the sync, skip the final barrier, and release the lease. In hybrid/cadence every byte dirtied since the last floor tick is never published; in gated the entire staged-uncited set is never cited (recoverable only by the manual `recover-staged`). This is a regression against pre-plan behaviour, where SIGTERM always ran a final barrier, and it breaks the §7b "Availability — degraded-to-today, never worse" claim. `floor_tick` states the exact reasoning error and avoids it ("a sync ack carries a seq … but publishes nothing, so treating it as 'the floor ran' would skip cadence for a tick — a silent RPO regression"); the drain, where there is no next tick, uses the weaker test. No test or drill leg covers it: both drain tests start with no pending sentinel, and B11's oracle asserts only `staged_uncited_count > 0` before SIGTERM, never an owed ack.


**Evidence:** /Users/ddalton/github/flint/lean/sidecar/src/sentinel.rs:973 `if !acks.iter().any(|a| a.seq.is_some())`; sentinel.rs:696 `seq: Some(report.seq)` (honor_sync); contrast sentinel.rs:858-862 `// Only a PUBLISH honor discharges the floor's own barrier … let mut published = false`; plan lines 217-223 (D10 rules 1-3); plan line 573 (§10 "the preStop drain cites everything staged in place"); tests /Users/ddalton/github/flint/lean/sidecar/src/tests.rs:2434 and :3201 both drain with no pending sentinel; §6 B11 row (plan line 390).


**Verifier:**

CONCEDED — the mechanism, the reachability and the consequence all check out, and nothing in the plan (§3 residuals, §9 ledger, §10/10.1/10.1b/10.1c/10.1d) discloses it.

Mechanism, verified line by line:
- /Users/ddalton/github/flint/lean/sidecar/src/sentinel.rs:973 `if !acks.iter().any(|a| a.seq.is_some())` gates the drain's final `declared_barrier()` (non-gated) / `declared_lane() + citation_pass(Drain)` (gated).
- sentinel.rs:696 `seq: Some(report.seq)` — `honor_sync` unconditionally returns `Some`: `SyncReport.seq` is a plain `u64` (sync.rs:39), so it can never be `None`. `sync_scoped` (sync.rs:97-…) pulls remote truth and applies it; it uploads nothing.
- The drain (sentinel.rs:945-988) loops `[Verb::Sync, Verb::Publish]` calling `honor_pending(verb, true)`. `forced: true` bypasses `sentinel_due()` (sentinel.rs:548-552), so a standing sync pending IS honored, lands in `acks` with `seq: Some(..)`, and the guard at :973 therefore skips the final barrier/citation entirely. `bin/flint_sync.rs:344-364` calls only `sc.drain()` on SIGTERM, and `lease::release` (lease.rs:137-148) publishes nothing — so nothing else covers it.
- The identical reasoning error is explicitly named and avoided 115 lines earlier in the same file: sentinel.rs:858-862 "Only a PUBLISH honor discharges the floor's own barrier: a sync ack carries a seq (the manifest it synced against) but publishes nothing, so treating it as 'the floor ran' would skip cadence for a tick — a silent RPO regression", with `floor_tick` using a `published` flag set only for `verb == Verb::Publish && a.status == "ok"`. The drain uses the weaker `seq.is_some()` test at the one place there is no next tick.

Reachability (sync pending standing, publish pending absent, at SIGTERM):
- `charge_budget` (sentinel.rs:317-332) sets `last_honor_unix = now` even for a sync honor (`charge_budget(0)`), and `sentinel_due` (:335-347) is verb-agnostic, so any sync consumed within `sentinelMinIntervalSecs` of the previous honor returns `Due::MinInterval` and the pending stands (~5 s at defaults).
- Budget exhaustion returns `Due::BudgetDeferred`, and the pending then stands until the next floor tick — bounded by `floor_secs`, i.e. 60 s at defaults but up to an hour on the plan's own 1-hour-floor drill fixture (§6). (The finding's "indefinitely" is an overstatement — `floor_tick` does force-honor it — but this does not change the outcome; it only sizes the window.)

Regression vs pre-plan: `git show cfb18e20:lean/sidecar/src/bin/flint_sync.rs` — the pre-tranche SIGTERM arm ran `sc.run_barrier()` unconditionally. So this state now forfeits a final publish that always ran before, contradicting plan line 217-219 (D10 rule 1: "all dirty files uploaded and cited, regardless of boundaries"), plan line 573/577 (§10 "3 — run-loop wiring … done | the preStop drain cites everything staged in place"), and §7b's "Availability — degraded-to-today, never worse" (plan line ~441). In hybrid/cadence the loss is every byte written since the last floor tick; in gated the whole staged-uncited set stays uncited (bytes survive as versions but are invisible to checkout/DR per §3 residual 3, recoverable only by the manual `recover-staged`), plus anything dirtied since the last lane tick is lost with the emptyDir on pod replacement — the routine event on this pure-spot fleet.

Coverage gap confirmed: `grep -n "drain()" tests.rs` returns exactly two call sites — tests.rs:2434 `gated_drain_cites_the_staged_versions_in_place` and tests.rs:3201 `the_drain_carries_a_delete_made_before_it` — and both construct their fixture with no pending sentinel of either verb, so the guard at :973 is never exercised with `acks` non-empty. B11's anti-vacuity guard (plan line 391) asserts only `staged_uncited_count > 0` before SIGTERM, never a standing owed ack.

Severity: critical by the stated rubric — reachable silent data loss on the one path whose entire purpose is not to lose data, against the plan's own D10 rule 1 and its no-regression claim, with the plan asserting the row "done".


## [C2] CRITICAL — Gated mode has no HITL citation-repair pass: a consumed HITL write is never re-cited, so it stays invisible to every pinned reader indefinitely — D13's 'within one floor' promise is unimplemented

**Plan section:** §2.4.2 (D13 + HITL exemption)  ·  **Reviewer:** crash-takeover  ·  **Verifier confidence:** high


D13 (plan line 179) promises 'HITL writes reach [pinned readers] through the ungated repair pass, i.e. within one floor', and the HITL exemption (line 184) says 'Repairs ride the next citation pass or, if none is due within one floor, trigger a repair-only citation (no local upserts)'. Neither exists in the gated path. The repair machinery lives only in the fused barrier (`repair_candidates`, barrier.rs:447-465, plus the `repairs_pending` fast-path guard at 319-328) — and gated mode never runs the fused barrier: the floor arm runs `gated_tick` (lane + citation), the sentinel honor runs `declared_lane` + `citation_pass`, the drain likewise. The citation's upserts are built exclusively from `stage.entries` (gated.rs:440-486), and `citation_due` returns None on an empty stage with the comment 'A repair-only pass is still driven by the ordinary barrier path' (gated.rs:376-378) — a path gated mode structurally cannot reach. Walkthrough: gateway acks a HITL PUT; the next lane tick consumes it (adopting it into the baseline and tree, dropping the inbox entry); the file is now clean-vs-baseline so it never enters the stage; the manifest keeps citing the OLD (version_id, etag) forever. Under `pinned_reads` every checkout, sibling sync, and DR re-materialization resolves the old cited version — the acked write is permanently invisible to exactly the readers D13 governs, with no conflict record and no gauge (`repairs_pending` is computed only in barrier_inner). On pod replacement the successor checks out pre-HITL bytes; the agent's later edit of that path 412-parks against the HITL version (foreign uuid), surfacing a conflict only then. The §10.1c fix makes this worse in its own scenario: the dropped-from-boundary path 'stays queued and the next lane consumes it the ordinary way' — which, if the path is clean by then, terminates in this same never-re-cited state. §10 marks Phase 3 done and lists only the heartbeat echo and drain sizing as open; `hitl_admitted_between_citations` (tests.rs:2098) tests admission only, never re-citation, and no test asserts a consumed HITL write becomes cited in gated mode.


**Evidence:** gated.rs:373-379 (citation_due's 'ordinary barrier path' comment — unreachable in gated), gated.rs:437-486 (upserts from stage.entries only), gated.rs:210-330 (lane consumes inbox, adopts to baseline, drops entries, stages only scan-dirty paths); barrier.rs:447-465 (repair pass exists only in barrier_inner); sentinel.rs:889-921 (gated floor arm calls gated_tick, never run_barrier); plan lines 179, 184, 580 (§10 open-remainder row omits it); tests.rs:2098-2122 (admission-only test).


**Verifier:**

I tried to kill this three ways — looking for a gated repair arm, for a route that re-stages a consumed HITL path, and for a place the plan already concedes it — and all three failed.

1) The repair machinery exists ONLY in the fused barrier. `repair_candidates` is computed inside `barrier_inner` (barrier.rs:454-465, guard at :320-328) and re-cites paths where `baseline.entries[p].etag != inst_base[p]`. `citation_pass` builds `upserts` exclusively by iterating `stage.entries` (gated.rs:440-485); nothing in it consults `baseline` vs `inst_base`. `citation_due` returns `None` on an empty stage with the comment "A repair-only pass is still driven by the ordinary barrier path" (gated.rs:373-379).

2) Gated mode structurally cannot reach that path. Floor arm: `sentinel.rs:894` calls `gated_tick`, `run_barrier` is only in the non-gated `else` (sentinel.rs:912). Sentinel honor: `honor_publish_gated` = `declared_lane` + `citation_pass` (sentinel.rs:649-650). Drain: `declared_lane` + `citation_pass` (sentinel.rs:981-985). The run loop (bin/flint_sync.rs:298-325) dispatches only `heartbeat_tick` (which just renews, sentinel.rs:830-838) and `floor_tick`. The only `run_barrier` left is the one-shot `flint-sync barrier` CLI (bin/flint_sync.rs:186), which the state-dir flock forbids beside a live `run`.

3) The walkthrough holds. `consume_inbox`'s clean-adopt branch writes the file and stores a baseline entry with the file's real size/mtime (barrier.rs:203-215), so the path is clean at the next `scan::classify` and never enters `to_stage` (gated.rs:240-247); the lane then drops the inbox entry (gated.rs:222-225). The manifest keeps the old `(etag, version_id)`, and the citation's baseline rewrite even re-pins `inst_base` to the installed manifest (gated.rs:690-691), so the divergence persists silently. Consequences I verified in code, not assumed: a pinned checkout resolves `get_version(cited)` exclusively (checkout.rs:145-152 — with a comment at :142-144 literally asserting "HITL writes still reach readers, through the ungated repair pass, within one floor", machinery that does not exist there); a sibling `sync` does the same under `pinned_reads` (sync.rs:212-222); and the gateway's own `GET /files/{path}` prefers the cited etag, so it 412s→409 "moved" for the very write it just acked (gateway.rs:396-411). Healing requires the agent to happen to rewrite that path (and then it 412-parks against the foreign version) or a manual `recover-staged`. After `noncurrentRetentionDays` the cited-but-noncurrent version is reaped and checkout refuses (checkout.rs:150-160) — the D8 endgame, reached with no agent error at all. Bytes are never destroyed (the reaper iterates only `upserts.keys()` and skips `is_current`, gated.rs:637-668), which is the one mitigating fact.

4) The plan does not handle it anywhere. §2.4.2 line 179 (D13) and line 184 (HITL exemption) promise the repair pass and a repair-only citation "within one floor"; §5 line 357 lists it as a Phase-3 deliverable; §10 line 580's remainder row names only the heartbeat echo and drain sizing. §3 residuals (3, 11), §9 (CT-3/SD-5, MD-3) and §10.1c/10.1d do not mention it — 10.1c's fix ("the entry stays queued and the next lane consumes it the ordinary way") terminates in exactly this state, and 10.1d's `Inv_AckBoundaryCoherent` names the hazard ("a citation repair still owed at ack time... a reader resolves to bytes already superseded here") only for the cadence path.

5) Neither gate covers it. tests.rs:2098-2120 asserts admission only and never consumes or cites the HITL files; no battery leg asserts a consumed HITL write becomes cited under gated. In the model, `CitePassStep` has no repair arm (LeanSubtree.tla:1126-1157) unlike `Install` (:832-839), `Inv_HITLDurable`'s amputation predicate only fires when a cited path in `sub` names a different generation (:1148-1153, :854-860) so "consumed but never cited" is outside it, `ProbeHITLCited` runs with `GatedCitation = FALSE`, and no .cfg sets `GatedCitation = TRUE` together with `SentinelEnabled = TRUE` (checked all Lean*.cfg).

Critical rather than major: the plan's own decided rule (D13 + the HITL exemption) is unimplemented while §10 marks Phase 3 done, and the shipped result is an acked user write invisible to every manifest-resolving reader indefinitely, with no conflict record and no gauge (`repairs_pending` lives only in `barrier_inner`; gauges.rs never reads `inst_base`) — the amputation class §10.1c says the design exists to make impossible.


## [C3] CRITICAL — A stale withheld tombstone amputates a re-created file at the gated citation: delete-then-recreate before a citation installs a boundary that omits a file present (and staged) on disk

**Plan section:** §2.4.1 / §2.4.2 (withheld deletes)  ·  **Reviewer:** crash-takeover  ·  **Verifier confidence:** high


The lane inserts confirmed-absent paths into the persisted `stage.withheld_deletes` (gated.rs:300-302) and nothing ever removes a path from that set when it reappears — the only clear is the wholesale one after a successful citation (gated.rs:698). Sequence: agent deletes P (cited at etag E0), two lane ticks pass (withheld_deletes += P), agent re-creates P, a later lane tick stages the new bytes (P now in stage.entries AND withheld_deletes), then a citation fires. `manifest::merge` applies upserts first and deletes second, and the delete arm removes the entry unconditionally when `theirs[P].etag == base[P]` — it never checks `mine_upserts` (manifest.rs:186-204). Net: the installed manifest omits P even though P exists on disk, was staged this pass, and — on a sentinel-declared boundary — is covered by an ok ack: `Inv_AckImpliesCited` violated (P's pre-boundary bytes are cited nowhere), which is the plan's own critical rubric. Downstream, a sibling whole-tree or in-scope sync sees P gone from the manifest and, P being locally clean there, DELETES its copy (sync.rs:262-295) — real byte destruction on siblings — before P reappears at the following citation when the author's lane re-stages it (via AdoptOwn on its own flush_uuid). The window is wide in gated mode: citations can be `visibilityLagBoundSecs` apart, and delete-then-recreate is a routine agent shape (checkpoint rotation, write-temp-swap workflows that briefly remove the target). The fused path is immune because deletes are recomputed per-scan, never persisted — merge's implicit upserts∩deletes=∅ contract held there and the gated caller silently breaks it. The plan's step sequence (§2.4.1 upload lane, §2.4.2 withheld deletes) never states a tombstone-cancellation rule, and no test or drill leg (B9-B21) exercises delete-then-recreate across a citation interval.


**Evidence:** gated.rs:300-303 (insert-only lifecycle), gated.rs:529 (`let deletes = stage.withheld_deletes.clone()` fed to merge), gated.rs:698 (only clear is post-citation); manifest.rs:186-204 (upserts inserted, then `merged.entries.remove(p)` for deletes with no mine_upserts check); sync.rs:262-295 (sibling remote-delete arm applies on locally-clean paths); plan lines 58 (D1 at-least guarantee), 182 (withheld deletes rule with no reappearance clause), §4 product 2 invariants (`Inv_AckImpliesCited`).


**Verifier:**

I tried to kill this on four routes and it survived all of them; one of them turned into the strongest evidence FOR it.

1. Lifecycle claim — CONFIRMED. `stage.withheld_deletes` is insert-only: the sole insert is gated.rs:300-302 (`for p in &classified.deletes { stage.withheld_deletes.insert(...) }`), the sole clear is gated.rs:698 (post-citation), and it is persisted in `PendingStage` (gated.rs:87) across lane ticks via save_stage/load_stage. `grep -n withheld gated.rs` returns no removal anywhere. The citation feeds it straight through: gated.rs:529 `let deletes = stage.withheld_deletes.clone();` → gated.rs:565 `manifest::merge(base, &theirs, &upserts, &deletes, &parked)`.

2. Merge amputation — CONFIRMED at manifest.rs:186-204: upserts are inserted into `merged` (186-188), then every `mine_deletes` path is removed (189-204) when `theirs_unchanged`. `theirs` is the pre-merge bucket document, so for a path cited at E0 with `base[P]==E0` the delete fires and `merged.entries.remove(P)` erases the entry the upsert just installed. There is no `mine_upserts.contains_key(p)` guard.

3. Reachability — CONFIRMED. scan.rs:110-129: absence at two consecutive scans ⇒ `deletes`; the gated lane never removes `baseline.entries[P]` on a withheld delete (only `report.deleted` does, gated.rs:679-681), so on re-creation classify sees size/mtime drift ⇒ `uploads` ⇒ `to_stage` (gated.rs:242-253) ⇒ `stage.entries[P]` with `base_version_id == V0`. At the citation the D7 stale-base check passes (`cited_now == pe.base_version_id`, gated.rs:441-443) and the HITL-inflight drop doesn't apply, so P is in BOTH `upserts` and `deletes`. Net: the installed manifest omits a file that exists on disk and was staged in that very pass.

4. The killer: the ack. gated sentinel honor is `declared_lane()` + `citation_pass(Sentinel)` and unconditionally acks `status: "ok"` with `uploaded: lane.staged.len()` (sentinel.rs:644-673). So the ack literally reports P as uploaded at seq S while the manifest at S does not cite P — a direct violation of `Inv_AckImpliesCited`/`Inv_AckBoundaryCoherent` as refined in §10.1d (P was locally dirty at consume, so it is inside the promise, not outside it). This is the exact mirror of §10.1d defect 1 ("a sentinel ack claimed a boundary that withheld the agent's delete", commit 8836f1ce), and strictly worse: that one left a stale extra citation, this one removes a live file from the manifest.

5. Attempted refutation via the formal model — BACKFIRED. LeanSubtree.tla:1183-1186 computes `dels == {p \in withheldDel[s] : manifest[p] # 0 /\ p \notin sc[s].citeDone /\ ...}`. The `p \notin citeDone` clause IS the tombstone-cancellation rule, and the model has AgentWrite/AgentDelete (1510) with `withheldDel' = @ \cup scanD` persisted across lane passes (1087), so the interleaving is expressible. The model is correct where the code is not — the green tranche-3 gate proves nothing here, it is model-vs-code drift, and the plan (§2.4.1 upload lane, §2.4.2 "Withheld deletes and GC", line 182) never states the rule the model relies on.

6. Attempted refutation via "the plan handles it elsewhere" — it does not. §3 residual 11 is raw-key visibility; §9 ledger, §10.1c (HITL-inflight + keep-current reaper) and §10.1d (declared-boundary lstat, heartbeat fence, stale merge base) all address different defects. Line 850's "deleted, declared, re-created and deleted again" is a product-1 invariant-formulation trace with gating OFF, not this. Drill matrix §6 B1-B25: no leg exercises delete-then-recreate across a citation interval; tests.rs has `a_gated_sentinel_boundary_carries_a_delete_made_before_the_touch` (3158) but no re-create leg.

Corrections to the finding, none fatal: the bucket bytes are NOT destroyed — the GC arm heads the key, sees the etag moved off `recognized` and records `gc-skip` (gated.rs:589-618), and the version reaper fails closed because `installed.entries.get(P)` is None (gated.rs:634-641). And the sibling's local delete (sync.rs:293 `std::fs::remove_file`, on an `inst_base` path gone from the manifest and locally clean) is re-fetchable at the next citation. So the damage is: a false ok-ack, a manifest that omits a live file for one citation interval (up to `visibilityLagBoundSecs`), a fresh checkout in that window materializing a tree without it, and sibling copies deleted. If the pod is replaced in that window the file is invisible to every manifest-resolving reader — the plan's own DR statement's definition of lost — recoverable only by the manual `recover-staged` verb (gated.rs:771+).

Severity stays critical on the plan's own rubric: it violates a stated invariant of §4 product 1 at a boundary the protocol acks as ok, and the guard that would prevent it exists in the model but not in the shipped code. Opt-in-ness of gated mode is the only mitigant, and the plan ships gated as a supported mode.


## [C4] CRITICAL — §2.2's containment fix is defeated by the temp-file sibling, and the two write helpers this tranche adds have no containment at all — the arbitrary-file-write primitive SD-4 named is still reachable

**Plan section:** §2.2 (Scope validation + write containment; §9 SD-4; §7b Security)  ·  **Reviewer:** security-dos  ·  **Verifier confidence:** high


§2.2 states the rule as "`write_file_atomic` becomes containment-safe: openat-style parent walk … refusing any target that escapes the workspace root or traverses a symlink component", and §7b claims the exposure was "closed (Phase 2) rather than made agent-triggerable". The shipped code validates only the FINAL TARGET and then writes through an unvalidated temp sibling. `contained_path` refuses symlink components, but `write_file_atomic` then does `std::fs::write(&tmp)` on `<target>.flint-sync-tmp` — a predictable name in a directory the app owns — and `fs::write` follows symlinks. An unprivileged in-pod process plants `inputs/config.json.flint-sync-tmp -> /root/.aws/credentials` (invisible: the scanner skips symlinks), any consume/sync/checkout write to `inputs/config.json` truncates and rewrites that file inside the SIDECAR's mount namespace with remote-supplied bytes, and the rename then moves the symlink onto the workspace path. Worse, this tranche adds two more write helpers with NO containment whatsoever, both writing into directories the app is *required* to be able to write: `control::write_atomic` (`.flint/capabilities.json`, `.flint/remote.seq`, `.flint/*.ack`, and `pending.json` via `save_stage`) and `state::write_atomic` (`baseline.json`, `intent.json`, `incarnation.json`). `.flint/remote.seq` is rewritten on every tick, so the app needs no remote cooperation at all: plant `.flint/remote.seq.tmp` as a symlink and the sidecar's own heartbeat performs the write. `control::write_atomic` also `create_dir_all(parent)`s through a symlinked `.flint` itself. Ack/ticker JSON carries agent- and remote-controlled strings (nonce ≤128B ×32, note ≤4KiB, `sync_requested_by`), so content is substantially attacker-chosen, and the injected sidecar sets no securityContext, so its own root filesystem is writable — this is a cross-container write primitive against the credential-holding container, not a workspace-local nuisance. The plan must extend the rule from "the target" to "every path the write touches" (O_NOFOLLOW/O_TMPFILE or a fixed temp directory the app cannot reach), and state it for the control-namespace and state-dir writers, not just `write_file_atomic`.


**Evidence:** lean/sidecar/src/barrier.rs:996-1014 (`write_file_atomic`: `tmp = path.with_file_name("{name}.flint-sync-tmp")` then `std::fs::write(&tmp, bytes)`; the containment walk in `resolve_contained`, barrier.rs:939-991, never sees `tmp`); lean/sidecar/src/control.rs:121-132 (`create_dir_all(parent)` + `fs::write(tmp)` + rename, no checks); lean/sidecar/src/state.rs:131-136 (same shape, `path.with_extension("tmp")`); lean/sidecar/src/gated.rs:190-194 (`save_stage` → `control::write_atomic`); lean/sidecar/src/tests.rs:1750-1793 — `write_file_atomic_refuses_symlink_escape` asserts only on `contained_path(...)` and a symlinked *directory component*, so the temp-sibling hole is untested and the Phase-2 gate passes with it open; plan §2.2 (line 130) and §7b Security bullet claim the class is closed.


**Verifier:**

I tried to kill this on four fronts and it survived all of them.

1. The code is exactly as claimed. `barrier.rs:996-1014`: `write_file_atomic` builds `tmp = path.with_file_name("<name>.flint-sync-tmp")` and calls `std::fs::write(&tmp, bytes)` — `File::create` semantics, O_CREAT|O_TRUNC with no O_NOFOLLOW, so it follows a symlink at `tmp`. The containment walk lives entirely in `resolve_contained` (`barrier.rs:939-991`, reached via `write_file_atomic_in`, `barrier.rs:913-921`), and it only ever sees `rel`/`target` — the tmp sibling is computed *after* validation, inside `write_file_atomic`, and is never lstat'd. Checkout has the same shape (`checkout.rs:101` `contained_path` → `checkout.rs:190` `write_file_atomic`), as does sync (`sync.rs:233`) and inbox consume (`barrier.rs:110,188`). So the hazard §2.2 names verbatim — "an on-demand arbitrary-file-write primitive outside the workspace", with remote-supplied bytes — is still reachable through the very function §2.2's rule names, with the rule implemented exactly as written.

2. The two new helpers have no containment at all, confirmed: `control.rs:121-132` (`create_dir_all(parent)` → `fs::write(tmp)` → `rename`) and `state.rs:131-136` (`with_extension("tmp")`, same shape). Call sites are real and self-triggered: `sentinel.rs:501` `write_ack` → `write_atomic(ack_path)` where `ack_path` = `control_path(...)` = `<root>/.flint/<verb>.ack` (`control.rs:142`, `lib.rs:264`), and `control.rs:284` `touch_remote_seq` → `write_json` → `write_atomic`, which the doc-comment at `control.rs:257` states writes **on every call** ("`updated_unix` refreshes on every call") and is driven from the tick at `sentinel.rs:768`. Also `gated.rs:193` (`save_stage`) and `gauges.rs:163`. `.flint/` is app-writable by construction — the agent drops `.flint/publish` there — so planting `.flint/publish.ack.tmp` or `.flint/remote.seq.tmp` as a symlink and then touching `.flint/publish` (or just waiting one tick) turns the sidecar's own heartbeat into the write. No remote cooperation needed at all; this is a strictly easier trigger than the one §2.2 was written against, and it is introduced *by this tranche*.

3. Invisibility and cross-container reach hold. `scan.rs:38-44` skips `.flint`/`.flint-sync` wholesale and `scan.rs:47-49` skips symlinks, so the planted sibling is never seen. `inject.rs:84-97` mounts the same `flint-workspace` emptyDir into both the app container and the injected sidecar, and `grep -rn securityContext` over `flint-lean-chart/` and `spdk-csi-driver/src/lean_operator/` returns **nothing** — no `readOnlyRootFilesystem`, no `runAsNonRoot` — so the symlink is resolved inside the credential-holding sidecar's own writable rootfs. (One small overreach in the finding: credentials arrive via `envFrom` a Secret, `inject.rs:120-132`, so `/root/.aws/credentials` is not where they actually live here — but that filename is the plan's own §2.2 example, and it does not affect the primitive.)

4. The plan does not handle it elsewhere. I checked §3 residuals 1-12 (residual 6 covers ack *forgeability by the app*, a different direction), §9 SD-4 (line 531: "FOLDED: containment-safe writes … Phase 2 gate red-first"), and §10's table (line 575: Phase 2 "done — `contained_path`/`check_contained`") plus §10.1/10.1b's mutation ledger. Nothing anywhere mentions the temp sibling, O_NOFOLLOW, O_TMPFILE, or a non-app-reachable temp directory. And the Phase-2 gate test is genuinely blind to it: `tests.rs:1750-1793` asserts only `contained_path(...)` refusals and a symlinked *directory component* end-to-end — it never plants a `.flint-sync-tmp` sibling, so the gate goes green with the hole open.

Severity: critical. §7b's Security bullet (line 438) — part of the hard "no regression on any radar axis under defaults" constraint — asserts the class was "closed (Phase 2) rather than made agent-triggerable". Both halves are false at HEAD: it is not closed, and this tranche made it *more* agent-triggerable by adding two unvalidated writers into a directory the app must be able to write, one of which fires every tick. The plan's §2.2 rule is scoped to "the target"; it must be restated as "every path the write touches" and extended to the control-namespace and state-dir writers.


## [C5] CRITICAL — A `pinned_reads` manifest containing legacy entries silently keeps the S3-wins arm — gated checkouts adopt uncited staged bytes on exactly the rollout path

**Plan section:** §2.4.2 (D7 entry schema) vs §2.4.2 (D13 reader rule)  ·  **Reviewer:** coherence-residuals  ·  **Verifier confidence:** high


D13: "Under `pinned_reads`, flint's readers resolve exclusively by the cited `version_id` and never S3-wins-adopt the current version", and it is called load-bearing precisely because "the moment the gated lane stages a path, the cited etag stops matching current — so without it EVERY gated checkout would 412 and S3-wins-adopt uncited mid-change bytes". The entry-schema bullet three paragraphs earlier says `version_id: None` ⇒ "today's `get_whole(key, If-Match etag)` path verbatim" and that "mixed manifests are normal during rollout … both forms are permanent reader cases". The two rules collide, and the code follows the schema bullet: checkout matches on `(pinned, entry.version_id)` and the `(true, None)` case falls into the `_` arm, i.e. If-Match GET with the S3-wins 412 adoption. `citation_pass` clones the predecessor manifest and only upserts the paths it staged, then stamps `pinned_reads = true`, so every path cited by a pre-version-surface binary keeps `version_id: None` inside a pinned manifest. Enabling gated on any existing workspace is therefore a window in which the first lane staging of each legacy-cited path makes a concurrent checkout 412 and adopt the uncited, mid-logical-change version — the exact failure the mode exists to prevent, delivered through the exact arm D13 was written to close, for up to `visibilityLagBoundSecs` per path. The gate test §5 Phase 3 names for this case, `legacy_entry_without_version_id_uses_etag_path`, does not exist, and no drill leg covers a mixed manifest (B9 is a fresh workspace where every entry carries a version id).


**Evidence:** /Users/ddalton/github/flint/lean/sidecar/src/checkout.rs:145 `match (pinned, entry.version_id.as_deref())` → `(true, Some(vid))` pinned arm, `_ => get_whole(&entry.key, Some(&entry.etag))` with the S3-wins adoption at checkout.rs:168-175; /Users/ddalton/github/flint/lean/sidecar/src/gated.rs:568 `merged.pinned_reads = true` over `merged = theirs.clone()`; plan line 174 ("`None` = a legacy/unversioned entry ⇒ today's `get_whole(key, If-Match etag)` path verbatim; mixed manifests are normal during rollout") vs line 179 (D13); plan line 361 names `legacy_entry_without_version_id_uses_etag_path` — absent from src/tests.rs.


**Verifier:**

I tried to kill this three ways (does the plan handle the mixed cell elsewhere? is the cell reachable? does some code path backfill or refuse?) and it survived all three.

1. The plan genuinely collides on the (pinned_reads=true, version_id=None) cell. Plan line 174 (D7 entry schema): "`None` = a legacy/unversioned entry ⇒ today's `get_whole(key, If-Match etag)` path verbatim; mixed manifests are normal during rollout and after any bucket-versioning change, so both forms are **permanent reader cases**" — unconditional, no pinned qualifier. Plan line 179 (D13): "Under `pinned_reads`, flint's readers resolve **exclusively** by the cited `version_id` and never S3-wins-adopt the current version." Neither §3 (residuals 10 and 11 cover the conformance probe and raw-key readers only), §9 (MD-1's fold → "D13 + two reader actions"; the decision index reads "gated readers resolve cited versions exclusively; cadence/hybrid keep the shipped S3-wins arm"), nor §10 (Phase-3 row: "`pinned_reads` reader rule in checkout and sync") ever names a pinned manifest that still carries unversioned entries.

2. The cell is reachable, not theoretical. `manifest::merge` starts from `let mut merged = theirs.clone()` (manifest.rs:170) and only overwrites `mine_upserts`; `citation_pass` builds `upserts` exclusively from `stage.entries` (gated.rs:436-486) and then stamps `merged.pinned_reads = true` (gated.rs:568). `LeanEntry::version_id` is `#[serde(default, skip_serializing_if=...)]` (manifest.rs:44-46), so a manifest written by any pre-D7 binary — or during an unversioned/version-stripping period, which line 174 itself calls normal — parses with `None` and those entries survive every subsequent gated citation untouched. The upload lane stages any classified-dirty path with no version-id precondition (gated.rs:240-296), so a legacy-cited path's current version moves while its installed entry is still `None`.

3. Checkout then takes the unsafe branch. `let pinned = m.pinned_reads` is manifest-level (checkout.rs:90); the match is `(pinned, entry.version_id.as_deref())` with only `(true, Some(vid))` resolving by version — `(true, None)` falls into `_ => get_whole(&entry.key, Some(&entry.etag))` whose `PreconditionFailed` arm does `store.get_whole(&entry.key, None).await?` (checkout.rs:145-186). So a checkout landing between that path's staging and its next citation 412s and adopts the uncited mid-logical-change bytes — through exactly the arm D13 exists to close, and contradicting §2.4.2's straggler claim (line 181) that "a `pinned_reads` checkout never sees them".

4. The damage does not self-correct. The adopting checkout records `baseline.entries[path].etag = meta.etag` (the adopted current version) while `baseline.inst_base[path]` is set from the manifest's old cited etag (checkout.rs:234-236). The path therefore scans clean against the baseline (never re-staged) and reads unchanged against inst_base (never foreign), so the successor's tree holds torn bytes that its own manifest does not cite, silently and with no conflict record, until the agent happens to rewrite that path.

Two corrections to the finding that do not save it: (a) the hole is checkout-only — `sync.rs:212-228` handles the same cell by falling back to `get_whole(key, Some(etag))` and `continue`ing on `PreconditionFailed`, i.e. it skips rather than adopts, which makes checkout's `_` arm the outlier rather than a consistent design choice; (b) per legacy path the window closes at that path's first gated citation, so the exposure is per-path-once rather than permanent — but a second, broader variant the finding understates is that before the *first* gated citation the manifest is still non-pinned, so every staged path is exposed through the same arm.

The verification-gap sub-claims check out: `legacy_entry_without_version_id_uses_etag_path` (plan line 358) has zero hits anywhere in the repo outside the plan and a peer review doc; the only pinned-reader tests are `pinned_reads_never_adopts_current` (tests.rs:1985) and `raw_key_reader_sees_uncited_bytes` (tests.rs:2036), both on fresh gated workspaces where every cited entry carries a version id; B9 (§6) is the same fresh-workspace shape, B13 is about `files/.flint/` legacy paths and B14 about the mixed-fleet marker — no leg exercises a mixed manifest.

Severity: critical stands under "violates its own stated invariant" — D13's rule and §2.4.2's pinned-checkout claim are both false in this cell, the mode's central coherence guarantee is broken for a manifest-resolving reader (the class §3 residual 11 explicitly reserves the promise for), and the resulting tree/manifest divergence is silent and non-self-healing. Mitigating only in that gated is opt-in and Phase 4 (operator/CRD) is not started.


## [C6] MAJOR — In gated mode an ok-ack claims a boundary the citation deliberately dropped — D1's at-least guarantee is falsified in shipped code, and no cfg can see it

**Plan section:** §10.1c (and §2.4.2's drop rules, §2.1's ack schema)  ·  **Reviewer:** formal-coverage  ·  **Verifier confidence:** high


§10.1c's fix for the reaper defect is that 'a staged path with a live inbox entry is dropped from the boundary rather than cited over'. §2.4.2 adds the same drop for a stale base version. Both drops happen inside `citation_pass`, and in gated mode `citation_pass` is what a publish sentinel honor runs. The ack that comes back is unconditionally `status: "ok"` with `parked` taken from the LANE only and no field naming the dropped path — so an agent that declared a coherent point containing p gets 'ok, uploaded 1' while the manifest at the acked seq still cites p's PREVIOUS generation. That is exactly what §2.1's D1 corollary forbids ('a sentinel-honoring barrier may never skip a dirty file... a cooldown that excluded a file's latest bytes would falsify the ack'), and the publish-ack schema in §2.1 has no way to express the exception: `conflicts[]` is documented and implemented as sync-only. The plan never reconciles the §10.1c/§2.4.2 drop rules with the D1 corollary. The model cannot catch it either, for two compounding reasons: (a) no cfg enables `SentinelEnabled` and `GatedCitation` together, so `Inv_AckImpliesCited` is never evaluated over a citation-lane honor; and (b) `BoundaryBroken`'s conflict-record exemption is justified in the module by the claim that a conflict record 'is `report.parked` in the ack the implementation writes' — a correspondence the gated honor path does not maintain, so even a faithful gated model would exempt the lie. Fix requires either a distinct ack status/dropped-path list for gated honors, or a rule that the honor re-runs until the declared set is cited.


**Evidence:** lean/sidecar/src/gated.rs:513-527 (`citation-hitl-inflight`: `upserts.remove(path)` + `stage.entries.remove(path)`, conflict record only) and gated.rs:460-470 (`citation-stale-base`, `drop_paths.push`); lean/sidecar/src/sentinel.rs:644-680 (`honor_publish_gated`: `status: "ok"`, `parked: lane.parked.len()`, `deleted: cite.deleted.len()`, nothing from `cite.dropped_stale_base`); sentinel.rs:161-179 (`AckReport.conflicts` documented '`sync` only'); plan lines 58 (D1 corollary), 68-80 (publish-ack schema); lean/formal/LeanSubtree.tla:1385-1391 (the exemption's justification) and 1412-1418 (`BoundaryBroken`); every sentinel cfg sets `GatedCitation = FALSE` (e.g. LeanSentinelHolds.cfg, LeanSentinelDeposal.cfg) and every gated cfg sets `SentinelEnabled = FALSE`/`MaxTouches = 0` (e.g. LeanGatedHolds.cfg).


**Verifier:**

I tried to kill this on four fronts (plan §3 residuals, §9 ledger, §10.1b/c/d status, and the model's own exemption clauses) and every cited fact held.

CODE. `citation_pass` drops paths from the boundary in two places: `citation-stale-base` (gated.rs:463-470 — `drop_paths.push`, then `stage.entries.remove` at 476-478) and `citation-hitl-inflight` (gated.rs:513-527 — `upserts.remove(path)` + `stage.entries.remove(path)` + a `ConflictRecord`). Both record the path in `report.dropped_stale_base` (gated.rs:149, written at 468 and 518) — and `grep -rn dropped_stale_base src/` shows **no reader anywhere in the crate**. `honor_publish_gated` (sentinel.rs:644-674) then writes `status: "ok"` (658) with `uploaded: lane.staged.len()` (667 — which still counts the dropped path, since it was staged), `parked: lane.parked.len()` (669 — lane only), and `..Default::default()` for the sync-transport fields, so `conflicts` is empty; `AckReport.conflicts` is documented "`sync` only" (sentinel.rs:164-168). The battery's own fixture proves the manifest at that seq does not name the agent's staged version (tests.rs:2968-2977, `citation_never_reaps_a_hitl_write_that_landed_mid_stage`). Reachability inside a real honor is not theoretical: `declared_lane` consumes the inbox at its *start* (gated.rs:222) and the citation reads the inbox again at window-open (gated.rs:513-514), so any gateway write landing during the lane's upload phase — the lane deliberately opens no window (gated.rs:36-43) — drops a declared path from the acked boundary.

PLAN. §2.1's ack schema (lines 68-80) has no field that can express the exception, and §2.1's D1 corollary (line 58) plus §9 SD-2 ("excluding a dirty file's latest bytes from a sentinel barrier would falsify the ack's at-least guarantee") state the opposite standard. §2.2 line 133 states the principle explicitly — "the conflict report rides the ack in FULL … 'never a silent winner' must survive the file transport" — but scopes it to `sync.ack` only. §10.1c lines 730-733 introduces the inflight drop and §2.4.2 line 178 the stale-base drop; neither is reconciled with D1 anywhere. `grep -n "dropped\|parked"` over the whole 956-line plan returns nothing on this; §3's twelve residuals, §9's 43-finding ledger, and §10.1b/c/d contain no entry for it. §5 Phase 3 and §6 (B9/B19/B20/B21) contain no leg or test asserting ack content for a dropped path.

MODEL. Both model claims hold. (a) Across all 50 cfgs, `GatedCitation=TRUE` always pairs with `SentinelEnabled=FALSE`/`MaxTouches=0` (LeanGatedHolds.cfg et al.) and `SentinelEnabled=TRUE` always with `GatedCitation=FALSE` (LeanSentinelHolds.cfg et al.); check.sh:130-163 runs the two families disjointly — so `Inv_AckImpliesCited`/`Inv_AckBoundaryCoherent` (LeanSubtree.tla:1609,1616, evaluated only in `AckOk`, 1440-1451) are never evaluated over a citation-lane honor, even though `CiteFinish` explicitly sets `honored` under `SentinelEnabled` (1220), i.e. the module *can* express it. (b) `CiteFinish` records `dropped == Staged(s) \ Valid(s)` into `conflicts` (1208, 1214), and `BoundaryBroken` (1412-1418) exempts any path with a conflict record — justified at 1388-1391 by "which is `report.parked` in the ack the implementation writes". That correspondence is real in the fused path (barrier.rs:430-440 writes both the conflict record *and* `report.parked`) and is exactly what the gated honor does not maintain. So a combined cfg would hold vacuously green on the very case at issue.

SEVERITY. I downgrade critical→major. No data is destroyed: the staged version survives (the reaper's scope is the installed set plus keep-current), the path is conflict-recorded in `conflicts.jsonl`, the inbox entry stays queued, and the agent's bytes are cited at a later boundary; gated is opt-in and its CRD surface (Phase 4) is "not started". What is real is a false ok-ack at the acked seq with zero ack-visible signal — the plan's own "silent loser" — which misleads any agent library built on §2.1, and is worst on the preStop drain (sentinel.rs:975-990), where it is the last boundary of the workspace's life and the pending record dies with the emptyDir, leaving the uncited version to `orphans.json`/`recover-staged`. The plan should either add a dropped-path list/status to the publish ack (the §2.2 rule generalized) or state the conflict exemption in §2.1 and pin it with a cfg that sets `SentinelEnabled` and `GatedCitation` together.


---

# Tier 2 — REFUTED (do not re-open without new evidence)


## [R1] CRITICAL — §2.1's rate limiter and §2.0's operator kill switch keep their enforcement state in the app-writable emptyDir, so "enforced sidecar-side" and D3.1's no-regression bound are erasable by the actor they bound

**Plan section:** §2.1 (D3 / D3.1) and §2.0 (D0.4); §7; §7b Hot loops  ·  **Reviewer:** security-dos  ·  **Verifier confidence:** high


§2.1 says "Rate limiting is two-level, enforced sidecar-side (never on agent honor)", and §7/§7b rest the entire abuse ceiling on it: "sentinel-driven published bytes cannot exceed budget × whole_put_max = 3.84 GiB/hour/workspace … regardless of dirty-set size". The shipped enforcement point is a JSON file in the SHARED workspace mount: `.flint-sync/sentinel-budget.json`, holding both the rolling-hour charges and `last_honor_unix` (the min-interval clock). It is re-read from disk on every `sentinel_due()` and rewritten on every `charge_budget()`. §1.2's own premise — "there is no path the sidecar can write that the app cannot" — makes this writable by the storming agent: `echo '{"charges":[],"last_honor_unix":0}' > .flint-sync/sentinel-budget.json` before each touch reduces both bounds to nothing, and `load_budget` uses `unwrap_or_default()` so a garbage file silently resets the meter rather than failing loud. The same defect voids two other stated guarantees: (a) D0.4's `sentinels: off` — the operator-of-record's switch for the case where consumption EATS APP DATA — is enforced by re-reading `.flint-sync/sentinels.json` on every tick, so the app re-arms it with one file write; (b) gated mode's "unbounded staleness is impossible by construction" is enforced from `last_citation_unix`/`stable_since_unix` in `.flint-sync/pending.json`, and `load_stage` also `unwrap_or_default()`s, so a corrupt or planted stage file both parks the manifest and silently forgets every staged-uncited version. §1.2 correctly reasons that no MAC can be anchored in-pod — but it then draws the mitigation only for acks ("agent libraries MUST NOT treat an ack as proof"), and never notices that the same premise disqualifies the shared mount as the home for the sidecar's own enforcement and safety state. The plan needs either process-memory-resident limiter state (rebuilt-not-trusted on restart) or an explicit statement that the budget, the posture and the gated clocks are advisory against a hostile agent — which would in turn retract §7's ceiling and §7b's hot-loops no-regression claim.


**Evidence:** lean/sidecar/src/sentinel.rs:62 (`const BUDGET: &str = "sentinel-budget.json"`), :258-259 (`budget_path()` = `state_dir().join(BUDGET)`), :296-310 (`load_budget`/`save_budget`, `unwrap_or_default()`), :317-348 (`charge_budget`/`sentinel_due` read that file for both bounds); lean/sidecar/src/lib.rs:262-265 (`state_dir()` = `root/.flint-sync`, inside the workspace root); spdk-csi-driver/src/lean_operator/inject.rs (one emptyDir `flint-workspace` mounted at `mount_path` into every app container and the sidecar); lean/sidecar/src/control.rs:149-158, sentinel.rs:788-791 (posture re-read from `.flint-sync/sentinels.json` per tick); lean/sidecar/src/gated.rs:177-188, :386-401 (stage clocks from `.flint-sync/pending.json`, `unwrap_or_default()`); plan line 21 (§1.2 premise), line 97 ("enforced sidecar-side"), line 419 (the 3.84 GiB/hour bound).


**Verifier:**

The finding rests on three limbs; the load-bearing one is factually wrong in code, and the severity claim is contradicted by the plan's own classification.

LIMB (a) — "D0.4's `sentinels: off` is re-read every tick, so the app re-arms it with one file write" — REFUTED by the code. The run loop reads the posture ONCE at startup into a local (`lean/sidecar/src/bin/flint_sync.rs:247` `let posture = sc.sentinel_preflight()?;`) and gates the poll arm on that in-memory value (`flint_sync.rs:332` `if !posture.enabled { continue; }`) — it never re-reads the file. The per-tick `load_posture()` in `sentinel.rs:791-793` is unreachable as an *enable* path because the loop short-circuits first; it can only add a disable. And `sentinel_preflight` handles `SentinelMode::Off` from env *before* consulting the file and overwrites `.flint-sync/sentinels.json` on every startup (`control.rs:173-178`), so a planted `{"enabled":true}` does not survive a restart either. `sentinels` reaches the sidecar as CRD→env (`lib.rs:196`, `SentinelMode::parse` at `lib.rs:152-157`), not from the mount. The kill switch is not app-erasable.

LIMB (b) — gated clocks — mechanically true (`gated.rs:181-188` `unwrap_or_default()`; `gated.rs:373-377` `citation_due` early-returns `None` on an empty stage, so the lag cap is never evaluated) but overstated. It is (i) opt-in mode only (`boundaryMode` default `hybrid`, §2.6 table), (ii) self-harm on the app's own workspace, (iii) explicitly recoverable by the plan's own stated path — D9's `recover-staged` re-cites uncited versions from bucket truth with a `ListObjectVersions` fallback when orphans.json is missing or stale (§2.4.3 D9, plan lines 211-212; §10 status marks it done, line 578), and (iv) HITL is unaffected: `citation_due`'s own comment and the §2.4.2 HITL exemption keep repair passes on the ordinary barrier path.

LIMB (c) — the budget file — the mechanism is real (`sentinel.rs:296-310` `load_budget` with `unwrap_or_default()`, `:317-348` `charge_budget`/`sentinel_due` reading `.flint-sync/sentinel-budget.json` from `state_dir()` = `root/.flint-sync`, `lib.rs:70,260-262`, inside the single emptyDir mounted at `mount_path` into every app container and the sidecar, `lean_operator/inject.rs:84-97`). But the finding's severity and framing do not survive the plan text:
- The plan explicitly places this mechanism OUTSIDE its safety argument: §4 product 5, line 317 — "**Liveness / rate limiting stays OUT of the safety gate** — fairness, per the house rule"; abstraction ledger line 309 — "budget/min-interval → out of the safety gate"; restated at line 914. So defeating the budget cannot "violate its own stated invariant," which is the critical bar.
- The no-regression constraint is scoped "under defaults" (line 6) and §7b's hot-loops argument (line 437) is about workload shape; §7 (line 419) says the bound holds "regardless of dirty-set size" — a workload-shape qualifier, not a tamper-resistance claim. §2.1's "enforced sidecar-side (never on agent honor)" states where the check lives (the sidecar code path, not voluntary agent compliance), which is literally true.
- The plan already states the universal premise the finding says it "never notices": §1.2 line 21 — "there is no path the sidecar can write that the app cannot" — and §3 residual 12 already concedes "Sentinel request amplification is bounded, not eliminated."
- The damage a tampering (i.e. deliberately malicious, not buggy) app unlocks is dominated by what the plan already declares untouched and out of scope: §7b line 437 states the cadence-driven hot->64 MiB amplification (the parent plan's 2.9 TiB/day pathology) is untouched here, so a malicious app already saturates the uploader on the cadence path with no sentinel and no tampering. The tamper delta is a request-rate term (poll-cadence honors instead of 60/hour), not a new byte ceiling.

Residual worth noting but not the finding as written: `load_stage`'s `unwrap_or_default()` (`gated.rs:186`) is a silent drop of the sidecar's own record, stylistically inconsistent with D2's explicit "never a silent drop" torn-body rule for `load_pending` (`sentinel.rs:270-280`), and the plan could usefully add one sentence saying the budget/min-interval counters are advisory against a tampering in-pod process. That is a minor documentation gap, not a critical defect.


## [R2] CRITICAL — D0.4's pre-existing-`.flint/` pre-flight cannot fire on the routine pure-spot path: the manifest arm was never built and both shipped arms are empty by construction on a replacement pod

**Plan section:** §2.0 D0.4  ·  **Reviewer:** coherence-residuals  ·  **Verifier confidence:** high


D0.4 disables sentinel consumption "if the baseline **or manifest** cites any `files/.flint/**` path, **or** `.flint/publish`/`.flint/sync` exist before `capabilities.json` has ever been written for this tree", because otherwise a file literally named `.flint/publish` is "*consumed* (renamed away) by the sentinel poll: a data grab from a non-participating workspace". The shipped pre-flight reads only the local baseline (`entries` + `inst_base`); the manifest arm does not exist. On a pod replacement — which §3 residual 3 calls the routine path on this pure-spot fleet, and which is how every sidecar upgrade lands — `sentinel_preflight` runs before `checkout`, so the state dir is fresh (baseline empty ⇒ arm (a) dead) and the workspace tree is empty (no sentinel-named file ⇒ arm (b) dead). The verdict written is `enabled`, and it is sticky. checkout then refuses to materialize the workspace's own `files/.flint/**` data (D0.3), the app recreates `.flint/publish` as data, and the poll consumes and retires it — destroying an app-owned file with no condition, no conflict record, and no way for the operator to see it. B22 only exercises the container-restart shape ("workspace with app-owned `.flint/publish` file, new sidecar"), so the drill matrix cannot catch it either. Note the plan's own arm (b) is also unsound as written for this path: `capabilities.json` is written before checkout, so "before the marker has ever been written" is a window in which the tree does not yet exist.


**Evidence:** /Users/ddalton/github/flint/lean/sidecar/src/control.rs:188-196 (`legacy_cited` from `baseline.entries`/`baseline.inst_base` only; `never_marked` = marker absent; no manifest read); /Users/ddalton/github/flint/lean/sidecar/src/bin/flint_sync.rs:246-252 (`gated_startup_check` → `sentinel_preflight` → `write_capabilities` → `checkout`); consumption/retirement path sentinel.rs:88-92 (`publish.pending.json`) and D2.4 retire; plan line 45 (D0.4) and line 402 (B22).


**Verifier:**

The finding's core mechanism does not survive the code. It cites control.rs:188-196 and flint_sync.rs:246-256 but stops at `sc.checkout()`; the manifest arm of D0.4 is in fact realized inside checkout, and it fires before any poll tick can consume anything.

1. `checkout` builds the merge base from the WHOLE manifest entry map, control citations included: /Users/ddalton/github/flint/lean/sidecar/src/checkout.rs:238 `baseline.inst_base = m.entries.iter().map(|(p, e)| (p.clone(), e.etag.clone())).collect();`, saved at :240. A `files/.flint/publish` citation is refused for materialization (checkout.rs:101-110, 213-222 — conflict record, `continue` before `baseline.entries.insert`) but it stays in `m.entries`, so it lands in `inst_base` regardless.
2. Immediately after that save, checkout re-runs the pre-flight: checkout.rs:247 `let posture = self.sentinel_preflight()?;` (then :248 `write_capabilities`). Arm (a) at control.rs:188-189 tests `baseline.inst_base.keys().any(is_control_path)` — with `CONTROL_DIR = ".flint"` (lib.rs:77) and scan.rs:96-98, `.flint/publish` matches — so on a replacement pod whose manifest cites the path, `legacy_cited` is TRUE and the posture is written `{enabled:false, reason:"preexisting-flint-paths"}`.
3. The earlier `enabled` verdict from flint_sync.rs:247 does not block that recomputation: the stickiness short-circuit at control.rs:182-186 returns early only when `!prior.enabled`. An enabled prior is always re-evaluated — exactly the case the finding relies on.
4. Ordering makes the disable effective before consumption is possible: `sc.checkout().await?` is at flint_sync.rs:255, before the `select!` loop is entered, and `sentinel_tick` re-reads the posture from disk on every tick (sentinel.rs:790-792), not from the stale in-memory value. So the poll arm never arms, the app-owned `.flint/publish` is never renamed into `publish.pending.json`, and there is no data grab — which is what plan leg B22 (line 402) asserts and tests.rs:1089-1115 exercises.

So "both shipped arms are empty by construction on a replacement pod" is false for the very scenario the finding needs (manifest cites `files/.flint/**`): arm (a) is evaluated a second time, against the manifest, at the first moment the manifest is known. The claimed critical outcome — consumption + retirement of an app-owned file — cannot occur on that path.

What is genuinely there, and is a smaller and different defect than the one claimed: run_loop reuses the STALE in-memory `posture` after checkout — flint_sync.rs:256 `sc.write_capabilities(&posture, false)?` overwrites the disabled marker checkout just wrote, and :262 gates `settle_pending_at_startup` on it. On this path `capabilities.json` advertises `verbs: [publish, sync, remote-seq]` with no reason while `.flint-sync/sentinels.json` says disabled, contradicting D0.4's "`capabilities.json` reports `verbs: []` with reason `preexisting-flint-paths`". The consequence is a mis-advertised verb (touches never consumed, never acked), not destruction of data — the file is left byte-identical because the poll consults the disk posture. That is an advertising-drift finding, not the critical data grab claimed here.

The secondary note (arm (b) can never fire on a fresh pod because `capabilities.json` is written before checkout) is technically true but inert: on that same path arm (a) covers the cited case via `inst_base`, so it changes no outcome.


---

# Tier 3 — UNVERIFIED (never routed to a verifier)


## [U1] MAJOR — D9's durable `orphans.json` is not implemented, and §10 reports Phase 3 done with only two named remainders

**Plan section:** §5 Phase 3 scope ("takeover surfacing + durable orphans.json"), §2.4.3 D9, §10 table row "3 — remainder", §3 residual 3  ·  **Reviewer:** ops-fleet  ·  **Independently reported by 2 reviewers**


D9 makes `<prefix>/.flint/lean/orphans.json` the durable, cluster-loss-surviving evidence of uncited work, the input to the operator's "candidates with no live lease" event/condition, and the reason the expensive `ListObjectVersions` path is the fallback rather than the mechanism. It is written nowhere in the sidecar or the operator. §5 lists it inside Phase 3's scope; §10's table marks Phase 3 done and its "remainder | open" row names only the heartbeat echo and drain sizing, so this deliverable falls out of the completion accounting entirely. Consequences: gated's DR posture has no evidence surface in exactly the window where conflicts.jsonl and the CR no longer exist; §3 residual 3's claim that uncited bytes are "*named durably* in `orphans.json`" is false; `recover-staged` always takes the expensive full-prefix version LIST; and §7's "no bucket-wide LISTs (staging list is prefix-scoped, claim-time and operator-cadence only)" leans on a summary that is never produced.


**Evidence:** Repo-wide grep for "orphans" over lean/sidecar/src and spdk-csi-driver/src/lean_operator returns no hits. flint-store's own trait doc already names the missing file: crates/flint-store/src/lib.rs:494-497 — "The claim-time/DR fallback when `orphans.json` is missing or stale — the expensive path, which is why the durable summary is written eagerly". `recover_staged` unconditionally calls `self.store.list_versions(&files_prefix)` (gated.rs:778-780). Plan lines 211-212, and §10's Phase-3 rows.


## [U2] MAJOR — D12's "structurally immune to arm starvation" does not hold: the shipped select loop serializes arms, so any long-running arm still starves lease renewal past the takeover threshold

**Plan section:** §2.1a (D12 rule 3); §2.1 (D3 loop structure); §6 leg B17  ·  **Reviewer:** security-dos  ·  **Independently reported by 2 reviewers**


§2.1a rule 3 says "Renewal runs on its own non-resettable interval (D3) — structurally immune to arm starvation", and D3's rewrite is justified against the shipped resettable-sleep select. Independent `tokio::time::interval`s do fix the sleep-reset defect, but they do not make the arms concurrent: all three timers plus SIGTERM are branches of ONE `tokio::select!` in ONE task, and the chosen branch's body is awaited inline, so nothing else is polled until it returns. Any arm that runs longer than the ~60 s quiet threshold (QUIET_POLLS=6 × 10 s) therefore still blocks renewal and invites deposal of the live writer — the exact failure D12 was added to prevent — and `MissedTickBehavior::Delay` means the skipped renewals never catch up. The blocking durations are agent-controllable and are precisely the ones this tranche introduces: a sentinel-honored barrier over a large dirty set, a scoped-sync honor whose manifest GET the plan itself measures at 27 s / 1.3 GiB, and a gated citation's serialized per-path ListObjectVersions round-trips. B17 ("continuous sentinel storm ⇒ NO takeover") will pass on a small-file storm and prove nothing about the shape that actually starves the arm. The plan should either state the renewal must run in its own task (spawned, not a select branch) or retract the structural-immunity claim and bound every arm's runtime against the quiet threshold.


**Evidence:** lean/sidecar/src/bin/flint_sync.rs:290-296 (three intervals, `MissedTickBehavior::Delay`), :298-343 (one `select!`, each arm `.await`ed in the loop body — `floor_tick`, `sentinel_tick` and `heartbeat_tick` cannot overlap); lean/sidecar/src/lease.rs:24 (`QUIET_POLLS = 6`) with the 10 s standby cadence at flint_sync.rs:168; lean/sidecar/src/barrier.rs:331-336 (the 27 s / 1.3 GiB manifest-fetch lesson quoted in-code); lean/sidecar/src/gated.rs:637-669 (serialized per-path LISTs); plan §2.1a rule 3, §6 leg B17.


## [U3] MAJOR — §2.1 contradicts itself on the skip-on-no-diff fast path: the correction was appended, the prescription left standing

**Plan section:** §2.1 (2026-08-25 correction) vs §2.1 D2 rule 2  ·  **Reviewer:** coherence-residuals  ·  **Independently reported by 2 reviewers**


The correction paragraph in §2.1 says the rule "a pending sentinel defeats the skip-on-no-diff fast path" is "**not** implemented and should not be — it contradicts §7 and D3.1". Two paragraphs later, D2 rule 2 still prescribes it verbatim: "A pending file, parsable or not, is 'work': it defeats the skip-on-no-diff fast path exactly as `repairs_pending` does". §10.1 and §10.1d both close with "§2.1 should be corrected to match §7" — the edit is still owed, and the mechanism text an implementer reads (D2's numbered consume/honor/ack/retire rules) is the half that is wrong. Anyone implementing D2 rule 2 as written would re-introduce a manifest CAS per no-diff honor at up to 720/hour/workspace, which §7 prices and D3.1 forbids, and would falsify §7's "the one term the meter deliberately leaves free is the *no-diff* honor (one HEAD, no data)".


**Evidence:** plan line 60 ("Also corrected here: §2.1's 'a pending sentinel defeats the skip-on-no-diff fast path' is **not** implemented and should not be") vs plan line 65 (D2 rule 2, "it defeats the skip-on-no-diff fast path exactly as `repairs_pending` does (`barrier.rs:203-206`)"); plan lines 620-634 (§10.1 deviation) and lines 869-882 (§10.1d, `LeanSentinelFastPathUnguarded`).


## [U4] MAJOR — §7's "no-diff honor = one HEAD" is wrong: the shipped honor costs 4 bucket requests including a PUT-priced lease renewal — the one deliberately unmetered term is understated ~15×

**Plan section:** §7 (hybrid paragraph, "the one term the meter deliberately leaves free"), §2.1 D3.1  ·  **Reviewer:** ops-fleet  ·  **Independently reported by 2 reviewers**


D3.1 and §7 both price a no-diff sentinel honor as "one HEAD + an ack rename" / "HEAD-priced (~$0.0003/hour)" at the 720/hour/workspace min-interval ceiling, and §10.1 re-uses that number to justify letting the fast path through. The shipped honor path renews the lease (a PUT) before every honor, then runs a full `declared_barrier` whose fast path still costs an epoch read, an inbox GET and the manifest HEAD. So a no-diff honor is 1 PUT + 2 GET + 1 HEAD. At 720/hour that is 720 PUT + 2,160 GET-priced per hour per workspace ≈ $0.0045/hour (≈15× the stated figure), and the fleet-correlated proxy term is 600 PUT/s + 1,800 GET/s at 3,000 storming workspaces, not a HEAD-only term. This is the same PUT-priced-renewal error OF-7 caught at the idle tick, surviving in the exact term the work meter is designed not to bound (units = 0 for a no-diff honor), i.e. the unbounded term is the mispriced one.


**Evidence:** sentinel.rs:565-573 `honor_pending` → `super::lease::renew(self).await` before the honor; :611 `let report = self.declared_barrier().await?` → barrier.rs:303 `self.verify_not_deposed().await?` (epoch read, barrier.rs:76-88), :306 `consume_inbox` → `inbox::load` = `store.get_whole(inbox_key)` (inbox.rs:52-60), :335 `self.store.head(&self.cfg.manifest_key())` fast path. `charge_budget` charges 0 units for `published_bytes == 0` (sentinel.rs:308-322), so only `sentinel_min_interval_secs` bounds the rate (sentinel.rs:334-346). Plan lines 105, 419, 626.


## [U5] MAJOR — §7 prices the D12 default-posture increase at half its real value: renewals now fire from two independent arms

**Plan section:** §7 ("This plan's default-posture delta") / §2.1a D12  ·  **Reviewer:** coherence-residuals  ·  **Independently reported by 2 reviewers**


§7 states the one deliberate default increase as "+1 PUT/min/workspace when floor > 30 s ⇒ +50 PUT/s fleet-wide ≈ +$21.6/day at 3,000 workspaces". D12 rule 1 keeps `lease::renew` at the head of every barrier-triggering arm AND rule 2 adds a heartbeat interval at `min(floor,30)`, and both are implemented as independent non-resettable timers with no debounce inside `renew`. At the default floor=60 that is 2 heartbeat renews + 1 floor renew = 3 renew CASes/minute against the shipped 1/minute — a delta of **+2 PUT/min**, ≈ +$43/day at the plan's own 3,000-workspace example, i.e. the plan's single stated default-posture regression is understated by 2×. The knock-on is that the "recorded control shape" B8 and the Phase-0/Phase-1 acceptance criteria are written against is stale in the same way §7 says the draft's was: §5 still describes "the real idle tick" as 4 requests (renew CAS + epoch read + inbox GET + manifest HEAD), which is only true if the heartbeat replaced the floor arm's renew rather than supplementing it. The correct idle shape at floor=60 is 6 requests/minute, 3 of them PUT-priced. An oracle built from the plan's number fails against correct code — the exact OF-7 failure the plan folded.


**Evidence:** /Users/ddalton/github/flint/lean/sidecar/src/bin/flint_sync.rs:287-296 (independent `floor_iv`, `renew_iv` at `floor.min(30)`, `poll_iv`); sentinel.rs:849 (`floor_tick` renews first); sentinel.rs:830-841 (`heartbeat_tick` renews); /Users/ddalton/github/flint/lean/sidecar/src/lease.rs:116-133 (`renew` has no debounce — every call is an `epoch_renew` write); plan line 414 (§7 delta) and line 345 (Phase 0 acceptance, "the real idle tick is 4 requests").


## [U6] MAJOR — B3's oracle and the Phase-1 acceptance criterion cannot pass against correct code — the ack's covered-nonce set is capped at 32, oldest dropped

**Plan section:** §6 B3 / §5 Phase 1 acceptance vs §2.1 D2 rule 3  ·  **Reviewer:** coherence-residuals  ·  **Independently reported by 2 reviewers**


§2.1's ack schema bounds `nonces` at 32 with the oldest dropped. §5's Phase-1 acceptance and §6's B3 both demand that a storm of 100 touches in 10 s end with "the final ack's `nonces[]` covers EVERY touched nonce", and B3's anti-vacuity guard additionally demands that "a mid-storm nonce (not the last) appears in the covered set". With min-interval 5 s the 100 touches coalesce into 2-3 pending records of ~35-50 nonces each, each truncated to its newest 32 — so "covers every touch" is unsatisfiable by construction and the mid-storm assertion is a coin flip on where the cut lands. This is the same class as the OF-7 finding the plan folded ("B8's old oracle would have failed against a correct sidecar on day one"), and it also undercuts SD-2's fold: the stated purpose of the covered-nonce set is that an agent whose nonce rode behind a later touch does not "re-touch in a loop, feeding the storm the rate limit exists to prevent" — with a 32-cap, storming agents beyond the cap do exactly that. §3 residual 6 states the nonce mechanism without mentioning the bound at all.


**Evidence:** plan line 71 ("nonces … EVERY coalesced nonce covered (bounded 32, oldest dropped)"); plan line 352 (Phase 1 acceptance) and line 381 (B3 row); /Users/ddalton/github/flint/lean/sidecar/src/sentinel.rs:56 `const MAX_NONCES: usize = 32;` and sentinel.rs:464-467 `let excess = pending.nonces.len().saturating_sub(MAX_NONCES); … drain(..excess)`; plan line 289 (§3 residual 6).


## [U7] MAJOR — §10 declares Phases 0-3 done without stating that no drill leg exists, and its "deviations named rather than buried" list omits the product-3 gate it did not meet

**Plan section:** §10 / §10.1 / §10.3 vs §5 phase gates  ·  **Reviewer:** coherence-residuals  ·  **Independently reported by 2 reviewers**


Every phase gate in §5 lists drill legs as part of the gate, and §6 is the acceptance instrument for the mechanism claims the battery cannot reach (straggler containment, spot-reclaim drains, mixed-fleet, request-count oracles). Not one of B1-B25 exists — `lean/e2e/` still contains only the 12-leg chaos rig, whose "sentinel" references are the container-restart file sentinel, not the boundary verbs. §10's status table and its verification-posture section discuss only the Rust battery and the TLC gate and never mention the drill matrix, so "done" is being read against roughly half of each phase's stated gate. §10.1's "Two deviations from the plan's stated discipline, named rather than buried" also omits a third: §5's Phase-3 gate requires "tranche-3 products 2 + 3 green BEFORE merge", and §10.3 defers product 3 while Phase 3 is marked done. That deferral's justification — "drill leg B12 covers the shape better than a model would" — leans on a leg that has not been written, so the straggler product is currently covered by neither the model nor the drill.


**Evidence:** plan lines 341-372 (§5 gates naming legs) vs plan lines 563-584 (§10 table) and 585-634 (§10.1) — no drill mention; plan line 394 of §10.3 ("drill leg B12 covers the shape better than a model would"); `ls /Users/ddalton/github/flint/lean/e2e/` → run-chaos.sh, chaos.yaml, run-chart.sh, run.sh only; `grep -n 'sentinel' lean/e2e/chaos.yaml` → container-restart file sentinel comments only.


## [U8] MAJOR — §2.1's refused-ack rule for the restarted claimant spinning in Waiting is not implemented: a crash-then-takeover restart strands the agent forever with a live-looking capabilities marker

**Plan section:** §2.1 (D2, refused acks, rule 2)  ·  **Reviewer:** crash-takeover


Plan line 88 (a 'Rules:' item under the refused-acks decision): 'The restarted process, upon claim_step observing a foreign live holder with a surviving pending file, writes/refreshes the refused ack (it can never honor).' §10's Phase 1 row claims done including 'refused-fenced acks'. In code, `claim()` loops on `ClaimOutcome::Waiting` with only an eprintln and a 10 s sleep — no `refuse_pending`, no `mark_fenced` — and `claim_step` itself touches neither; every `refuse_pending`/`mark_fenced`/`settle_fence` call site in the crate is inside sentinel.rs's honor/tick paths, all of which run only AFTER a successful claim. The stranding scenario the rule exists for: sidecar is SIGKILLed/OOM-killed mid-honor (so no settle_fence ran — the cooperative fence paths that DO settle never executed), a successor takes over, kubelet restarts the container over the surviving emptyDir; the restarted process spins in Waiting forever against the live successor (its token advances, quiet_polls never accumulate), `settle_pending_at_startup` is unreachable (it runs only after claim succeeds), the pending sentinel is never answered, and `capabilities.json` still says state:'live' with live verbs — the agent polls an ack that will never come on a marker that says it will. This is precisely the 'largest protocol hole' state §2.1 describes, closed on the cooperative-fence paths (heartbeat/floor/poll/drain all settle, verified) but open on the crash-restart-Waiting path the rule specifically names.


**Evidence:** bin/flint_sync.rs:159-172 (Waiting arm: log + sleep only), 233-269 (settle_pending_at_startup only after claim() returns); lease.rs:37-111 (claim_step has no ack/marker writes); grep for refuse_pending/mark_fenced/settle_fence: call sites only in sentinel.rs (honor_pending:568-594, settle_fence:774-782, tick arms) plus one test; plan lines 85-91 (the rule), 573 (§10 Phase 1 'done ... refused-fenced acks').


## [U9] MAJOR — The design sections' file:line citations are systematically stale at HEAD — the tranche rewrote the very files they cite, and only §10 was updated

**Plan section:** §1.1–§2.4 (throughout the design half)  ·  **Reviewer:** drift


The plan is the plan of record for a tranche that is now implemented, and its §1–§2 citations still point at pre-implementation line numbers of files the implementation itself rewrote. Confirmed drifted at HEAD: lib.rs:113-125 cited for the floor default (now the BoundaryMode enum; floor_secs is at lib.rs:177/226); bin/flint_sync.rs:143-174 cited as the run loop (run_loop is now at 233+, and the §2.1/D3 claim 'the shipped run loop recreates tokio::time::sleep(floor) inside select! on every iteration (bin/flint_sync.rs:151-155)' describes code that no longer exists — the loop was rewritten per D3, and the comment at flint_sync.rs:276 says so); state.rs:87-137 cited for the flock (now installed_etag/ConflictRecord; flock at ~112-160); barrier.rs:190/180-469 for verify_not_deposed/run_barrier (now 68-75/281+); barrier.rs:443-456 for step 7 (now ~603); barrier.rs:702-724 for write_file_atomic (now 913/996); barrier.rs:203-237 and 203-206 for the HEAD tick / repairs_pending fast path (now ~320-328); scan.rs:83-108 for the two-scan rule (classify now at 100+); manifest.rs:118-124 for the already-integrated merge check (now cas_write_stamped's doc); state.rs:180-212 for clear_intent_keys (now 221); checkout.rs:102-121 for the 412/S3-wins arm (shifted); lib.rs:69 for whole_put_max (WHOLE_PUT_MAX now at lib.rs:84); tests.rs:556 (test at 561); s3.rs:178-187 'put_whole discards it today' (the code now keeps version_id — the comment says 'used to be discarded'); s3.rs:418-433 for the A9 versioning recommendation (now the list_versions impl); s3.rs:763-840 for bootstrap_lifecycle (now 884); memory.rs:236 for impl ObjectStore (now 382); gen-cfgs.sh:42 for the 'blows past an hour' wall note (now line 62). The danger is amplified because a subset of citations is still exact (gateway.rs:110-118, lease.rs:24, bin/flint_lean_gateway.rs:96, all operator cites, templates/gateway.yaml:1-8, README.md:47-53/81-85) — a reader who spot-checks one of those will trust the rest. Several present-tense hazard claims are also now false at HEAD (§2.0 'the scanner skips only the root-level .flint-sync' — scan.rs:43 now skips CONTROL_DIR too), which is survivable only because the header points at §10; the line numbers carry no such disclaimer.


**Evidence:** Plan lines 14-15, 38, 58-95, 127-141, 170-175, 199, 226, 320-330 vs lean/sidecar/src/lib.rs:84,177,226; bin/flint_sync.rs:146-148,233,276-299; state.rs:93,112-160,221; barrier.rs:68-75,281,320-328,603,913,996; scan.rs:43-46,100; manifest.rs:107-147; checkout.rs:101-124; tests.rs:561; crates/flint-store/src/s3.rs:154,176-190,419,884; memory.rs:382; lean/formal/gen-cfgs.sh:62


## [U10] MAJOR — The tranche's verification evidence is split across the commit boundary: HEAD's committed plan mis-describes HEAD's code, and the 49-run formal gate is reproducible from no commit

**Plan section:** §10 headline / §10.1d  ·  **Reviewer:** drift


The working-tree plan (947 lines, uncommitted 'M') says battery 75/75 and formal 49/49, and both match the working tree (75 tests pass; 50 cfgs = 49 gate runs + LeanSubtreeDeep; check.sh prints PASS/49). But the committed plan at HEAD (812 lines, commit 69613005) says 'battery 67/67 ... formal 36/36' while HEAD's sidecar battery is already 75 — the three §10.1d defect-fix commits (8836f1cc 'sentinel ack claimed a boundary that withheld the agent's delete', 77333f5e 'heartbeat renewal arm exited on a fence', 7cb4e1bc 'restart between the manifest CAS and step 7 ate the agent's delete') landed with their tests but without the plan update. Worse for posture: the 13 product-1 formal runs (LeanSentinel*.cfg, LeanProbeAckAfterCrash/CoalescedAck/FastPathHonor/RefusedAck/SentinelHonored.cfg) are UNTRACKED, and LeanSubtree.tla/check.sh/README.md are modified-uncommitted — so the machine-checked justification for three committed fixes (including 'THE MODEL'S' finding, §10.1d defect 3) exists in no commit and dies to a git clean. Additionally §10.1's '26 mutations were applied ... The full list lives in the session record' and §10.1b's 'sixteen more mutations' place the mutation-matrix evidence outside the repo entirely; the plan is honest that it does so, but a plan of record whose central red-proof ledger is session-ephemeral cannot be re-audited. The fix is mechanical (commit the plan + formal artifacts alongside the fixes they justify), but until then the plan-of-record state at HEAD is internally wrong and the 49/49 claim describes only an uncommitted snapshot.


**Evidence:** git status: 'M docs/plans/flint-lean-boundary-verbs-plan.md', 13 '??' cfgs, 'M' on LeanSubtree.tla/check.sh/README.md/gen-cfgs.sh; git show HEAD:docs/plans/...:563 'Lean battery 67/67 ... formal gate 36/36'; git ls-tree HEAD lean/formal → 37 cfgs (=36 runs) vs worktree 50 (=49 runs); cargo test at HEAD-clean sidecar → '75 passed'; commits 8836f1cc/77333f5e/7cb4e1bc vs untracked LeanSentinelStaleMergeBase.cfg etc.; plan lines 587-590 ('the full list lives in the session record')


## [U11] MAJOR — No run pairs the sentinel with the gated lane, and the omission is in neither §10's nor the README's 'not modelled' list

**Plan section:** §4 (products 1 and 2), §10.1c, §10.1d  ·  **Reviewer:** formal-coverage


The shipped wiring makes a publish sentinel a citation source (§2.4.1(i), §10 status table 'a publish sentinel is a citation source, not a fused barrier'), and the preStop drain is a gated citation too. Yet the 22 tranche-3 runs partition cleanly: 13 sentinel cfgs with `GatedCitation = FALSE`, 9 gated cfgs with `SentinelEnabled = FALSE`. The module even hard-excludes the combination in one arm (`FastPath(s) == ... SentinelEnabled /\ ~GatedCitation`). So none of product 1's four invariants is ever evaluated over the gated honor path, where the boundary semantics differ materially: deletes are withheld until the citation, `Valid(s)` can shrink the boundary, and the ack's seq comes from a CAS that installed a pending set rather than from a fused barrier. §4's budget table discloses 'Gated off' for the P1 *breadth* cfg only — it names no product for sentinel×gated — and both §10.1d's and README:334-339's 'not modelled, named rather than omitted' lists omit it entirely, while §10.3 declares products 1, 2 and 4 done. Reviewers reading §10 will conclude the boundary verb is machine-checked in the mode it was designed for; it is checked only in the mode it is inert in.


**Evidence:** lean/formal/LeanSubtree.tla:1347-1348 (`FastPath` requires `~GatedCitation`); cfg matrix — LeanSentinel*.cfg and LeanProbe{SentinelHonored,RefusedAck,AckAfterCrash,CoalescedAck,FastPathHonor}.cfg all `GatedCitation = FALSE`; LeanGated*.cfg and LeanProbe{CitationInstalled,WithheldDelete,ForcedCite,RawUncited}.cfg all `SentinelEnabled = FALSE`, `MaxTouches = 0`; lean/sidecar/src/sentinel.rs:604-605, 644-650 (gated honor = `declared_lane` + `citation_pass`), sentinel.rs:982-984 (drain, same shape); plan lines 785-800 and lean/formal/README.md:334-339.


## [U12] MAJOR — The gated product runs with zero crashes and zero restarts, and §10.1c defers the citation's crash matrix to a product that disables gated

**Plan section:** §10.1c (closing paragraph), §4 budget table rows P2  ·  **Reviewer:** formal-coverage


All nine gated cfgs set `MaxCrashes = 0` and `MaxRestarts = 0`, so `CrashPod` and `Restart` are disabled in every run where the staging lane exists. §10.1c explains this away: 'the citation's own crash matrix ... is product 1's territory'. But every product-1 cfg sets `GatedCitation = FALSE`, so the citation's crash matrix belongs to no product at all. Three concrete windows are therefore unsearched: (i) the restart between the citation CAS and the baseline/inst_base rewrite — structurally identical to §10.1d defect 3, and the code applies the same `installed_etag` own-base fix in the citation lane, so the 'both lanes' claim of that fix has model evidence for one lane only; (ii) the crash between the staging PUT and the pending-record write (§2.4.2's PUT-first ordering); (iii) pod replacement losing the stage, which is the premise of D9/orphans.json, §3 residual 3 and legs B11(b)/B15. §4's own budget table asked for `MaxCrashes=1` on the P2 gated×GC row and `MaxCrashes=1(+replace)` on the backstop/endgame row; on disk both are 0, and neither §10.1c nor the README records the reduction.


**Evidence:** LeanGatedHolds.cfg, LeanGatedReapsCurrent.cfg, LeanGatedBackstop.cfg, LeanGatedSplitCitation.cfg, LeanGatedInflightHitl.cfg, LeanProbeCitationInstalled.cfg, LeanProbeWithheldDelete.cfg, LeanProbeForcedCite.cfg, LeanProbeRawUncited.cfg — all `MaxCrashes = 0`, `MaxRestarts = 0`; plan lines 325-326 (budget table) vs those files; lean/formal/README.md:211-212 ('no crashes or restarts'); lean/sidecar/src/gated.rs:551-560 (the citation lane's own `installed_etag` merge-base rule, untested by any run); plan lines 819-826 (§10.1d defect 3, 'Both lanes').


## [U13] MAJOR — §4's substrate requirement that the pending set dies with `CrashPod` is not implemented — the stage survives pod replacement in the model

**Plan section:** §4 (substrate bullet: '`pending` — ... dying with pod-replacement `CrashPod`')  ·  **Reviewer:** formal-coverage


`stage`, `stageBase` and `withheldDel` are grouped into `gatedVars`, and `Next` composes `UNCHANGED gatedVars` over the whole of `BaseNext` — which contains `CrashPod`. A pod replacement therefore leaves the crashed incarnation's entire staged-uncited pending set intact in the model, the exact opposite of the design (the pending record lives in the emptyDir; §2.6 'pod recreation ... destroys the emptyDir pending record and converts the whole uncited window into D9 orphans'). Combined with `MaxCrashes = 0` in every gated cfg this is currently latent, but it means the D9/orphans story has no model artifact and cannot get one by simply raising the crash budget: doing so would first have to fix the frame. It would also make `Inv_BoundaryAtomic` evaluate a dead sidecar's frozen `citeDone` against a `Valid(s)` that keeps moving as the inbox changes — a false-positive shape in the split-citation mutation.


**Evidence:** lean/formal/LeanSubtree.tla:205 (`gatedVars == <<stage, stageBase, withheldDel>>`), 432-450 (`CrashPod` clears only the sentinel fields; `stage`/`citeDone` untouched), 1520-1522 (`Next == (BaseNext /\ VersionsFollow /\ UNCHANGED gatedVars) \/ GatedNext`), 1598-1601 (`Inv_BoundaryAtomic` quantifies over all sidecars); plan line 306 (the requirement) and lines 210-213 (D9).


## [U14] MAJOR — `ProbeRawReaderSeesUncited` is satisfied by a plain HITL write at depth 1 — it proves nothing about residual 11 and cannot act as the regression fence §4 assigns it

**Plan section:** §4 (product 2 probes / reader substrate), §10.1c  ·  **Reviewer:** formal-coverage


The probe is the state predicate `\A p : objects[p] = manifest[p] \/ manifest[p] = 0`. `LeanProbeRawUncited.cfg` runs it with `MaxHitl = 1`, and `HitlWrite` is enabled in the initial state (window = 0, `InboxEnabled = TRUE`): one step sets `objects[p] := 2` while `manifest[p]` stays 1, violating the probe before any `StagePut` — indeed before `StartA`. So the counterexample TLC reports is an ordinary HITL write, a state that exists in cadence and hybrid too, not the gated staging lane's uncited current version that §3 residual 11 describes. Two consequences: the exposure is not proven present, and the stated regression fence ('a future design that quietly closes it must fail the probe and force the doc to change') does not work — a design that abolished uncited staging entirely would still fail this probe. Setting `MaxHitl = 0` in that cfg (or stamping an action ghost in `StagePut`, per the house rule the module itself cites) would make it real.


**Evidence:** lean/formal/LeanSubtree.tla:1692-1694 (the predicate; the comment concedes 'It names a state rather than an action on purpose'), 568-583 (`HitlWrite` — no `Running` guard, enabled at Init, leaves `manifest` unchanged under `InboxEnabled`); LeanProbeRawUncited.cfg (`MaxHitl = 1`, `GatedCitation = TRUE`); plan line 308 and line 762.


## [U15] MAJOR — Nothing in the gate requires a withheld delete to actually LAND at a citation; §4's `ProbeGC` re-run under gated was not built

**Plan section:** §4 (product 2 probes)  ·  **Reviewer:** formal-coverage


§4 requires '`ProbeGC` re-run with `GatedCitation=TRUE`' among product 2's probes. `LeanProbeGC.cfg` is the tranche-1 cfg with `GatedCitation = FALSE`, and no gated cfg carries a probe over `gh.gc`. The probe that looks like it covers this — `ProbeWithheldDelete` — counts `gh.withheld`, which is incremented in `LaneDone` when the delete is WITHHELD, never in `CiteFinish` when it is applied. So `LeanGatedHolds` can hold with `dels = {}` on every citation, and the delete-application half of the gated design (manifest entry removed, object deleted, version reaped, `Inv_NoResurrection` exercised) is never proven reachable. That is the same vacuity the plan's own house rule was written against, in the one product whose invariants (`Inv_CitedVersionLives`, `Inv_NoUncitedGC`) are most at risk during a delete-plus-reap step.


**Evidence:** plan line 314 ('`ProbeGC` re-run with `GatedCitation=TRUE`'); lean/formal/LeanProbeGC.cfg (`GatedCitation = FALSE`, `MaxHitl = 0`); LeanSubtree.tla:1083-1092 (`LaneDone` increments `gh.withheld`), 1180-1237 (`CiteFinish` computes `dels` and increments `gh.gc`), 1660 (`ProbeWithheldDelete == gh.withheld = 0`); check.sh has no gated `ProbeGC` invocation.


## [U16] MAJOR — Product 4 is declared done while two of the three artifacts §4 specified for it were never built — including the probe that carries D4's positive claim

**Plan section:** §10.2 (vs §4 product 4, §2.4.2 'Pinned in product 4')  ·  **Reviewer:** formal-coverage


§4 product 4 names two mutations and two probes. On disk there are three runs: `LeanScopedSyncHolds`, `LeanScopedSyncWholeBase`, `LeanProbeScopedDeferral`. Missing: (a) the repair-only-pass-installs-withheld-tombstones mutation — §2.4.2 states the repair-only tombstone-withholding rule is 'Pinned in product 4', and no constant, action or cfg exists for it, so the torn-rename hazard it names is unpinned anywhere in the model; (b) `ProbeOutOfScopeLater`, which is the only artifact that would show a deferred out-of-scope change actually INTEGRATES at a later consume. `ProbeScopedDeferral` fires on `gh.scopedDeferrals`, written inside `Sync` when the change is deferred — it proves the deferral happened, not that the deferred entry ever arrives, which is the entire reason D4's loss-avoidance argument is acceptable. `Inv_NoForeignLost` is likewise a stamp inside `Sync`, not an eventual-integration property. §10.2 reports the tranche as complete and names neither omission (drill leg B5's anti-vacuity guard asserts exactly the missing 'present after the next barrier' half — and the B-drill does not exist).


**Evidence:** plan line 316 (§4 product 4) and line 184 ('Pinned in product 4'); lean/formal/check.sh tranche-3 product-4 block (three runs); LeanSubtree.tla:1651 (`ProbeScopedDeferral == gh.scopedDeferrals = 0`), 917-1010 (`Sync` — `scopedDeferrals` written at the deferral, no later-integration ghost), 1567 (`Inv_NoForeignLost == ~gh.foreignLost`); plan §10.2 lines 906-925.


## [U17] MAJOR — §10.3's argument for deferring product 3 is unsound: its named fallback does not exist, and the invariants §4 gave it do not depend on the unbuilt proxy fence

**Plan section:** §10.3 ('Product 3 — defer, unchanged')  ·  **Reviewer:** formal-coverage


§10.3 reduces product 3 to 'a required-reachable probe' whose fence 'is P5 at the proxy, which is not built', and defers the rest to 'drill leg B12'. Both halves fail. (1) §4's product 3 also specifies a mutation — 'straggler drops successor-queued inbox entries unfenced — must fire against `Inv_HITLDurable`' — plus 'the successor never cites a foreign-epoch version'. Neither fence is P5: both are shipped flint code (the CT-5 fold: epoch-cell re-reads and cell-compared window ops). Nothing in the gate covers them: every cfg in which a deposed-but-alive writer can act (`AllowStall = TRUE`) sets `MaxHitl = 0`, so `Inv_HITLDurable` is never evaluated in a straggler world at all, and the model's `Consume` has no epoch guard whatsoever — a thawed zombie can empty the successor's inbox with no stamp anywhere, because `gh.amputated` is written only in `Upload`, `GCDelete`, `CASInstall` and `CitePassStep`. (2) The B-drill does not exist: lean/e2e/run-chaos.sh registers C1–C12 only, so 'B12 covers the shape better than a model would' defers coverage to an unwritten leg, in a plan that also declares Phases 1–3 done against gates listing B1–B21.


**Evidence:** plan lines 946-951 (§10.3), 315 (§4 product 3), 181 (CT-5's fold); AllowStall = TRUE cfgs — LeanSubtreeTakeover, LeanEpochOnlyHolds, LeanNoRotate, LeanNoEpochCheck, LeanProbeStragglerAttempt, LeanScopedSyncHolds/WholeBase, LeanProbeScopedDeferral, LeanSentinelDeposal, LeanSentinelFencedAck, LeanProbeRefusedAck — all `MaxHitl = 0`; LeanSubtree.tla:601-628 (`Consume`: `Running(s)` only, `inbox' = {}`), 370-376 + 673-700 + 706-731 + 794-882 + 1126-1178 (the four `amputated` stamp sites); lean/e2e/run-chaos.sh:751-762.


## [U18] MAJOR — The model's gated citation replaces the shipped three-way merge, foreign-entry queueing and CAS-retry with a plain overwrite, and the abstraction ledger does not say so

**Plan section:** §4 (product 2 substrate/ledger), §10.1c  ·  **Reviewer:** formal-coverage


In shipped code the citation lane runs the SAME `manifest::merge` as the fused barrier — merge base, foreign-entry detection, withheld deletes, parked set, the `installed_etag` own-base rule, and a bounded CAS-retry loop that re-verifies deposal on 412. In the model `CitePassStep` installs `IF p \in sub THEN stage[s][p] ELSE manifest[p]` — no merge base, no foreign detection, no `foreignQ` inbox queueing, no retry. Consequences: `MergeCapable` is inert under `GatedCitation` (because `CASInstall` is disabled), so `Inv_HITLDurable` in `LeanGatedHolds` cannot exercise any merge behaviour; the `MineIsNotForeign = FALSE` mutation — the one that found §10.1d defect 3 — has no effect in gated mode; and the class of defects the tranche-1 mutations exist to pin (whole-rewrite install, foreign entry dropped without queueing) is structurally unreachable in the mode whose citation reuses that very code. The module's gated abstraction list names only two collapses (citation+reaper are one step; the four sources collapse to nondeterminism) and §4's ledger obligation is unmet for the merge. This is the 'the abstraction was the bug' shape the plan says it is guarding against.


**Evidence:** lean/formal/LeanSubtree.tla:1126-1178 (`CitePassStep`) vs lean/sidecar/src/gated.rs:531-590 (`manifest::merge(base, &theirs, &upserts, &deletes, &parked)`, `prev_installed == expected` own-base rule, `attempt > 4` retry, `foreign` returned); LeanSubtree.tla:794-882 (`CASInstall`, gated off by `~GatedCitation` at 795); LeanSubtree.tla:1005-1011 and lean/formal/README.md:213-219 (the stated abstractions, merge not among them).


## [U19] MAJOR — The model's upload lane opens the HITL window; the shipped lane deliberately opens none — removing the mid-lane interleaving product 2 exists to search

**Plan section:** §2.4.1 (windowless lane) vs §4/§10.1c's product 2  ·  **Reviewer:** formal-coverage


`Scan` sets `window' = sc[s].epoch` and is the entry step of both the fused barrier AND the gated lane (`StagePut` requires `pc = "scanned"`). With `WindowCheck = TRUE` in every gated cfg, `HitlWrite` is disabled from the moment the lane starts until `LaneOnly`/`CiteFinish` clears it. The shipped lane opens no window at all — that is CT-3/SD-5's fix, the premise of §10.1c's own defect ('the lane opens no HITL window ... so a UI write can land on a path the lane has ALREADY staged') and of leg B19. So the model forbids precisely the interleaving the code permits: a HITL write arriving between `StagePut(p)` and the citation. §10.1c's counterexample had to squeeze through the narrower `Consume`→`Scan` gap instead. The infidelity shrinks the search space of the product whose whole subject is a foreign write racing the lane, and it is in neither the module's gated abstraction list nor the README's.


**Evidence:** lean/formal/LeanSubtree.tla:629-651 (`Scan`, `window' = sc[s].epoch` at 641), 1035-1041 (`StagePut` requires `pc = "scanned"`), 568-572 (`HitlWrite` guard `~(WindowCheck /\ window # 0)`); every gated cfg sets `WindowCheck = TRUE`; lean/sidecar/src/gated.rs:38-42 ('**The lane opens NO HITL window**') and gated.rs:490-494 (window opened inside `citation_pass` only); plan lines 162, 179 and leg B19 (line 6 of §6 table).


## [U20] MAJOR — §4's two reader actions were never built, so D13 (`pinned_reads`) and the dangling-citation endgame have no model artifact

**Plan section:** §4 (reader substrate), §10.1c  ·  **Reviewer:** formal-coverage


§4 requires `PinnedReader` (with its own invariant: never materializes post-boundary bytes under pre-boundary citations) and `RawReader`, and product 2's probe list includes `ProbeDanglingCitation` ('the recovery path must be reachable, not theoretical'). Neither reader action exists; the only reader is `CheckoutB`, which copies `manifest[p]` straight into `local` without consulting `versions` or `objects`. So the model cannot express the hazard D13 exists to prevent (a gated checkout 412ing and S3-wins-adopting `current[p]`, which §2.4.2 calls the rule 'the switch is incoherent without'), and it cannot express the endgame either: after `BackstopExpire` reaps a cited version, `Inv_CitedVersionLives` fires, but nothing shows that a checkout REFUSES rather than serving a hole, and nothing exercises `recover-staged`'s re-cite. Both are left entirely to the Rust battery (`pinned_reads_never_adopts_current`) and to leg B23, which does not exist. §10.1c's 'not modelled' list names `Inv_ManifestKeysUnderFiles` and the citation crash matrix — not the readers.


**Evidence:** plan line 308 (`PinnedReader`/`RawReader`) and line 314 (`ProbeDanglingCitation`); lean/formal/LeanSubtree.tla:535-546 (`CheckoutB`: `local = [p \in Paths |-> manifest[p]]`), 1246-1255 (`BackstopExpire`), 1638-1694 (probe list — no dangling-citation probe); plan lines 179 (D13), 208 (the endgame), 754-770 (§10.1c's omission list).


## [U21] MAJOR — §7's "O(1) requests per boundary" is false as implemented: the gated citation issues one ListObjectVersions per dirty path, re-incurring the copy bill at the same request price — and D10's drain sizing rests on the same wrong premise

**Plan section:** §7 (Gated, re-priced on §8 Q2); §2.4.2 (D7); §2.4.4 (D10 rule 3)  ·  **Reviewer:** security-dos


§2.4.2 and §7 sell versioned staging on "A citation becomes one manifest CAS naming versions that already exist — O(1) requests per boundary instead of O(dirty files)", explicitly retiring "14,400 COPY/day ≈ $0.07/day/workspace; at the 3,000-workspace fleet example, 43.2M COPY/day ≈ $216/day ≈ $6.5k/month plus ~500 sustained req/s", and §7 lists "no bucket-wide LISTs (staging list is prefix-scoped, claim-time and operator-cadence only)" among what the design refuses to spend. The shipped citation pass's exact version reaper loops over every upserted path and issues `list_versions(key)` — one ListObjectVersions per dirty key, serially, plus a DeleteObjectVersion per superseded version. ListObjectVersions is priced in the same S3 tier as COPY/PUT ($0.005/1k), so the plan's own worked example (50 dirty files cited every 5 min) costs 14,400 LIST/day — the retired bill, re-incurred 1:1, with only the copy's (already non-billable, intra-region) bandwidth actually saved. §7's conclusion that "gated citation traffic is now one CAS per boundary per workspace, the same order as the renewals" and that the proxy-QPS caveat is withdrawn does not hold. The same premise is load-bearing in §2.4.4/D10 ("§8 Q2 removes the per-*object* copy term … installing pending citations is one CAS") and in §10.1b defect 2, which re-fixed the drain on exactly that reasoning: the drain runs the same `citation_pass`, so a drain at the backlog cap (`stagedBacklogCapObjects` = 5,000) performs up to 5,000 serialized LIST round-trips plus deletes inside the grace budget the webhook is supposed to derive from "one CAS, no data movement". B23's oracle ("assert recovery moved zero data bytes (request oracle: one CAS, no PUT/COPY)") would also observe LIST+DELETE traffic it does not expect.


**Evidence:** lean/sidecar/src/gated.rs:637-669 (`for path in upserts.keys() { … self.store.list_versions(&key).await … delete_version(...) }`, sequential inside the citation pass); lean/sidecar/src/sentinel.rs:960-966 (the preStop drain calls the same `citation_pass(CitationSource::Drain)`); lean/sidecar/src/lib.rs:213-214, gated.rs:394-396 (`staged_backlog_cap_objects` = 5000 sets the upper bound on that loop); plan line 172 ("O(1) requests per boundary"), line 423 (the retired copy bill), line 425 ("Version-scoped DELETEs are free"), line 427 ("no bucket-wide LISTs"), §2.4.4 rule 3.


## [U22] MAJOR — The sidecar reads the forgeable `.flint/publish.ack` back as authoritative input to its own exactly-once state machine — a forged or forward-dated ack silently retires boundaries that never ran

**Plan section:** §1.2 (trust statement) and §2.1 (D2 — the uniform crash rule); §3 residual 6  ·  **Reviewer:** security-dos


§1.2 declares acks "advisory coordination signals, not attestations" and scopes the whole mitigation to consumers: "agent libraries MUST NOT treat an ack as proof of remote durability". §2.1's crash rule then makes the SIDECAR a consumer of that same forgeable file — "Crash after ack before retire → retire on restart (nonce match, no second barrier)" — and the code applies that test not only on restart but on EVERY honor: `honor_pending` calls `ack_matches` first and, on a match, retires the pending record and returns without renewing, without a barrier and without a citation. `ack_matches` accepts any ack whose `sentinel_mtime_unix_ns >=` the pending's, and for a bare touch (an explicitly supported form, "boundary with anonymous identity") the nonce test is vacuously true. Two consequences the plan does not carry: (a) any in-pod process can write `.flint/publish.ack` with a large `sentinel_mtime_unix_ns` and permanently, silently suppress every subsequent bare-touch boundary for the workspace — in gated mode that removes the sentinel citation source, leaving visibility to the lag cap; (b) no forgery is even needed, because the compared mtime is the agent's own file mtime and is arbitrarily settable (`utimensat`), so one touch written with a forward-dated mtime poisons the comparison for every later touch — and §2.1 tells bare-touch agents to match their boundary on precisely that field, so the poisoned ack is self-confirming. §3 residual 6 covers only "acks are forgeable in-pod" as an agent-side disambiguation problem. The plan needs a sidecar-side rule that does not trust the ack file (e.g. record the answered nonce/mtime in the state dir alongside the pending record and compare against that, or require `ack.completed_unix >= pending.consumed_at`).


**Evidence:** lean/sidecar/src/sentinel.rs:505-513 (`ack_matches` reads `.flint/<verb>.ack` via `read_ack`, tests `ack.sentinel_mtime_unix_ns < pending.consumed_mtime_unix_ns` and nonce containment only), :560-565 (`honor_pending`: match ⇒ `retire_pending` + return `Ok(None)`, before the renew and the barrier), :718-724 (the same test in `settle_pending_at_startup`), :233-238 (`mtime_ns` — the compared value is the sentinel file's own mtime); plan line 21 (§1.2), §2.1 crash matrix, §3 residual 6.


## [U23] MAJOR — D2.1's `S_ISREG` type check is a TOCTOU that leaves the FIFO wedge SD-8 claims to close, and a non-regular sentinel also drives unbounded per-tick conflict-record growth in a size-limited emptyDir

**Plan section:** §2.1 (D2.1 consumption discipline); §9 SD-8  ·  **Reviewer:** security-dos


§2.1 states the guard as "Type check first (review: security-dos): the poll `lstat`s the sentinel and consumes only `S_ISREG` — a FIFO, directory, symlink, or socket at the sentinel path is skipped … (a FIFO would block the body read forever)". The shipped sequence is `symlink_metadata(path)` → `read_bounded(path)` → `rename(path, staging)`, with `read_bounded` doing a plain `File::open`. The check and the open are separate syscalls on a path an unprivileged in-pod process fully controls, so the FIFO wedge is not closed, only narrowed: a loop that alternates `touch`/`mkfifo` at the sentinel path wins the race within seconds, `File::open` on a writer-less FIFO blocks indefinitely, and because the run loop's arms are serialized in one task (see the D12 starvation finding), that blocked open takes the lease-renewal and floor arms with it — a single unprivileged process can wedge the workspace into deposal. A directory swapped in at the same instant instead renames successfully into `.flint-sync/publish.consumed`, after which every consume attempt fails on `rename` onto an existing directory and the verb is dead for the pod's life. Separately, the non-racy path is itself a DoS: leaving any non-regular file at `.flint/publish` makes the sidecar append a `sentinel-not-regular-file` conflict record on EVERY poll tick — 86,400 records/day at the default 1 s poll, unbounded, into an emptyDir the operator gives a `sizeLimit` (which evicts the pod when exceeded), and `conflicts.jsonl` is read in full twice per sync honor. The plan's fold needs an atomic check (`O_NOFOLLOW|O_NONBLOCK` open then `fstat`) and rate-limiting or deduplication of the skip record.


**Evidence:** lean/sidecar/src/sentinel.rs:379-410 (`symlink_metadata` at :381, the `is_file()` gate at :387, `read_bounded(&path)` at :399, `rename` at :407), :240-249 (`read_bounded` → `std::fs::File::open`, blocking, follows symlinks); :388-395 (unconditional `append_conflict` per tick on the non-regular path); lean/sidecar/src/bin/flint_sync.rs:288-292, 331-343 (poll interval default 1 s; the poll arm body is awaited inline in the single select loop); spdk-csi-driver/src/lean_operator/inject.rs (emptyDir `size_limit` from `sizeLimitGib`); plan §2.1 D2.1, §9 SD-8.


## [U24] MAJOR — The sync sentinel is exempt from the work meter by construction, and its per-honor cost is a full manifest GET — an unmetered, agent-triggerable bandwidth amplifier §7 prices nowhere

**Plan section:** §2.2 (same budget pool) and §7 (Hybrid / what the design refuses to spend)  ·  **Reviewer:** security-dos


§2.2 says the sync sentinel shares "the same budget pool" as publish, and §7 asserts the design spends "no manifest GETs for news (HEAD stamp only)" and "no unbounded forced-barrier rate (budget)". In the shipped code the sync honor charges ZERO units unconditionally — the comment is explicit: "A sync publishes no bytes: it costs no budget units, only the min-interval" — so `sentinelHourlyBudget` never restrains it, while each honor performs a FULL `manifest::load` (a GET of the whole manifest document, not a HEAD) plus an inbox GET plus a full local scan. The only bound left is the shared 5 s min-interval: 720 full-manifest GETs/hour/workspace, triggerable by an agent (or a buggy library) touching `.flint/sync` in a loop. On the plan's own 1M-entry figure — a 264 MiB manifest taking 27 s to fetch and parse, the measurement that motivated HEAD-not-GET in the first place — that is ~186 GiB/hour/workspace of proxy egress and 5.4 hours of fetch work demanded per wall-clock hour, i.e. it also permanently occupies the single run-loop task (see the D12 starvation finding). D3.1's metering rule is byte-published-shaped, which is the right meter for publish and the wrong meter for a verb whose cost scales with manifest size; the plan needs a separate bound for sync honors (charge units by manifest bytes read, or a distinct min-interval/hourly cap), and §7 needs the term.


**Evidence:** lean/sidecar/src/sentinel.rs:700-706 (`honor_sync`: `self.charge_budget(0)?` with the "costs no budget units" comment), :335-346 (`sentinel_due` — one shared `last_honor_unix`, so sync honors are bounded only by the 5 s min-interval); lean/sidecar/src/sync.rs:113-121 (`manifest::load` + `inbox::load` per honor); lean/sidecar/src/manifest.rs:88-101 (`load` = `get_whole`, a full GET); lean/sidecar/src/barrier.rs:331-336 (the 27 s / 1.3 GiB / 264 MiB manifest measurement); plan §2.2, line 419 and line 427.


## [U25] MAJOR — D0.4's fleet-visible verdict is written from a pre-checkout snapshot, so on the pod-replacement path a workspace with disabled verbs advertises them as live — the opposite of B22's asserted oracle

**Plan section:** §2.0 (D0.4 pre-existing-data pre-flight); §6 leg B22  ·  **Reviewer:** security-dos


D0.4 requires that when the pre-flight trips, "the poll arm never arms, `capabilities.json` reports `\"verbs\": []` with the `\"reason\": \"preexisting-flint-paths\"`", and B22 asserts exactly that. On a fresh pod (the routine path on a pure-spot fleet) the ordering defeats it. `run_loop` computes the posture BEFORE checkout, when the baseline is empty, so signal (a) cannot fire and the verdict is `enabled: true`; checkout then populates `baseline.inst_base` from every manifest entry including the frozen `files/.flint/**` citations, re-runs the pre-flight, correctly persists `enabled: false, reason: preexisting-flint-paths`, and writes a correct `capabilities.json`; and then `run_loop` immediately overwrites that file using the STALE pre-checkout `posture` variable, advertising `verbs: [publish, sync, remote-seq]` with `reason: null`. The same stale value gates the startup settle and the poll arm's `if !posture.enabled { continue; }`. Actual consumption is prevented only because `sentinel_tick` independently re-reads the persisted posture — a second guard the plan does not describe and that a future refactor has no reason to keep — so the shipped outcome is the worst reporting case: the agent's ONLY discovery surface says the verbs are live, the agent touches `.flint/publish`, and nothing is ever consumed or acked. B22 will pass on a container-restart fixture (where the baseline already carries the citation) and fail on a replacement pod. Separately, D0.4's second signal is unreachable in `run`: `never_marked` is falsified by `run_loop`'s own marker write before checkout, so only signal (a) can ever fire on that path.


**Evidence:** lean/sidecar/src/bin/flint_sync.rs:247-248 (pre-flight and capabilities BEFORE checkout), :255-256 (`sc.checkout().await?` then `sc.write_capabilities(&posture, false)?` with the stale `posture`), :262 and :332 (stale value gates the settle and the poll arm); lean/sidecar/src/checkout.rs:236-247 (`baseline.inst_base = m.entries…` including refused control paths, then `sentinel_preflight()` + `write_capabilities` at the end of checkout); lean/sidecar/src/control.rs:186-208 (signal (a) reads `baseline.entries`/`inst_base`; signal (b) requires `!capabilities.json.exists()`); lean/sidecar/src/sentinel.rs:788-791 (`sentinel_tick`'s independent posture re-read — the only thing preventing consumption); plan §2.0 D0.4, §6 leg B22.


## [U26] MAJOR — D14's blast-radius argument mis-states what a leaked gateway bearer can already do: the same single token spans every workspace and already carries arbitrary manifest install and inbox-entry destruction

**Plan section:** §2.5 (D14, sync requests are carried never executed); §8 Q4  ·  **Reviewer:** security-dos


D14 justifies carrying rather than performing `sync_request` on the ground that performing it "would upgrade what a leaked gateway bearer can do from 'publish, plus hand over these N named objects' to 'rewrite and delete across a running agent's tree, at my timing, under a scope I choose'". That baseline is not the shipped surface. The gateway takes ONE bearer for ALL configured workspaces (`GatewayCore.token`, checked identically on every route), and that bearer already reaches `POST /lean/v1/{ws}/manifest` — an arbitrary manifest CAS, i.e. install any citation set, including one that uncites paths — and `POST /lean/v1/{ws}/inbox/drop`, which silently discards queued HITL entries so an acked UI write is never adopted into any manifest and is lost to every manifest-resolving reader. `require_current_epoch` is not an authorization control against a bearer holder: `GET /status` returns the cell's current epoch under the same token, so the caller simply reads it first. The conclusion (carry, don't execute) is still the right call, but the stated reason is unsound as written, and the plan's security posture in §7b ("D14 deliberately declines to widen the gateway bearer from publishing to rewriting a live tree") reads as a stronger property than the surface supports. The honest form names the sidecar-facing verbs as the dominant bearer capability and points at the per-workspace-token deferral as the fix, rather than resting the argument on a blast radius the bearer already exceeds.


**Evidence:** lean/sidecar/src/gateway.rs:62-64 and :189-201 (one `token`, one `authed` filter for every route including the sidecar-facing verbs); :555-594 (`handle_manifest_cas` — arbitrary `LeanManifest` installed after epoch validation), :534-548 (`handle_inbox_drop` — removes named entries), :456-494 (`handle_status_authed` returns `epoch` under the same bearer); :117-137 (`require_current_epoch` checks only that the claimed epoch equals the cell's current one); lean/sidecar/src/bin/flint_lean_gateway.rs:52-76 (one `FLINT_LEAN_GW_TOKEN` for the whole `id=prefix` map); gateway.rs:36-39 (the code's own "one shared bearer" v1 limit); plan §2.5 D14, §8 Q4, §7b Security bullet.


## [U27] MAJOR — §7's forced-barrier request shape (3 PUT + 3 GET) undercounts the shipped barrier by one PUT and three GETs — the inbox cell is CAS-written twice and re-read on every op

**Plan section:** §7 ("each forced barrier ≈ 3 PUT-priced (window CAS, manifest CAS, renew) + 3 GET-priced")  ·  **Reviewer:** ops-fleet


The hybrid storm pricing ($0.28/day/workspace uncapped, +$0.03/day/workspace at the budget) and the per-project proxy sizing note ("workspaces × (budget+floor)/hour barrier rate") both rest on 6 requests per forced barrier. The shipped barrier issues at least 10 control-plane requests: the window is opened AND cleared by two separate inbox CAS writes, and each of `open_window`/`clear_window`/`drop_entries` performs its own inbox GET before its CAS, on top of the consume's inbox GET; the epoch cell is read twice; and the merge GETs the whole manifest document. Counted: PUT-priced = renew, open_window CAS, manifest CAS, clear_window CAS (+drop_entries CAS when entries were consumed) = 4-5; GET-priced = epoch ×2, inbox ×3, manifest GET ×1 = 6. Corrected uncapped storm ≈ $0.39/day/workspace and ~1.7× the request rate the proxy sizing note asks operators to provision for.


**Evidence:** barrier.rs:303 and :498 (two `verify_not_deposed` epoch reads), :306 consume→`inbox::load`, :386 `inbox::open_window`, :390 `inbox::drop_entries`, :508 `manifest::load` (a whole-document GET — manifest.rs:88-100), :523 `manifest::cas_write`, :627 `inbox::clear_window`. Each of open_window/drop_entries/clear_window begins with its own `load(store,cfg)` GET then `cas_write` PUT (inbox.rs:129-152, 164-186, 190-212). Plan line 419.


## [U28] MAJOR — `stagedBacklogCapObjects`/`Bytes` and `noncurrentRetentionDays` are unreachable knobs — no env, no CRD, no chart; the backlog cap is settable only from unit tests and the retention value is read by nothing

**Plan section:** §2.6 knob table (rows `stagedBacklogCapObjects`/`…Bytes`, `noncurrentRetentionDays`), §8 Q3  ·  **Reviewer:** ops-fleet


This is the repo's recurring "knobs that exist and do NOTHING" class, in the section that exists to prevent it. `staged_backlog_cap_objects`/`_bytes` are live in the citation-source decision, but `bin/flint_sync.rs` parses no env for them (its list stops at quiesce/lag-bound), the CRD has no field, and the chart has no value — so every fleet runs the compile-time 5000/2 GiB forever. §2.6 justifies the knob solely as the "forced-citation source bounding the drain" and §8 Q3 gives an operator a tell for when to change it ("backlog-cap becoming the dominant citation source... means the cap is doing the pacing") — a revision path with no setting to revise. `noncurrent_retention_days` is worse: the field exists in LeanConfig with the documented default but has zero readers outside its own declaration, so D8's K=2 cross-validation (`noncurrentRetentionDays×86400 > 2×(visibilityLagBoundSecs + floor_secs)`) is implemented nowhere, in the sidecar or the operator. Phase 4 being unstarted explains the missing CRD field but not the missing sidecar env plumbing that the other six knobs already have — Phase 4 will have nothing to stamp against.


**Evidence:** Read at gated.rs:394-395 `if stage.entries.len() as u64 >= self.cfg.staged_backlog_cap_objects || bytes >= self.cfg.staged_backlog_cap_bytes`; declared lib.rs:213-218 with defaults at :239-241. Env parsing is bin/flint_sync.rs:91-96 only (min-interval, budget, poll, quiesce, lag bound); a repo-wide scan of `FLINT_SYNC_*` names yields no STAGED_BACKLOG or NONCURRENT variable. `noncurrent_retention_days` has no non-test reference anywhere in lean/sidecar/src. spdk-csi-driver/src/lean_operator/crd.rs:35-94 has no boundary fields; flint-lean-chart/values.yaml has no boundary block. Plan lines 257, 260, 467.


## [U29] MAJOR — D10 rule 2's "bounded retry within the remaining grace budget" is a fixed 3×/2 s loop with no grace input, and it delays the lease release the shipped path always performed — an unstated availability trade under defaults

**Plan section:** §2.4.4 D10 rule 2 and rule 3, §7b Availability ("degraded-to-today, never worse")  ·  **Reviewer:** ops-fleet


The rule says the drain "retries the final barrier within the remaining grace budget before releasing". The implementation retries a fixed three times with 2 s sleeps and has no notion of grace: the webhook sets no `terminationGracePeriodSeconds` (so the pod inherits the 30 s default — the plan's own stated hazard) and passes no grace value to the sidecar in any env var. The retry fires exactly when the store or proxy is misbehaving, which is when each attempt is slowest; three SDK-timeout-length attempts plus 4 s of sleeps can exceed 30 s and get SIGKILLed *before* `lease::release`, where the shipped single-attempt arm always released. A forfeited clean release costs the successor QUIET_POLLS=6 observations 10 s apart before it may supersede — on a pure-spot fleet where the plan itself calls pod replacement the routine path. This is a default-on mechanism (all three modes) trading the availability axis §7b says is never traded, and §7b does not state it.


**Evidence:** bin/flint_sync.rs:350-364: `for attempt in 0..3u32 { last = sc.drain().await; ... tokio::time::sleep(Duration::from_secs(2)).await }` then `let _ = lease::release(sc).await;`. No grace input: spdk-csi-driver/src/lean_operator/inject.rs:108-119 stamps 7 env vars, none grace-related, and inject.rs:127-145 builds the Container with no `termination_grace_period_seconds`; a grep for terminationGrace/preStop across the operator returns nothing. Successor wait: lease.rs:24 `QUIET_POLLS: u32 = 6` with the claim loop sleeping 10 s per poll (bin/flint_sync.rs:166-169). Plan lines 220, 448.


## [U30] MAJOR — §8 Q6's D1 corollary is recorded in one place, not the three the plan relies on — the parent plan and the CRD doc-comment both lack it

**Plan section:** §8 Q6 ("Recorded in three places on purpose"), §2.6 `perFilePublishCooldownSecs` row  ·  **Reviewer:** ops-fleet


Q6 hands `perFilePublishCooldownSecs` to the flush-profile tranche while keeping the binding constraint that a sentinel-honored barrier must never apply the cooldown (or the ack lies about a boundary that withheld a >64 MiB file). It states the mitigation explicitly: the constraint is written in three places "because a constraint written only in a plan that does not implement it is how it gets lost" — here, as an acceptance requirement on the flush-profile tranche, and in the knob's CRD doc-comment. Neither of the other two exists. `docs/plans/flint-lean-plan.md` mentions the per-file cooldown three times and contains zero occurrences of "sentinel", "boundary", or any never-thin rule, so the implementing tranche carries no obligation and no red-first test requirement; and there is no `perFilePublishCooldownSecs` field in the CRD to carry a doc-comment. The constraint therefore lives only in the plan that does not implement it — the exact failure mode the three-place rule was written to prevent.


**Evidence:** grep over docs/plans/flint-lean-plan.md: "cooldown" at :204, :299-300, :394 (all amplification/CR-surface context, none mentioning boundaries); grep for "sentinel|boundary-verbs|boundary verbs|D1 corollary" returns zero matches in that 485-line file. spdk-csi-driver/src/lean_operator/crd.rs:35-94 has no cooldown field. Plan lines 259, 492.


## [U31] MAJOR — §2.6 promises gateway `GET /status` fields that do not exist, and its "from the manifest HEAD it already can do" is wrong — the shipped handler does a whole-manifest GET

**Plan section:** §2.6 gauges/observability bullets, §1.2 trust statement, §3 residual 6  ·  **Reviewer:** ops-fleet


§1.2 makes the ack advisory and names gateway `GET /status` (`last_cited_seq`) as THE authoritative durability signal an agent library must consult before destroying local state, and §2.6's Phase-3 observability minimum says /status "gains `last_cited_seq`, `manifest_stamp_unix`, `boundary_source` (from the manifest HEAD it already can do)". None of the three fields exists in the shipped handler, and §10's Phase-3 observability row lists what landed (gauges.json, the withheld_reason stderr line, `flint-sync status`) without naming this bullet as remaining — so the trust statement the whole ack contract rests on has no implementation, and no phase owns it. The parenthetical is also wrong about the code: /status resolves the seq via `manifest::load`, a full GET of a document the plan elsewhere measures at 264 MiB / 27 s, so an agent following §1.2's advice pays a whole-manifest GET per check — the cost §7's "no manifest GETs for news (HEAD stamp only)" refuses to spend and the reason the barrier's own idle path uses HEAD.


**Evidence:** gateway.rs:457-495 `handle_status_authed` returns `Status { seq, window, inbox_depth, epoch, holder_id, holder_released, now_unix }`; `seq` comes from `manifest::load(core.store.as_ref(), &cfg)` (gateway.rs:465) which is `store.get_whole(&cfg.manifest_key(), None)` (manifest.rs:88-100). Contrast barrier.rs:330-336, the HEAD-not-GET idle path with its measured rationale. Plan lines 21, 275.


## [U32] MAJOR — Two drill legs — including the only proof of the hot-loops no-regression claim — belong to no phase gate

**Plan section:** §5 (all phases) vs §6 drill matrix  ·  **Reviewer:** coherence-residuals


§5 assigns drill legs phase by phase: Phase 0 → B13, B22; Phase 1 → B1-B4, B8, B16-B18; Phase 2 → B5-B7; Phase 3 → B9-B12, B15, B19-B21; Phase 4 → B23, B24. B14 and B25 appear nowhere in §5. B25 is the *only* empirical check of D3.1's work-metered budget, which §7b calls the defence of "the sub-axis the plan could most easily have cost" and which the hard no-regression constraint rests on; B14 is the only mixed-fleet/downgrade-detection leg, the fold for CT-6 (major) and half of MD-6 (major), and the thing that makes the "agents MUST check the capability marker" contract enforceable. Neither is demanded by any gate, so both can be silently skipped while every phase reads green. The §6 register instruction ("register with an 'of 25' count") cannot be reconciled with §5 either, because the Phase-5 gateway legs are unnumbered and so never enter the count.


**Evidence:** plan lines 341-372 (§5 phase gates, leg lists); plan line 404 (B25 row), line 396 (B14 row), line 406 ("Total: 25 legs"); plan line 437 (§7b hot-loops: "with B25 as the two-armed proof"); plan line 510 (CT-6 fold names B14).


## [U33] MAJOR — The `boundaryMode` knob row — the normative text for the CRD doc-comment — still prices the withdrawn copy design and omits the exposure §2.4.2 requires it to carry

**Plan section:** §2.6 knob table, `boundaryMode` row  ·  **Reviewer:** coherence-residuals


§2.6's table is explicitly the source for the CRD doc-comments ("Meaning / trade stated in the CRD doc-comment"), and Phase 4 implements it. Its `boundaryMode` row still reads "`gated` = **opt-in**: trades automatic-recovery RPO + copy requests + staging storage + proxy conformance". §8 Q2 withdrew the CopyObject engine, §7 says "The per-citation COPY term is gone", and §7b's own trade list correctly enumerates raw-key visibility instead. So the doc-comment an implementer would write names a cost that no longer exists and omits the permanent one — while §2.4.2, §3 residual 11 and §8 Q2 all require the raw-key exposure to be stated "in the CRD doc-comment" verbatim ("a reader that does not resolve through the manifest … observes mid-logical-change bytes"). The same row's "staging storage" wording also predates the switch (it is noncurrent-version storage now, bounded by `noncurrentRetentionDays`, which is a separate row).


**Evidence:** plan line 251 (knob table `boundaryMode` row) vs plan line 418 (§7 "the per-citation COPY term is gone"), line 446 (§7b trade list: RPO / raw-key visibility / storage / conformance / backstop), line 180 (§2.4.2 "says so in the CRD doc-comment and §3 residual 11"), line 294 (§3 residual 11).


## [U34] MAJOR — `forced-lag-cap` is measured from the last citation, not from the oldest staged work, so quiescent boundaries get stamped "forced" — B20's oracle and Q3's tell both misread it

**Plan section:** §2.4.1 (iii) / §6 B20 / §8 Q3  ·  **Reviewer:** coherence-residuals


§2.4.1 sells the bucket-visible `flint-boundary-source` stamp as letting "downstream consumers … distinguish a declared-coherent citation from a forced possibly-torn one *from the bucket alone*, and the fleet … count chronic forced citations" (OF-5's fold). The lag cap fires on `now - last_citation_unix >= bound`, and `last_citation_unix` only moves at a citation — while `citation_due` returns early when nothing is staged. So any workspace quiet for longer than `visibilityLagBoundSecs` stamps `forced-lag-cap` on the very next lane tick after a single write, even though the tree is quiescent and the boundary is perfectly coherent. Three consequences the plan does not account for: (1) the stamp does not mean what §2.4.1 tells consumers it means — a sparse-writer workspace's every boundary reads "possibly torn"; (2) B20 ("ONE write then silence ⇒ citation at ~`quiesceBoundSecs` with `boundary:"quiescence"` … FAIL if the source reads `forced-lag-cap`") fails or passes on how long the rig sat idle before the write, not on the mechanism; (3) §8 Q3's named tell for `quiesceBoundSecs` ("chronic `forced-lag-cap` … with `forced_citation_count` climbing while quiescence rarely fires: 30 s is too tight") points at the wrong knob for a workspace whose writes are simply sparser than the lag bound. Measuring the cap from the oldest staged entry (which the stage already records as `staged_unix`) would carry the meaning §2.4.1 assigns it.


**Evidence:** /Users/ddalton/github/flint/lean/sidecar/src/gated.rs:376-403 (`citation_due`: early return when the stage is empty, then `now.saturating_sub(stage.last_citation_unix) >= bound`); gated.rs:317-318 and gated.rs:699 (`last_citation_unix` set at first lane pass and at each citation only); gated.rs:357 (`staged_unix` per pending entry, unused by the cap); plan line 163 (§2.4.1 iii and the stamp's consumer claim), line 401 (B20 row), line 466 (§8 Q3 quiesce tell).


## [U35] MINOR — §4's constant list and budget table describe a model that was not built (`BoundaryEnabled`, `EagerStaging`, `MaxLanePasses`)

**Plan section:** §4 (opening paragraph and budget table)  ·  **Reviewer:** formal-coverage  ·  **Independently reported by 3 reviewers**


Three concrete drifts in the section a reviewer would use to audit coverage. (1) 'New constants: `BoundaryEnabled`, `GatedCitation`, `EagerStaging`, `SyncScope`' — the sentinel arm is `SentinelEnabled`, `EagerStaging` never existed and is a leftover of the copy design the same section withdraws, and eleven arms that do exist (`AtomicCitation`, `GCKeepsCurrent`, `CiteDropsInflightHitl`, `BackstopEnabled`, `MineIsNotForeign`, `ScopedInstBase`, `FoldPending`, `AckFromInstall`, `RefuseOnFence`, `FastPathGuards`, `MaxTouches`) are unnamed. (2) `MaxLanePasses` — introduced as the affordability device that 'keeps upload ticks from drawing `MaxBarriers`' and used in five of six budget rows — was never implemented; lane passes draw `MaxBarriers` (`Scan` increments `gh.barriers`), so every gated cfg is budgeted differently from the table. (3) The per-cfg budgets diverge row by row from disk (P2 gated×GC: table says `MaxHitl=0 MaxCrashes=1 MaxBarriers=1`, disk says `MaxHitl=1 MaxCrashes=0 MaxBarriers=2`; P4 scope: table says `MaxGen=3 MaxSyncs=2 MaxHitl=1`, disk says `MaxGen=2 MaxSyncs=1 MaxHitl=0`). §10.1c/§10.2 justify two of these changes; the table itself was never updated, so it now misdescribes the gate it was written to govern.


**Evidence:** plan lines 300, 319-328; lean/formal/LeanSubtree.tla:40-170 (CONSTANTS — no `BoundaryEnabled`, no `EagerStaging`, no `MaxLanePasses`), 629-651 (`Scan` increments `gh.barriers` for the lane too), 1347-1356 (`FastPath` also charges it); LeanGatedHolds.cfg, LeanScopedSyncHolds.cfg.


## [U36] MINOR — Stale internal cross-references contradict the tranche's own completion record

**Plan section:** header / §10.2 / §9  ·  **Reviewer:** drift  ·  **Independently reported by 2 reviewers**


Three spots were left un-updated as the doc grew: (a) §10.2's closing paragraph still says 'Products 1 (boundary × barrier × inbox, with the deposal arm) and 3 (straggler) remain unmodelled' — directly contradicting §10.3 ('Product 1 — DONE (§10.1d)'), §10.1d itself, and the headline's 49/49 which includes product 1's thirteen runs. (b) The header's Peers line still describes lean/formal/README.md as 'the 24-run TLC gate' while §10 claims 49/49 and the working-tree README says 'the 49-run gate'. (c) §9's GD-7 fold still reads 'Q1 now DECIDED — debounced 30-day DR raise with orphans-gated restore, standing ageout 3 → 7 (D8/D9)' — machinery §8 Q1 explicitly withdraws ('the raise, the debounce and the restore gate are withdrawn along with eagerOrphanAgeoutDays/drStagingAgeoutDays'), and GD-7 is absent from the §9 preamble's list of folds moved by Q2, so the preamble's 'read this before trusting a fold below' safety net does not cover it. Each is a small wound, but the doc's own device for surviving its rewrites (the preamble lists, the DECIDED annotations) fails at exactly these three points, and (a) will actively misdirect anyone deciding what to model next.


**Evidence:** Plan line 4 ('24-run TLC gate') vs line 565-566 ('formal gate 49/49') and lean/formal/README.md:16 ('the 49-run gate'); plan lines 929-931 ('Products 1 ... and 3 ... remain unmodelled') vs lines 936-941 ('Product 1 — DONE'); plan line 533 (GD-7 fold) vs §8 Q1 lines 458-459 (withdrawal) and lines 501-503 (preamble fold list omitting GD-7)


## [U37] MINOR — Consume-staging recovery runs only at startup: a transient fold failure strands a consumed touch mid-run, and the next consume's rename clobbers it, orphaning its nonce

**Plan section:** §2.1 (D2 consumption discipline / crash matrix)  ·  **Reviewer:** crash-takeover


The consume act is rename(sentinel → staging file), fold into the pending record, remove staging (sentinel.rs:403-410); the crash between rename and fold is recovered 'at startup, before the first poll' (recover_consume_staging, sentinel.rs:364-377) — and ONLY there. If `fold_into_pending` fails transiently mid-run (e.g. a momentary ENOSPC on the emptyDir when writing the pending record), the error propagates, the sentinel is already renamed away, and the touch sits in the staging file that no in-loop path ever re-examines: the boundary is neither honored nor acked until the next container restart. Worse, a subsequent agent touch renames the fresh sentinel ONTO the staging path (fs::rename replaces the destination), destroying the stranded touch's body — its nonce is then never named by any ack, the outcome `Inv_NoNonceOrphan` exists to forbid (the agent's re-touch loop on a missing nonce is the storm shape §2.1 itself warns about; the boundary is eventually covered by a later ack's mtime, so no data is lost). §2.1's crash matrix says 'Settle-before-consume guarantees a fresh sentinel can never clobber the surviving pending' — true for the pending record, but the staging file carries the same obligation between rename and fold and has no such guarantee mid-run. Cheap fix shape: run recover_consume_staging at the top of each poll tick (or fold staging before renaming a new sentinel onto it).


**Evidence:** sentinel.rs:364-377 (recovery called at startup only — sole call site is settle_pending_at_startup:717), sentinel.rs:403-410 (rename then fold then remove; fold error propagates leaving staging behind), sentinel.rs:407 (fs::rename over an existing staging file replaces it); plan line 93 (clobber guarantee stated for the pending record only), line 95 (D2/D3 consumption discipline with no mid-run staging recovery).


## [U38] MINOR — A gated no-diff honor (and the crash-after-citation re-run) produces an ok ack with seq null, breaking the ack schema and the §1.2 authoritative-durability recipe

**Plan section:** §2.1 (ack schema) / §2.4.1  ·  **Reviewer:** crash-takeover


The §2.1 ack schema defines `seq` as 'manifest seq installed by the honoring barrier' (non-optional in the JSON example), and §1.2 tells agents the authoritative durability check is comparing a remote read (gateway `last_cited_seq` / the manifest) against the boundary — which needs the ack to name a seq. In gated mode, `honor_publish_gated` sets `ack.seq = cite.seq`, and `citation_pass` early-returns `no_change` with `seq: None` when the stage is empty (gated.rs:425-428). Two reachable paths produce an ok ack with no seq: (a) a sentinel touch on a fully-cited tree (the priced no-diff honor); (b) the §2.1 uniform crash rule — crash after the citation CAS+baseline rewrite but before the ack, restart re-runs lane+citation, which now stages nothing and cites nothing, so the settling ack for a boundary that WAS installed carries seq null. The boundary claim itself is sound in both cases (every local byte already cited), but the cadence path's equivalent honor carries `Some(baseline.seq)` via the fast path (barrier.rs:341), so the two modes disagree, and an agent following §1.2's recipe has nothing to compare. The gated no-change arms should carry the current baseline seq (and its manifest etag) the way the cadence fast path does.


**Evidence:** gated.rs:425-428 (no_change return with CitationReport::default → seq None); sentinel.rs:657-675 (honor_publish_gated ack.seq = cite.seq, status 'ok'); barrier.rs:339-341 (cadence fast path sets report.seq = Some(baseline.seq)); plan §2.1 ack JSON (lines 68-80, seq non-optional) and §1.2 (authoritative remote check), sentinel.rs:148 (`seq: Option<u64>` with skip_serializing_if — the field vanishes from the ok ack entirely).


## [U39] MINOR — Three tests named in §5's gates for done phases have no equivalent in the shipped battery — including the only enforcement of §2.2's conflicts-ride-the-ack contract

**Plan section:** §5 Phase 1–3 gates  ·  **Reviewer:** drift


Of ~22 gate tests §5 names for Phases 0–3, 16 exist verbatim, two exist renamed with the same substance (sync_honor_refused_when_fenced → fenced_sync_honor_refused_and_tree_unmutated, tests.rs:1561; hitl_put_admitted_between_citations → hitl_admitted_between_citations, tests.rs:2098), and budget_exhaustion_defers_to_floor is absorbed into budget_meters_bytes_not_calls (which does assert 'sentinel-deferred' acks and Due::BudgetDeferred, tests.rs:1293-1302). Three have no recognizable equivalent: (1) sync_ack_carries_conflicts — §2.2 makes it contractual ('the conflict report rides the ack in full ... must survive the file transport') and sentinel.rs:685-705 implements it, but the only test that ever reads a sync ack is the fenced refusal (tests.rs:1587, read_ack(Verb::Sync)); no test asserts conflicts[] in the ack file (tests.rs:826/1831 assert on the directly-returned report, not the file transport); (2) clear_intent_keys_preserves_pending — never written; compounding it, §2.4.2 cites 'the clear_intent_keys preservation test (state.rs:180-212)' as if it already existed, but those lines were the load_intent/save_intent/clear_intent_keys functions at the plan's writing (verified against the pre-tranche commit), not a test — the property now holds structurally (pending.json is a separate file, gated.rs:79) but is asserted nowhere; (3) legacy_entry_without_version_id_uses_etag_path — no dedicated test (the legacy path is exercised incidentally by every pre-gated test, which is not the mixed-manifest case the gate named). The status table says these phases are done; a reviewer auditing gates by the plan's own names will find three missing.


**Evidence:** Plan lines 346-349, 352-355, 357-361 (named gate tests); grep 'fn <name>' lean/sidecar/src/tests.rs → 0 hits for sync_ack_carries_conflicts, clear_intent_keys_preserves_pending, legacy_entry_without_version_id_uses_etag_path, budget_exhaustion_defers_to_floor; tests.rs:1561,2098,1266-1302,1587,826,1831; sentinel.rs:173,685-705; git show eb392653^:lean/sidecar/src/state.rs lines 180-212 (functions, no #[test]); gated.rs:79


## [U40] MINOR — `MineIsNotForeign` breaks §4's preservation-by-construction rule — it is TRUE in all 49 cfgs and changes the merge every tranche-1/2 run exercises

**Plan section:** §4 (opening: 'new CONSTANTS default-FALSE in every existing cfg so tranche-1/2 state spaces are preserved by construction')  ·  **Reviewer:** formal-coverage


Every other tranche-3 arm is gated behind `SentinelEnabled`/`GatedCitation`/`SyncScope` and is inert in pre-existing cfgs. `MineIsNotForeign` is not: it is defaulted TRUE by `gen-cfgs.sh` in all 49 cfgs and it feeds `foreign(p)` inside `CASInstall`, which every tranche-1 and tranche-2 run executes. It is reachable there (LeanSubtree.cfg has `MaxCrashes = 1`, so `CrashPod` → `ClaimB` → a successor install is reachable with `MaxHitl = 1`), so the earlier state spaces are NOT preserved: the '24/24' baseline the plan cites certified a different merge rule from the one those same cfgs now check. Nothing is silently green — `check.sh`'s `mutation_run` still demands each tranche-1 counterexample by name — but the stated invariant of the extension discipline is violated and the plan should say the tranche-1 runs were re-derived rather than preserved.


**Evidence:** lean/formal/gen-cfgs.sh:34 (`local c_MineIsNotForeign=TRUE`) and :281 (FALSE only for LeanSentinelStaleMergeBase); `grep -h MineIsNotForeign *.cfg | sort | uniq -c` → 49 TRUE, 1 FALSE; LeanSubtree.tla:825-834 (`foreign(p) == ... /\ (MineIsNotForeign => manifest[p] \notin sc[s].known)`), 432-450 + 524-534 (crash → ClaimB path); plan line 300.


## [U41] MINOR — Several cfgs list invariants their own world cannot falsify, inflating apparent per-product coverage

**Plan section:** §4 (product 2 'Inv_NoResurrection under withheld deletes'), §10.2  ·  **Reviewer:** formal-coverage


`Inv_NoResurrection` is written only by `Restart` (via `RematerializeOnRestart`), yet it is listed as a checked invariant in `LeanGatedHolds.cfg` (`MaxRestarts = 0`) and `LeanScopedSyncHolds.cfg` (`MaxRestarts = 0`), where `Restart` is disabled. `Inv_HITLDurable` is listed in `LeanScopedSyncHolds.cfg` with `MaxHitl = 0`, where `HitlWrite` is disabled. Those lines read as coverage and are unfalsifiable by construction. It matters most for §4's explicit claim that product 2 checks 'Inv_NoResurrection under withheld deletes': gated mode WIDENS the resurrection window (a delete stays cited until a citation, which is exactly the `local[p]=0 /\ manifest[p]#0 /\ baseline[p]#0` shape the invariant's `res` predicate tests), and that is the one configuration in which the invariant cannot fire.


**Evidence:** lean/formal/LeanSubtree.tla:452-478 (`Restart` is the sole writer of `gh.resurrected`; `Running(s) /\ gh.restarts < MaxRestarts`), 1556-1557; LeanGatedHolds.cfg (`MaxRestarts = 0` + `INVARIANT Inv_NoResurrection`), LeanScopedSyncHolds.cfg (`MaxHitl = 0`, `MaxRestarts = 0` + `INVARIANT Inv_HITLDurable`, `INVARIANT Inv_NoResurrection`); plan line 314.


## [U42] MINOR — The gate's own run-class accounting in `README.md` is wrong in all three numbers

**Plan section:** §4 ('bump `check.sh`'s `$PASS` and the README count'), §10 (formal gate 49/49)  ·  **Reviewer:** formal-coverage


README states 'Forty-nine runs, ALL required: 9 strict (must hold), 21 mutations ..., 19 probes'. The actual split in check.sh is 10 strict runs, 19 non-probe mutation runs and 20 probe runs (49 total, which is right). The strict count is the one worth fixing: it is the number of configurations in which the invariants must HOLD, and it is the figure a reviewer would use to judge how much of the gate is positive evidence versus counterexample-hunting.


**Evidence:** lean/formal/README.md:20-24; lean/formal/check.sh — `grep -cE '^(strict_run|mutation_run) \$M' check.sh` = 49, of which 10 are `strict_run` invocations (LeanSubtree, LeanSubtreeTakeover, LeanNoWindowHolds, LeanEpochOnlyHolds, LeanSyncHolds, LeanScopedSyncHolds, LeanGatedHolds, LeanSentinelHolds, LeanSentinelRestart, LeanSentinelDeposal) and 20 are `LeanProbe*` cfgs.


## [U43] MINOR — §2.6's `BoundaryModeActive` echo is priced as free, but the lease cell has no payload field — §10.1b already found this and §2.6/§7b still assert the closure

**Plan section:** §2.6 Status ("the sidecar echoes {...} into the lease-heartbeat cell it already writes (D12 renews it every ≤30 s — the echo is free)"), §7b Day-2 operations  ·  **Reviewer:** ops-fleet


The operator↔sidecar mixed-version hole (OF-3) is closed in the plan by an echo asserted to be free because it rides an existing renewal. `EpochState`/`EpochLease` carry no free-form payload, so the echo requires a shared flint-store schema change that also touches the hub's arbitration path, or a separate heartbeat object at ~120 PUT/hour/workspace — which is the "instrument reports on itself" tax the plan elsewhere refuses. §10.1b states this and defers the echo to Phase 4, but §2.6 still prices it as free and §7b still claims "the capability marker + heartbeat echo make **both** mixed-fleet directions (agent↔sidecar, operator↔sidecar) detectable instead of silently hazardous" — a closure that, per §10.1b, is neither implemented nor free.


**Evidence:** crates/flint-store/src/lib.rs:391-404 `EpochState { holder_id, epoch, token, last_renew_unix, released }` and :407-412 `EpochLease { holder_id, epoch, token }` — no payload field; the S3 body written by `epoch_put`/`epoch_renew` is that fixed shape (s3.rs:674-681). Plan lines 266, 442, and §10.1b's own deferral note.


## [U44] MINOR — §2.6 and §7b call `cadence` the escape hatch that "pins the exact pre-boundary behavior", but sentinels still force barriers in cadence mode — §2.4.1 and the code both say so

**Plan section:** §2.6 knob table (`boundaryMode` row), §7b opt-in trade list  ·  **Reviewer:** ops-fleet


§2.6 describes `cadence` as "pre-boundary behavior" and §7b lists "`cadence` mode pins the exact pre-boundary behavior (the escape hatch)" among the opt-in trades. §2.4.1 contradicts this in the same document — "Sentinels still work (a sentinel triggers a fused barrier); this mode exists as the explicit 'old behavior' escape hatch" — and the code agrees: the poll arm consumes and honors regardless of `boundary_mode`, which only selects fused-vs-gated inside the honor. An operator reaching for the escape hatch because a storming agent is driving forced barriers and request cost gets no relief from it; the knob that actually stops the verbs is `sentinels: off`. Since the escape hatch is the fallback the no-regression argument leans on, the two sections should say what §2.4.1 and the code say.


**Evidence:** sentinel.rs:786-812 `sentinel_tick` polls and honors with no mode check (its own doc comment: "In `cadence` mode the arm still consumes and honors"); `honor_publish` branches only on `self.is_gated()` (sentinel.rs:604-606). Plan lines 159, 251, 450.


## [U45] MINOR — §2.3's news-freshness contract was not updated for gated mode, where the ticker only advances at citations

**Plan section:** §2.3 (D5) vs §10.1b  ·  **Reviewer:** coherence-residuals


§2.3 states that `observed_seq` comes off the barrier's existing manifest HEAD and that "Freshness of *news* is bounded by the floor tick". §10.1b records a design call that contradicts it: in gated mode the upload lane issues no manifest HEAD, so "inbound news therefore refreshes at coherent points" and "an agent learns of a sibling's publish up to `visibilityLagBoundSecs` later than it would in hybrid". That deviation is honestly named where it was decided but never propagated to §2.3, to the agent-facing contract text, or to §3's residual list — so the mechanism section an agent-library author reads still promises floor-bounded news, and a gated workspace's `observed_seq > integrated_seq` signal can lag by the full lag bound. (The `updated_unix` liveness heartbeat is unaffected — `ticker_from` runs on every floor tick — so the "3×floor ⇒ sidecar problem" rule still holds; only the news half is wrong.)


**Evidence:** plan line 151 (§2.3, "Freshness of *news* is bounded by the floor tick") vs plan lines 684-693 (§10.1b, "The news ticker moves at citation points in gated mode"); /Users/ddalton/github/flint/lean/sidecar/src/sentinel.rs:889-896 (gated arm passes `cite.seq` to `ticker_from`); control.rs:283 (`updated_unix = now_unix()` on every call).


## [U46] MINOR — Three §9 ledger dispositions still describe machinery §8 Q2/D8 withdrew, and the §9 preamble itself contradicts §2.4.2

**Plan section:** §9 review ledger (GD-7, GD-4, CT-7 folds + preamble)  ·  **Reviewer:** coherence-residuals


The §9 preamble exists to flag folds moved by the versioned-staging decision, but three survive uncorrected. GD-7's fold still reads "Q1 now DECIDED — debounced 30-day DR raise with orphans-gated restore, standing ageout 3 → 7 (D8/D9)", every element of which §8 Q1 and §2.4.3 explicitly withdraw ("the raise, the debounce and the restore gate are withdrawn along with `eagerOrphanAgeoutDays`/`drStagingAgeoutDays`") — and GD-7 is not in the preamble's list of moved folds. GD-4's fold cites "sweep uses `eagerOrphanAgeoutDays` never 3600", a knob that no longer exists. Worse, the preamble asserts that "CT-7's stage-ordering and **NotFound arm** … now apply to *versions of real keys*", while §2.4.2's list of what the switch **deletes** names "the stage-NotFound arm" outright — and D7's write-ordering paragraph explains why it is unnecessary ("a crash between them leaves an uncited version … never a pending entry naming a version that does not exist"). A reviewer auditing the ledger for coverage would look for three mechanisms that were deliberately removed.


**Evidence:** plan line 502 (§9 preamble, "CT-7's stage-ordering and NotFound arm … now apply to versions of real keys") vs plan line 172 (§2.4.2 deletes "the stage-NotFound arm; the lifecycle-refresh re-PUT"); plan line 526 (GD-7 fold) vs plan line 206/458 (§2.4.3 and §8 Q1 withdrawing the raise/debounce/restore and both ageout knobs); plan line 523 (GD-4 fold citing `eagerOrphanAgeoutDays`).


## [U47] MINOR — D0.2's promised warning conflict record for frozen legacy `.flint/` citations does not exist, and its gate test does not assert it

**Plan section:** §2.0 D0.2 / §5 Phase 0 gate / §6 B13  ·  **Reviewer:** coherence-residuals


D0.2 says legacy `files/.flint/...` citations "are carried forward frozen (never re-uploaded, never deleted by us) and a one-time warning conflict record (`conflicts.jsonl`) names them". §5's Phase-0 gate spells the test out as "… → still cited, **warning record present**", and B13's row says the same. `classify` simply `continue`s on control paths with no `append_conflict`, and the shipped `legacy_flint_citation_survives_upgrade` asserts only that the citation and object survive. The record is the only in-pod tell that publishing has silently stopped for those paths — the breaking change D0.4 names — so an upgraded workspace using `.flint/` as data gets no signal at all from either half of D0 (see also the D0.4 pre-flight finding).


**Evidence:** plan line 43 (D0.2), line 344 (Phase 0 gate, "warning record present"), line 395 (B13 row); /Users/ddalton/github/flint/lean/sidecar/src/scan.rs:117-122 (`if is_control_path(path) { continue; }`, no conflict emitted); /Users/ddalton/github/flint/lean/sidecar/src/tests.rs:962-968 (assertions are citation-survives and object-not-GC'd only).


## [U48] MINOR — §1.3's "no new remote surface" non-goal was not updated when §8 Q5 decided a pod-network-bound metrics listener

**Plan section:** §1.3 non-goals vs §8 Q5 (D15)  ·  **Reviewer:** coherence-residuals


§1.3, marked "(v1, DECIDED)", says "**No new remote surface for the app.** The app container gains zero network reachability. The optional UDS/localhost door (§2.5) is pod-internal and opt-in; the optional gateway verb is for HITL, not the app" — an enumeration of the only listeners v1 adds. §8 Q5 then decides D15, and explicitly rejects loopback: "the listener binds the pod network (`0.0.0.0:<port>`, default off), and the exposure is bounded by NetworkPolicy rather than by the bind address", shipped in Phase 6 of this same plan. §7b carries the correction ("D15's `/metrics` is the one listener that binds the pod network"), but §1.3 — the section a reader treats as the scope contract — was never amended, so the non-goal list is false as written for v1.


**Evidence:** plan line 27 (§1.3 third non-goal) vs plan lines 471-478 (§8 Q5/D15, pod-network bind and its rationale) and plan line 344 of §5 Phase 6; plan line 438 (§7b security bullet naming the exception).
