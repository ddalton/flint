-------------------------- MODULE ForgeMergeChain --------------------------
(***************************************************************************)
(* THE DIMENSION `ForgeSync.tla` DOES NOT HAVE.                            *)
(*                                                                         *)
(* On 2026-09-08 the F14 drill on runcj found forge publishing a ref whose  *)
(* PARENT had reached no pack in the bucket.  The syncer refused on every   *)
(* restart with exit 78 — `fsck --connectivity-only` reporting `broken      *)
(* link ... missing commit` — over a repository that was perfectly intact   *)
(* on the pod's disk and unrecoverable from S3.                            *)
(*                                                                         *)
(* ForgeSync could not have caught it, and the reason is worth stating      *)
(* precisely rather than as a shrug:                                        *)
(*                                                                         *)
(*   - it has NO server-built commits.  `merge`, `commit_tree`, `refs/for`  *)
(*     and `server_created` appear ZERO times in it.  It models client      *)
(*     `Pushes` and `FoldIds`, and the commit that went missing was one the *)
(*     SERVER created.                                                     *)
(*   - it has NO ancestry.  `holds[q]` maps a pack to a set of Pushes; a    *)
(*     commit is an atomic token with no parent.  "the tip is present, its  *)
(*     PARENT is not" is not expressible in it.                            *)
(*   - so its `Restore` is strictly weaker than the real one.  It refuses   *)
(*     only when the TIP is in no named pack:                              *)
(*                                                                         *)
(*         ~\E q \in usable : snap.main \in holds[q]                       *)
(*                                                                         *)
(*     while the shipped syncer walks the whole reachable graph.  THE MODEL *)
(*     WOULD CALL THE CORRUPT BUCKET RESTORABLE.                           *)
(*                                                                         *)
(* This module adds exactly that dimension and nothing else: server-built   *)
(* commits, the base each was built on, the pack a batch writes for them,   *)
(* and a restore predicate that is REACHABILITY rather than tip-presence.   *)
(* It is deliberately a separate, small module — ForgeSync is 1,300 lines   *)
(* about leases, folds and stragglers, and this question is orthogonal to   *)
(* all of it.                                                              *)
(*                                                                         *)
(* Two mutations reproduce the two shipped defects (both fixed in           *)
(* `8381b557`), and the gate requires TLC to FIND each:                     *)
(*                                                                         *)
(*   ExcludeMergeBase   the pack excluded a base the bucket did not hold.   *)
(*                      `judge_merge` takes its base from the EFFECTIVE ref *)
(*                      map, so a second merge's base is the first merge's  *)
(*                      TIP — loose, and the very thing the pack is meant   *)
(*                      to carry.  `pack-objects` got `M ^M`.               *)
(*   CoalesceRefUpdates FALSE = the shipped `update-ref --stdin` call, which*)
(*                      refuses two updates to one ref — at STEP 6, AFTER   *)
(*                      the pack, the upload and the snapshot CAS.  A batch *)
(*                      git rejects is published first and errors after.    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Clients,            \* client commit ids, e.g. {c1, c2, c3}
  Merges,             \* server-built commit ids, e.g. {m1, m2}
  Packs,              \* pack ids the batch may write, e.g. {q2}
  SeedPack,           \* the pack the bucket starts with
  SeedTip,            \* the commit that pack's ref names
  NoCommit,           \* the absent commit
  ExcludeMergeBase,   \* mutation: exclude every base, durable or not
  CoalesceRefUpdates  \* TRUE: one transaction entry per ref (the fix)

Commits == Clients \cup Merges

VARIABLES
  bucket,     \* SUBSET (Packs \cup {SeedPack})  — packs actually uploaded
  snapPacks,  \* SUBSET (Packs \cup {SeedPack})  — packs the snapshot NAMES
  snapRef,    \* the commit the snapshot's ref names
  holds,      \* [Packs \cup {SeedPack} -> SUBSET Commits]
  baseOf,     \* [Commits -> Commits \cup {NoCommit}]  first parent
  sideOf,     \* [Commits -> Commits \cup {NoCommit}]  second parent
  localTip,   \* the ref in the pod's own repository
  spent       \* packs already written (bounds the run)

vars == <<bucket, snapPacks, snapRef, holds, baseOf, sideOf, localTip, spent>>

AllPacks == Packs \cup {SeedPack}

(***************************************************************************)
(* ANCESTRY.  The thing ForgeSync cannot say.                              *)
(***************************************************************************)
RECURSIVE AncOf(_)
AncOf(c) ==
  IF c = NoCommit THEN {}
  ELSE {c} \cup AncOf(baseOf[c]) \cup AncOf(sideOf[c])

\* Everything a restore can actually see: the packs the snapshot NAMES
\* that are also IN the bucket.
Visible == UNION {holds[q] : q \in (snapPacks \cap bucket)}

TypeOK ==
  /\ bucket \subseteq AllPacks
  /\ snapPacks \subseteq AllPacks
  /\ snapRef \in Commits \cup {NoCommit}
  /\ localTip \in Commits \cup {NoCommit}
  /\ spent \subseteq Packs

Init ==
  \* The bucket already holds one pack with the client commits in it: a
  \* proposal's objects arrive inside the CLIENT's pack, and that pack is
  \* uploaded by the push that carried it. The corruption is never about
  \* the client's objects.
  /\ holds = [q \in AllPacks |-> IF q = SeedPack THEN Clients ELSE {}]
  /\ bucket = {SeedPack}
  /\ snapPacks = {SeedPack}
  /\ snapRef = SeedTip
  /\ localTip = SeedTip
  /\ baseOf = [c \in Commits |-> NoCommit]
  /\ sideOf = [c \in Commits |-> NoCommit]
  /\ spent = {}

(***************************************************************************)
(* ONE BATCH CARRYING TWO PROPOSALS FOR THE SAME REF.                      *)
(*                                                                         *)
(* Four agents proposing `refs/for/main` at once arrive in ONE batch; two   *)
(* is enough to show it. The second merge is judged against the FIRST's     *)
(* tip, because `judge_merge` reads the effective ref map that the first    *)
(* command already moved.                                                  *)
(***************************************************************************)
BatchTwoMerges(m1, m2, s1, s2, q) ==
  /\ m1 \in Merges /\ m2 \in Merges /\ m1 # m2
  /\ s1 \in Clients /\ s2 \in Clients /\ s1 # s2
  /\ s1 # localTip /\ s2 # localTip
  /\ q \in Packs /\ q \notin spent
  /\ baseOf[m1] = NoCommit /\ baseOf[m2] = NoCommit   \* not yet built
  /\ localTip # NoCommit
  /\ LET nb == [baseOf EXCEPT ![m1] = localTip, ![m2] = m1]
         ns == [sideOf EXCEPT ![m1] = s1,       ![m2] = s2]
         \* What the batch would exclude. `snapRef` is DURABLE — a CAS
         \* names only what it uploaded or a prior CAS named. The bases
         \* are not: one of them is m1, which exists at this moment only
         \* as a loose object this very pack is meant to carry.
         excl == IF ExcludeMergeBase
                   THEN {localTip, m1, snapRef}
                   ELSE {snapRef}
         \* Ancestry closure of the excluded set, under the NEW parent
         \* map — `pack-objects` walks, it does not just drop an id.
         AncU(S) == UNION {LET RECURSIVE A(_)
                               A(c) == IF c = NoCommit THEN {}
                                       ELSE {c} \cup A(nb[c]) \cup A(ns[c])
                           IN A(x) : x \in S}
         want == LET RECURSIVE B(_)
                     B(c) == IF c = NoCommit THEN {}
                             ELSE {c} \cup B(nb[c]) \cup B(ns[c])
                 IN B(m1) \cup B(m2)
         content == want \ AncU(excl)
     IN /\ baseOf' = nb
        /\ sideOf' = ns
        /\ holds' = [holds EXCEPT ![q] = content]
        \* Steps 4 and 5: the pack is uploaded, then ONE CAS names it and
        \* the new tip. This is where the bucket becomes wrong.
        /\ bucket' = bucket \cup {q}
        /\ snapPacks' = snapPacks \cup {q}
        /\ snapRef' = m2
        \* Step 6, LAST: the ref transaction. Without coalescing, git
        \* refuses two updates to one ref and the batch errors — AFTER
        \* everything above already happened.
        /\ localTip' = IF CoalesceRefUpdates THEN m2 ELSE localTip
        /\ spent' = spent \cup {q}

Next == \E m1, m2 \in Merges, s1, s2 \in Clients, q \in Packs :
          BatchTwoMerges(m1, m2, s1, s2, q)

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

(***************************************************************************)
(* THE INVARIANT ForgeSync IS MISSING.                                     *)
(*                                                                         *)
(* Not "the tip is in a named pack" but "EVERY COMMIT REACHABLE FROM THE    *)
(* TIP is", which is what `git fsck --connectivity-only` asks and what the  *)
(* shipped syncer runs on every start.                                     *)
(***************************************************************************)
Inv_RestorableFromBucket ==
  snapRef # NoCommit => AncOf(snapRef) \subseteq Visible

\* The pod and the bucket must not part company: a batch that publishes a
\* tip the local repository never moved to leaves every later push to that
\* ref refused as `disagreed` until a restart reconciles them.
Inv_LocalAgreesWithBucket == localTip = snapRef

\* Every pack the snapshot names is uploaded — ForgeSync's own
\* Inv_NamedIsUploaded, restated here so this module cannot pass by
\* naming packs that do not exist.
Inv_NamedIsUploaded == snapPacks \subseteq bucket

Inv == /\ TypeOK
       /\ Inv_NamedIsUploaded
       /\ Inv_RestorableFromBucket
       /\ Inv_LocalAgreesWithBucket
=============================================================================
