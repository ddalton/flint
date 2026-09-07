#!/usr/bin/env bash
# forge versus walgit, push for push — the P-legs pre-registered in
# docs/plans/flint-forge-simplification-2026-09-05.md §9.
#
#   BUCKET=... PREFIX=forge/<stamp> WPREFIX=walgit/<stamp> KEYFILE=... ./forge/e2e/walgit/run-compare.sh
#
# Both arms are reached DIRECTLY by the same agent pod and the same stock
# git: forge through its door (the deployed shape; the door's inactivity
# bound is 300 s and nothing here approaches it), walgit at its Service
# with a bearer token. The same bytes go to both arms in every leg: a
# repository is built once in the agent's /work and pushed to each arm
# in turn, the order alternating per repetition.
#
# Legs (LEGS selects; the default order puts the destructive one last):
#   P0  preconditions and provenance
#   P1  push latency at P1_SIZES MiB, P1_REPS reps interleaved       (§9 rule: within the rep-to-rep spread)
#   P4  two pushes to one ref from one base, concurrently: one winner (both must hold)
#   P9  repack amplification: P9_N pushes of P9_MB MiB to one branch  (bytes uploaded — CloudWatch, see cw-summary.sh)
#   P2  push rate: P2_N pushers for P2_SECS s to distinct branches     (pushes/s; requests per push — CloudWatch)
#   P7  P7_N concurrent clones of a P7_MB MiB branch, one alone first  (wall, ratio, peak upload-packs)
#   P5  cold start: the arm's pod deleted; time to the first ls-remote and the first clone
#   P11 undo: a force-push, and whether the previous tip can be recovered from the bucket
#   P10 the bucket cut off (a NetworkPolicy): reads, readiness, pushes, recovery — recorded, not scored
#
# INCONCLUSIVE is not PASS. A leg whose precondition did not hold says so
# and is counted apart. The verdict rule is applied by a human against
# the numbers in the log, per §9; this script does not declare a winner.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
: "${BUCKET:?BUCKET}"; : "${PREFIX:?PREFIX (forge)}"; : "${WPREFIX:?WPREFIX (walgit)}"; : "${KEYFILE:?KEYFILE}"
export AWS_ACCESS_KEY_ID; AWS_ACCESS_KEY_ID=$(jq -r .AccessKey.AccessKeyId "$KEYFILE")
export AWS_SECRET_ACCESS_KEY; AWS_SECRET_ACCESS_KEY=$(jq -r .AccessKey.SecretAccessKey "$KEYFILE")
export AWS_REGION=${REGION:-us-west-1} AWS_DEFAULT_REGION=${REGION:-us-west-1}
NS=${NS:-agents}; AGENT=${AGENT:-agent1}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
FREPO=${FREPO:-small}          # forge's FlintRepo (deploy.sh made it)
WREPO=${WREPO:-small}          # walgit's repository, acme/<WREPO>, created on first push
ARMS=${ARMS:-"forge walgit"}
LEGS=${LEGS:-"P0 P1 P4 P9 P2 P7 P5 P11 P10"}
P1_SIZES=${P1_SIZES:-"0 64 1024"}; P1_REPS=${P1_REPS:-5}
P2_N=${P2_N:-32}; P2_SECS=${P2_SECS:-60}
P7_MB=${P7_MB:-1024}; P7_N=${P7_N:-8}
P9_N=${P9_N:-48}; P9_MB=${P9_MB:-8}
STATUS_PORT=${STATUS_PORT:-9848}   # render.rs STATUS_PORT
RUN=$(date +%Y%m%d-%H%M%S)
RESULTS=${RESULTS:-$HERE/results}; WORK=$RESULTS/work-$RUN; mkdir -p "$WORK"
LOG=$RESULTS/compare-$RUN.log
exec > >(tee -a "$LOG") 2>&1

