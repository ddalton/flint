# flint forge — a Knox-minted JWT as the door's principal: design

Written 2026-09-08. **Design only — no code.** Status: proposal.

Supersedes the scope of `docs/plans/forge-oidc-authority-design.md`,
which was written against a different premise (an Auth service that
mints, groups carried as claims, revocation in scope). That document is
kept for its analysis of the deferred parts; where the two disagree,
this one is current. §0.2 lists what changed and why.

Read alongside `docs/plans/forge-file-api-design.md` (§4.4a authorship,
§4.5 branch policy, §7 the door),
`docs/architecture/forge/flint-forge-architecture.md` (the door's five
steps in front of two route tables, and the limits it records about
ServiceAccount-shaped principals).

---

## 0. What this is, and what it is not

### 0.1 The one sentence

The door accepts a JWT that **Knox minted with `doAs`**, verifies it
offline against Knox's JWKS, and uses its `sub` — an actual person — as
the principal for `spec.consumers`, for `policy.judge`, and as the
commit author.

**What it is not:**

- **Not an OIDC flow.** The door is a resource server. It never
  redirects, holds no client secret, keeps no session, and performs no
  code exchange. Whatever dance the application does with the Auth
  service and Knox is the application's business.
- **Not group-based authorization.** Deferred (§0.2). The roles and
  groups Knox resolves arrive as an unsigned HTTP header, and nothing in
  this design reads them.
- **Not revocation.** Deferred. `exp` is the only thing that ends a
  session — see §7.2, which is the one place that decision has teeth.
- **Not a replacement for TokenReview.** Pods keep using their projected
  ServiceAccount tokens. Both credential kinds are served at once, which
  is where the sharpest risk lives (§3).

### 0.2 What changed from the superseded document

| it assumed | actually |
|---|---|
| the Auth service **mints** tokens | it **relays**; Knox mints |
| ⇒ online introspection against Auth | ⇒ **offline JWKS** against Knox |
| `client_credentials` ⇒ no end user | **`doAs` puts the user in `sub`** |
| groups as a signed claim, in `principals:` | groups are an **unsigned header** — deferred entirely |
| a stable client id to bind a relay | the client id **is the Knox token id**, which rotates |
| a new `principals:` CRD field | **no CRD change** — a prefixed entry in the existing array |

Two whole problem classes left with the groups: the relay binding, and
the rotating-token-id difficulty that made that binding unbuildable.
**Everything this design trusts is signed.** That is the property to
protect if the scope ever grows back.

---

## 1. The flow, with the trust marked

```
 app ──(user id, header)──▶ Auth service ──(doAs)──▶ Knox
                                 ▲                     │
                                 └──── JWT{sub:alice} ◀─┘
                                 │
                    Authorization: Bearer <JWT>
                         SIGNED — verifiable
                                 │
                                 ▼
                              the door
                    verify ▸ principal ▸ authorize ▸ wake ▸ route
                                 │
                        X-Remote-User: alice
                       (door-set, never forwarded)
                                 │
                                 ▼
                 syncer ▸ policy.judge ▸ commit author
```

Roles and groups also arrive from the Auth service in a header. **This
design ignores them.** They are not read, not forwarded, and not added
to any allowlist — see §6, which exists to keep that true.

---

## 2. Where it fits in the existing door

The seam already exists and it is a trait:

```rust
#[async_trait::async_trait]
pub trait Reviewer: Send + Sync {
    async fn review(&self, token: &str) -> Result<Identity, String>;
}
```

Token in, identity out. `KnoxReviewer` is a third implementation beside
`KubeReviewer` and `CachingReviewer`, behind the same call site, in
front of the same `consumer_allows`. Both forge doors share one reviewer
(`RepoFileDoor::beside`), so the git door and the file door acquire this
together and cannot diverge on identity.

**D1. Offline verification, no verdict cache.** `CachingReviewer` exists
to save apiserver round trips; a signature check is microseconds and has
no round trip. Caching a Knox verdict would only delay `exp` by up to
the TTL, which — with revocation out of scope — is the one bound that
matters. `KnoxReviewer` is therefore **not** wrapped in
`CachingReviewer`. The JWKS *keys* are cached; the *verdicts* are not.

---

## 3. The discriminator — the sharpest risk in the design

Both credentials are RS256 JWTs: the pod's projected SA token and the
Knox token. The door must choose a verifier from the **unverified**
`iss` before it can verify anything.

The two directions fail differently, and the quiet one is worse:

- An issuer JWT sent to `TokenReview` is merely refused — confusing, harmless.
- **A pod token sent to the offline verifier is silent.** It would be
  checked against the wrong keys and refused today; but any future
  loosening turns it into a bypass, and it costs the 60 s online
  revocation window that makes a deleted pod's credential die.

