# Real-S3 qualification — 2026-08-26

The question this answers is the one no kind rig can: **does the chart
install from the images we would actually publish, and does the sidecar
reach real S3 over TLS?** Every prior lean drill points at MinIO over
plain HTTP, which is why three shipped-path defects survived 27 bucket
legs and 14 operator legs.

## Setup

- kind cluster `flint-lean-qual`; images built from the PRODUCTION
  recipes (`Dockerfile.operator.prebuilt`, `Dockerfile.sync.prebuilt`)
  and sideloaded with `kind load` — same recipes that publish, no
  Docker Hub push.
- Bucket `flint-lean-qual-20260826`, `us-west-1`, versioning **Enabled**.
- **No endpoint override anywhere.** The sidecar resolves
  `s3.us-west-1.amazonaws.com` and must validate a public CA chain out
  of its own trust store.

## What it proved

**F1 — the operator image carries the lean binaries.**
```
$ docker run --rm --entrypoint sh flint-lite-operator:qual -c 'ls /usr/local/bin/'
flint-hub-gateway  flint-lean-gateway  flint-lean-operator  flint-lite-operator
```
and in-cluster the operator started, generated its webhook cert and
applied `flint-lean-inject`. Before this commit `flint-lean-operator`
was not in that image and `helm install` was a CrashLoopBackOff.

**F2 — the sidecar image exists and is injected.** `agent-1` reached
`2/2 Running` with `initContainers: ['flint-sync']`.

**F3 — the sidecar completes a real TLS handshake.**
```
sidecar: barrier seq=Some(1) up=1 del=0 consumed=0 acks=0

s3://flint-lean-qual-20260826/tenants/qual1/
  .flint/lean/claim | epoch | inbox | manifest
  files/hello.txt

$ aws s3 cp .../files/hello.txt -
hello from a real cluster over TLS
```
A certless base fails at handshake here. `alpine` + `ca-certificates`
passes, and the probe's shell requirement is satisfied by the same base.

**The boundary-source stamp, read from the bucket as an operator would:**
```
manifest metadata:  boundary-source: cadence   epoch: 1   generation: 1
```
That field was written by no path but the gated citation lane until this
session; here it is on a plain cadence boundary, on real S3.

**`spec.mountPath` is honoured end to end.** A second workspace with
`mountPath: /flint` let the agent keep its OWN `/workspace`:
```
/workspace (mine): agent-owned scratch      <- the agent's emptyDir
/flint (flint):    published via /flint
s3://.../tenants/qual2/files/theirs.txt     <- only /flint reached S3
```

## Rough edge found

A pod declaring its own mount at the workspace path fails admission with
`spec.containers[0].volumeMounts[2].mountPath: Invalid value:
"/workspace": must be unique` — the API server's error, naming neither
flint nor `spec.mountPath`. `inject.rs:98` pushes the mount into every
container unconditionally with no collision check. The fix is a CLEAR
REFUSAL naming the knob, not a silent skip: skipping would let the
agent's volume shadow the workspace and run ungated against the wrong
directory, which is the silent-winner class this codebase refuses
everywhere else.
