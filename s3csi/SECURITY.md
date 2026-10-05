# What flint-passthrough guarantees about your credentials, and what it does not

The node plugin `s3.csi.chert.us` in passthrough mode, its broker
(`flint-s3-broker`) and its worker (`flint-s3-worker`), read as built on
2026-10-03 (`75f1d4a6`). A claim here names the property, where the
code enforces it, the test or rig leg that FAILS when it is broken, and
the window it leaves open. A row with no falsifier says so: that is the
finding, not a gap in the write-up. Lean shares the driver and inherits
P1–P3, P7, P8, P10 and P11; `lean/SAFETY.md` is its own claim.

Why this exists: the sts note (`docs/plans/passthrough-sts-secret-mode.md`)
said the plugin's only egress was "the broker, over a CA-pinned client".
Reading for this matrix found no pinning anywhere and plain http by
default (§4.1). Claims that live in prose drift; this file is where they
are checked against the code, and `s3csi/e2e/` is where they are
checked against a cluster.

## 1. The promises

**P1. A credential never reaches a tenant container.** The tenant pod
is admitted untouched: no sidecar, no injected env, no Secret volume, no
webhook. The keys exist on the node and in the worker pod only.

| | enforced by | in code |
|---|---|---|
| the tenant pod spec is not edited | there is no webhook; the plugin writes nothing into a pod | `s3csi/node.rs` `node_publish_volume` |
| the worker's env carries no secret | its env is the mode, the comm path and the preStop budget | `s3csi/worker.rs` `build_pod` |
| keys are written host-side, 0600, into the worker's memory-backed `comm` emptyDir | tmp file created at its mode (`create_new`, 2026-10-03), chown to the worker uid, rename | `s3csi/creds.rs` `write_files` |
| served on the loopback door, token-gated | `127.0.0.1:9911`; 401 without `auth.token` | `flint-s3-worker/src/main.rs` `door_response` |
| the door is reachable only inside the worker pod's network namespace | the worker is never `hostNetwork`, and admission refuses it | `worker::build_pod`; `flint-s3-csi-chart/templates/workers-policy.yaml` |
| **exception: the `static` arm** | the pod's Secret keys go to mount-s3's ENVIRONMENT over the launch socket, not the door; by design of the interim arm | `creds::static_arm`; leg S5c pins it |

**P2. A mount is granted only to a pod the CR names, and the broker
checks it again.** The plugin decides from identity kubelet authored;
the broker decides from a TokenReview and its own registration.

| | enforced by | in code |
|---|---|---|
| the pod's SA, namespace and uid come from kubelet's keys only; pod-authored keys are refused by name | `VolumeAttrs` | `s3csi/attrs.rs` |
| the CR is fetched in the pod's namespace | `resolve_cr` | `s3csi/resolve.rs` |
| the SA must be in `consumers`; an absent list denies | `authorize` → `MountConsumers::access` | `s3csi/resolve.rs`, `s3csi/policy.rs` |
| the broker validates the pod-bound token with its audience | TokenReview, `identity_from_review` | `s3csi/broker.rs` |
| the registration is found by nonce and must match the token's namespace, SA and pod uid, and the asked CR and mode | `decide` | `s3csi/broker.rs` |
| the CR is fetched again in the TOKEN's namespace and `consumers` re-checked | `assume` | `s3csi/broker.rs` |
| register and deregister need the node principal | `assume`/`register` | `s3csi/broker.rs` |

**P3. A token minted on one cluster buys nothing on another.** The
broker asks its OWN API server; a foreign token fails TokenReview.

| | enforced by | in code |
|---|---|---|
| foreign token → 400 `InvalidIdentityToken` | `review` | `s3csi/broker.rs` |

**P4. Read-only is enforced by the mounter, below the bind.** A pod's
`ro` bind is the belt; the mounter's own flag and the credential's
scope are the braces.

| | enforced by | in code |
|---|---|---|
| `--read-only` on mount-s3's argv for a read-only CR or a read-only grant | `mounter_args_for`; `publish_passthrough` appends it; shared mounts always carry it | `passthrough/mounter.rs`, `s3csi/node.rs` |
| the kernel mount is `MS_RDONLY`; the bind comes from an ro stage | `open_and_mount`, `bind_mount` | `s3csi/fuse.rs` |
| the broker credential is narrowed to read | `registered_access`, `decide` | `s3csi/node.rs`, `s3csi/broker.rs` |
| **the contract is NARROWING, not refusal** | a read-only consumer asking read-write gets read-only (lean refuses instead: a syncer cannot run narrowed) | `resolve::authorize`, `policy.rs` |

