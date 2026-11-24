#!/usr/bin/env bash
# run.sh: the catch-up gate's open model-checking runs on ONE large machine, with tla-mc.
#
#   ./run.sh list                      the runs, their inputs and what each should print
#   ./run.sh NAME... [-- TLAMC_ARGS]   run each in turn; one line per run in runs.out
#
# Each run gets a fresh work/NAME/ with its cfg and every module it reads copied in from
# formal/, and each copy's md5 asserted against the md5 the gate's tables cite. The
# assertion is what makes a result citable, so a mismatch stops the run before tla-mc starts.
#
# Environment (all optional):
#   TLAMC     the tla-mc binary (default: tla-mc on PATH); must print "tla-mc 0.3.0"
#             unless ALLOW_VERSION=1
#   COMPILE   1 (default) = -compile (needs cargo; ~10x faster on long runs), 0 = interpreted
#   WORKERS   threads (default: every core)
#   FPMEM     MB of RAM for the seen set before it spills (default: a quarter of RAM)
#   QMEM      MB of RAM for the BFS queue before it spills (default: an eighth of RAM)
#   META      directory for spill files and checkpoints (default: ./meta); put it on a large
#             NVMe: a three-replica arm can write hundreds of GB
#   CKPT      checkpoint interval in minutes (default 30; 0 = never)
#   CAP       seconds of wall per run, 0 = no cap (default 0); a capped run is BOUNDED, not a pass
#   RECOVER   1 = resume each NAME from its checkpoint in META/NAME instead of starting fresh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
formal=$(cd "$here/../.." && pwd)
cd "$here"

# NAME | cfg (from formal/) | cfg md5 | expect | spec:md5 then the modules it EXTENDS, each path:md5
# expect: HOLD = no error, a full pass; VIOL:<Inv> = must fail that invariant (a positive control).
QUEUE='
item7-crash   results/2026-11-09-resize-sym/cfg/R3_SymV_crash_r3f1a3.cfg      74e6f1fd HOLD  results/2026-11-09-resize-sym/FlintCatchupSRSymV.tla:f7dbaffd results/2026-11-09-resize-sym/FlintCatchupSRSym.tla:b2884e17 FlintCatchupSR.tla:a202f0f8
item7-zombie  results/2026-11-09-resize-sym/cfg/R3_SymV_zombie_r3f1a3.cfg     02b4e070 HOLD  results/2026-11-09-resize-sym/FlintCatchupSRSymV.tla:f7dbaffd results/2026-11-09-resize-sym/FlintCatchupSRSym.tla:b2884e17 FlintCatchupSR.tla:a202f0f8
live3-lost    results/2026-10-31-c31-ec2/jobsrc/lv3/FlintCatchupL17L3M.cfg    45b953f8 HOLD  FlintCatchupSLiveW2.tla:6604c794 FlintCatchupSLiveW.tla:752b1a56 FlintCatchupS.tla:61ae6829
live3-crash   results/2026-10-31-c31-ec2/jobsrc/lv3/FlintCatchupL17C3M.cfg    c0f8f964 HOLD  FlintCatchupSLiveW2.tla:6604c794 FlintCatchupSLiveW.tla:752b1a56 FlintCatchupS.tla:61ae6829
four-zombie   results/2026-11-07-vc-cluster/cfg-B_Four_zombie_r4f3a3.cfg.deferred fd1fd1e1 HOLD results/2026-11-07-vc-cluster/FlintCatchupSSymV.tla:35a7ec38 results/2026-11-07-vc-cluster/FlintCatchupSSym.tla:9f274f80 FlintCatchupS.tla:61ae6829
nodrain-ctl   results/2026-11-07-vc-cluster/cfg/M8_CtlNoDrain_zombie_r3f2a3.cfg 2eecb423 HOLD FlintCatchupS.tla:61ae6829
smoke-bug     results/2026-11-09-resize-sym/cfg/V_SymV_Bug_NoReserve.cfg      f6908fc3 VIOL:Inv_ArrayLeBases results/2026-11-09-resize-sym/FlintCatchupSRSymV.tla:f7dbaffd results/2026-11-09-resize-sym/FlintCatchupSRSym.tla:b2884e17 FlintCatchupSR.tla:a202f0f8
'

md5of() { if command -v md5sum >/dev/null 2>&1; then md5sum < "$1" | cut -c1-8; else md5 -q "$1" | cut -c1-8; fi; }
ram_mb() {
    if [ -r /proc/meminfo ]; then awk '/MemTotal/{print int($2/1024)}' /proc/meminfo
    else echo $(( $(sysctl -n hw.memsize) / 1048576 )); fi
}
row() { echo "$QUEUE" | awk -v n="$1" '$1 == n'; }

