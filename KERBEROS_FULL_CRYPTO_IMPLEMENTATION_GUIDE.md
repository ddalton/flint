# Kerberos Full Cryptography Implementation Guide

**Date**: December 19, 2025  
**Status**: Foundation Complete - Crypto Implementation Needed  
**Estimated Effort**: 4-6 hours  
**Lines to Add**: ~750 lines + 200 lines tests

---

## 📊 Current Status

### ✅ What's Complete (723 lines)
- **Keytab parser**: Loads MIT keytab format, extracts service keys
- **Protocol scaffolding**: RPCSEC_GSS, GSS-API wrapper, AP-REP structure
- **ASN.1 helpers**: Tag/length parsing, field extraction
- **Infrastructure**: Context management, integration with NFS server
- **Testing framework**: 13 unit tests for current functionality
- **Dependencies**: Added aes, hmac, sha1, sha2, cbc, pbkdf2

### ❌ What's Missing (Crypto Layer)
- **Ticket decryption**: Decrypt AP-REQ ticket with service key
- **Session key extraction**: Get session key from decrypted ticket
- **Authenticator validation**: Decrypt and validate authenticator
- **AP-REP encryption**: Create properly encrypted AP-REP response
- **AES-CTS mode**: Implement ciphertext stealing mode
- **Key derivation**: KDF for Kerberos encryption/integrity keys

---

## 🎯 Why This Is Needed

### Evidence from Investigation

**From tcpdump analysis:**
```
Client: Sends 757-828 byte AP-REQ with RPCSEC_GSS (flavor 6) ✅
Server: Responds with 116-120 byte AP-REP ✅
Client: Immediately closes connection ❌
Client: All subsequent gss_init_sec_context() fail ❌
```

**Root Cause:**
The client's GSSAPI library validates the AP-REP cryptographically:
1. Tries to decrypt AP-REP with session key
2. Our placeholder has dummy encrypted data
3. Decryption fails → context rejected
4. Client marks context as failed
5. Retries inherit failed state

**Confidence: 75-80%** that proper crypto will fix it.

---

## 🔧 Implementation Plan

### Phase 1: AES-CTS Mode (~100 lines, 1 hour)

**File**: Add to `src/nfs/kerberos.rs`

**Requirements**: 
- Implement AES-CTS (RFC 3962 Section 6)
- Ciphertext stealing for last block
- Handle partial blocks correctly

**Code Structure**:
```rust
/// AES-CTS encryption (RFC 3962)
fn aes_cts_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    // Requirements:
    // - Input must be >= 16 bytes
    // - Uses standard CBC for all but last 2 blocks
    // - Swaps last 2 blocks (ciphertext stealing)
    
    // Steps:
    // 1. Pad to block boundary
    // 2. CBC encrypt all full blocks
    // 3. Implement ciphertext stealing for final blocks
    // 4. Return ciphertext
}

fn aes_cts_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    // Reverse of encryption
    // 1. Handle swapped last 2 blocks
    // 2. CBC decrypt
    // 3. Remove padding
}
```

**Test Vectors**: Use RFC 3962 Appendix B test vectors

**Tests** (~30 lines):
```rust
#[test]
fn test_aes_cts_encrypt_decrypt() {
    // RFC 3962 test vectors
    let key = hex::decode("636869636b656e207465726961").unwrap();
    let plaintext = b"I would like the";
    let iv = [0u8; 16];
    
    let ciphertext = aes_cts_encrypt(&key, &iv, plaintext).unwrap();
    let decrypted = aes_cts_decrypt(&key, &iv, &ciphertext).unwrap();
    
    assert_eq!(decrypted, plaintext);
}
```

---

### Phase 2: Kerberos Key Derivation (~80 lines, 45 min)

**Requirements**:
- Implement KDF per RFC 3961/3962
- Derive encryption and integrity keys from base key
- Support different key usages

