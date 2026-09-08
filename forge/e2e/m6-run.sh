#!/usr/bin/env bash
# M6 — fold amplification on the wire. The measurement `6bc67980` ends by
# declaring owed: every figure behind the tiers' byte work comes from
# `foldsim.py` replaying a bucket listing, never from a cluster.
#
# TWO ARMS, ONE KNOB. Both are forge FlintRepos on one bucket, one image
# and one node; `syncerEnv` sets FLINT_FORGE_FOLD_MIN_MIB to 0 on
# `m6-before` and the shipped 256 on `m6-after`. `foldsim` shows the
# floor alone reproduces the entire pre-6bc67980 rule, so the knob IS the
# dimension and not a stand-in.
#
#   KUBECONFIG=... BUCKET=... PREFIX=... ./forge/e2e/m6-run.sh
#
# Pre-registered (verdict doc §5 M6):
#   P9  before/after >= 1.10x   (foldsim: 3.56x -> 1.67x = 2.13x)
#   P2  before/after >= 1.30x   (foldsim: 5.65x -> 3.08x = 1.83x)
#   falsifier: P2 arms within 0.10x  => foldsim does not transfer; stop.
#   falsifier: the legs disagree in DIRECTION => shape-specific, neither
#              number describes the fix.
set -uo pipefail
NS=${NS:-agents}; AGENT=${AGENT:-agent1}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?}"; : "${PREFIX:?}"
ARMS=${ARMS:-"m6-after m6-before"}
PAIRS=${PAIRS:-3}
# Which measurement legs run. P2 shares a repository with P9 by default,
# and P9's content is what P2's base rebuild has to re-upload — so
# LEGS=P2 on its own repository pair is the CONTROL for that confound.
LEGS=${LEGS:-"P9 P2"}
P9_N=${P9_N:-48}; P9_MB=${P9_MB:-8}
P2_N=${P2_N:-32}; P2_SECS=${P2_SECS:-60}
# DERIVED, never hardcoded: the walgit rig's 9899 is not this chart's
# port (it renders 9848 via FLINT_FORGE_STATUS_ADDR), and a wrong port
# reads as "the field is missing" rather than "I asked the wrong place".
status_port() { K -n "$NS" get deploy "forge-$1" -o jsonpath='{.spec.template.spec.containers[?(@.name=="syncer")].ports[?(@.name=="status")].containerPort}' 2>/dev/null; }
WORK=${WORK:-$(mktemp -d)}
# A per-invocation stamp in every tag and ref. Without it a re-run reuses
# the log name AND the ref of the last one: two seqpush processes then
# append to one file and push the same ref, every push is rejected
# non-fast-forward, and the leg reads a log that is a mixture of two
# runs. That happened here — killing the local driver does NOT kill the
# work already dispatched into the pod.
RUN=${RUN:-$(date +%s)}
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }
leg()    { printf '\n== %s ==\n' "$*"; }
inpod()  { K -n "$NS" exec "$AGENT" -c agent -- sh -c "$*" 2>&1; }
put_script() { K -n "$NS" exec -i "$AGENT" -c agent -- sh -c "cat > /work/$1 && chmod +x /work/$1"; }
armenv() { echo "DOOR=$DOOR NS=$NS"; }
arm_pod() { K -n "$NS" get pods -l "chert.us/repo=$1" -o json 2>/dev/null \
  | jq -r '[.items[] | select(.metadata.deletionTimestamp == null)] | sort_by(.metadata.creationTimestamp) | last | .metadata.name // empty'; }
arm_status() { local p port; p=$(arm_pod "$1"); port=$(status_port "$1")
  [ -n "$p" ] && [ -n "$port" ] && \
  K -n "$NS" exec "$p" -c syncer -- wget -qO- "http://127.0.0.1:$port/status" 2>/dev/null; }
arm_folds() { arm_status "$1" | jq -r '.foldsCommitted // empty'; }
# The bytes track BASE REBUILDS, not fold count: over M6's three pairs the
# floored arm did 24 folds / 4 rebuilds and the unfloored one 187 folds / 2.
# Recording only foldsCommitted made the expensive event invisible.
arm_bases() { arm_status "$1" | jq -r '.baseRebuilds // empty'; }
arm_floor() { local p; p=$(arm_pod "$1"); [ -n "$p" ] && \
  K -n "$NS" exec "$p" -c syncer -- sh -c 'echo ${FLINT_FORGE_FOLD_MIN_MIB:-unset}' 2>/dev/null; }

