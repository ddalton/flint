-------------------------- MODULE FlintClientIdentity --------------------------
(***************************************************************************)
(* The NFSv4.1 client-record lifecycle, keyed on an identity that is NOT   *)
(* unique — src/nfs/v4/state/client.rs `exchange_id` /                     *)
(* `remove_client_internal`, and the case-5 cascade in                     *)
(* src/nfs/v4/operations/session.rs `handle_create_session`.               *)
(*                                                                         *)
(* WHY THIS MODULE EXISTS.  The many-clusters drill (2026-08-22) found     *)
(* THREE defects in this one state machine by hand, in an afternoon.       *)
(* Three in one machine is not three bugs; it is a machine nobody had      *)
(* enumerated.  Each was fixed, and the fixes came with tests — but a      *)
(* test says the paths it walks are right, and says nothing about the      *)
(* paths it does not.  The open question the fixes could not answer was    *)
(* whether they are COMPLETE: whether carrying the obligation through      *)
(* case 4 survives NConnect = 3, an unmount interleaved between the two    *)
(* EXCHANGE_IDs of one mount, or a lease sweep landing mid-handshake.      *)
(* That is a question about interleavings, so it belongs here.             *)
(*                                                                         *)
(* THE LOAD-BEARING ABSTRACTION, stated first because getting it wrong     *)
(* makes every theorem below vacuous — and this repo has been burned by    *)
(* exactly that three times:                                               *)
(*                                                                         *)
(*   co_ownerid IS A MANY-TO-ONE KEY.  On NFSv4.1+ the Linux client        *)
(*   builds it as `Linux NFSv4.<minor> <nodename>` and nothing else: no    *)
(*   address, no cluster, no uniquifier unless nfs4_unique_id is set on    *)
(*   the node.  Two agent pods in two different clusters therefore         *)
(*   present the SAME bytes — captured on the wire from two kind clusters  *)
(*   mounting one hub, byte-identical `Linux NFSv4.2 agent`, with nothing  *)
(*   contrived about the setup.  RFC 8881 §18.35.5 then REQUIRES the       *)
(*   server to read that as one client returning, so flint cannot refuse   *)
(*   the collision; it can only decline to lose state over it.             *)
(*                                                                         *)
(*   Model agents with distinct owners — the natural abstraction — and     *)
(*   every defect below disappears and the module proves a theorem about   *)
(*   a system that does not exist.  `Collide = FALSE` is a REQUIRED run    *)
(*   for exactly that reason: it turns all three mutations green and is    *)
(*   the machine-checked statement that the unique-owner abstraction is    *)
(*   the bug, not a simplification.                                        *)
(*                                                                         *)
(*   A MOUNT IS NConnect EXCHANGE_IDs, NOT ONE.  The Linux client opens    *)
(*   one connection per `nconnect` and sends EXCHANGE_ID on each for       *)
(*   session-trunking detection, so the server sees several before any     *)
(*   CREATE_SESSION.  This is why pynfs EID5f passes over the case-4       *)
(*   defect: it uses a single connection.  NConnect = 1 is therefore also  *)
(*   a vacuity check, not a cheaper run.                                   *)
(*                                                                         *)
(* WHAT IS MODELLED.  Per EXCHANGE_ID the four arms the code actually      *)
(* distinguishes on (owner present?, incumbent confirmed?, verifier        *)
(* equal?) — RFC cases 1, 3/4 and 5.  The principal is always equal:       *)
(* AUTH_SYS derives it from the same nodename, so a co_ownerid collision   *)
(* is a principal collision too, and the cases that turn on a principal    *)
(* MISMATCH are unreachable in the situation this module is about.         *)
(*                                                                         *)
(* WHAT IS NOT.  Sequence-id / replay caching (§18.36.4) — a different     *)
(* machine with its own drills; back channels; and the wire.  Locks are    *)
(* modelled only as "client c holds one", because the defect is that they  *)
(* OUTLIVE c, not anything about ranges.                                   *)
(*                                                                         *)
(* THE THREE MUTATIONS, each the shipped code before its fix:              *)
(*                                                                         *)
(*   CarryObligation = FALSE — case 4 replaces an unconfirmed record and   *)
(*     starts the new one's `pending_replaces` at None, dropping the       *)
(*     obligation case 5 took on one connection earlier.  The confirming   *)
(*     CREATE_SESSION then owes no cleanup and the incumbent is never      *)
(*     discarded.  Cuts both ways in the real system: it MASKS the         *)
(*     cross-cluster steal (which is why the rig kept failing to           *)
(*     reproduce it) while breaking reboot cleanup for everyone.           *)
(*                                                                         *)
(*   CondIndexRemove = FALSE — `remove_client_internal` clears the owner   *)
(*     index unconditionally instead of only when it still names the       *)
(*     departing client.  One owner key, several clients: a departing      *)
(*     client evicts a LIVE peer from the index, and that peer's next      *)
(*     EXCHANGE_ID takes the no-record arm and mints a second clientid     *)
(*     for an owner that already has one.                                  *)
(*                                                                         *)
(*   CascadeLocks = FALSE — the case-5 cascade tears down sessions,        *)
(*     stateids, delegations and the client record but not the locks, and  *)
(*     structurally could not: the session handler held no reference to    *)
(*     the lock table.  `remove_client` drops the lease first, and the     *)
(*     only reaper iterates EXPIRED LEASES — so what is left behind is     *)
(*     unreachable by anything, which `Inv_LocksReapable` states directly. *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  Agents,           \* distinct real clients — agent pods, possibly in different clusters
  Collide,          \* TRUE: every agent presents ONE co_ownerid (the shipped reality)
  NConnect,         \* EXCHANGE_IDs per mount (the trunking probe); >= 2 is the default
  CarryObligation,  \* case 4 carries pending_replaces forward           (fix 1)
  CondIndexRemove,  \* the owner index is cleared only if it names us    (fix 3)
  CascadeLocks,     \* the case-5 cascade releases the victim's locks    (fix 2)
  MaxMounts,        \* mounts per agent
  MaxLocks

NoId == 0
NoOwner == "none"

OwnerOf(a) == IF Collide THEN "shared" ELSE a
Owners == { OwnerOf(a) : a \in Agents }

(***************************************************************************)
(* STATE-SPACE SHAPE.  Three choices below are canonicalisation, not       *)
(* semantics, and they are the difference between a run that converges and *)
(* one that does not.  At three agents this module first passed 36 MILLION *)
(* distinct states without terminating; almost all of that was bookkeeping *)
(* TLC could see but the protocol could not.  With all three in place the  *)
(* same configuration settles at ~407k and converges — 2 agents fell from  *)
(* 23,177 distinct states to 2,082 — so three clusters on one hub is a     *)
(* routine gate run rather than something to bound away.                   *)
(*                                                                         *)
(*   1. DEAD RECORDS ARE ERASED.  Every removed clientid used to keep its  *)
(*      owner / verifier / confirmed / pending forever.  Two behaviours    *)
(*      differing only in what a dead id's garbage happens to hold are the *)
(*      same behaviour, and nothing reads a dead id — every use is guarded *)
(*      by membership in `live`.                                           *)
(*                                                                         *)
(*   2. CLIENTIDS ARE REUSED.  A monotonic counter makes "allocated five   *)
(*      ids" distinguishable from "allocated four" even when the reachable *)
(*      configuration is identical, so the space grows with HISTORY rather *)
(*      than with state.  `FreeId` takes the lowest id nothing references  *)
(*      — not live, not any agent's handle, not an outstanding obligation. *)
(*      The real ClientManager does mint monotonically, but the identity   *)
(*      of a clientid is not a protocol property: only its relationships   *)
(*      are, and those are preserved.                                      *)
(*                                                                         *)
(*   3. VERIFIERS ARE PER-MOUNT, NOT GLOBAL.  A boot verifier matters only *)
(*      through EQUALITY with an incumbent's, so a globally increasing     *)
(*      counter (which never repeats, and therefore never lets a genuine   *)
(*      renewal be tested twice) buys nothing and costs a dimension.  It   *)
(*      is now the agent's mount number, bounded by MaxMounts.  This is a  *)
(*      REFINEMENT, not only a shrink: two agents can now coincidentally   *)
(*      share a verifier, which is the case-1 renewal arm firing across a  *)
(*      collision — a real interleaving the global counter made            *)
(*      unreachable.                                                       *)
(***************************************************************************)
MaxId == Cardinality(Agents) * 2 + 1
Ids == 1..MaxId

VARIABLES
  live, owner, verifier, confirmed, pending, ownerIdx, locks, leases,
  st, cur, conns, vmine, mounts, nlocks, superseded

vars == << live, owner, verifier, confirmed, pending, ownerIdx, locks,
           leases, st, cur, conns, vmine, mounts, nlocks, superseded >>

TypeOK ==
  /\ live \subseteq Ids
  /\ locks \subseteq Ids
  /\ leases \subseteq Ids
  /\ st \in [Agents -> {"idle","exchanging","mounted"}]

Init ==
  /\ live = {}
  /\ owner = [i \in Ids |-> NoOwner]
  /\ verifier = [i \in Ids |-> 0]
  /\ confirmed = [i \in Ids |-> FALSE]
  /\ pending = [i \in Ids |-> NoId]
  /\ ownerIdx = [o \in Owners |-> NoId]
  /\ locks = {}
  /\ leases = {}
  /\ st = [a \in Agents |-> "idle"]
  /\ cur = [a \in Agents |-> NoId]
  /\ conns = [a \in Agents |-> 0]
  /\ vmine = [a \in Agents |-> 0]
  /\ mounts = [a \in Agents |-> 0]
  /\ nlocks = 0
  /\ superseded = [a \in Agents |-> NoId]

\* The lowest clientid nothing refers to.
\*
\* `superseded` MUST be in here, and TLC is why.  Without it the strict
\* run failed Inv_ObligationHonoured on a trace that looked like a real
\* defect and was not: a1's handshake recorded an obligation against
\* clientid 1, clientid 1 was removed, and then FreeId handed 1 straight
\* back for a1's NEXT client — so the ghost aliased a live record it had
\* never referred to.  A ghost holding a raw id is only sound while that
\* id cannot be recycled underneath it.  The counterexample cost one
\* reading; a subtler aliasing artifact could have been argued about for
\* an afternoon, which is the standing reason this repo runs mutations.
Referenced ==
  live \cup { cur[a] : a \in Agents } \cup { pending[i] : i \in live }
       \cup { superseded[a] : a \in Agents }
FreeId ==
  LET avail == Ids \ Referenced
  IN IF avail = {} THEN NoId ELSE CHOOSE i \in avail : \A j \in avail : i =< j

(***************************************************************************)
(* `remove_client_internal`.  Fix 3 is the whole of the guard: clear the   *)
(* owner index only when it still names the departing client.  Locks are   *)
(* DELIBERATELY untouched — that is the code, and Inv_LocksReapable is     *)
(* what makes the omission visible.                                        *)
(***************************************************************************)
IdxAfterRemove(idx, id) ==
  IF CondIndexRemove
  THEN IF idx[owner[id]] = id THEN [idx EXCEPT ![owner[id]] = NoId] ELSE idx
  ELSE [idx EXCEPT ![owner[id]] = NoId]

StartMount(a) ==
  /\ st[a] = "idle"
  /\ mounts[a] < MaxMounts
  /\ st' = [st EXCEPT ![a] = "exchanging"]
  /\ conns' = [conns EXCEPT ![a] = 0]
  /\ vmine' = [vmine EXCEPT ![a] = mounts[a] + 1]
  /\ mounts' = [mounts EXCEPT ![a] = mounts[a] + 1]
  /\ cur' = [cur EXCEPT ![a] = NoId]
  /\ superseded' = [superseded EXCEPT ![a] = NoId]
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   locks, leases, nlocks >>

ExchangeId(a) ==
  LET o   == OwnerOf(a)
      ex  == ownerIdx[o]
      new == FreeId
  IN
  /\ st[a] = "exchanging"
  /\ conns[a] < NConnect
  /\ \/ /\ ex = NoId                      \* no record: allocate
        /\ new # NoId
        /\ live' = live \cup {new}
        /\ owner' = [owner EXCEPT ![new] = o]
        /\ verifier' = [verifier EXCEPT ![new] = vmine[a]]
        /\ confirmed' = [confirmed EXCEPT ![new] = FALSE]
        /\ pending' = [pending EXCEPT ![new] = NoId]
        /\ ownerIdx' = [ownerIdx EXCEPT ![o] = new]
        /\ cur' = [cur EXCEPT ![a] = new]
        /\ UNCHANGED << leases, superseded >>
     \/ /\ ex # NoId
        /\ ~confirmed[ex]                 \* CASE 4: replace the unconfirmed
        /\ new # NoId
        /\ LET inherited == IF CarryObligation THEN pending[ex] ELSE NoId
           IN /\ live' = (live \ {ex}) \cup {new}
              /\ owner' = [owner EXCEPT ![new] = o, ![ex] = NoOwner]
              /\ verifier' = [verifier EXCEPT ![new] = vmine[a], ![ex] = 0]
              /\ confirmed' = [confirmed EXCEPT ![new] = FALSE, ![ex] = FALSE]
              /\ pending' = [pending EXCEPT ![new] = inherited, ![ex] = NoId]
              /\ ownerIdx' = [IdxAfterRemove(ownerIdx, ex) EXCEPT ![o] = new]
              /\ leases' = leases \ {ex}
              /\ cur' = [cur EXCEPT ![a] = new]
              /\ UNCHANGED superseded
     \/ /\ ex # NoId
        /\ confirmed[ex]
        /\ verifier[ex] = vmine[a]        \* CASE 1: renewal, reuse the id
        /\ cur' = [cur EXCEPT ![a] = ex]
        /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                         leases, superseded >>
     \/ /\ ex # NoId
        /\ confirmed[ex]
        /\ verifier[ex] # vmine[a]        \* CASE 5: the incumbent "rebooted"
        /\ new # NoId
        /\ live' = live \cup {new}
        /\ owner' = [owner EXCEPT ![new] = o]
        /\ verifier' = [verifier EXCEPT ![new] = vmine[a]]
        /\ confirmed' = [confirmed EXCEPT ![new] = FALSE]
        /\ pending' = [pending EXCEPT ![new] = ex]
        /\ ownerIdx' = [ownerIdx EXCEPT ![o] = new]
        /\ leases' = leases
        /\ cur' = [cur EXCEPT ![a] = new]
        \* The GHOST records the obligation the RFC imposes, whatever the
        \* implementation then does with `pending`.
        /\ superseded' = [superseded EXCEPT ![a] = ex]
  /\ conns' = [conns EXCEPT ![a] = conns[a] + 1]
  /\ UNCHANGED << locks, st, vmine, mounts, nlocks >>

CreateSession(a) ==
  LET id  == cur[a]
      old == pending[id]
  IN
  /\ st[a] = "exchanging"
  /\ conns[a] = NConnect
  /\ id # NoId
  /\ id \in live
  /\ IF old # NoId /\ old \in live
     THEN /\ live' = live \ {old}
          /\ ownerIdx' = IdxAfterRemove(ownerIdx, old)
          /\ locks' = IF CascadeLocks THEN locks \ {old} ELSE locks
          /\ leases' = (leases \ {old}) \cup {id}
          /\ confirmed' = [confirmed EXCEPT ![id] = TRUE, ![old] = FALSE]
          /\ pending' = [pending EXCEPT ![id] = NoId, ![old] = NoId]
          /\ owner' = [owner EXCEPT ![old] = NoOwner]
          /\ verifier' = [verifier EXCEPT ![old] = 0]
     ELSE /\ live' = live
          /\ ownerIdx' = ownerIdx
          /\ locks' = locks
          /\ leases' = leases \cup {id}
          /\ confirmed' = [confirmed EXCEPT ![id] = TRUE]
          /\ pending' = [pending EXCEPT ![id] = NoId]
          /\ UNCHANGED << owner, verifier >>
  /\ st' = [st EXCEPT ![a] = "mounted"]
  /\ UNCHANGED << cur, conns, vmine, mounts, nlocks, superseded >>

TakeLock(a) ==
  /\ st[a] = "mounted"
  /\ nlocks < MaxLocks
  /\ cur[a] \in live
  /\ locks' = locks \cup {cur[a]}
  /\ nlocks' = nlocks + 1
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   leases, st, cur, conns, vmine, mounts, superseded >>

\* DESTROY_CLIENTID — a clean unmount.  Without fix 3 this is the action
\* that evicts a LIVE peer from the owner index.
Unmount(a) ==
  /\ st[a] = "mounted"
  /\ cur[a] \in live
  /\ live' = live \ {cur[a]}
  /\ ownerIdx' = IdxAfterRemove(ownerIdx, cur[a])
  /\ leases' = leases \ {cur[a]}
  /\ locks' = locks \ {cur[a]}
  /\ owner' = [owner EXCEPT ![cur[a]] = NoOwner]
  /\ verifier' = [verifier EXCEPT ![cur[a]] = 0]
  /\ confirmed' = [confirmed EXCEPT ![cur[a]] = FALSE]
  /\ pending' = [pending EXCEPT ![cur[a]] = NoId]
  /\ st' = [st EXCEPT ![a] = "idle"]
  /\ cur' = [cur EXCEPT ![a] = NoId]
  /\ superseded' = [superseded EXCEPT ![a] = NoId]
  /\ UNCHANGED << conns, vmine, mounts, nlocks >>

\* `courtesy_release_expired`: the ONLY production caller of
\* remove_client_locks, and it iterates EXPIRED LEASES — so it can only
\* ever reach a clientid that still has one.
LeaseExpire(id) ==
  /\ id \in leases
  /\ id \in live
  /\ \A a \in Agents : cur[a] # id \/ st[a] # "mounted"
  /\ locks' = locks \ {id}
  /\ live' = live \ {id}
  /\ ownerIdx' = IdxAfterRemove(ownerIdx, id)
  /\ leases' = leases \ {id}
  /\ owner' = [owner EXCEPT ![id] = NoOwner]
  /\ verifier' = [verifier EXCEPT ![id] = 0]
  /\ confirmed' = [confirmed EXCEPT ![id] = FALSE]
  /\ pending' = [pending EXCEPT ![id] = NoId]
  /\ UNCHANGED << st, cur, conns, vmine, mounts, nlocks, superseded >>

Next ==
  \/ \E a \in Agents : StartMount(a)
  \/ \E a \in Agents : ExchangeId(a)
  \/ \E a \in Agents : CreateSession(a)
  \/ \E a \in Agents : TakeLock(a)
  \/ \E a \in Agents : Unmount(a)
  \/ \E id \in Ids : LeaseExpire(id)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* THE INVARIANTS.                                                         *)
(***************************************************************************)

\* A lock must not outlive the client that holds it.
Inv_NoOrphanLocks == \A id \in locks : id \in live

\* And whatever holds one must stay reachable by the reaper, which
\* iterates expired LEASES.  A lock whose client has no lease can never be
\* collected by anything, survives restart (locks are persisted and
\* re-seeded), and denies its range to every other client forever.
Inv_LocksReapable == \A id \in locks : id \in leases

\* The owner index must not be emptied for an owner a confirmed client is
\* still using: that client's next EXCHANGE_ID would take the no-record
\* arm and mint a duplicate instead of renewing.
Inv_IndexCoversLiveOwners ==
  \A id \in live : confirmed[id] => ownerIdx[owner[id]] # NoId

\* RFC 8881: one confirmed record per co_ownerid.  Two agents sharing an
\* owner must COLLAPSE onto one record, not accumulate.
Inv_OneConfirmedPerOwner ==
  \A i, j \in live :
    (confirmed[i] /\ confirmed[j] /\ owner[i] = owner[j]) => i = j

\* A handshake that decided to supersede an incumbent must actually have
\* discarded it by the time the mount completes.  Stated against the
\* GHOST, so dropping `pending_replaces` cannot make it vacuously true.
Inv_ObligationHonoured ==
  \A a \in Agents :
    (st[a] = "mounted" /\ superseded[a] # NoId) => superseded[a] \notin live

Inv ==
  /\ Inv_NoOrphanLocks
  /\ Inv_LocksReapable
  /\ Inv_IndexCoversLiveOwners
  /\ Inv_OneConfirmedPerOwner
  /\ Inv_ObligationHonoured

=============================================================================