**Code Structure**:
```rust
/// Derive Kerberos encryption key from base key
/// RFC 3961 Section 5.1, RFC 3962 Section 4
fn derive_key(base_key: &[u8], enctype: EncType, usage: i32, key_type: &str) -> Result<Vec<u8>> {
    // For AES enctypes:
    // 1. Create usage string: usage_bytes || key_type_bytes
    // 2. Calculate n = (key_length + 127) / 128
    // 3. K1 = random-to-key(DR(base_key, usage||0xAA, n*128))
    // 4. Kc = random-to-key(DR(K1, usage||0x99, key_length))
    
    match enctype {
        EncType::AES128CtsHmacSha196 | EncType::AES256CtsHmacSha196 => {
            derive_key_aes_sha1(base_key, usage, key_type)
        }
        EncType::AES128CtsHmacSha256128 | EncType::AES256CtsHmacSha384196 => {
            derive_key_aes_sha2(base_key, usage, key_type)
        }
    }
}

fn derive_key_aes_sha1(base_key: &[u8], usage: i32, key_type: &str) -> Result<Vec<u8>> {
    // Implementation using HMAC-SHA1 based KDF
    // 1. Build constant from usage + key_type
    // 2. Use pseudo-random function (PRF) with base_key
    // 3. Return derived key
}
```

**Tests** (~25 lines):
```rust
#[test]
fn test_derive_key_aes128() {
    // Test with known vectors
    let base_key = /* test key */;
    let derived = derive_key(base_key, EncType::AES128CtsHmacSha196, 
                            key_usage::AP_REQ_AUTHENTICATOR, "kc").unwrap();
    assert_eq!(derived.len(), 16);
}
```

---

### Phase 3: Ticket Parsing and Decryption (~150 lines, 1.5 hours)

**Requirements**:
- Parse AP-REQ structure
- Extract encrypted ticket
- Decrypt with service key
- Parse decrypted ticket content

**Code Structure**:
```rust
/// Kerberos Ticket structure (RFC 4120 Section 5.3)
#[derive(Debug)]
struct Ticket {
    realm: String,
    sname: Vec<String>,  // Service name components
    enc_part: EncryptedData,
}

#[derive(Debug)]
struct EncryptedData {
    enctype: EncType,
    kvno: Option<u32>,
    cipher: Vec<u8>,
}

#[derive(Debug)]
struct EncTicketPart {
    flags: u32,
    key: SessionKey,  // THE SESSION KEY
    crealm: String,   // Client realm
    cname: Vec<String>,  // Client name
    transited: Vec<u8>,
    authtime: i64,
    starttime: Option<i64>,
    endtime: i64,
    renew_till: Option<i64>,
}

#[derive(Debug, Clone)]
struct SessionKey {
    enctype: EncType,
    key: Vec<u8>,
}

impl Ticket {
    /// Parse ticket from AP-REQ
    fn parse(data: &[u8]) -> Result<Self> {
        // Parse: Ticket ::= [APPLICATION 1] SEQUENCE {
        //   tkt-vno[0] INTEGER (5),
        //   realm[1] Realm,
        //   sname[2] PrincipalName,
        //   enc-part[3] EncryptedData
        // }
    }
    
    /// Decrypt ticket and extract session key
    fn decrypt(&self, service_key: &ServiceKey) -> Result<EncTicketPart> {
        // 1. Derive decryption key from service key
        let ke = derive_key(&service_key.key, service_key.enctype, 
                           key_usage::TGS_REP_ENC_PART, "ke")?;
        
        // 2. Decrypt with AES-CTS
        let iv = vec![0u8; 16];  // Kerberos uses zero IV
        let plaintext = aes_cts_decrypt(&ke, &iv, &self.enc_part.cipher)?;
        
        // 3. Verify checksum (last 12-16 bytes for HMAC)
        let checksum_len = match self.enc_part.enctype {
            EncType::AES128CtsHmacSha196 | EncType::AES256CtsHmacSha196 => 12,
            _ => 16,
        };
        
        let data_len = plaintext.len() - checksum_len;
        let data = &plaintext[..data_len];
        let checksum = &plaintext[data_len..];
        
        // 4. Compute expected checksum
        let ki = derive_key(&service_key.key, service_key.enctype,
                           key_usage::TGS_REP_ENC_PART, "ki")?;
        let expected = compute_hmac(&ki, data, checksum_len);
        
        if checksum != expected {
            return Err(KerberosError::DecryptionFailed("Checksum mismatch".to_string()));
        }
        
        // 5. Parse decrypted content
        EncTicketPart::parse(data)
    }
}

impl EncTicketPart {
    /// Parse decrypted ticket content
    fn parse(data: &[u8]) -> Result<Self> {
        // Parse: EncTicketPart ::= [APPLICATION 3] SEQUENCE {
        //   flags[0] TicketFlags,
        //   key[1] EncryptionKey,  ← THE SESSION KEY WE NEED!
        //   crealm[2] Realm,
        //   cname[3] PrincipalName,
        //   transited[4] TransitedEncoding,
        //   authtime[5] KerberosTime,
        //   starttime[6] KerberosTime OPTIONAL,
        //   endtime[7] KerberosTime,
        //   renew-till[8] KerberosTime OPTIONAL
        // }
        
        // Most important: extract key[1]
    }
}
```

