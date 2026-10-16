//! The RPCSEC_GSS front end (RFC 2203), shared by the NFS listeners that
//! do Kerberos: `server_v4::serve_tcp` (the RWX servers and the pNFS MDS),
//! and the pNFS data servers (pNFS Kerberos plan, Phase 3). One copy, so
//! the checks cannot drift apart between them.
//!
//! - [`gss_front`] takes an RPCSEC_GSS call: it answers INIT,
//!   CONTINUE_INIT and DESTROY itself, and for DATA checks the context,
//!   the floor, the call verifier and the sequence window (in that order)
//!   and unseals the body. What passes comes back as
//!   [`GssFront::Data`] for the caller to dispatch on the RPC procedure.
//! - [`floor_refusal`] is the floor for every other call (AUTH_NONE,
//!   AUTH_SYS).
//! - [`frame_reply`] frames an accepted reply: a GSS verifier and a sealed
//!   body for a DATA call, the plain segments otherwise.

use super::gss_framing::ValidatedCall;
use super::rpc::{AuthFlavor, AuthStat, CallMessage, ReplyBuilder};
use super::rpcsec_gss::{procedure as gss_proc, RpcGssCred, RpcSecGssManager};
use super::segment::Segment;
use super::v4::protocol::procedure;
use super::xdr::{XdrDecoder, XdrEncoder};
use bytes::Bytes;
use tracing::{info, warn};

/// What [`gss_front`] decided.
pub(crate) enum GssFront {
    /// Answered here (INIT, CONTINUE_INIT, DESTROY, or a refusal): send it.
    Reply(Vec<Segment>),
    /// A DATA call that passed every check: its unsealed arguments and the
    /// context to frame the reply with.
    Data { args: Bytes, validated: ValidatedCall },
}

/// An RPCSEC_GSS call (`call.cred.flavor == RpcsecGss`): see the module
/// docs.
pub(crate) async fn gss_front(call: &CallMessage, args: Bytes, gss_manager: &RpcSecGssManager) -> GssFront {
    // Decode RPCSEC_GSS credentials
    let gss_cred = match RpcGssCred::decode(&call.cred.body) {
        Ok(cred) => {
            info!("🔐 GSS Cred: version={}, procedure={}, seq={}, service={:?}",
                  cred.version, cred.procedure, cred.sequence_num, cred.service);
            cred
        }
        Err(e) => {
            warn!("❌ Failed to decode RPCSEC_GSS credentials: {}", e);
            return GssFront::Reply(vec![ReplyBuilder::garbage_args(call.xid).into()]);
        }
    };

    // Handle different GSS procedures
    match gss_cred.procedure {
        gss_proc::INIT => {
            info!("🔐 RPCSEC_GSS_INIT");
            GssFront::Reply(vec![handle_gss_init(call.xid, &gss_cred, args, gss_manager).await.into()])
        }

        gss_proc::CONTINUE_INIT => {
            info!("🔐 RPCSEC_GSS_CONTINUE_INIT");
            GssFront::Reply(vec![handle_gss_continue_init(call.xid, &gss_cred, args, gss_manager).await.into()])
        }

        gss_proc::DATA => {
            info!("🔐 RPCSEC_GSS_DATA");
            gss_data(call, &gss_cred, args, gss_manager).await
        }

        gss_proc::DESTROY => {
            info!("🔐 RPCSEC_GSS_DESTROY");
            gss_manager.handle_destroy(&gss_cred).await;
            // Return success
            GssFront::Reply(vec![ReplyBuilder::success(call.xid).finish().into()])
        }

        _ => {
            warn!("❌ Unknown RPCSEC_GSS procedure: {}", gss_cred.procedure);
            GssFront::Reply(vec![ReplyBuilder::proc_unavail(call.xid).into()])
        }
    }
}