**P5. A replacement credential is installed only when it is newer and
alive (`stsSecret`).** The rules are a pure function of (installed,
candidate, now).

| | enforced by | in code |
|---|---|---|
| unknown Secret keys refused by name; the kubelet token key ignored | `parse_sts_secret` | `s3csi/creds.rs` |
| the envelope (`namespace`, `serviceAccount`, `mount`), when present, must match the pod and CR | `check_envelope` | `s3csi/creds.rs` |
| same generation, same expiration, same key fingerprint → idempotent; same generation and expiration with OTHER keys → refused (2026-10-03); lower generation → refused; same generation, other expiration → refused; under 120 s left → refused; else installed | `sts_replace_decision`, `creds_fingerprint` (SHA-256 of the key tuple, kept in the state; no key material) | `s3csi/creds.rs`, `s3csi/state.rs` |
| a refusal never touches the installed file; `CredentialRefused` once per reason, `CredentialReplaced` on install | `refresh_sts_secret` | `s3csi/node.rs` |
| the timing budget: refresh ≥ the mounter's one ask + a republish period; the note says the same lead in both places | `the_credential_timing_budget_agrees` | `s3csi/node.rs` tests |

**P6. A refused refresh takes the key away from the pod's own mounter;
a shared class keeps it.** Revocation is bounded by the credential the
mounter already holds in memory.

| | enforced by | in code |
|---|---|---|
| a broker REFUSAL on an unshared volume removes `creds.json`; an OUTAGE keeps it | `republish`, `revocation_removes_the_key`, `classify_exchange` (4xx final, else transient) | `s3csi/node.rs`, `s3csi/creds.rs` |
| a TokenReview the API server did not ANSWER is an outage (503 `ServiceUnavailable`), not a refusal: only its verdict on the token refuses (2026-10-03) | `review_outcome`, `ReviewError` | `s3csi/broker.rs` |
| the pod is told: `CredentialRefreshFailed` (class wording on a shared member) | `republish` | `s3csi/node.rs` |
| the mount is never torn down on a refusal | `republish` | `s3csi/node.rs` |
| the door answers 503 once the file is gone | `door_response` | `flint-s3-worker/src/main.rs` |

Revocation windows, per mode (the time a pod can still read after its
entitlement is withdrawn):

| mode | the window | bounded by |
|---|---|---|
| broker (default) | the rest of the credential in the mounter's memory, plus up to one republish before the refusal is seen | `node.credsLifetimeSecs` (900) and the broker's `maxLifetimeSecs` (3600); refresh is attempted under 420 s left |
| `stsSecret` | the rest of the current session; the controller that stops minting is the revoker | the controller's session length |
| `static` | never: nothing refreshes | the Secret's own rotation, out of band |
| shared read-only (any backend that shares) | the member's pod lifetime: the class key stays for the siblings, and the refused pod is bound to the same superblock | `spec.sharing.readOnly` is opt-in; the CRD text says so |

**P7. The privileged part is the plugin, once per node.** The one act
that needs privilege, `open(/dev/fuse)` + `mount(2)`, is the plugin's;
the mounter runs where privilege cannot reach.

| | enforced by | in code |
|---|---|---|
| the worker pod: non-root, all capabilities dropped, not privileged, no escalation, read-only root fs, seccomp RuntimeDefault, no SA token | `build_pod` | `s3csi/worker.rs` |
| admission pins workers to the caller's node and image and refuses privileged/hostNetwork/hostPID/hostIPC/foreign hostPaths/SA tokens | ValidatingAdmissionPolicy | `flint-s3-csi-chart/templates/workers-policy.yaml` |
| the plugin is the privileged DaemonSet; it never uses hostNetwork or hostPID | `node.yaml` | `flint-s3-csi-chart/templates/node.yaml` |
| the worker never opens `/dev/fuse`: it receives the descriptor | `open_and_mount` runs in the plugin; `SCM_RIGHTS` | `s3csi/fuse.rs` |

