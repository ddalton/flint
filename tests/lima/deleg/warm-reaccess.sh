#!/usr/bin/env bash
#
# The warm re-access rig (design §9) — the leg the whole feature exists
# for, and the only one that measures VALUE rather than correctness.
#
# Claim under test: a Linux client holding a READ delegation satisfies
# open(2) locally and trusts its caches without CHANGE revalidation, so
# on a warm re-read the OPEN/CLOSE/ACCESS/GETATTR traffic goes to zero.
#
# Shape: two arms differing in EXACTLY ONE thing (FLINT_NFS_DELEGATIONS).
# Each arm: fresh mount, pass 1 over N files, sleep past the attribute
# cache, pass 2 over the same N files. Score the per-op deltas from
# /proc/self/mountstats.
#
# THE HARD PART IS NOT THE MEASUREMENT, IT IS PROVING THE RIG CAN SEE.
# "Flag ON shows ~zero metadata RPCs" is also what a broken rig produces
# — a mount that failed, a pass that read nothing, a client that never
# talked to this server at all. Quiet is the shape of success AND the
# shape of blindness. So the control arm must be demonstrably LOUD
# before the treatment's silence is allowed to mean anything, and a run
# where it is not is VOID, not PASS (§9's liveness precondition, which
# the oci-ab campaign paid for).
#
# Usage:  tests/lima/deleg/warm-reaccess.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$REPO/spdk-csi-driver/target/release/flint-nfs-server"
OUT="${1:-/tmp/flint-warm-reaccess}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${WARM_PORT:-20496}"
EXPORT="${WARM_EXPORT:-/tmp/flint-warm-export}"
VOL="warmvol"
HOST="host.lima.internal"
NFILES="${WARM_NFILES:-40}"
MNT=/mnt/flint-warm
# Short attribute cache so pass 2 is a genuine revalidation on the
# control arm without a 60s wait. IDENTICAL on both arms — this is a
# property of the mount, not of the thing under test.
ACREG=5
SLEEP_PAST=8

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
void() { echo "⊘ VOID: $*" >&2; exit 2; }

[ -x "$BIN" ] || fail "missing $BIN"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM not running"

stop() {
  limactl shell "$VM" -- sudo umount -f "$MNT" 2>/dev/null
  [ -f "$OUT/pid" ] && { kill "$(cat "$OUT/pid")" 2>/dev/null; rm -f "$OUT/pid"; }
}
trap stop EXIT

# Per-op counters for THIS mount, as JSON. Reading /proc/self/mountstats
# rather than nfsstat because it is per-mount: a second mount in the VM
# (another session's) would otherwise be counted into ours.
# The JSON is tagged and extracted by prefix rather than taken as the
# whole of stdout: `limactl shell` prepends its own warnings often
# enough (hypervisor notices, ssh chatter) that "the output IS the
# JSON" fails intermittently — and it fails by producing an unparseable
# file at the exact moment nobody is watching.
read_stats() {
  limactl shell "$VM" -- sudo python3 -c "
import json,re
# The per-op rows are ONE line each — 'OPEN: 1 1 0 332 408 0 0 0 0',
# right-aligned under leading whitespace. An earlier version of this
# parser expected a NAME: header followed by a numbers line, which is
# the older layout; against this kernel it matched nothing and every
# counter read zero. That is why the liveness precondition below exists
# and why it is checked BEFORE any ratio: a parser that silently
# matches nothing produces exactly the numbers a perfect result would.
want={'OPEN','CLOSE','ACCESS','GETATTR','LOOKUP','READ','OPEN_NOATTR','DELEGRETURN'}
row=re.compile(r'^\s*([A-Z_]+):\s+(\d+)\s')
out={}; inmine=False
for line in open('/proc/self/mountstats'):
    if line.startswith('device '):
        inmine = ' mounted on $MNT ' in line
        continue
    if not inmine: continue
    m=row.match(line)
    if m and m.group(1) in want:
        out[m.group(1)]=int(m.group(2))
print('MOUNTSTATS_JSON ' + json.dumps(out))
" | sed -n 's/^MOUNTSTATS_JSON //p' | tail -1
}

delta() {  # $1=before.json $2=after.json  -> prints 'OP=n' lines
  python3 -c "
import json,sys
a=json.load(open('$1')); b=json.load(open('$2'))
for k in sorted(set(a)|set(b)):
    print(f'{k}={b.get(k,0)-a.get(k,0)}')
"
}

