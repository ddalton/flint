//! What does lean's WORK SHAPE cost, in a client with nothing else in it?
//!
//! The "4x headroom" figure compared lean (384 ranged GETs, 6 GiB written
//! to files) against `curl -o /dev/null` (6 whole GETs, nothing written).
//! Those are different amounts of work, so the comparison was void — the
//! same /dev/null error already corrected once in the mount-s3 arm.
//!
//! This client does what lean does: split each object into chunks, fetch
//! them as ranged GETs with bounded concurrency, and pwrite each at its
//! offset into a real file. It speaks HTTP/1.1 over a raw socket rather
//! than through the AWS SDK, so a large gap in ITS favour indicts the SDK
//! layer, and no gap says lean is already at the shape's floor.
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn fetch_range(host: &str, path: &str, off: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut s = tokio::net::TcpStream::connect(host).await?;
    s.set_nodelay(true)?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
        path, host, off, off + len - 1
    );
    s.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(len as usize + 1024);
    s.read_to_end(&mut buf).await?;
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no header terminator"))?
        + 4;
    Ok(buf.split_off(head_end))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: &str| -> String {
        a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned().unwrap_or_else(|| d.into())
    };
    let host = get("--host", "127.0.0.1:9000");
    let keys_file = get("--keys", "keys.tsv");
    let out = get("--out", "/tmp/refout");
    let chunk: u64 = get("--chunk-mb", "16").parse::<u64>().unwrap() * 1024 * 1024;
    let par: usize = get("--par", "24").parse().unwrap();

    std::fs::create_dir_all(&out)?;
    let mut jobs: Vec<(String, u64, u64)> = Vec::new();
    let mut total = 0u64;
    let mut files = std::collections::HashMap::new();
    for line in std::fs::read_to_string(&keys_file)?.lines() {
        let (k, sz) = line.split_once('\t').unwrap();
        let size: u64 = sz.trim().parse().unwrap();
        total += size;
        let p = format!("{}/{}", out, k.replace('/', "_"));
        let f = std::fs::File::create(&p)?;
        f.set_len(size)?;
        files.insert(k.to_string(), Arc::new(f));
        let mut at = 0u64;
        while at < size {
            let n = chunk.min(size - at);
            jobs.push((k.to_string(), at, n));
            at += n;
        }
    }

    let t0 = std::time::Instant::now();
    let sem = Arc::new(tokio::sync::Semaphore::new(par));
    let mut set = tokio::task::JoinSet::new();
    for (key, off, len) in jobs {
        let sem = sem.clone();
        let host = host.clone();
        let f = files[&key].clone();
        set.spawn(async move {
            let _p = sem.acquire_owned().await.unwrap();
            let path = format!("/bucket/{}", key);
            let body = fetch_range(&host, &path, off, len).await.expect("range");
            assert_eq!(body.len() as u64, len, "short range at {off}");
            tokio::task::spawn_blocking(move || f.write_all_at(&body, off))
                .await
                .unwrap()
                .expect("pwrite");
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    let el = t0.elapsed().as_secs_f64();
    println!(
        "refclient: {:.2}s  {:.0} MiB/s  ({} MiB, chunk {} MiB, par {})",
        el,
        total as f64 / 1048576.0 / el,
        total / 1048576,
        chunk / 1048576,
        par
    );
    Ok(())
}