**Tests** (~40 lines):
```rust
#[test]
fn test_parse_ticket() {
    let ticket_der = /* sample ticket */;
    let ticket = Ticket::parse(&ticket_der).unwrap();
    assert_eq!(ticket.realm, "PNFS.TEST");
}

#[test]
fn test_decrypt_ticket() {
    let service_key = /* test key */;
    let ticket = /* test ticket */;
    let enc_part = ticket.decrypt(&service_key).unwrap();
    assert!(enc_part.key.key.len() > 0);
}
```

---

### Phase 4: Authenticator Validation (~120 lines, 1 hour)

**Requirements**:
- Parse encrypted authenticator from AP-REQ
- Decrypt with session key
- Validate timestamp and checksum

**Code Structure**:
```rust
#[derive(Debug)]
struct Authenticator {
    authenticator_vno: u32,
    crealm: String,
    cname: Vec<String>,
    cksum: Option<Checksum>,
    cusec: u32,
    ctime: i64,
    subkey: Option<SessionKey>,
    seq_number: Option<u32>,
}

impl Authenticator {
    /// Parse and decrypt authenticator from AP-REQ
    fn parse_and_decrypt(enc_data: &[u8], session_key: &SessionKey) -> Result<Self> {
        // 1. Derive decryption key
        let ke = derive_key(&session_key.key, session_key.enctype,
                           key_usage::AP_REQ_AUTHENTICATOR, "ke")?;
        
        // 2. Decrypt
        let iv = vec![0u8; 16];
        let plaintext = aes_cts_decrypt(&ke, &iv, enc_data)?;
        
        // 3. Verify checksum
        // ... similar to ticket
        
        // 4. Parse: Authenticator ::= [APPLICATION 11] SEQUENCE {
        //   authenticator-vno[0] INTEGER (5),
        //   crealm[1] Realm,
        //   cname[2] PrincipalName,
        //   cksum[3] Checksum OPTIONAL,
        //   cusec[4] INTEGER,
        //   ctime[5] KerberosTime,
        //   subkey[6] EncryptionKey OPTIONAL,
        //   seq-number[7] INTEGER OPTIONAL
        // }
        
        Self::parse_from_plaintext(&plaintext)
    }
    
    /// Validate authenticator
    fn validate(&self, tolerance_seconds: i64) -> Result<()> {
        // Check timestamp is within tolerance (usually 5 minutes)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        
        let time_diff = (now - self.ctime).abs();
        if time_diff > tolerance_seconds {
            return Err(KerberosError::InvalidAuthenticator(
                format!("Time skew too large: {} seconds", time_diff)
            ));
        }
        
        Ok(())
    }
}
```

**Tests** (~35 lines):
```rust
#[test]
fn test_authenticator_validation() {
    let auth = Authenticator {
        ctime: current_time(),
        // ... other fields
    };
    assert!(auth.validate(300).is_ok());  // 5 min tolerance
}

#[test]
fn test_authenticator_time_skew() {
    let auth = Authenticator {
        ctime: current_time() - 400,  // 6+ minutes ago
        // ...
    };
    assert!(auth.validate(300).is_err());  // Should fail
}
```

---

### Phase 5: AP-REP Encryption (~100 lines, 1 hour)

**Requirements**:
- Create properly encrypted AP-REP response
- Use real session key from ticket
- Include timestamp for mutual authentication