if [ "${1:-}" = list ] || [ $# -eq 0 ]; then
    echo "$QUEUE" | awk 'NF {printf "%-13s expect %-22s cfg %s (%s)\n", $1, $4, $2, $3; for (i = 5; i <= NF; i++) printf "%15s %s\n", (i == 5 ? "spec" : "module"), $i}'
    exit 0
fi

names=(); extra=()
while [ $# -gt 0 ]; do [ "$1" = -- ] && { shift; extra=("$@"); break; }; names+=("$1"); shift; done
for n in "${names[@]}"; do [ -n "$(row "$n")" ] || { echo "run: no run named $n (./run.sh list)" >&2; exit 2; }; done

TLAMC=${TLAMC:-tla-mc}
ver=$("$TLAMC" -version 2>&1 | head -1)
case "$ver" in
    "tla-mc 0.3.0"*) ;;
    *) [ "${ALLOW_VERSION:-0}" = 1 ] || { echo "run: $TLAMC prints '$ver', want tla-mc 0.3.0 (cargo install tla-mc --version 0.3.0 --locked), or ALLOW_VERSION=1" >&2; exit 2; };;
esac
mem=$(ram_mb)
WORKERS=${WORKERS:-$(getconf _NPROCESSORS_ONLN)}
FPMEM=${FPMEM:-$(( mem / 4 ))}
QMEM=${QMEM:-$(( mem / 8 ))}
META=${META:-$here/meta}
CKPT=${CKPT:-30}
CAP=${CAP:-0}
COMPILE=${COMPILE:-1}
tmo=()
if [ "$CAP" != 0 ]; then
    if command -v timeout >/dev/null 2>&1; then tmo=(timeout "$CAP")
    elif command -v gtimeout >/dev/null 2>&1; then tmo=(gtimeout "$CAP")
    else echo "run: CAP needs timeout (GNU coreutils)" >&2; exit 2; fi
fi
mkdir -p logs work "$META"
ulimit -n 65536 2>/dev/null || ulimit -n 20000 2>/dev/null || true

for n in "${names[@]}"; do
    set -- $(row "$n")
    cfg=$2 cfgmd5=$3 expect=$4
    shift 4
    mods=("$@")
    spec=$(basename "${mods[0]%%:*}")
    w=$here/work/$n
    if [ "${RECOVER:-0}" != 1 ]; then rm -rf "$w" "$META/$n"; fi
    mkdir -p "$w" "$META/$n"
    # the inputs, each asserted
    got=$(md5of "$formal/$cfg")
    if [ "$got" != "$cfgmd5" ]; then echo "run: $n: $cfg is $got, want $cfgmd5" >&2; exit 1; fi
    cp "$formal/$cfg" "$w/$n.cfg"
    inputs="cfg $got"
    for m in "${mods[@]}"; do
        p=${m%%:*} want=${m##*:}
        got=$(md5of "$formal/$p")
        [ "$got" = "$want" ] || { echo "run: $n: $p is $got, want $want" >&2; exit 1; }
        cp "$formal/$p" "$w/"
        inputs="$inputs $(basename "$p") $got"
    done
    flags=(-workers "$WORKERS" -fpmem "$FPMEM" -queue-mem "$QMEM" -metadir "$META/$n" -checkpoint "$CKPT")
    [ "$COMPILE" = 1 ] && flags=(-compile "${flags[@]}")
    [ "${RECOVER:-0}" = 1 ] && flags+=(-recover "$META/$n")
    log=$here/logs/$n.log
    {
        echo "RUN $n $(date -u +%FT%TZ) host $(hostname) cores $(getconf _NPROCESSORS_ONLN) ram ${mem} MB"
        echo "RUN $n $ver | inputs: $inputs"
        echo "RUN $n $TLAMC ${flags[*]} ${extra[*]:-} -config $n.cfg $spec | expect $expect | cap ${CAP}s"
    } | tee -a runs.out > "$log"
    t0=$(date +%s)
    set +e
    # ${a[@]+"${a[@]}"}: an empty array under set -u (bash 3.2 calls it unbound)
    (cd "$w" && ${tmo[@]+"${tmo[@]}"} nice -n 5 "$TLAMC" "${flags[@]}" ${extra[@]+"${extra[@]}"} -config "$n.cfg" "$spec") >> "$log" 2>&1
    rc=$?
    set -e
    wall=$(( $(date +%s) - t0 ))
    msg=$(grep -m1 -E "No error has been found|is violated|were violated|^Error" "$log" ||
          grep -m1 -iE "^tla-mc: .*(error|refus|panic)" "$log" || true)
    inv=$(sed -nE 's/.*Invariant ([A-Za-z_0-9]+) is violated.*/\1/p' "$log" | head -1)
    if [ "$rc" = 0 ] && grep -q "No error has been found" "$log"; then v=HOLD
    elif [ -n "$inv" ]; then v="VIOL:$inv"
    elif grep -qE "Temporal propert.*violated" "$log"; then v=VIOL:temporal
    elif [ "$rc" = 124 ]; then v=BOUNDED
    else v=ERROR; fi
    size=$(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" "$log" | tail -1 || true)
    [ -n "$size" ] || size=$(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" "$log" | tail -1 || true)
    as=MISMATCH
    [ "$v" = "$expect" ] && as=as-expected
    [ "$v" = BOUNDED ] && as="bounded (not a pass)"
    echo "DONE $n $v ($as; expect $expect) rc=$rc wall=${wall}s | $size | $msg" | tee -a runs.out
done
