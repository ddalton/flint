//! `flint-hub-gateway` — one door in front of every hub's file API.
//!
//! ## What problem this is
//!
//! A hub's file API is reachable at `status.apiEndpoint`, which is an
//! in-cluster headless Service name. That is deliberate — the operator
//! renders a BACKEND and never a routable address, because
//! `docs/plans/hub-api-service-design.md` shows NetworkPolicy cannot be
//! relied on to guard a per-share LoadBalancer and 3000 ClusterIPs
//! would exhaust a default service CIDR before the validated fleet
//! size. So the endpoint is addressable and not exposed, and something
//! has to bridge the gap for a project service that renders a UI.
//!
//! This is that something: ONE endpoint, deployed beside the hubs, that
//! resolves a project id to its share, wakes it if it is parked, and
//! proxies exactly the file routes.
//!
//! ## The three properties it exists to have
//!
//! 1. **It cannot reach `/status`.** The hub serves an unauthenticated
//!    `/status` on the same listener as the file API — tier recovery
//!    point, epoch holder, NFS client list, lifecycle phase. The
//!    gateway has no route that can produce that URL, by construction
//!    rather than by filtering ([`route`]).
//! 2. **It holds one credential, not three thousand.** Reading each
//!    share's token Secret would mean `get secrets` in every tenant
//!    namespace — where the tenants' S3 credentials also live. Instead
//!    a token is a pure function of the share's immutable identity
//!    ([`derive`]), so there is nothing to store and nothing to fan
//!    out. The gateway has no secrets RBAC at all.
//! 3. **It does not defeat the idle ladder.** A file-API call counts as
//!    activity on a share, so a gateway that polled hubs to find out
//!    whether they were up would pin every share it ever touched awake.
//!    Everything it needs — phase, hub phase, endpoint, conflict — is
//!    on the CR it already watches ([`resolve`]).
//!
//! ## What it is not
//!
//! Not a place to put business logic. It does not know what a project
//! IS, does not create shares, does not delete them, and has no opinion
//! about who the end user is — the project service authenticates people
//! and audits them, and calls this with one service credential. Adding
//! per-user identity here is `docs/plans/file-api-fleet-auth.md` §8,
//! and it belongs on the hub, not in a proxy.

pub mod derive;
pub mod proxy;
pub mod resolve;
pub mod route;

pub use proxy::{Config, Gateway};