**Code Structure**:
```rust
#[derive(Debug)]
struct EncAPRepPart {
    ctime: i64,
    cusec: u32,
    subkey: Option<SessionKey>,
    seq_number: Option<u32>,
}

impl EncAPRepPart {
    /// Create encrypted AP-REP part
    fn create(ctime: i64, cusec: u32, subkey: Option<SessionKey>) -> Self {
        Self {
            ctime,
            cusec,
            subkey,
            seq_number: Some(0),  // Initial sequence number
        }
    }
    
    /// Encode and encrypt AP-REP part
    fn encrypt(&self, session_key: &SessionKey) -> Result<Vec<u8>> {
        // 1. Encode as ASN.1:
        // EncAPRepPart ::= [APPLICATION 27] SEQUENCE {
        //   ctime[0] KerberosTime,
        //   cusec[1] INTEGER,
        //   subkey[2] EncryptionKey OPTIONAL,
        //   seq-number[3] INTEGER OPTIONAL
        // }
        let plaintext = self.encode_asn1();
        
        // 2. Compute checksum
        let ki = derive_key(&session_key.key, session_key.enctype,
                           key_usage::AP_REP_ENC_PART, "ki")?;
        let checksum = compute_hmac(&ki, &plaintext, 12);  // 12 bytes for SHA1-96
        
        // 3. Append checksum to plaintext
        let mut data_with_checksum = plaintext;
        data_with_checksum.extend_from_slice(&checksum);
        
        // 4. Encrypt with AES-CTS
        let ke = derive_key(&session_key.key, session_key.enctype,
                           key_usage::AP_REP_ENC_PART, "ke")?;
        let iv = vec![0u8; 16];
        let ciphertext = aes_cts_encrypt(&ke, &iv, &data_with_checksum)?;
        
        // 5. Wrap in EncryptedData structure
        Ok(EncryptedData {
            enctype: session_key.enctype,
            kvno: None,
            cipher: ciphertext,
        }.encode())
    }
}

impl KerberosContext {
    /// Generate proper AP-REP with encryption
    fn generate_ap_rep_with_crypto(
        session_key: &SessionKey,
        ctime: i64,
        cusec: u32
    ) -> Result<Vec<u8>> {
        // 1. Create encrypted AP-REP part
        let enc_part = EncAPRepPart::create(ctime, cusec, None);
        let encrypted = enc_part.encrypt(session_key)?;
        
        // 2. Build AP-REP: [APPLICATION 15] SEQUENCE {
        //   pvno[0] INTEGER (5),
        //   msg-type[1] INTEGER (15),
        //   enc-part[2] EncryptedData
        // }
        let mut ap_rep = Vec::new();
        ap_rep.extend_from_slice(&[0xA0, 0x03, 0x02, 0x01, 0x05]);  // pvno[0] = 5
        ap_rep.extend_from_slice(&[0xA1, 0x03, 0x02, 0x01, 0x0F]);  // msg-type[1] = 15
        ap_rep.push(0xA2);  // enc-part[2]
        Self::encode_length(&mut ap_rep, encrypted.len());
        ap_rep.extend_from_slice(&encrypted);
        
        // 3. Wrap in APPLICATION 15
        let mut result = vec![0x6F];  // APPLICATION 15
        Self::encode_length(&mut result, ap_rep.len());
        result.extend_from_slice(&ap_rep);
        
        // 4. Wrap in GSS-API
        let krb5_oid = [0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
        let gss_len = krb5_oid.len() + result.len();
        
        let mut token = vec![0x60];  // APPLICATION 0
        Self::encode_length(&mut token, gss_len);
        token.extend_from_slice(&krb5_oid);
        token.extend_from_slice(&result);
        
        Ok(token)
    }
}
```

**Tests** (~30 lines):
```rust
#[test]
fn test_ap_rep_encryption() {
    let session_key = SessionKey {
        enctype: EncType::AES128CtsHmacSha196,
        key: vec![/* test key */],
    };
    
    let ap_rep = KerberosContext::generate_ap_rep_with_crypto(
        &session_key, current_time(), 0
    ).unwrap();
    
    // Should be > 100 bytes with real encryption
    assert!(ap_rep.len() > 100);
    assert_eq!(ap_rep[0], 0x60);  // GSS tag
}
```

---

### Phase 6: Full Integration (~150 lines, 1.5 hours)

**Bring it all together:**

