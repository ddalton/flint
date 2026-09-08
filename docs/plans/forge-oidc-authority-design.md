# flint forge — a verified JWT identity as the authorization principal: design

Written 2026-09-08. **Design only — no code.** Status: proposal.

> **Framing, and the filename.** This file is `forge-oidc-authority-design.md`
> because that is where the question was asked. The design is **JWT bearer
> identity**: the door verifies a JWT, maps a claim to a principal, and
> authorizes it. There is no OIDC flow and no service-mesh integration
> (§0.2).

Read alongside `docs/plans/forge-file-api-design.md` (§4.4a authorship,
§4.5 branch policy, §7 the door), `docs/plans/flint-forge-design.md` §6
(the door and the `X-Remote-User` boundary),
`docs/architecture/forge/flint-forge-architecture.md` (the door's five
steps, and the honest limits it already records about ServiceAccount-
shaped principals), `docs/plans/file-api-fleet-auth.md` §5 and §8 (where
per-user identity was already said to belong, and the no-stored-secrets
posture), and `docs/plans/csi-node-mount-design.md` §4.1 plus the K-table
in §9 — which already contains a *verified* Apache Knox JWT recon and is
why §10 asks questions instead of assuming answers.

---

## 0. The question

### 0.1 As originally asked, verbatim

> "OIDC for authority. The OIDC identity becomes the principal:
> spec.consumers grows a users/groups form, and policy.judge's branch
> rules key on people rather than ServiceAccounts. That's a CRD change, a
> policy-model change, and the git door needs an answer too — git clients
> can't do an OIDC dance, so the two doors would authenticate
> differently. Defensible (the git door serves pods; the file door serves
> people) but only if authority stays keyed on one list."

### 0.2 The scope, corrected twice — and what each correction deleted

> "for the OIDC stuff, more interested in the JWT support. So the AuthN
> flow with JWT should be fine, such as with Knox."

**The door is a resource server, and only that.** No authorization-code
exchange, no client secret, no cookie, no session, no redirect, no login.
The application does whatever dance it does with Knox; that is the
application's problem and always was. The door's job is four steps: take
a JWT, verify it, map a claim to a principal, feed that principal into
**the same** `consumer_allows` decision it already makes for
ServiceAccounts.

*What that deleted:* no redirect endpoint, no callback route, no
state/nonce store, no PKCE, no client registration, no client secret to
mount or rotate, no session cookie and therefore no CSRF surface, no
logout, no refresh-token handling, and no dependence on the issuer's
*authorization* endpoint at all. An earlier draft carried a whole section
enumerating that not-built surface; it is gone.

> "you don't need to integrate with istio. Just support JWT"

**The door verifies the JWT itself.** An intermediate draft carried a
second deployment shape in which a mesh-integrated auth component
verified the signature and the door consumed claim headers. That shape —
its configuration, its manifests, its "trust these headers" mode, its
mesh questions and three of its falsifiers — is **deleted**. One
mechanism, in one place, in one process.

*Worth one line and no more:* if some proxy in front of the door also
validates the token, forge neither knows nor cares. The door still
verifies what it is handed, so a fronting validator is a defence in
depth rather than a component of this design, and nothing here depends on
one existing.

**"OIDC" survives in exactly one place:** an issuer's discovery document
is a *convenient way to locate a JWKS URL* (§6.2). It is optional, it is
resolved once, and nothing else reads it.

### 0.3 The answer, in six lines

1. **The two doors do not have to authenticate differently.** Both
   already take an *opaque bearer* and both discard the username half
   (`git.rs:503-511`, `repo_files.rs:188-195`). A JWT presented as the
   HTTP Basic password by a git credential helper is accepted by the git
   door with **zero git-protocol change**. The premise dissolves.
2. **`spec.consumers` grows one field, not a parallel model:**
   `principals: [jwt:user:…, jwt:group:…]` — prefixed entries in one
   array, evaluated by the *same* function at the *same* step of the
   *same* five-step order in both doors.
3. **`serviceAccounts` is untouched and no existing entry changes
   meaning** — `*` keeps meaning "any ServiceAccount", never "any human".
   The people wildcard is a different string in a different array.
4. **The CRD change is forge-only.** `s3csi::policy::Consumers` is shared
   with lean, passthrough, the broker and the CSI node plugin; this
   design does not touch it. Those four change by **zero lines**.
5. **`policy.judge` goes from `principal: &str` to `principals:
   &[String]`** — the caller's name plus their groups. Every existing
   rule is the singleton case and is byte-identical.
6. **The weakest link is not the signature check — it is the last hop**
   (§9.4). The door asserts the principal to the syncer as a plain
   `X-Remote-User` and presents no credential of its own, so the boundary
   is the NetworkPolicy the operator renders — which is **opt-in**
   (`door.namespace`) and enforced only where the CNI enforces it. A
   people-shaped principal makes that header worth more to forge, not
   less. The recommendation is in §9.4, and it is **not** "add a signing
   key".

---

## 1. What this is, and what it is NOT

**What it is.** One new credential kind — a JWT signed by a configured
issuer, verified in the door — producing a principal judged by machinery
forge already has: `spec.consumers` at the door, `policy.judge` at the
two enforcers, `X-Remote-User` between them.

| Not built | Why not |
|---|---|
| **Any OIDC flow.** No authorization-code exchange, redirect, callback, cookie, session, PKCE, client secret, refresh handling or logout. | §0.2. The application already has the token; `git` cannot follow a redirect. The single largest thing not built. |
| **Any service-mesh or ext_authz integration.** No `RequestAuthentication`, no claim-to-header trust mode, no mesh manifests in the chart. | §0.2. The door verifies what it is handed. A fronting validator, if one exists, is defence in depth and needs no code here. |
| **Discovery as a runtime dependency.** | Used, optionally and once, only to locate a JWKS URL. Nothing else reads it, and a door given a JWKS URL never fetches one. |
| **Token exchange (RFC 8693), or a JWT→ServiceAccount bridge.** | It would make forge an issuer. `spec.consumers` is an allow-list. Knox's own 3.x work is exactly this and is unreleased (`csi-node-mount-design.md` §9, "Endgame", VERIFIED unreleased) — do not design on it. |
| **Multiple issuers, or per-repository issuer configuration.** | One issuer per door. A per-repo issuer would let a repository owner nominate the authority that vouches for its own consumers, inverting the trust direction. Two issuers ⇒ two doors. |
| **Group *discovery*** — no `userinfo`, no LDAP, no directory. | Groups come from the token's claim and nowhere else. A directory call per request puts the issuer on the hot path of every `git fetch`. |
| **Revocation / introspection (RFC 7662).** | Offline verification only. `exp` plus the §6.4 lifetime ceiling is the whole bound, and §9 says so rather than implying a revocation that does not exist. |
| **Signing or binding the principal on the door→syncer hop.** | §9.4. Considered seriously and **not recommended here**; the smaller fix is to make the existing NetworkPolicy non-optional for repositories that name people. |
| **Any per-repository secret the door must hold.** | §9.5. The door has no secrets RBAC and must keep none — property #2 of the whole component (`lite_gateway/mod.rs:26-31`). The key material is door-level and mounted. |
| **Path- or directory-level rights.** | `consumers` and `branches` are the two levers. Per-directory ACLs are a different product. |
| **Any change to lean, passthrough, the s3 broker or the CSI node plugin.** | They authenticate pods, not people. §4. |
| **Removing or replacing TokenReview.** | Agents are pods and pods have ServiceAccount tokens. Both credential kinds are first-class, permanently. |
| **Widening `*`.** | §3.3. An upgrade must not admit one principal more than the CR admitted before it. |
| **A per-repository "JWT enabled" flag.** | It exists for free: a repository whose `principals` list is empty admits nobody, whatever the door is configured with. A second lever is a second thing to get wrong. |
| **Sender-constrained tokens (mTLS, DPoP), audit storage, per-user idle accounting, user provisioning.** | Separate designs. The door logs a line; it does not become a system of record. |

---

## 2. Ground truth — what the code does today

Cited so every claim can be checked, and so a reviewer can tell design
from description.

**The door's credential.** `basic_password` takes the HTTP Basic
*password* and **discards the username** (`git.rs:503-511`) — "a username
field would be a second, unverified opinion about who this is."
`presented_token` on the file door accepts `Bearer` *or* `Basic`, taking
the password identically (`repo_files.rs:188-195`). **Both doors already
accept an arbitrary opaque bearer**, which is what the whole design turns
on.

**The verifier.** `trait Reviewer { async fn review(&self, token: &str)
-> Result<Identity, String> }` (`git.rs:213-216`); `KubeReviewer` does a
`TokenReview` bound to `AUDIENCE = "forge.chert.us"` (`git.rs:74`,
`:218-236`); `CachingReviewer` caches by SHA-256 of the token for
`review_ttl` (default 60 s) and **caches a refusal but never a transport
failure** (`git.rs:279-307`). The file door shares the git door's
reviewer via `RepoFileDoor::beside` (`repo_files.rs:167-176`).

**The authorization decision.** `consumer_allows(consumers, repo_ns, id)`
(`git.rs:524-531`) — three accepted spellings: `*`, a fully-qualified
`system:serviceaccount:<ns>:<sa>` matched against `id.username`, and a
**bare name matched only when `id.namespace == repo_ns`**. One call per
request per door (`git.rs:974`, `repo_files.rs:455`), both answering 403
`NotAConsumer`. Absent list ⇒ deny (`git.rs:525`).

**A second, different matcher over the same field.**
`Consumers::allows(&self, sa)` (`s3csi/policy.rs:23-27`) is
namespace-blind and accepts only a bare name or `*`; used by the broker
(`broker.rs:185`) and, via `resolve::authorize` (`s3csi/resolve.rs:142`),
by the CSI node plugin (`s3csi/node.rs:323`). **The tree already has two
matchers with two meanings over one struct** — a shared *shape*, never a
shared rule. §4 makes that honest rather than extending it.

