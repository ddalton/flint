# flint-passthrough: `identity.mode: stsSecret` — a controller-fed credential, replaced in place

Status: **BUILT AND VERIFIED 2026-10-02** — on kind (legs S31–S33 of
`s3csi/e2e/run-s3csi.sh`) and against a REAL bucket with REAL 15-minute
STS sessions (`s3csi/e2e/aws-sts-secret.sh` R1: a rotated pod reads
across three expiries with zero errors; an unrotated one fails 1 s after
its session expires). Results in §7 and
`s3csi/e2e/results/2026-10-02-kind-sts-secret/`.

Provenance. PR #5 (`docs/plans/passthrough-sts-secret-mounts-assessment.md`,
on its branch) asked whether this driver can serve awc-docs PR #136's
read-only pod mounts — inline ephemeral CSI volumes with an STS session in
a `nodePublishSecretRef` Secret — and found one wire never connected:
`static` was the only arm that read that Secret and the only arm that
never refreshed. The decision (2026-10-02): assume AWC adopts this driver
in place of its proposed Go module, and build what the driver can address.
The candidate probe (the assessment's §4) is **deferred**, and §6 says why
and where it belongs.

Line references are to the working tree of 2026-10-02 (on top of `520ef8d7`).

---

## 1. What was built, against the requirements

| Req | What it asks | What the driver does now | Where |
|---|---|---|---|
| N3 refresh without remount | a new session reaches the FUSE process without unmounting | `stsSecret`: the Secret is parsed on every republish and, when it offers a higher generation with life left, `creds.json` is rewritten atomically; the mounter re-fetches it from the loopback door before `Expiration`. No remount — the same mount-s3 PID across every generation (S31) | `creds.rs` `parse_sts_secret`, `sts_replace_decision`; `node.rs` `refresh_sts_secret`, `credential_arm` |
| N3 monotonicity | never roll back; idempotent on the same generation | the generation rules in §2; the installed and the attempted generation are kept apart in `VolumeState` | `state.rs` `creds_generation`, `attempted_generation`; `creds.rs` `StsReplace` |
| N3 no ambient authority | static caches and node credentials must not extend authority | unchanged: `AWS_EC2_METADATA_DISABLED=true`; a stale `creds.json` fails visibly at expiry | `creds.rs` `base_env` |
| S2 node custody | the node holds the credential, the pod never sees it | the door form: `creds.json` 0600 in the worker's memory-backed comm dir, the worker's uid, nothing sensitive in mount-s3's environment (S31 reads the process's environ) — which today's `static` arm cannot say | `creds.rs` `door_arm`, `write_files` |
| N1 one worker per pod | each pod its own worker and target | `spec.sharing.readOnly` is REFUSED on a CR with `stsSecret` (as with `static`), at CRD validation and again in the plugin's sharing decision | `passthrough/spec.rs` `validate`; `node.rs` `sharing_decision` |
| N1 envelope | reject a Secret that disagrees with kubelet's view of the pod | each envelope field PRESENT in the Secret — `namespace`, `serviceAccount`, `mount` — must equal what kubelet asserted and what the pod named; a key outside the schema is refused by name. The schema is PROVISIONAL (§2) | `creds.rs` `sts_envelope_check`, `STS_KEY_*` |
| N4 repeat notification | keep saying the mounter is dead | `MounterDead` is emitted on EVERY republish while the probe fails, not on the transition only; kubelet's cadence (~60–90 s) is the interval | `node.rs` `republish` |
| N5 capacity | refuse the 17th publication | `workers.maxPerNode`: a ceiling counted at the API before every worker CREATE; over it, `ResourceExhausted` and a `WorkerCapacity` Warning. Off by default; AWC's number is 16 | `node.rs` `worker_capacity`; `worker.rs` `count_live_on_node`; chart `workers.maxPerNode` |
| N5 resources | 1Gi/500m requests, 4Gi/4CPU limits | a values change, not code; the chart's defaults stay (10m/64Mi, 1Gi limit) | `flint-s3-csi-chart/values.yaml` `workers.resources` |

One Rust-specific fix underneath: the republish handler's match on the
credential mode ended in a wildcard, and `static` sat under it. It is now
exhaustive over `(mode, token)`, so a sixth mode cannot be forgotten the
way the fourth was (`node.rs`, the comment above the match).

---

