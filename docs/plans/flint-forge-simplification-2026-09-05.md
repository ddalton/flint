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
| **A3 one Rust CGI runner replaces nginx + fcgiwrap; door and container split kept** | **accepted on the wire** 2026-09-05 (runby, run 7: the party table's legs green, the nginx control fails every one) | fcgiwrap buffering, the 4-worker ceiling, nginx's 3600 s cutoffs and two unset 60 s defaults | 382 lines of runner, four integration tests over real git; reverses design decision 15.1 for the runner |
| B1 early ack | **reject** | the wait | the guarantee: a successor restores strictly to the snapshot and deletes refs it does not name; the client's retry says "Everything up-to-date" |
| B2 progress lines on the sideband | optional | silence for humans | nothing: same pipe as the keepalives, so every buffering knob stays load-bearing |
| B3 the door as the only waiter | reject | nothing | the class into the door, which would then promise liveness it cannot verify |
| C1 operator reads the epoch cell from S3 | **reject** | the `/status` poll, the NetworkPolicy exception | a credential-free operator becomes a read-everything principal, for fields no decision reads |
| C2 syncer echoes the lease into a k8s Lease object | later | polling, HTTP surface, NetworkPolicy rule | +3 RBAC objects per repo, 30 writes/s at 300 live, a new lie (write before a failed renew) |
| C3 operator infers from the pod (readiness, exit codes) | complement | — | no push clock; suspend would rest on one signal |
| D1 hook = a role of the syncer binary, one tag for both images | **done** | one staged binary, the tag-drift class | the hook is `flint_forge::hook`, both binaries dispatch on their invoked name, the git image installs the syncer binary as both hooks, the chart derives both images from one tag, the operator warns on two |
| D2 syncer as receive-pack (gix/git2) | **reject** | 4 forks, the hook | a second pack/ref implementation on the durable path; gitoxide has no server side |
| D3 keep git, drop the hook | no | — | proc-receive *is* the interposition point; without it refs move before the CAS |
| D4 CRC inside the compose's part reads | **done** | the second full read of every large pack (~70 s at 40 GiB) | none: the parts go up in order, so a streaming CRC over them in the upload loop is the object's; proven against S3 |
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

Status 2026-09-05: **built**, ahead of the bitmap work on the user's
order. `flint-forge-gitcgi` (`forge/syncer/src/bin/`, 382 lines, hyper
1 behind the `gitcgi` feature) execs `git http-backend` per request
with the CGI environment taken from the request, streams both
directions as they arrive, parses the CGI `Status:` line, kills the
child when the client goes, answers 503 past a declared ceiling (64
by default) and relays the two LFS routes to the syncer's listener; it
sets no timeout of its own, since the door owns the idle bound. Four
integration tests run it against real git over real HTTP
(`forge/syncer/tests/gitcgi.rs`): the clone/push/fetch round trip, the
LFS relay, the ceiling answering rather than queueing, and the
keepalive oracle — a pre-receive that writes six lines a second apart
must be seen a second apart (first within 3 s, spread ≥ 4 s); through
fcgiwrap they had arrived together with the report. The git image now
holds one process (`Dockerfile.forge-git`: `ENTRYPOINT
flint-forge-gitcgi`; `nginx.conf` and `entrypoint.sh` deleted) and a
clone-push-clone smoke passes through the container's port.

Accepted on the wire the same evening (cluster runby, `forge/e2e/scale/`
run 7, `scale-20260905-183906` and `-191810`; the control
`-192355`): the party table's legs S5–S10 — a client stalled 70 s
mid-pack acknowledged; five pushes stopped mid-body holding five
receive-packs with a request beside them answered at once; a rollout
mid-push SIGKILLing the batch at 32 s with the client told failed, the
bucket unchanged and the retry converging (X6 answered); 48 keepalive
packets across a 232 s hook wait with a longest gap of 5.8 s; the
`receive.keepAlive=0` control cut 30.5 s after the upload under a 30 s
door bound; eight concurrent clones at the tip with eight upload-packs
at the peak — and the control arm, the same syncer behind the last
nginx + fcgiwrap image, failing exactly S5 (502 at 60 s), S6 (four
receive-packs, the request queued 60 s), S9 (49 packets in one burst
with the report) and S10 (four upload-packs for eight clones) while
its plain pushes passed. X3 and X4 are closed by removal; X5 is
carried; X6 is measured.

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
| X3 | nginx `client_body_timeout` and `send_timeout` unset (60 s defaults) | a 60 s stall mid-pack is a 408 | **gone with nginx** (A3); run 7 S5: a 70 s stall acknowledged through the runner, a 502 through the control |
| X4 | `FCGIWRAP_CHILDREN=4` | the fifth concurrent request queues until the door cuts it | **gone with fcgiwrap** (A3); run 7 S6/S10: five receive-packs and eight upload-packs at the peak through the runner, four through the control |
| X5 | `receive.keepAlive` not set explicitly | the guarantee rests on git's default | **pinned** explicitly at 5 s in the receive config |
| X6 | `terminationGracePeriodSeconds: 30` versus batches of minutes; SIGTERM seen only between batches | any rollout mid-push fails that push | **measured** (run 7 S7): SIGKILL at 32 s, told failed, bucket unchanged, retry converges, orphans swept — clean, and the push is lost; whether a roll waits for the batch is still the decision |
| X7 | a failed `/status` poll with a Ready pod yields `Starting`, and the door waits on it | a live repository leaves rotation on a blind poll | open |
| X8 | push-only activity clock; `requested-at` stamped only on wake | clone-only repositories are suspended and rewoken | open |
| X9 | readiness Serving-only, `Pushing` answers 503 | headless DNS withdrawn during a long push | unverified |
| X10 | two image tags nothing checks against each other; git floor asserted on the wrong image | the published-artifact drill's class | **done** by D1: one `server.tag` in the chart, the operator warns on two tags, the git image asserts the floor at build |
| X11 | a takeover of a repository nobody has published skipped the snapshot rotation ("the first CAS's If-None-Match is the fence"); a straggler mid-batch on the old epoch lands its create after the successor serves, and the successor's own first CAS is what 412s | the successor is fenced by its predecessor's push (no loss: the restart restores it) | **fixed**: the rotation creates the empty snapshot; found by `formal/ForgeSync.tla`'s first strict run; test with control |
| X12 | a successor that died between its takeover CAS and its rotation restarts through self-recognition, which skipped the rotation ("our own previous process died with its writes"); the straggler from the epoch before still holds a valid If-Match | same class as X11, one crash later | **fixed**: every claim but a released cell's rotates; the model's second strict run; test with control |
| X13 | **the holder has no lease term of its own.** A renewal that fails with anything but a 412 is "keep serving reads, keep trying" (`lease.rs`, since `1674f561`), and nothing in the syncer or the operator reads `lastRenewUnix`; a holder cut off from S3 while a challenger is not is deposed after six quiet polls and serves stale refs until it reaches S3 again and sees the 412. Falsifier 11's third leg ("the server then exits") passed on 2026-09-04 because its second leg pushed: any batch error exits the serving loop, the restart's claim failed against the dead S3, and the crash loop read as standing down — with no push, the server serves for as long as the outage lasts | stale reads from a deposed holder under an asymmetric partition; the design's failure-model row and falsifier 11 described a mechanism the code never had (both corrected 2026-09-05) | **built** `40b4a079` (2026-09-05): `Hold` records when a renewal last landed on the runtime's clock; the renewer refreshes `renewal_overdue` (no renewal for `heartbeat × QUIET_POLLS`) on every heartbeat; `Facts::serving()` — what `/healthz`, the readiness probe, the headless DNS and the door see — is false while overdue; the lease is kept, the process stays up, the next landed renewal restores readiness with the same epoch; `/status` carries `epoch.renewalOverdue` + `termSecs`. Two unit tests on virtual time (the falsifier-11 shape in-process, the memory double's `inject_epoch_renew_failures`); the second FAILS against the old rule. f11 reordered: stand-down BEFORE any push, judged by ready=false with restarts UNCHANGED. **Wire re-run owed** (the runbz campaign's forge image predates it) |
| X14 | **the door's wake bound is a constant and a restore is proportional to the repository.** 180 s at the door; the restore fetches every pack, then installs refs, then runs fsck, then reports serving; a challenger restores only after it claims, and `Recreate` hands the successor a fresh emptyDir | the 40 GiB drill repository restores in 139 s from the delete; the design's own arithmetic for a wake after an unclean death — 60 s of quiet polls plus that restore — is 199 s against 180 s, and git clients do not retry the 503; every roll is a full restore of unavailability | open — refs served from the snapshot before the packs land (walgit's refs level) and a waiting challenger that fetches the snapshot's packs before it claims turn both into O(delta); until then derive the bound from the snapshot's pack bytes rather than fixing it |
| X15 | **no undo.** The snapshot is replaced in place, versioning is OFF (design §3), the bare repository has no reflog and lives in an emptyDir; `repack -a -d` drops unreachable objects and the sweep deletes the old packs after the grace | a force-push or a bad merge is unrecoverable at the storage layer within at most `repack_threshold` (24) pushes; Continuity keeps every state, walgit retains superseded packs for a provenance window | open — cheapest shape: one immutable `snapshot.<seq>` copy beside the pointer per batch (a log entry, which also makes a stale reader's catch-up O(delta) instead of a restore), the sweep's reference set extended to the retained copies, repack keeping unreachable objects for the same window |
| X16 | **a transient store error inside a batch is a restart.** Before the CAS the batch retries nothing: every push in it is told `ng`, `run_and_report`'s error exits the serving loop, the pod restarts, re-claims, rotates, reconciles the cache (cheap — the emptyDir survives a container restart) and runs `fsck --connectivity-only` (not cheap on a large repository) | one S3 500 costs a restart, an fsck and every queued push | open — retry the batch under the writer lock with jittered backoff while the CAS has not been attempted; packs are immutable and content-named, so a repeated upload is idempotent |
| X17 | **the upload starts when `proc-receive` runs**, after quarantine migration, so a large push pays transfer then upload in series where Continuity writes the packfile to disk and S3 at once | large-push latency; the split on the 40 GiB run (1113 s) was never measured | open — measure first |
| X18 | **compaction has no tiers.** Past `repack_threshold` a full `repack -a -d -b` re-uploads the whole repository (measured `22807f9b`; 33× the bytes and an 816 s push on the wire, §9.1) | cost proportional to the repository every 24 pushes | **designed and built 2026-09-06** — `docs/plans/forge-compaction-tiers-design.md` (three candidates, six refutations, candidate C: geometric folds of plain packs by bytes beside the loop, one bitmap on the base, nothing new in the bucket); `forge/syncer/src/fold.rs`; `FLINT_FORGE_FOLD_FACTOR=0` keeps the old rule as the control until the re-match (§9.1) has run |
| X19 | **the snapshot carries a map of every ref** and is rewritten by every batch | at 466 k refs (walgit's reference monorepo) tens of MB per CAS and per restore; under 1 MB at agent-fleet ref counts | open — only for the monorepo case |
| X20 | **`batch_window_ms` (400) is a fixed wait** from the first push of a batch | every solitary push waits the window (0.48 s of a 1 KiB push's 0.58 s, §9.1); pushes arriving during a batch already queue naturally on the channel | **built 2026-09-06** — the collector drains what queued while the previous batch ran and waits for nothing at window 0, the new default; a window > 0 is kept as a knob; three tests on virtual time, the timed window as the control |
| X21 | **connectivity is proved at restore only**; nothing audits the snapshot against the bucket while serving | a lifecycle rule or a hand delete in the bucket is found at the next wake, by a refusal | open — a periodic HEAD of every named pack, sized, is one round per pack |
| X22 | **one pod and ~5 Kubernetes objects per repository**; Continuity serves millions of tiny repositories with one replica each and no per-repository control-plane object | the fleet case the design records as trigger (b) for N:1 | recorded in design §2; the server is multi-repo-capable by construction |

X13–X22 were found on 2026-09-05 by reading Cursor's Continuity
(2026-08-18) and walgit's design documents against the code, for page 7
of `docs/architecture/forge/`. None is a data-loss class: X13 is a
consistency defect the design claimed not to have, X14–X16 are
availability and recoverability, the rest are cost and scale. Three of
them are the neighbours' own answers and would be built as one wave if
forge keeps its place on the wire against walgit (§9): X15's log entry
per batch, X14's refs-first serving, X18's tiers.

## 8. Order

1. X1, X3, X5 (done), X10: lines, not designs.
2. D4 the CRC in the compose (done: `ComposeSpec::crc64: None`, hashed per part on a blocking thread beside its PUT; the memory double adopts the content's CRC, S3 validates ours; the gated `s3_compose` test passed against a real bucket, 4 parts, checksums equal).
3. D1 the multi-call hook and one tag (done; X10 with it).
4. A3 the runner, with the keepalive-gap probe as its acceptance and
   the party-table legs added to the rig first, so the runner is
   judged by the class it claims to remove — **built and accepted on
   the wire 2026-09-05** (run 7 on runby; the nginx control fails every
   leg it must).
5. X7 and X8 in the operator; X6 and X9 decided on the wire.
6. C2 only when the poll count is the operator's cost.
7. From the prior-art comparison (2026-09-05): X13 first — a term on
   the holder's clock, symmetric with the challenger's; lines, not a
   design — then, only after the walgit control arm has run (§9), X15
   (the log entry), X14 (refs first) and X18 (tiers) as one wave; X16
   as a small design; X17 measured before anything is built; X19–X22
   recorded, not scheduled.

The formal model exists now: `formal/ForgeSync.tla`, eight runs in
`scripts/check-tla.sh` (strict, liveness, five mutations, one
required-fail probe). Its first two strict runs refuted the module
against the code — X11 and X12 above — and both were fixed the same
day; the liveness run's first two executions refuted the model itself
(a queued push left waiting by a clean release, where the process's
exit closes every hook socket; a `NoSuchUpload` exit budgeted as a
crash), fixed in the module with no code change. What it carries from
this note: the snapshot may name
a pack the bucket holds without its index (the listing is an
observation that can lie, exactly the class `feedback_model_the_observation`
records); a push may be told failed and land anyway (run 3, finding 3);
a rollout is a crash at 30 s from the batch's point of view.

## 9. The walgit control arm — pre-registered 2026-09-05, run the same day (§9.1)

The architecture document's last page ends with "the next honest
comparison is walgit on the scale rig, push for push". This section is
that comparison, pre-registered: the arms, the legs, the metric and
the pass rule of each, and the verdict rule, written before anything
ran so the result could not be read to taste. It ran the same day on
the user's go; the results and the reading are §9.1, and the plan
above is kept as it was written, with the legs that did not run
(P3, P6, P8) still in its table.

**The question.** If walgit wins every leg forge passes, then "walgit
behind the door, with the export as a reader of its log" is a real
alternative and is written into the design as the decision it would
be, with its costs (a second store trait and protobuf formats in the
bucket, a pre-1.0 single-author dependency with no compatibility
promise, the door as a shim in front of `X-Walgit-Principal`, no pod
identity). If it does not, forge's core has earned its place on the
wire, which no document can settle, and X15, X14 and X18 are built as
one wave with the neighbours' shapes.

**The arms differ only in the server.** Forge is HEAD's runner image.
walgit is `tobi/walgit` at a pinned commit, built from its
`Containerfile` (no published image; node 24 + rust 1.97 + protobuf;
git ≥ 2.47 inside), configured with the S3 backend on the same bucket
under its own prefix, `[placement] serve/maintain` = everything,
`server.auth.mode = "token"`, `cache.mode = "disk"` on the same
NVMe-backed emptyDir, `bundles.require = false`, `wal.batch_window`
set to forge's 400 ms (one run at 0 for both if P1 shows the window),
compaction at its defaults. Both are reached DIRECTLY by the same
agent pod and the same stock git: the door cannot front walgit (it
routes to FlintRepo Services and sets `X-Remote-User`), so forge's arm
sets `X-Remote-User` from the client, which the rights drill showed is
what reaching the port already means, and walgit's sends its bearer.
The door's bounds are absent from both arms; S8 is not run.

**Legs, each with its metric and its rule.**

| leg | what | metric | rule |
|---|---|---|---|
| P1 | push latency at 1 KiB, 64 MiB, 1 GiB, 10 GiB; 5 reps, arms interleaved per rep | wall to `ok` | within the rep-to-rep spread |
| P2 | push rate: 32 agents pushing 4 KiB commits to distinct branches for 60 s | acknowledged pushes/s; S3 requests per push (CloudWatch request metrics per prefix) | the number forge has never measured; recorded either way |
| P3 | acknowledged means durable: S4's shape, the kill placed inside the multipart upload by `list-multipart-uploads` | told ok ⇒ in the bucket, told failed ⇒ unchanged; orphans left | both must hold; orphans counted |
| P4 | falsifier 2: two pushes to one ref from stale bases | exactly one winner, the loser told stale, nothing acknowledged lost | both must hold |
| P5 | cold start, the 1 GiB and the 10 GiB repository: forge's pod deleted, walgit's cache dir wiped | time to the first `ls-remote`, time to the first clone | X14's number; walgit claims refs in < 1 s |
| P6 | a roll mid-push (S7) and a roll idle | unavailability window; the push's fate | recorded |
| P7 | the read side: S10's 8 concurrent clones, then the 1,000-clone storm with `transfer.bundleURI` on for both (walgit's weekly bundle cut by a maintainer pass first) | peak `upload-pack`s; server egress; wall | within spread on egress |
| P8 | the party table: a 70 s client stall (S5), five stopped pushes and a sixth request (S6) | acknowledged after the stall; the sixth answered at once | walgit's own bounds are unknown — measured |
| P9 | repack amplification: 48 pushes of 8 MiB | bytes uploaded per push over the run | X18's number |
| P10 | S3 outage, f11's shape with the stand-down leg BEFORE the push leg | does the server stop serving reads it cannot verify | forge loses until X13; recorded, not scored |
| P11 | undo: force-push over a branch, then recover the previous tip | walgit `wal materialize --at-seq`; forge has nothing | forge loses by construction (X15); recorded, not scored |

Excluded, because there is no counterpart: the export, pod identity
and per-ServiceAccount policy, idle-to-zero and the wake, the web UI.

**The verdict rule.** walgit wins if it passes every P1–P9 leg forge
passes and beats forge by more than the rep-to-rep spread on P1, P2,
P5 and P9. A leg walgit loses on a default is re-run once with the
knob named and the re-run recorded beside the first; a leg that needs
a code change is a loss. walgit's own goal document says the monorepo
on a small host is its target and the long tail of tiny repositories
is served, not tuned for; the plan says so too, and P2 and P5 at 1 GiB
are where that shows if it does.

**Phase 0 — no cloud, first.** Build walgit's image (the remote x86
build node, or the Mac with `--platform linux/amd64`), run it in kind
against the composition rig's MinIO, push and clone with stock git,
cut a bundle, confirm the S3 backend and token auth. Anything that
fails here is a finding about walgit's maturity and is recorded as
such, not as a leg. **Phase 1** is a runby-class cluster (CP + 2
workers, i4i.xlarge, pure spot), ≈ $2 per campaign plus the build,
`forge/e2e/scale/` grown by an `ARM=walgit` deploy and the P-legs
beside the S-legs, results in `results/` with the walgit commit in
the log's first line. Phase 1 needs a go.

### 9.1 Results — runs 1 and 2 on runbz, 2026-09-05

Run on a trove cluster (CP + 2 × i4i.xlarge, all spot, us-west-1),
forge `drill-be76cc9c` (before X13) and walgit `e5295e6` on one worker,
the agent alone on the other. Run 1 was every leg of the rig
(`forge/e2e/walgit/results/compare-20260905-220006.log`); run 2 re-ran
P7, P5, P11 and P10 after the rig defects run 1 exposed were fixed
(`compare-20260905-224804.log`). Legs P3, P6 and P8 of the table above
were not built into the rig and did not run. The full tables, the
CloudWatch sums and the rig's own defects are in
`forge/e2e/walgit/README.md`; what follows is the reading.

| leg | forge | walgit | who |
|---|---|---|---|
| P1 1 KiB · 64 MiB · 1 GiB, median of 5 | 0.58 · 2.55 · 27.4 s | 0.10 · 1.88 · 30.8 s | walgit beyond spread at 1 KiB and 64 MiB; **forge beyond spread at 1 GiB** |
| P4 concurrent `--force-with-lease` | one winner, loser told stale | same | both hold |
| P9 48 × 8 MiB: wall; bytes to S3 | 1021 s (one push waited 816 s); 12.84 GB = 33× the content | 34 s; 0.40 GB = 1.05× | walgit, 30×: X18 |
| P2 32 pushers, 60 s | 1.1/s, median 79 s; 5.4 GB uploaded (two full repacks inside the minute) | 11.2/s, median 2.9 s; 4 MB uploaded | walgit, 10×: X18 again |
| P7 8 clones of 1 GiB | 64–65 s, 126–128 MiB/s | 64 s, 128 MiB/s | draw: the client NIC |
| P5 cold start: refs / complete clone | 119 / 138 s (run 1), 30 / 48 s (run 2) | run 1 the first clone refused (503, git does not retry); run 2 0 / 20 s | walgit on refs: X14; the clone itself a draw |
| P11 undo | none (X15) | recovered by hand: `wal materialize --at-seq` gives the pre-force tip, complete | by construction |
| P10 the bucket cut off 90 s | reads served; still ready at 90 s (pre-X13); push failed; clean recovery | reads and the push hang to their timeouts (every read is a conditional GET); still ready at 90 s; clean recovery | recorded, not scored |
| the bucket after both runs | 40.4 GB in 462 objects, five full packs of 5.6–7.0 GiB | 15.5 GB in 3,064 objects, one 4.4 GiB fold | for ≈ 8 GB of content |

**Under the rule.** walgit passed every leg forge passed and beat forge
beyond the rep-to-rep spread on P2, P5 and P9, and on P1 at two sizes
of three. It did not beat forge at 1 GiB, where stock `receive-pack`
plus one multipart upload finished 3.5 s ahead at the median with the
ranges disjoint, and its P5 pass needed a client that retries a 503,
which stock git is not. The rule's letter is met by neither side. What
the log settles is where forge loses and why, and it is one cause
almost everywhere: X18. The full repack every 24 packs re-uploaded the
repository five times in an hour of pushing, held one 8 MiB push for
816 s, and carried 5.4 GB of upload into a minute of tiny pushes; it is
33× on bytes and an order of magnitude on rate. Beside it, X20 is
0.48 s on a lone 1 KiB push and X14 is the difference between 30 s and
0 s to the first `ls-remote` after a cold start. P10 recorded what X13
was built for: the pre-X13 holder stayed ready through 90 s without a
renewal. And one thing walgit's shape costs showed on the wire:
walgit answered no read while its bucket was cut off — the `ls-remote` at +5 s hung to the rig's 60 s timeout, because every read there is a conditional GET on the manifest — where forge served from its clone. Verified reads are the safer choice under a partition with a live challenger and the costlier one under a partition with none; X13 is forge's middle, serve for six heartbeats, then stop.

**The decision, and why it is not taken here.** "walgit behind the
door" is not made real by this log: the legs walgit wins are the legs
whose forge-side fix was already on the list, walgit lost the largest
push, and it refused its first cold clone to a stock client. Forge has
not earned its place either: at rate on the write path it is ten times
behind, which no joint-by-joint argument answers. So the decision is
the re-match, and its terms are set here so it cannot be read to
taste: build X20 (a lone push pays no window) and X18 (tiers; the
design is `forge-compaction-tiers-design.md`, whose one real question
is the multi-pack index against an immutable layout), run this rig
again on a fresh cluster with the same legs and CloudWatch's bytes per
push as the headline; if walgit then still wins P2 and P9 beyond
spread, "walgit behind the door, with the export as a reader of its
log" is written into the design as the decision, with the costs named
at the top of this section. X15 is owed either way — P11 is a loss by
construction — and X14 is the next number after these two.
