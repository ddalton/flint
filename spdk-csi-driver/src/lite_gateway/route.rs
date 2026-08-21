//! What the gateway is willing to proxy, as a closed set.
//!
//! This is the security boundary of the whole component, so it is a
//! module rather than a few match arms buried in the handler.
//!
//! ## The invariant
//!
//! **No byte of the upstream PATH ever comes from the caller.** The
//! gateway matches an inbound request against the table below, gets a
//! [`Verb`], and asks the verb for a `&'static str`. A request whose
//! shape is not in the table is a 404 from the gateway and never
//! becomes an upstream call at all.
//!
//! The alternative — forwarding a captured tail, `/v1/projects/{p}/{tail}`
//! → `{endpoint}/{tail}` — is what makes `/status` reachable. The hub
//! serves `/status` UNAUTHENTICATED on the same listener as the file
//! API (`pnfs/mds/server.rs`), and it reports the tier's recovery
//! point, the epoch holder, the NFS client list and the share's
//! lifecycle phase. A gateway that faces the internet and forwards a
//! tail publishes all of that, and no amount of path normalisation
//! makes forwarding safe — `%2e%2e%2f`, over-long UTF-8, a second
//! decode inside hyper, a `..` the gateway resolves differently from
//! the hub. The fix is not to normalise better; it is to never hold a
//! caller-supplied path in the first place.
//!
//! ## Why the query string is different
//!
//! Query parameters ARE forwarded, filtered by name
//! ([`Verb::query_keys`]). They have to be: `path=`, `cursor=`,
//! `recursive=` and `limit=` are the API. A query key cannot change
//! which endpoint is hit, so the risk it carries is a hub-side one that
//! the hub already handles — `FsPath::parse` rejects traversal, and it
//! is the same parse whether the caller is the gateway or not. Filtering
//! by name is still worth doing: it keeps a caller from smuggling a
//! parameter a future hub might grow, and it means an added parameter
//! is a deliberate edit here.

/// The hub file-API operations the gateway exposes.
///
/// One variant per (method, path) pair the hub serves under `/files`.
/// The hub has exactly six; this has exactly six, and a test pins that
/// they are the same six.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `GET /files` — one directory page, or a recursive walk.
    List,
    /// `GET /files/content` — bytes, possibly ranged.
    Download,
    /// `PUT /files/content` — bytes in.
    Upload,
    /// `DELETE /files/content`.
    Delete,
    /// `POST /files/folder`.
    Folder,
    /// `POST /files/move`.
    Move,
}

impl Verb {
    /// The upstream path. A `&'static str` BY CONSTRUCTION — this is
    /// the invariant the module doc describes, expressed in the type.
    pub fn upstream_path(self) -> &'static str {
        match self {
            Verb::List => "/files",
            Verb::Download | Verb::Upload | Verb::Delete => "/files/content",
            Verb::Folder => "/files/folder",
            Verb::Move => "/files/move",
        }
    }

    pub fn method(self) -> reqwest::Method {
        match self {
            Verb::List | Verb::Download => reqwest::Method::GET,
            Verb::Upload => reqwest::Method::PUT,
            Verb::Delete => reqwest::Method::DELETE,
            Verb::Folder | Verb::Move => reqwest::Method::POST,
        }
    }

    /// Whether this changes the share. Used for one thing only: the
    /// read-only posture (`--read-only`), which lets a browse UI be
    /// deployed without write authority over every project.
    pub fn is_mutation(self) -> bool {
        !matches!(self, Verb::List | Verb::Download)
    }

    /// Query parameters forwarded verbatim, by name. Anything else the
    /// caller sends is dropped.
    pub fn query_keys(self) -> &'static [&'static str] {
        match self {
            Verb::List => &["path", "recursive", "cursor", "limit"],
            Verb::Download | Verb::Upload | Verb::Delete => &["path"],
            // Both take a JSON body and no query at all.
            Verb::Folder | Verb::Move => &[],
        }
    }

    /// Request headers forwarded upstream, by name.
    ///
    /// Conditional-request headers are the point: `If-Match` is what
    /// makes the hub's compare-and-swap work at all (v1.30.0), and
    /// dropping it would silently downgrade every conditional write in
    /// the fleet to a blind overwrite — a lost-update bug that no test
    /// on either side would see, because both would still answer 200.
    ///
    /// `Authorization` is deliberately absent: the caller's credential
    /// authenticates it to the GATEWAY and is never forwarded. The
    /// gateway presents its own.
    pub fn request_headers(self) -> &'static [&'static str] {
        match self {
            Verb::Download => &["range", "if-none-match"],
            Verb::Upload => &["if-match", "if-none-match", "content-type"],
            Verb::Delete | Verb::Move => &["if-match"],
            Verb::Folder => &["content-type"],
            Verb::List => &[],
        }
    }
}

