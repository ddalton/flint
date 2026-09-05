# flint forge, for agent fleets

A git server per repository, with S3 behind it. Agents are stock git
clients; the server is real git; the bucket is a bare git repository.
Design of record: `docs/plans/flint-forge-design.md`.

This page is the operator's and the agent author's half: what to
install, what to put in an agent image, and which knobs decide what a
thousand agents cost.

## Install

```
helm install flint-forge ./flint-forge-chart -n flint-system \
  --set door.namespace=flint-system
```

`door.namespace` is not optional in spirit. It renders the
NetworkPolicy that admits only the gateway to each repository's git
port, and that policy is what makes the principal mean anything —
without it, anything that can reach the port can claim to be anyone.
See "The trust boundary" below.

The door itself is the shared `flint-hub-gateway`, with `--git` (or
`FLINT_GATEWAY_GIT=true`). It is off by default, and off means the
`FlintRepo` CRD is neither listed nor watched by it.

## A repository

```yaml
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: proj, namespace: team-a }
spec:
  projectId: proj
  bucket: my-bucket
  keyPrefix: team-a/proj/
  credentialsSecretRef: forge-s3
  consumers:
    serviceAccounts: [agent-runner]
  branches:
    protected: [main]
    pushers:   { main: [system:serviceaccount:team-a:release-bot] }
    mergeInto: { main: [system:serviceaccount:team-a:agent-runner] }
    agentPattern: "agent/*"
  idle:
    suspendAfterSecs: 900
```

`consumers` is who may reach it at all. A bare ServiceAccount name
means "in this repository's namespace"; a fully qualified
`system:serviceaccount:<ns>:<sa>` names one anywhere, which is what a
repository shared across tenant namespaces needs.

`branches` is who may move what, and it is enforced twice: by
`pre-receive` at the edge, where the refusal is the message the pusher
reads, and by the syncer at the writer, where it is the guarantee. A
repository whose hooks are misconfigured does not become an open one.

**`agentPattern` bounds the branch NAME, not its owner.** The principal
a pod presents is its ServiceAccount, and many pods share one, so
nothing stops one agent pushing another's branch. Per-pod ownership
needs a per-pod principal, which the door does not mint.

## The agent image

Two lines of config and one binary:

```
git config --global credential.https://forge.example.com.helper \
    /usr/local/bin/flint-forge-credential
git config --global credential.https://forge.example.com.username pod
```

`flint-forge-credential` reads the pod's projected ServiceAccount token
and presents it as the HTTP basic password. Nothing is cached: when the
kubelet stops renewing the token, the credential stops working.

The pod needs the token projected with the right audience:

```yaml
volumes:
  - name: forge-token
    projected:
      sources:
        - serviceAccountToken:
            audience: forge.chert.us
            expirationSeconds: 3600
            path: token
volumeMounts:
  - name: forge-token
    mountPath: /var/run/secrets/forge.chert.us
    readOnly: true
```

A default ServiceAccount token has the API server's audience and the
door refuses it. That is the point of audiences, and accepting one
would make every pod token in the cluster a forge credential.

## The agent's workflow

```
git clone --single-branch -b main https://forge.example.com/git/team-a/proj.git
git switch -c agent/$POD_NAME
… work …
git commit -am "…" && git push -u origin agent/$POD_NAME   # durable when this returns
git push origin HEAD:refs/for/main                          # propose a merge
```

**`--single-branch` is not a nicety.** A full clone of a repository
with a thousand one-commit agent branches costs the server 0.54 CPU-s
instead of 0.13, and puts a 74 KB ref advertisement on every request.

A push to `refs/for/<target>` is how a protected branch moves: the
server merges with `merge-tree`, and the client is told which ref
actually moved. On a conflict it answers `ng` with the conflicted
paths and moves nothing. `-o strategy=ours|theirs` picks a resolution;
anything else is ignored rather than passed to git.

**A push is durable when it returns.** That is forge's one strong
guarantee, and it is the whole reason the syncer exists: the bucket
holds the pack and names the ref before the client is told `ok`.

## Uncommitted work

It has no RPO. That is git's contract, and forge keeps it.

For a harness that would rather not lose an hour of edits to a spot
reclamation, `docker/forge/wip-snapshot.sh` pushes the working tree to
`refs/wip/<pod>` on a timer. Run it in the agent's own pod — forge owns
repository servers, not agent pods, and injects nothing.

It is plumbing (`write-tree`, `commit-tree`, `push`) for a reason:
`git commit` against a throwaway index still moves HEAD, so a snapshot
written with it would rewrite the agent's own branch under it.

## Clone storms

A thousand agents cloning one repository at once is about 130 CPU-s —
sixteen seconds on eight cores — and 43 GB from one pod's network
interface. **Egress binds long before CPU does**, so the lever is
taking the server out of the transfer:

```yaml
spec:
  fleet:
    bundles: { enabled: true, everySecs: 3600, urlTtlSecs: 21600 }
```

The syncer cuts a bundle, uploads it beside the packs and advertises a
presigned URL; clients fetch it from the object store and ask the
server only for the remainder. Measured here: the server's share of a
clone fell from 42.55 MB to a 5.8 KB remainder.

**Three conditions, and the first one is on you:**

1. **The agent image must set `transfer.bundleURI=true`.** The client
   default is FALSE, so a stock git ignores the advertisement and this
   setting does nothing at all.
2. Both sides need git ≥ 2.40.
3. The session must be protocol v2, which needs the door to forward
   `Git-Protocol`. It does; a proxy in front of the door that strips it
   would silently degrade every clone to v0.

```
git config --global transfer.bundleURI true
```

**Partial clone (`--filter=blob:none`) is NOT a storm lever.** The
clone itself is cheap — 2.8 MB, 0.02 CPU-s — but the first checkout
fetches every blob at 2.8x the CPU of a bitmapped full clone. It is for
agents with a genuinely sparse working set.

## Large binaries: git LFS

The multi-modal case, and the reason it is not an afterthought. A pack
is delta-compressed and rewritten WHOLE by `repack -a`, so images,
audio, video and model weights committed as ordinary blobs make every
clone, every repack and every restore pay for them again.

```yaml
spec:
  lfs: { enabled: true, ttlSecs: 3600 }
```

The bytes live at `<keyPrefix>/lfs/objects/<oid>` — immutable and
content-named, the same layout the packs use — and the pointer files
stay small in git. **The objects never cross the repository server**:
the batch API hands the client a presigned URL, so an agent uploading a
4 GB checkpoint talks to the object store directly and the pod sees a
few hundred bytes of JSON. It is the same lever bundle URIs give for
the pack, applied to the bytes that dominate a multi-modal repository.

In the agent image, nothing special:

```
git lfs install
git lfs track "*.safetensors" "*.mp4" "*.wav"
git add .gitattributes && git commit -m "track large media with LFS"
```

An object already in the bucket is offered no upload at all, so a
rebased branch re-pushing the same checkpoint transfers nothing.

Two things to know:

- **Nothing collects LFS objects.** An object is referenced by a
  pointer file inside some tree of some commit, so deciding one is
  unreferenced means walking every reachable tree — and being wrong
  once deletes a checkpoint. An unreferenced object costs storage and
  nothing else, so forge leaves it. Reclaim with a bucket lifecycle
  rule if you must, and only against a prefix you are certain about.
- **A transfer URL is a bearer token for that object** until it
  expires. `ttlSecs` is the window; it is not a permission.

## Pruning agent branches

```yaml
spec:
  fleet:
    pruneAgentBranches: { pattern: "agent/*", afterSecs: 604800 }
```

A branch is taken only when BOTH hold: it is already contained in the
default branch, so nothing is lost that `main` does not have; and it
has been quiet longer than `afterSecs`, so a merge that just landed
does not delete the branch out from under the agent still pushing to
it. **An unmerged branch is never pruned by a clock.**

## Idling to zero

`idle.suspendAfterSecs` scales the server to zero when the door's
heartbeat is stale AND the server's own activity clock is quiet. The
next request wakes it and is HELD while it restores — up to 180 s,
because git clients do not retry a 503.

There is one rung, not lite's three: the cache is an `emptyDir`, so
scaling to zero already destroys it and waking is a restore from the
bucket either way.

## The legible export

```yaml
spec:
  export: { refs: [main], prefix: team-a/proj-export/, everySecs: 300 }
```

Publishes that ref's tree as a **lean workspace**, so lite, lean and
passthrough readers can mount what forge holds without any forge code
in them. Exactly one ref, refused at admission otherwise: a lean
workspace is one tree.

The export is a mirror derived from git, never a source of truth. A
foreign write into its prefix is overwritten by the next export.

## The trust boundary

`REMOTE_USER` — the principal both enforcers read — arrives as the
door's `X-Remote-User`, set from a verified `TokenReview`. No caller
can smuggle one past the door, which builds its upstream headers from
an allowlist.

But anything that can reach a server pod's git port directly can set
that header itself. Three things stand in the way: the port is on a
headless Service with no external address; the repository lives in its
tenant's namespace; and the operator renders a NetworkPolicy admitting
only the gateway's pods — **only when `door.namespace` is set**.

Where it is not rendered, reaching the port is the authorization. That
is a defensible posture on a single-tenant cluster and is not one on a
shared cluster.

## What forge is not

No web UI, no pull requests or review, no code search, no CI, no POSIX
volume, and no durability for untracked files. An agent's `.env`, build
output, datasets and checkpoints are not in forge unless they are
committed. Keep GitHub for humans and review; forge is where the agents
work, and a mirror job connects the two.
