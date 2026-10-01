# Can flint-passthrough serve AWC's read-only ABS STS pod mounts? — assessment

Status: **ASSESSMENT — no code; produced 2026-10-01.** Nothing here is
built. It answers whether `flint-passthrough` can satisfy awc-docs
[PR #136](https://github.infra.cloudera.com/AWC/awc-docs/pull/136)
("Propose read-only pod mounts using ABS STS"), which is itself
specification-only, and it names the one change that would make the
answer yes.

Provenance: the question *"can flint-passthrough satisfy the requirements
in PR #136, noting it needs inline/ephemeral volumes rather than the
PV/PVC shape the AWC Mountpoint driver uses?"*. I read PR #136's
`proposal.md`, `design.md` and its three capability specs on the PR's
spec branch, then the driver source.

- **[V]** means I checked it at the `path:line` given, in the working
  tree at `cb89aa52`.
- **[I]** means I inferred it and it is NOT verified.

Line numbers are from `cb89aa52`. The summary of this assessment was
posted on PR #136 as
[issuecomment-1692344](https://github.infra.cloudera.com/AWC/awc-docs/pull/136#issuecomment-1692344).

---

## The answer, in one paragraph

**The node half is mostly already built, in the exact inline-ephemeral
shape the spec requires, and the blocker is one wire that was never
connected: `static` is the only credential arm that reads a
`nodePublishSecretRef`, and it is the only arm that does not refresh.**
PR #136 is three capabilities; `flint-passthrough` is only the third of
them (`sts-fuse-node`, N1–N5, plus S2). The sender/receiver handoff
(H1–H4) and the admission guard and cross-cluster readiness (S1, S3) have
no equivalent here and are new work regardless. The spec's trust model
routes *around* flint's strongest arm: Engine B pushes a session into a
Secret, so there is no pod token to exchange and `broker` mode — the
default, the one with a rotation soak behind it — is architecturally
unavailable, forcing flint onto the arm its own source calls "today's
trust level; the interim arm" (`creds.rs:19` **[V]**).

---

## 1. Scope — what is and is not flint's to answer

PR #136 declares 13 requirements across three capabilities.

| Capability | Reqs | flint-passthrough |
|---|---|---|
| `sts-mount-handoff` | H1–H4 | **Out of scope.** No Engine A sender / Engine B receiver equivalent. New work. |
| `sts-mount-isolation` | S1–S4 | **Partial.** S2 (node credential custody) covered. S1 (admission guard), S3 (receiver trust + dataset routing) are cluster-side. |
| `sts-fuse-node` | N1–N5 | **Mostly built**, with §3 and §4 below. |

`design.md:292` proposes a NEW Go module at
`awc-core/services/s3-fuse-csi`. That module is substantially what
`flint-passthrough` already is, which is the reason this note exists.

---

## 2. What already matches, including the inline/ephemeral requirement

This is the part worth checking first, because it is the spec's own
delivery mechanism rather than an alternative to it.

- **Inline ephemeral only, by refusal.** `attrs.rs:171-173` **[V]**
  rejects anything whose `csi.storage.k8s.io/ephemeral` is not `"true"`.
  Not one mode among several — the only one.
- **The `CSIDriver` matches `design.md:302-304` field for field** **[V]**
  (`flint-s3-csi-chart/templates/csidriver.yaml`): `attachRequired:
  false`, `podInfoOnMount: true`, `volumeLifecycleModes: [Ephemeral]`,
  `fsGroupPolicy: None`, `requiresRepublish: true`. No PV, PVC,
  StorageClass, provisioner or controller service.
- **N1's "an annotation alone SHALL NOT create a mount" is structural.**
  The selector `chert.us/mount` is a **volumeAttribute**
  (`attrs.rs:18` **[V]**, parsed `attrs.rs:103-140` **[V]**), not a pod
  annotation. There is no annotation path to disable.
- **N2's "no endpoints, shell fragments, host paths or flags from the
  application" is a closed allowlist.** `attrs.rs:128-137` **[V]**: a pod
  may name the CR, a presentation uid/gid and an audit string; unknown
  keys are refused and named. Bucket, prefix, endpoint, image,
  credentials and access all come from the CR.
- **N2 read-only is enforced by the filesystem, not a bind flag.**
  `resolve.rs:179-180` **[V]**: "mount-s3's filesystem itself is
  read-only, which no remount undoes." Context: ROX on the block/NFS
  driver turned out to be client-side only (F70), so a `ro` bind was
  judged insufficient on purpose.

---

## 3. The blocker

`identity.mode: static` is flint's `nodePublishSecretRef` path.
`creds.rs:115-132` **[V]** already reads exactly the tuple the spec
delivers, `AWS_SESSION_TOKEN` included. Three things are missing:

1. **Static credentials are never refreshed.** `node.rs:1374-1445`
   **[V]** — the republish handler has arms for `WebIdentity` and
   `Broker`; `Static` falls through `_ => {}` at `node.rs:1444`. The
   tuple is materialised into the worker's launch env once and never
   updated, so a 900-second session dies permanently at expiry. Fails
   **N3** outright.
2. **No generation tracking.** `VolumeState` carries `creds_expiration`
   but no generation (`state.rs:52-55` **[V]**), so N3's monotonicity
   rules have nothing to key on.
3. **No envelope validation.** Static reads `AWS_*` verbatim, so N1's
   last scenario (reject an envelope disagreeing with kubelet-asserted
   namespace / ServiceAccount / source) is unimplemented.

### Why the first two thirds of the fix are cheap

The broker arm already does what N3 asks, in two lines
(`node.rs:796-797` **[V]**):

```rust
let mut m = creds::door_arm(&st.nonce);
m.files.push(creds::CommFile { name: creds::CREDS_FILE.into(), bytes: creds::creds_json(&c), mode: 0o600 });
```

Four things fall out in flint's favour:

- **`Creds` is already the spec's tuple** — `creds.rs:38-45` **[V]** is
  `{access_key_id, secret_access_key, session_token, expiration}`, with
  expiration "RFC 3339, as STS returns it".
- **The worker's door is broker-independent** — it reads
  `comm/auth.token` and serves `comm/creds.json` over loopback
  (`crates/flint-s3-worker/src/main.rs:30-34`, `:563` **[V]**). Any arm
  that can produce a `Creds` can use it.
- **The door's auth token already exists for every mode** —
  `nonce: creds::new_nonce()` is set unconditionally at `VolumeState`
  construction (`node.rs:899`, `:1092`, `:1701` **[V]**).
- **The input already arrives and is discarded** — kubelet re-delivers
  Secret data in `req.secrets` on every ~60–90 s republish.

The hard part — reload with no remount, under live reads — is already
measured on this exact channel: a reader every 5 s for 30 min across
rotations, zero errors (`s3csi/e2e/results/2026-10-01-ec2-step4/`, leg
P8 **[V]**). Through the broker, not through a Secret; the mechanism is
shared, the path is not qualified.

---

## 4. The staging requirement, and the one thing that does not fit

[issuecomment-1692360](https://github.infra.cloudera.com/AWC/awc-docs/pull/136#issuecomment-1692360)
asks that a newly minted session be **usable on the receiver before it
displaces a working one**: a candidate can be structurally perfect and
still unusable because B has not loaded the new root or recovery-key
material, and installing it then interrupts reads the old session would
have served. **This invalidates the naive form of the §3 fix** —
validate-identity-then-overwrite is exactly the sequence being rejected.

Three of its requirements are already met or cheaper than they look:

- **The atomic switch exists.** `write_files` is temp →
  `set_permissions` → `chown` → `rename(2)` (`creds.rs:204-221`
  **[V]**).
- **A credential-validating probe exists and is already trusted.**
  `fuse.rs:228-230` **[V]**: a bounded `readdir` "proves the daemon
  serves lookups (and that its credential works) at the cost of one LIST
  against the store", used by `wait_ready` at publish
  (`fuse.rs:323-326` **[V]**). `read_dir(&p)?.next()` treats an empty
  directory as success — matching "an empty authorized listing is
  success".
- **Flint's documented objection to probing does not apply.** The
  republish probe is statfs-only (`node.rs:1455` **[V]**) because
  readdir there "would be a LIST per volume per minute"
  (`fuse.rs:231-233` **[V]**). A probe per *replacement* is ~1 per
  session lifetime — roughly 15× cheaper than what was rejected.

**What does not fit.** The probe must use the CANDIDATE credentials while
the PREVIOUS generation keeps serving. There is one `creds.json`, and the
mounter is the only component that makes S3 requests — so probing the
candidate through the mount requires installing it first, which is the
displacement being prohibited. Flint therefore needs a **plugin-side
probe**: the node plugin itself issues a bounded `ListObjectsV2` with the
candidate tuple, and only on success writes the new `creds.json`.

`aws-sdk-s3` 1.141.0 and `aws-config` (with `credentials-process`) are
already dependencies (`spdk-csi-driver/Cargo.toml:245-246` **[V]**), but
**nothing in `src/s3csi/` uses them** — only `src/tier/s3_acceptance.rs`
**[V]**. So this is a new outbound network path from the plugin, which
must honour `spec.endpoint`, path-style, region and Proxinator routing,
with its own timeout and TLS handling. **[I]** This is the step that moves
the work from "wire two existing halves together" to "plus a capability
the module does not have".

Worth noting the requirement is *easier* to satisfy in flint's shape than
in the spec's: the plugin is a privileged node component that can make an
S3 request without touching the tenant's mount, so staging is a clean
two-slot operation, whereas rclone would have to be restarted or
reconfigured to prove a candidate works. **[I]**

---

## 5. The change, in order

**Add a fifth mode; do NOT mutate `static`.** Today's `static` has no
expiration field at all (`creds.rs:115-132` **[V]**) because its users
are long-lived keys. The new arm must *require* an RFC 3339 expiration
and refuse a Secret without one, so bolting it onto `static` either
breaks existing users or makes the validation unenforceable. A separate
`identity.mode: stsSecret` keeps `static`'s contract intact.

1. **Route the new mode through the door.** Parse `secrets` into a
   `Creds`, require expiration, emit `door_arm` + `creds.json`. Reuses
   `creds_json`, `write_files`, the worker door. *Small.*
2. **Add the republish arm.** A `CredentialMode::StsSecret` branch at
   `node.rs:1374`: re-parse `req.secrets`, rewrite when newer. The
   per-volume lock (`self.lock(&vid)`) already serialises republishes for
   one volume, so generation ordering needs no new concurrency work.
   *Small.*
3. **Plugin-side candidate probe (§4).** Bounded prefix-scoped
   `ListObjectsV2` with the candidate tuple before committing; empty
   listing is success; recheck remaining lifetime and publication state
   before the `rename`. *The largest piece, and the new capability.*
4. **Two-watermark generation state.** Installed generation and
   pending/attempted generation tracked SEPARATELY in `VolumeState`, so a
   failed probe neither advances the installed watermark nor poisons the
   candidate — the same candidate must be retryable once material loads.
   Reject lower, idempotent on identical same-generation, reject under
   120 s remaining, never discard a still-valid installed generation.
   *Medium.*
5. **Envelope validation.** Compare namespace / ServiceAccount / source —
   and the root IDs and issuer/recovery-key version §4 requires — against
   what `attrs.rs` already parses. **BLOCKED:** the envelope schema is a
   cross-team contract that PR #136 specifies but has not built, and
   the review notes the nonsecret readiness interface is still
   unsettled with awc-docs PR #132. Build 1–4 against a provisional
   schema.

A side effect worth having: this **removes secrets from the child's
environment** entirely. `Materialized`'s Debug impl notes "Env VALUES may
be secrets (the static arm)" (`creds.rs:105` **[V]**); the door form
carries nothing sensitive in env, which strengthens S2 rather than merely
satisfying N3.

Already correct, no work: N3's "neither static provider caching nor
ambient node credentials SHALL extend storage authority" —
`base_env()` sets `AWS_EC2_METADATA_DISABLED=true` (`creds.rs:110-111`
**[V]**), and a stale `creds.json` fails visibly at expiry.

---

## 6. The smaller gaps

- **N5 capacity.** No hard worker cap, no `ResourceExhausted` on the 17th
  publication; flint surfaces kubelet's own capacity refusals as
  retryable (`node.rs:552`, `worker.rs:395` **[V]**). Defensible, not
  what the scenario tests.
- **N5 resources.** Defaults are 10m/64Mi requests, 1Gi limit
  (`flint-s3-csi-chart/values.yaml:233-235` **[V]**) against the
  specified 1Gi/500m requests and 4Gi/4CPU limits. Values change.
- **N4 repeat notification.** `MounterDead` fires only on probe
  *transition* (`node.rs:1456`, emitted `:1468` **[V]**), so a dropped Event is lost. N4
  wants re-notification on a 60 s interval until cleanup.
- **N3 mechanism.** The spec requires a *process*-credentials helper
  (`Version: 1`, cache caps 30/30/1 s); flint uses the container door.
  Both are valid SDK providers, but the acceptance criteria as written
  are not testable against the door and would need restating. Note
  `aws-config`'s `credentials-process` feature is already enabled
  (`Cargo.toml:245` **[V]**) if the process shape is ever wanted.
- **FUSE runtime.** `design.md:315-317` selects rclone v1.75.1; flint
  runs mount-s3 1.24.0. The spec calls its own choice "a qualification
  candidate, not a claim", so this is a divergence to settle — but S4's
  gate is written against rclone.

---

## 7. The design question to settle before any code

**What is the policy object?** PR #136 puts the source in an envelope
inside the Secret and has the driver validate it against pod metadata.
`flint-passthrough` puts it in a `FlintPassthroughMount` CR in the pod's
namespace, with `spec.consumers.readOnlyServiceAccounts` gating which
ServiceAccounts may mount (`resolve.rs:144-166` **[V]**).

Under the CR model Engine B creates two namespace objects before the pod
— the per-run Secret and a per-run CR — instead of one, and the envelope
check is satisfied by the CR's consumer list plus the kubelet-asserted
ServiceAccount, with the source in a schema-validated API object rather
than an opaque blob. Defensible and arguably better, but a real deviation
from the spec text. **[I]**

### One feature that conflicts

`spec.sharing.readOnly` shares a single mounter across pods of one class,
whereas N1 requires each pod receive "its own worker and target". Opt-in,
off by default. It also cannot revoke a single member before its pod
exits (`passthrough-read-only-mount-sharing.md` §10). If this driver were
adopted here, that knob should be **refused** in this configuration, not
merely left off.

---

## 8. To verify before building

1. **That `readdir` on a mount-s3 mount actually fails on a bad
   credential**, which `fuse.rs:228-230` asserts but I did not run. The
   whole staging design in §4 rests on a LIST being an authorization
   probe. **[I]**
2. **That a plugin-side `ListObjectsV2` can reach the same endpoint the
   mounter uses**, through Proxinator, with the CR's path-style and
   region. §4's probe is worthless if it validates against a different
   route than the mounter takes. **[I]**
3. **Re-run P8 against the Secret path** once step 2 lands. The existing
   soak rotates broker-minted keys; a Secret-driven rotation has a
   different upstream failure mode (Engine B stops updating), even though
   the reload channel is the same.
4. **Whether `creds_json`'s always-present `Token` field is right for a
   no-session static Secret.** `creds.rs:62-72` **[V]** says the Rust SDK
   requires `Token` for the refreshable form while the CRT tolerates its
   absence; a long-lived key with no expiration cannot use the door form
   at all, which is a second reason §5 adds a mode rather than changing
   `static`.
