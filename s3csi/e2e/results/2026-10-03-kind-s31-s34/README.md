# Kind S31 + S34, 2026-10-03 — the box (10.0.0.249)

Images built on the box from `612a17f5` (tag `pt1003`); one kind node
(`kindest/node:v1.34.0`), RustFS behind the `minio` Service. The box was
shared with two TLC runs and another kind rig (load ~11 throughout).

| run | log | rig | result |
|---|---|---|---|
| 1 | `run1-legs-S31-S34.log` | as pushed in `612a17f5` | S31 23/24, S34 vacuous — **both rig bugs**, below |
| 2 | `run2-legs-S31-S34.log` | both rig bugs fixed | **S31 24/24**; S34 shaped (20mbit, 4 × 64 MiB) but vacuous: statfs max 23 ms |
| 3 | `run3-legs-S34-24-readers.log` | S34 alone, 24 × 8 MiB at 10mbit | **S34 FAILS — the defect:** statfs max 3971 ms, the join replaced the live mounter, 24/24 readers ENOTCONN |

## Run 1's two rig bugs

- **S31 step 6 re-offered the wrong generation.** It patched only the
  key id, but step 5 had left the Secret at generation 5 with an unknown
  envelope key — so the offer was generation 5 again, refused again for
  that key, and (refusals are said once) silently. The door was unmoved,
  as it would be either way. Step 6 now writes generation 4 whole, with
  its expiration and envelope, changing only the key id. Run 2:
  "generation 4 re-offered with its expiration and OTHER keys is
  refused, and the refusal says to mint under 5".
- **S34 never shaped the link.** It read the pod's veth peer with
  `nsenter -t <pid> -n cat /sys/class/net/eth0/iflink`; sysfs belongs to
  the caller's MOUNT namespace, so that is the kind node's own eth0 (peer
  47, on the host) and no veth matched. Checked on the box with a
  throwaway container: sysfs said 68 (the caller's), netlink said
  `eth0@if67`. The leg now parses `ip -o link show eth0` in the netns.

## What S34 found

Network-bound load does not reach the probe: four readers saturating a
20mbit link for ~100 s left statfs at ≤ 23 ms, because Mountpoint
answers statfs without S3. FUSE-thread exhaustion does: Mountpoint
serves FUSE on 16 threads by default (the plugin passes no
`--max-threads`), and with 24 readers each parked on a slow read the
statfs request queues behind them —

    node statfs on the shared source during the reads: 24 samples, max 3971 ms:
    1185 3971 2138 1 1 1 ...

— the join's `shared_mount_alive` (3 s, via `fuse::wait_ready_opts`,
which returns the same `Err` for a timeout as for ENOTCONN) read the
serving mounter as dead and replaced it: one "not serving; replacing it
for the new member" line, a new worker uid, and every reader
`cat: read error: Transport endpoint is not connected`. No reader had a
MounterDead event by the time the leg read them.

The leg's defaults are now run 3's (24 / 8 MiB / 10mbit), so it fails
until the join path stops replacing on a timeout (`s3csi/SECURITY.md`
§4.12, open list 9). The plugin logs were not kept (the runner deleted
the cluster); the leg's lines above are the evidence.