# The CONTROL PLANE is spot. A reclaimed CP makes every never-observed
# oracle "pass" silently, so a leg that cannot see the API server is VOID,
# not green. Checked either side of every leg.
cp_alive() { K get --raw /readyz >/dev/null 2>&1 && echo yes || echo NO; }

# Bytes UPLOADED under an arm's prefix since the last call, by diffing
# pack keys against a seen-file. NOT the resident delta, which the sweeps
# make an undercount; NOT CloudWatch, which cannot resolve a 35 s leg and
# has already been contradicted by this bucket once.
# A LISTING THAT FAILS MUST NOT READ AS "NO NEW BYTES". This function
# used to end the aws call with `2>/dev/null` and no status check, so a
# NoCredentials error produced an empty stream and the function returned
# 0 — the same value it returns when genuinely nothing was uploaded. The
# P2-isolated control ran that way and reported 0.0 MiB on both arms with
# 7,074 keys actually in the bucket. Every byte figure in it was void and
# nothing in the run said so.
uploaded_since() { # <arm> <seen-file> -> bytes, or "ERR" and rc 1
  local arm=$1 seen=$2 total=0 key size rc
  touch "$seen"
  local raw="$WORK/.ls-$arm.$$"
  aws s3api list-objects-v2 --bucket "$BUCKET" \
      --prefix "$PREFIX/$arm/git/objects/pack/" \
      --query 'Contents[].[Key,Size]' --output text > "$raw" 2>"$raw.err"
  rc=$?
  # A pipeline's exit status is the LAST stage's, so the listing is run
  # on its own line and $? captured before anything else touches it.
  if [ $rc -ne 0 ]; then
    echo "ERR"; rm -f "$raw" "$raw.err"; return 1
  fi
  while read -r key size; do
    [ -n "$key" ] || continue
    if ! grep -qxF "$key" "$seen"; then echo "$key" >> "$seen"; total=$((total + size)); fi
  done < <(grep -v '^None' "$raw")
  rm -f "$raw" "$raw.err"
  echo "$total"
}

install_scripts() {
  put_script lib.sh <<'EOS'
auth() { T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf 'x:%s' "$T" | base64 -w0)"; }
G() { auth; git -c http.extraHeader="$A" "$@"; }
url() { echo "$DOOR/git/$NS/$1.git"; }
ms() { awk '{printf "%d", $1*1000}' /proc/uptime; }
EOS
  put_script seqpush.sh <<'EOS'
# seqpush.sh <repo> <ref> <n> <mb> <tag>
. /work/lib.sh
repo=$1; ref=$2; n=$3; mb=$4; tag=$5; d=/work/seq-$tag; log=/work/seq-$tag.log
rm -rf "$d" "$log"; mkdir -p "$d"; cd "$d"
git init -q -b main; git config user.email seq@invalid; git config user.name seq; git config core.compression 0
git config pack.window 0; git config pack.depth 0
i=1; while [ $i -le $n ]; do
  dd if=/dev/urandom of="blob$i" bs=1M count=$mb status=none; git add -A >/dev/null; git commit -qm "c$i" >/dev/null
  t0=$(ms); G push -q "$(url "$repo")" "HEAD:refs/heads/$ref" >/dev/null 2>&1; rc=$?; t1=$(ms)
  echo "$i $((t1-t0)) $rc" >> "$log"; i=$((i+1)); done
echo done
EOS
  put_script pusher.sh <<'EOS'
# pusher.sh <repo> <i> <secs> <tag>
. /work/lib.sh
repo=$1; i=$2; secs=$3; tag=$4; d=/work/rate-$tag-$i; log=/work/rate-$tag-$i.log
rm -rf "$d" "$log"; mkdir -p "$d"; cd "$d"
git init -q -b main; git config user.email r$i@invalid; git config user.name r$i
echo 0 > f; git add f; git commit -qm init >/dev/null
end=$(( $(ms) + secs*1000 )); n=0
while [ "$(ms)" -lt "$end" ]; do
  n=$((n+1)); echo "$n $(ms)" >> f; git commit -qam "c$n" >/dev/null
  t0=$(ms); G push -q "$(url "$repo")" "HEAD:refs/heads/agent/p2-$tag-$i" >/dev/null 2>&1; rc=$?; t1=$(ms)
  echo "$t0 $((t1-t0)) $rc" >> "$log"
done
EOS
}

