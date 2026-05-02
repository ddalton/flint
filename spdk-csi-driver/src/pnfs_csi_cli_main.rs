//! `pnfs-csi-cli` — a minimal command-line driver for the
//! `pnfs_csi::PnfsCsi` API. Used only by the end-to-end test harness
//! (`tests/lima/pnfs/csi-e2e.sh`) to exercise the same MDS gRPC verbs
//! the CSI driver uses, without spinning up a full Kubernetes cluster.
//!
//! Two subcommands:
//!
//!   pnfs-csi-cli create --endpoint host:port --volume-id ID --size-bytes N
//!     → prints `volume_context` as a JSON object to stdout, exits 0.
//!
//!   pnfs-csi-cli delete --endpoint host:port --volume-id ID
//!     → exits 0 on success.
//!
//! Errors go to stderr and surface as a non-zero exit code with the
//! `PnfsError` variant in the message, so shell scripts can grep on
//! it. JSON output is single-line so simple `jq -r` extraction works.

use std::process::ExitCode;
use spdk_csi_driver::pnfs_csi::{PnfsCsi, PnfsError};

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         pnfs-csi-cli create --endpoint <host:port> --volume-id <id> --size-bytes <n>\n  \
         pnfs-csi-cli delete --endpoint <host:port> --volume-id <id>"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }

    // Tiny hand-rolled flag parser — pulling in clap for two
    // subcommands and three flags is overkill for a test-only binary.
    let mut endpoint: Option<String> = None;
    let mut volume_id: Option<String> = None;
    let mut size_bytes: Option<u64> = None;
    let mut i = 2;
    while i < args.len() {
        let key = &args[i];
        let val = match args.get(i + 1) {
            Some(v) => v.clone(),
            None => usage(),
        };
        match key.as_str() {
            "--endpoint" => endpoint = Some(val),
            "--volume-id" => volume_id = Some(val),
            "--size-bytes" => size_bytes = val.parse::<u64>().ok(),
            _ => usage(),
        }
        i += 2;
    }
    let endpoint = endpoint.unwrap_or_else(|| usage());
    let volume_id = volume_id.unwrap_or_else(|| usage());

    let pnfs = PnfsCsi::new(&endpoint);

    let result: Result<(), PnfsError> = match args[1].as_str() {
        "create" => {
            let size = size_bytes.unwrap_or_else(|| usage());
            match pnfs.create_volume(&volume_id, size).await {
                Ok(ctx) => {
                    // Single-line JSON for easy `jq -r` consumption.
                    let pairs: Vec<String> = ctx.iter()
                        .map(|(k, v)| format!("{}:{}",
                            json_quote(k), json_quote(v)))
                        .collect();
                    println!("{{{}}}", pairs.join(","));
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "delete" => pnfs.delete_volume(&volume_id).await,
        _ => usage(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pnfs-csi-cli: {}", e);
            ExitCode::from(1)
        }
    }
}

/// Hand-rolled JSON string-quote — the values we emit are file paths,
/// hostnames, and small integers, none of which contain anything
/// fancier than `/` or `.`. Pulling in `serde_json` for one helper
/// would be overkill given pnfs_csi is otherwise serde-free.
fn json_quote(s: &str) -> String {
    let escaped: String = s.chars().flat_map(|c| match c {
        '"' => vec!['\\', '"'],
        '\\' => vec!['\\', '\\'],
        '\n' => vec!['\\', 'n'],
        '\r' => vec!['\\', 'r'],
        '\t' => vec!['\\', 't'],
        c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32).chars().collect(),
        c => vec![c],
    }).collect();
    format!("\"{}\"", escaped)
}
