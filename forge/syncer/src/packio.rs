//! Moving pack files between the local repository and the bucket.
//!
//! Packs are content-named by git, so every PUT here is
//! `Unconditional` — the one place in flint where that variant is
//! correct, and for the reason its doc comment gives: a re-upload is
//! byte-identical and its POINT is to refresh the object's age, which
//! is what the sweep reads (`LeanChunkGC` rule 4). A retried upload
//! must therefore never be skipped as "already there".

use std::io::Read;
use std::path::Path;

use bytes::Bytes;
use flint_store::{
    ComposeSpec, Crc64Nvme, GenerationStamps, ObjectStore, PartSource, PutCondition,
};

use super::{ForgeError, ForgeResult};

/// Above this, the upload is a multipart compose rather than one PUT.
/// `put_whole` holds the whole object in RAM; a repacked repository is
/// one pack, and one pack is the largest object forge ever writes.
pub const WHOLE_PUT_MAX: u64 = 64 * 1024 * 1024;

fn crc_of(path: &Path) -> ForgeResult<u64> {
    let mut crc = Crc64Nvme::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
    }
    Ok(crc.finalize())
}

/// Upload one local file to `key`, whole or composed by size.
pub async fn upload_file(
    store: &dyn ObjectStore,
    key: &str,
    path: &Path,
    epoch: u64,
) -> ForgeResult<()> {
    let size = std::fs::metadata(path)?.len();
    let crc = crc_of(path)?;
    let stamps = GenerationStamps {
        generation: 0,
        epoch,
        flush_uuid: uuid::Uuid::new_v4().to_string(),
        boundary_source: None,
        posix: None,
    };
    if size <= WHOLE_PUT_MAX {
        let body = std::fs::read(path)?;
        store
            .put_whole(key, Bytes::from(body), &PutCondition::Unconditional, &stamps, crc)
            .await?;
        return Ok(());
    }
    // Part grid: within [min_part_size, …], at most max_parts,
    // contiguous from zero.
    let min_part = store.min_part_size().max(1);
    let max_parts = store.max_parts().max(1) as u64;
    let mut chunk = WHOLE_PUT_MAX.max(min_part);
    if size.div_ceil(chunk) > max_parts {
        chunk = size.div_ceil(max_parts).div_ceil(min_part) * min_part;
    }
    let mut parts = Vec::new();
    let mut off = 0u64;
    while off < size {
        let len = chunk.min(size - off);
        parts.push(PartSource::Local { offset: off, len });
        off += len;
    }
    let spec = ComposeSpec {
        key,
        local_path: path,
        parts,
        base_key: None,
        base_etag: None,
        condition: PutCondition::Unconditional,
        stamps,
        crc64: crc,
    };
    store.compose_generation(&spec).await?;
    Ok(())
}

/// Fetch `key` into `path`, creating parent directories. Written to a
/// temporary name and renamed, so a torn download can never be mistaken
/// for a pack — git would read a truncated `.idx` as corruption of the
/// repository rather than of the transfer.
pub async fn fetch_to_file(store: &dyn ObjectStore, key: &str, path: &Path) -> ForgeResult<()> {
    let (_, body) = store.get_whole(key, None).await?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Put a small derived document (`info/refs`, `objects/info/packs`,
/// `HEAD`) with no precondition: they are regenerated from the
/// snapshot on every batch and nothing reads them back.
pub async fn put_small(
    store: &dyn ObjectStore,
    key: &str,
    body: Vec<u8>,
    epoch: u64,
) -> ForgeResult<()> {
    let crc = flint_store::crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch,
        flush_uuid: uuid::Uuid::new_v4().to_string(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(key, Bytes::from(body), &PutCondition::Unconditional, &stamps, crc)
        .await
        .map_err(ForgeError::from)?;
    Ok(())
}
