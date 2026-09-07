//! Every git invocation the syncer makes, in one place.
//!
//! Forge writes no git internals: the object database, the ref store,
//! the merge and the packing are git's, run as subprocesses against a
//! bare repository. What this module adds is the discipline of naming
//! each invocation once, with the reason it is spelled the way it is —
//! several of these were wrong in the design's first draft precisely
//! because they read as obvious.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{ForgeError, ForgeResult};

/// The all-zero object id, in a command from `receive-pack`, means
/// "this ref did not exist" (as an old-oid) or "delete it" (as a new
/// one). Length varies with the repository's hash algorithm, so the
/// test is "all zeros", never a fixed 40.
pub fn is_zero(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|b| b == b'0')
}

pub fn zero_oid(len: usize) -> String {
    "0".repeat(len)
}

/// A completed `git` run whose exit status the caller judges. Several
/// git commands use a non-zero status to report a RESULT rather than a
/// failure (`merge-base --is-ancestor`, `merge-tree` on conflict), so
/// the runner never turns status into an error on its own.
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

#[derive(Debug, Clone)]
pub struct Git {
    pub repo: PathBuf,
}

impl Git {
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Git { repo: repo.into() }
    }

    /// Run git in the repository, with `stdin` fed to it if given.
    ///
    /// `GIT_CONFIG_NOSYSTEM` and an empty `HOME` keep a developer's or
    /// an image's global config out of the server's decisions: a
    /// `[merge] tool` or an `alias` inherited from the host would make
    /// the syncer's behaviour depend on where it happens to run.
    pub async fn run_in(
        &self,
        dir: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> ForgeResult<Output> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", "/nonexistent")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            ForgeError::Git(format!("cannot exec git {}: {e}", args.first().unwrap_or(&"")))
        })?;
        if let Some(bytes) = stdin {
            let mut sink = child.stdin.take().expect("piped");
            sink.write_all(bytes).await?;
            sink.shutdown().await?;
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub async fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> ForgeResult<Output> {
        let repo = self.repo.clone();
        self.run_in(&repo, args, stdin).await
    }

    /// Run in the repository with extra environment. `GIT_INDEX_FILE`
    /// is the one that matters: the export keeps its own index beside
    /// its scratch tree, so materialising a ref never touches the bare
    /// repository's own index or its HEAD.
    pub async fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> ForgeResult<Output> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", "/nonexistent")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().await?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Run and require success — for the invocations where a non-zero
    /// status genuinely is a failure.
    pub async fn must(&self, args: &[&str], stdin: Option<&[u8]>) -> ForgeResult<String> {
        let out = self.run(args, stdin).await?;
        if out.ok() {
            Ok(out.stdout)
        } else {
            Err(ForgeError::Git(format!(
                "git {} exited {}: {}",
                args.join(" "),
                out.status,
                out.stderr.trim()
            )))
        }
    }

    /// Create the bare repository and set the posture forge depends on.
    ///
    /// `procReceiveRefs = refs/` is the load-bearing one: it routes
    /// EVERY ref under `refs/` through `proc-receive`, which is what
    /// lets the syncer be the only decider. It also means git itself no
    /// longer checks old-oid or `denyNonFastForwards` for those
    /// commands — the syncer's step 2 is not an extra check, it is the
    /// only one.
    pub async fn init_bare(
        &self,
        default_branch: &str,
        hooks_path: Option<&str>,
    ) -> ForgeResult<()> {
        std::fs::create_dir_all(&self.repo)?;
        let repo = self.repo.clone();
        let out = self.run_in(&repo, &["rev-parse", "--git-dir"], None).await?;
        if !out.ok() {
            let parent = self.repo.parent().unwrap_or(Path::new("."));
            let name = self
                .repo
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| ForgeError::State("repo path has no final component".into()))?;
            let branch = default_branch.trim_start_matches("refs/heads/");
            let o = self
                .run_in(
                    parent,
                    &["init", "--bare", "--quiet", &format!("--initial-branch={branch}"), name],
                    None,
                )
                .await?;
            if !o.ok() {
                return Err(ForgeError::Git(format!("git init --bare: {}", o.stderr.trim())));
            }
        }
        for (k, v) in [
            // Every ref decision is the syncer's.
            ("receive.procReceiveRefs", "refs/"),
            // A push is always a pack, never loose objects: the unit
            // the syncer uploads is the unit git wrote.
            ("receive.unpackLimit", "1"),
            // git's detached auto-gc is a second, unowned writer of
            // objects/pack/ — it can delete a pack mid-upload and its
            // consolidated pack would have to be uploaded before the
            // next push could be acknowledged (design §10).
            ("receive.autogc", "false"),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
            // `-o strategy=…` on a refs/for push.
            ("receive.advertisePushOptions", "true"),
            // The base pack's bitmap is written by `pack-objects
            // --write-bitmap-index` (fold.rs); `repack` never runs, so
            // `repack.writeBitmaps` would decide nothing. Objects above
            // this size skip the delta search in every `pack-objects`,
            // `upload-pack`'s included: a 256 MiB blob tier cost 17 CPU-s
            // per clone at git's 512m default and 0.6 s at 1m
            // (compaction-tiers design §6, gate G3).
            ("core.bigFileThreshold", "1m"),
            // A malformed object must be refused at the door, not
            // uploaded and discovered at restore.
            ("receive.fsckObjects", "true"),
            // While the hooks wait for the bucket, receive-pack sends
            // an empty sideband packet whenever the hooks have been
            // quiet this long, and that packet is what keeps every
            // party between here and the client from reading the wait
            // as a dead connection (the scale drill's 40 GiB push
            // waited 8 minutes in proc-receive). git's default is the
            // same 5 s; it is set here so the guarantee does not rest
            // on a default.
            ("receive.keepAlive", "5"),
            ("core.logAllRefUpdates", "true"),
        ] {
            self.must(&["config", k, v], None).await?;
        }
        if let Some(path) = hooks_path.filter(|p| !p.is_empty()) {
            self.must(&["config", "core.hooksPath", path], None).await?;
        }
        Ok(())
    }

    /// Every ref, as the local repository has it.
    pub async fn refs(&self) -> ForgeResult<BTreeMap<String, String>> {
        let out = self
            .must(&["for-each-ref", "--format=%(objectname) %(refname)"], None)
            .await?;
        let mut map = BTreeMap::new();
        for line in out.lines() {
            if let Some((oid, name)) = line.split_once(' ') {
                map.insert(name.to_string(), oid.to_string());
            }
        }
        Ok(map)
    }

    /// One ref, or None if it does not exist.
    pub async fn ref_oid(&self, name: &str) -> ForgeResult<Option<String>> {
        let out = self.run(&["rev-parse", "--verify", "--quiet", name], None).await?;
        let oid = out.stdout.trim();
        Ok(if out.ok() && !oid.is_empty() { Some(oid.to_string()) } else { None })
    }

    /// Apply ref updates as ONE transaction. `update-ref --stdin`
    /// prepares every command before committing any, so either all the
    /// refs in a batch move or none do — which is what makes the
    /// reports the syncer sends afterwards true of the whole batch.
    pub async fn update_refs(&self, cmds: &[RefUpdate]) -> ForgeResult<()> {
        if cmds.is_empty() {
            return Ok(());
        }
        let mut script = String::new();
        for c in cmds {
            if is_zero(&c.new_oid) {
                script.push_str(&format!("delete {} {}\n", c.name, c.old_oid));
            } else if is_zero(&c.old_oid) {
                script.push_str(&format!("create {} {}\n", c.name, c.new_oid));
            } else {
                script.push_str(&format!("update {} {} {}\n", c.name, c.new_oid, c.old_oid));
            }
        }
        let out = self.run(&["update-ref", "--stdin"], Some(script.as_bytes())).await?;
        if out.ok() {
            Ok(())
        } else {
            Err(ForgeError::Git(format!(
                "update-ref transaction refused: {}",
                out.stderr.trim()
            )))
        }
    }

    /// Is `old` an ancestor of `new`? The fast-forward test, which git
    /// no longer performs for us under `proc-receive`.
    pub async fn is_ancestor(&self, old: &str, new: &str) -> ForgeResult<bool> {
        let out = self.run(&["merge-base", "--is-ancestor", old, new], None).await?;
        match out.status {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(ForgeError::Git(format!("merge-base --is-ancestor: {}", out.stderr.trim()))),
        }
    }

    pub async fn has_object(&self, oid: &str) -> ForgeResult<bool> {
        let out = self.run(&["cat-file", "-e", &format!("{oid}^{{object}}")], None).await?;
        Ok(out.ok())
    }

    /// Pack names present locally, e.g. `pack-<sha>.pack` — only those
    /// whose index is there too.
    ///
    /// A pack without its `.idx` is invisible to git and must be
    /// invisible here. `receive-pack` migrates a push's quarantine in
    /// the order `.keep`, `.pack`, `.rev`, `.idx` (git's `tmp-objdir.c`,
    /// `pack_copy_priority`), so while one push's batch is in step 4 a
    /// concurrent push's pack can be on disk with its index a rename
    /// away. Listing it then would upload and name a pack with no
    /// index; the index is never uploaded later, because a pack the
    /// snapshot already names is skipped; and a restore of that
    /// snapshot installs refs into objects git cannot see, which is a
    /// refusal and unrecoverable. The index is the last file to land,
    /// so its presence is the proof the pack is complete.
    pub fn local_packs(&self) -> ForgeResult<Vec<String>> {
        let dir = self.repo.join("objects/pack");
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in rd {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("pack-") && name.ends_with(".pack")) {
                continue;
            }
            let idx = format!("{}.idx", name.trim_end_matches(".pack"));
            if !dir.join(&idx).exists() {
                continue;
            }
            out.push(name);
        }
        out.sort();
        Ok(out)
    }

    /// The files that must travel with a pack for the restored
    /// repository to be clone-ready without a local repack: the index
    /// always, the bitmap and its reverse index when the pack carries
    /// them (§8 — uploading the bitmap is what saves 42 s and 125
    /// CPU-s on a 1 GiB corpus at restore).
    pub fn pack_siblings(&self, pack: &str) -> Vec<String> {
        let stem = pack.trim_end_matches(".pack");
        let dir = self.repo.join("objects/pack");
        let mut v = vec![pack.to_string()];
        for ext in [".idx", ".bitmap", ".rev"] {
            let name = format!("{stem}{ext}");
            if dir.join(&name).exists() {
                v.push(name);
            }
        }
        v
    }

    pub fn pack_path(&self, name: &str) -> PathBuf {
        self.repo.join("objects/pack").join(name)
    }

    /// Pack the objects reachable from `tips` but not from `excludes`,
    /// into `objects/pack/`, and return the new pack's name.
    ///
    /// This is what makes a server-side merge durable. `merge-tree
    /// --write-tree` and `commit-tree` write LOOSE objects, and a sync
    /// that uploaded only the packs a push brought would acknowledge a
    /// merge whose commit and tree are in no pack at all — the restore
    /// would fail `fsck` on a ref pointing at nothing (design §4, §6).
    ///
    /// `None` means the pack would have been empty, which is not an
    /// error: a merge that changed nothing new (a fast-forward
    /// resolution) creates no objects.
    pub async fn pack_new_objects(
        &self,
        tips: &[String],
        excludes: &[String],
    ) -> ForgeResult<Option<String>> {
        let mut revs = String::new();
        for t in tips {
            revs.push_str(t);
            revs.push('\n');
        }
        for e in excludes {
            if !is_zero(e) {
                revs.push('^');
                revs.push_str(e);
                revs.push('\n');
            }
        }
        let base = self.repo.join("objects/pack/pack");
        let base = base.to_string_lossy().into_owned();
        let out = self
            .run(
                &["pack-objects", "--revs", "--delta-base-offset", "--non-empty", "-q", &base],
                Some(revs.as_bytes()),
            )
            .await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!("pack-objects: {}", out.stderr.trim())));
        }
        let hash = out.stdout.trim();
        if hash.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!("pack-{hash}.pack")))
    }

    /// The CONTROL rule (X18, `fold_factor == 0`): consolidate into one
    /// pack with a bitmap, dropping what the new pack supersedes. Run
    /// between batches under the writer lock — never by git itself
    /// (§10). Kept until the tiers' measurement has run.
    pub async fn repack(&self) -> ForgeResult<()> {
        self.must(&["repack", "-a", "-d", "-b", "-q"], None).await?;
        Ok(())
    }

    /// A tier fold: every object of `inputs`, reachable or not, into
    /// one pack at `out_base` (a path prefix OUTSIDE `objects/pack`, so
    /// git never scans the result before it is durable). Existing
    /// deltas are reused and the window is off: a fold is a roll-up,
    /// not a recompression — the base rebuild keeps the search.
    /// Returns the pack's file name, or `None` for an empty result.
    pub async fn pack_fold(
        &self,
        inputs: &[String],
        out_base: &Path,
        threads: usize,
    ) -> ForgeResult<Option<String>> {
        let mut stdin = String::new();
        for p in inputs {
            stdin.push_str(p);
            stdin.push('\n');
        }
        let threads = threads.to_string();
        let base = out_base.to_string_lossy().into_owned();
        let out = self
            .run(
                &[
                    "-c", "pack.window=0", "-c", &format!("pack.threads={threads}"),
                    "pack-objects", "--stdin-packs", "--delta-base-offset", "--non-empty", "-q", &base,
                ],
                Some(stdin.as_bytes()),
            )
            .await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!("pack-objects --stdin-packs: {}", out.stderr.trim())));
        }
        Ok(pack_name_of(&out.stdout))
    }

    /// The base rebuild: everything reachable from every ref, with the
    /// bitmap, into one pack at `out_base`. The only place unreachable
    /// objects are dropped. `--all` reads stdin to EOF, which `run`
    /// gives it as `/dev/null`.
    pub async fn pack_base(&self, out_base: &Path, threads: usize) -> ForgeResult<Option<String>> {
        let threads = threads.to_string();
        let base = out_base.to_string_lossy().into_owned();
        let out = self
            .run(
                &[
                    "-c", &format!("pack.threads={threads}"),
                    "pack-objects", "--all", "--indexed-objects", "--write-bitmap-index",
                    "--delta-base-offset", "--non-empty", "-q", &base,
                ],
                None,
            )
            .await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!("pack-objects --all: {}", out.stderr.trim())));
        }
        Ok(pack_name_of(&out.stdout))
    }

    /// Before a base rebuild: the reflog would keep what the rebuild
    /// drops, and a warm restart's proof walks reflogs unless told not
    /// to. Retention belongs in the bucket (X15), not in an emptyDir.
    pub async fn reflog_expire_all(&self) -> ForgeResult<()> {
        self.must(&["reflog", "expire", "--expire=now", "--all"], None).await?;
        Ok(())
    }

    /// The restore's proof that the packs the bucket handed back
    /// actually contain what the snapshot's refs name. `--no-reflogs`:
    /// what it proves is the snapshot's refs, which have no reflog, and
    /// a warm restart's reflog may name what a base rebuild dropped.
    pub async fn fsck_connectivity(&self) -> ForgeResult<()> {
        let out = self
            .run(&["fsck", "--connectivity-only", "--no-reflogs", "--no-progress"], None)
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(ForgeError::Refused(format!(
                "restored repository fails fsck --connectivity-only: {}",
                out.stderr.trim()
            )))
        }
    }

    /// The INCREMENTAL proof (`follow.rs`): every object reachable
    /// from `tips` and not from `known` must be present and readable.
    ///
    /// `fsck --connectivity-only` walks everything the refs reach, and
    /// on a warm restart nearly all of that was proved by this same
    /// process minutes ago. `rev-list --objects` over the tips that
    /// moved, with the previously proved tips as the uninteresting
    /// side, walks the delta and the boundary — and fails the same way
    /// fsck does, by refusing to read an object that is not there.
    ///
    /// `--quiet` suppresses the listing, not the traversal: git still
    /// opens every commit, tree and blob it walks, which is the whole
    /// of the proof.
    pub async fn prove_reachable(&self, tips: &[String], known: &[String]) -> ForgeResult<()> {
        if tips.is_empty() {
            return Ok(());
        }
        let mut stdin = String::new();
        for t in tips {
            stdin.push_str(t);
            stdin.push('\n');
        }
        for k in known {
            stdin.push('^');
            stdin.push_str(k);
            stdin.push('\n');
        }
        let out = self
            .run(&["rev-list", "--objects", "--quiet", "--stdin"], Some(stdin.as_bytes()))
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(ForgeError::Refused(format!(
                "the {} tip(s) this restore added are not connected in the packs it holds: {}",
                tips.len(),
                out.stderr.trim()
            )))
        }
    }

    /// A three-way merge with no worktree and no index.
    ///
    /// Exit 0 is a clean merge, 1 is a conflict, anything else is a
    /// failure. On a conflict git has ALREADY written the objects it
    /// built; they are unreachable and the next repack drops them, so
    /// the caller moves no ref and reports the paths.
    pub async fn merge_tree(
        &self,
        base: &str,
        head: &str,
        strategy: Option<&str>,
    ) -> ForgeResult<MergeOutcome> {
        let mut args: Vec<String> =
            vec!["merge-tree".into(), "--write-tree".into(), "--name-only".into()];
        if let Some(s) = strategy {
            // `-X ours|theirs` needs git >= 2.43; below it the option
            // is silently a different thing, which is why the floor is
            // asserted at start rather than discovered here.
            args.push(format!("-X{s}"));
        }
        args.push(base.to_string());
        args.push(head.to_string());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let out = self.run(&refs, None).await?;
        match out.status {
            0 => {
                let tree = out.stdout.lines().next().unwrap_or("").trim().to_string();
                if tree.is_empty() {
                    return Err(ForgeError::Git("merge-tree wrote no tree".into()));
                }
                Ok(MergeOutcome::Clean { tree })
            }
            1 => {
                // First line is the tree; then the conflicted paths,
                // then a blank line and informational messages.
                let mut lines = out.stdout.lines();
                let _tree = lines.next();
                let paths: Vec<String> = lines
                    .take_while(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .collect();
                Ok(MergeOutcome::Conflict { paths })
            }
            _ => Err(ForgeError::Git(format!("merge-tree: {}", out.stderr.trim()))),
        }
    }

    pub async fn commit_tree(
        &self,
        tree: &str,
        parents: &[String],
        message: &str,
        author: &str,
    ) -> ForgeResult<String> {
        let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
        for p in parents {
            args.push("-p".into());
            args.push(p.clone());
        }
        args.push("-m".into());
        args.push(message.to_string());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        // A merge the server performs is authored on the pusher's
        // behalf, and the identity comes from the door's verified
        // principal — never from the client's config. An EMPTY
        // principal is a deployment without a door, and git refuses an
        // empty ident outright ("fatal: empty ident name"), so the
        // fallback names the server rather than letting a
        // misconfiguration surface at the client as a git internal
        // error. Found by the end-to-end push test, where no door sets
        // `REMOTE_USER`.
        let author = if author.trim().is_empty() { "flint-forge" } else { author };
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            .args(&refs)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", "/nonexistent")
            .env("GIT_AUTHOR_NAME", author)
            .env("GIT_AUTHOR_EMAIL", format!("{author}@forge.chert.us"))
            .env("GIT_COMMITTER_NAME", "flint-forge")
            .env("GIT_COMMITTER_EMAIL", "forge@chert.us")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = cmd.output().await?;
        if !out.status.success() {
            return Err(ForgeError::Git(format!(
                "commit-tree: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub async fn symbolic_head(&self, target: &str) -> ForgeResult<()> {
        self.must(&["symbolic-ref", "HEAD", target], None).await?;
        Ok(())
    }

    pub async fn head_target(&self) -> ForgeResult<String> {
        Ok(self.must(&["symbolic-ref", "HEAD"], None).await?.trim().to_string())
    }

    /// git's own version, as (major, minor).
    ///
    /// Deliberately not run with `-C <repo>`: the floor is a fact about
    /// the BINARY, and the start-up order asks for it before the
    /// repository exists. Running it in the repository made a fresh
    /// server exit with "cannot change to …/repo.git" before it had
    /// ever created the directory it was complaining about — found by
    /// the end-to-end push test, which is the only one that starts the
    /// server the way the pod does.
    pub async fn version(&self) -> ForgeResult<(u32, u32)> {
        let mut cmd = Command::new("git");
        cmd.arg("--version")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", "/nonexistent")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let raw = cmd.output().await?;
        if !raw.status.success() {
            return Err(ForgeError::Git(format!(
                "git --version: {}",
                String::from_utf8_lossy(&raw.stderr).trim()
            )));
        }
        let out = String::from_utf8_lossy(&raw.stdout).into_owned();
        let v = out.split_whitespace().nth(2).unwrap_or("");
        let mut parts = v.split('.');
        let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok((major, minor))
    }
}

/// One ref movement, in the shape `receive-pack` hands it to
/// `proc-receive` and the shape `update-ref --stdin` takes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefUpdate {
    pub name: String,
    pub old_oid: String,
    pub new_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean { tree: String },
    Conflict { paths: Vec<String> },
}

/// `pack_siblings` for a pack that lives in `dir` rather than in the
/// repository — the fold's scratch directory.
pub fn siblings_in(dir: &Path, pack: &str) -> Vec<String> {
    let stem = pack.trim_end_matches(".pack");
    let mut v = vec![pack.to_string()];
    for ext in [".idx", ".bitmap", ".rev"] {
        let name = format!("{stem}{ext}");
        if dir.join(&name).exists() {
            v.push(name);
        }
    }
    v
}

/// `pack-objects` prints the new pack's hash on stdout, or nothing.
fn pack_name_of(stdout: &str) -> Option<String> {
    let hash = stdout.trim();
    if hash.is_empty() {
        None
    } else {
        Some(format!("pack-{hash}.pack"))
    }
}
