# flint forge — can the architecture be simpler? (2026-09-05)

**Status: exploration, with one defect fixed and a short list of do-now
items. No architectural change is made here.** Read
`flint-forge-design.md` first; this note reopens exactly one of its
decisions (§15 decision 1) and confirms the rest.

The question came after the scale drill (`forge/e2e/scale/README.md`,
"Results — runbx"), where three of the four defects found on the fixed
tree lived in the layers *between* the client and the syncer: the
door's total timeout, the CRC pre-pass that ticked no progress, and
fcgiwrap buffering git's keepalives. Each was fixed in a line or two.
The question is whether the layers themselves should go, and the goals
in order are data integrity, correctness, performance.

Method: four read-only investigations, one per option below, each
evaluated against the code (file:line) and the drill findings, each
asked to say what an option *removes* versus what it *moves* to another
component, and to name the experiment that would settle it. The claims
that matter most were then re-verified by hand: git's quarantine
migration order from the v2.43.0 source, the syncer's listing and
restore code, the operator's phase derivation, and the chart's image
pinning.

## 0. The verdict in one table

| option | verdict | removes | moves / costs |
|---|---|---|---|
| A1 syncer serves smart-HTTP, door stays | no | nginx, fcgiwrap, their knobs | the bucket credential into the process that parses untrusted HTTP; `index-pack` of a 40 GiB push into the lease holder's cgroup |
| A2 drop the door too | **reject** | one hop | wake-from-zero (the door holds the request while a parked repo starts), per-repo addressing at 3,000 repos, TokenReview per pod, the NetworkPolicy boundary |
| **A3 one Rust CGI runner replaces nginx + fcgiwrap; door and container split kept** | **do** | fcgiwrap buffering, the 4-worker ceiling, nginx's 3600 s cutoffs and two unset 60 s defaults | ~300 lines of runner to get right; reopens design decision 15.1 |
| B1 early ack | **reject** | the wait | the guarantee: a successor restores strictly to the snapshot and deletes refs it does not name; the client's retry says "Everything up-to-date" |
| B2 progress lines on the sideband | optional | silence for humans | nothing: same pipe as the keepalives, so every buffering knob stays load-bearing |
| B3 the door as the only waiter | reject | nothing | the class into the door, which would then promise liveness it cannot verify |
| C1 operator reads the epoch cell from S3 | **reject** | the `/status` poll, the NetworkPolicy exception | a credential-free operator becomes a read-everything principal, for fields no decision reads |
| C2 syncer echoes the lease into a k8s Lease object | later | polling, HTTP surface, NetworkPolicy rule | +3 RBAC objects per repo, 30 writes/s at 300 live, a new lie (write before a failed renew) |
| C3 operator infers from the pod (readiness, exit codes) | complement | — | no push clock; suspend would rest on one signal |
| D1 hook = a role of the syncer binary, one tag for both images | **do** | one staged binary, the tag-drift class | ~50 lines + a render test |
| D2 syncer as receive-pack (gix/git2) | **reject** | 4 forks, the hook | a second pack/ref implementation on the durable path; gitoxide has no server side |
| D3 keep git, drop the hook | no | — | proc-receive *is* the interposition point; without it refs move before the CAS |
| D4 CRC inside the compose's part reads | **do** | the second full read of every large pack (~70 s at 40 GiB) | none: CRC64NVME composes across parts |
| E drill rig | keep the controls; stamp provenance; auto-size | N marker checks | — |

## 1. The path today, and who must cooperate for a long push

```
client → door (warp + reqwest) → nginx → fcgiwrap → git http-backend
       → receive-pack → pre-receive / proc-receive hook → UDS → syncer
       → pack upload (multipart) → ONE snapshot CAS → update-ref → ack
```

The wait a client experiences is the syncer's batch: upload, CAS,
`update-ref`, then the reply to the hook (`batch.rs:217-303`). The hook
blocks on one line with no timeout (`uds.rs:104-122`); git blocks on
the hook; everything upstream must either keep bytes moving or
tolerate silence. The table is the defect class the drill kept finding
members of.

