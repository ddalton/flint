# Ref scale — what a push costs as the branch count grows

The rate leg on runcc (2026-09-06,
`forge/e2e/results/tiers-rate-ab-2026-09-06.log`) put 32 pushers against
forge three times in a row and watched **both** arms fall from 17.4 to
9.7 pushes a second over three minutes. The arms differed in the fold
planner and fell together, so the fold was not it. What grew between
the repetitions was the branch count: every pusher pushed to a new
branch, about 900 a minute.

The suspect was X19, written down in the simplification note the day
before and never measured: *the snapshot carries a map of every ref and
is rewritten by every batch*. This rig measures the curve.

```sh
bash forge/e2e/refscale/run-refscale.sh              # the timer arm
ARM=batch bash forge/e2e/refscale/run-refscale.sh    # derived files per batch
RUNGS="0 2000 8000" PROBES=15 bash forge/e2e/refscale/run-refscale.sh
```

## The control is git itself

`receive-pack` advertises **every** ref to the client before a push
begins, so any git server gets slower as the ref count grows and some
of the curve is nobody's fault. Every rung therefore also times the
identical push to a plain bare repository holding the identical refs,
with no hook, no syncer and no bucket. What forge owns is the
difference, and reading the forge column alone would have charged forge
for git's own advertisement.

## What a batch pays per push, and how much of it is O(refs)

| | O(refs)? | where |
|---|---|---|
| `git for-each-ref` for the agreed view | yes | `batch.rs` step 1 |
| the snapshot CAS, serialised whole | yes | step 5 |
| `update-server-info` | yes | step 7, derived |
| `info/refs`, uploaded whole | yes | step 7, derived |
| `objects/info/packs` | O(packs) | step 7, derived |
| the lease renewal, the pack uploads | no | steps 3-4 |

Three of the four O(refs) costs are the **dumb protocol's**, which
nothing on the serving path reads — the smart protocol answers from the
local repository and the syncer from the snapshot. They are on a timer
now (`FLINT_FORGE_DERIVED_EVERY_SECS`, default 60; 0 restores the
per-batch behaviour and is this rig's `ARM=batch`).
