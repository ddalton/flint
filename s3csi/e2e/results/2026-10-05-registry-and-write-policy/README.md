# 2026-10-05 — the registry split (S37) and the write session policy (write-grant drill), on the box

Two fixes from the 2026-10-05 passthrough gap analysis
(`s3csi/SECURITY.md` §4.13 and §4.14), each with a known-bad arm.

## S37 — two broker replicas, eight tenants refreshing on every republish

Kind `flint-s3csi` (one node, x86_64) on the Linux box, from
`run-s3csi.sh setup` with this tree's chart and `CREDS_LIFETIME=120`
(so every republish refreshes: 120 s is under the plugin's 420 s refresh
point). The leg installs `broker.replicas=2`, publishes eight tenants
of the `datasets` CR on the node, reads every 5 s for 300 s, then asks
both replicas' logs, the tenants' events and every worker's door.

| run | log | images | result |
|---|---|---|---|
| 1 | `s37-run1-1.57.1.log` | the published `1.57.1` (plugin, broker, workers; pulled from Docker Hub) under this tree's chart | **THE DEFECT: 5 ok, 4 bad.** Both replicas issued (76 lines); 3 refusals `no live publish registration for RoleSessionName …` in the replicas' logs; a `CredentialRefreshFailed` on a tenant carrying that refusal as `broker refused the exchange: 403: AccessDenied`; **13 read errors** across 476 reads — the refusal removed the pod's key, and reads failed until the next republish re-minted it. "Every door moved" still held, because by the leg's end every refused pod had been re-minted: the removal is a window, which is why the refusal count, the event and the read errors are the fingerprints. (Its first BAD, "4 broker replicas Running", was the rig counting the old ReplicaSet's terminating pods — fixed for run 3.) |
| 2 | `s37-run2-regfix.log` | `flint-s3-csi:regfix`, `flint-s3-worker:regfix` (x86_64 musl, built from this tree; lean images are the published 1.57.1 retagged) | **8 ok, 1 bad — the 1 is the same rig miscount.** Both replicas issued (81 lines), zero refusals, zero `CredentialRefreshFailed`, 8/8 doors moved, **480 reads, zero errors**. |
| 3 | `s37-run3-regfix.log` | the same images, the leg counting live replicas only | **9 ok, 0 bad.** Two live replicas, 8/8 tenants, 480 reads, zero errors, 8/8 doors moved, zero refusals, both replicas issued (68 lines), zero events, chart restored. |

The fix: the plugin's exchange carries its registration in the same
request (`creds::BrokerClient::exchange_registered`, a `Registration`
form field under the plugin's own bearer); the broker accepts it under
the node principal and inserts it (`broker::assume`,
`carried_registration`), and a registration it cannot accept is a 503
the plugin keeps its key through. `webIdentity` is refused on a broker
above one replica (`/v1/status` `replicas`).

## Write-grant drill — `lean/e2e/access/write-grant-minio.sh`

MinIO (Chainguard's image) in Docker on the box, a bucket-wide parent
user, `AssumeRole` with the exact `write_session_policy` the broker's
unit test writes out; a flint-sync built from this tree with
`--features "s3 http"`.

| run | log | policy | result |
|---|---|---|---|
| result | `write-grant-run1.log` | `write_session_policy` as built | **17 ok, 0 bad.** A: a writer syncer checked out the empty prefix, published three creates (a 20 MiB file among them: multipart create, parts, complete) and an edit + delete over two more barriers, with no denial in any log; a reader on the parent's keys saw the edit, the delete and the 20 MiB file byte-identical. B: PUT, GET, DELETE and LIST of another prefix all `AccessDenied`; the parent can. C: ListBucket, PutObject, GetObject, DeleteObject and a multipart create/upload/list-parts/abort under the prefix all allowed; a multipart create in another prefix denied. D: GetBucketVersioning and ListMultipartUploads denied; the parent can. |
| control | `write-grant-control.log` | the same with `s3:PutObject` removed (`POLICY_OVERRIDE`) | **11 ok, 8 bad — as it must.** The writer's first barrier died at `store: not authorized: put_whole: 403 AccessDenied` (the conformance probe named it first), the next two likewise, the reader saw nothing, C2 and C3 were refused `PutObject`/`CreateMultipartUpload`; B and D unchanged. So A is the row that catches a policy missing an action the syncer needs — D14's worry, now observable. |

## Unit tests and controls

`cargo test --lib -- s3csi:: passthrough::` on the box: 119/119 (three
new: `the_write_session_policy_writes_the_prefix_and_nothing_else`,
`a_carried_registration_is_parsed_from_the_form_or_refused_by_name`,
`exchange_registered_carries_the_registration_under_the_node_token_in_one_request`;
`sts_attaches_the_read_policy_to_a_read_grant_and_the_write_policy_to_a_read_write_one`
inverted its write assertion). Mutation controls (`mutations.log`), each
failing its test and passing restored: the nonce check in `decide`
disabled; a write grant minted with the READ policy; the `Registration`
field dropped from the plugin's exchange. `cargo check --bins` clean.
