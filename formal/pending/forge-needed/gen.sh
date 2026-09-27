#!/usr/bin/env bash
# ForgeSyncNeeded's worlds, from the gated cfgs (../../ForgeSync.cfg and
# ../../ForgeSyncFoldReachableCoverage.cfg): each world is one of them
# with the sandbox's two constants appended and, where named, one
# constant flipped and the invariant list replaced. Expectations are
# WORLDS.tsv, written before any run (2026-09-26).
set -eu
cd "$(dirname "$0")"
F=../..
emit() { # <name> <base cfg> <NameNeeded> <NeededNamesQueued> <invariants|-> <flips...>
  local name=$1 base=$2 nn=$3 nq=$4 invs=$5; shift 5
  local out=ForgeSyncNeeded$name.cfg
  sed -e 's/^  GraceOutlivesUpload = TRUE$/  GraceOutlivesUpload = TRUE\n  NameNeeded = '"$nn"'\n  NeededNamesQueued = '"$nq"'/' "$F/$base" > "$out"
  for kv in "$@"; do
    sed -i '' -e "s/^  ${kv%%=*} = .*/  ${kv%%=*} = ${kv#*=}/" "$out"
  done
  if [ "$invs" != - ]; then
    awk -v invs="$invs" '
      /^INVARIANTS/ { print; n=split(invs, a, ","); for (i=1;i<=n;i++) print "  " a[i]; skip=1; next }
      skip && /^  / { next }
      { skip=0; print }' "$out" > "$out.tmp" && mv "$out.tmp" "$out"
  fi
  grep -q "NameNeeded = $nn" "$out" || { echo "gen: $out lacks NameNeeded" >&2; exit 1; }
}
ALL="TypeOK,Inv_AckedIsDurable,Inv_LandedPackComplete,Inv_NamedIsUploaded,Inv_NoSkipOverMovement,Inv_NoRenewOverWedge,Inv_NoStragglerLandAfterRestore,Inv_NoUnrestorable,Inv_ProofIsOfTheBucket,Inv_ProofNeverRestsOnRetention,Inv_NamedIsLanded"
# The claim: the strict shape, and the reachable supersede, both hold.
emit Holds             ForgeSync.cfg                      TRUE  FALSE "$ALL"
emit Reachable         ForgeSync.cfg                      TRUE  FALSE "$ALL" FoldReachableCoverage=TRUE
# The teeth.
emit QueuedReachable   ForgeSyncFoldReachableCoverage.cfg TRUE  TRUE  -
emit QueuedNamed       ForgeSync.cfg                      TRUE  TRUE  TypeOK,Inv_NamedIsLanded
emit DirectoryNamed    ForgeSync.cfg                      FALSE FALSE TypeOK,Inv_NamedIsLanded
emit DirectoryReachable ForgeSyncFoldReachableCoverage.cfg FALSE FALSE -

# ── ForgeSyncRewind (ref rewinds), written 2026-09-26 before any run ──
remit() { # <name> <invariants|-> <flips...>
  local name=$1 invs=$2; shift 2
  local out=ForgeSyncRewind$name.cfg
  sed -e 's/^  NeededNamesQueued = FALSE$/  NeededNamesQueued = FALSE\n  MaxRewinds = 1\n  MaxResends = 1\n  NeededTrustsDisk = FALSE\n  NeededSkipsRetained = FALSE/' \
      ForgeSyncNeededReachable.cfg > "$out"
  for kv in "$@"; do sed -i '' -e "s/^  ${kv%%=*} = .*/  ${kv%%=*} = ${kv#*=}/" "$out"; done
  if [ "$invs" != - ]; then
    awk -v invs="$invs" '
      /^INVARIANTS/ { print; n=split(invs, a, ","); for (i=1;i<=n;i++) print "  " a[i]; skip=1; next }
      skip && /^  / { next }
      { skip=0; print }' "$out" > "$out.tmp" && mv "$out.tmp" "$out"
  fi
  grep -q "MaxRewinds = " "$out" || { echo "gen: $out lacks MaxRewinds" >&2; exit 1; }
}
RALL="TypeOK,Inv_AckedIsDurable,Inv_LandedPackComplete,Inv_NamedIsUploaded,Inv_NoSkipOverMovement,Inv_NoRenewOverWedge,Inv_NoStragglerLandAfterRestore,Inv_NoUnrestorable,Inv_ProofIsOfTheBucket,Inv_ProofNeverRestsOnRetention"
remit Holds            "$RALL"
remit Strict           "$RALL" FoldReachableCoverage=FALSE
remit TrustsDisk       "$RALL" NeededTrustsDisk=TRUE
remit SkipsRetained    "$RALL" NeededSkipsRetained=TRUE
remit ProbeCollected   TypeOK,ProbeRewoundCollected
remit ProbeResurrected TypeOK,ProbeResurrected
