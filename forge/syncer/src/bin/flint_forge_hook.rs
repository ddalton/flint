//! `flint-forge-hook` — the two server-side hooks as a small binary
//! without the AWS SDK, for the tests and the local rigs. The code is
//! `flint_forge::hook`; the git image installs `flint-forge-syncer`
//! under the hook names instead, so the hook in a pod is the same
//! build as the syncer it talks to.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(role) = flint_forge::hook::role_of(&args) else {
        eprintln!(
            "flint-forge: this binary is `pre-receive` or `proc-receive`, invoked as {:?}",
            args.first().map(String::as_str).unwrap_or("")
        );
        std::process::exit(2);
    };
    std::process::exit(flint_forge::hook::run_hook(role));
}