/// RPCSEC_GSS_DATA: the checks, in RFC 2203's order, then the unseal.
async fn gss_data(
    call: &CallMessage,
    gss_cred: &RpcGssCred,
    args: Bytes,
    gss_manager: &RpcSecGssManager,
) -> GssFront {
    // 1. Context lookup, expiry and the base key. NOT the replay
    //    window -- see step 2b.
    let validated = match gss_manager.validate_data(gss_cred).await {
        Ok(v) => v,
        Err(e) => {
            warn!("❌ GSS DATA validation failed: {}", e.reason());
            // CREDPROBLEM/CTXPROBLEM tell the client to refresh and
            // retry. SYSTEM_ERR, which is what this used to send,
            // tells it to give up.
            return GssFront::Reply(vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()]);
        }
    };

    // 1b. The negotiated service against the export's floor.
    //     A context established for svc_none is a real Kerberos
    //     identity and still not privacy; an export that asks
    //     for krb5p must refuse it here, not merely decline to
    //     advertise it.
    {
        let arrived = crate::nfs::sec_policy::SecLevel::of_call(
            AuthFlavor::RpcsecGss,
            Some(validated.service),
        );
        if let crate::nfs::sec_policy::Admission::TooWeak { arrived, floor } =
            crate::nfs::sec_policy::active()
                .admit(arrived, call.procedure == procedure::NULL)
        {
            warn!(
                "🔒 Refusing sec={} call: this export requires at least sec={} ({})",
                arrived.name(),
                floor.name(),
                crate::nfs::sec_policy::SecPolicy::ENV
            );
            return GssFront::Reply(vec![ReplyBuilder::auth_error(call.xid, AuthStat::TooWeak).into()]);
        }
    }

    // 2. The call verifier: a MIC over the header up to and
    //    including the credential (RFC 2203 §5.3.1). Previously
    //    decoded and discarded, so any verifier was accepted.
    if let Err(e) =
        crate::nfs::gss_framing::verify_call_verifier(&validated, &call.verf, &call.cred_span)
    {
        warn!("❌ GSS call verifier rejected: {}", e.reason());
        return GssFront::Reply(vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()]);
    }

    // 2b. ONLY NOW spend the sequence number. RFC 2203 §5.3.3.1
    //     puts the header checksum before the sequence check, and
    //     this used to do the reverse -- inside step 1, where
    //     `verify_sequence` also ADVANCES the window. The wire
    //     drill (tests/krb5/run-gssneg.sh, leg N5) showed what
    //     that bought an attacker: replay a captured record with
    //     its seq_num rewritten to a large number, hold no key at
    //     all, and the MIC check duly rejects the call -- after
    //     the window has already been parked at that number. The
    //     real client's next calls then fall outside it and are
    //     refused as replays. An unauthenticated wedge of a live
    //     mount, costing one packet.
    if let Err(e) = gss_manager.accept_sequence(gss_cred).await {
        warn!("❌ GSS sequence rejected: {}", e.reason());
        return GssFront::Reply(vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()]);
    }

    // 3. Unseal the body BEFORE the COMPOUND is decoded. For
    //    krb5i/krb5p the args are still wrapped at this point.
    let args = match crate::nfs::gss_framing::unseal_call_body(&validated, args) {
        Ok(a) => a,
        Err(e) if e.is_garbage() => {
            warn!("❌ GSS body framing malformed: {}", e.reason());
            return GssFront::Reply(vec![ReplyBuilder::garbage_args(call.xid).into()]);
        }
        Err(e) => {
            warn!("❌ GSS body rejected: {}", e.reason());
            return GssFront::Reply(vec![ReplyBuilder::auth_error(call.xid, e.auth_stat()).into()]);
        }
    };

    GssFront::Data { args, validated }
}

