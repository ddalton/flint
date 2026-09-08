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
        self.run_env_stdin(args, env, None).await
    }

    /// `run_env` with a body on stdin. The incremental proof needs
    /// both at once: the tips go in on stdin, and the object directory
    /// it is permitted to read comes in on the environment.
    pub async fn run_env_stdin(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: Option<&[u8]>,
    ) -> ForgeResult<Output> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", "/nonexistent")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
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
            // Partial clone. Without this `upload-pack` IGNORES a
            // client's `--filter`, silently, and serves a full clone
            // while git prints a warning the user does not read — which
            // is how `forge/e2e/gitqual` first passed the leg for it.
            // A blobless clone of a repository whose size is its blobs
            // is the difference between a working agent and a pod that
            // downloads a tier it will never open, and the objects it
            // then asks for on demand are served from the same local
            // repository every other read comes from.
            ("uploadpack.allowFilter", "true"),
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
    /// Fold every loose ref into `packed-refs`.
    ///
    /// forge sets `gc.auto=0` and never repacks, so every ref it has
    /// ever accepted stays a loose file for the life of the repository.
    /// `receive-pack` then walks all of them on EVERY push — for the
    /// advertisement, for the connectivity check, and once more through
    /// the quarantine's alternate — and so does `update-server-info`.
    /// Measured on a scratch repository at 8,002 refs: a lone one-ref
    /// push costs 683 ms with loose refs and 121 ms packed, warm; 1041
    /// vs 275 ms cold. The wire bytes are identical either way, so this
    /// is filesystem cost and nothing a client can see. `gc.auto` would
    /// never have rescued it: that counts loose OBJECTS, not refs.
    ///
    /// Storage only — no ref changes value, so nothing in the bucket,
    /// the snapshot, or a reader's view moves. Runs on the derived-files
    /// timer, never on a push's path.
    pub async fn pack_refs(&self) -> ForgeResult<()> {
        self.must(&["pack-refs", "--all"], None).await?;
        Ok(())
    }

    pub async fn reflog_expire_all(&self) -> ForgeResult<()> {
        self.must(&["reflog", "expire", "--expire=now", "--all"], None).await?;
        Ok(())
    }

    /// The proof that the packs **the snapshot names** contain what the
    /// snapshot's refs name — that is, that this repository can be
    /// restored from the bucket.
    ///
    /// Scoped deliberately, and this is the whole point of the method:
    /// the object directory is NOT the bucket. A fold's superseded
    /// inputs stay on disk for `fold_retain_secs` so a reader mid-clone
    /// keeps the pack it is streaming, while the ledger sweep is on its
    /// way to deleting them from the bucket. An unscoped walk therefore
    /// answers "is the disk coherent" and was being read as "can this
    /// be restored" — a repository could pass, serve for the whole
    /// retention window, and become unrestorable the moment the sweep
    /// ran. runcd (2026-09-07) came within one sweep of the difference,
    /// and the audit named it F2.
    ///
    /// `--no-reflogs`: what it proves is the snapshot's refs, which
    /// have no reflog, and a warm restart's reflog may name what a base
    /// rebuild dropped.
    pub async fn fsck_connectivity_over(&self, packs: &[String]) -> ForgeResult<()> {
        let odb = ScopedOdb::build(&self.repo, packs)?;
        let out = self
            .run_env(
                &["fsck", "--connectivity-only", "--no-reflogs", "--no-progress"],
                &[("GIT_OBJECT_DIRECTORY", odb.path.as_str())],
            )
            .await?;
        self.fsck_verdict(out, Some(packs.len()))
    }

    /// The same walk over every pack on disk, retained ones included.
    ///
    /// This answers a genuinely different question from
    /// `fsck_connectivity_over` — "is what this process is serving
    /// coherent", not "is the bucket restorable" — and it is not the
    /// restore's proof. Kept for the tests that assert the former.
    pub async fn fsck_connectivity_all(&self) -> ForgeResult<()> {
        let out = self
            .run(&["fsck", "--connectivity-only", "--no-reflogs", "--no-progress"], None)
            .await?;
        self.fsck_verdict(out, None)
    }

    fn fsck_verdict(&self, out: Output, scoped: Option<usize>) -> ForgeResult<()> {
        if out.ok() {
            return Ok(());
        }
        // `git fsck` puts what is WRONG on stdout ("missing commit <oid>",
        // "broken link from ... to ...") and only chatter on stderr
        // ("notice: HEAD points to an unborn branch"). Reporting stderr
        // alone named the wrong cause on runcd (2026-09-07): the refusal
        // blamed an unborn HEAD while the real fault was thirteen missing
        // commits, and an operator reading the log would have chased the
        // notice. Lead with the faults, and keep only the first few — a
        // broken repository can print thousands of lines.
        let faults: Vec<&str> = out
            .stdout
            .lines()
            .filter(|l| !l.starts_with("dangling ") && !l.trim().is_empty())
            .collect();
        let shown = faults.iter().take(6).cloned().collect::<Vec<_>>().join("; ");
        let more = faults.len().saturating_sub(6);
        let mut why = if faults.is_empty() {
            String::new()
        } else if more > 0 {
            format!("{shown} (and {more} more)")
        } else {
            shown
        };
        let notes = out.stderr.trim();
        if !notes.is_empty() {
            if !why.is_empty() {
                why.push_str(" | ");
            }
            why.push_str(notes);
        }
        let over = match scoped {
            // Naming the scope in the message matters: an operator who
            // reads "fails fsck" while `git fsck` in a shell on the same
            // pod passes will chase the wrong thing, and the difference
            // between the two is exactly the fault being reported.
            Some(n) => format!("over the {n} pack(s) the snapshot names"),
            None => "over every pack on disk".to_string(),
        };
        Err(ForgeError::Refused(format!(
            "restored repository fails fsck --connectivity-only {over}: {why}"
        )))
    }

    /// The object ids an index names, read from the `.idx` alone.
    ///
    /// `git show-index` reads an index on stdin and prints one
    /// `<offset> <oid> (<crc>)` line per object, so this costs the index
    /// and never the pack.
    pub async fn pack_object_ids(&self, idx: &Path) -> ForgeResult<Vec<String>> {
        let bytes = std::fs::read(idx)?;
        let out = self.run(&["show-index"], Some(&bytes)).await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!(
                "show-index {}: {}",
                idx.display(),
                out.stderr.trim()
            )));
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
            .collect())
    }

    /// Every object reachable from every ref, by object id.
    ///
    /// This is what a base rebuild is contracted to pack (`--all`), and
    /// so what its output must be checked against: a base MAY drop
    /// objects its inputs held — that is the point of it, and a rewind
    /// makes it happen — but it may never drop a REACHABLE one.
    pub async fn reachable_object_ids(&self) -> ForgeResult<Vec<String>> {
        let out = self.run(&["rev-list", "--objects", "--all"], None).await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!("rev-list --objects --all: {}", out.stderr.trim())));
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
            .filter(|s| s.len() >= 40)
            .collect())
    }

    /// Every object reachable from `tips`, by object id — what a base
    /// rebuild that read exactly those refs is contracted to have
    /// packed. Empty tips reach nothing.
    pub async fn reachable_from(&self, tips: &[String]) -> ForgeResult<Vec<String>> {
        if tips.is_empty() {
            return Ok(Vec::new());
        }
        let stdin = tips.join("\n") + "\n";
        let out = self.run(&["rev-list", "--objects", "--stdin"], Some(stdin.as_bytes())).await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!("rev-list --objects --stdin: {}", out.stderr.trim())));
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
            .filter(|s| s.len() >= 40)
            .collect())
    }

    /// Objects reachable from `now` and not from `then`: what ARRIVED
    /// between two readings of the refs. Small when the readings are
    /// close, which is the only way it is used.
    pub async fn objects_since(&self, now: &[String], then: &[String]) -> ForgeResult<Vec<String>> {
        if now.is_empty() {
            return Ok(Vec::new());
        }
        let mut stdin = String::new();
        for t in now {
            stdin.push_str(t);
            stdin.push('\n');
        }
        for t in then {
            stdin.push('^');
            stdin.push_str(t);
            stdin.push('\n');
        }
        let out = self.run(&["rev-list", "--objects", "--stdin"], Some(stdin.as_bytes())).await?;
        if !out.ok() {
            return Err(ForgeError::Git(format!("rev-list --objects --stdin (since): {}", out.stderr.trim())));
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
            .filter(|s| s.len() >= 40)
            .collect())
    }

    /// Every object in `want` must be named by `out_idx`. Returns the
    /// first that is not, or `None`.
    pub async fn pack_holds_all(
        &self,
        out_idx: &Path,
        want: &[String],
    ) -> ForgeResult<Option<String>> {
        let have: std::collections::HashSet<String> =
            self.pack_object_ids(out_idx).await?.into_iter().collect();
        Ok(want.iter().find(|o| !have.contains(*o)).cloned())
    }

    /// Every object the `inputs` indexes name must also be named by
    /// `out_idx`. Returns the first object that is not, or `None`.
    ///
    /// A fold rolls packs up and its commit then stops naming the
    /// inputs, so an output that does not cover them silently strands
    /// whatever only they held. `pack-objects` is trusted to preserve
    /// its inputs everywhere else in the design; on runcd (2026-09-07)
    /// that trust was misplaced, and the repository could not be
    /// restored afterwards. This is the check that was missing.
    pub async fn pack_covers(
        &self,
        out_idx: &Path,
        inputs: &[PathBuf],
    ) -> ForgeResult<Option<String>> {
        let have: std::collections::HashSet<String> =
            self.pack_object_ids(out_idx).await?.into_iter().collect();
        for idx in inputs {
            for oid in self.pack_object_ids(idx).await? {
                if !have.contains(&oid) {
                    return Ok(Some(oid));
                }
            }
        }
        Ok(None)
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
    /// Scoped to the packs the snapshot names, for the same reason
    /// `fsck_connectivity_over` is: the boundary this walk stops at is
    /// the previous proof's tips, and that proof is a statement about
    /// the bucket. Walking the disk instead would let a retained pack —
    /// one the sweep is about to delete — carry the delta.
    pub async fn prove_reachable_over(
        &self,
        packs: &[String],
        tips: &[String],
        known: &[String],
    ) -> ForgeResult<()> {
        if tips.is_empty() {
            return Ok(());
        }
        let odb = ScopedOdb::build(&self.repo, packs)?;
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
            .run_env_stdin(
                &["rev-list", "--objects", "--quiet", "--stdin"],
                &[("GIT_OBJECT_DIRECTORY", odb.path.as_str())],
                Some(stdin.as_bytes()),
            )
            .await?;
        if out.ok() {
            Ok(())
        } else {
            Err(ForgeError::Refused(format!(
                "the {} tip(s) this restore added are not connected in the {} pack(s) the \
                 snapshot names: {}",
                tips.len(),
                packs.len(),
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

    // ── The file API's plumbing (docs/plans/forge-file-api-design.md §4.2) ──
    //
    // A REST write is structurally the same shape as a server-side
    // merge: it invents objects the server was not handed, and the
    // batch packs and uploads them before any ref moves. What it needs
    // beyond the merge path is a tree edited BY PATH — which git gives
    // through a scratch index — and content that survives the trip,
    // which `Output` does not provide.

    /// Run git and keep stdout as BYTES.
    ///
    /// `Output::stdout` is `from_utf8_lossy`, which is right for oids
    /// and ref names and destroys everything else: a 1 KiB blob of all
    /// 256 byte values comes back through it with 512 replacement
    /// characters. No caller before the file API ever read content, so
    /// this is a second runner rather than a change to the first —
    /// `must()`'s error text still wants a `String`.
    pub async fn run_bytes(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: Option<&[u8]>,
    ) -> ForgeResult<OutputBytes> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", "/nonexistent")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        if let Some(bytes) = stdin {
            let mut sink = child.stdin.take().expect("stdin piped");
            sink.write_all(bytes).await?;
            sink.shutdown().await?;
        }
        let out = child.wait_with_output().await?;
        Ok(OutputBytes {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Write a blob and return its oid. Bytes in, oid out — the oid is
    /// ASCII, so the lossy runner is safe for the OUTPUT here even
    /// though it would not be for the input.
    pub async fn hash_object(&self, bytes: &[u8]) -> ForgeResult<String> {
        Ok(self
            .must(&["hash-object", "-w", "--stdin"], Some(bytes))
            .await?
            .trim()
            .to_string())
    }

    /// A blob's bytes, by oid. Callers resolve the path to an oid with
    /// [`Git::ls_tree`] first, which is also the only reliable
    /// existence check: `cat-file --batch-check` EXITS 0 on a missing
    /// path and reports it as text, and an absent submodule commit is
    /// indistinguishable from an absent path through it.
    pub async fn cat_blob(&self, oid: &str) -> ForgeResult<Vec<u8>> {
        let out = self.run_bytes(&["cat-file", "blob", oid], &[], None).await?;
        if out.status != 0 {
            return Err(ForgeError::Git(format!(
                "git cat-file blob {oid} exited {}: {}",
                out.status,
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Entries of `rev:path`, or of `rev` itself when `path` is empty.
    ///
    /// `-z` throughout because git ACCEPTS a path containing a newline
    /// (measured), which a line-oriented parse would split in half.
    /// `--format` rather than the default columns so the delimiter is
    /// ours and the size is present.
    ///
    /// An empty result means "no such path": git cannot store an empty
    /// directory, so there is no case where a real path lists as
    /// nothing.
    pub async fn ls_tree(
        &self,
        rev: &str,
        path: &str,
        recursive: bool,
    ) -> ForgeResult<Vec<TreeEntry>> {
        const FMT: &str = "--format=%(objectmode) %(objecttype) %(objectname) %(objectsize) %(path)";
        let spec = if path.is_empty() { rev.to_string() } else { format!("{rev}:{path}") };
        let mut args = vec!["ls-tree", "-z", FMT];
        if recursive {
            args.push("-r");
        }
        args.push(&spec);
        let out = self.run_bytes(&args, &[], None).await?;
        if out.status != 0 {
            // 128 here is "not a valid object name" — an absent path or
            // an unborn branch, not a failure of the server.
            return Ok(Vec::new());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut entries = Vec::new();
        for rec in text.split('\0') {
            if rec.is_empty() {
                continue;
            }
            // mode SP type SP oid SP size SP path — path may contain
            // spaces, so split only four times.
            let mut it = rec.splitn(5, ' ');
            let (Some(mode), Some(kind), Some(oid), Some(size), Some(p)) =
                (it.next(), it.next(), it.next(), it.next(), it.next())
            else {
                continue;
            };
            entries.push(TreeEntry {
                mode: mode.to_string(),
                kind: kind.to_string(),
                oid: oid.to_string(),
                // Trees and gitlinks report "-", not a number.
                size: size.trim().parse::<u64>().ok(),
                path: p.to_string(),
            });
        }
        Ok(entries)
    }

    /// One entry of `rev:path`, or `None`. The single-path form uses
    /// `-- <path>` rather than `rev:path` so that a directory answers
    /// with its own tree entry instead of its contents.
    pub async fn tree_entry(&self, rev: &str, path: &str) -> ForgeResult<Option<TreeEntry>> {
        const FMT: &str = "--format=%(objectmode) %(objecttype) %(objectname) %(objectsize) %(path)";
        let out = self
            .run_bytes(&["ls-tree", "-z", FMT, rev, "--", path], &[], None)
            .await?;
        if out.status != 0 {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(rec) = text.split('\0').find(|r| !r.is_empty()) else {
            return Ok(None);
        };
        let mut it = rec.splitn(5, ' ');
        let (Some(mode), Some(kind), Some(oid), Some(size), Some(p)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return Ok(None);
        };
        Ok(Some(TreeEntry {
            mode: mode.to_string(),
            kind: kind.to_string(),
            oid: oid.to_string(),
            size: size.trim().parse::<u64>().ok(),
            path: p.to_string(),
        }))
    }

    /// Apply `edits` to `parent_tree` and return the new tree's oid.
    ///
    /// A scratch index, never `mktree`: `mktree` validates almost
    /// nothing — it ACCEPTS an entry named `.git` (measured), and
    /// forge's own restore proof is `fsck --connectivity-only`, which
    /// does not flag it. `read-tree` + `update-index` gets git's
    /// `verify_path` for free, so `/abs`, `a/../b`, `.git/config`,
    /// `.GIT/config` and file-vs-directory collisions are all refused
    /// before anything is written.
    ///
    /// The index is per CALL. Sharing one across concurrent requests
    /// gives either a hard `index.lock` failure or a phantom write in
    /// which every racer commits everyone's edits (both measured).
    ///
    /// **What this does NOT guard: a file-over-directory collision.**
    /// `Set { path: "a/b" }` where `a/b/` is a directory succeeds with
    /// exit 0, no stderr, and **silently replaces the whole subtree** —
    /// every file under `a/b/` is gone from the result (measured;
    /// `--cacheinfo` refuses the same edit, `--index-info` does not).
    /// A caller that serves file writes MUST read the target's kind
    /// first and refuse, which the API layer does because it needs the
    /// kind for the ETag and the mode anyway.
    pub async fn build_tree(
        &self,
        parent_tree: Option<&str>,
        edits: &[IndexEdit],
    ) -> ForgeResult<String> {
        let idx = ScratchIndex::new(&self.repo)?;
        let env = [("GIT_INDEX_FILE", idx.path.as_str())];

        // git's intrinsic empty tree makes the unborn branch and the
        // ordinary case one code path: `read-tree` accepts it without
        // the object existing on disk. `read-tree HEAD` on a commitless
        // repository is a fatal error, which is the branch this avoids.
        let base = parent_tree.unwrap_or(EMPTY_TREE_OID);
        // `-c index.skipHash` drops the index file's trailing checksum.
        // The index is deleted before this function returns and is read
        // only by the git that wrote it, so the checksum protects
        // nothing; it is ~17% of the read-tree/write-tree pair at 50k
        // entries. An older git ignores the unknown key.
        let skip = ["-c", "index.skipHash=true"];
        let read = self
            .run_bytes(&[&skip[..], &["read-tree", base][..]].concat(), &env, None)
            .await?;
        if read.status != 0 {
            return Err(ForgeError::Git(format!(
                "git read-tree {base} exited {}: {}",
                read.status,
                read.stderr.trim()
            )));
        }

        if !edits.is_empty() {
            let mut script = Vec::new();
            for e in edits {
                let path = match e {
                    IndexEdit::Set { path, .. } | IndexEdit::Remove { path } => path,
                };
                validate_tree_path(path).map_err(|why| {
                    ForgeError::Refused(format!("{path}: {why}"))
                })?;
                match e {
                    IndexEdit::Set { path, mode, oid } => {
                        script.extend_from_slice(format!("{mode} {oid}\t{path}\0").as_bytes());
                    }
                    // Mode 0 is the delete. `--remove` and
                    // `--force-remove` are both "this operation must be
                    // run in a work tree" in a bare repository
                    // (measured), so this is the only spelling that
                    // works here.
                    IndexEdit::Remove { path } => {
                        script.extend_from_slice(
                            format!("0 {}\t{path}\0", zero_oid(40)).as_bytes(),
                        );
                    }
                }
            }
            let upd = self
                .run_bytes(
                    &[&skip[..], &["update-index", "-z", "--index-info"][..]].concat(),
                    &env,
                    Some(&script),
                )
                .await?;
            if upd.status != 0 {
                return Err(ForgeError::Refused(format!(
                    "the path was refused by git: {}",
                    upd.stderr.trim()
                )));
            }
            // The status is NOT the verdict. `--index-info` reports a
            // refused path as exit 0 with `Ignoring path <p>` and drops
            // the entry, after which `write-tree` returns a valid oid
            // for a tree that is missing the write. Anything this
            // module's own validator did not catch is caught here
            // rather than answered 200.
            if upd.stderr.contains("Ignoring path") {
                return Err(ForgeError::Refused(format!(
                    "git refused a path in this write: {}",
                    upd.stderr.trim()
                )));
            }
        }

        let wrote = self
            .run_bytes(&[&skip[..], &["write-tree"][..]].concat(), &env, None)
            .await?;
        if wrote.status != 0 {
            return Err(ForgeError::Git(format!(
                "git write-tree exited {}: {}",
                wrote.status,
                wrote.stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&wrote.stdout).trim().to_string())
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

/// Refuse a path git would silently drop, or that would escape the tree.
///
/// **This is load-bearing, and the reason is a measurement.**
/// `update-index --index-info` validates paths and then reports the
/// refusal as **exit 0 with `Ignoring path <p>` on stderr** — it drops
/// the entry and succeeds. `--cacheinfo` exits 0 as well, with
/// `error: Invalid path`. So git's own check exists but its STATUS
/// cannot be read as a verdict: a caller that trusted the exit code
/// would build a tree missing the file it was asked to write, and
/// `write-tree` would hand back a perfectly valid oid for it.
///
/// The rules mirror git's `verify_path`. `build_tree` also scans stderr
/// for `Ignoring path`, so a rule git has that this does not still
/// becomes an error rather than a silent omission.
pub fn validate_tree_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("the path is empty".into());
    }
    if path.starts_with('/') {
        return Err("the path is absolute".into());
    }
    if path.contains('\0') {
        return Err("the path contains a NUL".into());
    }
    // git ACCEPTS a newline in a path (measured). `--index-info -z`
    // would carry it, but nothing else in this system would survive it
    // — least of all a listing a browser renders.
    if path.contains('\n') || path.contains('\r') {
        return Err("the path contains a newline".into());
    }
    for part in path.split('/') {
        if part.is_empty() {
            return Err("the path has an empty component".into());
        }
        if part == "." || part == ".." {
            return Err("the path contains `.` or `..`".into());
        }
        // `.git` in any case, and the spellings git itself rejects on
        // case-insensitive and 8.3 filesystems. A tree entry under any
        // of these is a hook the next clone would run.
        let lower = part.to_ascii_lowercase();
        let squeezed = lower.trim_end_matches(['.', ' ']);
        if squeezed == ".git" || lower.starts_with("git~") {
            return Err("the path names a git directory".into());
        }
    }
    Ok(())
}

/// git's intrinsic empty tree. Known to every git without the object
/// existing on disk, which is what lets [`Git::build_tree`] treat an
/// unborn branch and an ordinary commit as one path. SHA-1's value —
/// a SHA-256 repository has a different one, so if forge ever moves,
/// take this from `git hash-object -t tree /dev/null` rather than
/// editing the constant.
pub const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A completed `git` run whose stdout is CONTENT rather than text.
/// See [`Git::run_bytes`] for why this exists alongside [`Output`].
pub struct OutputBytes {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// One entry of a git tree, as `ls-tree --format` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// `100644` file, `100755` executable, `120000` symlink,
    /// `160000` submodule (a gitlink), `040000` directory.
    pub mode: String,
    /// `blob`, `tree`, or `commit` — a gitlink reports `commit`.
    pub kind: String,
    pub oid: String,
    /// `None` for trees and gitlinks, which report `-` rather than a
    /// number. A blob's size is available WITHOUT reading the blob,
    /// which is what makes the API's size cap enforceable before any
    /// allocation.
    pub size: Option<u64>,
    pub path: String,
}

impl TreeEntry {
    /// The API's type name for this entry. A symlink is data, never
    /// something to follow; a gitlink names a commit that is not in
    /// this repository at all.
    pub fn kind_name(&self) -> &'static str {
        match self.mode.as_str() {
            "040000" | "40000" => "directory",
            "120000" => "symlink",
            "160000" => "submodule",
            "100755" => "executable",
            _ => "file",
        }
    }

    pub fn is_regular_file(&self) -> bool {
        matches!(self.mode.as_str(), "100644" | "100755")
    }
}

/// One path-level change to apply to a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEdit {
    /// Add `path`, or replace what is there, at `mode`.
    ///
    /// The mode is an INPUT, never an inheritance: `--index-info`
    /// silently demoted a `100755` entry to `100644` when the caller
    /// named the wrong one (measured). A content write must read the
    /// existing mode and pass it back.
    Set { path: String, mode: String, oid: String },
    /// Remove `path`. A rename is a `Remove` and a `Set` in one call.
    Remove { path: String },
}

/// A per-call index file, deleted on drop.
///
/// Named like [`ScopedOdb`] and for the same reason: two of these must
/// never collide, because a shared index gives either a hard
/// `index.lock` failure or a phantom write in which concurrent callers
/// each commit the others' edits.
struct ScratchIndex {
    dir: PathBuf,
    path: String,
}

impl ScratchIndex {
    fn new(repo: &Path) -> ForgeResult<Self> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Inside the repository so the index lands on the same
        // filesystem as the objects it names, but not under `objects/`,
        // where git reports unknown files as garbage.
        let dir = repo.join(format!("forge-index-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("index");
        Ok(ScratchIndex { path: file.to_string_lossy().into_owned(), dir })
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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

/// A scratch object directory holding hardlinks to exactly the packs a
/// proof is entitled to read, for the duration of that proof.
///
/// git reads objects from the repository's object directory, and that
/// directory is deliberately not the bucket: `fold::commit` leaves a
/// roll-up's superseded inputs on disk for `fold_retain_secs` so a
/// reader mid-clone keeps the pack it is streaming, and the ledger
/// sweep deletes them from the bucket on its own schedule. A proof
/// taken over the directory is true of the disk and says nothing about
/// what a restart could fetch. Pointing `GIT_OBJECT_DIRECTORY` at a
/// directory holding only the named packs makes the two questions the
/// same one again — refs still come from the real repository, since
/// only objects are scoped.
///
/// Hardlinks, so one costs no bytes and a concurrent `unlink_retained`
/// cannot pull a file out from under a running walk.
struct ScopedOdb {
    dir: PathBuf,
    path: String,
}

impl ScopedOdb {
    fn build(repo: &Path, packs: &[String]) -> ForgeResult<Self> {
        // Unique per proof: the serving loop's checkpoint and a warm
        // pass can be walking at the same time, and a shared directory
        // would have one proof's cleanup empty the other's odb.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Inside the bare repository, so the hardlinks land on the same
        // filesystem as objects/pack, but NOT under objects/ — git
        // reports unknown files there as garbage.
        let dir = repo.join(format!("forge-proof-{}-{n}", std::process::id()));
        let me = ScopedOdb {
            path: dir.to_string_lossy().into_owned(),
            dir,
        };
        std::fs::create_dir_all(me.dir.join("pack"))?;
        let src = repo.join("objects/pack");
        for p in packs {
            let stem = p.trim_end_matches(".pack");
            for ext in [".pack", ".idx"] {
                let file = format!("{stem}{ext}");
                std::fs::hard_link(src.join(&file), me.dir.join("pack").join(&file)).map_err(
                    |e| {
                        ForgeError::Refused(format!(
                            "the snapshot names {file}, which this repository does not hold: {e}"
                        ))
                    },
                )?;
            }
        }
        Ok(me)
    }
}

impl Drop for ScopedOdb {
    fn drop(&mut self) {
        // Hardlinks only; removing them frees nothing but the entries.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