# ── P0 ─────────────────────────────────────────────────────────────
# MANDATORY and FIRST. On runce the P9 leg passed while measuring
# nothing, because P0 — the leg that installs seqpush.sh — had not run.
leg_P0() {
  leg "P0 preconditions: both arms answer, the arm assignment is READ not assumed, scripts installed"
  [ "$(cp_alive)" = yes ] && ok "control plane answers /readyz" || { bad "control plane is not answering — every leg below would be VOID"; return 1; }
  K -n "$NS" get pod "$AGENT" -o jsonpath='{.status.phase}' 2>/dev/null | grep -q Running \
    && ok "agent $AGENT is Running" || { bad "agent is not Running"; return 1; }
  inpod 'awk "{print \$1}" /proc/uptime' >/dev/null 2>&1 \
    && ok "agent clock has sub-second resolution (/proc/uptime; busybox date has no %N)" \
    || bad "no sub-second clock in the agent"
  install_scripts; ok "push scripts installed on the agent"

  local arm floor folds seen
  for arm in $ARMS; do
    floor=$(arm_floor "$arm"); folds=$(arm_folds "$arm")
    note "$arm: pod $(arm_pod "$arm"), FLINT_FORGE_FOLD_MIN_MIB=$floor, foldsCommitted=$folds"
    [ -n "$folds" ] || { bad "$arm: /status does not report foldsCommitted — no leg below can be scored"; return 1; }
    inpod "export $(armenv); . /work/lib.sh; G ls-remote \"\$(url $arm)\" >/dev/null 2>&1; echo rc=\$?" | grep -q rc=0 \
      && ok "$arm answers ls-remote through the door" || bad "$arm does not answer ls-remote"
  done
  # THE ARM ASSIGNMENT IS THE EXPERIMENT. Two arms that turn out to carry
  # the same floor are one arm, and every ratio below would be 1.00x for
  # a reason that has nothing to do with the fix.
  # ...and it must be read off THE ARMS THIS RUN USES. These two lines
  # named m6-after/m6-before literally. Under ARMS="m6p2-after m6p2-before"
  # that still PASSED — by reading the floors of the previous run's repos,
  # which were still in the cluster. The check that calls itself the
  # experiment was reporting on repositories the run never touched.
  local a b fa fb
  set -- $ARMS; a=$1; b=$2
  fa=$(arm_floor "$a"); fb=$(arm_floor "$b")
  if [ "$fa" = "256" ] && [ "$fb" = "0" ]; then
    ok "the arms differ in the ONE variable under test: $a=256 MiB, $b=0"
  else
    bad "arm assignment is wrong ($a=$fa $b=$fb) — the arms are not the experiment"; return 1
  fi

  # THE BYTE ORACLE MUST BE PROVEN TO WORK BEFORE ANY LEG SCORES BYTES.
  # `uploaded_since` returned 0 on a credential failure for a whole run;
  # 0 is also its answer when nothing was uploaded, so the run looked
  # merely uninteresting instead of broken. Prove the listing answers,
  # and prove it answers FOR THIS PREFIX.
  for arm in $ARMS; do
    seen=$(mktemp)
    if ! uploaded_since "$arm" "$seen" >/dev/null; then
      bad "$arm: cannot list s3://$BUCKET/$PREFIX/$arm/ — every byte figure would be a silent 0 (AWS_PROFILE set?)"
      rm -f "$seen"; return 1
    fi
    rm -f "$seen"
  done
  ok "the bucket listing answers for every arm — byte figures can be scored"
}

