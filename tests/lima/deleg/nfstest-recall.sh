#!/usr/bin/env bash
#
# Design §9's nfstest recall set (recall01-54), on the wire, TWO clients.
#
# This is the leg that needs a second NFS client: nfstest drives the
# conflicting operation from a different client and then asserts, on the
# FIRST client's packet trace, that CB_RECALL went out, that the right
# delegation was recalled, that DELEGRETURN carried the right stateid,
# and that the conflicting open was only answered afterwards.
#
# THE SECOND CLIENT IS A NETWORK NAMESPACE, and that is not a shortcut.
# Linux keys NFS client state on the netns: a mount made inside
# `flintcli2` gets its own nfs_client, its own clientid and its own
# callback channel, which is exactly what "a different client" means to
# a delegation. It also has a PRIVATE MOUNT NAMESPACE, without which its
# mount would stack on client 1's at the same path and every result
# would be about the wrong client.
#
# Why not two VMs: lima's vzNAT NATs each guest separately. The host
# reaches both and each guest reaches the gateway, but the guests cannot
# reach each other — ARP fails — so nfstest cannot ssh from client 1 to
# client 2. Guest-to-guest needs the privileged socket_vmnet helper.
#
# ⚠ THE TWO CLIENTS MUST RUN DIFFERENT NFS VERSIONS. nfstest picks the
# second client's version from --client-nfsvers, requiring one that
# DIFFERS from --nfsversion; give it only the same version and it
# silently runs zero tests. Its default list is "4.0,4.1", so against
# a 4.1 main client it picks 4.0 — which flint does not serve, and the
# mount fails. Hence 4.2 for client 1 and 4.1 for client 2: different,
# and both spoken by flint.
#
# Usage:  tests/lima/deleg/nfstest-recall.sh [outdir]
set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="${NFSTEST_BIN:-$REPO/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/flint-nfs-server}"
OUT="${1:-/tmp/flint-nfstest-recall}"
VM="${LIMA_VM:-flint-nfs-client}"
PORT="${NFSTEST_PORT:-20497}"
EXPORT="/var/tmp/flint-nfstest-export"
SRV=10.200.0.2
CLI1=10.200.0.1
CLI2=10.200.0.3
IFACE=br-flint

mkdir -p "$OUT"
fail() { echo "✗ $*" >&2; exit 1; }
note() { echo "  · $*"; }

[ -f "$BIN" ] || fail "missing $BIN (cross-build flint-nfs-server for aarch64-unknown-linux-musl)"
limactl list "$VM" 2>/dev/null | grep -q Running || fail "VM $VM is not running"
limactl shell "$VM" -- test -x /usr/bin/nfstest_delegation \
  || fail "nfstest not installed in the VM (sudo apt-get install -y nfstest)"

# ── topology: client1 / server / client2 on one bridge ───────────────
limactl cp "$REPO/tests/lima/deleg/nfstest-3way-setup.sh" "$VM:/tmp/nfstest-3way-setup.sh" >/dev/null
limactl shell "$VM" -- sudo bash /tmp/nfstest-3way-setup.sh > "$OUT/topology.log" 2>&1 \
  || { cat "$OUT/topology.log"; fail "topology setup failed"; }
grep -q "MOUNT ISOLATION FAILED" "$OUT/topology.log" \
  && { cat "$OUT/topology.log"; fail "client 2's mounts are visible to client 1"; }
note "topology: client1 $CLI1 · server $SRV · client2 $CLI2"

# passwordless ssh client1 -> client2, which nfstest shells out to.
limactl shell "$VM" -- bash -c '
  set -e
  mkdir -p ~/.ssh && chmod 700 ~/.ssh
  [ -f ~/.ssh/id_ed25519 ] || ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519 -q
  grep -qF "$(cat ~/.ssh/id_ed25519.pub)" ~/.ssh/authorized_keys 2>/dev/null \
    || cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
  chmod 600 ~/.ssh/authorized_keys
  grep -q "Host 10.200.0.3" ~/.ssh/config 2>/dev/null || printf "%s\n" \
    "Host 10.200.0.3" "  StrictHostKeyChecking no" "  UserKnownHostsFile /dev/null" \
    "  LogLevel ERROR" >> ~/.ssh/config
  chmod 600 ~/.ssh/config
' > "$OUT/ssh.log" 2>&1 || { cat "$OUT/ssh.log"; fail "ssh setup failed"; }
limactl shell "$VM" -- bash -c "ssh -o BatchMode=yes $CLI2 'sudo -n true'" >/dev/null 2>&1 \
  || fail "client 2 is not reachable with passwordless ssh + sudo"
note "passwordless ssh + sudo to client 2 OK"

# ── the server, in its own netns, as root, on ext4 ───────────────────
limactl shell "$VM" -- sudo sh -c "ip netns exec flintns pkill -f flint-nfs-server 2>/dev/null; exit 0"
sleep 2
limactl shell "$VM" -- sudo sh -c "rm -rf $EXPORT; mkdir -p $EXPORT/.flint-nfs $EXPORT/tmp; printf nfstestvol > $EXPORT/.flint-nfs/volume-id; chmod 0777 $EXPORT $EXPORT/tmp" \
  || fail "could not prepare the export"
