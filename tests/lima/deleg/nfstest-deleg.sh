#!/usr/bin/env bash
#
# Design §9's "nfstest_delegation as suite #2", on the wire.
#
# nfstest is the second conformance suite beyond pynfs, and it asks a
# question pynfs does not: it reads the PACKET TRACE and asserts what
# the client did with the delegation — that no second OPEN was sent for
# the same file, that READs carried the delegation stateid, that a read
# from another process generated no READ at all, that DELEGRETURN
# followed the CLOSE. Those are the claims §1 makes for the feature,
# and nothing else measures them.
#
# TWO RIG FACTS, both of which produced convincing false results first:
#
# 1. THE SERVER RUNS INSIDE THE VM, AS ROOT, ON ext4. A macOS-hosted
#    server runs as a non-root uid against a root client, and 35 of 42
#    nfstest_posix "failures" were once exactly that. Never quote an
#    nfstest number taken with the server on the macOS host.
#
# 2. CLIENT AND SERVER NEED DISTINCT IP ADDRESSES. nfstest identifies
#    calls and replies by source/destination address. Run both ends on
#    127.0.0.1 and every packet has src == dst, so its matching finds
#    nothing: it reported "OPEN should be sent" about a trace that
#    plainly contained OPENs, and "READ delegation should be granted"
#    about a trace that plainly contained rd_deleg_stid. Two failures,
#    one cause, neither of them the server's. Hence the veth pair: the
#    server sits in a netns at 10.200.0.2, the client stays in the root
#    namespace at 10.200.0.1. The filesystem is not namespaced, so the
#    export is the same ext4 path on both sides.
#    `--client-ipaddr` must be passed explicitly too — nfstest otherwise
#    auto-detects the VM's main address and tcpdump captures nothing,
#    which surfaces as "Packet trace file is empty".
#
# Usage:  tests/lima/deleg/nfstest-deleg.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="${NFSTEST_BIN:-$REPO/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/flint-nfs-server}"
OUT="${1:-/tmp/flint-nfstest-deleg}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${NFSTEST_PORT:-20497}"
EXPORT="/var/tmp/flint-nfstest-export"
MTPOINT="/mnt/flintdeleg"
SRV_IP=10.200.0.2
CLI_IP=10.200.0.1
IFACE=veth-flint

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
note() { echo "  · $*"; }

[ -f "$BIN" ] || fail "missing $BIN — cross-build it:
  RUSTFLAGS=-C link-self-contained=no \\
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=<zigcc> \\
  CC_aarch64_unknown_linux_musl=<zigcc> \\
  cargo build --release --bin flint-nfs-server --target aarch64-unknown-linux-musl"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"
limactl shell "$VM" -- test -x /usr/bin/nfstest_delegation \
  || fail "nfstest not installed in the VM (sudo apt-get install -y nfstest)"

# ── the veth pair ────────────────────────────────────────────────────
limactl shell "$VM" -- sudo bash -c '
set -e
NS=flintns
ip netns del $NS 2>/dev/null || true
ip link del veth-flint 2>/dev/null || true
ip netns add $NS
ip link add veth-flint type veth peer name veth-flint-ns
ip link set veth-flint-ns netns $NS
ip addr add 10.200.0.1/24 dev veth-flint
ip link set veth-flint up
ip netns exec $NS ip addr add 10.200.0.2/24 dev veth-flint-ns
ip netns exec $NS ip link set veth-flint-ns up
ip netns exec $NS ip link set lo up
' > "$OUT/netns.log" 2>&1 || { cat "$OUT/netns.log"; fail "netns setup failed"; }
limactl shell "$VM" -- ping -c1 -W2 "$SRV_IP" >/dev/null 2>&1 \
  || fail "the netns is up but $SRV_IP does not answer"
note "netns ready — client $CLI_IP <-> server $SRV_IP"

start_server() {   # $1 = arm, $2 = 1|"" for FLINT_NFS_DELEGATIONS
  # Separate statements: bash expands every word of a `local` before it
  # performs any of the assignments, so `log=...$arm...` on the same
  # line reads an unset `arm` and trips `set -u`.
  local arm="$1"
  local flag="$2"
  local log="/var/tmp/flint-nfstest-$arm.log"
  limactl shell "$VM" -- sudo sh -c "ip netns exec flintns pkill -f flint-nfs-server 2>/dev/null; exit 0"
  sleep 2
  limactl shell "$VM" -- sudo sh -c "rm -rf $EXPORT; mkdir -p $EXPORT/.flint-nfs $EXPORT/tmp; printf nfstestvol > $EXPORT/.flint-nfs/volume-id; chmod 0777 $EXPORT $EXPORT/tmp" \
    || fail "$arm: could not prepare the export"
  limactl shell "$VM" -- sudo sh -c "${flag:+FLINT_NFS_DELEGATIONS=$flag }FLINT_NFS_DELEG_REPORT_SECS=5 FLINT_NFS_GRACE_SECS=90 nohup setsid ip netns exec flintns $BIN --bind-addr 0.0.0.0 --port $PORT --export-path $EXPORT --volume-id nfstestvol </dev/null >$log 2>&1 & echo started" >/dev/null \
    || fail "$arm: could not start the server"
  sleep 6
  limactl shell "$VM" -- sudo sh -c "ip netns exec flintns ss -ltn | grep -q ':$PORT '" \
    || fail "$arm: server is not listening on $PORT"
  # The server's OWN word for the arm, not the launcher's intent.
  local said
  said=$(limactl shell "$VM" -- sudo grep -o "deleg reporter:.*" "$log" 2>/dev/null | head -1)
  if [ -n "$flag" ]; then
    echo "$said" | grep -q "delegations are ON" \
      || fail "G5($arm): asked for delegations ON, server said: $said"
  else
    echo "$said" | grep -q "delegations are OFF" \
      || fail "G5($arm): control arm did not announce delegations OFF: $said"
  fi
  note "$arm: $said"
}

