//! MAC'd data server filehandles (pNFS Kerberos plan, Phase 4).
//!
//! A files layout names, per data server slot, the filehandle the client
//! sends that DS. The v2 handle (`nfs::v4::filehandle_pnfs`) is
//! `2|instance|file_id|stripe`: anyone who reaches a DS and knows or
//! guesses a file id reads and writes that file's stripes, whatever the
//! MDS would grant them. The DS keeps no table of layouts (a files-layout
//! client sends the DS its open, lock or delegation stateid, RFC 8881
//! §13.9.1, so a table would mirror every OPEN to every DS).
//!
//! Instead the MDS signs the handle it hands out, and the DS checks the
//! signature: version [`VERSION`], 33 bytes,
//! `5 | key_id(4) | file_id(8) | stripe(4) | mac(16)`, with
//! `mac = HMAC-SHA256(key, 5 ‖ key_id ‖ file_id ‖ stripe ‖ node)[..16]`.
//! `node` is the machine host of the session that asked for the layout
//! (`nfs::node_authz::caller`), empty under AUTH_SYS: a handle minted for
//! one node's session does not verify on another node's session at the
//! DS. Version 4 is the MDS's own kernel handles (`fh_kernel`), so this is 5.
//!
//! Keys: [`KEY_ENV`] (current, signs) and [`PREVIOUS_KEY_ENV`] (still
//! verifies), each `<key_id>:<64 hex digits>`. Rotation: move current to
//! previous, set a new current, restart the DSes, then the MDS; layouts
//! minted under a key that is gone get NFS4ERR_BADHANDLE at the DS, and
//! the client fetches a fresh layout (E4, `tests/krb5/results/pnfs-e4-2026-10-16/`).

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The MAC'd DS handle's version byte.
pub const VERSION: u8 = 5;
/// Its length.
pub const LEN: usize = 33;
/// The signing key, `<key_id>:<hex>`.
pub const KEY_ENV: &str = "FLINT_PNFS_FH_KEY";
/// The previous key, still accepted.
pub const PREVIOUS_KEY_ENV: &str = "FLINT_PNFS_FH_KEY_PREVIOUS";
/// DS: `enforce` refuses every handle but a verified v5.
pub const AUTH_ENV: &str = "FLINT_PNFS_DS_FH_AUTH";

const MAC_LEN: usize = 16;

/// One key.
#[derive(Clone, PartialEq, Eq)]
pub struct FhKey {
    pub id: u32,
    secret: Vec<u8>,
}

impl std::fmt::Debug for FhKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FhKey({})", self.id)
    }
}

impl FhKey {
    /// `<key_id>:<hex>`, at least 32 bytes of key.
    pub fn parse(value: &str) -> Result<Self, String> {
        let (id, hex) = value
            .trim()
            .split_once(':')
            .ok_or_else(|| "expected <key_id>:<hex>".to_string())?;
        let id: u32 = id.trim().parse().map_err(|_| format!("key id {id:?} is not a number"))?;
        let hex = hex.trim();
        if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("the key is not hex".into());
        }
        let secret: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked"))
            .collect();
        if secret.len() < 32 {
            return Err(format!("the key is {} bytes; at least 32", secret.len()));
        }
        Ok(Self { id, secret })
    }

    fn mac(&self, file_id: u64, stripe: u32, node: &[u8]) -> Hmac<Sha256> {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.secret).expect("HMAC takes any key length");
        m.update(&[VERSION]);
        m.update(&self.id.to_be_bytes());
        m.update(&file_id.to_be_bytes());
        m.update(&stripe.to_be_bytes());
        m.update(node);
        m
    }
}

/// The current key and the previous one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FhKeys {
    pub current: FhKey,
    pub previous: Option<FhKey>,
}

