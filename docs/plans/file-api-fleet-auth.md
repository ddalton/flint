# File API auth at fleet scale — one root key, no stored secrets

**Recommendation: derive each share's bearer token from one root key rather than
storing one secret per share, and keep the root key out of your process
entirely by delegating the MAC to a KMS.** The project service then holds an IAM
permission, not a credential store — and "the front door needs knowledge of all
the hubs' secrets" stops being true, because there is nothing to know: a token is
a pure function of the share's identity.

This is a design note for the consumer of the hub's HTTP file API (the "front
door" of `docs/flint-lite-operator.md`). It changes nothing in flint. The
endgame in §8 does, and is deliberately reachable from here without redoing the
caller.

Scope is the **HTTP file API only**. NFS on 2049 is a different boundary with a
different answer (network reachability — see §7).

## 1. The problem

A project service that lets a person browse a project's files talks to that
project's hub over the file API, which authenticates with a per-share bearer
token. At 3000 projects that is 3000 secrets the service must be able to
produce on demand: a store to keep them in, a fan-out to read them, a rotation
story, and a very large thing to lose. The obvious shortcuts are all worse than
they look, in particular granting the service `get secrets` across the workspace
namespaces — see §5.

## 2. What the hub enforces today

| behaviour | where |
|---|---|
| one token per hub, compared with `constant_time_eq` | `pnfs/mds/fileapi/mod.rs:573-588` |
| token resolved from `tokenFile`, else `$FLINT_FILE_API_TOKEN` | `pnfs/config.rs:865-900` |
| no token ⇒ routes are **not mounted** (404, not 401) | `pnfs/config.rs:872`, `fileapi/mod.rs:580` |
| token value re-read every 10s, compared **per request** | `fileapi/token.rs:58`, `fileapi/mod.rs:593` |
| whether the routes exist at all: decided **once**, before the listener binds | `pnfs/mds/server.rs:647` |
| Secret projected read-only at `/etc/flint/api-token/token` | `lite_operator/render.rs:310, 635` |
| routes refuse with 503 until phase is `Serving`/`Sweeping` | `fileapi/mod.rs:441-457` |

Three consequences that shape everything below:

1. **The token is an opaque string.** The hub does no parsing, no format check,
   no expiry — it compares bytes. Any derivation scheme the caller likes works
   without touching the hub.
2. **A rotation is live — this section's original claim is obsolete.** It said
   a rotation needed a pod restart. That was true when this note was written and
   stopped being true in `c72faea`: `TokenSource` re-reads the projected file
   every 10s and the auth filter compares against `current()` per request, so a
   rotation lands on the next request. Nothing rolls the hub for a token and
   nothing should — `checksum/creds` still covering only `credentialsSecretRef`
   is now the correct behaviour rather than the gap §9 called it.
   **The boot-time fact that remains** is a different one, and it is the one that
   matters for provisioning order: with no token at startup the route table is
   never assembled, so `/files*` is 404 and turning the API on later needs a
   restart. Env-sourced tokens also stay boot-time — no file, nothing to
   re-read (`config.rs:914`).
3. **There is exactly one token.** `resolve_token()` returns `Option<String>`,
   so a hub cannot accept a fleet credential *and* a project-scoped one at the
   same time. See §9.

## 3. The reframe

A per-hub secret protects each project from *other callers of the API*. If the
project service is the only caller, there are no other callers, and 3000 tokens
buy no containment against the threat that matters — compromise of the project
service, which can open every project by construction.

So the question is not "how do I store 3000 secrets safely" but "what is the
smallest thing the service must hold to produce any project's token". The answer
is one key.

## 4. The design: derived tokens

```
token(share) = base64url_nopad(
    HMAC-SHA256(root_key,
        "flint-fileapi/v1:" || endpoint || ":" || bucket || ":" || keyPrefix || ":" || version)
)
```

- **`keyPrefix`, not the CR name.** The prefix is immutable once set
  (`lite_operator/crd.rs`), while names are yours to reuse. Include `bucket` and
  `endpoint` because a prefix is only unique within a bucket.
- **`"flint-fileapi/v1:"` is domain separation.** It keeps this key's outputs
  from ever colliding with another use of the same root, and gives the scheme a
  version to change under.
- **`version` is an integer held on the CR** as an annotation the operator does
  not interpret (e.g. `frontdoor.example.com/token-version: "3"`). It is what
  makes single-project revocation possible; see §6.
- Output is 32 bytes → 43 characters. Nothing in the hub cares.

**Who does what.** The project service holds the root and is the only component
that does. At provisioning it computes the token, writes the share's Secret, and
creates the FlintShare. When a person opens a project it recomputes — it never
reads the Secret back. The operator stays out of this entirely: it must not hold
the root, because it would gain the ability to mint every project's token while
having no need to produce any.

**RBAC follows from "write, never read".** Grant the service `create` and
`patch` on Secrets in the workspace namespace and deliberately withhold `get`,
`list` and `watch`. It writes tokens it already knows and can never read one
back — which is what keeps the S3 credentials in those same namespaces out of
its reach. (Confirm your client library does not do a get-before-update; use
server-side apply, which needs only `patch`.)