**D2. Exact `iss` compare, default to TokenReview, no fallback.** A
token whose `iss` is not the configured Knox issuer goes to
`TokenReview`, which is the conservative direction: it fails closed
against an apiserver rather than against a cached key set. No "try one,
then the other" — a fallback is how both verifiers eventually accept
everything.

**D3. Refuse to start if the issuers are equal.** If the cluster's
ServiceAccount issuer and the configured Knox issuer are the same
string, routing is undefined and the door must not run. Checked once at
start-up, not per request.

**D4. The oracle for the router is a per-verifier COUNTER, never a
status code.** A broken router still refuses — both verifiers reject a
token meant for the other — so a test that asserts 401 passes whether
the routing works or not. Each verifier carries an `AtomicU64`, and the
tests assert which one was *reached*. This is the single most
falsifiable-looking, least falsifying test in the design if written the
obvious way.

---

## 4. Verification

**D5. What is checked, in order:** signature against the JWKS key named
by `kid` · `iss` exact · `aud` contains the configured audience · `exp`
· `nbf` · clock skew ≤ 60 s.

**D6. `aud` is supported and OPTIONAL, and its absence is loud.**

`--jwt-audience <value>`: when set, a token whose `aud` does not contain
it is refused. When unset, the check is skipped and the door logs a
warning at start-up naming the consequence, so the posture is visible in
a pod's logs rather than inferred from a missing flag.

Optional rather than mandatory because of what Knox can actually do
here: **it does not mint a per-application audience.** It can set one,
but only as a **static, topology-wide** value, and this deployment
currently leaves it unset — tokens are scoped by **issuer + TTL + `sub`
and nothing else**. Whether `aud` is populated at all is an application
and Knox-configuration decision, not forge's, so the door supports it
and enforces it when told to.

**What that costs, stated plainly, because the design cannot fix it.**
A topology-wide audience does not separate forge from its siblings. Any
service behind the same Knox topology that receives a user's token can
replay it against forge **as that user** — a compromised, careless or
merely log-happy sibling becomes a forge credential for every user it
has seen. The exposure is bounded by `spec.consumers` (the `sub` must
still be listed), so it is *a forge user's token stolen from a sibling*
rather than *anyone with a Knox token*; and with revocation deferred
(§7.2) there is no second line.

**The clean fix is a deployment decision, not a code one:** a dedicated
Knox topology for forge gives forge its own audience value, and D6 then
does what audiences are for. Worth establishing whether that is
practical before accepting the weaker posture.

**D6a. `spec.consumers` now carries weight it was not designed for.**
With `aud` unable to isolate, it is the ONLY forge-specific check in the
chain. `serviceAccounts: ["*"]` stops meaning "any pod we trust" and
starts meaning "any user of any service in this Knox topology". The
operator should warn on a wildcard in any repository that also names a
`jwt:user:` principal.

**D7. A lifetime ceiling, enforced door-side.** A token whose
`exp - iat` exceeds `--jwt-max-lifetime` (default 1 h) is refused
however valid its signature. Knox's shipped `knox.token.ttl` default is
120 days; the mitigation is that TTL is set per token at mint, so the
Auth service's refresh loop can request short ones. The ceiling is the
door refusing to be the place that assumption is unchecked.

**D8. JWKS: fetched, cached, refreshed on an unknown `kid`,** with a
floor between refetches so an unknown-kid storm cannot become a
denial-of-service against Knox. A static PEM is also accepted, for a
deployment that would rather not have the door reach Knox at all.

**D9. A JWKS transport failure is a 503, never a 401.** "I could not
reach the key server" and "your credential is bad" are different
answers, and returning the second for the first sends the caller to
rotate a credential that was fine.

---

## 5. The principal, and authorization

**D10. `sub` is the principal.** It reaches `X-Remote-User`, and
therefore `policy.judge` and the commit author. Branch patterns become
per-person — `agent/alice/*` bounds Alice's branches, which is the
limitation the architecture document currently records as unfixable.

**D11. `sub` must be a stable identifier.** Branch policy and commit
history key on it permanently. A display name that can be reassigned
would silently transfer both. Stated as a requirement on the issuer,
because forge cannot detect a violation.

**D12. `Identity` stops being ServiceAccount-shaped.** Today:

```rust
pub struct Identity {
    pub username: String,
    pub namespace: String,        // a person has none
    pub service_account: String,  // nor this
    pub pod_uid: Option<String>,
    pub pod_name: Option<String>,
}
```