# ── P9 — the LADDER ────────────────────────────────────────────────
leg_P9() { # <pair>
  local pair=$1 arm t0 t1 up f0 f1 nbad ngood pushed
  leg "P9 pair $pair: $P9_N sequential pushes of $P9_MB MiB — the ladder (foldsim: 3.56x -> 1.67x)"
  for arm in $(order "$pair"); do
    [ "$(cp_alive)" = yes ] || { bad "CP gone before $arm — VOID"; return 1; }
    uploaded_since "$arm" "$WORK/seen-p9-$pair-$arm" >/dev/null \
      || { bad "$arm: baseline listing FAILED — an empty seen-file would score the WHOLE repo as new"; continue; }
    f0=$(arm_folds "$arm"); t0=$(date +%s)
    inpod "export $(armenv); /work/seqpush.sh $arm agent/p9-$pair-$RUN $P9_N $P9_MB $arm-$pair-$RUN" >/dev/null 2>&1
    t1=$(date +%s); f1=$(arm_folds "$arm")
    K -n "$NS" cp "$AGENT:/work/seq-$arm-$pair-$RUN.log" "$WORK/p9-$pair-$arm.log" -c agent >/dev/null 2>&1
    if [ ! -s "$WORK/p9-$pair-$arm.log" ]; then inconc "$arm: no per-push log — nothing measured (did P0 run?)"; continue; fi
    ngood=$(awk '$3==0' "$WORK/p9-$pair-$arm.log" | wc -l | tr -d ' ')
    nbad=$(awk '$3!=0' "$WORK/p9-$pair-$arm.log" | wc -l | tr -d ' ')
    [ "$nbad" = 0 ] || { bad "$arm: $nbad of $P9_N pushes failed"; continue; }
    [ "$ngood" = "$P9_N" ] || { bad "$arm: only $ngood of $P9_N pushes in the log"; continue; }
    up=$(uploaded_since "$arm" "$WORK/seen-p9-$pair-$arm")
    [ "$up" = ERR ] && { bad "$arm: bucket listing FAILED — the bytes for this leg are void, not zero"; continue; }
    pushed=$((P9_N * P9_MB * 1048576))
    echo "$pair $arm $up $pushed $((f1-f0)) $((t1-t0))" >> "$WORK/p9.tsv"
    note "$arm: uploaded $(python3 -c "print(f'{$up/1048576:.0f}')") MiB for $(python3 -c "print(f'{$pushed/1048576:.0f}')") MiB pushed = $(python3 -c "print(f'{$up/max($pushed,1):.2f}')")x in $((t1-t0)) s; folds $((f1-f0))"
    [ $((f1-f0)) -ge 1 ] || inconc "$arm: NO fold committed — the ladder never ran and this ratio scores nothing"
  done
}

# ── P2 — the FLOOR ─────────────────────────────────────────────────
leg_P2() { # <pair>
  local pair=$1 arm t0 t1 up f0 f1 acks
  leg "P2 pair $pair: $P2_N pushers x ${P2_SECS}s of tiny commits — the floor (foldsim: 5.65x -> 3.08x)"
  for arm in $(order "$pair"); do
    [ "$(cp_alive)" = yes ] || { bad "CP gone before $arm — VOID"; return 1; }
    uploaded_since "$arm" "$WORK/seen-p2-$pair-$arm" >/dev/null \
      || { bad "$arm: baseline listing FAILED — an empty seen-file would score the WHOLE repo as new"; continue; }
    f0=$(arm_folds "$arm"); b0=$(arm_bases "$arm"); t0=$(date +%s)
    inpod "export $(armenv); for i in \$(seq 1 $P2_N); do ( /work/pusher.sh $arm \$i $P2_SECS $arm-$pair-$RUN ) & done; wait; echo done" >/dev/null 2>&1
    t1=$(date +%s); f1=$(arm_folds "$arm"); b1=$(arm_bases "$arm")
    inpod "cat /work/rate-$arm-$pair-$RUN-*.log" > "$WORK/p2-$pair-$arm.log" 2>/dev/null
    acks=$(awk '$3==0' "$WORK/p2-$pair-$arm.log" 2>/dev/null | wc -l | tr -d ' ')
    [ "${acks:-0}" -gt 0 ] || { bad "$arm: no push acknowledged"; continue; }
    up=$(uploaded_since "$arm" "$WORK/seen-p2-$pair-$arm")
    [ "$up" = ERR ] && { bad "$arm: bucket listing FAILED — the bytes for this leg are void, not zero"; continue; }
    echo "$pair $arm $up $acks $((f1-f0)) $((t1-t0)) $((b1-b0))" >> "$WORK/p2.tsv"
    note "$arm: $acks acks in $((t1-t0)) s = $(python3 -c "print(f'{$acks/max($((t1-t0)),1):.1f}')")/s; uploaded $(python3 -c "print(f'{$up/1048576:.1f}')") MiB = $(python3 -c "print(f'{$up/max($acks,1)/1024:.1f}')") KiB/push; folds $((f1-f0)), base rebuilds $((b1-b0))"
    [ $((f1-f0)) -ge 1 ] || inconc "$arm: NO fold committed during P2 — the bytes score nothing"
  done
}

