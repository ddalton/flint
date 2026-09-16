------------------------------ MODULE LeanSubtree ------------------------------
(***************************************************************************)
(* The lean (checkout/publish) subtree protocol, modelled BEFORE the code  *)
(* exists — the FlintExtents posture.  Spec of record:                     *)
(* docs/plans/flint-lean-plan.md v2 (f0776e0), §2.1 barrier + §2.2         *)
(* inbox/window/takeover.  This module is deliberately SEPARATE from the   *)
(* flint corpus in formal/: lean is a separate system that reuses tier::   *)
(* as a library, and its protocol lives in the bucket, not in the hub.     *)
(*                                                                         *)
(* One subtree.  Two syncer incarnations (A, then B after takeover), a    *)
(* gateway abstracted to its bucket effects (HITL writes, window check,    *)
(* per-request epoch validation), and the bucket substrate: lease cell,    *)
(* manifest (seq + per-path citation), whole-file objects (per-path        *)
(* generation = ETag), the inbox/window cell.                              *)
(*                                                                         *)
(* Generations model ETags: objects[p] = g means the object at p currently *)
(* has content/ETag g; 0 = absent.  If-Match is modelled as an equality    *)
(* test against the writer's expected generation.  Whole-PUT atomicity is  *)
(* assumed (real S3 gives it for PutObject).                               *)
(*                                                                         *)
(* Deliberate abstractions (tranche 1 — each is a README entry, not a      *)
(* claim of coverage):                                                     *)
(*   - The scan is ATOMIC, so the rename-vs-walk race and the             *)
(*     two-consecutive-scans deletion rule are UNREPRESENTABLE here.       *)
(*   - The 6-quiet-poll takeover observation is abstracted into a single   *)
(*     ClaimB action enabled when A is stalled or dead.  The poll protocol *)
(*     itself is machine-checked in flint's FlintTierEpoch.tla; lean       *)
(*     re-scopes it and tranche 2 may refine it here.                      *)
(*   - Checkout reads the manifest's citations without modelling           *)
(*     hydrate's 412/S3-wins arm.                                          *)
(*   - The sync verb, multi-subtree layout, partial checkout, preStop      *)
(*     timing, and all perf axes are out of scope (tranche 2+ / Phase 0b). *)
(*   - conflicts is a set of (path, generation) RECORDS.  The             *)
(*     implementation obligation this hides: a conflict record must        *)
(*     preserve the BYTES (conflict-suffixed key or versioning), not just  *)
(*     the reference — otherwise "both versions recoverable" is false.     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  Paths,          \* model values, e.g. {p1, p2}
  MaxGen,         \* generation mint budget (Init uses 1; mints start at 2)
  MaxSeq,         \* manifest CAS budget
  MaxHitl,        \* HITL write budget
  MaxBarriers,    \* barrier (scan) budget
  MaxCrashes,     \* pod hard-kill budget
  MaxRestarts,    \* container-restart budget
  MaxSyncs,       \* sync-verb budget
  AllowStall,     \* enable the stall/thaw arm (the straggler world)
  \* ---- protocol arms: TRUE = plan v2; FALSE = the refuted design -------
  InboxEnabled,        \* FALSE: gateway direct-manifest-bump (the v1 reading)
  MergeCapable,        \* FALSE: whole-rewrite writer (412 -> re-seed -> overwrite)
  ConflictSurfacing,   \* FALSE: LOCAL-WINS silently (the flush.rs:1391 reuse)
  WindowCheck,         \* FALSE: gateway admits HITL during an open window
  Rotation,            \* successor CAS-rotates manifest seq before serving
  EpochCheck,          \* per-request epoch validation on syncer writes
  GuardedGC,           \* HEAD etag-guard before the GC delete
  DeletesAfterCAS,     \* FALSE: v1 order upload -> delete -> CAS
  RematerializeOnRestart, \* TRUE = mutation: re-checkout over a live tree
  \* ---- tranche 2: the sync verb x barrier product ----------------------
  SyncEnabled,         \* FALSE in every tranche-1 cfg (spaces preserved)
  SyncScanFirst,       \* FALSE: sync judges dirt from the LAST BARRIER's
                       \* snapshot instead of its own scan (the refuted
                       \* design; the review's steady-state destruction)
  \* ---- tranche 3 product 4: the SCOPED sync verb (boundary-verbs D4) ----
  SyncScope,           \* TRUE: the sentinel's scoped form — remote changes
                       \* apply only to in-scope paths.  FALSE in every
                       \* tranche-1/2 cfg, so those state spaces are
                       \* preserved by construction.
  ScopedInstBase,      \* TRUE  = D4: a scoped sync advances the MERGE BASE
                       \* only for paths it applied or verified in scope.
                       \* FALSE = the mutation: it advances the whole
                       \* instBase to bucket-current, so every out-of-scope
                       \* foreign entry reads as already-integrated at the
                       \* next merge and is lost from the inbox flow
                       \* FOREVER.  `instBase` is the object the model has
                       \* refuted naive designs on twice; D4 rewrites its
                       \* per-path semantics, which is why it is modelled.
  \* ---- tranche 3 product 2: gated citation x version GC x backstop -----
  \* (boundary-verbs D6/D7/D8/D13.)  Generations are unique mints, so a
  \* generation IS a version id and `manifest[p]` already cites one; the
  \* substrate this product adds is `versions[p]` — the generations that
  \* still EXIST as stored versions, which on a versioned bucket is a
  \* different question from what the object currently reads as.
  \*
  \* That distinction is the whole tranche.  `Inv_NoDangling` asks
  \* "does the object exist" and was the right question until D7: gated
  \* staging makes the CITED version noncurrent, so an object can exist,
  \* read as newer uncited bytes, and have nothing behind its citation.
  GatedCitation,       \* FALSE in every pre-existing cfg: the gated
                       \* actions are disabled and `versions`/`stage`/
                       \* `withheldDel` stay frozen at Init, so every
                       \* earlier state space is preserved by construction.
  AtomicCitation,      \* TRUE = D6: ONE CAS installs the entire pending
                       \* set.  FALSE = the mutation: the install is split
                       \* across two CASes, so a reader can see half a
                       \* logical change.
  GCKeepsCurrent,      \* TRUE = the reaper never reclaims the CURRENT
                       \* version.  FALSE = the shipped rule THIS MODEL
                       \* REFUTED: "delete every version of a touched key
                       \* except the one the installed manifest cites"
                       \* deletes a foreign write that landed between the
                       \* lane and the citation — and it was current,
                       \* acked, and about to be read.
  CiteDropsInflightHitl, \* TRUE = a staged path with a LIVE INBOX ENTRY is
                       \* dropped from the boundary rather than cited
                       \* over.  The base-version check cannot see that
                       \* write (it reads the baseline, and the citation
                       \* lane consumes nothing) and the lane opens no
                       \* window, so the inbox is the only witness — and
                       \* the window CAS has already loaded it.
  \* ---- tranche 3 product 1: the boundary VERB x barrier x inbox --------
  \* (boundary-verbs D1/D2/D3/D12.)  The sentinel is a FILE in the tree,
  \* not a bucket object: the agent touches `.flint/publish`, the syncer
  \* renames it into its own state dir (the consume), honors it with a
  \* real barrier, writes `.flint/publish.ack`, and retires the pending
  \* record.  What this product searches is the interleaving of those
  \* four steps with the barrier, the inbox, restarts and DEPOSAL.
  SentinelEnabled,     \* FALSE in every pre-existing cfg: every sentinel
                       \* action is disabled and the new sc fields stay
                       \* frozen at their (empty) Init values, so earlier
                       \* state spaces are preserved by construction.
  MaxTouches,          \* agent touch budget (a touch id doubles as the
                       \* nonce AND as the sentinel's mtime clock).
  FoldPending,         \* TRUE = D2.1: a consume never overwrites the
                       \* standing pending record, it FOLDS into it, so
                       \* every coalesced nonce is still named by the ack
                       \* that eventually lands.  FALSE = the mutation:
                       \* the rename clobbers the old record and orphans
                       \* its nonces forever.
  AckFromInstall,      \* TRUE = the uniform crash rule: an ok ack is only
                       \* ever written from a barrier that ran STRICTLY
                       \* AFTER the consume.  FALSE = the mutation the
                       \* review retracted from the draft's crash matrix:
                       \* ack from persisted state, which is the same
                       \* observable state for crash-before-CAS as for
                       \* crash-after-step-7 and asserts publication of
                       \* writes that never uploaded.
  RefuseOnFence,       \* TRUE = D2: a deposed incarnation answers every
                       \* owed sentinel with `refused-fenced` and never
                       \* with ok — on the honor path AND on the D12
                       \* heartbeat arm, which is decoupled from publish
                       \* cadence and so usually discovers deposal first.
                       \* FALSE = the mutation: success-ack-after-fence.
  FastPathGuards,      \* TRUE = the shipped skip-on-no-diff fast path
                       \* with all of its guards: nothing local, no
                       \* citation repair owed, and the remote manifest
                       \* where we left it.  FALSE = the mutation that
                       \* drops the last two.  This arm is modelled
                       \* because §10.1 records a DELIBERATE deviation
                       \* from §2.1 here — the shipped honor path lets a
                       \* no-diff sentinel take the fast path, on the
                       \* strength of a prose argument.  This is that
                       \* argument, machine-checked.
  GatedRepair,         \* TRUE = the citation lane carries the same
                       \* citation-repair the fused barrier has: re-cite
                       \* an object this syncer INTEGRATED (a consumed
                       \* HITL write) whose citation is still behind it.
                       \* FALSE = what shipped, where the repair lived
                       \* only in the fused barrier — which gated mode
                       \* structurally never runs — so an acked HITL
                       \* write stayed cited at its predecessor forever
                       \* (§2.4.2's exemption and D13's "within one
                       \* floor", both prose-only until C2).
  LaneCancelsStaged,   \* TRUE = a withheld delete cancels the version the
                       \* stage still holds for that path (and vice
                       \* versa, in the lane).  FALSE = the shipped lane
                       \* before C3's second half: the stage and the
                       \* tombstone set both reach the citation and
                       \* merge order decides, which cites a file the
                       \* agent deleted.  FALSE in every pre-existing
                       \* cfg — including `LeanGatedInflightHitl`, whose
                       \* counterexample runs through exactly the shape
                       \* this arm closes (see the ledger note there).
  AckHonest,           \* TRUE = a boundary that does not carry a path the
                       \* agent declared is answered with a partial ack
                       \* naming it, never with `ok`: a gated citation
                       \* that DROPPED the path, or (2026-09-14) a fused
                       \* install whose merge kept another writer's
                       \* entry over the agent's delete.  FALSE = the
                       \* mutation, which is what shipped until then:
                       \* `status: "ok"` whenever nothing was parked.
                       \* FALSE in every pre-existing cfg: citeDropped
                       \* stays {} there, so their spaces are preserved.
  MineIsNotForeign,    \* TRUE = an entry that matches OUR OWN BASELINE is
                       \* not a foreign change, whatever the merge base
                       \* says.  The merge base is rewritten at step 7,
                       \* so a barrier that crashed between its manifest
                       \* CAS and that rewrite leaves the workspace's own
                       \* installed entry looking like somebody else's —
                       \* and delete/modify then resolves conservatively
                       \* AGAINST the agent's own delete.  FALSE = the
                       \* shipped rule THIS MODEL REFUTED.
  BackstopEnabled,     \* TRUE = the noncurrent-retention lifecycle rule
                       \* fires.  It is a BACKSTOP, never the reaper, and
                       \* enabling it is a mutation: on `files/` it cannot
                       \* tell cited from uncited, so it runs a clock
                       \* against live cited data (D8's inversion, and
                       \* §2.4.3's abandoned-mid-stage endgame).
  TwoScanDelete,       \* TRUE = a path is delete-eligible only if it was
                       \* ALSO ABSENT at the PREVIOUS scan — the shipped
                       \* two-consecutive-scans rule (`scan.rs` classify:
                       \* absent now and in `prev_scan` is a FIRST
                       \* absence, withheld).  Until 2026-09-12 this read
                       \* `p \in prevScan` — present at the previous scan
                       \* — which is a one-scan rule with a spurious
                       \* conjunct; the narrow runs did not notice (their
                       \* comment records that prevScan changed nothing
                       \* there), and the declared-removal mutation could
                       \* not be expressed against it.
                       \* FALSE in every PRE-EXISTING cfg, deliberately:
                       \* this tranche is the first to model `prev_scan`
                       \* at all, and turning it on globally would shrink
                       \* every earlier run's delete space and let a
                       \* pinned mutation stop finding its counterexample.
  MaxNarrows,          \* bound on Narrow actions (0 disables the verb).
  NarrowAtomic,        \* TRUE = the SHIPPED rule: a narrow removes the
                       \* path from the baseline, from `prevScan` and
                       \* from the tree in ONE step.  FALSE selects a
                       \* naive arm via NarrowUnlinkFirst.
  NarrowUnlinkFirst,   \* only under ~NarrowAtomic.  TRUE = unlink and
                       \* leave the citation (classify reads a DELETE);
                       \* FALSE = uncite and leave the file (classify
                       \* reads an UPLOAD).  Both are the naive orders
                       \* §4.2 says are each destructive on their own.
  StampBoundarySource, \* TRUE = every install stamps the manifest with
                       \* the SAME clock its ack will name.  FALSE = the
                       \* mutation, and it is what shipped: the barrier
                       \* installed through an UNSTAMPED CAS, so every
                       \* boundary read as the default clock no matter
                       \* what drove it — and a drain that renamed its own
                       \* ack left the bucket and the agent naming two
                       \* different clocks for one boundary.  The ack is a
                       \* LOCAL file; the manifest is what the fleet
                       \* reads, so the bucket held the wrong one.
  \* ---- tranche 5: DECLARED removals (delete/rename design §3-§6) --------
  MaxRemovals,         \* budget for HitlRemove/HitlRename; 0 in every
                       \* pre-existing cfg, so `removals` stays {} and
                       \* those state spaces are preserved by construction.
  DeclaredSkipsWalk,   \* TRUE = §4: a declared removal goes STRAIGHT into
                       \* the delete set at the scan that follows its
                       \* consume.  FALSE = the mutation: the consume only
                       \* unlinks and the walk infers the absence — under
                       \* the two-scan rule that is one barrier late, and a
                       \* rename's two halves land in two generations.
  FreePaths,           \* the paths that start UNPUBLISHED (a rename needs
                       \* a free destination; every pre-existing cfg has
                       \* {} and starts every path at gen 1 as before).
  RenameWaitsForDestination, \* TRUE = a rename's removal is performed only
                       \* once its destination is INTEGRATED AND CLEAN.
                       \* FALSE = the mutation: the source goes as soon as
                       \* it is clean, whatever became of the destination.
  EarlyInboxDrop,      \* TRUE = the rule that SHIPPED until 2026-09-12: a
                       \* consume clears the inbox at once ("durably in
                       \* the baseline").  FALSE = consumed entries leave
                       \* the cell at Finish / CiteFinish, after the
                       \* manifest cites them.  TRUE is the mutation: a
                       \* pod REPLACEMENT between the two takes the
                       \* baseline with the emptyDir, and an acked write
                       \* is then tracked by nothing.
  \* ---- tranche 6: the PER-BARRIER lease (writer-lease design §4-§5) ----
  \* The cell is held for ONE barrier's commit section — claim after the
  \* uploads, release after the baseline — instead of for the pod's life,
  \* and it carries a FIFO ticket.  Both syncers run from the start; the
  \* takeover (`ClaimB`) is the life-lease world's and does not exist
  \* here: a dead holder mid-commit is deposed by the generic `Claim`.
  BarrierLease,        \* FALSE in every pre-existing cfg: `StartA`/`ClaimB`
                       \* keep the life lease, `Scan` opens the window,
                       \* every fence kills, and the three new cell
                       \* variables stay frozen at Init — those state
                       \* spaces are preserved by construction.
  Ticket,              \* TRUE = release names the queue head as the
                       \* HANDOFF and only the handoff may acquire a
                       \* released cell.  FALSE = the mutation (falsifier
                       \* L5): release names nobody and any syncer may
                       \* acquire — random arbitration, and under
                       \* fairness one writer claims forever.
  DeadHandoffSkip,     \* TRUE = a released cell whose handoff is quiet
                       \* (dead or stalled, the 20 s rule) may be acquired
                       \* by anyone and the handoff is dropped.  FALSE =
                       \* the mutation: a crashed waiter wedges the cell
                       \* for every survivor, forever.
  InfiniteBarriers,    \* TRUE = the LIVENESS abstraction, and nothing
                       \* else: the barrier budget, the manifest seq and
                       \* the epoch SATURATE instead of stopping the world
                       \* (every counter a no-change barrier moves is
                       \* monotone, so with budgets the state graph has no
                       \* cycle and TLC cannot exhibit "claims forever").
                       \* Only meaningful under FairSpec; every safety run
                       \* keeps FALSE.
  ConditionalGC,       \* TRUE = the GC delete carries If-Match on the
                       \* recognised etag: guard and delete are ONE
                       \* request, which is what this module's atomic
                       \* `GCDelete` has always modelled.  FALSE = what
                       \* SHIPS (`barrier.rs` step 6): a HEAD, then an
                       \* unconditional DELETE.  Under the life lease the
                       \* lease covered that window; under the barrier
                       \* lease the other writer's uploads hold no lease,
                       \* and a supersede landing between the two dangles
                       \* its citation.  THE MODEL FOUND THIS the moment
                       \* the step was made two steps — see
                       \* LeanBarrierLeaseGCUnconditional.  Modelled only
                       \* under BarrierLease (TRUE everywhere else, so
                       \* every earlier state space is preserved).
  VerifyAdoptedCitations,\* TRUE = an entry this barrier ADOPTED (the 412
                       \* arm found its own bytes already there and cited
                       \* the existing object) is re-verified INSIDE the
                       \* commit section and withheld if the object is
                       \* gone.  FALSE = what SHIPS: the adopt cites blind.
                       \* Under the life lease nothing else could GC the
                       \* object; under the barrier lease the other
                       \* writer's commit can uncite it and its GC — HEAD-
                       \* guarded on an etag it learned at checkout —
                       \* deletes it between the adopt and the adopter's
                       \* CAS.  A same-bytes re-PUT would not help: a real
                       \* etag is the content hash.  The commit section is
                       \* the one race-free place to look, because GCs run
                       \* only under the lease.  THE MODEL FOUND THIS at
                       \* depth 28 of the first two-writer stall run.
                       \* Only under BarrierLease; TRUE elsewhere.
  HitlOverwritesTrackedOnly, \* TRUE = a HITL write (the gateway) overwrites
                       \* only a version the workspace TRACKS: the object at
                       \* the key is the manifest's citation or an inbox
                       \* entry (or absent).  FALSE = what shipped until
                       \* 2026-09-13: any current object, including another
                       \* writer's upload its commit has not cited — which,
                       \* under the barrier lease (no window during
                       \* uploads), loses the acked HITL write
                       \* (LeanBarrierLeaseHitlOverUncited).  FALSE in every
                       \* pre-existing cfg, so earlier state spaces are
                       \* preserved by construction.
  SyncKeepsHiddenBase, \* TRUE = a sync does NOT advance the merge base for
                       \* a path whose remote truth came from an inbox
                       \* overlay that differs from the manifest: the
                       \* manifest's version was hidden from it, never
                       \* applied or verified (`sync.rs` step 5, built
                       \* 2026-09-13 for LeanBarrierLeaseSyncOverlayStale).
                       \* FALSE in every pre-existing cfg — the shipped
                       \* advance — so earlier state spaces are preserved
                       \* by construction.
  MaxSameBytes,        \* budget for `AgentWriteSame`: the agent writes a
                       \* file with bytes some version it RECOGNISES
                       \* already has.  A generation here is a unique
                       \* mint; a real S3 etag is the MD5 of the bytes,
                       \* so two PUTs of identical bytes carry ONE etag
                       \* and an etag-guarded GC cannot tell them apart.
                       \* The module modelled every write as a fresh mint
                       \* until the live drill found what that hid
                       \* (finding 13, runcv A3).  0 in every
                       \* pre-existing cfg, so the action is unreachable
                       \* and `touched` stays {} — those state spaces are
                       \* preserved by construction.
  VerifyUploadedCitations,\* TRUE = CASInstall re-verifies EVERY citation
                       \* its own uploads add, not only the adopted ones,
                       \* and withholds what is gone (`barrier.rs`, the
                       \* commit section's re-read, 79e7dac9).  FALSE =
                       \* the rule before it: an upload that LANDED is
                       \* cited without a look, which is sound only while
                       \* no other writer's GC can recognise its etag.
                       \* BarrierLease only.  FALSE in every pre-existing
                       \* cfg: with unique mints the other writer's GC
                       \* never recognises an uncited upload, but a HITL
                       \* write over one does move it, and
                       \* LeanBarrierLeaseHitlOverUncited must keep
                       \* finding its counterexample.
  \* ---- tranche 7: MODEL THE IMPLEMENTATION (2026-09-15) -----------------
  \* Three things the code has done since v1.52.0 that this module did
  \* not.  Each was a named gap in the CHANGELOG, and each changes which
  \* interleavings exist, so a green run over the old shape was a claim
  \* about a design the code no longer has.
  WriterQueue,         \* TRUE = what ships: the other writers' changes a
                       \* merge carries into the manifest go to THIS
                       \* writer's LOCAL queue (`state::queue_foreign`,
                       \* keyed by path, a later change replacing the
                       \* queued one), deletions included, and the next
                       \* consume drains it BEFORE the shared inbox's
                       \* entries and applies its deletions AFTER them
                       \* (`barrier.rs` `consume_counted`).  The queue is
                       \* a file in the state directory: a restart keeps
                       \* it, a pod replacement takes it.  FALSE = this
                       \* module's shape until now: foreign upserts
                       \* re-queued into the SHARED inbox, foreign
                       \* deletions never reaching the other tree at all.
                       \* BarrierLease only; FALSE in every pre-existing
                       \* cfg, where `fq` stays {}.
  EmptyInstall,        \* TRUE = what ships: a barrier that adds nothing
                       \* installs nothing.  Three routes (`barrier.rs`):
                       \* the skip-on-no-diff fast path on EVERY barrier
                       \* (not only a sentinel honor); the PULL-ONLY
                       \* boundary — nothing uploaded, deleted, consumed,
                       \* removed or re-cited — which queues theirs and
                       \* takes it as the merge base with no claim, no
                       \* window and no CAS; and a commit whose merge
                       \* equals theirs, which skips the CAS and keeps
                       \* the seq.  FALSE = every install advanced the
                       \* seq, the CHANGELOG's "no empty-install rule".
                       \* Requires WriterQueue (the pull-only route
                       \* queues locally; it has no window to write the
                       \* inbox under).
  TombstoneHeadsKey,   \* TRUE = the fix for what modelling the queue found
                       \* (2026-09-15): a queued DELETION applies only
                       \* while the key is still absent — a key holding an
                       \* object means a newer write superseded it, which
                       \* is the rule the queue's UPSERTS already follow
                       \* ("superseded").  FALSE = what shipped: the
                       \* deletion runs after the consume's entries, over
                       \* whatever they just adopted, so a UI write that
                       \* re-created a path a peer deleted is removed from
                       \* the tree the same consume adopted it into, and
                       \* the window clear then drops its inbox entry —
                       \* acked, and tracked by nothing
                       \* (LeanBarrierLeaseQueueTombstoneOverHitl).
                       \* WriterQueue only.
  \* Three more, found by TRACE VALIDATION (lean/formal/trace/): the first
  \* syncer traces checked against this module were rejected at exactly
  \* these steps.  Each is what the code does; FALSE keeps every earlier
  \* state space.
  CommitLoadsCurrent,  \* TRUE = the commit merges onto the manifest it
                       \* LOADS after the claim and CASes against that
                       \* (`barrier.rs` commit section), so a writer whose
                       \* last seq is stale does not first lose a CAS.
                       \* FALSE = CASInstall If-Matches the seq remembered
                       \* from the writer's last boundary (CASMiss first).
  Upload412Preserves,  \* TRUE = `upload_one`'s 412 policy since review
                       \* 2026-09-12 (inbox-1): a FOREIGN version at the
                       \* key is preserved as a conflict copy and then
                       \* superseded knowingly; the path parks only when
                       \* that races (a second 412) or the preserve fails.
                       \* FALSE = the park this module always modelled.
  DeclaredConfirmsAbsence, \* TRUE = a DECLARED barrier (a sentinel honor)
                       \* confirms a first absence with a fresh stat and
                       \* deletes it at once (`confirm_absences`); and the
                       \* skip-on-no-diff fast path refuses a pending first
                       \* absence and still advances the two-scan clock.
                       \* FALSE = the two-scan rule on every barrier and a
                       \* fast path blind to first absences.
  OrphanTrack,         \* FINDING 10 (open): a writer lost for good between
                       \* its upload and its commit leaves bytes at a
                       \* CITED key that no manifest cites and nothing
                       \* tracks.  TRUE = the candidate fix: a writer that
                       \* finds such an object appends it to the shared
                       \* inbox, as a UI write is tracked, and the normal
                       \* consume integrates it.  The action is allowed
                       \* WHENEVER the object is untracked — the grace that
                       \* keeps a live writer's in-flight upload out of it
                       \* is not modelled, so a green run means the grace
                       \* is not what keeps anything safe.  FALSE = what
                       \* ships.  BarrierLease only.
  Writers,             \* tranche 7: the writers, IN START ORDER, as a
                       \* sequence of names.  <<"A", "B">> in every cfg
                       \* before the third-writer worlds, and the life
                       \* lease's actions (StartA, ClaimB, CheckoutB) and
                       \* the stall (A only) still name those two.
  BaselineKeepsUncollected, \* TRUE = what SHIPS, found by replaying the
                       \* storm (W4 phase 2): step 7 drops a path from the
                       \* baseline only for the deletes whose OBJECT the GC
                       \* actually collected (`report.deleted`).  A delete
                       \* whose GC SKIPPED — the key held bytes this writer
                       \* does not recognize — keeps its baseline entry, so
                       \* the tree reads as locally deleted at the next
                       \* consume and an incoming version is PRESERVED
                       \* rather than adopted.  FALSE = the model as it was:
                       \* every published delete clears the baseline.
  AbandonOnStoreError, \* TRUE = the code's other way out of a commit
                       \* section, which the 2026-09-15 storm traces show
                       \* and the model had no step for: the store REFUSES
                       \* a request the commit makes (S3 answered the
                       \* window-open PUT with 409 ConditionalRequestConflict),
                       \* the barrier returns the error, releases the cell
                       \* and keeps its pending sentinel, and the next
                       \* barrier redoes the work.  Nothing is installed and
                       \* nothing is acked.  FALSE = the model as it was: a
                       \* claimed barrier only ever installs, misses the CAS
                       \* or is fenced.  BarrierLease only.
  HandoffAtClaim,      \* TRUE = what SHIPS, as the 2026-09-15 storm traces
                       \* show it: `epoch_handoff` hands the cell to the head
                       \* of the waiter list THE HOLDER READ AT ITS CLAIM
                       \* (`lease.waiters`), and writes the rest of that same
                       \* stale list back as the queue — so a writer that took
                       \* a ticket after the claim is neither handed the cell
                       \* nor kept in the queue.  FALSE = the FIFO the model
                       \* assumed: the head of the queue as it stands at the
                       \* release.  BarrierLease + Ticket only.
  ProjectedTrace,      \* Trace validation only (W4 phase 2).  TRUE = this
                       \* run replays ONE PATH of a live leg, so a barrier's
                       \* reason to claim may lie in a path the projection
                       \* dropped: `WantsCell` then holds on a scanned
                       \* barrier whatever the projected paths show.  It
                       \* relaxes NOTHING else, and no gate run sets it.
  QueueForeignChanges  \* TRUE = what ships.  FALSE = the mutation: the
                       \* merge base moves past the other writers'
                       \* changes and nothing queues them, so the tree
                       \* never receives them — the direct fingerprint of
                       \* the harm `Inv_AckBoundaryCoherent`'s queue
                       \* exemption must NOT excuse (the known-bad run for
                       \* that relaxation).  WriterQueue only.

\* A cfg cannot write a sequence literal; it substitutes one of these
\* (`Writers <- TwoWriters`).
TwoWriters   == <<"A", "B">>
ThreeWriters == <<"A", "B", "C">>
\* Trace validation only (W4 phase 2): a storm leg runs six writers, and a
\* replay is driven step by step, so the state space is the trace's length
\* rather than the world's.  No GATE run uses these.
FourWriters  == <<"A", "B", "C", "D">>
FiveWriters  == <<"A", "B", "C", "D", "E">>
SixWriters   == <<"A", "B", "C", "D", "E", "F">>
Syncers == {Writers[i] : i \in DOMAIN Writers}
\* The first writer, the one the stall and the life lease name.
ASSUME Len(Writers) >= 2 /\ Writers[1] = "A" /\ Writers[2] = "B"
Sources == {"none", "cadence", "sentinel"}

VARIABLES
  \* ---- bucket -----------------------------------------------------------
  cellEpoch,   \* subtree lease cell: current epoch (0 = never claimed)
  cellHolder,  \* "A" | "B" | "none"
  cellQueue,   \* tranche 6: the FIFO ticket — a sequence of waiters
  cellSeen,    \* the queue AS THE HOLDER READ IT at its claim (the code's
               \* `lease.waiters`).  Always <<>> under ~HandoffAtClaim, so
               \* that world's state space is the one every earlier run
               \* explored.
  cellHandoff, \* tranche 6: "none" | the syncer a release named
  cellReleased,\* tranche 6: TRUE between a release and the next claim.
               \* HELD = cellEpoch > 0 /\ ~cellReleased; FRESH = epoch 0.
  manSeq,      \* manifest document seq (the CAS token)
  manSrc,      \* the boundary-source stamp on the INSTALLED manifest —
               \* the fleet-visible answer to "which clock installed
               \* this?".  Real bucket state (object metadata), not a
               \* ghost: an operator reads it without the agent's ack.
  manifest,    \* [Paths -> Nat]: cited generation per path (0 = uncited)
  objects,     \* [Paths -> Nat]: current object generation (0 = absent)
  inbox,       \* SUBSET (Paths \X Nat): pending HITL entries
  removals,    \* SUBSET Paths: DECLARED removals pending in the cell
               \* (delete/rename design §3) — recorded from outside the
               \* pod, performed by the syncer at its next consume.
  window,      \* 0 = closed; else the opener's epoch
  \* ---- syncers ---------------------------------------------------------
  sc,          \* [Syncers -> record], fields below
  \* ---- the versioned substrate (tranche 3 product 2) --------------------
  versions,    \* [Paths -> SUBSET Nat]: every generation still STORED for
               \* the path.  `objects[p]` is which one it currently reads
               \* as; a PUT over a versioned bucket destroys nothing, so
               \* the two diverge exactly while work is staged-uncited.
  stage,       \* [Syncers -> [Paths -> Nat]]: the gated pending set —
               \* staged-but-uncited generation per path (0 = none).
  stageBase,   \* [Syncers -> [Paths -> Nat]]: the generation the BASELINE
               \* cited when we staged.  D7's re-validation guard: if the
               \* baseline has moved by citation time, a HITL consume or a
               \* sync landed after we staged, and installing our staged
               \* generation would let work that PREDATES the foreign
               \* bytes win against them.
  withheldDel, \* [Syncers -> SUBSET Paths]: deletes withheld from the
               \* manifest until a citation, so a rename never becomes
               \* reader-visible as gone/absent at an undeclared point.
  \* ---- environment / ghosts --------------------------------------------
  hitlAcked,   \* SUBSET (Paths \X Nat): writes acked to the user
  conflicts,   \* SUBSET (Paths \X Nat): surfaced conflict records
  gh           \* ghost/counter record, fields below

gatedVars == <<stage, stageBase, withheldDel>>
leaseVars == <<cellQueue, cellHandoff, cellReleased, cellSeen>>

vars == <<cellEpoch, cellHolder, cellQueue, cellHandoff, cellReleased, cellSeen,
          manSeq, manSrc, manifest, objects, inbox, removals, window,
          sc, versions, stage, stageBase, withheldDel, hitlAcked,
          conflicts, gh>>

(* sc[s] fields:
     st       "unstarted" | "claiming" | "running" | "stalled" | "dead"
     pc       "idle" | "consumed" | "scanned" | "delDone" | "cased"
              | "waiting" | "claimed"   (tranche 6: the barrier lease —
              uploads done and queued for the cell; cell claimed, the
              commit section from here to Finish)
     epoch    the epoch this incarnation believes it holds (tranche 6:
              the LAST epoch it held; 0 before its first claim)
     expSeq   the manifest seq it will If-Match
     local    [Paths -> Nat]  the live tree (0 = absent)
     baseline [Paths -> Nat]  the persisted baseline snapshot
     known    SUBSET Nat      generations this incarnation recognizes:
                              checkout + own mints + surfaced consumes.
                              Blind adoption (the LOCAL-WINS mutation) does
                              NOT extend it — that is what makes silent
                              destruction attributable.
     instBase [Paths -> Nat]  the manifest view at this incarnation's last
                              install (or checkout): the MERGE BASE.  It is
                              deliberately distinct from baseline — consume
                              advances baseline (the object-ETag guard) but
                              not instBase, so a consumed adoption is not
                              mistaken for a foreign manifest entry.
     instSnap [Paths -> Nat]  the document THIS barrier installed, snapshot
                              at CASInstall.  Finish MUST advance from this
                              snapshot, never from the live manifest — a
                              HITL bump landing between install and finish
                              would otherwise be absorbed into the merge
                              base and clobbered next barrier (the
                              implementation threads `installed` through
                              for exactly this reason).
     instSeq  Nat             the seq THIS barrier installed.
     instSrc  Sources         tranche 6: the clock stamped on THIS
                              barrier's install (BarrierLease only).  With
                              two writers the live manifest's stamp is
                              whoever installed LAST, which is not what
                              this workspace's ack names.
     scanU    SUBSET Paths    upload set frozen at scan
     scanD    SUBSET Paths    delete-eligible set frozen at scan
     scanGen  [Paths -> Nat]  local generations frozen at scan (re-stat guard:
                              the barrier publishes walk-time content; post-
                              scan agent edits are next barrier's dirt)
     upDone   SUBSET Paths    uploaded (or adopted-own) this barrier
     adopted  SUBSET Paths    tranche 6: the subset of upDone that was
                              ADOPTED — cited without a PUT — and so is
                              re-verified under the lease (BarrierLease
                              only; {} otherwise)
     touched  SUBSET Paths    paths the agent REWROTE with the bytes their
                              baseline already cites (`AgentWriteSame`): the
                              walk sees a new stat, so the path is dirty,
                              while the generation — the etag — is the
                              baseline's.  Leaves at Finish once an upload
                              of it is cited; a withheld one stays dirty.
                              {} unless MaxSameBytes > 0
     repairMoved SUBSET Paths the last install's DECLINED citation repairs:
                              paths this writer integrated whose key held a
                              newer generation at the CAS (BarrierLease
                              only; {} otherwise) — see BoundaryIncoherent
     fq       SUBSET (Paths \X Gens)  tranche 7, WriterQueue only: the
                              writer-LOCAL foreign queue — <<p, g>> is a
                              change another writer made that this writer's
                              merge carried into the manifest but not yet
                              into the tree; g = 0 is a DELETION.  At most
                              one entry per path.  {} otherwise
     noInst   BOOLEAN         tranche 7, EmptyInstall only: this barrier's
                              commit found the merge equal to theirs and
                              installed nothing (CASInstall to Finish)
     parked   SUBSET Paths    412-parked this barrier
     gcDone   SUBSET Paths    delete-set entries processed this barrier
     gcHeaded SUBSET Paths    tranche 6, ~ConditionalGC only: delete-set
                              entries whose HEAD has been read this barrier
     gcSeen   [Paths -> Nat]  ...and the generation that HEAD saw — what
                              the unconditional DELETE is guarded on,
                              which is not what it deletes
     citeDone SUBSET Paths    which staged paths THIS citation has already
                              installed.  Non-empty and not the whole
                              valid pending set = a reader can see half a
                              logical change; the single-CAS design never
                              produces that state, which is exactly why
                              the split-install mutation stays.
     stageCarried BOOLEAN     this incarnation's pending set survived a
                              lane pass (set at Scan when the stage is
                              already non-empty).  Only under gated; FALSE
                              otherwise, so earlier state spaces are
                              preserved by construction.
     sentTok  Nat             the standing (unconsumed) sentinel file:
                              0 = none, else the touch id.  A second
                              touch OVERWRITES it — the agent's own
                              doing, and not an orphan: the protocol
                              owes an ack for CONSUMED nonces.
     pendN    SUBSET Nonces   the pending record's covered nonce set.
                              Non-empty IS the record's existence
                              (`PendLive`), and its maximum IS the
                              record's covered mtime, because every
                              touch in this model carries a nonce.
     pendDirty SUBSET Paths   the paths that were LOCALLY DIRTY at the
                              consume — the agent's own un-published
                              work, which is what the boundary owes it.
                              A path that was clean at consume time
                              carries no promise: its content came from
                              the remote, and the remote is entitled to
                              move it (an inbox adoption does exactly
                              that, and TLC produced one as a
                              counterexample before this field existed).
     pendMint Nat             the generation MINT WATERMARK at consume:
                              every generation >= this was created
                              after the declaration.  D1's guarantee is
                              at-LEAST ("the published state may include
                              later bytes for a racing file, never
                              earlier ones"), so the promise cannot be
                              stated as snapshot equality — see
                              `BoundaryBroken`.
     pendCov  [Paths -> Nat]  the tree AT CONSUME TIME — D1's at-least
                              guarantee is stated at exactly that
                              instant ("every write visible on disk at
                              consume time is in the published set"),
                              so this is what the ack promises.
     honored  BOOLEAN         a barrier COMPLETED strictly after the
                              latest consume.  Lost on restart (it is
                              in-memory barrier state), which is what
                              makes the uniform crash rule reachable.
     pendReRun BOOLEAN        this pending record survived a restart —
                              the ghost `ProbeAckAfterCrash` names.
     owed     SUBSET Nonces   every nonce this incarnation has CONSUMED.
                              Never shrinks while the pod lives; dies
                              with the pod, because a pod replacement
                              takes the agent and the tree with it.
     ackN     SUBSET Nonces   `.flint/<verb>.ack`'s covered nonce set.
                              Status is deliberately absent: the shipped
                              `ack_matches` does not read it.
     lastDirty SUBSET Paths   the dirt set frozen at the LAST barrier's
                              scan.  Tracked only under SyncEnabled (it
                              stays {} otherwise, so every tranche-1
                              state space is preserved by construction).
                              This is what the refuted sync reads.
   gh fields:
     amputated  BOOLEAN  an acked HITL write silently lost (either stamp site)
     resurrected BOOLEAN an unpublished delete undone by re-materialize
     stragglerInstalls, stragglerCas, deposedPuts : Nat
     barriers, done, gc, gcCited, refusals, cited, takeovers, crashes, restarts : Nat
     adoptOwn : Nat      the own-crashed-PUT 412 adoption fired
     stallUsed BOOLEAN
     nextGen, hitl : Nat
     scopedDeferrals : Nat  paths a SCOPED sync saw changed remotely and
                            deliberately left for the inbox flow (the
                            action-written non-vacuity ghost: the probe
                            names the ACTION, never the situation)
     deferredPaths : SUBSET Paths  which ones, still outstanding
     deferredLater : Nat    U16: a deferred out-of-scope path that a LATER
                            consume actually integrated.  `scopedDeferrals`
                            proves the deferral happened; D4's whole
                            loss-avoidance argument is that the entry
                            ARRIVES, and `Inv_NoForeignLost` is a stamp
                            written inside `Sync`, not an eventual-
                            integration property.  Without this the
                            deferral could be a synonym for the loss.
     staged, cites, reaped, withheld, forcedCites, citeSpan : Nat
     carriedCite BOOLEAN    a citation installed a pending set that had
                            survived a lane pass: the durability/visibility
                            split actually ACCUMULATED, rather than every
                            citation happening to follow its own lane
     touches, acks, honors, refusedAcks, coalesced, fastPaths,
     ackAfterRestart : Nat
     fastHonor  BOOLEAN     a pending sentinel was honored by a
                            SKIP-ON-NO-DIFF pass rather than a full
                            barrier — without this the FastPathGuards
                            runs could hold vacuously
     ackEarly   BOOLEAN     an ok ack was written while the agent's own
                            declared work was neither cited, superseded
                            by later bytes, nor surfaced
     ackIncoherent BOOLEAN  an ok ack named a manifest that did not cite
                            everything this workspace had integrated
     fencedOkAck BOOLEAN    an ok ack was written by an incarnation the
                            cell had already deposed
     srcMismatch BOOLEAN    an ok ack named one clock while the manifest
                            it acked carried another.  The ack is a LOCAL
                            file and the manifest is what the FLEET reads,
                            so this is the bucket disagreeing with the
                            agent about the same boundary
     foreignLost BOOLEAN    a sync advanced the merge base for a path it
                            neither applied nor surfaced a conflict for —
                            i.e. it claimed to have integrated a generation
                            it never saw.  That is exactly the silent,
                            permanent loss D4 exists to prevent: the next
                            merge computes `changed = FALSE` for it and it
                            is never queued again.
     ---- tranche 6: the barrier lease (each written by ONE action) ----
     claimed  SUBSET Syncers  who has claimed the cell at least once (Claim)
     interleaved BOOLEAN  an upload LANDED while another syncer held the
                            cell in its commit section (Upload) — the
                            required-reachable probe: two writers really
                            do overlap uploads with a commit
     handoffs 0|1          a claim by the syncer the release NAMED (Claim)
     deposals Nat          a quiet holder was deposed mid-commit (Claim)
     deadSkips 0|1         a quiet handoff was skipped (SkipDeadHandoff)
     enqueues 0|1          a syncer queued for the cell (Enqueue)
     abandoned 0|1         a fenced holder abandoned its barrier and kept
                            running (the fence arms, under BarrierLease)
     adoptWithheld 0|1     a CAS withheld an adopted entry whose object
                            was gone (CASInstall, under the fix)
     sameBytes Nat         `AgentWriteSame`'s budget counter
     uploadWithheld 0|1    a CAS withheld an entry its own LANDED upload
                            added, because the object was gone or moved
                            (CASInstall, under VerifyUploadedCitations)
     staleOverride BOOLEAN a CAS cited its own upload's generation over a
                            citation the key STILL HOLDS, while the key no
                            longer held the upload (CASInstall, BarrierLease
                            only) — see Inv_NoStaleOverride
     hitlRetired SUBSET (Paths \X Nat)  acked UI writes whose object was
                            deleted or overwritten by a party entitled to:
                            a writer that integrated it, or the UI's own
                            later write (GCDelete, Upload, HitlWrite; only
                            under MaxSameBytes > 0) — see Inv_HITLTracked
     ---- tranche 7 (each written by ONE action, read only by its probe) ----
     pullOnlys 0|1        a PULL-ONLY boundary ran (PullOnly)
     emptyInstalls 0|1    a commit installed nothing (CASInstall)
     tombRemoved 0|1      a queued deletion removed a clean copy (Consume)
     tombSuperseded 0|1   a queued deletion was superseded by an object
                            at the key — the fix firing (Consume) *)

------------------------------------------------------------------------------
(* Helpers *)

Gens == 0..MaxGen

Nonces      == 1..MaxTouches
PendLive(s) == sc[s].pendN # {}
NoPend      == [p \in Paths |-> 0]
\* `ack_matches`: every pending nonce named by the standing ack.  The
\* implementation ALSO requires the ack's covered mtime not to be older
\* than the pending's; here every touch carries a nonce and touch ids
\* are monotone, so the subset test implies it (ledger entry).
AckMatches(s) == sc[s].pendN \subseteq sc[s].ackN

(* WHICH CLOCK a boundary belongs to.  One function, read by BOTH sides:
   the installer stamps it on the manifest and the ack names it to the
   agent.  That is the whole point — the shipped code computed the two
   independently, so a drain could rewrite its ack to `drain` while the
   manifest it installed still said `sentinel`, and the BUCKET (which is
   what an operator reads, and the ack is not) held the wrong one.       *)
\* Read the SAME test the install reads. `honored` is set BY the install,
\* so at stamp time it is still FALSE — asking it there would stamp every
\* sentinel honor "cadence" and the invariant would fire on correct code
\* (the first pilot did exactly that). A live pending record is what makes
\* a boundary a sentinel honor, and it outlives the ack: `AckOk` requires
\* it, and `RetirePending` runs only afterwards.
BoundaryClock(s) == IF SentinelEnabled /\ PendLive(s) THEN "sentinel" ELSE "cadence"

(* What the install actually writes.  The mutation is not "stamps the
   wrong thing" but the subtler shape that shipped: the install goes
   through an UNSTAMPED CAS, so every boundary reads as the default
   clock however it was driven.                                          *)
InstallSource(s) == IF StampBoundarySource THEN BoundaryClock(s) ELSE "cadence"

(* Tranche 6: WHICH DOCUMENT an ack is judged against.  An ok ack names
   a seq (`remote.seq`) — the document this workspace installed, or, on
   the fast path, the one it found unmoved since its last install or
   checkout (`manSeq = expSeq`, and `instSnap` tracks both).  With one
   writer that document IS the live manifest at ack time: nothing else
   could move it between the honor and the ack.  With two, the other
   writer's commit can land in between — TLC's first sentinel run under
   the barrier lease was exactly that, a fast-path honor followed by the
   other writer's delete followed by the ack — and a later install that
   merges from theirs and preserves this workspace's entries is the
   two-writer rule working, not an incoherent ack.  The live manifest
   stays the subject of every durability invariant.                    *)
AckedDoc(s) == IF BarrierLease THEN sc[s].instSnap ELSE manifest
AckedSrc(s) == IF BarrierLease THEN sc[s].instSrc ELSE manSrc


(* ---- tranche 7: the merge's per-path judgements, hoisted out of
   CASInstall so the PULL-ONLY boundary — which merges and installs
   nothing — reads the same rules.  CASInstall's bodies are unchanged;
   see the comments there for why each reads as it does.                *)
ForeignEntry(s, p) ==
  /\ MergeCapable
  /\ manifest[p] # sc[s].instBase[p]
  /\ (MineIsNotForeign => manifest[p] \notin sc[s].known)
RepairOwed(s, p) ==
  /\ p \notin (sc[s].scanU \cup sc[s].scanD \cup sc[s].parked)
  /\ sc[s].baseline[p] # sc[s].instBase[p]
  /\ objects[p] = sc[s].baseline[p]
RepairDeclined(s, p) ==
  /\ p \notin (sc[s].scanU \cup sc[s].scanD \cup sc[s].parked)
  /\ sc[s].baseline[p] # sc[s].instBase[p]
  /\ objects[p] # sc[s].baseline[p]
\* What a merge queues for the TREE (`merge_onto`'s `foreign` and `gone`):
\* theirs moved off the merge base at a path this barrier neither uploaded,
\* re-cited nor parked — an upsert — or dropped a path the base had that
\* this barrier neither uploaded, deleted, re-cited nor parked — a deletion.
\* A citation repair is one of the merge's own upserts, so it is neither.
MineInMerge(s) == sc[s].parked \cup (sc[s].scanU \cap sc[s].upDone)
MergeForeign(s) ==
  {<<p, manifest[p]>> : p \in {q \in Paths :
     /\ q \notin MineInMerge(s)
     /\ ~RepairOwed(s, q)
     /\ ForeignEntry(s, q)
     /\ manifest[q] # 0}}
MergeGone(s) ==
  {<<p, 0>> : p \in {q \in Paths :
     /\ MergeCapable
     /\ q \notin MineInMerge(s) \cup sc[s].scanD
     /\ ~RepairOwed(s, q)
     /\ sc[s].instBase[q] # 0
     /\ manifest[q] = 0}}
\* `state::queue_foreign`: keyed by path, a later change replaces the
\* queued one.  Under the mutation nothing is queued at all.
QueueUpsert(q, new) ==
  IF QueueForeignChanges
  THEN {pr \in q : pr[1] \notin {x[1] : x \in new}} \cup new
  ELSE q
QueuedUpserts(s) == {pr \in sc[s].fq : pr[2] # 0}
QueuedDeletes(s) == {pr[1] : pr \in {x \in sc[s].fq : x[2] = 0}}

Deposed(s)  == cellEpoch > sc[s].epoch
Running(s)  == sc[s].st = "running"

(* ---- tranche 6: the per-barrier lease --------------------------------
   The COMMIT SECTION runs from the claim to the release.  Only there does
   the cell's epoch mean anything to a syncer: before the claim it holds
   nothing, and `Deposed` merely compares against the last epoch it held.
   Under the life lease every post-claim step of the incarnation is in
   the commit section, which is what `DeposedHolder` collapses to.       *)
Holding(s)       == sc[s].pc \in {"claimed", "delDone", "cased"}
DeposedHolder(s) == Deposed(s) /\ (~BarrierLease \/ Holding(s))
Fenced(s)        == EpochCheck /\ DeposedHolder(s)
CellFresh        == cellEpoch = 0
CellHeld         == cellEpoch > 0 /\ ~cellReleased
Quiet(t)         == sc[t].st \in {"stalled", "dead"}
InQueue(s)       == \E i \in 1..Len(cellQueue) : cellQueue[i] = s
Without(q, s)    == SelectSeq(q, LAMBDA t : t # s)
\* Uploads complete: the barrier wants the cell.  Under the life lease
\* this IS `CASReady`; under the barrier lease the claim sits between.
UploadsDone(s)   == sc[s].scanU \subseteq (sc[s].upDone \cup sc[s].parked)
PreCommitReady(s) == sc[s].pc = "scanned" /\ UploadsDone(s)
\* Tranche 7: the PULL-ONLY boundary (`barrier.rs`, before the commit
\* section): nothing uploaded, deleted, consumed from the shared inbox,
\* removed or re-cited — so the merge can only add nothing, and the code
\* never claims for it.  (An adopt is an upload here: scanU = {} rules it
\* out.)  Defined over the same helpers the commit's merge reads.
PullOnlyReady(s) ==
  /\ BarrierLease /\ EmptyInstall
  /\ sc[s].pc = "scanned"
  /\ sc[s].scanU = {} /\ sc[s].scanD = {}
  /\ sc[s].consumed = {} /\ sc[s].declared = {}
  /\ ~\E p \in Paths : RepairOwed(s, p)
WantsCell(s)     == \/ PreCommitReady(s) /\ ~PullOnlyReady(s)
                    \/ sc[s].pc = "waiting"
                    \* A projected replay: the claim's reason may be a path
                    \* this run does not model (see the constant).
                    \/ ProjectedTrace /\ sc[s].pc = "scanned"
\* Every claim bumps the epoch; every claim follows a scan, so the
\* barrier budget bounds it (+2 for the life lease's StartA and ClaimB).
\* Under the liveness abstraction it saturates there instead.
EpochBound       == MaxBarriers + 2
NextEpoch        == IF InfiniteBarriers /\ cellEpoch >= EpochBound
                    THEN cellEpoch ELSE cellEpoch + 1
\* The claim rule (protocol of record §3): a FRESH cell; a RELEASED cell
\* whose handoff is nobody or me; or a HELD cell whose holder has been
\* quiet for the 60 s rule (stalled or dead) — the DEPOSAL, which is the
\* only arm that rotates.
ClaimEnabled(s) ==
  \/ CellFresh
  \/ cellReleased /\ cellHandoff \in {"none", s}
  \/ CellHeld /\ cellHolder # s /\ Quiet(cellHolder)
\* The 20 s rule on a handoff: the named waiter is quiet, so the released
\* cell is anybody's and the waiter is dropped.
SkipEnabled(s) ==
  /\ DeadHandoffSkip
  /\ cellReleased /\ cellHandoff \notin {"none", s}
  /\ Quiet(cellHandoff)
\* What a release writes: the queue head becomes the handoff (the
\* ticket), or nobody under the mutation.
ReleaseCell ==
  /\ cellReleased' = TRUE
  \* What ships (HandoffAtClaim): `epoch_handoff` names the head of the
  \* list the holder read at its CLAIM and writes the REST OF THAT LIST
  \* back as the queue, so a ticket taken after the claim is dropped.
  \* The model's original rule is the head of the queue as it stands.
  /\ cellHandoff' = IF ~Ticket THEN "none"
                    ELSE IF HandoffAtClaim
                         THEN (IF cellSeen = <<>> THEN "none" ELSE Head(cellSeen))
                         ELSE (IF cellQueue = <<>> THEN "none" ELSE Head(cellQueue))
  /\ cellQueue'   = IF ~Ticket THEN cellQueue
                    ELSE IF HandoffAtClaim
                         THEN (IF cellSeen = <<>> THEN <<>> ELSE Tail(cellSeen))
                         ELSE (IF cellQueue = <<>> THEN cellQueue ELSE Tail(cellQueue))
  /\ cellSeen'    = <<>>
\* What a fence does to the incarnation.  Life lease: the process exits.
\* Barrier lease: the barrier is ABANDONED — its manifest never installed,
\* its uploads standing as uncited generations the next barrier adopts —
\* and the syncer keeps running; the next barrier claims again.
FencedSc(s) ==
  IF BarrierLease
  THEN [sc EXCEPT ![s].pc = "idle",
        ![s].scanU = {}, ![s].scanD = {},
        ![s].scanGen = [p \in Paths |-> 0],
        ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {},
        ![s].gcHeaded = {}, ![s].gcSeen = [p \in Paths |-> 0],
        ![s].adopted = {}, ![s].noInst = FALSE]
  ELSE [sc EXCEPT ![s].st = "dead"]
\* `touched` is dirt the generations cannot show: a rewrite with the bytes
\* the baseline cites (see MaxSameBytes).  {} in every earlier cfg.
Dirty(s)    == {p \in Paths : sc[s].local[p] # sc[s].baseline[p]} \cup sc[s].touched
USet(s)     == {p \in Dirty(s) : sc[s].local[p] # 0}
\* The two-consecutive-scans rule.  Under ~TwoScanDelete this is the
\* one-scan rule every pre-existing cfg was written against, so their
\* state spaces are unchanged by construction.
DSet(s)     == {p \in Dirty(s) : sc[s].local[p] = 0
                                 /\ \/ ~TwoScanDelete
                                    \/ p \notin sc[s].prevScan
                                    \/ DeclaredConfirmsAbsence /\ SentinelEnabled
                                       /\ sc[s].pendN # {}}
CitedGens   == {manifest[p] : p \in Paths} \ {0}

\* The generation an acked pair refers to is destroyed by a syncer that
\* never legitimately learned it, with no surfaced record: the amputation
\* stamp for the destruction site.
Destroys(s, p, cur) ==
  /\ <<p, cur>> \in hitlAcked
  /\ cur \notin sc[s].known
  /\ <<p, cur>> \notin conflicts


------------------------------------------------------------------------------
(* Initial state: a project seeded with every path published at gen 1.     *)

Init ==
  /\ cellEpoch = 0 /\ cellHolder = "none" /\ manSeq = 1
  /\ cellQueue = <<>> /\ cellHandoff = "none" /\ cellReleased = FALSE
  /\ cellSeen = <<>>
  /\ manSrc = "none"
  /\ manifest = [p \in Paths |-> IF p \in FreePaths THEN 0 ELSE 1]
  /\ objects  = [p \in Paths |-> IF p \in FreePaths THEN 0 ELSE 1]
  /\ inbox = {} /\ removals = {} /\ window = 0
  \* Every path starts published at gen 1, so gen 1 is its only version
  \* — except the FreePaths, which start absent.
  /\ versions = [p \in Paths |-> IF p \in FreePaths THEN {} ELSE {1}]
  /\ stage = [s \in Syncers |-> [p \in Paths |-> 0]]
  /\ stageBase = [s \in Syncers |-> [p \in Paths |-> 0]]
  /\ withheldDel = [s \in Syncers |-> {}]
  /\ sc = [s \in Syncers |->
       [st |-> "unstarted", pc |-> "idle", epoch |-> 0, expSeq |-> 0,
        installed |-> FALSE,
        local |-> [p \in Paths |-> 0], baseline |-> [p \in Paths |-> 0],
        scope |-> Paths, prevScan |-> {},
        instBase |-> [p \in Paths |-> 0],
        instSnap |-> [p \in Paths |-> 0], instSeq |-> 0, instSrc |-> "none",
        known |-> {}, scanU |-> {}, scanD |-> {},
        scanGen |-> [p \in Paths |-> 0], upDone |-> {}, parked |-> {},
        gcDone |-> {}, gcTook |-> {}, gcHeaded |-> {}, gcSeen |-> [p \in Paths |-> 0],
        adopted |-> {}, touched |-> {}, repairMoved |-> {},
        lastDirty |-> {}, stageCarried |-> FALSE,
        citeDone |-> {},
        sentTok |-> 0, pendN |-> {}, pendCov |-> [p \in Paths |-> 0],
        pendMint |-> 0, pendDirty |-> {},
        honored |-> FALSE, pendReRun |-> FALSE, owed |-> {}, ackN |-> {},
        citeDropped |-> {},
        fq |-> {}, noInst |-> FALSE,
        declared |-> {}, consumed |-> {}]]
  /\ hitlAcked = {} /\ conflicts = {}
  /\ gh = [amputated |-> FALSE, resurrected |-> FALSE,
           stragglerInstalls |-> 0, stragglerCas |-> 0, deposedPuts |-> 0,
           barriers |-> 0, done |-> 0, gc |-> 0, gcCited |-> 0, refusals |-> 0,
           cited |-> 0, takeovers |-> 0, crashes |-> 0, restarts |-> 0,
           adoptOwn |-> 0, stallUsed |-> FALSE, nextGen |-> 2, hitl |-> 0,
           syncs |-> 0, syncApplied |-> 0, syncConflicts |-> 0,
           syncDestroyed |-> FALSE,
           scopedDeferrals |-> 0, foreignLost |-> FALSE,
           deferredPaths |-> {}, deferredLater |-> 0,
           staged |-> 0, cites |-> 0, reaped |-> 0, withheld |-> 0,
           forcedCites |-> 0, citeSpan |-> 0,
           carriedCite |-> FALSE,
           touches |-> 0, acks |-> 0, honors |-> 0, refusedAcks |-> 0,
           coalesced |-> 0, fastPaths |-> 0, ackAfterRestart |-> 0,
           fastHonor |-> FALSE, ackEarly |-> FALSE,
           ackIncoherent |-> FALSE, fencedOkAck |-> FALSE,
           srcMismatch |-> FALSE,
           partialAcks |-> 0, declaredDrops |-> 0,
           narrows |-> 0, narrowed |-> {}, narrowRecited |-> {},
           removals |-> 0, removalsApplied |-> 0, removalsRefused |-> 0,
           renamed |-> {}, renamesApplied |-> 0, renameRefused |-> {},
           citedPairs |-> {},
           claimed |-> {}, interleaved |-> FALSE, handoffs |-> 0,
           deposals |-> 0, deadSkips |-> 0, enqueues |-> 0, abandoned |-> 0,
           adoptWithheld |-> 0, sameBytes |-> 0, uploadWithheld |-> 0,
           staleOverride |-> FALSE, hitlRetired |-> {},
           pullOnlys |-> 0, emptyInstalls |-> 0,
           tombRemoved |-> 0, tombSuperseded |-> 0, orphanTracks |-> 0]

------------------------------------------------------------------------------
(* Lifecycle *)

StartA ==
  /\ ~BarrierLease           \* the life lease: claim, then checkout
  /\ sc["A"].st = "unstarted" /\ cellHolder = "none"
  /\ cellHolder' = "A" /\ cellEpoch' = 1 /\ UNCHANGED cellSeen
  /\ sc' = [sc EXCEPT
       !["A"].st = "running", !["A"].epoch = 1, !["A"].expSeq = manSeq,
       !["A"].local = [p \in Paths |-> manifest[p]],
       !["A"].baseline = [p \in Paths |-> manifest[p]],
       !["A"].prevScan = IF TwoScanDelete
                            THEN {q \in Paths : manifest[q] # 0} ELSE {},
       !["A"].instBase = [p \in Paths |-> manifest[p]],
       !["A"].known = CitedGens]
  /\ UNCHANGED leaseVars
  /\ UNCHANGED <<manSeq, manSrc, manifest, objects, inbox, removals, window,
                 hitlAcked, conflicts, gh>>

(* Tranche 6: under the barrier lease a syncer starts by CHECKING OUT and
   holds nothing — `checkout` is lease-free, and the cell is claimed
   inside each barrier.  Both syncers start this way; there is no
   takeover.  B starts no earlier than A: pure symmetry breaking (the two
   are interchangeable here, except that only A can stall).             *)
StartLease(s) ==
  /\ BarrierLease /\ sc[s].st = "unstarted"
  /\ \A i \in DOMAIN Writers :
       Writers[i] = s => \A j \in 1..(i - 1) : sc[Writers[j]].st # "unstarted"
  /\ sc' = [sc EXCEPT
       ![s].st = "running", ![s].expSeq = manSeq,
       ![s].local = [p \in Paths |-> manifest[p]],
       ![s].baseline = [p \in Paths |-> manifest[p]],
       ![s].prevScan = IF TwoScanDelete
                          THEN {q \in Paths : manifest[q] # 0} ELSE {},
       ![s].instBase = [p \in Paths |-> manifest[p]],
       \* The document a never-installed writer's fast-path ack names.
       ![s].instSnap = [p \in Paths |-> manifest[p]], ![s].instSeq = manSeq,
       ![s].instSrc = manSrc,
       ![s].touched = {},
       ![s].known = CitedGens]
  /\ UNCHANGED leaseVars
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                 inbox, removals, window, hitlAcked, conflicts, gh>>

CrashPod(s) ==
  /\ ~GatedCitation          \* gated replaces this with CrashPodGated
  /\ sc[s].st \in {"running", "stalled"}
  /\ gh.crashes < MaxCrashes
  \* Pod REPLACEMENT: the emptyDir goes, and with it the pending record,
  \* the workspace tree, `.flint/` and the agent that was waiting on it.
  \* Nothing is owed to a process that no longer exists — which is
  \* exactly why `Inv_NoNonceOrphan` is per-incarnation.
  /\ sc' = [sc EXCEPT ![s].st = "dead",
       ![s].sentTok = 0, ![s].pendN = {}, ![s].pendCov = NoPend,
       ![s].pendMint = 0, ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE,
       ![s].owed = {}, ![s].ackN = {},
       \* Tranche 7: the writer-local queue is a file in the same emptyDir.
       ![s].fq = {}, ![s].noInst = FALSE,
       ![s].declared = {}, ![s].consumed = {}]
  /\ gh' = [gh EXCEPT !.crashes = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* Pod REPLACEMENT under gated staging.  §4's substrate rule: `pending`
   lives in the emptyDir, so it DIES with the pod — "pod recreation
   destroys the emptyDir pending record and converts the whole uncited
   window into D9 orphans" (§2.6).  The ungated CrashPod cannot express
   that: stage/stageBase/withheldDel are framed at the Next composition
   (`UNCHANGED gatedVars` over BaseNext), so a crashed incarnation's
   entire staged set survived it — the exact opposite of the design, and
   the reason no gated cfg could safely raise MaxCrashes (review: U13).

   `citeDone` must go WITH it, and that is the half a frame fix alone
   misses.  Clearing only gatedVars leaves a dead syncer's frozen
   citeDone quantified by Inv_BoundaryAtomic against a Valid(s) that
   keeps moving as the inbox changes — a false positive that surfaces
   EARLIER (depth 9) than the one it was meant to remove (depth 16), on
   a STRICT run.  Both, or neither.                                     *)
CrashPodGated(s) ==
  /\ GatedCitation
  /\ sc[s].st \in {"running", "stalled"}
  /\ gh.crashes < MaxCrashes
  /\ sc' = [sc EXCEPT ![s].st = "dead",
       ![s].sentTok = 0, ![s].pendN = {}, ![s].pendCov = NoPend,
       ![s].pendMint = 0, ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE,
       ![s].owed = {}, ![s].ackN = {},
       ![s].citeDone = {},
       ![s].declared = {}, ![s].consumed = {}]
  /\ stage' = [stage EXCEPT ![s] = [p \in Paths |-> 0]]
  /\ stageBase' = [stageBase EXCEPT ![s] = [p \in Paths |-> 0]]
  /\ withheldDel' = [withheldDel EXCEPT ![s] = {}]
  /\ gh' = [gh EXCEPT !.crashes = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, versions>>

(* Container restart in the SAME pod: the emptyDir survives, so local,
   baseline, known, expSeq and the incarnation epoch persist; only the
   in-memory barrier state is lost.  The restart matrix's marker-present
   row: never re-materialize — unless the mutation arm says otherwise.   *)
Restart(s) ==
  /\ Running(s)
  /\ gh.restarts < MaxRestarts
  /\ LET res == \E p \in Paths :
          sc[s].local[p] = 0 /\ manifest[p] # 0 /\ sc[s].baseline[p] # 0
         newLocal ==
           IF RematerializeOnRestart
           THEN [p \in Paths |-> IF sc[s].local[p] = 0 /\ manifest[p] # 0
                                 THEN manifest[p] ELSE sc[s].local[p]]
           ELSE sc[s].local
     IN
       /\ sc' = [sc EXCEPT ![s].pc = "idle",
            \* `honored` is in-memory barrier state and dies here; the
            \* pending record is a FILE in the surviving emptyDir.  That
            \* asymmetry is the uniform crash rule's whole subject.
            ![s].honored = FALSE,
            ![s].pendReRun = IF SentinelEnabled /\ PendLive(s)
                             THEN TRUE ELSE @,
            ![s].local = newLocal,
            ![s].known = IF RematerializeOnRestart THEN @ \cup CitedGens ELSE @,
            ![s].scanU = {}, ![s].scanD = {},
            ![s].scanGen = [p \in Paths |-> 0],
            ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {},
            ![s].gcHeaded = {}, ![s].gcSeen = [p \in Paths |-> 0],
            ![s].adopted = {}, ![s].noInst = FALSE,
            \* `consumed` is in-memory barrier state; `declared` is the
            \* intent journal, a FILE in the surviving emptyDir (and so is
            \* tranche 7's queue, `fq`, which this leaves alone).
            ![s].consumed = {}]
       /\ gh' = [gh EXCEPT !.restarts = @ + 1,
            !.resurrected = @ \/ (RematerializeOnRestart /\ res)]
  \* Tranche 6: a restarted container that finds the cell HELD by its own
  \* holder_id releases it at startup — it holds nothing in memory, and
  \* the intent journal replays what the previous container left.
  /\ IF BarrierLease /\ CellHeld /\ cellHolder = s
     THEN ReleaseCell ELSE UNCHANGED leaseVars
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* A node freeze / partition: the process persists but takes no steps.
   Costs no crash budget — a stall is not a data-loss event (the
   WriterLimbo lesson).  One stall per run bounds the space.            *)
StallA ==
  /\ AllowStall /\ sc["A"].st = "running" /\ ~gh.stallUsed
  /\ sc' = [sc EXCEPT !["A"].st = "stalled"]
  /\ gh' = [gh EXCEPT !.stallUsed = TRUE]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

ThawA ==
  /\ sc["A"].st = "stalled"
  /\ sc' = [sc EXCEPT !["A"].st = "running"]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, gh>>

(* A deposed incarnation's next successful cell read (heartbeat renew)
   discovers the higher epoch and self-fences.                          *)
RenewDiscover(s) ==
  /\ Running(s) /\ DeposedHolder(s)
  \* D12 x D2.  The heartbeat renewal runs on its own interval,
  \* decoupled from publish cadence, so on deposal it is usually the
  \* FIRST arm to find out — ahead of the floor tick and ahead of a poll
  \* arm with nothing due.  It therefore owes the refused acks: exiting
  \* unsettled here strands the waiting agent with a marker still
  \* advertising live verbs, which is the hole D2 exists to close.
  \* (Shipped code returned from this arm without settling; the model
  \* was being written when the code was read, and the leg for it is
  \* `the_heartbeat_arm_settles_owed_acks_when_it_finds_the_fence`.)
  \* Tranche 6: `renew_if_due` between the commit's long steps finds the
  \* fence.  A fence is no longer a death and no `refused-fenced` ack
  \* exists: the barrier is abandoned and the pending record stands for
  \* the next barrier to honor.
  /\ LET settle == ~BarrierLease /\ SentinelEnabled /\ RefuseOnFence /\ PendLive(s) IN
       /\ sc' = [FencedSc(s) EXCEPT
            ![s].ackN = IF settle THEN @ \cup sc[s].pendN ELSE @,
            ![s].pendN = IF settle THEN {} ELSE @,
            ![s].pendCov = IF settle THEN NoPend ELSE @,
            ![s].pendMint = IF settle THEN 0 ELSE @,
            ![s].pendDirty = IF settle THEN {} ELSE @,
            ![s].honored = FALSE,
            ![s].pendReRun = IF BarrierLease THEN @ ELSE FALSE]
       /\ gh' = [gh EXCEPT !.refusedAcks = @ + (IF settle THEN 1 ELSE 0),
                          !.abandoned = IF BarrierLease THEN 1 ELSE @]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* Takeover: B observes the quiet cell (abstracting the 6-poll protocol)
   and claims.  Rotation: the successor CAS-rewrites the manifest
   (seq++, content-identical) BEFORE serving — the straggler fence.     *)
ClaimB ==
  /\ ~BarrierLease           \* the life lease's takeover; see Claim
  /\ sc["B"].st = "unstarted" /\ cellHolder = "A"
  /\ sc["A"].st \in {"stalled", "dead"}
  /\ (Rotation => manSeq < MaxSeq)
  /\ cellHolder' = "B" /\ cellEpoch' = cellEpoch + 1 /\ UNCHANGED cellSeen
  /\ manSeq' = IF Rotation THEN manSeq + 1 ELSE manSeq
  \* A rotation is a fence, not a boundary: it publishes nothing, so it
  \* clears the stamp rather than inheriting the deposed holder's.
  /\ manSrc' = IF Rotation THEN "none" ELSE manSrc
  /\ sc' = [sc EXCEPT !["B"].st = "claiming",
                      !["B"].epoch = cellEpoch + 1]
  /\ gh' = [gh EXCEPT !.takeovers = @ + 1]
  /\ UNCHANGED leaseVars
  /\ UNCHANGED <<manifest, objects, inbox, removals, window, hitlAcked, conflicts>>

CheckoutB ==
  /\ sc["B"].st = "claiming"
  /\ sc' = [sc EXCEPT !["B"].st = "running", !["B"].expSeq = manSeq,
       !["B"].local = [p \in Paths |-> manifest[p]],
       !["B"].baseline = [p \in Paths |-> manifest[p]],
       !["B"].prevScan = IF TwoScanDelete
                            THEN {q \in Paths : manifest[q] # 0} ELSE {},
       !["B"].instBase = [p \in Paths |-> manifest[p]],
       !["B"].known = CitedGens]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, gh>>

------------------------------------------------------------------------------
(* The agent (local edits only — the syncer publishes them later) *)

AgentWrite(s, p) ==
  /\ Running(s) /\ gh.nextGen <= MaxGen
  /\ sc' = [sc EXCEPT ![s].local[p] = gh.nextGen,
                      ![s].known = @ \cup {gh.nextGen}]
  \* Re-creating a narrowed path is the FEATURE (`sync.rs:13-22` names
  \* merge -> inbox -> consume as the designed destination for
  \* out-of-scope changes), so it leaves the narrow ledger — otherwise
  \* Inv_NarrowNeverRecites would fire on legitimate widening and the
  \* invariant would be unsound rather than strong.
  /\ gh' = [gh EXCEPT !.nextGen = @ + 1, !.narrowed = @ \ {p}]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* Finding 13: the agent writes bytes a version it RECOGNISES already
   has — most simply, deletes a file and writes it back unchanged.  A
   real etag is the MD5 of the bytes, so the write carries THAT version's
   etag, not a fresh one.  Back to the baseline's own generation, the
   path is dirty only by its stat, which `touched` carries.  Restricted
   to recognised generations: bytes that coincide with a version this
   writer never saw would also have to extend `known`, which is the
   amputation stamp's witness, and finding 13 does not need them.      *)
AgentWriteSame(s, p) ==
  /\ BarrierLease /\ Running(s)
  /\ gh.sameBytes < MaxSameBytes
  /\ \E g \in sc[s].known \ {0} :
       /\ sc' = [sc EXCEPT ![s].local[p] = g,
                           ![s].touched = IF g = sc[s].baseline[p]
                                          THEN @ \cup {p} ELSE @]
       /\ gh' = [gh EXCEPT !.sameBytes = @ + 1, !.narrowed = @ \ {p}]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

AgentDelete(s, p) ==
  /\ Running(s) /\ sc[s].local[p] # 0
  /\ sc' = [sc EXCEPT ![s].local[p] = 0]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, gh>>

------------------------------------------------------------------------------
(* The NARROW verb (scoped-read design §4).                                *)

(* §4.2, as a machine-checkable claim.  `classify` reads exactly two
   states a narrow must pass through:

     present in scan, absent from baseline          -> UPLOAD
     present in baseline, absent from scan+prevScan -> DELETE

   so unlink-then-uncite crashes into publishing deletions, and
   uncite-then-unlink crashes into re-uploading identical bytes and
   re-citing everything just dropped.  NEITHER ORDER IS SAFE ALONE.

   The action drops one CLEAN, HELD path from the scope.  Clean because
   the shipped door refuses a narrow over a path with unpublished
   changes; held because dropping what was never held is a no-op.       *)
Narrow(s) ==
  /\ Running(s) /\ sc[s].pc = "idle"
  /\ gh.narrows < MaxNarrows
  /\ \E p \in sc[s].scope :
       /\ sc[s].baseline[p] # 0
       /\ sc[s].local[p] = sc[s].baseline[p]
       /\ sc' = [sc EXCEPT
            ![s].scope    = @ \ {p},
            \* Unlink: the tree half.  Skipped by the uncite-first arm.
            ![s].local    = IF NarrowAtomic \/ NarrowUnlinkFirst
                              THEN [sc[s].local EXCEPT ![p] = 0]
                              ELSE sc[s].local,
            \* Uncite: the held-set half.  Skipped by the unlink-first arm.
            ![s].baseline = IF NarrowAtomic \/ ~NarrowUnlinkFirst
                              THEN [sc[s].baseline EXCEPT ![p] = 0]
                              ELSE sc[s].baseline,
            \* prev_scan goes WITH the citation, never after it.
            \*
            \* THIS HALF IS NOT CHECKED HERE, AND THE GREEN RUN MUST NOT
            \* BE READ AS IF IT WERE.  `Dirty(s)` is `local[p] #
            \* baseline[p]`, so setting `baseline[p] = 0` alone already
            \* makes the path undirty and `prevScan` cannot change any
            \* classification.  MEASURED: deleting this line leaves
            \* LeanNarrowHolds green over 10,823 distinct states (vs
            \* 10,179 with it).  In `scan.rs` the two ARE separate
            \* structures and `classify` consults both, so the shipped
            \* rule has a conjunct this abstraction collapses.  Kept
            \* because the code needs it; recorded because the model is
            \* WEAKER than the code here, which is the safe direction for
            \* a miss and the wrong direction for a claim.
            ![s].prevScan = IF NarrowAtomic THEN @ \ {p} ELSE @]
       /\ gh' = [gh EXCEPT !.narrows = @ + 1, !.narrowed = @ \cup {p}]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

------------------------------------------------------------------------------
(* The gateway, abstracted to its bucket effects *)

(* A HITL (UI) write: object PUT + inbox entry (plan v2), or a direct
   manifest bump (the refuted v1 reading, InboxEnabled = FALSE).  The
   user reads fresh, so the PUT always matches the current object.      *)
HitlWrite(p) ==
  /\ gh.hitl < MaxHitl /\ gh.nextGen <= MaxGen
  /\ ~(WindowCheck /\ window # 0)
  /\ HitlOverwritesTrackedOnly =>
       \/ objects[p] = 0
       \/ objects[p] = manifest[p]
       \/ <<p, objects[p]>> \in inbox
  /\ (~InboxEnabled => manSeq < MaxSeq)
  /\ LET g == gh.nextGen IN
       /\ objects' = [objects EXCEPT ![p] = g]
       /\ IF InboxEnabled
          THEN /\ inbox' = inbox \cup {<<p, g>>}
               /\ UNCHANGED <<manifest, manSeq, manSrc>>
          ELSE /\ manifest' = [manifest EXCEPT ![p] = g]
               /\ manSeq' = manSeq + 1
               \* The refuted direct-bump path: a party that holds no
               \* lease installed this, so it carries no syncer clock.
               /\ manSrc' = "none"
               /\ UNCHANGED inbox
       /\ hitlAcked' = hitlAcked \cup {<<p, g>>}
       /\ gh' = [gh EXCEPT !.nextGen = @ + 1, !.hitl = @ + 1,
               !.hitlRetired = IF MaxSameBytes > 0 /\ <<p, objects[p]>> \in hitlAcked
                               THEN @ \cup {<<p, objects[p]>>} ELSE @]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, window, sc, conflicts, removals>>

(* The refusal the window is FOR (availability shape, bounded to keep the
   space small; the starvation bound is a tranche-2 liveness question).  *)
HitlRefused ==
  /\ WindowCheck /\ window # 0 /\ gh.refusals < 1
  /\ gh' = [gh EXCEPT !.refusals = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, sc, hitlAcked, conflicts>>

(* Tranche 5: a DECLARED removal (delete/rename design §3).  The caller
   outside the pod records intent in the cell and touches no object;
   the syncer performs it at its next consume.  Not window-gated (§12):
   a removal touches nothing when it is recorded.                        *)
Tracked(p) == manifest[p] # 0 \/ \E pr \in inbox : pr[1] = p

HitlRemove(p) ==
  /\ gh.removals < MaxRemovals
  /\ p \notin removals /\ Tracked(p)
  /\ removals' = removals \cup {p}
  /\ gh' = [gh EXCEPT !.removals = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox,
                 window, sc, hitlAcked, conflicts>>

(* A rename is a server-side copy to the destination (a fresh
   generation of the same bytes — create first, §5) and then ONE CAS
   carrying the destination entry and the source removal (§6).  The
   destination is acked as a HITL write is.  Window-gated, because it
   carries an entry.                                                    *)
HitlRename(p, q) ==
  /\ gh.removals < MaxRemovals /\ gh.nextGen <= MaxGen
  /\ p # q /\ p \notin removals /\ q \notin removals
  /\ Tracked(p) /\ ~Tracked(q) /\ objects[q] = 0
  /\ ~(WindowCheck /\ window # 0)
  /\ LET g == gh.nextGen IN
       /\ objects' = [objects EXCEPT ![q] = g]
       /\ inbox' = inbox \cup {<<q, g>>}
       /\ removals' = removals \cup {p}
       /\ hitlAcked' = hitlAcked \cup {<<q, g>>}
       \* The triple carries the SOURCE's generation at rename time:
       \* atomicity is about those bytes, not the name — an agent that
       \* re-creates the old name after the unlink has made a new file.
       /\ gh' = [gh EXCEPT !.nextGen = @ + 1, !.removals = @ + 1,
                          !.renamed = @ \cup {<<p, q, objects[p], g>>}]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, window, sc, conflicts>>

(* Finding 10's candidate fix.  An object sits at a CITED key under a
   generation the manifest does not cite and no inbox entry names: a
   writer's upload whose commit never came.  Any live writer may track it —
   an inbox entry, appended like the gateway's — and
   the consume that follows adopts it (clean) or preserves it (dirty), and
   the next commit cites it. *)
TrackOrphan(p) ==
  /\ BarrierLease /\ OrphanTrack
  /\ \E s \in Syncers : Running(s)
  \* No window guard.  The append the code would use waits for the window
  \* and admits past a dead barrier's at its deadline (`admits_hitl`); the
  \* model has no deadline, and its first run of this arm parked forever
  \* behind the window of the very writer that died holding the cell.
  \* Safety never rested on the window (LeanNoWindowHolds), so allowing the
  \* append at any time is the stronger check.
  /\ manifest[p] # 0 /\ objects[p] # 0 /\ objects[p] # manifest[p]
  /\ <<p, objects[p]>> \notin inbox
  /\ inbox' = inbox \cup {<<p, objects[p]>>}
  \* No budget: each firing adds a (path, generation) pair the inbox did
  \* not hold, and generations are budgeted, so the arm is finite.  A
  \* budget of one spent itself on a LIVE writer's in-flight upload in the
  \* first run and left the real orphan untracked — the code's sweep runs
  \* again, so the model must too.
  /\ gh' = [gh EXCEPT !.orphanTracks = 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, removals,
                 window, sc, hitlAcked, conflicts>>

------------------------------------------------------------------------------
(* The publish barrier (plan §2.1, seven steps; scan+intent merged, the
   consume->scan gap preserved — that gap is where strict-world parks
   come from) *)

(* Step 1: consume the inbox FIRST.  Clean paths adopt; dirty paths keep
   local (locally-dirty wins) and surface a conflict record; stale
   entries (superseded object) drop.  A barrier never runs against an
   unconsumed inbox.                                                     *)
Consume(s) ==
  /\ Running(s) /\ sc[s].pc = "idle"
  \* An honored boundary owes its ack BEFORE anything else runs: the
  \* ack write is the last step of the honoring pass, in the same
  \* single-threaded arm, so the loop cannot start another pass in
  \* between.  Without this the model interleaves a consume there and
  \* reports an incoherence the implementation cannot reach — the ack
  \* would be judged against a baseline that moved after the boundary
  \* it names.
  /\ ~(SentinelEnabled /\ sc[s].honored)
  /\ gh.barriers < MaxBarriers \/ InfiniteBarriers
  /\ LET
       \* Tranche 7: the writer-LOCAL queue's upserts join the entries (the
       \* code runs them first; with one entry per path and the key holding
       \* one generation, order among them changes nothing here), and an
       \* entry whose etag the baseline already holds is settled BEFORE
       \* every other arm, with no record — `consume_counted`'s "already".
       \* An entry whose object is gone leaves a `consume-object-missing`
       \* record.  Under ~WriterQueue: the rules as they were.
       cand == IF WriterQueue THEN inbox \cup QueuedUpserts(s) ELSE inbox
       already == IF WriterQueue
                  THEN {pr \in cand : sc[s].baseline[pr[1]] = pr[2]} ELSE {}
       missing == IF WriterQueue
                  THEN {pr \in cand \ already : objects[pr[1]] = 0} ELSE {}
       live == {pr \in cand \ already : objects[pr[1]] = pr[2]}
       adoptable == {pr \in live :
                       /\ sc[s].local[pr[1]] = sc[s].baseline[pr[1]]
                       /\ pr[1] \notin sc[s].touched}
       conflicted == live \ adoptable
       adoptPaths == {pr[1] : pr \in adoptable}
       surfPaths == IF ConflictSurfacing
                    THEN {pr[1] : pr \in conflicted} ELSE {}
       advPaths == adoptPaths \cup surfPaths
       local1 == [p \in Paths |->
                    IF p \in adoptPaths THEN objects[p] ELSE sc[s].local[p]]
       base1  == [p \in Paths |->
                    IF p \in advPaths THEN objects[p] ELSE sc[s].baseline[p]]
       \* ...then the queued DELETIONS, against the tree the entries left
       \* (the code's second loop): nothing on disk settles; a copy that is
       \* dirty — or was never in the baseline — stays and publishes, with
       \* a `consume-foreign-delete-vs-dirty` record; a clean copy goes.
       tAbsent  == {p \in QueuedDeletes(s) : local1[p] = 0}
       \* Under the fix a key that holds an object has superseded the
       \* deletion: settled, nothing removed, no record.
       tSuper   == IF TombstoneHeadsKey
                   THEN {p \in QueuedDeletes(s) \ tAbsent : objects[p] # 0}
                   ELSE {}
       tKept    == {p \in QueuedDeletes(s) \ (tAbsent \cup tSuper) :
                      \/ base1[p] = 0
                      \/ local1[p] # base1[p]
                      \/ p \in sc[s].touched}
       tRemoved == QueuedDeletes(s) \ (tAbsent \cup tSuper \cup tKept)
       local2 == [p \in Paths |-> IF p \in tRemoved THEN 0 ELSE local1[p]]
       base2  == [p \in Paths |-> IF p \in tAbsent \cup tRemoved THEN 0 ELSE base1[p]]
       \* ---- the DECLARED removals (tranche 5, design §4-§6) ---------
       \* After the entries, so a rename's destination is in the tree
       \* before its source leaves it (§5).  A rename's removal waits
       \* for its destination to be INTEGRATED AND CLEAN; a destination
       \* the agent had taken (the consume surfaced it, the agent's
       \* version wins) REFUSES the move — the source stays, rather
       \* than surviving only as a conflict copy.  A dirty source
       \* refuses.  A clean or already-absent source is unlinked and
       \* declared.  Refusals leave the cell now, with a record;
       \* performed removals leave it at Finish, after the manifest.
       dstOf(p) == {pr[2] : pr \in {x \in gh.renamed : x[1] = p}}
       dstReady(p) == ~RenameWaitsForDestination
                      \/ \A q \in dstOf(p) : base2[q] = objects[q] /\ local2[q] = base2[q]
       dstTaken(p) == \E q \in dstOf(p) :
                        q \in surfPaths \/ (base2[q] = objects[q] /\ local2[q] # base2[q])
       refused == {p \in removals :
                     \/ (local2[p] # 0 /\ local2[p] # base2[p])
                     \/ dstTaken(p)}
       applied == {p \in removals \ refused :
                     /\ (local2[p] = 0 \/ local2[p] = base2[p])
                     /\ dstReady(p)}
       declaredNow == IF DeclaredSkipsWalk THEN applied ELSE {}
     IN
       /\ sc' = [sc EXCEPT ![s].pc = "consumed",
            ![s].local = [p \in Paths |-> IF p \in applied THEN 0 ELSE local2[p]],
            ![s].baseline = base2,
            ![s].fq = IF WriterQueue THEN {} ELSE @,
            ![s].known = @ \cup {objects[p] : p \in advPaths},
            \* `baseline.prev_scan.insert(entry.path)`: a consumed path
            \* gets the two-scan protection as if the walk had seen it,
            \* so an agent delete right after the consume is a FIRST
            \* absence.  Gated like Scan's write, for the same reason.
            ![s].prevScan = IF TwoScanDelete
                            THEN (@ \cup adoptPaths) \ (tAbsent \cup tRemoved) ELSE @,
            ![s].declared = @ \cup declaredNow,
            \* What this barrier consumed, to leave the cell at Finish.
            ![s].consumed = IF EarlyInboxDrop THEN {} ELSE inbox]
       /\ conflicts' = conflicts
            \cup (IF ConflictSurfacing THEN conflicted ELSE {})
            \cup {<<p, local2[p]>> : p \in refused}
            \cup missing
            \cup {<<p, 0>> : p \in tKept}
       \* The rule that shipped cleared the cell here ("durably in the
       \* baseline"); the baseline dies with the pod.  See EarlyInboxDrop.
       /\ inbox' = IF EarlyInboxDrop THEN {} ELSE inbox
       /\ removals' = removals \ refused
       \* U16: the arrival half of D4. A path this workspace deferred
       \* out of scope is integrated HERE, one consume later — which is
       \* the only reason deferring it was not simply losing it.
       /\ gh' = [gh EXCEPT
            !.deferredLater = @ + Cardinality(advPaths \cap gh.deferredPaths),
            !.deferredPaths = @ \ advPaths,
            !.removalsApplied = @ + Cardinality(applied),
            !.removalsRefused = @ + Cardinality(refused),
            !.renamesApplied = @ + Cardinality({pr \in gh.renamed : pr[1] \in applied}),
            !.renameRefused = @ \cup {pr \in gh.renamed : pr[1] \in refused},
            !.tombRemoved = IF tRemoved # {} THEN 1 ELSE @,
            !.tombSuperseded = IF tSuper # {} THEN 1 ELSE @]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, window,
                 hitlAcked>>

(* Steps 2+3: scan-diff against the persisted baseline and CAS the
   window open (the intent).  A stale window (a lower epoch's) may be
   overridden; an open window at my own or a higher epoch blocks.       *)
Scan(s) ==
  /\ Running(s) /\ sc[s].pc = "consumed"
  \* Tranche 6: the window belongs to the commit section and opens AT
  \* the claim; a scan before the claim neither opens nor waits for it.
  /\ BarrierLease \/ window = 0 \/ window < sc[s].epoch
  /\ sc' = [sc EXCEPT ![s].pc = "scanned",
       \* A declared removal skips the two-scan guard (§4): the guard
       \* protects against absence INFERRED by a walk, and a
       \* declaration is not an inference.
       ![s].scanU = USet(s), ![s].scanD = DSet(s) \cup sc[s].declared,
       ![s].lastDirty = IF SyncEnabled THEN USet(s) \cup DSet(s) ELSE {},
       ![s].scanGen = [p \in Paths |-> sc[s].local[p]],
       \* `prev_scan = scanned.keys()`.  Read UNPRIMED above by DSet, so
       \* this scan classifies against the PREVIOUS walk and leaves its
       \* own behind — the whole content of "two consecutive scans".
       \*
       \* Gated on TwoScanDelete so it stays `{}` in every pre-existing
       \* cfg. Writing it unconditionally would add a varying component
       \* to `sc` and GROW all 55 earlier runs' state spaces — which
       \* costs wall clock for nothing and, worse, quietly falsifies the
       \* "earlier state spaces are preserved by construction" claim the
       \* rest of this harness is built on.
       ![s].prevScan = IF TwoScanDelete
                         THEN {q \in Paths : sc[s].local[q] # 0}
                         ELSE @,
       \* The pending set survived a lane pass: the durability/visibility
       \* split actually ACCUMULATED across ticks, which is the claim
       \* ProbeCitationInstalled has to make non-vacuous.
       ![s].stageCarried = \E q \in Paths : stage[s][q] # 0,
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {}]
  /\ window' = IF BarrierLease THEN window ELSE sc[s].epoch
  /\ gh' = [gh EXCEPT !.barriers = IF InfiniteBarriers THEN @ ELSE @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 hitlAcked, conflicts>>

(* Step 4: guarded per-key upload.  If-Match = the persisted baseline
   generation (If-None-Match:* for creates falls out as baseline = 0).
   The 412 arm: own/known generation => adopt (the crashed-PUT resume);
   foreign => park + surface, or LOCAL-WINS overwrite under the
   mutation.  EpochCheck rejects a deposed writer per-request — the
   syncer takes the rejection as deposal and fences.                   *)
UploadFenced(s) ==
  /\ ~GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ Fenced(s)               \* unreachable under BarrierLease: no holder uploads
  /\ sc[s].scanU \ (sc[s].upDone \cup sc[s].parked) # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, gh>>

(* Tranche 6: under the barrier lease the upload is a NON-HOLDER action —
   every PUT is If-Match on the base etag, S3's own optimistic lock, so
   it needs no lease and is never fenced; `Inv_NoDeposedPut` applies to
   the commit section only (protocol of record, straggler rules).       *)
Upload(s, p) ==
  /\ ~GatedCitation          \* gated replaces this with StagePut
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ p \in sc[s].scanU \ (sc[s].upDone \cup sc[s].parked)
  /\ ~Fenced(s)
  /\ LET cur == objects[p]
         want == sc[s].scanGen[p]
         \* Tranche 6: `upload_one`'s 412 arm adopts ONLY when the object
         \* already holds the bytes being uploaded (CRC match); any other
         \* generation it recognises is SUPERSEDED knowingly, If-Match on
         \* what the HEAD saw.  The life-lease arm below adopts any
         \* recognised generation and then cites the walk's bytes — a
         \* generation no object holds — which `Inv_NoDangling` cannot
         \* see with one writer (the object still exists) and TLC showed
         \* on the first two-writer strict run, once the OTHER writer's
         \* GC removed the adopted object.  The gated lane got this fix
         \* in product 1 x 2; the ungated arm keeps its coarse rule under
         \* the life lease so those state spaces are unchanged.
         supersede == BarrierLease /\ cur \in sc[s].known /\ cur # want
     IN
       IF cur = sc[s].baseline[p] \/ supersede
       THEN \* If-Match passes: the PUT lands
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.narrowRecited = @ \cup ({p} \cap gh.narrowed), !.deposedPuts =
                     @ + (IF DeposedHolder(s) THEN 1 ELSE 0),
                     !.hitlRetired = IF /\ MaxSameBytes > 0 /\ cur # want
                                        /\ cur \in sc[s].known /\ <<p, cur>> \in hitlAcked
                                     THEN @ \cup {<<p, cur>>} ELSE @,
                     \* the required-reachable overlap: this PUT landed
                     \* while another syncer was in its commit section
                     !.interleaved = @ \/ (BarrierLease /\ CellHeld /\ cellHolder # s)]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, inbox, removals,
                        window, hitlAcked, conflicts>>
       ELSE IF cur \in sc[s].known
       THEN \* 412, but the current ETag is one I minted or consumed:
            \* adopt (my own crashed/torn earlier PUT, or content already
            \* integrated).  AdoptOwn convergence.  Tranche 6: remembered,
            \* because nothing was PUT and the object is another writer's
            \* to GC until this barrier's CAS cites it.
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p},
                             ![s].adopted = IF BarrierLease THEN @ \cup {p} ELSE @]
         /\ gh' = [gh EXCEPT !.narrowRecited = @ \cup ({p} \cap gh.narrowed), !.adoptOwn = @ + 1]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                        inbox, removals, window, hitlAcked, conflicts>>
       ELSE IF ConflictSurfacing
       THEN \* foreign ETag: park the path, surface the conflict, never
            \* overwrite an ETag this syncer did not itself publish.
            \* Under Upload412Preserves the conflict record IS the preserved
            \* copy, and the upload then supersedes the foreign version
            \* knowingly — or parks, when that second PUT races (either).
         \/ /\ sc' = [sc EXCEPT ![s].parked = @ \cup {p}]
            /\ conflicts' = conflicts \cup {<<p, cur>>}
            /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                           inbox, removals, window, hitlAcked, gh>>
         \/ /\ Upload412Preserves
            /\ objects' = [objects EXCEPT ![p] = want]
            /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
            /\ conflicts' = conflicts \cup {<<p, cur>>}
            /\ gh' = [gh EXCEPT !.narrowRecited = @ \cup ({p} \cap gh.narrowed),
                        !.deposedPuts = @ + (IF DeposedHolder(s) THEN 1 ELSE 0),
                        !.interleaved = @ \/ (BarrierLease /\ CellHeld /\ cellHolder # s)]
            /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest,
                           inbox, removals, window, hitlAcked>>
       ELSE \* MUTATION: the inherited LOCAL-WINS arbitration — re-read
            \* the ETag and overwrite blind.  known does NOT grow.
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT
              !.amputated = @ \/ Destroys(s, p, cur),
              !.deposedPuts = @ + (IF DeposedHolder(s) THEN 1 ELSE 0)]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, inbox, removals,
                        window, hitlAcked, conflicts>>

(* Steps 5+6 in the chosen order.  The GC delete (etag-guarded HEAD:
   refuse any ETag the syncer does not recognize).  Under
   DeletesAfterCAS = FALSE (the v1 order) the deletes run BEFORE the
   CAS — the dangling-manifest mutation.  Tranche 6: the deletes are
   commit-section work in either order, so under the barrier lease the
   pre-CAS arm runs from the claim, not from the uploads.               *)
GCPhase(s) ==
  \/ (DeletesAfterCAS /\ sc[s].pc = "cased")
  \/ (~DeletesAfterCAS
      /\ (IF BarrierLease THEN sc[s].pc = "claimed" ELSE PreCommitReady(s)))

(* Tranche 6, ~ConditionalGC: the HEAD half of the shipped two-request
   GC.  Reads the object's generation and remembers it; the DELETE below
   is then guarded on what was SEEN, not on what is there.              *)
GCHead(s, p) ==
  /\ BarrierLease /\ ~ConditionalGC
  /\ Running(s) /\ GCPhase(s)
  /\ p \in sc[s].scanD \ (sc[s].gcDone \cup sc[s].gcHeaded)
  /\ sc' = [sc EXCEPT ![s].gcHeaded = @ \cup {p},
                      ![s].gcSeen = [@ EXCEPT ![p] = objects[p]]]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                 inbox, removals, window, hitlAcked, conflicts, gh>>

GCDelete(s, p) ==
  /\ Running(s)
  /\ GCPhase(s)
  /\ p \in sc[s].scanD \ sc[s].gcDone
  /\ ~Fenced(s)
  \* Two requests (the shipped shape) or one: with If-Match the guard and
  \* the delete are the same request and `cur` is what is there.
  /\ ConditionalGC \/ ~BarrierLease \/ p \in sc[s].gcHeaded
  /\ LET cur == IF ConditionalGC \/ ~BarrierLease THEN objects[p] ELSE sc[s].gcSeen[p]
         now == objects[p]
     IN
       \* v2 (deletes-after-CAS): the delete set is "keys the NEW manifest
       \* no longer references" — a key the merge re-cited (delete/modify
       \* resolved foreign-wins) is NOT garbage.  The v1 order cannot make
       \* this check (no new manifest yet) — that asymmetry is part of the
       \* defect the DeletesAfterCAS mutation pins.
       IF \/ cur = 0
          \/ GuardedGC /\ cur \notin sc[s].known
          \/ DeletesAfterCAS /\ manifest[p] # 0
       THEN \* already absent, still referenced, or unrecognized ETag
         /\ sc' = [sc EXCEPT ![s].gcDone = @ \cup {p}]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                        inbox, removals, window, hitlAcked, conflicts, gh>>
       ELSE
         /\ objects' = [objects EXCEPT ![p] = 0]
         /\ sc' = [sc EXCEPT ![s].gcDone = @ \cup {p},
                            ![s].gcTook = IF BaselineKeepsUncollected
                                          THEN @ \cup {p} ELSE @]
         /\ gh' = [gh EXCEPT !.gc = @ + 1,
              !.amputated = @ \/ Destroys(s, p, now),
              !.hitlRetired = IF /\ MaxSameBytes > 0 /\ now \in sc[s].known
                                 /\ <<p, now>> \in hitlAcked
                              THEN @ \cup {<<p, now>>} ELSE @]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, inbox, removals,
                        window, hitlAcked, conflicts>>

GCDeleteFenced(s) ==
  /\ Running(s) /\ Fenced(s)
  /\ GCPhase(s)
  /\ sc[s].scanD \ sc[s].gcDone # {}
  /\ sc' = FencedSc(s)
  /\ gh' = [gh EXCEPT !.abandoned = IF BarrierLease THEN 1 ELSE @]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

PreDeletesDone(s) ==
  /\ Running(s) /\ ~DeletesAfterCAS
  /\ (IF BarrierLease THEN sc[s].pc = "claimed" ELSE PreCommitReady(s))
  /\ sc[s].scanD \subseteq sc[s].gcDone
  /\ sc' = [sc EXCEPT ![s].pc = "delDone"]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, gh>>

(* Step 5: the manifest CAS.  Parked paths withhold their entry.  A
   merge-capable writer preserves foreign entries and queues them; the
   whole-rewrite writer (MergeCapable = FALSE) writes the local view and
   re-seeds on 412 — the amputation engine.  A deposed writer is caught
   by EpochCheck (per-request), by Rotation (seq mismatch => cell
   re-read => fence), or — with both arms off — LANDS: the straggler
   install.                                                             *)
CASReady(s) ==
  IF DeletesAfterCAS
  THEN (IF BarrierLease THEN sc[s].pc = "claimed" ELSE PreCommitReady(s))
  ELSE sc[s].pc = "delDone"

CASFenced(s) ==
  /\ Running(s) /\ CASReady(s)
  /\ Fenced(s)
  /\ sc' = FencedSc(s)
  /\ gh' = [gh EXCEPT !.stragglerCas = @ + 1,
                      !.abandoned = IF BarrierLease THEN 1 ELSE @]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* The commit section's OTHER exit (2026-09-15, W4 phase 2).  A request    *)
(* the commit makes is refused by the store — S3's 409 on the window-open  *)
(* PUT in the storm leg — so the barrier fails, releases the cell and      *)
(* keeps the pending sentinel; its uploads stand as uncited generations    *)
(* the next barrier adopts, exactly as a fenced barrier's do.  The syncer  *)
(* emits no trace event for this, only prose (`Publish honor failed`).     *)
AbandonBarrier(s) ==
  /\ AbandonOnStoreError /\ BarrierLease
  /\ Running(s) /\ sc[s].pc # "idle"
  /\ sc' = FencedSc(s)
  /\ gh' = [gh EXCEPT !.abandoned = 1]
  \* Holding the cell, the failed barrier hands it back; the failure can
  \* equally happen BEFORE the claim (an upload the store refused), and
  \* then there is nothing to release.
  /\ IF cellHolder = s /\ ~cellReleased
     THEN ReleaseCell /\ window' = 0
     ELSE UNCHANGED leaseVars /\ UNCHANGED window
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                 inbox, removals, hitlAcked, conflicts>>

CASMiss(s) ==
  /\ Running(s) /\ CASReady(s)
  /\ ~Fenced(s)
  /\ manSeq # sc[s].expSeq
  /\ IF DeposedHolder(s)
     THEN \* the 412 handler re-reads the cell and discovers deposal
       /\ sc' = FencedSc(s)
       /\ gh' = [gh EXCEPT !.stragglerCas = @ + 1,
                           !.abandoned = IF BarrierLease THEN 1 ELSE @]
       /\ UNCHANGED <<window, conflicts>>
       /\ UNCHANGED leaseVars
     ELSE IF MergeCapable
     THEN \* refresh the token and retry as a three-way merge
       /\ sc' = [sc EXCEPT ![s].expSeq = manSeq]
       /\ UNCHANGED <<window, conflicts, gh>>
       /\ UNCHANGED leaseVars
     ELSE \* the whole-rewrite writer: re-seed and FAIL the barrier;
          \* the next barrier overwrites from the local walk.  Tranche 6:
          \* a commit that fails for a non-fence reason RELEASES.
       /\ sc' = [sc EXCEPT ![s].expSeq = manSeq, ![s].pc = "idle",
            ![s].scanU = {}, ![s].scanD = {},
            ![s].scanGen = [p \in Paths |-> 0],
            ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {},
            ![s].gcHeaded = {}, ![s].gcSeen = [p \in Paths |-> 0],
            ![s].adopted = {}]
       /\ window' = 0
       /\ UNCHANGED <<conflicts, gh>>
       /\ IF BarrierLease THEN ReleaseCell ELSE UNCHANGED leaseVars
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 hitlAcked>>

CASInstall(s) ==
  /\ ~GatedCitation          \* gated replaces this with CitePassStep
  /\ Running(s) /\ CASReady(s)
  /\ ~Fenced(s)
  /\ CommitLoadsCurrent \/ manSeq = sc[s].expSeq
  /\ LET
       \* Merge semantics: base = instBase (the last-installed view), mine
       \* = the scan-time walk, theirs = the current bucket manifest.  The
       \* merge STARTS FROM THEIRS (untouched paths keep theirs' entry —
       \* writing the walk view instead betrays a foreign entry one
       \* barrier after preserving it, once Finish absorbs it into the
       \* base).  A foreign change (theirs # base) is PRESERVED —
       \* including over a local delete (delete/modify resolves
       \* conservative).  The REPAIR arm re-cites an object this syncer
       \* integrated (consume advanced baseline past the citation),
       \* guarded on the object still holding that generation — the
       \* implementation's citation-repair with its HEAD guard.
       \* An entry this incarnation RECOGNIZES is not a foreign change,
       \* whatever the merge base says.  Step 7 rewrites the merge base
       \* AND the baseline, so a restart between the manifest CAS and
       \* step 7 leaves both behind — and our own freshly installed
       \* entry then reads as somebody else's change at the next merge.
       \* delete/modify resolves conservatively, so the agent's delete
       \* is dropped from the boundary it is about to be acked for, and
       \* the path is queued into the inbox as a phantom conflict
       \* nobody else ever touched.  TLC found this in shipped code.
       \*
       \* `known` stands for two witnesses the implementation already
       \* has and neither of which step 7 can lose: the entry's `epoch`
       \* is the publishing writer's LEASE EPOCH, which is ours across
       \* a container restart (a HITL write carries 0, a successor a
       \* higher one); and the entry's etag can equal what our own
       \* baseline holds for the path.  Two routes, and the model
       \* produced one counterexample for each.
       foreign(p) == ForeignEntry(s, p)
       repair(p) == RepairOwed(s, p)
       \* The same candidate with its HEAD guard failed: the key moved past
       \* what this writer integrated.  The code declines silently ("the
       \* next consume reconciles it"); recorded for the ack's judgement.
       declined(p) == RepairDeclined(s, p)
       \* Tranche 6: an adopted entry is re-verified HERE, under the
       \* lease — the object must still hold the bytes the adopt found.
       \* If it does not, another writer's GC took it between the adopt
       \* and this CAS; the entry is withheld like a parked one and the
       \* path stays dirty for the next barrier to re-upload.
       \*
       \* Finding 13: an upload that LANDED is no safer.  The other
       \* writer's GC deletes If-Match an etag it recognises, and a real
       \* etag is the content hash — identical bytes PUT by this writer
       \* carry it too (`AgentWriteSame`).  So every citation this
       \* commit adds is re-verified, not only the adopted ones.
       gone == {p \in sc[s].adopted :
                  VerifyAdoptedCitations /\ objects[p] # sc[s].scanGen[p]}
               \cup
               {p \in (sc[s].scanU \cap sc[s].upDone) \ sc[s].adopted :
                  /\ BarrierLease /\ VerifyUploadedCitations
                  /\ objects[p] # sc[s].scanGen[p]}
       inst == [p \in Paths |->
         IF p \in sc[s].parked \cup gone THEN manifest[p]
         ELSE IF p \in sc[s].scanU \cap sc[s].upDone THEN sc[s].scanGen[p]
         ELSE IF repair(p) THEN sc[s].baseline[p]
         ELSE IF foreign(p) THEN manifest[p]
         ELSE IF p \in sc[s].scanD THEN 0
         ELSE IF MergeCapable THEN manifest[p]  \* start-from-theirs
         ELSE sc[s].scanGen[p]]                 \* whole-rewrite walk view
       \* Under the liveness abstraction the seq saturates (see the
       \* constant): with nothing to merge the token is never contended.
       \* Tranche 7: a merge equal to theirs installs NOTHING — no CAS, so
       \* no seq, no stamp, and the budget does not bind.
       nothing == EmptyInstall /\ inst = manifest
       seq2 == IF InfiniteBarriers \/ nothing THEN manSeq ELSE manSeq + 1
       foreignQ ==
         IF MergeCapable /\ InboxEnabled /\ ~WriterQueue
         THEN {<<p, manifest[p]>> : p \in
                {q \in Paths :
                   /\ q \notin sc[s].parked \cup (sc[s].scanU \cap sc[s].upDone)
                   /\ foreign(q)
                   /\ manifest[q] # 0}}
         ELSE {}
       inbox2 == inbox \cup foreignQ
       \* An install amputates when it drops the last tracked reference to
       \* an acked generation the installer NEVER LEARNED (pr[2] \notin
       \* known).  A syncer that consumed the write and then published a
       \* delete of it speaks for the workspace — that is integration
       \* followed by ordinary editing, not amputation.
       amp == \E pr \in hitlAcked :
                /\ (manifest[pr[1]] = pr[2]) \/ (pr \in inbox)
                /\ inst[pr[1]] # pr[2]
                /\ pr \notin inbox2
                /\ pr \notin conflicts
                /\ pr[2] \notin sc[s].known
       cite == \E pr \in hitlAcked : inst[pr[1]] = pr[2]
     IN
       /\ manSeq < MaxSeq \/ InfiniteBarriers \/ nothing
       /\ manifest' = inst
       /\ manSeq' = seq2
       /\ manSrc' = IF nothing THEN manSrc ELSE InstallSource(s)
       /\ inbox' = inbox2
       /\ window' = 0
       \* A withheld adoption leaves the path dirty (upDone loses it, so
       \* Finish does not advance its baseline) and a record behind.
       /\ sc' = [sc EXCEPT ![s].pc = "cased",
                           ![s].upDone = @ \ gone,
                           \* A delete the merge outranked (foreign(p) is
                           \* tested before scanD in `inst`): the install
                           \* still cites the path, so this boundary does
                           \* not carry the agent's delete.  The syncer's
                           \* `BarrierReport.outranked`; the 2026-09-14 box run
                           \* found the ok ack over it (Inv_AckImpliesCited,
                           \* depth 20).
                           ![s].citeDropped = IF AckHonest
                                              THEN {p \in sc[s].scanD : inst[p] # 0}
                                              ELSE @,
                           ![s].instSnap = inst, ![s].instSeq = seq2,
                           ![s].instSrc = IF ~BarrierLease THEN @
                                          ELSE IF nothing THEN manSrc
                                          ELSE InstallSource(s),
                           \* Tranche 7: the other writers' changes go to
                           \* THIS writer's queue, deletions included, in
                           \* the step that moves the merge base past them
                           \* (the code journals them with the CAS and
                           \* queues them at step 7; a restart between
                           \* re-queues from the journal).
                           ![s].fq = IF WriterQueue
                                     THEN QueueUpsert(@, MergeForeign(s) \cup MergeGone(s))
                                     ELSE @,
                           ![s].noInst = nothing,
                           ![s].repairMoved = IF BarrierLease
                                              THEN {p \in Paths : declined(p)}
                                              ELSE {}]
       /\ conflicts' = conflicts \cup {<<p, objects[p]>> : p \in gone}
       /\ gh' = [gh EXCEPT
            !.emptyInstalls = IF nothing THEN 1 ELSE @,
            !.adoptWithheld = IF gone \cap sc[s].adopted # {} THEN 1 ELSE @,
            !.uploadWithheld = IF gone \ sc[s].adopted # {} THEN 1 ELSE @,
            \* Finding 13's second route: a peer's PUT If-Match the same etag
            \* landed over our identical-bytes upload and the peer COMMITTED
            \* it; citing ours now replaces a citation the key holds with one
            \* it does not, and the peer's committed bytes go uncited.
            !.staleOverride = @ \/
               (BarrierLease /\ \E p \in (sc[s].scanU \cap sc[s].upDone) \ gone :
                  /\ objects[p] # sc[s].scanGen[p]
                  /\ objects[p] # 0
                  /\ manifest[p] = objects[p]),
            !.amputated = @ \/ amp,
            !.cited = IF cite THEN 1 ELSE @,
            !.citedPairs = @ \cup {pr \in hitlAcked : inst[pr[1]] = pr[2]},
            !.stragglerCas = @ + (IF DeposedHolder(s) THEN 1 ELSE 0),
            !.stragglerInstalls = @ + (IF DeposedHolder(s) THEN 1 ELSE 0)]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, objects, hitlAcked, removals>>

(* Step 7: rewrite the baseline, clear the barrier state.  The baseline
   advances ONLY for keys whose bytes this syncer integrated (its own
   uploads and its own landed deletes) — never for merge-preserved
   foreign entries, whose bytes arrive at the next Consume.  known
   likewise never absorbs cited-but-unintegrated generations.           *)
Finish(s) ==
  /\ Running(s) /\ sc[s].pc = "cased"
  /\ DeletesAfterCAS => sc[s].scanD \subseteq sc[s].gcDone
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       \* A barrier that BEGAN after the consume has now completed: this
       \* is the only thing that entitles an ok ack (D2's uniform rule).
       ![s].honored = IF SentinelEnabled /\ PendLive(s) THEN TRUE ELSE @,
       \* Tranche 7: a commit that installed nothing marks no boundary of
       \* its own (`note_boundary` is skipped), so its ack names no clock.
       ![s].installed = ~sc[s].noInst, ![s].noInst = FALSE,
       ![s].baseline = [p \in Paths |->
         IF p \in sc[s].scanU \cap sc[s].upDone THEN sc[s].scanGen[p]
         ELSE IF /\ p \in sc[s].scanD /\ sc[s].instSnap[p] = 0
                 /\ (~BaselineKeepsUncollected \/ p \in sc[s].gcTook)
              THEN 0
         ELSE @[p]],
       ![s].instBase = sc[s].instSnap,
       ![s].expSeq = sc[s].instSeq,
       \* The baseline records the walk's stat for what was cited.  A
       \* same-bytes rewrite AFTER the scan is absorbed here — an
       \* under-approximation, named: the next barrier's own
       \* `AgentWriteSame` reaches the same upload.
       ![s].touched = @ \ (sc[s].scanU \cap sc[s].upDone),
       ![s].scanU = {}, ![s].scanD = {},
       ![s].scanGen = [p \in Paths |-> 0],
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {},
       ![s].gcHeaded = {}, ![s].gcSeen = [p \in Paths |-> 0],
       ![s].adopted = {},
       ![s].declared = {}, ![s].consumed = {}]
  \* The window clear's CAS: what this barrier integrated leaves the
  \* cell now, after the manifest cites it — the consumed entries and
  \* the performed removals.
  /\ inbox' = inbox \ sc[s].consumed
  /\ removals' = removals \ sc[s].declared
  /\ gh' = [gh EXCEPT !.done = IF InfiniteBarriers THEN @ ELSE @ + 1]
  \* Tranche 6: the RELEASE is the last step of the commit section, so a
  \* crash anywhere between the CAS and here leaves the cell HELD by a
  \* dead holder — the deposal's whole subject.  A holder deposed after
  \* its CAS landed releases nothing: the cell is the successor's.
  /\ IF BarrierLease /\ ~Deposed(s) THEN ReleaseCell ELSE UNCHANGED leaseVars
  /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                 window, hitlAcked, conflicts>>

------------------------------------------------------------------------------
(* TRANCHE 6: the per-barrier lease — the cell as a FIFO ticket.           *)
(*                                                                          *)
(* Protocol of record: the writer-lease design §4 and the lease-v2 note.    *)
(* The cell is FRESH, HELD(holder, epoch, waiters) or RELEASED(last holder, *)
(* epoch, waiters, handoff).  A barrier claims AFTER its uploads and        *)
(* releases after its baseline; between barriers nobody holds anything.    *)
(* The abstractions are the life lease's: the 60 s quiet rule on a holder   *)
(* and the 20 s rule on a handoff are each ONE action enabled when the      *)
(* party is stalled or dead (the poll protocol itself is checked in         *)
(* flint's FlintTierEpoch.tla).                                             *)

(* The claim.  Three arms in one action — fresh, released-and-mine, and    *)
(* the DEPOSAL of a quiet holder — because they are one CAS on one object  *)
(* and differ only in whether the manifest is rotated: a released cell is  *)
(* a clean handoff, a deposal is the straggler fence (unchanged).  The     *)
(* window opens HERE, at the claim, not at the scan.                        *)
Claim(s) ==
  /\ BarrierLease /\ Running(s) /\ WantsCell(s)
  /\ ClaimEnabled(s)
  /\ LET deposal == CellHeld /\ cellHolder # s
         handoff == cellReleased /\ cellHandoff = s
         e == NextEpoch
     IN
       /\ (deposal /\ Rotation => manSeq < MaxSeq)
       /\ cellEpoch' = e /\ cellHolder' = s
       /\ cellReleased' = FALSE /\ cellHandoff' = "none"
       /\ cellQueue' = Without(cellQueue, s)
       \* The waiter list this holder carries to its release.
       /\ cellSeen' = IF HandoffAtClaim THEN Without(cellQueue, s) ELSE <<>>
       /\ window' = e
       /\ manSeq' = IF deposal /\ Rotation THEN manSeq + 1 ELSE manSeq
       /\ manSrc' = IF deposal /\ Rotation THEN "none" ELSE manSrc
       /\ sc' = [sc EXCEPT ![s].pc = "claimed", ![s].epoch = e]
       /\ gh' = [gh EXCEPT !.claimed = @ \cup {s},
                           !.handoffs = IF handoff THEN 1 ELSE @,
                           !.deposals = @ + (IF deposal THEN 1 ELSE 0)]
  /\ UNCHANGED <<manifest, objects, inbox, removals, hitlAcked, conflicts>>

(* A syncer that cannot claim takes a ticket: one CAS-append, only if     *)
(* absent.  A separate action so the queue is observable.  A waiter that  *)
(* was dropped as a quiet handoff and then thawed re-queues from here.     *)
Enqueue(s) ==
  /\ BarrierLease /\ Running(s) /\ WantsCell(s)
  /\ ~ClaimEnabled(s) /\ ~SkipEnabled(s)
  /\ sc[s].pc = "scanned" \/ ~InQueue(s)
  /\ cellQueue' = IF InQueue(s) THEN cellQueue ELSE Append(cellQueue, s)
  /\ sc' = [sc EXCEPT ![s].pc = "waiting"]
  /\ gh' = [gh EXCEPT !.enqueues = 1]
  /\ UNCHANGED <<cellSeen, cellEpoch, cellHolder, cellHandoff, cellReleased,
                 manSeq, manSrc, manifest, objects, inbox, removals, window,
                 hitlAcked, conflicts>>

(* The handoff is quiet (crashed, or frozen): after two polls anybody may *)
(* acquire the released cell and the named waiter is dropped.  Without    *)
(* this a crashed waiter wedges the cell for every survivor, forever.     *)
SkipDeadHandoff(s) ==
  /\ BarrierLease /\ Running(s) /\ WantsCell(s)
  /\ SkipEnabled(s)
  /\ LET e == NextEpoch IN
       /\ cellEpoch' = e /\ cellHolder' = s
       /\ cellReleased' = FALSE /\ cellHandoff' = "none"
       /\ cellQueue' = Without(cellQueue, s)
       /\ cellSeen' = IF HandoffAtClaim THEN Without(cellQueue, s) ELSE <<>>
       /\ window' = e
       /\ sc' = [sc EXCEPT ![s].pc = "claimed", ![s].epoch = e]
       /\ gh' = [gh EXCEPT !.claimed = @ \cup {s}, !.deadSkips = 1]
  /\ UNCHANGED <<manSeq, manSrc, manifest, objects, inbox, removals,
                 hitlAcked, conflicts>>


------------------------------------------------------------------------------
(* TRANCHE 2: the sync verb (v1, HITL) x the barrier.                       *)
(*                                                                          *)
(* Harness-invoked, never background, serialized against this syncer's own *)
(* barrier (hence pc = "idle").  Sync BEGINS WITH A FULL SCAN: "locally     *)
(* dirty" means dirty per THAT scan against the baseline, never per the     *)
(* last barrier's snapshot — otherwise sync honors a remote delete (or      *)
(* fetches a remote edit) over the agent's un-scanned latest work, which is *)
(* steady-state destruction of live work by the verb itself.  Policy:       *)
(* locally-dirty wins; remote changes apply only to locally-clean paths;    *)
(* every skipped apply is a surfaced conflict, never silent.                *)

Sync(s) ==
  /\ SyncEnabled
  /\ Running(s) /\ sc[s].pc = "idle"
  /\ gh.syncs < MaxSyncs
  \* The agent's scope (D4).  A whole-tree sync is the shipped verb and
  \* is modelled as scope = Paths; the sentinel's scoped form lets TLC
  \* choose any proper non-empty subset, which is stronger than fixing
  \* one — the loss this product exists to catch depends on WHICH path
  \* is left out.
  /\ \E scope \in (IF SyncScope
                   THEN {sub \in SUBSET Paths : sub # {} /\ sub # Paths}
                   ELSE {Paths}) :
     LET
       \* Ground truth, independent of the arm under test.
       trueDirty == Dirty(s)
       \* What THIS arm believes is dirty.
       dirt == IF SyncScanFirst THEN trueDirty ELSE sc[s].lastDirty
       \* Remote truth = the manifest, overlaid by live inbox entries (a
       \* HITL write no barrier has re-cited yet is still remote truth).
       remote(p) == IF \E pr \in inbox : pr[1] = p /\ objects[p] = pr[2]
                    THEN objects[p] ELSE manifest[p]
       changed == {p \in Paths : remote(p) # sc[s].instBase[p]}
       \* Out-of-scope remote changes are NOT integrated and NOT
       \* advanced: they stay foreign and reach this workspace through
       \* the normal merge -> inbox -> consume path at the next barrier.
       deferred == changed \ scope
       applicable == (changed \ dirt) \cap scope
       conflicted == (changed \cap dirt) \cap scope
       \* Which paths this sync is ENTITLED to advance the merge base
       \* for: the ones it applied, plus the ones it verified unchanged.
       \* A conflicted path is deliberately NOT advanced — its local
       \* bytes won, and the remote generation is still owed to us.
       advanced == applicable \cup (Paths \ changed)
       \* Paths whose manifest version the overlay HID from this sync.
       hidden == {p \in Paths : remote(p) # manifest[p]}
       keep(p) == SyncKeepsHiddenBase /\ p \in hidden
       newInstBase ==
         IF SyncScope /\ ScopedInstBase
         THEN [p \in Paths |->
                IF p \in advanced /\ ~keep(p) THEN manifest[p] ELSE sc[s].instBase[p]]
         ELSE [p \in Paths |-> IF keep(p) THEN sc[s].instBase[p] ELSE manifest[p]]
       \* THE LOSS STAMP.  The merge base moved for a path this sync
       \* neither applied nor surfaced a conflict for, and whose bytes
       \* we do not hold: we have just claimed to have integrated a
       \* generation we never saw.  `foreign(p)` is FALSE at every
       \* subsequent merge, so it is never queued again — silent and
       \* permanent.
       lost == \E p \in Paths :
                 /\ p \notin applicable
                 /\ p \notin conflicted
                 /\ newInstBase[p] # sc[s].instBase[p]
                 /\ sc[s].baseline[p] # remote(p)
       \* A DESTROYING apply: the path was TRULY dirty, sync moved its
       \* local content anyway (a remote fetch or a remote-delete), and
       \* no conflict was surfaced for it.  Under SyncScanFirst this is
       \* unreachable by construction; under the refuted arm it is the
       \* review's finding.
       destroys == \E p \in applicable :
                     /\ p \in trueDirty
                     /\ remote(p) # sc[s].local[p]
     IN
       /\ sc' = [sc EXCEPT
            ![s].local = [p \in Paths |->
              IF p \in applicable THEN remote(p) ELSE @[p]],
            ![s].baseline = [p \in Paths |->
              IF p \in applicable THEN remote(p) ELSE @[p]],
            ![s].instBase = newInstBase,
            ![s].touched = @ \ applicable,
            ![s].known = @ \cup {remote(p) : p \in applicable},
            ![s].lastDirty = {}]
       /\ conflicts' = conflicts \cup {<<p, remote(p)>> : p \in conflicted}
       /\ gh' = [gh EXCEPT !.syncs = @ + 1,
            !.syncApplied = @ + Cardinality(applicable),
            !.syncConflicts = @ + Cardinality(conflicted),
            !.syncDestroyed = @ \/ destroys,
            !.scopedDeferrals = @ + (IF SyncScope THEN Cardinality(deferred) ELSE 0),
            !.deferredPaths = @ \cup (IF SyncScope THEN deferred ELSE {}),
            !.foreignLost = @ \/ lost]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked>>

------------------------------------------------------------------------------
(* TRANCHE 3, PRODUCT 2: gated manifest advance — durability split from     *)
(* visibility (boundary-verbs D6/D7/D8/D13).                                *)
(*                                                                          *)
(* Two lanes instead of one fused barrier:                                  *)
(*   - the UPLOAD LANE puts in place, minting a new VERSION and citing       *)
(*     nothing.  The cited generation survives as a noncurrent version —     *)
(*     that is the premise the whole design rests on, and it is what makes   *)
(*     `objects[p]` (what the key reads as) a different question from        *)
(*     `versions[p]` (what is still fetchable).                              *)
(*   - the CITATION LANE installs the entire pending set in ONE CAS, then    *)
(*     applies the withheld deletes and runs the EXACT version reaper.       *)
(*                                                                          *)
(* The lane opens no HITL window; window open/clear belong to the citation.  *)
(* Modelled abstractions, named rather than assumed: the citation and its    *)
(* reaper are one step (the real code holds the HITL window across both, so  *)
(* no foreign write can interleave), and the four citation SOURCES collapse  *)
(* to nondeterminism — a citation is enabled whenever the stage is           *)
(* non-empty, which is strictly more permissive than any of them.            *)

Staged(s) == {p \in Paths : stage[s][p] # 0}

\* D7's re-validation: a staged entry is still installable only if the
\* baseline it staged against has not moved under it.
(* Which staged entries this citation may install.
   D7 ALSO specifies a base-version re-validation — drop a staged entry
   whose BASELINE moved under it.  That guard is deliberately NOT modelled
   as an arm, because the model showed it is UNREACHABLE given the lane's
   own discipline: the lane never advances the baseline, so a staged path
   is by construction locally-dirty, and every route that could move a
   baseline (consume, sync) refuses dirty paths and surfaces a conflict
   instead.  It stays in the implementation as defence in depth; what the
   model says is that it is not what protects anything today, and the
   hazard it was written for arrives by a route it cannot see. *)
Valid(s) ==
  IF CiteDropsInflightHitl
  THEN {p \in Staged(s) : ~\E pr \in inbox : pr[1] = p}
  ELSE Staged(s)

(* The upload lane.  Same guard chain as Upload — If-Match on the
   recognized baseline, AdoptOwn on a known ETag, park on a foreign one —
   but the PUT lands as a new VERSION and no manifest CAS runs.          *)
StagePut(s, p) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ p \in sc[s].scanU \ (sc[s].upDone \cup sc[s].parked)
  /\ ~(EpochCheck /\ Deposed(s))
  /\ LET cur == objects[p]
         want == sc[s].scanGen[p]
     IN
       IF cur = sc[s].baseline[p]
       THEN
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ versions' = [versions EXCEPT ![p] = @ \cup {want}]
         /\ stage' = [stage EXCEPT ![s] = [@ EXCEPT ![p] = want]]
         /\ stageBase' = [stageBase EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.staged = @ + 1,
                     !.deposedPuts = @ + (IF Deposed(s) THEN 1 ELSE 0)]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, inbox, removals,
                        window, withheldDel, hitlAcked, conflicts>>
       ELSE IF cur \in sc[s].known /\ cur = want
       THEN \* our own crashed/torn earlier PUT of THESE bytes: adopt the
            \* current version as the staged one.  The equality matters
            \* and the model was coarser than the code without it —
            \* `upload_one`'s 412 arm adopts only when the object's CRC
            \* is the CRC of the body it is uploading, and otherwise
            \* SUPERSEDES it knowingly (the arm below).  Adopting any
            \* recognized generation stages bytes the agent has already
            \* replaced, which reads as a boundary that lost the
            \* agent's declared write — a counterexample the
            \* implementation cannot produce.
         /\ stage' = [stage EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ stageBase' = [stageBase EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.adoptOwn = @ + 1]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                        versions, inbox, removals, window, withheldDel, hitlAcked,
                        conflicts>>
       ELSE IF cur \in sc[s].known
       THEN \* our own earlier PUT, OLDER content: supersede it knowingly
            \* (If-Match on what we recognize), which is what the shipped
            \* arm does rather than citing the stale generation.
         /\ objects' = [objects EXCEPT ![p] = want]
         /\ versions' = [versions EXCEPT ![p] = @ \cup {want}]
         /\ stage' = [stage EXCEPT ![s] = [@ EXCEPT ![p] = want]]
         /\ stageBase' = [stageBase EXCEPT ![s] = [@ EXCEPT ![p] = cur]]
         /\ sc' = [sc EXCEPT ![s].upDone = @ \cup {p}]
         /\ gh' = [gh EXCEPT !.adoptOwn = @ + 1,
                     !.deposedPuts = @ + (IF Deposed(s) THEN 1 ELSE 0)]
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, inbox, removals,
                        window, withheldDel, hitlAcked, conflicts>>
       ELSE \* foreign ETag: park and surface; never overwrite.
         /\ sc' = [sc EXCEPT ![s].parked = @ \cup {p}]
         /\ conflicts' = conflicts \cup {<<p, cur>>}
         /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects,
                        versions, inbox, removals, window, stage, stageBase,
                        withheldDel, hitlAcked, gh>>

StagePutFenced(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ EpochCheck /\ Deposed(s)
  /\ sc[s].scanU \ (sc[s].upDone \cup sc[s].parked) # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, versions,
                 inbox, removals, window, stage, stageBase, withheldDel, hitlAcked,
                 conflicts, gh>>

(* The lane ends.  Deletes are WITHHELD — a rename r->s must not become
   reader-visible as r-gone/s-absent at a point nobody declared.

   ...and a withheld delete CANCELS any version this stage was still
   holding for that path.  The stage and the tombstone set carry no
   ordering between them, so a citation handed both installs one by
   accident of merge order: the shipped merge applied deletes last
   (right for create-then-delete, which amputates a re-created file),
   and making upserts win instead cites a file the agent deleted.  Only
   the lane knows which it saw last, so the lane is where they cancel.
   TLC found this the first time the sentinel and the citation lane ran
   in one world — against a fix two hours old.                          *)
LaneDone(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "scanned"
  /\ sc[s].scanU \subseteq (sc[s].upDone \cup sc[s].parked)
  /\ withheldDel' = [withheldDel EXCEPT ![s] = @ \cup sc[s].scanD]
  /\ stage' = [stage EXCEPT ![s] =
       [p \in Paths |-> IF LaneCancelsStaged /\ p \in sc[s].scanD THEN 0 ELSE @[p]]]
  /\ stageBase' = [stageBase EXCEPT ![s] =
       [p \in Paths |-> IF LaneCancelsStaged /\ p \in sc[s].scanD THEN 0 ELSE @[p]]]
  /\ sc' = [sc EXCEPT ![s].pc = "laneDone"]
  /\ gh' = [gh EXCEPT !.withheld = @ + Cardinality(sc[s].scanD \ withheldDel[s])]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, versions,
                 inbox, removals, window, hitlAcked, conflicts>>

(* No coherent point is due: the tick ends with the bytes DURABLE and the
   manifest un-advanced, and the pending set survives to the next lane.
   This is the mode working, and it is the state every invariant in this
   product has to hold in.                                              *)
LaneOnly(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ sc[s].citeDone = {}
  /\ window' = 0
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       ![s].scanU = {}, ![s].scanD = {},
       ![s].scanGen = [p \in Paths |-> 0],
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {}]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, versions,
                 inbox, removals, stage, stageBase, withheldDel, hitlAcked, conflicts,
                 gh>>

CiteFenced(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ EpochCheck /\ Deposed(s)
  /\ Valid(s) \ sc[s].citeDone # {}
  /\ sc' = [sc EXCEPT ![s].st = "dead"]
  /\ gh' = [gh EXCEPT !.stragglerCas = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, versions,
                 inbox, removals, window, stage, stageBase, withheldDel, hitlAcked,
                 conflicts>>

(* THE citation.  Under AtomicCitation it installs the whole valid
   pending set in one CAS — the versions already exist, so there is no
   copy phase and no half-boundary.  Under the mutation TLC may install
   any non-empty subset, which is what a two-CAS design looks like from
   a reader's side.                                                     *)
CitePassStep(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ ~(EpochCheck /\ Deposed(s))
  /\ manSeq = sc[s].expSeq
  /\ manSeq < MaxSeq
  /\ Valid(s) \ sc[s].citeDone # {}
  /\ \E sub \in SUBSET (Valid(s) \ sc[s].citeDone) :
       /\ sub # {}
       /\ AtomicCitation => sub = Valid(s) \ sc[s].citeDone
       /\ LET \* §2.4.2's ungated repair, in the lane that never had it.
              \* Same shape and same guard as the fused barrier's arm
              \* (`repair(p)`, in Finish): the object must still hold
              \* the generation this workspace integrated, or the
              \* citation would name bytes it never saw.
              repairG(p) ==
                /\ GatedRepair
                /\ p \notin sub
                /\ sc[s].baseline[p] # sc[s].instBase[p]
                /\ objects[p] = sc[s].baseline[p]
              inst == [p \in Paths |->
                IF p \in sub THEN stage[s][p]
                ELSE IF repairG(p) THEN sc[s].baseline[p]
                ELSE manifest[p]]
              \* This citation names a generation OTHER than the acked
              \* user bytes the key currently holds, and says nothing
              \* about it.  Not "the agent integrated the user's bytes
              \* and then edited" — `known` exempts that case throughout
              \* this model — but work that PREDATES the user's write
              \* winning against it.  Two arms reach it, and each has its
              \* own guard: the write was consumed after we staged (D7's
              \* base re-validation), or it is still in flight in the
              \* inbox (the window the lane deliberately does not open).
              staleWin == \E p \in sub :
                /\ <<p, objects[p]>> \in hitlAcked
                /\ objects[p] # stage[s][p]
                /\ <<p, objects[p]>> \notin conflicts
              forced == \E p \in sub : sc[s].local[p] # stage[s][p]
          IN
            /\ manifest' = inst
            /\ manSeq' = manSeq + 1
            /\ manSrc' = InstallSource(s)
            /\ sc' = [sc EXCEPT ![s].citeDone = @ \cup sub,
                                ![s].expSeq = manSeq + 1,
                                ![s].instSnap = inst,
                                ![s].instSeq = manSeq + 1]
            /\ gh' = [gh EXCEPT
                 !.cites = @ + 1,
                 !.amputated = @ \/ staleWin,
                 !.forcedCites = @ + (IF forced THEN 1 ELSE 0),
                 !.citeSpan = IF Cardinality(sub) > @ THEN Cardinality(sub) ELSE @,
                 !.carriedCite = @ \/ (sc[s].stageCarried /\ Cardinality(sub) > 1),
                 !.stragglerCas = @ + (IF Deposed(s) THEN 1 ELSE 0),
                 !.stragglerInstalls = @ + (IF Deposed(s) THEN 1 ELSE 0)]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, objects, versions, inbox, removals, window,
                 stage, stageBase, withheldDel, hitlAcked, conflicts>>

(* The citation completes: withheld deletes land WITH it, the EXACT
   version reaper runs, the baseline advances, the window clears.

   The reaper is flint's own and it is the ONLY reaper that can tell
   cited from uncited.  Lifecycle cannot do this job on `files/`: gated
   staging makes the cited version noncurrent the moment a newer
   generation is staged, so a NoncurrentVersionExpiration rule runs a
   clock against live cited data and never reaches the newest uncited
   bytes, which are current.  That inversion is BackstopExpire.         *)
CiteFinish(s) ==
  /\ GatedCitation
  /\ Running(s) /\ sc[s].pc = "laneDone"
  /\ sc[s].citeDone # {} /\ sc[s].citeDone = Valid(s)
  \* Two different questions, and the shipped code answers them in two
  \* different places: may this boundary UNCITE the path (the manifest
  \* CAS — yes, the agent deleted it), and may it DELETE THE OBJECT (the
  \* GC, which HEADs first and refuses an etag it does not recognize).
  \* Guarding the uncite with the GC's guard, as this model did, means a
  \* foreign write to a path the agent deleted keeps the path CITED —
  \* and an ok ack for a boundary that declared it gone.  TLC reported
  \* that as a defect; the defect was in the model.
  /\ LET uncite == {p \in withheldDel[s] :
                      /\ manifest[p] # 0
                      /\ p \notin sc[s].citeDone}
         dels == {p \in uncite : ~GuardedGC \/ objects[p] \in sc[s].known}
         man2 == [p \in Paths |-> IF p \in uncite THEN 0 ELSE manifest[p]]
         obj2 == [p \in Paths |-> IF p \in dels THEN 0 ELSE objects[p]]
         \* The reaper runs over the paths this citation INSTALLED and
         \* keeps what the INSTALLED DOCUMENT cites — never the writer's
         \* own idea of what it cited.
         scope == sc[s].citeDone
         \* ...plus the CURRENT version, unconditionally.  If current is
         \* not what we just cited then a foreign write landed between
         \* the lane and this citation: live bytes somebody is about to
         \* read, not a generation this workspace superseded.  THE MODEL
         \* FOUND THIS — the rule without this clause deleted an acked
         \* HITL write in shipped code.
         current(p) == IF GCKeepsCurrent /\ obj2[p] # 0 THEN {obj2[p]} ELSE {}
         ver2 == [p \in Paths |->
                    IF p \in scope /\ man2[p] # 0
                    \* FAIL CLOSED: if the installed manifest names no
                    \* version for this path we do not know what is
                    \* cited, and "delete everything unrecognized" would
                    \* reap live data.  Reclaim nothing.
                    THEN (versions[p] \cap {man2[p]}) \cup current(p)
                    ELSE versions[p]]
         dropped == Staged(s) \ Valid(s)
     IN
       /\ manifest' = man2
       /\ objects' = obj2
       /\ versions' = ver2
       /\ stage' = [stage EXCEPT ![s] = [p \in Paths |-> 0]]
       /\ stageBase' = [stageBase EXCEPT ![s] = [p \in Paths |-> 0]]
       /\ withheldDel' = [withheldDel EXCEPT ![s] = @ \ uncite]
       /\ window' = 0
       \* A dropped staged generation is never silently forgotten.
       /\ conflicts' = conflicts \cup {<<p, sc[s].baseline[p]>> : p \in dropped}
       /\ sc' = [sc EXCEPT ![s].pc = "idle",
            ![s].honored = IF SentinelEnabled /\ PendLive(s) THEN TRUE ELSE @,
            ![s].installed = TRUE,
            \* Which paths this boundary does NOT carry.  Kept apart
            \* from `conflicts` on purpose: a conflict record is the
            \* ack's `report.parked` in the FUSED path, which is why
            \* BoundaryBroken exempts it — a correspondence the gated
            \* honor cannot maintain, because the drop happens inside
            \* the citation and the honor writes one ack for the lot.
            ![s].citeDropped = dropped,
            ![s].citeDone = {}, ![s].stageCarried = FALSE,
            ![s].instBase = man2,
            ![s].baseline = [p \in Paths |->
              IF p \in sc[s].citeDone THEN stage[s][p]
              ELSE IF p \in uncite THEN 0
              ELSE @[p]],
            ![s].known = @ \cup {stage[s][p] : p \in sc[s].citeDone},
            ![s].scanU = {}, ![s].scanD = {},
            ![s].scanGen = [p \in Paths |-> 0],
            ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {},
            ![s].declared = {}, ![s].consumed = {}]
       /\ inbox' = inbox \ sc[s].consumed
       /\ removals' = removals \ sc[s].declared
       /\ gh' = [gh EXCEPT !.done = @ + 1,
            !.gc = @ + Cardinality(dels),
            !.gcCited = @ + Cardinality(dels),
            !.reaped = @ + Cardinality(UNION {versions[p] \ ver2[p] : p \in Paths}),
            !.cited = IF \E pr \in hitlAcked : man2[pr[1]] = pr[2]
                      THEN 1 ELSE @,
            !.citedPairs = @ \cup {pr \in hitlAcked : man2[pr[1]] = pr[2]},
            !.declaredDrops = @ + (IF SentinelEnabled /\ PendLive(s)
                                   THEN Cardinality(dropped \cap sc[s].pendDirty)
                                   ELSE 0)]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, hitlAcked>>

(* The noncurrent-retention BACKSTOP.  It is not the reaper and it must
   never be creditable for the reaper's work: on `files/` it cannot tell
   cited from uncited, so when a workspace is abandoned mid-stage it
   reaps the CITED (now noncurrent) version while the uncited current
   one survives.  The manifest then dangles — checkout refuses rather
   than serving a hole, and `recover-staged` re-cites the survivor
   FORWARD.  Enabling this action is a mutation, and the counterexample
   it must find IS the abandoned-mid-stage endgame.                     *)
BackstopExpire(p) ==
  /\ GatedCitation /\ BackstopEnabled
  /\ \E g \in versions[p] :
       /\ g # objects[p]          \* noncurrent only, exactly as S3
       /\ versions' = [versions EXCEPT ![p] = @ \ {g}]
  /\ gh' = [gh EXCEPT !.reaped = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, sc, stage, stageBase, withheldDel, hitlAcked,
                 conflicts>>

GatedNext ==
  \/ \E s \in Syncers, p \in Paths : StagePut(s, p)
  \/ \E s \in Syncers :
       StagePutFenced(s) \/ LaneDone(s) \/ LaneOnly(s) \/ CiteFenced(s)
       \/ CitePassStep(s) \/ CiteFinish(s) \/ CrashPodGated(s)
  \/ \E p \in Paths : BackstopExpire(p)

------------------------------------------------------------------------------
(* TRANCHE 3, PRODUCT 1: the boundary VERB x the barrier x the inbox.      *)
(*                                                                         *)
(* The agent declares a coherent point by touching `.flint/publish`; the   *)
(* syncer consumes it (rename into its own state dir), honors it with a   *)
(* real barrier, writes `.flint/publish.ack`, and retires the pending      *)
(* record.  Four steps, each of which a crash, a restart or a deposal can  *)
(* land between, and all of them racing the inbox and the manifest CAS.    *)
(*                                                                         *)
(* Modelled abstractions, named rather than assumed:                       *)
(*   - The two verbs collapse to ONE.  `sync` differs in what its honor    *)
(*     does (tranche 2/product 4 model that) and not in the consume /      *)
(*     honor / ack / retire protocol, which is what this product searches. *)
(*   - A touch id doubles as the nonce and as the sentinel's mtime clock:  *)
(*     ids are monotone, so "the ack's covered mtime is not older than the *)
(*     pending's" is implied by the nonce subset test in `AckMatches`.     *)
(*   - The bare touch (a sentinel with no nonce) is not modelled; every    *)
(*     touch carries one, which is what lets `pendN # {}` stand in for     *)
(*     "the pending file exists".  The torn-body rule is a battery leg.    *)
(*   - The min-interval and the hourly work budget are OUT of the safety   *)
(*     gate (the house rule for rate limiting): a deferred honor is        *)
(*     modelled as a later honor, which is strictly more permissive.       *)
(*   - The refusal (write the refused ack, retire, flip the marker, exit)  *)
(*     is one step, as is the ack rename.  The crash point that matters —  *)
(*     between the ack and the retire — is modelled, because it is the one *)
(*     the retracted crash matrix got wrong.                               *)

(* The agent's declaration.  A second touch overwrites an unconsumed
   sentinel: one file, one body.  That is the agent's own doing and not
   an orphan — the protocol owes an ack for nonces it CONSUMED.         *)
Touch(s) ==
  /\ SentinelEnabled /\ Running(s)
  /\ gh.touches < MaxTouches
  /\ sc' = [sc EXCEPT ![s].sentTok = gh.touches + 1]
  /\ gh' = [gh EXCEPT !.touches = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* The consume: rename the sentinel out of the agent's reach and FOLD it
   into the standing pending record (D2.1).  Folding rather than
   overwriting is the whole rule — a rename onto a live pending record
   clobbers its nonces, and the agents behind them wait forever.

   `honored` is cleared unconditionally: D1's guarantee is that the
   barrier which acknowledges a sentinel BEGINS ITS SCAN strictly after
   consuming it, so a barrier that completed before this consume
   entitles nothing.                                                    *)
TakeSentinel(s) ==
  /\ SentinelEnabled /\ Running(s)
  /\ sc[s].sentTok # 0
  /\ sc[s].pc = "idle"
  /\ LET t == sc[s].sentTok
         fold == FoldPending /\ PendLive(s)
     IN
       /\ sc' = [sc EXCEPT
            ![s].sentTok = 0,
            ![s].pendN = IF fold THEN @ \cup {t} ELSE {t},
            ![s].pendCov = sc[s].local,
            ![s].pendMint = gh.nextGen,
            ![s].pendDirty = Dirty(s),
            ![s].honored = FALSE,
            ![s].owed = @ \cup {t}]
       /\ gh' = [gh EXCEPT !.coalesced = @ + (IF fold THEN 1 ELSE 0)]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* Skip-on-no-diff (`barrier.rs`): nothing local to publish, no citation
   repair owed, and the remote manifest document where we left it — so
   every local byte is already cited and the barrier returns without a
   window, without a CAS, and without touching the manifest.
   §2.1 prescribes that a pending sentinel DEFEAT this fast path;
   §10.1 records why the shipped code deliberately does not (§7 prices a
   no-diff honor at one HEAD, and defeating it would cost a manifest CAS
   at up to 720/hour/workspace — the exact amplification the budget
   exists to prevent).  That deviation rests on an argument in a
   document.  `FastPathGuards` is that argument, machine-checked: with
   the guards on the strict runs must hold, and with the last two
   dropped the ack must be caught claiming a boundary that is not
   installed.                                                           *)
FastPathClean(s) ==
  /\ USet(s) = {} /\ DSet(s) = {}
  /\ DeclaredConfirmsAbsence => {p \in Dirty(s) : sc[s].local[p] = 0} = {}
  /\ (~FastPathGuards \/ \A p \in Paths : sc[s].baseline[p] = sc[s].instBase[p])
  /\ (~FastPathGuards \/ manSeq = sc[s].expSeq)

FastPath(s) ==
  \* Tranche 7: the shipped skip-on-no-diff runs on EVERY barrier, not only
  \* a sentinel honor — and it also requires that the consume took nothing
  \* from the shared inbox (`consumed.is_empty()`), stale entries included.
  /\ SentinelEnabled \/ (BarrierLease /\ EmptyInstall)
  /\ EmptyInstall => sc[s].consumed = {}
  /\ ~GatedCitation
  /\ Running(s) /\ sc[s].pc = "consumed"
  \* A no-diff pass IS a barrier tick and charges the barrier budget.
  \* Not bookkeeping: without it Consume -> FastPath -> Consume is a
  \* free cycle that consumes nothing, and the state graph's DIAMETER
  \* grows without bound (the pilot ran to depth 148 and 17M states
  \* before this line existed).
  /\ gh.barriers < MaxBarriers \/ InfiniteBarriers
  /\ FastPathClean(s)
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       ![s].honored = IF SentinelEnabled /\ PendLive(s) THEN TRUE ELSE @,
       \* No boundary was installed, so this honor stamps nothing.
       ![s].installed = FALSE,
       ![s].prevScan = IF DeclaredConfirmsAbsence /\ TwoScanDelete
                       THEN {q \in Paths : sc[s].local[q] # 0} ELSE @,
       ![s].lastDirty = IF SyncEnabled THEN {} ELSE @]
  /\ gh' = [gh EXCEPT !.barriers = IF InfiniteBarriers THEN @ ELSE @ + 1,
                      !.fastPaths = IF InfiniteBarriers THEN @ ELSE @ + 1,
                      !.fastHonor = @ \/ PendLive(s)]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* Tranche 7: the PULL-ONLY boundary.  The barrier found nothing of its
   own to publish and the manifest moved (else the fast path took it), so
   the merge can only add nothing: queue the other writers' changes and
   take theirs as the merge base.  No claim, no window, no CAS — the code
   returns before its commit section, which is what took 65 of 191 claims
   out of the writers drill's cell queue.  A sentinel it honors is
   answered against theirs: the ack names `theirs.seq`.                  *)
PullOnly(s) ==
  /\ Running(s) /\ PullOnlyReady(s)
  /\ sc' = [sc EXCEPT ![s].pc = "idle",
       ![s].fq = QueueUpsert(@, MergeForeign(s) \cup MergeGone(s)),
       ![s].instBase = manifest,
       ![s].expSeq = manSeq,
       ![s].instSnap = manifest, ![s].instSeq = manSeq, ![s].instSrc = manSrc,
       ![s].repairMoved = {p \in Paths : RepairDeclined(s, p)},
       ![s].citeDropped = IF AckHonest THEN {} ELSE @,
       ![s].honored = IF SentinelEnabled /\ PendLive(s) THEN TRUE ELSE @,
       ![s].installed = FALSE,
       ![s].scanU = {}, ![s].scanD = {},
       ![s].scanGen = [p \in Paths |-> 0],
       ![s].upDone = {}, ![s].parked = {}, ![s].gcDone = {}, ![s].gcTook = {},
       ![s].gcHeaded = {}, ![s].gcSeen = [p \in Paths |-> 0],
       ![s].adopted = {},
       ![s].declared = {}, ![s].consumed = {}]
  /\ gh' = [gh EXCEPT !.pullOnlys = 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* THE PROMISE, evaluated at the instant the ack is written and stamped
   into a ghost (the house's action-written rule).  D1: "everything
   ordered-before T is a coherent point; publish it", with the at-least
   guarantee stated at CONSUME time — so every path whose consume-time
   content is still the tree's content must be cited by the manifest the
   ack names.  Two exemptions, both of them the protocol working:

     - the AGENT itself moved the path after the consume, so its
       declared bytes are superseded rather than owed.  A DELETE is
       the case that needs this clause on its own: it supersedes
       without minting anything, so the watermark below cannot see it,
       and TLC's third counterexample was exactly an agent deleting
       its own declared file after declaring it.
     - the citation names a generation MINTED AFTER the declaration.
       D1's guarantee is at-LEAST: "the published state may include
       later bytes for a racing file, never earlier ones".  Stating the
       promise as snapshot equality is WRONG, and TLC said so on the
       first strict run of this product — its counterexample was an
       agent that deleted a path, declared, re-created it, let the
       barrier publish the re-creation, then deleted it again, so the
       consume-time snapshot matched the tree again at ack time while
       the manifest legitimately cited later bytes.  The mint watermark
       is what distinguishes "later" from "stale", and it is why
       `pendMint` exists.
     - the path carries a surfaced conflict record — it was answered
       loudly, which is `report.parked` in the ack the implementation
       writes.  "Never a silent winner" is the standing rule; a silent
       LOSER is what this invariant is looking for.

   The two exemptions overlap and neither subsumes the other, which is
   why both are here and each has a counterexample behind it: an agent
   can move a path away and back (write, delete, write) so that the
   tree matches the declaration again at ack time while the manifest
   legitimately cites later bytes — the watermark is what covers that —
   and it can supersede by deleting, which mints nothing at all — the
   tree comparison is what covers that.  What neither covers is an
   agent restoring byte-identical content, which this model cannot
   express (mints are unique) and a real filesystem can: ledger entry,
   and harmless, because the boundary then names bytes equal to the
   declared ones.

   Note what the conflict clause does NOT excuse: a citation of a
   generation minted BEFORE the declaration and different from the
   declared one is stale whichever direction it points — an unpublished
   write (manifest older than the agent's bytes) and an unpublished
   DELETE (the agent declared the path gone, the manifest still cites
   its old generation) are the same defect and the same test.          *)
BoundaryBroken(s) ==
  \E p \in sc[s].pendDirty :
    /\ AckedDoc(s)[p] # sc[s].pendCov[p]
    /\ AckedDoc(s)[p] < sc[s].pendMint
    /\ sc[s].local[p] = sc[s].pendCov[p]
    \* The conflict exemption reads the ack's `report.parked`.  A path
    \* the CITATION dropped is in no such field — the gated honor
    \* writes one ack for the whole boundary — so its record excuses
    \* nothing here.  That is the exemption holding the finding's own
    \* combined world vacuously green until it was narrowed.  Note the
    \* shape: the drop DEFEATS the exemption, it does not exclude the
    \* path from the search.  Writing it as a plain conjunct excuses
    \* exactly the case the clause exists to catch.
    /\ (p \in sc[s].citeDropped \/ ~\E pr \in conflicts : pr[1] = p)

(* The OTHER half of what an ok ack asserts, and it is not the same
   claim.  `BoundaryBroken` asks whether the agent's own declared work
   survived; this asks whether the point the ack names is a coherent
   one AT ALL: every generation this workspace has integrated — its
   persisted baseline, which includes the inbox writes it adopted and
   the foreign entries it merged — is cited by the manifest the ack
   points at.  When it is not, a reader resolving that manifest gets
   bytes this workspace has already superseded, and a re-checkout would
   MATERIALIZE them over the newer ones.

   This is what `repairs_pending` defends in the shipped fast path, and
   why the two halves need separate invariants: TLC's second
   counterexample was an inbox adoption whose citation repair was still
   owed — no work of the agent's was at risk, and the boundary was
   still not the point the ack claimed.  Parked paths are exempt:
   their conflict record is the ack's `report.parked`.               *)
BoundaryIncoherent(s) ==
  \E p \in Paths :
    /\ sc[s].baseline[p] # AckedDoc(s)[p]
    /\ ~\E pr \in conflicts : pr[1] = p
    \* Under the barrier lease a PEER's lease-free upload can supersede, at
    \* the key, a generation this writer integrated before its commit
    \* re-cited it, and the repair's HEAD guard then rightly declines.  The
    \* harm this stamp names cannot follow: a reader of the acked document
    \* is sent to a key that holds neither the superseded generation nor
    \* the cited one, so its conditional read fails into the NEWER object.
    \* Exempt exactly the paths the install declined for that reason, at
    \* that CAS — a repair skipped while the key still held the integrated
    \* generation (the fast-path finding) is not exempt.  The first run of
    \* the two-writer sentinel world with HitlOverwritesTrackedOnly found
    \* this in 16 steps (README, tranche 6).
    /\ p \notin sc[s].repairMoved
    \* Tranche 7, the third refinement: the acked document is AHEAD of the
    \* tree at p by exactly a change another writer made that waits in THIS
    \* writer's queue for its next consume.  A reader of that document gets
    \* the newer bytes, not bytes this workspace superseded, so the stamp's
    \* harm runs the other way (the 2026-09-13 box run's depth-19 trace).
    \* Only the queued generation is excused: the doc ahead by anything the
    \* queue does not hold still fires, and the QueueForeignChanges mutation
    \* is the run that proves it.
    /\ ~(WriterQueue /\ <<p, AckedDoc(s)[p]>> \in sc[s].fq)

AckOk(s) ==
  /\ SentinelEnabled /\ Running(s) /\ sc[s].pc = "idle"
  /\ PendLive(s) /\ ~AckMatches(s)
  /\ ~(RefuseOnFence /\ DeposedHolder(s))
  /\ (AckFromInstall => sc[s].honored)
  \* D1, at the one place a gated boundary can break it: a citation
  \* that dropped a declared path installed a point that does not
  \* carry it, and `ok` would assert the opposite.
  /\ (AckHonest => sc[s].pendDirty \cap sc[s].citeDropped = {})
  /\ sc' = [sc EXCEPT ![s].ackN = @ \cup sc[s].pendN, ![s].honored = FALSE,
       ![s].installed = FALSE, ![s].citeDropped = {}]
  /\ gh' = [gh EXCEPT
       !.acks = @ + 1,
       !.honors = @ + (IF sc[s].honored THEN 1 ELSE 0),
       !.ackAfterRestart = @ + (IF sc[s].pendReRun THEN 1 ELSE 0),
       !.ackEarly = @ \/ BoundaryBroken(s),
       !.ackIncoherent = @ \/ BoundaryIncoherent(s),
       !.fencedOkAck = @ \/ DeposedHolder(s),
       \* Only an ack that claims its OWN install can disagree with the
       \* stamp; an ack that installed nothing names no clock.
       \* Scoped to an honor that actually INSTALLED. A no-diff honor
       \* installs no boundary, so its ack names the VERB that ran while
       \* the stamp still names whoever last installed — two different
       \* questions, and only a real install owes them the same answer.
       \* (The first pilot conflated them and fired on correct code.)
       !.srcMismatch = @ \/ (sc[s].honored /\ sc[s].installed
                             /\ AckedSrc(s) # BoundaryClock(s))]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* The honest answer when the boundary does not carry the declared
   point: the agent is still ANSWERED — a partial ack names the nonces
   and the dropped paths — but nothing claims the point landed.  An
   agent that treats it as failure and re-touches is behaving
   correctly, and the next boundary carries the path the ordinary way
   (the racing write is queued in the inbox; the lane consumes it, the
   local file is still dirty, and the conflict rule publishes it).

   `Inv_NoNonceOrphan` is what makes this an ANSWER rather than a
   silence, and it is checked over this action like any other.        *)
AckPartial(s) ==
  /\ SentinelEnabled /\ AckHonest /\ Running(s) /\ sc[s].pc = "idle"
  /\ PendLive(s) /\ ~AckMatches(s)
  /\ ~(RefuseOnFence /\ DeposedHolder(s))
  /\ sc[s].pendDirty \cap sc[s].citeDropped # {}
  /\ sc' = [sc EXCEPT ![s].ackN = @ \cup sc[s].pendN, ![s].honored = FALSE,
       ![s].installed = FALSE, ![s].citeDropped = {}]
  /\ gh' = [gh EXCEPT !.acks = @ + 1, !.partialAcks = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

(* Retire AFTER the ack rename.  Splitting these two is not ceremony:
   the crash between them is the one the draft's crash matrix answered
   wrongly, and it is the only reachable way to observe a pending record
   that a standing ack already answers.                                 *)
RetirePending(s) ==
  /\ SentinelEnabled /\ Running(s) /\ sc[s].pc = "idle"
  /\ PendLive(s) /\ AckMatches(s)
  /\ sc' = [sc EXCEPT ![s].pendN = {}, ![s].pendCov = NoPend,
       ![s].pendMint = 0, ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts, gh>>

(* D2's refused ack: deposal must never strand a waiting agent.  Write
   the refusal naming every covered nonce, retire, flip the marker, and
   exit fenced — one step here.                                         *)
AckRefused(s) ==
  /\ SentinelEnabled /\ Running(s)
  \* Tranche 6: no `refused-fenced` ack exists under the barrier lease —
  \* a fence abandons the barrier and the next one honors the record.
  /\ ~BarrierLease
  /\ PendLive(s) /\ RefuseOnFence /\ DeposedHolder(s)
  /\ sc' = [sc EXCEPT ![s].ackN = @ \cup sc[s].pendN,
       ![s].pendN = {}, ![s].pendCov = NoPend, ![s].pendMint = 0,
       ![s].pendDirty = {},
       ![s].honored = FALSE, ![s].pendReRun = FALSE,
       ![s].st = "dead"]
  /\ gh' = [gh EXCEPT !.refusedAcks = @ + 1]
  /\ UNCHANGED leaseVars /\ UNCHANGED <<cellEpoch, cellHolder, manSeq, manSrc, manifest, objects, inbox, removals,
                 window, hitlAcked, conflicts>>

SentinelNext ==
  \E s \in Syncers :
    Touch(s) \/ TakeSentinel(s) \/ FastPath(s) \/ AckOk(s)
    \/ AckPartial(s) \/ RetirePending(s) \/ AckRefused(s)

------------------------------------------------------------------------------
------------------------------------------------------------------------------
(* Under gated, ANY action that moves an object mints a version: the
   bucket is versioned, so a PUT destroys nothing and a delete leaves
   every prior version fetchable (S3 writes a delete marker).  Composing
   this once at the Next level rather than threading it through twenty
   actions is also what keeps every pre-gated state space intact — with
   GatedCitation = FALSE, `versions` is frozen at Init and adds no
   distinct states at all.                                             *)
VersionsFollow ==
  IF GatedCitation
  THEN versions' = [p \in Paths |->
         IF objects'[p] = 0 THEN versions[p]
         ELSE versions[p] \cup {objects'[p]}]
  ELSE versions' = versions

BaseNext ==
  \/ StartA
  \/ \E s \in Syncers : CrashPod(s) \/ Restart(s) \/ RenewDiscover(s)
  \/ StallA \/ ThawA \/ ClaimB \/ CheckoutB
  \/ \E s \in Syncers :
       StartLease(s) \/ Claim(s) \/ Enqueue(s) \/ SkipDeadHandoff(s)
  \/ \E s \in Syncers, p \in Paths :
       AgentWrite(s, p) \/ AgentWriteSame(s, p) \/ AgentDelete(s, p) \/ Upload(s, p)
       \/ GCDelete(s, p) \/ GCHead(s, p)
  \/ \E s \in Syncers : Narrow(s)
  \/ \E p \in Paths : HitlWrite(p) \/ HitlRemove(p) \/ TrackOrphan(p)
  \/ \E p, q \in Paths : HitlRename(p, q)
  \/ HitlRefused
  \/ \E s \in Syncers :
       Consume(s) \/ Scan(s) \/ UploadFenced(s) \/ GCDeleteFenced(s)
       \/ PreDeletesDone(s) \/ CASFenced(s) \/ CASMiss(s) \/ CASInstall(s)
       \/ Finish(s) \/ Sync(s) \/ PullOnly(s) \/ AbandonBarrier(s)
  \/ SentinelNext

Next ==
  \/ (BaseNext /\ VersionsFollow /\ UNCHANGED gatedVars)
  \/ GatedNext

Spec == Init /\ [][Next]_vars

\* Gated mode is frozen and not supported under the barrier lease (design
\* §4.3, D3): its upload lane runs outside any barrier.
ASSUME ~(BarrierLease /\ GatedCitation)
\* `touched` is cleared where a barrier or a sync rewrites the baseline,
\* not at a narrow's uncite or a declared removal — keep them apart.
ASSUME MaxSameBytes > 0 => BarrierLease /\ MaxNarrows = 0 /\ MaxRemovals = 0
\* Tranche 7: the queue and the empty install are the barrier lease's code.
ASSUME WriterQueue => BarrierLease /\ MergeCapable /\ InboxEnabled
ASSUME EmptyInstall => WriterQueue
ASSUME ~QueueForeignChanges => WriterQueue

(* ---- tranche 6: FAIRNESS, for the liveness runs only -------------------
   Weak fairness on each syncer's OWN barrier step — not on "some syncer
   takes a step", which would let one writer discharge the other's
   obligation.  Under `Ticket` the queue head's claim is continuously
   enabled from the release until it fires, so WF is enough; under the
   mutation the claim is enabled only between one release and the next
   claim, WF asks nothing, and one writer claims forever.               *)
BarrierStep(s) ==
  /\ \/ Consume(s) \/ Scan(s) \/ Claim(s) \/ Enqueue(s) \/ SkipDeadHandoff(s)
     \/ \E p \in Paths : Upload(s, p) \/ GCDelete(s, p) \/ GCHead(s, p)
     \/ PreDeletesDone(s) \/ CASMiss(s) \/ CASInstall(s) \/ Finish(s)
     \/ CASFenced(s) \/ GCDeleteFenced(s) \/ RenewDiscover(s) \/ PullOnly(s)
     \/ FastPath(s)
  /\ VersionsFollow /\ UNCHANGED gatedVars

FairSpec == Spec /\ \A s \in Syncers : WF_vars(BarrierStep(s))

\* Every live writer that queued for the cell eventually holds it.  A
\* waiter that dies or freezes is released from the claim (a frozen
\* waiter needs `ThawA`, which is not fair, and a dead one needs nothing).
Waiting(s) == sc[s].pc = "waiting" /\ Running(s)
NoStarvation ==
  \A s \in Syncers : [](Waiting(s) => <>(Holding(s) \/ ~Running(s)))

------------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
  /\ cellEpoch \in 0..EpochBound /\ cellHolder \in Syncers \cup {"none"}
  /\ cellQueue \in Seq(Syncers) /\ Len(cellQueue) <= Cardinality(Syncers)
  /\ cellSeen \in Seq(Syncers) /\ Len(cellSeen) <= Cardinality(Syncers)
  /\ \A i, j \in 1..Len(cellQueue) : cellQueue[i] = cellQueue[j] => i = j
  /\ cellHandoff \in Syncers \cup {"none"} /\ cellReleased \in BOOLEAN
  /\ manSeq \in 1..MaxSeq+1 /\ manSrc \in Sources
  /\ manifest \in [Paths -> Gens] /\ objects \in [Paths -> Gens]
  /\ inbox \subseteq (Paths \X Gens) /\ window \in 0..EpochBound
  /\ hitlAcked \subseteq (Paths \X Gens) /\ conflicts \subseteq (Paths \X Gens)
  /\ versions \in [Paths -> SUBSET Gens]
  /\ stage \in [Syncers -> [Paths -> Gens]]
  /\ stageBase \in [Syncers -> [Paths -> Gens]]
  /\ \A s \in Syncers : sc[s].scope \subseteq Paths /\ sc[s].prevScan \subseteq Paths
  /\ removals \subseteq Paths
  /\ \A s \in Syncers : sc[s].declared \subseteq Paths
                       /\ sc[s].consumed \subseteq (Paths \X Gens)
                       /\ sc[s].repairMoved \subseteq Paths
                       /\ sc[s].touched \subseteq Paths
                       /\ sc[s].fq \subseteq (Paths \X Gens)
                       /\ \A x, y \in sc[s].fq : x[1] = y[1] => x = y

\* §4.2: A NARROW IS AN UNWATCH, NEVER AN ABSENCE.  No path a narrow
\* dropped may lose its object — a workspace that stops holding a file
\* must not take the bucket's copy with it.  This is what the
\* unlink-first arm violates: the citation survives the unlink, the
\* next scan classifies the path delete-eligible, and GC publishes it.
Inv_NarrowNeverDeletes == \A p \in gh.narrowed : objects[p] # 0

\* The other half.  A narrowed path must not be re-uploaded and
\* re-cited, which is what the uncite-first arm does: the file survives
\* the uncite, the next scan reads present-and-not-in-baseline as a
\* local ADD, and the barrier silently undoes the narrow.  A path the
\* agent legitimately re-creates leaves `gh.narrowed` at AgentWrite, so
\* this never fires on widening.
Inv_NarrowNeverRecites == gh.narrowRecited = {}

\* Non-vacuity: the verb actually fires.  Probed via a ghost only
\* Narrow writes — probe the ACTION, never the situation.
ProbeNarrow == gh.narrows = 0

\* An acked HITL write is never silently lost: its bytes are never
\* destroyed by a writer that did not legitimately learn them, and no
\* manifest install drops its last tracked reference without a conflict
\* record.  (Both stamp sites feed gh.amputated.)
Inv_HITLDurable == ~gh.amputated

\* Every cited manifest entry has a live object behind it: the barrier
\* order (uploads -> CAS -> deletes) keeps checkouts satisfiable.
Inv_NoDangling == \A p \in Paths : manifest[p] # 0 => objects[p] # 0
\* No commit replaces a citation the key still holds with a generation of
\* its own upload the key no longer holds (finding 13's second route).
\* `Inv_NoDangling` cannot see it — an object exists — and the loss is the
\* PEER's: its committed bytes stay at the key, cited by nothing.
Inv_NoStaleOverride == ~gh.staleOverride

\* A deposed writer's manifest CAS never lands.
Inv_NoStragglerInstall == gh.stragglerInstalls = 0

\* A deposed writer's data PUT never lands.
Inv_NoDeposedPut == gh.deposedPuts = 0

\* A container restart never resurrects an unpublished delete.
Inv_NoResurrection == ~gh.resurrected

\* The sync verb never destroys genuinely-dirty local work without
\* surfacing it (tranche 2).
Inv_SyncNeverDestroysDirty == ~gh.syncDestroyed

\* D4 (boundary-verbs plan §2.2).  A sync never advances the MERGE BASE
\* for a path it did not integrate and did not surface.  Violating this
\* is not a stale read — it is permanent: `foreign(p)` at every later
\* merge compares theirs against the base we just falsified, computes
\* "unchanged", and the entry is never queued into the inbox again.
Inv_NoForeignLost == ~gh.foreignLost

\* ---- tranche 3, product 2: version lifetime (D7/D8) ---------------------

\* THE invariant of this product.  Every cited generation is still
\* STORED — which on a versioned bucket is a strictly stronger claim
\* than Inv_NoDangling's "the object exists".  Gated staging makes the
\* cited version NONCURRENT, so an object can exist, read as newer
\* uncited bytes, and have nothing at all behind its citation.
\*
\* Not hypothetical: the shipped implementation violated this for one
\* session, because the store reported its ObjectMeta before the version
\* id was minted, every citation named the empty version, and the exact
\* reaper — matching nothing — deleted every live version of every cited
\* key.  The unit tests caught it only because assertions happened to sit
\* in the right places.
Inv_CitedVersionLives ==
  \A p \in Paths : manifest[p] # 0 => manifest[p] \in versions[p]

\* The reaper never removes the version a path currently READS as.  That
\* generation is either the citation it just installed or live
\* staged-uncited work; either way it is not garbage.
Inv_NoUncitedGC ==
  \A p \in Paths : objects[p] # 0 => objects[p] \in versions[p]

\* A boundary is all-or-nothing.  No reachable state may show a citation
\* that has installed SOME of its pending set and not the rest — that is
\* a reader seeing half a logical change, which is the one thing gated
\* mode exists to prevent.  The single-CAS design makes it true by
\* construction; the split-install mutation is what keeps that from
\* being an untested claim.
Inv_BoundaryAtomic ==
  \A s \in Syncers :
    sc[s].citeDone = {} \/ sc[s].citeDone = Valid(s)

\* ---- tranche 3, product 1: the boundary verb (D1/D2/D12) ----------------

\* THE invariant of this product.  An ok ack asserts that the coherent
\* point the agent declared is INSTALLED — not durable, not queued:
\* cited by the manifest, at the seq the ack names.  Stamped at the ack
\* rather than checked over states, because the promise is about the
\* instant the agent is told "done" and nothing later can un-tell it.
Inv_AckImpliesCited == ~gh.ackEarly

\* The second half, and the one the plan's draft called
\* `Inv_AckNotEarly`: the boundary an ok ack names is a coherent point,
\* citing every generation this workspace has integrated.  A citation
\* repair still owed at ack time means a reader — or this workspace's
\* own next checkout — resolves to bytes already superseded here.
Inv_AckBoundaryCoherent == ~gh.ackIncoherent

\* Every CONSUMED nonce is still named by something: the pending record
\* that will answer it, or the ack that already did (ok or refused).
\* Consuming is the commitment point — the rename takes the sentinel out
\* of the agent's reach, so nothing else can ever answer it.  Per
\* incarnation, because a pod replacement takes the agent and the tree
\* with the pending file.
Inv_NoNonceOrphan ==
  \A s \in Syncers : sc[s].owed \subseteq (sc[s].pendN \cup sc[s].ackN)

\* One boundary, ONE clock.  The agent reads the ack; the fleet reads the
\* manifest's stamp; an operator asking "did my agent's publish land, or
\* was that the floor?" is a different process from the agent, often in a
\* different cluster, and it has only the bucket to ask.  Shipped code
\* computed the two independently and they disagreed — with the bucket
\* holding the wrong one, which is the worse half.
\* Scoped to acks that claim their own install: a no-diff honor installs
\* nothing, and its ack is answering "your point is already true" rather
\* than naming a new boundary.
Inv_BoundaryNamesItsClock == ~gh.srcMismatch

\* A deposed incarnation never tells an agent its boundary landed.  The
\* plan calls this `Inv_RefusedNeverInstalled`; it is stated here as the
\* ack side, which is the side the agent reads and the only side a
\* fenced incarnation still controls.
Inv_NoFencedOkAck == ~gh.fencedOkAck

\* ---- tranche 6: the per-barrier lease -----------------------------------

\* D1, the safety half of the lease: ONE writer in the commit section at
\* a time.  A deposed straggler may still BELIEVE it is in its commit
\* section — that is what the fences are for — so the claim is over the
\* holders the cell still recognises.  True under the life lease too.
Inv_CommitExclusive ==
  \A s, t \in Syncers :
    (Holding(s) /\ Holding(t) /\ s # t) => (Deposed(s) \/ Deposed(t))

\* Model coherence, pinned so a wrong claim arm cannot hide: a HELD cell's
\* holder is in its commit section at the cell's epoch — alive, frozen or
\* dead there.  A restart releases, a failed commit releases, a fence
\* yields; nothing else may leave a held cell behind a syncer that is not
\* committing.
\* FINDING 10, as a CONVERGENCE property a safety run can check: once
\* nothing at all can move (every budget spent, every writer idle), each
\* object at a cited key is the citation or is tracked by the inbox — no
\* bytes are left that the manifest, a fresh checkout and the live trees
\* disagree about.  A pod replaced between its upload and its commit
\* violates it as shipped; `OrphanTrack` is the candidate fix.
\* "Nothing can move" is the SYNCERS' progress, not `Next`: an agent can
\* always delete a file, and a budget-bounded world ends with barriers, not
\* with agents.
SyncerProgress ==
  \/ \E s \in Syncers :
       \* A writer that has not started yet can still start: its first
       \* run of this check called that quiescence and reported a world
       \* whose only live writer had not checked out.
       \/ StartLease(s)
       \/ BarrierStep(s)
       \/ PullOnly(s) \/ FastPath(s) \/ TakeSentinel(s)
       \/ AckOk(s) \/ AckPartial(s) \/ RetirePending(s)
  \/ \E p \in Paths : TrackOrphan(p)
Inv_QuiescentConverged ==
  ~ENABLED SyncerProgress =>
    \A p \in Paths :
      \/ objects[p] = manifest[p]
      \/ objects[p] = 0
      \* An object at a key the manifest does not cite is garbage no
      \* checkout serves — a delete's CAS whose GC never ran (the crash
      \* between them) — a leak, not the disagreement this is about.
      \/ manifest[p] = 0
      \/ \E pr \in inbox : pr = <<p, objects[p]>>

Inv_CellHeldByHolder ==
  BarrierLease /\ CellHeld =>
    (Holding(cellHolder) /\ sc[cellHolder].epoch = cellEpoch)

------------------------------------------------------------------------------
(* Non-vacuity probes — each names an ACTION via a ghost that only that
   action writes, and TLC is REQUIRED to violate it (the A2 probe rule:
   probe the action, never the situation).                              *)

ProbeBarrierDone      == gh.done = 0
ProbeHITLCited        == gh.cited = 0
ProbeTakeover         == gh.takeovers = 0
ProbeStragglerAttempt == gh.stragglerCas = 0
ProbePark             == conflicts = {}
ProbeGC               == gh.gc = 0
ProbeRefusal          == gh.refusals = 0
ProbeAdoptOwn         == gh.adoptOwn = 0
ProbeRestart          == gh.restarts = 0
ProbeSyncApplied      == gh.syncApplied = 0
ProbeSyncConflict     == gh.syncConflicts = 0
\* Action-written (Sync's own ghost): a SCOPED sync actually deferred a
\* remote change, rather than the scoped arm never having fired.
ProbeScopedDeferral   == gh.scopedDeferrals = 0
\* U16, MEASURED AND OPEN (2026-08-26). This probe does NOT fire, and
\* that is the finding rather than a missing cfg. `gh.deferredPaths`
\* becomes non-empty (the deferral happens), but
\* `deferredPaths # {} /\ inbox # {}` is UNREACHABLE across the full
\* state space at MaxBarriers = 3 (6.2M states) and again at 4 (13.5M
\* states) — so the deferred path is never queued into the inbox while
\* the deferral stands, and no consume can integrate it.
\*
\* D4's loss-avoidance argument is precisely that the deferred entry
\* ARRIVES through merge -> inbox -> consume. `Inv_NoForeignLost` and
\* `ProbeScopedDeferral` are both stamps written INSIDE `Sync`: they
\* prove the deferral, never the arrival. So the model currently cannot
\* distinguish "deferred" from "lost", which is exactly what the review
\* alleged. NOT wired into check.sh: a must-fail run that does not fail
\* would turn the gate red over an unbuilt artifact rather than a
\* regression. The instrumentation stays because it is what a fix needs.
ProbeOutOfScopeLater  == gh.deferredLater = 0

\* ---- tranche 3, product 2 ------------------------------------------------
\* One CAS installed >= 2 paths from a pending set that had SURVIVED a
\* lane pass.  Both halves matter: without the size the split is
\* untested, and without the carry every citation might simply be
\* following its own lane, which is hybrid wearing gated's name.
\* A pod replacement actually happened IN THE GATED WORLD. Without this
\* the crash-matrix run is green over a state space that never crashed
\* — the vacuity that let MaxCrashes=0 stand in all 19 gated cfgs
\* while §10.1c deferred the citation's crash matrix to product 1,
\* whose own cfgs never crash either (review: U12).
ProbeGatedCrashReachable == gh.crashes = 0
\* U41: `Inv_NoResurrection` was listed as checked by LeanGatedHolds
\* while that cfg ran MaxRestarts = 0, and `Restart` is the ONLY writer
\* of the state the invariant tests — so the line read as coverage and
\* was unfalsifiable by construction. It matters here more than
\* anywhere: gated mode WIDENS the resurrection window, because a
\* delete stays cited until a citation, which is exactly the
\* local[p]=0 /\ manifest[p]#0 /\ baseline[p]#0 shape `res` tests.
\* Restarts are now ON in the gated world (127k states, 2 s), and this
\* is the probe that keeps the fix honest — without it, enabling the
\* knob and never reaching a restart would look identical.
ProbeGatedRestartReachable == gh.restarts = 0
ProbeCitationInstalled == ~gh.carriedCite
\* A delete was actually withheld from the manifest until a citation.
ProbeWithheldDelete    == gh.withheld = 0
\* U15: `ProbeWithheldDelete` counts `gh.withheld`, bumped in `LaneDone`
\* when a delete is WITHHELD — never in `CiteFinish` when it is APPLIED.
\* So `LeanGatedHolds` can hold with `dels = {}` at every citation, and
\* the delete-application half of the gated design — manifest entry
\* removed, object deleted, version reaped — is never shown reachable.
\* That is the one step where `Inv_CitedVersionLives` and
\* `Inv_NoUncitedGC` are most at risk, so its reachability is not
\* something to assume. `gcCited` is bumped ONLY by `CiteFinish`, so
\* unlike a `ProbeGC` re-run this cannot be satisfied by the cadence
\* GC path.
ProbeGatedGC           == gh.gcCited = 0
\* A citation actually fired mid-change (a staged path had already been
\* edited again locally) — the lag/backlog caps' shape, and the reason
\* the source is stamped bucket-visibly.
ProbeForcedCite        == gh.forcedCites = 0
\* REQUIRED-REACHABLE, deliberately.  §3 residual 11: a reader that does
\* not resolve through the manifest sees mid-logical-change bytes where
\* it previously saw the last boundary.  This probe proves the exposure
\* is PRESENT rather than assumed away — and a future design that
\* quietly closes it must fail this probe and force the residual to be
\* rewritten.  It names a state rather than an action on purpose: the
\* exposure IS a state, and there is no action that "does" it.
\* ---- tranche 3, product 1 ------------------------------------------------
\* An ack was actually written off a REAL barrier install (not merely
\* written): without this the strict runs could hold with the honor path
\* never having fired.
ProbeSentinelHonored == gh.honors = 0
\* The refusal fired: deposal answered a waiting agent.
ProbeRefusedAck      == gh.refusedAcks = 0
\* An ack was written for a pending record that SURVIVED A RESTART —
\* the uniform crash rule's own path, exercised.
ProbeAckAfterCrash   == gh.ackAfterRestart = 0
\* Two touches actually coalesced into one pending record.  Product 2's
\* lesson, applied: the orphan mutation checks a state space that never
\* contained two live nonces unless this fires.
ProbeCoalescedAck    == gh.coalesced = 0
\* A pending sentinel was honored by the SKIP-ON-NO-DIFF pass rather
\* than a full barrier.  This is what makes the FastPathGuards runs
\* non-vacuous: without it, "the fast path is sound" could hold because
\* the fast path never ran.
ProbeFastPathHonor   == ~gh.fastHonor

\* ---- tranche 3, product 1 x 2: the sentinel over the CITATION lane ------
\* A gated citation actually DROPPED a path the agent had declared.  The
\* honesty rule is vacuous without it: "an ok ack never claims a dropped
\* path" holds trivially in a world where nothing is ever dropped.
ProbeDeclaredDrop == gh.declaredDrops = 0
\* ...and the partial ack — the honest answer — actually fired, so the
\* agent is ANSWERED rather than left waiting on a boundary that will
\* never be claimed.
ProbePartialAck   == gh.partialAcks = 0

\* U20, OPEN (assessed 2026-08-26). This is a STATE PREDICATE over
\* `objects` vs `manifest`, not an action-written ghost — it breaks
\* this model's own house rule ("the probe names the ACTION, never the
\* situation") because there IS no reader action to name.
\*
\* §4 requires two that were never built: `PinnedReader` (with its own
\* invariant — never materializes post-boundary bytes under
\* pre-boundary citations) and `RawReader`, plus product 2's
\* `ProbeDanglingCitation`. The only reader in the module is the
\* checkout embedded in the barrier, which copies `manifest[p]` into
\* `local` without consulting `versions` or `objects` at all. So the
\* model cannot express the hazard D13 exists to prevent — a gated
\* checkout 412ing and S3-wins-adopting `current[p]` — nor the
\* endgame: after `BackstopExpire` reaps a cited version
\* `Inv_CitedVersionLives` fires, but nothing shows a checkout REFUSING
\* rather than serving a hole.
\*
\* Both are currently carried by the Rust battery
\* (`pinned_reads_never_adopts_current`, and as of 2026-08-26
\* `sync_under_pinned_reads_resolves_the_cited_version_not_the_current_one`)
\* and by drill legs B23/B24. That is real evidence, but it is not the
\* model, and §10.1c's "not modelled" list does not name the readers.
\* Building them is a tranche — a reader action over the `versions`
\* substrate plus an invariant — not a probe, and it is deliberately
\* NOT faked here with a predicate that would read as coverage.
ProbeRawReaderSeesUncited ==
  \A p \in Paths : objects[p] = manifest[p] \/ manifest[p] = 0

\* ---- tranche 5: DECLARED removals ----------------------------------------

\* Every acked HITL write is TRACKED by something in the world until it
\* is legitimately superseded or was once CITED: in the cell, surfaced
\* as a conflict, integrated into a LIVE incarnation's baseline, or
\* published in some generation — after which a later deletion is the
\* workspace's own editing (the crash between a delete's CAS and its
\* GC leaves an orphan object, not a lost write).  What the early drop
\* violated: consumed, then the pod replaced, and the write was acked,
\* durable, and known to nothing — never cited, never to be.
Inv_HITLTracked ==
  \A pr \in hitlAcked :
    \/ objects[pr[1]] # pr[2]
    \/ manifest[pr[1]] = pr[2]
    \/ pr \in gh.citedPairs
    \/ pr \in inbox
    \/ pr \in conflicts
    \/ \E s \in Syncers : sc[s].st # "dead" /\ sc[s].baseline[pr[1]] = pr[2]
    \* Superseded, and sticky.  The first clause says it for unique
    \* generations: once a writer that integrated the write deletes or
    \* overwrites it, the key never reads as it again.  Under MaxSameBytes
    \* it can — an agent re-creating the UI write's exact bytes after
    \* publishing their delete carries the same generation, and TLC's first
    \* same-bytes runs in BLWORLD stopped on exactly that twice (once with
    \* the re-creation's pod then replaced: finding 10's loss of the
    \* agent's own work, not the UI's).  `hitlRetired` is written only
    \* under MaxSameBytes, so every other state space is unchanged.
    \/ pr \in gh.hitlRetired

\* A rename that was PERFORMED is one manifest generation: no reader
\* of the manifest ever sees the moved BYTES under both names (§4, §6)
\* — the old name citing the generation that was moved while the new
\* name is cited.  A refused rename legitimately leaves both — an extra
\* file, never a hole (§5) — and an agent that re-creates the old name
\* after the unlink has made a NEW file, which is cited as its own.
Inv_RenameAtomic ==
  \A r \in gh.renamed \ gh.renameRefused :
    ~(manifest[r[1]] = r[3] /\ manifest[r[2]] # 0)

\* ...and never neither: the old name stays cited until the new name
\* has been cited (or is still tracked in the cell for the barrier
\* that will cite it).  Once the destination was cited, whatever the
\* workspace does to either name afterwards is its own editing.  A
\* REFUSED rename is answered, not performed: the source's fate is
\* then the agent's (it may have deleted it before the request even
\* arrived), and the copy survives under the conflict record.
Inv_RenameNoHole ==
  \A r \in gh.renamed \ gh.renameRefused :
    \/ manifest[r[1]] # 0
    \/ manifest[r[2]] # 0
    \/ <<r[2], r[4]>> \in gh.citedPairs
    \/ \E e \in inbox : e[1] = r[2]
    \* The copy was overwritten by a later write to the new name before
    \* any barrier cited it: that writer read the name, and the bytes
    \* it replaced are its to have replaced.
    \/ objects[r[2]] # r[4]

ProbeRemovalApplied == gh.removalsApplied = 0
ProbeRemovalRefused == gh.removalsRefused = 0
ProbeRenameApplied  == gh.renamesApplied = 0

\* ---- tranche 6: the per-barrier lease -----------------------------------
\* REQUIRED-REACHABLE: an upload landed while the other writer was in
\* its commit section.  The house rule — never prove a guarantee by the
\* attack's absence: the lease's claim is that two writers OVERLAP and
\* only the commit serialises.  Written by Upload.
ProbeWritersInterleave == ~gh.interleaved
\* A claim by the syncer the release NAMED actually happened (Claim).
ProbeHandoffFired      == gh.handoffs = 0
\* A quiet holder in its commit section was deposed by the other writer
\* (Claim's deposal arm).
ProbeDeposalMidCommit  == gh.deposals = 0
\* A quiet handoff was skipped and the cell taken by a survivor.
ProbeDeadHandoffSkipped == gh.deadSkips = 0
\* A syncer actually queued for the cell — without this the strict runs
\* could hold with the ticket never exercised.
ProbeEnqueued          == gh.enqueues = 0
\* A fenced holder abandoned its barrier and KEPT RUNNING (a fence is
\* not a death).  Written only by the fence arms under BarrierLease.
ProbeFenceAbandoned    == gh.abandoned = 0
\* The fix actually fired: a CAS under the lease found an adopted object
\* gone and withheld the entry.  Without this the strict runs could hold
\* with the race never reached — which is exactly how it hid until now.
ProbeAdoptWithheld     == gh.adoptWithheld = 0
\* Finding 13's fix actually fired: a CAS found the object under one of
\* its own LANDED uploads gone or moved and withheld the entry.
ProbeUploadWithheld    == gh.uploadWithheld = 0
\* Finding 10's candidate fix fires.
ProbeOrphanTracked     == gh.orphanTracks = 0

\* ---- tranche 7: model the implementation -------------------------------
\* Each new arm is REQUIRED-REACHABLE in the world whose strict run leans
\* on it: a pull-only boundary, a commit that installed nothing, a queued
\* deletion that removed a clean copy, and one the key superseded.
ProbePullOnly          == gh.pullOnlys = 0
ProbeEmptyInstall      == gh.emptyInstalls = 0
ProbeTombstoneApplied  == gh.tombRemoved = 0
ProbeTombstoneSuperseded == gh.tombSuperseded = 0

------------------------------------------------------------------------------
(* GHOST-STATE REDUCTION (2026-09-15).  Two fingerprint reductions the cfgs
   opt into; neither changes an action, and neither is read by one.

   `StrictView` is the state as TLC should DISTINGUISH it in a run that
   checks no probe.  Most of `gh` is non-vacuity bookkeeping: counters a
   probe reads and nothing else does.  In a strict or must-fail run two
   states that differ only there have identical futures and identical
   invariant values, and TLC explored both — `LeanBarrierLeaseAdoptVerified`
   was 641,858 states with them and is 320,184 without.  The view keeps
   every `gh` field an ACTION reads (budgets, `nextGen`, `renamed`, ...)
   and every field an INVARIANT reads (the sticky stamps, `citedPairs`,
   `hitlRetired`, ...), and drops `sc`'s `pendReRun` and `stageCarried`,
   which only feed probe counters.

   That is sound only while it stays true, and a guard added next month
   that reads a dropped counter would make it silently unsound — so it is
   not a comment's claim: `view-census.py` (run first by `check.sh`) fails
   the gate when any field outside `StrictGh` is read anywhere but a probe
   definition or its own (or another dropped field's) update.  A probe cfg
   never gets the view: the field it names must be distinguished.

   `PathSym`: nothing in the module names a path, so the paths are
   interchangeable wherever they START interchangeable (`FreePaths` is {}
   or all of `Paths`).  Symmetry is unsound for liveness, so the FairSpec
   cfgs never get it.                                                     *)
StrictGh ==
  [amputated |-> gh.amputated, resurrected |-> gh.resurrected,
   stragglerInstalls |-> gh.stragglerInstalls, deposedPuts |-> gh.deposedPuts,
   barriers |-> gh.barriers, crashes |-> gh.crashes, restarts |-> gh.restarts,
   stallUsed |-> gh.stallUsed, nextGen |-> gh.nextGen, hitl |-> gh.hitl,
   refusals |-> gh.refusals, syncs |-> gh.syncs,
   syncDestroyed |-> gh.syncDestroyed, foreignLost |-> gh.foreignLost,
   touches |-> gh.touches, ackEarly |-> gh.ackEarly,
   ackIncoherent |-> gh.ackIncoherent, fencedOkAck |-> gh.fencedOkAck,
   srcMismatch |-> gh.srcMismatch, narrows |-> gh.narrows,
   narrowed |-> gh.narrowed, narrowRecited |-> gh.narrowRecited,
   removals |-> gh.removals, renamed |-> gh.renamed,
   renameRefused |-> gh.renameRefused, citedPairs |-> gh.citedPairs,
   sameBytes |-> gh.sameBytes, staleOverride |-> gh.staleOverride,
   hitlRetired |-> gh.hitlRetired]
StrictSc ==
  [s \in Syncers |-> [sc[s] EXCEPT !.pendReRun = FALSE, !.stageCarried = FALSE]]
StrictView ==
  <<cellEpoch, cellHolder, cellQueue, cellHandoff, cellReleased, cellSeen,
    manSeq, manSrc, manifest, objects, inbox, removals, window,
    StrictSc, versions, stage, stageBase, withheldDel, hitlAcked,
    conflicts, StrictGh>>
PathSym == Permutations(Paths)

==============================================================================
