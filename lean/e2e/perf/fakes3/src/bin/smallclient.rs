//! What does lean's SMALL-FILE work shape cost in a client with no SDK
//! in it?
//!
//! `refclient` answered that for ranged GETs on big objects. Small files
//! are a different shape: one whole GET per object over a KEPT-ALIVE
//! connection, then a tmp-write + rename per object. This client does
//! exactly that — `--conns` workers, each owning one persistent HTTP/1.1
//! connection (the same thing hyper's pool hands the SDK), pulling keys
//! off one shared counter — and nothing else. Whatever gap opens between
//! this and flint-sync on the same files, server and node is the price
//! of the layers above the socket, and no gap says the shape itself is
//! the floor.
//!
//! `--write 0` drops the file write and keeps everything else, so the
//! two halves of the shape can be told apart on one node.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

async fn one_conn(
    host: String,
    keys: Arc<Vec<(String, u64)>>,
    next: Arc<AtomicUsize>,
    out: Option<std::path::PathBuf>,
) -> std::io::Result<(usize, u64)> {
    let s = tokio::net::TcpStream::connect(&host).await?;
    s.set_nodelay(true)?;
    let (r, mut w) = s.into_split();
    let mut r = BufReader::with_capacity(64 * 1024, r);
    let mut n = 0usize;
    let mut bytes = 0u64;
    let mut line = String::new();
    loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= keys.len() {
            break;
        }
        let (key, size) = &keys[i];
        let req = format!("GET /bucket/{} HTTP/1.1\r\nHost: {}\r\n\r\n", key, host);
        w.write_all(req.as_bytes()).await?;
        // status line + headers; the only header we need is the length
        let mut clen: Option<usize> = None;
        line.clear();
        r.read_line(&mut line).await?;
        if !line.starts_with("HTTP/1.1 200") {
            return Err(std::io::Error::other(format!("{key}: {}", line.trim())));
        }
        loop {
            line.clear();
            r.read_line(&mut line).await?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                clen = v.trim().parse().ok();
            }
        }
        let clen = clen.ok_or_else(|| std::io::Error::other("no content-length"))?;
        let mut body = vec![0u8; clen];
        r.read_exact(&mut body).await?;
        assert_eq!(clen as u64, *size, "short body for {key}");
        bytes += clen as u64;
        if let Some(out) = &out {
            // the same shape as write_via_tmp_fast: tmp sibling, write, rename
            let target = out.join(key.rsplit('/').next().unwrap());
            let tmp = target.with_file_name(format!(
                "{}.tmp",
                target.file_name().unwrap().to_string_lossy()
            ));
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                std::fs::write(&tmp, &body)?;
                std::fs::rename(&tmp, &target)
            })
            .await
            .unwrap()?;
        }
        n += 1;
    }
    Ok((n, bytes))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: &str| -> String {
        a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned().unwrap_or_else(|| d.into())
    };
    let host = get("--host", "127.0.0.1:9000");
    let keys_file = get("--keys", "keys.tsv");
    let conns: usize = get("--conns", "32").parse().unwrap();
    let write: bool = get("--write", "1") == "1";
    let out = get("--out", "/tmp/smallout");

    let keys: Vec<(String, u64)> = std::fs::read_to_string(&keys_file)?
        .lines()
        .map(|l| {
            let (k, sz) = l.split_once('\t').unwrap();
            (k.to_string(), sz.trim().parse().unwrap())
        })
        .collect();
    let total = keys.len();
    let keys = Arc::new(keys);
    let outdir = if write {
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out)?;
        Some(std::path::PathBuf::from(&out))
    } else {
        None
    };
    let next = Arc::new(AtomicUsize::new(0));
    let t0 = std::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..conns {
        set.spawn(one_conn(host.clone(), keys.clone(), next.clone(), outdir.clone()));
    }
    let (mut n, mut bytes) = (0usize, 0u64);
    while let Some(r) = set.join_next().await {
        let (a, b) = r.unwrap()?;
        n += a;
        bytes += b;
    }
    let el = t0.elapsed().as_secs_f64();
    assert_eq!(n, total, "not every key was fetched");
    println!(
        "smallclient: {:.3}s  {:.0} files/s  ({} files, {} bytes, conns {}, write {})",
        el,
        n as f64 / el,
        n,
        bytes,
        conns,
        write
    );
    Ok(())
}
