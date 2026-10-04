# Kind S35 + S36, 2026-10-04 — on the Mac

The box was powered off, so these ran on a one-node kind cluster on the
Mac (arm64, Docker Desktop), setup from `run-s3csi.sh setup`.

| run | log | images | result |
|---|---|---|---|
| 1 | `run1-legs-S35-S36-on-1.57.1.log` | the published `1.57.1` (pulled from Docker Hub) | S35: the door is refused from a sibling pod and the node, and inside serves 401 without the token / 200 with it — but the listener check read `/proc/net/tcp` through `nsenter` and found nothing (rig bug). **S36 FAILS: the defect** — `workers.maxPerNode=7` with 6 live, six pods at once: **peak 9 workers, 3 pods mounted**, only 3 told WorkerCapacity. |
| 2 | `run2-legs-S35-S36-capfix.log` | `flint-s3-csi:capfix` (arm64, built from this tree), workers `1.57.1` | **S35 6/0**: `ss` in the worker's netns shows the door on `127.0.0.1:9911` only. **S36 5/0**: peak 7 (= the ceiling), exactly one of six mounted, five told WorkerCapacity. |

The fix: `worker_capacity` takes a node-wide lock before the count and
returns it; each of its three callers holds it across `worker::ensure`,
so the next count sees the worker just created. Held only when a ceiling
is set; taken after the volume and class locks.

Unit tests: `cargo test --lib -- s3csi:: passthrough::` 116/116 on the
Mac. The worker test's new `hostNetwork` assertion FAILS with
`host_network: Some(true)` put into the worker spec and passes restored.
The lock itself is checked by S36, run 1 being its known-bad arm.