| party | knob today | where |
|---|---|---|
| git client | none in flint; libcurl has no inactivity bound unless `http.lowSpeedLimit/Time` (off by default) | rigs set only `http.extraHeader` |
| door | inactivity bound over both directions, streamed verbs only | `lite_gateway/git.rs:781-846, 970-1021`; chart `upstreamTimeoutSecs: 300` |
| nginx | `fastcgi_read/send_timeout 3600s`, buffering off both ways; **`client_body_timeout` and `send_timeout` are unset and default to 60 s** | `docker/forge/nginx.conf:26-32` |
| fcgiwrap | `NO_BUFFERING` in the request params; **`FCGIWRAP_CHILDREN=4`**: a fifth concurrent push or clone queues silently until the door's idle bound cuts it | `nginx.conf:68`, `docker/forge/entrypoint.sh:7` |
| git receive-pack | `receive.keepAlive` **not set**, so git's 5 s default carries the guarantee | `gitcmd.rs:177-199` |
| hook | none; correct as is | `flint_forge_hook.rs:188` |
| syncer | no batch deadline; the wedge detector is the progress-gated renewer, 6 × 10 s | `lease.rs:242-295`, `packio.rs` |
| kubernetes | **`terminationGracePeriodSeconds: 30`**, but SIGTERM is a `select!` arm the loop reaches only between batches (`server.rs:202-215` vs `276-296`): a rollout during an 872 s push is SIGKILLed at 30 s, the client is told failed, the bucket is unchanged, the successor sweeps the orphan | `render.rs:85,570` |

Not a member: the status listener runs on its own task, so the TCP
liveness probe cannot kill a batch (`server.rs:92-99`).

Three members of this class were not on any list before today (the two
nginx defaults, the worker ceiling, the grace period). That is the
argument for A3: a runner the syncer's authors own has exactly the
knobs it declares.

## 2. The front: one hop, or one fewer process

### A1 — the syncer serves smart-HTTP itself

It already runs an HTTP listener for `/status` (`server.rs:477-592`),
but a hand-rolled one: `Content-Length` bodies only, `Connection:
close`, no chunked framing either way. Serving git means hyper, a CGI
runner (environment from the request, streamed stdin, `Status:` and
header parsing from stdout, chunked response, killing the child on
client abort, a semaphore where `-F 4` was), and gzip inflation of
request bodies if `receive-pack` is exec'd directly rather than through
`http-backend`, which inflates them itself.

What it costs is the boundary the two containers draw today. The
git-http container has no S3 environment (`render.rs:291-296, 455`);
the syncer has the bucket's read-write credential. A1 puts the
credential in the process that parses untrusted HTTP and spawns git
per request, and puts `index-pack` of a 40 GiB push in the lease
holder's cgroup beside the `put_whole` bodies held in RAM
(`batch.rs:229-233`). Today an OOM in git-http restarts that container
and the lease survives (`entrypoint.sh:9-11`); under A1 it fences the
repository. The two userspace copies A1 saves are invisible: the 40 GiB
push ran at 47 MiB/s bounded by S3.

**No** (confidence 0.7). The measurement that would change this: push
10 GiB from an agent pod straight to the repo pod's port with the
policy removed, as `run-rights.sh` F6 does, against the same push
through the door. Under 10 % apart and hop count is not the lever.

### A2 — the door goes away too

Rejected on structure, not on cost (confidence 0.9):

- **Wake.** A parked repository has no pod. The door holds the request
  and arms `requested-at` on the CR (`git.rs:347-360, 393-398`); with
  nothing in front, an Ingress to a scaled-to-zero pod answers 503, and
  git clients do not retry a 503 (`git.rs:28-30`). Idle-to-zero, the
  §2 fleet arithmetic, stops working.
- **Addressing.** A routable Service or Ingress per repository, which
  §2 rejected at 3,000 repositories.