impl FhKeys {
    /// From already-read values (`None` = unset). No current key is
    /// `Ok(None)`: handles are not signed. A malformed one is an error,
    /// never "unsigned".
    pub fn from_values(current: Option<&str>, previous: Option<&str>) -> Result<Option<Self>, String> {
        let current = match current.map(str::trim).filter(|v| !v.is_empty()) {
            None => {
                if previous.is_some_and(|p| !p.trim().is_empty()) {
                    return Err(format!("{PREVIOUS_KEY_ENV} is set without {KEY_ENV}"));
                }
                return Ok(None);
            }
            Some(v) => FhKey::parse(v).map_err(|e| format!("{KEY_ENV}: {e}"))?,
        };
        let previous = match previous.map(str::trim).filter(|v| !v.is_empty()) {
            None => None,
            Some(v) => Some(FhKey::parse(v).map_err(|e| format!("{PREVIOUS_KEY_ENV}: {e}"))?),
        };
        if previous.as_ref().is_some_and(|p| p.id == current.id) {
            return Err(format!("{KEY_ENV} and {PREVIOUS_KEY_ENV} share key id {}", current.id));
        }
        Ok(Some(Self { current, previous }))
    }

    /// From this process's env.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_values(std::env::var(KEY_ENV).ok().as_deref(), std::env::var(PREVIOUS_KEY_ENV).ok().as_deref())
    }

    /// The handle for `file_id`'s stripe `stripe`, for a layout asked for
    /// on `node`'s session.
    pub fn mint(&self, file_id: u64, stripe: u32, node: &[u8]) -> Vec<u8> {
        let k = &self.current;
        let tag = k.mac(file_id, stripe, node).finalize().into_bytes();
        let mut fh = Vec::with_capacity(LEN);
        fh.push(VERSION);
        fh.extend_from_slice(&k.id.to_be_bytes());
        fh.extend_from_slice(&file_id.to_be_bytes());
        fh.extend_from_slice(&stripe.to_be_bytes());
        fh.extend_from_slice(&tag[..MAC_LEN]);
        fh
    }

    /// `(file_id, stripe)` of a v5 handle that verifies for `node` under
    /// the current or the previous key; `None` otherwise (wrong length or
    /// version, unknown key, bad MAC). The comparison is constant-time.
    pub fn verify(&self, fh: &[u8], node: &[u8]) -> Option<(u64, u32)> {
        if fh.len() != LEN || fh[0] != VERSION {
            return None;
        }
        let key_id = u32::from_be_bytes(fh[1..5].try_into().ok()?);
        let file_id = u64::from_be_bytes(fh[5..13].try_into().ok()?);
        let stripe = u32::from_be_bytes(fh[13..17].try_into().ok()?);
        let key = [Some(&self.current), self.previous.as_ref()]
            .into_iter()
            .flatten()
            .find(|k| k.id == key_id)?;
        key.mac(file_id, stripe, node).verify_truncated_left(&fh[17..]).ok()?;
        Some((file_id, stripe))
    }

    /// Which of `nodes` a v5 handle verifies for. For a refusal's log line
    /// (a handle of one node sent on another node's session), never to
    /// admit one.
    pub fn signed_for<'a>(&self, fh: &[u8], nodes: &'a [String]) -> Option<&'a str> {
        nodes.iter().map(String::as_str).find(|n| self.verify(fh, n.as_bytes()).is_some())
    }
}

/// Whether `fh` is a v5 handle (verified or not).
pub fn is_signed(fh: &[u8]) -> bool {
    fh.first() == Some(&VERSION) && fh.len() == LEN
}

/// The node a session belongs to, for the MAC: its client's machine host
/// (`host/`, `nfs/`, `root/` principals), empty for anything else.
pub fn node_of(client_principal: &[u8]) -> Vec<u8> {
    match crate::nfs::node_authz::caller(client_principal) {
        crate::nfs::node_authz::Caller::Machine(host) => host.into_bytes(),
        _ => Vec::new(),
    }
}

/// [`AUTH_ENV`]: `enforce` (true) or off. A typo is an error.
pub fn parse_auth(value: Option<&str>) -> Result<bool, String> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("off") | Some("false") | Some("0") => Ok(false),
        Some("enforce") | Some("true") | Some("1") | Some("on") => Ok(true),
        Some(other) => Err(format!("{AUTH_ENV}={other:?}: expected enforce or off")),
    }
}