**P8. One node, many pods: one worker per volume, one class per shared
key.** A pod cannot reach another pod's worker, door or state.

| | enforced by | in code |
|---|---|---|
| one worker per volume id; a pod with another volume's id is never adopted | `worker_name`, `ensure` | `s3csi/worker.rs` |
| the door is in the worker's own network namespace, behind a per-volume nonce | `build_pod`; `new_nonce` | `s3csi/worker.rs`, `s3csi/node.rs` |
| state dirs cannot escape the plugin root | `volume_dir` | `s3csi/state.rs` |
| the shared class key is (node, namespace, CR, uid, gid, credential mode, argv) | `share_key` | `s3csi/state.rs` |

**P9. Sharing is read-only and only for credentials that are a function
of the CR alone.**

| | enforced by | in code |
|---|---|---|
| a sharing CR is refused with `static` and `stsSecret` | `validate` | `passthrough/spec.rs` |
| shared only for ambient, or the broker on an `sts`/`static` backend; `rest` and unknown backends get a mounter of their own; an unreadable broker is a RETRY | `sharing_decision` | `s3csi/node.rs` |
| a shared mount is always `--read-only` | `publish_shared` | `s3csi/node.rs` |
| every member registers and exchanges its OWN credential into the class's comm dir | `publish_shared` | `s3csi/node.rs` |
| a refused member's refresh leaves the class key alone | `revocation_removes_the_key` | `s3csi/node.rs` |
| a join replaces the class's mounter only on evidence of death (ENOTCONN, no mount, worker not Running); a mounter that is silent but not dead is waited for, never replaced under its members | `shared_liveness`, `fuse::classify_probe` | `s3csi/node.rs`, `s3csi/fuse.rs` |

**P10. Key material is not logged.**

| | enforced by | in code |
|---|---|---|
| the worker masks argv values whose flag names a secret, token or password | `redact` | `flint-s3-worker/src/main.rs` |
| the door logs `Expiration` only | `door_response` | `flint-s3-worker/src/main.rs` |
| `Creds`, `Materialized`, `SaToken`, `Backend`, `StaticKeys` have redacting `Debug` | | `s3csi/creds.rs`, `s3csi/attrs.rs`, `s3csi/broker.rs` |
| the broker logs identity fields, never the token or the form | `assume` | `s3csi/broker.rs` |

**P11. A node's worker count has a ceiling.** `workers.maxPerNode`,
counted at the API before every worker create; a join creates none.

| | enforced by | in code |
|---|---|---|
| `ResourceExhausted` + `WorkerCapacity` over the ceiling | `worker_capacity`, `count_live_on_node` | `s3csi/node.rs`, `s3csi/worker.rs` |
| the count and the create are one step: a node-wide lock from the count until the worker is an API object | `worker_capacity` (returns the guard), its three callers | `s3csi/node.rs` |

**P12. The `static` arm is interim.** Keys from the pod's
`nodePublishSecretRef`, delivered by kubelet (the node SA has no
Secrets RBAC), never refreshed, never shared.

| | enforced by | in code |
|---|---|---|
| read from `req.secrets` only | `static_arm` | `s3csi/creds.rs` |
| republish does nothing for it | `republish` | `s3csi/node.rs` |
| sharing refused | `validate` | `passthrough/spec.rs` |

## 2. What the protocol ASSUMES

| assumption | how it is verified | if it is false |
|---|---|---|
| kubelet authors the pod identity keys in `volume_context` (the CSI contract) | `attrs.rs` refuses pod-authored spellings; S6, S7 | a pod could name another SA; P2 falls |
| the pod network between plugin and broker is encrypted by the SERVICE MESH (decided 2026-10-03: TLS is the mesh's job, not the chart's; see §4.1) | not by this repo — the mesh's PeerAuthentication STRICT on the broker's namespace is the operator's | a token and the keys it buys are readable in flight |
| mount-s3 honours `Expiration` and asks the door once, 300 s before it | measured on real STS sessions (R1, 2026-10-02) | the budget in `the_credential_timing_budget_agrees` is wrong; rotation gaps |
| the store enforces the credential's expiry and session policy | real STS: R1; AWS: A6. The kind rig's `static` backend does NOT (synthetic `Expiration`) | P6's windows are longer than stated; S10 on kind is a weaker result than its AWS twin |
| the kubelet root on the node is root-only | kubelet's own | the persisted SA token and the state nonce are readable (§4.6) |
| node clocks are within `STS_MIN_SECS_LEFT` of the controller's | not verified | a candidate is refused as near-dead, or installed already dead |

