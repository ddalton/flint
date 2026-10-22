#!/usr/bin/env bash
# kernel-deadline-test.sh <node> -- prove the state-driven-deadline MECHANISM
# (nvme_recovery #4) on a real node kernel, apart from Flint's own recovery.
# Part of restart-gate.sh; first run on bench7, 2026-10-22. A 256 MiB
# malloc bdev is exported on 127.0.0.1 under a NON-Flint NQN (so the agent's
# reaper and loss-detector ignore it), connected with the same-node policy
# (fast_io_fail_tmo off, reconnect-delay 2) but a TEST-sized backstop
# (ctrl-loss-tmo 40), mounted ext4, and written once per 0.2 s with O_DIRECT.
#   A. wait + resume: delete the subsystem for 30 s -> no write may fail; after
#      re-creating it, writes resume within a few seconds.
#   B. backstop: delete it again -> writes fail only at ~ctrl_loss_tmo (30-60 s)
#      and the kernel removes the controller.
#   C. evidence: dirty data on a fresh mount, target gone -> umount blocks;
#      writing delete_controller (what NodeUnstage does when the consumer has
#      left) lets it finish within seconds.
# Prints "SDL-KERNEL A|B|C PASS|FAIL ..." lines and "SDL-KERNEL PASS|FAIL".
set -u
NODE=$1
NS=flint-system Q=nqn.2026-10.io.example:sdl-kernel-test
P=$(kubectl -n $NS get pods -l app=flint-csi-node --field-selector "spec.nodeName=$NODE" -o name)
[ -n "$P" ] || { echo "SDL-KERNEL FAIL: no csi-node pod on $NODE"; exit 1; }
rpc() { kubectl -n $NS exec "$P" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock "$@" < /dev/null; }
drv() { kubectl -n $NS exec "$P" -c flint-csi-driver -- sh -c "$1" < /dev/null; }
export_up() {
  rpc nvmf_create_subsystem $Q -a -s SDLTEST0001 >/dev/null &&
  rpc nvmf_subsystem_add_ns $Q sdlmal >/dev/null &&
  rpc nvmf_subsystem_add_listener $Q -t tcp -a 127.0.0.1 -s 4420 -f ipv4 >/dev/null
}
export_down() { rpc nvmf_delete_subsystem $Q >/dev/null 2>&1; }
CONNECT='hid=$(cat /proc/sys/kernel/random/uuid)
nvme connect -t tcp -a 127.0.0.1 -s 4420 -n '$Q' -q nqn.2014-08.org.nvmexpress:uuid:$hid -I $hid --ctrl-loss-tmo 40 --reconnect-delay 2 >/tmp/sdl.connect 2>&1 || { cat /tmp/sdl.connect; exit 10; }
ctrl=""; dev=""
for i in $(seq 1 50); do
  for c in /sys/class/nvme/nvme*; do [ "$(cat $c/subsysnqn 2>/dev/null)" = '$Q' ] && [ "$(cat $c/state)" = live ] && ctrl=${c##*/}; done
  for b in /sys/block/nvme*; do n=${b##*/}; case $n in *c*) continue;; esac; [ "$(cat $b/device/subsysnqn 2>/dev/null)" = '$Q' ] && dev=/dev/$n; done
  [ -n "$ctrl" ] && [ -n "$dev" ] && break; sleep 0.2; done
[ -n "$ctrl" ] && [ -n "$dev" ] || exit 11
echo -1 > /sys/class/nvme/$ctrl/fast_io_fail_tmo
echo "$ctrl $dev $(cat /sys/class/nvme/$ctrl/fast_io_fail_tmo) $(cat /sys/class/nvme/$ctrl/ctrl_loss_tmo)"'
ctrl_present() { drv 'for c in /sys/class/nvme/nvme*; do [ "$(cat $c/subsysnqn 2>/dev/null)" = '$Q' ] && echo ${c##*/}; done' | head -1; }
now() { drv 'date +%s'; }
fails=0
res() { echo "SDL-KERNEL $*"; case "$*" in *FAIL*) fails=$((fails+1)) ;; esac; }
cleanup() {
  drv 'touch /tmp/sdl.stop; umount -l /mnt/sdl 2>/dev/null; umount -l /mnt/sdl2 2>/dev/null; nvme disconnect -n '$Q' >/dev/null 2>&1; true'
  export_down; rpc bdev_malloc_delete sdlmal >/dev/null 2>&1
}
trap cleanup EXIT

rpc bdev_malloc_create -b sdlmal 256 512 >/dev/null || { echo "SDL-KERNEL FAIL: malloc bdev"; exit 1; }
export_up || { echo "SDL-KERNEL FAIL: export"; exit 1; }
read -r CTRL DEV FIO CLT <<<"$(drv "$CONNECT")"
[ -n "${DEV:-}" ] || { echo "SDL-KERNEL FAIL: connect"; exit 1; }
echo "connected $CTRL $DEV fast_io_fail_tmo=$FIO ctrl_loss_tmo=$CLT"
case "$FIO" in off|-1) ;; *) res "setup FAIL: fast_io_fail_tmo reads $FIO, want off" ;; esac
drv "rm -f /tmp/sdl.ok /tmp/sdl.fail /tmp/sdl.stop; mkfs.ext4 -F -q $DEV && mkdir -p /mnt/sdl && mount $DEV /mnt/sdl && dd if=/dev/zero of=/mnt/sdl/f bs=1M count=8 status=none && sync" \
  || { echo "SDL-KERNEL FAIL: mkfs/mount"; exit 1; }
