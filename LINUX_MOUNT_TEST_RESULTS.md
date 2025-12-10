# Linux NFS Mount Testing - Results

**Date:** December 10, 2024  
**Cluster:** MNTT (RKE2 v1.34.1)  
**Status:** ⚠️ **Longhorn Works, Flint Needs Investigation**

---

## ✅ Longhorn NFS Ganesha - SUCCESS

### Test Results

**Share Manager Pod:**
- Pod: `share-manager-pvc-7d63f5da-e9f8-4038-829b-294d39094669`
- IP: `10.42.239.160`
- Server: NFS Ganesha
- Status: ✅ **WORKING**

**Mount Test:**
```bash
# Client: ubuntu:24.04 pod
# Mount: Automatic via Kubernetes NFS volume
# Protocol: NFSv4.1
# Result: ✅ SUCCESS
```

**Evidence:**
```
✅ Pod mounted volume successfully
✅ Created file: test-longhorn.txt
✅ File operations working
✅ Server logs show: CREATE_SESSION from Linux NFSv4.1 client
```

**Ganesha Logs:**
```
INFO :             NFS SERVER INITIALIZED
INFO :CREATE_SESSION client addr=::ffff:10.65.171.171
INFO :Client Record ... name=(37:Linux NFSv4.1 mntt-2.vpc.cloudera.com)
```

---

## ⚠️ Flint NFS Server - PROTOCOL ISSUE

### Test Results

**Server Pod:**
- Pod: `flint-nfs-server-test`
- IP: `10.42.239.158`
- Server: Flint NFSv4.2
- Status: ⚠️ **RUNNING BUT MOUNT FAILS**

**Mount Test:**
```bash
# Attempted protocols:
mount -o vers=4.2,tcp 10.42.239.158:/  # ❌ access denied
mount -o vers=4.1,tcp 10.42.239.158:/  # ❌ access denied
mount -o vers=4,tcp 10.42.239.158:/    # ❌ access denied
```

**Error:** `mount.nfs: access denied by server while mounting`

**Server Logs:**
```
✅ NFSv4.2 TCP server listening on 0.0.0.0:2049
✅ TCP connection from 10.42.239.162:37878
✅ TCP connection closed cleanly
❌ NO NFSv4 request processing logged
```

### Network Connectivity

✅ **TCP Port 2049:** Reachable  
✅ **Server Process:** Running  
✅ **Client → Server:** TCP connection established  
❌ **Protocol Negotiation:** Fails immediately  

---

## 🔍 Analysis

### Why Longhorn Works

**Longhorn + NFS Ganesha:**
1. ✅ Full NFSv4.1 server implementation (mature, tested)
2. ✅ Proper EXCHANGE_ID / CREATE_SESSION handling
3. ✅ Complete AUTH_SYS / AUTH_NULL support
4. ✅ Proper export configuration
5. ✅ Compatible with Linux NFS client

**Evidence from logs:**
- Client successfully negotiates NFSv4.1
- CREATE_SESSION completes
- Client identified as "Linux NFSv4.1"
- File operations work

### Why Flint Fails

**Possible Issues:**

1. **Missing Portmapper?**
   - Traditional NFS needs rpcbind/portmapper
   - NFSv4 shouldn't need it, but client might be trying

2. **Export Configuration?**
   - Server might need explicit export permissions
   - Ganesha has export config, Flint might not

3. **Protocol Negotiation?**
   - Client connects but disconnects immediately
   - No RPC requests logged
   - Suggests very early protocol failure

4. **Authentication?**
   - Client might be trying AUTH_SYS
   - Server might not be handling auth properly

5. **Root Path Difference?**
   - Ganesha exports: `/export/pvc-xxx`
   - Flint exports: `/` (root)

---

## 🔧 Debugging Steps Needed

### 1. Check Server Logs with DEBUG

The server is running with `-v` but we're not seeing detailed protocol logs. Need to check:
- Why TCP connection closes immediately
- Are RPC messages being received?
- Is AUTH being rejected?

### 2. Compare with Ganesha

**Ganesha shows:**
```
CREATE_SESSION client addr=::ffff:10.65.171.171
Client Record ... Linux NFSv4.1
```

**Flint shows:**
```
TCP connection from 10.42.239.162:37878
TCP connection closed cleanly
(no RPC processing)
```

**Missing:** RPC call logging, protocol negotiation

### 3. Tcpdump Analysis

Capture packets to see what the client is actually sending:
```bash
tcpdump -i any -n port 2049 -A
```

---

## 📊 Comparison Summary

| Aspect | Longhorn (Ganesha) | Flint |
|--------|-------------------|-------|
| Server Binary | NFS Ganesha (C) | flint-nfs-server (Rust) |
| Protocol | NFSv4.1 | NFSv4.2 |
| TCP Listen | ✅ Working | ✅ Working |
| TCP Connect | ✅ Working | ✅ Working |
| RPC Processing | ✅ Working | ❌ Not seeing requests |
| Client Auth | ✅ Working | ❌ Failing |
| Mount Result | ✅ SUCCESS | ❌ FAILED |

---

## 🎯 Next Steps

### Immediate (Debug Current Issue)

1. **Add more verbose logging** to see RPC messages
2. **Check if RPC calls are being received** but rejected early
3. **Verify AUTH handling** - might need to accept AUTH_SYS
4. **Test with tcpdump** to see actual protocol exchange

### Alternative (Quick Win)

1. **Add portmapper support** (rpcbind) - some clients expect it
2. **Test with NFSv3** - simpler protocol, easier to debug
3. **Compare packet captures** between Ganesha and Flint

### Long-Term

1. **Full NFSv4.1/4.2 compatibility testing** with Linux client
2. **Authentication mechanisms** (AUTH_NULL, AUTH_SYS, AUTH_GSS)
3. **Export permissions** and security

---

## 💡 Key Finding

**The infrastructure works:**
- ✅ Docker image built successfully
- ✅ Pod deployed and running
- ✅ Server listening on port 2049
- ✅ TCP connectivity confirmed
- ✅ Network between pods working

**The protocol doesn't:**
- ❌ Client connects but gets "access denied"
- ❌ Server sees connection but no RPC processing
- ❌ Protocol negotiation failing silently

**This is a protocol implementation issue, not infrastructure!**

---

**Testing Platform:** Kubernetes cluster (mntt)  
**Client:** Ubuntu 24.04 with nfs-common  
**Server:** Flint NFSv4.2 (Rust)  
**Comparison:** NFS Ganesha (proven working)  
**Status:** Need to debug protocol layer

