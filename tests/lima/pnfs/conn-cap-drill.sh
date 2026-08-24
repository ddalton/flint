#!/usr/bin/env bash
# Blocker 6 differential: does FLINT_NFS_MAX_CONNECTIONS actually refuse?
#
# Runs the HUB binary (flint-pnfs-mds --standalone) in the Lima VM and
# opens more TCP connections than the cap allows.
#
# TWO ARMS, and the second is the point. "Connection 3 failed" on its own
# proves nothing — a connection can fail because the server died, because
# the port moved, because the VM is wedged. The control arm re-runs the
# IDENTICAL probe with the cap disabled and requires all three to
# succeed. Only the DIFFERENCE between the arms is evidence, and if both
# arms agree the drill reports VOID rather than a pass.
set -uo pipefail

VM=${LIMA_VM:-flint-nfs-client}
PORT=${NFS_PORT:-20490}
BIN=/tmp/flint-pnfs-mds-vm
CFG=/tmp/lite-pynfs.yaml

vm() { limactl shell "$VM" -- sudo bash -lc "$1"; }

start_hub() { # $1 = cap value
  vm "systemctl stop flint-mds-vm 2>/dev/null || true;
      systemctl reset-failed flint-mds-vm 2>/dev/null || true;
      rm -rf /srv/flint-mds-export /srv/flint-mds-state;
      mkdir -p /srv/flint-mds-export/tmp /srv/flint-mds-state;
      chmod 0777 /srv/flint-mds-export/tmp;
      systemd-run --unit=flint-mds-vm --collect \
        --setenv=FLINT_NFS_MAX_CONNECTIONS=$1 \
        --setenv=RUST_LOG=info \
        $BIN --config $CFG" >/dev/null 2>&1
  sleep 4
  vm "ss -lntp | grep -q ':$PORT'" || { echo "FAIL: hub did not come up with cap=$1"; exit 1; }
}

# Open N concurrent connections from inside the VM and report how many
# stayed open. Python holds them all simultaneously — sequential connects
# would each free their slot and never reach the cap.
probe() { # $1 = how many
  vm "python3 - <<'PY'
import socket
held, refused = [], 0
for i in range($1):
    s = socket.socket()
    s.settimeout(5)
    try:
        s.connect(('127.0.0.1', $PORT))
        # A capped server accepts the TCP handshake (the kernel does that)
        # and then closes. Read to find out which happened: b'' is EOF =
        # refused-by-the-app.
        s.settimeout(1.5)
        try:
            if s.recv(1) == b'':
                refused += 1; s.close(); continue
        except socket.timeout:
            pass          # still open: the server is holding it
        held.append(s)
    except OSError:
        refused += 1
print(f'HELD={len(held)} REFUSED={refused}')
for s in held: s.close()
PY"
}

echo "── arm A: cap = 2, probing 4 connections ──"
start_hub 2
A=$(probe 4); echo "  $A"
A_HELD=$(sed -n 's/.*HELD=\([0-9]*\).*/\1/p' <<<"$A")

echo "── arm B (control): cap DISABLED, same probe ──"
start_hub 0
B=$(probe 4); echo "  $B"
B_HELD=$(sed -n 's/.*HELD=\([0-9]*\).*/\1/p' <<<"$B")

vm "systemctl stop flint-mds-vm 2>/dev/null || true" >/dev/null 2>&1

echo
if [ -z "$A_HELD" ] || [ -z "$B_HELD" ]; then
  echo "VOID: a probe produced no parseable result — the drill did not run"; exit 1
fi
if [ "$B_HELD" -ne 4 ]; then
  echo "VOID: the control arm held $B_HELD/4 with the cap DISABLED."
  echo "      Arm A's refusals cannot be attributed to the cap — something else"
  echo "      is dropping connections. This is not a pass and not a failure."
  exit 1
fi
if [ "$A_HELD" -eq 4 ]; then
  echo "FAIL: cap=2 held all 4 connections — the cap is not enforced."; exit 1
fi
if [ "$A_HELD" -gt 2 ]; then
  echo "FAIL: cap=2 held $A_HELD connections — more than the cap allows."; exit 1
fi
echo "PASS: cap=2 held $A_HELD/4, cap=0 held $B_HELD/4."
echo "      The difference is attributable to FLINT_NFS_MAX_CONNECTIONS and nothing else."
