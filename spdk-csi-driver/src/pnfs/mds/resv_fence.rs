//! resv_fence — the MDS's own NVMe/TCP initiator lane, reservation
//! commands only. Design doc §5 "Fencing, per the RFC": RFC 9561 §2.2
//! maps SCSI PRs to NVMe Reservations — the MDS registers its own key,
//! holds RTYPE=4h (Exclusive Access – Registrants Only), and fences a
//! client with Reservation Acquire (Preempt) naming the client's key.
//! The client's key is its NFSv4 client id — the same u64 GETDEVICEINFO
//! hands out as `sbv_pr_key` and the kernel registers verbatim
//! (`bl_register_dev` → `ops->pr_register(bdev, 0, pr_key, true)`), so
//! naming a victim needs nothing beyond what the allocator already
//! knows.
//!
//! This is a deliberately minimal NVMe/TCP host: ICReq/ICResp, Fabrics
//! Connect (admin + one I/O queue — each queue is its own TCP
//! connection), controller enable, then Reservation Report / Register /
//! Acquire on the I/O queue. No keep-alive management, no Identify, no
//! multipath — a fence session lives for milliseconds and is torn down
//! by closing the sockets. The admin connection is HELD OPEN while the
//! I/O queue is used: dropping it destroys the controller and the I/O
//! queue with it.
//!
//! Wire layouts are transcribed from `spdk/include/spdk/nvmf_spec.h` and
//! `nvme_spec.h` (v26.05 — the version flint's tgt builds). The unit
//! tests run against a scripted in-process fake speaking the same PDU
//! grammar; the lima fence rig (`tests/lima/pnfs/block-rig.sh FENCE=1`)
//! is the proof against a real tgt — a self-consistent encode/decode bug
//! here cannot survive it.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One reservation-capable NVMe/TCP target namespace, addressed the way
/// the fence lane needs it. `nsid` is 1 by the subsystem-per-volume
/// invariant (each block export carries exactly one namespace); a wrong
/// nsid surfaces as Invalid Namespace on the first command, never as a
/// silent misdirection.
pub struct ResvEndpoint {
    pub traddr: String,
    pub trsvcid: u16,
    pub subnqn: String,
    pub hostnqn: String,
    pub hostid: [u8; 16],
    pub nsid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registrant {
    pub rkey: u64,
    pub hostid: [u8; 16],
    pub holder: bool,
}

/// Parsed Reservation Report (extended data structure — NVMe-oF mandates
/// EDS; SPDK refuses the short form outright).
#[derive(Debug, Clone)]
pub struct ResvView {
    pub generation: u32,
    pub rtype: u8,
    pub ptpl: bool,
    pub registrants: Vec<Registrant>,
}

impl ResvView {
    pub fn has_key(&self, key: u64) -> bool {
        self.registrants.iter().any(|r| r.rkey == key)
    }

    pub fn holder_key(&self) -> Option<u64> {
        self.registrants.iter().find(|r| r.holder).map(|r| r.rkey)
    }

    /// One greppable line — the rig's assertion surface.
    pub fn summary(&self) -> String {
        let keys: Vec<String> = self
            .registrants
            .iter()
            .map(|r| {
                if r.holder {
                    format!("{:#x}(holder)", r.rkey)
                } else {
                    format!("{:#x}", r.rkey)
                }
            })
            .collect();
        format!(
            "gen={} rtype={:#x} ptpl={} keys=[{}]",
            self.generation,
            self.rtype,
            self.ptpl as u8,
            keys.join(", ")
        )
    }
}

/// What a release session actually did. `released=false` is the
/// idempotent no-op: no reservation was held, so there was nothing to
/// release (a replayed release, or a volume whose fence was already
/// lifted).
#[derive(Debug)]
pub struct ReleaseOutcome {
    pub released: bool,
    pub after: ResvView,
}

/// What a fence session actually did — each step is conditional on the
/// reported state, so re-running a fence is idempotent by construction.
#[derive(Debug)]
pub struct FenceOutcome {
    /// Our key was absent and we registered it.
    pub registered: bool,
    /// No reservation was held and we acquired EA-RO (or we preempted a
    /// foreign holder to take it — see the WARN).
    pub acquired: bool,
    /// The victim's key was present and we preempted it away.
    pub preempted: bool,
    pub after: ResvView,
}

/// Exclusive Access – Registrants Only: the RFC 9561 §2.2 reservation
/// type. Non-registrants are refused READs as well as writes.
pub const RTYPE_EA_REG_ONLY: u8 = 0x4;

const PDU_IC_REQ: u8 = 0x00;
const PDU_IC_RESP: u8 = 0x01;
const PDU_C2H_TERM: u8 = 0x03;
const PDU_CAPSULE_CMD: u8 = 0x04;
const PDU_CAPSULE_RESP: u8 = 0x05;
const PDU_C2H_DATA: u8 = 0x07;

const OPC_FABRICS: u8 = 0x7f;
const FCTYPE_PROPERTY_SET: u8 = 0x00;
const FCTYPE_CONNECT: u8 = 0x01;
const FCTYPE_PROPERTY_GET: u8 = 0x04;

const OPC_RESV_REGISTER: u8 = 0x0d;
const OPC_RESV_REPORT: u8 = 0x0e;
const OPC_RESV_ACQUIRE: u8 = 0x11;
const OPC_RESV_RELEASE: u8 = 0x15;

/// SQE byte 1: PSDT=01b — "SGL for data transfer", what the kernel host
/// sets on every NVM command over fabrics.
const FLAGS_SGL: u8 = 0x40;

/// SGL1 descriptor-type byte (SQE offset 39): Data Block + Offset — data
/// travels in-capsule.
const SGL_IN_CAPSULE: u8 = 0x01;
/// Transport Data Block + Transport subtype — data comes back in C2HData
/// PDUs (controller-to-host transfers).
const SGL_TRANSPORT: u8 = 0x5a;

/// C2HData PDU flag: the completion is carried INLINE in this data PDU
/// and no separate CapsuleResp follows (SPDK sets this on the last data
/// PDU of a read/report — `tcp.c` `SPDK_NVME_TCP_C2H_DATA_FLAGS_SUCCESS`,
/// bit 3). Missing this is a hang: the host waits forever for a response
/// capsule the target never sends.
const C2H_DATA_FLAGS_SUCCESS: u8 = 1 << 3;

/// NVMe status codes we name in errors (generic SCT).
const SC_RESERVATION_CONFLICT: u16 = 0x83;

/// Whole-operation deadline. A fence session is a handful of small
/// round-trips on a LAN; anything slower is a wedged tgt and the caller
/// needs the error, not a hang — the functional-fence backstop still
/// runs behind it.
const OP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to poll CSTS.RDY after CC.EN (SPDK flips it on the next
/// property read in practice).
const READY_TRIES: u32 = 50;

// ---------------------------------------------------------------------------
// PDU + SQE builders (pure, unit-tested)
// ---------------------------------------------------------------------------

fn ic_req() -> [u8; 128] {
    let mut p = [0u8; 128];
    p[0] = PDU_IC_REQ;
    p[2] = 128; // hlen
    p[4..8].copy_from_slice(&128u32.to_le_bytes()); // plen
    // pfv=0, hpda=0, no digests, maxr2t=0 — all zero already.
    p
}

/// Capsule = 8-byte common header + 64-byte SQE + optional in-capsule
/// data at `pdo` (aligned per the target's CPDA from ICResp).
fn capsule(sqe: &[u8; 64], data: &[u8], cpda: u8) -> Vec<u8> {
    let hdr_end: usize = 8 + 64;
    let pdo = if data.is_empty() {
        0
    } else {
        let align = 4 * (cpda as usize + 1);
        hdr_end.div_ceil(align) * align
    };
    let plen = if data.is_empty() { hdr_end } else { pdo + data.len() };
    let mut p = vec![0u8; plen];
    p[0] = PDU_CAPSULE_CMD;
    p[2] = hdr_end as u8; // hlen: header + SQE, excluding data
    p[3] = pdo as u8;
    p[4..8].copy_from_slice(&(plen as u32).to_le_bytes());
    p[8..72].copy_from_slice(sqe);
    if !data.is_empty() {
        p[pdo..].copy_from_slice(data);
    }
    p
}

fn sgl_in_capsule(sqe: &mut [u8; 64], len: u32) {
    // addr=0 (offset from start of in-capsule data), length, type byte.
    sqe[32..36].copy_from_slice(&len.to_le_bytes());
    sqe[39] = SGL_IN_CAPSULE;
}

fn sgl_transport(sqe: &mut [u8; 64], len: u32) {
    sqe[32..36].copy_from_slice(&len.to_le_bytes());
    sqe[39] = SGL_TRANSPORT;
}

fn connect_sqe(cid: u16, qid: u16, sqsize: u16, kato_ms: u32) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_FABRICS;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4] = FCTYPE_CONNECT;
    sgl_in_capsule(&mut s, 1024);
    // recfmt=0 @40, qid @42, sqsize @44 (0-based), cattr=0 @46, kato @48.
    s[42..44].copy_from_slice(&qid.to_le_bytes());
    s[44..46].copy_from_slice(&sqsize.to_le_bytes());
    s[48..52].copy_from_slice(&kato_ms.to_le_bytes());
    s
}

