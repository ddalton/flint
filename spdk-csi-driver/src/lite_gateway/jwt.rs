//! A JWT an external issuer minted, verified at the door
//! (`docs/plans/forge-knox-jwt-design.md`).
//!
//! The door is a RESOURCE SERVER and nothing else. It never redirects,
//! holds no client secret, keeps no session and performs no code
//! exchange; whatever dance an application does with its auth service
//! and the issuer is that application's business. What arrives here is
//! a bearer token, and the only question is whether it is genuine and
//! who it names.
//!
//! ## Offline, and uncached
//!
//! `CachingReviewer` exists to spare the apiserver a round trip.
//! Signature verification has no round trip to spare — it is
//! microseconds of arithmetic against a key already in memory — so a
//! verdict cache would buy nothing and cost the one bound that matters:
//! with revocation out of scope, `exp` is the only thing that ends a
//! session, and a cached verdict honours it up to a TTL late. This
//! reviewer is therefore deliberately NOT wrapped. The signing KEYS are
//! cached; the verdicts never are.
//!
//! ## What a signature does and does not prove
//!
//! It proves the issuer said this. Where the issuer mints with an
//! impersonation grant, what the issuer said rests in turn on whoever
//! asked it to — so the door's verification is genuine and its
//! *meaning* is set upstream, by controls forge cannot see and does not
//! implement. Design §7.1 states them as preconditions for that reason.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::s3csi::broker::Identity;

use super::git::{ReviewError, Reviewer};

/// How long the door will believe a fetched key set before it may
/// refetch on an unknown `kid`. A floor, not a schedule: without it a
/// stream of tokens naming keys that do not exist is a denial-of-service
/// against the issuer, delivered by the door.
pub const JWKS_REFETCH_FLOOR: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// The `iss` this reviewer accepts, matched EXACTLY. Also what the
    /// router keys on — see `git::RoutingReviewer`.
    pub issuer: String,
    /// The audience required in `aud`, when the deployment sets one.
    ///
    /// `None` means the check is skipped, and the door says so at
    /// start-up. Optional because the issuer may not be able to mint a
    /// per-application audience — a topology-wide value, or none at
    /// all, is a configuration this door does not control. What that
    /// costs is design D6: a shared audience does not separate this
    /// service from its siblings, so any of them holding a user's token
    /// can replay it here as that user.
    pub audience: Option<String>,
    /// The longest `exp - iat` the door will accept, whatever the
    /// signature says.
    ///
    /// A token's lifetime is set by its issuer and some issuers default
    /// to months. With revocation out of scope this is the only bound
    /// on a stolen token, so the door refuses to be the place that
    /// assumption goes unchecked.
    pub max_lifetime: Duration,
    /// Tolerance for clock skew on `exp` and `nbf`.
    pub leeway: Duration,
}

impl Default for JwtConfig {
    fn default() -> Self {
        JwtConfig {
            issuer: String::new(),
            audience: None,
            max_lifetime: Duration::from_secs(3600),
            leeway: Duration::from_secs(60),
        }
    }
}

/// Where the signing keys come from.
pub enum Keys {
    /// A JWKS endpoint, fetched and cached, refetched when a token
    /// names a `kid` that is not held.
    Jwks { url: String, http: reqwest::Client, cache: Mutex<KeyCache> },
    /// Keys configured directly, for a deployment that would rather the
    /// door never reached the issuer at all.
    Static(HashMap<String, DecodingKey>),
}

#[derive(Default)]
pub struct KeyCache {
    pub keys: HashMap<String, DecodingKey>,
    pub fetched: Option<Instant>,
}

/// One JWKS key, in the subset the door needs. RSA only: the issuers
/// this was written for sign RS256, and a key type the door cannot
/// verify is better skipped at parse time than mis-verified later.
#[derive(Debug, Deserialize)]
struct JwkKey {
    kid: Option<String>,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<JwkKey>,
}