- **Auth.** TokenReview needs a kube client and `create tokenreviews`
  in every tenant-namespace pod, plus the verdict cache
  (`git.rs:253-314`) per pod with no sharing across 1,000 agents.
- **Boundary.** `X-Remote-User` and the NetworkPolicy peer rule
  (`render.rs:1077`) become meaningless; every agent must reach port
  8080; LFS `verify` needs the client-facing host the door supplies
  (`git.rs:711-730`).

The design question that settles it has no measurement: how does a
suspended repository wake on first request with nothing in front? If
the answer is "it does not idle", the capacity table is void.

### A3 — one Rust process in the git container; door and split kept

Replace fcgiwrap with a direct CGI runner and, once that runner speaks
HTTP, notice that nginx contributes only the LFS regex route
(`nginx.conf:40-50`) and two header mappings (`:85`, `:90`), all
trivial in the runner. So: one ~300-line Rust binary in the git-http
container, spawning `git http-backend` per request, with the door and
the container split exactly as they are.

Removes, outright: fcgiwrap's buffering (and the `NO_BUFFERING` build
guard, `Dockerfile.forge-git:28-39`), the four-worker ceiling, nginx's
3600 s cutoffs and its two unset 60 s defaults, and the `proxy_buffering`
trap a half-step (keep nginx, drop fcgiwrap) would introduce. Keeps:
the door's idle bound where it belongs, the credential boundary, the
NetworkPolicy, the chart's values, the CRD, the door's URL formula and
every rig. The chart loses nothing but a comment.

This reopens design decision 15.1, "no CGI runner". The decision was
right about what it feared: flint must not implement git. A runner
that execs `http-backend` implements no git; it replaces two third-party
processes whose knobs we did not know we had.

**Do** (confidence 0.75). The settling experiment is the keepalive-gap
probe the drill already ran (run 3, finding 2): a hook wait forced past
300 s with toxiproxy-slowed S3 from `forge/e2e/latency/`, the door's
`Activity` gaps logged; pass is every gap ≤ 5 s and the push
acknowledged, with fcgiwrap-without-`NO_BUFFERING` as the failing
control. Not before the multi-pack-bitmap work, which is the open
performance item.

## 3. The wait: the hook must keep waiting

### B1 — early acknowledgement: rejected

"Objects local" is true the moment `proc-receive` runs, so early ack
means `ok` before `batch.rs:215-303`. What is lost is decision 3 of the
design, and the loss is silent:

- Pod replaced after `ok`, before the CAS: the successor restores
  strictly to the snapshot, fetching only `snap.packs`
  (`restore.rs:104-125`) and **deleting every local ref the snapshot
  does not name** (`restore.rs:139-150`); the pending queue is an
  in-memory channel (`server.rs:171`). The commit is gone, and because
  git updated the client's remote-tracking ref on `ok`, the retry says
  `Everything up-to-date`. Falsifier 1's benign outcome becomes the
  silent-loss outcome.
- Deposed mid-upload: fence, `ng`, exit; the successor rotated first
  and aborts the straggler's multipart. With early ack the client holds
  `ok` for objects that exist nowhere durable.

"Durable later" needs a journal on a disk that survives pod replacement
and node loss, which is the second durability tier the S3-only design
exists to avoid. Nothing found rescues it.

### B2 — progress on the sideband: feasible, legibility only

`proc-receive`'s stderr goes to the same sideband muxer that emits the
keepalives, so `eprintln!("uploading 12/40 GiB")` from the hook reaches
the client as `remote:` lines. The UDS reply would become a stream fed
from `Hold::progress`. But those lines travel the same pipe as the
keepalives: a buffering fcgiwrap would have buffered them identically.
`NO_BUFFERING` stays load-bearing, `receive.keepAlive` becomes
redundant, no party and no knob is removed. Worth doing for the human
at the other end of an 8-minute push; not a simplification.

### B3 — the door keeps the client alive: rejected

