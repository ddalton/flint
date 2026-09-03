#!/usr/bin/env bash
#
# The TIER LEG (design §9) — the claim that only flint can make.
#
# flint's evict/hydrate cycle moves a file's real ctime without bumping
# F14, so today every tier cycle spuriously invalidates every warm
# reader's cache: the client's attribute timer expires, it GETATTRs,
# the ctime moved, it throws away the page cache and re-reads bytes it
# already had. A READ-delegation holder does not revalidate at all, so
# the whole tier cycle becomes invisible to it.
#
# This rig evicts files OUT FROM UNDER a live delegation holder and
# scores what the client does next, in two arms differing in EXACTLY
# ONE thing (FLINT_NFS_DELEGATIONS).
#
#   phase 1  write N files through the mount; wait for them to flush to
#            the bucket and then EVICT to 0-byte stubs (watermark 50 on
#            a >50%-full disk = always evict). No restart anywhere: a
#            restart would revoke the delegations this leg is about.
#   phase 2  COLD pass. Drop the client's caches and read every file.
#            This hydrates them and, on the ON arm, takes the READ
#            delegations. It is also the LIVENESS PRECONDITION: a cold
#            holder MUST generate server reads. If it does not, the rig
#            is blind and the run is VOID, not PASS.
#   phase 3  the tier cycle UNDER the holder: wait for every file to
#            evict again while the delegation is live. Assert the stub.
#   phase 4  WARM pass, after sleeping past acregmax.
#              ON  — expect ~zero: no GETATTR, no READ.
#              OFF — expect the storm: GETATTR and READ both >= NFILES.
#            The OFF arm being LOUD is what licenses the ON arm's
#            silence to mean anything (§9 liveness precondition, the
#            oci-ab campaign's G-COLD confound).
#   phase 5  content oracle. A delegation that eliminates RPCs by
#            serving the WRONG bytes is the failure this feature could
#            actually cause, and RPC counts cannot see it.
#
# Topology: the hub runs on the macOS host (so the export lives on a
# real, >50%-full host filesystem, which is what arms the watermark),
# the Lima VM is the kernel client, and MinIO runs IN THE VM. MinIO is
# in the guest rather than in Docker because Docker Desktop on this host
# has no working registry egress — the pull stalls at zero bytes — while
# the VM's network is the one every other rig here already relies on.
# lima forwards the guest's 0.0.0.0:9002 to the host's 127.0.0.1:9002,
# so the hub's S3 endpoint is unchanged. PRIVATE ports throughout: two
# sessions share this VM and this host.
#
# Usage:  tests/lima/deleg/tier-leg.sh [outdir]
# Exit:   0 PASS · 1 FAIL · 2 VOID (the rig could not see)
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO/spdk-csi-driver/target/release"
OUT="${1:-/tmp/flint-tier-leg}"
VM="${LIMA_VM:-flint-nfs-client}"

MDS_PORT="${TIER_LEG_MDS_PORT:-20497}"
MINIO_PORT="${TIER_LEG_MINIO_PORT:-9002}"
MINIO_UNIT=flint-tierleg-minio
MINIO_DATA=/var/tmp/minio-tierleg
MINIO_USER=flintleg
MINIO_PASS=flintleg123
BUCKET=flint-tier-leg

EXPORT_DIR=/tmp/flint-tier-leg-export
STATE_DIR=/tmp/flint-tier-leg-state
CFG=/tmp/flint-tier-leg.yaml
MDS_LOG="$OUT/mds.log"
PIDFILE="$OUT/mds.pid"
MNT=/mnt/flint-tier-leg

NFILES="${TIER_LEG_NFILES:-20}"
FILE_MIB=1
# Identical on both arms: a property of the mount, not of the thing
# under test. Short enough that phase 4 is a genuine revalidation
# without a minute of waiting.
ACREG=5
SLEEP_PAST=8
# Watermark 50 on a >50%-full disk means "evict every clean, quiesced
# file on the next tick". It is the eviction lever, not a threshold
# being tested.
WATERMARK=50

mkdir -p "$OUT"
fail() { echo "✗ FAIL: $*" >&2; exit 1; }
void() { echo "⊘ VOID: $*" >&2; exit 2; }
say()  { echo; echo "▶ $*"; }
pass() { echo "  ✓ $*"; }

vm() { limactl shell "$VM" -- sudo sh -c "$1"; }