/// A data server's policy for the handles it is sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DsPolicy {
    /// Verifies signed handles; `None`: a signed handle cannot be checked.
    pub keys: Option<FhKeys>,
    /// [`AUTH_ENV`] `enforce`: only a verified signed handle is served.
    pub enforce: bool,
}

impl DsPolicy {
    /// From this process's env; an error for a bad key or mode, and for
    /// `enforce` without a key (it would refuse everything).
    pub fn from_env() -> Result<Self, String> {
        let keys = FhKeys::from_env()?;
        let enforce = parse_auth(std::env::var(AUTH_ENV).ok().as_deref())?;
        if enforce && keys.is_none() {
            return Err(format!("{AUTH_ENV}=enforce needs {KEY_ENV}"));
        }
        Ok(Self { keys, enforce })
    }

    /// What the DS uses for a PUTFH of `fh` on a session whose client was
    /// made by `owner`: the handle to resolve (a verified signed handle as
    /// the v2 handle of the same stripe), or why it is refused
    /// (NFS4ERR_BADHANDLE, which E4 showed the client recovers from with a
    /// fresh layout). A signed handle must verify for the session's node,
    /// whatever the mode; unsigned ones (v2, the MDS's own) only when not
    /// enforcing.
    pub fn admit(&self, fh: &[u8], owner: Option<&[u8]>) -> Result<Vec<u8>, &'static str> {
        if is_signed(fh) {
            let Some(keys) = &self.keys else {
                return Err("a signed handle, and this data server has no key");
            };
            let node = owner.map(node_of).unwrap_or_default();
            let (file_id, stripe) = keys
                .verify(fh, &node)
                .ok_or("a signed handle that does not verify for this session's node")?;
            return Ok(crate::nfs::v4::filehandle_pnfs::generate_pnfs_filehandle_from_id(0, file_id, stripe).data);
        }
        if self.enforce {
            return Err("an unsigned handle (FLINT_PNFS_DS_FH_AUTH=enforce)");
        }
        Ok(fh.to_vec())
    }
}

static DS_POLICY: std::sync::OnceLock<DsPolicy> = std::sync::OnceLock::new();

/// Set this data server's policy (once, at startup).
pub fn init_ds_policy(policy: DsPolicy) {
    let _ = DS_POLICY.set(policy);
}