```rust
impl KerberosContext {
    /// Accept AP-REQ with FULL cryptography
    pub fn accept_token(keytab: &Keytab, token: &[u8]) -> Result<(Self, Vec<u8>)> {
        info!("🔐 Accepting Kerberos token with FULL CRYPTOGRAPHY: {} bytes", token.len());
        
        // 1. Parse GSS wrapper (already done)
        let ap_req_data = Self::parse_gss_wrapper(token)?;
        
        // 2. Parse AP-REQ
        let (ticket, enc_authenticator, ap_options) = Self::parse_ap_req(ap_req_data)?;
        
        // 3. Find service key for this ticket
        let service_name = ticket.sname.join("/");
        let service_key = keytab.find_key(&service_name)
            .ok_or_else(|| KerberosError::PrincipalNotFound(service_name.clone()))?;
        
        info!("   Found service key: {}@{}", service_key.principal, service_key.realm);
        
        // 4. Decrypt ticket to get session key
        let enc_ticket_part = ticket.decrypt(service_key)?;
        let session_key = enc_ticket_part.key;
        
        info!("   ✅ Ticket decrypted, extracted session key: {} bytes", session_key.key.len());
        
        // 5. Decrypt and validate authenticator
        let authenticator = Authenticator::parse_and_decrypt(&enc_authenticator, &session_key)?;
        authenticator.validate(300)?;  // 5 minute tolerance
        
        info!("   ✅ Authenticator validated: time_skew={}s", 
              current_time() - authenticator.ctime);
        
        // 6. Create context
        let client_name = enc_ticket_part.cname.join("/");
        let context = KerberosContext {
            client_principal: format!("{}@{}", client_name, enc_ticket_part.crealm),
            service_principal: format!("{}@{}", service_name, service_key.realm),
            session_key: session_key.key.clone(),
            enctype: session_key.enctype,
            established: true,
            client_realm: enc_ticket_part.crealm,
        };
        
        // 7. Generate encrypted AP-REP
        let ap_rep = Self::generate_ap_rep_with_crypto(
            &session_key,
            authenticator.ctime,
            authenticator.cusec
        )?;
        
        info!("✅ FULL CRYPTO: Kerberos context established: client={}", context.client_principal);
        info!("   Session key: {} bytes, enctype={:?}", context.session_key.len(), context.enctype);
        debug!("   Generated encrypted AP-REP: {} bytes", ap_rep.len());
        
        Ok((context, ap_rep))
    }
}
```

**Tests** (~40 lines):
```rust
#[test]
fn test_full_crypto_end_to_end() {
    // This is an integration test with real Kerberos structures
    // Requires generating test AP-REQ with known keys
    
    let keytab = create_test_keytab();
    let ap_req = create_test_ap_req_token();  // Helper function
    
    let (context, ap_rep) = KerberosContext::accept_token(&keytab, &ap_req).unwrap();
    
    assert!(context.established);
    assert_eq!(context.client_realm, "TEST.REALM");
    assert!(context.session_key.len() > 0);
    assert!(ap_rep.len() > 100);  // Should be substantial with real crypto
}
```

---

### Phase 7: Helper Functions (~200 lines, 1 hour)

**HMAC computation:**
```rust
fn compute_hmac(key: &[u8], data: &[u8], truncate_to: usize) -> Vec<u8> {
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).unwrap();
    mac.update(data);
    let result = mac.finalize();
    result.into_bytes()[..truncate_to].to_vec()
}
```

**ASN.1 parsing helpers:**
```rust
fn parse_integer(data: &[u8]) -> Result<(i64, &[u8])> {
    // Parse ASN.1 INTEGER
}

fn parse_octet_string(data: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    // Parse ASN.1 OCTET STRING
}

fn parse_generalized_time(data: &[u8]) -> Result<(i64, &[u8])> {
    // Parse ASN.1 GeneralizedTime to Unix timestamp
}
```

---

## 📋 Testing Strategy

### Unit Tests (15-20 new tests)
1. ✅ AES-CTS encrypt/decrypt with RFC test vectors
2. ✅ Key derivation with known inputs/outputs
3. ✅ Ticket parsing with sample tickets
4. ✅ Authenticator validation (valid/expired/skewed)
5. ✅ AP-REP encryption/structure
6. ✅ End-to-end with synthetic AP-REQ
7. ✅ ASN.1 parsing edge cases
8. ✅ Checksum validation
9. ✅ Error handling (bad keys, corrupted data)
10. ✅ Multiple encryption types

