#!/usr/bin/env bash
# Partition the hub TLA+ gate across N processes, and print the matrix.
#
# WHY THIS FILE EXISTS, RATHER THAN A LIST IN THE WORKFLOW
#
# The obvious way to shard a gate is to write the groups into the YAML by
# hand.  That creates a second place where the set of modules is
# recorded, and a second place is a place to forget: add a module to
# check-tla.sh, forget the YAML, and the module is never checked again by
# anything — silently, in green, which is precisely how the hub gate came
# to spend months unable to run on Linux at all without anyone noticing.
#
# So the module set is READ OUT OF check-tla.sh, which is the only thing
# that defines it.  Omission is not expressible.
#
# BALANCE
#
# Wall clock per module is not proportional to run count — a single deep
# FlintReplication config outweighs a dozen cheap probes — so the weights
# come from measurement when measurement exists.  scripts/tla-module-cost.tsv
# (module<TAB>seconds, written from a real gate profile) is consulted
# first; anything absent falls back to run count times DEFAULT_SECS, which
# is a guess and is labelled as one in --explain.  A wrong weight costs a
# slower shard, never a missed run: correctness rests on the partition
# being a partition, not on the weights being right.
#
# Usage:
#   scripts/tla-shards.sh [N]            # JSON matrix for N shards (default 6)
#   scripts/tla-shards.sh [N] --explain  # human-readable table
set -euo pipefail

cd "$(dirname "$0")/.."
GATE=scripts/check-tla.sh
COSTS=scripts/tla-module-cost.tsv
N=${1:-6}
MODE=${2:-json}
DEFAULT_SECS=${TLA_DEFAULT_SECS:-30}

[ -f "$GATE" ] || { echo "no $GATE" >&2; exit 2; }
case "$N" in ''|*[!0-9]*) echo "shard count must be a number, got '$N'" >&2; exit 2 ;; esac
[ "$N" -ge 1 ] || { echo "shard count must be >= 1" >&2; exit 2; }

awk -v n="$N" -v mode="$MODE" -v defsecs="$DEFAULT_SECS" -v costs="$COSTS" '
  # ── the module set, straight from the gate ──────────────────────────
  # Column 0 and unconditional: every call in check-tla.sh is both, which
  # is what makes a static count equal to the runtime count.  If that
  # ever stops being true the count assertion in check-tla.sh fails
  # loudly rather than quietly under-checking.
  FNR==NR {
    if ($0 ~ /^(strict_run|mutation_run|liveness_mutation_run)[ ]/) {
      runs[$2]++; total_runs++
      if (!($2 in seen)) { seen[$2]=1; mods[++m]=$2 }
    }
    next
  }
  # ── measured cost, if any ───────────────────────────────────────────
  # A row whose provenance comment says NOT MEASURED carries the fallback
  # estimate, not a measurement. It must not be reported as measured —
  # "all 18 modules measured" over six guesses is the kind of confident
  # summary that stops anyone going back to fill them in.
  { if ($0 !~ /^#/ && NF>=2) { cost[$1]=$2; if ($0 !~ /NOT MEASURED/) measured[$1]=1 } }

  END {
    if (m==0) { print "no modules found in the gate — refusing to emit an empty matrix" > "/dev/stderr"; exit 3 }
    if (n > m) n = m

    for (i=1; i<=m; i++) {
      mod = mods[i]
      w[i] = (mod in cost) ? cost[mod] : runs[mod]*defsecs
      idx[i] = i
    }
    # LPT: heaviest first into the lightest shard.  Insertion sort — m is
    # ~20, and a shell-portable awk has nothing better.
    for (i=2; i<=m; i++) {
      key=idx[i]; j=i-1
      while (j>=1 && w[idx[j]] < w[key]) { idx[j+1]=idx[j]; j-- }
      idx[j+1]=key
    }
    for (b=1; b<=n; b++) { load[b]=0; list[b]=""; bruns[b]=0 }
    for (i=1; i<=m; i++) {
      src=idx[i]; best=1
      for (b=2; b<=n; b++) if (load[b] < load[best]) best=b
      load[best] += w[src]
      bruns[best] += runs[mods[src]]
      list[best] = (list[best]=="" ? mods[src] : list[best] " " mods[src])
    }

    # ── the partition IS the safety property: assert it ───────────────
    # Every module in exactly one shard, and the shard run counts summing
    # to the gate total.  A generator that quietly dropped a module would
    # produce a matrix in which every shard passes and the gate is
    # smaller than it claims.
    check=0
    for (b=1; b<=n; b++) check += bruns[b]
    if (check != total_runs) {
      printf "PARTITION BROKEN: shards total %d runs, gate has %d\n", check, total_runs > "/dev/stderr"
      exit 3
    }

    if (mode == "--explain") {
      printf "%d modules, %d runs, %d shards\n\n", m, total_runs, n
      for (b=1; b<=n; b++)
        printf "shard %d: %3d runs, ~%5ds  %s\n", b, bruns[b], load[b], list[b]
      printf "\nweights: "
      nm=0; for (i=1;i<=m;i++) if (!(mods[i] in measured)) nm++
      if (nm==0) printf "all %d modules measured (%s)\n", m, costs
      else {
        printf "%d of %d modules measured; %d ESTIMATED:", m-nm, m, nm
        for (i=1;i<=m;i++) if (!(mods[i] in measured)) printf " %s", mods[i]
        printf "\n"
      }
      exit 0
    }

    printf "["
    for (b=1; b<=n; b++) {
      if (b>1) printf ","
      printf "{\"id\":%d,\"modules\":\"%s\",\"runs\":%d,\"est\":%d}", b, list[b], bruns[b], load[b]
    }
    printf "]\n"
  }
' "$GATE" "$([ -f "$COSTS" ] && echo "$COSTS" || echo /dev/null)"