## 2. The mode

### The Secret (PROVISIONAL schema)

The pod names it with `nodePublishSecretRef`; kubelet fetches it with
kubelet's credentials and re-delivers it in `NodePublishVolumeRequest.secrets`
on every republish. The node ServiceAccount needs no Secrets RBAC, exactly
as for `static`.

| Key | Required | Meaning |
|---|---|---|
| `AWS_ACCESS_KEY_ID` | yes | |
| `AWS_SECRET_ACCESS_KEY` | yes | |
| `AWS_SESSION_TOKEN` | no | an STS session always carries one; an expiring static key does not, and the rig uses that |
| `AWS_CREDENTIAL_EXPIRATION` | yes | RFC 3339, as STS returns it (`Expiration`) |
| `generation` | yes | a whole number the controller increments on every mint |
| `namespace` | no | if present, must equal the pod's namespace (kubelet-asserted) |
| `serviceAccount` | no | if present, must equal the pod's ServiceAccount (kubelet-asserted) |
| `mount` | no | if present, must equal the `FlintPassthroughMount` the pod named — flint's policy object, where #136 has a "source" |

Anything else is refused **by name**, with the accepted keys listed. The
one exception is kubelet's own `csi.storage.k8s.io/serviceAccount.tokens`
(present when `serviceAccountTokenInSecrets` is on), which is not the
Secret's. The strictness is deliberate: a misspelt envelope key must not
pass as an absent one.

The envelope PR #136 specifies is not settled (it depends on awc-docs
PR #132). These three fields are what flint can check today against what
kubelet asserts; when the real schema lands, `STS_KEY_*` in `creds.rs` is
the one place to change, and the check stays "every present field must
agree".

### The rules (`creds::sts_replace_decision`)

Given the installed generation `g` with expiration `e` (what the door
serves now) and the Secret's candidate `g'` with `e'`:

| Candidate | Verdict | On the tenant pod |
|---|---|---|
| nothing installed yet, ≥ 120 s left | install | (first publish; no event) |
| `g' == g`, `e' == e` | idempotent — nothing happens, nothing is said | — |
| `g' < g` | refused: never rolls back | `CredentialRefused` |
| `g' == g`, `e' != e` | refused: a replacement needs a HIGHER generation | `CredentialRefused` |
| `g' > g`, under 120 s left | refused: as good as expired by the time the mounter fetched it | `CredentialRefused` |
| `g' > g`, ≥ 120 s left | install, atomically; the installed watermark moves | `CredentialReplaced` (Normal) |
| unparseable, unknown key, envelope disagrees | refused, by name | `CredentialRefused` |

Every refusal leaves `creds.json` exactly as it was: a still-valid
installed credential is never discarded. A refused generation is not
poisoned — the same generation re-offered with life installs (S31 step 3)
— which is what keeping the attempted generation apart from the installed
one buys. The same refusal is said once, not once per republish (the
message is kept in the state and compared); a different refusal, or an
install, resets it.

"Unchanged" is judged on generation and expiration: the state keeps no
key material.

### Timing

Kubelet republishes every ~60–90 s. A candidate under 120 s is refused.
And the consumer has its own clock, measured in R1 (§7): **mount-s3's
CRT asks the door again exactly 300 s before the credential it holds
expires, once, and takes whatever is there**; it asks again only once
that credential has expired, and then on every request. So the
replacement must be INSTALLED by T−300 s, and the republish that
installs it can be ~90 s away: a controller should offer the next
generation **at least seven minutes before the current one expires**
and must bump `generation` every time it mints. With a 15-minute
session that is a rotation at the 8-minute mark. (The broker arm
used to refresh at 270 s left, i.e. AFTER the CRT's last ask, so the CRT
learned of the new key only at the old one's expiry, on the first request
after it: zero errors in P8, but no margin. It now refreshes at 420 s
left — `BROKER_REFRESH_SECS` in `node.rs` — for the same reason. P8
should be re-run with the door log as its oracle to show the switch now
happens with the margin.) An idle mount does none
of this: with no request to sign it holds its credential past expiry
and fetches on its next request, which is harmless so long as the door
then holds a live generation.

### What the worker sees