`namespace` and `service_account` become `Option<String>`, or the type
becomes an enum. Optionals are the smaller change and keep
`consumer_allows`'s three arms intact; the enum is the honest one. **Take
the optionals**, and make `consumer_allows`'s ServiceAccount arm require
both to be present, so a person can never match a bare ServiceAccount
name by having the right `username`.

**D13. No CRD change.** `consumer_allows` already matches a bare
`username`, so a **prefixed entry in the existing array** is enough:

```yaml
spec:
  consumers:
    serviceAccounts:
      - agent-runner                     # unchanged: a ServiceAccount
      - jwt:user:alice@example.com       # new: a person, via Knox
```

One prefix, one matcher arm, no schema edit, no pruning risk, no blast
radius to lean, passthrough, the broker or the CSI node plugin — all of
which deserialize the same `Consumers` type. It is also exactly the
syntax the deferred groups design would extend, so Phase 2 is additive
rather than a migration.

**D14. An unprefixed entry keeps meaning a ServiceAccount, and `*` keeps
meaning both.** Backward compatibility is not a courtesy here: every
`FlintRepo` in the fleet has one of these lists, and a changed meaning
would re-authorize them all at once.

---

## 6. The headers the door does not read

The door builds its upstream request from a strict allowlist
(`GIT_REQUEST_HEADERS`) and sets `X-Remote-User` itself from the
verified identity, so a caller cannot smuggle one. F13 measured this on
the wire: a forged `X-Remote-User` naming `kube-system:admin` was
overridden and the commit was authored as the caller's real identity.

**D15. `X-Roles` and `X-Groups` are neither read nor forwarded, and must
never be added to the allowlist.** Adding one line there to "make groups
work" would let every caller set their own groups. It would look like
plumbing in review. A test asserts the allowlist does not contain them,
so the one-line version of this mistake fails CI rather than a drill.

---

## 7. What forge trusts and cannot verify

### 7.1 Two upstream preconditions

`doAs` is impersonation, so the door's verification proves *"Knox
asserts this is Alice"*, and Knox's assertion rests on the Auth service,
whose assertion rests on a header from the application. Both links are
outside flint and their absence is **invisible from forge's side** —
nothing is broken here when they fail.

1. **Knox's proxy-user allowlist** must restrict which identities the
   Auth service may `doAs`, and from which hosts. Without it, the Auth
   service is effectively a private key for every user.
2. **The app → Auth service hop must be authenticated**, and the
   application must only assert users it authenticated itself.

Written as preconditions, not assumptions. A deployment that cannot
state both should not key branch policy on people.

### 7.2 `exp` is the only thing that ends a session

Revocation is out of scope, so a deprovisioned user's token stays valid
until it expires — and it is now a *person's* rights, not a shared
robot's. D7's ceiling bounds the damage; the Auth service's refresh loop
is what makes short lifetimes practical. This is the sole place the
deferral has teeth, and it should be revisited before the door faces
anything but a pilot.

### 7.3 The last hop is unchanged

The syncer takes `X-Remote-User` on trust behind the NetworkPolicy the
operator renders — only when `door.namespace` is set, and only where the
CNI enforces it. This design does not change that joint, but it does
raise what forging the header is worth: `jwt:user:alice@example.com`
buys exactly Alice's branch rights where a ServiceAccount bought coarse
shared ones. Not a blocker; a reason the operator guard that marks a
repository `Degraded` when it names people with no policy rendered
(~20 lines) is worth more than it was.

---

## 8. Questions for the deployment

One can block; the rest shape configuration.

1. ~~Can Knox mint a forge-specific `aud`?~~ **ANSWERED: no.** It can
   set one, but only as a static topology-wide value, and it is
   currently unused. Tokens are scoped by issuer + TTL + `sub`. This no
   longer blocks — D6 supports `aud` optionally — but it moves the
   isolation forge does not get onto `spec.consumers` and the network
   (D6a). **The follow-up worth asking: can forge have its own Knox
   topology?** That, and not a code change, is what would restore it.
2. **Can the Auth service request short-lived access tokens** and
   refresh, so D7's ceiling is satisfiable? More load-bearing now that
   `aud` cannot isolate: TTL is one of the three things scoping a token.
3. **What is in `sub`** — an email, a directory uid, a display name? D11
   requires stability.
4. **What is Knox's `iss`,** exactly, and is it distinct from the
   cluster's ServiceAccount issuer? (D3 refuses to start otherwise.)
5. **Is the JWKS reachable** from every cluster that runs a door, or
   should this deployment take the static-PEM path (D8)?

---

## 9. Falsifiers

Each is an assertion plus a control that must fail when the mechanism is
removed. A test without its control is not on this list.