fn connect_data(hostid: &[u8; 16], cntlid: u16, subnqn: &str, hostnqn: &str) -> Vec<u8> {
    let mut d = vec![0u8; 1024];
    d[0..16].copy_from_slice(hostid);
    d[16..18].copy_from_slice(&cntlid.to_le_bytes());
    let sub = subnqn.as_bytes();
    let host = hostnqn.as_bytes();
    d[256..256 + sub.len().min(223)].copy_from_slice(&sub[..sub.len().min(223)]);
    d[512..512 + host.len().min(223)].copy_from_slice(&host[..host.len().min(223)]);
    d
}

fn prop_set_sqe(cid: u16, ofst: u32, value: u64, eight_byte: bool) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_FABRICS;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4] = FCTYPE_PROPERTY_SET;
    s[40] = eight_byte as u8; // attrib.size: 0 = 4 bytes, 1 = 8 bytes
    s[44..48].copy_from_slice(&ofst.to_le_bytes());
    s[48..56].copy_from_slice(&value.to_le_bytes());
    s
}

fn prop_get_sqe(cid: u16, ofst: u32, eight_byte: bool) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_FABRICS;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4] = FCTYPE_PROPERTY_GET;
    s[40] = eight_byte as u8;
    s[44..48].copy_from_slice(&ofst.to_le_bytes());
    s
}

fn resv_report_sqe(cid: u16, nsid: u32, buf_len: u32) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_RESV_REPORT;
    s[1] = FLAGS_SGL;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4..8].copy_from_slice(&nsid.to_le_bytes());
    sgl_transport(&mut s, buf_len);
    // cdw10: number of DWORDs to transfer, 0-based.
    s[40..44].copy_from_slice(&(buf_len / 4 - 1).to_le_bytes());
    // cdw11: EDS — NVMe-oF mandates the extended data structure.
    s[44..48].copy_from_slice(&1u32.to_le_bytes());
    s
}

fn resv_register_sqe(cid: u16, nsid: u32) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_RESV_REGISTER;
    s[1] = FLAGS_SGL;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4..8].copy_from_slice(&nsid.to_le_bytes());
    sgl_in_capsule(&mut s, 16);
    // cdw10: RREGA=0 (register), IEKEY=0, CPTPL=11b (persist through
    // power loss — the same persistence the kernel client demands; it
    // is what makes ptpl_file mandatory on the export).
    s[40..44].copy_from_slice(&(0b11u32 << 30).to_le_bytes());
    s
}

fn resv_register_data(crkey: u64, nrkey: u64) -> [u8; 16] {
    let mut d = [0u8; 16];
    d[0..8].copy_from_slice(&crkey.to_le_bytes());
    d[8..16].copy_from_slice(&nrkey.to_le_bytes());
    d
}

/// RACQA 0 = Acquire, 1 = Preempt.
fn resv_acquire_sqe(cid: u16, nsid: u32, racqa: u8, rtype: u8) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_RESV_ACQUIRE;
    s[1] = FLAGS_SGL;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4..8].copy_from_slice(&nsid.to_le_bytes());
    sgl_in_capsule(&mut s, 16);
    let cdw10 = (racqa as u32) | ((rtype as u32) << 8);
    s[40..44].copy_from_slice(&cdw10.to_le_bytes());
    s
}

fn resv_acquire_data(crkey: u64, prkey: u64) -> [u8; 16] {
    let mut d = [0u8; 16];
    d[0..8].copy_from_slice(&crkey.to_le_bytes());
    d[8..16].copy_from_slice(&prkey.to_le_bytes());
    d
}