## 3. How the claim is checked today

| promise | unit tests (`cargo test --lib`) | rig legs (`s3csi/e2e/`) | newest run | falsifier gap |
|---|---|---|---|---|
| P1 | `door_refuses_without_the_token_and_serves_with_it`; `door_arm_points_at_loopback_with_a_token_file`; `files_are_written_with_mode`; `passthrough_worker_is_unprivileged_and_hostpath_free` | S1, S3, S31 (no secret in mount-s3's env) | campaign 3 run 4 (2026-09-30); sts run 1 (2026-10-02) | S35 (2026-10-04, kind): the door listens on 127.0.0.1 only; a sibling pod and the node are refused at the worker's IP; the worker test asserts no `hostNetwork`/`hostPID`/`hostIPC` |
| P2 | `token_review_identity_needs_authenticated_sa_and_audience`; `decide_refuses_each_break_in_the_chain_by_name`; `a_grant_is_the_registration_narrowed_by_the_cr_never_widened`; `consumers_absent_denies_and_names_the_field`; `listed_sa_passes_others_named_in_the_refusal`; `unknown_attributes_are_refused_by_name`; `non_ephemeral_and_missing_pod_info_are_refused` | S6, S7, S10; A11 | campaign 3 run 4 | the absent-pod-uid-extra path (§4.2) has no test |
| P3 | (generic) | M2 | design doc only: "multi 22/0" 2026-09-04; no results README | none written since |
| P4 | `write_flags_track_read_only`; `a_read_only_volume_registers_a_read_grant_on_the_wire` | S5, S24 (shared mount refuses a write); R3, R6, R9; A9; O5 | campaign 3 run 4; `run-rights.sh` has no results README | the append in `publish_passthrough` has no unit test |
| P5 | `sts_secret_parses_the_tuple_and_its_envelope`; `…refuses_each_missing_or_malformed_field_by_name`; `…refuses_unknown_keys_by_name_but_ignores_the_token_key`; `sts_envelope_mismatch_is_refused_per_field_and_absence_passes`; `sts_replace_decision_table` (incl. a reused generation with other keys); `the_key_fingerprint_follows_the_keys_and_not_the_expiration`; `the_credential_timing_budget_agrees` (+ two mutation controls, 2026-10-03) | S31 (step 6, the reused generation, written 2026-10-03, NOT RUN); R1 | sts runs 1–2; R1 13/0 (2026-10-02) | S31 step 6 until it runs |
| P6 | `a_refusal_removes_the_key_only_from_an_unshared_mounter`; `exchange_errors_sort_refusals_from_outages`; `exchange_outages_are_unavailable_and_refusals_are_denied`; `a_token_review_the_api_server_did_not_answer_is_unavailable_not_refused` | S10, S29, S8 (outage control); L3 | campaign 3 run 4; review-fixes run 2 (2026-09-30) | no leg fails the API server under the broker (S8 fails the broker itself) |
| P7 | `passthrough_worker_is_unprivileged_and_hostpath_free` | S2, S3, S19 | campaign 3 run 4 | no leg sets TLS or the NetworkPolicy; nothing checks the broker link |
| P8 | `uid_gid_must_be_integers`; `dir_names_cannot_escape`; `name_and_hash_are_stable_and_label_sized` | S15, S24; P3 (16 tenants) | campaign 3 run 4; kind S24+S30 (2026-10-01) | cross-pod door reachability; state-dir mode |
| P9 | `only_read_only_members_with_cr_scoped_credentials_share`; `sharing_cannot_run_on_a_per_pod_secret`; `the_class_is_node_namespace_cr_owner_mode_and_argv`; `a_shared_mount_round_trips_and_membership_is_a_set`; `a_shared_worker_is_named_by_its_class_and_says_so`; `a_shared_mounter_is_dead_only_on_evidence_and_silence_is_busy`; `only_enotconn_is_a_dead_mount_and_silence_is_not` | S24, S28, S29, S30; S34 | kind S24+S30 46/0 (2026-10-01); sts run 1; S34 61/0 + S9 + S24 86/0 (2026-10-03, after the §4.12 fix) | a hung (not dead) shared mounter is waited for indefinitely (§4.12) |
| P10 | `redaction_masks_secret_shaped_arguments`; `creds_json_is_the_container_credentials_shape`; `our_audience_token_is_picked_and_never_debug_printed`; `the_start_up_line_never_prints_a_secret`; `token_is_written_once_at_0600_and_reloaded` | S3, S31 | sts runs | nothing greps events or the broker log for key material |
| P11 | `the_worker_ceiling_admits_up_to_and_not_past_its_number` | S33, S36 | S36 on kind 2026-10-04: 1.57.1 peak 9 over a ceiling of 7 (the race); fixed, peak 7, one of six mounted | — |
| P12 | `static_arm_needs_both_keys_and_passes_region`; `sharing_cannot_run_on_a_per_pod_secret` | S5c | sts run 2, 2/2 | — |

## 4. What is NOT claimed

1. **Transport to the broker is not TLS, and nothing pins.** The chart's
   broker URL is `http://…svc:80` (`_helpers.tpl`, whose comment says the
   TLS wiring "is not done yet"); `FLINT_S3CSI_BROKER_CA` is never set by
   the chart, and where it is set `add_root_certificate` ADDS a root to
   the default store rather than pinning; the broker serves http unless
   both `FLINT_S3B_TLS_CERT/_KEY` are set. On an untrusted pod network
   the 1-hour pod-bound token and the 15-minute keys it buys are readable
   in flight. The worker NetworkPolicy is off by default and egress-only,
   and the plugin has none. Earlier notes that said "CA-pinned" were
   wrong and are corrected. **Decided 2026-10-03: the chart stays http;
   encryption on the pod network is the service mesh's job** (Istio
   mTLS, PeerAuthentication STRICT on the broker's namespace), so this
   is §2's assumption, not an open item.
2. **The broker's identity binding is as strong as the TokenReview it
   gets.** The audience is checked only when `status.audiences` is
   present, and the pod-uid binding only when the token carries the
   pod-uid extra; neither the pod name nor the node is verified; one
   node principal serves every node, so any node's plugin can register
   for any pod; `/v1/status` is unauthenticated; `requireRegistration:
   false` turns the nonce binding off.
3. **Passthrough narrows, lean refuses** (P4). A read-only consumer
   asking read-write gets a read-only mount and no error.
4. ~~**`stsSecret` judges "unchanged" on (generation, expiration).**~~
   **Closed 2026-10-03:** the state keeps a SHA-256 fingerprint of the
   installed key tuple, and the same generation and expiration with
   other keys is refused ("a generation is never reused"); a state
   written before the fingerprint existed is judged as before until its
   next install.
5. ~~**An API-server failure at the broker reads as a refusal.**~~
   **Closed 2026-10-03:** a TokenReview the API server did not answer
   (transport error, timeout, 5xx, or a 401/403/429 on the broker's own
   request) is 503 `ServiceUnavailable`, which the plugin keeps the key
   through; only the API server's verdict on the token is 400. Still
   true: `CredentialRefreshFailed` fires on every failed refresh,
   outages included.
6. **Two things persist on node disk under the kubelet root:** the
   pod-bound SA token (`volumes/<vid>/token`, 0600) and the nonce in
   `state.json`, which is both the door's auth token and the
   registration binding. ~~`write_files` creates its tmp file under the
   umask before the chmod~~ (closed 2026-10-03: born at its mode).
   ~~Lean's `launch.json` carries the arm's env, static keys included,
   with default permissions in the comm dir~~ (closed 2026-10-05: born
   0600, `persist_launch`).
7. **uid/gid: the pod's `volumeAttributes.chert.us/uid` overrides the
   CR's `spec.uid`** (as built; the design doc §3.6 said the opposite
   order and is corrected). It is presentation inside the pod's own
   mount and the class key for sharing, not a grant; `--uid 0` reaches
   mount-s3 while the worker process maps 0 to 65534.