run_arm() {
  local arm="$1" flag="$2"
  local log="$OUT/server-$arm.log"
  echo "▶ arm=$arm  FLINT_NFS_DELEGATIONS=${flag:-<unset>}"

  stop
  while lsof -ti :"$PORT" >/dev/null 2>&1; do lsof -ti :"$PORT" | xargs kill -9 2>/dev/null; sleep 1; done
  rm -rf "$EXPORT"; mkdir -p "$EXPORT/.flint-nfs" "$EXPORT/warm"
  printf '%s' "$VOL" > "$EXPORT/.flint-nfs/volume-id"
  # Content the oracle can check, distinct per file.
  for i in $(seq 1 "$NFILES"); do
    printf 'warm-file-%03d-original\n' "$i" > "$EXPORT/warm/f$i"
  done
  chmod -R 0777 "$EXPORT/warm"

  env ${flag:+FLINT_NFS_DELEGATIONS=$flag} FLINT_NFS_DELEG_REPORT_SECS=5 \
      "$BIN" --bind-addr 0.0.0.0 --port "$PORT" \
             --export-path "$EXPORT" --volume-id "$VOL" > "$log" 2>&1 &
  echo $! > "$OUT/pid"
  until grep -qE "NFSv4.2 server on|Address already" "$log" 2>/dev/null; do sleep 1; done
  grep -q "Address already" "$log" && fail "$arm: port $PORT squatted"
  # The SERVER's word for which arm this is, not the launcher's intent.
  if [ -n "$flag" ]; then
    grep -q "delegations are OFF" "$log" && fail "$arm: asked ON, server says OFF"
  else
    grep -q "delegations are OFF" "$log" || fail "$arm: control did not announce OFF"
  fi

  limactl shell "$VM" -- sudo mkdir -p "$MNT"
  limactl shell "$VM" -- sudo mount -t nfs4 \
      -o "minorversion=1,proto=tcp,port=$PORT,acregmin=1,acregmax=$ACREG,hard" \
      "$HOST:/" "$MNT" || fail "$arm: mount failed"
  limactl shell "$VM" -- test -f "$MNT/warm/f1" || fail "$arm: mount is empty"

  # ── pass 1: cold-ish open+read of every file ──────────────────────
  check_json() { python3 -c "import json,sys;json.load(open(sys.argv[1]))" "$1" \
      || fail "$arm: unparseable mountstats capture $1 — the rig cannot see the mount"; }
  # An EMPTY object parses as JSON perfectly well, so `check_json`
  # cannot catch a parser that matched no rows — the very failure that
  # produced this rig's first (void) run. Demand the ops actually exist.
  check_json() {
      python3 -c "
import json,sys
d=json.load(open(sys.argv[1]))
assert d, 'no per-op rows matched'
assert 'GETATTR' in d, 'GETATTR row missing: ' + repr(sorted(d))
" "$1" || fail "$arm: mountstats capture $1 has no per-op rows — the rig cannot see the mount"; }
  read_stats > "$OUT/$arm.p1.before.json"; check_json "$OUT/$arm.p1.before.json"
  limactl shell "$VM" -- bash -c "for i in \$(seq 1 $NFILES); do cat $MNT/warm/f\$i > /dev/null; done"
  read_stats > "$OUT/$arm.p1.after.json"; check_json "$OUT/$arm.p1.after.json"

  sleep "$SLEEP_PAST"

  # ── pass 2: the WARM re-access this feature is about ──────────────
  read_stats > "$OUT/$arm.p2.before.json"; check_json "$OUT/$arm.p2.before.json"
  limactl shell "$VM" -- bash -c "for i in \$(seq 1 $NFILES); do cat $MNT/warm/f\$i > /dev/null; done"
  read_stats > "$OUT/$arm.p2.after.json"; check_json "$OUT/$arm.p2.after.json"

  # ── pass 3: the SAME re-access with NO sleep. This is the
  # discriminator for whatever pass 2 leaves behind. If a residual
  # per-file op vanishes here, it was driven by the attribute-cache
  # timer — i.e. the delegation is not suppressing revalidation. If it
  # survives, it is per-open behaviour independent of the timer. Two
  # very different findings, and without this pass they are
  # indistinguishable and the temptation is to guess.
  read_stats > "$OUT/$arm.p3.before.json"; check_json "$OUT/$arm.p3.before.json"
  limactl shell "$VM" -- bash -c "for i in \$(seq 1 $NFILES); do cat $MNT/warm/f\$i > /dev/null; done"
  read_stats > "$OUT/$arm.p3.after.json"; check_json "$OUT/$arm.p3.after.json"

  # Content oracle: the bytes must still be right. A delegation that
  # eliminates RPCs by serving the WRONG thing is the failure this
  # feature could actually cause, and RPC counts cannot see it.
  limactl shell "$VM" -- bash -c "
     bad=0
     for i in \$(seq 1 $NFILES); do
       want=\$(printf 'warm-file-%03d-original' \$i)
       got=\$(cat $MNT/warm/f\$i)
       [ \"\$got\" = \"\$want\" ] || bad=\$((bad+1))
     done
     echo \$bad" > "$OUT/$arm.badcontent"

  delta "$OUT/$arm.p1.before.json" "$OUT/$arm.p1.after.json" > "$OUT/$arm.p1.delta"
  delta "$OUT/$arm.p2.before.json" "$OUT/$arm.p2.after.json" > "$OUT/$arm.p2.delta"
  delta "$OUT/$arm.p3.before.json" "$OUT/$arm.p3.after.json" > "$OUT/$arm.p3.delta"
  grep -c "deleg: granted READ delegation" "$log" > "$OUT/$arm.grants" || true
  cp "$log" "$OUT/server-$arm.final.log"
  stop
}

run_arm off ""
run_arm on 1

python3 - "$OUT" "$NFILES" <<'PY'
import os, sys
out, n = sys.argv[1], int(sys.argv[2])
META = ("OPEN", "OPEN_NOATTR", "CLOSE", "ACCESS", "GETATTR")

def d(arm, p):
    r = {}
    for line in open(os.path.join(out, f"{arm}.{p}.delta")):
        k, v = line.strip().split("=")
        r[k] = int(v)
    return r

def meta(x): return sum(x.get(k, 0) for k in META)
def grants(arm): return int(open(os.path.join(out, f"{arm}.grants")).read().strip() or 0)
def badcontent(arm): return int(open(os.path.join(out, f"{arm}.badcontent")).read().strip() or 0)

off1, off2, off3 = d("off", "p1"), d("off", "p2"), d("off", "p3")
on1,  on2,  on3  = d("on",  "p1"), d("on",  "p2"), d("on",  "p3")

print(f"\n{'':<14}{'pass1':>7}{'pass2':>7}{'pass3':>7}   per-op pass2 / pass3")
for name, a, b, c in (("flag OFF", off1, off2, off3), ("flag ON ", on1, on2, on3)):
    p2 = " ".join(f"{k}={b.get(k,0)}" for k in META if b.get(k, 0)) or "(none)"
    p3 = " ".join(f"{k}={c.get(k,0)}" for k in META if c.get(k, 0)) or "(none)"
    print(f"{name:<14}{meta(a):>7}{meta(b):>7}{meta(c):>7}   {p2}  /  {p3}")
print(f"\ngrants: off={grants('off')}  on={grants('on')}   files={n}")

# ── LIVENESS PRECONDITION, before anything else is allowed to mean
# anything. Three-state: the third state is the one that does the work.
if meta(off1) == 0:
    print("⊘ VOID: the CONTROL arm's pass 1 made ZERO metadata RPCs — the rig "
          "is not measuring this server. Quiet is what a broken rig produces.")
    sys.exit(2)
ratio_off = meta(off2) / meta(off1)
print(f"control pass2/pass1 = {ratio_off:.2f}  (must be >= 0.80 for the run to count)")
if ratio_off < 0.80:
    print("⊘ VOID: the control arm is not loud on re-access, so there is no "
          "storm here for delegations to eliminate and the ON arm's silence "
          "attributes to nothing.")
    sys.exit(2)

# ── grant-coverage floor: without it, "no RPCs" could mean "no reads".
if grants("on") < 0.95 * n:
    print(f"✗ the ON arm granted {grants('on')} delegations for {n} files "
          f"(floor {0.95*n:.0f}) — the measurement is not about delegations")
    sys.exit(1)
if grants("off") != 0:
    print(f"✗ the CONTROL arm granted {grants('off')} — the flag is not what separates the arms")
    sys.exit(1)

# ── content oracle: RPC elimination that serves wrong bytes is the
# failure this feature can actually cause, and no counter can see it.
for arm in ("off", "on"):
    if badcontent(arm) != 0:
        print(f"✗ arm {arm}: {badcontent(arm)} file(s) served WRONG CONTENT")
        sys.exit(1)
print("content oracle: every file read back correct on both arms")

ratio_on = meta(on2) / meta(on1) if meta(on1) else 1.0
print(f"treatment pass2/pass1 = {ratio_on:.2f}")
saved = 1 - (meta(on2) / meta(off2)) if meta(off2) else 0
print(f"\nwarm-re-access metadata RPCs (past the attr cache): "
      f"{meta(off2)} -> {meta(on2)}  ({saved*100:.1f}% eliminated)")
saved3 = 1 - (meta(on3) / meta(off3)) if meta(off3) else 0
print(f"re-access INSIDE the attr cache:                  "
      f"{meta(off3)} -> {meta(on3)}  ({saved3*100:.1f}% eliminated)")

# Report the residual honestly rather than rounding it away. §9 predicts
# "pass-2 metadata RPCs < 5% of pass-1"; anything above that is a real
# gap between the design's claim and the measurement, and the shape of
# what is left says where to look.
resid = {k: on2.get(k, 0) for k in META if on2.get(k, 0)}
if resid:
    print(f"\nresidual on the ON arm past the attr cache: {resid}")
    if meta(on2) > 0.05 * meta(on1):
        print(f"NOTE: that is {meta(on2)/meta(on1)*100:.0f}% of pass 1, above §9's "
              f"<5% prediction. Not a rig failure — a measured shortfall.")
PY
rc=$?
echo
case $rc in
  0) echo "✓ rig ran; guards held" ;;
  2) echo "⊘ VOID — the run proves nothing; do not quote a number from it" ;;
  *) echo "✗ a guard failed (rc=$rc)" ;;
esac
exit $rc
