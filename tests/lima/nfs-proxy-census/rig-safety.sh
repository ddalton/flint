# Sourced by the nfs-proxy-census rigs: two ways a rig has hurt the box.
# Checked by rig-safety-check.sh, each with an arm that shows the old way failing.

# unmount_hard MNT: take an NFS mount down so that nothing keeps retrying
# its server. A lazy unmount (`umount -l`) only detaches the name. While
# any process still holds a file, a cwd or a lock under it, the superblock
# and its NFS client live on, and once the rig deletes the server they
# retry it for as long as the box is up. On 2026-09-30, after
# `umount -f -l /mnt/px5` at 18:14, the box logged 2,823 "nfs: server
# 172.18.0.4 not responding" lines over five hours. So:
# - kill the holders (SIGKILL: hard-mount waits are killable, not
#   interruptible);
# - unmount for real, then with -f to abort in-flight RPCs;
# - detach lazily only as the last resort, and say so.
# `fuser -M` acts only if MNT is a mount point; without it, `-k -m` on a
# plain directory kills every process on the parent filesystem.
unmount_hard() {
  local m=$1 i
  for i in 1 2 3; do
    timeout 5 mountpoint -q "$m" || return 0
    sudo timeout 10 fuser -s -M -k -KILL -m "$m" 2>/dev/null
    sleep 1
    sudo timeout 30 umount "$m" 2>/dev/null && return 0
    sudo timeout 30 umount -f "$m" 2>/dev/null && return 0
  done
  echo "WARN: $m is still busy after killing its holders; detaching lazily, and its NFS client may keep retrying the server:" >&2
  sudo timeout 10 fuser -v -M -m "$m" >&2
  sudo umount -f -l "$m"
  return 1
}

# arp_guard: exit the rig before the host's neighbor table fills. There is
# ONE table for every network namespace on the host. kindnet adds entries
# for each pod in both its node's namespace and its own. At gc_thresh3
# the host can no longer resolve its own LAN neighbors and drops off the
# network: ssh times out, ping gets no replies, and the kernel keeps
# running. In phase D run 1 (2026-09-30) that came at ~350 pods with the
# default limit of 1024, and looked like a hang until the journal showed
# `neighbor table overflow`. The first field of /proc/net/stat/arp_cache
# is that table's entry count, in hex.
arp_guard() {  # [limit]: exits at >= 75% of it (default gc_thresh3)
  local n max=${1:-$(cat /proc/sys/net/ipv4/neigh/default/gc_thresh3)}
  n=$((16#$(awk 'NR==2 {print $1}' /proc/net/stat/arp_cache)))
  if [ $((n * 4)) -ge $((max * 3)) ]; then
    echo "ABORT: $n ARP neighbor entries against gc_thresh3=$max; the host is about to lose its network"
    exit 1
  fi
}