HTTP gives the door nothing: response headers are already sent
(`http-backend` writes them before exec'ing `receive-pack`), a
zero-length chunk ends the body, and any other byte is fed to
`send-pack`'s parser. The only harmless bytes are sideband packets,
which makes the door a protocol-aware proxy that must parse the
client's capabilities and then converts upstream silence into a promise
it cannot verify. That moves the class into the door and removes
nothing.

## 4. The operator: do not fold the poll into the lease

What the operator consumes from `/status` (`status.rs:111-135`, a
cache refreshed only at phase changes): `phase`, through
`is_quiescible` = Serving, for Ready versus Starting and for the door's
dial-or-503 (`reconcile.rs:176-180`, `resolve.rs:506-521`);
`activity.idleSecs` for scale-to-zero (`hubstatus.rs:253`,
`idle.rs:155-172`); the rest is display. **Nothing reads
`epoch.lastRenewUnix`, `fenced`, `rpoClean` or `progress`**, so the
frozen `lastRenewUnix` the drill noted is diagnostic-only. Restart is
not an operator decision either: a fence exits 1, a refusal 78, the
kubelet restarts, and the claim-against-a-healthy-holder case is
prevented by the challenger's token-quiet rule (`lease.rs:110-112`),
not by any observation the operator makes.

The defects that exist are in the *consumer* and the *signal*, and no
transport fixes them:

- **D3, a blind poll is not neutral.** A failed poll with a Ready pod
  yields `Starting` (`reconcile.rs:178`), and the door waits on
  Starting (`resolve.rs:427`). The CRD promises a missed poll "must not
  take a live repository out of rotation" (`crd.rs:344-345`); the code
  does exactly that. The NetworkPolicy self-blinding warning
  (`flint_forge_operator.rs:151-160`) is this class.
- **D1, suspend is a race with a push.** The activity clock counts
  pushes only (`status.rs:112`) and the door stamps `requested-at`
  only on a wake, so a repository that is cloned constantly and never
  pushed is suspended after `suspendAfterSecs` and woken by the next
  clone with a 180 s hold. A push that lands as `replicas: 0` is applied
  meets the 30 s grace above.
- **D4, unverified.** Readiness is Serving-only and `Pushing` answers
  503 (`server.rs:499-511`), so a push longer than three probe periods
  makes the pod NotReady and the headless Service withdraws the DNS
  name the door dials. The push in flight has its connection; a second
  clone during it may not resolve. Needs a wire check.

C1 (operator reads the epoch cell) removes D3 and the NetworkPolicy
exception, at the price of the boundary: the operator has no `secrets`
verb and no `flint_store` dependency today, the CR names the syncer's
read-write Secret (`crd.rs:106-108`) mounted by the kubelet, and the
door in `--git-only` builds no credential. Every way to give the
operator a read of `git/epoch` (the write Secret, a second read-only
Secret per repository, or ambient identity as the lean operator does)
makes a credential-free principal into a read-everything one for
fields no decision needs. Rejected.

C2 (the syncer writes a coordination Lease) is the only shape that
removes polling, and it is real: compare `resourceVersion` advance
rather than `renewTime`, echo phase in annotations, and order the write
*after* a successful S3 renew or it outlives a 412 by a heartbeat. It
costs a ServiceAccount, a `resourceNames`-scoped Role and a RoleBinding
per repository (9,000 objects at 3,000) and 30 writes per second at
300 live, which is the fleet plan's B2 amplification. Revisit when the
300 polls per pass are the operator's cost, not before.

Cheapest correct moves, none of which change the transport: `phase_of`
keeps the last observed phase with an age when a poll fails and the pod
is Ready; read container exit codes so 78 sets `refused` as the CRD
says; stamp `requested-at` on ordinary requests, or count git-http
activity, so clones keep a repository awake; drop the four dead
`/status` fields or make them live.

Experiments, on the kind Cilium rig with no code: delete the
NetworkPolicy's status rule while a repository is 2/2 Ready and assert
whether the CR flips to Starting and a clone is held (D3); with
`suspendAfterSecs: 30`, start a 2 GiB push 100 ms after a reconcile and
record whether `replicas: 0` lands mid-push (D1), with a hand-stamped
`requested-at` as the control.

## 5. Processes on the push path

### D1 — the hook is a role of the syncer binary: do

The hook is one binary with two names (`flint_forge_hook.rs:59-70`,
symlinks at `Dockerfile.forge-git:56-58`) and already a consumer of the
syncer's library. Making it a subcommand of `flint-forge-syncer` is
~50 lines, drops one staged binary from the build, and, if the git
image is built `FROM` the syncer image or copies its binary, puts both
containers under one tag. Today the two images are two free-form
strings in the chart (`values.yaml:29,33`) that nothing checks against
each other; `Chart.yaml` already says `forge.4` while both images say
`forge.6`, and the published-artifact drill hit exactly this class. Add
a render test asserting the tags agree, and a `git --version` floor on
the hook's side: the 2.43 floor is asserted on the syncer's git
(`restore.rs:29-38`) and not on the one that runs `receive-pack`.

Do **not** collapse the two containers. The socket is a rendezvous by
name between processes with no common ancestor (the hook is spawned by
`receive-pack` under fcgiwrap; the syncer is not in that chain), so one
container does not turn it into a pipe. The split is what lets nginx
die without the lease dying, and the syncer's exit code is the pod's
restart-or-refuse semantic (`flint_forge_syncer.rs:245-260`), which a
supervisor would have to reproduce.

### D2 — the syncer as receive-pack: rejected

gitoxide has stream indexing with thin-pack resolution, ref
transactions and connectivity fsck; it has no server-side receive-pack
or upload-pack, and push itself was unshipped as of the last knowledge
of it. `upload-pack` must stay git's regardless (bitmaps, negotiation,
delta reuse), so git stays in the image and clones still fork. D2 is
therefore *writing* receive-pack: a second implementation of pack
parsing, hash verification, thin fix-up and the ref transaction on the
durable path, replacing exactly the checks acknowledged-means-durable
rests on (`gitcmd.rs:194-196`). Streaming the received pack to S3
while indexing does not even compose: the pack's name is its trailer
after thin fix-up, unknown until the end, so the upload goes under a
temporary key and a server-side copy, or content naming is abandoned,
which breaks the sweep predicate and the restore's stem match. 3-6k
lines against §15 decision 1. The experiment that would reopen it:
`strace -f -ttt` a push and decompose the measured 225 ms git intercept;
if fork and exec are under 150 ms of it, there is nothing to win.

