//! `flint-forge-credential`: a git credential helper that presents the
//! pod's own ServiceAccount token as the HTTP basic password.
//!
//! This is the whole of an agent's credential story. There is no key,
//! no secret mounted into the workspace, and nothing to rotate: the
//! kubelet keeps the projected token fresh on disk, the helper reads it
//! on every invocation, and the door exchanges it for an identity with
//! a `TokenReview`. An agent that is deleted loses its credential the
//! moment its token stops being renewed.
//!
//! Install it in the agent image with
//!
//! ```text
//! git config --global credential.https://forge.chert.us.helper \
//!     /usr/local/bin/flint-forge-credential
//! git config --global credential.https://forge.chert.us.username pod
//! ```
//!
//! Environment:
//!   FLINT_FORGE_TOKEN_FILE  projected token path
//!                           (default /var/run/secrets/forge.chert.us/token)
//!   FLINT_FORGE_USERNAME    basic username (default `pod`; the door
//!                           reads the password and ignores this, but
//!                           HTTP basic requires a field)
//!
//! The token must be projected with `audience: forge.chert.us`. A
//! default ServiceAccount token has the API server's audience and the
//! door refuses it — deliberately, since a token minted for one
//! audience being accepted by another is exactly the confused-deputy
//! shape audiences exist to prevent.

use std::io::{stdin, BufRead, Write};

const DEFAULT_TOKEN_FILE: &str = "/var/run/secrets/forge.chert.us/token";

fn main() {
    // git calls the helper as `<helper> <operation>` with the request
    // on stdin. `store` and `erase` are no-ops: there is nothing to
    // store — the credential is a file the kubelet owns — and nothing
    // to erase.
    let op = std::env::args().nth(1).unwrap_or_default();
    if op != "get" {
        // Drain stdin so git never sees a broken pipe on a helper that
        // simply had nothing to do.
        let _ = stdin().lock().lines().count();
        return;
    }
    let _ = stdin().lock().lines().count();

    let path =
        std::env::var("FLINT_FORGE_TOKEN_FILE").unwrap_or_else(|_| DEFAULT_TOKEN_FILE.to_string());
    let token = match std::fs::read_to_string(&path) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            eprintln!(
                "flint-forge-credential: cannot read the projected token at {path}: {e}. \
                 The pod needs a projected ServiceAccount token with audience forge.chert.us."
            );
            std::process::exit(1);
        }
    };
    if token.is_empty() {
        eprintln!("flint-forge-credential: the projected token at {path} is empty");
        std::process::exit(1);
    }
    let username = std::env::var("FLINT_FORGE_USERNAME").unwrap_or_else(|_| "pod".into());
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "username={username}");
    let _ = writeln!(out, "password={token}");
}
