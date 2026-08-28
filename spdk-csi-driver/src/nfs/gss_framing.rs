//! RPCSEC_GSS message framing — RFC 2203 §5.3.
//!
//! This is the layer between the RPC decoder and NFS. Before it existed,
//! a `krb5i` or `krb5p` call reached `handle_compound` with its body
//! still wrapped, the call verifier was decoded and thrown away, and
//! every reply went out with a null verifier — so the only reason
//! integrity and privacy did not corrupt anything is that
//! [`super::rpcsec_gss`] refused them outright.
//!
//! The trap this module exists to get right: for `rpc_gss_integ_data`
//! the checksum covers the **inner `rpc_gss_data_t` octet stream**, not
//! the XDR-opaque-wrapped form. Including the four-octet length prefix
//! or the padding produces a MIC that verifies against itself forever
//! and against no other implementation ever.

use bytes::Bytes;

use super::krb::token::{ContextKey, PerMessageTokens};
use super::rpc::{Auth, AuthFlavor, AuthStat};
use super::rpcsec_gss::GssService;
use super::xdr::{XdrDecoder, XdrEncoder};

/// Why a call was refused, and what the client should be told.
///
/// RFC 2203 §5.3.3.3. The distinction is load-bearing: a client told
/// `CREDPROBLEM` or `CTXPROBLEM` destroys its context and re-initialises,
/// where an accepted-status error makes it fail the operation. Sending
/// `SYSTEM_ERR` for an expired context turns a routine refresh into a
/// hung mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GssReject {
    /// The credential or the call verifier did not check out.
    CredProblem(String),
    /// No such context, or it is unusable now.
    CtxProblem(String),
    /// The body did not decode. This is the client's framing, not its
    /// credential, so it is a garbage-args case rather than an auth one.
    Garbage(String),
}

impl GssReject {
    pub fn auth_stat(&self) -> AuthStat {
        match self {
            GssReject::CredProblem(_) => AuthStat::RpcsecGssCredProblem,
            GssReject::CtxProblem(_) => AuthStat::RpcsecGssCtxProblem,
            // Never reached through the auth-error path; see `is_garbage`.
            GssReject::Garbage(_) => AuthStat::RpcsecGssCredProblem,
        }
    }

    pub fn is_garbage(&self) -> bool {
        matches!(self, GssReject::Garbage(_))
    }

    pub fn reason(&self) -> &str {
        match self {
            GssReject::CredProblem(m) | GssReject::CtxProblem(m) | GssReject::Garbage(m) => m,
        }
    }
}

/// A DATA call that passed context lookup, sequence and verifier checks.
pub struct ValidatedCall {
    pub service: GssService,
    /// The RPC sequence number from the credential.
    pub seq_num: u32,
    /// The authenticated Kerberos client principal, when there is one.
    /// This — not the credential bytes — is the identity RFC 8881
    /// §18.35.5 compares.
    pub client_principal: Option<String>,
    /// The per-message machinery, keyed per RFC 4121 §2.
    ///
    /// `None` only in the keytab-less placeholder mode, which holds no
    /// key material and therefore cannot produce or check a token. That
    /// mode is refused for anything but `svc_none`.
    pub tokens: Option<PerMessageTokens<ContextKey>>,
}

impl std::fmt::Debug for ValidatedCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately never prints key material.
        f.debug_struct("ValidatedCall")
            .field("service", &self.service)
            .field("seq_num", &self.seq_num)
            .field("principal", &self.client_principal)
            .field("keyed", &self.tokens.is_some())
            .finish()
    }
}

impl std::fmt::Display for GssReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            GssReject::CredProblem(_) => "CREDPROBLEM",
            GssReject::CtxProblem(_) => "CTXPROBLEM",
            GssReject::Garbage(_) => "GARBAGE_ARGS",
        };
        write!(f, "{kind}: {}", self.reason())
    }
}