/// Response headers copied back to the caller, by name.
///
/// An allowlist rather than a copy-everything-minus-hop-by-hop, because
/// the failure directions are not symmetric: forgetting to strip
/// something the hub grows later leaks it, while forgetting to add
/// something breaks a feature loudly. `ETag` and `Content-Range` are
/// here because the conditional-write and Range protocols are useless
/// without them.
pub const RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
    "retry-after",
];

/// The whole table, for tests and for the route builder.
pub const ALL: &[Verb] = &[
    Verb::List,
    Verb::Download,
    Verb::Upload,
    Verb::Delete,
    Verb::Folder,
    Verb::Move,
];

/// Build the upstream URL.
///
/// `endpoint` is `status.apiEndpoint`, which the operator produced from
/// `render::api_endpoint` and never from anything a tenant controls.
/// `query` has already been filtered to [`Verb::query_keys`].
pub fn upstream_url(endpoint: &str, verb: Verb, query: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    if query.is_empty() {
        format!("{base}{}", verb.upstream_path())
    } else {
        format!("{base}{}?{query}", verb.upstream_path())
    }
}

/// Keep only the parameters this verb declares, preserving order.
///
/// Values are re-encoded from the decoded form, so a caller cannot
/// smuggle a `#` or a second `?` through a double encoding and cut the
/// rest of the URL off.
pub fn filter_query(verb: Verb, pairs: &[(String, String)]) -> String {
    let allowed = verb.query_keys();
    let mut out = String::new();
    for (k, v) in pairs {
        if !allowed.contains(&k.as_str()) {
            continue;
        }
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&percent_encode(k));
        out.push('=');
        out.push_str(&percent_encode(v));
    }
    out
}