/// This data server's policy; the default (no keys, not enforcing) before
/// [`init_ds_policy`].
pub fn ds_policy() -> &'static DsPolicy {
    static DEFAULT: std::sync::OnceLock<DsPolicy> = std::sync::OnceLock::new();
    DS_POLICY.get().unwrap_or_else(|| DEFAULT.get_or_init(DsPolicy::default))
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: &str = "1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const K2: &str = "2:ff0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn keys(cur: &str, prev: Option<&str>) -> FhKeys {
        FhKeys::from_values(Some(cur), prev).unwrap().unwrap()
    }

    #[test]
    fn a_handle_verifies_for_its_node_and_nothing_else() {
        let k = keys(K1, None);
        let fh = k.mint(0xdead_beef, 3, b"w1.corp");
        assert_eq!(fh.len(), LEN);
        assert_eq!(fh[0], VERSION);
        assert_eq!(k.verify(&fh, b"w1.corp"), Some((0xdead_beef, 3)));
        assert_eq!(k.verify(&fh, b"w2.corp"), None, "another node's session");
        assert_eq!(k.verify(&fh, b""), None, "an AUTH_SYS session");
        // Every byte matters: file id, stripe, key id, MAC.
        for i in 1..LEN {
            let mut bad = fh.clone();
            bad[i] ^= 1;
            assert_eq!(k.verify(&bad, b"w1.corp"), None, "byte {i} flipped");
        }
        assert_eq!(k.verify(&fh[..32], b"w1.corp"), None, "truncated");
        let v2 = crate::nfs::v4::filehandle_pnfs::generate_pnfs_filehandle_from_id(0, 0xdead_beef, 3);
        assert_eq!(k.verify(&v2.data, b"w1.corp"), None, "an unsigned v2 handle");
    }

    #[test]
    fn a_refused_handle_names_the_node_it_was_signed_for() {
        let k = keys(K1, None);
        let nodes = vec!["w1.corp".to_string(), "w2.corp".to_string()];
        assert_eq!(k.signed_for(&k.mint(7, 0, b"w2.corp"), &nodes), Some("w2.corp"));
        assert_eq!(k.signed_for(&k.mint(7, 0, b"w3.corp"), &nodes), None, "not an attached node");
        assert_eq!(k.signed_for(&keys(K2, None).mint(7, 0, b"w1.corp"), &nodes), None, "another key");
    }

    #[test]
    fn rotation_keeps_the_previous_key_and_drops_the_one_before() {
        let old = keys(K1, None);
        let fh = old.mint(7, 0, b"");
        let rotated = keys(K2, Some(K1));
        assert_eq!(rotated.verify(&fh, b""), Some((7, 0)), "minted under the previous key");
        let fresh = rotated.mint(7, 0, b"");
        assert_ne!(fresh, fh);
        assert_eq!(rotated.verify(&fresh, b""), Some((7, 0)));
        let again = keys("3:aa0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f", Some(K2));
        assert_eq!(again.verify(&fh, b""), None, "key 1 is gone");
        assert_eq!(again.verify(&fresh, b""), Some((7, 0)));
    }

    #[test]
    fn keys_are_validated_never_dropped() {
        assert_eq!(FhKeys::from_values(None, None), Ok(None));
        assert_eq!(FhKeys::from_values(Some(" "), None), Ok(None));
        assert!(FhKeys::from_values(Some("1:abcd"), None).is_err(), "too short");
        assert!(FhKeys::from_values(Some("x:".to_string().as_str()), None).is_err());
        assert!(FhKeys::from_values(Some("1:zz"), None).is_err());
        assert!(FhKeys::from_values(None, Some(K1)).is_err(), "previous without current");
        assert!(FhKeys::from_values(Some(K1), Some(K1)).is_err(), "same key id twice");
        assert_eq!(parse_auth(None), Ok(false));
        assert_eq!(parse_auth(Some("enforce")), Ok(true));
        assert!(parse_auth(Some("enforcing")).is_err());
    }

    #[test]
    fn a_session_node_is_its_machine_host() {
        assert_eq!(node_of(b"gss:host/W1.corp@CORP"), b"w1.corp".to_vec());
        assert_eq!(node_of(b"gss:nfs/w1.corp@CORP"), b"w1.corp".to_vec());
        assert_eq!(node_of(b"gss:alice@CORP"), Vec::<u8>::new());
        assert_eq!(node_of(b"sys:w1:0"), Vec::<u8>::new());
    }

    #[test]
    fn a_data_server_serves_signed_handles_for_their_node_and_unsigned_ones_unless_enforcing() {
        let k = keys(K1, None);
        let w1 = b"gss:nfs/w1.corp@CORP".as_slice();
        let signed = k.mint(9, 1, b"w1.corp");
        let v2 = crate::nfs::v4::filehandle_pnfs::generate_pnfs_filehandle_from_id(0, 9, 1).data;
        let mds = b"\x04some-mds-kernel-handle".to_vec();
        for enforce in [false, true] {
            let p = DsPolicy { keys: Some(k.clone()), enforce };
            assert_eq!(p.admit(&signed, Some(w1)), Ok(v2.clone()), "verified, resolved as v2");
            assert!(p.admit(&signed, Some(b"gss:nfs/w2.corp@CORP")).is_err(), "another node's session");
            assert!(p.admit(&signed, None).is_err(), "no session");
            assert_eq!(p.admit(&v2, Some(w1)).is_ok(), !enforce, "v2: only when not enforcing");
            assert_eq!(p.admit(&mds, Some(w1)).is_ok(), !enforce, "the MDS's handle: only when not enforcing");
        }
        // A signed handle at a DS with no key: refused, never served unchecked.
        let none = DsPolicy::default();
        assert!(none.admit(&signed, Some(w1)).is_err());
        assert_eq!(none.admit(&v2, Some(w1)), Ok(v2.clone()));
    }
}

