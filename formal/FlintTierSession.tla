--------------------------- MODULE FlintTierSession ---------------------------
(***************************************************************************)
(* The multi-volume hub's TWO-LEVEL LEASE — model BEFORE code (the         *)
(* FlintExtents/FlintComposition posture), step 0 of                       *)
(* docs/plans/multi-volume-hub-design.md.                                  *)
(*                                                                         *)
(* WHY THE LAYER EXISTS: one hub serving N volumes cannot heartbeat N      *)
(* epoch cells (1,000 volumes x one PUT/10s ~ $1,300/mo of heartbeats).    *)
(* The design moves liveness to ONE session cell per hub                   *)
(* (.flint-hubs/<hub-id>) and demotes each volume's cell to a CAS record   *)
(* {owner hub, session generation, claim generation}: claims and releases  *)
(* are per-session events, the timer traffic is O(hubs).  This module      *)
(* asks whether the single-cell theorems FlintTierEpoch proved survive     *)
(* the indirection — because the assumption that broke three times         *)
(* elsewhere ("the abstraction was the bug") breaks here too if unproven:  *)
(* the volume claimant no longer rewrites the cell the loser's HEARTBEAT   *)
(* watches.                                                                *)
(*                                                                         *)
(* THE CENTRAL FACT (S3, store/memory.rs alike): CAS conditions apply to   *)
(* ONE object.  A takeover cannot atomically bind "the session is quiet"   *)
(* to "the volume cell is mine" — they are different keys.  The protocol   *)
(* therefore DEPOSES FIRST: the watcher CAS-writes a deposed flag into    *)
(* the owner's SESSION cell (If-Match the quiet-observed token) and only   *)
(* then claims volume cells naming that session.  Depose converts the      *)
(* watcher's flaky evidence (a quiet count in its own memory) into STABLE  *)
(* STORE STATE before any volume changes hands; the loser's next beat is   *)
(* a CAS mismatch => fence => exit(70), hub-scoped — one failed beat       *)
(* forfeits ALL its volumes at once.                                       *)
(*                                                                         *)
(* FAITHFULNESS NOTES (the epoch module's rules, inherited):               *)
(*   - Read-then-CAS pairs collapse into one action WHERE THE CAS          *)
(*     revalidates the read (session start/beat/depose, every volume-cell  *)
(*     claim and release).  The quiet COUNT is multi-round state and       *)
(*     stays decomposed (ObserveSess / PollQuietS / DeposeSess).           *)
(*   - The watch arms ONCE (ObserveSess requires an unarmed watcher) and   *)
(*     re-syncs only when the observed token actually moved — otherwise    *)
(*     WF(ObserveSess) would fairly reset the quiet count forever and      *)
(*     starve every takeover in the liveness runs.                         *)
(*   - beat-fail => fence => exit(70) is ONE step (BeatFail), as           *)
(*     RenewDeposed is in FlintTierEpoch.                                  *)
(*   - The DATA PLANE IS OUT OF SCOPE: publishes here are witness-only     *)
(*     (plan captures the volume, land compares the store's cell).  The    *)
(*     per-object CAS/stamp arbitration a landed publish then faces is     *)
(*     FlintTierEpoch's proven axis (successor_check, StampCheck) and is   *)
(*     unchanged by this layer — one volume's flush pipeline against its   *)
(*     cell is exactly that module with "epoch cell" read as "volume       *)
(*     cell".  What THIS module owns is the indirection: session           *)
(*     liveness judged in one place, volume ownership recorded in          *)
(*     another.                                                            *)
(*                                                                         *)
(* THE THEOREMS (strict run):                                              *)
(*   - Inv_NoBeatingSessionDeposed: a session whose beats land is never    *)
(*     deposed — beat-time token rotation's teeth at the session layer     *)
(*     (the NoRotate mutation must rediscover real-S3 gate bug 1's        *)
(*     shape one level up).                                                *)
(*   - Inv_NoStragglerLand: a clean release is a BARRIER — no publish      *)
(*     planned before it lands after it (drain-on-release's teeth; the     *)
(*     NoDrain mutation must rediscover the straggler landing under the    *)
(*     next owner's reign).  Clean release is what makes handoff skip     *)
(*     the lease wait, so its drain is load-bearing.                       *)
(*   - ZombieOwnershipResolves (liveness): a hub that lost a volume        *)
(*     eventually stops believing it owns it.  STRICT: the only door out   *)
(*     of a live hub's ownership is a depose, depose is stable store       *)
(*     state, and WF(BeatFail) turns it into the hub-scoped fence.  The    *)
(*     NoDepose mutation — takeover straight off the quiet count,          *)
(*     session cell left alive — must find the IMMORTAL MULTI-VOLUME      *)
(*     ZOMBIE: the loser's beats keep SUCCEEDING, no fence ever fires,     *)
(*     and it believes (and publishes) forever.  That lasso is this        *)
(*     module's reason to exist: it is the naive two-level lease, and it   *)
(*     is unsound.  The NoFence mutation finds the same lasso through     *)
(*     the other door (depose lands, the 412 is swallowed).                *)
(*   - DeadSessionWatchResolves (liveness): a down hub's volume with a     *)
(*     live watcher is eventually claimed (or a budget honestly ends the   *)
(*     world).                                                             *)
(*                                                                         *)
(* THE RESIDUAL (probe run, REQUIRED TO FAIL against the unmutated         *)
(* model): Inv_NoStaleLand — a publish planned under a valid reign lands   *)
(* after depose+takeover (plan and land are separate steps; no cross-      *)
(* object CAS closes it here).  Same shape as FlintTierEpoch's            *)
(* ProbeStale; the window is BOUNDED by ZombieOwnershipResolves and the    *)
(* landed object is arbitrated by the data plane's stamps (that module's   *)
(* NoSuccessorOverwrite theorem) — the residual costs a flush cycle,       *)
(* never correctness.                                                      *)
(*                                                                         *)
(* Out of scope: fork/base references and manifest provenance (no          *)
(* protocol — forks pin immutable versionIds), satellite refresh (reads    *)
(* only), MPU sweep + import (FlintTierEpoch), marker visibility           *)
(* (FlintTierMarker), transport loss (chaos phase B/L).                    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Hubs,           \* exactly TWO hub identities (each watches the other)
  Vols,           \* volume slots, e.g. {v1, v2}
  Misses,         \* quiet polls before a foreign SESSION is judged dead
  MaxBeats,       \* heartbeat budget (bounds tokens)
  MaxPubs,        \* publish-plan budget
  MaxCrashes,     \* process-death budget
  MaxClaims,      \* claim/depose budget (bounds the ping-pong)
  Depose,         \* TRUE = takeover deposes the owner's SESSION first
  SessRotate,     \* TRUE = a beat rotates the session token
  FenceOnFail,    \* TRUE = a failed beat fences the hub (exit(70))
  DrainOnRelease  \* TRUE = release refuses while that volume's flush flies

ASSUME Cardinality(Hubs) = 2

Other(h) == CHOOSE g \in Hubs \ {h} : TRUE

VARIABLES
  \* ── the store (every action touching it is one CAS round) ──
  sess,      \* [Hubs -> [gen, tok, beats, deposed]] — the session cells
  vcell,     \* [Vols -> [mode, owner, sgen, claim]] — the volume cells
  nextTok,   \* token generator (etags unforgeable + fresh)
  \* ── per-hub incarnation state (wiped by crash/fence) ──
  hst,       \* [Hubs -> {"down", "up"}]
  myGen,     \* [Hubs -> Nat] — believed session generation (0 down)
  owned,     \* [Hubs -> SUBSET Vols] — believed-owned volumes
  pfl,       \* [Hubs -> [active, vol, rel]] — one in-flight publish
  watch,     \* [Hubs -> [tok, quiet, beats]] — session watch (0 = unarmed)
  \* ── budgets + history witnesses ──
  beatBudget, pubs, crashes, claimBudget,
  beatingSessDeposed,  \* T1 witness
  stragglerLand,       \* T2 witness (release drain's teeth)
  staleLand            \* probe witness (the plan/land window)

vars == <<sess, vcell, nextTok, hst, myGen, owned, pfl, watch, beatBudget,
          pubs, crashes, claimBudget, beatingSessDeposed, stragglerLand,
          staleLand>>

NoF     == [active |-> FALSE, vol |-> CHOOSE v \in Vols : TRUE, rel |-> FALSE]
NoWatch == [tok |-> 0, quiet |-> 0, beats |-> 0]

TypeOK ==
  /\ sess \in [Hubs -> [gen: Nat, tok: Nat, beats: Nat, deposed: BOOLEAN]]
  /\ vcell \in [Vols -> [mode: {"free", "owned", "released"},
                         owner: Hubs, sgen: Nat, claim: Nat]]
  /\ nextTok \in Nat
  /\ hst \in [Hubs -> {"down", "up"}]
  /\ myGen \in [Hubs -> Nat]
  /\ owned \in [Hubs -> SUBSET Vols]
  /\ pfl \in [Hubs -> [active: BOOLEAN, vol: Vols, rel: BOOLEAN]]
  /\ watch \in [Hubs -> [tok: Nat, quiet: 0..Misses, beats: Nat]]
  /\ beatBudget \in 0..MaxBeats /\ pubs \in 0..MaxPubs
  /\ crashes \in 0..MaxCrashes /\ claimBudget \in 0..MaxClaims
  /\ beatingSessDeposed \in BOOLEAN /\ stragglerLand \in BOOLEAN
  /\ staleLand \in BOOLEAN

Init ==
  /\ sess = [h \in Hubs |-> [gen |-> 0, tok |-> 0, beats |-> 0,
                             deposed |-> FALSE]]
  /\ vcell = [v \in Vols |-> [mode |-> "free",
                              owner |-> CHOOSE h \in Hubs : TRUE,
                              sgen |-> 0, claim |-> 0]]
  /\ nextTok = 1
  /\ hst = [h \in Hubs |-> "down"]
  /\ myGen = [h \in Hubs |-> 0]
  /\ owned = [h \in Hubs |-> {}]
  /\ pfl = [h \in Hubs |-> NoF]
  /\ watch = [h \in Hubs |-> NoWatch]
  /\ beatBudget = MaxBeats /\ pubs = 0 /\ crashes = 0
  /\ claimBudget = MaxClaims
  /\ beatingSessDeposed = FALSE /\ stragglerLand = FALSE /\ staleLand = FALSE

unchangedWitnesses ==
  UNCHANGED <<beatingSessDeposed, stragglerLand, staleLand>>

\* The store's judgment that an incarnation is over: deposed by a
\* watcher, or superseded by the hub's own restart.  STABLE — both
\* transitions are one-way while the incarnation lives, which is
\* exactly what the volume-cell claim needs (no cross-object CAS
\* could revalidate a flaky judgment at claim time).
SessDeadOfRecord(g, sg) == sess[g].deposed \/ sess[g].gen > sg

MineOfRecord(h, v) ==
  vcell[v].mode = "owned" /\ vcell[v].owner = h /\ vcell[v].sgen = myGen[h]

(***************************************************************************)
(* The session.  Start supersedes the hub's OWN cell (self-recognition:    *)
(* whatever generation is recorded there is a dead prior incarnation);     *)
(* volumes are NOT adopted — cells naming the old generation are           *)
(* re-claimed through the ordinary claim actions.                          *)
(***************************************************************************)

StartSession(h) ==
  /\ hst[h] = "down"
  /\ sess' = [sess EXCEPT ![h] = [gen |-> @.gen + 1, tok |-> nextTok,
                                  beats |-> 0, deposed |-> FALSE]]
  /\ myGen' = [myGen EXCEPT ![h] = sess[h].gen + 1]
  /\ nextTok' = nextTok + 1
  /\ hst' = [hst EXCEPT ![h] = "up"]
  /\ owned' = [owned EXCEPT ![h] = {}]
  /\ pfl' = [pfl EXCEPT ![h] = NoF]
  /\ watch' = [watch EXCEPT ![h] = NoWatch]
  /\ UNCHANGED <<vcell, beatBudget, pubs, crashes, claimBudget>>
  /\ unchangedWitnesses

BeatOk(h) ==
  /\ hst[h] = "up"
  /\ beatBudget > 0
  /\ sess[h].gen = myGen[h] /\ ~sess[h].deposed
  /\ LET t == IF SessRotate THEN nextTok ELSE sess[h].tok IN
       sess' = [sess EXCEPT ![h].tok = t, ![h].beats = @ + 1]
  /\ nextTok' = nextTok + 1
  /\ beatBudget' = beatBudget - 1
  /\ UNCHANGED <<vcell, hst, myGen, owned, pfl, watch, pubs, crashes,
                 claimBudget>>
  /\ unchangedWitnesses

\* The beat's CAS mismatch IS the deposition notice — fence + exit(70)
\* collapsed to one step.  Hub-scoped: every volume forfeits at once.
BeatFail(h) ==
  /\ hst[h] = "up"
  /\ FenceOnFail
  /\ ~(sess[h].gen = myGen[h] /\ ~sess[h].deposed)
  /\ hst' = [hst EXCEPT ![h] = "down"]
  /\ myGen' = [myGen EXCEPT ![h] = 0]
  /\ owned' = [owned EXCEPT ![h] = {}]
  /\ pfl' = [pfl EXCEPT ![h] = NoF]
  /\ watch' = [watch EXCEPT ![h] = NoWatch]
  /\ UNCHANGED <<sess, vcell, nextTok, beatBudget, pubs, crashes,
                 claimBudget>>
  /\ unchangedWitnesses

(***************************************************************************)
(* The watch: arm once, count quiet, depose on the observed token.         *)
(***************************************************************************)

ObserveSess(h) ==
  /\ hst[h] = "up"
  /\ watch[h].tok = 0                      \* arm ONCE (see header note)
  /\ sess[Other(h)].gen > 0
  /\ watch' = [watch EXCEPT ![h] = [tok |-> sess[Other(h)].tok, quiet |-> 0,
                                    beats |-> sess[Other(h)].beats]]
  /\ UNCHANGED <<sess, vcell, nextTok, hst, myGen, owned, pfl, beatBudget,
                 pubs, crashes, claimBudget>>
  /\ unchangedWitnesses

PollQuietS(h) ==
  /\ hst[h] = "up"
  /\ watch[h].tok # 0
  /\ IF sess[Other(h)].tok = watch[h].tok
       THEN /\ watch[h].quiet < Misses     \* capped: further polls no-op
            /\ watch' = [watch EXCEPT ![h].quiet = @ + 1]
       ELSE watch' = [watch EXCEPT ![h] =
              [tok |-> sess[Other(h)].tok, quiet |-> 0,
               beats |-> sess[Other(h)].beats]]
  /\ UNCHANGED <<sess, vcell, nextTok, hst, myGen, owned, pfl, beatBudget,
                 pubs, crashes, claimBudget>>
  /\ unchangedWitnesses

\* Depose: CAS If-Match(observed token) writing the deposed flag.  The
\* witness fires when the target's beats advanced past the observation —
\* with rotation that interleaving is refused by the CAS itself (the
\* token moved), which is exactly theorem T1 one layer up.
DeposeSess(h) ==
  /\ Depose
  /\ hst[h] = "up"
  /\ claimBudget > 0
  /\ watch[h].tok # 0 /\ watch[h].quiet >= Misses
  /\ sess[Other(h)].tok = watch[h].tok
  /\ ~sess[Other(h)].deposed
  /\ beatingSessDeposed' =
       (beatingSessDeposed \/ sess[Other(h)].beats > watch[h].beats)
  /\ sess' = [sess EXCEPT ![Other(h)].deposed = TRUE]
  /\ claimBudget' = claimBudget - 1
  /\ UNCHANGED <<vcell, nextTok, hst, myGen, owned, pfl, watch, beatBudget,
                 pubs, crashes, stragglerLand, staleLand>>

(***************************************************************************)
(* Volume-cell claims: each is one CAS on the cell (revalidated reads).    *)
(***************************************************************************)

ClaimCell(h, v) ==
  /\ vcell' = [vcell EXCEPT ![v] = [mode |-> "owned", owner |-> h,
                                    sgen |-> myGen[h], claim |-> @.claim + 1]]
  /\ owned' = [owned EXCEPT ![h] = @ \cup {v}]
  /\ claimBudget' = claimBudget - 1

\* Free or cleanly-released: claimable by ANY hub with no wait — the
\* clean release's fast-handoff promise (its safety is the drain, T2).
ClaimIdle(h, v) ==
  /\ hst[h] = "up" /\ claimBudget > 0
  /\ vcell[v].mode \in {"free", "released"}
  /\ ClaimCell(h, v)
  /\ UNCHANGED <<sess, nextTok, hst, myGen, pfl, watch, beatBudget, pubs,
                 crashes>>
  /\ unchangedWitnesses

\* Takeover from a foreign owner.  STRICT (Depose): only from a session
\* the STORE records as over — stable evidence.  MUTATED (~Depose): off
\* the watcher's own quiet count, session cell left alive — the naive
\* two-level lease, and the immortal-zombie door.
ClaimDead(h, v) ==
  /\ hst[h] = "up" /\ claimBudget > 0
  /\ vcell[v].mode = "owned" /\ vcell[v].owner # h
  /\ IF Depose
       THEN SessDeadOfRecord(vcell[v].owner, vcell[v].sgen)
       ELSE watch[h].tok # 0 /\ watch[h].quiet >= Misses
  /\ ClaimCell(h, v)
  /\ UNCHANGED <<sess, nextTok, hst, myGen, pfl, watch, beatBudget, pubs,
                 crashes>>
  /\ unchangedWitnesses

\* Self-recognition at the volume layer: a cell naming OUR hub under an
\* older generation is a dead prior incarnation — re-claim immediately.
ClaimSelfDead(h, v) ==
  /\ hst[h] = "up" /\ claimBudget > 0
  /\ vcell[v].mode = "owned" /\ vcell[v].owner = h
  /\ vcell[v].sgen < myGen[h]
  /\ ClaimCell(h, v)
  /\ UNCHANGED <<sess, nextTok, hst, myGen, pfl, watch, beatBudget, pubs,
                 crashes>>
  /\ unchangedWitnesses

\* Clean release: final barrier + release token, one CAS (guard = the
\* If-Match).  DrainOnRelease refuses while that volume's publish is in
\* flight; the mutation releases anyway and flags the straggler.
Release(h, v) ==
  /\ hst[h] = "up" /\ v \in owned[h]
  /\ MineOfRecord(h, v)
  /\ IF DrainOnRelease
       THEN /\ ~(pfl[h].active /\ pfl[h].vol = v)
            /\ pfl' = pfl
       ELSE pfl' = IF pfl[h].active /\ pfl[h].vol = v
                     THEN [pfl EXCEPT ![h].rel = TRUE] ELSE pfl
  /\ vcell' = [vcell EXCEPT ![v].mode = "released"]
  /\ owned' = [owned EXCEPT ![h] = @ \ {v}]
  /\ UNCHANGED <<sess, nextTok, hst, myGen, watch, beatBudget, pubs,
                 crashes, claimBudget>>
  /\ unchangedWitnesses

(***************************************************************************)
(* Publishes, witness-only (data-plane arbitration is FlintTierEpoch's).   *)
(* Plan gates on BELIEF (the session-local gate — no store read: that is   *)
(* the economical design this module exists to check); land compares the   *)
(* store's cell and records what it sees.                                  *)
(***************************************************************************)

PubPlan(h) ==
  /\ hst[h] = "up"
  /\ ~pfl[h].active
  /\ pubs < MaxPubs
  /\ \E v \in owned[h] :
       pfl' = [pfl EXCEPT ![h] = [active |-> TRUE, vol |-> v, rel |-> FALSE]]
  /\ pubs' = pubs + 1
  /\ UNCHANGED <<sess, vcell, nextTok, hst, myGen, owned, watch, beatBudget,
                 crashes, claimBudget>>
  /\ unchangedWitnesses

PubLand(h) ==
  /\ hst[h] = "up"
  /\ pfl[h].active
  /\ staleLand' = (staleLand \/ ~MineOfRecord(h, pfl[h].vol))
  /\ stragglerLand' = (stragglerLand \/ pfl[h].rel)
  /\ pfl' = [pfl EXCEPT ![h] = NoF]
  /\ UNCHANGED <<sess, vcell, nextTok, hst, myGen, owned, watch, beatBudget,
                 pubs, crashes, claimBudget, beatingSessDeposed>>

(***************************************************************************)
(* The environment: process death.  The session cell and volume cells      *)
(* remain — the dead-session takeover exists for exactly this.             *)
(***************************************************************************)

Crash(h) ==
  /\ hst[h] = "up"
  /\ crashes < MaxCrashes
  /\ hst' = [hst EXCEPT ![h] = "down"]
  /\ myGen' = [myGen EXCEPT ![h] = 0]
  /\ owned' = [owned EXCEPT ![h] = {}]
  /\ pfl' = [pfl EXCEPT ![h] = NoF]
  /\ watch' = [watch EXCEPT ![h] = NoWatch]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<sess, vcell, nextTok, beatBudget, pubs, claimBudget>>
  /\ unchangedWitnesses

Next ==
  \E h \in Hubs :
    \/ StartSession(h) \/ BeatOk(h) \/ BeatFail(h)
    \/ ObserveSess(h) \/ PollQuietS(h) \/ DeposeSess(h)
    \/ \E v \in Vols :
         ClaimIdle(h, v) \/ ClaimDead(h, v) \/ ClaimSelfDead(h, v)
         \/ Release(h, v)
    \/ PubPlan(h) \/ PubLand(h) \/ Crash(h)

\* Protocol machinery is weakly fair; crashes are the environment.
\* BeatOk is deliberately NOT fair (a holder may stall forever — the
\* zombie premise); BeatFail IS (a deposed hub's beat eventually runs,
\* and its failure is the fence).  StartSession and Release are NOT
\* fair: restarts and releases are operator/API-paced, and a fair
\* Release would dissolve every zombie lasso by fiat.
Fairness ==
  \A h \in Hubs :
    /\ WF_vars(BeatFail(h))
    /\ WF_vars(ObserveSess(h)) /\ WF_vars(PollQuietS(h))
    /\ WF_vars(DeposeSess(h))
    /\ WF_vars(PubLand(h))
    /\ \A v \in Vols : WF_vars(ClaimDead(h, v))

Spec == Init /\ [][Next]_vars /\ Fairness

(***************************************************************************)
(* Theorems (strict) and the residual probe (required-fail).               *)
(***************************************************************************)

Inv_NoBeatingSessionDeposed == ~beatingSessDeposed
Inv_NoStragglerLand         == ~stragglerLand

\* PROBE — reachable in the strict protocol; kept OUT of the strict
\* run's invariant list.  Plan and land are separate steps and no
\* cross-object CAS binds the session to the data key; the window is
\* bounded by ZombieOwnershipResolves and the landed object is
\* arbitrated by the data plane's epoch stamps.
Inv_NoStaleLand == ~staleLand

Inv == TypeOK /\ Inv_NoBeatingSessionDeposed /\ Inv_NoStragglerLand

\* A hub that lost a volume eventually stops believing it owns it —
\* the property the DEPOSE step exists to buy (NoDepose and NoFence
\* must both find the immortal-zombie lasso).
ZombieOwns(h, v) ==
  hst[h] = "up" /\ v \in owned[h] /\ ~MineOfRecord(h, v)

ZombieOwnershipResolves ==
  \A h \in Hubs : \A v \in Vols :
    [](ZombieOwns(h, v) => <>(~ZombieOwns(h, v)))

\* A down hub's volume of record, with a live watcher, is eventually
\* claimed — or a budget honestly ends the world.
DeadSessionWatchResolves ==
  \A v \in Vols : \A g \in Hubs : \A h \in Hubs \ {g} :
    []((hst[g] = "down" /\ vcell[v].mode = "owned" /\ vcell[v].owner = g
        /\ hst[h] = "up")
       => <>(vcell[v].owner = h \/ hst[g] # "down" \/ hst[h] # "up"
             \/ claimBudget = 0))

================================================================================
