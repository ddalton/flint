#!/bin/bash
# rig-selftest.sh — offline proof that drive-ab.sh's guards actually fire.
#
# A measurement rig earns trust the same way a drill leg does: by being seen
# to FAIL when the thing it checks is broken. Every guard here gets a negative
# leg that violates exactly one precondition and asserts the specific void
# reason. The first leg is the anchor — if the happy path did not come back
# clean, a rig that voided everything unconditionally would pass all the
# negative legs and prove nothing.
#
# No cluster, no AWS, no network: kubectl and aws are faked on PATH and the
# fake node returns scripted timings. Run: ./rig-selftest.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DRIVER="$HERE/drive-ab.sh"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n     %s\n' "$1" "${2:-}"; }

FLINT_IP=10.0.0.1
S3_IP=10.0.0.2

mkfakes() { # $1 = workdir
  local w=$1; mkdir -p "$w/bin" "$w/state"
  cp "$DRIVER" "$w/drive-ab.sh"   # $HERE becomes $w: .pushed-digest and the
  chmod +x "$w/drive-ab.sh"       # stripe-width gate resolve inside the fake
  : > "$w/state/reqs_flint"; : > "$w/state/reqs_s3"
  echo 0 > "$w/state/reqs_flint"; echo 0 > "$w/state/reqs_s3"

  cat > "$w/bin/kubectl" <<'KUBECTL'
#!/bin/bash
# fake kubectl — answers only what drive-ab.sh asks, from $FKSTATE.
args="$*"
case "$args" in
  *"get nodes -l oci-ab/role=client"*) echo "${FK_NODE:-runbz-aws-4}";;
  *"get svc registry-flint"*)          echo "${FK_FLINT_IP:-10.0.0.1}";;
  *"get svc registry-s3"*)             echo "${FK_S3_IP:-10.0.0.2}";;
  *"deploy/flint-pnfs-mds"*"ds-count"*) echo "${FK_DS_WANT:-3}";;
  *"get pods -l app=flint-pnfs-ds"*)
      i=0; while [ $i -lt "${FK_DS_RUNNING:-3}" ]; do echo Running; i=$((i+1)); done
      i=0; while [ $i -lt "${FK_DS_PENDING:-0}" ]; do echo Pending; i=$((i+1)); done;;
  *"logs deploy/flint-pnfs-mds"*"--since-time"*)
      echo "DEBUG      Number of DSes in stripe: 3";;
  *"logs deploy/flint-pnfs-mds"*"--since=10m"*)
      [ "${FK_MDS_DEBUG:-1}" = 1 ] && echo "DEBUG      Number of DSes in stripe: 3";;
  *"logs deploy/flint-pnfs-mds"*)
      i=0; while [ $i -lt "${FK_REJECTIONS:-0}" ]; do echo "WARN DS registration rejected"; i=$((i+1)); done;;
  *"logs deploy/registry-flint"*)
      n=$(cat "$FKSTATE/reqs_flint"); i=0
      while [ $i -lt "$n" ]; do echo '10.0.0.9 - - [x] "GET /v2/python/blobs/sha256:ab HTTP/1.1" 200 123'; i=$((i+1)); done;;
  *"logs deploy/registry-s3"*)
      n=$(cat "$FKSTATE/reqs_s3"); i=0
      while [ $i -lt "$n" ]; do echo '10.0.0.9 - - [x] "GET /v2/python/blobs/sha256:ab HTTP/1.1" 200 123'; i=$((i+1)); done;;
  *) : ;;
esac
exit 0
KUBECTL

  cat > "$w/bin/aws" <<'AWSFAKE'
#!/bin/bash
# fake aws — ec2 describe-instances + ssm send/get. Decodes the b64 payload
# drive-ab.sh ships and dispatches on what the command actually does, so the
# fake node's answers depend on the real command text.
sub="$1 $2"
case "$sub" in
"ec2 describe-instances")
    echo "${FK_IID:-i-0fake}"; exit 0;;