The door form, the same as the broker arm: `AWS_CONTAINER_CREDENTIALS_FULL_URI`
at the loopback door and the door's auth token file. The CRT inside
mount-s3 re-fetches `creds.json` before `Expiration` and never knows a
Secret existed. The first publish refuses a Secret under the floor the
same way (a `FailedMount` naming it; kubelet retries, the controller
refreshes, the same pod mounts).

---

## 3. The ceiling: `workers.maxPerNode`

Set (the chart knob, `FLINT_S3CSI_MAX_WORKERS_PER_NODE`), the plugin counts
this node's live workers **at the API** — not the watch cache, which
mid-relist would under-count and admit — before every worker create: a
per-pod mounter, a shared class's first mounter, a lean syncer. A pod
joining a shared mounter creates nothing and is not counted against it;
the worker this publish is about is excluded, so a retry adopting its own
Pending worker is never refused. Over the ceiling the publish returns
`ResourceExhausted` and the tenant gets a `WorkerCapacity` Warning naming
the count, the knob and what frees room; kubelet retries. A misspelt
value is a startup error, not "no ceiling". Zero or unset: kubelet's own
admission stays the only limit, surfaced as before (`WorkerNotAdmitted`).

---

## 4. `MounterDead`, repeated

The probe's verdict is still kept per volume, but the event is emitted on
every republish while the mounter is dead, and its text says so. Each is
its own Event object (the plugin does not aggregate), so a reader counts
them and a watcher sees each. One per ~60–90 s per dead volume is the
cost; a dead volume is an outage, and that is cheap for an outage.

---

## 5. Sharing and `stsSecret`

Refused by name, as for `static`, at CRD validation (`spec.sharing.readOnly`
with `identity.mode stsSecret`) and again in `sharing_decision`. Two
reasons: the members' authority would not be the same function of the CR
(each pod's Secret is its own), and the spec this mode serves wants a
worker per pod outright.

---

## 6. Deferred, and why

**The candidate probe** (PR #136's reviewer: a replacement session must
be proven usable before it displaces a working one). Not built, on
purpose. The assessment put it in the node plugin as a `ListObjectsV2`
with the candidate tuple. That is the wrong component:

- the plugin never speaks S3 today; its only egress is the broker, over a
  CA-pinned client. S3 egress from a privileged DaemonSet is new TLS
  trust, proxy routing and NetworkPolicy — and the assessment's own
  verify item 2 concedes the probe may take a different route than the
  mounter, which makes it worthless in the one case it exists for;
- "B has not loaded the new root material" is known to the receiver that
  mints and writes the Secret. The right place to prove a session usable
  is before the Secret is written, in that receiver — which PR #136
  classes as new work regardless. Then the node's contract is exactly §2:
  a newer, well-formed generation displaces the old one atomically;
- if a node-side proof is still demanded, it belongs in the **worker
  pod**, which shares mount-s3's network namespace and route and runs
  non-root with every capability dropped. The worker has no S3 client
  (its deps are libc, nix, serde), so that is a SigV4 signer plus TLS in
  that binary or the SDK in a deliberately thin image. Real work, but it
  proves the right thing.

**The envelope schema**: provisional (§2); one constant block to change.

**A process-credentials helper** (`Version: 1`): not built. The door is
an equally valid SDK provider and mount-s3 already consumes it; the
spec's acceptance text would need restating against the door. If the
process shape is ever wanted, `aws-config`'s `credentials-process`
feature is already on and a worker subcommand printing the comm file is
small.

**The FUSE runtime**: the spec qualifies rclone; this driver runs
mount-s3. A divergence to settle with AWC, not code.

**Out of this driver's reach**: the sender/receiver handoff (H1–H4), the
admission guard (S1), receiver trust and dataset routing (S3).

---

## 7. How to know it worked

**Unit tests** (`spdk-csi-driver`, box): `parse_sts_secret` — the tuple
and envelope parse, the token is optional, each missing or blank required
key is named, a bad expiration and a bad generation are named, an unknown
key is refused by name while the kubelet token key is ignored only when
the caller says so; `sts_envelope_check` — each field refused on
disagreement and passed on agreement, absence passes; the decision table
— every row of §2 including the 120 s boundary (exactly 120 installs,
119 refuses) and "idempotent near its end says nothing"; the mode parses
and round-trips; the sharing refusal names it in `spec.rs` and
`sharing_decision`; the lean operator calls it Cooperative like `static`.

**Kind** (`run-legs.sh S31 S32 S33`, plus S5c and S24 as controls that
the `static` arm and sharing still behave): RESULTS BELOW.

**The consumer's side, newly observable.** Until this change nothing had
ever observed the mounter FETCHING a replacement: the worker's door
logged nothing per request, and P8's oracle sampled `creds.json` — what
the plugin WROTE. Under the broker's static backend the key never
changes, so P8 could not tell a re-fetch from none: a mounter that never
re-read the door would have read on with the same key and passed. The
worker now logs one line per fetch with the expiration it served
(`flint-s3-worker: door served creds.json Expiration=…`, never the key),
and S31 waits for mount-s3 to fetch generation 2 BEFORE generation 1
expires. That is the reload channel judged where the consumer sees it.
P8's claim should be read accordingly: zero errors across rewrites, not a
proven re-fetch — until it is re-run with the door log as its oracle.

**What is NOT shown here.** A real STS session (with a token) against a
real bucket — a credential the STORE enforces. The kind rig's RustFS
checks neither the expiration nor a token, so "reads continue across the
replacement" proves the plumbing, not acceptance of a new session. The
proper test, in order: (1) a real session from `sts get-session-token`
(15-minute minimum) on a drill IAM user against a real bucket; (2) the
KNOWN-BAD control first — one session left to expire with no replacement,
and the reader FAILING at expiry, without which zero errors is vacuous;
(3) a stand-in controller rewriting the Secret with the next generation
every ten minutes for three or more sessions, a reader every 5 s with
zero errors, and the door log showing each generation fetched before its
predecessor expired. The box's kind cluster can run it (`STORE=s3` is a
rig knob and mount-s3 in kind reaches AWS); it needs a bucket and an IAM
user. Also unshown: the first-publish refusal of a near-dead Secret
(unit-tested only).