/// RRELA 0 = Release (1 = Clear, which we never send: Clear also wipes
/// every registration and its blast radius is the whole namespace).
/// `rtype` must NAME the held reservation type — SPDK refuses a
/// mismatch with Invalid Field, so the caller passes the reported one.
/// Data is the 8-byte CRKEY alone (unlike acquire's 16).
fn resv_release_sqe(cid: u16, nsid: u32, rrela: u8, rtype: u8) -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = OPC_RESV_RELEASE;
    s[1] = FLAGS_SGL;
    s[2..4].copy_from_slice(&cid.to_le_bytes());
    s[4..8].copy_from_slice(&nsid.to_le_bytes());
    sgl_in_capsule(&mut s, 8);
    let cdw10 = (rrela as u32) | ((rtype as u32) << 8);
    s[40..44].copy_from_slice(&cdw10.to_le_bytes());
    s
}

fn cqe_status(cqe: &[u8; 16]) -> u16 {
    u16::from_le_bytes([cqe[14], cqe[15]]) >> 1
}

fn cqe_dw0(cqe: &[u8; 16]) -> u32 {
    u32::from_le_bytes([cqe[0], cqe[1], cqe[2], cqe[3]])
}

fn status_err(what: &str, status: u16) -> String {
    let sc = status & 0xff;
    let sct = (status >> 8) & 0x7;
    let name = match (sct, sc) {
        (0, x) if x == SC_RESERVATION_CONFLICT & 0xff => " (RESERVATION CONFLICT)",
        (0, 0x0b) => " (INVALID NAMESPACE)",
        (0, 0x02) => " (INVALID FIELD)",
        _ => "",
    };
    format!("{what}: NVMe status sct={sct:#x} sc={sc:#x}{name}")
}

/// Parse the extended-data-structure Reservation Status page. Layout is
/// packed: gen@0, rtype@4, regctl@5..7, ptpls@9, 64-byte header, then
/// 64-byte registrant entries (cntlid@0, rcsts bit0@2, rkey@8,
/// hostid@16..32).
fn parse_report(buf: &[u8]) -> Result<ResvView, String> {
    if buf.len() < 64 {
        return Err(format!("reservation report truncated: {} bytes", buf.len()));
    }
    let generation = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let rtype = buf[4];
    let regctl = u16::from_le_bytes([buf[5], buf[6]]) as usize;
    let ptpl = buf[9] != 0;
    let mut registrants = Vec::with_capacity(regctl);
    for i in 0..regctl {
        let off = 64 + i * 64;
        if buf.len() < off + 64 {
            return Err(format!(
                "reservation report names {regctl} registrants but carries {}",
                i
            ));
        }
        let e = &buf[off..off + 64];
        let mut hostid = [0u8; 16];
        hostid.copy_from_slice(&e[16..32]);
        registrants.push(Registrant {
            rkey: u64::from_le_bytes(e[8..16].try_into().unwrap()),
            hostid,
            holder: e[2] & 1 != 0,
        });
    }
    Ok(ResvView { generation, rtype, ptpl, registrants })
}

// ---------------------------------------------------------------------------
// The queue: one TCP connection speaking capsules
// ---------------------------------------------------------------------------

struct Queue {
    stream: TcpStream,
    cpda: u8,
    next_cid: u16,
}

impl Queue {
    async fn open(traddr: &str, trsvcid: u16) -> Result<Self, String> {
        let addr = format!("{traddr}:{trsvcid}");
        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("connect {addr}: {e}"))?;
        stream.set_nodelay(true).ok();
        stream
            .write_all(&ic_req())
            .await
            .map_err(|e| format!("ICReq {addr}: {e}"))?;
        let mut resp = [0u8; 128];
        stream
            .read_exact(&mut resp)
            .await
            .map_err(|e| format!("ICResp {addr}: {e}"))?;
        if resp[0] != PDU_IC_RESP {
            return Err(format!("expected ICResp, got PDU type {:#x}", resp[0]));
        }
        if resp[11] & 0b11 != 0 {
            // We offered no digests; a target demanding them is not one
            // of ours.
            return Err("target enabled header/data digests we did not offer".into());
        }
        Ok(Queue { stream, cpda: resp[10], next_cid: 0 })
    }

    /// Send one command capsule, collect C2HData (if any) and the
    /// response capsule. Strictly one command in flight — reservation
    /// sessions have no need for more.
    async fn roundtrip(
        &mut self,
        mut sqe: [u8; 64],
        data_out: &[u8],
    ) -> Result<([u8; 16], Vec<u8>), String> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        let pdu = capsule(&sqe, data_out, self.cpda);
        self.stream
            .write_all(&pdu)
            .await
            .map_err(|e| format!("capsule write: {e}"))?;

        let mut data_in = Vec::new();
        loop {
            let mut common = [0u8; 8];
            self.stream
                .read_exact(&mut common)
                .await
                .map_err(|e| format!("PDU header read: {e}"))?;
            let plen = u32::from_le_bytes([common[4], common[5], common[6], common[7]]) as usize;
            if plen < 8 || plen > 1 << 22 {
                return Err(format!("implausible PDU plen {plen}"));
            }
            let mut rest = vec![0u8; plen - 8];
            self.stream
                .read_exact(&mut rest)
                .await
                .map_err(|e| format!("PDU body read: {e}"))?;
            match common[0] {
                PDU_CAPSULE_RESP => {
                    if rest.len() < 16 {
                        return Err("short CapsuleResp".into());
                    }
                    let mut cqe = [0u8; 16];
                    cqe.copy_from_slice(&rest[0..16]);
                    let got = u16::from_le_bytes([cqe[12], cqe[13]]);
                    if got != cid {
                        return Err(format!("CQE for cid {got}, expected {cid}"));
                    }
                    return Ok((cqe, data_in));
                }
                PDU_C2H_DATA => {
                    // Header: common(8) + cccid(2)@8 + datao(4)@12 +
                    // datal(4)@16. Data sits at `pdo` from the PDU start
                    // and runs `datal` bytes (any trailing padding is
                    // NOT data). `rest` begins at PDU offset 8.
                    let flags = common[1];
                    let pdo = common[3] as usize;
                    if rest.len() < 12 {
                        return Err("short C2HData header".into());
                    }
                    let datal =
                        u32::from_le_bytes([rest[8], rest[9], rest[10], rest[11]]) as usize;
                    if pdo < 8 || pdo - 8 + datal > rest.len() {
                        return Err(format!("C2HData datao/datal out of range (pdo={pdo}, datal={datal})"));
                    }
                    data_in.extend_from_slice(&rest[pdo - 8..pdo - 8 + datal]);
                    // SPDK sets SUCCESS on the last data PDU and sends NO
                    // separate CapsuleResp — the completion is implicit
                    // (all-zero status). Synthesize it and return, or the
                    // read hangs forever waiting for a capsule.
                    if flags & C2H_DATA_FLAGS_SUCCESS != 0 {
                        let cccid = u16::from_le_bytes([rest[0], rest[1]]);
                        if cccid != cid {
                            return Err(format!("C2HData SUCCESS for cccid {cccid}, expected {cid}"));
                        }
                        let mut cqe = [0u8; 16];
                        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
                        return Ok((cqe, data_in));
                    }
                }
                PDU_C2H_TERM => {
                    return Err(format!(
                        "target terminated the connection: FES {:#x}",
                        rest.get(0..2)
                            .map(|f| u16::from_le_bytes([f[0], f[1]]))
                            .unwrap_or(0)
                    ));
                }
                other => return Err(format!("unexpected PDU type {other:#x}")),
            }
        }
    }
}

