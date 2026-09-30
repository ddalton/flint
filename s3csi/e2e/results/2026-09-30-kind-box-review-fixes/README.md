# The passthrough review's five fixes on the box's kind rig — 2026-09-30

Rig: the Linux box (10.0.0.249), kind `flint-s3csi` recreated after the
box's reboot (control-plane + worker, v1.34.0), RustFS behind the Service
still named `minio`, `mc` from Chainguard. Images built from `5926334a`
(= origin/main) on the box, `kind load`ed; the lean images are the older
`dev` ones. `run-s3csi.sh setup`, then `run-legs.sh`.

| run | legs | file | result |
|---|---|---|---|
| 1 | S23 S24 S25 S26 S27 S29 S28 | `pt-legs-run1.log` | 62 ok, 2 bad — both a leg defect: the plugin log is ANSI-coloured, so `grep tenant=…` could not match; every product assertion held |
| 2 | S29 S28, with `plugin_log` stripping the colour | `pt-legs-run2-s29-s28.log` | 15 ok, 0 bad |

## What each new leg saw

- **S25 (fix 1, memory target; fix 3a, pre-pull).** `reader`'s mount-s3
  runs `--memory-target 682` (two thirds of the chart's 1Gi); a CR naming
  `--memory-target 900` in `mountOptions` keeps it with the plugin's
  absent; both nodes' plugin pods ran the `prepull-passthrough` init
  container to exit 0; a `helm upgrade` to a 256Mi worker limit was refused
  at render naming the floor and left the release at its revision.
- **S26 (fix 2, admission).** With every worker asking kubelet for 1000Gi,
  `noroom` got `WorkerNotAdmitted` within 5 s naming `OutOfmemory`, the
  no-reschedule warning and `maxPods`; it stayed Pending, not Failed; once
  the chart was restored it mounted on the same pod and read content.
- **S27 (fix 3b, kept Pending worker).** With the worker image set to a tag
  no node has, the worker for `pull-reader` was created in 2 s and was the
  SAME pod (uid `ba118e9c-…`) 150 s later, through the 45 s deadline and
  kubelet's retries; the plugin logged "kept for kubelet's retry" and
  "resuming rather than starting over" and zero "retrying an unfinished
  publish" cleanups; the tag was then loaded, the same pod's next pull
  succeeded, and the mount landed on that worker (uid unchanged).
- **S28 (fix 4, sharing retries).** With the broker scaled to 0 and a fresh
  plugin, `shared-retry`'s FailedMount read "asks for sharing and the
  broker's backend could not be read … retrying"; no own worker was made;
  with the broker back it published as a MEMBER of the shared class and
  read through the shared mount. The rig can only take the whole broker
  away, so the case that separates old from new (status unreadable while
  the exchange works) has no shape here; what is pinned is the message
  and the convergence.
- **S29 (fix 5, shared key on refusal).** `shared-d` under a second SA
  joined the class; its SA was dropped from `consumers`; its
  `CredentialRefreshFailed` (65 s / 90 s later in the two runs) said the
  class's credential stays; `creds.json` was present in all 50 samples over
  the following 100 s (a republish period, in which the old code's removal
  and re-mint would have shown); `shared-a` read an uncached object; the
  revoked member kept reading, as the CRD field documents.

S23 (F72 order) and S24 (sharing) ran as regression: unchanged, all ok.

## Left on the box

The rig is up (`run-s3csi.sh teardown` brings it down); the worktree
`~/flint-pt` on branch `pt-5926334a`, images `flint-s3-csi:dev` and
`flint-s3-worker:dev` on the host and both nodes. The `pull-test` tag S27
made was removed from the host and the worker node.
