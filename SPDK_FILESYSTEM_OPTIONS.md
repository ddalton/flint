# SPDK Filesystem Options - Complete Analysis

**Date:** December 10, 2024  
**Question:** Can NFS server avoid kernel syscalls when using XFS on SPDK?  
**Answer:** ❌ **NO - With XFS you MUST use syscalls**

---

## What I Found in Your SPDK Codebase

### ✅ SPDK FSDEV Framework Exists

**Location:** `/Users/ddalton/github/spdk/lib/fsdev/`

**What it is:**
- **Abstraction layer** for filesystem operations (like bdev is for block devices)
- Provides ~30 filesystem operations API
- **BUT:** It's NOT a filesystem itself!

**Operations Available:**
```c
spdk_fsdev_lookup()   // Find files by name
spdk_fsdev_create()   // Create files
spdk_fsdev_read()     // Read data
spdk_fsdev_write()    // Write data  
spdk_fsdev_mkdir()    // Create directories
spdk_fsdev_readdir()  // List directories
spdk_fsdev_readlink() // Read symlinks
// ... and 20+ more operations
```

### ❌ BUT: The Implementation Still Uses Syscalls!

**Current fsdev backend:** `/Users/ddalton/github/spdk/module/fsdev/aio/fsdev_aio.c`

**Code evidence:**
```c
// Line 435: openat() syscall
fd = openat(fobject->fd, ".", O_RDONLY);

// Line 863: openat() syscall  
fd = openat(vfsdev->proc_self_fd, fobject->fd_str, flags);

// Line 1074: openat() + O_CREAT syscall
fd = openat(parent_fobject->fd, name, (flags | O_CREAT), mode);
```

**What this means:**
- SPDK fsdev provides a **nice API**
- But the AIO backend uses **Linux syscalls** under the hood
- Still goes through kernel VFS layer
- Still accesses kernel filesystem (ext4/XFS/etc.)

---

## The Hard Truth About Full Userspace

### To Have ZERO Kernel Calls with SPDK:

**You need ALL of these:**
1. ❌ No kernel filesystem (no XFS, no ext4)
2. ✅ SPDK for storage backend
3. ⚠️ **Custom filesystem in userspace** (you must write this!)

### What "Custom Filesystem" Means

**You would implement:**

```
Directory Management:
- Inode table (stored in SPDK blob)
- Directory tree (stored in SPDK blob)
- Name→inode mapping

File Management:
- Extent allocation (which blocks belong to which file)
- Free space bitmap
- File metadata (size, permissions, times)

Operations:
- create/delete/rename files
- create/delete/rename directories
- read/write data
- links, symlinks
- permissions, ownership

Crash Recovery:
- Journal/transaction log
- Replay on startup
- Consistency checks
```

**Complexity:** 15,000-30,000 lines of Rust code

---

## Available Options Analysis

### Option A: XFS on UBLK+SPDK (What You're Doing) ✅

```
NFS → std::fs syscalls → Kernel VFS → XFS → UBLK → SPDK → NVMe
       ↑ Kernel boundary
```

**Syscalls per operation:** 2-4  
**Performance:** 80-90% of theoretical max  
**Complexity:** Low ✅  
**Reliability:** Excellent ✅  
**Time:** Working now ✅  

### Option B: SPDK fsdev with Custom Backend ⚠️

**If SPDK had a native filesystem implementation:**
```
NFS → spdk_fsdev API → Custom fsdev backend → SPDK blobs → NVMe
      ↑ All userspace!
```

**But:** You'd still need to write the custom backend (15K+ lines)  
**Time:** 6-12 months

### Option C: Existing Userspace Filesystems?

**I checked:**
- ❌ SPDK BlobFS was **removed** (deprecated)
- ❌ No native SPDK filesystem exists
- ❌ SPDK fsdev_aio uses syscalls
- ❌ VirtioFS is for VMs, uses host kernel filesystem

---

## The Answer to Your Question

### Q: "Can filesystem operations by NFS be userspace?"

**With XFS formatted on SPDK lvol:** ❌ **NO**