/// An enabled controller: admin queue held open (dropping it destroys
/// the controller), I/O queue ready for NVM commands.
struct Session {
    _admin: Queue,
    io: Queue,
}

impl ResvEndpoint {
    async fn open_session(&self) -> Result<Session, String> {
        // Admin queue: connect, enable, wait ready.
        let mut admin = Queue::open(&self.traddr, self.trsvcid).await?;
        let (cqe, _) = admin
            .roundtrip(
                connect_sqe(0, 0, 31, 10_000),
                &connect_data(&self.hostid, 0xffff, &self.subnqn, &self.hostnqn),
            )
            .await?;
        let st = cqe_status(&cqe);
        if st != 0 {
            return Err(status_err(
                &format!("admin Connect to {} as {}", self.subnqn, self.hostnqn),
                st,
            ));
        }
        let cntlid = (cqe_dw0(&cqe) & 0xffff) as u16;

        // CC: EN=1, IOSQES=6 (64B), IOCQES=4 (16B), NVM command set.
        let (cqe, _) = admin
            .roundtrip(prop_set_sqe(0, 0x14, 0x0046_0001, false), &[])
            .await?;
        let st = cqe_status(&cqe);
        if st != 0 {
            return Err(status_err("property set CC.EN", st));
        }
        let mut ready = false;
        for _ in 0..READY_TRIES {
            let (cqe, _) = admin.roundtrip(prop_get_sqe(0, 0x1c, false), &[]).await?;
            let st = cqe_status(&cqe);
            if st != 0 {
                return Err(status_err("property get CSTS", st));
            }
            if cqe_dw0(&cqe) & 1 == 1 {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if !ready {
            return Err("controller never reached CSTS.RDY".into());
        }

        // I/O queue: its own TCP connection, bound by cntlid.
        let mut io = Queue::open(&self.traddr, self.trsvcid).await?;
        let (cqe, _) = io
            .roundtrip(
                connect_sqe(0, 1, 31, 0),
                &connect_data(&self.hostid, cntlid, &self.subnqn, &self.hostnqn),
            )
            .await?;
        let st = cqe_status(&cqe);
        if st != 0 {
            return Err(status_err("I/O queue Connect", st));
        }
        Ok(Session { _admin: admin, io })
    }

    async fn report_on(&self, s: &mut Session) -> Result<ResvView, String> {
        let (cqe, data) = s
            .io
            .roundtrip(resv_report_sqe(0, self.nsid, 4096), &[])
            .await?;
        let st = cqe_status(&cqe);
        if st != 0 {
            return Err(status_err("Reservation Report", st));
        }
        parse_report(&data)
    }

    /// Read-only view of the namespace's reservation state.
    pub async fn report(&self) -> Result<ResvView, String> {
        tokio::time::timeout(OP_DEADLINE, async {
            let mut s = self.open_session().await?;
            self.report_on(&mut s).await
        })
        .await
        .map_err(|_| format!("reservation report on {} timed out", self.subnqn))?
    }

    /// THE fence: make `our_key` the EA-RO holder and remove
    /// `victim_key`'s registration. Every step is conditioned on the
    /// reported state, so replays converge. On return the victim is a
    /// non-registrant under an Exclusive Access – Registrants Only
    /// reservation: its very next read or write gets RESERVATION
    /// CONFLICT from the target's per-command check.
    pub async fn fence_preempt(
        &self,
        our_key: u64,
        victim_key: u64,
    ) -> Result<FenceOutcome, String> {
        if victim_key == our_key {
            return Err("refusing to preempt the MDS's own reservation key".into());
        }
        tokio::time::timeout(OP_DEADLINE, self.fence_preempt_inner(our_key, victim_key))
            .await
            .map_err(|_| format!("fence session to {} timed out", self.subnqn))?
    }

    async fn fence_preempt_inner(
        &self,
        our_key: u64,
        victim_key: u64,
    ) -> Result<FenceOutcome, String> {
        tracing::debug!("resv fence {}: opening session", self.subnqn);
        let mut s = self.open_session().await?;
        tracing::debug!("resv fence {}: session up, reading report", self.subnqn);
        let before = self.report_on(&mut s).await?;
        tracing::debug!("resv fence {}: report before = {}", self.subnqn, before.summary());

        let mut registered = false;
        if !before.has_key(our_key) {
            let (cqe, _) = s
                .io
                .roundtrip(
                    resv_register_sqe(0, self.nsid),
                    &resv_register_data(0, our_key),
                )
                .await?;
            let st = cqe_status(&cqe);
            if st != 0 {
                return Err(status_err("Reservation Register (MDS key)", st));
            }
            registered = true;
        }

        let mut acquired = false;
        match before.holder_key() {
            Some(k) if k == our_key => {}
            Some(foreign) => {
                // No conforming client ever acquires (the kernel only
                // registers), so a foreign holder is either a previous
                // MDS identity or an intruder — take the reservation
                // over it, loudly.
                tracing::warn!(
                    "resv fence {}: foreign reservation holder {:#x} — preempting it to \
                     take EA-RO",
                    self.subnqn,
                    foreign
                );
                let (cqe, _) = s
                    .io
                    .roundtrip(
                        resv_acquire_sqe(0, self.nsid, 1, RTYPE_EA_REG_ONLY),
                        &resv_acquire_data(our_key, foreign),
                    )
                    .await?;
                let st = cqe_status(&cqe);
                if st != 0 {
                    return Err(status_err("Reservation Acquire (preempt holder)", st));
                }
                acquired = true;
            }
            None => {
                let (cqe, _) = s
                    .io
                    .roundtrip(
                        resv_acquire_sqe(0, self.nsid, 0, RTYPE_EA_REG_ONLY),
                        &resv_acquire_data(our_key, 0),
                    )
                    .await?;
                let st = cqe_status(&cqe);
                if st != 0 {
                    return Err(status_err("Reservation Acquire (EA-RO)", st));
                }
                acquired = true;
            }
        }

        // The preempt proper — only if the victim actually holds a
        // registration (preempting an unknown key is a spec-defined
        // Reservation Conflict, not a no-op). An unregistered victim is
        // already fenced by the EA-RO reservation above.
        let mut preempted = false;
        if before.has_key(victim_key) && victim_key != our_key {
            let (cqe, _) = s
                .io
                .roundtrip(
                    resv_acquire_sqe(0, self.nsid, 1, RTYPE_EA_REG_ONLY),
                    &resv_acquire_data(our_key, victim_key),
                )
                .await?;
            let st = cqe_status(&cqe);
            if st != 0 {
                return Err(status_err("Reservation Acquire (preempt victim)", st));
            }
            preempted = true;
        }

        let after = self.report_on(&mut s).await?;
        if after.has_key(victim_key) {
            return Err(format!(
                "victim key {victim_key:#x} SURVIVED the preempt — resv state: {}",
                after.summary()
            ));
        }
        if after.holder_key() != Some(our_key) {
            return Err(format!(
                "MDS key {our_key:#x} is not the reservation holder after the fence — \
                 resv state: {}",
                after.summary()
            ));
        }
        Ok(FenceOutcome { registered, acquired, preempted, after })
    }

    /// THE unfence: drop the EA-RO reservation the fence acquired, so
    /// non-registrant I/O (every kernel blocklayout client — none of
    /// them registers a key) flows again. Report-first and conditional,
    /// like the fence: no holder is the idempotent no-op, and a FOREIGN
    /// holder is a loud error rather than a release attempt — SPDK
    /// treats a non-holder's release as a silent no-op, which here
    /// would report "released" over a reservation still standing.
    /// The MDS's registration stays: it costs nothing, keeps the ptpl
    /// entry warm, and the next fence skips its register step.
    pub async fn release(&self, our_key: u64) -> Result<ReleaseOutcome, String> {
        tokio::time::timeout(OP_DEADLINE, self.release_inner(our_key))
            .await
            .map_err(|_| format!("release session to {} timed out", self.subnqn))?
    }

    async fn release_inner(&self, our_key: u64) -> Result<ReleaseOutcome, String> {
        let mut s = self.open_session().await?;
        let before = self.report_on(&mut s).await?;
        tracing::debug!("resv release {}: report before = {}", self.subnqn, before.summary());
        match before.holder_key() {
            None => return Ok(ReleaseOutcome { released: false, after: before }),
            Some(k) if k != our_key => {
                return Err(format!(
                    "reservation holder is {k:#x}, not the MDS — refusing to release a \
                     foreign reservation ({})",
                    before.summary()
                ));
            }
            Some(_) => {}
        }
        // RTYPE must name the held type (SPDK: Invalid Field on
        // mismatch) — take it from the report, not a constant, so a
        // future fence with a different type still releases.
        let (cqe, _) = s
            .io
            .roundtrip(
                resv_release_sqe(0, self.nsid, 0, before.rtype),
                &our_key.to_le_bytes(),
            )
            .await?;
        let st = cqe_status(&cqe);
        if st != 0 {
            return Err(status_err("Reservation Release", st));
        }
        let after = self.report_on(&mut s).await?;
        if after.holder_key().is_some() {
            return Err(format!(
                "reservation STILL HELD after the release — resv state: {}",
                after.summary()
            ));
        }
        Ok(ReleaseOutcome { released: true, after })
    }
}

// ---------------------------------------------------------------------------
// Tests — encoder goldens + a scripted in-process target speaking the
// same PDU grammar. The lima rig against real SPDK is the ground truth;
// these guard regressions in the encode/decode and the fence state
// machine's conditional steps.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn connect_capsule_matches_the_spec_offsets() {
        let sqe = connect_sqe(7, 1, 31, 10_000);
        assert_eq!(sqe[0], 0x7f);
        assert_eq!(sqe[4], 0x01, "fctype connect");
        assert_eq!(u16::from_le_bytes([sqe[2], sqe[3]]), 7);
        assert_eq!(u32::from_le_bytes([sqe[32], sqe[33], sqe[34], sqe[35]]), 1024);
        assert_eq!(sqe[39], 0x01, "in-capsule SGL type byte");
        assert_eq!(u16::from_le_bytes([sqe[42], sqe[43]]), 1, "qid");
        assert_eq!(u16::from_le_bytes([sqe[44], sqe[45]]), 31, "sqsize");
        assert_eq!(u32::from_le_bytes([sqe[48], sqe[49], sqe[50], sqe[51]]), 10_000);

        let data = connect_data(&[0xaa; 16], 0xffff, "nqn.sub", "nqn.host");
        assert_eq!(&data[0..16], &[0xaa; 16]);
        assert_eq!(u16::from_le_bytes([data[16], data[17]]), 0xffff);
        assert_eq!(&data[256..263], b"nqn.sub");
        assert_eq!(&data[512..520], b"nqn.host");
        assert_eq!(data.len(), 1024);

        let pdu = capsule(&sqe, &data, 0);
        assert_eq!(pdu[0], PDU_CAPSULE_CMD);
        assert_eq!(pdu[2], 72, "hlen");
        assert_eq!(pdu[3], 72, "pdo, cpda=0");
        assert_eq!(
            u32::from_le_bytes([pdu[4], pdu[5], pdu[6], pdu[7]]),
            72 + 1024
        );
    }

