#!/bin/bash
# SP4_MACH_CRED against the real kernel client (docs/nfs-kerberos.md,
# "Sessions are bound to their machine").
#
# - A sec=krb5i mount asks for SP4_MACH_CRED, and gets it.
# - The client survives a killed connection and a server restart: each
#   time its SEQUENCE on the new connection is refused, it binds the
#   connection with its machine key, and carries on.
# - A user with a valid ticket who has the session id (as a sniffer under
#   krb5i would) is refused on its own connection.
# - Control: the same theft against an SP4_NONE client is not refused by
#   the binding, so the refusals above are the binding's. The kernel does
#   its state management over krb5i whenever rpc.gssd runs, whatever the
#   mount's sec=, so the control stops rpc.gssd and mounts sec=sys.
#
# Needs the flint-nfs-client VM (setup-kdc.sh, setup-pynfs.sh).
set -uo pipefail
PORT=${PORT:-20530}
CTRL_PORT=${CTRL_PORT:-20531}
SRV=flintsrv.flint.test
BIN=${BIN:-/tmp/flint-nfs-server}
HERE=$(cd "$(dirname "$0")" && pwd)
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }

# Our servers by pid, never by name (see run-pynfs-gss.sh).
PIDS=()
start() { # <port> <export> <log>
  sudo mkdir -p "$2/.flint-nfs"
  echo -n "vol-$1" | sudo tee "$2/.flint-nfs/volume-id" >/dev/null
  sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_GRACE_SECS=5 RUST_LOG=info \
    "$BIN" --export-path "$2" --volume-id "vol-$1" --bind-addr 0.0.0.0 --port "$1" >>"$3" 2>&1 &
  PIDS+=($!)
  sleep 3
}
cleanup() {
  for m in /mnt/mc /mnt/mc-ctrl; do sudo umount -f "$m" 2>/dev/null; done
  for p in "${PIDS[@]}"; do sudo kill "$p" 2>/dev/null; done
}
trap cleanup EXIT

# The session id the server logged for its newest session, as hex.
last_session() { # <log>
  grep -a 'CREATE_SESSION: Session SessionId' "$1" | tail -1 |
    sed -E 's/.*SessionId\(\[([0-9, ]+)\]\).*/\1/' |
    python3 -c 'import sys; print(bytes(int(x) for x in sys.stdin.read().split(",")).hex())'
}
steal() { # <port> <session hex>
  (cd /opt/pynfs/nfs4.1 && PYTHONPATH=/opt/pynfs:/opt/pynfs/nfs4.1 /opt/pynfs/.venv/bin/python \
    "$HERE/steal_session.py" "$SRV" "$1" "$2" 2>&1)
}

sudo rm -rf /srv/mc /srv/mc-ctrl /tmp/mc.log /tmp/mc-ctrl.log
sudo mkdir -p /mnt/mc /mnt/mc-ctrl
sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -x rpc.gssd 2>/dev/null; sleep 1
sudo sh -c 'rpc.gssd -f > /tmp/gssd-mc.log 2>&1 &'
sleep 2
echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
klist >/dev/null 2>&1; leg $? "testuser has a ticket (the attacker's own)"

start $PORT /srv/mc /tmp/mc.log
sudo kill -0 "${PIDS[0]}" 2>/dev/null; leg $? "server up on $PORT"

echo "--- sec=krb5i mount ---"
sudo timeout 60 mount -t nfs4 -o vers=4.2,sec=krb5i,port=$PORT "$SRV:/" /mnt/mc
mountpoint -q /mnt/mc; leg $? "mounted sec=krb5i"
grep -aq 'is SP4_MACH_CRED, bound to gss:' /tmp/mc.log; leg $? "the client asked for and got SP4_MACH_CRED"
grep -a 'is SP4_MACH_CRED' /tmp/mc.log | tail -1 | sed 's/^/    /'
echo one | sudo tee /mnt/mc/one >/dev/null
[ "$(sudo cat /mnt/mc/one)" = one ]; leg $? "write and read"

echo "--- the connection dies ---"
before=$(grep -ac 'CONN_NOT_BOUND_TO_SESSION' /tmp/mc.log)
sudo ss -K "( dport = :$PORT )" >/dev/null 2>&1
echo two | sudo timeout 60 tee /mnt/mc/two >/dev/null
[ "$(sudo timeout 60 cat /mnt/mc/two)" = two ]; leg $? "I/O after the connection was killed"
after=$(grep -ac 'CONN_NOT_BOUND_TO_SESSION' /tmp/mc.log)
[ "$after" -gt "$before" ]; leg $? "the new connection was refused until bound ($before -> $after)"
grep -aE 'BIND_CONN_TO_SESSION: (Session found|registered)' /tmp/mc.log | tail -1 | grep -q .
leg $? "the client bound it with its machine key"

echo "--- someone else with the session id ---"
sid=$(last_session /tmp/mc.log)
echo "    sniffed session $sid"
out=$(steal $PORT "$sid"); echo "$out" | sed 's/^/    /'
echo "$out" | grep -q '^sequence NFS4ERR_CONN_NOT_BOUND_TO_SESSION$'; leg $? "SEQUENCE on the thief's connection: refused"
echo "$out" | grep -q '^bind_conn NFS4ERR_WRONG_CRED$'; leg $? "binding with the thief's own ticket: refused"
echo "$out" | grep -q '^destroy_session NFS4ERR_WRONG_CRED$'; leg $? "destroying the session: refused"
echo three | sudo tee /mnt/mc/three >/dev/null
[ "$(sudo timeout 60 cat /mnt/mc/three)" = three ]; leg $? "the mount is unharmed"

echo "--- the server restarts (sessions persist, connections do not) ---"
sudo kill "${PIDS[0]}"; sleep 1
start $PORT /srv/mc /tmp/mc.log
before=$(grep -ac 'CONN_NOT_BOUND_TO_SESSION' /tmp/mc.log)
echo four | sudo timeout 90 tee /mnt/mc/four >/dev/null
[ "$(sudo timeout 60 cat /mnt/mc/four)" = four ]; leg $? "I/O after the restart"
grep -aq 'SP4_MACH_CRED' /tmp/mc.log; leg $? "the protection is still in force"

echo "--- control: no rpc.gssd, sec=sys, SP4_NONE ---"
sudo umount -f /mnt/mc 2>/dev/null
sudo pkill -x rpc.gssd; sleep 1
start $CTRL_PORT /srv/mc-ctrl /tmp/mc-ctrl.log
sudo timeout 60 mount -t nfs4 -o vers=4.2,sec=sys,port=$CTRL_PORT "$SRV:/" /mnt/mc-ctrl
mountpoint -q /mnt/mc-ctrl; leg $? "mounted sec=sys"
! grep -aq 'is SP4_MACH_CRED' /tmp/mc-ctrl.log; leg $? "an AUTH_SYS client is SP4_NONE"
sudo ls /mnt/mc-ctrl >/dev/null
out=$(steal $CTRL_PORT "$(last_session /tmp/mc-ctrl.log)"); echo "$out" | sed 's/^/    /'
! echo "$out" | grep -q '^sequence NFS4ERR_CONN_NOT_BOUND_TO_SESSION$'
leg $? "an SP4_NONE session is not refused by the binding (the reason for the krb5i floor)"

echo "=== $ok ok, $bad bad ==="
[ "$bad" = 0 ]
