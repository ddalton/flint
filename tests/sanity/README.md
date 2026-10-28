# csi-sanity on the kind tier

`kind-sanity.sh` runs [csi-sanity](https://github.com/kubernetes-csi/csi-test)
against the driver built from this tree on a throwaway three-node kind
cluster. SPDK runs in the chart's kind mode on a 1536 MB in-memory disk per
node, the controller's socket is reached through the chart's sanity proxy
(`driver.enableSanityTestProxy`, socat on port 10000), and csi-sanity runs
inside a kind worker so it reaches that node plugin's socket and shares its
`/var/lib/kubelet`.

```sh
tests/sanity/kind-sanity.sh                 # build, run four passes, tear down
BUILD=0 TAG=<tag> KEEP=1 tests/sanity/kind-sanity.sh
PASSES="shim-r3" NICE_SPDK=1 tests/sanity/kind-sanity.sh
```

It needs a Linux host: NodeStage connects kernel NVMe/TCP initiators, so
`nvme_tcp` must be loaded, and every kind node shares that one kernel. It is
not a CI gate (a hosted runner has no `nvme_tcp`). Results land in
`$WORK/results/<pass>/`: the suite's log and JUnit report, controller and
node logs for the pass's window, and a census of what the pass left behind
(lvols per node, shim PVs, the host's NVMe controllers, files left in the
mount and staging directories). `summary.txt` collects one line per pass.

## The two modes

- **raw** (`raw-r1`, `raw-r3`): csi-sanity talks to the controller
  directly. This is where argument checks, error codes and the idempotency
  of PV-free calls are tested. The suite names volumes with 48 to 128 bytes
  and uppercase hex, beyond the driver's 46-byte storage-id limit
  (`identity::MAX_STORAGE_ID_LEN`), so every spec that creates a volume
  fails here with INVALID_ARGUMENT, by design.
- **shim** (`shim-r1`, `shim-r3`): csi-sanity talks to `coshim`, which
  forwards every call to the controller and does what Kubernetes would do
  around it. As the external-provisioner, it names each volume `pvc-<uuid>`
  (derived from the suite's name), writes the PV from the CreateVolume
  response and deletes it after DeleteVolume: the driver keeps a volume's
  placement in its PV, so without one ControllerPublish, the snapshot and
  expand paths and NodeStage's restage fail NOT_FOUND before testing
  anything. As the attacher, it writes an attached VolumeAttachment after
  each ControllerPublish and deletes it after ControllerUnpublish: the node
  reconcile derives each export's allowed hosts from it, and without one a
  consumer was fenced off its own volume's legs mid-spec (a raid base
  dropped and the leg went stale). Its attachments name their own attacher
  (`coshim.sanity`), so the driver's external-attacher sidecar ignores them.

`coshim` is test tooling only: it is not part of the driver, the image or
the chart, and the driver does not know it exists.

## Known failures (2026-10-28, after the csi-sanity fixes)

- raw: every spec that creates a volume, by the name limit above.
- shim: CreateSnapshot with an existing name and a different source volume
  answers OK, where the spec wants ALREADY_EXISTS. Snapshot names embed
  the source volume, so a collision across sources is never seen;
  Kubernetes names snapshots `snapshot-<uid>` and never sends one.
- shim-r3: cloning a replicated volume answers FAILED_PRECONDITION, by
  design until the clone path supports replicated sources (snapshot and
  restore does).
- shim-r3: ControllerExpand of a replicated volume that was never staged
  answers UNAVAILABLE ("no replica sync record"): the expansion guard
  refuses without a record proving every leg holds the acknowledged
  history, and a never-assembled volume has none yet.

## Traps the rig works around

- kind mounts `/sys` read-only in its nodes, and the node pods inherit it:
  `nvme disconnect` then fails silently (nvme-cli 3.1 exits 0), so every
  NodeUnstage strands a kernel controller in `connecting`. The script
  remounts `/sys` read-write in each node.
- csi-sanity creates its mount and staging directories with a plain
  `mkdir` (the parent must exist) and removes them with `os.Remove`, so one
  file a driver leaves behind fails the setup of every later spec.
  `rmpath.sh` replaces the removal: it logs the leftover, refuses while
  anything under the path is still mounted, and removes the rest.
- csi-sanity v5.6.0 publishes no release binary; the script builds it with
  `go install`.