"ssm send-command")
    # pull the b64 out of --parameters commands="echo <b64> | base64 -d | bash"
    payload=""
    for a in "$@"; do case "$a" in commands=*) payload="${a#commands=}";; esac; done
    b64=$(echo "$payload" | sed -n 's/^echo \([A-Za-z0-9+/=]*\) .*/\1/p')
    cid="cmd-$$-$RANDOM"
    echo "$b64" | base64 -d > "$FKSTATE/$cid.cmd" 2>/dev/null || : > "$FKSTATE/$cid.cmd"
    echo "$cid"; exit 0;;
"ssm get-command-invocation")
    cid=""; query=""
    prev=""
    for a in "$@"; do
      case "$prev" in --command-id) cid="$a";; --query) query="$a";; esac
      prev="$a"
    done
    cmd=$(cat "$FKSTATE/$cid.cmd" 2>/dev/null)
    kind=other
    case "$cmd" in
      *"/proc/loadavg"*)   kind=idle;;
      *"system prune"*)    kind=cold;;
      *"pull --quiet"*)    kind=pull;;
    esac
    if [ "$query" = Status ]; then
      # FK_SSM_FAIL_ON names the ONE command kind that fails, so a leg can
      # break the pull without also breaking the cold step ahead of it.
      if [ "${FK_SSM_FAIL_ON:-none}" = "$kind" ]; then echo Failed; else echo "${FK_SSM_STATUS:-Success}"; fi
      exit 0
    fi
    if [ "$query" = StandardErrorContent ]; then echo ""; exit 0; fi
    case $kind in
      idle) echo "${FK_LOADAVG:-0.20}"; echo "${FK_NPROC:-4}";;
      cold) echo "#RIG images=${FK_IMAGES_AFTER_PRUNE:-0} socistate=absent soci=${FK_SOCI:-active}";;
      pull)
        # model backend traffic: the arm's own registry gets FK_REQS_OWN,
        # the other gets FK_REQS_LEAK (0 unless a leg is testing attribution)
        own=${FK_REQS_OWN:-42}; leak=${FK_REQS_LEAK:-0}
        case "$cmd" in
          *"${FK_FLINT_IP:-10.0.0.1}:5000"*)
             echo $(( $(cat "$FKSTATE/reqs_flint") + own )) > "$FKSTATE/reqs_flint"
             echo $(( $(cat "$FKSTATE/reqs_s3")   + leak )) > "$FKSTATE/reqs_s3";;
          *"${FK_S3_IP:-10.0.0.2}:5000"*)
             echo $(( $(cat "$FKSTATE/reqs_s3")    + own )) > "$FKSTATE/reqs_s3"
             echo $(( $(cat "$FKSTATE/reqs_flint") + leak )) > "$FKSTATE/reqs_flint";;
        esac
        echo "#RIG pull_ms=${FK_PULL_MS:-900} ready_ms=${FK_READY_MS:-1200} prc=${FK_PULL_RC:-0} rrc=${FK_RUN_RC:-0} digest=${FK_DIGEST:-sha256:aaaa}";;
      *) echo "";;
    esac
    exit 0;;
esac
exit 0
AWSFAKE
  chmod +x "$w/bin/kubectl" "$w/bin/aws"
}

# run the driver under the fakes; $1=workdir, rest=driver args. Extra env via FKENV.
drive() {
  local w=$1; shift
  ( export FKSTATE="$w/state" PATH="$w/bin:$PATH" KC=/dev/null CLUSTER=runbz \
           DS_SETTLE_TRIES=${DS_SETTLE_TRIES:-1} REPS=${REPS:-1} \
           FK_FLINT_IP=$FLINT_IP FK_S3_IP=$S3_IP
    eval "${FKENV:-}"
    "$w/drive-ab.sh" "$@" )
}

newdir() { local d; d=$(mktemp -d); mkfakes "$d"; echo "$d"; }

echo "── drive-ab.sh rig self-test ─────────────────────────────────────"

# ── LEG 1 (ANCHOR): clean run produces valid NDJSON, all arms valid ──────
# Without this leg every negative leg below is vacuous: a rig that voided
# unconditionally would "pass" all of them.
W=$(newdir); FKENV='' out=$(drive "$W" run 2>/dev/null | tail -1)
if [ -n "$out" ] && [ -f "$out" ]; then
  lines=$(wc -l < "$out" | tr -d ' ')
  if python3 -c "
