# Git on a flint-lean workspace

The recommended way to do git-based collaboration between agents whose
workspaces are flint-lean checkouts. **Use plain git against a real git
host; let flint-lean keep the workspace durable.** Two mechanisms, two
network paths, two different jobs — they never touch each other, which
is exactly why nothing special is needed to make them coexist.

```
   agent pod
   ├─ your code ──▶ /workspace  (local emptyDir, real POSIX)
   │                   ▲   │
   │       checkout ───┘   └─── snapshots (every floorSecs, + preStop)
   │                               │
   │                     flint-lean sidecar ──▶ proxy ──▶ S3   ← DURABILITY
   │
   └─ git push / pull / clone ──HTTPS──▶ GitHub / Gitea / …    ← COLLABORATION
```

## Why this shape

flint-lean materializes the whole workspace as **plain files on a local
emptyDir** at pod start and publishes changed files back to your bucket
at a cadence. So the working tree lives on ordinary local disk with zero
interception — **plain `git` just works**, including index locks, local
clones, and the hard links git uses internally. There is nothing to
special-case, and none of flint-lite's `LINK`→`NOTSUPP` limitation
applies here (that is an NFS-over-S3 constraint; a lean checkout is
local disk).

Collaboration — several agents integrating each other's code — is git's
own job, over its normal HTTPS remote. Do **not** reach for a
git-over-S3 remote here:

- flint-lean pods hold **no bucket credentials** and reach S3 only
  through a project-scoped, epoch-arbitrated proxy — not the open bucket
  a git-over-S3 remote helper expects. Its writes would be unscoped and
  would not carry flint-lean's epoch stamp.
- flint-lean is **itself** a checkout/publish-to-S3 engine. Layering a
  second S3 sync on top is redundant and gives you two conflicting
  coherence models.
- flint-lean is **one writer per workspace subtree** — it is not built
  for N agents merging into one tree. That is precisely what a git
  remote is for.

So each agent gets its own lean workspace, and the agents meet at the
git host.

## The division of labor

|                  | git → git host | flint-lean → S3 |
|------------------|----------------|-----------------|
| **Owns**         | committed **and pushed** code; sharing/merging between agents; history, review, conflict resolution | the **entire** local tree — code, `.git`, uncommitted edits, untracked files, build caches, sqlite | 
| **Blind to**     | uncommitted edits, unpushed commits, and everything outside the repo | combining two agents' work (single-writer, no merge) |
| **Coherence**    | git refs, fast-forward rules, merges | one claim per subtree, epoch-fenced |
| **Wire / auth**  | HTTPS egress; a git credential (PAT / deploy key) | the injected sidecar, via the proxy; your app container holds nothing |

The two cover each other's gaps exactly: git's blind spot (uncommitted /
unpushed / non-repo state) is flint-lean's whole job, and flint-lean's
blind spot (multi-writer integration) is git's whole job.

## Two tiers of durability

A hard pod kill (SIGKILL, node loss, spot reclaim) is the case to reason
about:

- **Pushed commits are safe unconditionally** — they live on the git
  host, nothing to do with the pod.
- **Everything else is safe to the last flint-lean barrier.** Uncommitted
  edits, unpushed local commits, untracked files, and build state are
  durable as of the most recent snapshot, so a hard kill loses **at most
  `floorSecs`** of that state (default 60s — this is the RPO contract).

A **graceful** termination is better than that: flint-lean runs a final
publish barrier on `preStop`, so an orderly restart loses nothing. And a
container restart over a still-live tree is a first-class state — the
sidecar diffs against its persisted baseline and resumes, it does not
re-checkout.

Net: commit and push at natural breakpoints for durable, shareable
history; rely on flint-lean to carry the in-flight remainder across pod
churn.

## Setup

You need a flint-lean workspace (see the chart's `NOTES.txt`) and one
extra secret: a **git credential** for the host. The git credential is
the *only* storage-facing secret your workload holds — the S3 side is
the sidecar's, never the app's.

```yaml
# 1. The workspace (operator/webhook consume this; see the chart).
apiVersion: flint.io/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj1 }
spec:
  projectId: team-a/proj1          # durable claim identity
  bucket: my-bucket
  keyPrefix: tenants/proj1
  endpoint: http://s3-proxy.svc:9000
  credentialsSecretRef: proj1-proxy-creds
  floorSecs: 60                    # the RPO for in-flight work
  maxFiles: 250000                 # the measured v1 checkout cap
---
# 2. A git credential for HTTPS pushes (a PAT / deploy token).
apiVersion: v1
kind: Secret
metadata: { name: proj1-git }
type: Opaque
stringData:
  # host + token; the entrypoint below turns this into git creds.
  GIT_HOST: github.com
  GIT_TOKEN: ghp_xxx
```

```yaml
# 3. Opt a pod in with the label; mount the git credential.
apiVersion: v1
kind: Pod
metadata:
  name: agent
  labels:
    flint.io/lean-workspace: proj1     # webhook injects the sidecar
spec:
  containers:
    - name: agent
      image: your-agent:latest
      workingDir: /workspace           # the lean checkout
      envFrom:
        - secretRef: { name: proj1-git }
      # Configure git for HTTPS token auth once, then work normally.
      command: ["/bin/sh", "-c"]
      args:
        - |
          git config --global credential.helper store
          printf 'https://x-access-token:%s@%s\n' "$GIT_TOKEN" "$GIT_HOST" \
            > ~/.git-credentials
          exec your-agent
```

The sidecar materializes `/workspace` before your container starts and
publishes snapshots at `floorSecs`; your agent clones, edits, commits,
and pushes over HTTPS as it would anywhere.

## Keep the checkout inside its bounds

flint-lean does a **full** checkout, bounded on two axes — total bytes
(the emptyDir budget) and file count (`maxFiles`, v1 cap 250k). Git
repos and their neighbours can strain both:

- A repo with many **loose** objects (lots of tiny commits, no gc) is
  many files. Run `git gc` / keep it packed so `.git` stays a handful of
  files rather than tens of thousands.
- `node_modules`, build trees, and vendored deps count against the same
  caps. Prefer regenerating them from a cache step over carrying them in
  the durable workspace, or size the workspace for them deliberately.

If checkout would exceed a cap it is refused at admission, not silently
truncated — so a workspace that stops starting is telling you the tree
outgrew its bounds.

## When this is the wrong tool

- **Agents must share one live working tree** (edit the same
  uncommitted files, a shared sqlite DB, a shared build tree): that is a
  live-shared-POSIX need, not a git-collaboration need — use the
  flint-lite hub (NFS), not lean-plus-git.
- **You want no git server at all** and insist on S3 as the remote: run
  git-remote-s3 with its **own** bucket and credentials, understanding
  that flint-lean's data plane is bypassed and you lose its
  zero-credential and arbitration guarantees. See the discussion in the
  flint-lean plan for why routing it through the lean proxy does not
  work.
