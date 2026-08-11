--------------------------- MODULE FlintAdmission ---------------------------
(***************************************************************************)
(* The block-layout ADMISSION layer — the same-node-zombie tranche the     *)
(* lease sweep's §9 note owed (pnfs-block-layout-design.md; memory item    *)
(* 19's "admission model tranche").                                        *)
(*                                                                         *)
(* THE RESIDUAL, precisely: after the sweep's fence → revoke →             *)
(* auto-unfence, the standing zombie barrier is the durable HOST eviction  *)
(* — per-Host-NQN, RFC 8154's own granularity.  A successor pod on a       *)
(* DIFFERENT node re-admits a different NQN and the dead node's transport  *)
(* stays refused.  A successor on the SAME node re-admits the very NQN the *)
(* eviction removed, and the reservation is already released — so a        *)
(* process on that node still holding a pre-revocation extent mapping      *)
(* could write bytes the successor now owns.                               *)
(*                                                                         *)
(* WHAT CARRIES SAFETY in the shipped world: the client KERNEL honours     *)
(* lease expiry — fs/nfs discards layout state when the lease dies, and    *)
(* every userspace write goes THROUGH the kernel's blocklayout driver, so  *)
(* a discarded layout means no raw write, no matter how wedged userspace   *)
(* is.  `ClientHonorsLease` is that assumption as a constant:              *)
(*   TRUE  (shipped cfg)      — Inv_NoStaleDeviceWrite HOLDS with the      *)
(*                              same-host door wide open.                  *)
(*   FALSE + same-host        — TLC MUST find the stale write: the         *)
(*                              residual, machine-checked instead of a     *)
(*                              prose apology (FlintAdmissionZombie.cfg).  *)
(*   FALSE + cross-host       — HOLDS anyway: the eviction barrier is      *)
(*                              real at per-host granularity               *)
(*                              (FlintAdmissionCrossHost.cfg).             *)
(* ¬ClientHonorsLease is not hypothetical: a VM frozen (live-migrated,     *)
(* SIGSTOP'd, clock-stalled) past its own lease timer resumes believing    *)
(* its layouts are live.                                                   *)
(*                                                                         *)
(* SCOPE, stated rather than smuggled:                                     *)
(*  - Only the SWEEP fence path (expiry-gated) is modelled.  The manual    *)
(*    FenceBlockClient/UnfenceBlockClient levers carry an explicit         *)
(*    operator contract ("unfence after the client is verified recovered   *)
(*    or gone" — the RPC doc); modelling them green would launder that     *)
(*    contract into a theorem.                                             *)
(*  - Zombie SELF-re-admission is excluded: re-admission needs a fresh     *)
(*    EXCHANGE_ID (the old clientid is expired at top-of-COMPOUND), and    *)
(*    minting a fresh incarnation discards the old kernel state — a        *)
(*    client cannot be both frozen-holding-mappings and alive-enough to    *)
(*    re-handshake.  The modelled door is the SUCCESSOR's re-admission.    *)
(*  - Host eviction is coarsened to always-evict (the code keeps a shared  *)
(*    NQN while another live client's row holds it); harsher eviction      *)
(*    cannot manufacture a stale write, so the coarsening is safe for      *)
(*    this invariant.                                                      *)
(*                                                                         *)
(* One volume, one extent-slot; each grant is a fresh incarnation (gen     *)
(* bump — the allocator's fresh_only shape).  The device-write guard is    *)
(* the code's real gate stack: transport (host on the allow-list) AND      *)
(* no EA-RO reservation (a non-registrant write under 4h is refused —     *)
(* rig-proven).                                                            *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
  Zombie, Successor,      \* model values: the dying client and its successor
  H1, H2,                 \* model values: hosts (nodes / NQNs)
  NoOwner,                \* model value: the extent slot is unowned
  ClientHonorsLease,      \* BOOLEAN: kernel discards layouts at lease expiry
  SameHostReadmit,        \* BOOLEAN: successor lands on the zombie's node
  GenMax                  \* incarnation bound (3 suffices: z's, s's, headroom)

Clients == {Zombie, Successor}

HostOf == [c \in Clients |->
             IF c = Zombie THEN H1
             ELSE IF SameHostReadmit THEN H1 ELSE H2]

VARIABLES
  admitted,     \* subset of hosts: the export allow-list (block_hosts ∪ attach)
  fencedC,      \* subset of clients: durable fenced_clients records
  resvHeld,     \* the volume-wide EA-RO reservation
  expired,      \* subset of clients: lease dead (the sweep's aliveness verdict)
  mappings,     \* {<<client, gen>>}: extent mappings a client kernel holds
  curGen,       \* current incarnation of the extent slot
  owner,        \* client owning curGen, or NoOwner
  staleWrote,   \* ghost: a stale mapping's bytes reached the device
  sweepFired,   \* ghost: the sweep chain really ran (probe)
  readmitFired  \* ghost: the same-host door was really walked through (probe)

vars == <<admitted, fencedC, resvHeld, expired, mappings, curGen, owner,
          staleWrote, sweepFired, readmitFired>>

Init ==
  /\ admitted = {} /\ fencedC = {} /\ resvHeld = FALSE /\ expired = {}
  /\ mappings = {} /\ curGen = 1 /\ owner = NoOwner
  /\ staleWrote = FALSE /\ sweepFired = FALSE /\ readmitFired = FALSE

(* ControllerPublish / LAYOUTGET admission: refused while any fence record
   names the host (node_attach's NQN-level guard + admit_block_host's
   per-client guard — one guard here because hosts are the granularity). *)
Attach(c) ==
  /\ c \notin expired
  /\ \A f \in fencedC : HostOf[f] # HostOf[c]
  /\ admitted' = admitted \union {HostOf[c]}
  /\ readmitFired' = \/ readmitFired
                     \/ (c = Successor /\ sweepFired /\ HostOf[c] = HostOf[Zombie])
  /\ UNCHANGED <<fencedC, resvHeld, expired, mappings, curGen, owner,
                 staleWrote, sweepFired>>

(* LAYOUTGET(RW): a fresh incarnation of the slot (fresh_only). *)
Grant(c) ==
  /\ c \notin expired /\ c \notin fencedC
  /\ HostOf[c] \in admitted
  /\ owner = NoOwner
  /\ curGen < GenMax
  /\ curGen' = curGen + 1
  /\ owner' = c
  /\ mappings' = mappings \union {<<c, curGen + 1>>}
  /\ UNCHANGED <<admitted, fencedC, resvHeld, expired,
                 staleWrote, sweepFired, readmitFired>>

(* Lease expiry.  ClientHonorsLease is the kernel's fs/nfs discard: the
   mappings die WITH the lease.  Without it (frozen VM), they persist. *)
Expire(c) ==
  /\ c \notin expired
  /\ expired' = expired \union {c}
  /\ mappings' = IF ClientHonorsLease
                 THEN {m \in mappings : m[1] # c}
                 ELSE mappings
  /\ UNCHANGED <<admitted, fencedC, resvHeld, curGen, owner,
                 staleWrote, sweepFired, readmitFired>>

(* The sweep's fence arm: durable record + host eviction (attach rows
   included — the F5-regression lesson) + EA-RO acquisition. *)
SweepFence(c) ==
  /\ c \in expired /\ owner = c /\ c \notin fencedC
  /\ fencedC' = fencedC \union {c}
  /\ admitted' = admitted \ {HostOf[c]}
  /\ resvHeld' = TRUE
  /\ UNCHANGED <<expired, mappings, curGen, owner,
                 staleWrote, sweepFired, readmitFired>>

(* The sweep's revoke arm — delivered-gated: the bulk return runs only
   with the reservation confirmed held (UnconfirmedFence refuses). *)
SweepRevoke(c) ==
  /\ c \in fencedC /\ owner = c /\ resvHeld
  /\ owner' = NoOwner
  /\ sweepFired' = TRUE
  /\ UNCHANGED <<admitted, fencedC, resvHeld, expired, mappings, curGen,
                 staleWrote, readmitFired>>

(* The sweep's auto-unfence: record cleared; the volume-wide reservation
   releases only when no sibling fence remains. *)
Unfence(c) ==
  /\ c \in fencedC /\ owner # c
  /\ fencedC' = fencedC \ {c}
  /\ resvHeld' = IF fencedC \ {c} = {} THEN FALSE ELSE resvHeld
  /\ UNCHANGED <<admitted, expired, mappings, curGen, owner,
                 staleWrote, sweepFired, readmitFired>>

(* The hazard: a STALE mapping's write reaching the device.  The gates are
   the code's real ones — transport (allow-list) and reservation (EA-RO
   refuses non-registrants).  Only an expired client models the zombie;
   gen < curGen makes the mapping stale (the slot was re-granted). *)
ZombieWrite(c) ==
  /\ c \in expired
  /\ \E m \in mappings : m[1] = c /\ m[2] < curGen
  /\ HostOf[c] \in admitted
  /\ ~resvHeld
  /\ staleWrote' = TRUE
  /\ UNCHANGED <<admitted, fencedC, resvHeld, expired, mappings, curGen,
                 owner, sweepFired, readmitFired>>

Next ==
  \/ \E c \in Clients : Attach(c)
  \/ \E c \in Clients : Grant(c)
  \/ \E c \in Clients : Expire(c)
  \/ \E c \in Clients : SweepFence(c)
  \/ \E c \in Clients : SweepRevoke(c)
  \/ \E c \in Clients : Unfence(c)
  \/ \E c \in Clients : ZombieWrite(c)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* THE invariant: no stale mapping's bytes ever reach the device. *)
Inv_NoStaleDeviceWrite == staleWrote = FALSE

(* Probes (mutation-run: TLC must FIND these violated in the shipped cfg,
   or the strict green is about a state space where nothing happened). *)
ProbeSweepFires   == sweepFired = FALSE
ProbeReadmitFires == readmitFired = FALSE

=============================================================================
