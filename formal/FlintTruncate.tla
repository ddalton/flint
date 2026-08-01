----------------------------- MODULE FlintTruncate -----------------------------
(***************************************************************************)
(* The pNFS TRUNCATE GATE — the one correctness invariant the pNFS layer   *)
(* holds in its OWN hands.                                                 *)
(*                                                                         *)
(* Almost everything else in flint's pNFS has a referee: layout op         *)
(* sequencing is RFC 8881 and pynfs adjudicates it; single-client data     *)
(* integrity is fsx/fsstress's job; and DS failure is not re-placed at     *)
(* this layer at all (placements are PINNED — layout.rs mutates            *)
(* `placements` only on load, delete and rename), so a dead DS is an       *)
(* availability event handled by the lvol underneath, which is             *)
(* FlintReplication's machine, already modelled.  What is left without a   *)
(* referee is this: a size change is applied to the MDS stub instantly and *)
(* to N data servers over the network, so between those two moments the    *)
(* DSes still hold bytes past the new EOF.  `truncate_dirty` is the gate   *)
(* that is supposed to make that window unobservable.                      *)
(*                                                                         *)
(* WHAT THE IMPLEMENTATION ACTUALLY IS (mds/operations/mod.rs,             *)
(* mds/layout.rs — modelled, not idealised):                               *)
(*                                                                         *)
(*   note_truncate(key, n)  fires on ANY applied size change, grow or      *)
(*     shrink, from SETATTR and from OPEN-with-truncate (dispatcher.rs     *)
(*     :1031, :1232 — both gate on the APPLIED attr bit, not on status).   *)
(*     It marks the gate and THEN fans out; the fanout is per-DS and       *)
(*     returns all-or-nothing.                                             *)
(*   mark_truncate_dirty   keeps the OLDEST timestamp and the SMALLEST     *)
(*     size (and_modify, keeping the smaller of the two) — MarkKeepsMin.   *)
(*   clear_truncate_dirty_if(gate, confirmed)  removes the mark IFF        *)
(*     `confirmed <= min` — GateClearGuarded.  This is the predicate the   *)
(*     whole gate rests on.                                                *)
(*   the retry task  re-reads `truncate_dirty_state` for the deepest       *)
(*     pending size, fans that out, and clears with the value it just      *)
(*     read — a repair writing its own guard's input, which is the shape   *)
(*     F62 was.  Modelled as Retarget: the re-read is a separate step from *)
(*     the clear, so TLC may interleave a fresh SETATTR between them.      *)
(*   LAYOUTGET  returns TRYLATER while the mark is present                 *)
(*     (operations/mod.rs:171).  This is the gate's ONLY tooth.            *)
(*   note_truncate recalls the file's layouts before the fanout            *)
(*     (RecallOnTruncate).  Whether that recall is HONOURED is a separate  *)
(*     constant (RecallReaches) because the emitted CB is currently        *)
(*     malformed — see below.  LAYOUTGET is TWO steps, not one             *)
(*     (LayoutGetCheck / LayoutGetInsert), because the code is two steps   *)
(*     with no lock between; PublishRecheck is the proposed repair.        *)
(*                                                                         *)
(* TWO THEOREMS, and they do not both hold.                                *)
(*                                                                         *)
(*   Inv_ClearImpliesFlushed — the gate's own claim: whenever the mark is  *)
(*     absent, no DS holds content past the MDS size.  HOLDS.  This is a   *)
(*     real result about a check-then-act shape that looks unsafe: the     *)
(*     `confirmed <= min` predicate is load-bearing, and the               *)
(*     BlindClear mutation must rediscover the loss.                       *)
(*                                                                         *)
(*   Inv_NoStaleServe — no client ever reads content past the MDS size.    *)
(*     This was F65, and it is STILL NOT SATISFIED by shipped code.  The    *)
(*     gate is a LAYOUTGET-time check, so a layout ACQUIRED BEFORE the      *)
(*     truncate walked straight past it and the read never reached the MDS; *)
(*     TLC found it three steps from Init.  note_truncate now recalls and   *)
(*     revokes the file's layouts between the mark and the fanout — the     *)
(*     right shape — and a 2026-07-31 audit found three further defects,    *)
(*     each isolated by its own failing run.  TWO ARE NOW FIXED; read the   *)
(*     verdict column, not the count:                                       *)
(*                                                                         *)
(*       RecallOnTruncate = FALSE   F65 itself: no recall at all.  Fixed.   *)
(*       RecallReaches = FALSE      STILL FALSE, BUT NOT FOR THE REASON     *)
(*         THIS COMMENT USED TO GIVE.  The audit's C1 (the CB carried the   *)
(*         layout stateid verbatim where §12.5.3 wants seqid+1), C2         *)
(*         (CB_SEQUENCE hardcoded slot 0 / seqid 1) and C3 (`Ok(_reply) =>  *)
(*         Acked` discarding the status, which is what let C1 and C2 hide)  *)
(*         are ALL FIXED — callback.rs now classifies the reply and returns *)
(*         Refused on a bad CB_SEQUENCE or CB_LAYOUTRECALL status, and on   *)
(*         a compound where CB_LAYOUTRECALL never ran at all.               *)
(*                                                                          *)
(*         What keeps the arm FALSE is the RESIDUAL: revocation is          *)
(*         server-side, so it binds only clients the recall reaches, and a  *)
(*         client with no live back channel at all cannot be reached however *)
(*         well the CB is encoded.  See FlintTruncateLostRecall.cfg.        *)
(*       PublishRecheck = FALSE     the audit's C6.  FIXED — hence TRUE in  *)
(*         the shipped cfg.  layoutget reads the gate at                    *)
(*         operations/mod.rs:288 and publishes at layout.rs:988; there is   *)
(*         still no lock between them (LayoutManager has no Mutex or RwLock *)
(*         at all), so the repair is not mutual exclusion but a RE-READ:    *)
(*         layout.rs:989 re-checks truncate_dirty after the insert and      *)
(*         removes the grant it just published, returning                   *)
(*         GRANT_RACED_TRUNCATE.  The mark precedes the recall's snapshot   *)
(*         and the insert precedes the recheck, so either the recall sees   *)
(*         the layout or the recheck sees the mark.                         *)
(*                                                                         *)
(*     The shipped cfg therefore STILL does not list Inv_NoStaleServe —     *)
(*     now for one reason rather than three.  Listing it would be the model *)
(*     asserting a delivery the code does not achieve, which is this        *)
(*     module's own worst failure mode.                                     *)
(*     FlintTruncateNoStaleServe.cfg is the CONDITIONAL green: what closing *)
(*     F65 requires, not what it currently does.                            *)
(*                                                                         *)
(*     AND A RESIDUAL beyond all three: revocation is SERVER-side, so even  *)
(*     a correctly-emitted recall binds only clients it reaches.  Closing   *)
(*     that needs the DS to refuse, not the MDS to ask.                     *)
(*                                                                         *)
(* ABSTRACTIONS, STATED — this is the module's own THE-ABSTRACTION-WAS-    *)
(* THE-BUG surface, so read these before citing a green run:               *)
(*                                                                         *)
(*   1. Every DS is modelled as holding the SAME logical offset set.  The  *)
(*      real stripe map scatters offsets across DSes, but the gate is      *)
(*      per-file and its fanout is all-DSes-or-nothing, so distribution    *)
(*      changes WHICH DS exposes a byte, never WHETHER one does.  A model  *)
(*      of the stripe map itself would be a different module.              *)
(*   2. Content is the set of offsets holding REAL pre-truncate bytes.     *)
(*      set_len(n) is `dsData \cap 1..n`: shrinking drops real content,    *)
(*      growing adds only zeros, which are not content.  This is why       *)
(*      re-extension by a stale fanout is not by itself a stale-read.      *)
(*   3. A read is ATOMIC with respect to revocation.  So the              *)
(*      RecallOnTruncate green says a recall closes the window the MDS     *)
(*      controls; it says NOTHING about a read already on the wire to a    *)
(*      DS, which no MDS-side mechanism can reach.  Fencing that needs the *)
(*      DS to refuse, and this module cannot speak to it.                  *)
(*   4. Whether a conforming client ISSUES the offending read is a client- *)
(*      behaviour question — the Linux client would have to read before    *)
(*      revalidating size.  The model says the SERVER does not stop it.    *)
(*      That is a statement about flint's code, which is the only thing    *)
(*      flint can fix.                                                     *)
(*   5. EVERY DS EVENTUALLY CONFIRMS.  Land(j, d) is guarded only by the   *)
(*      job's state, so nothing in this module can hold a DS down: the     *)
(*      fanout can always complete.  The preamble above delegates a dead   *)
(*      DS to "FlintReplication's machine, already modelled" — and that is *)
(*      true of the DATA plane and FALSE as a delegation of the CONTROL    *)
(*      plane.  FlintReplication.tla contains ZERO occurrences of mds,     *)
(*      pnfs, layout, truncate, DsControl or control_endpoint across 4887  *)
(*      lines; it has no notion of the DsControl listener, of DeviceRegistry *)
(*      membership under an MDS incarnation, or of truncate at all.  So    *)
(*      MDS->DS control reachability is unmodelled ANYWHERE in the corpus, *)
(*      and every green here is conditional on the fanout being able to    *)
(*      complete.                                                          *)
(*                                                                          *)
(*      What this does NOT do is void the existing runs, and the           *)
(*      distinction matters because the opposite mistake is this           *)
(*      workstream's worst: a permanent park is ALREADY a legal behaviour  *)
(*      (Park -> Retarget -> Park, no fairness at Spec, no cfg declares a  *)
(*      PROPERTY), and the parked-gate state is two steps from Init —      *)
(*      SetSize(0); Park(1).  Inv_ClearImpliesFlushed's antecedent is      *)
(*      genuinely falsified there today.  Adding a DS-availability guard   *)
(*      to Land would only SHRINK the transition relation, so on its own   *)
(*      it can produce no counterexample this module cannot already reach. *)
(*                                                                          *)
(*      The gap that is real is a different one: a client REFUSED by the   *)
(*      gate is a DISABLED ACTION.  LayoutGetCheck is guarded by ~gated,   *)
(*      so a client answered TRYLATER takes no step and leaves no state,   *)
(*      and the module cannot tell the 4.28ms park runat measured from the *)
(*      251.01s one.  Closing that needs `refused` and the fallback        *)
(*      ceiling as MODEL STATE, not a guard on Land.                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  DSes,              \* the file's pinned data servers
  Clients,           \* layout holders
  MaxOff,            \* logical offsets are 1..MaxOff; sizes are 0..MaxOff
  MaxSets,           \* SETATTR/OPEN-truncate budget
  MaxJobs,           \* concurrent fanout budget (>= MaxSets to never wedge)
  GateClearGuarded,  \* TRUE = clear_truncate_dirty_if's `confirmed <= min`
  MarkKeepsMin,      \* TRUE = mark_truncate_dirty's and_modify(min)
  RecallOnTruncate,  \* TRUE = recall+revoke held layouts when the gate arms
  RecallReaches,     \* TRUE = every recall is delivered AND honoured
  PublishRecheck     \* TRUE = a layout re-reads the gate AFTER being published

VARIABLES
  size,        \* the MDS stub's size — the authoritative attribute
  dsData,      \* [DSes -> SUBSET (1..MaxOff)] real content still on each DS
  gated,       \* the truncate-dirty mark is present
  gmin,        \* its SMALLEST unconfirmed target size (meaningful when gated)
  job,         \* [1..MaxJobs -> fanout record]
  sets,        \* SETATTR budget spent
  held,        \* [Clients -> BOOLEAN] — layout is IN the map (recallable)
  granting,    \* [Clients -> BOOLEAN] — passed layoutget's gate check, not yet
               \*   inserted.  The window between operations/mod.rs:234 and
               \*   layout.rs:862, in which a layout is invisible to the recall
               \*   snapshot because it is not in `layouts` yet.
  staleServed  \* ghost: a client was handed content past the MDS size

vars == <<size, dsData, gated, gmin, job, sets, held, granting, staleServed>>

Offsets == 1..MaxOff
Sizes == 0..MaxOff
JobIds == 1..MaxJobs

\* A fanout is idle, live (dispatched, landing per-DS), or parked: parked is
\* truncate_fanout having returned FALSE — a DS that is unregistered or
\* advertises no DsControl listener — with the background retry armed.
JobStates == {"idle", "live", "parked"}

NoJob == [st |-> "idle", target |-> 0, landed |-> {}]

MinOf(a, b) == IF a <= b THEN a ELSE b

TypeOK ==
  /\ size \in Sizes
  /\ dsData \in [DSes -> SUBSET Offsets]
  /\ gated \in BOOLEAN
  /\ gmin \in Sizes
  /\ job \in [JobIds -> [st : JobStates, target : Sizes, landed : SUBSET DSes]]
  /\ sets \in 0..MaxSets
  /\ held \in [Clients -> BOOLEAN]
  /\ granting \in [Clients -> BOOLEAN]
  /\ staleServed \in BOOLEAN

Init ==
  /\ size = MaxOff
  /\ dsData = [d \in DSes |-> Offsets]
  /\ gated = FALSE
  /\ gmin = 0
  /\ job = [j \in JobIds |-> NoJob]
  /\ sets = 0
  /\ held = [c \in Clients |-> FALSE]
  /\ granting = [c \in Clients |-> FALSE]
  /\ staleServed = FALSE

(***************************************************************************)
(* The MDS side                                                            *)
(***************************************************************************)

\* SETATTR(size) / OPEN-with-truncate, then note_truncate.  The stub's size
\* changes and the gate is marked in the same step — the code marks BEFORE
\* awaiting the fanout, deliberately ("Gate before fanning out"), and that
\* ordering is the reason the window is closed for FRESH layouts.
\*
\* mark_truncate_dirty keeps the smaller size when already dirty.  The
\* MarkKeepsMin=FALSE arm overwrites instead (a plain `insert`), which is
\* the reading of that line a reviewer might not question.
SetSize(n) ==
  /\ sets < MaxSets
  /\ \E j \in JobIds :
       /\ job[j].st = "idle"
       /\ job' = [job EXCEPT ![j] = [st |-> "live", target |-> n, landed |-> {}]]
  /\ size' = n
  /\ gated' = TRUE
  /\ gmin' = IF ~gated THEN n
             ELSE IF MarkKeepsMin THEN MinOf(gmin, n) ELSE n
  /\ sets' = sets + 1
  \* The F65 fix: recall and revoke every outstanding layout for the file
  \* before the fanout.
  \*
  \* `lost` is the honest part.  The implementation revokes SERVER-SIDE
  \* whatever the recall outcome — Acked, TimedOut, NoChannel, Transport
  \* all revoke — but server-side revocation does not stop a client that
  \* never got the message: it does not know, and its reads go straight
  \* to a DS.  So a recall closes the window only for clients it actually
  \* reaches, and `lost` is the set it did not.  RecallReaches = TRUE
  \* asserts delivery; the arm that lets `lost` be non-empty is what
  \* states the residual instead of assuming it away.
  /\ \E lost \in SUBSET Clients :
       /\ RecallReaches => lost = {}
       /\ held' = IF RecallOnTruncate
                    THEN [c \in Clients |-> IF c \in lost THEN held[c] ELSE FALSE]
                    ELSE held
  \* `granting` is deliberately NOT touched.  recall_layouts_for_file
  \* iterates `layouts`, and a layout still between its gate check and its
  \* insert is not in that map — so the recall cannot see it.  That is the
  \* implementation, and modelling LayoutGet as one atomic action (as this
  \* module first did) is exactly what hid it.
  /\ UNCHANGED <<dsData, granting, staleServed>>

\* One DS confirms set_len(target).  Truncation drops the real content above
\* the target; extension adds only zeros, which are not content.
Land(j, d) ==
  /\ job[j].st = "live"
  /\ d \notin job[j].landed
  /\ dsData' = [dsData EXCEPT ![d] = @ \cap 1..job[j].target]
  /\ job' = [job EXCEPT ![j].landed = @ \cup {d}]
  /\ UNCHANGED <<size, gated, gmin, sets, held, granting, staleServed>>

\* truncate_fanout returned TRUE: every pinned DS confirmed.  The gate is
\* lifted only if this confirmation satisfies the DEEPEST cut still pending
\* — that predicate IS clear_truncate_dirty_if's `confirmed <= min`, and
\* the BlindClear arm drops it.
Complete(j) ==
  /\ job[j].st = "live"
  /\ job[j].landed = DSes
  /\ LET lifts == (~GateClearGuarded) \/ (job[j].target <= gmin)
     IN /\ gated' = IF gated /\ lifts THEN FALSE ELSE gated
        /\ gmin' = gmin
  /\ job' = [job EXCEPT ![j] = NoJob]
  /\ UNCHANGED <<size, dsData, sets, held, granting, staleServed>>

\* truncate_fanout returned FALSE — a pinned DS is unregistered with this
\* MDS incarnation, or advertises no control listener.  The mark stays and
\* the background retry is armed.
Park(j) ==
  /\ job[j].st = "live"
  /\ job[j].landed # DSes
  /\ job' = [job EXCEPT ![j].st = "parked"]
  /\ UNCHANGED <<size, dsData, gated, gmin, sets, held, granting, staleServed>>

\* The retry loop's re-read: "Re-read the deepest pending size each round".
\* Separating the read from the clear is the whole point — it lets TLC put
\* a fresh SetSize between them, which is how a repair that writes its own
\* guard's input gets caught if it can be.
Retarget(j) ==
  /\ job[j].st = "parked"
  /\ gated
  /\ job' = [job EXCEPT ![j] = [st |-> "live", target |-> gmin, landed |-> {}]]
  /\ UNCHANGED <<size, dsData, gated, gmin, sets, held, granting, staleServed>>

\* The retry's other exit: "the mark may also have been lifted (file
\* removed, or a deeper concurrent truncate confirmed everywhere)" — the
\* task returns and the job disappears.
RetryStandDown(j) ==
  /\ job[j].st = "parked"
  /\ ~gated
  /\ job' = [job EXCEPT ![j] = NoJob]
  /\ UNCHANGED <<size, dsData, gated, gmin, sets, held, granting, staleServed>>

(***************************************************************************)
(* The client side                                                         *)
(***************************************************************************)

\* LAYOUTGET, in TWO steps, because the implementation is two steps.
\*
\* The gate is read at operations/mod.rs:234 and the layout is published at
\* layout.rs:862, with NO lock between them — LayoutManager has no Mutex or
\* RwLock at all; `layouts` and `truncate_dirty` are independent DashMaps.
\* Collapsing these into one atomic action (this module's first cut) asserts
\* an atomicity the code does not have, and asserting it is what made TLC
\* green on a property the code does not hold.
LayoutGetCheck(c) ==
  /\ ~held[c]
  /\ ~granting[c]
  /\ ~gated                            \* TRYLATER while the mark is present
  /\ granting' = [granting EXCEPT ![c] = TRUE]
  /\ UNCHANGED <<size, dsData, gated, gmin, job, sets, held, staleServed>>

\* The publish.  From here the layout is in `layouts` and a LATER recall can
\* see it — but a recall that already ran cannot.
\*
\* PublishRecheck is the proposed fix: re-read the gate after publishing and
\* revoke what was just inserted if it is dirty.  Sufficient by construction
\* (the mark precedes the recall's snapshot, and the insert precedes the
\* recheck), which is what the strict arm has to demonstrate.
LayoutGetInsert(c) ==
  /\ granting[c]
  /\ granting' = [granting EXCEPT ![c] = FALSE]
  /\ held' = [held EXCEPT ![c] = ~(PublishRecheck /\ gated)]
  /\ UNCHANGED <<size, dsData, gated, gmin, job, sets, staleServed>>

LayoutReturn(c) ==
  /\ held[c]
  /\ held' = [held EXCEPT ![c] = FALSE]
  /\ UNCHANGED <<size, dsData, gated, gmin, job, sets, granting, staleServed>>

\* A read issued directly to a DS under a held layout — the MDS is not on
\* this path at all, which is exactly why a LAYOUTGET-time check cannot
\* cover it.  The ghost records only reads that were actually SERVED
\* content the MDS considers gone.
Read(c, d, o) ==
  /\ held[c]
  /\ staleServed' = IF o > size /\ o \in dsData[d] THEN TRUE ELSE staleServed
  /\ UNCHANGED <<size, dsData, gated, gmin, job, sets, held, granting>>

Next ==
  \/ \E n \in Sizes : SetSize(n)
  \/ \E j \in JobIds, d \in DSes : Land(j, d)
  \/ \E j \in JobIds : Complete(j)
  \/ \E j \in JobIds : Park(j)
  \/ \E j \in JobIds : Retarget(j)
  \/ \E j \in JobIds : RetryStandDown(j)
  \/ \E c \in Clients : LayoutGetCheck(c)
  \/ \E c \in Clients : LayoutGetInsert(c)
  \/ \E c \in Clients : LayoutReturn(c)
  \/ \E c \in Clients, d \in DSes, o \in Offsets : Read(c, d, o)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* THEOREM 1 — the gate's own claim.                                       *)
(*                                                                         *)
(* Whenever the mark is absent, every DS has been cut to the MDS's size.   *)
(* This is what makes the LAYOUTGET check sound: a layout handed out while *)
(* clear cannot reach a byte the file no longer has.  It is stated over    *)
(* the DS state and never mentions layouts, so it is independent of        *)
(* Theorem 2 and of the recall arm that fixes it.                          *)
(***************************************************************************)
Inv_ClearImpliesFlushed ==
  ~gated => \A d \in DSes : dsData[d] \subseteq 1..size

(***************************************************************************)
(* THEOREM 2 — what the gate does NOT cover.                               *)
(*                                                                         *)
(* No client is ever served content past the MDS's size.  As shipped       *)
(* (RecallOnTruncate = FALSE) this is FALSE, and the counterexample is     *)
(* short: acquire a layout while clear, truncate, read.  The gate is a     *)
(* LAYOUTGET-time check and the read never reaches the MDS.                *)
(***************************************************************************)
Inv_NoStaleServe == ~staleServed

Inv == TypeOK /\ Inv_ClearImpliesFlushed

================================================================================
