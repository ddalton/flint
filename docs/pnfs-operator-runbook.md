# pNFS operator runbook

Operational recipes for the chart-deployed pNFS fleet (MDS Deployment
+ DS StatefulSet, durable-DS plan Phases 0–4). Every recipe below was
drilled live; the drill scripts live in `tests/k8s/pnfs-drills/` and
each section links the script that rehearses it. Measured numbers are
from the runn validation cluster (i4i.large workers, 3-DS fleet,
2026-07-06) — treat them as expectations, not guarantees.

Quick model of what matters: clients mount the MDS's stable ClusterIP
and talk to each DS through its per-pod ClusterIP Service; data I/O
goes client→DS directly. The MDS is out of the data path (an MDS
outage stalls metadata only), a single DS outage stalls only I/O to
stripes it holds, and every piece of MDS state that matters survives
restart in sqlite on its PVC.

---

## MDS pod roll / restart

Drill: `tests/k8s/pnfs-drills/mds-roll.sh` (and the harsher in-place
`pkill -9` variant, drilled 2026-07-06). Measured: rollout ~40 s,
kill -9 recovery ~2 s process restart; **max client-visible stall 1 s**
in both shapes; zero recalls; DSes re-register at their next heartbeat
(≤10 s, NACK fast path).

    kubectl rollout restart -n flint-system deploy/flint-pnfs-mds
    kubectl rollout status  -n flint-system deploy/flint-pnfs-mds

Expected in the new pod's log: `MDS reloaded N persisted placements`,
`Stale-device sweep holds for ...s boot grace`, then one
`DS registered successfully` per DS within a heartbeat interval.

Do NOT run two MDS replicas: the Deployment is Recreate-strategy and
the RWO PVC is the fence. Scale-cycle (`--replicas=0` then `1`)
instead of deleting the pod by hand — a bare pod delete races the
ReplicaSet and the replacement can inherit a dead staging mount.

## DS pod reschedule (drain, rebalance, spot reclaim with grace)

Drill: `tests/k8s/pnfs-drills/ds-reschedule.sh`. Measured: cross-node
reschedule 49–54 s; per-pod ClusterIP unchanged; identity stamp
unchanged (PVC followed); **max client stall 1 s**, zero errors.

    kubectl cordon <node>
    kubectl delete pod -n flint-system flint-pnfs-ds-N   # StatefulSet reschedules it
    kubectl uncordon <node>

The graceful path keeps the old DS serving while it terminates, and
NodeStage self-heals the NVMe-oF target on the new node if needed.
Verify afterwards: the DS log shows `Identity marker verified` (NOT a
refusal), and the MDS log shows a re-registration with an endpoint
transition WARN but **no** "DIFFERENT data volume" WARN.

## Node death (kubelet gone, pod stuck)

Drill: `tests/k8s/pnfs-drills/node-death.sh`. Measured: node NotReady
~37 s after kubelet death; **DS replacement Ready on another node 64 s
after kubelet death** once the taint is applied; client stall 1 s,
zero errors.

A StatefulSet will NOT reschedule a pod off a dead node on its own —
only kubelet can confirm a pod's death, so the pod sits Terminating
forever. The operator action, once you are confident the node is
actually dead (not just partitioned):

    # 1. wait for NotReady (~40 s), then:
    kubectl taint nodes <node> node.kubernetes.io/out-of-service=nodeshutdown:NoExecute
    # force-deletes the node's pods AND force-detaches their volumes
    # (NodeOutOfServiceVolumeDetach) — no 6-minute attach-detach waits.

    # 2. the StatefulSet replaces the DS elsewhere; PVC + ClusterIP follow.

    # 3. when the node comes back (BEFORE letting workloads return):
    kubectl taint nodes <node> node.kubernetes.io/out-of-service-

CAUTION: the taint evicts EVERYTHING on the node. If the node hosted
lvol storage for other volumes, those consumers see the storage node's
spdk-tgt keep running (kubelet death does not stop containers) — data
keeps serving; it is the *pods* on the dead node that move.

Validated against a real AWS spot reclaim (runn, 2026-07-06, v1.10.0
gate): taint → ds replacement Ready on another node in **26 s**, r2
export PVC re-attached from the surviving replica leg. Three
real-reclaim extras the drill's kubelet-kill can't show:

- **Delete the Node object once the instance is confirmed terminated**
  (`kubectl delete node <node>`). A DaemonSet rolling update will
  otherwise schedule its next pod onto the dead node and wedge the
  whole roll on a Pending pod (it eats the maxUnavailable budget).
- **MDS-node blackhole delays DS re-registration.** kill-9 of the MDS
  process gets DSes an RST → NACK → same-tick re-register. A
  *reclaimed node* sends nothing: each DS's heartbeat channel sits in
  TCP retransmit until the kernel gives up. Observed: all DSes
  re-registered **~6 min** after the replacement MDS came up, with no
  intervention. Files pinned to not-yet-re-registered DSes are
  stub-IO-guarded (clients hang-retry) for that window. A per-RPC
  heartbeat deadline / TCP_USER_TIMEOUT would shrink it (residual).
