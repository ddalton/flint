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

\* Owner strings.  With Collide every agent maps onto one key, which is
\* what a fleet applying one manifest per cluster actually produces.
OwnerOf(a) == IF Collide THEN "shared" ELSE a
Owners == { OwnerOf(a) : a \in Agents }

\* Clientids are minted from a counter, exactly as ClientManager does.
MaxId == Cardinality(Agents) * MaxMounts * NConnect + 1
Ids == 1..MaxId

VARIABLES
  live,        \* SUBSET Ids — the client table
  owner,       \* [Ids -> Owners]
  verifier,    \* [Ids -> Nat]      the client's boot verifier
  confirmed,   \* [Ids -> BOOLEAN]  a CREATE_SESSION landed
  pending,     \* [Ids -> Ids \cup {NoId}]  pending_replaces
  ownerIdx,    \* [Owners -> Ids \cup {NoId}]  owner_to_id
  locks,       \* SUBSET Ids — clientids holding a lock
  leases,      \* SUBSET Ids — clientids with a live lease
  nextId,
  vctr,        \* globally unique boot verifiers
  st,          \* [Agents -> {"idle","exchanging","mounted"}]
  cur,         \* [Agents -> Ids \cup {NoId}]  id its last EXCHANGE_ID produced
  conns,       \* [Agents -> Nat]  EXCHANGE_IDs issued this mount
  vmine,       \* [Agents -> Nat]  this mount's boot verifier
  mounts,      \* [Agents -> Nat]
  nlocks,
  superseded   \* GHOST [Agents -> Ids \cup {NoId}] — what this mount's
               \* handshake DECIDED to discard, recorded independently of
               \* whether the implementation kept the obligation.

vars == << live, owner, verifier, confirmed, pending, ownerIdx, locks,
           leases, nextId, vctr, st, cur, conns, vmine, mounts, nlocks,
           superseded >>

TypeOK ==
  /\ live \subseteq Ids
  /\ locks \subseteq Ids
  /\ leases \subseteq Ids
  /\ nextId \in 1..(MaxId + 1)
  /\ st \in [Agents -> {"idle","exchanging","mounted"}]

Init ==
  /\ live = {}
  /\ owner = [i \in Ids |-> "shared"]
  /\ verifier = [i \in Ids |-> 0]
  /\ confirmed = [i \in Ids |-> FALSE]
  /\ pending = [i \in Ids |-> NoId]
  /\ ownerIdx = [o \in Owners |-> NoId]
  /\ locks = {}
  /\ leases = {}
  /\ nextId = 1
  /\ vctr = 1
  /\ st = [a \in Agents |-> "idle"]
  /\ cur = [a \in Agents |-> NoId]
  /\ conns = [a \in Agents |-> 0]
  /\ vmine = [a \in Agents |-> 0]
  /\ mounts = [a \in Agents |-> 0]
  /\ nlocks = 0
  /\ superseded = [a \in Agents |-> NoId]

(***************************************************************************)
(* `remove_client_internal`, as an operator over a candidate index.  The   *)
(* whole of fix 3 is the guard: clear the entry only when it still names   *)
(* the departing client.  Locks are DELIBERATELY not touched here — that   *)
(* is the code, and `Inv_LocksReapable` is what makes the omission         *)
(* visible.                                                                *)
(***************************************************************************)
IdxAfterRemove(idx, id) ==
  IF CondIndexRemove
  THEN IF idx[owner[id]] = id THEN [idx EXCEPT ![owner[id]] = NoId] ELSE idx
  ELSE [idx EXCEPT ![owner[id]] = NoId]

(***************************************************************************)
(* A fresh mount: a new boot verifier, and the connection counter reset.   *)
(* `superseded` is cleared because the ghost tracks THIS handshake.        *)
(***************************************************************************)
StartMount(a) ==
  /\ st[a] = "idle"
  /\ mounts[a] < MaxMounts
  /\ nextId <= MaxId
  /\ st' = [st EXCEPT ![a] = "exchanging"]
  /\ conns' = [conns EXCEPT ![a] = 0]
  /\ vmine' = [vmine EXCEPT ![a] = vctr]
  /\ vctr' = vctr + 1
  /\ mounts' = [mounts EXCEPT ![a] = mounts[a] + 1]
  /\ cur' = [cur EXCEPT ![a] = NoId]
  /\ superseded' = [superseded EXCEPT ![a] = NoId]
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                  locks, leases, nextId, nlocks >>