    #[test]
    fn capsule_honors_cpda_alignment() {
        let sqe = [0u8; 64];
        // cpda=7 → 32-byte alignment → pdo 96.
        let pdu = capsule(&sqe, &[1, 2, 3, 4], 7);
        assert_eq!(pdu[3], 96);
        assert_eq!(pdu.len(), 100);
        assert_eq!(&pdu[96..], &[1, 2, 3, 4]);
        // No data → pdo 0, plen 72.
        let pdu = capsule(&sqe, &[], 7);
        assert_eq!(pdu[3], 0);
        assert_eq!(pdu.len(), 72);
    }

    #[test]
    fn resv_sqes_carry_the_documented_cdws() {
        let reg = resv_register_sqe(1, 1);
        assert_eq!(reg[0], 0x0d);
        assert_eq!(reg[1], 0x40, "PSDT=SGL");
        assert_eq!(
            u32::from_le_bytes([reg[40], reg[41], reg[42], reg[43]]),
            0b11 << 30,
            "RREGA=register, CPTPL=persist"
        );

        let acq = resv_acquire_sqe(2, 1, 1, RTYPE_EA_REG_ONLY);
        assert_eq!(acq[0], 0x11);
        assert_eq!(
            u32::from_le_bytes([acq[40], acq[41], acq[42], acq[43]]),
            1 | (4 << 8),
            "RACQA=preempt, RTYPE=EA-RO"
        );
        let d = resv_acquire_data(0x1111, 0x2222);
        assert_eq!(u64::from_le_bytes(d[0..8].try_into().unwrap()), 0x1111);
        assert_eq!(u64::from_le_bytes(d[8..16].try_into().unwrap()), 0x2222);

        let rep = resv_report_sqe(3, 1, 4096);
        assert_eq!(rep[0], 0x0e);
        assert_eq!(rep[39], 0x5a, "transport SGL for C2H data");
        assert_eq!(
            u32::from_le_bytes([rep[40], rep[41], rep[42], rep[43]]),
            1023,
            "NUMD 0-based"
        );
        assert_eq!(u32::from_le_bytes([rep[44], rep[45], rep[46], rep[47]]), 1, "EDS");

        let rel = resv_release_sqe(4, 1, 0, RTYPE_EA_REG_ONLY);
        assert_eq!(rel[0], 0x15);
        assert_eq!(rel[1], 0x40, "PSDT=SGL");
        assert_eq!(
            u32::from_le_bytes([rel[40], rel[41], rel[42], rel[43]]),
            4 << 8,
            "RRELA=release, IEKEY=0, RTYPE=EA-RO"
        );
        assert_eq!(
            u32::from_le_bytes([rel[32], rel[33], rel[34], rel[35]]),
            8,
            "release data is the 8-byte CRKEY alone"
        );
    }

