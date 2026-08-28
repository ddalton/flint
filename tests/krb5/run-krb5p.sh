#!/bin/bash
# The krb5p interop drill: a real Linux NFS client, over RPCSEC_GSS
# privacy, against flint — with a capture.
set -uo pipefail
PORT=20499
SRV=flintsrv.flint.test
EXPORT=/srv/flintexport
VOL=krb5p-drill
MNT=/mnt/krb5p
PCAP=/tmp/krb5p.pcap
ok=0; bad=0
leg() { if [ "$1" = 0 ]; then echo "  ok   $2"; ok=$((ok+1)); else echo "  BAD  $2"; bad=$((bad+1)); fi; }

# F30: the export must prove its identity before a byte is served.
sudo mkdir -p "$EXPORT/.flint-nfs"
echo -n "$VOL" | sudo tee "$EXPORT/.flint-nfs/volume-id" >/dev/null
echo hello-from-krb5p | sudo tee "$EXPORT/probe.txt" >/dev/null
sudo mkdir -p "$MNT"

# Verbose, in the foreground, so its rejection reason is visible.
sudo systemctl stop rpc-gssd 2>/dev/null; sudo pkill -f rpc.gssd 2>/dev/null; sleep 1
sudo rm -f /tmp/gssd.log
sudo sh -c 'rpc.gssd -f -vvv -rrr > /tmp/gssd.log 2>&1 &'
sleep 2
pgrep -f rpc.gssd >/dev/null; leg $? "rpc.gssd is running (verbose)"

sudo pkill -f flint-nfs-server 2>/dev/null; sleep 1
sudo rm -f /tmp/flint-nfs.log
sudo tcpdump -i lo -s 0 -w "$PCAP" "port $PORT" >/dev/null 2>&1 &
TCPDUMP=$!
sleep 1

sudo env KRB5_KTNAME=/tmp/flint-nfs.keytab RUST_LOG=info \
  /tmp/flint-nfs-server --export-path "$EXPORT" --volume-id "$VOL" \
  --bind-addr 0.0.0.0 --port $PORT > /tmp/flint-nfs.log 2>&1 &
sleep 4
pgrep -f flint-nfs-server >/dev/null; leg $? "flint-nfs-server is listening"

echo "--- server said ---"; tail -5 /tmp/flint-nfs.log

echo flintflint | kinit testuser@FLINT.TEST >/dev/null 2>&1
klist >/dev/null 2>&1; leg $? "testuser has a TGT"

echo "--- mounting sec=krb5p ---"
sudo timeout 60 mount -t nfs4 -o vers=4.1,sec=krb5p,port=$PORT,soft,timeo=50,retrans=1 \
  "$SRV:/" "$MNT" 2>&1 | tail -5
mountpoint -q "$MNT"; leg $? "mount -o sec=krb5p succeeded"

if mountpoint -q "$MNT"; then
  sudo timeout 20 ls -la "$MNT" 2>&1 | head -5; leg $? "readdir over krb5p"
  got=$(sudo timeout 20 cat "$MNT/probe.txt" 2>&1)
  [ "$got" = "hello-from-krb5p" ]; leg $? "read the probe file (got: $got)"
  sudo timeout 20 umount -f "$MNT" 2>/dev/null
fi

echo "--- /proc/mounts ---"; grep krb5p /proc/mounts || echo "(unmounted)"
sleep 1; sudo kill $TCPDUMP 2>/dev/null; sleep 1
sudo pkill -f flint-nfs-server 2>/dev/null
echo "=== rpc.gssd said ==="; sudo tail -40 /tmp/gssd.log
sudo pkill -f rpc.gssd 2>/dev/null
echo "=== pcap ==="; sudo ls -l "$PCAP"
echo "=== $ok ok, $bad bad ==="