run_tests() {      # $1 = arm, $2 = runtest list
  local arm="$1"
  local tests="$2"
  limactl shell "$VM" -- sudo sh -c "
    cd /var/tmp
    timeout 2400 nfstest_delegation --server $SRV_IP --export /tmp --port $PORT \
      --nfsversion 4.1 --mtpoint $MTPOINT -i $IFACE --client-ipaddr $CLI_IP \
      --runtest $tests 2>&1" > "$OUT/nfstest-$arm.log" 2>&1
  grep -E "tests \(" "$OUT/nfstest-$arm.log" | tail -1
}

echo "▶ arm=on  (FLINT_NFS_DELEGATIONS=1) — the full basic set"
start_server on 1
ON_ALL=$(run_tests on-all basic)
note "basic: $ON_ALL"

echo "▶ arm=on  — the READ-delegation subset (basic01,03,05)"
ON_READ=$(run_tests on-read basic01,basic03,basic05)
note "read subset: $ON_READ"

echo "▶ arm=off (control) — same READ subset"
start_server off ""
OFF_READ=$(run_tests off-read basic01,basic03,basic05)
note "read subset: $OFF_READ"

python3 - "$OUT" <<'PY'
import os, re, sys
out = sys.argv[1]
bad = []

def counts(name):
    p = os.path.join(out, f"nfstest-{name}.log")
    txt = open(p, errors="replace").read()
    m = re.findall(r"(\d+) tests \((\d+) passed, (\d+) failed\)", txt)
    if not m:
        return None
    t, p_, f = m[-1]
    return int(t), int(p_), int(f)

def fails(name):
    p = os.path.join(out, f"nfstest-{name}.log")
    return [l.split("FAIL:",1)[1].strip()
            for l in open(p, errors="replace") if "FAIL:" in l]

on_all, on_read, off_read = counts("on-all"), counts("on-read"), counts("off-read")
for label, c in (("on-all", on_all), ("on-read", on_read), ("off-read", off_read)):
    if c is None:
        bad.append(f"G1: {label} produced no result line — the run did not happen")
if bad:
    print("\n".join("  ✗ " + b for b in bad)); sys.exit(1)

print(f"\n  basic (flag ON)     {on_all[1]}/{on_all[0]} passed, {on_all[2]} failed")
print(f"  read subset ON      {on_read[1]}/{on_read[0]} passed, {on_read[2]} failed")
print(f"  read subset OFF     {off_read[1]}/{off_read[0]} passed, {off_read[2]} failed")

# The READ subset must be CLEAN with the flag on. flint grants read
# delegations; if the tests that only need read delegations do not pass,
# that is a defect and not a documented non-goal.
if on_read[2] != 0:
    bad.append(f"the READ-delegation subset failed {on_read[2]} assertion(s) "
               f"with the flag ON: {fails('on-read')}")

# Every failure in the full set must be a WRITE-delegation one. flint
# grants only READ delegations (design §1, an explicit non-goal), so
# those are expectations; anything else is a finding.
allowed = ("WRITE delegation should be granted",
           "OPEN should be sent with the filehandle of the file to be opened")
for f in fails("on-all"):
    if not any(f.startswith(a) for a in allowed):
        bad.append(f"unexpected failure in the basic set: {f!r}")

# THE CONTROL. With the flag off, "READ delegation should be granted"
# has to fail — otherwise the clean ON run says nothing about the
# feature, only about nfstest's willingness to pass.
off_f = fails("off-read")
if not any("READ delegation should be granted" in f for f in off_f):
    bad.append("CONTROL: with delegations OFF, nfstest did NOT complain that "
               "no read delegation was granted — the ON result is therefore "
               "not attributable to the feature")
else:
    n = sum("READ delegation should be granted" in f for f in off_f)
    print(f"  · control is loud — {n} 'READ delegation should be granted' failures with the flag off")

# And the cascade, stated as evidence rather than assumed: the
# filehandle assertion fails on the control too, alongside the missing
# delegation. That is why it is tolerated in the ON set.
if any("filehandle of the file to be opened" in f for f in off_f):
    print("  · 'OPEN with the filehandle' also fails on the control — "
          "it is a dependent of the delegation, not an independent defect")

if bad:
    print("\n✗ FAILED")
    for b in bad: print(f"  ✗ {b}")
    sys.exit(1)
print("\n✓ nfstest_delegation basic set PASSES — read delegations fully conformant; "
      "every failure is a write-delegation non-goal")
PY