s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$MINIO_PORT" "$@"
}

# ── pre-flight ────────────────────────────────────────────────────────
[ -x "$BIN_DIR/flint-pnfs-mds" ] || fail "missing $BIN_DIR/flint-pnfs-mds"
command -v aws    >/dev/null || fail "aws CLI not found"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"

stop_server() {
  [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null
  for _ in $(seq 1 20); do
    kill -0 "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null || break
    sleep 0.5
  done
  kill -9 "$(cat "$PIDFILE" 2>/dev/null)" 2>/dev/null
  rm -f "$PIDFILE"
}

cleanup() {
  set +e
  # umount BEFORE killing the server: a dead server under a live mount
  # D-states umount.
  vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
  stop_server
  [ "${KEEP:-0}" = "1" ] || vm "systemctl stop $MINIO_UNIT 2>/dev/null" 2>/dev/null
}
trap cleanup EXIT

start_server() { # $1 = arm ("off"|"on")
  local flag=""
  [ "$1" = "on" ] && flag=1
  cat > "$CFG" <<EOF
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone

mds:
  bind:
    address: "0.0.0.0"
    port: $MDS_PORT

  layout:
    type: file
    stripeSize: 8388608
    policy: stripe

  dataServers: []

  state:
    backend: sqlite
    config:
      path: $STATE_DIR/state.db

  tier:
    enabled: true
    bucket: $BUCKET
    keyPrefix: "$1/"
    endpoint: "http://127.0.0.1:$MINIO_PORT"
    flushFloorSecs: 2
    quiesceSecs: 1
    tickSecs: 2
    epochHeartbeatSecs: 2
    epochLeaseMisses: 3
    watermarkPct: $WATERMARK
    reserveBytes: 67108864
    ballastBytes: 16777216
    hydrateWarmAfterImport: false

exports:
  - path: $EXPORT_DIR
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: rw

logging:
  level: ${TIER_LEG_LOG:-info}
  format: text
  components:
    mds: ${TIER_LEG_LOG:-info}

monitoring:
  prometheus:
    enabled: false
    port: 0
    path: /metrics
  health:
    enabled: false
    port: 0
    path: /health
  metrics: []
EOF
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    ${flag:+FLINT_NFS_DELEGATIONS=$flag} FLINT_NFS_DELEG_REPORT_SECS=5 \
    FLINT_TIER_REPORT_SECS=5 \
    ${TIER_LEG_RUST_LOG:+RUST_LOG=$TIER_LEG_RUST_LOG} \
    nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG" >>"$MDS_LOG" 2>&1 &
  echo $! >"$PIDFILE"
  disown
  for _ in $(seq 1 40); do
    kill -0 "$(cat "$PIDFILE")" 2>/dev/null \
      || { tail -30 "$MDS_LOG"; fail "$1: hub died on startup"; }
    lsof -nP -iTCP:$MDS_PORT -sTCP:LISTEN >/dev/null 2>&1 && break
    sleep 1
  done
  lsof -nP -iTCP:$MDS_PORT -sTCP:LISTEN >/dev/null 2>&1 \
    || { tail -30 "$MDS_LOG"; fail "$1: hub never bound :$MDS_PORT"; }

  # The SERVER's word for which arm this is, not the launcher's intent.
  # (The reporter names its own posture at startup.)
  for _ in $(seq 1 20); do
    grep -q "deleg reporter" "$MDS_LOG" && break
    sleep 1
  done
  if [ "$1" = "on" ]; then
    grep -q "delegations are ON" "$MDS_LOG" \
      || { grep "deleg reporter" "$MDS_LOG" | tail -2; fail "on: server does not say ON"; }
  else
    grep -q "delegations are OFF" "$MDS_LOG" \
      || { grep "deleg reporter" "$MDS_LOG" | tail -2; fail "off: control does not say OFF"; }
  fi
}

# Per-op counters for THIS mount, as JSON (warm-reaccess.sh's parser:
# per-op rows are ONE line each, right-aligned under whitespace).
read_stats() {
  limactl shell "$VM" -- sudo python3 -c "
import json,re
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

check_json() {
  python3 -c "
import json,sys
d=json.load(open(sys.argv[1]))
assert d, 'no per-op rows matched'
assert 'GETATTR' in d, 'GETATTR row missing: ' + repr(sorted(d))
assert 'READ' in d, 'READ row missing: ' + repr(sorted(d))
" "$1" || void "mountstats capture $1 has no per-op rows — the rig cannot see the mount"
}

delta() {
  python3 -c "
import json
a=json.load(open('$1')); b=json.load(open('$2'))
for k in sorted(set(a)|set(b)):
    print(f'{k}={b.get(k,0)-a.get(k,0)}')
"
}

# Every file 0 bytes on the server's disk == evicted to a stub.
wait_evicted() { # $1 = label, $2 = deadline secs
  local t0=$SECONDS
  while :; do
    local n
    n=$(find "$EXPORT_DIR/tier" -maxdepth 1 -type f -size 0 2>/dev/null | wc -l | tr -d ' ')
    [ "$n" = "$NFILES" ] && { pass "$1: all $NFILES files evicted to 0-byte stubs"; return 0; }
    [ $((SECONDS - t0)) -gt "$2" ] && {
      tail -25 "$MDS_LOG"
      fail "$1: only $n/$NFILES files evicted within $2s"
    }
    sleep 2
  done
}

run_arm() { # $1 = arm
  local arm="$1"
  say "arm=$arm  (FLINT_NFS_DELEGATIONS=$([ "$arm" = on ] && echo 1 || echo '<unset>'))"

  vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
  stop_server
  rm -rf "$EXPORT_DIR" "$STATE_DIR"
  mkdir -p "$EXPORT_DIR/tier" "$STATE_DIR"
  chmod -R 0777 "$EXPORT_DIR"
  : > "$MDS_LOG"
  s3 s3 rm "s3://$BUCKET/$arm/" --recursive >/dev/null 2>&1

  start_server "$arm"

  vm "mkdir -p $MNT"
  vm "mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT,acregmin=1,acregmax=$ACREG,hard host.lima.internal:/ $MNT" \
    || fail "$arm: mount failed"
  vm "test -d $MNT/tier" || fail "$arm: mount is empty"

  # ── phase 1: write, flush, evict ────────────────────────────────────
  vm "for i in \$(seq 1 $NFILES); do dd if=/dev/urandom of=$MNT/tier/f\$i bs=1M count=$FILE_MIB status=none; done; sync"
  vm "for i in \$(seq 1 $NFILES); do md5sum $MNT/tier/f\$i; done" > "$OUT/$arm.md5.orig"
  [ "$(wc -l < "$OUT/$arm.md5.orig" | tr -d ' ')" = "$NFILES" ] \
    || fail "$arm: wrote $(wc -l < "$OUT/$arm.md5.orig") of $NFILES files"
  pass "wrote $NFILES × ${FILE_MIB} MiB through the mount"
  wait_evicted "phase 1" 120

  # ── phase 2: COLD pass — the liveness precondition, and the grant ───
  vm "sync; echo 3 > /proc/sys/vm/drop_caches"
  read_stats > "$OUT/$arm.cold.before.json"; check_json "$OUT/$arm.cold.before.json"
  vm "for i in \$(seq 1 $NFILES); do cat $MNT/tier/f\$i > /dev/null; done"
  read_stats > "$OUT/$arm.cold.after.json";  check_json "$OUT/$arm.cold.after.json"
  delta "$OUT/$arm.cold.before.json" "$OUT/$arm.cold.after.json" > "$OUT/$arm.cold.delta"
  local cold_read
  cold_read=$(sed -n 's/^READ=//p' "$OUT/$arm.cold.delta")
  [ "${cold_read:-0}" -ge "$NFILES" ] \
    || void "$arm: the COLD pass produced only ${cold_read:-0} READ ops for $NFILES evicted files — a cold holder must generate server reads or nothing downstream attributes to the delegation"
  pass "cold pass is loud: READ=$cold_read (>= $NFILES)"

  grep -c "deleg: granted READ delegation" "$MDS_LOG" > "$OUT/$arm.grants"
  local grants; grants=$(cat "$OUT/$arm.grants")
  if [ "$arm" = "on" ]; then
    [ "$grants" -ge 1 ] || fail "on: the hub granted $grants delegations — nothing to measure"
    pass "hub granted $grants READ delegations during the cold pass"
  else
    [ "$grants" = "0" ] || fail "off: control granted $grants delegations with the flag unset"
    pass "control granted 0 delegations"
  fi

  # ── phase 3: the tier cycle UNDER the holder ────────────────────────
  wait_evicted "phase 3 (under the holder)" 120
  local csize
  csize=$(vm "stat -c %s $MNT/tier/f1" | tr -d '\r')
  [ "$csize" = "$((FILE_MIB * 1048576))" ] \
    || fail "$arm: client stat of an evicted file says $csize — GETATTR must serve the LOGICAL size"
  pass "client still sees the logical size ($csize) through the marker"

  # ── phase 4: the WARM pass ──────────────────────────────────────────
  sleep "$SLEEP_PAST"
  read_stats > "$OUT/$arm.warm.before.json"; check_json "$OUT/$arm.warm.before.json"
  vm "for i in \$(seq 1 $NFILES); do cat $MNT/tier/f\$i > /dev/null; done"
  read_stats > "$OUT/$arm.warm.after.json";  check_json "$OUT/$arm.warm.after.json"
  delta "$OUT/$arm.warm.before.json" "$OUT/$arm.warm.after.json" > "$OUT/$arm.warm.delta"
  echo "  warm delta: $(tr '\n' ' ' < "$OUT/$arm.warm.delta")"

  # ── phase 5: content oracle ─────────────────────────────────────────
  vm "for i in \$(seq 1 $NFILES); do md5sum $MNT/tier/f\$i; done" > "$OUT/$arm.md5.now"
  diff -q "$OUT/$arm.md5.orig" "$OUT/$arm.md5.now" >/dev/null \
    || fail "$arm: content changed across the tier cycle"
  pass "content oracle: all $NFILES files byte-identical"

  cp "$MDS_LOG" "$OUT/mds-$arm.log"
  vm "umount -lf $MNT 2>/dev/null" 2>/dev/null
  stop_server
}

# ── MinIO (in the guest; lima forwards the port to the host) ──────────
say "starting MinIO in $VM on :$MINIO_PORT"
limactl shell "$VM" -- sudo bash -c "
  command -v minio >/dev/null || {
    curl -fsSL -o /usr/local/bin/minio \
      https://dl.min.io/server/minio/release/linux-arm64/minio && \
    chmod +x /usr/local/bin/minio
  }
  systemctl stop $MINIO_UNIT 2>/dev/null
  rm -rf $MINIO_DATA && mkdir -p $MINIO_DATA
  systemd-run --unit=$MINIO_UNIT --collect \
    -E MINIO_ROOT_USER=$MINIO_USER -E MINIO_ROOT_PASSWORD=$MINIO_PASS \
    /usr/local/bin/minio server --address 0.0.0.0:$MINIO_PORT $MINIO_DATA
" >/dev/null 2>&1 || fail "could not start MinIO in $VM"

# Health-checked FROM THE HOST, not from the guest: what this rig needs
# is that the hub (a host process) can reach the endpoint, and lima's
# port forward is the part most likely to be missing.
for _ in $(seq 1 60); do
  curl -fs "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null 2>&1 && break
  sleep 1
done
curl -fs "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null 2>&1 \
  || fail "MinIO not reachable from the HOST on :$MINIO_PORT (lima port-forward?)"
s3 s3 mb "s3://$BUCKET" >/dev/null 2>&1
s3 s3 ls "s3://$BUCKET" >/dev/null 2>&1 \
  || fail "bucket $BUCKET not usable through the forwarded endpoint"
pass "MinIO up in the guest, reachable from the host, bucket $BUCKET ready"

for a in ${TIER_LEG_ARMS:-off on}; do run_arm "$a"; done

# ── the score ─────────────────────────────────────────────────────────
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

off_w, on_w = d("off", "warm"), d("on", "warm")
off_c, on_c = d("off", "cold"), d("on", "cold")

print()
print(f"  {'op':<12}{'cold OFF':>10}{'cold ON':>10}{'warm OFF':>10}{'warm ON':>10}")
for k in ("READ", "GETATTR", "OPEN", "OPEN_NOATTR", "CLOSE", "ACCESS", "LOOKUP", "DELEGRETURN"):
    print(f"  {k:<12}{off_c.get(k,0):>10}{on_c.get(k,0):>10}{off_w.get(k,0):>10}{on_w.get(k,0):>10}")

fails, voids = [], []

# LIVENESS FIRST, with the expectations stated in advance: a tier cycle
# moved ctime under N cached files, so a non-delegated client must
# re-read all N and must revalidate metadata once per file. Quiet is
# exactly what a broken rig produces, so a control that is not loud
# makes the run VOID, not PASS.
#
# Metadata revalidation is counted across OPEN + OPEN_NOATTR + GETATTR
# rather than GETATTR alone. The first cut demanded GETATTR >= N and
# would have VOIDed a perfectly good run: Linux revalidates by sending
# OPEN_NOATTR, whose reply carries the attributes, so the control shows
# GETATTR=0 and OPEN_NOATTR=N. The op that carries revalidation is the
# client's choice; the claim being tested is that revalidation happens
# at all.
REVAL = ("OPEN", "OPEN_NOATTR", "GETATTR")
off_reval = sum(off_w.get(k, 0) for k in REVAL)
on_reval = sum(on_w.get(k, 0) for k in REVAL)

if off_reval < n:
    voids.append(f"control warm revalidation (OPEN+OPEN_NOATTR+GETATTR)={off_reval}, "
                 f"expected >= {n} (once per file after the tier cycle)")
if off_w.get("READ", 0) < n:
    voids.append(f"control warm READ={off_w.get('READ',0)}, expected >= {n} "
                 f"(the spurious re-read storm the feature exists to remove)")

on_meta = sum(on_w.get(k, 0) for k in META)
off_meta = sum(off_w.get(k, 0) for k in META)

if not voids:
    # THE LEG'S CLAIM: a tier evict/hydrate cycle under a holder costs
    # the holder zero data re-reads. That is what "tier cycling becomes
    # invisible to warm readers" has to mean to be worth anything.
    if on_w.get("READ", 0) != 0:
        fails.append(f"warm ON re-read {on_w['READ']} times — the holder did not trust "
                     f"its cache across the tier cycle")

print()
if voids:
    for v in voids:
        print(f"  \u2298 {v}")
    print("\n\u2298 VOID — the control arm was not loud, so the treatment's silence means nothing")
    raise SystemExit(2)
for f in fails:
    print(f"  \u2717 {f}")
if fails:
    print("\n\u2717 tier leg FAILED")
    raise SystemExit(1)

print(f"  \u2713 control is loud: warm READ={off_w.get('READ',0)} "
      f"revalidation={off_reval} after a tier cycle it could not see through")
print(f"  \u2713 the re-read storm is GONE: warm READ {off_w.get('READ',0)} \u2192 "
      f"{on_w.get('READ',0)} across the SAME tier cycle")
print(f"  \u2713 OPEN/CLOSE round trips are gone: OPEN+OPEN_NOATTR+CLOSE "
      f"{off_w.get('OPEN',0)+off_w.get('OPEN_NOATTR',0)+off_w.get('CLOSE',0)} \u2192 "
      f"{on_w.get('OPEN',0)+on_w.get('OPEN_NOATTR',0)+on_w.get('CLOSE',0)}")

# REPORTED, NOT ASSERTED. The design's tier-leg text says the warm
# holder re-reads "with zero server RPCs". The data path delivers that;
# the metadata path does not, and the totals here are too close to let
# that pass unremarked. Stating it is the point — an assertion tuned
# until it passes would bury the one number a reader should argue with.
print()
print(f"  \u26a0 metadata RPCs did NOT go to zero: ON={on_meta} vs OFF={off_meta} "
      f"(shape changed, total did not)")
print(f"      OFF  OPEN_NOATTR={off_w.get('OPEN_NOATTR',0)} CLOSE={off_w.get('CLOSE',0)} "
      f"GETATTR={off_w.get('GETATTR',0)} ACCESS={off_w.get('ACCESS',0)}")
print(f"      ON   OPEN_NOATTR={on_w.get('OPEN_NOATTR',0)} CLOSE={on_w.get('CLOSE',0)} "
      f"GETATTR={on_w.get('GETATTR',0)} ACCESS={on_w.get('ACCESS',0)}")
print(f"      The holder stops OPENing and CLOSEing and starts GETATTRing and ACCESSing "
      f"instead. Past acregmax the attribute cache has expired, so this leg cannot show "
      f"the total elimination warm-reaccess.sh measured INSIDE that window (80 \u2192 0).")

print("\n\u2713 tier leg PASS on its data claim — evict/hydrate costs a delegation holder "
      "zero re-reads; see the metadata note above")
PY
RC=$?
echo "RIG_EXIT=$RC"
exit $RC