PASS=0; FAIL=0; INCONC=0
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }
leg()    { printf '\n══ %s — %s ══\n' "$1" "$2"; }
K() { kubectl "$@"; }
now() { date +%s; }
window() { echo "$1 $2 $3 $4" >> "$WORK/windows.txt"; }   # leg arm-or-all start end — cw-summary.sh reads these (run 1 wrote three fields and lost the end)
stats() { # median min max of numbers on stdin
  python3 -c 'import sys,statistics as s; v=[float(x) for x in sys.stdin.read().split()]; print(f"{s.median(v):.2f} {min(v):.2f} {max(v):.2f} n={len(v)}" if v else "none")'
}
inpod() { K -n "$NS" exec "$AGENT" -c agent -- sh -c "$*" 2>&1; }
put_script() { K -n "$NS" exec -i "$AGENT" -c agent -- sh -c "cat > /work/$1 && chmod +x /work/$1"; }
WALGIT_TOKEN=$(K -n "$NS" get secret walgit-token -o jsonpath='{.data.WALGIT_TOKEN_AGENT}' 2>/dev/null | base64 -d)
armenv() { echo "ARM=$1 WALGIT_TOKEN=$WALGIT_TOKEN DOOR=$DOOR NS=$NS"; }
forge_pod()  { K -n "$NS" get pods -l "chert.us/repo=$FREPO" -o json 2>/dev/null | jq -r '[.items[] | select(.metadata.deletionTimestamp == null)] | sort_by(.metadata.creationTimestamp) | last | .metadata.name // empty'; }
walgit_pod() { K -n "$NS" get pods -l app=walgit -o json 2>/dev/null | jq -r '[.items[] | select(.metadata.deletionTimestamp == null)] | sort_by(.metadata.creationTimestamp) | last | .metadata.name // empty'; }
arm_pod()    { case "$1" in forge) forge_pod;; walgit) walgit_pod;; esac; }
arm_repo()   { case "$1" in forge) echo "$FREPO";; walgit) echo "$WREPO";; esac; }
pod_ready()  { K -n "$NS" get pod "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null; }
restarts()   { K -n "$NS" get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo 0; }
forge_phase() { local p; p=$(forge_pod); [ -n "$p" ] && K -n "$NS" exec "$p" -c syncer -- wget -qO- "http://127.0.0.1:$STATUS_PORT/status" 2>/dev/null | jq -r '.phase // "?"' || echo "?"; }
upload_packs() { # arm — processes named upload-pack in the serving container
  case "$1" in
    forge)  K -n "$NS" exec "$(forge_pod)" -c git-http -- sh -c "pgrep -f 'upload-pack --stateless-rp[c]' | wc -l" 2>/dev/null;;
    walgit) K -n "$NS" exec "$(walgit_pod)" -- sh -c "grep -l 'upload-pac[k]' /proc/[0-9]*/cmdline 2>/dev/null | wc -l" 2>/dev/null;;
  esac | tr -d '[:space:]'
}
up_watch() { ( while [ ! -e "$2.stop" ]; do echo "$(now) $(upload_packs "$1")"; sleep 0.5; done ) > "$2" 2>/dev/null & }
stop_watch() { touch "$1.stop"; kill "$2" 2>/dev/null; wait "$2" 2>/dev/null; }
max_col() { awk -v c="$2" 'BEGIN{m=0}{if($c+0>m)m=$c+0}END{print m}' "$1" 2>/dev/null; }
cleanup() { touch "$WORK"/*.stop 2>/dev/null; inpod "touch /work/stop" >/dev/null 2>&1; K -n "$NS" delete networkpolicy p10-forge p10-walgit --ignore-not-found >/dev/null 2>&1; }
trap cleanup EXIT

install_agent_scripts() {
  put_script lib.sh <<'EOS'
# ARM, WALGIT_TOKEN, DOOR, NS come from the caller's env.
G() { case "$ARM" in
        walgit) git -c http.extraHeader="Authorization: Bearer $WALGIT_TOKEN" "$@";;
        *) T=$(cat /var/run/secrets/forge/token); git -c http.extraHeader="Authorization: Basic $(printf 'x:%s' "$T" | base64 -w0)" "$@";;
      esac; }
url() { case "$ARM" in walgit) echo "http://walgit.$NS.svc:8080/acme/$1.git";; *) echo "$DOOR/git/$NS/$1.git";; esac; }
ms() { awk '{printf "%d\n", $1*1000}' /proc/uptime; }   # busybox date has no %N; 10 ms resolution, monotonic
EOS
  put_script build.sh <<'EOS'
# build.sh <name> <mb>: a repository of <mb> MiB of incompressible content (0 = one 1 KiB file); prints the tip.
set -e
name=$1; mb=$2
d=/work/$name; rm -rf "$d"; mkdir -p "$d/blobs"; cd "$d"
git init -q -b main; git config user.email cmp@invalid; git config user.name cmp
git config pack.window 0; git config pack.depth 0; git config core.compression 0
if [ "$mb" -eq 0 ]; then head -c 1024 /dev/urandom > blobs/b0
else files=$(( (mb + 31) / 32 )); i=0; while [ $i -lt $files ]; do dd if=/dev/urandom of="blobs/b$i" bs=1M count=32 status=none; i=$((i+1)); done; fi
git add -A >/dev/null; git commit -qm "$name: $mb MiB" >/dev/null
git rev-parse HEAD
EOS
  put_script tpush.sh <<'EOS'
# tpush.sh <dir> <repo> <ref>: push HEAD of /work/<dir> to <ref>, print "<ms> <rc>"
. /work/lib.sh
cd "/work/$1"; t0=$(ms); G push -q "$(url "$2")" "HEAD:refs/heads/$3" >/work/tpush.err 2>&1; rc=$?; t1=$(ms)
echo "$((t1-t0)) $rc"
EOS
  put_script lsremote.sh <<'EOS'
. /work/lib.sh
G ls-remote "$(url "$1")" "refs/heads/$2" 2>/dev/null | cut -f1
EOS
  put_script clone.sh <<'EOS'
# clone.sh <repo> <ref> <dir>: single-branch clone; prints "<ms> <rc> <head>"
. /work/lib.sh
rm -rf "/work/$3"; t0=$(ms); G clone -q --single-branch --branch "$2" "$(url "$1")" "/work/$3" >/work/clone.err 2>&1; rc=$?; t1=$(ms)
echo "$((t1-t0)) $rc $(git -C "/work/$3" rev-parse HEAD 2>/dev/null || echo none)"
EOS
  put_script clones.sh <<'EOS'
# clones.sh <repo> <ref> <n> <tag>: n concurrent single-branch clones; per clone "<i> <start_ms> <end_ms> <rc> <head>" in /work/clone-<tag>.log
. /work/lib.sh
repo=$1; ref=$2; n=$3; tag=$4; log=/work/clone-$tag.log; rm -f "$log"
i=1; while [ $i -le $n ]; do
  ( d=/work/clone-$tag-$i; rm -rf "$d"; s=$(ms)
    G clone -q --single-branch --branch "$ref" "$(url "$repo")" "$d" >/dev/null 2>&1; rc=$?
    h=$(git -C "$d" rev-parse HEAD 2>/dev/null); echo "$i $s $(ms) $rc ${h:-none}" >> "$log"; rm -rf "$d" ) &
  i=$((i+1)); done
wait; echo "done $n"
EOS
  put_script race.sh <<'EOS'
# race.sh <repo> <ref> <base>: two clones of <ref> at <base>, each commits, both push with --force-with-lease=<ref>:<base> at once; prints "rcA rcB tipA tipB"
. /work/lib.sh
repo=$1; ref=$2; base=$3
for c in ra rb; do rm -rf /work/$c; G clone -q --single-branch --branch "$ref" "$(url "$repo")" /work/$c >/dev/null 2>&1
  git -C /work/$c config user.email $c@invalid; git -C /work/$c config user.name $c; echo "$c $(ms)" > /work/$c/$c.txt; git -C /work/$c add $c.txt; git -C /work/$c commit -qm "from $c"; done
( cd /work/ra && G push --force-with-lease=refs/heads/$ref:$base origin HEAD:refs/heads/$ref >/work/ra.out 2>&1; echo $? > /work/ra.rc ) &
( cd /work/rb && G push --force-with-lease=refs/heads/$ref:$base origin HEAD:refs/heads/$ref >/work/rb.out 2>&1; echo $? > /work/rb.rc ) &
wait
echo "$(cat /work/ra.rc) $(cat /work/rb.rc) $(git -C /work/ra rev-parse HEAD) $(git -C /work/rb rev-parse HEAD)"
EOS
  put_script seqpush.sh <<'EOS'
# seqpush.sh <repo> <ref> <n> <mb> <tag>: n sequential commits of <mb> MiB each, pushed one by one; per push "<i> <ms> <rc>" in /work/seq-<tag>.log
. /work/lib.sh
repo=$1; ref=$2; n=$3; mb=$4; tag=$5; d=/work/seq-$tag; log=/work/seq-$tag.log; rm -rf "$d" "$log"; mkdir -p "$d"; cd "$d"
git init -q -b main; git config user.email seq@invalid; git config user.name seq; git config core.compression 0
i=1; while [ $i -le $n ]; do
  dd if=/dev/urandom of="blob$i" bs=1M count=$mb status=none; git add -A >/dev/null; git commit -qm "c$i" >/dev/null
  t0=$(ms); G push -q "$(url "$repo")" "HEAD:refs/heads/$ref" >/dev/null 2>&1; rc=$?; t1=$(ms)
  echo "$i $((t1-t0)) $rc" >> "$log"; i=$((i+1)); done
echo done
EOS
  put_script pusher.sh <<'EOS'
# pusher.sh <repo> <i> <secs> <tag> <run>: tiny commits pushed to agent/p2-<run>-<tag>-<i> as fast as they are acknowledged, for <secs>; per push "<t_ms> <ms> <rc>" in /work/rate-<tag>-<i>.log
. /work/lib.sh
repo=$1; i=$2; secs=$3; tag=$4; run=$5; d=/work/rate-$tag-$i; log=/work/rate-$tag-$i.log; rm -rf "$d" "$log"; mkdir -p "$d"; cd "$d"
git init -q -b main; git config user.email r$i@invalid; git config user.name r$i
echo 0 > f; git add f; git commit -qm init >/dev/null
end=$(( $(ms) + secs*1000 )); n=0
while [ "$(ms)" -lt "$end" ] && [ ! -e /work/stop ]; do
  n=$((n+1)); echo "$n $(ms)" >> f; git commit -qam "c$n" >/dev/null
  t0=$(ms); G push -q "$(url "$repo")" "HEAD:refs/heads/agent/p2-$run-$tag-$i" >/dev/null 2>&1; rc=$?; t1=$(ms)
  echo "$t0 $((t1-t0)) $rc" >> "$log"
done
EOS
}

wait_arm() { # arm secs — the arm answers an ls-remote
  local i; for i in $(seq 1 "$1_"); do :; done 2>/dev/null
  local arm=$1 secs=$2 t; for t in $(seq 1 "$secs"); do
    [ "$(inpod "$(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/seed-$RUN; echo rc=\$?" | tail -1)" = rc=0 ] && return 0; sleep 1; done; return 1
}

# ── P0 ─────────────────────────────────────────────────────────────
leg_P0() {
  leg P0 "preconditions and provenance: both arms answer, the agent's clock has milliseconds, versions recorded"
  K -n "$NS" wait --for=condition=Ready "pod/$AGENT" --timeout=2m >/dev/null 2>&1 || { bad "agent pod not ready"; return; }
  install_agent_scripts
  local d; d=$(inpod '. /work/lib.sh; a=$(ms); sleep 0.3; b=$(ms); echo $((b-a))' | tail -1)
  if [ "${d:-0}" -ge 250 ] 2>/dev/null && [ "$d" -le 600 ]; then ok "agent clock: a 300 ms sleep measures ${d} ms (sub-second resolution)"; else bad "the agent's clock has no sub-second resolution: 300 ms measured as '${d}'"; return; fi
  local fp wp; fp=$(forge_pod); wp=$(walgit_pod)
  case " $ARMS " in *" forge "*)
    [ -n "$fp" ] && [ "$(forge_phase)" = serving ] && ok "forge: $fp serving ($(K -n "$NS" get pod "$fp" -o jsonpath='{.spec.containers[?(@.name=="syncer")].image}')) on $(K -n "$NS" get pod "$fp" -o jsonpath='{.spec.nodeName}')" || { bad "forge $FREPO is not serving"; return; };; esac
  case " $ARMS " in *" walgit "*)
    [ -n "$wp" ] && [ "$(pod_ready "$wp")" = True ] && ok "walgit: $wp ready ($(K -n "$NS" exec "$wp" -- walgit --version 2>/dev/null | head -1)) on $(K -n "$NS" get pod "$wp" -o jsonpath='{.spec.nodeName}')" || { bad "walgit is not ready"; return; };; esac
  note "agent on $(K -n "$NS" get pod "$AGENT" -o jsonpath='{.spec.nodeName}')"
  # a first push to each arm creates walgit's repository and warms both
  local tip; tip=$(inpod "/work/build.sh seed 0" | tail -1)
  for arm in $ARMS; do
    local r; r=$(inpod "$(armenv "$arm") /work/tpush.sh seed $(arm_repo "$arm") agent/seed-$RUN" | tail -1)
    [ "${r##* }" = 0 ] && ok "$arm: seed push to agent/seed-$RUN told ok (${r%% *} ms); ls-remote $(inpod "$(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/seed-$RUN" | tail -1 | cut -c1-8)" || bad "$arm: seed push failed: $(inpod 'tail -2 /work/tpush.err')"
  done
}

# ── P1 ─────────────────────────────────────────────────────────────
leg_P1() {
  leg P1 "push latency at ${P1_SIZES} MiB, ${P1_REPS} reps, the same bytes to both arms, order alternating"
  local rep mb arm order r
  for mb in $P1_SIZES; do
    for rep in $(seq 1 "$P1_REPS"); do
      inpod "/work/build.sh p1-$mb-$rep $mb" >/dev/null 2>&1
      order=$ARMS; [ $((rep % 2)) -eq 0 ] && order=$(echo "$ARMS" | awk '{for(i=NF;i>0;i--) printf "%s ", $i}')
      for arm in $order; do
        r=$(inpod "$(armenv "$arm") /work/tpush.sh p1-$mb-$rep $(arm_repo "$arm") agent/p1-$mb-r$rep-$RUN" | tail -1)
        echo "$mb $rep $arm ${r%% *} ${r##* }" >> "$WORK/p1.txt"
        [ "${r##* }" = 0 ] || note "$arm ${mb} MiB rep $rep: rc ${r##* }: $(inpod 'tail -1 /work/tpush.err')"
      done
      inpod "rm -rf /work/p1-$mb-$rep" >/dev/null 2>&1
    done
    for arm in $ARMS; do
      local s; s=$(awk -v m="$mb" -v a="$arm" '$1==m && $3==a && $5==0 {print $4/1000}' "$WORK/p1.txt" | stats)
      note "$arm ${mb} MiB: median/min/max s = $s"
    done
    local nf; nf=$(awk -v m="$mb" '$1==m && $5!=0' "$WORK/p1.txt" | wc -l | tr -d ' ')
    [ "$nf" = 0 ] && ok "${mb} MiB: every push on both arms told ok" || bad "${mb} MiB: $nf push(es) failed"
  done
}

# ── P4 ─────────────────────────────────────────────────────────────
leg_P4() {
  leg P4 "two pushes to one ref from one base, concurrently: exactly one winner, the ref is the winner's tip"
  local arm base r rca rcb ta tb tip won
  for arm in $ARMS; do
    inpod "/work/build.sh p4 0" >/dev/null 2>&1
    base=$(inpod "$(armenv "$arm") /work/tpush.sh p4 $(arm_repo "$arm") agent/p4-$RUN >/dev/null; $(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/p4-$RUN" | tail -1)
    r=$(inpod "$(armenv "$arm") /work/race.sh $(arm_repo "$arm") agent/p4-$RUN $base" | tail -1)
    set -- $r; rca=$1; rcb=$2; ta=$3; tb=$4
    tip=$(inpod "$(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/p4-$RUN" | tail -1)
    won=$(( (rca==0) + (rcb==0) ))
    local wtip=""; [ $rca -eq 0 ] && wtip=$ta; [ $rcb -eq 0 ] && wtip=$tb
    if [ $won -eq 1 ] && [ "$tip" = "$wtip" ]; then
      ok "$arm: one winner (rc a=$rca b=$rcb), the ref is the winner's tip ${tip:0:8}; loser said: $(inpod "grep -h -o -i 'stale.*\|rejected.*\|fetch first.*' /work/ra.out /work/rb.out | head -1")"
    else bad "$arm: $won winner(s) (rc a=$rca b=$rcb), ref ${tip:0:8}, a ${ta:0:8}, b ${tb:0:8}"; fi
  done
}

# ── P9 ─────────────────────────────────────────────────────────────
leg_P9() {
  leg P9 "repack amplification: ${P9_N} pushes of ${P9_MB} MiB to one branch; bytes uploaded per arm from CloudWatch (cw-summary.sh)"
  local arm t0 t1 s nbad
  for arm in $ARMS; do
    t0=$(now)
    inpod "$(armenv "$arm") /work/seqpush.sh $(arm_repo "$arm") agent/p9-$RUN $P9_N $P9_MB $arm" >/dev/null 2>&1
    t1=$(now); window P9 "$arm" "$t0" "$t1"
    K -n "$NS" cp "$AGENT:/work/seq-$arm.log" "$WORK/p9-$arm.log" -c agent >/dev/null 2>&1
    s=$(awk '$3==0 {print $2/1000}' "$WORK/p9-$arm.log" | stats); nbad=$(awk '$3!=0' "$WORK/p9-$arm.log" | wc -l | tr -d ' ')
    [ "$nbad" = 0 ] && ok "$arm: ${P9_N} pushes told ok in $((t1-t0)) s; per push median/min/max s = $s; window $t0..$t1" || bad "$arm: $nbad of ${P9_N} pushes failed"
    note "$arm: objects under its prefix now: $(aws s3 ls "s3://$BUCKET/$([ "$arm" = forge ] && echo "$PREFIX" || echo "$WPREFIX")/" --recursive --summarize 2>/dev/null | grep -E 'Total (Objects|Size)' | tr '\n' ' ')"
  done
}

# ── P2 ─────────────────────────────────────────────────────────────
leg_P2() {
  leg P2 "push rate: ${P2_N} pushers for ${P2_SECS} s, tiny commits to distinct branches (requests per push from CloudWatch)"
  local arm t0 t1 i acks naks rate lat
  for arm in $ARMS; do
    inpod "rm -f /work/stop /work/rate-$arm-*.log" >/dev/null 2>&1
    t0=$(now)
    inpod "for i in \$(seq 1 $P2_N); do ( $(armenv "$arm") /work/pusher.sh $(arm_repo "$arm") \$i $P2_SECS $arm $RUN ) & done; wait; echo done" >/dev/null 2>&1
    t1=$(now); window P2 "$arm" "$t0" "$t1"
    inpod "cat /work/rate-$arm-*.log" > "$WORK/p2-$arm.log" 2>/dev/null
    acks=$(awk '$3==0' "$WORK/p2-$arm.log" | wc -l | tr -d ' '); naks=$(awk '$3!=0' "$WORK/p2-$arm.log" | wc -l | tr -d ' ')
    rate=$(python3 -c "print(f'{$acks/max($P2_SECS,1):.1f}')"); lat=$(awk '$3==0 {print $2/1000}' "$WORK/p2-$arm.log" | stats)
    [ "$acks" -gt 0 ] && ok "$arm: $acks acknowledged, $naks failed in ${P2_SECS} s = ${rate} pushes/s from ${P2_N} pushers; per-push latency median/min/max s = $lat" || bad "$arm: no push acknowledged"
  done
}

# ── P7 ─────────────────────────────────────────────────────────────
leg_P7() {
  leg P7 "${P7_N} concurrent clones of a ${P7_MB} MiB branch, one alone first, the serving processes watched"
  local arm tip r solo t0 t1 wall nok peak pw
  inpod "/work/build.sh p7 $P7_MB" >/dev/null 2>&1
  for arm in $ARMS; do
    r=$(inpod "$(armenv "$arm") /work/tpush.sh p7 $(arm_repo "$arm") agent/p7-$RUN" | tail -1)
    [ "${r##* }" = 0 ] || { inconc "$arm: the ${P7_MB} MiB branch did not land"; continue; }
    r=$(inpod "$(armenv "$arm") /work/clone.sh $(arm_repo "$arm") agent/p7-$RUN solo" | tail -1); solo=$(python3 -c "print(f'{${r%% *}/1000:.1f}')")
    up_watch "$arm" "$WORK/up-$arm"; pw=$!
    t0=$(now); inpod "$(armenv "$arm") /work/clones.sh $(arm_repo "$arm") agent/p7-$RUN $P7_N $arm" >/dev/null 2>&1; t1=$(now); wall=$((t1-t0))
    stop_watch "$WORK/up-$arm" "$pw"; peak=$(max_col "$WORK/up-$arm" 2)
    K -n "$NS" cp "$AGENT:/work/clone-$arm.log" "$WORK/p7-$arm.log" -c agent >/dev/null 2>&1
    nok=$(awk '$4==0' "$WORK/p7-$arm.log" | wc -l | tr -d ' ')
    [ "$nok" = "$P7_N" ] && ok "$arm: one clone alone ${solo} s; ${P7_N} concurrent all complete in ${wall} s ($(python3 -c "print(f'{$wall/max($solo,0.1):.1f}')")× solo, $(python3 -c "print(f'{$P7_MB*$P7_N/max($wall,1):.0f}')") MiB/s aggregate); peak serving processes ${peak:-n/a}" \
      || bad "$arm: $nok of ${P7_N} clones completed"
  done
  inpod "rm -rf /work/p7 /work/solo" >/dev/null 2>&1
}

# ── P5 ─────────────────────────────────────────────────────────────
leg_P5() {
  leg P5 "cold start: the arm's pod deleted (a fresh emptyDir), time to the first ls-remote and the first clone of the ${P7_MB} MiB branch"
  local arm pod t0 tls tcl r i tip
  for arm in $ARMS; do
    pod=$(arm_pod "$arm"); [ -n "$pod" ] || { inconc "$arm: no pod"; continue; }
    tip=$(inpod "$(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/p7-$RUN" | tail -1)
    [ -n "$tip" ] || { inconc "$arm: agent/p7-$RUN not present (run P7 first)"; continue; }
    K -n "$NS" delete pod "$pod" --wait=false >/dev/null 2>&1; t0=$(now)
    tls=""; for i in $(seq 1 600); do
      r=$(inpod "$(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/p7-$RUN" | tail -1)
      [ "$r" = "$tip" ] && { tls=$(( $(now) - t0 )); break; }; sleep 1; done
    [ -n "$tls" ] || { bad "$arm: no correct ls-remote within 600 s of the delete"; continue; }
    # Refs first is not packs first: an arm may answer ls-remote and still
    # refuse upload-pack while its packs are not local (walgit answers 503
    # + Retry-After, which git does not retry — run 1). So the clone is
    # retried every 5 s and the refusals are counted; the metric is the
    # time to the first clone that COMPLETES and is correct.
    local refused=0 rc=1 head="" tcl="" first_err=""
    for i in $(seq 1 120); do
      r=$(inpod "$(armenv "$arm") /work/clone.sh $(arm_repo "$arm") agent/p7-$RUN cold" | tail -1); set -- $r; rc=$2; head=$3
      if [ "$rc" = 0 ] && [ "$head" = "$tip" ]; then tcl=$(( $(now) - t0 )); break; fi
      refused=$((refused+1)); [ -n "$first_err" ] || first_err=$(inpod "grep -m1 -i 'error\|fatal' /work/clone.err" | head -1 | cut -c1-100)
      sleep 5
    done
    if [ -n "$tcl" ]; then
      ok "$arm: first correct ls-remote ${tls} s after the delete; first COMPLETE clone ${tcl} s after it ($(python3 -c "print(f'{$1/1000:.1f}')") s for the clone itself), after $refused refused attempt(s)${first_err:+ — first refusal: $first_err}"
    else bad "$arm: no complete clone within 600 s of the delete (ls-remote was correct at ${tls} s; $refused attempts; first refusal: $first_err)"; fi
    inpod "rm -rf /work/cold" >/dev/null 2>&1
  done
}

# ── P11 ────────────────────────────────────────────────────────────
leg_P11() {
  leg P11 "undo: a branch force-pushed back one commit; can the previous tip be recovered from the bucket?"
  local arm t1 t2 r seq got wp
  for arm in $ARMS; do
    inpod "/work/build.sh p11 0 >/dev/null; cd /work/p11 && echo two > two && git add two && git commit -qm two" >/dev/null 2>&1
    t2=$(inpod "cd /work/p11 && git rev-parse HEAD" | tail -1); t1=$(inpod "cd /work/p11 && git rev-parse HEAD~1" | tail -1)
    inpod "$(armenv "$arm") /work/tpush.sh p11 $(arm_repo "$arm") agent/p11-$RUN" >/dev/null 2>&1
    local seq_before=""
    [ "$arm" = walgit ] && seq_before=$(K -n "$NS" exec "$(walgit_pod)" -- walgit wal ls "acme/$WREPO" 2>/dev/null | grep -o -E '^[[:space:]]*[0-9]+' | tr -d ' ' | sort -n | tail -1)
    inpod ". /work/lib.sh; $(armenv "$arm") sh -c '. /work/lib.sh; cd /work/p11 && G push -q --force \"\$(url $(arm_repo "$arm"))\" $t1:refs/heads/agent/p11-$RUN'" >/dev/null 2>&1
    r=$(inpod "$(armenv "$arm") /work/lsremote.sh $(arm_repo "$arm") agent/p11-$RUN" | tail -1)
    [ "$r" = "$t1" ] || { inconc "$arm: the force-push did not land (${r:0:8})"; continue; }
    case "$arm" in
      walgit)
        wp=$(walgit_pod)
        # The sequence to recover is the head BEFORE the force-push (read
        # above); the output goes into the cache emptyDir — a container's
        # /tmp is its writable layer on the node's 8 GiB root, and run 1's
        # materialize there evicted both arms.
        # walgit writes the repository at <out>/acme/<repo>.git, not at
        # <out>: both runs' rigs looked at <out> and read nothing, and the
        # recovery was verified by hand after run 2 (README, results).
        local out="/var/lib/walgit/mat-$RUN" mat repo
        repo="$out/acme/$WREPO.git"
        K -n "$NS" exec "$wp" -- rm -rf "$out" >/dev/null 2>&1
        mat=$(K -n "$NS" exec "$wp" -- sh -c "walgit wal materialize acme/$WREPO --at-seq ${seq_before:-0} --out $out 2>&1 | tail -2; git -C $repo rev-parse refs/heads/agent/p11-$RUN 2>/dev/null || git -C $repo for-each-ref --format='%(refname) %(objectname)' 2>/dev/null | grep p11-$RUN | tail -1")
        got=$(echo "$mat" | grep -o -E '\b[0-9a-f]{40}\b' | tail -1)
        [ "$got" = "$t2" ] && ok "walgit: wal materialize --at-seq $seq_before recovers the pre-force tip ${t2:0:8}" \
          || note "walgit: materialize at seq ${seq_before:-?} gave ${got:-nothing} (wanted ${t2:0:8}): $(echo "$mat" | tr '\n' ' ' | cut -c1-200)"
        K -n "$NS" exec "$wp" -- rm -rf "$out" >/dev/null 2>&1;;
      forge)
        # X15: a destructive push keeps an immutable copy of the
        # snapshot it replaced under <prefix>/git/undo/<seq>.json, and
        # the sweep treats the packs that copy names as referenced. The
        # recovery this leg asks about is therefore: does the bucket
        # still say what the branch was, and are the packs that hold it
        # still there? Both are read from the bucket alone, with no
        # server in the picture.
        local ukey uoid upacks missing=0
        ukey=$(aws s3 ls "s3://$BUCKET/$PREFIX/$FREPO/git/undo/" 2>/dev/null | awk '{print $4}' | sed 's/\.json$//' | sort -n | tail -1)
        if [ -z "$ukey" ]; then
          note "forge: no undo point under $PREFIX/$FREPO/git/undo/ — is FLINT_FORGE_UNDO_WINDOW_SECS 0?"
        else
          aws s3 cp "s3://$BUCKET/$PREFIX/$FREPO/git/undo/$ukey.json" "$WORK/undo-$ukey.json" >/dev/null 2>&1
          uoid=$(python3 -c "import json,sys; d=json.load(open('$WORK/undo-$ukey.json')); print(d['refs'].get('refs/heads/agent/p11-$RUN',''))" 2>/dev/null)
          upacks=$(python3 -c "import json; print(' '.join(json.load(open('$WORK/undo-$ukey.json'))['packs']))" 2>/dev/null)
          for pk in $upacks; do
            aws s3api head-object --bucket "$BUCKET" --key "$PREFIX/$FREPO/git/objects/pack/$pk" >/dev/null 2>&1 || missing=$((missing+1))
          done
          if [ "$uoid" = "$t2" ] && [ "$missing" -eq 0 ]; then
            ok "forge: undo point seq $ukey holds the pre-force tip ${t2:0:8}, and every pack it names is still in the bucket ($(echo "$upacks" | wc -w | tr -d ' ') of them)"
          else
            note "forge: undo point seq ${ukey:-none} gave ${uoid:-nothing} (wanted ${t2:0:8}), $missing pack(s) missing"
          fi
        fi;;
    esac
  done
}

# ── P10 ────────────────────────────────────────────────────────────
leg_P10() {
  leg P10 "the bucket cut off from the arm's pod: reads at +5 s, readiness over 90 s, a push, recovery (recorded, not scored)"
  local arm sel pod r0 t0 rd ready_at i r rc
  for arm in $ARMS; do
    pod=$(arm_pod "$arm"); case "$arm" in forge) sel="chert.us/repo: $FREPO";; walgit) sel="app: walgit";; esac
    # The cut is judged against a SERVING arm: run 1 cut forge while its
    # syncer was mid-repack (readiness already withdrawn for that), and
    # the "not ready at 16 s" it recorded meant nothing.
    if [ "$arm" = forge ]; then
      local ph; for i in $(seq 1 60); do ph=$(forge_phase); [ "$ph" = serving ] && break; sleep 5; done
      [ "$ph" = serving ] || { inconc "forge is $ph, not serving, before the cut"; continue; }
    fi
    [ "$(pod_ready "$pod")" = True ] || { inconc "$arm: pod $pod not ready before the cut"; continue; }
    r0=$(restarts "$pod")
    cat <<YAML | K apply -f - >/dev/null
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: { name: p10-$arm, namespace: $NS }
spec:
  podSelector: { matchLabels: { $sel } }
  policyTypes: [Egress]
  egress:
    - to: [ { ipBlock: { cidr: "10.0.0.0/8" } } ]
    - ports: [ { protocol: UDP, port: 53 }, { protocol: TCP, port: 53 } ]
YAML
    t0=$(now); sleep 5
    r=$(inpod "$(armenv "$arm") timeout 60 sh /work/lsremote.sh $(arm_repo "$arm") agent/seed-$RUN; echo rc=\$?" | tail -1)
    note "$arm +5 s: a read during the outage → $r"
    ready_at=""; for i in $(seq 1 18); do rd=$(pod_ready "$pod"); [ "$rd" != True ] && { ready_at="not ready after $(( $(now) - t0 )) s (restarts $r0 → $(restarts "$pod"))"; break; }; sleep 5; done
    note "$arm readiness: ${ready_at:-still ready after 90 s (restarts $r0 → $(restarts "$pod"))}"
    inpod "/work/build.sh p10 0" >/dev/null 2>&1
    r=$(inpod "$(armenv "$arm") timeout 120 sh /work/tpush.sh p10 $(arm_repo "$arm") agent/p10-$arm-$RUN; echo" | tail -2 | head -1)
    note "$arm: a push during the outage → ${r:-timed out}"
    K -n "$NS" delete networkpolicy "p10-$arm" >/dev/null 2>&1
    for i in $(seq 1 120); do pod=$(arm_pod "$arm"); [ "$(pod_ready "$pod")" = True ] && break; sleep 2; done
    r=$(inpod "$(armenv "$arm") /work/tpush.sh p10 $(arm_repo "$arm") agent/p10-$arm-$RUN" | tail -1)
    [ "${r##* }" = 0 ] && ok "$arm: recovers once the bucket returns (push told ok, pod ready, restarts $r0 → $(restarts "$pod"))" || bad "$arm: did not recover: $r"
  done
}

# ── run ────────────────────────────────────────────────────────────
echo "compare $RUN · bucket $BUCKET · forge $PREFIX · walgit $WPREFIX · arms: $ARMS · legs: $LEGS"
for l in $LEGS; do "leg_$l"; done
printf '\n══ verdict ══\nPASS %s  FAIL %s  INCONCLUSIVE %s\nwindows for cw-summary.sh: %s/windows.txt\n' "$PASS" "$FAIL" "$INCONC" "$WORK"
[ $FAIL -eq 0 ] && [ $INCONC -eq 0 ] && exit 0; [ $FAIL -eq 0 ] && exit 2; exit 1
