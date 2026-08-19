---------------------------- MODULE FlintTierEpoch ----------------------------
(***************************************************************************)
(* The flint-lite S3-tier volume epoch — L2 step 7's A8 fencing protocol   *)
(* (spdk-csi-driver/src/tier/epoch.rs, the publish steps of flush.rs, and  *)
(* the store CAS semantics of store/memory.rs — which real_s3_acceptance   *)
(* holds equal to S3's).  Modeled AFTER the code shipped and after chaos   *)
(* phases A/B/H drilled it: the drills sample interleavings, this module   *)
(* enumerates them.                                                        *)
(*                                                                         *)
(* The facts encoded, each from the implementation:                        *)
(*                                                                         *)
(*   - The epoch object is a CAS cell in the bucket: acquire is            *)
(*     If-None-Match:* (fresh) or If-Match(observed token) with epoch+1    *)
(*     (supersede); renew is If-Match(own token) and ROTATES the token     *)
(*     (the UUID salt — real-S3 gate bug 1: without rotation a live        *)
(*     holder's renews are invisible to the quiet-poll judge).             *)
(*   - A foreign holder is judged dead ONLY by the store's own evidence:   *)
(*     its token unchanged across lease_misses consecutive polls.  Every   *)
(*     path out of claim() runs the MPU abort-sweep before holding.        *)
(*   - The heartbeat's 412 fences and exits (exit(70)); restart is a       *)
(*     fresh claim.  Self-recognition: a store record carrying OUR         *)
(*     holder_id is a dead prior incarnation — supersede immediately.      *)
(*   - Every publish is conditional (PutCondition has no unconditional    *)
(*     variant): If-Match(base etag) or If-None-Match:*.  An MPU Complete  *)
(*     first fails NoSuchUpload if the assembly was swept, then checks     *)
(*     the same condition.  A failed publish HEAD-rediscovers the          *)
(*     current object and retries next tick (A6 local-wins).               *)
(*                                                                         *)
(* FAITHFULNESS NOTES (the reasoning, so it cannot drift silently):        *)
(*                                                                         *)
(*   - Read-then-CAS pairs collapse into one action WHERE THE CAS          *)
(*     revalidates the read (acquire/renew/publish): the store rejects a   *)
(*     stale observation atomically, so the two-round decomposition adds   *)
(*     no behaviors.  The quiet COUNT is genuinely multi-round state and   *)
(*     stays decomposed (ObserveForeign / PollQuiet / Takeover).           *)
(*   - guard.fence() -> exit(70) is modeled as ONE step (renew-fail =>    *)
(*     idle).  The code's fence-to-exit gap, and the flusher's             *)
(*     pre-publish guard consult inside it (flush.rs:835), only NARROW     *)
(*     the stale-publish window that the probes below prove open at both   *)
(*     ends — the consult's necessity is timing, not logic, so it gets     *)
(*     no mutation run (a mutation that cannot lose proves nothing —       *)
(*     README, the dropped run 5q).                                        *)
(*   - THE QUIET-WAIT IS A TIMING AXIOM, NOT A SAFETY LAYER: with token    *)
(*     rotation on, the takeover CAS structurally refuses whenever the     *)
(*     holder renewed after the observer's last poll — deleting the        *)
(*     quiet-wait cannot depose a RENEWING holder (machine-checked: the    *)
(*     seize mutation cannot lose, so it is not in the gate).  What the    *)
(*     wait buys is TIME (misses x heartbeat) for a live-but-stalled       *)
(*     holder to get a renew in — quantitative content TLA cannot          *)
(*     discharge, exactly like FlintClaims' marker grace.                  *)
(*                                                                         *)
(* THE THEOREMS (strict run):                                              *)
(*   - Inv_NoPreSweepMpuLand: an assembly initiated under an older store   *)
(*     epoch never Completes once a successor claims — the sweep's        *)
(*     teeth (NoSweep mutation must rediscover the loss; chaos phase E's   *)
(*     orphan class).                                                      *)
(*   - Inv_NoRenewingHolderDeposed: a holder whose renews land is never    *)
(*     taken over — token rotation's teeth (NoRotate mutation must         *)
(*     rediscover real-S3 gate bug 1).                                     *)
(*   - DeposedEventuallyFenced: a deposed incarnation eventually stops     *)
(*     believing it holds (heartbeat 412 => exit; the NoFence liveness     *)
(*     mutation must find the immortal-zombie lasso).                      *)
(*   - DeadHolderWatchResolves: a watcher over a dead holder's frozen      *)
(*     token eventually takes over.                                        *)
(*                                                                         *)
(* THE RESIDUAL (probe run, REQUIRED TO FAIL against the unmutated         *)
(* model — the phase-H window, stated exactly):                            *)
(*   - Inv_NoStalePublishLand: a deposed-but-unfenced incarnation's        *)
(*     publish lands (the fresh-key If-None-Match:* create has no base     *)
(*     for CAS to fence and the sweep only kills assemblies that EXIST     *)
(*     at claim time).  Phase H measured the heartbeat's single CAS        *)
(*     beating the flusher's multi-step publish; this probe is the         *)
(*     interleaving where it loses.                                        *)
(*                                                                         *)
(* THE THEOREM THIS MODULE FORCED INTO THE CODE (StampCheck):              *)
(*   - Inv_NoSuccessorOverwrite — a zombie's If-Match publish landing      *)
(*     OVER the successor's object — began as the second required-fail     *)
(*     probe.  TLC found two routes sharper than the drill intuition:      *)
(*     the 412 rediscovery arm adopting the successor's etag, and a        *)
(*     FRESH-world hub frozen mid-claim whose wake-up import ingests       *)
(*     the successor's etag so its first flush lands with a SUCCEEDING     *)
(*     condition (no 412 ever fires).  The fix is two-legged and both      *)
(*     legs are modeled under StampCheck: flush.rs successor_check (a      *)
(*     stamp above ours store-verifies, then fences; a FABRICATED stamp    *)
(*     — the store still shows our reign — keeps A6 local-wins, so an      *)
(*     outside writer cannot crash-loop a healthy hub) and epoch.rs        *)
(*     startup_reverify (serve() refuses to proceed past a store epoch     *)
(*     ahead of the claim — the SweepDone arm here).  The NoStampCheck     *)
(*     mutation preserves the pre-fix counterexample as the regression     *)
(*     test.                                                               *)
(*                                                                         *)
(* Out of scope: transport failures and the lease-window self-fence        *)
(* (chaos phase B/L own them; no lossy store here), content/CRC            *)
(* (FlintTierMarker's axis), tombstones/re-key (FlintTierArbitrate's),     *)
(* multi-client NFS semantics (not a bucket protocol).                     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Hubs,        \* hub incarnation slots, e.g. {h1, h2} — distinct holder_ids
  Misses,      \* lease_misses: quiet polls before a foreign holder is dead
  MaxPubs,     \* publish-attempt budget (each plan consumes one)
  MaxCrashes,  \* process-death budget
  MaxRenews,   \* heartbeat-renew budget (bounds tokens)
  MaxClaims,   \* claim budget (bounds the epoch ping-pong of two hubs
               \* endlessly re-judging each other — each acquire, self-
               \* recognition, or takeover consumes one)
  Sweep,       \* TRUE = every claim aborts all in-flight assemblies (A8)
  TokenRotate, \* TRUE = renew rotates the CAS token (real-S3 bug 1's fix)
  FenceOn412,  \* TRUE = a failed renew fences + exits the incarnation
  StampCheck,  \* TRUE = a 412's arbitration fences on a foreign object
  FenceBeforeRelease \* TRUE = a clean shutdown FENCES before marking the
                     \* cell released (the shipped order). FALSE marks
                     \* first and keeps holding — the straggler window
                     \* Inv_NoPostReleaseLand exists to forbid.
               \* stamped with an epoch ABOVE ours (the successor fence
               \* this module's ProbeOverwrite forced into the code —
               \* store-verified there; the model's dataEp is never
               \* forged, so the verify's fabrication lane needs no
               \* modeling)

VARIABLES
  \* ── the store (each action touching it is one CAS round) ──
  epochObj,    \* [held, holder, ep, tok, renews, released] — the epoch cell
  nextTok,     \* token generator (etags are unforgeable + fresh)
  dataTag,     \* the one data key's etag lineage; 0 = absent
  dataEp,      \* epoch stamped on the current data object (0 = absent)
  uploads,     \* in-flight assemblies: {[hub, cond, epInit]}
  sweptEp,     \* highest epoch whose OWN holder completed its sweep
  \* ── per-hub incarnation state (wiped by crash/exit) ──
  st,          \* [Hubs -> {"idle","watching","claimed","holding"}]
  lease,       \* [Hubs -> [ep, tok]] — the believed lease (0s when idle)
  lastTok,     \* [Hubs -> Nat] — claim-loop last observed token
  quiet,       \* [Hubs -> 0..Misses] — consecutive unchanged observations
  renewsSeen,  \* [Hubs -> Nat] — store renew count at last token change
  baseTag,     \* [Hubs -> Nat] — believed data etag (registry / HEAD)
  flush,       \* [Hubs -> [active, stage, kind, cond, epInit]]
  \* ── budgets + history witnesses ──
  pubs, crashes, renewBudget, claimBudget,
  preSweepMpuLand,        \* T1 witness
  renewingHolderDeposed,  \* T2 witness
  stalePublishLand,       \* probe 1 witness (the phase-H window)
  succOverwrite,          \* probe 2 witness (local-wins vs the successor)
  postReleaseLand         \* T3 witness: a publish landed from a reign that
                          \* had already marked its cell released

vars == <<epochObj, nextTok, dataTag, dataEp, uploads, sweptEp, st, lease, lastTok,
          quiet, renewsSeen, baseTag, flush, pubs, crashes, renewBudget,
          claimBudget, preSweepMpuLand, renewingHolderDeposed, stalePublishLand,
          succOverwrite, postReleaseLand>>

NoFlush   == [active |-> FALSE, stage |-> "none", kind |-> "none",
              cond |-> 0, epInit |-> 0]
ZeroLease == [ep |-> 0, tok |-> 0]

TypeOK ==
  /\ epochObj \in [held: BOOLEAN, holder: Hubs, ep: Nat, tok: Nat, renews: Nat,
                   released: BOOLEAN]
  /\ nextTok \in Nat
  /\ dataTag \in Nat /\ dataEp \in Nat
  /\ uploads \subseteq [hub: Hubs, cond: Nat, epInit: Nat]
  /\ sweptEp \in Nat
  /\ st \in [Hubs -> {"idle", "watching", "claimed", "holding"}]
  /\ lease \in [Hubs -> [ep: Nat, tok: Nat]]
  /\ lastTok \in [Hubs -> Nat]
  /\ quiet \in [Hubs -> 0..Misses]
  /\ renewsSeen \in [Hubs -> Nat]
  /\ baseTag \in [Hubs -> Nat]
  /\ flush \in [Hubs -> [active: BOOLEAN, stage: {"none", "planned", "initiated"},
                         kind: {"none", "put", "mpu"}, cond: Nat, epInit: Nat]]
  /\ pubs \in 0..MaxPubs /\ crashes \in 0..MaxCrashes
  /\ renewBudget \in 0..MaxRenews
  /\ claimBudget \in 0..MaxClaims
  /\ preSweepMpuLand \in BOOLEAN /\ renewingHolderDeposed \in BOOLEAN
  /\ stalePublishLand \in BOOLEAN /\ succOverwrite \in BOOLEAN

Init ==
  /\ epochObj = [held |-> FALSE, holder |-> CHOOSE h \in Hubs : TRUE,
                 ep |-> 0, tok |-> 0, renews |-> 0, released |-> FALSE]
  /\ nextTok = 1
  /\ dataTag = 0 /\ dataEp = 0
  /\ uploads = {}
  /\ sweptEp = 0
  /\ st = [h \in Hubs |-> "idle"]
  /\ lease = [h \in Hubs |-> ZeroLease]
  /\ lastTok = [h \in Hubs |-> 0]
  /\ quiet = [h \in Hubs |-> 0]
  /\ renewsSeen = [h \in Hubs |-> 0]
  /\ baseTag = [h \in Hubs |-> 0]
  /\ flush = [h \in Hubs |-> NoFlush]
  /\ pubs = 0 /\ crashes = 0 /\ renewBudget = MaxRenews
  /\ claimBudget = MaxClaims
  /\ preSweepMpuLand = FALSE /\ renewingHolderDeposed = FALSE
  /\ stalePublishLand = FALSE /\ succOverwrite = FALSE
  /\ postReleaseLand = FALSE

unchangedWitnesses ==
  UNCHANGED <<preSweepMpuLand, renewingHolderDeposed, stalePublishLand,
              succOverwrite, postReleaseLand>>

(***************************************************************************)
(* The claim.  Create and supersede are read+CAS pairs collapsed to one    *)
(* action each (the CAS revalidates the read — see the header note); the   *)
(* quiet count is honestly multi-round.                                    *)
(***************************************************************************)

AcquireCreate(h) ==
  /\ st[h] = "idle"
  /\ claimBudget > 0
  /\ claimBudget' = claimBudget - 1
  /\ ~epochObj.held
  /\ epochObj' = [held |-> TRUE, holder |-> h, ep |-> 1,
                  tok |-> nextTok, renews |-> 0, released |-> FALSE]
  /\ lease' = [lease EXCEPT ![h] = [ep |-> 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ st' = [st EXCEPT ![h] = "claimed"]
  /\ UNCHANGED <<dataTag, dataEp, uploads, sweptEp, lastTok, quiet, renewsSeen,
                 baseTag, flush, pubs, crashes, renewBudget>>
  /\ unchangedWitnesses

\* A8 self-recognition: the store carries OUR holder_id — a previous
\* incarnation died holding.  Supersede immediately, no wait.
SelfRecognize(h) ==
  /\ st[h] = "idle"
  /\ claimBudget > 0
  /\ claimBudget' = claimBudget - 1
  /\ epochObj.held /\ epochObj.holder = h
  /\ epochObj' = [held |-> TRUE, holder |-> h, ep |-> epochObj.ep + 1,
                  tok |-> nextTok, renews |-> 0, released |-> FALSE]
  /\ lease' = [lease EXCEPT ![h] = [ep |-> epochObj.ep + 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ st' = [st EXCEPT ![h] = "claimed"]
  /\ UNCHANGED <<dataTag, dataEp, uploads, sweptEp, lastTok, quiet, renewsSeen,
                 baseTag, flush, pubs, crashes, renewBudget>>
  /\ unchangedWitnesses

ObserveForeign(h) ==
  /\ st[h] = "idle"
  /\ epochObj.held /\ epochObj.holder # h
  /\ st' = [st EXCEPT ![h] = "watching"]
  /\ lastTok' = [lastTok EXCEPT ![h] = epochObj.tok]
  /\ quiet' = [quiet EXCEPT ![h] = 0]
  /\ renewsSeen' = [renewsSeen EXCEPT ![h] = epochObj.renews]
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, uploads, sweptEp, lease,
                 baseTag, flush, pubs, crashes, renewBudget, claimBudget>>
  /\ unchangedWitnesses

PollQuiet(h) ==
  /\ st[h] = "watching"
  /\ IF epochObj.tok = lastTok[h]
       THEN /\ quiet[h] < Misses          \* capped: further polls are no-ops
            /\ quiet' = [quiet EXCEPT ![h] = quiet[h] + 1]
            /\ UNCHANGED <<lastTok, renewsSeen>>
       ELSE /\ lastTok' = [lastTok EXCEPT ![h] = epochObj.tok]
            /\ quiet' = [quiet EXCEPT ![h] = 0]
            /\ renewsSeen' = [renewsSeen EXCEPT ![h] = epochObj.renews]
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, uploads, sweptEp, st,
                 lease, baseTag, flush, pubs, crashes, renewBudget,
                 claimBudget>>
  /\ unchangedWitnesses

\* Takeover: the CAS (If-Match on the last observed token) IS the guard;
\* the witness fires when the store-side holder renewed after this
\* observer's snapshot — with rotation that interleaving is refused by
\* the CAS itself, which is exactly the theorem.
Takeover(h) ==
  /\ st[h] = "watching"
  /\ claimBudget > 0
  /\ claimBudget' = claimBudget - 1
  /\ quiet[h] >= Misses
  /\ epochObj.held /\ epochObj.tok = lastTok[h]
  /\ renewingHolderDeposed' =
       (renewingHolderDeposed \/ epochObj.renews > renewsSeen[h])
  /\ epochObj' = [held |-> TRUE, holder |-> h, ep |-> epochObj.ep + 1,
                  tok |-> nextTok, renews |-> 0, released |-> FALSE]
  /\ lease' = [lease EXCEPT ![h] = [ep |-> epochObj.ep + 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ st' = [st EXCEPT ![h] = "claimed"]
  /\ UNCHANGED <<dataTag, dataEp, uploads, sweptEp, lastTok, quiet,
                 renewsSeen, baseTag, flush, pubs, crashes, renewBudget,
                 preSweepMpuLand, stalePublishLand, succOverwrite,
                 postReleaseLand>>

(***************************************************************************)
(* The clean handoff.  A hub that is shutting down flushes, FENCES itself, *)
(* and only then CAS-marks its cell released; a successor may claim a      *)
(* released cell with no quiet wait.                                       *)
(*                                                                         *)
(* The order is the whole theorem.  Marking before fencing would leave a   *)
(* window where the cell invites an instant successor while the outgoing   *)
(* hub can still land a publish — Inv_NoPostReleaseLand is exactly that    *)
(* straggler, specialized from FlintTierSession's Inv_NoStragglerLand to   *)
(* the single-cell protocol.  The release is guarded on the holder's OWN   *)
(* token, so a deposed hub cannot mark a live successor's reign.           *)
(***************************************************************************)

CleanRelease(h) ==
  /\ st[h] = "holding"
  /\ ~flush[h].active                     \* the final flush has completed
  /\ epochObj.held
  /\ epochObj.tok = lease[h].tok           \* the CAS: still our reign
  /\ epochObj' = [epochObj EXCEPT !.released = TRUE, !.tok = nextTok]
  /\ nextTok' = nextTok + 1
  \* Fenced BEFORE the mark is observable: idle with a zero lease is the
  \* model's fence — every publish guard below reads through lease[h].
  \* FenceBeforeRelease = FALSE is the mutation: mark the cell but keep
  \* holding, i.e. the order the code must NOT ship. It invites a
  \* successor while this reign can still land a publish.
  /\ IF FenceBeforeRelease
       THEN /\ st' = [st EXCEPT ![h] = "idle"]
            /\ lease' = [lease EXCEPT ![h] = ZeroLease]
       ELSE /\ lease' = [lease EXCEPT ![h] = [ep |-> lease[h].ep, tok |-> nextTok]]
            /\ UNCHANGED st
  /\ UNCHANGED <<dataTag, dataEp, uploads, sweptEp, lastTok, quiet, renewsSeen,
                 baseTag, flush, pubs, crashes, renewBudget, claimBudget>>
  /\ unchangedWitnesses

\* A released cell is claimable on sight: its holder has proven it is
\* finished, so waiting out Misses polls would buy nothing.  This is what
\* makes a wake-from-hibernation fast — the woken hub has a FRESH
\* server_id (its PVC was deleted), so SelfRecognize cannot fire for it.
ClaimReleased(h) ==
  /\ st[h] \in {"idle", "watching"}
  /\ claimBudget > 0
  /\ claimBudget' = claimBudget - 1
  /\ epochObj.held /\ epochObj.released
  /\ epochObj' = [held |-> TRUE, holder |-> h, ep |-> epochObj.ep + 1,
                  tok |-> nextTok, renews |-> 0, released |-> FALSE]
  /\ lease' = [lease EXCEPT ![h] = [ep |-> epochObj.ep + 1, tok |-> nextTok]]
  /\ nextTok' = nextTok + 1
  /\ st' = [st EXCEPT ![h] = "claimed"]
  /\ UNCHANGED <<dataTag, dataEp, uploads, sweptEp, lastTok, quiet, renewsSeen,
                 baseTag, flush, pubs, crashes, renewBudget>>
  /\ unchangedWitnesses

\* Every claim path sweeps before holding (claim() runs takeover_sweep on
\* all three arms); the successor's import seeds its data-etag belief.
\* The acquire and the sweep are SEPARATE store requests: a pre-takeover
\* Complete can land in between (the strict run's first counterexample —
\* the guarantee starts when the sweep RETURNS, so sweptEp marks the
\* reign clean only then).  A deposed-in-claimed zombie's sweep still
\* clears CURRENT uploads (aborts are unconditional): it can abort the
\* live successor's in-flight assembly — one failed flush cycle, retried
\* next tick; disruption, never loss — and does NOT mark the reign.
SweepDone(h) ==
  /\ st[h] = "claimed"
  /\ uploads' = IF Sweep THEN {} ELSE uploads
  /\ sweptEp' = IF epochObj.held /\ epochObj.holder = h
                    /\ epochObj.ep = lease[h].ep
                  THEN lease[h].ep ELSE sweptEp
  \* Registry rows are DURABLE: a re-claiming hub keeps its own etag
  \* beliefs (orch.startup() rebuilds from local rows); only a fresh
  \* incarnation (no rows) imports its belief from the bucket — and
  \* WITH StampCheck the import-start epoch verify fences a hub whose
  \* claim completed into a world a successor already published
  \* (the strict run's second counterexample: a FRESH-world hub frozen
  \* mid-claim imports the successor's etag and its first flush lands
  \* with a SUCCEEDING condition — the 412-arm check never runs).
  /\ IF StampCheck /\ dataEp > lease[h].ep
       THEN /\ st' = [st EXCEPT ![h] = "idle"]
            /\ lease' = [lease EXCEPT ![h] = ZeroLease]
            /\ UNCHANGED baseTag
       ELSE /\ baseTag' =
                 [baseTag EXCEPT ![h] = IF baseTag[h] = 0 THEN dataTag ELSE @]
            /\ st' = [st EXCEPT ![h] = "holding"]
            /\ UNCHANGED lease
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, lastTok, quiet,
                 renewsSeen, flush, pubs, crashes, renewBudget, claimBudget>>
  /\ unchangedWitnesses

(***************************************************************************)
(* The heartbeat.  Success rotates the token (TokenRotate) and counts the  *)
(* renew; failure means deposed — fence + exit(70) collapsed to one step   *)
(* (see the header note).  FenceOn412=FALSE is the mutation: the 412 is   *)
(* swallowed and the zombie never stops believing.                         *)
(***************************************************************************)

RenewOk(h) ==
  /\ st[h] = "holding"
  /\ renewBudget > 0
  /\ epochObj.held /\ epochObj.tok = lease[h].tok
  /\ LET t == IF TokenRotate THEN nextTok ELSE epochObj.tok IN
       /\ epochObj' = [epochObj EXCEPT !.tok = t, !.renews = @ + 1]
       /\ lease' = [lease EXCEPT ![h].tok = t]
  /\ nextTok' = nextTok + 1
  /\ renewBudget' = renewBudget - 1
  /\ UNCHANGED <<dataTag, dataEp, uploads, sweptEp, st, lastTok, quiet,
                 renewsSeen, baseTag, flush, pubs, crashes, claimBudget>>
  /\ unchangedWitnesses

RenewDeposed(h) ==
  /\ st[h] = "holding"
  /\ FenceOn412
  /\ ~(epochObj.held /\ epochObj.tok = lease[h].tok)
  /\ st' = [st EXCEPT ![h] = "idle"]
  /\ lease' = [lease EXCEPT ![h] = ZeroLease]
  /\ flush' = [flush EXCEPT ![h] = NoFlush]
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, uploads, sweptEp,
                 lastTok, quiet, renewsSeen, baseTag, pubs, crashes,
                 renewBudget, claimBudget>>
  /\ unchangedWitnesses

(***************************************************************************)
(* The flush pipeline, decomposed at store-request granularity: plan       *)
(* (tick + guard checks pass, condition captured from the registry         *)
(* belief), then the store step(s).  A deposition interleaving between     *)
(* plan and land is the whole point.  A failed publish HEAD-rediscovers    *)
(* the current etag (A6 arbitration) — which is how the zombie learns      *)
(* the SUCCESSOR'S etag and probe 2 fires.                                 *)
(***************************************************************************)

Stale(h) ==
  ~(epochObj.held /\ epochObj.holder = h /\ epochObj.ep = lease[h].ep)

CondOK(c) == IF c = 0 THEN dataTag = 0 ELSE dataTag = c

FlushPlan(h) ==
  /\ st[h] = "holding"
  /\ ~flush[h].active
  /\ pubs < MaxPubs
  /\ \E k \in {"put", "mpu"} :
       flush' = [flush EXCEPT ![h] = [active |-> TRUE, stage |-> "planned",
                                      kind |-> k, cond |-> baseTag[h],
                                      epInit |-> 0]]
  /\ pubs' = pubs + 1
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, uploads, sweptEp, st,
                 lease, lastTok, quiet, renewsSeen, baseTag, crashes,
                 renewBudget, claimBudget>>
  /\ unchangedWitnesses

\* A publish landing from the very reign whose cell already says
\* "finished". The release is a barrier: if this can be set, a successor
\* invited in by the mark can be overwritten by its predecessor.
PostRelease(h) ==
  /\ epochObj.released
  /\ epochObj.ep = lease[h].ep
  /\ epochObj.holder = h

LandWitnesses(h, c) ==
  /\ stalePublishLand' = (stalePublishLand \/ Stale(h))
  /\ succOverwrite' =
       (succOverwrite \/ (Stale(h) /\ c # 0 /\ dataEp > lease[h].ep))
  /\ postReleaseLand' = (postReleaseLand \/ PostRelease(h))
  /\ UNCHANGED renewingHolderDeposed

\* The 412 rediscovery arm carries the SUCCESSOR FENCE (StampCheck):
\* an observed object stamped above our epoch is machine-readable
\* deposition — fence + exit (one step, like RenewDeposed) instead of
\* adopting the successor's etag for a local-wins overwrite.
PutLand(h) ==
  /\ flush[h].active /\ flush[h].stage = "planned" /\ flush[h].kind = "put"
  /\ IF CondOK(flush[h].cond)
       THEN /\ dataTag' = dataTag + 1
            /\ dataEp' = lease[h].ep
            /\ baseTag' = [baseTag EXCEPT ![h] = dataTag + 1]
            /\ LandWitnesses(h, flush[h].cond)
            /\ UNCHANGED <<preSweepMpuLand, st, lease>>
       ELSE IF StampCheck /\ dataEp > lease[h].ep
       THEN /\ st' = [st EXCEPT ![h] = "idle"]
            /\ lease' = [lease EXCEPT ![h] = ZeroLease]
            /\ UNCHANGED <<dataTag, dataEp, baseTag>>
            /\ unchangedWitnesses
       ELSE /\ baseTag' = [baseTag EXCEPT ![h] = dataTag]   \* HEAD rediscover
            /\ UNCHANGED <<dataTag, dataEp, st, lease>>
            /\ unchangedWitnesses
  /\ flush' = [flush EXCEPT ![h] = NoFlush]
  /\ UNCHANGED <<epochObj, nextTok, uploads, sweptEp, lastTok,
                 quiet, renewsSeen, pubs, crashes, renewBudget, claimBudget>>

MpuInit(h) ==
  /\ flush[h].active /\ flush[h].stage = "planned" /\ flush[h].kind = "mpu"
  /\ uploads' = uploads \cup
       {[hub |-> h, cond |-> flush[h].cond, epInit |-> epochObj.ep]}
  /\ flush' = [flush EXCEPT ![h].stage = "initiated",
                            ![h].epInit = epochObj.ep]
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, sweptEp, st, lease,
                 lastTok, quiet, renewsSeen, baseTag, pubs, crashes,
                 renewBudget, claimBudget>>
  /\ unchangedWitnesses

\* Complete: NoSuchUpload first (the sweep's fence), then the same
\* conditional check a plain PUT gets; failure aborts the assembly
\* (A9 abort-on-every-failure) and HEAD-rediscovers.
MpuComplete(h) ==
  /\ flush[h].active /\ flush[h].stage = "initiated"
  /\ LET u == [hub |-> h, cond |-> flush[h].cond, epInit |-> flush[h].epInit]
         fenceOnStamp == StampCheck /\ dataEp > lease[h].ep IN
     IF u \notin uploads
       THEN \* NoSuchUpload — fenced by a takeover sweep; the follow-up
            \* arbitration HEAD sees the same stamps a 412 would.
            IF fenceOnStamp
              THEN /\ st' = [st EXCEPT ![h] = "idle"]
                   /\ lease' = [lease EXCEPT ![h] = ZeroLease]
                   /\ UNCHANGED <<dataTag, dataEp, uploads, baseTag>>
                   /\ unchangedWitnesses
              ELSE /\ baseTag' = [baseTag EXCEPT ![h] = dataTag]
                   /\ UNCHANGED <<dataTag, dataEp, uploads, st, lease>>
                   /\ unchangedWitnesses
       ELSE IF CondOK(u.cond)
         THEN /\ uploads' = uploads \ {u}
              /\ dataTag' = dataTag + 1
              /\ dataEp' = lease[h].ep
              /\ baseTag' = [baseTag EXCEPT ![h] = dataTag + 1]
              /\ preSweepMpuLand' =
                   (preSweepMpuLand
                    \/ (epochObj.ep > u.epInit /\ sweptEp >= epochObj.ep))
              /\ LandWitnesses(h, u.cond)
              /\ UNCHANGED <<st, lease>>
         ELSE IF fenceOnStamp
           THEN /\ uploads' = uploads \ {u}      \* abort-on-failure
                /\ st' = [st EXCEPT ![h] = "idle"]
                /\ lease' = [lease EXCEPT ![h] = ZeroLease]
                /\ UNCHANGED <<dataTag, dataEp, baseTag>>
                /\ unchangedWitnesses
           ELSE /\ uploads' = uploads \ {u}
                /\ baseTag' = [baseTag EXCEPT ![h] = dataTag]
                /\ UNCHANGED <<dataTag, dataEp, st, lease>>
                /\ unchangedWitnesses
  /\ flush' = [flush EXCEPT ![h] = NoFlush]
  /\ UNCHANGED <<epochObj, nextTok, sweptEp, lastTok, quiet,
                 renewsSeen, pubs, crashes, renewBudget, claimBudget>>

(***************************************************************************)
(* The environment: process death.  In-memory state vanishes; the epoch    *)
(* cell, the data object, and any initiated assemblies (the orphans the    *)
(* sweep exists for) remain.                                               *)
(***************************************************************************)

Crash(h) ==
  /\ st[h] # "idle"
  /\ crashes < MaxCrashes
  /\ st' = [st EXCEPT ![h] = "idle"]
  /\ lease' = [lease EXCEPT ![h] = ZeroLease]
  /\ flush' = [flush EXCEPT ![h] = NoFlush]
  /\ quiet' = [quiet EXCEPT ![h] = 0]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<epochObj, nextTok, dataTag, dataEp, uploads, sweptEp,
                 lastTok, renewsSeen, baseTag, pubs, renewBudget,
                 claimBudget>>
  /\ unchangedWitnesses

Next ==
  \E h \in Hubs :
    \/ AcquireCreate(h) \/ SelfRecognize(h) \/ ObserveForeign(h)
    \/ PollQuiet(h) \/ Takeover(h) \/ SweepDone(h)
    \/ CleanRelease(h) \/ ClaimReleased(h)
    \/ RenewOk(h) \/ RenewDeposed(h)
    \/ FlushPlan(h) \/ PutLand(h) \/ MpuInit(h) \/ MpuComplete(h)
    \/ Crash(h)

\* Protocol machinery is weakly fair; crashes are the environment.
\* RenewOk is deliberately NOT fair (a holder may stall forever — the
\* zombie premise); RenewDeposed IS (a deposed incarnation's heartbeat
\* eventually runs, and its failure is the fence).
Fairness ==
  \A h \in Hubs :
    /\ WF_vars(AcquireCreate(h)) /\ WF_vars(SelfRecognize(h))
    /\ WF_vars(ObserveForeign(h)) /\ WF_vars(PollQuiet(h))
    /\ WF_vars(Takeover(h)) /\ WF_vars(SweepDone(h))
    /\ WF_vars(ClaimReleased(h))
    /\ WF_vars(RenewDeposed(h))
    /\ WF_vars(PutLand(h)) /\ WF_vars(MpuInit(h)) /\ WF_vars(MpuComplete(h))

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* Theorems (strict) and residual probes (required-fail).                  *)
(***************************************************************************)

Inv_NoPreSweepMpuLand      == ~preSweepMpuLand
Inv_NoRenewingHolderDeposed == ~renewingHolderDeposed
\* The clean handoff is a BARRIER: no publish from a reign that has
\* already marked its cell released. The mark invites an immediate
\* successor, so a straggler landing after it would overwrite a live
\* hub's object. Discharged by CleanRelease fencing (idle + zero lease)
\* in the same step it writes the mark.
Inv_NoPostReleaseLand      == ~postReleaseLand

\* PROBES — reachable in the shipped protocol; kept OUT of the strict
\* run's invariant list.  Probe 1 is the phase-H wake-up window; probe 2
\* is the local-wins-vs-successor gap (code-fix candidate: the arbitrate
\* Foreign arm should compare epoch stamps).
Inv_NoStalePublishLand   == ~stalePublishLand
Inv_NoSuccessorOverwrite == ~succOverwrite

Inv == TypeOK /\ Inv_NoPreSweepMpuLand /\ Inv_NoRenewingHolderDeposed
       /\ Inv_NoPostReleaseLand
       /\ Inv_NoSuccessorOverwrite

\* A deposed incarnation eventually stops believing it holds (heartbeat
\* fence, or a crash) — the bound on every stale-publish window.
DeposedHolding(h) ==
  st[h] = "holding" /\ ~(epochObj.held /\ epochObj.tok = lease[h].tok)

DeposedEventuallyFenced ==
  \A h \in Hubs : [](DeposedHolding(h) => <>(~DeposedHolding(h)))

\* A watcher over a dead holder's frozen token resolves the watch: it
\* claims, or the configuration otherwise breaks (the holder restarts,
\* the watcher crashes).
DeadHolderWatchResolves ==
  \A h \in Hubs : \A g \in Hubs \ {h} :
    []((st[h] = "watching" /\ epochObj.held /\ epochObj.holder = g
        /\ st[g] = "idle")
       => <>(st[h] # "watching" \/ st[g] # "idle" \/ claimBudget = 0))

================================================================================