/// The export's minimum security flavor, for a call that is not
/// RPCSEC_GSS (AUTH_NONE or AUTH_SYS; the GSS DATA path checks its own,
/// with the negotiated service): the AUTH_TOOWEAK reply, or None.
///
/// NULL is exempt: it carries no arguments and returns no data, and it is
/// how clients and monitoring probe liveness before they have any
/// credential to offer. Refusing it would break the probe without
/// protecting anything.
pub(crate) fn floor_refusal(call: &CallMessage) -> Option<Segment> {
    let arrived = crate::nfs::sec_policy::SecLevel::of_call(call.cred.flavor, None);
    if let crate::nfs::sec_policy::Admission::TooWeak { arrived, floor } = crate::nfs::sec_policy::active()
        .admit(arrived, call.procedure == procedure::NULL)
    {
        warn!(
            "🔒 Refusing sec={} call: this export requires at least sec={} ({})",
            arrived.name(),
            floor.name(),
            crate::nfs::sec_policy::SecPolicy::ENV
        );
        return Some(ReplyBuilder::auth_error(call.xid, AuthStat::TooWeak).into());
    }
    None
}

/// Frame an accepted RPC reply, adding the RPCSEC_GSS verifier and
/// sealing the body per the negotiated service when the call was
/// authenticated.
///
/// Shared by COMPOUND and NULL deliberately. NULL used to be routed into
/// the COMPOUND decoder on the GSS path, which answered a legal
/// `sess.c.null()` with GARBAGE_ARGS (found by pynfs `--security=krb5`,
/// 2026-08-27); giving the two paths one framing function is what stops
/// the fix from drifting back apart.
pub(crate) fn frame_reply(
    xid: u32,
    gss: Option<&crate::nfs::gss_framing::ValidatedCall>,
    results: Vec<crate::nfs::segment::Segment>,
) -> Vec<crate::nfs::segment::Segment> {
    // The unauthenticated path — every ordinary READ — emits the RPC
    // header as its own segment and passes the body segments through
    // untouched. `append_bytes` below would otherwise copy the whole
    // payload a SECOND time, after the compound encoder already had.
    if gss.is_none() {
        // A body that is ONE segment carried no READ payload — every
        // metadata reply is a handful of words. Segmenting it buys
        // nothing (there is no large copy to avoid) and costs an extra
        // encoder allocation, so take the original flattening path and
        // stay byte-for-byte and allocation-for-allocation identical to
        // what shipped before segmentation existed.
        //
        // This is not a micro-optimisation on a hunch: the first
        // measured run after segmentation showed `read` up but `meta`
        // down 10% with the spread widening from 2.7% to 9.4%, which is
        // what paying for segmentation on replies that cannot benefit
        // looks like.
        if results.len() == 1 {
            let mut reply =
                ReplyBuilder::success_with_verf(xid, &crate::nfs::rpc::Auth::null());
            reply.encoder().append_bytes(results[0].as_mem());
            return vec![reply.finish().into()];
        }
        let header = ReplyBuilder::success_with_verf(xid, &crate::nfs::rpc::Auth::null()).finish();
        let mut segs = Vec::with_capacity(results.len() + 1);
        segs.push(header.into());
        segs.extend(results);
        return segs;
    }

    // GSS binds a MIC over, or seals, the body as one octet stream, so
    // from here it has to be contiguous. krb5 is a correctness path,
    // not the throughput path, and flattening it costs what the old
    // code cost everyone.
    let results: Bytes = if results.len() == 1 {
        results.into_iter().next().expect("len checked").into_mem()
    } else {
        let total: usize = crate::nfs::segment::total_len(&results);
        let mut flat = bytes::BytesMut::with_capacity(total);
        for seg in &results {
            flat.extend_from_slice(seg.as_mem());
        }
        flat.freeze()
    };

    let (verf, body) = match gss {
        None => unreachable!("handled above"),
        Some(v) => {
            let verf = match crate::nfs::gss_framing::reply_verifier(v) {
                Ok(a) => a,
                Err(e) => {
                    warn!("❌ GSS reply verifier failed: {}", e.reason());
                    return vec![ReplyBuilder::auth_error(xid, e.auth_stat()).into()];
                }
            };
            match crate::nfs::gss_framing::seal_reply_body(v, &results) {
                Ok(b) => (verf, b),
                Err(e) => {
                    warn!("❌ GSS reply sealing failed: {}", e.reason());
                    return vec![ReplyBuilder::auth_error(xid, e.auth_stat()).into()];
                }
            }
        }
    };

    let mut reply = ReplyBuilder::success_with_verf(xid, &verf);
    // Per RFC 5531 procedure results are appended directly, no length prefix.
    reply.encoder().append_bytes(&body);
    vec![reply.finish().into()]
}

