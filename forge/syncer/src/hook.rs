//! The two server-side hooks, dispatched on the name the binary was
//! invoked as — `pre-receive` and `proc-receive`, each a symlink.
//!
//! The code lives here, in the library, and TWO binaries carry it:
//! `flint-forge-hook`, the small one without the AWS SDK that the
//! tests and the local rigs spawn, and `flint-forge-syncer` itself,
//! which the git image installs as both hooks. In the pod the hook and
//! the syncer it talks to are then the same build by construction, so
//! the socket protocol between them cannot drift between two images
//! pinned by two strings (the tag-drift class the published-artifact
//! drill found).
//!
//! ## `pre-receive`: the policy, at the edge
//!
//! It sees every command, including the `refs/for/*` merge proposals,
//! applies the rendered policy against `REMOTE_USER`, and refuses the
//! whole push if any command is refused — which is git's semantics for
//! this hook and not a choice. Its refusal is the one the pusher reads,
//! so the message names the rule. It is not the guarantee: the syncer
//! applies the same document again, because a repository whose hooks
//! were misconfigured would otherwise accept a push to `main` from
//! anyone who could reach the door (see `policy`).
//!
//! ## `proc-receive`: a relay, and nothing else
//!
//! git spawns this once per push. It negotiates the `proc-receive`
//! version, reads the command list and the push options, hands them to
//! the syncer over the pod's Unix socket, waits, and writes back the
//! per-ref report it is given.
//!
//! It decides nothing, and that is the design's central correction
//! (§4). `receive-pack` serialises nothing between pushes, and with
//! `receive.procReceiveRefs` set it performs no old-oid check and no
//! `denyNonFastForwards` for the handed-off commands — so a hook that
//! decided anything would be deciding it concurrently with every other
//! push, against a ref nobody had checked.
//!
//! A syncer that cannot be reached, or that dies mid-batch, produces
//! `ng` for every ref. That is the correct answer and not a
//! degradation: a push forge cannot make durable is a push forge must
//! not acknowledge.

use std::io::{stdin, stdout, BufRead, Write};
use std::path::PathBuf;

use super::gitcmd::RefUpdate;
use super::pktline::{read_until_flush, write_flush, write_str};
use super::policy::{Policy, Verdict};
use super::uds::{ask, HookRequest, SOCKET_NAME};

/// The syncer's state directory, as a hook sees it. `GIT_DIR` is set by
/// `receive-pack` for every hook it runs, and is `.` with the cwd at
/// the repository root — which is how the default resolves in the pod
/// without anything being configured.
fn state_dir() -> PathBuf {
    let git_dir = std::env::var("GIT_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(git_dir).join("flint-forge")
}

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("FLINT_FORGE_SOCKET") {
        return PathBuf::from(p);
    }
    state_dir().join(SOCKET_NAME)
}

/// The hook names git invokes.
pub const ROLES: [&str; 2] = ["pre-receive", "proc-receive"];

/// Which hook this process is, if it is one: the final component of
/// argv[0] when git ran a symlink named for the hook, else an explicit
/// first argument, which is what the tests and a wrapper script use.
/// `None` means "not a hook", which for the syncer binary means "the
/// syncer".
pub fn role_of(args: &[String]) -> Option<&'static str> {
    let by_name = args.first().and_then(|a| a.rsplit('/').next());
    let explicit = args.get(1).map(|s| s.as_str());
    for cand in [by_name, explicit].into_iter().flatten() {
        if let Some(r) = ROLES.iter().find(|r| **r == cand) {
            return Some(r);
        }
    }
    None
}

/// Run the hook named `role` over this process's stdin/stdout and
/// return its exit status. stderr from a hook reaches the pushing
/// client, prefixed by git, so what is printed there says what failed
/// in terms the pusher can act on.
pub fn run_hook(role: &str) -> i32 {
    let result = match role {
        "pre-receive" => pre_receive(),
        "proc-receive" => run(),
        other => {
            eprintln!("flint-forge: a hook is `pre-receive` or `proc-receive`, not {other:?}");
            return 2;
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flint-forge: {e}");
            1
        }
    }
}