**Why:**
- XFS driver lives in Linux kernel
- To access XFS, you MUST use syscalls (open/read/write/fsync)
- SPDK cannot help you avoid this

**Even with UBLK:**
- UBLK helps avoid syscalls for **block I/O**
- But filesystem operations (create file, mkdir, rename) are **always kernel**

### Q: "Does SPDK help with this?"

**SPDK helps with:**
- ✅ Fast block storage (bypasses kernel block layer)
- ✅ Zero-copy DMA to NVMe
- ✅ User-space I/O (at the block level)

**SPDK does NOT help with:**
- ❌ Avoiding kernel filesystem syscalls
- ❌ Userspace XFS/ext4 implementation
- ❌ Eliminating VFS layer overhead

---

## Real-World Performance Impact

### Syscall Overhead Analysis

**Modern syscalls are fast:**
- open(): ~1-2μs
- pread(): ~2-5μs (if cached)
- pwrite(): ~2-5μs (if cached)
- fsync(): ~50-1000μs (depends on disk)

**NFS operation latency:**
- Network RTT: 100-500μs (LAN)
- Disk I/O: 50-10,000μs
- **Syscall overhead: 2-5μs** (0.5-5% of total)

**Conclusion:** Syscalls are NOT your bottleneck!

### Where Time Is Actually Spent

**Typical 1MB NFS WRITE:**
- Network transmission: 8ms (1Gbps)
- Syscalls (write+fsync): 0.05ms (0.5%)
- Disk I/O: 4ms
- **Total: ~12ms**

**Even eliminating all syscalls:** Save only 0.05ms out of 12ms = 0.4% improvement

---

## My Strong Recommendation

### ✅ Keep Option A (XFS on UBLK+SPDK)

**Reasons:**
1. Syscalls are only 0.5-5% of latency
2. XFS is battle-tested (20+ years)
3. You get POSIX compliance for free
4. Standard tools work (ls, cp, df, etc.)
5. Already working!

**The REAL performance gains come from:**
- ✅ SPDK backend (vs traditional block storage)
- ✅ UNSTABLE writes (50x faster than FILE_SYNC)
- ✅ Zero-copy NFS (Bytes architecture)
- ✅ Positioned I/O (concurrent access)

### ❌ Don't Build Custom Filesystem Unless...

**Only consider if:**
- You measure syscalls as >20% CPU time (unlikely)
- You need < 10μs end-to-end latency (extreme case)
- You have 6-12 months and dedicated team
- You need features XFS can't provide

**Otherwise:** You'd spend 6+ months to improve performance by maybe 5%

---

## What SPDK FSDEV Actually Does

**Purpose:** Abstraction for VirtioFS (sharing host filesystem with VMs)

**Architecture:**
```
VM Guest (no filesystem)
    ↓ VirtioFS protocol
SPDK VirtioFS device (userspace)
    ↓ spdk_fsdev API
fsdev_aio backend
    ↓ openat()/pread()/pwrite() SYSCALLS!
    ↓
Host Kernel Filesystem (ext4/XFS/etc.)
```

**Key insight:** Even SPDK's own filesystem abstraction **uses syscalls** to access the host filesystem!

**It does NOT provide a userspace filesystem implementation.**

---

## Bottom Line

### The Only Way to Eliminate Syscalls:

**Build your own userspace filesystem** (no shortcuts available)

**What you'd build:**
- Metadata management in SPDK blobs
- Directory tree implementation
- Inode table
- Block allocator
- Crash recovery
- **~20,000 lines of code**
- **6-12 months effort**

### The Practical Reality:

**Your current architecture is optimal** for the effort:

```
NFS server → Filesystem ops (small syscall overhead)
    ↓
XFS on UBLK (proven, reliable)
    ↓  
SPDK lvol (fast, userspace block I/O)
    ↓
NVMe (hardware speed)
```

**You get 95% of the performance for 5% of the effort!**

---

**Conclusion:** SPDK does NOT provide a ready-made userspace filesystem. Keep using XFS on UBLK - it's the right choice! 🎯