import json,sys
all_rows=[json.loads(l) for l in open('$out') if l.strip()]
rows=[r for r in all_rows if r.get('record')!='substrate']
sub=[r for r in all_rows if r.get('record')=='substrate']
assert len(sub)==1, f'expected exactly one substrate verdict row, got {len(sub)}'
assert len(rows)==3, f'expected 3 arms, got {len(rows)}'
assert all(r['valid'] for r in rows), [r for r in rows if not r['valid']]
assert all(r['ready_ms']==1200 for r in rows), rows
assert sorted(r['arm'] for r in rows)==['A1','A3','A5'], rows
" 2>/tmp/leg1.err; then ok "clean run: 3 arms all valid + 1 substrate verdict row, parseable NDJSON ($lines lines)"
  else bad "clean run" "$(cat /tmp/leg1.err)"; fi
else bad "clean run" "no results file produced"; fi

# v1 regression: prove the output is JSON at all (v1 piped SSM stdout into
# json.load, so `run` could not emit a parseable line).
if [ -n "${out:-}" ] && [ -f "${out:-/nonexistent}" ] && python3 -c "
import json;[json.loads(l) for l in open('$out') if l.strip()]" 2>/dev/null; then
  ok "v1 defect #1 closed: every emitted line is valid JSON"
else bad "v1 defect #1" "output still unparseable"; fi

# ── negative legs: each violates ONE precondition ────────────────────────
expect_void() { # $1=label $2=env $3=substring expected in a void reason
  local w; w=$(newdir)
  local o; o=$(FKENV="$2" drive "$w" run 2>/dev/null | tail -1)
  if [ -f "${o:-/nonexistent}" ] && grep -q "$3" "$o"; then ok "$1"
  else bad "$1" "no void matching '$3' in ${o:-<no file>}: $(head -3 "${o:-/dev/null}" 2>/dev/null)"; fi
}
expect_refuse() { # $1=label $2=env $3=substring expected on stderr, run must exit!=0
  local w; w=$(newdir); local err rc
  err=$(FKENV="$2" drive "$w" run 2>&1 >/dev/null); rc=$?
  if [ $rc -ne 0 ] && echo "$err" | grep -q "$3"; then ok "$1"
  else bad "$1" "rc=$rc stderr=$(echo "$err" | tail -2)"; fi
}

expect_void  "G-COLD fires when the prune leaves images behind" \
             'export FK_IMAGES_AFTER_PRUNE=2' 'G-COLD:2-images-survived-prune'
expect_void  "G-IDLE fires when the client node is loaded (2.25/cpu > 1.5)" \
             'export FK_LOADAVG=9.0 FK_NPROC=4' 'G-IDLE:loadavg=9.0'
expect_void  "G-SSM fires when the measured command itself fails" \
             'export FK_SSM_FAIL_ON=pull' 'G-SSM:command-did-not-succeed'
expect_void  "G-PULL fires on a non-zero pull rc (v1 measured this as FAST)" \
             'export FK_PULL_RC=1' 'G-PULL:pull-rc=1'
expect_void  "G-RUN fires on a non-zero run rc" \
             'export FK_RUN_RC=3' 'G-RUN:run-rc=3'
expect_void  "G-ATTR fires when the other backend served requests" \
             'export FK_REQS_LEAK=5' 'G-ATTR:registry-s3-served-'
expect_void  "G-ATTR fires when the arm's own backend served none (warm hit)" \
             'export FK_REQS_OWN=0' 'G-ATTR:own-backend-served-0-requests'

# G-INTEG needs a pushed-digest reference on disk.
W=$(newdir); echo "sha256:pushed" > "$W/.pushed-digest"
o=$(FKENV='export FK_DIGEST=sha256:corrupt' drive "$W" run 2>/dev/null | tail -1)
if [ -f "${o:-/nonexistent}" ] && grep -q 'G-INTEG:pulled=sha256:corrupt,pushed=sha256:pushed' "$o"; then
  ok "G-INTEG fires when the pulled digest != the pushed digest"
