//! Kerberos 5 cryptography, to specification.
//!
//! This module exists because the crypto in [`super::kerberos`] is not
//! RFC 3961/3962 — it derives Ke with Kc's constant (0x99 where the
//! spec says 0xAA), zero-pads where DR requires n-fold, and encrypts
//! with no confounder and the HMAC *inside* the ciphertext. None of
//! that interoperates with a real KDC, and nothing caught it because
//! every existing test round-trips the implementation against itself.
//!
//! Every function here is pinned to published test vectors. A
//! self-consistency test is not evidence for a wire format: if it
//! round-trips but disagrees with the RFC, it is simply a private
//! cipher that no other implementation speaks.

pub mod nfold;   // RFC 3961 §5.1 n-fold
pub mod kdf;     // RFC 3961 §5.1 DR/DK, RFC 3962 §4, RFC 8009 §3
pub mod profile; // RFC 3961 §5.3 simplified profile (confounder + HMAC)
pub mod token;   // RFC 4121 §4.2 per-message Wrap and MIC tokens

#[cfg(test)]
mod interop; // MIT krb5 interop fixtures — the gate vectors cannot close