    #[test]
    fn report_parse_reads_the_packed_layout() {
        let mut buf = vec![0u8; 64 + 2 * 64];
        buf[0..4].copy_from_slice(&7u32.to_le_bytes()); // gen
        buf[4] = 0x4; // rtype
        buf[5..7].copy_from_slice(&2u16.to_le_bytes()); // regctl (packed @5)
        buf[9] = 1; // ptpls
        // Registrant 0: holder, key 0x666c.
        let e0 = 64;
        buf[e0 + 2] = 1;
        buf[e0 + 8..e0 + 16].copy_from_slice(&0x666cu64.to_le_bytes());
        buf[e0 + 16..e0 + 32].copy_from_slice(&[0xbb; 16]);
        // Registrant 1: not holder, key 42.
        let e1 = 128;
        buf[e1 + 8..e1 + 16].copy_from_slice(&42u64.to_le_bytes());

        let v = parse_report(&buf).expect("parses");
        assert_eq!(v.generation, 7);
        assert_eq!(v.rtype, 0x4);
        assert!(v.ptpl);
        assert_eq!(v.registrants.len(), 2);
        assert_eq!(v.holder_key(), Some(0x666c));
        assert!(v.has_key(42));
        assert!(!v.has_key(43));
        assert!(v.summary().contains("0x666c(holder)"));

        // A regctl that promises more than the buffer carries is an
        // error, not a partial parse.
        buf[5..7].copy_from_slice(&9u16.to_le_bytes());
        assert!(parse_report(&buf).is_err());
    }

    #[test]
    fn cqe_status_strips_the_phase_bit() {
        let mut cqe = [0u8; 16];
        // SC 0x83 (reservation conflict), SCT 0 → status field 0x83 << 1.
        cqe[14..16].copy_from_slice(&(0x83u16 << 1).to_le_bytes());
        assert_eq!(cqe_status(&cqe), 0x83);
        assert!(status_err("x", 0x83).contains("RESERVATION CONFLICT"));
    }

    // -- the scripted fake target --------------------------------------------

    /// A minimal in-process NVMe/TCP target: accepts two connections
    /// (admin, io), walks each through IC handshake + connect, answers
    /// property gets with RDY, and serves a scripted reservation state
    /// machine (register/acquire/report against a Vec of registrants).
    /// Shared by block_export's fence tests.
    pub(crate) struct FakeNvmeTarget {
        pub addr: std::net::SocketAddr,
        pub state: std::sync::Arc<std::sync::Mutex<FakeResvState>>,
    }

    #[derive(Default)]
    pub(crate) struct FakeResvState {
        pub registrants: Vec<(u64, [u8; 16], bool)>, // (key, hostid, holder)
        pub rtype: u8,
        pub generation: u32,
        pub hostnqns_seen: Vec<String>,
    }

    impl FakeResvState {
        fn report_bytes(&self) -> Vec<u8> {
            let mut buf = vec![0u8; 64 + self.registrants.len() * 64];
            buf[0..4].copy_from_slice(&self.generation.to_le_bytes());
            buf[4] = self.rtype;
            buf[5..7].copy_from_slice(&(self.registrants.len() as u16).to_le_bytes());
            buf[9] = 1;
            for (i, (key, hostid, holder)) in self.registrants.iter().enumerate() {
                let off = 64 + i * 64;
                buf[off + 2] = *holder as u8;
                buf[off + 8..off + 16].copy_from_slice(&key.to_le_bytes());
                buf[off + 16..off + 32].copy_from_slice(hostid);
            }
            buf
        }
    }

