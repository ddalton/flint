//! git's pkt-line framing — the only wire format forge implements, and
//! it implements it because `proc-receive` speaks it on the hook's
//! stdin and stdout and nothing else will.
//!
//! Four hex digits of length (counting themselves), then the payload.
//! `0000` is a flush. That is the whole format.

use std::io::{Read, Write};

/// One line, or a flush.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pkt {
    Line(Vec<u8>),
    Flush,
}

pub fn read_pkt(r: &mut impl Read) -> std::io::Result<Option<Pkt>> {
    let mut hdr = [0u8; 4];
    match r.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let text = std::str::from_utf8(&hdr)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "pkt-line header"))?;
    let len = usize::from_str_radix(text, 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "pkt-line length"))?;
    if len == 0 {
        return Ok(Some(Pkt::Flush));
    }
    if len < 4 {
        // 0001..0003 are delimiters this side of the protocol never
        // sends; refusing is better than guessing.
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "short pkt-line"));
    }
    let mut buf = vec![0u8; len - 4];
    r.read_exact(&mut buf)?;
    Ok(Some(Pkt::Line(buf)))
}

pub fn write_line(w: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() + 4;
    if len > 65520 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "pkt-line too long"));
    }
    write!(w, "{len:04x}")?;
    w.write_all(payload)
}

pub fn write_str(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    write_line(w, s.as_bytes())
}

pub fn write_flush(w: &mut impl Write) -> std::io::Result<()> {
    w.write_all(b"0000")
}

/// Read pkt-lines until a flush (or EOF), as UTF-8 with the trailing
/// LF trimmed — which is how every line in the `proc-receive`
/// conversation is shaped.
pub fn read_until_flush(r: &mut impl Read) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    loop {
        match read_pkt(r)? {
            None | Some(Pkt::Flush) => return Ok(out),
            Some(Pkt::Line(b)) => {
                let s = String::from_utf8_lossy(&b);
                out.push(s.trim_end_matches('\n').to_string());
            }
        }
    }
}