**The principal on the wire, and what the door does NOT send.** The door
sets `x-remote-user` from the verified identity and from nothing else;
the upstream header map is built from a static allowlist plus that one
line (`git.rs:1026-1029`, `repo_files.rs:550`, allowlist at
`git.rs:147-156`). **`authorization` is deliberately absent from that
allowlist** — "the caller's credential authenticates it to the DOOR and
is never forwarded" (`git.rs:141-146`). The door presents **no credential
of its own** on the upstream hop either, because it holds none: it has no
secrets RBAC at all (`lite_gateway/mod.rs:26-31`). `gitcgi` `env_remove`s
`REMOTE_USER` before every request and re-sets it only from
`x-remote-user` (`flint_forge_gitcgi.rs:200-215`). The syncer's file
listener **refuses an empty principal outright** with 403 `no-principal`
(`filehttp.rs:274-286`); the hook does not — it judges the empty string,
which fails every membership test (`hook.rs:122`, `policy.rs:148-155`).

**The judge.** `Policy::judge(&self, principal: &str, ref_name: &str,
new_oid: &str) -> Verdict` (`forge/syncer/src/policy.rs:156`), with
`judge_merge` at `:199`. Membership is exact-string or the literal `*`:

```rust
who.iter().any(|p| p == "*" || p == principal)     // policy.rs:173, :201
```

`glob_match` (`policy.rs:71`) applies to **ref names only** — there is no
principal pattern of any kind. The principal is a scalar `String`
everywhere: `HookRequest.principal` (`uds.rs:35`), `PushRequest.principal`
(`batch.rs:42`), `FileWrite.principal` (`fileapi.rs:563`). Two enforcers
read one rendered document: `pre-receive` (`hook.rs:110-136`) and the
syncer's judge step (`batch.rs:493`, calling `policy.judge` at `:507`).
The document is a ConfigMap from `render::policy_document`
(`forge_operator/render.rs:233`), mounted read-only at `/etc/flint-forge`;
a policy edit deliberately does **not** roll the pod (`render.rs:690-692`).

**Authorship.** `X-Flint-Author` is read by the syncer
(`filehttp.rs:288`) and is *untrusted by design*. One gap: it is **not**
in `route::request_headers` (`route.rs:109-129`), so it does not reach
the syncer through the repo file door at all.

**The other bearer.** `spec.fileApi.tokenSecret` (`crd.rs:302`) is a
Secret with key `token`, mounted as the file API's shared bearer — a
credential the **syncer requires and the door structurally cannot
present**, since the door forwards no `Authorization` and holds no
secrets. The two are alternatives today, not layers. §9.5.

**No JWT crate.** `spdk-csi-driver/Cargo.toml` carries `sha2`, `hmac`,
`base64`, `serde_json`, `reqwest`(rustls) and, transitively in
`Cargo.lock`, `ring 0.17.14`, `aws-lc-rs 1.18.0`, `untrusted`, `pem`.
No `jsonwebtoken`, no `josekit`, no JWKS client. §6.5.

---

## 3. The one list

### 3.1 The shape

```yaml
apiVersion: chert.us/v1alpha1
kind: FlintRepo
metadata: { name: proj, namespace: team-a }
spec:
  projectId: proj
  bucket: my-bucket
  keyPrefix: team-a/proj/
  consumers:
    # Principals the CLUSTER vouches for, ONLINE. Byte-identical to today.
    serviceAccounts:
      - agent-runner                                   # this namespace
      - system:serviceaccount:ci:release-bot           # any namespace
    # Principals a JWT ISSUER vouches for, OFFLINE. New. Prefixed entries.
    principals:
      - jwt:user:alice@example.com
      - jwt:group:platform
  branches:
    protected: [main]
    pushers:
      main: [jwt:group:platform]
      "release/*": [jwt:user:alice@example.com, system:serviceaccount:ci:release-bot]
    mergeInto:
      main: [jwt:group:engineering, system:serviceaccount:team-a:agent-runner]
    agentPattern: "agent/*"
```

**Two arrays inside one field, split on exactly one axis: who vouched,
and how.** `serviceAccounts` is what the apiserver asserted, *online*,
for a token bound to a live pod. `principals` is what an issuer asserted,
*offline*, in a token forge cannot revoke. Those two facts have different
threat models (§9) and a reviewer must see which is which without reading
the string. A single flat array mixing `agent-runner` and
`alice@example.com` would hide the distinction that matters most.

That is still **one list** in the sense the question demands: one CR
field, one function, one deny-by-default rule, one call site per door,
one step in the order. Nothing else grants access to a repository, and a
verifier never grants — it only says who you are.

### 3.2 The matching function

```rust
/// Who the door verified, and which authority said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// `TokenReview` at the apiserver said so, online.
    ServiceAccount(Identity),
    /// The configured JWT issuer said so, verified offline.
    Person {
        /// The subject claim, verbatim. Matched as an exact string.
        subject: String,
        /// The groups claim, verbatim, in the order presented.
        groups: Vec<String>,
        /// The `iss` that was verified. Carried for the audit line.
        issuer: String,
    },
}

impl Principal {
    /// What travels as `X-Remote-User`, and what `policy.judge` sees
    /// first. Prefixed for a person so the two namespaces of principal
    /// strings can never collide.
    pub fn remote_user(&self) -> String {
        match self {
            Principal::ServiceAccount(id) => id.username.clone(),
            Principal::Person { subject, .. } => format!("jwt:user:{subject}"),
        }
    }
    /// What travels as `X-Remote-Groups`. Empty for a ServiceAccount.
    pub fn remote_groups(&self) -> Vec<String> {
        match self {
            Principal::ServiceAccount(_) => Vec::new(),
            Principal::Person { groups, .. } =>
                groups.iter().map(|g| format!("jwt:group:{g}")).collect(),
        }
    }
}
```

```rust
/// Is this principal allowed to reach this repository at all?
///
/// TWO ARMS, DISJOINT BY CONSTRUCTION. A ServiceAccount never reads
/// `principals`; a person never reads `serviceAccounts`. That is what
/// keeps an existing CR's meaning fixed under upgrade (§3.3) — the two
/// halves of one list cannot leak into each other.
pub fn consumer_allows(c: Option<&RepoConsumers>, repo_ns: &str, p: &Principal) -> bool {
    let Some(c) = c else { return false };   // absent = deny. Unchanged.
    match p {
        // Byte-identical to today's body (git.rs:526-530).
        Principal::ServiceAccount(id) => c.service_accounts.iter().any(|e| {
            e == "*"
                || e == &id.username
                || (e == &id.service_account && id.namespace == repo_ns)
        }),
        Principal::Person { subject, groups, .. } => c.principals.iter().any(|e| {
            e == "jwt:*"
                || e.strip_prefix("jwt:user:").is_some_and(|u| u == subject)
                || e.strip_prefix("jwt:group:")
                     .is_some_and(|g| groups.iter().any(|h| h == g))
        }),
    }
}
```

Exact, case-sensitive, whole-string comparison on both sides — the idiom
the tree already uses in three places (`policy.rs:25`, `git.rs:526`,
`forge/syncer/src/policy.rs:173`). **No globbing on a principal**, here
or in the judge. `jwt:group:eng*` is a group literally named `eng*`.

**Why the prefix is `jwt:` and not `oidc:`.** The authority is "a JWT the
configured issuer signed", which is true whether or not any OIDC is
involved — and under §0.2 none is. `oidc:` would name a protocol nothing
in this design speaks.

### 3.3 What an existing entry still means — the non-widening rule

> **Upgrading the door must not admit one principal more than the CR
> admitted before it.**

- **`*` in `serviceAccounts`** means, and must keep meaning, *any
  ServiceAccount the door authenticated*. If it also admitted every
  person the issuer knows, every repository that opted into "any pod"
  would silently have opted into "any employee" on upgrade. The people
  wildcard is `jwt:*` in `principals`.
- **A bare name** means that ServiceAccount in the repository's namespace
  (`git.rs:517-523`). Unchanged: reachable only from the `ServiceAccount`
  arm.
- **An unknown prefix** — `jwt:users:alice@example.com`, note the typo —
  matches nothing: fail-closed but *silent*, and a silent refusal in an
  allow-list reads to an operator as "JWT auth is broken". CEL refuses
  the CR instead (§4.2).

### 3.4 Why one prefixed array, and not `users:` + `groups:`

- **One loop, one `match`.** Two arrays are two membership rules and two
  places to forget one.
- **A user and a group are the same kind of fact** — a name the issuer
  vouched for. Whether Alice may push should use the same arithmetic
  whether the entry names her or her team.
- **It extends without a new field.** A future `spiffe://…` or
  `jwt:claim:department=platform` entry is a new prefix, not a new CRD
  field. Every new CRD field is a future prune hazard; the tree already
  carries a tombstone for one (`flintpassthroughmounts.yaml:101`,
  `rule: "false"`).
- **Kubernetes already spells principals this way** —
  `system:serviceaccount:ns:sa`, `system:node:…` — and `serviceAccounts`
  *already* contains a prefixed spelling.

Honest counter-argument, recorded: two arrays are more discoverable in
`kubectl explain` and could each carry their own CEL `pattern`. One CEL
`all()` rule gives the same enforcement (§4.2). Not decisive either way;
the loop-count argument is.

**Rejected: overloading `serviceAccounts` itself** — zero CRD change,
literally one array, and the field name would be a lie. The honest name
(`principals`) is unavailable for the existing field because renaming a
CRD field **prunes** it. Rejected on naming, not on mechanism.

---

## 4. The CRD change, and its blast radius