### Integration Test
Create a synthetic AP-REQ with known keys and verify full flow.

---

## 🚀 Implementation Order

**Session 1 (90 min):** AES-CTS + Key Derivation + Tests  
**Session 2 (90 min):** Ticket Parsing + Decryption + Tests  
**Session 3 (60 min):** Authenticator Validation + Tests  
**Session 4 (90 min):** AP-REP Encryption + Integration Tests  
**Session 5 (30 min):** Final testing + deployment + validation

**Total: 5 sessions, ~5-6 hours**

---

## 📚 References

**RFCs to implement:**
- RFC 4120 - Kerberos V5 (ticket/authenticator structures)
- RFC 3961 - Encryption and Checksum Specifications
- RFC 3962 - AES Encryption for Kerberos 5
- RFC 1964 - GSS-API Kerberos Mechanism

**Test Vectors:**
- RFC 3962 Appendix B - AES-CTS test vectors
- RFC 6649 - Additional test vectors

**Rust Crates:**
- `aes` - AES block cipher
- `cbc` - CBC mode
- `hmac` - HMAC implementation
- `sha1`, `sha2` - Hash functions

---

## 🎯 Expected Outcome

**After implementation:**
- ✅ Client successfully validates AP-REP
- ✅ GSS context establishment completes
- ✅ `sec=krb5` mount succeeds
- ✅ Linux client makes direct connections to Data Servers
- ✅ **True parallel I/O with file striping**
- ✅ Files appear in `/mnt/pnfs-data/` on DSes

**Performance expectation:**
- Current (through MDS): 55-92 MB/s
- With 2 DSes parallel: 150-180 MB/s
- With 4 DSes parallel: 300-350 MB/s

---

## 💡 Code Organization

**New functions in `kerberos.rs`:**
```
Lines 1-250:    Existing (keytab parser, infrastructure)
Lines 251-350:  AES-CTS implementation
Lines 351-430:  Key derivation functions
Lines 431-580:  Ticket structures and decryption
Lines 581-700:  Authenticator structures and validation
Lines 701-800:  AP-REP encryption
Lines 801-1000: ASN.1 parsing helpers
Lines 1001-1200: Integration (accept_token)
Lines 1201-1400: Unit tests
```

**Total: ~1400 lines** (production-quality, well-tested)

---

## 🔍 Risk Assessment

**High confidence areas:**
- AES-CTS (well-specified in RFC 3962)
- Key derivation (clear algorithms)
- Encryption/decryption (standard crypto)

**Medium confidence areas:**
- ASN.1 parsing (complex, many edge cases)
- Ticket structure variations (optional fields)

**Mitigation:**
- Extensive unit tests
- Test with real Kerberos tokens (capture and replay)
- Incremental validation

---

## 📝 Deliverables

1. **~800 lines** of crypto implementation
2. **~200 lines** of comprehensive unit tests
3. **Working `sec=krb5` mount** from Linux client
4. **Verified parallel I/O** with file striping
5. **Performance measurements** with multiple DSes
6. **Documentation** of crypto implementation

---

## 🎉 Final State

**After completion:**
- ✅ 100% Pure Rust Kerberos with full cryptography
- ✅ Zero glibc dependencies
- ✅ Production-ready RPCSEC_GSS
- ✅ Parallel I/O functional
- ✅ ~1400 lines, 30+ tests
- ✅ Complete NFS/pNFS/Kerberos stack

**Start a new session and reference this guide!**

---

## 🔧 Quick Start for New Session

```bash
# 1. Navigate to project
cd /Users/ddalton/projects/rust/flint/spdk-csi-driver

# 2. Open the file
code src/nfs/kerberos.rs

# 3. Start with Phase 1: AES-CTS
# Implement aes_cts_encrypt() and aes_cts_decrypt()
# Add RFC 3962 test vectors

# 4. Run tests frequently
cargo test kerberos

# 5. Build and deploy after each phase
cargo build --bin flint-pnfs-mds
```

**This document contains everything needed for the next session!**