## 5. Where the root key lives

In preference order. The property to optimise is *not* "encrypted at rest" —
it is **how long a compromise of the project service keeps paying out**.

1. **Do not store it: delegate the MAC.** AWS KMS supports HMAC keys —
   `GenerateMac` with `HMAC_SHA_256` over the derivation message above. The key
   material never enters your process, so a compromised service can mint tokens
   *while it is compromised* and not one second longer; revocation is an IAM
   change rather than a fleet rotation; and every mint lands in CloudTrail,
   which is the closest thing this design has to an audit trail of who opened
   which project. The cost is one API call per token, and you mint on project
   open, not per request — cache the result for the session. At 300 live
   projects this is nothing. **This is the recommendation.**
2. **Vault Transit** (`transit/hmac/<key>`) is the same property on-prem or
   anywhere without KMS: the key never leaves the vault.
3. **A cloud secrets manager** (Secrets Manager, SSM SecureString) read once at
   startup via a workload identity (IRSA), never written to disk. The key is now
   in your process memory — a memory disclosure or a core dump is a fleet-wide
   compromise that outlives the incident.
4. **A Kubernetes Secret in the project service's own namespace**, projected as
   a file (not env, which leaks into `/proc` and crash reporters), readable only
   by that service's ServiceAccount, with encryption-at-rest on. This works
   everywhere and is the weakest of the four. Acceptable as a starting point;
   plan to move to 1 or 2.

**Where it must never live:**

- **Not in any share's namespace.** That is where the tenant S3 credentials are,
  and it is the blast radius this design exists to keep small.
- **Not projected into a hub.** A hub must hold *only its own* derived token. A
  hub that could recompute another project's token would turn a single-project
  compromise into a fleet one.
- **Not in the operator.** It has no need to produce tokens (§4) and already
  carries a fleet-wide blast radius of its own.
- **Not in a browser, an agent pod, or anything a tenant workload can read.**
  End users get bytes proxied by the project service; they never see a token.

## 6. Lifecycle

| operation | what the project service does | cost |
|---|---|---|
| provision | mint, `create` the Secret, create the FlintShare with `token-version: 1` | 2 API calls |
| open a project | recompute the token, call the API | 0 reads, 1 KMS call (cacheable) |
| revoke one project | bump `token-version`, rewrite that Secret, bounce that hub | 1 project |
| rotate the fleet | new root/KMS key, rewrite every Secret, bounce the **live** hubs | see below |
| a share wakes | hub reads the projected file at boot — picks up whatever is current | free |

**Fleet rotation is cheaper than it sounds, because of the idle ladder.** A
suspended or hibernated share reads its token at its next boot, so rewriting its
Secret is enough — no bounce, no wake. Only the live set pays a restart. At the
planned fleet shape (3000 shares, 300 live) that is 3000 cheap writes and 300
brief `Recreate` bounces, not 3000 restarts.

**The bounce is an availability event, and it should not be one.** Restarting a
hub stalls every mounted client on that share. With a `hard` mount — which the
consumer recipes mandate — in-flight I/O blocks in uninterruptible sleep until
the new pod answers: the pod's termination grace (120s,
`lite_operator/render.rs:105`), then the NFS grace period (60s,
`pnfs/config.rs:962`) while clients reclaim. Nothing is lost and nothing must be
remounted, because the `state.db` on the PVC keeps the `serverId` stable — it is
a stall, not a wedge. But it means rotating an HTTP credential costs an NFS
outage on the same share, coupling the two doors in exactly the way the rest of
the design keeps them apart. §9 removes it. Until then: rotate in a window where
a hub restart is acceptable, and **never force-delete a consumer that is blocked
on the mount** — that turns a stall into a pod stuck in `Terminating` that pins
the volume, and `umount` during the outage blocks the same way.

**Handle `401` with one retry at the previous version.** During a rotation a hub
may still be running the old value. Recompute with `version - 1` (or the
previous root) and retry once; if that succeeds, the hub is stale and wants a
bounce. Without this rule a rotation is a visible outage for every project a
user opens mid-roll — and the rule itself exists only to paper over the restart
requirement, so §9 retires it.

**Never log the token.** It rides an `Authorization` header, not a query
parameter, so it stays out of access logs by default — keep it that way when you
add request tracing.

## 7. What this does not solve

- **No per-end-user identity.** The hub sees one caller and one credential. Who
  looked at which file is a question only the project service can answer, and it
  must answer it in its own audit log. If that log is the compliance story,
  write it before the API call, not after.
- **No expiry.** A derived token is valid until its version changes. §8 is what
  fixes this.
- **A compromised service mints everything while it is compromised.** Option 1
  in §5 bounds that to the window rather than forever; nothing here bounds it to
  a subset of projects, because the service legitimately reaches all of them.
- **A compromised hub still holds its own project's token** — and can read its
  own project, which it can do anyway, being the filesystem.