fn parse_jwks(body: &str) -> Result<HashMap<String, DecodingKey>, String> {
    let set: JwkSet =
        serde_json::from_str(body).map_err(|e| format!("JWKS is not a key set: {e}"))?;
    let mut out = HashMap::new();
    for k in set.keys {
        if k.kty != "RSA" {
            continue;
        }
        if let Some(alg) = k.alg.as_deref() {
            if !alg.starts_with("RS") {
                continue;
            }
        }
        let (Some(n), Some(e), Some(kid)) = (k.n.as_deref(), k.e.as_deref(), k.kid) else {
            continue;
        };
        match DecodingKey::from_rsa_components(n, e) {
            Ok(key) => {
                out.insert(kid, key);
            }
            Err(e) => return Err(format!("JWKS carries an unusable RSA key: {e}")),
        }
    }
    if out.is_empty() {
        return Err("JWKS carried no usable RSA signing key".into());
    }
    Ok(out)
}

/// The claims the door reads. Everything else in the token is ignored,
/// which is deliberate: a claim nobody reads cannot become an
/// accidental authorization input.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    iat: Option<i64>,
    exp: i64,
}

/// Just enough of a token to route it, WITHOUT verifying anything.
///
/// Used only to choose which verifier should look at it. Nothing this
/// returns is trusted — see `git::RoutingReviewer`, which documents why
/// reading an unverified claim is safe here and what would make it
/// unsafe.
pub fn unverified_issuer(token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Iss {
        iss: String,
    }
    let payload = token.split('.').nth(1)?;
    let raw = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload,
    )
    .ok()?;
    serde_json::from_slice::<Iss>(&raw).ok().map(|i| i.iss)
}

pub struct JwtReviewer {
    pub cfg: JwtConfig,
    pub keys: Keys,
}

impl JwtReviewer {
    pub fn with_jwks(cfg: JwtConfig, url: String, http: reqwest::Client) -> Arc<Self> {
        Arc::new(JwtReviewer {
            cfg,
            keys: Keys::Jwks { url, http, cache: Mutex::new(KeyCache::default()) },
        })
    }

    pub fn with_static_pem(cfg: JwtConfig, kid: &str, pem: &[u8]) -> Result<Arc<Self>, String> {
        let key = DecodingKey::from_rsa_pem(pem)
            .map_err(|e| format!("the configured public key is not an RSA PEM: {e}"))?;
        let mut m = HashMap::new();
        m.insert(kid.to_string(), key);
        Ok(Arc::new(JwtReviewer { cfg, keys: Keys::Static(m) }))
    }

    fn cached_key(&self, kid: &str) -> Option<DecodingKey> {
        match &self.keys {
            Keys::Static(m) => m.get(kid).cloned(),
            Keys::Jwks { cache, .. } => {
                cache.lock().ok().and_then(|c| c.keys.get(kid).cloned())
            }
        }
    }