8. ~~**The ceiling is check-then-act** (P11).~~ Fixed 2026-10-04: S36
   put nine workers on a node capped at seven with 1.57.1 (six pods at
   once, room for one); a node-wide lock now spans the count and the
   create, and the same burst peaks at seven.
9. **Revocation is bounded by memory, not by the plugin.** Nothing on
   the node can take back the credential mount-s3 already holds; the
   store's own session expiry is the only hard stop. Under sharing, per
   member, it is the pod's lifetime.
10. ~~**The door's unreachability from outside the worker is by
    construction, not by a leg.**~~ S35 (2026-10-04) tries it: loopback
    only in the worker's netns, refused from a sibling pod and from the
    node, while inside the token is served. A pod on the HOST network of
    the same node is still not tried — the worker test asserts the
    worker is not one.
11. **S10 on kind runs against a store that never expires anything.**
    Its real twin is A6/R1 on AWS.
12. **A HUNG shared mounter blocks new members; it is not replaced.**
    Until 2026-10-03 the opposite held — a BUSY one was replaced as if
    dead, stranding every member — and was fixed that day: death now
    needs evidence (ENOTCONN, no mount, or a GET saying the worker is
    not Running), and a join that meets silence answers `Unavailable`
    and is retried by kubelet (`shared_liveness`; kind S34 61/0 —
    the join met the mounter silent and waited, and a killed worker is
    still replaced, on ENOTCONN). The price: a mount-s3 that hangs
    without dying, in a worker that stays Running, is indistinguishable
    from a busy one, so new members wait until it dies or its worker is
    deleted; its members see `MounterUnresponsive` every republish
    meanwhile. The history: the join path's `shared_mount_alive` asked `fuse::wait_ready_opts` for a statfs
    within 3 s, and that function returns the same `Err` for ENOTCONN, a
    statfs error and a timeout. Mountpoint answers statfs without S3, so
    network-bound reads never slow it (S34, four readers at 20mbit: max
    23 ms) — but statfs is a FUSE request like any other, and when every
    one of Mountpoint's FUSE threads (16 by default; the plugin passes no
    `--max-threads`) is parked on a slow read, it queues. Kind S34
    2026-10-03, 24 readers of 8 MiB at 10mbit: statfs took up to 3971 ms,
    a fifth member's publish replaced the live mounter, and all 24
    readers got `Transport endpoint is not connected`; none of them got a
    MounterDead event inside the leg's window. Reachable on any shared
    class whose members read more files at once than the mounter has
    threads, over a link slower than they want.

