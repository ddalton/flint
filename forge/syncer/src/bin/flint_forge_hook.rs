//! The `proc-receive` hook: a relay, and nothing else.
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

use std::io::{stdin, stdout, Write};
use std::path::PathBuf;

use flint_forge::gitcmd::RefUpdate;
use flint_forge::pktline::{read_until_flush, write_flush, write_str};
use flint_forge::uds::{ask, HookRequest, SOCKET_NAME};

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("FLINT_FORGE_SOCKET") {
        return PathBuf::from(p);
    }
    let git_dir = std::env::var("GIT_DIR").unwrap_or_else(|_| ".".into());
    PathBuf::from(git_dir).join("flint-forge").join(SOCKET_NAME)
}

fn main() {
    if let Err(e) = run() {
        // stderr from a hook reaches the pushing client, prefixed by
        // git. Say what failed in terms the pusher can act on.
        eprintln!("flint-forge: {e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
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
        return Ok(());
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
            return Ok(());
        }
    };

    for result in &response.results {
        match result {
            flint_forge::batch::CommandResult::Ok { name, alt_ref, old_oid, new_oid } => {
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
            flint_forge::batch::CommandResult::Ng { name, reason } => {
                write_str(&mut out, &format!("ng {name} {reason}\n"))?;
            }
        }
    }
    write_flush(&mut out)?;
    out.flush()?;
    Ok(())
}
