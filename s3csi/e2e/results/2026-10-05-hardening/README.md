# 2026-10-05 — the hardening round: AWS's own verdict on the write policy; the operator's allowlists; the table TTL; four smaller bounds

What this directory records, and the order it was built in. Everything
here ran on the Linux box (`ddalton@10.0.0.249`) from the tree that
became this commit; the AWS half ran against a bucket and identities
made for the day and torn down after (`s3csi/e2e/aws-access-iam.sh`).

## 1. The write policy against AWS STS and S3 (`aws-write-grant-*.log`)

`s3csi/e2e/aws-write-grant.sh` is `lean/e2e/access/write-grant-minio.sh`
with AWS as the evaluator: AssumeRole on a role that is `s3:*` on the
whole bucket, WITH the exact `write_session_policy` the broker attaches to
a read-write exchange on the `sts` backend (written out by the broker's
unit test for `flint-access-drill-20261005/team/proj1`), then the same
four sections — a writer syncer whole on the narrowed keys, another
prefix out of reach, the mount-s3 action set in and out of the prefix,
the operator's bucket verbs denied — each denial with its control on the
unnarrowed keys.

| run | result | what it says |
|---|---|---|
| `aws-write-grant-run1.log` | **22/0** | STS accepted the policy (`PackedPolicySize` 25 % of the cap); the syncer checked out, published three files (one 20 MiB, so a multipart upload), edited and deleted, and was denied nothing; a reader on the unnarrowed keys saw every write, the 20 MiB byte-identical; PUT/GET/DELETE/LIST of another prefix, LIST of the whole bucket, HeadBucket and a multipart upload elsewhere were all `AccessDenied`; Put/Head/Get/Delete, a multipart upload (create, part, list-parts, abort) and the versioned verbs under the prefix were allowed; GetBucketVersioning, ListMultipartUploads and GetLifecycleConfiguration were denied, and the unnarrowed keys did all three |
| `aws-write-grant-control.log` | **16/8** | the same drill with `s3:PutObject` removed from the policy (`POLICY_OVERRIDE`): the writer failed at its first PUT — AWS: *"not authorized to perform: s3:PutObject … because no session policy allows the s3:PutObject action"* — and every write after it; the reader saw nothing; the denials and their controls still passed. This is the positive control: the drill fails when the policy is weak, and fails at the writer, where D14's worry said it would |
| `aws-write-grant-attempt1.log` | 19/3 | the first run, kept for the record: A2's first PUT came back "dispatch failure" from the SDK (a transport failure; barriers 2 and 3 landed and the reader saw every write, so not the policy's); C1b asserted HeadBucket must be ALLOWED — wrong: HeadBucket carries no prefix, the prefix-conditioned ListBucket statement does not reach it, and mount-s3 does not need it (it is given `--region` and probes with ListObjectsV2 under its `--prefix`, which is how the read grant mounted on real nodes under the same condition in EC2 campaign 3); C5 looked for `team/proj1/README.md`, a key the syncer never writes (its objects are `files/<path>@<uuid>`). The two checks were corrected and the drill re-run whole |

The drill also recorded what AWS says about the policy's size: 25 % of
the packed cap for a 27-character bucket and a 10-character prefix, so
a long bucket and prefix have room.

**Still not run:** Ceph RGW (SECURITY.md §5 row 14).

## 2. Unit tests, mutation controls (`box-test.log`, `mutations.log`)

`unit-tests.log`: `cargo test --lib -- s3csi:: passthrough::` on the
box — **127 passed, 0 failed** (the new ones: the registration TTL, the
JWT shape check, the in-flight review bound, the state file's mode and
its stale tmp, the identity-mode list and its refusal text, the endpoint
host matcher, the webIdentity re-register period under half the TTL
floor, the plugin's `Launch` Debug); `crates/flint-s3-worker` **6 passed**
(the worker's `Launch` Debug). `cargo check --bins` clean (the one
warning, an unused `event` in `flint_lean_operator.rs`, predates this);
clippy's remarks on the touched files are the pre-existing
`io::Error::new(Other)` ones in `fuse.rs`.

`mutations.log`: six positive controls, each flipping the line a new test
pins, each test FAILING on the mutated build (the `Compiling` line is in
the log — the test ran the mutated binary, not a cached one) and passing
again restored:

| mutation | test that caught it |
|---|---|
| `identity_mode_allowed` admits every mode | `identity_modes_default_leaves_ambient_out_and_a_refusal_names_the_knob` |
| `endpoint_allowed` admits every host | `endpoint_allow_matches_hosts_and_suffixes_and_empty_admits_any` |
| `live_registration` ignores the TTL | `a_registration_past_the_ttl_is_not_live_and_a_renewal_restarts_its_clock` |
| `jwt_shaped` takes any token | `a_token_that_is_not_a_jwt_is_refused_without_a_review` |
| `write_private` skips the 0700 on the dir | `state_is_born_0600_in_a_0700_dir_and_a_stale_tmp_does_not_widen_it` |
| `review` takes no permit | `in_flight_reviews_are_bounded_and_the_overflow_is_an_outage_not_a_refusal` |

## 3. Kind: S38 (the allowlists) with S1, S6 and S37 beside it (`kind-legs.log`)

kind on the box (`kind-flint-s3csi`, one node, images built from this
tree as `:hard`, MinIO rig, `CREDS_LIFETIME=120`).

| log | legs | result |
|---|---|---|
| `kind-legs-run1-s1-s6.log` | S1, S6 | **11/0** (the plugin with the new env registers and is Ready; consumers refusals unchanged). This run's S38 used `pt-ambient`, a CR only `aws-passthrough.sh` applies, and was answered NotFound — a rig error, not a finding; it was stopped there, the leg given its own ambient CR, and re-run |
| `kind-legs-run2-s38-s37.log` | S38, S37 | **17/0** |

S38: with the chart's default `node.identityModes` and
`endpointAllow=*.amazonaws.com`, the ambient CR's pod was refused naming
the mode, the knob and the list; the MinIO-endpoint CR's pod was refused
naming the host and the knob; no worker existed for either. Restored
(ambient listed, no endpoint list): the same endpoint CR mounted and read,
and the ambient CR got past the allowlist (its mounter then had no ambient
chain on kind, which is the platform's, not the allowlist's).

S37 re-run on this tree: 480 reads, zero errors, every door's expiration
moved, zero "no live publish registration", both replicas issued (65
issued lines), zero CredentialRefreshFailed — the TTL and the review
bound changed nothing for a refreshing tenant.

An earlier attempt built nothing: the box's snap docker reads a build
context only under `$HOME`, and the tree was on `/mnt/nvme2`.

## Cost

STS and IAM are free; the bucket held ~20 MiB for minutes. No cluster
was provisioned: kind on the box, MinIO-in-Docker nowhere this time.
The AWS fixtures were deleted with `aws-access-iam.sh down`, which
verifies the zero set by name — see the end of this file.

```
$ WORK=… PROFILE=trove-admin bash s3csi/e2e/aws-access-iam.sh down
purged 114 versions
bucket flint-access-drill-20261005 deleted
user flint-acc-rw-20261005 deleted
user flint-acc-ro-20261005 deleted
user flint-acc-sts-20261005 deleted
role flint-acc-role-20261005 deleted
zero set: bucket, 3 users, role all gone
```

The three key JSONs were deleted on the box and on the Mac after it.