- **A DS export claim on a `numReplicas: 1` class dies with its home
  node — unrecoverably.** Fleets deployed before the r2 claim template
  keep their original r1 claims (StatefulSet claims are never
  retrofitted). Check with:
  `kubectl get pv <pv> -o jsonpath='{.spec.csi.volumeAttributes.flint\.csi\.storage\.io/replica-count}'`
  and migrate deliberately: delete the DS's PVC + pod together; the
  replacement claim provisions from the current (r2) template, the DS
  stamps a fresh identity marker and rejoins empty. Stripes that lived
  only on the lost claim are gone; placement pins to that DS name
  remain and newly written files reuse it safely.

## After ANY csi-node DaemonSet roll (the landmine)

A csi-node pod-template change restarts spdk-tgt (a native-sidecar
init container) on every node; all NVMe-oF export objects on the node
die with it and every MOUNTED flint volume there goes EIO — including
the pNFS fleet's own PVCs. After the roll:

    # DS pods: plain delete, one at a time (clean unstage → restage)
    kubectl delete pod -n flint-system flint-pnfs-ds-N
    # MDS: scale-cycle, never bare-delete
    kubectl scale deploy -n flint-system flint-pnfs-mds --replicas=0
    kubectl scale deploy -n flint-system flint-pnfs-mds --replicas=1

Drivers ≥ the 2026-07-06 self-heal re-publish dead targets at mount
time on their own. On OLDER drivers a replacement pod can stick in
ContainerCreating with `bdev_nvme_attach_controller … Input/output
error` — delete the volume's VolumeAttachment (match
`.spec.source.persistentVolumeName`) so the attacher re-runs
ControllerPublishVolume and re-creates the target.

If a bounced pod comes up with a READ-ONLY /data (`touch: Read-only
file system`), the replacement raced the lazy unmount and inherited a
stale staging mount: delete the pod again and let termination finish
(`--wait=true`) before the replacement schedules.

## Replica failure underneath a DS (the durability payoff)

Drill: `tests/k8s/pnfs-drills/replica-under-ds.sh`. With the DS PVC on
a `numReplicas: 2` StorageClass, losing one raid leg is **invisible to
pNFS clients**: measured — raid degrades to 1/2 but stays online, the
DS keeps serving (files kept landing on it all through the degraded
window), zero client errors, 1 s stall, checksums clean.

Leg recovery (after the underlying fault is fixed) is initiator-side
on the DS's node, via `rpc.py` in that node's spdk-tgt container:

    bdev_nvme_attach_controller -b <ctrl> -t tcp -a <leg-node-ip> -s 4420 \
      -f ipv4 -n nqn.2024-11.com.flint:volume:<pv>_1 \
      -q nqn.2024-11.com.flint:node:<ds-node>
    bdev_raid_add_base_bdev raid_<pv> <ctrl>n1

then confirm `bdev_raid_get_bdevs all` shows 2/2 online. (Target-side
leg failures go through the Tier-2 rebuild machinery instead — see
docs/tier2-operator-runbook.md.)

To move an existing fleet's NEW ordinals onto a replicated SC: the
StatefulSet claim template is immutable — `kubectl delete sts
flint-pnfs-ds --cascade=orphan`, then helm upgrade with the new
`pnfs.server.dataServers.storage.storageClassName` and count. Existing
PVCs keep their old SC; only new ordinals get the replicated one.

## Placement pins are per file-key, forever

Placements pin at first LAYOUTGET and persist in sqlite. Two
operational consequences:

- pNFS pods mount the **export root**, so file names are a global
  namespace: re-creating a file with a name that was striped under an
  older, narrower fleet reuses the OLD pin (correct, but the file
  won't use new DSes). Benchmarks and drills must use unique names.
- NFS `REMOVE` (rm) does not currently forget the pin — only CSI
  DeleteVolume does. A recreated same-name file inherits the old
  stripe map. Tracked as a follow-up in the durable-DS plan.

## Scaling the DS fleet

UP is safe: `--set pnfs.server.dataServers.count=N+1`. Existing files
keep their pinned placements (zero bytes ever move); new files stripe
over the wider fleet. DOWN is NOT supported until the drain milestone
— a removed DS strands every file pinned to it (LAYOUTGET refuses
rather than re-maps).

## Known residuals (fix work tracked in the durable-DS plan)

- **In-flight I/O wedge on abrupt DS loss — and it poisons the whole
  node**: if a DS's Service has no endpoints, the files-layout client
  marks that composite **deviceid INVALID (permanent — not the 120 s
  retryable UNAVAILABLE)**, every read of every file pinned to that
  device falls back to the MDS, is refused with NFS4ERR_DELAY (the
  stub-IO guard — the alternative was silent zeros), and the fallback
  loop never re-drives the pNFS path even after the DS returns.
  Anatomy measured live (runn, 2026-07-06): retry loop at ~110 ms per
  stuck file; the deviceid cache hangs off the node's shared
  nfs_client, so **fresh pods on the same node inherit the poisoning**,
  and deleted pods leave detached mounts whose in-flight retries spin
  forever and keep the nfs_client pinned. Files on other deviceids
  read fine throughout. Remedies: run affected consumers on a
  different node (verified: same file, clean sha, instantly) or reboot
  the poisoned node. Durable fix: MDS proxy I/O.
- **helm --reuse-values** silently nils new chart defaults — always
  pass the full values file.
