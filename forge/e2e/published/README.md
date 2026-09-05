# The published artifact

Every other drill in `forge/e2e` runs images built from the checkout,
and `build-forge-images.sh` states the reason in its own header: *"a
drill that verifies a claim scored from the current code must run the
current code, not a release."* That is correct, and it leaves a hole
nothing else covers.

Twelve falsifiers are green against `drill-<sha7>` images. A user runs
`helm install ./flint-forge-chart` and gets whatever tag `values.yaml`
names, pulled from Docker Hub. **Those are not the same artifact, and
only one of them had ever been drilled.**

So this leg overrides no image. It installs the chart as shipped, lets
the chart's own defaults choose, forces a real pull, and then asks the
released thing to do the job forge exists for.

```
CTX=kind-forge-pub ./run-published.sh
```

kind only — no AWS, no spend. The store is an in-cluster MinIO this rig
stands up itself, so it does not depend on the s3csi rig.

## The legs

| | claim |
|---|---|
| P0 | every image the chart names is actually published |
| P1 | …and is the NEWEST published tag, not an older one |
| P2 | the images are PULLED, not found in the node cache |
| P3 | `helm install` the chart with no `--set` of any image |
| P4 | a `FlintRepo` becomes a serving repository |
| P5 | clone, commit, push — and the BUCKET names the ref |
| P6 | `main` is protected and the refusal names the remedy |
| P7 | the image carries the `git propose` verb |
| P8 | the pod is destroyed and a clone is served from S3 alone |

## What it found on its first run, 2026-09-05

**The chart cannot install itself.** `values.yaml` pins
`1.46.0-forge.1`; the chart's own `door.yaml` passes `--git-only`; and
`1.46.0-forge.1` is the one published tag whose `flint-hub-gateway`
predates the `--git` → `--git-only` rename. The door crashloops on
`error: unexpected argument '--git-only' found`, `helm --wait` times
out, and `helm install ./flint-forge-chart --set door.deploy=true`
fails for everyone. The template moved forward with the source; the
image tag it pins did not.

Bisected across the published tags: **forge.1 rejects the flag,
forge.2 through forge.6 accept it.** The chart pins precisely the one
broken image.

**A tag bump is sufficient.** Re-run with `OVERRIDE_TAG=1.46.0-forge.6`
— 17 passed, 0 failed, 2 pending. There is no second failure behind the
first: clone, durable push judged against the bucket, protected-main
refusal, `refs/for` merge, and a pod destroyed with a fresh clone
restored from S3 alone passing `fsck --strict`, all green on a
published artifact.

## Two traps, both of which produced a confident wrong answer first

**`docker run <image> <binary> --flags` does not run `<binary>`.**
Those become ARGS to the image's ENTRYPOINT — here the operator, which
failed on kubeconfig before parsing a flag. Read as "the flag is
accepted", it exonerated an image that is in fact broken, and the
conclusion survived two tags before the pod's own log contradicted it.
`--entrypoint /usr/local/bin/flint-hub-gateway` is what tests the
gateway.

**A provenance check that runs before the pods it is about checks
nothing.** The registry-digest assertion first ran straight after
`helm --wait` and found two operator digests, which it reported as
"all images". The syncer and git images run in the repository's
namespace and that pod does not exist until the operator has
reconciled. It now runs after P4, across both namespaces, and requires
all three images to have appeared rather than judging whatever turned
up.

## `OVERRIDE_TAG`

Answers one question and no other: when the chart's default is broken,
is bumping the tag ENOUGH? It runs every leg against a named tag and
turns **P1 PENDING**, because a run that was told its images is not
evidence about which images the chart chooses — which is the entire
claim of this drill.

## Not covered here

kind's default CNI does not enforce a NetworkPolicy, so the
`X-Remote-User` trust boundary is inert. That claim belongs to
`run-rights.sh`, which runs on Cilium. Nothing here is evidence about
it.
