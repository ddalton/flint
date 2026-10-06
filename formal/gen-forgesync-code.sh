#!/usr/bin/env bash
# 2026-10-05: the ForgeSync worlds on the CODE's combination — the shipped
# baseline (ForgeSync.cfg / ForgeSyncLive.cfg) with the two rules the code now
# runs: NameNeeded (381bcc90: a batch names what its pushes need) and
# ReclaimKeptSet (restore.rs reclaim_at_rest: the reclaim builds nothing and
# drops what the kept named packs cover). The 24 ForgeSync worlds before these
# keep both OFF and are unchanged (ForgeSyncLive: 1,781,559 distinct, as on
# TLC). Each world here is ForgeSync.cfg or ForgeSyncLive.cfg with the two
# rules ON, one constant flipped where named, and the invariant list replaced
# where named. Expectations: WORLDS-ForgeSyncCode.tsv, written before any run.
set -eu
cd "$(dirname "$0")"
emit() { # <name> <base cfg> <INVARIANTS list|-> <PROPERTIES list|-> <flips...>
  local name=$1 base=$2 invs=$3 props=$4; shift 4
  local out=ForgeSyncCode$name.cfg
  cp "$base" "$out"
  for kv in NameNeeded=TRUE ReclaimKeptSet=TRUE RestoreDropsNamedRetention=TRUE "$@"; do
    grep -q "^  ${kv%%=*} = " "$out" || { echo "gen: $out has no ${kv%%=*}" >&2; exit 1; }
    sed -i '' -e "s/^  ${kv%%=*} = .*/  ${kv%%=*} = ${kv#*=}/" "$out"
  done
  if [ "$invs" != - ]; then
    awk -v invs="$invs" '
      /^INVARIANTS/ { print; n=split(invs, a, ","); for (i=1;i<=n;i++) print "  " a[i]; skip=1; next }
      skip && /^  / { next }
      { skip=0; print }' "$out" > "$out.tmp" && mv "$out.tmp" "$out"
  fi
  if [ "$props" != - ]; then
    awk -v props="$props" '
      /^VIEW/ && !done { print "PROPERTIES"; n=split(props, a, ","); for (i=1;i<=n;i++) print "  " a[i]; done=1 }
      { print }' "$out" > "$out.tmp" && mv "$out.tmp" "$out"
  fi
}
# Overlap worlds: the first push is distinguished, so symmetry over syncers only.
overlap() { emit "$@" PacksOverlap=TRUE; sed -i '' -e '/^SYMMETRY$/{n;s/^  Sym$/  SymOverlap/;}' "ForgeSyncCode$1.cfg"
  grep -q "^  SymOverlap$" "ForgeSyncCode$1.cfg" || { echo "gen: $1 kept Sym" >&2; exit 1; }; }
ALL="TypeOK,Inv_AckedIsDurable,Inv_LandedPackComplete,Inv_NamedIsUploaded,Inv_NoSkipOverMovement,Inv_NoRenewOverWedge,Inv_NoStragglerLandAfterRestore,Inv_NoUnrestorable,Inv_ProofIsOfTheBucket,Inv_ProofNeverRestsOnRetention,Inv_NamedIsLanded"
MUT="${ALL%,Inv_ProofNeverRestsOnRetention,Inv_NamedIsLanded}"
# Inv_NamedIsLanded ("nothing named that no ref reaches") is an idealisation
# of non-overlapping packs: with PacksOverlap the needed listing names p2's
# pack for p1's object, and p2's commit has not landed. It is not a
# durability claim, so the overlap hold checks every OTHER invariant
# (first box run 2026-10-05: it fired at depth 11 and hid the rest).
ALLO="${ALL%,Inv_NamedIsLanded}"
# The claim on the code's combination.
emit    ""                    ForgeSync.cfg     "$ALL" -
emit    Live                  ForgeSyncLive.cfg -      -
overlap Overlap               ForgeSync.cfg     "$ALLO" -
# NameNeeded's tooth (the forge-needed sandbox's QueuedNamed).
emit    NeededQueued          ForgeSync.cfg     TypeOK,Inv_NamedIsLanded - NeededNamesQueued=TRUE
# ReclaimKeptSet's teeth (the forge-keptset sandbox's). With NameNeeded and
# packs that do not overlap the kept-set reclaim NEVER commits — no named
# pack is covered by the others — so WhileServing, NoRenew and both probes
# held at exactly ForgeSyncCode's count (30,277,694; box 2026-10-05). They
# run with PacksOverlap, where the reclaim acts; AnyDrop fires either way.
emit    KeptSetAnyDrop        ForgeSync.cfg     "$MUT" - KeptSetAnyDrop=TRUE
overlap KeptSetVsOriginal     ForgeSync.cfg     "$MUT" - KeptSetVsOriginal=TRUE
overlap KeptSetWhileServing   ForgeSync.cfg     "$MUT" - KeptSetWhileServing=TRUE
overlap KeptSetNoRenew        ForgeSync.cfg     "$MUT" - KeptSetNoRenew=TRUE
# Non-vacuity: each must be violated.
overlap ProbeKeptSetCommits   ForgeSync.cfg     TypeOK ProbeKeptSetCommits
overlap ProbeKeptSetResidue   ForgeSync.cfg     TypeOK ProbeKeptSetDropsResidue
overlap ProbeKeptSetCovered   ForgeSync.cfg     TypeOK ProbeKeptSetDropsCovered
# Ref REWINDS on the code's combination (2026-10-05; the rewind sandbox,
# formal/pending/forge-needed, ran only before the shipped baseline and with
# the refuted reachable-coverage fold). One rewind, one re-push. Inv_NamedIsLanded
# is out: a rewind is exactly a named pack holding what no ref reaches.
#   Rewind        — the code at ce2a1af6: the restore drops named packs from
#                   retention; a batch that re-names a retained pack does not
#                   (NamingUnretains=FALSE). RECORD: whether the code loses.
#   RewindNoFix   — the code before ce2a1af6. RECORD.
#   RewindModel   — both rules (the sandbox's). Must hold.
RALL="$ALLO"
emit    Rewind                ForgeSync.cfg     "$RALL" - MaxRewinds=1 MaxResends=1
emit    RewindNoFix           ForgeSync.cfg     "$RALL" - MaxRewinds=1 MaxResends=1 RestoreDropsNamedRetention=FALSE
emit    RewindModel           ForgeSync.cfg     "$RALL" - MaxRewinds=1 MaxResends=1 NamingUnretains=TRUE
emit    RewindTrustsDisk      ForgeSync.cfg     "$RALL" - MaxRewinds=1 MaxResends=1 NamingUnretains=TRUE NeededTrustsDisk=TRUE
emit    RewindProbeCollected  ForgeSync.cfg     TypeOK,ProbeRewoundCollected - MaxRewinds=1 MaxResends=1 NamingUnretains=TRUE
emit    RewindProbeResurrected ForgeSync.cfg    TypeOK,ProbeResurrected - MaxRewinds=1 MaxResends=1 NamingUnretains=TRUE
