# writers-live — the evidence rig (plan §2a E3, E4, E6) and the teardown (§8)

Each script's header is its full contract. This page covers what each piece
captures, where the files land, and how a collected leg's `traces/` and
`nodes/` are built from them.

| Piece | Runs | Captures | Local | Bucket |
|---|---|---|---|---|
| `shipper.sh` (E3) | every node, root, over SSM: `BUCKET=… NODE=<k8s node> shipper.sh start\|flush\|stop\|status` | container logs of `flint-workers`, `flint-system`, `wl-*`, `flint-lean*` pods, as byte copies that survive rotation and pod deletion; cgroup memory/cpu/OOM every 10 s; `chronyc -c tracking` every 60 s; `dmesg -w`; kubelet + containerd journal | `/mnt/nvme/evidence/`, state in `/mnt/nvme/shipper/` | `_rig/evidence/<node>/` every 5 min and at `stop`, `SHA256SUMS` uploaded last |
| `podwatch.sh` (E4) | CP host, root, `KUBECONFIG=/etc/kubernetes/admin.conf` | `kubectl get events -A -w`; pods every 60 s AND a pod watch (a short-lived replacement worker is still mapped to its tenant) | `/mnt/nvme/podwatch/` | `_rig/evidence/cp/`; also uploads `/mnt/nvme/history/` to `_rig/history/` |
| `sampler.py` (E6) | CP host, one per leg: `sampler.py --bucket B --prefix writers/<leg> --out /mnt/nvme/history/<leg> --until-file F` | `current`, `epoch` and `inbox` bodies on every change (500 ms conditional GETs); every generation document listed; every chunk a new pointer cites; a final listing of the prefix | `/mnt/nvme/history/<leg>/` | via `podwatch.sh flush` |
| `extract_traces.py` | Mac, after `teardown.sh pull` | builds `traces/<agent>.jsonl`, `nodes/<node>/chrony.jsonl` and `agent_nodes` for a collect dir | — | — |
| `teardown.sh` | Mac, by hand, one step at a time | `pull`, `bucket`, `policy`, `project`, `zeroset` | — | deletes only with `--yes` |

Every evidence write is refused on the root filesystem's device (the 8 GB
EBS root); `ALLOW_ROOTFS=1` / `--allow-rootfs` exist for local tests only.

## Layout

```
_rig/evidence/<node>/                     (= /mnt/nvme/evidence on that node)
  pods/<ns>_<pod>_<uid>/<container>/<N>.log         first inode seen for restart N
                                  /<N>.log.i<inode> a later inode (after a rotation)
                                  /<N>.log.<ts>.gz  kubelet's compressed rotation, copied only if never tailed
  cgroups.jsonl   {ts_ms, path, pod_uid, container_id, memory_current, memory_peak, memory_max, cpu_usage_usec, oom, oom_kill}
  chrony.jsonl    {ts_ms, offset_ms, synced, system_time_s, last_offset_s, ..., leap}
  chrony-errors.jsonl, dmesg.log, journal.log
  _shipper/logs/  the loops' own logs (tail launches, resumes, LOSS lines)
  _shipper/tails/ the tail registry: launch offsets, lost_after, died_after_vanish
  SHA256SUMS
_rig/evidence/cp/
  events.jsonl  pods.jsonl  pods-watch.jsonl  _podwatch/logs/  SHA256SUMS
_rig/history/<leg>/
  current/<ts_ms>-<etag>.json  epoch/…  inbox/…  manifests/<seq>-<uuid>.json  chunks/<addr>.json
  index.jsonl  {ts_ms, sent_ms, name, etag, key, bytes, http_date[, status]}
  errors.jsonl  listing.json (or listing.error.json)  run.json
_rig/history/SHA256SUMS
```

The pod directory keeps the UID. Worker names are deterministic
(`s3w-<hash of volume>`), so a worker re-created after a deletion (leg A4)
has the same name, and without the UID its logs would overwrite the old
worker's.

## From evidence to the collect layout (README §3)

```sh
teardown.sh pull ./pulled                       # every SHA256SUMS verified
extract_traces.py --evidence ./pulled/evidence --collect collect/<leg>   # what mac.sh verdict runs
```