- **NFS is untouched.** Port 2049 is AUTH_SYS: the client asserts its own
  uid/gid and the server takes it (`nfs/server_v4.rs:652`). Reachability is the
  boundary there — `networkPolicy` in both charts, off by default, with
  `nfsClientCIDRs`. Nothing in this note changes that, and a token on the HTTP
  door does not compensate for an open 2049.

## 8. The endgame: audience-scoped ServiceAccount tokens

Replace the shared secret with an identity the hub can verify without holding
anything:

- The project service requests a token for its own ServiceAccount via the
  Kubernetes TokenRequest API with `audience: flint:<project-id>` and a few
  minutes of `expirationSeconds`.
- The hub validates the JWT offline against the cluster's OIDC JWKS — signature,
  issuer, expiry, `aud` equal to **its own** project id, `sub` on an allowlist of
  ServiceAccounts.

What it buys over §4: no Secret per share and nothing to rotate; tokens that
expire in minutes; and a token minted for one project that is *rejected by every
other hub* — real per-project scoping rather than a naming convention. It works
cross-cluster by configuring the issuer and JWKS URL.

**Shape of the change.** Auth material is resolved in exactly one place
(`FileApiConfig::resolve_token`, `pnfs/config.rs:865`) and checked in exactly one
place (`auth_filter`, `pnfs/mds/fileapi/mod.rs:573`). Keep `tokenFile` for the
single-hub case and add beside it:

```yaml
monitoring:
  fileApi:
    oidc:
      issuer: https://oidc.eks.eu-west-1.amazonaws.com/id/…
      jwksUrl: …                    # fetched once at boot, refreshed on kid miss
      audience: flint:tenant-a      # rendered by the operator from the CR
      subjects: [system:serviceaccount:frontdoor:projects]
```

The hub already carries `reqwest` with rustls for the fetch and `hmac`/`sha2`;
it does **not** carry a JWT crate today, so that is a new dependency
(`jsonwebtoken`). The operator change is one rendered block plus the CRD fields.

**§4 is forward-compatible with this.** Both are "produce a credential for
project X at call time" behind one function in the project service. Moving from
HMAC to TokenRequest changes that function's body and nothing above it.

## 9. Open items

- **~~Re-read the token instead of restarting the hub.~~ SHIPPED in `c72faea`.**
  `TokenSource` holds the value behind an `RwLock`, re-reads `tokenFile` on a
  10s interval (`fileapi/token.rs:58`) and the auth filter takes `current()` per
  request (`fileapi/mod.rs:593`). A rotation costs nothing: no bounce, no
  stalled mounts, no fleet split. What remains open below was the rest of the
  original item, and the paragraph it replaced read:
  the value is resolved once at boot
  (`pnfs/mds/server.rs:647`) and captured into the auth filter
  (`pnfs/mds/fileapi/mod.rs:468, 573`), so a rotation reaches a running hub only
  via a restart — and a restart stalls every mounted client (§6). Resolve it
  from the projected file behind a short TTL instead, so a rotation costs
  nothing: no bounce, no stalled mounts, no fleet split between hubs that
  happened to restart and hubs that did not.

  Four constraints it has to respect. Keep "no token at boot ⇒ routes are not
  mounted" exactly as it is — that is a provisioning decision, not a runtime one
  — and re-read only once the routes exist. If the file later empties or cannot
  be read, hold the last good value and log loudly; never fall back to
  unauthenticated, and never tear the routes down on a transient read error.
  **The mount must stay a whole-directory projection** — `render.rs:632` and the
  chart both mount `/etc/flint/api-token` with no `subPath`, and a `subPath`
  mount is frozen at pod start, which would silently return this to boot-time
  behaviour. And propagation is bounded by the kubelet's own sync of mounted
  Secrets (~1 minute, more with the API cache), so "live" means a minute or two,
  not instant — which is why the next item matters.

- **Multiple accepted tokens per hub.** `resolve_token()` returns
  `Option<String>`; a set would let a fleet credential and a project-scoped one
  coexist — the case per-hub secrets were reaching for and cannot serve today.
  With live re-read it also makes rotation overlap-safe: add the new token,
  migrate callers, drop the old one, with no window where either is rejected.
  That is what retires the `401`-retry rule in §6.

- **Considered and rejected: hashing the token Secret into `checksum/creds`.**
  It works — fold `tokenSecretRef` into `lite_operator/reconcile.rs:844` and a
  rotation becomes a deliberate rollout instead of a silent no-op. It was the
  first proposal here, and it is the wrong trade: it makes changing an HTTP
  credential cost an NFS availability event on that share, for every mounted
  client, and at fleet rotation it bounces every live hub. Live re-read gets the
  same correctness with none of that. Recorded so it is not re-proposed.
- **Who writes the Secret.** This note says the project service, to keep the
  root in one component. If the operator ever needs to mint (e.g. it starts
  creating shares on its own), revisit — but do not split the root across two
  components.
- **Watch the derivation message if `spec.bucket` is ever absent.** A share with
  no bucket has no `endpoint`/`bucket`/`keyPrefix`; use the namespace and CR name
  with a distinct domain-separation prefix, and accept that such a share's token
  does not survive a rename.
