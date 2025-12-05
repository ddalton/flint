# NFSv3 Server Testing Guide

This document describes how to test the Flint NFSv3 server for RFC 1813 compliance.

## Unit and Integration Tests

Run the built-in Rust tests:

```bash
cd spdk-csi-driver
cargo test --lib nfs
```

These tests verify:
- ✅ XDR encoding/decoding (RFC 4506 compliance)
- ✅ RPC message handling (RFC 5531 compliance)
- ✅ File handle management
- ✅ All NFSv3 operations (NULL, GETATTR, LOOKUP, READ, WRITE, CREATE, etc.)
- ✅ Concurrent operations
- ✅ Large file handling
- ✅ Deep directory trees
- ✅ Filesystem statistics

## Manual Testing with Real NFS Client

### 1. Start the NFS Server

```bash
# Create a test directory
mkdir -p /tmp/nfs-test-export
echo "Hello from NFS" > /tmp/nfs-test-export/test.txt

# Build and run the server
cargo build --release --bin flint-nfs-server
sudo ./target/release/flint-nfs-server \
  --export-path /tmp/nfs-test-export \
  --volume-id test-vol \
  --port 2049
```

### 2. Mount from a Client

**On macOS:**
```bash
mkdir /tmp/nfs-mount
sudo mount -t nfs -o vers=3,tcp,resvport 127.0.0.1:/ /tmp/nfs-mount
```

**On Linux:**
```bash
mkdir /tmp/nfs-mount
sudo mount -t nfs -o vers=3,tcp,nolock 127.0.0.1:/ /tmp/nfs-mount
```

### 3. Test Basic Operations

```bash
# Read existing file
cat /tmp/nfs-mount/test.txt

# Create a new file
echo "New file" > /tmp/nfs-mount/new.txt

# Create directory
mkdir /tmp/nfs-mount/mydir

# List contents
ls -la /tmp/nfs-mount

# Copy large file
dd if=/dev/zero of=/tmp/nfs-mount/large.bin bs=1M count=100

# Verify
ls -lh /tmp/nfs-mount/large.bin

# Remove file
rm /tmp/nfs-mount/new.txt

# Remove directory
rmdir /tmp/nfs-mount/mydir

# Unmount
sudo umount /tmp/nfs-mount
```

## Standard NFS Conformance Tests

### Connectathon NFS Test Suite

The industry-standard test suite for NFS compliance:

**Download and build:**
```bash
git clone https://github.com/Connectathon/Connectathon-NFS-Tests.git
cd Connectathon-NFS-Tests
make
```

**Run tests:**
```bash
# Mount the NFS server first
sudo mount -t nfs -o vers=3,tcp 127.0.0.1:/ /mnt/nfs-test

# Run basic tests
cd /mnt/nfs-test
./cthon04/runtests -b -t /mnt/nfs-test

# Run general tests
./cthon04/runtests -g -t /mnt/nfs-test

# Run special tests
./cthon04/runtests -s -t /mnt/nfs-test

# Run all tests
./cthon04/runtests -a -t /mnt/nfs-test
```

### NFStest - Python-based NFS Test Tool

**Install:**
```bash
pip install nfstest
```

**Run tests:**
```bash
# Basic functionality test
nfstest_posix --server 127.0.0.1 --export / --nfsversion 3

# File locking test
nfstest_lock --server 127.0.0.1 --export / --nfsversion 3

# Cache validation test
nfstest_cache --server 127.0.0.1 --export / --nfsversion 3

# Delegation test (NFSv4 - we don't support this yet)
# nfstest_delegation --server 127.0.0.1 --export / --nfsversion 3
```

## Performance Testing

### fio (Flexible I/O Tester)

```bash
# Mount the NFS export
sudo mount -t nfs -o vers=3,tcp 127.0.0.1:/ /mnt/nfs-test

# Sequential read test
fio --name=seq-read --rw=read --bs=1M --size=1G \
    --numjobs=4 --directory=/mnt/nfs-test

# Sequential write test
fio --name=seq-write --rw=write --bs=1M --size=1G \
    --numjobs=4 --directory=/mnt/nfs-test

# Random read/write test
fio --name=rand-rw --rw=randrw --bs=4k --size=100M \
    --numjobs=8 --directory=/mnt/nfs-test
```

### iozone

```bash
# Install iozone (on macOS: brew install iozone)
# On mounted NFS share
cd /mnt/nfs-test
iozone -a -g 2G
```

## Stress Testing

### Multiple Concurrent Clients

```bash
# Terminal 1
mount -t nfs -o vers=3,tcp 127.0.0.1:/ /tmp/mount1
while true; do echo $(date) >> /tmp/mount1/client1.log; sleep 1; done

# Terminal 2
mount -t nfs -o vers=3,tcp 127.0.0.1:/ /tmp/mount2
while true; do echo $(date) >> /tmp/mount2/client2.log; sleep 1; done

# Terminal 3
mount -t nfs -o vers=3,tcp 127.0.0.1:/ /tmp/mount3
while true; do echo $(date) >> /tmp/mount3/client3.log; sleep 1; done
```