/// `pre-receive`: every command on stdin as `<old> <new> <ref>`, and an
/// exit status that accepts or refuses ALL of them.
fn pre_receive() -> std::io::Result<i32> {
    let policy = match Policy::load(&state_dir()) {
        Ok(Some(p)) => p,
        // No document is the pre-operator posture and is permissive by
        // design; an unreadable one is not, because a rendering bug
        // must never read as "no policy" (see `policy`).
        Ok(None) => return Ok(0),
        Err(e) => {
            eprintln!("flint-forge: {e}");
            return Ok(1);
        }
    };
    let principal = std::env::var("REMOTE_USER").unwrap_or_default();
    let mut refusals = Vec::new();
    let mut line = String::new();
    let mut input = stdin().lock();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let mut parts = line.split_whitespace();
        let (Some(_old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Verdict::Refuse(why) = policy.judge(&principal, name, new) {
            refusals.push(why);
        }
    }
    if refusals.is_empty() {
        return Ok(0);
    }
    // git prints these to the pusher verbatim. One line per rule, and
    // the whole push is refused: `pre-receive` has no per-ref verdict.
    for why in &refusals {
        eprintln!("flint-forge: {why}");
    }
    if refusals.len() > 1 {
        eprintln!("flint-forge: the push is refused as a whole; pre-receive has no per-ref answer");
    }
    Ok(1)
}

fn run() -> std::io::Result<i32> {
    let mut input = stdin().lock();
    let mut out = stdout().lock();

    // ── version and capability negotiation ───────────────────────────
    //
    // The server offers a version and its capabilities; we echo the
    // version and only what we actually implement. `push-options` is
    // the one that matters: without echoing it, receive-pack sends no
    // options and `-o strategy=theirs` would vanish silently.
    let hello = read_until_flush(&mut input)?;
    let offers_push_options = hello
        .iter()
        .any(|l| l.split('\0').nth(1).map(|caps| caps.split(' ').any(|c| c == "push-options")).unwrap_or(false));
    if offers_push_options {
        write_str(&mut out, "version=1\0push-options")?;
    } else {
        write_str(&mut out, "version=1\0")?;
    }
    write_flush(&mut out)?;
    out.flush()?;

    // ── the commands, then the options ───────────────────────────────
    let mut commands = Vec::new();
    for line in read_until_flush(&mut input)? {
        let mut parts = line.split(' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        commands.push(RefUpdate {
            name: name.to_string(),
            old_oid: old.to_string(),
            new_oid: new.to_string(),
        });
    }
    let options = if offers_push_options { read_until_flush(&mut input)? } else { Vec::new() };

    if commands.is_empty() {
        write_flush(&mut out)?;
        out.flush()?;
        return Ok(0);
    }

    let request = HookRequest {
        principal: std::env::var("REMOTE_USER").unwrap_or_default(),
        options,
        commands: commands.clone(),
    };

    let socket = socket_path();
    let response = match ask(&socket, &request) {
        Ok(r) => r,
        Err(e) => {
            // Every ref fails, with the reason on the ref rather than
            // only on stderr, so `git push` prints it per branch.
            for c in &commands {
                write_str(
                    &mut out,
                    &format!("ng {} the repository server is not accepting writes ({e})\n", c.name),
                )?;
            }
            write_flush(&mut out)?;
            out.flush()?;
            return Ok(0);
        }
    };

    for result in &response.results {
        match result {
            super::batch::CommandResult::Ok { name, alt_ref, old_oid, new_oid } => {
                write_str(&mut out, &format!("ok {name}\n"))?;
                if let Some(alt) = alt_ref {
                    // The client asked to update `refs/for/main`; what
                    // moved is `refs/heads/main`, and this is how git
                    // tells it so.
                    write_str(&mut out, &format!("option refname {alt}\n"))?;
                    if let (Some(old), Some(new)) = (old_oid, new_oid) {
                        write_str(&mut out, &format!("option old-oid {old}\n"))?;
                        write_str(&mut out, &format!("option new-oid {new}\n"))?;
                    }
                }
            }
            super::batch::CommandResult::Ng { name, reason } => {
                write_str(&mut out, &format!("ng {name} {reason}\n"))?;
            }
        }
    }
    write_flush(&mut out)?;
    out.flush()?;
    Ok(0)
}

#[cfg(test)]
mod role_tests {
    use super::role_of;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// git runs the symlink, so argv[0] names the hook; an explicit
    /// argument names it for a test; the syncer's own name is no hook.
    #[test]
    fn the_role_is_the_invoked_name_or_an_explicit_argument() {
        assert_eq!(role_of(&args(&["/usr/local/share/flint-forge/hooks/pre-receive"])), Some("pre-receive"));
        assert_eq!(role_of(&args(&["hooks/proc-receive"])), Some("proc-receive"));
        assert_eq!(role_of(&args(&["/usr/local/bin/flint-forge-hook", "proc-receive"])), Some("proc-receive"));
        assert_eq!(role_of(&args(&["/usr/local/bin/flint-forge-syncer"])), None);
        assert_eq!(role_of(&args(&["flint-forge-syncer", "--version"])), None);
        assert_eq!(role_of(&[]), None);
    }
}
