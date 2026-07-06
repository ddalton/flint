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

## Scaling the DS fleet

UP is safe: `--set pnfs.server.dataServers.count=N+1`. Existing files
keep their pinned placements (zero bytes ever move); new files stripe
over the wider fleet. DOWN is NOT supported until the drain milestone
— a removed DS strands every file pinned to it (LAYOUTGET refuses
rather than re-maps).

## Known residuals (fix work tracked in the durable-DS plan)

- **In-flight I/O wedge on abrupt DS loss**: if a DS's Service has no
  endpoints, in-flight client RPCs fall back to the MDS, are refused
  with NFS4ERR_DELAY (the stub-IO guard — the alternative was silent
  zeros), and the kernel never re-drives them down the pNFS path.
  Graceful reschedules avoid this (measured stall 1 s); a hard kill
  mid-I/O can wedge affected processes until their pod is recreated
  **on another node** — kernel NFS client state is per-node, so
  same-node restarts inherit the wedge. Durable fix: MDS proxy I/O.
- **helm --reuse-values** silently nils new chart defaults — always
  pass the full values file.
