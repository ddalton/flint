//! The epoch cell (A8) on any store that has conditional writes: every
//! transition is one `put_whole` guarded on the token the caller holds,
//! and the token is the cell's ETag.
//!
//! The S3 backend has its own copy of this, written before a second
//! backend existed (`s3.rs`, `epoch_write`). The body here is the same
//! JSON, field for field, so a cell written by one reads in the other;
//! `the_body_keeps_the_s3_backends_field_names` pins that. A backend whose
//! ETag would not change at every write (Azure's native blob lease keeps
//! one id across renewals) must not stand in for this: a holder is judged
//! dead when the token stops moving.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{crc64_nvme, EpochLease, EpochState, GenerationStamps, ObjectStore, PutCondition, StoreError, StoreResult};

#[derive(Serialize, Deserialize)]
struct EpochBody {
    holder_id: String,
    epoch: u64,
    renewed_unix: u64,
    /// Every write changes the bytes, so every write changes a content
    /// ETag: two renewals in one second would otherwise repeat the token.
    salt: String,
    #[serde(default)]
    released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    echo: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    waiters: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<String>,
}

/// Everything one transition writes.
pub(crate) struct EpochWrite<'a> {
    pub holder_id: &'a str,
    pub epoch: u64,
    pub condition: PutCondition,
    pub released: bool,
    pub echo: Option<&'a str>,
    pub waiters: Vec<String>,
    pub handoff: Option<String>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub(crate) async fn read<S: ObjectStore + ?Sized>(s: &S, key: &str) -> StoreResult<Option<EpochState>> {
    match s.get_whole(key, None).await {
        Ok((meta, bytes)) => {
            let body: EpochBody =
                serde_json::from_slice(&bytes).map_err(|e| StoreError::Other(format!("epoch body: {e}")))?;
            Ok(Some(EpochState {
                holder_id: body.holder_id,
                epoch: body.epoch,
                token: meta.etag,
                last_renew_unix: meta.last_modified_unix,
                released: body.released,
                echo: body.echo,
                waiters: body.waiters,
                handoff: body.handoff,
            }))
        }
        Err(StoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) async fn write<S: ObjectStore + ?Sized>(s: &S, key: &str, w: EpochWrite<'_>) -> StoreResult<EpochLease> {
    let EpochWrite { holder_id, epoch, condition, released, echo, waiters, handoff } = w;
    let body = Bytes::from(
        serde_json::to_vec(&EpochBody {
            holder_id: holder_id.to_string(),
            epoch,
            renewed_unix: now_unix(),
            salt: uuid::Uuid::new_v4().to_string(),
            released,
            echo: echo.map(str::to_string),
            waiters: waiters.clone(),
            handoff,
        })
        .map_err(|e| StoreError::Other(format!("epoch body: {e}")))?,
    );
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps { generation: 0, epoch, flush_uuid: "epoch-cell".into(), boundary_source: None, posix: None };
    let meta = s.put_whole(key, body, &condition, &stamps, crc).await?;
    Ok(EpochLease { holder_id: holder_id.to_string(), epoch, token: meta.etag, waiters })
}

pub(crate) async fn acquire<S: ObjectStore + ?Sized>(
    s: &S,
    key: &str,
    holder_id: &str,
    supersede: Option<&EpochState>,
) -> StoreResult<EpochLease> {
    let epoch = supersede.map_or(1, |st| st.epoch + 1);
    // The claimant leaves the queue; the rest stay queued behind it.
    let waiters = supersede
        .map(|st| st.waiters.iter().filter(|w| w.as_str() != holder_id).cloned().collect())
        .unwrap_or_default();
    let condition = match supersede {
        None => PutCondition::IfNoneMatchAny,
        Some(st) => PutCondition::IfMatch(st.token.clone()),
    };
    write(s, key, EpochWrite { holder_id, epoch, condition, released: false, echo: None, waiters, handoff: None }).await
}

pub(crate) async fn renew<S: ObjectStore + ?Sized>(
    s: &S,
    key: &str,
    lease: &EpochLease,
    echo: Option<&str>,
) -> StoreResult<EpochLease> {
    let condition = PutCondition::IfMatch(lease.token.clone());
    let waiters = lease.waiters.clone();
    write(s, key, EpochWrite { holder_id: &lease.holder_id, epoch: lease.epoch, condition, released: false, echo, waiters, handoff: None })
        .await
}

/// Mark, never delete: the epoch number must survive a release.
pub(crate) async fn release<S: ObjectStore + ?Sized>(s: &S, key: &str, lease: &EpochLease) -> StoreResult<()> {
    let condition = PutCondition::IfMatch(lease.token.clone());
    let waiters = lease.waiters.clone();
    write(s, key, EpochWrite { holder_id: &lease.holder_id, epoch: lease.epoch, condition, released: true, echo: None, waiters, handoff: None })
        .await
        .map(|_| ())
}

pub(crate) async fn handoff<S: ObjectStore + ?Sized>(
    s: &S,
    key: &str,
    lease: &EpochLease,
    echo: Option<&str>,
) -> StoreResult<()> {
    let mut rest = lease.waiters.clone();
    let head = if rest.is_empty() { None } else { Some(rest.remove(0)) };
    let condition = PutCondition::IfMatch(lease.token.clone());
    write(s, key, EpochWrite { holder_id: &lease.holder_id, epoch: lease.epoch, condition, released: true, echo, waiters: rest, handoff: head })
        .await
        .map(|_| ())
}

pub(crate) async fn enqueue<S: ObjectStore + ?Sized>(
    s: &S,
    key: &str,
    observed: &EpochState,
    holder_id: &str,
) -> StoreResult<EpochState> {
    let mut waiters = observed.waiters.clone();
    if !waiters.iter().any(|w| w == holder_id) {
        waiters.push(holder_id.to_string());
    }
    let lease = write(
        s,
        key,
        EpochWrite {
            holder_id: &observed.holder_id,
            epoch: observed.epoch,
            condition: PutCondition::IfMatch(observed.token.clone()),
            released: observed.released,
            echo: observed.echo.as_deref(),
            waiters: waiters.clone(),
            handoff: observed.handoff.clone(),
        },
    )
    .await?;
    Ok(EpochState {
        holder_id: observed.holder_id.clone(),
        epoch: observed.epoch,
        token: lease.token,
        last_renew_unix: observed.last_renew_unix,
        echo: observed.echo.clone(),
        released: observed.released,
        waiters,
        handoff: observed.handoff.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cell's bytes are the S3 backend's: the same names, and the
    /// optional ones left out when empty, as there.
    #[test]
    fn the_body_keeps_the_s3_backends_field_names() {
        let b = EpochBody {
            holder_id: "h".into(),
            epoch: 3,
            renewed_unix: 7,
            salt: "s".into(),
            released: true,
            echo: Some("e".into()),
            waiters: vec!["w".into()],
            handoff: Some("w".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&b).unwrap();
        let mut names: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["echo", "epoch", "handoff", "holder_id", "released", "renewed_unix", "salt", "waiters"]);
        let bare = EpochBody { echo: None, waiters: vec![], handoff: None, ..b };
        let v: serde_json::Value = serde_json::to_value(&bare).unwrap();
        assert!(v.get("echo").is_none() && v.get("waiters").is_none() && v.get("handoff").is_none());
        // A cell written without the optional fields reads with defaults.
        let old: EpochBody = serde_json::from_str(r#"{"holder_id":"h","epoch":1,"renewed_unix":0,"salt":"x"}"#).unwrap();
        assert!(!old.released && old.waiters.is_empty());
    }
}
