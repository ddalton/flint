//! `pnfs-csi-cli` — a minimal command-line driver for the
//! `pnfs_csi::PnfsCsi` API. Used only by the end-to-end test harnesses
//! (`tests/lima/pnfs/csi-e2e.sh`, `tests/lima/pnfs/block-rig.sh`) to
//! exercise the same MDS gRPC verbs — and, for the block rig, the same
//! node-side session code — the CSI driver uses, without spinning up a
//! full Kubernetes cluster.
//!
//! Subcommands:
//!
//!   pnfs-csi-cli create --endpoint host:port --volume-id ID --size-bytes N
//!     → prints `volume_context` as a JSON object to stdout, exits 0.
//!
//!   pnfs-csi-cli delete --endpoint host:port --volume-id ID
//!     → exits 0 on success.
//!
//!   pnfs-csi-cli attach --endpoint host:port --volume-id ID --node NAME
//!     → AttachBlockNode only; prints the session coordinates as JSON.
//!
//!   pnfs-csi-cli detach --endpoint host:port --volume-id ID --node NAME
//!     → DetachBlockNode only.
//!
//!   pnfs-csi-cli stage --endpoint host:port --volume-id ID --node NAME
//!     → the PRODUCTION NodeStage path minus kubelet: AttachBlockNode,
//!       then `pnfs_block_session::ensure_session` (nvme connect as the
//!       MDS-admitted NQN, fast_io_fail backfill, §4a eui link). Prints
//!       `{"device":…,"hostNqn":…,"subnqn":…,"nguid":…}`. Run as root
//!       on the client node — this is what the block rig stages with,
//!       so the rig proves shipped code, not a bash reimplementation.
//!
//!   pnfs-csi-cli unstage --volume-id ID
//!     → the NodeUnstage inverse: derive the volume's subsystem NQN and
//!       NGUID, tear the session down (link first, then disconnect).
//!       No endpoint — teardown is node-local, like NodeUnstage itself.
//!
//!   pnfs-csi-cli reestablish
//!     → one pass of `pnfs_block_session::reestablish_sessions` (what
//!       the node agent runs on a timer): re-run ensure_session for
//!       every durable record whose kernel controller is gone. Prints
//!       `records=N repaired=N failed=N`. Node-local, no endpoint.
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
           [--stripe-size <bytes>] [--stripe-width <n>] [--dir-gid <gid>] [--dir-mode <octal>]\n  \
         pnfs-csi-cli delete --endpoint <host:port> --volume-id <id>\n  \
         pnfs-csi-cli attach --endpoint <host:port> --volume-id <id> --node <name>\n  \
         pnfs-csi-cli detach --endpoint <host:port> --volume-id <id> --node <name>\n  \
         pnfs-csi-cli stage --endpoint <host:port> --volume-id <id> --node <name>\n  \
         pnfs-csi-cli unstage --volume-id <id>\n  \
         pnfs-csi-cli block-status --endpoint <host:port>\n  \
         pnfs-csi-cli reestablish"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }

    // Tiny hand-rolled flag parser — pulling in clap for a test-only
    // binary with this few flags is overkill.
    let mut endpoint: Option<String> = None;
    let mut volume_id: Option<String> = None;
    let mut size_bytes: Option<u64> = None;
    let mut node: Option<String> = None;
    let mut opts = spdk_csi_driver::pnfs_csi::VolumeOptions::default();
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
            "--node" => node = Some(val),
            "--stripe-size" => opts.stripe_size = val.parse::<u64>().unwrap_or_else(|_| usage()),
            "--stripe-width" => opts.stripe_width = val.parse::<u32>().unwrap_or_else(|_| usage()),
            "--dir-gid" => opts.dir_gid = val.parse::<u32>().unwrap_or_else(|_| usage()),
            "--dir-mode" => {
                opts.dir_mode =
                    u32::from_str_radix(val.trim_start_matches("0o"), 8).unwrap_or_else(|_| usage())
            }
            _ => usage(),
        }
        i += 2;
    }
    if args[1] == "reestablish" {
        let (records, repaired, failed) =
            spdk_csi_driver::pnfs_block_session::reestablish_sessions().await;
        println!("records={records} repaired={repaired} failed={failed}");
        return if failed == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) };
    }
    // Volume-free, like `reestablish`: this asks about the SHARD, not
    // about one volume — a target serves every block volume it hosts,
    // which is exactly why the roller has to ask this way.
    if args[1] == "block-status" {
        let pnfs = PnfsCsi::new(endpoint.unwrap_or_else(|| usage()));
        return match pnfs.block_export_status().await {
            Ok(st) => {
                println!(
                    "export enabled={} node={} traddr={} initiators={}",
                    st.enabled,
                    if st.export_node.is_empty() { "-" } else { &st.export_node },
                    if st.export_traddr.is_empty() { "-" } else { &st.export_traddr },
                    st.initiators.len()
                );
                for i in &st.initiators {
                    println!(
                        "initiator volume={} node={} source={} nqn={}",
                        i.volume_id,
                        if i.node_name.is_empty() { "-" } else { &i.node_name },
                        i.source,
                        i.host_nqn
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("pnfs-csi-cli: {}", e);
                ExitCode::from(1)
            }
        };
    }
    let volume_id = volume_id.unwrap_or_else(|| usage());
    // Everything except unstage talks to the MDS.
    let need_endpoint = args[1] != "unstage";
    let pnfs = if need_endpoint {
        Some(PnfsCsi::new(endpoint.unwrap_or_else(|| usage())))
    } else {
        None
    };
    let need_node = matches!(args[1].as_str(), "attach" | "detach" | "stage");
    if need_node && node.is_none() {
        usage();
    }

    let result: Result<(), PnfsError> = match args[1].as_str() {
        "create" => {
            let size = size_bytes.unwrap_or_else(|| usage());
            match pnfs.unwrap().create_volume_with(&volume_id, size, &opts).await {
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
        "delete" => pnfs.unwrap().delete_volume(&volume_id).await,
        "attach" => {
            match pnfs.unwrap().attach_block_node(&volume_id, &node.unwrap()).await {
                Ok(a) => {
                    print_attach(&a, None);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "detach" => pnfs
            .unwrap()
            .detach_block_node(&volume_id, &node.unwrap())
            .await
            .map(|detail| println!("{}", detail)),
        "stage" => {
            // Kernel floor BEFORE the attach RPC — a node that can
            // never stage must not plant a durable attach row the MDS
            // would then hold open for it.
            match spdk_csi_driver::pnfs_block_session::kernel_block_layout_support() {
                Err(e) => Err(PnfsError::Mds(format!("kernel admission: {e}"))),
                Ok(()) => match pnfs.unwrap().attach_block_node(&volume_id, &node.unwrap()).await {
                    Ok(a) => {
                        match spdk_csi_driver::pnfs_block_session::ensure_session(&a).await {
                            Ok(dev) => {
                                print_attach(&a, Some(&dev));
                                Ok(())
                            }
                            Err(e) => Err(PnfsError::Mds(format!("session: {e}"))),
                        }
                    }
                    Err(e) => Err(e),
                },
            }
        }
        "unstage" => {
            // Same derivation NodeUnstage uses: bare volume (the rig
            // never shard-pins, but strip defensively) → subsystem NQN
            // + NGUID.
            let bare = spdk_csi_driver::pnfs_csi::parse_shard_suffix(&volume_id)
                .map(|(b, _)| b)
                .unwrap_or(&volume_id);
            let subnqn = spdk_csi_driver::identity::block_volume_export_nqn(bare);
            let (_uuid, nguid) = spdk_csi_driver::nvmeof_export::stable_ns_identity(bare);
            match spdk_csi_driver::pnfs_block_session::teardown_session(&subnqn, &nguid).await {
                Ok(detail) => {
                    println!("{}", detail);
                    Ok(())
                }
                Err(e) => Err(PnfsError::Mds(format!("teardown: {e}"))),
            }
        }
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

/// Print the attach answer (plus the staged device, when there is one)
/// as single-line JSON.
fn print_attach(a: &spdk_csi_driver::pnfs_csi::BlockAttach, device: Option<&str>) {
    let mut pairs = vec![
        format!("\"traddr\":{}", json_quote(&a.traddr)),
        format!("\"trsvcid\":{}", a.trsvcid),
        format!("\"subnqn\":{}", json_quote(&a.subnqn)),
        format!("\"nguid\":{}", json_quote(&a.nguid)),
        format!("\"hostNqn\":{}", json_quote(&a.host_nqn)),
    ];
    if let Some(dev) = device {
        pairs.push(format!("\"device\":{}", json_quote(dev)));
    }
    println!("{{{}}}", pairs.join(","));
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