drv 'setsid nohup sh -c "while [ ! -f /tmp/sdl.stop ]; do if dd if=/dev/zero of=/mnt/sdl/f bs=4k count=1 oflag=direct conv=notrunc status=none 2>/dev/null; then date +%s >> /tmp/sdl.ok; else date +%s >> /tmp/sdl.fail; fi; sleep 0.2; done" >/dev/null 2>&1 &'
sleep 5
[ "$(drv 'wc -l < /tmp/sdl.ok')" -gt 5 ] || { echo "SDL-KERNEL FAIL: writer not writing"; exit 1; }

# ---- A. wait + resume
TA=$(now); export_down
sleep 30
fa=$(drv "awk '\$1>=$TA' /tmp/sdl.fail 2>/dev/null | wc -l")
st=$(drv "cat /sys/class/nvme/$CTRL/state 2>/dev/null")
TR=$(now); export_up || res "A FAIL: re-export"
resumed=""
for i in $(seq 1 30); do
  r=$(drv "awk '\$1>$TR' /tmp/sdl.ok | head -1"); [ -n "$r" ] && { resumed=$((r - TR)); break; }; sleep 1
done
fa2=$(drv "awk '\$1>=$TA' /tmp/sdl.fail 2>/dev/null | wc -l")
if [ "$fa" = 0 ] && [ "$fa2" = 0 ] && [ -n "$resumed" ] && [ "$resumed" -le 10 ]; then
  res "A PASS: target gone 30 s (controller $st), 0 failed writes, resumed ${resumed} s after the export returned"
else
  res "A FAIL: failed writes during=$fa total=$fa2 controller=$st resumed=${resumed:-never}"
fi

# ---- B. backstop
TB=$(now); export_down
first=""
for i in $(seq 1 90); do
  f=$(drv "awk '\$1>=$TB' /tmp/sdl.fail 2>/dev/null | head -1"); [ -n "$f" ] && { first=$((f - TB)); break; }; sleep 1
done
sleep 3; left=$(ctrl_present)
if [ -n "$first" ] && [ "$first" -ge 30 ] && [ "$first" -le 60 ] && [ -z "$left" ]; then
  res "B PASS: writes failed ${first} s after the target went (ctrl_loss_tmo 40), controller removed"
else
  res "B FAIL: first failed write at ${first:-never} s, controller still present=[${left}]"
fi
drv 'touch /tmp/sdl.stop; umount -l /mnt/sdl 2>/dev/null; true'

# ---- C. evidence: delete_controller unblocks a stuck umount
export_up || res "C FAIL: re-export"
read -r CTRL2 DEV2 _ _ <<<"$(drv "$CONNECT")"
if [ -z "${DEV2:-}" ]; then res "C FAIL: reconnect"; else
  drv "mkfs.ext4 -F -q $DEV2 && mkdir -p /mnt/sdl2 && mount $DEV2 /mnt/sdl2 && dd if=/dev/urandom of=/mnt/sdl2/g bs=1M count=64 status=none" || res "C FAIL: mount/write"
  export_down
  drv 'rm -f /tmp/sdlc.rc; setsid nohup sh -c "timeout 90 umount /mnt/sdl2; echo rc=\$? > /tmp/sdlc.rc" >/dev/null 2>&1 &'
  sleep 10
  blocked=$(drv 'test -f /tmp/sdlc.rc && echo no || echo yes')
  TC=$(now); drv "echo 1 > /sys/class/nvme/$CTRL2/delete_controller" 2>/dev/null
  done_s=""
  for i in $(seq 1 20); do
    drv 'test -f /tmp/sdlc.rc' && { done_s=$(( $(now) - TC )); break; }; sleep 1
  done
  if [ "$blocked" = yes ] && [ -n "$done_s" ] && [ "$done_s" -le 15 ]; then
    res "C PASS: umount blocked 10 s with the target gone; finished ${done_s} s after delete_controller ($(drv 'cat /tmp/sdlc.rc'))"
  else
    res "C FAIL: umount blocked=$blocked finished-after-delete=${done_s:-no}"
  fi
fi
[ $fails = 0 ] && echo "SDL-KERNEL PASS" || echo "SDL-KERNEL FAIL ($fails)"
