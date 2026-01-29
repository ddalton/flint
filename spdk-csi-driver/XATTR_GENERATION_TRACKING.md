# SPDK Lvol Xattr Support for Generation Tracking

This document describes the new RPC methods added to SPDK for storing generation metadata in logical volumes using blob xattrs.

## Overview

The `lvol-xattr-rpc.patch` adds three new RPC methods to SPDK v25.09.x:
- `bdev_lvol_set_xattr` - Set custom metadata on an lvol
- `bdev_lvol_get_xattr` - Read custom metadata from an lvol  
- `bdev_lvol_remove_xattr` - Remove custom metadata from an lvol

These enable **generation tracking** for detecting out-of-sync replicas in distributed storage without external state management.

## Why Xattrs?

✅ **Expansion-safe**: Stored in blob metadata, unaffected by volume resize  
✅ **Zero I/O overhead**: No impact on read/write data path  
✅ **Self-contained**: Metadata travels with the volume  
✅ **Persistent**: Survives node restarts and failures  
✅ **No regressions**: Pure addition, doesn't modify existing SPDK code  

## RPC Method Signatures

### Set Xattr
```bash
rpc.py bdev_lvol_set_xattr \
  --name "lvs1/pvc-12345" \
  --xattr_name "csi.generation" \
  --xattr_value "AAAFSQAAAAAAAAAAAAAAAAAAAAAA{BASE64}"
```

**Parameters:**
- `name` (string, required): Lvol bdev name (e.g., "lvs1/volume-name")
- `xattr_name` (string, required): Xattr key name
- `xattr_value` (string, required): Base64-encoded binary value

**Returns:** `true` on success

### Get Xattr
```bash
rpc.py bdev_lvol_get_xattr \
  --name "lvs1/pvc-12345" \
  --xattr_name "csi.generation"
```

**Returns:**
```json
{
  "name": "lvs1/pvc-12345",
  "xattr_name": "csi.generation",
  "xattr_value": "AAAFSQAAAAAAAAAAAAAAAAAAAAAA{BASE64}",
  "value_len": 24
}
```

### Remove Xattr
```bash
rpc.py bdev_lvol_remove_xattr \
  --name "lvs1/pvc-12345" \
  --xattr_name "csi.generation"
```

**Returns:** `true` on success

## Generation Metadata Format

The CSI driver stores generation metadata as a 24-byte binary structure:

```c
struct GenerationMetadata {
    uint32_t magic;      // 0x4753504B ("GSPK")
    uint64_t generation; // Monotonically increasing counter
    uint64_t timestamp;  // Unix timestamp (seconds)
    uint32_t node_id;    // Node identifier
    // Optional: add CRC32 for validation
};
```

## Python Example: Generation Tracking

```python
#!/usr/bin/env python3
import subprocess
import json
import base64
import struct
import time

def pack_generation_metadata(generation, node_id):
    """Pack generation metadata into binary format"""
    magic = 0x4753504B  # "GSPK"
    timestamp = int(time.time())
    data = struct.pack('<IQQ I', magic, generation, timestamp, node_id)
    return base64.b64encode(data).decode('ascii')

def unpack_generation_metadata(b64_data):
    """Unpack generation metadata from base64"""
    data = base64.b64decode(b64_data)
    magic, generation, timestamp, node_id = struct.unpack('<IQQI', data[:24])
    
    if magic != 0x4753504B:
        return 0, 0, 0  # Not initialized
    
    return generation, timestamp, node_id

def rpc_call(method, params):
    """Call SPDK RPC method"""
    cmd = ["sudo", "/usr/local/scripts/rpc.py", method]
    for key, value in params.items():
        cmd.extend([f"--{key}", str(value)])
    
    result = subprocess.check_output(cmd, stderr=subprocess.STDOUT)
    return json.loads(result)

def set_generation(lvol_name, generation, node_id):
    """Set generation on an lvol"""
    value = pack_generation_metadata(generation, node_id)
    rpc_call("bdev_lvol_set_xattr", {
        "name": lvol_name,
        "xattr_name": "csi.generation",
        "xattr_value": value
    })
    print(f"✓ Set generation {generation} on {lvol_name}")

def get_generation(lvol_name):
    """Get generation from an lvol"""
    try:
        result = rpc_call("bdev_lvol_get_xattr", {
            "name": lvol_name,
            "xattr_name": "csi.generation"
        })
        gen, ts, node = unpack_generation_metadata(result['xattr_value'])
        return gen, ts, node
    except subprocess.CalledProcessError:
        # Xattr not set yet
        return 0, 0, 0

# Example: Check 3 replicas for consistency
replicas = [
    "lvs1/pvc-12345-replica-0",
    "lvs2/pvc-12345-replica-1", 
    "lvs3/pvc-12345-replica-2"
]

print("Checking replica generations...")
generations = []
for replica in replicas:
    gen, ts, node = get_generation(replica)
    generations.append((replica, gen, ts, node))
    print(f"  {replica}: gen={gen}, timestamp={ts}, node={node}")

max_gen = max(g[1] for g in generations)
stale_replicas = [g[0] for g in generations if g[1] < max_gen]

if stale_replicas:
    print(f"\n⚠️  Stale replicas detected: {stale_replicas}")
    print(f"   These replicas need to be rebuilt from generation {max_gen}")
else:
    print("\n✓ All replicas are current")

# Increment generation on volume attach
new_gen = max_gen + 1
node_id = 123  # Current node ID
print(f"\nIncrementing generation to {new_gen} for volume attach...")
for replica in replicas:
    set_generation(replica, new_gen, node_id)

print("\n✓ Generation tracking complete")
```

