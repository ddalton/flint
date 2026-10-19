# The lean protocol, on one page

The objects and the rules of lean's publish protocol as they ship, written
from the model of the shipped shape (`formal/LeanP1.tla`), in the style of
Raft's figure 2: state, then rules as guard and effect, then the invariants
as predicates on state. Every rule names the invariant it holds up and the
model world that fails when the rule is taken away. `SAFETY.md` §3.2 says
which of these claims is checked, and how far; `FINDINGS.md` has every
defect the rules were written against.

This is the shape since simplification step 5 (2026-09-25): the gateway
COMMITS each UI verb itself (P2), a writer's baseline IS its merge base and
what a tree is owed is derived from the document at each consume (P1-lite),
and what a commit stops citing is collected only after the retire age G
(M1). The shape before it — the inbox cell's entries and removals, the
writer-local queue, citation repairs, the journal's installed document — is
`formal/LeanCore.tla` (and `LeanP2.tla` for the step between); neither
describes the code any more. The trace check (`formal/trace/TraceCore.tla`)
replays the code's conformance traces against `LeanP1.tla`: since
2026-10-17, 31 traces that between them take every rule on this page
(`formal/README.md`, "The code-trace check", says which steps are not
traced).

## The objects

| object | what it is | where it lives |
|---|---|---|
| **handle** `<<path, gen>>` | one immutable object: the path it was minted under and a generation unique across the run. Nothing overwrites a handle; only the retire reap, the collector (without G) and the orphan sweep delete one | `<prefix>/files/<path>@<flush>` |
| `live` | the handles that exist | the bucket |
| `doc`, `seq` | the committed document (path → handle) and the pointer's generation. The pointer CAS is the only commit, the gateway's and a writer's alike | the manifest and its pointer (`.flint/lean/current`) |
| `tomb` | per path, the handle the last delete there retired; cleared when the path is cited again | the manifest's tombstones |
| `base` | provenance: the handle each minted handle was written from | the upload's base; recorded in the model |
| `gw` | per path, a UI save's fresh handle between its PUT and its CAS | the gateway, in flight |
| `acked` | the pairs the gateway acknowledged (a save or a rename that COMMITTED) | the observation the third invariant is about |
| `udel` | the ACKNOWLEDGED deletes, with the version each removed: the gateway's (a UI delete) and a commit's (a writer's delete that left the document). Only grows. A ghost of the acknowledgements, not stored state: the tombstones are `tomb`, cleared on re-citation and pruned after `TOMBSTONE_KEEP_SEQS` (decided 2026-10-14) | the gateway's answer to the request; the commit's report (`BarrierReport::deleted`) and the publish ack |
| `conflicts` | conflict records, each naming a preserved copy | `.flint/conflicts` |
| `holder` | the one writer inside its commit section | the lease cell |
| `retiring`, `aged` | what a commit stopped citing less than G ago (the sweeps spare it), and at least G ago (a writer reaps it) | the retire logs, `.flint/lean/retired/{at}-{flush}-{uuid}.json` |
| `rdoc`, `rlag` | a reader's loaded document, and whether G has elapsed since it loaded | a ghost: a checkout, a gateway read |
| per writer `w[s]` | `local` (the tree), `baseline` (the version integrated at each path — and the MERGE BASE), `synced` (the pointer it last left the tree at, `Baseline::manifest_etag`), `derived` and `skipped` (THE CHEAP PATH'S RECORD, written only by the consume: the document it last derived against and the paths it left as the agent's work, `Baseline::derived_etag` / `skipped`), `integrated` and `took` (the trees' memory, per path), and the barrier in flight (`uploads`, `deletes`, `snap`, `upDone`, `gone`, `verified`, `inst`, `retire`, `collected`, `adv`) | the workspace: the tree, the baseline file |

What is NOT an object any more: the inbox cell's entries, `saw`, removals
and refused records (the cell now carries only the two verb requests,
"please publish" and "please pull"); the writer-local queue; the merge base
kept apart from the baseline (`instBase`); the journal's installed document.

## The rules

Guards read the state; effects write it.

| rule | when | what | holds up | pinned by |
|---|---|---|---|---|
| **GPut**(p) | no save, delete or rename in flight at p | mint h derived from `doc[p]` (the version the UI read); `live ∪= {h}`; not yet acknowledged | — | — |
| **GCas**(p) | a save in flight at p; NOT the lease — the gateway never waits for the writers (G1) | the CAS lands only over the version the UI read (`doc[p] = base[h]`): then `doc[p] := h`, the tombstone clears, ack `<<p, h>>`. Otherwise 412, nothing acknowledged, nothing recorded, and h is an orphan for the sweep. What a landed save replaced is not deleted here: nothing cites it, and the retire age then the sweep take it | `Prop_NoSilentRevert` | `LeanP1GatewayBlind` (a save judged against nothing) |
| **GRename**(p, q) | p cited, q not (`DestinationExists`), neither in flight | ONE CAS: `doc[q] := doc[p]`, `doc[p] := Nil`, the source's tombstone names the handle; ack `<<q, h>>`. A citation move: no bytes, no mint | `Inv_OneName` | `LeanP1RenameTwoCAS` (two CASes) |
| **GDelete**(p) | p cited, nothing in flight | one CAS: `doc[p] := Nil`, the tombstone names what it retired. Deletes no object | S3 | — |
| **Edit**(s, p) | — | mint h derived from the tree's newest version at p: its bytes (`local[p]`), else, while a barrier uploads p, the snapshot it publishes, else `baseline[p]`; `local[p] := h`. Provenance only (a ghost the claims read), never the merge base, which is the baseline: a restart between the CAS and step 7 surfaces the tree's own snapshot as `commit-surfaced-foreign` when the agent wrote since (NOTES.txt E27, E31, E33) | — | — |
| **Delete**(s, p) | `local[p] ≠ Nil` | `local[p] := Nil` | — | — |
| **Checkout**(s) | the writer is off | picks a scope T (`{Paths}` unscoped); `local`, `baseline` := `doc` on T, nothing elsewhere; `inst` := `doc`; `synced`, `derived` := `seq`; nothing skipped | — | — |
| **Consume**(s) | idle | THE CHEAP PATH first (the scan trigger): `derived = seq` and every `skipped` path still dirty ⇒ nothing is derived (one pointer GET, a stat per skipped path). Otherwise, owed = every path where `doc ≠ baseline`, the tree is clean (`local = baseline`) and the tree HOLDS (its baseline cites it, or its scope covers it): what the scope declined stays declined. Also taken: a dirty path whose bytes ARE the document's, or absent in both (content convergence — a restart between this writer's CAS and step 7 leaves exactly that). The tree and the baseline take `doc` there; `synced`, `derived` := `seq`; `skipped` := the paths left untaken, which are the agent's (dirty, the document differs). A path whose fetch or write fails stays as it was and owed; then `derived` := none and `synced` stays | `Inv_ShortcutSound`, `Inv_NoRegress` | `LeanP1OwedUnmarked` (skipped not re-checked), `LeanP1NoConvergence` (recorded) |
| **Scan**(s) | consumed | `uploads` := paths whose tree differs from the baseline and is present; `deletes` := any subset of the absent ones (the two-scan guard); `snap` := the tree | — | — |
| **Skip**(s) | consumed; nothing dirty; `seq = synced` | back to idle: no scan, no CAS | — | — |
| **Upload**(s, p) | scanned, p in `uploads` | the snapshot's handle lands, no condition; bytes PUT once before land at a COPY handle | R1 | — |
| **PullOnly**(s) | scanned; nothing to publish | `synced := seq`; `derived` untouched, so a document that moved since the consume is derived at the next | `Inv_ShortcutSound` | — |
| **Claim**(s) | scanned, every upload landed, something to publish, the cell free | `holder := s` | `Inv_OneHolder` | — |
| **Verify**(s) | claimed, not yet verified | `gone` := the uploads a sweep took: withheld, recorded, dirty again | `Inv_CitationsLive` | `LeanP1VerifyOff` (R4a) |
| **Install**(s) | claimed, verified | the three-way merge onto the CURRENT document, the merge base being the baseline. Mine = the uploads that survived the re-read, and the deletes; theirs = what differs from the baseline. Mine wins modify/modify, and R7 records theirs where mine publishes over a version this tree never integrated at that path. Mine re-creating a path theirs deleted records the deleted version. Mine DELETING over a version theirs changed APPLIES, and theirs is preserved with a record (M3, `commit-deleted-over-theirs`). The pointer CAS installs the result; what the document cited and the result does not is `retire`d and logged. Each delete that leaves the document is acknowledged (`udel`), as the gateway's is (E21). Nothing is marked owed here: a change the tree has not taken is found by the next consume, because the CAS did not replace the derived document (`adv` false) | `Inv_CitationsLive`, `Inv_OneName`, `Inv_AckedNamed`, `Prop_NoSilentRevert`, `Prop_DeleteSettles` | `LeanP1NoR7` (R7), `LeanP1DeleteOutranked` (M3) |
| **Collect**(s) | cased, not yet collected | with the retire age: nothing is deleted now — the CAS's retire log holds the set until it is G old. Without it: the retired set in one batch, sparing what the installed document still cites | `Inv_CitationsLive`, `Inv_ReaderFetches` | `LeanP1CollectorGreedy` (recorded), `LeanP1ReaderLoses` (no G) |
| **Sweep**(s, h) | inside a commit section; h live, cited by nothing, preserved by no record, not a save in flight (G3), not named by a retire log younger than G, not the sweeper's own upload | `live \= {h}` | `Inv_CitationsLive`, `Inv_AckedNamed` | `LeanP1SweepFree` (R4b), `LeanP1SweepNoGrace` (G3) |
| **Age** | — | G elapses: everything retiring is due; a reader that loaded before now has lagged | — | — |
| **Reap**(s, h) | inside a commit section; h retired at least G ago, not cited | `live \= {h}`; the log goes once its handles are gone (`untracked.rs::reap_retired`) | `Inv_ReaderFetches` | `LeanP1ReaderLoses` |
| **Finish**(s) | cased, collected | the baseline follows what was published (its uploads, its landed deletes); `synced := seq` if the install was the document; `derived := seq` only if the CAS replaced exactly the derived document (`adv`: the install is it plus this tree's own changes), and the published paths leave `skipped`; the section closes | `Inv_ShortcutSound` | `LeanP1AdvanceUnguarded` |
| **Restart**(s) | — | keeps the tree and the baseline (with `synced`, `derived` and `skipped`, on disk); drops the barrier in flight and releases the cell it held. No journal is read back: content convergence settles an upload the CAS already cited | `Inv_NoRegress`, `NR1` | `LeanP1ProbeRestartAfterCas` (reachable), `LeanP1Rematerialise` (NR1) |
| **RescopeBegin**(s) | idle, no rescope in flight | THE DOOR: a target scope T; refused if a path T drops (held, not admitted) has unpublished changes. Else the INTENT (T, the drop set) is durable before the first mutation | — | — |
| **RescopeFirst**(s) | an intent "saved" (a fresh one, or a replay after a crash) | keeps the still-cited dirty paths of the drop set (a replay must converge, not refuse); records what it drops (`ScopeIntent::held`) with the intent; UNCITES the rest (the baseline lets go) | `Prop_NarrowNeverDeletes` | `LeanP1NarrowUnlinkFirst` (unlink first, crash, replay: the absence publishes as a delete) |
| **RescopeSecond**(s) | the uncite landed | UNLINKS what it uncited — only while the tree's bytes there are still the recorded ones (L-131); then the WIDEN: fetches each admitted citation the tree does not hold, adopts bytes that ARE the document's, keeps a file the agent made there (L-130); the scope := T; `derived` := 0 (the next consume derives in full); the intent clears. A restart between the halves sends the replay back to RescopeFirst, which re-derives what it keeps from the RECORDED drop set; the barrier replays before any consume | `Prop_AgentWorkKept`, `Prop_ScopeRespected` | `LeanP1UnlinkBlind` (L-131), `LeanP1WidenOverwrites` (L-130) |
| **RPullRead**(s) | a reader, idle; NOT (the pointer is the one its last pull synced against and nothing is still owed: `derived` set and every `skipped` path still dirty) | reads the pointer (`rnow := seq`) | — | — |
| **RPullSync**(s) | the pointer read | a whole-tree sync against the document as it is now (converging as `Sync` does), recording what it derived as a consume would (a failed fetch: nothing derived); then `memo := rnow`, the pointer read BEFORE the sync, so a document that moved in between is pulled again | `Inv_ReaderSound` | `LeanP1ReaderOwedIgnored` (judged by the pointer alone, L-129) |
| **Touch**(s) | idle | the agent's publish request (`.flint/publish`, the gateway's `boundary_request`, the UDS door), consumed at once and folded into the pending record: its nonce appended (the oldest dropped past the cap), a new record minting its id, a fold into a standing one keeping it (L-133). It declares the tree as it stands: the paths changed whose bytes the document does not already hold, at any path, and not a narrow's leftover | `AK1`, `AK3` | `LeanP1FoldClobbers` (AK3), `LeanP1TouchDeclaresLeftover`, `LeanP1TouchDeclaresRenamed` (AK1) |
| **Honor**(s) | a writer, idle; a record pending and not yet answered; a barrier that began after its latest fold has ended | the ack: `ok` iff that barrier withheld nothing and the carrier dropped and uncited nothing for this record, else `partial` naming them; its seq and clock are the barrier's | `AK1`, `AK4` | `LeanP1AckIgnoresWithheld`, `LeanP1AckIgnoresUncited`, `LeanP1CarrierKeepsDelete` (AK1), `LeanP1StampIndependent` (AK4) |
| **HonorReader**(s) | a reader, idle, a record pending | the ack `refused-read-only`; nothing is published | `AK3` | — |
| **RetireByAck**(s) | idle; the ack answers the pending record | the record is retired, never re-run (the restart rule, `ack_matches`) | `AK3` | — |
| **Depose**(t) | scanned, uploads landed, something to publish; ANOTHER writer holds the cell (judged dead: the model has no clock) | `holder := t`; the manifest ROTATES (a content-identical CAS, `seq + 1`), so a CAS the deposed holder computed fails; the old holder is marked deposed | `DP1` | `LeanP1InstallUnfenced` (an install not fenced) |
| **Fence**(s) | deposed | before its CAS: the barrier is abandoned, nothing installed; after it: the collector is skipped (it deletes only under the lease) | `DP1`, `DP2` | `LeanP1InstallUnfenced`, `LeanP1DeletesOffLease` |
| **Sync**(s) | idle, something owed | takes what is owed, and converges as the consume does (a dirty path whose bytes ARE the document's, or absent in both; since 2026-10-17, the code-trace check's one mismatch), and records what it derived as a consume would (`derived := seq`, `skipped` := the dirty paths it left) — only when it took everything owed: a fetch or a write it could not complete records nothing, as the consume's `left` (L-128); a scoped sync in the code records nothing, so the next consume derives. With no document at all, a sync of a tree that holds paths refuses rather than read the absence as an empty document (L-127) | `Inv_NoRegress` | — |

## The invariants

Predicates on a state, and two on a step. Each world's list is in
`gen-leanp1.sh`; the expectations were written before any run
(`WORLDS-LeanP1.tsv`).

| invariant | says | violated without |
|---|---|---|
| **Inv_CitationsLive** | every citation names a live handle: `∀ p: doc[p] ≠ Nil ⇒ doc[p] ∈ live` | the commit re-reading its uploads (`LeanP1VerifyOff`), the sweep under the lease (`LeanP1SweepFree`), the sweep sparing a save in flight (`LeanP1SweepNoGrace`) |
| **Inv_OneName** | one handle is never cited under two names | a rename in one CAS (`LeanP1RenameTwoCAS`) |
| **AN5** (was `Inv_AckedNamed`) | an acknowledged version that is gone is accounted for along its chain — the path it was acked at, the path its handle was minted at, and every path it was acked at since (a rename): a citation, tombstone or acknowledged delete there of a version derived from it, a record preserving it, a tree that took it in there and no longer holds it, or a later acknowledged write; or an acknowledged strict descendant exists; or a conflict on the chain names a version derived from it. A preserving record's copy lives under `.flint/lean/conflicts/`; where a bucket lifecycle rule expires that prefix after N days, the claim holds for N days for a version that copy alone accounts for (SAFETY.md §2, §4.7). The original (one path, no descendant) is FALSE at three removals (E38, E39) and stays in `LeanP1.tla` as the record | the delete recording what it took (`LeanP1MA1Rem3Age`) |
| **Inv_OneHolder** | a writer in its commit section is the cell's holder | the lease |
| **NR5** (an action property; was `Inv_NoRegress`) | no step moves a writer's baseline back onto the document when the document is an ancestor of it with other bytes, unless a conflict or an acknowledged delete at that path, or an acknowledgement at another path, names a version derived from the baseline's (L-123's class). The original ghost form is FALSE at three removals (E29) and stays as the record | the gateway's delete acknowledging (`LeanP1MN2Rem3`) |
| **Inv_ShortcutSound** | an idle writer with no rescope in flight, whose pointer is where it left it and that nothing marked owed, is owed nothing | the merge marking what it saw untaken (`LeanP1OwedUnmarked`); a consume or a sync that left a path recording nothing as derived (`LeanP1ConsumeLeftRecorded`, `LeanP1SyncLeftRecorded`, L-128) |
| **Inv_ReaderFetches** | a reader that loaded the document less than G ago can fetch every handle it cites (one lagging longer re-resolves the pointer) | the retire age (`LeanP1ReaderLoses`) |
| **Prop_NoSilentRevert** | no lost update: a commit replaces a published version only if its tree held it at the path, the new version derives from it, a record preserves it, it moves to another path, or it was the human's own earlier acknowledged version there | R7 (`LeanP1NoR7`), the gateway judging a save against the version read (`LeanP1GatewayBlind`) |
| **Prop_DeleteSettles** | a barrier that published a delete leaves the tree and the document agreeing at that path | M3 (`LeanP1DeleteOutranked`: theirs outranking mine) |
| **Inv_ReaderSound** | a reader that skips a tick is owed nothing | the reader re-checking what its baseline says is owed (`LeanP1ReaderOwedIgnored`, L-129) |
| **Prop_AgentWorkKept** | bytes the agent wrote that nobody has PUT yet leave its tree only by the agent's own hand | the widen keeping a file the agent made (`LeanP1WidenOverwrites`, L-130), the unlink taking only the bytes it uncited (`LeanP1UnlinkBlind`, L-131) |
| **Prop_ScopeRespected** | no step but the agent's writes the tree at a path it neither holds nor admits | the consume's scope filter (`LeanP1ScopeIgnored`) |
| **Prop_NarrowNeverDeletes** | a narrow is an unwatch, never an absence: no barrier publishes the delete of a path a rescope unlinked that the agent has not touched since | the uncite before the unlink (`LeanP1NarrowUnlinkFirst`) |
| **AK1** (a step property) | an ack names every path the request declared, and the agent has not touched since, whose bytes in the ack's document are not the declared ones, unless a later version of them is there or a record surfaced them; `ok` names none. It may name more | the ack reading what the barrier withheld (`LeanP1AckIgnoresWithheld`), the carrier's uncited set (`LeanP1AckIgnoresUncited`), a per-fold carrier (`LeanP1CarrierPerFold`), a request read after the carry (`LeanP1RequestAfterCarry`), and the declaration rules (`LeanP1TouchDeclaresLeftover`, `LeanP1TouchDeclaresRenamed`) |
| **AK3** | every consumed request is pending or was answered, but for what the nonce cap dropped | a fold that replaces the standing record (`LeanP1FoldClobbers`) |
| **AK4** | an ack whose barrier installed names the clock its CAS stamped on the bucket | the clock stamped apart from the ack (`LeanP1StampIndependent`) |
| **NR1** (a step property) | a path the tree deleted and has not published comes back only by the agent's own write | a restart that rematerialises (`LeanP1Rematerialise`), a consume that takes the deleted path (`LeanP1ConsumeTakesDeleted`) |
| **DP1** (a step property) | an install is the lease holder's | the fence (`LeanP1InstallUnfenced`) |
| **DP2** (a step property) | the collector deletes only under the lease (the sweep and the reap are guarded in place) | the per-batch lease check (`LeanP1DeletesOffLease`, L-132) |

`Prop_UISaveCompletes` is the liveness claim: fair to the gateway's CAS
alone, every save the gateway starts is answered (it lands, or it is a
412), with the writers free to stop anywhere, the lease holder included
(G1). It is PROVED in TLAPS for every bound (`LeanP1ProofL1.tla`,
2026-10-14) and checked by TLC at a reduced bound (`LeanP1LiveHoldsSmall`);
`LeanP1LiveWaitsOnLeaseSmall` is its mutation (a gateway that waits for
the lease).

Every claim in this table is PROVED in TLAPS for every path count, writer
count and bound under the shipped rules
(`formal/results/2026-10-07-tlaps-m5/`, `formal/results/2026-10-14-tlaps-live/`;
AK1, AK3, AK4, NR1, DP1 and DP2 with the chain re-proved on their model in
`formal/results/2026-10-15-tlaps-rows15-17/`; `lean/SAFETY.md` §3.3), and
so is `TypeOK`: the theorem names are in `formal/README.md`.

## Honest limits

Scope and rescope are modelled (2026-10-02), at one rescope and one restart
over two paths (`LeanP1ScopeHolds`). A scoped SYNC (a request scope
narrower than the workspace's, D4) is not; it records nothing as derived,
so the next consume derives in full. The apply's keep set (a still-cited
dirty path stays cited) is not load-bearing for the agent's work under the
unlink's byte check: an uncited dirty file is kept and publishes as an
add, and R7 records what it lands over.

A failed fetch or write is modelled (2026-10-02): a consume, a sync or a
widen may fail on any path it would take, once per run (`MaxFetchFails`).
The path stays as it was and stays owed; the consume and the sync record
nothing as derived (`derived_etag` = none) and the consume does not move
the pointer it integrated, so the next consume derives again
(`LeanP1FetchHolds`, two writers on one path; `LeanP1ScopeFetchHolds`, with
scope and rescope). Recording a derive that left something is
`LeanP1ConsumeLeftRecorded` / `LeanP1SyncLeftRecorded` (L-128's class); both
break `Inv_ShortcutSound`.

A reader is modelled (2026-10-02): a tree in `Readers` never publishes, and
its tick is `RPullRead` then `RPullSync` (`LeanP1ReaderHolds`, a writer and a
reader on one path with a failed fetch; `LeanP1ReaderScopeHolds`, two paths,
scoped or not). The request cell, the reader's second etag, is not.

A widen's failed fetch leaves the path unheld but covered, so the next
consume owes it; the code also records it (`rescope-refused`).

The retire log is written after the CAS. A crash between the two leaves
those handles to the orphan sweep's write-age rule; the model does not
have that window. The gateway's lease-held `cas_manifest` verb writes no
retire log either.

`took` — per path, every version a tree has taken in there — is the trees'
memory, and the shipped baseline keeps only the CURRENT one, so the
AN5 disjunct that reads it reads more than the workspace on
disk holds. Nothing asks the code to compute it; the model keeps it as
state a step writes, so the claim stays a predicate on a state.

## What this page does not cover

The ingress namespace, the gateway's lost reply (a commit that landed
behind a lost response is answered committed, MN2; tests only), and the
gateway's read path (the manifest cache and the CRC
check, M8). The last is pinned by tests only
(`a_warm_read_fetches_the_pointer_and_the_object_and_no_manifest_entries`,
`a_read_whose_bytes_do_not_match_the_citation_is_refused`).