else bad "G-INTEG" "$(head -3 "${o:-/dev/null}" 2>/dev/null)"; fi
# and must NOT fire when they agree
o=$(FKENV='export FK_DIGEST=sha256:pushed' drive "$W" run 2>/dev/null | tail -1)
if [ -f "${o:-/nonexistent}" ] && ! grep -q 'G-INTEG' "$o" && grep -q '"valid":true' "$o"; then
  ok "G-INTEG stays quiet when the digests agree (guard is not a blanket refuse)"
else bad "G-INTEG falsifiability" "$(head -3 "${o:-/dev/null}" 2>/dev/null)"; fi


expect_refuse "G-SETTLE refuses to measure a fleet in motion (2/3 DSes)" \
              'export FK_DS_RUNNING=2' 'G-SETTLE:ds-active=2/3'
expect_refuse "G-SETTLE refuses on recent DS rejections" \
              'export FK_REJECTIONS=4' 'G-SETTLE:'
expect_refuse "v1 defect #2 closed: a missing instance-id fails loudly" \
              'export FK_IID=None' 'no running instance tagged trove/runbz/'

# ── G-CLOCK: shim a BSD-style date with no %N support ────────────────────
W=$(newdir)
cat > "$W/bin/date" <<'DATEFAKE'
#!/bin/bash
for a in "$@"; do case "$a" in +%s%N) echo "1788280000N"; exit 0;; esac; done
exec /bin/date "$@"
DATEFAKE
chmod +x "$W/bin/date"
err=$(drive "$W" run 2>&1 >/dev/null); rc=$?
if [ $rc -ne 0 ] && echo "$err" | grep -q 'G-CLOCK'; then
  ok "G-CLOCK fires on a date(1) without %N (would poison every arithmetic)"
else bad "G-CLOCK" "rc=$rc $(echo "$err" | tail -2)"; fi