/// Percent-encode everything outside the unreserved set.
///
/// Deliberately conservative: `/` is encoded too. A path lands in the
/// `path=` VALUE, and the hub reads it as a query parameter and parses
/// it with `FsPath::parse` — nothing needs a bare slash to survive, and
/// encoding it removes a whole class of "where does this URL end"
/// disagreement between the gateway's writer and the hub's reader.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE ONE THAT MATTERS.
    ///
    /// Whatever a caller sends — a traversal, an encoded traversal, a
    /// null byte, a whole second URL — the upstream path is one of six
    /// literals. This asserts the property directly rather than testing
    /// a list of payloads someone thought of, because the payload list
    /// is exactly what an attacker is better at than the author.
    #[test]
    fn no_caller_input_can_reach_the_upstream_path() {
        let hostile = [
            "../status",
            "..%2Fstatus",
            "%2e%2e%2fstatus",
            "....//status",
            "/status",
            "files/../status",
            "a\u{0}/status",
            "http://evil.example/status",
            "\\..\\status",
        ];
        for verb in ALL {
            for h in hostile {
                let q = filter_query(*verb, &[("path".into(), h.to_string())]);
                let url = upstream_url("http://hub.ns.svc.cluster.local:8080", *verb, &q);
                let (path, _) = url["http://hub.ns.svc.cluster.local:8080".len()..]
                    .split_once('?')
                    .unwrap_or((&url["http://hub.ns.svc.cluster.local:8080".len()..], ""));
                assert_eq!(
                    path,
                    verb.upstream_path(),
                    "{verb:?} with {h:?} produced {url}"
                );
                assert!(
                    !url.contains("/status"),
                    "{verb:?} with {h:?} produced a URL containing /status: {url}"
                );
            }
        }
    }

    /// The anti-vacuity guard for the test above: the payloads DO
    /// survive into the request (encoded, in the query), so the
    /// assertion is not passing because everything got thrown away.
    #[test]
    fn the_hostile_payloads_are_still_forwarded_just_not_as_path() {
        let q = filter_query(Verb::Download, &[("path".into(), "../status".to_string())]);
        assert_eq!(q, "path=..%2Fstatus");
        let url = upstream_url("http://h:8080", Verb::Download, &q);
        assert_eq!(url, "http://h:8080/files/content?path=..%2Fstatus");
    }

    #[test]
    fn unknown_query_parameters_are_dropped() {
        let q = filter_query(
            Verb::List,
            &[
                ("path".into(), "/".into()),
                ("token".into(), "steal-me".into()),
                ("limit".into(), "10".into()),
                ("redirect".into(), "http://evil".into()),
            ],
        );
        assert_eq!(q, "path=%2F&limit=10");
    }

    #[test]
    fn a_body_verb_forwards_no_query_at_all() {
        for v in [Verb::Folder, Verb::Move] {
            let q = filter_query(v, &[("path".into(), "/x".into())]);
            assert_eq!(q, "", "{v:?} takes a JSON body, not a query");
            assert_eq!(upstream_url("http://h:8080", v, &q), format!("http://h:8080{}", v.upstream_path()));
        }
    }

    /// The gateway must not forward the caller's credential to a hub.
    /// It authenticates the caller to the GATEWAY; a hub that received
    /// it would be holding the credential to every other project.
    #[test]
    fn authorization_is_never_a_forwarded_request_header() {
        for v in ALL {
            assert!(
                !v.request_headers().contains(&"authorization"),
                "{v:?} forwards the caller's Authorization header"
            );
            assert!(
                !v.request_headers().iter().any(|h| h.eq_ignore_ascii_case("cookie")),
                "{v:?} forwards cookies"
            );
        }
    }

    /// v1.30.0's conditional writes are an end-to-end protocol; the
    /// gateway sits in the middle of it. If `If-Match` is dropped the
    /// hub sees an unconditional write and answers 200, so the lost
    /// update is invisible from both ends.
    #[test]
    fn the_conditional_write_headers_survive_the_proxy() {
        assert!(Verb::Upload.request_headers().contains(&"if-match"));
        assert!(Verb::Delete.request_headers().contains(&"if-match"));
        assert!(Verb::Move.request_headers().contains(&"if-match"));
        assert!(RESPONSE_HEADERS.contains(&"etag"), "a caller cannot send If-Match it never received");
    }

    #[test]
    fn ranged_downloads_survive_the_proxy() {
        assert!(Verb::Download.request_headers().contains(&"range"));
        assert!(RESPONSE_HEADERS.contains(&"content-range"));
        assert!(RESPONSE_HEADERS.contains(&"accept-ranges"));
    }

    #[test]
    fn every_verb_maps_under_files_and_only_the_two_reads_are_non_mutating() {
        for v in ALL {
            assert!(v.upstream_path().starts_with("/files"), "{v:?}");
        }
        let muts: Vec<_> = ALL.iter().filter(|v| v.is_mutation()).collect();
        assert_eq!(muts.len(), 4, "List and Download are the only reads: {muts:?}");
    }

    /// A trailing slash on the published endpoint is a plausible
    /// operator edit and would otherwise produce `//files`, which some
    /// routers treat as a different path.
    #[test]
    fn a_trailing_slash_on_the_endpoint_is_absorbed() {
        assert_eq!(
            upstream_url("http://h:8080/", Verb::List, ""),
            "http://h:8080/files"
        );
    }
}
