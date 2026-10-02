#!/bin/bash
# Checks rig-safety.sh on the box, each piece with an arm that shows the
# failure it guards against.
#   BIN=<dir with flint-pnfs-mds> bash rig-safety-check.sh
# 1. unmount_hard: a hard mount whose holder ignores SIGTERM, then the
#    server is killed. The OLD sequence (kill the holder, then
#    `umount || umount -f -l`) leaves an NFS client retrying the dead
#    server. That arm must show the leak, or the check proves nothing.
#    unmount_hard must leave no client behind.
# 2. arp_guard: the host's neighbor table counts entries in other network
#    namespaces (the premise of the guard), and the guard fires at 75%.
set -u
cd "$(dirname "$0")" && source ./rig-safety.sh
BIN=${BIN:?BIN=<dir with flint-pnfs-mds>}
ROOT=$HOME/rig-safety; MNT=/mnt/rsc; HP=20595; HPX=$(printf %x $HP)
exec 9>$HOME/.rig-safety.lock
flock -n 9 || { echo "another run holds the lock"; exit 1; }
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
HUB=; H=
hub_up() {
  sudo setsid $BIN/flint-pnfs-mds --config $ROOT/mds.yaml >> $ROOT/hub.log 2>&1 < /dev/null & disown
  for _ in $(seq 1 50); do timeout 1 bash -c "echo > /dev/tcp/127.0.0.1/$HP" 2>/dev/null && break; sleep 0.2; done
  HUB=$(pgrep -f "^$BIN/flint-pnfs-mds --config $ROOT/mds.yaml" | head -1)
}
hub_down() { [ -n "$HUB" ] && sudo kill -KILL $HUB 2>/dev/null; HUB=; sleep 1; }
client_lingers() { grep -qi " $HPX " /proc/fs/nfsfs/servers; }
cleanup() {
  [ -z "$HUB" ] && hub_up   # a live server lets a leaked client tear down cleanly
  [ -n "$H" ] && sudo kill -KILL $H 2>/dev/null
  timeout 5 mountpoint -q $MNT && sudo umount -f -l $MNT
  for _ in $(seq 1 30); do client_lingers || break; sleep 1; done
  hub_down
  sudo ip netns del rsc 2>/dev/null
}
trap cleanup EXIT INT TERM
sudo rm -rf $ROOT; mkdir -p $ROOT/data/exports $ROOT/data/state; sudo mkdir -p $MNT
cat > $ROOT/mds.yaml <<Y
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "127.0.0.1", port: $HP }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: $ROOT/data/state/state.db } }
exports:
  - path: $ROOT/data/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [ { network: 0.0.0.0/0, permissions: rw } ]
logging: { level: "info", format: text }
Y
client_lingers && { echo "a client for port $HP already exists; refusing to run"; exit 1; }

arm() {  # $1 = old | new
  hub_up
  sudo timeout 30 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=$HP 127.0.0.1:/ $MNT || { echo "mount failed"; exit 1; }
  echo held | sudo tee $MNT/f > /dev/null
  # The holder ignores SIGTERM and keeps a file open under the mount.
  sudo rm -f $ROOT/holder.pid
  sudo setsid python3 -c "import os, signal, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
f = open('$MNT/f')
open('$ROOT/holder.pid', 'w').write(str(os.getpid()))
time.sleep(86400)" < /dev/null > /dev/null 2>&1 & disown
  for _ in $(seq 1 20); do [ -s $ROOT/holder.pid ] && break; sleep 0.2; done
  H=$(cat $ROOT/holder.pid 2>/dev/null)
  [ -n "$H" ] && [ -d /proc/$H ] || { echo "the holder did not start"; exit 1; }
  case $1 in
    old) sudo kill $H; sudo timeout 30 umount $MNT || sudo umount -f -l $MNT ;;
    new) unmount_hard $MNT ;;
  esac
  sleep 2
  hub_down          # the rig deletes the server
  # The retries come from lease renewal, about once a minute. The old arm
  # waits for the first one; the new arm watches twice as long, or its
  # "no retries" would hold for want of waiting.
  local t0=$(date +%s) m=$(retries)
  case $1 in
    old) while [ $(retries) -eq $m ] && [ $(( $(date +%s) - t0 )) -lt 300 ]; do sleep 5; done
         RETRY_S=$(( $(date +%s) - t0 )); echo "first retry of the dead server after ${RETRY_S}s" ;;
    new) sleep $(( RETRY_S * 2 > 60 ? RETRY_S * 2 : 60 )) ;;
  esac
}
retries() { sudo dmesg | grep -c "nfs: server 127.0.0.1 not responding"; }
RETRY_S=150

echo "== unmount, OLD sequence (the known-bad arm)"
M0=$(retries)
arm old
check "old: the mount is gone from the namespace" '! mountpoint -q $MNT'
check "old: but its NFS client lingers against the dead server (the leak)" 'client_lingers'
check "old: and the kernel retries it" '[ $(retries) -gt $M0 ]'
cleanup; H=
check "old arm cleaned up: the leaked client is gone" '! client_lingers'

echo "== unmount, unmount_hard"
M1=$(retries)
arm new
check "new: the mount is gone" '! mountpoint -q $MNT'
check "new: the holder was killed" '[ ! -d /proc/$H ]'
check "new: no NFS client left for the server" '! client_lingers'
check "new: no retries of the dead server" '[ $(retries) -eq $M1 ]'
H=

echo "== arp_guard"
entries() { echo $((16#$(awk 'NR==2 {print $1}' /proc/net/stat/arp_cache))); }
sudo ip netns add rsc && sudo ip -n rsc link add name rsa type veth peer name rsb && sudo ip -n rsc link set rsa up && sudo ip -n rsc link set rsb up || { echo "netns setup failed"; exit 1; }
E0=$(entries)
for i in $(seq 1 100); do sudo ip -n rsc neigh add 10.99.$((i / 250)).$((i % 250 + 1)) lladdr 02:00:00:00:00:01 dev rsa nud permanent; done
E1=$(entries)
echo "host table entries: $E0 before, $E1 after 100 entries in another namespace"
check "the host's table counts another namespace's entries (+100)" '[ $((E1 - E0)) -ge 100 ]'
check "the guard passes well under the limit" '( arp_guard $((E1 * 4)) ) > /dev/null'
check "the guard fires at 75% of the limit" '! ( arp_guard $((E1 * 4 / 3)) ) > /dev/null'
sudo ip netns del rsc

echo "RESULT: $ok passed, $bad failed"