- **Which writers belong to the leg.** By default, the collect dir's
  `meta.json` `agents` (drill.sh writes it). Earlier legs' workers are still
  in the evidence, and a namespace filter alone would not drop them if a
  namespace name were reused. `--tenant-ns REGEX` and `--all-agents`
  override this.
- A **trace line** is a CRI record on **stderr** whose message, after its
  `P` (partial) records are joined with their `F` continuation on the same
  stream, starts `{"ts_ms":` and parses as JSON. The CRI prefix
  (`<RFC3339Nano> <stream> <P|F> `) is stripped.
- A **worker pod** is a pod whose `pods.jsonl` / `pods-watch.jsonl` lines
  carry `chert.us/tenant-pod: <ns>/<pod>`, matched by the UID in its evidence
  directory. Its **agent** is `--agent-map["<ns>/<pod>"]` (or `["<pod>"]`),
  else the tenant pod's name. drill.sh sets `AGENT_ID=$HOSTNAME`, which is
  the pod name, so no map is needed.
- **One pod, one copy.** A pod directory found under two node directories is
  read once, from the larger copy, and reported. The same UID in two places
  comes from a flush under another NODE name, or from podwatch given the
  shipper's `EVID`. The shipper also records its NODE at the first start and
  keeps using it.
- **Order**: a unit is (pod, container, restart N). Units are ordered by
  their first CRI timestamp, and so are a unit's rotation segments. File names
  are never used for ordering: containerd trims trailing zeros, so the
  timestamps are parsed, not compared as text.
- `nodes/<node>/chrony.jsonl` keeps only synchronised samples. `offset_ms` is
  node clock minus true time, taken from chronyc CSV field 5 (System time),
  where a positive value means slow, so `offset_ms = -field5 × 1000`.
- `extract_report.json` lists everything not extracted: unmapped worker pods
  (their lines go to `unmapped/`, never `traces/`), malformed trace lines,
  unterminated partials, duplicate segments, and overlapping pods of one agent.

## Known limits

- **Shipper loss bounds** (the full list is in the `shipper.sh` header):
  - A log file created and removed within one 2 s discovery interval is never
    seen.
  - A tail killed abnormally after its file vanished loses its unread
    remainder. This is recorded, never silent.
  - Rotation and pod deletion lose nothing: tails follow the descriptor, not
    the name. `tail -F` with inotify drops the rotated file's last writes;
    coreutils' own FIXME says so. This was read from the source; a Mac (polling
    tail) cannot show it.
- **A spliced trace line.** `flint-s3-worker` tees the syncer's stderr in raw
  4096-byte reads (`crates/flint-s3-worker/src/main.rs`, the stderr tee)
  while its own `eprintln!` lines, such as "forwarding signal" at SIGTERM,
  come from another thread. One of those can land inside a syncer trace line.
  That is most likely during a pod deletion (leg A4). `extract_traces.py`
  counts the result as `malformed`. The fix belongs in the worker (tee whole
  lines).
- **E6 on the default layout.** The syncer publishes chunked manifests by
  default (`chunked: true`, not on the CRD), and a chunked pointer names no
  generation object. `manifests/` stays empty, and the history is the pointer
  bodies plus their chunks. Chunks are reaped only an hour after they are
  unreferenced, so a 500 ms sampler gets every one. The sampler does not
  resolve manifests; `flint-sync manifest` produces `bucket/manifest.json`
  and `heads.json`.
- **Clock skew.** The store's `Date` header has one-second resolution. With
  `sent_ms` and `ts_ms` it brackets skew; chrony measures it.

## Self-tests (no AWS, no cluster, no Docker)

```sh
python3 sampler_selftest.py        # SigV4 vs AWS's published vectors + a fake S3 that verifies every signature
python3 extract_traces.py --selftest
bash shipper_localtest.sh          # GNU tail (gtail) + fake aws/chronyc/dmesg/journalctl
bash podwatch_localtest.sh         # fake kubectl
bash teardown_localtest.sh         # fake aws + fake trove
```
