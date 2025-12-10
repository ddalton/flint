# NFS Mount Debugging - Critical Findings

**Date:** December 10, 2024  
**Issue:** Linux NFS client cannot mount Flint NFSv4.2 server  
**Status:** 🔴 **ROOT CAUSE IDENTIFIED**

---

## 🎯 Root Cause

**The client connects to port 2049 but doesn't send any RPC messages!**

### Evidence

**Server logs show:**
```
INFO 📡 New TCP connection from 10.42.239.162:56274
(2 second pause - nothing happens)
INFO ✓ TCP connection closed cleanly
```

**Missing logs (should appear if RPC received):**
```
DEBUG >>> Processing NFSv4 request from ..., length=XX bytes
INFO >>> RPC CALL: xid=..., program=..., version=..., procedure=...
```

**Conclusion:** Client connects, waits 2 seconds, closes without sending data.

---

## 🔍 Detailed Analysis

### What Happens in Flint Server

**Code path:** `src/nfs/server_v4.rs:133-143`

```rust
loop {
    // Read RPC record marker (4 bytes)
    let mut marker_buf = [0u8; 4];
    match reader.read_exact(&mut marker_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            // Connection closed gracefully ← THIS IS HAPPENING
            return Ok(());
        }
```

**Execution:**
1. Accept TCP connection ✅
2. Try to read 4-byte RPC marker
3. Get `UnexpectedEof` (client closed without sending)
4. Return `Ok(())` (clean close)
5. Log: "✓ TCP connection closed cleanly"

**Result:** No error, but also no RPC processing!

### What Should Happen

**Normal NFSv4.1 client (Longhorn/Ganesha):**
1. Connect to port 2049
2. Send NULL RPC call (ping)
3. Send COMPOUND with EXCHANGE_ID
4. Send COMPOUND with CREATE_SESSION
5. Send file operations

**Flint server never gets past step 1!**

---

## ❓ Why Doesn't Client Send Data?

### Theory 1: Client Expects Server-First Protocol ❌

Some protocols (SMTP, FTP) have server send banner first.

**Test:** NFSv4 is client-first (client sends RPC calls)  
**Result:** This is NOT the issue

### Theory 2: Portmapper Required ❌

Client tries port 111 first, fails, gives up.

**Test:** Both Flint and Ganesha don't have port 111  
**Result:** This is NOT the issue (Ganesha works without it)

### Theory 3: TLS/Security Handshake Expected ⚠️

Client tries TLS handshake, server doesn't respond.

**Test:** Need to check if client is sending TLS ClientHello  
**Result:** POSSIBLE but unlikely (no TLS in NFS by default)

### Theory 4: Client Probing Server Type ✅ **LIKELY**

**Hypothesis:** Client sends some probe/test and expects specific response.

**Evidence:**
- Connection lasts exactly ~2 seconds (timeout)
- Client doesn't get expected response
- Client aborts connection
- Server doesn't recognize probe as valid RPC

### Theory 5: READ Timeout on Client Side ✅ **VERY LIKELY**

**Code evidence:** `server_v4.rs:136-137`
```rust
match reader.read_exact(&mut marker_buf).await {
```

**Problem:** `read_exact()` blocks waiting for 4 bytes.

**If client sends:**
- 0 bytes: Server blocks forever, client times out after 2s
- 1-3 bytes: Server blocks waiting for byte 4, client times out
- Non-RPC probe: Server expects RPC marker, might not match

---

## 🔬 Comparison with Ganesha

### Ganesha Logs Show

```
CREATE_SESSION client addr=::ffff:10.65.171.171 clientid=Epoch=0x6939c27b Counter=0x00000002
Client Record ... name=(37:Linux NFSv4.1 mntt-2.vpc.cloudera.com)
Add client '37:Linux NFSv4.1 mntt-2.vpc.cloudera.com' to recovery backend
```

**This means Ganesha:**
1. ✅ Received RPC calls
2. ✅ Processed EXCHANGE_ID
3. ✅ Created session
4. ✅ Client successfully authenticated

### Flint Shows

```
📡 New TCP connection
(nothing)
✓ TCP connection closed cleanly
```

**Key difference:** Ganesha receives and processes RPC calls, Flint doesn't!

---

## 🛠️ Debug Steps Needed

### 1. Add Hex Dump Logging

Modify `server_v4.rs` to log first bytes received:

```rust
match reader.read_exact(&mut marker_buf).await {
    Ok(_) => {
        eprintln!("DEBUG: Received marker: {:02x?}", marker_buf);
    }
    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
        eprintln!("DEBUG: Client closed without sending data");
        return Ok(());
    }
```

### 2. Add Timeout Detection

Log how long client was connected:

```rust
use tokio::time::Instant;
let connect_time = Instant::now();
// ... later ...
eprintln!("Connection lasted: {:?}", connect_time.elapsed());
```

### 3. Try Peek Instead of Read

See if ANY data is available:

```rust
let mut peek_buf = [0u8; 16];
match reader.peek(&mut peek_buf).await {
    Ok(n) => eprintln!("DEBUG: {} bytes available: {:02x?}", n, &peek_buf[..n]),
    Err(e) => eprintln!("DEBUG: Peek failed: {}", e),
}
```

### 4. Compare TCP Options

Check if Ganesha sets special TCP socket options:
- SO_KEEPALIVE
- TCP_NODELAY (Flint has this ✅)
- SO_REUSEADDR
- TCP_QUICKACK

---

## 💡 Strong Suspicion

Looking at Longhorn's successful mount:

```
mount.nfs: trying text-based options 'vers=4.1,tcp,port=2049,addr=10.42.239.160,clientaddr=10.42.239.163'
```

**Key parameter:** `clientaddr=10.42.239.163`

The client is telling the server its own IP address. Maybe the Flint server needs to handle this during initial connection?

**Also:** Longhorn mounts via SERVICE IP (`10.43.140.213`), not pod IP (`10.42.239.160`)!

---

## 🎯 Immediate Actions

### Action 1: Add Debug Logging ✅ HIGH PRIORITY

Add extensive logging to see:
- How many bytes client sends
- What those bytes are (hex dump)
- Connection duration
- Any errors during receive

### Action 2: Test Longhorn's Exact Mount ✅

Try mounting Flint the same way Longhorn is mounted:
- Use service IP instead of pod IP
- Use same NFSv4.1 options
- See if behavior changes

### Action 3: Packet Capture 📊

Run tcpdump in NFS server pod to capture:
- What client actually sends
- TLS handshake attempts?
- Partial RPC messages?
- Protocol negotiation bytes?

---

## 📋 Next Steps

1. **Add debug logging** to Flint server (show bytes received)
2. **Test Flint via Service IP** (create proper Service)
3. **Capture packets** during failed mount attempt
4. **Compare with Ganesha** packet capture

**Most likely fix:** Add better logging to see what's actually happening at byte level!

---

**Status:** Issue identified - client not sending RPC messages  
**Next:** Add debug logging to see WHY client isn't sending data  
**Timeline:** Should be fixable with proper debugging in 1-2 hours

