# S24 + S30 on kind — 2026-10-01

The two legs the §11 work rewrote or added, run on the box's kind cluster
`flint-s3csi` (2 nodes, RustFS in-cluster) with images built from
`7b5a6da7` (`:s30-0930`, musl release via cargo-zigbuild, the docker snap
fed a build context under `$HOME`), the day after the box rebooted under
them. `run-legs.sh S24 S30` after a fresh `setup`: **46 ok, 0 bad**, no
notes (`legs-S24-S30.log`).

- **S24** (step 1, `252e7d83`): the shared mounter of a sharing CR that
  names no `spec.cache` runs with no `--cache` and no cache directory
  under its `/tmp` after the reads; the plugin logs `sharing: no block
  cache`; the CONTROL arm — shared-c's class recreated against the CR
  with a 256 MiB cache named — gets `--cache /tmp --max-cache-size 256`
  and a populated cache directory. The sharing mechanics (one worker per
  class, the creator out first, the last member brings it down) as
  before.
- **S30** (step 2, `7b5a6da7`): with `workers.cacheHostPath` set, the
  plugin mounts the root as a type-Directory hostPath; the startup sweep
  removed an `s3w-orphan` directory no record names and left `planted`
  alone; a sharing CR with no cache got `--cache /tmp --max-cache-size
  512` (the chart's `cacheSizeMib`) with its scratch a hostPath at
  `<root>/<worker>`, no emptyDir, 0700 1001:1001, populated by a read;
  a per-pod CR with no cache kept its emptyDir and ran no cache; a
  per-pod CR naming 128 MiB was placed at its own ceiling; both
  directories went with their mounters; a leftover of the same name was
  emptied before the next worker (same class, same worker name); a root
  absent on the nodes failed the plugin pod with `hostPath type check
  failed` in 30 s; the chart refused a relative path and a zero size at
  render time; the chart restored without a placement.

On kind the "device" is a directory on the node's own disk: this is the
placement MECHANICS. The speed claim (§11 step 4) is M1 on a node with
the instance store mounted, not yet run.
