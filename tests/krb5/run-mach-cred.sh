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
# - All of the above with FLINT_NFS_REQUIRE_MACH_CRED on (the chart's
#   default with Kerberos): the real client asks, so it is not refused.
# - A sec=krb5 mount (no integrity on the data) still gets SP4_MACH_CRED:
#   the kernel does its state management over krb5i whenever rpc.gssd
#   runs, whatever the mount's sec=.
# - Required: without rpc.gssd the client asks for SP4_NONE (sec=sys),
#   and its EXCHANGE_ID is refused, so it cannot mount.
# - Control, the knob off: the same client mounts, and the theft against
#   its SP4_NONE session is not refused by the binding, so the refusals
#   above are the binding's and the knob's.
#
# Needs the flint-nfs-client VM (setup-kdc.sh, setup-pynfs.sh).
set -uo pipefail
PORT=${PORT:-20530}
CTRL_PORT=${CTRL_PORT:-20531}
REQ_PORT=${REQ_PORT:-20532}
NONE_PORT=${NONE_PORT:-20533}
SRV=flintsrv.flint.test
BIN=${BIN:-/tmp/flint-nfs-server}
HERE=$(cd "$(dirname "$0")" && pwd)
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }

# Our servers by pid, never by name (see run-pynfs-gss.sh).
PIDS=()
start() { # <port> <export> <log> [VAR=value...]
  sudo mkdir -p "$2/.flint-nfs"
  echo -n "vol-$1" | sudo tee "$2/.flint-nfs/volume-id" >/dev/null
  sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_GRACE_SECS=5 RUST_LOG=info "${@:4}" \
    "$BIN" --export-path "$2" --volume-id "vol-$1" --bind-addr 0.0.0.0 --port "$1" >>"$3" 2>&1 &
  PIDS+=($!)
  sleep 3
}
cleanup() {
  for m in /mnt/mc /mnt/mc-req /mnt/mc-none /mnt/mc-ctrl; do sudo umount -f "$m" 2>/dev/null; done
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

REQUIRE=FLINT_NFS_REQUIRE_MACH_CRED=true
sudo rm -rf /srv/mc /srv/mc-req /srv/mc-none /srv/mc-ctrl /tmp/mc.log /tmp/mc-req.log /tmp/mc-none.log /tmp/mc-ctrl.log
sudo mkdir -p /mnt/mc /mnt/mc-req /mnt/mc-none /mnt/mc-ctrl
sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -x rpc.gssd 2>/dev/null; sleep 1
sudo sh -c 'rpc.gssd -f > /tmp/gssd-mc.log 2>&1 &'
sleep 2
echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
klist >/dev/null 2>&1; leg $? "testuser has a ticket (the attacker's own)"

start $PORT /srv/mc /tmp/mc.log $REQUIRE
sudo kill -0 "${PIDS[0]}" 2>/dev/null; leg $? "server up on $PORT"
grep -aq 'SP4_MACH_CRED required' /tmp/mc.log; leg $? "the server requires SP4_MACH_CRED"

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
start $PORT /srv/mc /tmp/mc.log $REQUIRE
before=$(grep -ac 'CONN_NOT_BOUND_TO_SESSION' /tmp/mc.log)
echo four | sudo timeout 90 tee /mnt/mc/four >/dev/null
[ "$(sudo timeout 60 cat /mnt/mc/four)" = four ]; leg $? "I/O after the restart"
grep -aq 'SP4_MACH_CRED' /tmp/mc.log; leg $? "the protection is still in force"

echo "--- required: sec=krb5 (no integrity on the data) still asks ---"
sudo umount -f /mnt/mc 2>/dev/null
start $REQ_PORT /srv/mc-req /tmp/mc-req.log $REQUIRE
sudo timeout 60 mount -t nfs4 -o vers=4.2,sec=krb5,port=$REQ_PORT "$SRV:/" /mnt/mc-req
mountpoint -q /mnt/mc-req; leg $? "mounted sec=krb5"
grep -aq 'is SP4_MACH_CRED, bound to gss:' /tmp/mc-req.log; leg $? "its client was made over krb5i, with SP4_MACH_CRED"
sudo umount -f /mnt/mc-req 2>/dev/null

# A server of its own: the kernel would reuse a lingering client of the
# krb5 mount's server, made with SP4_MACH_CRED.
echo "--- required: no rpc.gssd, sec=sys, SP4_NONE is refused ---"
sudo pkill -x rpc.gssd; sleep 1
start $NONE_PORT /srv/mc-none /tmp/mc-none.log $REQUIRE
if sudo timeout 60 mount -t nfs4 -o vers=4.2,sec=sys,port=$NONE_PORT "$SRV:/" /mnt/mc-none; then
  sudo umount -f /mnt/mc-none; false
fi
leg $? "an SP4_NONE client cannot mount"
grep -aq 'asked for SP4_NONE, and SP4_MACH_CRED is required' /tmp/mc-none.log; leg $? "its EXCHANGE_ID was refused"
grep -a 'asked for SP4_NONE' /tmp/mc-none.log | tail -1 | sed 's/^/    /'

echo "--- control, the knob off: no rpc.gssd, sec=sys, SP4_NONE ---"
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
