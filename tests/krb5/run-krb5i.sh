#!/bin/bash
# krb5i: the service that was implemented, unit-tested, and never mounted.
set -uo pipefail
PORT=20501
SRV=flintsrv.flint.test
EXPORT=/srv/flintexport-i
VOL=krb5i-drill
MNT=/mnt/krb5i
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }

sudo mkdir -p "$EXPORT/.flint-nfs"
echo -n "$VOL" | sudo tee "$EXPORT/.flint-nfs/volume-id" >/dev/null
echo hello-from-krb5i | sudo tee "$EXPORT/probe.txt" >/dev/null
sudo mkdir -p "$MNT"

sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -f rpc.gssd 2>/dev/null; sleep 1
sudo rm -f /tmp/gssd-i.log
sudo sh -c 'rpc.gssd -f -vvv -rrr > /tmp/gssd-i.log 2>&1 &'
sleep 2
pgrep -f rpc.gssd >/dev/null; leg $? "rpc.gssd is running"

sudo pkill -f flint-nfs-server 2>/dev/null; sleep 1
sudo rm -f /tmp/flint-i.log
sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab RUST_LOG=info \
  /tmp/flint-nfs-server --export-path "$EXPORT" --volume-id "$VOL" \
  --bind-addr 0.0.0.0 --port $PORT > /tmp/flint-i.log 2>&1 &
sleep 4
pgrep -f flint-nfs-server >/dev/null; leg $? "flint-nfs-server is listening"

echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
klist >/dev/null 2>&1; leg $? "testuser has a TGT"

echo "--- mounting sec=krb5i ---"
sudo timeout 60 mount -t nfs4 -o vers=4.1,sec=krb5i,port=$PORT,soft,timeo=50,retrans=1 \
  "$SRV:/" "$MNT" 2>&1 | tail -5
mountpoint -q "$MNT"; leg $? "mount -o sec=krb5i succeeded"

# ANTI-VACUITY: a mount can succeed having silently negotiated something
# else. Prove the kernel actually bound krb5i, not krb5 or sys.
grep -q "sec=krb5i" /proc/mounts; leg $? "the kernel really bound sec=krb5i (/proc/mounts)"
echo "    /proc/mounts: $(grep krb5i /proc/mounts | head -1)"

if mountpoint -q "$MNT"; then
  sudo timeout 20 ls -la "$MNT" >/dev/null 2>&1; leg $? "readdir over krb5i"
  got=$(sudo timeout 20 cat "$MNT/probe.txt" 2>&1)
  [ "$got" = "hello-from-krb5i" ]; leg $? "read the probe file (got: $got)"
  # A write, which krb5p's drill never did: MICs the call AND the reply.
  echo written-over-krb5i | sudo timeout 20 tee "$MNT/w.txt" >/dev/null 2>&1
  back=$(sudo timeout 20 cat "$MNT/w.txt" 2>&1)
  [ "$back" = "written-over-krb5i" ]; leg $? "write then read back over krb5i (got: $back)"
  sudo timeout 20 umount -f "$MNT" 2>/dev/null
fi

sudo pkill -f flint-nfs-server 2>/dev/null
sudo pkill -f rpc.gssd 2>/dev/null
echo "=== server log: GSS service lines ==="
grep -iE "service|integrity|GSS DATA" /tmp/flint-i.log | tail -6
echo "=== $ok ok, $bad bad ==="