| # | asserts | control |
|---|---|---|
| F1 | the router reaches the Knox verifier for a Knox `iss` | **counter**, not status: force the router to always pick TokenReview; F1 fails on the counter while every status stays identical |
| F2 | a pod token is never accepted by the Knox verifier | delete the `iss` check ⇒ F2 fails |
| F3 | the door refuses to start when the two issuers are equal | remove the start-up guard ⇒ F3 fails |
| F4 | with `--jwt-audience` set, a token for another audience is refused | delete the `aud` check ⇒ F4 fails; control: the same token WITH the right `aud` is accepted |
| F4a | with `--jwt-audience` UNSET, a token of any audience is accepted AND the start-up warning was emitted | drop the warning ⇒ F4a fails. The check is the warning, not the acceptance: silently skipping is the failure mode |
| F5 | an expired token is refused, and is not accepted a second time from any cache | wrap `KnoxReviewer` in `CachingReviewer` ⇒ F5 fails |
| F6 | a token signed by an unknown key is refused | accept-any-key ⇒ F6 fails |
| F7 | `sub` becomes the commit author; a caller-supplied `X-Remote-User` is overridden | already green on the wire (F13 P3); mutation: stop setting the header ⇒ fails |
| F8 | a JWKS transport failure answers 503 and the door recovers on the next request | restore the `starts_with("TokenReview:")` heuristic ⇒ the failure is cached and F8 fails |
| F9 | `jwt:user:alice` matches the person and NOT a ServiceAccount literally named `jwt:user:alice` | drop the prefix arm ⇒ F9 fails |
| F10 | an unprefixed entry still matches only a ServiceAccount | make the prefix optional ⇒ F10 fails |

**F8 names a defect that exists today.** `CachingReviewer` decides what
is safe to cache with `Err(e) => !e.starts_with("TokenReview:")`. A
second reviewer's transport failure does not match that prefix and would
therefore be **cached** — the door would stay shut for the whole TTL
after Knox came back. Latent while there is one reviewer; live the
moment there are two. Fix it with typed errors before, not after.

---

## 10. Scope and cost

| # | change | prod | test |
|---|---|---|---|
| 1 | `KnoxReviewer` — third `Reviewer` impl | 250 | 350 |
| 2 | JWKS fetch, `kid` rotation, key cache, static-PEM path | 200 | 250 |
| 3 | `iss` router + start-up guard + counters | 80 | 200 |
| 4 | `Identity` optionals + every construction site | 150 | 150 |
| 5 | `consumer_allows` prefix arm | 40 | 120 |
| 6 | typed reviewer errors (the F8 fix) | 40 | 80 |
| 7 | flags, chart values, docs | 80 | 60 |
| | | **~840** | **~1,210** |

`jsonwebtoken` is the candidate crate; **its availability under the
musl/zigbuild release recipe is unverified** and is the first thing to
check, because the whole choice rests on `ring` cross-building. If it
does not, `josekit` or a hand-rolled RS256 verify over an existing
dependency are the alternatives.

No CRD change. No chart-breaking change. Off unless `--jwt-issuer` is
set, so an install that does not configure it behaves exactly as today.

---

## 11. Decisions

| id | decision |
|---|---|
| D1 | offline verification; JWKS keys cached, verdicts never |
| D2 | route on unverified `iss`, exact compare, default TokenReview, no fallback |
| D3 | refuse to start when the two issuers collide |
| D4 | the router's oracle is a per-verifier counter, not a status code |
| D5 | signature · `iss` · `aud` · `exp` · `nbf` · skew ≤ 60 s |
| D6 | `aud` supported and optional; unset is loud, not silent |
| D6a | `consumers` is the only forge-specific check left — warn on `*` beside a `jwt:user:` entry |
| D7 | door-side lifetime ceiling, default 1 h |
| D8 | JWKS with kid-refresh and a refetch floor; static PEM accepted |
| D9 | key-server failure is 503, never 401 |
| D10 | `sub` is the principal, and the commit author |
| D11 | `sub` must be stable (a requirement on the issuer) |
| D12 | `Identity` gains optionals; the SA arm requires both present |
| D13 | no CRD change — `jwt:user:` prefix in the existing array |
| D14 | unprefixed keeps meaning a ServiceAccount; `*` keeps meaning both |
| D15 | `X-Roles`/`X-Groups` never read, never forwarded, never allowlisted |

**Deferred, with the reason:** groups (unsigned in transit, and the
rotating token id makes the relay binding unbuildable), revocation (a
Knox runtime dependency in every cluster), a signed door→syncer
assertion (the NetworkPolicy is the existing joint; revisit on a
multi-tenant cluster whose CNI does not enforce).