### 4.1 What reads `Consumers` today — the full inventory

`s3csi::policy::Consumers` (`spdk-csi-driver/src/s3csi/policy.rs:18`) is
embedded by **three** CRDs and enforced by **four** servers:

| CRD | Field | Rust | Generated YAML |
|---|---|---|---|
| `FlintRepo` (forge) | `spec.consumers` | `forge_operator/crd.rs:118` | `flint-forge-chart/crds/flintrepos.yaml:93-105` |
| `FlintLeanWorkspace` | `spec.consumers` | `lean_operator/crd.rs:67` | `flint-lean-chart/crds/flintleanworkspaces.yaml:91-103` |
| `FlintPassthroughMount` | `spec.consumers` | `passthrough/spec.rs:81` | `flint-passthrough-chart/crds/flintpassthroughmounts.yaml:172-184` — **hand-written**, guarded by the drift test `the_crd_and_the_struct_agree_on_every_field` (`passthrough/spec.rs:199`) |

`FlintShare` (lite) does **not** embed it. Enforcers: the s3 broker
(`broker.rs:185`), the CSI node plugin via `resolve::authorize`
(`resolve.rs:142`, `node.rs:323`), the forge git door (`git.rs:974`) and
the forge repo-file door (`repo_files.rs:455`). No admission webhook
reads it, and **there is no CEL or structural validation on
`serviceAccounts` entries anywhere today** — a typo is accepted by the
API server and simply never matches.

### 4.2 The change: a forge-local type, same YAML key

**Do not extend the shared `Consumers`.** That would change three CRD
schemas — including the hand-written passthrough YAML and its drift test
— and would put a field into lean and passthrough that **nothing reads**:
a silent no-op in a security-relevant list, which is the shape of defect
this repo's standing rule refuses ("an error must not return a legal
value").

Instead `FlintRepoSpec.consumers` changes *type*. The YAML key does not
change, `serviceAccounts` does not change, nothing outside forge is
touched:

```rust
// forge_operator/crd.rs, beside BranchPolicy.
//
// Forge's own consumers block. It is NOT s3csi::policy::Consumers, and
// that is deliberate: the two matchers over that struct already mean
// different things (Consumers::allows is namespace-blind, git.rs's
// consumer_allows is namespace-aware), so the sharing was a shared
// SHAPE, never a shared rule. Splitting the type makes the divergence
// visible instead of latent, and keeps a forge-only field out of the
// lean and passthrough schemas where nothing would read it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoConsumers {
    /// Unchanged in name, type, default and meaning.
    #[serde(default)]
    pub service_accounts: Vec<String>,

    /// Principals a JWT issuer vouches for. Prefixed entries:
    /// `jwt:user:<sub>`, `jwt:group:<group>`, or `jwt:*` for "any
    /// person the issuer authenticated". NEVER a ServiceAccount —
    /// those live above, because the apiserver vouches for them ONLINE
    /// and this list is verified OFFLINE.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub principals: Vec<String>,
}
```

plus one CEL rule on `FlintRepoSpec`, so a typo'd prefix is a refusal
rather than a silent non-match:

```rust
#[x_kube(validation = Rule::new(
    "!has(self.consumers) || self.consumers.principals.all(p, \
     p == 'jwt:*' || p.startsWith('jwt:user:') || p.startsWith('jwt:group:'))")
    .message("spec.consumers.principals entries must be jwt:user:<sub>, \
              jwt:group:<group> or jwt:*"))]
```

Only `==`, `startsWith` and `all` — no character classes, no escapes. A
`[` inside a CEL string has previously taken out a whole CRD in this repo
(`project_idle_lifecycle_wave`), which is why the rule is written this
way and not as a regex.

§9.4 adds one more guard, and it belongs in the **operator** rather than
in CEL, because the door's namespace is chart configuration and not part
of the CR: a repository whose `principals` is non-empty while no door
NetworkPolicy is rendered is marked `Degraded` with a condition that says
why.

### 4.3 Migration

- **Additive and optional.** `principals` absent ⇒ empty ⇒ no person is
  admitted. Every existing `FlintRepo` deserializes byte-identically.
