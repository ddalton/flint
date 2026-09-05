//! Clone bundles: the lever that moves a storm off the server's NIC
//! (design §8).
//!
//! A thousand agents cloning one repository at once is 130 CPU-seconds
//! — sixteen on eight cores — and 43 GB from one pod's network
//! interface. **Egress binds long before CPU does**, so the lever that
//! matters is not making the server faster but taking it out of the
//! transfer: the syncer cuts a bundle, uploads it beside the packs, and
//! advertises a presigned URL. Clients fetch the bundle from the object
//! store and ask the server only for the remainder.
//!
//! Three conditions, each verified on git 2.50.1 and each easy to miss:
//!
//! - **The client must opt in.** `transfer.bundleURI` defaults to
//!   FALSE, so a stock client ignores the advertisement entirely. An
//!   agent image that does not set it makes this whole module inert,
//!   which is why the guide says so and why falsifier 8's control is
//!   the opt-in switched off rather than the advertisement removed.
//! - **Both sides need git ≥ 2.40.**
//! - **The session must be protocol v2**, which `http-backend` sees
//!   only if the door forwards `Git-Protocol`. A door that drops it
//!   degrades every clone to v0 and no bundle is ever advertised.
//!
//! And one rule of forge's own: like the export, a bundle NEVER CASes
//! the snapshot. It is stashed and the next batch's single CAS carries
//! it.

use std::path::PathBuf;

use super::gitcmd::Git;
use super::{packio, ForgeError, ForgeResult, Syncer};

/// The config id the advertisement is written under. Stable, so
/// re-signing overwrites the URL rather than accumulating entries —
/// upload-pack advertises every `bundle.<id>.uri` it finds, and a
/// client handed a stale one pays a failed fetch before falling back.
pub const BUNDLE_ID: &str = "full";

#[derive(Debug, Clone)]
pub struct BundleConfig {
    /// A floor between cuts. A bundle is a full copy of the repository
    /// — 0.26 s here, 5.8 s and 1.05 GB on a 1 GiB corpus, then a PUT
    /// of that size — so cutting one per push would spend more than
    /// the storm it saves.
    pub every_secs: u64,
    /// How long a presigned URL is good for. Short and re-signed
    /// rather than S3's seven-day maximum: the URL is a bearer token
    /// for that object, handed to every agent that asks for a clone.
    pub url_ttl_secs: u64,
}

