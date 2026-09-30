# S23 (F72) and S24 (read-only mount sharing) on the box's kind rig — 2026-09-30

Rig: the Linux box (10.0.0.249), kind cluster `flint-s3csi` (control-plane +
worker, v1.34.0), store RustFS behind the Service still named `minio`
(MinIO's public images vanished 2026-09-29), `mc` from Chainguard's
`minio-client:latest-dev`. Images built from `8e025b46` plus the cache-root
fix (`58bd0235`), loaded with `kind load`; the lean images are the 12-day-old
`dev` ones because `lean/syncer` at HEAD does not build with `--features s3`.
Legs run with `run-legs.sh S23 S24` after `run-s3csi.sh setup`.

## Runs

| run | file | result |
|---|---|---|
| S23 + S24, fix in place (`workers.quiesceSecs=30`) | `pt-legs.log` | 23 ok, 0 bad |
| control 1: old order (`quiesceSecs=0`), shaping MISPLACED on the node's eth0 | `pt-control-1-misshaped.log` | 5 ok, 1 bad (only the log-line check) — INCONCLUSIVE |
| control 2: old order, shaping in the pod's netns; then restored | `pt-control.log` | old order 6 ok, 1 bad (only the log-line check); restored 7 ok |

S24, live: `shared-a` and `shared-b` published to ONE worker (the join took
14 ms against 1.6 s for the create), `shared-c` (uid 1002) to its own; the
default block cache (768 MiB at `workers.scratchSize=1Gi`) in use; the
creator leaving first left the mounter up for the joiner; the last member
out brought it down, and the plugin logged the mounter's own exit ~2 s after
the detach.

## What the old-order control taught

The object landed whole under the OLD order both times, the second time
with the 40 Mbit/s shaping verified inside the pod's network namespace. A
probe pod with mount-s3 `--debug` and two timed 48 MiB writes explains it:

| write | wall time |
|---|---|
| same process opens, writes, closes | 11.15 s |
| opener closes first; a child writes and closes last | 12.18 s |

Both closes blocked for the whole upload, and every FUSE request in
mount-s3's log — write, flush, release — carries `pid=0`. The tenant's
processes are outside the mounter's pid namespace (the node plugin, which
performs the `mount(2)`, has no `hostPID`; the worker is its own pod), so the
kernel reports pid 0, Mountpoint's `are_from_same_process(0, 0)` is true, and
every close completes the upload inside FLUSH. RELEASE has nothing left to
do; a container cannot exit with an upload in flight. **F72's window does not
open in this deployment.** It opens where the mounter can see the tenant's
pids (`hostPID` on the plugin, or a shared pid namespace): then the pids are
real and distinct, the tgid lookup fails inside the worker, and Mountpoint
defers to RELEASE. The fix stays (about 2 s per unpublish here); S23 pins the
ORDER and that a child-written file lands whole and clean, and is not a
falsifier of data loss on this rig.

## Found on the way

- `spec.cache` had never produced a working mount (mount-s3 does not create
  the cache directory's parent); the cache is now the scratch root.
- The first S23 shaped the kind NODE's interface: the pid came from grepping
  crictl's JSON, whose first `"pid"` is the process spec's `1`. The helper
  now asks for `.info.pid` and the leg refuses a pid in pid 1's netns. The
  stray node qdisc was removed by hand.
- The rig's store had to change: quay.io/minio/* answers 401 to everyone,
  Docker Hub's minio/* is denied, dl.min.io is 410. The lean rigs still name
  `minio/minio` and are broken the same way.