    impl FakeNvmeTarget {
        pub(crate) async fn spawn() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let state = std::sync::Arc::new(std::sync::Mutex::new(FakeResvState::default()));
            let st = state.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { return };
                    let st = st.clone();
                    tokio::spawn(async move {
                        let _ = serve_conn(stream, st).await;
                    });
                }
            });
            FakeNvmeTarget { addr, state }
        }
    }

    async fn serve_conn(
        mut s: tokio::net::TcpStream,
        state: std::sync::Arc<std::sync::Mutex<FakeResvState>>,
    ) -> std::io::Result<()> {
        // IC handshake.
        let mut icreq = [0u8; 128];
        s.read_exact(&mut icreq).await?;
        assert_eq!(icreq[0], PDU_IC_REQ);
        let mut icresp = [0u8; 128];
        icresp[0] = PDU_IC_RESP;
        icresp[2] = 128;
        icresp[4..8].copy_from_slice(&128u32.to_le_bytes());
        // cpda deliberately nonzero: forces the host's alignment path.
        icresp[10] = 1; // 8-byte alignment → pdo stays 72
        s.write_all(&icresp).await?;

        loop {
            let mut common = [0u8; 8];
            if s.read_exact(&mut common).await.is_err() {
                return Ok(());
            }
            assert_eq!(common[0], PDU_CAPSULE_CMD);
            let plen = u32::from_le_bytes(common[4..8].try_into().unwrap()) as usize;
            let mut rest = vec![0u8; plen - 8];
            s.read_exact(&mut rest).await?;
            let sqe: [u8; 64] = rest[0..64].try_into().unwrap();
            let pdo = common[3] as usize;
            let data = if pdo >= 8 { &rest[pdo - 8..] } else { &[][..] };
            let cid = u16::from_le_bytes([sqe[2], sqe[3]]);

            let mut cqe = [0u8; 16];
            cqe[12..14].copy_from_slice(&cid.to_le_bytes());
            let mut c2h: Option<Vec<u8>> = None;

            match (sqe[0], sqe[4]) {
                (OPC_FABRICS, FCTYPE_CONNECT) => {
                    let nqn_end = data[512..768].iter().position(|&b| b == 0).unwrap_or(223);
                    state
                        .lock()
                        .unwrap()
                        .hostnqns_seen
                        .push(String::from_utf8_lossy(&data[512..512 + nqn_end]).into_owned());
                    cqe[0..4].copy_from_slice(&1u32.to_le_bytes()); // cntlid 1
                }
                (OPC_FABRICS, FCTYPE_PROPERTY_SET) => {}
                (OPC_FABRICS, FCTYPE_PROPERTY_GET) => {
                    let ofst = u32::from_le_bytes(sqe[44..48].try_into().unwrap());
                    if ofst == 0x1c {
                        cqe[0..4].copy_from_slice(&1u32.to_le_bytes()); // CSTS.RDY
                    }
                }
                (OPC_RESV_REPORT, _) => {
                    c2h = Some(state.lock().unwrap().report_bytes());
                }
                (OPC_RESV_REGISTER, _) => {
                    let nrkey = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    let mut st = state.lock().unwrap();
                    st.registrants.push((nrkey, [0xdd; 16], false));
                    st.generation += 1;
                }
                (OPC_RESV_ACQUIRE, _) => {
                    let cdw10 = u32::from_le_bytes(sqe[40..44].try_into().unwrap());
                    let racqa = (cdw10 & 0x7) as u8;
                    let rtype = ((cdw10 >> 8) & 0xff) as u8;
                    let crkey = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    let prkey = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    let mut st = state.lock().unwrap();
                    match racqa {
                        0 => {
                            if !st.registrants.iter().any(|(k, _, _)| *k == crkey) {
                                cqe[14..16]
                                    .copy_from_slice(&(SC_RESERVATION_CONFLICT << 1).to_le_bytes());
                            } else {
                                st.rtype = rtype;
                                for r in st.registrants.iter_mut() {
                                    r.2 = r.0 == crkey;
                                }
                                st.generation += 1;
                            }
                        }
                        1 => {
                            if !st.registrants.iter().any(|(k, _, _)| *k == prkey) {
                                // Spec: preempting an unknown key is a
                                // conflict — the host must not rely on
                                // it as a no-op.
                                cqe[14..16]
                                    .copy_from_slice(&(SC_RESERVATION_CONFLICT << 1).to_le_bytes());
                            } else {
                                let was_holder =
                                    st.registrants.iter().any(|(k, _, h)| *k == prkey && *h);
                                st.registrants.retain(|(k, _, _)| *k != prkey);
                                if was_holder {
                                    st.rtype = rtype;
                                    for r in st.registrants.iter_mut() {
                                        r.2 = r.0 == crkey;
                                    }
                                }
                                st.generation += 1;
                            }
                        }
                        _ => cqe[14..16].copy_from_slice(&(0x02u16 << 1).to_le_bytes()),
                    }
                }
                (OPC_RESV_RELEASE, _) => {
                    // Mirrors SPDK nvmf_ns_reservation_release: CRKEY
                    // must match a registrant (else conflict), RTYPE
                    // must match the held type (else invalid field),
                    // no-holder release is a success no-op, and only
                    // the holder's release clears the reservation.
                    let cdw10 = u32::from_le_bytes(sqe[40..44].try_into().unwrap());
                    let rrela = (cdw10 & 0x7) as u8;
                    let rtype = ((cdw10 >> 8) & 0xff) as u8;
                    let crkey = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    let mut st = state.lock().unwrap();
                    if rrela != 0 {
                        cqe[14..16].copy_from_slice(&(0x02u16 << 1).to_le_bytes());
                    } else if !st.registrants.iter().any(|(k, _, _)| *k == crkey) {
                        cqe[14..16]
                            .copy_from_slice(&(SC_RESERVATION_CONFLICT << 1).to_le_bytes());
                    } else if st.registrants.iter().any(|(_, _, h)| *h) {
                        if rtype != st.rtype {
                            cqe[14..16].copy_from_slice(&(0x02u16 << 1).to_le_bytes());
                        } else if st.registrants.iter().any(|(k, _, h)| *k == crkey && *h) {
                            st.rtype = 0;
                            for r in st.registrants.iter_mut() {
                                r.2 = false;
                            }
                        }
                        // Non-holder registrant: success no-op (SPDK:
                        // "not the reservation holder, this isn't an
                        // error").
                    }
                    // No holder at all: success no-op.
                }
                _ => cqe[14..16].copy_from_slice(&(0x01u16 << 1).to_le_bytes()),
            }

            if let Some(body) = c2h {
                // Real SPDK sets SUCCESS on the last data PDU and sends
                // NO trailing CapsuleResp — model that exactly, or the
                // host's inline-completion path goes untested (the very
                // bug the lima rig caught: a hang on the report).
                let mut hdr = [0u8; 24];
                hdr[0] = PDU_C2H_DATA;
                hdr[1] = C2H_DATA_FLAGS_SUCCESS | (1 << 2); // SUCCESS | LAST_PDU
                hdr[2] = 24;
                hdr[3] = 24; // pdo
                hdr[4..8].copy_from_slice(&((24 + body.len()) as u32).to_le_bytes());
                hdr[8..10].copy_from_slice(&cid.to_le_bytes()); // cccid
                hdr[16..20].copy_from_slice(&(body.len() as u32).to_le_bytes()); // datal
                s.write_all(&hdr).await?;
                s.write_all(&body).await?;
            } else {
                let mut resp = [0u8; 24];
                resp[0] = PDU_CAPSULE_RESP;
                resp[2] = 24;
                resp[4..8].copy_from_slice(&24u32.to_le_bytes());
                resp[8..24].copy_from_slice(&cqe);
                s.write_all(&resp).await?;
            }
        }
    }

    fn endpoint(addr: std::net::SocketAddr) -> ResvEndpoint {
        ResvEndpoint {
            traddr: addr.ip().to_string(),
            trsvcid: addr.port(),
            subnqn: "nqn.2024-11.com.flint:block:pvc-t".into(),
            hostnqn: crate::identity::block_mds_host_nqn(),
            hostid: crate::identity::BLOCK_MDS_HOST_ID,
            nsid: 1,
        }
    }

    #[tokio::test]
    async fn fence_registers_acquires_and_preempts_the_victim() {
        let tgt = FakeNvmeTarget::spawn().await;
        // The victim (client id 42) is registered, kernel-style; nobody
        // holds a reservation.
        tgt.state.lock().unwrap().registrants.push((42, [0xcc; 16], false));

        let out = endpoint(tgt.addr)
            .fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, 42)
            .await
            .expect("fence converges");
        assert!(out.registered && out.acquired && out.preempted);
        assert!(!out.after.has_key(42), "victim gone");
        assert_eq!(out.after.holder_key(), Some(crate::identity::BLOCK_MDS_PR_KEY));
        assert_eq!(out.after.rtype, RTYPE_EA_REG_ONLY);
        // The fence lane identified itself as the MDS host.
        assert!(tgt
            .state
            .lock()
            .unwrap()
            .hostnqns_seen
            .iter()
            .all(|n| n == &crate::identity::block_mds_host_nqn()));
    }

    #[tokio::test]
    async fn fence_replay_is_idempotent_and_unregistered_victim_is_skipped() {
        let tgt = FakeNvmeTarget::spawn().await;
        tgt.state.lock().unwrap().registrants.push((42, [0xcc; 16], false));

        let ep = endpoint(tgt.addr);
        ep.fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, 42).await.expect("first");
        // Replay: our key present, EA-RO ours, victim gone — every step
        // must skip rather than error (preempting an absent key is a
        // spec Reservation Conflict, which the report-first flow avoids).
        let out = ep
            .fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, 42)
            .await
            .expect("replay converges");
        assert!(!out.registered && !out.acquired && !out.preempted);
        assert_eq!(out.after.holder_key(), Some(crate::identity::BLOCK_MDS_PR_KEY));
    }

    #[tokio::test]
    async fn fence_refuses_to_preempt_its_own_key() {
        let tgt = FakeNvmeTarget::spawn().await;
        let err = endpoint(tgt.addr)
            .fence_preempt(
                crate::identity::BLOCK_MDS_PR_KEY,
                crate::identity::BLOCK_MDS_PR_KEY,
            )
            .await
            .unwrap_err();
        assert!(err.contains("own reservation key"), "{err}");
    }

    #[tokio::test]
    async fn release_after_a_fence_drops_the_reservation_and_keeps_the_registration() {
        let tgt = FakeNvmeTarget::spawn().await;
        tgt.state.lock().unwrap().registrants.push((42, [0xcc; 16], false));

        let ep = endpoint(tgt.addr);
        ep.fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, 42).await.expect("fence");

        let out = ep.release(crate::identity::BLOCK_MDS_PR_KEY).await.expect("release");
        assert!(out.released);
        assert_eq!(out.after.holder_key(), None, "no holder after the release");
        assert!(
            out.after.has_key(crate::identity::BLOCK_MDS_PR_KEY),
            "the MDS registration STAYS — only the reservation goes"
        );
        assert_eq!(out.after.rtype, 0);

        // Replay: nothing held → the idempotent no-op.
        let again = ep.release(crate::identity::BLOCK_MDS_PR_KEY).await.expect("replay");
        assert!(!again.released);
        assert_eq!(again.after.holder_key(), None);
    }

    #[tokio::test]
    async fn release_refuses_a_foreign_holder() {
        let tgt = FakeNvmeTarget::spawn().await;
        {
            let mut st = tgt.state.lock().unwrap();
            st.registrants.push((0xbeef, [0xee; 16], true));
            st.rtype = RTYPE_EA_REG_ONLY;
        }
        let err = endpoint(tgt.addr)
            .release(crate::identity::BLOCK_MDS_PR_KEY)
            .await
            .unwrap_err();
        assert!(err.contains("foreign"), "{err}");
        // And it did not touch the foreign reservation.
        let st = tgt.state.lock().unwrap();
        assert!(st.registrants.iter().any(|(k, _, h)| *k == 0xbeef && *h));
    }

    #[tokio::test]
    async fn a_second_fence_names_a_second_victim_without_touching_the_first_state() {
        let tgt = FakeNvmeTarget::spawn().await;
        {
            let mut st = tgt.state.lock().unwrap();
            st.registrants.push((42, [0xcc; 16], false));
            st.registrants.push((43, [0xce; 16], false));
        }
        let ep = endpoint(tgt.addr);
        ep.fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, 42).await.expect("fence 42");
        let out = ep.fence_preempt(crate::identity::BLOCK_MDS_PR_KEY, 43).await.expect("fence 43");
        assert!(out.preempted && !out.registered && !out.acquired);
        assert!(!out.after.has_key(42) && !out.after.has_key(43));
        assert_eq!(out.after.holder_key(), Some(crate::identity::BLOCK_MDS_PR_KEY));
    }
}