### Results (box, 2026-10-02; `s3csi/e2e/results/2026-10-02-kind-sts-secret/README.md` has the tables)

- **Unit:** 116 passed, 0 failed (`s3csi::`, `passthrough::`,
  `lean_operator::boundary`); 23 by name.
- **Kind run 1** (`S5c S31 S32 S33 S24`): 52 ok, 2 bad — S5c ran 5 s
  after setup (timing; 2/2 on re-run) and S31's fetch wait on an IDLE
  mount (the CRT fetches lazily; the leg now reads every 5 s). S31 20/21
  otherwise: every rule in §2, the envelope, the unknown key, the
  once-only refusal, the same mount-s3 pid through four generations.
  S32 3/3: two `MounterDead` in 130 s. S33 6/6: ceiling 6 → the seventh
  publish refused with `WorkerCapacity`, kubelet's `FailedMount` carries
  it, lifted → the same pod mounts. S24 23/23.
- **Kind run 2** (S31 with the reader): 21/21 — mount-s3 fetched
  generation 2 from the door 213 s before generation 1 expired.
- **Real bucket, real sessions (R1): 13 ok, 0 bad.** Rotated pod: 355
  reads, 0 errors, across the expiries of sessions 1, 2 and 3.
  Unrotated pod: failed 1 s after its session expired and never
  recovered (192 of 361 reads) — the known-bad that makes the zero
  mean something. The mounter fetched generations 2, 3 and 4 299, 298
  and 297 s before their predecessors expired, once each; the dead
  door was asked 361 times. Four distinct session keys landed; three
  `CredentialReplaced` events. Bucket, user and key torn down and
  verified by unfiltered listings.

---

## 8. For the controller's author (Engine B, or anyone minting sessions)

1. Create the `FlintPassthroughMount` in the pod's namespace with
   `identity.mode: stsSecret` and the ServiceAccount in `consumers`.
2. Create the Secret in the pod's namespace with the keys of §2,
   `generation: 1`, and ≥ 120 s of life. Then the pod, naming both.
3. On every mint: write the whole Secret again with `generation + 1` and
   the new expiration, at least four minutes before the current one
   expires. Never reuse a generation with different contents.
4. Watch the pod's Events: `CredentialReplaced` (Normal) says a
   generation landed; `CredentialRefused` (Warning) says why one did not,
   once per reason. `FailedMount` on first publish names a Secret that
   is malformed or near-dead.
5. Do not set `spec.sharing.readOnly` on such a CR; it is refused.
6. Prove a session usable BEFORE writing it into the Secret (§6).