## 5. The open list

| # | what would close it | cost |
|---|---|---|
| 1 | ~~TLS to the broker by default in the chart~~ — not this repo's: the service mesh encrypts the pod network (decided 2026-10-03; §2) | — |
| 2 | ~~broker: a TokenReview transport error answers 503, not 400~~ — done 2026-10-03 (`review_outcome`, `ReviewError`; the node principal's review answers 503 the same way) | — |
| 3 | ~~`sts_replace_decision`: a reused generation with other keys is refused~~ — done 2026-10-03 (`creds_fingerprint` in the state; S31 step 6 on kind 2026-10-03, S31 24/24) | — |
| 4 | ~~legs: the door from a sibling pod and the node; the capacity race~~ — done 2026-10-04 (S35; S36 found the race, fixed); still open: a NetworkPolicy leg, an M2 results README | S |
| 5 | ~~`write_files`: create the tmp at its mode~~ — done 2026-10-03 (`create_new` + `mode`; a stale tmp is replaced) | — |
| 6 | ~~the worker test asserts `hostNetwork` absent; `worker_capacity` gets a unit test~~ — done 2026-10-04 (positive-controlled) | — |
| 7 | ~~run S34 (the probe under load) on the box, and calibrate its floor~~ — run 2026-10-03: four readers never reach the probe; 24 do, and the join replaces the mounter (§4.12) | — |
| 8 | ~~lean's `launch.json` written 0600~~ — done 2026-10-05 (`persist_launch`: created 0600, a stale tmp removed first; positive-controlled) | — |
| 9 | ~~the join path replaces a shared mounter only on evidence of death, and answers silence with `Unavailable`~~ — done 2026-10-03 (`shared_liveness`, `fuse::Probe`; S34 61/0 with both arms) | — |

Docs corrected with this file (2026-10-03): the sts note's "CA-pinned
client"; the sharing design's §5 paragraph that said a refusal removes
the shared key; the CSI design's §4.4 static row (the keys go to
mount-s3's environment, not a profile file) and §3.6 uid order.