impl ValidatedCall {
    fn need_tokens(&self) -> Result<&PerMessageTokens<ContextKey>, GssReject> {
        self.tokens.as_ref().ok_or_else(|| {
            GssReject::CtxProblem(
                "context holds no key material (placeholder mode); per-message \
                 protection is impossible"
                    .into(),
            )
        })
    }

    /// The GSS sequence number to stamp into tokens this server emits.
    ///
    /// RFC 2203 §5.2.2 turns `sequence_req_flag` and `replay_det_req_flag`
    /// OFF — RPCSEC_GSS runs its own replay window over the credential's
    /// `seq_num` instead — so the peer does not order tokens by this
    /// field. Mirroring the request's number keeps it deterministic and
    /// reproducible in tests. ⚠ UNPROVEN AGAINST A REAL PEER: this is one
    /// of the values a packet capture would settle.
    fn token_seq(&self) -> u64 {
        self.seq_num as u64
    }
}

/// `rpc_gss_data_t { unsigned int seq_num; opaque arg }` — RFC 2203 §5.3.2.
///
/// Returned as the inner octet stream, which is exactly what the
/// integrity checksum covers.
fn encode_data_t(seq_num: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&seq_num.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Split `rpc_gss_data_t` and check its sequence number against the
/// credential's.
///
/// The inner `seq_num` is not decoration: it is what binds the sealed
/// body to the credential the server authenticated. A body lifted from
/// another call of the same context would otherwise verify.
fn split_data_t(inner: &[u8], expect_seq: u32) -> Result<Bytes, GssReject> {
    if inner.len() < 4 {
        return Err(GssReject::Garbage("rpc_gss_data_t shorter than its seq_num".into()));
    }
    let got = u32::from_be_bytes([inner[0], inner[1], inner[2], inner[3]]);
    if got != expect_seq {
        return Err(GssReject::CredProblem(format!(
            "sealed seq_num {got} does not match the credential's {expect_seq}"
        )));
    }
    Ok(Bytes::copy_from_slice(&inner[4..]))
}

/// Verify the call verifier — RFC 2203 §5.3.1.
///
/// `GSS_GetMIC` over the RPC header up to and including the credential.
/// The span comes from [`super::rpc::CallMessage::cred_span`]; it could
/// not be reconstructed here, which is why this check did not exist.
pub fn verify_call_verifier(
    v: &ValidatedCall,
    verf: &Auth,
    cred_span: &[u8],
) -> Result<(), GssReject> {
    if verf.flavor != AuthFlavor::RpcsecGss {
        return Err(GssReject::CredProblem(format!(
            "call verifier flavor is {:?}, expected RPCSEC_GSS",
            verf.flavor
        )));
    }
    let tokens = v.need_tokens()?;
    tokens
        .verify_mic(verf.body.as_ref(), cred_span)
        .map(|_| ())
        .map_err(|e| GssReject::CredProblem(format!("call verifier MIC failed: {e}")))
}

/// Strip the RPCSEC_GSS body wrapper — RFC 2203 §5.3.2.
///
/// Returns the procedure arguments as NFS would see them. `svc_none`
/// passes through untouched; the other two must be unwrapped BEFORE the
/// COMPOUND is decoded, which is the step that was missing.
pub fn unseal_call_body(v: &ValidatedCall, args: Bytes) -> Result<Bytes, GssReject> {
    match v.service {
        GssService::None => Ok(args),

        GssService::Integrity => {
            let mut dec = XdrDecoder::new(args);
            let inner = dec
                .decode_opaque()
                .map_err(|e| GssReject::Garbage(format!("databody_integ: {e}")))?;
            let checksum = dec
                .decode_opaque()
                .map_err(|e| GssReject::Garbage(format!("integ checksum: {e}")))?;

            // Over the INNER stream — no length prefix, no padding.
            let tokens = v.need_tokens()?;
            tokens
                .verify_mic(checksum.as_ref(), inner.as_ref())
                .map_err(|e| GssReject::CredProblem(format!("integrity MIC failed: {e}")))?;

            split_data_t(inner.as_ref(), v.seq_num)
        }

        GssService::Privacy => {
            let mut dec = XdrDecoder::new(args);
            let sealed = dec
                .decode_opaque()
                .map_err(|e| GssReject::Garbage(format!("databody_priv: {e}")))?;

            let tokens = v.need_tokens()?;
            let opened = tokens
                .unwrap(sealed.as_ref())
                .map_err(|e| GssReject::CredProblem(format!("privacy unwrap failed: {e}")))?;

            split_data_t(&opened.message, v.seq_num)
        }
    }
}

/// Apply the RPCSEC_GSS body wrapper to results — RFC 2203 §5.3.2,
/// `proc_res_arg_t` in place of `proc_req_arg_t`.
pub fn seal_reply_body(v: &ValidatedCall, results: &[u8]) -> Result<Bytes, GssReject> {
    match v.service {
        GssService::None => Ok(Bytes::copy_from_slice(results)),

        GssService::Integrity => {
            let inner = encode_data_t(v.seq_num, results);
            let tokens = v.need_tokens()?;
            let mic = tokens
                .emit_mic(v.token_seq(), &inner)
                .map_err(|e| GssReject::CtxProblem(format!("reply MIC: {e}")))?;
            let mut enc = XdrEncoder::new();
            enc.encode_opaque(&inner);
            enc.encode_opaque(&mic);
            Ok(enc.finish())
        }

        GssService::Privacy => {
            let inner = encode_data_t(v.seq_num, results);
            let tokens = v.need_tokens()?;
            let sealed = tokens
                .emit_wrap(v.token_seq(), &inner)
                .map_err(|e| GssReject::CtxProblem(format!("reply wrap: {e}")))?;
            let mut enc = XdrEncoder::new();
            enc.encode_opaque(&sealed);
            Ok(enc.finish())
        }
    }
}

/// The reply verifier — RFC 2203 §5.3.3.2.
///
/// `GSS_GetMIC` over the request's sequence number in network order.
/// Every reply on this server carried AUTH_NONE instead, which a
/// conforming client rejects.
pub fn reply_verifier(v: &ValidatedCall) -> Result<Auth, GssReject> {
    let tokens = v.need_tokens()?;
    let mic = tokens
        .emit_mic(v.token_seq(), &v.seq_num.to_be_bytes())
        .map_err(|e| GssReject::CtxProblem(format!("reply verifier: {e}")))?;
    Ok(Auth {
        flavor: AuthFlavor::RpcsecGss,
        body: Bytes::from(mic),
    })
}

/// The INIT/CONTINUE_INIT reply verifier — RFC 2203 §5.2.3.1.
///
/// `GSS_GetMIC` over the sequence window, in network order, and ONLY
/// when the context completed; any other major status takes a NULL
/// verifier.
pub fn init_reply_verifier(
    tokens: &PerMessageTokens<ContextKey>,
    seq_window: u32,
) -> Result<Auth, GssReject> {
    let mic = tokens
        .emit_mic(0, &seq_window.to_be_bytes())
        .map_err(|e| GssReject::CtxProblem(format!("init verifier: {e}")))?;
    Ok(Auth {
        flavor: AuthFlavor::RpcsecGss,
        body: Bytes::from(mic),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::krb::token::Role;

    /// RFC 8009 Appendix A's 128-bit base key — a published value, so the
    /// context these tests run on is not invented.
    const BASE: &str = "3705D96080C17728A0E800EAB6E0D23C";
    const ETYPE: i32 = 19;
    const SEQ: u32 = 0x2A;
    const ARGS: &[u8] = b"COMPOUND-args-would-be-here";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn tokens(role: Role) -> PerMessageTokens<ContextKey> {
        let key = ContextKey::new(ETYPE, &unhex(BASE)).unwrap();
        PerMessageTokens::new(key, role, false)
    }

    fn server(service: GssService) -> ValidatedCall {
        ValidatedCall { service, seq_num: SEQ, client_principal: None, tokens: Some(tokens(Role::Acceptor)) }
    }

    /// Build a client-side call body the way a conforming initiator would.
    fn client_body(service: GssService, seq: u32, args: &[u8]) -> Bytes {
        let cli = tokens(Role::Initiator);
        let inner = encode_data_t(seq, args);
        let mut enc = XdrEncoder::new();
        match service {
            GssService::None => return Bytes::copy_from_slice(args),
            GssService::Integrity => {
                let mic = cli.emit_mic(seq as u64, &inner).unwrap();
                enc.encode_opaque(&inner);
                enc.encode_opaque(&mic);
            }
            GssService::Privacy => {
                let sealed = cli.emit_wrap(seq as u64, &inner).unwrap();
                enc.encode_opaque(&sealed);
            }
        }
        enc.finish()
    }

    #[test]
    fn every_service_round_trips_a_call_body() {
        for service in [GssService::None, GssService::Integrity, GssService::Privacy] {
            let v = server(service);
            let got = unseal_call_body(&v, client_body(service, SEQ, ARGS))
                .unwrap_or_else(|e| panic!("{service:?}: {e}"));
            assert_eq!(got.as_ref(), ARGS, "{service:?}");
        }
    }

    /// THE TRAP THIS MODULE EXISTS FOR.
    ///
    /// RFC 2203 §5.3.2.2 puts the integrity checksum over the inner
    /// `rpc_gss_data_t` octet stream. Computing it over the XDR-opaque
    /// *wrapped* form instead — length prefix and padding included — is
    /// self-consistent, round-trips against itself forever, and
    /// interoperates with nothing. It must be refused.
    #[test]
    fn the_integrity_checksum_covers_the_inner_stream_not_the_wrapped_form() {
        let cli = tokens(Role::Initiator);
        let inner = encode_data_t(SEQ, ARGS);

        // The wrong span: the opaque-wrapped octets.
        let wrapped = {
            let mut e = XdrEncoder::new();
            e.encode_opaque(&inner);
            e.finish()
        };
        assert_ne!(
            wrapped.as_ref(),
            inner.as_slice(),
            "the two spans must actually differ, or this test proves nothing"
        );

        let mut e = XdrEncoder::new();
        e.encode_opaque(&inner);
        e.encode_opaque(&cli.emit_mic(SEQ as u64, &wrapped).unwrap());
        let err = unseal_call_body(&server(GssService::Integrity), e.finish())
            .expect_err("a MIC over the wrapped form must be refused");
        assert!(matches!(err, GssReject::CredProblem(_)), "got {err}");

        // Control: the same body with the checksum over the INNER stream
        // is accepted, so the refusal above is the span and not the shape.
        let mut e = XdrEncoder::new();
        e.encode_opaque(&inner);
        e.encode_opaque(&cli.emit_mic(SEQ as u64, &inner).unwrap());
        assert_eq!(
            unseal_call_body(&server(GssService::Integrity), e.finish())
                .unwrap()
                .as_ref(),
            ARGS
        );
    }

    /// The inner seq_num binds the body to the credential the server
    /// authenticated. Without this check a body lifted from another call
    /// on the same context verifies perfectly.
    #[test]
    fn a_body_sealed_under_another_sequence_number_is_refused() {
        for service in [GssService::Integrity, GssService::Privacy] {
            let body = client_body(service, SEQ + 1, ARGS);
            let err = unseal_call_body(&server(service), body)
                .expect_err("{service:?}: mismatched inner seq must be refused");
            assert!(matches!(err, GssReject::CredProblem(_)), "{service:?}: got {err}");
        }
    }

    #[test]
    fn a_tampered_privacy_body_is_refused() {
        // Flip a bit INSIDE the sealed token, not at the end of the
        // enclosing opaque — an XDR opaque is padded to a 4-octet
        // boundary and `decode_opaque` discards the padding, so
        // corrupting the last octet of the *body* is very often invisible.
        // The first draft of this test did exactly that and passed while
        // proving nothing.
        let mut dec = XdrDecoder::new(client_body(GssService::Privacy, SEQ, ARGS));
        let mut sealed = dec.decode_opaque().unwrap().to_vec();
        let n = sealed.len();
        sealed[n - 1] ^= 0x01;

        let mut e = XdrEncoder::new();
        e.encode_opaque(&sealed);
        let err = unseal_call_body(&server(GssService::Privacy), e.finish())
            .expect_err("a flipped octet must not decrypt");
        assert!(matches!(err, GssReject::CredProblem(_)), "got {err}");
    }

    /// The reply verifier is a MIC over the request's seq_num, and the
    /// client checks it. Every reply used to carry AUTH_NONE.
    #[test]
    fn the_reply_verifier_is_a_mic_over_the_request_sequence_number() {
        let v = server(GssService::None);
        let verf = reply_verifier(&v).unwrap();
        assert_eq!(verf.flavor, AuthFlavor::RpcsecGss, "never AUTH_NONE");

        // The client verifies with the acceptor's usage over the same message.
        tokens(Role::Initiator)
            .verify_mic(verf.body.as_ref(), &SEQ.to_be_bytes())
            .expect("client must accept the reply verifier");

        // And it does not verify over a different sequence number.
        assert!(tokens(Role::Initiator)
            .verify_mic(verf.body.as_ref(), &(SEQ + 1).to_be_bytes())
            .is_err());
    }

    #[test]
    fn reply_bodies_round_trip_for_every_service() {
        for service in [GssService::None, GssService::Integrity, GssService::Privacy] {
            let v = server(service);
            let sealed = seal_reply_body(&v, ARGS).unwrap_or_else(|e| panic!("{service:?}: {e}"));
            if service == GssService::None {
                assert_eq!(sealed.as_ref(), ARGS);
                continue;
            }
            // Decode it the way a client would, with the peer's usages.
            let cli = tokens(Role::Initiator);
            let mut dec = XdrDecoder::new(sealed);
            let inner = match service {
                GssService::Integrity => {
                    let inner = dec.decode_opaque().unwrap();
                    let mic = dec.decode_opaque().unwrap();
                    cli.verify_mic(mic.as_ref(), inner.as_ref()).expect("client MIC");
                    inner.to_vec()
                }
                _ => {
                    let sealed = dec.decode_opaque().unwrap();
                    cli.unwrap(sealed.as_ref()).expect("client unwrap").message
                }
            };
            assert_eq!(&inner[..4], &SEQ.to_be_bytes(), "{service:?}: seq binding");
            assert_eq!(&inner[4..], ARGS, "{service:?}");
        }
    }

    /// A keyless (placeholder) context must refuse per-message work with
    /// CTXPROBLEM — "re-init and retry" — rather than passing it through.
    #[test]
    fn a_keyless_context_cannot_seal_or_verify() {
        // A WELL-FORMED body, so the refusal is the missing key material
        // and not the framing check that runs before it.
        let body = client_body(GssService::Integrity, SEQ, ARGS);
        let v = ValidatedCall { service: GssService::Integrity, seq_num: SEQ, client_principal: None, tokens: None };
        assert_eq!(
            unseal_call_body(&v, body).unwrap_err().auth_stat(),
            AuthStat::RpcsecGssCtxProblem
        );
        assert_eq!(
            reply_verifier(&v).unwrap_err().auth_stat(),
            AuthStat::RpcsecGssCtxProblem
        );
    }

    /// A malformed body is the client's framing, not its credential — so
    /// it must map to GARBAGE_ARGS, not to an auth error that tells the
    /// client to throw away a perfectly good context.
    #[test]
    fn malformed_framing_is_garbage_not_an_auth_failure() {
        let err = unseal_call_body(&server(GssService::Integrity), Bytes::from_static(&[0, 0]))
            .expect_err("truncated body");
        assert!(err.is_garbage(), "got {err}");
    }
}