- **Nothing is removed, so nothing is pruned.** No tombstone needed.
- **Order of deployment.** CRD first (the chart's `crds/` copy plus the
  operator's compiled-in `crd()`), then the door image. A new door with
  an old CRD sees the field dropped on read; an old door with a new CRD
  ignores it and admits nobody new. Both orders are safe; CRD-first is
  one fewer question.
- **Cost of the type split, stated.** `Consumers` and `RepoConsumers` can
  now drift on `serviceAccounts`. Mitigation: a unit test asserting the
  two serialize that half identically, in the spirit of
  `BranchPolicy::render`'s field-by-field mapping (`crd.rs:236`) — "a
  field added to either side that nobody maps here fails to compile."
- **Test fixtures:** `forge_operator/{idle,reconcile,render}.rs`
  construct `FlintRepoSpec` literally; the compiler finds them all.

---

## 5. Which door authenticates how, and telling the two JWTs apart

### 5.1 The premise, checked

The question assumes git clients cannot present an issuer's token. They
can, and the door already accepts it, because **the git door takes the
Basic *password* as an opaque token and ignores the username**
(`git.rs:503-511`). A credential helper is a program that prints
`username=…` and `password=…`:

```
git config credential.https://forge.example.com.helper '!f() { \
  echo username=jwt; echo password=$(get-my-token); }; f'
```

So the honest split is **not door-by-door**; it is credential-by-
credential, and both doors take both kinds:

| | git door (`/git/...`) | file door (`/v1/repos/...`) |
|---|---|---|
| ServiceAccount token | Basic password → TokenReview | `Bearer`/Basic → TokenReview |
| Issuer JWT | Basic password → JWT verify | `Bearer`/Basic → JWT verify |
| Authorization | `consumer_allows` | `consumer_allows` |
| Principal upstream | `X-Remote-User` + `X-Remote-Groups` | identical |

One authorization model, two authentication mechanisms. "Two
authorization models" would mean two allow-lists, two matching rules, or
two orders of operations; there are none. The door's five-step order
(authenticate → authorize → route → wake → bound) is unchanged, including
the property that authentication precedes the wake, so an unauthenticated
peer still cannot scale a parked repository up.

### 5.2 The discriminator — a first-class part of the design

**Both credentials are JWTs.** A kubelet-minted ServiceAccount token is
an RS256 JWT with the cluster's issuer in `iss`; a Knox token is an RS256
JWT with Knox's issuer in `iss`. `presented_token` hands the door one
opaque string and the door must pick a verifier. Getting it wrong fails
in both directions:

- **An issuer JWT sent to `TokenReview`** — refused by the apiserver.
  Safe, but it burns a round trip and returns a message ("token is not
  authenticated") that sends an operator hunting the wrong problem.
- **A ServiceAccount token sent to the JWT verifier** — checked offline
  against a foreign key and refused. Safe **unless the two issuers are
  the same string**, in which case a pod token gets verified *offline*
  and silently loses the property that makes a deleted pod's credential
  die within 60 s rather than at `exp`.

```rust
enum Route { Kube, Jwt }

/// Decide WHICH verifier, never WHETHER. Every failure lands on `Kube`,
/// the ONLINE verifier and the authoritative refuser.
fn route_of(token: &str, cfg: Option<&JwtConfig>) -> Route {
    let Some(cfg) = cfg else { return Route::Kube };   // today's door, exactly
    // 1. Not three dot-separated segments? Not a JWT. -> Kube
    // 2. base64url-decode segment 1, parse JSON, read `iss`.
    // 3. `iss` EXACTLY equal to cfg.issuer -> Jwt
    //    anything else, or any parse failure -> Kube
}
```

Four properties, each the reason for a line:

1. **The unverified claim chooses a verifier, never grants.** Forging
   `iss` routes you to the JWT verifier, which checks the signature
   against the issuer's key and refuses. There is no path in which the
   routing decision grants anything.
2. **Comparison is exact, never prefix or suffix.**
   `https://knox.example.com` must not match
   `https://knox.example.com.evil.test`; a `starts_with` here is the bug.
3. **The default is `Kube`**, the online verifier, which gives an
   authoritative answer for an unknown token. Defaulting to `Jwt` would
   send Kubernetes tokens to an offline verifier.
4. **No fallback.** One route, one verdict. "Try one, then the other"
   gives a bad JWT a second chance, doubles apiserver load and pollutes
   the refusal cache.

**The startup refusal (D6).** The door reads its own projected token at
`/var/run/secrets/kubernetes.io/serviceaccount/token`, decodes `iss`, and
**refuses to start if it equals the configured JWT issuer.** Without that
check the ambiguity is invisible: everything keeps working, and the only
symptom is that a deleted pod's token stays valid until `exp`. (A cluster
with multiple SA issuers is covered only for the door's own. Noted, not
handled.)

### 5.3 Refusals that must not be a 401

The git door answers 401 **with** `WWW-Authenticate: Basic`
(`git.rs:490-499`) because git only sends a credential after a challenge;
the file door answers 401 **without** it (`repo_files.rs:197-206`)
because a Basic challenge reaching a browser puts a native password
dialog in front of someone who has no password. Both stay.

New failure mode: **the key source is unavailable and the token's `kid`
is not cached.** That is not "your credential is bad" — it is "ask
again". **503 with `Retry-After`**, never 401. A 401 there sends a client
into a refresh loop against an issuer that is already down and tells a
user their login expired when it did not. This mirrors
`CachingReviewer`'s existing rule that a refusal is cached and a
transport failure is not (`git.rs:294-300`).

**A key-source problem must never take down ServiceAccount access.** The
door starts and serves regardless; only a *JWT* presented before the key
source resolves gets 503. `readyz` is **not** gated on the key source.
(The alternative — hold `readyz` False until the JWKS resolves — was
considered and rejected: it turns an issuer blip at rollout into an
outage for agents that have nothing to do with the issuer.)

### 5.4 Whether the git door should accept a person at all

It should, for the reason the question contains: if it does not, the two
doors *do* diverge and a repository's authority becomes "who may read it
in a browser" plus "who may push to it", decided by two mechanisms with
two failure modes. One switch (JWT off ⇒ off at both doors) keeps them in
step, and a deployment that wants people reading but not pushing already
has the lever: `door.readOnly` on a second door, or a `principals` list
scoped to who should push.

---

## 6. Verifying the JWT

A new module, `spdk-csi-driver/src/lite_gateway/jwt.rs`.

### 6.1 Configuration

```rust
pub struct JwtConfig {
    /// Compared to `iss` as an EXACT string. Need not be a URL — Knox
    /// 2.1.0 issues the literal `KNOXSSO` (csi-node-mount-design §4.1).
    pub issuer: String,
    /// REQUIRED, non-empty. The door refuses to start without it. §6.3.
    pub audience: String,
    /// Exactly one of Jwks | Static | Hmac. §6.2.
    pub keys: KeySource,
    /// Allowed signature algorithms. An allowlist, never the token's own
    /// `alg` header alone. Constrained by the key source (§6.2).
    pub algorithms: Vec<Algorithm>,      // default [RS256, ES256]
    /// Claim holding the subject. Default `sub`.
    pub subject_claim: String,
    /// Claim holding the groups. Default `groups`. A literal claim KEY,
    /// not a path — Knox spells it `knox.groups`, with a dot in it.
    pub groups_claim: String,
    /// Clock leeway on `exp` and `nbf`, seconds. Default 60.
    pub leeway_secs: u64,
    /// Hard ceiling on a presented token's lifetime. Default 3600. §9.3.
    pub max_lifetime_secs: u64,
    /// Bounds on the group set. Default 64 groups, 128 bytes each. §7.3.
    pub max_groups: usize,
    pub max_group_len: usize,
}

pub struct JwtVerifier { cfg: JwtConfig, keys: Keys, http: reqwest::Client }

impl JwtVerifier {
    pub async fn verify(&self, token: &str) -> Result<Principal, AuthError>;
}

/// The distinction the door's status code depends on. `Refused` is a
/// property of the TOKEN and is stable; `Unavailable` is a property of
/// the NETWORK and is not. §5.3.
pub enum AuthError { Refused(String), Unavailable(String) }
```

### 6.2 Key material — three sources, exactly one chosen

A Knox deployment may hand out any of these, so all three are covered;
configuring two is **refused at startup** (two sources are two answers to
one question).

**(a) JWKS URL — the recommended default.**

```yaml
door:
  jwt:
    issuer: "KNOXSSO"
    audience: "forge.chert.us"
    jwksUrl: "https://knox.example.com/gateway/knoxtoken/api/v1/jwks.json"
```

Fetched on first need, cached by `kid`. On an unknown `kid`, refetch —
**single-flighted and rate-limited to at most one fetch per 60 s** — so a
thousand simultaneous clones after a rotation cause one fetch, not a
thousand. Still unknown ⇒ `Refused`. Fetch failed ⇒ `Unavailable` ⇒ 503.

**Keys are cached without a TTL and replaced only by a successful
fetch.** A key does not become wrong because time passed, and expiring
the cache on a timer would turn an issuer outage into a forge outage on a
schedule. Recommended because **rotation costs nothing**: the issuer
rotates, one token arrives with a new `kid`, one fetch happens, the door
carries on. Requires egress from the door's pod to the issuer.

**(b) Static public keys — right when (a) cannot reach.**

```yaml
door:
  jwt:
    staticKeysPath: /etc/flint-forge/jwt-keys   # PEM per file; FILENAME is the kid
```

Choose it when the JWKS is unreachable from the cluster (air-gapped, or a
network the door's namespace cannot leave), when the JWKS endpoint is
itself behind authentication — which breaks offline verification — or
when the deployment wants **zero runtime dependency** on the issuer: with
static keys the door makes no outbound call and an issuer outage is
invisible to it.

Two rules make it survivable: **several keys are accepted at once**, so a
rotation has an overlap window rather than a cutover; and an unknown
`kid` is logged at warning naming the `kid` and issuer, because with no
JWKS to refetch an unknown `kid` means "someone rotated and nobody
updated the mount", which is otherwise a silent total outage for people.
Re-read the directory on a short interval so a rotation needs no pod
bounce — the trick `TokenSource` already uses for lite's `tokenFile`
(`file-api-fleet-auth.md` §9, SHIPPED).

**(c) A shared HMAC secret (HS256) — supported, discouraged, fenced.**

Two hazards. A symmetric secret means the verifier can also **mint**
tokens, so a compromised door becomes an issuer for everything that
trusts that secret. And HS256 sitting in an allowlist beside RS256 is the
classic **algorithm confusion** bug: sign a token with HMAC using the
*public* RSA key as the secret, and a verifier that takes the algorithm
from the token's own header accepts it.

**Rule: the algorithm allowlist is a function of the key source.** With
`KeySource::Hmac` the allowlist may contain only HMAC algorithms and no
asymmetric one; with `Jwks` or `Static` it may contain only asymmetric
algorithms and no HMAC one. Enforced at **startup**, not at verify time,
so a misconfiguration is a pod that refuses to start rather than a door
that accepts forged tokens.

**Discovery is a convenience for locating (a), and nothing more.**

```yaml
door:
  jwt:
    discoveryUrl: "https://idp.example.com/.well-known/openid-configuration"
```

Resolved **once**: fetch, read `jwks_uri`, use it as if it had been
configured under `jwksUrl`; optionally cross-check the document's
`issuer` against `cfg.issuer` and refuse on a mismatch. Nothing else in
this design reads a discovery document, and a door given `jwksUrl` never
fetches one. Knox 2.1.0 publishes **no** discovery document (KNOX-3141,
Open, fix-version 3.0.0 — VERIFIED in `csi-node-mount-design.md` §4.1),
which is exactly why this is optional and why `issuer` is compared as an
opaque string rather than required to be a URL.

### 6.3 The checks, in order

Each refusal carries a distinct message.

1. **`alg` from the allowlist** — set on `Validation`; the token's header
   `alg` selects *within* the allowlist and never widens it (§6.2's rule).
2. **`kid` → key**, per §6.2. *If the key set holds exactly one key and
   the token carries no `kid`, use that key* — an explicit, documented
   fallback, because some issuers omit `kid`. More than one key and no
   `kid` ⇒ `Refused`.
3. **Signature.**
4. **`iss == cfg.issuer`,** exact string — made again against the
   *verified* payload, never trusted from the routing step of §5.2.
5. **`aud` contains `cfg.audience`.** Non-negotiable, non-empty. This
   repo's own recon records the failure mode: Knox's
   `AbstractJWTFilter.validateAudiences` treats an empty expected set as
   *"just consider any audience acceptable"* (`csi-node-mount-design.md`
   §9, K1 row). A door with no expected audience accepts tokens minted
   for any sibling application behind the same issuer. **The door refuses
   to start with an empty `audience`.**
6. **`exp` required,** enforced with `leeway_secs`. Absent ⇒ refused.
7. **`nbf` enforced only if present.** Requiring it would refuse every
   Knox token: Knox JWTs carry `sub`, `aud`, `exp`, `knox.id`, optional
   `knox.groups`, and **no `jti`, no `nbf`** (`csi-node-mount-design.md`
   §4.1, VERIFIED).
8. **Lifetime ceiling** — `exp - iat > max_lifetime_secs` ⇒ refused,
   naming the ceiling. With no `iat`, use `exp - now`. §9.3 says why this
   is the single most valuable line in the module.
9. **Subject non-empty**; groups read from `groups_claim` as an array of
   strings (a bare string is accepted as a one-element list). Bounds
   enforced: more than `max_groups`, a group longer than
   `max_group_len`, or a group containing a comma or a non-printable byte
   ⇒ **refused, not truncated**. Truncating silently drops authority; a
   comma is the `X-Remote-Groups` separator (§7.3).

### 6.4 Caching, and what happens when the issuer is down

**The key material is cached; the verification result is not.** A
verification is one signature check — tens of microseconds — so a result
cache buys nothing and would re-introduce the only question the
TokenReview path has to answer ("how long does a dead credential still
work"). The JWT path therefore has **no TTL at all**, which is a real
simplification and not a stylistic one.

| Situation | Answer |
|---|---|
| `kid` cached, or static keys | **Verifies normally.** The door works with the issuer down — better availability than TokenReview, which needs the apiserver every 60 s. |
| `kid` unknown, fetch fails | **503 + `Retry-After`.** Never 401. |
| Bad signature, wrong `aud`, expired, over the ceiling | **401.** A stable property of the token. |
| Any of the above while an agent presents a ServiceAccount token | **Unaffected.** §5.3. |

### 6.5 Which crate

**Recommendation: `jsonwebtoken`.** It is the crate this repo already
named for exactly this job (`docs/plans/file-api-fleet-auth.md` §8: "it
does not carry a JWT crate today, so that is a new dependency
(`jsonwebtoken`)"), and its backend (`ring 0.17.14`) is already in
`Cargo.lock`, so the cross-build toolchain question is probably already
answered.

**Uncertain, check before committing (O1):** whether `ring` is actually
*compiled* for the musl release targets or is present in the lock only
transitively while rustls uses `aws-lc-rs`. If it does not build under
the zigbuild recipe, the fallback is a `jsonwebtoken` version with an
`aws-lc-rs` backend, or RS256 verification written directly against
`aws-lc-rs` (a direct dependency via rustls). **Do not hand-roll JWT
parsing** in either case: `alg` confusion and `alg: none` are the two
classic ways to write a verifier that verifies nothing.

`jsonwebtoken` ships no JWKS fetching or caching; that part is written
here (~120 lines) and is modelled on `CachingReviewer`.

---

## 7. `policy.judge` keyed on people

### 7.1 What a rule looks like

```yaml
branches:
  protected: [main, "release/*"]
  pushers:
    main: [jwt:group:platform]
    "release/*": [jwt:user:alice@example.com, system:serviceaccount:ci:release-bot]
  mergeInto:
    main: [jwt:group:engineering, system:serviceaccount:team-a:agent-runner]
  agentPattern: "agent/*"
  allowNonFastForward: ["agent/*"]
```

**Existing rules keyed on ServiceAccounts do not change meaning at all.**
A pod's principal string is still `system:serviceaccount:<ns>:<sa>`,
matched by the same exact comparison. The new principals occupy a *new
namespace of strings* that no existing rule can collide with — the second
reason for the prefix, after readability.

### 7.2 The judge becomes set-valued

A `groups` claim is a list, so the caller is not one principal but a set:
their own name plus one entry per group.

```rust
// forge/syncer/src/policy.rs
pub fn judge(&self, principals: &[String], ref_name: &str, new_oid: &str) -> Verdict
fn judge_merge(&self, principals: &[String], target_full: &str) -> Verdict
```

and the two membership sites (`policy.rs:173`, `:201`) become

```rust
who.iter().any(|p| p == "*" || principals.iter().any(|q| q == p))
```

**Does this change the judge's semantics?** For every existing input, no
— a one-element slice makes the new expression identical to the old,
including the two edge behaviours that already have tests:

- an **empty** principal is `[""]`, which fails every named list and
  still matches `"*"`, so `tests.rs:1247`
  `an_unauthenticated_push_is_not_a_privileged_one` continues to hold;
- **rule selection is unchanged.** Which `pushers` entry applies is
  decided by `glob_match` on the *ref name* only (`pushers_for`,
  `policy.rs:139`). Groups affect membership within the selected rule and
  nothing else. (The pre-existing wart that `pushers_for` takes the first
  lexicographic `BTreeMap` match rather than the most specific one is
  unchanged and out of scope.)

**One invariant becomes load-bearing and must be written down:**

> "Any of" is the correct quantifier for an **allow**-list and the wrong
> one for a **deny**-list. Today every principal-sensitive rule in
> `Policy` is an allow-list over a deny-by-default base — `protected`
> denies without consulting the principal; `pushers` and `mergeInto`
> allow. The day a rule denies a *named* principal, that rule's
> quantifier must become "all of", or membership of a second group is the
> way around a personal denial.

### 7.3 Getting the group set to both enforcers

The policy has **two** enforcers by design — `pre-receive` at the edge
and the syncer's judge step — "because hooks can be misconfigured and the
writer cannot" (`crd.rs:190-192`). Both need the set.

- **Door → server:** a second header, `X-Remote-Groups`, comma-joined,
  set by the door from the verified principal and from nothing else,
  written into the same allowlist-built header map as `x-remote-user`
  (`git.rs:1026-1029`, `repo_files.rs:550`). Comma is safe because §6.3
  refuses a group containing one. `x-remote-groups` must **not** be added
  to `GIT_REQUEST_HEADERS` — like `x-remote-user`, it is written, never
  forwarded.
- **gitcgi → hook:** `env_remove("REMOTE_GROUPS")` unconditionally, then
  set only from `x-remote-groups`, exactly as `REMOTE_USER` is handled
  (`flint_forge_gitcgi.rs:200-215`). The trust boundary is unchanged and
  is still the operator's NetworkPolicy; it now protects two headers.
- **Hook and syncer:** `hook.rs:122` and `:209` gain the groups;
  `HookRequest` (`uds.rs:35`), `PushRequest` (`batch.rs:42`) and
  `FileWrite` (`fileapi.rs:563`) each gain `groups: Vec<String>`.
- **`filehttp.rs:274-286`** keeps refusing an empty principal, unchanged.
- **One correctness detail at `server.rs:434-442`:** batched file-API
  writes are partitioned so a chain is never judged under a mixed
  principal (`let who = writes[0].principal.clone(); … partition(|w| w.principal == who)`).
  The partition key must become `(principal, groups)` — otherwise two
  writes from the same person at two token vintages, with different group
  sets, are all judged under the first one's groups.

### 7.4 The alternative considered and rejected

**Groups gate the door only; `branches` keys on individuals.** Simpler —
no judge change, no second header, no set-valued membership. Rejected
because "the platform team may push `main`" is the rule organisations
actually want, and writing it as a list of individuals means the policy
goes stale the first time someone joins the team. The judge change is
~80 lines; the operational cost of the alternative is unbounded.

---

## 8. Authorship and authority, kept apart

`X-Flint-Author` is *author*; the door-verified principal is *committer*,
and the committer is what `policy.judge` judges
(`forge-file-api-design.md` §4.4a). That split is unchanged. Two
consequences are new:

- **When the principal is JWT-verified, the door writes `X-Flint-Author`
  itself and drops the caller's.** The door has just established who this
  person is; continuing to take the application's word for it would put
  an unverified opinion beside a verified one, and would let an
  application attribute a commit to a colleague the door had just
  authenticated. For a ServiceAccount principal the header passes through
  untouched — precisely the many-users-behind-one-credential case §4.4a
  exists for.
- **A gap this runs into:** `x-flint-author` is not in
  `route::request_headers` (`route.rs:109-129`), so today it does not
  reach the syncer through the repo file door at all. Under JWT the door
  *writes* it rather than forwarding it, closing the gap on that path
  only. The ServiceAccount path still needs the allowlist entry — a
  pre-existing bug this design surfaces rather than causes.

### 8.1 The cheaper sibling — when to build that instead

**JWT for authorship only.** The backend keeps its ServiceAccount token
for authority; the door verifies nothing new; the person's identity
travels as `X-Flint-Author` and lands in the commit. Roughly 60
production lines against this document's ~800.

**Build the sibling instead when all three hold:**

1. A trusted backend sits between people and forge and already
   authenticates and audits them — exactly `lite_gateway/mod.rs`'s stated
   model, "no opinion about who the end user is".
2. What you want is **history that names people** — a blame view, a
   review trail, a "who changed this" answer.
3. Every person who reaches that backend may do the same things in the
   repository.

**Build this document when (3) fails** — when the repository itself must
refuse Alice and admit Bob, or admit the platform team to `main` and
nobody else. That is an *authorization* difference, and no amount of
authorship metadata produces it: an unverified author header cannot be a
judge's input, because then anyone who can set the header can be anyone.

The two compose; the sibling can ship first and is not wasted work.

---

## 9. Threat model deltas

### 9.1 What is true today

A stolen ServiceAccount token is bounded three ways at once. It is
**audience-bound** to `forge.chert.us`, so a generic pod token is not a
forge credential (`git.rs:70-74`). It is **pod-bound**, so `TokenReview`
starts refusing it within seconds of the pod's deletion. And the review
is **online**, so the door's window is `review_ttl` (60 s), not `exp`
(`git.rs:253-258`; the architecture doc's "a deleted pod's token dies
within 60 s, not at `exp`"). It also lives only in a pod's projected
volume — there is no browser to steal it from.

### 9.2 What changes

| | ServiceAccount token | Issuer JWT |
|---|---|---|
| Where it lives | a pod's projected volume | a browser, a laptop, a shell history, a CI log |
| Revocation visible to forge | yes, within 60 s | **no. Ever.** |
| Bound by | pod lifetime + 60 s | `exp`, and nothing else |
| Audience separation | `forge.chert.us`, enforced | only if the issuer can mint a forge-specific `aud` (§10 Q4) |
| Trust root | the cluster's apiserver | the apiserver **∪** the issuer |
| Blast radius if the authority is compromised | any pod identity | any *person's* identity, including everyone named in `pushers` |

**Offline verification cannot see a revocation** — this repo's own recon
records the same property from the other side ("TSS revocation is
invisible to offline verifiers", `csi-node-mount-design.md` §9). And the
**trust root becomes a union**: forge believes two authorities, and a
compromise of either is a compromise of the repository.

### 9.3 What the door does about `exp`

- **Enforce `exp` with ≤ 60 s leeway.** One constant, on `exp` and on
  `nbf` when present. It is the same order as `review_ttl` on purpose:
  the leeway *is* a stolen-token window, and there is no reason for it to
  exceed the window the other credential kind already has.
- **Refuse a token whose presented lifetime exceeds a ceiling** (default
  1 h). The most valuable line in §6, and it comes straight from this
  repo's own recon: Knox's shipped `homepage.xml` sets `knox.token.ttl`
  to **120 days**. A 120-day bearer is a standing credential, and a door
  that accepts one has no bound worth the name. Refuse *loudly*, naming
  the ceiling, so the operator fixes the topology instead of wondering.
- **Require a forge-specific `aud`** and refuse to start without one
  (§6.3 check 5). Without it, a token stolen from any sibling application
  behind the same issuer is a forge credential.
- **Log every refusal with `iss`, `sub`, `exp` and the rule that fired**,
  and the first accept per token hash, rate-limited. There is no
  revocation, so the log is the only forensic artifact.

### 9.4 The last hop is a plain header, and that matters more now

**The finding.** The door presents **no credential at all** on the
upstream hop. It forwards an allowlisted header set plus `X-Remote-User`
set from the verified principal, deliberately never the caller's
`Authorization` (`git.rs:141-146`), and it holds no fleet credential
because it has no secrets RBAC (`lite_gateway/mod.rs:26-31`). So the
syncer learns the principal **as a plain header**, and the only thing
between an attacker and forging it is the NetworkPolicy the forge
operator renders — which is rendered **only when `door.namespace` is
set**, and enforced **only where the CNI enforces it** (kind's default
CNI enforces nothing). That was measured on kind with Cilium on
2026-09-05: 17 legs, a forged header overridden at the door, refused at
the port with the policy in place, and **merged into `main` with the
policy deleted**.

**A stronger AuthN at the door does not strengthen that hop.** It
lengthens the trust chain by one link while the last link stays a network
boundary:

```
issuer --JWT--> [door: verify, consumers] --plain header--> [syncer: judge]
                                          ^ NetworkPolicy, opt-in
```

**Is that acceptable for a people-shaped principal? It is weaker than for
a ServiceAccount, and the doc should say so plainly.** Three reasons:

1. **The blast radius is sharper.** The architecture document already
   records the honest limit of the ServiceAccount case — "the principal
   is the ServiceAccount, which many pods share, so a branch pattern
   bounds a branch's name and not its owner". Forging a shared identity
   gets you rights that are, by construction, coarse. Forging
   `jwt:user:alice@example.com` gets you *exactly Alice's* rights, and
   this design's whole purpose is that rules are written for her
   specifically. **Making the principal finer makes forging it worth
   more.**
2. **Groups are worse than users.** `X-Remote-Groups: jwt:group:platform`
   is one header value that grants a whole team's rights, and unlike a
   ServiceAccount there is no pod that must exist and no `TokenReview`
   that could ever have said no.
3. **The boundary is opt-in.** A chart with no `door.namespace` renders
   no policy and refuses nothing — the architecture doc already lists
   this among the "two defaults worth knowing before the first install".
   A repository that names people while that policy is absent has an
   authority model that is really "reachability".

**Should the door sign or bind the principal it asserts?** Considered
seriously. The candidates:

| Option | What it buys | What it costs |
|---|---|---|
| **NetworkPolicy only** (today) | nothing new | the boundary is opt-in and CNI-dependent |
| **HMAC with one shared secret** | a forged header is rejected | the operator must distribute a secret to every tenant namespace; any compromised syncer can forge to any other; **reintroduces secret distribution** |
| **Per-repo derived key** (lite's `derive.rs` shape) | per-repo blast radius, no stored secrets | the door must hold a **root key**, which today's forge-only door explicitly does not have (`values.yaml`: a forge-only cluster "has no hubs, no root key and no inbound token") |
| **Door signs a short-lived assertion; syncer verifies a public key** the operator renders into the policy ConfigMap | a compromised syncer can forge nothing; the door holds one private key in its own namespace, mounted, still no secrets RBAC | ~200 lines, a rotation story, clock skew and replay to think about |

The asymmetric option is genuinely the strongest, and it is worth stating
what it would buy: today, without the NetworkPolicy, anything that can
reach the syncer's port sets `X-Remote-User` to a listed pusher and
merges into `main`. With a signed assertion, the same attacker can only
present an unsigned one, which the syncer would judge as the **empty
principal** — failing every named list, so protected refs hold. That
converts "reaching the port is the authorization" into "reaching the port
gets you nobody's rights", which is a large improvement.

**Recommendation: do not build it in this design. Close the specific gap
instead.**

> **The operator must mark `Degraded` — with a condition and an event
> that name the reason — any `FlintRepo` whose `consumers.principals` is
> non-empty while no door NetworkPolicy is being rendered for it.** A
> repository that names people without the header boundary has an
> authority model it does not advertise, and the operator is the only
> component that knows both facts. ~20 lines, no key, no rotation, no new
> failure mode.

Reasons for that over building the signature now:

- It is **one small change to an existing component**, and simplicity is
  the stated primary criterion.
- The signature does not remove the need for the network boundary; it
  *reduces what crossing it gets you*. Both are worth having, and the
  cheap one is not yet fully deployed.
- A signing key is a rotation story, a clock-skew story and a replay
  story, and each is a place to be wrong. This design already adds a
  verifier; adding a *signer* in the same change doubles the new
  cryptographic surface.
- **It is separable.** Nothing here forecloses it: `X-Remote-User` and
  `X-Remote-Groups` are the only assertions, and binding them later is
  additive.

**Recorded as the right next design**, with a name so it can be found:
*"the door signs what it asserts"* — asymmetric, the operator renders the
door's public key into the repository's policy ConfigMap (which already
exists, is already mounted read-only and already updates in place without
rolling the pod, `render.rs:690-692`), and the syncer treats an unsigned
or stale assertion as the empty principal. Explicitly **not** HMAC, so no
secret is distributed and no syncer can forge to another.

### 9.5 No new secret, and the one that already exists

**This design introduces no per-repository secret the door must hold.**
The key material is *door-level* — one JWKS URL, or one directory of
PEMs, or one HMAC secret — mounted into the door's own pod in the door's
own namespace, never read through the API, never per repository. The
"no secrets RBAC anywhere" property (`lite_gateway/mod.rs:26-31`,
`file-api-fleet-auth.md` §5) survives intact, and that is a constraint
this design was written under rather than a happy accident.

**`spec.fileApi.tokenSecret` stays what it is: an alternative, not a
layer.** It is a shared bearer the syncer requires and the door
structurally cannot present, since the door forwards no `Authorization`
and holds no secrets. A repository is therefore reached *either* through
the door (TokenReview or JWT, then `consumers`, then `X-Remote-User`)
*or* directly with that shared token — and the second path carries no
principal at all, so `policy.judge` sees the empty string. Two
consequences worth stating: a repository that names people in
`principals` and *also* publishes a `tokenSecret` has a second way in
that its branch rules cannot express; and §9.4's operator check should
say so too, because "reaching the port" and "holding the shared token"
are the same class of bypass.

### 9.6 What already exists and now matters more

A stolen JWT can **push**. Three existing mechanisms bound the damage and
belong in the deployment guide rather than being reinvented:
`branches.protected` (a protected ref is never deleted and moves only via
a listed pusher or `refs/for/`); the destructive-push undo that keeps the
replaced state for 7 days (`project_forge_undo_x15`); and
`door.readOnly`, "the difference between a compromised door that reads
every repository and one that rewrites them" (`git.rs:190-192`).

---

## 10. Knox — what must be confirmed, narrowed to the JWT

**Nothing here assumes what Knox does.** The repo already contains a
verified Knox JWT recon (`docs/plans/csi-node-mount-design.md` §4.1 and
the K-table in §9), and it already shaped §6:

- A Knox JWT is a plain RS256 JWT with a public JWKS at
  `knoxtoken/api/v1/jwks.json` (Knox ≥ 1.6.0), `iss` defaulting to the
  **literal string `KNOXSSO`**, claims `sub`, `aud`, `exp`, `knox.id`,
  optional `knox.groups` — **no `jti`, no `nbf`**. (VERIFIED.)
- Knox 2.1.0 publishes **no** `/.well-known/openid-configuration`
  (KNOX-3141, Open). Hence optional discovery and an opaque `iss`.
- `knox.token.ttl` defaults to 30 s but the **shipped `homepage.xml` sets
  120 days**. (VERIFIED.) Hence §9.3's ceiling.
- Knox's own audience check treats an empty expected set as "any audience
  acceptable". (VERIFIED, from source.) Hence §6.3 check 5.

Six questions remain, each answerable in a couple of minutes with a
shell.

**Q1 — What signs the token, and what is in `iss`?** Obtain one real
token and decode it. Is `iss` the literal `KNOXSSO`, a URL, or something
else? *This also settles the off-ramp:* if `iss` names an upstream
provider — Entra, Keycloak, Okta, Ping — that Knox merely federates to,
point the door at **that** issuer and Knox is not in this design at all.
That outcome is strictly simpler and should be preferred if available.

**Q2 — Where is the key published, is it reachable, is it public?**
`curl -sk https://<gw>/gateway/knoxtoken/api/v1/jwks.json` from inside
the cluster. Public and reachable ⇒ §6.2(a), the default. Unreachable or
behind authentication ⇒ §6.2(b), static keys, and someone owns rotation.
A shared HMAC secret ⇒ §6.2(c) and its fencing rule. Also: one key or
several, and do tokens carry a `kid`?

**Q3 — What algorithm?** RS256 is expected; confirm from the token
header, because the allowlist is configuration and a wrong one refuses
everything — or, worse, admits HS256 beside RS256 (§6.2c).

**Q4 — What is in `aud`, and can a forge-specific one be issued?** Is
`knox.token.audiences` set on the topology forge would trust, and can a
*separate* audience be minted for forge? If every application behind this
gateway receives tokens with the same `aud`, **a token stolen from any of
them is a forge credential** and §9.3's audience control does not exist.
Second most consequential question after Q1.

**Q5 — Groups and subject: present, named what, shaped how?** Is
`knox.groups` in the token? A JSON array of strings, or one comma-joined
string? (Note the dot in the claim name — §6.1's `groups_claim` is a
literal key, not a JSON path, precisely because of this.) How many groups
does a typical user carry — §6.1 bounds at 64, and if real users carry
300 that bound is wrong and the CGI environment is a problem too. And
what is in `sub`: an email, a `uid`, an LDAP DN? `principals` entries
must be written in exactly that spelling.

**Q6 — Lifetime, `nbf`, and who mints the token a git client will
hold.** What is `knox.token.ttl` on that topology, and can a 15-minute
token be requested (`lifespan=PT15M` is honoured on 2.1.0 per the
recon)? If the answer is 120 days and cannot be changed, §9.3's ceiling
refuses every token and this design is not deployable against that
topology as configured. Is there any `nbf`? And: a person running
`git push` needs a token in a credential helper — is there a CLI that
produces one, and what is its refresh story? A design whose tokens live
15 minutes and whose users have no refresh path is one nobody uses.

**The 20-minute recipe:** obtain one real token; `cut -d. -f1,2`,
base64-decode both halves, and paste header and payload; `curl` the JWKS
URL from inside the cluster and paste the status code; say whether `sub`
is an email and whether `knox.groups` is present. **Q1, Q2, Q4 and Q6 can
each individually make this design unbuildable as written.**

---

## 11. Decisions

| # | Decision | Alternative rejected | Why |
|---|---|---|---|
| D0 | The door is a **resource server**: verify a bearer JWT, map a claim, authorize. No OIDC flow, no mesh integration. | A door that runs a login; a door that trusts a mesh's claim headers | The app already has the token; `git` cannot follow a redirect; and one mechanism in one process is the simplest thing that is correct (§0.2) |
| D1 | One `spec.consumers` field, two arrays: `serviceAccounts` (cluster-vouched, online) + `principals` (issuer-vouched, offline, prefixed) | A flat array; or `users:`+`groups:` siblings | One matcher loop; the split is on *who vouched and how*, the axis a reviewer needs; a flat array hides it; siblings duplicate the rule |
| D2 | Prefixed entries `jwt:user:`, `jwt:group:`, `jwt:*` | `oidc:` prefixes; structured objects | Nothing here speaks OIDC; Kubernetes' own idiom; extends without a new (prunable) CRD field |
| D3 | `FlintRepoSpec.consumers` becomes a **forge-local** `RepoConsumers`; `s3csi::policy::Consumers` untouched | Add `principals` to the shared `Consumers` | Extending it changes 3 CRD schemas including a hand-written one with a drift test, and puts a field into lean and passthrough that nothing reads — a silent no-op in a security list. The two matchers already diverge; the split makes it honest |
| D4 | `*` in `serviceAccounts` never admits a person; `jwt:*` is the people wildcard | Let `*` mean "anyone" | An upgrade must not widen an existing CR by one principal |
| D5 | Both doors accept both credential kinds | git door = TokenReview only, file door = JWT only | Both already take an opaque bearer and ignore the username; splitting gives one repository two effective authorities — the failure the question names |
| D6 | Discriminate on the **unverified** `iss`, exact compare, default `Kube`, no fallback; and **refuse to start if the JWT issuer equals the cluster's SA-token issuer** | Try one verifier then the other; sniff `alg` or token shape; allow the issuer collision | The unverified claim chooses a verifier, never grants; defaulting to the online verifier keeps unknown tokens authoritative; a fallback doubles load and pollutes the refusal cache; the issuer collision silently turns pod tokens into offline-verified ones with no symptom |
| D7 | Key material cached; verification result **not** cached | Cache verdicts like `CachingReviewer` | Verification is local and microseconds; a result cache only re-introduces "how long does a dead credential live" |
| D8 | Transport failure ⇒ 503; token failure ⇒ 401. Key-source trouble never affects ServiceAccount access, and `readyz` is not gated on it | One 401 for everything; hold `readyz` until the JWKS resolves | Mirrors `CachingReviewer`'s rule; a 401 during an issuer outage sends clients into a refresh loop and lies to the user; gating `readyz` turns an issuer blip into an outage for agents |
| D9 | `aud` required and non-empty; the door refuses to start without one | Optional audience | An empty expected-audience set means "accept anything" — then any sibling application's token is a forge credential |
| D10 | Hard ceiling on presented token lifetime (default 1 h) | Trust `exp` | The shipped Knox default is 120 days; there is no revocation, so `exp` must itself be bounded |
| D11 | Three key sources — JWKS (default), static PEMs, HMAC — exactly one configured; two is a startup refusal | JWKS only; or a static/JWKS fallback chain | A Knox deployment may publish any of them; two sources are two answers to one question |
| D12 | The algorithm allowlist is a **function of the key source**: HMAC only with `Hmac`, asymmetric only with `Jwks`/`Static`. Enforced at startup | One global allowlist | Algorithm confusion — HS256 signed with the RSA public key as the secret — is the classic JWT bypass |
| D13 | Discovery is optional and used **only** to locate a JWKS URL, resolved once | Require discovery; refetch it | Knox 2.1.0 publishes none; requiring it would exclude the exact deployment this is for |
| D14 | `judge(principals: &[String], …)`; membership is "any of" | Groups gate the door only, judge stays scalar | "The platform team may push main" is the rule people want; the singleton case is byte-identical, so nothing existing changes |
| D15 | Record "any of is for allow-lists" as an invariant | Leave it implicit | The day a deny rule names a principal, group membership becomes the way around it |
| D16 | `X-Remote-Groups`, comma-joined; a group containing a comma is **refused** at verification; count and length bounds **refuse**, never truncate | Repeated headers; JSON; base64; truncation | One header, one env var, fail-closed, survives the CGI boundary unchanged; truncation silently drops authority and the caller cannot tell |
| D17 | For a JWT principal the door **writes** `X-Flint-Author`; for a ServiceAccount it passes through | Always trust the header | Do not put an unverified opinion beside a verified one; the ServiceAccount case is §4.4a's whole point and must survive |
| D18 | One issuer per door | Per-repository issuer | A repo owner must not nominate the authority that vouches for its own consumers |
| D19 | **Do not sign the door→syncer assertion in this design.** Instead the operator marks `Degraded` any repository that names people while no door NetworkPolicy is rendered | Build the signature now; HMAC with a shared secret | §9.4. ~20 lines against ~200, no key, no rotation, no replay window; the signature stays additive later. HMAC is refused outright — it distributes a secret and lets any syncer forge to any other |
| D20 | **No per-repository secret the door must hold** | A per-repo verification key or bearer | The door has no secrets RBAC and must keep none — property #2 of the component (`lite_gateway/mod.rs:26-31`) |
| D21 | `jsonwebtoken` | Hand-rolled RS256; `josekit` | Already named in `file-api-fleet-auth.md` §8; its backend is already in the lock; hand-rolling is how `alg` confusion ships |

**Open questions, honestly labelled:**

- **O1.** Whether `ring` compiles under the musl/zigbuild release recipe
  or is in `Cargo.lock` only transitively and gated out. Decides D21's
  exact form. Checkable in one build.
- **O2.** Whether the file door should accept a token *directly from a
  browser* or only from a backend. This design assumes a backend forwards
  the user's token; browser-direct raises CORS and token-in-JavaScript
  questions out of scope here.
- **O3.** Whether `principals` should also accept
  `jwt:claim:<key>=<value>` on day one. Cheap to add later; not built.
- **O4.** Whether a repository should be able to *require* a person
  (refuse ServiceAccounts). Not built; nobody has asked.
- **O5.** Whether §9.4's operator guard should be a hard refusal rather
  than `Degraded`. A hard refusal is safer and could strand an existing
  repository on upgrade. Leaning condition-plus-event on first release,
  hard refusal later.

---

## 12. Falsifiers

Repo style: each is an **assertion** plus a **positive control** that
must fail when the named mechanism is removed. A falsifier with no
mutation that breaks it has not been shown to test anything — the record
contains three green drill shapes that measured nothing
(`project_forge_undo_x15`) and a test that fired through the wrong arm of
an `||` (`feedback_or_gate_needs_the_other_arm_shut`).

**F1 — An unlisted person is refused at both doors.** Assert: a fully
valid JWT whose `sub` is in no `principals` entry gets 403 `NotAConsumer`
at `/git/...` *and* at the file routes, and the upstream is never
dialled. Control: add `jwt:user:<sub>` ⇒ 200 at both. Mutation: make the
`Person` arm of `consumer_allows` return `true` ⇒ the refusal legs fail.

**F2 — `*` in `serviceAccounts` does not admit a person.** *(the
non-widening control)* Assert: `serviceAccounts: ["*"]`, `principals: []`,
valid JWT ⇒ 403. Control, moving exactly one dimension: the same CR with
`principals: ["jwt:*"]` ⇒ 200. Mutation: add `|| e == "*"` to the
`Person` arm ⇒ the first leg fails.

**F3 — The two arms do not leak into each other.** Assert:
`principals: ["jwt:user:alice@example.com"]` refuses a ServiceAccount
literally named that; and `serviceAccounts: ["system:serviceaccount:t:sa"]`
refuses a JWT whose `sub` is exactly that. Mutation: collapse the arms
into one list scan ⇒ both fail.

**F4 — A bad signature is refused, and the JWKS is fetched once.**
Assert: a token with a valid `kid` and a foreign signature ⇒ 401, and a
counting JWKS server records **at most one** fetch across 100 concurrent
such requests. Control: the correctly signed token ⇒ 200. Mutation:
remove the single-flight ⇒ the fetch-count assertion fails. Mutation:
disable signature verification ⇒ the 401 assertion fails.

**F5 — An unreachable issuer with a warm key admits; a cold `kid` answers
503, never 401; ServiceAccounts are untouched.** Three arms, one test.
After one successful verification, black-hole the JWKS URL: (a) the same
token still verifies (200); (b) a token with an unknown `kid` gets 503
with `Retry-After`; (c) a ServiceAccount token still gets 200. Mutation:
return `Refused` instead of `Unavailable` on a fetch error ⇒ (b) fails.
Mutation: gate `readyz` on the key source ⇒ (c) fails.

**F6 — `aud` is enforced, and cannot be unset.** Assert: a token whose
`aud` is `other-app` ⇒ 401 naming the audience. Control: the same token
with the forge audience added ⇒ 200. Second assertion: the door **refuses
to start** with an empty configured audience. Mutation: delete the
startup check ⇒ it starts, and that leg fails.

**F7 — The lifetime ceiling refuses a standing credential.** Assert: a
valid, correctly signed token with `exp - iat = 120 days` ⇒ 401 whose
message names the ceiling. Control: the same token at 15 minutes ⇒ 200.
This is Knox's shipped default and the leg most likely to fire on a real
deployment.

**F8 — The discriminator routes each JWT to the right verifier.** *(an
auth bypass in either direction)* The oracle is a **counter on each
verifier**, not the status code — a broken router still refuses, so
status alone proves nothing. Assert: presenting a ServiceAccount token
increments the TokenReview counter by exactly 1 and the JWT-verify
counter by 0; presenting an issuer JWT does the reverse. Assert: a JWT
whose `iss` is `https://knox.example.com.evil.test`, with the configured
issuer `https://knox.example.com`, routes to **Kube** and is refused —
guarding against a sloppy `starts_with`. Assert: a non-JWT opaque string
routes to Kube. Mutations: route everything to the JWT verifier ⇒ the
counter leg fails; change the `iss` compare to `starts_with` ⇒ the suffix
leg fails.

**F9 — The issuer may not be the cluster's own.** Assert: configure the
JWT issuer to the door's own SA-token `iss`; the door refuses to start.
Mutation: delete the check ⇒ it starts, and the test detects the real
consequence by asserting a pod token is *still refused* after its pod is
deleted — which it would not be, having been verified offline.

**F10 — Algorithm confusion is impossible by construction.** Assert: with
`KeySource::Jwks` configured, a startup that also lists HS256 in the
allowlist **refuses to start**. Assert: a token signed HS256 using the
RSA public key as the HMAC secret is refused. Mutation: allow a mixed
allowlist ⇒ the startup leg fails and the forged token is admitted.

**F11 — Static keys work with no network, and an unknown `kid` is loud.**
Assert: with `staticKeysPath` configured and no egress at all, a
correctly signed token verifies; a token with an unknown `kid` is refused
**and a warning naming the `kid` and issuer is emitted**. Control: add
the new PEM to the directory and, without a pod restart, the same token
verifies. Mutation: drop the re-read interval ⇒ the rotation leg fails.

**F12 — `X-Remote-Groups` cannot be smuggled past the door.** Assert:
send both `X-Remote-User` and `X-Remote-Groups` from the client; the
upstream recorder sees exactly one of each, both set from the verified
principal, and the caller's values nowhere. (Mirrors the existing
`x-remote-user` test at `repo_files.rs:856-885`.) Mutation: drop the
door's overwrite ⇒ the forged value reaches the recorder.
**Wire leg:** on a CNI that enforces NetworkPolicy, a direct push to the
repository port carrying a forged `X-Remote-Groups` is refused with the
policy in place and **merges with the policy deleted** — the 17-leg shape
of 2026-09-05. The deletion arm is the control; without it the leg proves
only that the port was unreachable.

**F13 — A repository that names people without the boundary is flagged.**
*(§9.4's recommendation)* Assert: with no `door.namespace` configured,
applying a `FlintRepo` whose `principals` is non-empty produces a
`Degraded` condition and an event naming the missing NetworkPolicy, and
the same CR with an empty `principals` does not. Control: set
`door.namespace` and the condition clears. Mutation: delete the operator
check ⇒ the first leg fails.

**F14 — A group rule admits only through the group.**
`pushers: {main: [jwt:group:platform]}`. Assert: Alice with `platform` in
her groups claim pushes `main`; the *same person* with the claim removed
is refused. Mutation: delete the `groups.iter().any(...)` term ⇒ the
**refusal** leg must fail — the control here is the refusal, not the
success, because a matcher that allows everything passes the success leg.

**F15 — Every existing ServiceAccount rule is unchanged.** Assert: the
whole `forge/syncer/tests/push_chain.rs` and `tests.rs:1098-1260` policy
suite passes byte-identically against the set-valued judge with
`REMOTE_GROUPS` unset. Control: inject a hardcoded extra principal into
the slice ⇒ `an_unauthenticated_push_is_not_a_privileged_one` and the
"may be pushed only by" legs must fail. Green against a *singleton* slice
proves nothing on its own; the mutation is what makes it a test.

**F16 — The two enforcers agree about groups.** Assert: with the hook
present and with `pre-receive` deleted, a group-authorised push is
allowed in both cases and a group-*un*authorised push is refused in both
cases. Mutation: pass an empty group slice to the syncer's judge only ⇒
the hook-deleted refusal leg must fail.

**F17 — `X-Flint-Author` is ignored for a JWT principal.** Assert: a
JWT-authenticated file write with `X-Flint-Author: someone-else` produces
a commit whose author is the verified `sub`. Control: the same write with
a ServiceAccount principal keeps the header's value (§4.4a must not
regress). Mutation: remove the overwrite ⇒ the first leg fails.

**F18 — A malformed `principals` entry is refused at admission.** Assert:
`kubectl apply` of a CR with `principals: [jwt:users:alice]` fails with
the CEL message. Control: the corrected entry applies. Second assertion:
the CRD itself still installs — the rule uses only `==`, `startsWith` and
`all`, and a malformed CEL rule takes out the whole CRD, not just a
field.

**F19 — An oversized group set is refused, not truncated.** Assert: a
token with 65 groups against a bound of 64 ⇒ 401 naming the bound, and
`X-Remote-Groups` is never sent. Control: 64 groups ⇒ 200 with all 64
present upstream. Mutation: truncate instead of refusing ⇒ the first leg
passes and "the request never reached upstream" fails.

**Drill leg (on the wire, not local):** against the real issuer, a person
in `principals` clones, pushes an `agent/*` branch, is refused on `main`,
proposes via `refs/for/main`, and is merged — then is removed from the
group and the *same unexpired token* is refused at the next request. That
last clause is the leg that shows `consumers`, and not the token, is the
authority; without it the drill measures only that a login works.

---

## 13. Scope and cost

Order of magnitude, production / test, in lines.

| Piece | Prod | Test |
|---|---|---|
| `lite_gateway/jwt.rs` — verifier, three key sources, JWKS cache + single-flight, static-key re-read, config | 280 | 460 |
| `Principal` enum, `Reviewer` return type, `route_of` + routing reviewer, `consumer_allows` | 130 | 190 |
| `RepoConsumers` + CEL + regenerated `flintrepos.yaml` | 40 | 60 |
| Door config, CLI flags, startup refusals (D6, D9, D12), chart values and template | 110 | 70 |
| Operator: `Degraded` when `principals` is set with no door policy (§9.4) | 20 | 50 |
| `X-Remote-Groups` end-to-end: door → gitcgi → hook → `HookRequest`/`PushRequest`/`FileWrite` → `server.rs` partition key | 120 | 150 |
| `flint_forge::policy` set-valued judge + both enforcers + call sites | 80 | 200 |
| `X-Flint-Author` rule (and allowlisting it on the ServiceAccount path) | 20 | 60 |
| **Total** | **~800** | **~1240** |

**Order 10^3 lines, roughly 40 % production.** One to two weeks of build
plus a wire drill (~200 lines of shell, in the shape of `forge/e2e/`),
and the drill cannot run until §10 is answered.

For comparison, the §8.1 sibling — identity for authorship only — is
**~60 prod / ~120 test**, an afternoon, and delivers item (2) of §8.1
alone.

**Cheapest correct order to build it, if approved:**

1. `RepoConsumers` + CEL + `consumer_allows` with the `Principal` enum,
   the `Person` arm unreachable (no verifier yet). F1–F3 and F18 pass
   here, and the non-widening property is nailed down before any
   credential can exercise it. **~170 lines.**
2. The operator's §9.4 guard. F13. **Do this before step 3**, so a
   repository cannot name people while the header boundary is absent.
3. `jwt.rs` with the JWKS source, `route_of` and the startup refusals,
   behind an unset-by-default issuer. F4–F10. **The door is now usable
   for read access by people — a shippable increment.**
4. Static keys and the HMAC fence. F11.
5. `X-Remote-Groups` and the set-valued judge. F12, F14–F16, F19.
6. The authorship rule. F17.

Steps 1–3 are the majority of the value. Steps 5–6 are what makes "branch
rules key on people" true rather than "branch rules key on one person at
a time". Step 4 may never be needed — §10 Q2 decides.

---

## 14. Where this document is uncertain

Stated plainly rather than papered over.

- **§10 is not a formality.** Q1, Q2, Q4 and Q6 can each individually
  make this unbuildable against the deployed Knox as configured. The best
  outcome is Q1 revealing an upstream provider behind Knox, in which case
  Knox leaves the design entirely and the door is pointed at that
  provider's issuer and JWKS.
- **The crate choice (D21) rests on an unverified build fact** (O1).
- **Group cardinality is guessed.** 64 groups / 128 bytes is chosen to be
  obviously safe for a CGI environment and an HTTP header, not measured
  against a real directory. If real users carry hundreds of groups, D16's
  single header and the CGI environment both need rethinking, and that
  redesign is bigger than the bound suggests.
- **§9.4's recommendation is a judgement, not a finding.** The signing
  option is genuinely stronger, and the argument against it is cost and
  sequencing rather than correctness. If the deployment is multi-tenant
  and the CNI does not enforce NetworkPolicy, that judgement flips and
  *"the door signs what it asserts"* should be written before this design
  ships.
- **The `pushers_for` first-match-wins wart** (`policy.rs:139`, a
  `BTreeMap` scanned lexicographically rather than by specificity) is
  pre-existing and untouched. It becomes more visible once rules name
  groups, because `main` and `*` are then more likely both to be written.
- **Whether people should reach the git door at all** is a judgement
  (§5.4), not a finding. A deployment that wants people never to push
  through git will find this design permits it and would need D5
  revisited.
- **HS256 support (§6.2c) is included because a Knox deployment might
  hand one out**, not because it is a good idea. If §10 Q2 answers
  "public RSA JWKS", delete `KeySource::Hmac` and D12 shrinks to a
  compile-time fact instead of a startup check.
