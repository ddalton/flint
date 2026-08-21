//! The bearer token the file API checks, re-read while the hub runs.
//!
//! The token used to be resolved once at startup and captured into the
//! route table, which made a rotation reach a running hub only through a
//! pod restart — and a hub restart stalls every mounted client on that
//! share. With a `hard` mount, in-flight I/O blocks in uninterruptible
//! sleep until the new pod answers: the pod's termination grace, then
//! the NFS grace period while clients reclaim. Nothing is lost and
//! nothing must be remounted, because the `state.db` on the PVC keeps
//! the `serverId` stable, so it is a stall rather than a wedge. It is
//! still an NFS availability event, and paying one to change an HTTP
//! credential couples the hub's two doors in exactly the way the rest of
//! the design keeps them apart.
//!
//! So the file-backed token is re-read on a timer instead. A rotation is
//! then a Secret edit and nothing else: no bounce, no stalled mounts,
//! and no fleet split between the hubs that happened to restart and the
//! hubs that did not.
//!
//! Four things this deliberately does NOT do.
//!
//! - **It never promotes "no token" into "serve it open".** Whether the
//!   routes exist at all is decided once, at boot, by
//!   [`FileApiConfig::resolve_token`](crate::pnfs::config::FileApiConfig::resolve_token):
//!   with no token there is no `TokenSource` and no route table. This
//!   type only ever answers "what is the current token", never "is
//!   there one".
//! - **It never falls back on a bad read.** An emptied or unreadable
//!   file keeps the last good value and logs. A projected Secret is
//!   updated by an atomic symlink swap, so a transient read is possible
//!   in principle; treating one as "the token is gone" would either open
//!   the surface or take it down, and both are worse than continuing to
//!   accept the credential the operator last set.
//! - **It polls rather than watches.** The kubelet updates a projected
//!   Secret by swapping the `..data` symlink, so an inotify watch on the
//!   file path stops firing after the first rotation — the inode it was
//!   watching is no longer the one being read. Watching the directory
//!   correctly is more machinery than a stat-and-read of a tmpfs file,
//!   and the kubelet's own sync of mounted Secrets (~1 minute) dominates
//!   end-to-end latency anyway, so a faster mechanism would buy nothing.
//! - **It cannot help an env-sourced token.** A process's environment is
//!   fixed at container start. `FLINT_FILE_API_TOKEN` therefore stays
//!   boot-time, and [`TokenSource::refresh`] is a no-op for it. Both the
//!   chart and the operator use the file, which is the path that matters.
//!
//! One constraint lives outside this file and has to stay true: the
//! Secret is mounted as a whole directory (`/etc/flint/api-token`), with
//! no `subPath`. A `subPath` mount is frozen at pod start, so "tidying"
//! the mount that way would silently return the hub to boot-time
//! behaviour with nothing here failing.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How often the file is re-read. Well inside the kubelet's own sync
/// interval, so the poll is never the thing a rotation waits on.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// The current bearer token, and where to re-read it from.
///
/// Cloneable by `Arc`: the route table holds one, the refresher task
/// holds another, and both see the same value.
pub struct TokenSource {
    /// `None` = the token came from the environment and cannot change
    /// under a running process.
    path: Option<PathBuf>,
    current: RwLock<Arc<str>>,
}

impl TokenSource {
    /// Build a source from the token already resolved at boot.
    ///
    /// `path` is `monitoring.fileApi.tokenFile` when it was the source,
    /// and `None` when the value came from the environment.
    pub fn new(initial: impl Into<Arc<str>>, path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            path,
            current: RwLock::new(initial.into()),
        })
    }

    /// A source that can never change — for tests, and for any caller
    /// that has a token but no file behind it.
    pub fn fixed(token: impl Into<Arc<str>>) -> Arc<Self> {
        Self::new(token, None)
    }

    /// The token to check this request against.
    pub fn current(&self) -> Arc<str> {
        // A poisoned lock would mean a panic inside `refresh`, which
        // holds the guard only across an assignment. Prefer the value to
        // taking the API down.
        match self.current.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// True when there is a file to re-read; false for env-sourced.
    pub fn is_refreshable(&self) -> bool {
        self.path.is_some()
    }

    /// Re-read the file, keeping the last good value on any failure.
    ///
    /// Returns true when the token actually changed, which is the only
    /// case worth logging: a rotation is a rare, operationally
    /// interesting event, and a poll that found no change is noise.
    pub fn refresh(&self) -> bool {
        let Some(path) = self.path.as_ref() else {
            return false;
        };
        let next = match std::fs::read_to_string(path) {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            Ok(_) => {
                tracing::warn!(
                    "file API token file {} is empty — keeping the previous token",
                    path.display()
                );
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    "file API token file {} unreadable: {} — keeping the previous token",
                    path.display(),
                    e
                );
                return false;
            }
        };

        if *self.current() == *next {
            return false;
        }
        // The value is never logged, here or anywhere else.
        tracing::info!(
            "🔑 file API token rotated from {} — new requests are checked against it",
            path.display()
        );
        match self.current.write() {
            Ok(mut g) => *g = next.into(),
            Err(p) => *p.into_inner() = next.into(),
        }
        true
    }
}

/// Poll the token file for the life of the process.
///
/// Spawned beside the status listener. Does nothing at all for an
/// env-sourced token, rather than waking every ten seconds to discover
/// there is no file.
pub fn spawn_refresher(source: Arc<TokenSource>) -> Option<tokio::task::JoinHandle<()>> {
    if !source.is_refreshable() {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        // The first tick fires immediately; the value is already current
        // at boot, so skip it rather than re-reading what we just read.
        tick.tick().await;
        loop {
            tick.tick().await;
            source.refresh();
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).expect("write token file");
    }

    #[test]
    fn a_rotation_is_picked_up_without_rebuilding_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write(&path, "first");

        let src = TokenSource::new("first", Some(path.clone()));
        assert_eq!(&*src.current(), "first");

        write(&path, "second");
        assert!(src.refresh(), "a changed file must report a rotation");
        assert_eq!(&*src.current(), "second");

        // The same content twice is not a rotation, so it does not log.
        assert!(!src.refresh());
    }

    #[test]
    fn trailing_whitespace_is_not_a_rotation() {
        // `kubectl create secret --from-literal` and an editor disagree
        // about the trailing newline; a token that gained one has not
        // changed, and reporting it as a rotation would log on every
        // deploy.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write(&path, "same\n");

        let src = TokenSource::new("same", Some(path.clone()));
        assert!(!src.refresh());
        assert_eq!(&*src.current(), "same");
    }

    #[test]
    fn an_emptied_file_keeps_the_last_good_token() {
        // Fails closed to the previous credential, never open. An empty
        // read is far more likely to be a mid-rotation artefact than an
        // instruction to stop authenticating.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write(&path, "good");

        let src = TokenSource::new("good", Some(path.clone()));
        write(&path, "   ");
        assert!(!src.refresh());
        assert_eq!(&*src.current(), "good");
    }

    #[test]
    fn an_unreadable_file_keeps_the_last_good_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        write(&path, "good");

        let src = TokenSource::new("good", Some(path.clone()));
        std::fs::remove_file(&path).unwrap();
        assert!(!src.refresh());
        assert_eq!(&*src.current(), "good");
    }

    #[test]
    fn an_env_sourced_token_never_refreshes() {
        // Not a limitation worth working around: a process's environment
        // is fixed at container start, so there is nothing to re-read.
        let src = TokenSource::fixed("from-env");
        assert!(!src.is_refreshable());
        assert!(!src.refresh());
        assert_eq!(&*src.current(), "from-env");
    }
}