### D3 — keep git, drop the hook: no

Receive-pack's interposition points are its hooks. The no-hook variant
(let receive-pack update refs, withhold report-status until the CAS)
moves refs before the CAS, so a fetch in that window serves refs the
bucket lacks and a crash makes the restore delete them; it also loses
the decider between reception and ref update that the staleness test,
the N-pushes-one-CAS batch and the `refs/for` merge all need.
`proc-receive` is already the minimum.

### D4 — the CRC pre-pass inside the compose: do

`packio.rs:34-49, 125-131` reads every pack over the whole-PUT limit a
second time to compute CRC64NVME before the multipart upload: ~70 s per
40 GiB, and it was the read that ticked no progress in run 3. CRC64NVME
composes across parts, so the checksum can be accumulated inside the
compose's own part reads and supplied at Complete. Same safety, one
read fewer. Measure with `/proc/self/io` `rchar` on a 2 GiB pack before
and after.

Not worth it: replacing `refs`, `has_object` and `is_ancestor` with
gix saves ≤ 15 ms per push for the dependency tree the crate avoids.

## 6. The drill rig

The scale rig is one 738-line script for five legs plus deploy,
teardown and node prep, and the other rigs total ~5,000 lines. What
should stay is the shape that made it useful: **the window-open and
`SWEEP=none` arms are the controls**, the runs that fail for the right
reason against the unfixed tree, and the run 1 vacuity (a 10 GiB
restore in 30 s inside a 60 s window) is why the window-closed guards
exist. Do not delete either. What can be simpler:

