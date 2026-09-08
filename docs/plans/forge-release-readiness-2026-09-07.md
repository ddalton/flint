# forge release readiness — 2026-09-07

Written while the cluster work was closing out, for the release the user
asked for once "all this is done". **Nothing here was published; this is
a checklist, not a record of a release.** Last release was `v1.45.0`
(2026-09-04); forge has never shipped, so its first release is a MINOR
bump — `v1.46.0` — under the policy in `project_release_policy`.

## What is ready

- **Images have a release path.** `publish-images.sh` has a `forge`
  scope publishing `flint-forge-{operator,syncer,git}` from
  `spdk-csi-driver/docker/Dockerfile.forge-{operator,syncer}.prebuilt`
  and `Dockerfile.forge-git`.
- **The chart execs what the image installs.** The gate that caught the
  1.45.0 lean miss, applied by hand to forge: the chart execs
  `/usr/local/bin/flint-forge-operator` and `/usr/local/bin/flint-hub-gateway`,
  and `Dockerfile.forge-operator.prebuilt` installs both. Checked
  2026-09-07.
- **CHANGELOG.** `[Unreleased]` carries forge's whole history, ~1,400
  lines, with the day's five entries at the top.
- **The suite.** 140 forge lib tests green at `9b81ac9f`.

## BLOCKER 1 — `release.sh` has no `forge` scope — **FIXED 2026-09-07**

A `forge` scope now exists, modelled on lean's, with the three gates
below plus a fourth the others do not have (see BLOCKER 2). Exercised
against a disarmed copy of the script — `helm push` and `push_chart`
stubbed — because it cannot be run for real without publishing.

**The recipe gate caught something on its first run: itself.** It was
written against `Dockerfile.operator.prebuilt` and the forge chart
pulls an image built by `Dockerfile.forge-operator.prebuilt`, so it
refused a release that was fine. Same wrong-recipe miss recorded
against an earlier forge check. Fixed and re-run; it now passes on
correct config and refuses on drift.

### The original finding

`publish-images.sh` takes `[all|lean|s3csi|forge]`; `release.sh` takes
only `lean`, `s3csi|passthrough`, `all`, and no block in it packages or
pushes `flint-forge-chart`. **There is no tooling path to release the
forge chart.** Doing it by hand is what the release policy exists to
stop — "releases were pushed by hand" is the note at the top of
`release.sh` itself.

A forge block should be modelled on the lean one (`release.sh` ~line
299) and must carry the same gates, because each is a shipped incident:

1. `tag_exists` for every image the chart names, at the chart's
   appVersion — refuse if any is not on Docker Hub.
2. The recipe check: every `/usr/local/bin/<bin>` the chart execs must
   appear in the Dockerfile that builds the image the chart pulls.
   (Passes today, by hand — but nothing enforces it.)
3. Digest equality for any aliased image name, if forge grows one.

**I did not write this block.** Those gates encode knowledge of past
incidents, the failure mode surfaces mid-release, and it cannot be
exercised end to end without publishing. It wants the user's hand or at
least the user's review, not a speculative draft written while they were
out.

## BLOCKER 2 — the chart's two versions disagree — **FIXED 2026-09-07**

`Chart.yaml` appVersion is now `1.46.0-forge.6`, agreeing with
`values.yaml image.tag`. The release still sets both to `1.46.0`.

**And a gate now enforces the agreement, because `tag_exists` cannot.**
Both `-forge.4` and `-forge.6` were on Docker Hub, so a gate modelled
only on lean's would have verified one tag while the chart pulled the
other and passed. Mutation-checked: putting the two back out of sync
refuses with the reason named.

### The original finding

```
flint-forge-chart/Chart.yaml    appVersion: "1.46.0-forge.4"
flint-forge-chart/values.yaml   image.tag:   1.46.0-forge.6
```

Two answers to "which tag does this chart pull". The lean gate reads
appVersion and checks THAT tag exists on Docker Hub, so a forge block
written the same way would verify `-forge.4` while the chart actually
pulls `-forge.6`. Whichever is right, they must agree before a release,
and the release should set both to `1.46.0`.

This is the same family as the standing **image tag provenance drift**
item (open, third instance): `--version` prints the crate version, so it
cannot distinguish these. Detect by CONTENT marker plus a control arm.

## Before cutting

- [ ] Resolve BLOCKER 2; set chart `appVersion` and `values.image.tag`
      to `1.46.0`, chart `version` to its release value.
- [ ] Add the `forge` scope to `release.sh` with the three gates above.
- [ ] Rebuild BOTH musl targets AFTER the release commit (the 1.43.0
      note) — `mv` restores mtime, so a stale binary looks fresh.
- [ ] Pass the bases explicitly to `publish-images.sh` (the 1.45.0
      note: worker images were built FROM stale default bases).
- [ ] Re-run the PUBLISHED-artifact drill: install the shipped chart
      from the registry into a clean cluster. It has failed before —
      "the shipped chart could not install itself".
- [ ] Move `[Unreleased]` to `## [1.46.0] - <date>`.
- [ ] Annotated tag `v1.46.0`; images at `:1.46.0`, `:1.46`, `:1`,
      `:latest`.

## Known and deliberately NOT fixed for this release

- **The cadence's whole-repository base rebuild.** `7202c2b5` removed
  the pack cap's path into the base rule; the tier-ratio path remains
  and is forge's largest remaining byte cost against walgit, which
  rebuilds a base only on a schedule and deleted its own ratio trigger
  after that trigger fired forever. Changing it is coupled to GC —
  forge's base rebuild is its only reachability GC — so it is a design
  decision, not a knob turn, and it is not a correctness defect.
- **`7202c2b5` is unmeasured on the wire.** Its numbers come from
  replaying the wire shape through the real planner. The architecture
  doc says so in the same words.
