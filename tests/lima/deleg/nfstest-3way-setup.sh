#!/bin/bash
# Three endpoints on one L2 segment inside the VM, so nfstest's recall
# tests have a genuine second NFS client.
#
#   root ns    10.200.0.1   client 1 — nfstest runs and captures here
#   flintns    10.200.0.2   flint-nfs-server (root, ext4)
#   flintcli2  10.200.0.3   client 2 — sshd here, distinct clientid
#
# WHY A NETNS IS A REAL SECOND CLIENT. Linux keys NFS client state on the
# network namespace: a mount made from inside flintcli2 gets its own
# nfs_client, its own clientid and its own callback channel. For a
# delegation recall that is exactly what "a different client" means. The
# filesystem is NOT namespaced, so the ext4 export is one path for all
# three, and the server still runs on Linux as root.
#
# WHY NOT TWO VMs: lima's vzNAT NATs each guest separately — the host
# reaches both, each guest reaches the gateway, and the guests cannot
# reach each other (ARP fails). Guest-to-guest needs the privileged
# socket_vmnet helper.
set -eu
NS_SRV=flintns
NS_CLI=flintcli2

pkill -f "sshd.*sshd-cli2" 2>/dev/null || true
ip netns del $NS_SRV 2>/dev/null || true
ip netns del $NS_CLI 2>/dev/null || true
ip link del br-flint 2>/dev/null || true
ip link del veth-srv 2>/dev/null || true
ip link del veth-cli 2>/dev/null || true

ip link add br-flint type bridge
ip addr add 10.200.0.1/24 dev br-flint
ip link set br-flint up

ip netns add $NS_SRV
ip link add veth-srv type veth peer name veth-srv-ns
ip link set veth-srv master br-flint
ip link set veth-srv up
ip link set veth-srv-ns netns $NS_SRV
ip netns exec $NS_SRV ip addr add 10.200.0.2/24 dev veth-srv-ns
ip netns exec $NS_SRV ip link set veth-srv-ns up
ip netns exec $NS_SRV ip link set lo up

ip netns add $NS_CLI
ip link add veth-cli type veth peer name veth-cli-ns
ip link set veth-cli master br-flint
ip link set veth-cli up
ip link set veth-cli-ns netns $NS_CLI
ip netns exec $NS_CLI ip addr add 10.200.0.3/24 dev veth-cli-ns
ip netns exec $NS_CLI ip link set veth-cli-ns up
ip netns exec $NS_CLI ip link set lo up

# sshd for client 2. Its children inherit both namespaces, so every
# command nfstest runs over this connection acts as 10.200.0.3 AND sees
# its own mounts.
#
# `unshare --mount` is not optional. A netns isolates the network, which
# is what makes client 2 a distinct NFSv4 client — it does NOT isolate
# mounts. Without a private mount namespace, client 2 mounting the same
# path as client 1 stacks a second NFS superblock on top of client 1's,
# and from then on client 1's file operations travel through client 2's
# mount. The tests still pass; they are just measuring the wrong client.
#
# It runs as a TRANSIENT SYSTEMD UNIT because neither `nohup ... &` nor
# `setsid` survives the ssh session that launches it, and sshd's own
# daemonize forks away from `unshare`, taking the mount namespace with
# it — the listener then never comes up at all.
mkdir -p /run/sshd
systemctl stop sshd-cli2 2>/dev/null || true
systemd-run --unit=sshd-cli2 --collect \
  ip netns exec $NS_CLI unshare --mount --propagation private \
    /usr/sbin/sshd -D \
      -o ListenAddress=10.200.0.3 \
      -o Port=22 \
      -o UsePAM=yes

sleep 1
echo "topology up:"
ip netns exec $NS_SRV ip -4 -o addr show veth-srv-ns | awk '{print "  server  " $4}'
ip netns exec $NS_CLI ip -4 -o addr show veth-cli-ns | awk '{print "  client2 " $4}'
ip -4 -o addr show br-flint | awk '{print "  client1 " $4}'
ping -c1 -W2 10.200.0.2 >/dev/null && echo "  ping server OK"
ping -c1 -W2 10.200.0.3 >/dev/null && echo "  ping client2 OK"
ip netns exec $NS_CLI ss -ltn | grep -q ":22" && echo "  sshd on client2 OK"

# PROVE the mount isolation rather than assume it. If client 2's mounts
# were visible here, its NFS mount would shadow client 1's at the same
# path and every later result would be about the wrong client.
mkdir -p /mnt/isotest
if ssh -o BatchMode=yes -o StrictHostKeyChecking=no 10.200.0.3 \
     "sudo mount -t tmpfs none /mnt/isotest" >/dev/null 2>&1; then
  if mount | grep -q "/mnt/isotest"; then
    echo "  ✗ MOUNT ISOLATION FAILED — client 2's mounts are visible to client 1"
    exit 1
  fi
  ssh -o BatchMode=yes -o StrictHostKeyChecking=no 10.200.0.3 \
    "sudo umount /mnt/isotest" >/dev/null 2>&1 || true
  echo "  mount isolation OK — client 2's mounts are private"
else
  echo "  (mount-isolation probe skipped: ssh to client 2 not ready yet)"
fi
