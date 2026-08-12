----------------------------- MODULE FlintExtents -----------------------------
(***************************************************************************)
(* The pNFS BLOCK-LAYOUT extent allocator — grant/fence/reuse core.        *)
(*                                                                         *)
(* docs/plans/pnfs-block-layout-design.md §9 is this module's spec; this   *)
(* file is TRANCHE 1 of it, deliberately: the grant lifecycle, the recall/ *)
(* fence machinery, physical block reuse, and the target-side reservation  *)
(* state.  It models code that DOES NOT EXIST YET — that is the point (the *)
(* module and its drills come before the allocator, the way FlintTruncate  *)
(* would have wanted to come before F65) — so every green here is a        *)
(* statement about the DESIGN, and the design must be implemented against  *)
(* these runs, not vice versa.                                             *)
(*                                                                         *)
(* THE HEADLINE HAZARD, and why the WRITE is the theorem: a block-layout   *)
(* client holds raw LBAs and writes them over NVMe/TCP with the MDS        *)
(* nowhere on the path.  When the MDS frees a client's extent and hands    *)
(* the physical range to a new owner, a stale holder that never heard the  *)
(* recall corrupts the NEW owner's committed bytes.  FlintTruncate's       *)
(* read-only shape was right for its layer; copied here it would be a      *)
(* blind spot, because the entire point of RFC 8154/9561 fencing is        *)
(* stopping WRITES.  Inv_NoStaleExtentWrite is co-equal with the read      *)
(* theorem and listed first.                                               *)
(*                                                                         *)
(* TRANCHE 1 FINDING — THE DOC'S OWN ARM SET WAS INSUFFICIENT.  §9 as      *)
(* first written closed the grant-vs-reclaim races with two grant-side     *)
(* belts: RecallBlocksGrant (refuse at the gate check while a reclaim is   *)
(* in flight) and PublishRecheck (re-read the reclaim after publishing,    *)
(* self-revoke — C6's shape, inherited from FlintTruncate).  Working the   *)
(* two-step honesty rule through THIS module's actions shows a window      *)
(* neither can close: the reclaim snapshots its holders BEFORE a racing    *)
(* grant publishes, and can complete-and-free BETWEEN that grant's insert  *)
(* and its recheck — the recheck then reads reclaim.active = FALSE and     *)
(* revokes nothing, and the blocks are already free under a live grant.    *)
(* FlintTruncate survives the analogous interleave only because its        *)
(* "free" (the DS set_len fanout) destroys the CONTENT, so a layout that   *)
(* slips through reads truncated-to-zero bytes, not a new owner's; an      *)
(* extent free destroys NOTHING — the harm arrives with the next owner.    *)
(* The repair is on the FREE side: the free transaction re-validates the   *)
(* grants table (sqlite-native — the free and the grant insert execute     *)
(* over the same tables) and refuses while a live unfenced grant covers    *)
(* any block.  That is the FreeRevalidates arm, and                        *)
(* FlintExtentsStaleSnapshotFree.cfg pins the refuted bookkeeping-only     *)
(* design permanently, MarkOverwrite-style.  With FreeRevalidates          *)
(* carrying safety, PublishRecheck and RecallBlocksGrant become PROGRESS   *)
(* arms (they keep the reclaim from wedging behind grants it must then     *)
(* fence) — they are DEFERRED to the liveness tranche, where they get      *)
(* teeth an invariant cannot give them.                                    *)
(*                                                                         *)
(* WHAT THIS TRANCHE MODELS:                                               *)
(*   - Physical blocks over a small domain; extents are BLOCK-SETS, not    *)
(*     atomic identities (the scalar-raidHost lesson: fixed extent         *)
(*     identities make range aliasing unrepresentable).                    *)
(*   - gen[b], bumped on every free->provisional edge: the reuse detector. *)
(*     Zeros from a fresh extent are not stale content; bytes from a       *)
(*     PREVIOUS OWNER are — which is why gen, and not content-emptiness,   *)
(*     carries both theorems.                                              *)
(*   - Two-step LAYOUTGET (GrantCheck / GrantInsert) because the           *)
(*     implementation will be two steps: an in-memory gate read, then a    *)
(*     sqlite transaction.  Collapsing them is what made FlintTruncate     *)
(*     green on a property the code did not hold.                          *)
(*   - The reclaim (recall-then-free) with its holder SNAPSHOT taken at    *)
(*     start — check-then-act modelled as the code will actually be —      *)
(*     plus ReclaimResnapshot, the retry loop's separate re-read step      *)
(*     (the F62 idiom: the re-read must be its own action so TLC can       *)
(*     interleave against it).                                             *)
(*   - resv, the TARGET-side reservation registry, as REAL STATE: the set  *)
(*     of clients whose registrations have been preempted (RTYPE=4h EARO:  *)
(*     a preempted host's reads AND writes are refused).  All clients      *)
(*     start registered; TgtRestart without PTPL clears the reservation    *)
(*     and every exclusion with it — spdk-tgt is the most-restarted        *)
(*     component in the system (FlintReplication proved it), and without   *)
(*     this action §5's "PTPL is mandatory" cannot be STATED, let alone    *)
(*     checked.                                                            *)
(*                                                                         *)
(* FIX ARMS (TRUE = belt exists), each with a run that fails without it:   *)
(*   GrantsExclusive      the grant transaction validates physical         *)
(*                        disjointness against live grant rows.  The       *)
(*                        mutation is §8's recorded landmine: the PK does  *)
(*                        not police overlap.                              *)
(*   RecallBeforeReuse    frees happen only through the reclaim (recall    *)
(*                        every holder, wait for return-or-fence).  The    *)
(*                        mutation frees directly under live grants — the  *)
(*                        F65-of-extents.                                  *)
(*   FreeRevalidates      the free transaction re-validates holders        *)
(*                        (tranche-1 finding, above).                      *)
(*   FenceReaches         EVERY fence lands as a real exclusion at the    *)
(*                        target.  STAYS FALSE IN THE SHIPPED CFG — and    *)
(*                        now for a sharper reason than unproven           *)
(*                        hardware: the fence rig (2026-08-10, real        *)
(*                        kernel + real spdk-tgt) proved a CONFIRMED      *)
(*                        preempt really excludes, but the code's preempt  *)
(*                        arm is best-effort and can FAIL at runtime (tgt  *)
(*                        unreachable), so "every fence lands" remains a   *)
(*                        lie about the code.  The graduation is           *)
(*                        FreeRequiresDelivered below: trust CONFIRMED     *)
(*                        exclusions (rig-proven mechanism), never         *)
(*                        unconfirmed ones.                                *)
(*   FreeRequiresDelivered the free transaction additionally requires      *)
(*                        every FENCED holder to be in resv — the code's   *)
(*                        delivered bit (set only on a verified preempt:   *)
(*                        post-report holder==MDS key, victim absent).     *)
(*                        An unconfirmed fence refuses the free = the      *)
(*                        code QUARANTINES that extent.  This is what      *)
(*                        lets the shipped cfg claim both stale theorems   *)
(*                        in the fences-CAN-fail world — a strictly        *)
(*                        stronger claim than Target.cfg's FenceReaches    *)
(*                        ideal.  LostFence (the single-flag A/B) pins     *)
(*                        the hole it closes.                              *)
(*   PersistReservations  PTPL: the exclusion registry survives            *)
(*                        TgtRestart.  §5 calls PTPL mandatory;            *)
(*                        FlintExtentsTgtAmnesia.cfg is that sentence      *)
(*                        with teeth.                                      *)
(*                                                                         *)
(* ABSTRACTIONS, STATED (the module's THE-ABSTRACTION-WAS-THE-BUG          *)
(* surface):                                                               *)
(*   1. One volume, one namespace, one target.  Multi-target/trunking is   *)
(*      scenery until the code has it; resv would become a per-target      *)
(*      function then ("if two tgt incarnations can ever expose one        *)
(*      extent range, the serving-target state is a SET from day one").    *)
(*   2. One grant per client, whole-grant recalls.  Range granularity of   *)
(*      recalls changes WHICH grant a reclaim waits on, not whether the    *)
(*      snapshot/free machinery is sound; per-range recall records are     *)
(*      the allocator tranche's business.                                  *)
(*   3. Harm is gen-mismatch, NOT fenced-writing.  The doc's one-line      *)
(*      sketch had staleWrite fire on `client \in fenced` too; that is     *)
(*      deliberately NOT modelled, because a fenced holder writing its     *)
(*      own not-yet-freed blocks is EXPECTED under §8's                    *)
(*      quarantine-not-free GC and absorbed by it — firing the ghost       *)
(*      there would make LostFence's counterexample a technicality        *)
(*      instead of a corruption.  The ghost fires exactly when bytes       *)
(*      land (or are read) across an ownership generation.                 *)
(*   4. A read/write is ATOMIC w.r.t. fencing — nothing here says what     *)
(*      happens to I/O already inside the target when the preempt lands;   *)
(*      that is NVMe-level ordering, checked on hardware, never here.      *)
(*   5. Client crash is not distinguished from client silence: a client    *)
(*      that never returns IS the unresponsive holder the fence path       *)
(*      exists for.  ClientCrash as a separate action arrives with lease   *)
(*      machinery in a later tranche.                                      *)
(*   6. Byte durability at an extent is an axiom WITH NO LICENSE — block-  *)
(*      class volumes are single-replica lvols and no formal durability    *)
(*      story exists until server-side replication lands (§12).  Citing    *)
(*      a green here as a durability claim would be the axiom laundering   *)
(*      itself.                                                            *)
(*                                                                         *)
(* TRANCHE 2 (2026-08-09, same day): LAYOUTCOMMIT, size coupling, and the  *)
(* provisioning scrub — behind CommitEnabled = FALSE so every tranche-1    *)
(* state space stays bit-identical (the MaintEnabled pattern; verified by  *)
(* distinct-count match on the flagship).  THREE MORE SPEC CORRECTIONS,    *)
(* each the same species as tranche 1's — the doc's sketch named a harm    *)
(* the mechanism cannot produce, or a predicate legal behaviour violates:  *)
(*                                                                         *)
(*   a. Inv_SizeCommitCoupled is NOT "no provisional extent within fsize"  *)
(*      — hole-filling writes allocate INVALID extents inside fsize        *)
(*      legitimately (and read as zeros either way, which is what a hole   *)
(*      reads as).  The honest invariant is TRANSACTIONAL: a commit's      *)
(*      size-advance applies only WITH its range promotion.  The zeroSized *)
(*      ghost fires when the size half lands while the range half was      *)
(*      refused — which is precisely the half-stub shape the current       *)
(*      operations/mod.rs LAYOUTCOMMIT stub would have (F67's silent-zeros *)
(*      lineage: size says data, extents say INVALID).                     *)
(*   b. ForgedCommit (CommitChecksGen = FALSE) cannot violate              *)
(*      Inv_NoStaleExtentWrite as the doc's matrix claimed: a commit       *)
(*      writes no bytes — it corrupts BOOKKEEPING, promoting extents the   *)
(*      committer no longer owns (the new owner's uncommitted state served *)
(*      as committed data).  It gets its own theorem, Inv_NoForgedCommit.  *)
(*   c. ProvisionalInvisible is the SCRUB-AT-ALLOCATION belt.  All reuse   *)
(*      is intra-volume (the allocator is per-volume, §8), so the          *)
(*      disclosure is not cross-tenant — it is deleted-data resurrection:  *)
(*      a fresh INVALID extent on a reused range carries the previous      *)
(*      incarnation's bytes, violating the new-extent-reads-zeros          *)
(*      contract (a deleted secrets file resurfacing inside a new file).   *)
(*      A same-incarnation orphan handoff (returned-uncommitted extent     *)
(*      re-granted to another client of the same live file) is DELIBERATE- *)
(*      LY not a disclosure: both clients hold the file RW.  The dirt      *)
(*      tracking (everWritten/priorBytes) is pinned all-FALSE whenever the *)
(*      belt is on, so belted state spaces never pay for it.               *)
(*                                                                         *)
(* OWED (later tranches): MdsCrash/MdsRestart + durable (PersistGrants,    *)
(* RecoverConservative); the MDS fallback lane (FallbackChecksGrants — the *)
(* MDS as data-path actor); Split/Merge + SplitKeepsDisjoint +             *)
(* Inv_NoPhysicalAliasing; PublishRecheck / RecallBlocksGrant with         *)
(* LIVENESS teeth; quarantine as distinct state.  The check-tla.sh matrix  *)
(* grows with each; the header count moves with it, as a named             *)
(* deliverable.                                                            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Clients,             \* block-layout clients (SPDK or kernel initiators)
  NBlocks,             \* physical domain: blocks are 1..NBlocks
  MaxGrants,           \* LAYOUTGET budget (GrantCheck starts)
  MaxReclaims,         \* reclaim / direct-free budget
  MaxTgtRestarts,      \* spdk-tgt restart budget
  GrantsExclusive,     \* TRUE = the grant transaction polices disjointness
  RecallBeforeReuse,   \* TRUE = frees only through the reclaim machinery
  FreeRevalidates,     \* TRUE = the free transaction re-validates holders
  QuarantineEnabled,   \* TRUE = the reclaim COMPLETES over an
                       \*   unconfirmed fence, parking those blocks
                       \*   (the code's extent_quarantine) instead of
                       \*   refusing to finish — and a later
                       \*   ReleaseQuarantine may free them once the
                       \*   exclusion IS confirmed.  FALSE reproduces
                       \*   the pre-2026-08-12 abstraction, in which a
                       \*   quarantined block was simply never freed.
  QuarantineChecksDelivered, \* the release re-applies the delivered
                       \*   belt.  Its A/B is the whole point: a release
                       \*   that skips it frees a range whose holder the
                       \*   target never excluded.
  QuarantineIsolated,  \* TRUE = a PARKED range lives in its own home and
                       \*   the extent-table operations cannot see it at
                       \*   all.  This is ONE code fact, not a policy:
                       \*   `reclaim_complete`'s quarantine branch DELETEs
                       \*   the `extents` row and INSERTs the range into
                       \*   `extent_quarantine` — a third table, whose
                       \*   physical disjointness from the other two
                       \*   `verify_volume_invariants` enforces on every
                       \*   write.  So the grant transaction cannot mint
                       \*   it (it allocates from `extent_free` or the
                       \*   arena watermark, never from quarantine) and
                       \*   cannot re-grant it either (re-grant walks
                       \*   `extents`, where the row no longer is);
                       \*   `merge_extents_window` and `commit_extents`
                       \*   walk `extents` too and miss it for the same
                       \*   reason.  FALSE is the plausible refactor —
                       \*   keep the extent row and flag it — under which
                       \*   a parked range looks exactly like an ORPHAN
                       \*   (allocated, no live holder) and is re-grantable
                       \*   at its old generation.  Its A/B must find the
                       \*   corruption; see FlintExtentsQuarantineVisible.
  FenceReaches,        \* TRUE = every fence lands as a target exclusion
  FreeRequiresDelivered, \* TRUE = the free additionally requires every
                       \*   FENCED holder's exclusion CONFIRMED at the
                       \*   target (in resv) — the code's delivered bit;
                       \*   an unconfirmed fence quarantines instead
  PersistReservations, \* TRUE = PTPL: exclusions survive TgtRestart
  CommitEnabled,       \* tranche-2 master switch — FALSE keeps tranche-1
                       \*   state spaces bit-identical (MaintEnabled pattern)
  ProvisionalInvisible,\* TRUE = fresh provisional extents are scrubbed at
                       \*   allocation: the prior incarnation's bytes are
                       \*   unobservable
  CommitGatesSize,     \* TRUE = a commit's size-advance applies only WITH
                       \*   its range promotion (one transaction)
  CommitChecksGen,     \* TRUE = LAYOUTCOMMIT validates (client,
                       \*   gen-at-grant) against the grant it was made
                       \*   under (§8)
  CommitGraceEnabled,  \* TRUE = a client that has RETURNED its layout may
                       \*   still commit the range it wrote under that
                       \*   layout, provided the generation still matches
                       \*   (graceG).  FALSE reproduces the shipped-until-
                       \*   2026-08-11 world, where LAYOUTRETURN destroyed
                       \*   the row LAYOUTCOMMIT validates against and the
                       \*   client's already-written bytes stayed forever
                       \*   provisional — the Linux client returns before
                       \*   it commits, so this was live data loss.
  MergeEnabled,        \* merge-tranche master switch (MaintEnabled
                       \*   pattern — FALSE keeps every prior state space
                       \*   bit-identical). The code's merge policy: adjacent
                       \*   same-state extent rows coalesce into one row.
                       \*   Here at block granularity a "merge" is its one
                       \*   semantic residue, GEN COARSENING — the merged
                       \*   row carries ONE generation for blocks that had
                       \*   several. The model merges ANY block pair (the
                       \*   code only physically-contiguous neighbours):
                       \*   a superset of code behaviours, so its green is
                       \*   the stronger claim. The row BUDGET is pure
                       \*   representation (row counts do not exist at
                       \*   block granularity) and is deliberately not
                       \*   modelled.
  MergeChecksHolders,  \* TRUE = merge only QUIESCENT blocks (no grant
                       \*   rows at all, fenced included). The mutation
                       \*   coarsens gen under a live grant — which is
                       \*   exactly "gen moved under a live unfenced
                       \*   grant", Inv_RecallCompletesBeforeReuse.
  MergeTakesMin        \* MUTATION (code takes MAX): the merged gen is the
                       \*   MINIMUM of the pair. Kept as the A/B for the
                       \*   gen-choice question; see the run's header for
                       \*   its verdict.

VARIABLES
  alloc,          \* [Blocks -> {"free","provisional","committed",
                  \*   "quarantined"}] — WHICH HOME the physical range is
                  \*   in, and (for the two extent states) its lifecycle.
                  \*   The code has exactly three homes and enforces their
                  \*   physical disjointness in `verify_volume_invariants`:
                  \*   `extents` (provisional/committed), `extent_free`
                  \*   (free), `extent_quarantine` (quarantined).  Rendering
                  \*   a parked range as still-provisional — which this
                  \*   module did until 2026-08-12 — makes it look like an
                  \*   ORPHAN to the grant path, and TLC duly found the
                  \*   grant.  The bug was the abstraction, again.
  gen,            \* [Blocks -> Nat] bumped on every free->provisional edge
  grants,         \* [Clients -> [live, held, g]] published layout grants;
                  \*   live is the CLIENT's view (it holds the layout until
                  \*   it returns it) — MDS-side death is `fenced`
  granting,       \* [Clients -> [open, blks]] the checked-but-unpublished
                  \*   two-step window
  reclaim,        \* [active, blks, waiting] the recall-then-free job;
                  \*   waiting is the holder SNAPSHOT taken at start
  fenced,         \* SUBSET Clients — MDS-side revocation bookkeeping
  resv,           \* SUBSET Clients — clients PREEMPTED at the target
                  \*   (EARO: their reads and writes are refused there)
  nGrants, nReclaims, nRestarts,   \* budgets
  fsize,          \* observable file size in blocks (files are prefixes;
                  \*   0 = empty; pinned 0 while ~CommitEnabled)
  everWritten,    \* [Blocks -> BOOLEAN] bytes have ever landed on the
                  \*   physical block — tracked ONLY when
                  \*   ~ProvisionalInvisible (scrubbing makes the history
                  \*   irrelevant, and pinning it FALSE keeps belted state
                  \*   spaces free of it)
  priorBytes,     \* [Blocks -> BOOLEAN] this provisional incarnation still
                  \*   carries a PREVIOUS incarnation's bytes: set at
                  \*   unscrubbed reuse, cleared by the new owner's own
                  \*   write and by the free
  \* Ghosts, single-writer each (the A2Probe standing rule):
  staleRead,      \* ClientRead crossed an ownership generation
  staleWrite,     \* ClientWrite crossed an ownership generation — THE theorem
  reuseFired,     \* GrantInsert re-cycled a previously-owned block
  fenceFired,     \* Fence executed
  tgtRestarted,   \* TgtRestart executed
  resnapshotGrew, \* ReclaimResnapshot found holders the snapshot missed
  commitFired,    \* LayoutCommit executed (probe witness)
  truncateFired,  \* TruncateStart executed (probe witness)
  zeroSized,      \* a size-advance applied while its range promotion was
                  \*   refused — the F67 shape (writer: LayoutCommit)
  forgedCommit,   \* an INVALID commit applied (writer: LayoutCommit)
  disclosedRead,  \* a rightful read of a provisional block served a
                  \*   previous incarnation's bytes (writer: ClientRead)
  deliveredFreeFired, \* the delivered-conditional free freed blocks a
                  \*   fenced+delivered holder still held (writer:
                  \*   ReclaimComplete, gated on FreeRequiresDelivered)
  mergeFired,     \* Merge executed (writer: Merge, gated on
                  \*   MergeEnabled — legacy spaces never pay)
  graceG,         \* [Clients -> [Blocks -> Nat]] the generation a client
                  \*   held for a block WHEN IT RETURNED the layout; 0 =
                  \*   none.  Deliberately NOT holdership: HeldBy and every
                  \*   free/reclaim belt keep reading `grants` alone, so a
                  \*   grace record can never block a reclaim or count as a
                  \*   conflicting holder.  It exists to answer one
                  \*   question — "which generation did this client write
                  \*   under?" — and the gen check is what keeps it safe:
                  \*   after a free+reuse the block's gen has moved and the
                  \*   stale grace record refuses on its own.
  qHold,              \* [Blocks -> SUBSET Clients] the fenced holders a
                      \*   PARKED block was quarantined WITH — the code's
                      \*   extent_quarantine.fenced_clients CSV.  {} = not
                      \*   parked.  Provenance is load-bearing: without it
                      \*   a release could free any block whose holders
                      \*   happen to be fenced, which skips the recall
                      \*   entirely — TLC found exactly that on the first
                      \*   draft of this tranche.
  quarantineReleased, \* a PARKED block (reclaim completed while its
                      \*   fence was unconfirmed) was later freed once
                      \*   the exclusion was confirmed — writer:
                      \*   ReleaseQuarantine.  Without this witness the
                      \*   shipped green could be "the release never
                      \*   fired" wearing the sweep's label.
  commitAfterReturn \* a commit was validated through graceG rather than a
                  \*   live grant (writer: LayoutCommit; probe witness)

\* The tranche-2 additions, grouped so tranche-1 actions can leave them
\* unchanged in one stroke.
sizeVars == <<fsize, commitFired, truncateFired, zeroSized, forgedCommit,
              graceG, commitAfterReturn>>

vars == <<alloc, gen, grants, granting, reclaim, fenced, resv,
          nGrants, nReclaims, nRestarts,
          fsize, everWritten, priorBytes,
          staleRead, staleWrite, reuseFired, fenceFired, tgtRestarted,
          resnapshotGrew, commitFired, truncateFired, zeroSized,
          forgedCommit, disclosedRead, deliveredFreeFired, mergeFired,
          graceG, commitAfterReturn, quarantineReleased, qHold>>

Blocks == 1..NBlocks
GenBound == MaxReclaims + 1        \* one initial bump + one per free
Ranges == (SUBSET Blocks) \ {{}}

ZeroG == [b \in Blocks |-> 0]
NoGrant == [live |-> FALSE, held |-> {}, g |-> ZeroG]
NoWindow == [open |-> FALSE, blks |-> {}]
NoReclaim == [active |-> FALSE, blks |-> {}, waiting |-> {}]

\* Live holders of a block / of any block in a range — fenced INCLUDED:
\* a fenced grant's row is dead to the MDS but its blocks are not
\* re-grantable until FREED (§8's quarantine discipline, simplified: the
\* blocks pass through the reclaim before any new owner sees them).
HeldBy(b) == {c \in Clients : grants[c].live /\ b \in grants[c].held}
LiveHolders(R) == {c \in Clients : grants[c].live /\ grants[c].held \cap R # {}}

\* "This range has NO `extents` row, so no extent-table operation can see
\* it."  True exactly of a PARKED range in the shipped world: the
\* quarantine branch deleted the row and moved the range to a third
\* table.  Every use below is an operation the code implements as a walk
\* over `extents` — grant (re-grant and allocate), merge, commit — and
\* the one flag is what makes them all miss it together, because in the
\* code they miss it for one reason.  Deliberately NOT written as a
\* lifecycle enumeration: "which home" is the stable concept, and
\* enumerating states is what went stale the day "committed" arrived.
NotAnExtent(b) == QuarantineIsolated /\ alloc[b] = "quarantined"

TypeOK ==
  /\ alloc \in [Blocks -> {"free", "provisional", "committed", "quarantined"}]
  /\ gen \in [Blocks -> 0..GenBound]
  /\ grants \in [Clients ->
       [live : BOOLEAN, held : SUBSET Blocks, g : [Blocks -> 0..GenBound]]]
  /\ granting \in [Clients -> [open : BOOLEAN, blks : SUBSET Blocks]]
  /\ reclaim \in
       [active : BOOLEAN, blks : SUBSET Blocks, waiting : SUBSET Clients]
  /\ fenced \subseteq Clients
  /\ resv \subseteq Clients
  /\ nGrants \in 0..MaxGrants
  /\ nReclaims \in 0..MaxReclaims
  /\ nRestarts \in 0..MaxTgtRestarts
  /\ fsize \in 0..NBlocks
  /\ everWritten \in [Blocks -> BOOLEAN]
  /\ priorBytes \in [Blocks -> BOOLEAN]
  /\ staleRead \in BOOLEAN /\ staleWrite \in BOOLEAN
  /\ reuseFired \in BOOLEAN /\ fenceFired \in BOOLEAN
  /\ tgtRestarted \in BOOLEAN /\ resnapshotGrew \in BOOLEAN
  /\ commitFired \in BOOLEAN /\ truncateFired \in BOOLEAN
  /\ zeroSized \in BOOLEAN /\ forgedCommit \in BOOLEAN
  /\ disclosedRead \in BOOLEAN /\ deliveredFreeFired \in BOOLEAN
  /\ mergeFired \in BOOLEAN
  /\ graceG \in [Clients -> [Blocks -> 0..GenBound]]
  /\ commitAfterReturn \in BOOLEAN
  /\ quarantineReleased \in BOOLEAN
  /\ qHold \in [Blocks -> SUBSET Clients]
  \* Structure: g is normalised to 0 outside held (state-space hygiene and
  \* a modelling-bug tripwire, not a claim about the code).
  /\ \A c \in Clients : \A b \in Blocks :
       b \notin grants[c].held => grants[c].g[b] = 0
  \* Structure: the parked STATE and its provenance are one fact written
  \* in two places — a row in `extent_quarantine` carries the range AND
  \* its `fenced_clients` CSV, and neither exists without the other.  A
  \* tripwire, not a claim: if these ever drift, the sweep is reading
  \* provenance for a range nothing parked, or missing it for one that is.
  /\ \A b \in Blocks : (alloc[b] = "quarantined") <=> (qHold[b] # {})

Init ==
  /\ alloc = [b \in Blocks |-> "free"]
  /\ gen = ZeroG
  /\ grants = [c \in Clients |-> NoGrant]
  /\ granting = [c \in Clients |-> NoWindow]
  /\ reclaim = NoReclaim
  /\ fenced = {} /\ resv = {}
  /\ nGrants = 0 /\ nReclaims = 0 /\ nRestarts = 0
  /\ fsize = 0
  /\ everWritten = [b \in Blocks |-> FALSE]
  /\ priorBytes = [b \in Blocks |-> FALSE]
  /\ staleRead = FALSE /\ staleWrite = FALSE
  /\ reuseFired = FALSE /\ fenceFired = FALSE
  /\ tgtRestarted = FALSE /\ resnapshotGrew = FALSE
  /\ commitFired = FALSE /\ truncateFired = FALSE
  /\ zeroSized = FALSE /\ forgedCommit = FALSE
  /\ graceG = [c \in Clients |-> ZeroG]
  /\ commitAfterReturn = FALSE
  /\ quarantineReleased = FALSE
  /\ qHold = [b \in Blocks |-> {}]
  /\ disclosedRead = FALSE /\ deliveredFreeFired = FALSE
  /\ mergeFired = FALSE

(***************************************************************************)
(* The MDS: grants                                                         *)
(***************************************************************************)

\* LAYOUTGET step 1: the in-memory gate read.  A range is requestable if
\* every block is free or an orphan (provisional with no live holder — a
\* returned grant's uncommitted extents, re-grantable until reclaimed).
\* No reclaim-awareness here in tranche 1: RecallBlocksGrant is deferred
\* to the liveness tranche, and safety must not depend on it (the finding).
GrantCheck(c, R) ==
  /\ nGrants < MaxGrants
  /\ ~grants[c].live
  /\ ~granting[c].open
  /\ c \notin fenced        \* no re-admission before lease recovery (owed)
  \* A PARKED range is not an orphan, however much it looks like one: it
  \* is allocated-but-unheld only because the quarantine branch swept its
  \* grant rows, and it has no `extents` row for the re-grant to find.
  \* This clause is not a new belt over the code — it is the model finally
  \* rendering the third table.  (2026-08-12: without it TLC re-grants a
  \* parked block through the orphan door and the sweep then frees it
  \* under the new owner's live grant.)
  /\ \A b \in R : ~NotAnExtent(b)
  /\ \A b \in R : alloc[b] = "free" \/ HeldBy(b) = {}
  /\ granting' = [granting EXCEPT ![c] = [open |-> TRUE, blks |-> R]]
  /\ nGrants' = nGrants + 1
  /\ UNCHANGED <<alloc, gen, grants, reclaim, fenced, resv,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, sizeVars,
                 everWritten, priorBytes, disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

\* LAYOUTGET step 2: the sqlite transaction.  With GrantsExclusive it
\* re-validates disjointness INSIDE the transaction and refuses a range
\* the check approved on a stale read — two windows racing to the same
\* free block lose exactly one of them here.  The mutation is the
\* PK-does-not-police-overlap landmine: the insert trusts the check, and
\* two live grants share a block.
\*
\* WHAT THE TRANSACTION CAN SEE — and the limit that makes FreeRevalidates
\* load-bearing: disjointness is policed over the extent/grant ROWS, and a
\* freed block has LEFT the tables.  So `occupied` is "an allocated block
\* with a live holder", never "any block some client still believes it
\* holds" — no grant-time check can protect against a free that should not
\* have happened, because the free is precisely the step that destroys the
\* evidence.  The protection for that belongs to the free side, which is
\* the tranche-1 finding.
GrantInsert(c) ==
  /\ granting[c].open
  /\ LET R == granting[c].blks
         \* An ALLOCATED block (provisional or committed — any state with
         \* extent rows in the tables) with a live holder refuses.  This
         \* read "provisional /\ held" until tranche 2 added the committed
         \* state, whereupon TLC produced two live grants overlapping a
         \* committed block in 6 states: a predicate enumerating states
         \* goes stale the day the state set grows; "not free" does not.
         occupied == \E b \in R : alloc[b] # "free" /\ HeldBy(b) # {}
         \* ...and a PARKED range refuses whatever the disjointness policy
         \* is, because this is not a policy: the transaction allocates
         \* from `extent_free` or the arena watermark and re-grants from
         \* `extents`, and a parked range is in NEITHER.  Deliberately
         \* OUTSIDE the GrantsExclusive conjunct — flipping the
         \* disjointness policy must not hand the allocator a third
         \* table it cannot read in any world.
         parked == \E b \in R : NotAnExtent(b)
     IN IF (GrantsExclusive /\ occupied) \/ parked
        THEN \* transaction refuses; the window closes, nothing published
          /\ granting' = [granting EXCEPT ![c] = NoWindow]
          /\ UNCHANGED <<alloc, gen, grants, reclaim, fenced, resv,
                         nGrants, nReclaims, nRestarts, staleRead,
                         staleWrite, reuseFired, fenceFired, tgtRestarted,
                         resnapshotGrew, sizeVars, everWritten, priorBytes,
                         disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>
        ELSE
          LET fresh == {b \in R : alloc[b] = "free"}
              gen2 == [b \in Blocks |->
                        IF b \in fresh THEN gen[b] + 1 ELSE gen[b]]
          IN
          /\ alloc' = [b \in Blocks |->
                        IF b \in fresh THEN "provisional" ELSE alloc[b]]
          /\ gen' = gen2
          /\ grants' = [grants EXCEPT ![c] =
               [live |-> TRUE, held |-> R,
                g |-> [b \in Blocks |-> IF b \in R THEN gen2[b] ELSE 0]]]
          /\ granting' = [granting EXCEPT ![c] = NoWindow]
          /\ reuseFired' = (reuseFired \/ \E b \in fresh : gen[b] > 0)
          \* The provisioning scrub, or its absence: with the belt on,
          \* fresh extents are zeroed at allocation and the previous
          \* incarnation's bytes are gone; without it, a reused range
          \* enters its new life still carrying them.
          /\ priorBytes' = IF ProvisionalInvisible THEN priorBytes
                           ELSE [b \in Blocks |->
                                  IF b \in fresh THEN everWritten[b]
                                  ELSE priorBytes[b]]
          /\ UNCHANGED <<reclaim, fenced, resv, nGrants, nReclaims,
                         nRestarts, staleRead, staleWrite, fenceFired,
                         tgtRestarted, resnapshotGrew, sizeVars,
                         everWritten, disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

(***************************************************************************)
(* The MDS: reclaim (recall-then-free)                                     *)
(***************************************************************************)

\* The belted free path opens: pick allocated blocks, SNAPSHOT the live
\* unfenced holders, recall them (recall delivery is not modelled as an
\* arm — a lost recall is indistinguishable from a slow client in a
\* safety model, and the fence is the designed backstop for both).
ReclaimStart(R) ==
  /\ RecallBeforeReuse
  /\ nReclaims < MaxReclaims
  /\ ~reclaim.active
  \* Provisional blocks are reclaimable outright (returned-uncommitted
  \* space); COMMITTED blocks only once the size no longer covers them —
  \* committed data inside the file is never GC'd out from under it, and
  \* the only way a committed block leaves the file is TruncateStart
  \* cutting the size first.
  /\ R \subseteq {b \in Blocks :
       \/ alloc[b] = "provisional"
       \/ (alloc[b] = "committed" /\ b > fsize)}
  /\ R # {}
  /\ reclaim' = [active |-> TRUE, blks |-> R,
                 waiting |-> LiveHolders(R) \ fenced]
  /\ nReclaims' = nReclaims + 1
  /\ UNCHANGED <<alloc, gen, grants, granting, fenced, resv, nGrants,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew, sizeVars, everWritten,
                 priorBytes, disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

\* The retry loop's separate re-read step (the F62 idiom): recompute the
\* holder set from the live tables.  This is what eventually surfaces a
\* grant the start-time snapshot missed — the ghost records exactly that
\* event, and its probe is the non-vacuity licence for FreeRevalidates'
\* green (the belt refuses frees precisely when this set is non-empty).
ReclaimResnapshot ==
  /\ reclaim.active
  /\ LET w2 == LiveHolders(reclaim.blks) \ fenced
     IN /\ reclaim' = [reclaim EXCEPT !.waiting = w2]
        /\ resnapshotGrew' =
             (resnapshotGrew \/ (reclaim.waiting = {} /\ w2 # {}))
  /\ UNCHANGED <<alloc, gen, grants, granting, fenced, resv, nGrants,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, sizeVars, everWritten,
                 priorBytes, disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

\* An unresponsive holder is fenced: revoked server-side (bookkeeping in
\* `fenced` — its grant row is dead to the MDS) and preempted at the
\* target.  The ∃ is the honest arm: the MDS PROCEEDS BELIEVING THE FENCE
\* LANDED EITHER WAY, and FenceReaches = FALSE lets the exclusion silently
\* not exist — RecallReaches' analog, kept FALSE in the shipped cfg until
\* spdk-tgt reservation enforcement is proven on real hardware.
Fence(c) ==
  /\ reclaim.active
  /\ c \in reclaim.waiting
  /\ fenced' = fenced \cup {c}
  /\ \E landed \in BOOLEAN :
       /\ FenceReaches => landed
       /\ resv' = IF landed THEN resv \cup {c} ELSE resv
  /\ reclaim' = [reclaim EXCEPT !.waiting = @ \ {c}]
  /\ fenceFired' = TRUE
  /\ UNCHANGED <<alloc, gen, grants, granting, nGrants, nReclaims,
                 nRestarts, staleRead, staleWrite, reuseFired,
                 tgtRestarted, resnapshotGrew, sizeVars, everWritten,
                 priorBytes, disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

\* The reclaim's per-block verdict, ONE definition so the three
\* assignments in ReclaimComplete cannot drift apart (they did while this
\* was inlined thrice).  Outside the quarantine world it is constantly
\* TRUE, which is what keeps every legacy cfg's state graph bit-identical.
ReclaimFrees(b) ==
  \/ ~(QuarantineEnabled /\ FreeRequiresDelivered)
  \/ (HeldBy(b) \cap fenced) \subseteq resv

\* The free.  Every snapshotted holder has returned or been fenced; with
\* FreeRevalidates the transaction ALSO re-validates the live tables and
\* refuses while any live unfenced grant covers a block — the tranche-1
\* finding: without this, a grant the snapshot missed is freed under
\* (FlintExtentsStaleSnapshotFree.cfg), and no grant-side belt closes it.
ReclaimComplete ==
  /\ reclaim.active
  /\ reclaim.waiting = {}
  /\ FreeRevalidates =>
       \A b \in reclaim.blks : HeldBy(b) \subseteq fenced
  \* The DELIVERED belt (the 2026-08-10 graduation): a fenced holder's
  \* blocks free only when its exclusion was CONFIRMED at the target
  \* (`resv` — the code's delivered bit, set on a verified preempt).  An
  \* UNDELIVERED fence refuses the free — in code that extent
  \* quarantines, which this model renders as "never freed".  Orthogonal
  \* to FreeRevalidates on purpose: each mutation stays single-flag
  \* attributable.  This is what lets the shipped cfg claim the stale
  \* theorems WITHOUT FenceReaches — the fence may fail to land, and the
  \* free machinery still never reuses a block out from under a client
  \* the target has not actually excluded.
  \* WITHOUT QuarantineEnabled this is the pre-2026-08-12 rendering: the
  \* free simply cannot happen while any fenced holder is unconfirmed, so
  \* the reclaim never completes and the block is never freed.  That was
  \* always an ABSTRACTION of the code, not a description of it — the code
  \* completes the reclaim and PARKS the offending ranges in
  \* extent_quarantine.  Kept as the default so every legacy cfg's state
  \* graph stays bit-identical.
  /\ (FreeRequiresDelivered /\ ~QuarantineEnabled) =>
       \A b \in reclaim.blks : (HeldBy(b) \cap fenced) \subseteq resv
  \* WITH it, the reclaim completes either way: the blocks whose fenced
  \* holders are ALL confirmed-excluded go to the free list, and the rest
  \* LEAVE THE EXTENT TABLE for the quarantine — which is the code, step
  \* for step (DELETE FROM extents, INSERT INTO extent_quarantine, DELETE
  \* the grant rows).  Parking as a distinct home rather than "stays
  \* allocated" is the 2026-08-12 correction: an allocated-but-unheld
  \* block is an ORPHAN, and orphans are re-grantable.
  /\ alloc' = [b \in Blocks |->
                IF b \in reclaim.blks
                  THEN (IF ReclaimFrees(b) THEN "free" ELSE "quarantined")
                  ELSE alloc[b]]
  /\ reclaim' = NoReclaim
  \* Canonicalise: a freed block's dirt flag is recomputed at its next
  \* reuse from everWritten; holding it at FALSE meanwhile keeps free
  \* blocks from splitting states on a value nothing reads.  A PARKED
  \* block keeps its dirt — it is not reusable yet, and the sweep clears
  \* the flag at the moment it becomes so.
  /\ priorBytes' = [b \in Blocks |->
                     IF b \in reclaim.blks /\ ReclaimFrees(b)
                       THEN FALSE ELSE priorBytes[b]]
  \* Provenance for the parked blocks: WHO was holding them, fenced but
  \* not confirmed-excluded, at the moment the reclaim gave up on them.
  \* This is extent_quarantine.fenced_clients, and it is what the sweep
  \* re-checks later — not "whoever happens to hold the block now".  A
  \* block that FREES here is un-parked in the same stroke: the two homes
  \* are disjoint by construction in the code (verify_volume_invariants
  \* enforces it), and letting a stale qHold survive a free lets the sweep
  \* free the block AGAIN, under its next owner's live grant — TLC found
  \* that too.
  /\ qHold' = [b \in Blocks |->
                IF b \in reclaim.blks
                  THEN (IF ReclaimFrees(b) THEN {} ELSE HeldBy(b) \cap fenced)
                  ELSE qHold[b]]
  \* Probe witness: the delivered-conditional free actually FREED blocks
  \* a fenced+delivered holder still held — the exact event the code
  \* flip (quarantine -> clean free) exists to produce.  Gated on the
  \* arm so unbelted state spaces never pay for it.  (Parenthesized per
  \* the tranche-1 precedence trap.)
  /\ deliveredFreeFired' =
       (deliveredFreeFired \/
         (FreeRequiresDelivered /\
           (\E b \in reclaim.blks : (HeldBy(b) \cap fenced \cap resv) # {})))
  /\ UNCHANGED <<gen, grants, granting, fenced, resv, nGrants, nReclaims,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew, sizeVars, everWritten,
                 disclosedRead, mergeFired, quarantineReleased>>

\* THE DELIVERY RETRY (2026-08-12).  The reconcile pass re-runs
\* `fence_preempt` for every (volume, client) in fenced_clients and marks
\* the fence DELIVERED when it confirms — so a fence that failed to land
\* at fence time (tgt unreachable) can become confirmed later.  Modelled
\* because WITHOUT IT THE SWEEP IS UNREACHABLE: `Fence` only ever fires
\* once per client (the waiting set excludes the already-fenced), so an
\* unlanded exclusion could never become landed and every parked block
\* would stay parked forever.  The probe caught exactly that — the first
\* draft's shipped green was vacuous with respect to the sweep.
FenceRetry(c) ==
  /\ QuarantineEnabled
  /\ c \in fenced
  /\ c \notin resv
  /\ \E landed \in BOOLEAN :
       /\ FenceReaches => landed
       /\ resv' = IF landed THEN resv \cup {c} ELSE resv
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, nGrants,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, sizeVars,
                 everWritten, priorBytes, disclosedRead, deliveredFreeFired,
                 mergeFired, quarantineReleased, qHold>>

\* THE QUARANTINE SWEEP (2026-08-12).  A block the reclaim PARKED —
\* completed the job, but this range's fence was not confirmed at the
\* target, so freeing it would have been FlintExtentsLostFence's
\* corruption — becomes free once every remembered holder IS confirmed
\* excluded.  The delivery retry that makes that reachable already ships
\* (the reconcile pass re-runs the preempt for every fenced pair and
\* marks it delivered on success); what did not exist is anything that
\* revisits the parked range afterwards, so the capacity leaked forever.
\*
\* The predicate is the SAME one ReclaimComplete applies — re-applied
\* later, which is exactly what the module could not express before and
\* therefore had never checked.  Its A/B (QuarantineChecksDelivered =
\* FALSE) must find the corruption: releasing a range whose holder the
\* target never excluded is the un-fenced client writing into the next
\* owner's bytes.
\*
\* IT CHECKS qHold, NOT HeldBy — provenance, not circumstance.  Gating on
\* "every CURRENT holder is fenced-and-confirmed" frees blocks that were
\* never quarantined at all, skipping the recall entirely; TLC found that
\* on the first draft of this tranche.  The sweep must act on what was
\* PARKED (the code's extent_quarantine.fenced_clients CSV), which is
\* also why an UNFENCED client cannot rescue a range: in code its
\* fenced_clients row is gone, so the delivered join yields nothing and
\* the range stays parked — conservatively, forever, until the operator
\* lever.
\* DELIBERATELY NOT gated on ~reclaim.active.  The sweep runs in the
\* reconcile task and a reclaim is driven from the dispatcher / lease
\* sweep; nothing serialises them but sqlite's per-transaction write
\* lock, so a model that disabled the sweep mid-reclaim would be
\* checking a tidier server than the one that ships.  It is safe for a
\* reason the model can state: a parked range has no extent row, so
\* ReclaimStart can never have selected it, and the two act on disjoint
\* blocks.  Stated, not assumed — the interleaving is in the graph.
ReleaseQuarantine(b) ==
  /\ QuarantineEnabled
  /\ alloc[b] = "quarantined"         \* PARKED — a row in the third table
  /\ QuarantineChecksDelivered => qHold[b] \subseteq resv
  /\ alloc' = [alloc EXCEPT ![b] = "free"]
  /\ qHold' = [qHold EXCEPT ![b] = {}]
  /\ priorBytes' = [priorBytes EXCEPT ![b] = FALSE]
  /\ quarantineReleased' = TRUE
  /\ UNCHANGED <<gen, grants, granting, reclaim, fenced, resv, nGrants,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, sizeVars,
                 everWritten, disclosedRead, deliveredFreeFired,
                 mergeFired, graceG, commitAfterReturn>>

\* The merge policy's one block-level residue (see MergeEnabled's
\* header): two same-state allocated blocks coarsen to ONE generation —
\* the code's row-coalesce carries the MAX of its constituents' gens.
\* Gated on unequal gens: an equal-gen merge changes nothing a block can
\* see (pure row bookkeeping), so representing it would only pad the
\* state space with ghost flips.  The quiescence guard is the belt under
\* test: coarsening under a LIVE grant moves gen under it, which is
\* Inv_RecallCompletesBeforeReuse's exact clause
\* (FlintExtentsMergeHeld.cfg).  The MIN-vs-MAX choice gets its own A/B
\* (FlintExtentsMergeMin.cfg) — see that cfg's header for the verdict.
Merge(b1, b2) ==
  /\ MergeEnabled
  /\ b1 # b2
  /\ alloc[b1] # "free"
  \* ...and never over a PARKED range: `merge_extents_window` walks
  \* `extents`, which a quarantined range has left.  Same one code fact
  \* as the grant path's refusal, hence the same flag.
  /\ ~NotAnExtent(b1)
  /\ alloc[b1] = alloc[b2]
  /\ gen[b1] # gen[b2]
  /\ MergeChecksHolders => (HeldBy(b1) = {} /\ HeldBy(b2) = {})
  /\ LET g == IF MergeTakesMin
                THEN (IF gen[b1] < gen[b2] THEN gen[b1] ELSE gen[b2])
                ELSE (IF gen[b1] > gen[b2] THEN gen[b1] ELSE gen[b2])
     IN gen' = [gen EXCEPT ![b1] = g, ![b2] = g]
  /\ mergeFired' = TRUE
  /\ UNCHANGED <<alloc, grants, granting, reclaim, fenced, resv, nGrants,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, sizeVars,
                 everWritten, priorBytes, disclosedRead,
                 deliveredFreeFired, quarantineReleased, qHold>>

\* The unbelted world: RecallBeforeReuse = FALSE frees allocated blocks
\* outright, holders or no holders — the F65-of-extents.
FreeDirect(R) ==
  /\ ~RecallBeforeReuse
  /\ nReclaims < MaxReclaims
  /\ R \subseteq {b \in Blocks : alloc[b] = "provisional"}
  /\ R # {}
  /\ alloc' = [b \in Blocks |-> IF b \in R THEN "free" ELSE alloc[b]]
  \* Same disjointness as ReclaimComplete: a freed block is not parked.
  /\ qHold' = [b \in Blocks |-> IF b \in R THEN {} ELSE qHold[b]]
  /\ nReclaims' = nReclaims + 1
  /\ UNCHANGED <<gen, grants, granting, reclaim, fenced, resv, nGrants,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew, sizeVars, everWritten,
                 priorBytes, disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased>>

(***************************************************************************)
(* The MDS: commit and size (tranche 2, CommitEnabled)                     *)
(***************************************************************************)

\* LAYOUTCOMMIT, carrying newsize.  ONE implementation transaction commits
\* the range promotion AND the size advance; the two arms split exactly
\* the ways that transaction can be miswritten.  `valid` is §8's check:
\* the (client, gen-at-grant) pair must match a live unfenced grant row.
\* A fenced client REACHES this action freely — reservations fence the
\* NVMe data path only, the NFS control path stays open — which is the
\* entire reason CommitChecksGen exists.
\*
\*   valid, any arms          -> both halves apply (the one honest commit)
\*   invalid, both belts      -> the whole transaction refuses (disabled)
\*   invalid, ~CommitGatesSize-> the HALF-STUB: the range half refuses but
\*                               the size half lands — fsize now claims
\*                               data the extents say is INVALID, F67's
\*                               silent-zeros shape (ghost: zeroSized)
\*   invalid, ~CommitChecksGen-> the FORGED COMMIT: both halves apply for
\*                               a committer who no longer owns the range
\*                               (ghost: forgedCommit)
LayoutCommit(c, R, n) ==
  /\ CommitEnabled
  /\ R # {} /\ n >= fsize /\ n <= NBlocks
  \* Reachable two ways: under a LIVE grant, or — when CommitGraceEnabled
  \* — through graceG, the generation memory a LayoutReturn left behind.
  \* The second door is what a real Linux client walks through: it
  \* LAYOUTRETURNs and then LAYOUTCOMMITs, and with the door shut its
  \* written bytes stay provisional forever.
  /\ \A b \in R : \/ (grants[c].live /\ b \in grants[c].held)
                  \/ (CommitGraceEnabled /\ graceG[c][b] # 0)
  \* A PARKED range has no extent row to promote, so `commit_extents`
  \* refuses it (CommitRejected) before any of the belts below get a
  \* say — including the forged-commit arm, which is why this sits above
  \* them rather than inside `valid`.  Reachable without it: a client
  \* that returned its layout keeps a grace record, a LATER holder is
  \* fenced and the range parks, and the returner's commit then promotes
  \* a quarantined range back into the file.
  /\ \A b \in R : ~NotAnExtent(b)
  /\ LET liveHold(b) == grants[c].live /\ b \in grants[c].held
         genOf(b)    == IF liveHold(b) THEN grants[c].g[b] ELSE graceG[c][b]
         viaGrace    == \E b \in R : ~liveHold(b)
         \* THE BELT, unchanged in substance and now shared by both doors:
         \* the generation the client wrote under must still be the
         \* block's.  After a free+reuse the gen has moved, so a stale
         \* grace record refuses exactly like a stale live grant would —
         \* which is why grace can be forgotten lazily without becoming
         \* a correctness dependency.
         \* NOTE the shape: `valid` does NOT consult CommitChecksGen —
         \* the flag branches BELOW, which is what makes the
         \* forged-commit mutation reachable (folding it in here would
         \* route CommitChecksGen=FALSE into the honest arm and quietly
         \* disarm that mutation).
         valid == c \notin fenced /\ \A b \in R : genOf(b) = gen[b]
     IN IF valid
        THEN /\ alloc' = [b \in Blocks |->
                           IF b \in R THEN "committed" ELSE alloc[b]]
             /\ fsize' = n
             /\ commitFired' = TRUE
             /\ commitAfterReturn' = (commitAfterReturn \/ viaGrace)
             /\ UNCHANGED <<zeroSized, forgedCommit, quarantineReleased, qHold>>
        ELSE IF CommitChecksGen /\ CommitGatesSize
        THEN FALSE          \* refused whole: a disabled action
        ELSE IF CommitChecksGen
        THEN \* ~CommitGatesSize: the half-stub world
             /\ fsize' = n
             /\ zeroSized' = (zeroSized \/ n > fsize)
             /\ commitFired' = TRUE
             /\ commitAfterReturn' = (commitAfterReturn \/ viaGrace)
             /\ UNCHANGED <<alloc, forgedCommit, quarantineReleased, qHold>>
        ELSE \* ~CommitChecksGen: the forged commit applies whole
             /\ alloc' = [b \in Blocks |->
                           IF b \in R THEN "committed" ELSE alloc[b]]
             /\ fsize' = n
             /\ forgedCommit' = TRUE
             /\ commitFired' = TRUE
             /\ commitAfterReturn' = (commitAfterReturn \/ viaGrace)
             /\ UNCHANGED zeroSized
  /\ UNCHANGED <<gen, grants, granting, reclaim, fenced, resv, nGrants,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, truncateFired,
                 everWritten, priorBytes, disclosedRead, deliveredFreeFired,
                 mergeFired, graceG, quarantineReleased, qHold>>

\* SETATTR-shrink: the size cut is metadata-only and immediate; the blocks
\* beyond the new size stay allocated until the reclaim machinery frees
\* them through the belted path.  FlintTruncate remains the authority on
\* the file-layout truncate GATE — deliberately not re-modelled here; what
\* this action supplies is the only legal route by which a committed block
\* leaves the file (size first, then recall-then-free).
TruncateStart(n) ==
  /\ CommitEnabled
  /\ n < fsize
  /\ fsize' = n
  /\ truncateFired' = TRUE
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, resv,
                 nGrants, nReclaims, nRestarts, staleRead, staleWrite,
                 reuseFired, fenceFired, tgtRestarted, resnapshotGrew,
                 commitFired, zeroSized, forgedCommit, everWritten,
                 priorBytes, disclosedRead, deliveredFreeFired, mergeFired,
                 graceG, commitAfterReturn, quarantineReleased, qHold>>

(***************************************************************************)
(* The target                                                              *)
(***************************************************************************)

\* spdk-tgt restarts.  Without PTPL the reservation and every registration
\* die with the process: all exclusions are forgotten and every client —
\* including every fenced one — can reach the namespace again.
TgtRestart ==
  /\ nRestarts < MaxTgtRestarts
  /\ resv' = IF PersistReservations THEN resv ELSE {}
  /\ tgtRestarted' = TRUE
  /\ nRestarts' = nRestarts + 1
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, nGrants,
                 nReclaims, staleRead, staleWrite, reuseFired, fenceFired,
                 resnapshotGrew, sizeVars, everWritten, priorBytes,
                 disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

(***************************************************************************)
(* The clients — raw NVMe I/O under a held layout; the MDS is not on this  *)
(* path at all, which is the entire reason this module exists.             *)
(***************************************************************************)

ClientRead(c, b) ==
  /\ grants[c].live
  /\ b \in grants[c].held
  /\ c \notin resv                      \* EARO refuses a preempted host
  /\ staleRead' = (staleRead \/ gen[b] # grants[c].g[b])
  \* The RIGHTFUL owner reading its own fresh provisional extent, which —
  \* unscrubbed — still carries a previous incarnation's bytes: the
  \* deleted-data-resurrection read. Distinct from staleRead (that is the
  \* OLD owner reaching into the new world); requiring gen-match keeps the
  \* two ghosts attributable.
  /\ disclosedRead' = (disclosedRead \/
       (gen[b] = grants[c].g[b] /\ alloc[b] = "provisional"
        /\ priorBytes[b]))
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, resv,
                 nGrants, nReclaims, nRestarts, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, sizeVars,
                 everWritten, priorBytes, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

ClientWrite(c, b) ==
  /\ grants[c].live
  /\ b \in grants[c].held
  /\ c \notin resv
  /\ staleWrite' = (staleWrite \/ gen[b] # grants[c].g[b])
  \* Dirt tracking, active only when the scrub belt is off (with it on,
  \* history is irrelevant and the flags stay pinned FALSE — belted state
  \* spaces never pay for them). A rightful write overwrites whatever the
  \* previous incarnation left, so it clears the block's priorBytes.
  /\ everWritten' = IF ProvisionalInvisible THEN everWritten
                    ELSE [everWritten EXCEPT ![b] = TRUE]
  /\ priorBytes' = IF gen[b] = grants[c].g[b]
                     THEN [priorBytes EXCEPT ![b] = FALSE]
                     ELSE priorBytes
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, resv,
                 nGrants, nReclaims, nRestarts, staleRead, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew, sizeVars,
                 disclosedRead, deliveredFreeFired, mergeFired, quarantineReleased, qHold>>

LayoutReturn(c) ==
  /\ grants[c].live
  /\ grants' = [grants EXCEPT ![c] = NoGrant]
  /\ reclaim' = [reclaim EXCEPT !.waiting = @ \ {c}]
  \* The return destroys the grant, which is right — a returned client is
  \* not a holder and must not block a reclaim.  What it must NOT destroy
  \* is the memory of WHICH GENERATION the client wrote under: the Linux
  \* client returns before it commits, and without this the bytes it
  \* already wrote stay provisional forever (live data loss, rig-found).
  \* graceG is never consulted by any holder/free/conflict predicate.
  \* Gated on the tranche switch (the MaintEnabled pattern): with
  \* CommitEnabled = FALSE there is no LayoutCommit to serve, so the
  \* memory is dead weight and pinning it keeps every tranche-1 state
  \* space BIT-IDENTICAL — the property this module's tranches are
  \* required to preserve, and the reason the flagship's distinct count
  \* is quotable across them.
  /\ graceG' = IF CommitEnabled /\ CommitGraceEnabled
               THEN [graceG EXCEPT ![c] =
                       [b \in Blocks |-> IF b \in grants[c].held
                                         THEN grants[c].g[b]
                                         ELSE graceG[c][b]]]
               ELSE graceG
  /\ UNCHANGED <<alloc, gen, granting, fenced, resv, nGrants, nReclaims,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew, everWritten,
                 priorBytes, disclosedRead, deliveredFreeFired, mergeFired,
                 fsize, commitFired, truncateFired, zeroSized, forgedCommit,
                 commitAfterReturn, quarantineReleased, qHold>>

Next ==
  \/ \E c \in Clients, R \in Ranges : GrantCheck(c, R)
  \/ \E c \in Clients : GrantInsert(c)
  \/ \E R \in Ranges : ReclaimStart(R)
  \/ ReclaimResnapshot
  \/ \E c \in Clients : Fence(c)
  \/ ReclaimComplete
  \/ \E c \in Clients : FenceRetry(c)
  \/ \E b \in Blocks : ReleaseQuarantine(b)
  \/ \E b1, b2 \in Blocks : Merge(b1, b2)
  \/ \E R \in Ranges : FreeDirect(R)
  \/ TgtRestart
  \/ \E c \in Clients, b \in Blocks : ClientRead(c, b)
  \/ \E c \in Clients, b \in Blocks : ClientWrite(c, b)
  \/ \E c \in Clients : LayoutReturn(c)
  \/ \E c \in Clients, R \in Ranges, n \in 0..NBlocks : LayoutCommit(c, R, n)
  \/ \E n \in 0..NBlocks : TruncateStart(n)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Invariants                                                              *)
(***************************************************************************)

\* No two live unfenced grants share a physical block.  Fenced-vs-live
\* overlap is EXPECTED (fence-then-reuse is the design working); the
\* invariant scopes to grants the MDS still considers live.
Inv_NoConflictingGrants ==
  \A c1, c2 \in Clients :
    (/\ c1 # c2
     /\ grants[c1].live /\ c1 \notin fenced
     /\ grants[c2].live /\ c2 \notin fenced)
      => grants[c1].held \cap grants[c2].held = {}

\* No block moves through the allocator's lifecycle while a live unfenced
\* grant covers it: its generation never moves under the grant, and it
\* never LEAVES THE EXTENT TABLE under the grant — for the free list
\* (freed-under-grant is already the bug even before a new owner appears)
\* or, since 2026-08-12, for the quarantine.  Parking is a lifecycle move
\* like any other: a range whose extent row is gone is one the sweep may
\* hand to the allocator at any moment, so a live grant over it is the
\* same bug one step earlier.  Vacuous wherever QuarantineEnabled is
\* FALSE (nothing is ever parked), which is every cfg but three.
Inv_RecallCompletesBeforeReuse ==
  \A c \in Clients :
    (grants[c].live /\ c \notin fenced) =>
      \A b \in grants[c].held :
        /\ gen[b] = grants[c].g[b]
        /\ alloc[b] \notin {"free", "quarantined"}

(***************************************************************************)
(* THE THEOREMS — descendants of Inv_NoStaleServe, and the WRITE is        *)
(* first-class: a stale write corrupts the NEW owner's bytes, which is     *)
(* strictly worse than a stale read.  GRADUATED 2026-08-10: the shipped    *)
(* cfg now claims BOTH, licensed not by FenceReaches (still FALSE there —  *)
(* the code's preempt arm can fail at runtime) but by                      *)
(* FreeRequiresDelivered: the free trusts only target-CONFIRMED            *)
(* exclusions, whose realness the fence rig proved on real hardware        *)
(* (device counters froze under a live raw-path writer;                    *)
(* make test-pnfs-fence-rig and its restart/ptpl/unfence descendants).     *)
(* FlintExtentsLostFence.cfg is the permanent single-flag A/B pinning the  *)
(* hole; FlintExtentsTarget.cfg remains the FenceReaches ideal world.      *)
(***************************************************************************)
Inv_NoStaleExtentWrite == ~staleWrite
Inv_NoStaleExtentRead == ~staleRead

(***************************************************************************)
(* Tranche-2 theorems.  SizeCommitCoupled is TRANSACTIONAL, not the        *)
(* doc-sketched state predicate ("no provisional extent within fsize") —   *)
(* hole-filling writes make that one false on legal behaviour.  What must  *)
(* hold is that no size-advance ever applies without its range promotion:  *)
(* F67's silent-zeros shape is exactly a size that claims data the extent  *)
(* map does not back.                                                      *)
(***************************************************************************)
Inv_SizeCommitCoupled == ~zeroSized
Inv_NoForgedCommit == ~forgedCommit
Inv_NoPriorOwnerDisclosure == ~disclosedRead

Inv == TypeOK /\ Inv_NoConflictingGrants /\ Inv_RecallCompletesBeforeReuse
InvCommit == Inv /\ Inv_SizeCommitCoupled /\ Inv_NoForgedCommit
                 /\ Inv_NoPriorOwnerDisclosure
InvTarget == InvCommit /\ Inv_NoStaleExtentWrite /\ Inv_NoStaleExtentRead

================================================================================