## CSI Driver Integration Workflow

### On NodePublishVolume (Pod Attach)

1. **Attach all NVMe-oF replicas** to the node
2. **Read generations** from all replicas using `bdev_lvol_get_xattr`
3. **Compare generations** to find max generation
4. **Identify stale replicas** (generation < max)
5. **Rebuild stale replicas** by copying from current replica
6. **Increment generation** on all replicas using `bdev_lvol_set_xattr`
7. **Create RAID1** (md-raid or SPDK RAID) over replicas
8. **Mount volume** to pod

### On NodeUnpublishVolume (Pod Detach)

1. **Unmount volume** from pod
2. **Destroy RAID device**
3. **Detach NVMe-oF replicas**
4. No generation update needed (happens on next attach)

## Building SPDK with Xattr Support

The patch is automatically applied during Docker build:

```bash
cd /path/to/spdk-csi-driver
docker build -f docker/Dockerfile.spdk -t spdk-csi:xattr .
```

Build log will show:
```
✅ Lvol xattr RPC support applied (bdev_lvol_set/get/remove_xattr)
```

## Testing the RPC Methods

After building and starting SPDK:

```bash
# Create an lvol for testing
rpc.py bdev_malloc_create 512 4096 -b malloc0
rpc.py bdev_lvol_create_lvstore malloc0 lvs1
rpc.py bdev_lvol_create -l lvs1 -n test_vol 100

# Set a generation
GEN_DATA=$(echo -n "GSPK" | base64)  # Simple test value
rpc.py bdev_lvol_set_xattr \
  --name lvs1/test_vol \
  --xattr_name csi.generation \
  --xattr_value "$GEN_DATA"

# Read it back
rpc.py bdev_lvol_get_xattr \
  --name lvs1/test_vol \
  --xattr_name csi.generation

# Should return:
# {
#   "name": "lvs1/test_vol",
#   "xattr_name": "csi.generation",
#   "xattr_value": "R1NQSw==",
#   "value_len": 4
# }

# Remove it
rpc.py bdev_lvol_remove_xattr \
  --name lvs1/test_vol \
  --xattr_name csi.generation
```

## Advantages Over Alternatives

| Approach | Pros | Cons |
|----------|------|------|
| **Xattrs (This)** | ✅ Self-contained<br>✅ Zero overhead<br>✅ Expansion-safe | Requires SPDK patch |
| Reserved blocks at end | ✅ No SPDK changes | ❌ Breaks expansion<br>❌ Device mapper complexity |
| External metadata (K8s) | ✅ No SPDK changes | ❌ External dependency<br>❌ Not self-contained |
| Sidecar metadata lvols | ✅ No SPDK changes | ❌ Complexity<br>❌ Multiple volumes to manage |

## Upstreaming to SPDK

This patch is designed to be upstream-friendly:
- **Non-invasive**: Pure addition, no modifications to existing code
- **Follows SPDK patterns**: Uses existing RPC registration macros
- **Documented**: Clear use case and benefits
- **Safe**: No impact on existing functionality

Consider submitting to: spdk@lists.01.org

## Troubleshooting

### Error: "bdev_lvol_set_xattr: command not found"
- Ensure you built SPDK with the patch applied
- Check build logs for "✅ Lvol xattr RPC support applied"

### Error: "not an lvol bdev"
- Verify the bdev name is correct
- Ensure it's an lvol, not a malloc or nvme bdev

### Error: "invalid base64 value"  
- Xattr values must be base64-encoded
- Use Python's `base64.b64encode()` or shell's `base64`

### Error: "xattr not found" (ENOENT)
- The xattr hasn't been set yet (this is expected on first read)
- Initialize with generation 0

## See Also

- [SPDK Blob Documentation](https://spdk.io/doc/blob.html)
- [SPDK RPC Documentation](https://spdk.io/doc/jsonrpc.html)
- [Xattr API Reference](https://spdk.io/doc/blob_8h.html)
