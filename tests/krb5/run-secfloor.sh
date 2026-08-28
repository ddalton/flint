#!/bin/bash
# The security floor: does FLINT_NFS_MIN_SEC actually REFUSE, or is it a
# knob that exists and does nothing?
#
# Reshaped 2026-08-27 after the first run's ANTI-VACUITY leg failed: a
# krb5p floor refused its own sec=krb5p mount, because a Linux client
# runs NFSv4 state management over krb5i whatever the data flavor is.
# krb5p is now refused AS A FLOOR at startup, and krb5i is the strongest
# usable one.
set -uo pipefail
SRV=flintsrv.flint.test
EXPORT=/srv/flintexport-s
VOL=secfloor-drill
MNT=/mnt/secfloor
BIN=${BIN:-/tmp/flint-nfs-server-new}
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }

sudo mkdir -p "$EXPORT/.flint-nfs"; echo -n "$VOL" | sudo tee "$EXPORT/.flint-nfs/volume-id" >/dev/null
echo floor-probe | sudo tee "$EXPORT/probe.txt" >/dev/null
sudo mkdir -p "$MNT"
sudo pkill -f rpc.gssd 2>/dev/null; sleep 1
sudo sh -c 'rpc.gssd -f -vvv -rrr > /tmp/gssd-s.log 2>&1 &'; sleep 2
echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1

start() {
  sudo pkill -f flint-nfs-server 2>/dev/null; sleep 1
  if [ -z "$1" ]; then
    sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab RUST_LOG=info "$BIN" \
      --export-path "$EXPORT" --volume-id "$VOL" --bind-addr 0.0.0.0 --port "$2" > "$3" 2>&1 &
  else
    sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_MIN_SEC="$1" RUST_LOG=info "$BIN" \
      --export-path "$EXPORT" --volume-id "$VOL" --bind-addr 0.0.0.0 --port "$2" > "$3" 2>&1 &
  fi
  sleep 4
}
try_mount() {
  sudo umount -f "$MNT" 2>/dev/null
  sudo timeout 30 mount -t nfs4 -o vers=4.1,sec=$1,port=$2,soft,timeo=25,retrans=1 \
    "$SRV:/" "$MNT" >/dev/null 2>&1
  mountpoint -q "$MNT"
}

echo "--- S1: no floor, sec=sys still works (the shipped default is unchanged) ---"
start "" 20540 /tmp/f-s1.log
try_mount sys 20540; leg $? "unset floor: sec=sys mounts"
sudo umount -f "$MNT" 2>/dev/null

echo "--- S2: floor = krb5i ---"
start krb5i 20541 /tmp/f-s2.log
grep -q "minimum security flavor: krb5i" /tmp/f-s2.log; leg $? "server logged the floor it enforces"
try_mount sys 20541; [ $? -ne 0 ]; leg $? "S2a: sec=sys REFUSED under a krb5i floor"
# ANTI-VACUITY: the refusal above passes just as well against a dead
# server. Mount an ADMITTED flavor on the SAME process to prove it lives.
try_mount krb5i 20541; leg $? "S2b: sec=krb5i mounts on the SAME server (so S2a was the floor, not a corpse)"
got=$(sudo timeout 20 cat "$MNT/probe.txt" 2>&1); [ "$got" = "floor-probe" ]
leg $? "S2c: and it serves data (got: $got)"
sudo umount -f "$MNT" 2>/dev/null
# And the flavor ABOVE the floor must still be admitted end to end.
try_mount krb5p 20541; leg $? "S2d: sec=krb5p mounts too (privacy is above the floor, not beside it)"
sudo umount -f "$MNT" 2>/dev/null
grep -q "Refusing sec=sys" /tmp/f-s2.log; leg $? "server logged the refusal by name"
echo "    $(grep -m1 -o 'Refusing sec=.*' /tmp/f-s2.log)"

echo "--- S3: krb5p is refused AS A FLOOR, at startup ---"
sudo pkill -f flint-nfs-server 2>/dev/null; sleep 1
sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_MIN_SEC=krb5p "$BIN" \
  --export-path "$EXPORT" --volume-id "$VOL" --bind-addr 0.0.0.0 --port 20542 > /tmp/f-s3.log 2>&1
rc=$?; [ $rc -ne 0 ]; leg $? "S3a: FLINT_NFS_MIN_SEC=krb5p refuses to start (exit $rc)"
grep -q "state management" /tmp/f-s3.log; leg $? "S3b: and the error explains WHY, not just THAT"
grep -q "krb5i" /tmp/f-s3.log; leg $? "S3c: and points at the usable floor"

echo "--- S4: a typo must not silently mean no floor ---"
sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab FLINT_NFS_MIN_SEC=krb5ii "$BIN" \
  --export-path "$EXPORT" --volume-id "$VOL" --bind-addr 0.0.0.0 --port 20543 > /tmp/f-s4.log 2>&1
rc=$?; [ $rc -ne 0 ]; leg $? "S4a: a typo refuses to start (exit $rc)"
grep -q "FLINT_NFS_MIN_SEC" /tmp/f-s4.log; leg $? "S4b: and names the variable"

sudo pkill -f flint-nfs-server 2>/dev/null; sudo pkill -f rpc.gssd 2>/dev/null
echo "=== $ok ok, $bad bad ==="