# Alternating order: runce's byte regression was LEG ORDER, and that log
# had to be corrected in place. Odd pairs run after-first, even before-first.
order() { set -- $ARMS $1; if [ $(( $3 % 2 )) -eq 1 ]; then echo "$1 $2"; else echo "$2 $1"; fi; }

main() {
  echo "M6 — bucket=$BUCKET prefix=$PREFIX pairs=$PAIRS work=$WORK"
  leg_P0
  if [ "$FAIL" -ne 0 ]; then
    echo; echo "P0 recorded $FAIL failure(s) — refusing to run the measurement legs."
    echo "A precondition leg that reports FAIL and runs on anyway is the"
    echo "failure this drill exists to avoid: every ratio below it would be"
    echo "scored against a rig that was already known to be wrong."
    exit 1
  fi
  local p
  for p in $(seq 1 "$PAIRS"); do
    case " $LEGS " in *" P9 "*) leg_P9 "$p";; esac
    case " $LEGS " in *" P2 "*) leg_P2 "$p";; esac
  done
  echo
  echo "== RESULT =="
  [ "$(cp_alive)" = yes ] || echo "  *** CONTROL PLANE GONE — THIS RUN IS VOID, NOT GREEN ***"
  python3 - "$WORK" <<'PY'
import sys, pathlib
w = pathlib.Path(sys.argv[1])
# P9 divides uploaded by PUSHED BYTES, so its figure is a dimensionless
# amplification and "x" is honest. P2 divides by ACKS, so its figure is
# bytes per push — printing that with an "x" suffix (as this summary did)
# reads as a ratio and invites comparing 663521.73 against a 1.30x bar.
LEGS = (("p9", "uploaded / pushed-bytes", "x", 1.0),
        ("p2", "uploaded / ack",          " KiB/push", 1024.0))
for leg, unit, suffix, div in LEGS:
    f = w / f"{leg}.tsv"
    if not f.exists(): print(f"  {leg.upper()}: no rows"); continue
    rows = [l.split() for l in f.read_text().split("\n") if l.strip()]
    by = {}
    for r in rows:
        pair, arm, up, denom, folds = r[0], r[1], int(r[2]), int(r[3]), int(r[4])
        bases = int(r[6]) if len(r) > 6 else None
        by.setdefault(arm, []).append((up/max(denom,1)/div, folds, bases, up))
    print(f"  {leg.upper()} ({unit}) — ranges, never means:")
    for arm, v in sorted(by.items()):
        r = [x[0] for x in v]; fo = [x[1] for x in v]
        bs = [x[2] for x in v if x[2] is not None]
        mib = [x[3]/1048576 for x in v]
        extra = f"   base rebuilds {min(bs)}..{max(bs)}" if bs else ""
        print(f"    {arm:<12} n={len(r)}  {min(r):.2f}{suffix} .. {max(r):.2f}{suffix}"
              f"   [{min(mib):.1f}..{max(mib):.1f} MiB]   folds {min(fo)}..{max(fo)}{extra}")
    aft = [k for k in by if "after"  in k]
    bef = [k for k in by if "before" in k]
    if len(aft) == 1 and len(bef) == 1:
        ra = [x[0] for x in by[aft[0]]]; rb = [x[0] for x in by[bef[0]]]
        print(f"    before/after: {min(rb)/max(ra):.2f}x .. {max(rb)/min(ra):.2f}x")
        if max(ra) >= min(rb):
            print("    *** the ranges OVERLAP — no separation at this n ***")
        if max(rb) < min(ra):
            print("    *** INVERTED — the floored arm costs MORE, every pair ***")
PY
  echo
  echo "  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC   (INCONCLUSIVE is not PASS)"
  echo "  rows: $WORK/p9.tsv $WORK/p2.tsv"
}
main "$@"
