#!/bin/bash
# Census step 0b, part 3: on a >= 6.9 kernel, knfsd's unlock_filesystem
# revokes NFSv4 state, which 6.8 could not. Question: does the Linux
# client's recovery after SEQ4_STATUS_ADMIN_STATE_REVOKED on ws-a leave
# the open on ws-b (same client, same session) undisturbed?
# Also re-runs the two-export crossing on this kernel.
set -u
# REVOKE=0 is the control arm: the same writes with no revocation must succeed.
# DELEG=0 disables knfsd delegations (fs.leases-enable=0), matching the flint
# hub (delegations off), so locks live on the server with a lock stateid.
REVOKE=${REVOKE:-1}; DELEG=${DELEG:-1}
OUT=/tmp/census-knfsd/out-r${REVOKE}-d${DELEG}; R=/srv/census
LEASES_WAS=$(cat /proc/sys/fs/leases-enable)
sudo umount -f /mnt/ka /mnt/kb /mnt/kroot 2>/dev/null
sudo umount $R/ws-a $R/ws-b 2>/dev/null; sudo rm -rf $OUT $R; mkdir -p $OUT
sudo sysctl -q fs.leases-enable=$DELEG
sudo mkdir -p $R/ws-a $R/ws-b /mnt/ka /mnt/kb /mnt/kroot
sudo mount -t tmpfs -o size=16m census-a $R/ws-a; sudo mount -t tmpfs -o size=16m census-b $R/ws-b
echo a | sudo tee $R/ws-a/f >/dev/null; echo b | sudo tee $R/ws-b/f >/dev/null; sudo chmod -R 777 $R
printf '[nfsd]\nlease-time=15\ngrace-time=15\n' | sudo tee /etc/nfs.conf.d/census.conf >/dev/null
sudo systemctl start nfs-server; sleep 2
sudo exportfs -o rw,fsid=0,no_subtree_check,crossmnt,no_root_squash 127.0.0.1:$R
sudo exportfs -o rw,fsid=11,no_subtree_check,no_root_squash 127.0.0.1:$R/ws-a
sudo exportfs -o rw,fsid=12,no_subtree_check,no_root_squash 127.0.0.1:$R/ws-b
sleep 16   # out of grace before taking locks
cap() { sudo tcpdump -i lo -s 0 -w $OUT/$1.pcap port 2049 >/dev/null 2>&1 & CAP=$!; sleep 1; }
stop() { sleep 1; sudo kill $CAP; wait $CAP 2>/dev/null; }
O="-o nfsvers=4.2,proto=tcp,hard"
cap k1-mounts;   sudo mount -t nfs4 $O 127.0.0.1:/ws-a /mnt/ka; sudo mount -t nfs4 $O 127.0.0.1:/ws-b /mnt/kb
                 sudo mount -t nfs4 $O 127.0.0.1:/ /mnt/kroot; ls -la /mnt/kroot/ws-a /mnt/kroot/ws-b >/dev/null
                 stat -c '%d %n' /mnt/kroot /mnt/kroot/ws-a /mnt/kroot/ws-b; stop
cap k2-locks;    sed -e "s#LOCKED_PATH#$OUT/locked#; s#REVOKED_PATH#$OUT/revoked#" <<'PY' > $OUT/holder.py

import fcntl, time, os
fa = open('/mnt/ka/f', 'r+'); fcntl.lockf(fa, fcntl.LOCK_EX, 1, 0)
fb = open('/mnt/kb/f', 'r+'); fcntl.lockf(fb, fcntl.LOCK_EX, 1, 0)
open('LOCKED_PATH', 'w').close()
while not os.path.exists('REVOKED_PATH'): time.sleep(0.5)
time.sleep(20)
for n, f in (('ws-a', fa), ('ws-b', fb)):
    try:
        f.seek(0); f.write('z'); f.flush(); os.fsync(f.fileno()); print(n, 'write after revoke: ok')
    except OSError as e: print(n, 'write after revoke:', e)
    try:
        fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, 0); print(n, 'lock re-take: ok')
    except OSError as e: print(n, 'lock re-take:', e)
PY
                 python3 $OUT/holder.py > $OUT/holder.txt 2>&1 &
LOCKER=$!
for i in $(seq 1 40); do [ -e $OUT/locked ] && break; sleep 0.5; done; stop
cap k3-revoke-ws-a; sudo dmesg -C
                 if [ "$REVOKE" = 1 ]; then echo $R/ws-a | sudo tee /proc/fs/nfsd/unlock_filesystem >/dev/null; echo "unlock_filesystem rc=$?"; else echo "control: no revoke"; fi
                 touch $OUT/revoked; wait $LOCKER; stop
sudo umount /mnt/kroot/ws-a /mnt/kroot/ws-b 2>/dev/null; sudo umount /mnt/kroot /mnt/ka /mnt/kb
sudo exportfs -u 127.0.0.1:$R/ws-a; sudo exportfs -u 127.0.0.1:$R/ws-b; sudo exportfs -u 127.0.0.1:$R
sudo rm -f /etc/nfs.conf.d/census.conf; sudo systemctl stop nfs-server
sudo umount $R/ws-a $R/ws-b; sudo sysctl -q fs.leases-enable=$LEASES_WAS
echo "--- holder (REVOKE=$REVOKE DELEG=$DELEG)"; cat $OUT/holder.txt; echo "--- dmesg"; sudo dmesg | grep -i nfs | tail -5
sudo chmod -R a+r $OUT
