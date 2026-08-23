-------------------------- MODULE FlintClientIdentity --------------------------
(***************************************************************************)
(* The NFSv4.1 client-record lifecycle, keyed on an identity that is NOT   *)
(* unique — src/nfs/v4/state/client.rs `exchange_id` /                     *)
(* `remove_client_internal`, the case-5 cascade in                         *)
(* src/nfs/v4/operations/session.rs `handle_create_session`, and the       *)
(* two-phase lease sweep in src/nfs/v4/dispatcher.rs                       *)
(* `courtesy_release_expired` + src/nfs/v4/state/mod.rs `cleanup_expired`. *)
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
(*   THE SWEEP IS DRIVEN BY WHOEVER SENDS TRAFFIC, NOT BY WHOSE LEASE IT   *)
(*   IS.  `courtesy_release_expired` runs at the top of EVERY COMPOUND and *)
(*   reaps EVERY expired client, not the caller's.  On one cluster that is *)
(*   nearly invisible; with several clusters on one hub it means cluster   *)
(*   B's traffic is what releases cluster A's locks, while A's own renewal *)
(*   is in flight on another thread.  That is the interleaving the lease   *)
(*   dimension below exists to enumerate.                                  *)
(*                                                                         *)
(* WHAT IS MODELLED.  Per EXCHANGE_ID the four arms the code actually      *)
(* distinguishes on (owner present?, incumbent confirmed?, verifier        *)
(* equal?) — RFC cases 1, 3/4 and 5.  The principal is always equal:       *)
(* AUTH_SYS derives it from the same nodename, so a co_ownerid collision   *)
(* is a principal collision too, and the cases that turn on a principal    *)
(* MISMATCH are unreachable in the situation this module is about.  Plus,  *)
(* under `ModelLease`, lease lapse / the two-phase sweep / SEQUENCE        *)
(* renewal / BADSESSION recovery.                                          *)
(*                                                                         *)
(* WHAT IS NOT.  Sequence-id / replay caching (§18.36.4) — a different     *)
(* machine with its own drills; back channels; and the wire.  Locks are    *)
(* modelled only as "client c holds one", because the defect is that they  *)
(* OUTLIVE c, not anything about ranges.  Real time is not modelled: a     *)
(* lease lapses by a nondeterministic action, which is strictly weaker     *)
(* than assuming any particular 90s / 30s relationship and therefore       *)
(* cannot be wrong about one.                                              *)
(*                                                                         *)
(* WHAT THE LEASE DIMENSION FOUND, added 2026-08-22 after the identity     *)
(* fixes.  Three more defects, all shipped, none of them visible to the    *)
(* drill that found the first three:                                       *)
(*                                                                         *)
(*   1. An ORPHANED LOCK.  The sweep strips locks in phase 1 and retires   *)
(*      the record in phase 2, so a lock granted in between has no client, *)
(*      no lease and no reaper.  Fixed by retiring the record FIRST.       *)
(*                                                                         *)
(*   2. A SILENT LOSS.  Phase 2 re-read `get_expired_clients()`, making a  *)
(*      second decision about who was expired after phase 1 had already    *)
(*      acted on the first.  Fixed by passing the snapshot                 *)
(*      (`cleanup_expired_ids`).                                           *)
(*                                                                         *)
(*   3. AN UNGUARDED OWNER-INDEX REMOVAL.  The conditional-index fix from  *)
(*      the drill went in on `remove_client_internal` and stopped there;   *)
(*      the public `remove_client` — reached from DESTROY_CLIENTID, the    *)
(*      lease sweep and the case-5 cascade — kept an unconditional         *)
(*      `owner_to_id.remove`.  This module found it by applying its index  *)
(*      guard at EVERY removal site uniformly, so the asymmetry in the     *)
(*      code had nowhere to hide.  That is the argument for a model over   *)
(*      a test: a test walks the site it was written for.                  *)
(*                                                                         *)
(*   And one DESIGN result about a fix rather than a defect: sr_status_    *)
(*   flags is addressed to a CLIENTID, and under a co_ownerid collision    *)
(*   two clusters share one — so the flag can reach the wrong cluster      *)
(*   entirely.  See FlintClientIdentityLeaseNotify.cfg and its Unique      *)
(*   counterpart, which differ in one constant.                            *)
(*                                                                         *)
(* THE MUTATIONS, each the shipped code before — or, for the last two,     *)
(* WITHOUT — its fix:                                                      *)
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
(*                                                                         *)
(*   AtomicSweep = FALSE — the SHIPPED sweep, and the reason this          *)
(*     dimension was added.  `courtesy_release_expired` reads              *)
(*     `get_expired_clients()`, strips those clients' locks, and then      *)
(*     calls `cleanup_expired()` — which reads `get_expired_clients()` A   *)
(*     SECOND TIME to decide whose sessions, stateids, delegations and     *)
(*     client record to destroy.  `renew_lease` is documented LOCK-FREE    *)
(*     ("per-client locking only, not global"), so a SEQUENCE arriving     *)
(*     between the two reads renews the lease and the second read no       *)
(*     longer sees the client.  Its locks are already gone.  Everything    *)
(*     else survives.  See Inv_LockLossIsDetectable.                       *)
(*                                                                         *)
(*   NotifyRevoked = FALSE — the SHIPPED SEQUENCE reply.                   *)
(*     `dispatcher.rs` hardcodes `status_flags: 0` with the comment "For   *)
(*     basic implementation, 0 is sufficient", so                          *)
(*     SEQ4_STATUS_EXPIRED_SOME_STATE_REVOKED is never set and a client    *)
(*     that kept its session across a revocation is never told it lost     *)
(*     anything.  Setting the flag is not a one-line change — the server   *)
(*     keeps no record of WHAT it revoked — which is the cost `revoked`    *)
(*     below exists to make visible.                                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  Agents,           \* distinct real clients — agent pods, possibly in different clusters
  Collide,          \* TRUE: every agent presents ONE co_ownerid (the shipped reality)
  NConnect,         \* EXCHANGE_IDs per mount (the trunking probe); >= 2 is the default
  CarryObligation,  \* case 4 carries pending_replaces forward           (fix 1)
  CondIndexRemove,  \* the owner index is cleared only if it names us    (fix 3)
  CascadeLocks,     \* the case-5 cascade releases the victim's locks    (fix 2)
  ModelLease,       \* enumerate lease lapse / sweep / renewal / recovery
  AtomicSweep,      \* sweep phase 2 acts on phase 1's snapshot, not a re-read
  NotifyRevoked,    \* SEQUENCE reports revoked state in sr_status_flags
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
(*                                                                         *)
(*   The lease dimension is gated on `ModelLease` for the same reason.  It *)
(*   is very nearly orthogonal to the trunking-probe dimension, so running *)
(*   their full cross product multiplies the space in order to enumerate   *)
(*   interleavings that share no variable.  ModelLease = FALSE leaves      *)
(*   every lease variable at its initial value and costs the identity runs *)
(*   nothing; the ONE place the two dimensions genuinely meet — a sweep    *)
(*   landing mid-handshake — gets its own run with both switched on.       *)
(***************************************************************************)
MaxId == Cardinality(Agents) * 2 + 1
Ids == 1..MaxId

VARIABLES
  live, owner, verifier, confirmed, pending, ownerIdx, locks, leases,
  st, cur, conns, vmine, mounts, nlocks, superseded,
  expired, sweepSnap, stripPending, revoked, believes, notified

vars == << live, owner, verifier, confirmed, pending, ownerIdx, locks,
           leases, st, cur, conns, vmine, mounts, nlocks, superseded,
           expired, sweepSnap, stripPending, revoked, believes, notified >>

\* The six lease-dimension variables, untouched by every identity action.
leaseVars == << expired, sweepSnap, stripPending, revoked, believes, notified >>

TypeOK ==
  /\ live \subseteq Ids
  /\ locks \subseteq Ids
  /\ leases \subseteq Ids
  /\ expired \subseteq leases
  /\ sweepSnap \subseteq Ids
  /\ stripPending \subseteq Ids
  /\ revoked \subseteq Ids
  /\ believes \subseteq Agents
  /\ notified \subseteq Agents
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
  /\ expired = {}
  /\ sweepSnap = {}
  /\ stripPending = {}
  /\ revoked = {}
  /\ believes = {}
  /\ notified = {}

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
\*
\* `sweepSnap` and `revoked` are here for exactly that reason and were
\* added WITH the lease dimension rather than after it, because the
\* aliasing failure above is not one this module gets to learn twice.
Referenced ==
  live \cup { cur[a] : a \in Agents } \cup { pending[i] : i \in live }
       \cup { superseded[a] : a \in Agents }
       \cup sweepSnap \cup stripPending \cup revoked
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
  /\ believes' = believes \ {a}
  /\ notified' = notified \ {a}
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   locks, leases, nlocks, expired, sweepSnap, stripPending,
                   revoked >>

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
  /\ UNCHANGED leaseVars

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
          \* The displaced record leaves the lease dimension with it.
          /\ expired' = expired \ {old}
          /\ sweepSnap' = sweepSnap \ {old}
          /\ stripPending' = stripPending \ {old}
          /\ revoked' = revoked \ {old}
     ELSE /\ live' = live
          /\ ownerIdx' = ownerIdx
          /\ locks' = locks
          /\ leases' = leases \cup {id}
          /\ confirmed' = [confirmed EXCEPT ![id] = TRUE]
          /\ pending' = [pending EXCEPT ![id] = NoId]
          /\ UNCHANGED << owner, verifier, expired, sweepSnap, stripPending,
                           revoked >>
  /\ st' = [st EXCEPT ![a] = "mounted"]
  /\ UNCHANGED << cur, conns, vmine, mounts, nlocks, superseded, believes,
                   notified >>

TakeLock(a) ==
  /\ st[a] = "mounted"
  /\ nlocks < MaxLocks
  /\ cur[a] \in live
  /\ locks' = locks \cup {cur[a]}
  /\ nlocks' = nlocks + 1
  \* The client's own belief that it holds a byte range.  Tracked only
  \* under ModelLease, so the identity runs pay nothing for it.
  /\ believes' = IF ModelLease THEN believes \cup {a} ELSE believes
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   leases, st, cur, conns, vmine, mounts, superseded,
                   expired, sweepSnap, stripPending, revoked, notified >>

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
  /\ expired' = expired \ {cur[a]}
  /\ sweepSnap' = sweepSnap \ {cur[a]}
  /\ stripPending' = stripPending \ {cur[a]}
  /\ revoked' = revoked \ {cur[a]}
  /\ believes' = believes \ {a}
  /\ notified' = notified \ {a}
  /\ UNCHANGED << conns, vmine, mounts, nlocks >>

\* `courtesy_release_expired`, in the ModelLease = FALSE form: atomic, and
\* only for an id no agent is actively using.  The two-phase form below is
\* the one that matches the shipped code.
LeaseExpire(id) ==
  /\ ~ModelLease
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
  /\ UNCHANGED leaseVars

(***************************************************************************)
(* THE LEASE DIMENSION.                                                    *)
(*                                                                         *)
(* Note what is NOT assumed: no clock, no 90s, no 30s sweep period.  A     *)
(* lease lapses whenever `LeaseLapse` is taken and is renewed whenever     *)
(* `Sequence` is.  That is strictly weaker than any particular timing, so  *)
(* a result here cannot be an artifact of the numbers — and the numbers    *)
(* are exactly what a rig cannot control, which is why the L4 kind leg     *)
(* could watch the timer work and still say nothing about this.            *)
(***************************************************************************)

\* The lease period passes with no traffic from this client — idle, or
\* partitioned away while perfectly alive.  The client is not told, and
\* has no way to know.
LeaseLapse(id) ==
  /\ ModelLease
  /\ id \in leases
  /\ id \in live
  /\ id \notin expired
  /\ expired' = expired \cup {id}
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   locks, leases, st, cur, conns, vmine, mounts, nlocks,
                   superseded, sweepSnap, stripPending, revoked, believes,
                   notified >>

\* THE FIX, AS SHIPPED — and it is an ORDERING fix, not an atomicity one.
\* Modelled that way on purpose: `courtesy_release_expired` retires the
\* records from ONE reading and only then strips the locks, in a tight loop
\* with no await, but the two are still separate steps and a model that
\* collapsed them would be checking an idealisation the code does not
\* implement.
\*
\* Step 1: the record dies.  Note what this buys — `TakeLock` requires
\* `cur[a] \in live`, so from here on NO further lock can be granted to this
\* id.  That is the whole reason the order is this way round and not the
\* other: the shipped order leaves its window open while the client is still
\* ALIVE, so a lock can be granted into it and then orphaned forever.
SweepRetire(id) ==
  /\ ModelLease
  /\ AtomicSweep
  /\ id \in expired
  /\ live' = live \ {id}
  /\ ownerIdx' = IdxAfterRemove(ownerIdx, id)
  /\ leases' = leases \ {id}
  /\ expired' = expired \ {id}
  /\ owner' = [owner EXCEPT ![id] = NoOwner]
  /\ verifier' = [verifier EXCEPT ![id] = 0]
  /\ confirmed' = [confirmed EXCEPT ![id] = FALSE]
  /\ pending' = [pending EXCEPT ![id] = NoId]
  /\ stripPending' = stripPending \cup {id}
  /\ UNCHANGED << locks, st, cur, conns, vmine, mounts, nlocks, superseded,
                   sweepSnap, revoked, believes, notified >>

\* Step 2: the locks go.  Anything granted before step 1 is collected here;
\* nothing can have been granted after it.
SweepStrip(id) ==
  /\ ModelLease
  /\ AtomicSweep
  /\ id \in stripPending
  /\ locks' = locks \ {id}
  /\ stripPending' = stripPending \ {id}
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   leases, expired, st, cur, conns, vmine, mounts, nlocks,
                   superseded, sweepSnap, revoked, believes, notified >>

\* PHASE 1 of the SHIPPED sweep, driven by ANY client's COMPOUND:
\*   let expired = leases.get_expired_clients();            <-- read 1
\*   for cid in &expired { lock_mgr.remove_client_locks(cid); }
\* `sweepSnap` is that local vector, which phase 2 never sees.
SweepLocks(id) ==
  /\ ModelLease
  /\ ~AtomicSweep
  /\ id \in expired
  /\ id \notin sweepSnap
  /\ locks' = locks \ {id}
  /\ sweepSnap' = sweepSnap \cup {id}
  \* Server-side memory of the revocation, so a later SEQUENCE could
  \* report it.  This does NOT exist in the shipped server; tracking it
  \* only under NotifyRevoked is what keeps the model honest that setting
  \* sr_status_flags means ADDING this, not flipping a constant.
  /\ revoked' = IF NotifyRevoked THEN revoked \cup {id} ELSE revoked
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   leases, expired, st, cur, conns, vmine, mounts, nlocks,
                   superseded, stripPending, believes, notified >>

\* PHASE 2 — `cleanup_expired()`, which opens with read 2:
\*   let expired_clients = self.leases.get_expired_clients();
\* It re-reads, and so silently skips anyone who renewed in between.  It
\* does not touch locks either way: LockManager lives outside StateManager
\* and phase 1 is the only thing that ever reaches it.  The snapshot is
\* consumed — the real one is a local that goes out of scope, and nothing
\* anywhere remembers it.
SweepState(id) ==
  /\ ModelLease
  /\ ~AtomicSweep
  /\ id \in sweepSnap
  /\ sweepSnap' = sweepSnap \ {id}
  /\ IF id \in expired
     THEN /\ live' = live \ {id}
          /\ ownerIdx' = IdxAfterRemove(ownerIdx, id)
          /\ leases' = leases \ {id}
          /\ expired' = expired \ {id}
          /\ owner' = [owner EXCEPT ![id] = NoOwner]
          /\ verifier' = [verifier EXCEPT ![id] = 0]
          /\ confirmed' = [confirmed EXCEPT ![id] = FALSE]
          /\ pending' = [pending EXCEPT ![id] = NoId]
          \* The client record is gone, so there is nobody left to tell.
          /\ revoked' = revoked \ {id}
     ELSE UNCHANGED << live, ownerIdx, leases, expired, owner, verifier,
                        confirmed, pending, revoked >>
  /\ UNCHANGED << locks, st, cur, conns, vmine, mounts, nlocks, superseded,
                   stripPending, believes, notified >>

\* A SEQUENCE from a live client: renews the lease (`renew_lease`, which is
\* LOCK-FREE and therefore concurrent with any sweep) and carries
\* sr_status_flags back — but only if the server kept a record to report.
\* Guarded to the cases that change something: renewing an unexpired lease
\* is a no-op, and enumerating it buys nothing.
Sequence(a) ==
  /\ ModelLease
  /\ st[a] = "mounted"
  /\ cur[a] \in live
  /\ cur[a] \in leases
  /\ \/ cur[a] \in expired
     \/ (NotifyRevoked /\ cur[a] \in revoked)
  /\ expired' = expired \ {cur[a]}
  /\ IF NotifyRevoked /\ cur[a] \in revoked
     THEN /\ notified' = notified \cup {a}
          /\ revoked' = revoked \ {cur[a]}
     ELSE UNCHANGED << notified, revoked >>
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   locks, leases, st, cur, conns, vmine, mounts, nlocks,
                   superseded, sweepSnap, stripPending, believes >>

\* The client's next COMPOUND finds its client record gone and gets
\* BADSESSION / STALE_CLIENTID.  This is the GOOD outcome: it is loud, the
\* client does a full recovery, and it stops believing anything.
Recover(a) ==
  /\ ModelLease
  /\ st[a] = "mounted"
  /\ cur[a] \notin live
  /\ st' = [st EXCEPT ![a] = "idle"]
  /\ cur' = [cur EXCEPT ![a] = NoId]
  /\ superseded' = [superseded EXCEPT ![a] = NoId]
  /\ believes' = believes \ {a}
  /\ notified' = notified \ {a}
  /\ UNCHANGED << live, owner, verifier, confirmed, pending, ownerIdx,
                   locks, leases, conns, vmine, mounts, nlocks,
                   expired, sweepSnap, stripPending, revoked >>

Next ==
  \/ \E a \in Agents : StartMount(a)
  \/ \E a \in Agents : ExchangeId(a)
  \/ \E a \in Agents : CreateSession(a)
  \/ \E a \in Agents : TakeLock(a)
  \/ \E a \in Agents : Unmount(a)
  \/ \E id \in Ids : LeaseExpire(id)
  \/ \E id \in Ids : LeaseLapse(id)
  \/ \E id \in Ids : SweepRetire(id)
  \/ \E id \in Ids : SweepStrip(id)
  \/ \E id \in Ids : SweepLocks(id)
  \/ \E id \in Ids : SweepState(id)
  \/ \E a \in Agents : Sequence(a)
  \/ \E a \in Agents : Recover(a)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* THE INVARIANTS.                                                         *)
(***************************************************************************)

\* A lock must not outlive the client that holds it.
\*
\* `stripPending` is the fix's own window — between retiring the record and
\* stripping the locks — and it is admitted for the same reason `sweepSnap`
\* is admitted below: this is a claim about SETTLED states.  The window is
\* SAFE where the shipped one is not, and the asymmetry is the point: here
\* the client is already gone, so `TakeLock` is disabled and nothing new can
\* enter; in the shipped order the client is still LIVE throughout, which is
\* exactly how a lock gets granted into it and orphaned.
Inv_NoOrphanLocks == \A id \in locks : id \in live \/ id \in stripPending

\* And whatever holds one must stay reachable by the reaper, which
\* iterates expired LEASES.  A lock whose client has no lease can never be
\* collected by anything, survives restart (locks are persisted and
\* re-seeded), and denies its range to every other client forever.
\*
\* `stripPending` again, and this is the sharpest place to be precise about
\* what the shipped fix does and does not achieve.  It is an ORDERING fix,
\* not an atomicity one: `courtesy_release_expired` retires the record —
\* which drops the lease — and only then strips the locks, so between those
\* two steps a lock genuinely does exist with no lease behind it.  What
\* makes that window sound rather than an orphan is that the thing which
\* closes it is ALREADY SCHEDULED: it is the next statement in the same
\* loop, with no await between, and no new lock can enter because the
\* client is gone.  Contrast the shipped order, where the window is opened
\* by the strip and closed by a re-read that may decide to do nothing at
\* all — there, "reapable" is false permanently, not transiently.
Inv_LocksReapable == \A id \in locks : id \in leases \/ id \in stripPending

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

(***************************************************************************)
(* THE LEASE THEOREM.  Losing a lock to an expired lease is CORRECT — it   *)
(* is the entire point of courtesy release.  What is not correct is losing *)
(* it SILENTLY.  A client that still believes it holds a byte range, and   *)
(* whose server no longer agrees, has exactly three acceptable futures:    *)
(*                                                                         *)
(*   its record is gone   — its next COMPOUND gets BADSESSION /            *)
(*                          STALE_CLIENTID and it does a full recovery;    *)
(*   it was told          — SEQUENCE came back carrying                    *)
(*                          SEQ4_STATUS_EXPIRED_SOME_STATE_REVOKED; or     *)
(*   it is owed the flag  — the server holds a record it will report on    *)
(*                          this client's next SEQUENCE.                   *)
(*                                                                         *)
(* Anything else is two writers who both believe they hold the range, with *)
(* nothing on the wire that either could have noticed.                     *)
(*                                                                         *)
(* TWO DELIBERATE WEAKENINGS, both load-bearing, both written down here    *)
(* because a silent one would make this theorem say less than it appears   *)
(* to say:                                                                 *)
(*                                                                         *)
(*   `sweepSnap` — an id still in phase 1's snapshot is MID-SWEEP and its  *)
(*   fate is not yet decided, so this is a claim about SETTLED states and  *)
(*   not about the instant between the sweep's two phases.                 *)
(*                                                                         *)
(*   `revoked` — notification is DEFERRED by construction: a client learns *)
(*   on its next SEQUENCE, not at the instant of revocation, so a server   *)
(*   holding a record it will report is discharging the obligation rather  *)
(*   than in breach of it.  Without this disjunct the notify fix fails a   *)
(*   safety property that NO notify fix could satisfy — which is exactly   *)
(*   how this run first came back red, and the reason it is spelled out    *)
(*   instead of quietly patched.  It costs nothing under the shipped       *)
(*   posture: NotifyRevoked = FALSE leaves `revoked` empty forever, so the *)
(*   disjunct is unavailable precisely when the defect is present.         *)
(***************************************************************************)
Inv_LockLossIsDetectable ==
  \A a \in Agents :
    (/\ st[a] = "mounted"
     /\ a \in believes
     /\ cur[a] \notin locks
     /\ cur[a] \notin sweepSnap)
      => \/ cur[a] \notin live      \* gone: its next COMPOUND gets BADSESSION
         \/ a \in notified          \* told: SEQ4_STATUS_..._STATE_REVOKED
         \/ cur[a] \in revoked      \* tellable: the server owes it a flag

(***************************************************************************)
(* VACUITY PROBE for the mid-handshake run, and it is a REQUIRED-FAIL.     *)
(*                                                                         *)
(* `LeaseHandshake` exists to answer whether the case-5 obligation carry   *)
(* survives a sweep that retires records while a mount's trunking probe is *)
(* still in flight. It passes — but a run that passes because the          *)
(* interleaving is never REACHED proves nothing at all, and this repo has  *)
(* shipped exactly that mistake often enough to write the check down.      *)
(*                                                                         *)
(* TLC must VIOLATE this: a state where some agent is mid-handshake and a  *)
(* sweep has retired a record and not yet stripped its locks. The          *)
(* counterexample IS the evidence that LeaseHandshake has teeth.           *)
(***************************************************************************)
Probe_SweepLandsMidHandshake ==
  \A a \in Agents : st[a] # "exchanging" \/ stripPending = {}

Inv ==
  /\ Inv_NoOrphanLocks
  /\ Inv_LocksReapable
  /\ Inv_IndexCoversLiveOwners
  /\ Inv_OneConfirmedPerOwner
  /\ Inv_ObligationHonoured
  /\ Inv_LockLossIsDetectable

=============================================================================