    /// Fetch the key set, honouring the refetch floor.
    ///
    /// A failure here is [`ReviewError::Unreachable`] and never
    /// `Refused`: "I could not reach the key server" says nothing about
    /// the caller's credential, and answering as though it did sends
    /// them to rotate one that was fine.
    async fn refresh(&self) -> Result<(), ReviewError> {
        let Keys::Jwks { url, http, cache } = &self.keys else {
            return Ok(());
        };
        if let Ok(c) = cache.lock() {
            if let Some(at) = c.fetched {
                if at.elapsed() < JWKS_REFETCH_FLOOR {
                    // Recently fetched and the kid is still unknown.
                    // Refusing here rather than refetching is what stops
                    // a stream of unknown-kid tokens from becoming a
                    // denial-of-service against the issuer.
                    return Err(ReviewError::Refused(
                        "the token names a signing key this issuer's key set does not contain"
                            .into(),
                    ));
                }
            }
        }
        let res = http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| ReviewError::Unreachable(format!("jwks: {e}")))?;
        if !res.status().is_success() {
            return Err(ReviewError::Unreachable(format!(
                "jwks: the key server answered {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ReviewError::Unreachable(format!("jwks: {e}")))?;
        let keys = parse_jwks(&body).map_err(ReviewError::Unreachable)?;
        if let Ok(mut c) = cache.lock() {
            c.keys = keys;
            c.fetched = Some(Instant::now());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Reviewer for JwtReviewer {
    async fn review(&self, token: &str) -> Result<Identity, ReviewError> {
        let header = decode_header(token)
            .map_err(|e| ReviewError::Refused(format!("not a usable JWT: {e}")))?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512) {
            // An algorithm allowlist, and `none`/HMAC are the reason it
            // exists: a token that names its own algorithm can otherwise
            // choose one the verifier will accept with the wrong key.
            return Err(ReviewError::Refused(format!(
                "{:?} is not an accepted signing algorithm",
                header.alg
            )));
        }
        let Some(kid) = header.kid.clone() else {
            return Err(ReviewError::Refused("the token names no signing key (kid)".into()));
        };

        let key = match self.cached_key(&kid) {
            Some(k) => k,
            None => {
                self.refresh().await?;
                self.cached_key(&kid).ok_or_else(|| {
                    ReviewError::Refused(
                        "the token names a signing key this issuer's key set does not contain"
                            .into(),
                    )
                })?
            }
        };

        let mut v = Validation::new(header.alg);
        v.set_issuer(&[self.cfg.issuer.as_str()]);
        v.leeway = self.cfg.leeway.as_secs();
        match self.cfg.audience.as_deref() {
            Some(aud) => v.set_audience(&[aud]),
            // Not merely "no audience configured": `jsonwebtoken`
            // validates `aud` by DEFAULT once a token carries one, and
            // would refuse every token here. Skipping is the explicit
            // posture the door warns about at start-up.
            None => v.validate_aud = false,
        }
        v.validate_exp = true;
        v.validate_nbf = true;

        let data = decode::<Claims>(token, &key, &v)
            .map_err(|e| ReviewError::Refused(format!("token rejected: {e}")))?;

        if data.claims.sub.trim().is_empty() {
            return Err(ReviewError::Refused("the token names no subject".into()));
        }
        // The lifetime ceiling. Checked from the CLAIMS rather than from
        // the wire, so a token minted with a months-long life is refused
        // on its first use and not merely when it eventually expires.
        if let Some(iat) = data.claims.iat {
            let life = data.claims.exp.saturating_sub(iat);
            if life > self.cfg.max_lifetime.as_secs() as i64 {
                return Err(ReviewError::Refused(format!(
                    "the token's lifetime is {life}s, longer than this door accepts ({}s)",
                    self.cfg.max_lifetime.as_secs()
                )));
            }
        }
        Ok(Identity::person(data.claims.sub))
    }
}

#[cfg(test)]
mod tests {
    //! Signed tokens, real keys, real verification.
    //!
    //! An RSA keypair is generated once and used to mint tokens whose
    //! claims each test bends one at a time. Nothing here asserts an
    //! absence without a positive control minted from the SAME key, so
    //! "it was refused" can never be a rig that refuses everything.

    use super::*;
    use crate::s3csi::broker::Vouched;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;

    /// A FIXED test keypair, and a second private key that nothing
    /// trusts.
    ///
    /// **These are test fixtures and nothing else.** They are in the
    /// repository, they are public, and any deployment that used them
    /// would be trivially forgeable. Fixed rather than generated
    /// because a 2048-bit keygen per test is seconds of nothing, and
    /// because a signature test wants the same key every run.
    const TEST_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDTiNFUOFMMk0XN
iLZcsWZJl4PG6ylguutPeEU2UbQlS4ytjq9MNsq2ZyoBKeCuSc4QCxCGV0rJHYuT
U4286oaGHolh73jpW4EWELNWzqXeE38rWDXtIbil7JhqWTh6u6AovuoFOX9FqWxU
qWiCbpto8jj4zHbeAkOqLT66jNyUT+PM6D1PKMAg3uLmpKyOexgeaJpQn+TZs+FA
vJtn25grIiBm8hY7teseOtrLG2Zrtc3QF3evnTu7D18x79/pxCi64oKLTrjCekaH
nK1V2XvM3evHiRw24XKONqmovoC9HJ0ne83u1lvY8K3utcuO607pejRNq95haHhA
PjC2GbtdAgMBAAECggEAA59O6FiLkYZPFnEuIEG4bO+vhb8+puWkhGiclMzK3y2f
Q9SS62TvzTZQiZMQQiPI58zstEQ352A+ZOA0J+VDNvY8Z4UshqB2wiw4ifbBb99Z
GOKqj7w5V3wI8x9CgJWIbVIxmzPMGmMHlB6Ph9ZBiodFUvtbWLtksbwTHCov18lL
b1ap/wZNQ/G75qhd0RXL9/z0CqhxrgF654upCgInd+7QQDe6mB9Y6wQ1UUiv+W75
Tw8eMted5VT3QRrIk/xzR23PlSFH1kbVCNrDO94aPHGgjFwNohCy5IKFZxtiOjlX
VqqYcPf6+l/QXLPF/y0Km29+VJFhpB0A2d1HsfBpEQKBgQD9e20p8GIes6RsQzn2
pGrDY5RBfLnHAI+kbO4eibB18o6lKPAohWnY5n38q0mol1ocZTo/pX8jgTtVyY1S
xiF/IvUvllc1/4sSMB/RPi1U20H6m0w7FdpYSwalxFCH04Kn5PWk/ve93FbwzmCM
ESH6sSZAhdQhKSKO9Dk/if3msQKBgQDVork3mOB0i9pKwFOBhC90xZGPYOBHmrdV
g8f3nHweforioJ0z1XKrinT+LQx6kMqdZJuT3TisK0K1gMjRgvrwVgnzGxfL29Ex
j9InZarftHLwxtF/LzWIgfA4cXPFGzNK+L6X5j/raEKo4KiT9jiym2uO8Mw6kbew
3n69rssibQKBgDaLFoRNu29LzHeXR6Ow4WBFzyMASaFul3oUDnD3w8a9eMBFPNgb
TRllD3sNCH6EgtlVVuFXJTJonnHpOsWy6IZI6WVh/kYaRLyXKmGF9Y8q1tmsDQ0x
uJgDHN0SjxmLA7RI6iqkyn5KKVMLtW6uSRd+gvKjWXABP/RuzNrFQKMhAoGBAIw5
wAs/PG0jcwhHz0gfBKtIFzAebXhylE38LuBXhZzagL0aobTpMNhqDDreROeabHP9
GqVmupE/4AyU2Lu0lpP0VZmNugPkaB55AX88m3k0z5E9Xzt1OFU+vPe/eDbzkKpw
NWItDt2s1LxWojBkmHibzXDIm7UB+qmMkXJd7hXNAoGAXBflvAqYZX1jlkvQStu7
YP2QzuEcFAxi8q5loQZsf5FfA/usV2E14DHsv6LwBSaM13lPAnwjuwks6qysfOeT
h64Z7EVQX6PtWTp3Sutt+ZNLQJto7aSrJzP6lvmh6S53bBvI4GmSJLbjBmH4eHSi
8VlKaD4+Mw8mJB/EO3uwdI0=
-----END PRIVATE KEY-----";
    const TEST_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA04jRVDhTDJNFzYi2XLFm
SZeDxuspYLrrT3hFNlG0JUuMrY6vTDbKtmcqASngrknOEAsQhldKyR2Lk1ONvOqG
hh6JYe946VuBFhCzVs6l3hN/K1g17SG4peyYalk4erugKL7qBTl/RalsVKlogm6b
aPI4+Mx23gJDqi0+uozclE/jzOg9TyjAIN7i5qSsjnsYHmiaUJ/k2bPhQLybZ9uY
KyIgZvIWO7XrHjrayxtma7XN0Bd3r507uw9fMe/f6cQouuKCi064wnpGh5ytVdl7
zN3rx4kcNuFyjjapqL6AvRydJ3vN7tZb2PCt7rXLjutO6Xo0TaveYWh4QD4wthm7
XQIDAQAB
-----END PUBLIC KEY-----";
    /// A well-formed key the reviewer has never been given.
    const OTHER_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChMSZdDqf6/qhc
pjTcCDTLsCHuuWh++xW5WmgoaVOfJ1/hrte/vr8ZVsZC4UZB+f40Iz2YV+SRl0sV
vzJIlot0aBgP2b1BAXiUtARf233sAmsyLr0LTecQHDoDFli5h7OSbaM9xacNEjrQ
27Ns1W8Bdvn8c5OTMCfcE7sZS5cVAvvJXJ0f3NmVXW1dnhIMMKKQIBJgH98oSWRE
rtbfd72ctH49OWXJtM+8ysW82p74VwIf5hahJpSiOY8SuhPiqFZT1CRPEIWUqONs
AEkrLPfhCtNdzKUptKKL+OaKyP4inPVGBs/v2g19h0zXSTauc9zJud7CvFKVrL9d
s0WE/XF9AgMBAAECggEAJjbRwoQD/PQ8k+Jva66iXZu/H6pjBJ+gEdZGFTaLNZMP
HyDlUzb0dRxmWlqv3lpXEqM8Kg7ESGPW0CwIzr6qBwnakn9rZ6rinFZlJxiRLM+R
/E6qULDCU8ZtVmgI1ss+HjvR1IY0SVwGB5feXLHo5C8BqwD3fpCPEpS26ZNLGRbA
fHcx2vgWk5okc9l9QMM/0KJESwe7/Y6sEy05p0EEMDbqWuIe1sMxGVjhTJNOv2Sh
bVjCuXDDu8sqzFAZ1zXTaxxtsHAkyCQOI01VgoNIIGqXYuTcs0KXfTH5Oq9gqJYu
fWO6EtBMKCHA0DCLlvXHJcTwBIyrnRb3AYOOLjlCnQKBgQDYA0qtrYI6UiUmjIFY
V/cXFXgeY5BW0xtsHzSxcXlHu76dMOOtc3ZvTZfVKuYbPEQeAYSY0rn4ODUW7UNu
EA62S/w5MN0P725qRLZSNLypATecO7ikajV50kklD7anPMEe8C3U4fNNpLlQX0Pb
V9ooTtMTEkrO3SArMtjUzs2c6wKBgQC/B+7eD5wGqNp087blqPJF7refq36PpMSf
a9xu4BWzsiXuuix3fHPttsp/x2mPeJhW8iM1PT04igAkhMiNJtn5Hykej4wzMJeD
XPqhy+6Xm52i1X1pY+GDYloV/vEiDrDdT+VY3UF4nFHeDbkjheY0erPR6NDQSuLy
1c6qFmdxNwKBgDGdh/CectQMfCX/jdIJ2mI99yobulKHCLxr6oF7S90THXQjf5ge
diyYiPBYeyP0Ur0Fojwr4rVFy8PpWVyVaZurllJYi94WI6lbAPmezVqQQgKroPx6
vK+vgkd19YEyLjV5+zzzbRv/YuU4DHD8G8q4WDkAMZiUJ8hkVHOE0KtjAoGBAIoB
vy2p9TxSbFAlabKM1Up0ZS/zAyHfFTVfBQcM2GDTiNfopAtGW7IWZkDd3YMKynO1
xn3F2h6og+XeD8z5jmuNeXVcmxq4Nh1u5JpS6/GXONDUjx++SsNSIGbXoXjLSDb6
a2RBo/TwaayUGXZyW5b6NkKlgYWZNE/e6siyGtUNAoGAStqK59d3OK0TM7cCR6vW
eg8ebHnc2frX1zaDJafYqxylKL06G2ihwxUN3C6a2OmtE4CCx8ogUFyRQGMoof/B
PFQp/OBfI98S/jjXDQAl8FWCTHaNezVYkNfDKgOzyCl835mbmRoaCGdNqjVwNPlf
FPR3Nl+Roh3riCJV1QReFzE=
-----END PRIVATE KEY-----";

    fn keypair() -> (EncodingKey, Vec<u8>) {
        (
            EncodingKey::from_rsa_pem(TEST_PRIV_PEM.as_bytes()).expect("enc key"),
            TEST_PUB_PEM.as_bytes().to_vec(),
        )
    }

    fn other_key() -> EncodingKey {
        EncodingKey::from_rsa_pem(OTHER_PRIV_PEM.as_bytes()).expect("other key")
    }

    #[derive(Serialize)]
    struct Mint {
        iss: String,
        sub: String,
        aud: String,
        iat: i64,
        exp: i64,
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn mint(key: &EncodingKey, kid: &str, m: Mint) -> String {
        let mut h = Header::new(Algorithm::RS256);
        h.kid = Some(kid.to_string());
        encode(&h, &m, key).expect("mint")
    }

    fn good(iss: &str, sub: &str, aud: &str) -> Mint {
        let t = now();
        Mint {
            iss: iss.into(),
            sub: sub.into(),
            aud: aud.into(),
            iat: t,
            exp: t + 600,
        }
    }

    fn reviewer(pub_pem: &[u8], audience: Option<&str>) -> Arc<JwtReviewer> {
        JwtReviewer::with_static_pem(
            JwtConfig {
                issuer: "https://knox.example/token".into(),
                audience: audience.map(String::from),
                ..Default::default()
            },
            "k1",
            pub_pem,
        )
        .expect("reviewer")
    }

    /// The happy path, and the identity it produces. `sub` becomes the
    /// principal, and it is marked as vouched-for by an ISSUER — which
    /// is what keeps it from matching a bare ServiceAccount entry.
    #[tokio::test]
    async fn a_good_token_yields_the_subject_as_a_person() {
        let (enc, pubk) = keypair();
        let r = reviewer(&pubk, Some("forge.chert.us"));
        let t = mint(&enc, "k1", good("https://knox.example/token", "alice@example.com", "forge.chert.us"));
        let id = r.review(&t).await.expect("accepted");
        assert_eq!(id.username, "alice@example.com");
        assert_eq!(id.vouched, Vouched::Issuer);
        assert!(id.service_account.is_empty(), "a person carries no ServiceAccount");
        assert!(id.namespace.is_empty());
    }

    /// Every refusal, each against the SAME positive control, so none of
    /// them can pass because the rig refuses everything.
    #[tokio::test]
    async fn the_checks_that_must_refuse() {
        let (enc, pubk) = keypair();
        let other_enc = other_key();
        let iss = "https://knox.example/token";
        let r = reviewer(&pubk, Some("forge.chert.us"));

        // The control, first: this exact shape is accepted.
        let ok = mint(&enc, "k1", good(iss, "alice", "forge.chert.us"));
        assert!(r.review(&ok).await.is_ok(), "the control was refused — every case below is vacuous");

        // Signed by a key the door does not hold.
        let forged = mint(&other_enc, "k1", good(iss, "alice", "forge.chert.us"));
        assert!(r.review(&forged).await.is_err(), "a token signed by an unknown key was accepted");

        // Wrong issuer.
        let wrong_iss = mint(&enc, "k1", good("https://evil.example", "alice", "forge.chert.us"));
        assert!(r.review(&wrong_iss).await.is_err());

        // Wrong audience — the leg that matters when several services
        // share an issuer.
        let wrong_aud = mint(&enc, "k1", good(iss, "alice", "some-other-service"));
        assert!(r.review(&wrong_aud).await.is_err(), "a token for another service was accepted");

        // Expired.
        let t = now();
        let expired = mint(&enc, "k1", Mint {
            iss: iss.into(), sub: "alice".into(), aud: "forge.chert.us".into(),
            iat: t - 7200, exp: t - 3600,
        });
        assert!(r.review(&expired).await.is_err());

        // An unknown `kid`: a static key set cannot refetch, so this is
        // a refusal rather than a hunt.
        let unknown_kid = mint(&enc, "k9", good(iss, "alice", "forge.chert.us"));
        assert!(r.review(&unknown_kid).await.is_err());

        // Not a JWT at all.
        assert!(r.review("not-a-token").await.is_err());
        assert!(r.review("").await.is_err());
    }

    /// A lifetime the door will not accept however good the signature.
    /// Issuers default to months; with revocation out of scope this is
    /// the only bound on a stolen token.
    #[tokio::test]
    async fn a_token_that_lives_too_long_is_refused() {
        let (enc, pubk) = keypair();
        let iss = "https://knox.example/token";
        let r = reviewer(&pubk, Some("forge.chert.us"));
        let t = now();
        // 120 days — a real issuer default.
        let long = mint(&enc, "k1", Mint {
            iss: iss.into(), sub: "alice".into(), aud: "forge.chert.us".into(),
            iat: t, exp: t + 120 * 24 * 3600,
        });
        let err = r.review(&long).await.expect_err("a 120-day token was accepted");
        assert!(err.message().contains("lifetime"), "{}", err.message());
        // …and it is a REFUSAL, not an outage: nothing about retrying
        // helps, so it must not be reported as a transient failure.
        assert!(err.is_cacheable(), "a too-long lifetime was reported as a transport failure");

        // The control: the same key, same audience, a sane lifetime.
        let ok = mint(&enc, "k1", good(iss, "alice", "forge.chert.us"));
        assert!(r.review(&ok).await.is_ok());
    }

    /// With no audience configured the check is SKIPPED rather than
    /// failing every token — `jsonwebtoken` validates `aud` by default
    /// once a token carries one, so getting this wrong refuses
    /// everything rather than accepting too much.
    #[tokio::test]
    async fn an_unset_audience_skips_the_check_instead_of_refusing_everything() {
        let (enc, pubk) = keypair();
        let iss = "https://knox.example/token";
        let r = reviewer(&pubk, None);
        for aud in ["forge.chert.us", "some-other-service", ""] {
            let t = mint(&enc, "k1", good(iss, "alice", aud));
            assert!(r.review(&t).await.is_ok(), "aud={aud:?} was refused with no audience configured");
        }
        // The issuer is still checked — "no audience" is not "no
        // checks".
        let wrong = mint(&enc, "k1", good("https://evil.example", "alice", "x"));
        assert!(r.review(&wrong).await.is_err());
    }

    /// `alg` comes from the token, so it is an allowlist. Without one a
    /// caller picks the algorithm its verifier will use.
    #[tokio::test]
    async fn a_token_naming_a_symmetric_algorithm_is_refused() {
        let (_, pubk) = keypair();
        let r = reviewer(&pubk, Some("forge.chert.us"));
        let mut h = Header::new(Algorithm::HS256);
        h.kid = Some("k1".into());
        let hs = encode(&h, &good("https://knox.example/token", "alice", "forge.chert.us"), 
                        &EncodingKey::from_secret(b"not-a-secret")).expect("mint hs256");
        let err = r.review(&hs).await.expect_err("an HS256 token was accepted");
        assert!(err.message().contains("algorithm"), "{}", err.message());
    }

    /// The router reads this WITHOUT verifying anything, so it must be
    /// robust to junk rather than merely correct on good input.
    #[test]
    fn the_unverified_issuer_peek_survives_rubbish() {
        for junk in ["", "a", "a.b", "....", "a.!!!.c", "a.e30.c"] {
            let _ = unverified_issuer(junk);
        }
        let (enc, _) = keypair();
        let t = mint(&enc, "k1", good("https://knox.example/token", "alice", "aud"));
        assert_eq!(unverified_issuer(&t).as_deref(), Some("https://knox.example/token"));
    }
}