# ── arm order must rotate across reps ────────────────────────────────────
W=$(newdir); o=$(REPS=3 drive "$W" run 2>/dev/null | tail -1)
if [ -f "${o:-/nonexistent}" ]; then
  firsts=$(python3 -c "
import json
rows=[json.loads(l) for l in open('$o') if l.strip()]
rows=[r for r in rows if r.get('record')!='substrate']
seen={}
for r in rows: seen.setdefault(r['rep'],r['arm'])
print(','.join(seen[k] for k in sorted(seen)))")
  if [ "$(echo "$firsts" | tr ',' '\n' | sort -u | wc -l | tr -d ' ')" -eq 3 ]; then
    ok "arm order rotates per rep ($firsts) — order effects cannot alias onto an arm"
  else bad "arm rotation" "first arms were $firsts"; fi
else bad "arm rotation" "no results"; fi

# ── score: refuses on too few paired reps, reports on enough ─────────────
tmp=$(mktemp)
printf '{"rep":1,"arm":"A5","valid":true,"ready_ms":1000}\n{"rep":1,"arm":"A3","valid":true,"ready_ms":2000}\n' > "$tmp"
if KC=/dev/null "$DRIVER" score "$tmp" 2>/dev/null | grep -q "REFUSED — 1 paired reps < 3"; then
  ok "score REFUSES a headline from 1 paired rep"
else bad "score refusal" "$(KC=/dev/null "$DRIVER" score "$tmp" 2>&1 | tail -3)"; fi
: > "$tmp"
for r in 1 2 3; do
  printf '{"rep":%s,"arm":"A5","valid":true,"ready_ms":1000}\n{"rep":%s,"arm":"A3","valid":true,"ready_ms":2000}\n' "$r" "$r" >> "$tmp"
done
echo '{"record":"substrate","verdict":"PASS"}' >> "$tmp"
if KC=/dev/null "$DRIVER" score "$tmp" 2>/dev/null | grep -q "A5/A3: median 0.500"; then
  ok "score computes paired per-rep ratios (median 0.500 over 3 reps)"
else bad "score ratio" "$(KC=/dev/null "$DRIVER" score "$tmp" 2>&1 | tail -3)"; fi
printf '{"rep":4,"arm":"A5","valid":false,"void":"G-IDLE:x"}\n' >> "$tmp"
if KC=/dev/null "$DRIVER" score "$tmp" 2>/dev/null | grep -q "VOID rep4 A5: G-IDLE:x"; then
  ok "score surfaces voids instead of silently dropping them"
else bad "score void reporting" "$(KC=/dev/null "$DRIVER" score "$tmp" 2>&1 | tail -3)"; fi
rm -f "$tmp"


# ── substrate stripe-width gate: three states, and 2 is NOT a pass ───────
# flint-29's specific ask. The gate exits 0/1/2 and the failure mode most
# likely to bite this campaign next is exit 2 read as success.
gate_leg() { # $1=label $2=gate exit code $3=expected verdict substring
  local w; w=$(newdir)
  printf 'import sys\nsys.exit(%s)\n' "$2" > "$w/stripe-width-gate.py"
  local o; o=$(drive "$w" run 2>/dev/null | tail -1)
  if [ -f "${o:-/nonexistent}" ] && grep -q "\"verdict\":\"$3" "$o"; then ok "$1"
  else bad "$1" "$(grep substrate "${o:-/dev/null}" 2>/dev/null || echo "no substrate row")"; fi
}
gate_leg "substrate gate exit 0 records PASS"                    0 'PASS'
gate_leg "substrate gate exit 1 records FAIL"                    1 'FAIL'
gate_leg "substrate gate exit 2 records INCONCLUSIVE, not PASS"  2 'INCONCLUSIVE'
gate_leg "an unknown gate exit is INCONCLUSIVE, never PASS"      7 'INCONCLUSIVE'

# the verdict must gate the QUOTABLE number
score_with() { # $1=label $2=verdict-row-or-empty $3=grep expectation $4=invert?
  local t; t=$(mktemp)
  for r in 1 2 3; do
    printf '{"rep":%s,"arm":"A5","valid":true,"ready_ms":1000}\n{"rep":%s,"arm":"A3","valid":true,"ready_ms":2000}\n' "$r" "$r" >> "$t"
  done
  [ -n "$2" ] && echo "$2" >> "$t"
  local outp; outp=$(KC=/dev/null "$DRIVER" score "$t" 2>/dev/null)
  if [ "${4:-no}" = invert ]; then
    echo "$outp" | grep -q "$3" && bad "$1" "$outp" || ok "$1"
  else
    echo "$outp" | grep -q "$3" && ok "$1" || bad "$1" "$outp"
  fi
  rm -f "$t"
}
score_with "score prints the headline when the substrate PASSed" \
  '{"record":"substrate","verdict":"PASS"}' 'A5/A3: median 0.500'
score_with "score WITHHOLDS the headline on INCONCLUSIVE" \
  '{"record":"substrate","verdict":"INCONCLUSIVE:gate-could-not-ask-the-question"}' 'HEADLINE WITHHELD'
score_with "score WITHHOLDS the headline on FAIL" \
  '{"record":"substrate","verdict":"FAIL"}' 'HEADLINE WITHHELD'
score_with "score WITHHOLDS the headline when no substrate row exists at all" \
  '' 'HEADLINE WITHHELD'
score_with "an uncertified ratio is stamped [uncertified], not quoted bare" \
  '{"record":"substrate","verdict":"FAIL"}' 'A5/A3: \[uncertified\]'
score_with "a certified ratio carries no [uncertified] stamp" \
  '{"record":"substrate","verdict":"PASS"}' 'uncertified' invert
score_with "the substrate row is never counted as a measurement rep" \
  '{"record":"substrate","verdict":"PASS"}' 'valid=6 void=0'

# ── two runs in the same second must not merge into one file ─────────────
W=$(newdir)
o1=$(drive "$W" run 2>/dev/null | tail -1); o2=$(drive "$W" run 2>/dev/null | tail -1)
if [ -f "${o1:-/x}" ] && [ -f "${o2:-/x}" ] && [ "$o1" != "$o2" ] \
   && [ "$(grep -c '"arm"' "$o1")" -eq 3 ] && [ "$(grep -c '"arm"' "$o2")" -eq 3 ]; then
  ok "two runs in the same second stay in separate files (3 arms each)"
else bad "same-second run collision" "o1=$o1($(grep -c '"arm"' "${o1:-/dev/null}" 2>/dev/null)) o2=$o2($(grep -c '"arm"' "${o2:-/dev/null}" 2>/dev/null))"; fi

echo "──────────────────────────────────────────────────────────────────"
echo "  passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ]
