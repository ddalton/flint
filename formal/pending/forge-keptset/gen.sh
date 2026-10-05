#!/usr/bin/env bash
# OPEN3 (2026-09-29): ForgeSyncKeptSet's worlds, from the gated shipped
# cfgs (../../ForgeSync.cfg, ../../ForgeSyncLive.cfg): each is one of them
# with the sandbox's five constants appended (ReclaimKeptSet ON, the four
# mutations OFF), one constant flipped where named, and the invariant or
# property list replaced where named. Expectations are WORLDS.tsv,
# written before any run.
set -eu
cd "$(dirname "$0")"
F=../..
emit() { # <name> <base cfg> <INVARIANTS list|-> <PROPERTIES list|-> <flips...>
  local name=$1 base=$2 invs=$3 props=$4; shift 4
  local out=ForgeSyncKeptSet$name.cfg
  sed -e 's/^  GraceOutlivesUpload = TRUE$/  GraceOutlivesUpload = TRUE\n  ReclaimKeptSet = TRUE\n  KeptSetAnyDrop = FALSE\n  KeptSetVsOriginal = FALSE\n  KeptSetWhileServing = FALSE\n  KeptSetNoRenew = FALSE\n  PacksOverlap = FALSE/' "$F/$base" > "$out"
  for kv in "$@"; do
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
  grep -q "^  ReclaimKeptSet = " "$out" || { echo "gen: $out lacks ReclaimKeptSet" >&2; exit 1; }
}
ALL="TypeOK,Inv_AckedIsDurable,Inv_LandedPackComplete,Inv_NamedIsUploaded,Inv_NoSkipOverMovement,Inv_NoRenewOverWedge,Inv_NoStragglerLandAfterRestore,Inv_NoUnrestorable,Inv_ProofIsOfTheBucket,Inv_ProofNeverRestsOnRetention"
# Mutations drop the retention witness, as the gated mutation cfgs do:
# it can trip first and hide the loss the mutation exists to show.
MUT="${ALL%,Inv_ProofNeverRestsOnRetention}"
# The claim: the shipped rules with the code's collector.
emit Holds         ForgeSync.cfg     "$ALL" -
emit Live          ForgeSyncLive.cfg -      -
# The teeth: each must fire.
emit AnyDrop       ForgeSync.cfg     "$MUT" - KeptSetAnyDrop=TRUE
emit VsOriginal    ForgeSync.cfg     "$MUT" - KeptSetVsOriginal=TRUE
emit WhileServing  ForgeSync.cfg     "$MUT" - KeptSetWhileServing=TRUE
emit NoRenew       ForgeSync.cfg     "$MUT" - KeptSetNoRenew=TRUE
# Non-vacuity: each probe must be violated.
emit ProbeCommits  ForgeSync.cfg     TypeOK ProbeKeptSetCommits
emit ProbeResidue  ForgeSync.cfg     TypeOK ProbeKeptSetDropsResidue
emit ProbeCovered  ForgeSync.cfg     TypeOK ProbeKeptSetDropsCovered
# The sandbox's identity: with ReclaimKeptSet OFF it must be the gated
# module exactly. ForgeSyncLive held at 1,781,559 distinct (shipped gate,
# 2026-09-28); this must match to the state.
emit OffLive       ForgeSyncLive.cfg -      -    ReclaimKeptSet=FALSE
# 2026-10-05: packs that overlap (PacksOverlap), so the kept set can drop a
# pack BECAUSE the kept packs cover it. ProbeCovered and VsOriginal were
# vacuous without it. Symmetry over syncers only: the first push is special.
overlap() { emit "$@" PacksOverlap=TRUE; sed -i '' -e '/^SYMMETRY$/{n;s/^  Sym$/  SymOverlap/;}' "ForgeSyncKeptSet$1.cfg"
  grep -q "^  SymOverlap$" "ForgeSyncKeptSet$1.cfg" || { echo "gen: $1 kept Sym" >&2; exit 1; }; }
overlap OverlapHolds         ForgeSync.cfg "$ALL" -
overlap OverlapVsOriginal    ForgeSync.cfg "$MUT" - KeptSetVsOriginal=TRUE
overlap OverlapProbeCovered  ForgeSync.cfg TypeOK ProbeKeptSetDropsCovered