### Bonnie++ (Comprehensive benchmark)

```bash
# Install bonnie++ (on macOS: brew install bonnie++)
bonnie++ -d /mnt/nfs-test -u root
```

## Debugging and Monitoring

### Enable verbose logging

```bash
./target/release/flint-nfs-server \
  --export-path /tmp/nfs-test-export \
  --volume-id test-vol \
  --verbose
```

### Monitor NFS traffic with tcpdump

```bash
sudo tcpdump -i lo0 -n port 2049 -X
```

### Check NFS statistics (Linux only)

```bash
nfsstat -c  # Client stats
nfsstat -s  # Server stats
```

## RFC 1813 Compliance Checklist

Based on [RFC 1813](https://www.rfc-editor.org/rfc/rfc1813.html):

### Implemented Procedures
- ✅ NULL (Procedure 0) - Do nothing
- ✅ GETATTR (Procedure 1) - Get file attributes
- ✅ LOOKUP (Procedure 3) - Lookup filename
- ✅ READ (Procedure 6) - Read from file
- ✅ WRITE (Procedure 7) - Write to file
- ✅ CREATE (Procedure 8) - Create a file
- ✅ MKDIR (Procedure 9) - Create a directory
- ✅ REMOVE (Procedure 12) - Remove a file
- ✅ RMDIR (Procedure 13) - Remove a directory
- ✅ READDIR (Procedure 16) - Read from directory
- ✅ FSSTAT (Procedure 18) - Get dynamic file system information
- ✅ FSINFO (Procedure 19) - Get static file system information

### Not Yet Implemented (Optional for basic functionality)
- ⬜ SETATTR (Procedure 2) - Set file attributes
- ⬜ ACCESS (Procedure 4) - Check access permission
- ⬜ READLINK (Procedure 5) - Read from symbolic link
- ⬜ SYMLINK (Procedure 10) - Create a symbolic link
- ⬜ MKNOD (Procedure 11) - Create a special device
- ⬜ RENAME (Procedure 14) - Rename a file or directory
- ⬜ LINK (Procedure 15) - Create link to an object
- ⬜ READDIRPLUS (Procedure 17) - Extended read from directory
- ⬜ PATHCONF (Procedure 20) - Retrieve POSIX information
- ⬜ COMMIT (Procedure 21) - Commit cached data on a server to stable storage

### Protocol Features
- ✅ XDR encoding (RFC 4506)
- ✅ RPC v2 (RFC 5531)
- ✅ TCP transport
- ✅ UDP transport
- ✅ File handle management
- ✅ Null authentication (AUTH_NULL)
- ⬜ Unix authentication (AUTH_UNIX)
- ⬜ Kerberos authentication (AUTH_GSS)

## Expected Performance Targets

From `NFS_IMPLEMENTATION_ROADMAP.md`:

- **Read throughput:** 1-2 GB/s (over NFS)
- **Write throughput:** 800 MB/s - 1.5 GB/s
- **Metadata latency:** < 500μs for operations like GETATTR, LOOKUP

## Troubleshooting

### "Permission denied" errors
```bash
# On macOS, NFS requires reserved ports
mount -t nfs -o vers=3,tcp,resvport 127.0.0.1:/ /mnt/test
```

### "Stale file handle" errors
```bash
# Restart the NFS server, then remount
sudo umount /mnt/test
sudo mount -t nfs -o vers=3,tcp 127.0.0.1:/ /mnt/test
```

### Server not responding
```bash
# Check server is running
ps aux | grep flint-nfs-server

# Check port is listening
sudo lsof -i :2049

# Check firewall rules
sudo pfctl -s rules | grep 2049  # macOS
sudo iptables -L | grep 2049     # Linux
```

## Continuous Integration

Add to `.github/workflows/test.yml`:

```yaml
- name: Test NFS Server
  run: |
    cargo test --lib nfs
    
    # Start NFS server in background
    mkdir -p /tmp/nfs-test
    cargo run --release --bin flint-nfs-server -- \
      --export-path /tmp/nfs-test \
      --volume-id ci-test &
    
    sleep 2
    
    # Mount and test
    mkdir -p /tmp/nfs-mount
    sudo mount -t nfs -o vers=3,tcp,nolock 127.0.0.1:/ /tmp/nfs-mount
    
    # Basic smoke tests
    echo "test" > /tmp/nfs-mount/test.txt
    cat /tmp/nfs-mount/test.txt
    
    sudo umount /tmp/nfs-mount
```

## References

- [RFC 1813 - NFS Version 3 Protocol Specification](https://www.rfc-editor.org/rfc/rfc1813.html)
- [RFC 4506 - XDR: External Data Representation Standard](https://www.rfc-editor.org/rfc/rfc4506.html)
- [RFC 5531 - RPC: Remote Procedure Call Protocol Specification Version 2](https://www.rfc-editor.org/rfc/rfc5531.html)
- [Connectathon NFS Test Suite](https://github.com/Connectathon/Connectathon-NFS-Tests)
- [NFStest Documentation](https://wiki.linux-nfs.org/wiki/index.php/NFStest)