- **Provenance by stamp, not by marker.** S0 now greps the binary for
  one string per fix, and the list grows with every fix. Stamp the git
  revision into the image at build (`/etc/flint-forge/rev`, and the
  syncer's `--version` printing it, which today prints the crate
  version and is useless for this) and let S0 compare one value.
- **Auto-size the transfer.** `BIG_MB`, `MIN_BIG_MB`, `MAX_MB` and
  `TARGET_RESTORE_SECS` exist to make a transfer outlast the window.
  The rig measures the restore rate; it can pick the size from the
  window and the rate and refuse to run a vacuous leg, instead of
  documenting the clamp.
- **Promote the party table to legs.** The four experiments in §1's
  class (client stall 70 s, five concurrent pushes, rollout at +60 s of
  a long push, `receive.keepAlive=0` as the failing control) are each
  a few lines on this rig and would have found the three unlisted knobs.

## 7. Defects found by the exploration

| id | defect | consequence | status |
|---|---|---|---|
| X1 | `local_packs` listed every `pack-*.pack`; git migrates quarantine as `.keep`, `.pack`, `.rev`, `.idx` (`tmp-objdir.c`, `pack_copy_priority`, confirmed in v2.43.0), so a concurrent push's pack is visible before its index; a batch in step 4 uploads and names it, never uploads the index (a named pack is skipped for good), and a restore installs refs into objects git cannot see: `Refused`, exit 78, unrecoverable | data loss at restore | **fixed**: the listing requires the `.idx`; test `a_pack_without_its_index_is_neither_uploaded_nor_named`, which fails against the old listing |
| X2 | a pack refused at `proc-receive` is still uploaded and named when any other push in the batch is accepted (`batch.rs:197-202`); the design's "deleted locally once the `.keep` drops" (§4) has no code | cost, not integrity | open |
| X3 | nginx `client_body_timeout` and `send_timeout` unset (60 s defaults) | a 60 s stall mid-pack is a 408 | **pinned** to 3600 s beside the backend bounds; goes away under A3 |
| X4 | `FCGIWRAP_CHILDREN=4` | the fifth concurrent request queues until the door cuts it | open; goes away under A3 |
| X5 | `receive.keepAlive` not set explicitly | the guarantee rests on git's default | **pinned** explicitly at 5 s in the receive config |
| X6 | `terminationGracePeriodSeconds: 30` versus batches of minutes; SIGTERM seen only between batches | any rollout mid-push fails that push | open: decide whether a roll waits for the batch |
| X7 | a failed `/status` poll with a Ready pod yields `Starting`, and the door waits on it | a live repository leaves rotation on a blind poll | open |
| X8 | push-only activity clock; `requested-at` stamped only on wake | clone-only repositories are suspended and rewoken | open |
| X9 | readiness Serving-only, `Pushing` answers 503 | headless DNS withdrawn during a long push | unverified |
| X10 | two image tags nothing checks against each other; git floor asserted on the wrong image | the published-artifact drill's class | open, render test |

## 8. Order

1. X1, X3, X5 (done), X10: lines, not designs.
2. D4 the CRC in the compose, measured.
3. D1 the multi-call hook and one tag.
4. A3 the runner, with the keepalive-gap probe as its acceptance and
   the party-table legs added to the rig first, so the runner is
   judged by the class it claims to remove.
5. X7 and X8 in the operator; X6 and X9 decided on the wire.
6. C2 only when the poll count is the operator's cost.

What the formal model must carry from this note: the snapshot may name
a pack the bucket holds without its index (the listing is an
observation that can lie, exactly the class `feedback_model_the_observation`
records); a push may be told failed and land anyway (run 3, finding 3);
a rollout is a crash at 30 s from the batch's point of view.
