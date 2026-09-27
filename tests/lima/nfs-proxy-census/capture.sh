#!/bin/bash
# Census step 0b: the Linux client's COMPOUND shapes against a pseudo-root
# with two exports (ws-a, ws-b) — the layout the proxy will present.
set -u
OUT=/tmp/census; rm -rf $OUT; mkdir -p $OUT
R=/srv/census
sudo umount $R/ws-a $R/ws-b 2>/dev/null; sudo rm -rf $R; sudo mkdir -p $R/ws-a $R/ws-b
# SEPARATE filesystems: a subdirectory of the root export is not a crossing
sudo mount -t tmpfs -o size=16m census-a $R/ws-a; sudo mount -t tmpfs -o size=16m census-b $R/ws-b
sudo mkdir -p $R/ws-a/sub
echo hello | sudo tee $R/ws-a/f >/dev/null; echo x | sudo tee $R/ws-a/sub/g >/dev/null
sudo chmod -R 777 $R
# short lease/grace so the recovery legs finish in seconds
printf '[nfsd]\nlease-time=15\ngrace-time=15\n' | sudo tee /etc/nfs.conf.d/census.conf >/dev/null
sudo systemctl restart nfs-server; sleep 2
sudo exportfs -o rw,fsid=0,no_subtree_check,crossmnt,no_root_squash 127.0.0.1:$R
sudo exportfs -o rw,fsid=11,no_subtree_check,no_root_squash 127.0.0.1:$R/ws-a
sudo exportfs -o rw,fsid=12,no_subtree_check,no_root_squash 127.0.0.1:$R/ws-b
sudo exportfs -v | grep census
sudo mkdir -p /mnt/pa /mnt/pb /mnt/root
cap() { sudo tcpdump -i lo -s 0 -w $OUT/$1.pcap port 2049 >/dev/null 2>&1 & CAP=$!; sleep 1; }
stop() { sleep 1; sudo kill $CAP; wait $CAP 2>/dev/null; }
O="-o nfsvers=4.2,proto=tcp,hard"

cap s1-pv-mount;      sudo mount -t nfs4 $O 127.0.0.1:/ws-a /mnt/pa; ls -la /mnt/pa >/dev/null; stop
cap s2-second-mount;  sudo mount -t nfs4 $O 127.0.0.1:/ws-b /mnt/pb; ls -la /mnt/pb >/dev/null; stop
cap s3-root-readdir;  sudo mount -t nfs4 $O 127.0.0.1:/ /mnt/root; ls -la /mnt/root; stop
cap s4-crossing;      ls -la /mnt/root/ws-a /mnt/root/ws-b >/dev/null; stat -c '%d %n' /mnt/root /mnt/root/ws-a /mnt/root/ws-b; stop
cap s5-rename-within; mv /mnt/pa/f /mnt/pa/f2; mv /mnt/pa/f2 /mnt/pa/f; stop
cap s6-rename-across; python3 -c "import os
try: os.rename('/mnt/root/ws-a/f','/mnt/root/ws-b/f'); print('rename OK (!)')
except OSError as e: print('rename:', e)"; stop
cap s7-lookupp;       sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null; ls /mnt/pa/sub/.. >/dev/null; cd /mnt/pa/sub && ls .. >/dev/null; cd /; stop
cap s8-open-lock;     python3 - <<'PY' &
import fcntl, time
f = open('/mnt/pa/f', 'r+'); fcntl.lockf(f, fcntl.LOCK_EX, 1, 0)
open('/tmp/census/locked', 'w').close()
time.sleep(60)
try:
    f.seek(0); print('read after:', f.read().strip())
except OSError as e: print('read after:', e)
try:
    f.seek(0); f.write('y'); f.flush(); import os; os.fsync(f.fileno()); print('write after: ok')
except OSError as e: print('write after:', e)
PY
LOCKER=$!
for i in $(seq 1 20); do [ -e $OUT/locked ] && break; sleep 0.5; done; stop
cap s9-admin-revoke;  echo $R/ws-a | sudo tee /proc/fs/nfsd/unlock_filesystem >/dev/null; echo "unlock_filesystem rc=$?"; sleep 25; stop
cap s10-server-restart; sudo systemctl restart nfs-server; sleep 2
  sudo exportfs -o rw,fsid=0,no_subtree_check,crossmnt,no_root_squash 127.0.0.1:$R
  sudo exportfs -o rw,fsid=11,no_subtree_check,no_root_squash 127.0.0.1:$R/ws-a
  sudo exportfs -o rw,fsid=12,no_subtree_check,no_root_squash 127.0.0.1:$R/ws-b
  wait $LOCKER; sleep 2; ls /mnt/pb >/dev/null; stop
cap s11-umount;       sudo umount /mnt/root/ws-a /mnt/root/ws-b 2>/dev/null; sudo umount /mnt/root /mnt/pb /mnt/pa; stop

# teardown: back to the VM's own exports and defaults
sudo exportfs -u 127.0.0.1:$R/ws-a; sudo exportfs -u 127.0.0.1:$R/ws-b; sudo exportfs -u 127.0.0.1:$R
sudo rm -f /etc/nfs.conf.d/census.conf; sudo systemctl restart nfs-server; sudo umount $R/ws-a $R/ws-b
sudo exportfs -v | grep -c census || true
mount | grep -c ' type nfs4' || true
ls -la $OUT