/// Handle RPCSEC_GSS_INIT
async fn handle_gss_init(
    xid: u32,
    gss_cred: &RpcGssCred,
    args: Bytes,
    gss_manager: &RpcSecGssManager,
) -> Bytes {
    // Extract init token from args
    // In RPCSEC_GSS_INIT, args contains the GSS init token
    let mut decoder = XdrDecoder::new(args);
    let init_token = match decoder.decode_opaque() {
        Ok(token) => token.to_vec(),
        Err(e) => {
            warn!("❌ Failed to decode GSS init token: {}", e);
            return ReplyBuilder::garbage_args(xid);
        }
    };

    info!("🔐 GSS_INIT: service={:?}, token_len={}", gss_cred.service, init_token.len());

    // Handle the initialization
    let init_res = gss_manager.handle_init(gss_cred, &init_token).await;

    // RFC 2203 §5.2.3.1: on GSS_S_COMPLETE the INIT reply verifier is a
    // MIC over the sequence window. Only on complete — any other major
    // status takes a NULL verifier, because there is no context to sign
    // with. This was unconditionally AUTH_NONE.
    let verf = if init_res.major_status == 0 {
        match gss_manager.tokens_for(&init_res.handle).await {
            Some(t) => match crate::nfs::gss_framing::init_reply_verifier(&t, init_res.sequence_window) {
                Ok(a) => a,
                Err(e) => {
                    warn!("❌ GSS INIT verifier failed: {}", e.reason());
                    return ReplyBuilder::auth_error(xid, e.auth_stat());
                }
            },
            // Placeholder mode: established, but holds no key material.
            None => crate::nfs::rpc::Auth::null(),
        }
    } else {
        crate::nfs::rpc::Auth::null()
    };

    let init_result_data = init_res.encode();
    let mut reply = ReplyBuilder::success_with_verf(xid, &verf);
    let encoder = reply.encoder();
    encoder.append_bytes(&init_result_data);

    info!("✅ GSS_INIT complete: handle_len={}, major={}, minor={}",
          init_res.handle.len(), init_res.major_status, init_res.minor_status);

    reply.finish()
}

/// Handle RPCSEC_GSS_CONTINUE_INIT
async fn handle_gss_continue_init(
    xid: u32,
    gss_cred: &RpcGssCred,
    args: Bytes,
    gss_manager: &RpcSecGssManager,
) -> Bytes {
    // Extract continuation token from args
    let mut decoder = XdrDecoder::new(args);
    let token = match decoder.decode_opaque() {
        Ok(t) => t.to_vec(),
        Err(e) => {
            warn!("❌ Failed to decode GSS continue token: {}", e);
            return ReplyBuilder::garbage_args(xid);
        }
    };

    info!("🔐 GSS_CONTINUE_INIT: token_len={}", token.len());

    // Handle the continuation
    let init_res = gss_manager.handle_continue_init(gss_cred, &token).await;

    // Build RPC reply
    let mut encoder = XdrEncoder::new();

    encoder.encode_u32(xid);
    encoder.encode_u32(1);  // REPLY
    encoder.encode_u32(0);  // MSG_ACCEPTED
    encoder.encode_u32(0);  // AUTH_NONE
    encoder.encode_u32(0);
    encoder.encode_u32(0);  // SUCCESS

    let init_result_data = init_res.encode();
    encoder.append_bytes(&init_result_data);

    info!("✅ GSS_CONTINUE_INIT complete: major={}, minor={}",
          init_res.major_status, init_res.minor_status);

    encoder.finish()
}