(***************************************************************************)
(* EXCHANGE_ID on one connection.  The arms are client.rs's, with the      *)
(* principal always equal (see the header).                               *)
(***************************************************************************)
ExchangeId(a) ==
  LET o   == OwnerOf(a)
      ex  == ownerIdx[o]
      new == nextId
  IN
  /\ st[a] = "exchanging"
  /\ conns[a] < NConnect
  /\ nextId <= MaxId
  /\ \/ /\ ex = NoId                      \* no record: allocate
        /\ live' = live \cup {new}
        /\ owner' = [owner EXCEPT ![new] = o]
        /\ verifier' = [verifier EXCEPT ![new] = vmine[a]]
        /\ confirmed' = [confirmed EXCEPT ![new] = FALSE]
        /\ pending' = [pending EXCEPT ![new] = NoId]
        /\ ownerIdx' = [ownerIdx EXCEPT ![o] = new]
        /\ nextId' = nextId + 1
        /\ cur' = [cur EXCEPT ![a] = new]
        /\ UNCHANGED << leases, superseded >>
     \/ /\ ex # NoId
        /\ ~confirmed[ex]                 \* CASE 4: replace the unconfirmed
        /\ LET inherited == IF CarryObligation THEN pending[ex] ELSE NoId
           IN /\ live' = (live \ {ex}) \cup {new}
              /\ owner' = [owner EXCEPT ![new] = o]
              /\ verifier' = [verifier EXCEPT ![new] = vmine[a]]
              /\ confirmed' = [confirmed EXCEPT ![new] = FALSE]
              /\ pending' = [pending EXCEPT ![new] = inherited]
              /\ ownerIdx' = [IdxAfterRemove(ownerIdx, ex) EXCEPT ![o] = new]
              /\ leases' = leases \ {ex}
              /\ nextId' = nextId + 1
              /\ cur' = [cur EXCEPT ![a] = new]
              /\ UNCHANGED superseded
     \/ /\ ex # NoId
        /\ confirmed[ex]
        /\ verifier[ex] = vmine[a]        \* CASE 1: renewal, reuse the id
        /\ cur' = [cur EXCEPT ![a] = ex]
        /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                        leases, nextId, superseded >>
     \/ /\ ex # NoId
        /\ confirmed[ex]
        /\ verifier[ex] # vmine[a]        \* CASE 5: the incumbent "rebooted"
        /\ live' = live \cup {new}
        /\ owner' = [owner EXCEPT ![new] = o]
        /\ verifier' = [verifier EXCEPT ![new] = vmine[a]]
        /\ confirmed' = [confirmed EXCEPT ![new] = FALSE]
        /\ pending' = [pending EXCEPT ![new] = ex]
        /\ ownerIdx' = [ownerIdx EXCEPT ![o] = new]
        /\ nextId' = nextId + 1
        /\ cur' = [cur EXCEPT ![a] = new]
        /\ leases' = leases
        \* The GHOST records the obligation the RFC imposes, whatever the
        \* implementation then does with `pending`.
        /\ superseded' = [superseded EXCEPT ![a] = ex]
  /\ conns' = [conns EXCEPT ![a] = conns[a] + 1]
  /\ UNCHANGED << locks, st, vmine, mounts, vctr, nlocks >>

(***************************************************************************)
(* CREATE_SESSION.  `mark_confirmed` returns the pending replacement and   *)
(* the handler cascades.  CascadeLocks is fix 2: release the victim's      *)
(* locks BEFORE the record goes, because `remove_client` drops the lease   *)
(* and the only reaper iterates expired leases.                            *)
(***************************************************************************)
CreateSession(a) ==
  LET id  == cur[a]
      old == pending[id]
  IN
  /\ st[a] = "exchanging"
  /\ conns[a] = NConnect
  /\ id # NoId
  /\ id \in live
  /\ confirmed' = [confirmed EXCEPT ![id] = TRUE]
  /\ pending' = [pending EXCEPT ![id] = NoId]
  /\ IF old # NoId /\ old \in live
     THEN /\ live' = live \ {old}
          /\ ownerIdx' = IdxAfterRemove(ownerIdx, old)
          /\ locks' = IF CascadeLocks THEN locks \ {old} ELSE locks
          /\ leases' = (leases \ {old}) \cup {id}
     ELSE /\ live' = live
          /\ ownerIdx' = ownerIdx
          /\ locks' = locks
          /\ leases' = leases \cup {id}
  /\ st' = [st EXCEPT ![a] = "mounted"]
  /\ UNCHANGED << owner, verifier, nextId, vctr, cur, conns, vmine, mounts,
                  nlocks, superseded >>

TakeLock(a) ==
  /\ st[a] = "mounted"
  /\ nlocks < MaxLocks
  /\ cur[a] \in live
  /\ locks' = locks \cup {cur[a]}
  /\ nlocks' = nlocks + 1
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                  leases, nextId, vctr, st, cur, conns, vmine, mounts,
                  superseded >>

(***************************************************************************)
(* DESTROY_CLIENTID — a clean unmount.  This is the action that, without   *)
(* fix 3, evicts a LIVE peer from the owner index.                         *)
(***************************************************************************)
Unmount(a) ==
  /\ st[a] = "mounted"
  /\ cur[a] \in live
  /\ live' = live \ {cur[a]}
  /\ ownerIdx' = IdxAfterRemove(ownerIdx, cur[a])
  /\ leases' = leases \ {cur[a]}
  /\ locks' = locks \ {cur[a]}
  /\ st' = [st EXCEPT ![a] = "idle"]
  /\ cur' = [cur EXCEPT ![a] = NoId]
  /\ superseded' = [superseded EXCEPT ![a] = NoId]
  /\ UNCHANGED << owner, verifier, confirmed, pending, nextId, vctr, conns,
                  vmine, mounts, nlocks >>

(***************************************************************************)
(* `courtesy_release_expired`: the ONLY production caller of               *)
(* remove_client_locks, and it iterates EXPIRED LEASES.  So it can only    *)
(* ever reach a clientid that still has one.                               *)
(***************************************************************************)
LeaseExpire(id) ==
  /\ id \in leases
  /\ id \in live
  /\ \A a \in Agents : cur[a] # id \/ st[a] # "mounted"
  /\ locks' = locks \ {id}
  /\ live' = live \ {id}
  /\ ownerIdx' = IdxAfterRemove(ownerIdx, id)
  /\ leases' = leases \ {id}
  /\ UNCHANGED << owner, verifier, confirmed, pending, nextId, vctr, st,
                  cur, conns, vmine, mounts, nlocks, superseded >>

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

\* And whatever holds one must remain reachable by the reaper, which
\* iterates expired LEASES.  A lock whose client has no lease can never
\* be collected by anything, survives restarts (locks are persisted and
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

\* A handshake that decided to supersede an incumbent must have actually
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