impl Default for BundleConfig {
    fn default() -> Self {
        BundleConfig { every_secs: 3600, url_ttl_secs: 6 * 3600 }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// The tip the current bundle was cut at.
    pub tip: Option<String>,
    pub name: Option<String>,
    pub cut_unix: u64,
    pub signed_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Skip(String),
    Cut { tip: String },
}

pub fn plan(cfg: &BundleConfig, tip: Option<&str>, last: &Record, now: u64) -> Plan {
    let Some(tip) = tip else {
        return Plan::Skip("the default branch does not exist yet".into());
    };
    if last.tip.as_deref() == Some(tip) {
        return Plan::Skip(format!("the bundle is already at {tip}"));
    }
    if last.cut_unix > 0 && now.saturating_sub(last.cut_unix) < cfg.every_secs {
        return Plan::Skip(format!(
            "inside the {}s floor ({}s since the last cut)",
            cfg.every_secs,
            now.saturating_sub(last.cut_unix)
        ));
    }
    Plan::Cut { tip: tip.to_string() }
}

/// Does the presigned URL need refreshing?
///
/// At half the TTL, so a client that takes the advertisement and then
/// takes its time still has a URL that resolves. Signing is local
/// computation — no request — so the only cost of being early is a
/// config write.
pub fn needs_resign(cfg: &BundleConfig, last: &Record, now: u64) -> bool {
    last.name.is_some() && now.saturating_sub(last.signed_unix) >= cfg.url_ttl_secs / 2
}

fn record_path(sc: &Syncer) -> PathBuf {
    sc.cfg.state_dir.join("bundle.json")
}

pub fn load_record(sc: &Syncer) -> Record {
    std::fs::read(record_path(sc))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_record(sc: &Syncer, r: &Record) -> ForgeResult<()> {
    std::fs::create_dir_all(&sc.cfg.state_dir)?;
    let body = serde_json::to_vec_pretty(r)
        .map_err(|e| ForgeError::State(format!("bundle record will not serialise: {e}")))?;
    std::fs::write(record_path(sc), body)?;
    Ok(())
}

/// Write the advertisement into the repository's config.
///
/// `uploadpack.advertiseBundleURIs` and the `bundle.*` section are what
/// upload-pack reads; they are undocumented in git-config(1) at 2.50.1
/// and were confirmed by running a clone against them.
pub async fn advertise(git: &Git, url: &str) -> ForgeResult<()> {
    for (k, v) in [
        ("uploadpack.advertiseBundleURIs", "true"),
        ("bundle.version", "1"),
        // `all` — there is one bundle and a client needs it. `any`
        // would tell a client that any single listed bundle suffices,
        // which is true only once there are several.
        ("bundle.mode", "all"),
    ] {
        git.must(&["config", k, v], None).await?;
    }
    git.must(&["config", &format!("bundle.{BUNDLE_ID}.uri"), url], None).await?;
    Ok(())
}

/// Stop advertising. Used when the bundle the config names has been
/// swept, so a client is never handed a URL that 404s.
pub async fn withdraw(git: &Git) -> ForgeResult<()> {
    let out = git
        .run(&["config", "--unset", &format!("bundle.{BUNDLE_ID}.uri")], None)
        .await?;
    // Exit 5 is "the key does not exist", which is the state we want.
    if !out.ok() && out.status != 5 {
        return Err(ForgeError::Git(format!("withdrawing the bundle URI: {}", out.stderr.trim())));
    }
    git.must(&["config", "uploadpack.advertiseBundleURIs", "false"], None).await?;
    Ok(())
}

/// Cut, upload and advertise, if one is due. Returns the bundle's
/// object name, which the caller stashes for the NEXT snapshot CAS.
pub async fn maybe_run(
    sc: &mut Syncer,
    cfg: &BundleConfig,
    now: u64,
) -> ForgeResult<Option<String>> {
    sc.check_fence()?;
    let branch = format!("refs/heads/{}", sc.cfg.default_branch.trim_start_matches("refs/heads/"));
    let tip = sc.git.ref_oid(&branch).await?;
    let mut record = load_record(sc);

    match plan(cfg, tip.as_deref(), &record, now) {
        Plan::Skip(why) => {
            if needs_resign(cfg, &record, now) {
                if let Some(name) = record.name.clone() {
                    let url = sc.store.presign_get(&sc.cfg.bundle_key(&name), cfg.url_ttl_secs).await?;
                    advertise(&sc.git, &url).await?;
                    record.signed_unix = now;
                    save_record(sc, &record)?;
                    eprintln!("flint-forge: bundle URL re-signed for another {}s", cfg.url_ttl_secs);
                }
            } else {
                eprintln!("flint-forge: bundle skipped — {why}");
            }
            Ok(None)
        }
        Plan::Cut { tip } => {
            // Named by the tip it carries: immutable, so the PUT is
            // unconditional and the sweep can tell an old one from the
            // live one by the snapshot's list alone.
            let name = format!("{tip}.bundle");
            let path = sc.cfg.state_dir.join(&name);
            std::fs::create_dir_all(&sc.cfg.state_dir)?;
            let _ = std::fs::remove_file(&path);
            let out = sc
                .git
                .run(&["bundle", "create", &path.to_string_lossy(), &branch], None)
                .await?;
            if !out.ok() {
                return Err(ForgeError::Git(format!("bundle create: {}", out.stderr.trim())));
            }
            let epoch = sc.lease()?.epoch;
            packio::upload_file(sc.store.as_ref(), &sc.cfg.bundle_key(&name), &path, epoch).await?;
            // The local copy has served its purpose; the bucket holds
            // it and the clients fetch it from there.
            let _ = std::fs::remove_file(&path);

            let url = sc.store.presign_get(&sc.cfg.bundle_key(&name), cfg.url_ttl_secs).await?;
            advertise(&sc.git, &url).await?;
            record = Record {
                tip: Some(tip),
                name: Some(name.clone()),
                cut_unix: now,
                signed_unix: now,
            };
            save_record(sc, &record)?;
            eprintln!("flint-forge: bundle cut and advertised ({name})");
            Ok(Some(name))
        }
    }
}
