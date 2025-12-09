# NFS Lock Test

Simple test program to verify NLM (Network Lock Manager) functionality.

## Quick Start

```bash
# Compile
gcc -o nfs-lock-test nfs-lock-test.c

# Run (from REMOTE machine with NFS mounted)
./nfs-lock-test /mnt/nfs
```

## What It Tests

1. **Exclusive (write) locks** - fcntl F_WRLCK
2. **Lock conflict detection** - Parent/child process lock conflicts
3. **Shared (read) locks** - Multiple concurrent read locks

## Requirements

- Must run from **remote machine** (not localhost)
- NFS must be mounted WITHOUT `nolock` option
- Server must have NLM implementation running

## Expected Output

```
╔═══════════════════════════════════════════════════════════╗
║            NFS File Locking Test Suite                   ║
╚═══════════════════════════════════════════════════════════╝

Test file: /mnt/nfs/lock-test.dat

=== Test 1: Exclusive Lock ===
Acquiring exclusive lock...
✓ Exclusive lock acquired
✓ Lock test passed (no conflict)
✓ Lock released

=== Test 2: Lock Conflict Detection ===
Parent: Acquiring lock on bytes 0-100...
✓ Parent: Lock acquired
  Child: Testing for lock conflict on bytes 50-150...
  ✓ Child: Conflict detected (lock held by PID 1234)
✓ Lock conflict detection working correctly

=== Test 3: Shared (Read) Locks ===
Acquiring shared lock (fd1)...
✓ Shared lock acquired (fd1)
Acquiring second shared lock (fd2)...
✓ Second shared lock acquired (fd2)
✓ Multiple shared locks working correctly

╔═══════════════════════════════════════════════════════════╗
║                  ALL TESTS PASSED ✓                       ║
╚═══════════════════════════════════════════════════════════╝

NLM (Network Lock Manager) is working correctly!
```

## See Also

- [NLM Testing Guide](../../docs/NLM_TESTING.md) - Comprehensive testing guide
- [NLM Next Steps](../../docs/NLM_NEXT_STEPS.md) - Implementation analysis
