# F73 — disk discovery opens, and tries to unbind, NVMe disks the host is using; in kind mode it reaches the HOST's disks

Status: **FOUND 2026-09-27 on the build box; FIXED 2026-09-27 (uncommitted).
See "The fix".** Found running the F71 ROX test on the box's kind tier
(`tests/regression/kind-up.sh`, flint-driver 1.56.0).

## What happened

The node agent's startup discovery, in a kind node, called:

```
bdev_uring_create {"filename": "/dev/nvme0n1", "name": "uring_nvme0n1"}
bdev_uring_create {"filename": "/dev/nvme1n1", "name": "uring_nvme1n1"}
```

These are the BOX's two data disks, both mounted ext4 (`/mnt/nvme`,
`/mnt/nvme2`) and holding model-checker state. A kind node shares the
host kernel: `lspci` inside it lists the host's PCI bus, and its `/dev`
tmpfs is populated with the host's device nodes when the container starts.
The driver then looked for an LVS on each, found none, and listed both as
clean disks available for initialisation. Nothing was written: dmesg shows
no filesystem error, and the runs on those disks continued. But one
"initialize" from the dashboard or the API would have created an LVS
over two live filesystems.

## Why it is not only kind

`auto_recover_spdk_state` (`minimal_disk_service.rs`) runs at every
node-agent start, after an SPDK restart (the baseline-collapse path), and
on every `GET /api/disks`. For each NVMe controller `lspci` reports:

1. `is_system_disk_physical` is asked whether to skip it. It returns
   `false` for every device: "For now, assume no system disks in our test
   environment".
2. For a device bound to the kernel `nvme` driver,
   `ensure_device_bdev_exists` calls `try_unbind_and_attach_nvme` FIRST. That
   writes the PCI address to `/sys/bus/pci/devices/<addr>/driver/unbind`,
   which removes the disk from the kernel with no check that anything uses
   it. Only if that fails does it rebind to `nvme` and open the disk with
   `bdev_uring_create`.

On the box the unbind did not happen, by luck of the environment rather
than a rule: kind mounts `/sys` read-only (the node agent's own log shows
EROFS on another sysfs write), or no userspace driver was available, and
`detect_available_userspace_driver` fails before the unbind. On a real
node with a writable `/sys` and `uio_pci_generic` or `vfio-pci` loaded,
the same code unbinds a mounted NVMe disk, root included (on EC2 the root
EBS volume is an NVMe controller). Whether production nodes have been
protected by one of those conditions every time is not established.

## The fix

1. **Nothing the host holds is touched.** Before any unbind or
   `bdev_uring_create` on a kernel-bound disk, the disk is opened
   `O_RDONLY | O_EXCL`. On Linux, an exclusive open of a block device fails
   with `EBUSY` when it is mounted, a dm, md or LVM member, or claimed by
   another exclusive opener. That works from inside a container, where the
   host's mount table is not visible (the reason `bdev_to_disk_info` gave
   for guessing). A busy disk is skipped and says so. If it cannot be told
   (no device node, a permission error), the disk is not touched either. A
   disk SPDK already has a bdev for (an agent restart beside a running
   target) keeps it, because that check comes first.
2. **Kind mode discovers nothing.** `DEVICE_DISCOVERY_MODE=none` turns
   physical discovery off, and the chart sets it whenever
   `spdkTarget.kindMode.enabled`: kind's storage is the malloc virtual disk
   the spdk-tgt container creates.

Not fixed here: `is_system_disk_physical` still answers `false`. The
exclusive open covers what it was for (a mounted disk) without trusting
it, so it can be removed separately. The kind tier's other failure (an
NVMe-oF device never appears in a kind node's `/dev`, so the block path
cannot mount) is F71's, and unchanged.