limactl shell "$VM" -- sudo sh -c "FLINT_NFS_DELEGATIONS=1 FLINT_NFS_DELEG_REPORT_SECS=5 FLINT_NFS_GRACE_SECS=90 nohup setsid ip netns exec flintns $BIN --bind-addr 0.0.0.0 --port $PORT --export-path $EXPORT --volume-id nfstestvol </dev/null >/var/tmp/flint-recall.log 2>&1 & echo started" >/dev/null
sleep 6
limactl shell "$VM" -- sudo sh -c "ip netns exec flintns ss -ltn | grep -q ':$PORT '" \
  || fail "server did not come up on $PORT"
SAID=$(limactl shell "$VM" -- sudo grep -o "deleg reporter:.*" /var/tmp/flint-recall.log 2>/dev/null | head -1)
echo "$SAID" | grep -q "delegations are ON" \
  || fail "the server does not report delegations ON: $SAID"
note "$SAID"

# ── the run ──────────────────────────────────────────────────────────
echo "▶ recall01-54, two clients"
limactl shell "$VM" -- sh -c "
  cd /var/tmp
  timeout 5400 nfstest_delegation --server $SRV --export /tmp --port $PORT \
    --nfsversion 4.2 --mtpoint /mnt/flintdeleg -i $IFACE --client-ipaddr $CLI1 \
    --client-nfsvers 4.1 --client $CLI2 \
    --runtest recall 2>&1" > "$OUT/recall.log" 2>&1
limactl shell "$VM" -- sudo cp /var/tmp/flint-recall.log /tmp/server-recall.log 2>/dev/null
limactl cp "$VM:/tmp/server-recall.log" "$OUT/server.log" >/dev/null 2>&1 || true

python3 - "$OUT" <<'PY'
import re, os, sys
out = sys.argv[1]
lines = open(os.path.join(out, "recall.log"), errors="replace").read().splitlines()

m = re.findall(r"(\d+) tests \((\d+) passed, (\d+) failed\)", "\n".join(lines))
if not m:
    print("✗ G1: no result line — the run did not happen"); sys.exit(1)
total, passed, failed = map(int, m[-1])

cur, tests = None, {}
for l in lines:
    t = re.match(r"\s*\*\*\*\s+(.*)", l)
    if t:
        cur = t.group(1).strip(); tests.setdefault(cur, {"p": 0, "f": [] })
    if cur:
        if "PASS:" in l: tests[cur]["p"] += 1
        if "FAIL:" in l: tests[cur]["f"].append(l.split("FAIL:", 1)[1].strip())

clean = [t for t, v in tests.items() if not v["f"]]
dirty = {t: v for t, v in tests.items() if v["f"]}
bad = []

print(f"\n  {total} assertions — {passed} passed, {failed} failed")
print(f"  {len(tests)} recall tests — {len(clean)} fully clean, {len(dirty)} with failures")

# G1 — the whole set has to have run. A --client that did not parse
# yields zero tests and zero failures, which reads as a pass.
if len(tests) < 50:
    bad.append(f"G1: only {len(tests)} recall tests ran, expected 54 — "
               "did --client parse, and do the two clients differ in NFS version?")

# THE GATE. flint grants READ delegations only (§1, an explicit
# non-goal for WRITE), so "WRITE delegation should be granted" is an
# expectation. ANY other failure is a finding and must not be absorbed.
unexpected = {}
for t, v in dirty.items():
    for f in v["f"]:
        if not f.startswith("WRITE delegation should be granted"):
            unexpected.setdefault(t, []).append(f)
if unexpected:
    for t, fs in list(unexpected.items())[:10]:
        bad.append(f"unexpected failure in {t!r}: {fs}")

# Every failing test must be a WRITE-delegation test, and must fail ONLY
# at the grant. A write test that also broke somewhere else would be
# hidden by the line above if we only counted failure strings.
for t, v in dirty.items():
    if "WRITE delegation" not in t:
        bad.append(f"a READ-delegation test failed: {t!r} -> {v['f']}")

# A grant floor: the read tests must actually have obtained delegations,
# or "no unexpected failures" is a statement about a rig that never got
# far enough to be wrong.
if len(clean) < 10:
    bad.append(f"only {len(clean)} recall tests came through clean — too few to "
               "conclude the read-delegation recall path works")

srv = os.path.join(out, "server.log")
if os.path.exists(srv):
    txt = open(srv, errors="replace").read()
    revoked = txt.count("REVOKED delegation")
    print(f"  · server-side revocations during the run: {revoked}")
    if revoked:
        bad.append(f"{revoked} delegation(s) were REVOKED — a recall was not "
                   "honoured in time; every one of these tests returns promptly")

if bad:
    print("\n✗ FAILED")
    for b in bad: print(f"  ✗ {b}")
    sys.exit(1)
print("\n✓ recall set PASSES — every failure is the WRITE-delegation non-goal; "
      "the read-delegation recall path is clean end to end")
PY
