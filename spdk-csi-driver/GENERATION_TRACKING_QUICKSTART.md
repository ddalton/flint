# Generation Tracking Quick Reference

## Quick Start

### Prerequisites

1. Build SPDK with xattr patch:
   ```bash
   cd spdk-csi-driver
   docker build -f docker/Dockerfile.spdk -t spdk-csi:xattr .
   ```

2. Verify patch applied:
   ```bash
   # Check build logs for:
   ✅ Lvol xattr RPC support applied (bdev_lvol_set/get/remove_xattr)
   ```

### Normal Operation

Generation tracking works automatically - no configuration needed!

**Volume Creation → First Attach:**
```
🔍 Checking generations for 3 replicas...
   Replica 0: uninitialized
   Replica 1: uninitialized  
   Replica 2: uninitialized
📈 New generation: 1 (from 0)
✅ Generation tracking complete
```

**Subsequent Attaches:**
```
🔍 Checking generations for 3 replicas...
   Replica 0: generation=5
   Replica 1: generation=5
   Replica 2: generation=5
✅ All replicas are in sync
📈 New generation: 6 (from 5)
```

**Stale Replica Detected:**
```
🔍 Checking generations for 3 replicas...
   Replica 0: generation=10
   Replica 1: generation=7   ⚠️ Stale
   Replica 2: generation=10
⚠️ WARNING: Detected 1 out-of-sync replicas
⚠️ Proceeding in DEGRADED mode with current replicas only
📈 New generation: 11
```

## Manual Operations

### Check Generation on a Replica

```bash
# Via SPDK RPC (on node where replica exists)
rpc.py bdev_lvol_get_xattr \
  --name "replica-lvol-uuid" \
  --xattr_name "csi.generation"
```

Returns:
```json
{
  "name": "replica-lvol-uuid",
  "xattr_name": "csi.generation",
  "xattr_value": "R1NQSwAAAAoAAAAAAAA...",  // base64 encoded
  "value_len": 24
}
```

### Decode Generation (Python)

```python
#!/usr/bin/env python3
import base64
import struct

def decode_generation(b64_value):
    data = base64.b64decode(b64_value)
    magic, gen, ts, node = struct.unpack('<IQQI', data[:24])
    
    if magic != 0x4753504B:
        print("Invalid magic!")
        return
    
    print(f"Generation: {gen}")
    print(f"Timestamp: {ts}")
    print(f"Node ID: 0x{node:08x}")

# Example
decode_generation("R1NQSwAAAAoAAAAAAAAAAAAAAAA=")
```

### Reset Generation (Testing Only)

```bash
# Remove generation xattr (forces re-initialization)
rpc.py bdev_lvol_remove_xattr \
  --name "replica-lvol-uuid" \
  --xattr_name "csi.generation"
```

⚠️ **WARNING**: This makes replica appear uninitialized. Only use for testing!

## Troubleshooting

### Issue: "bdev_lvol_set_xattr: command not found"

**Cause**: SPDK not built with xattr patch

**Fix**:
1. Rebuild SPDK container: `docker build -f docker/Dockerfile.spdk ...`
2. Check build logs for xattr patch confirmation
3. Redeploy pods with new image

### Issue: "WARNING: Detected N out-of-sync replicas"

**Cause**: Replica was offline during previous attaches

**What Happens**: 
- Volume attaches in DEGRADED mode with current replicas
- Stale replicas excluded from RAID

**Fix** (manual rebuild required):
1. Verify replica node is healthy
2. Identify current replica (highest generation)
3. Copy data to stale replica (future: automatic)
4. Re-attach volume to include rebuilt replica

### Issue: "Failed to decode generation: InvalidMagic"

**Cause**: Corrupted xattr metadata

**Fix**:
```bash
# Remove corrupted xattr
rpc.py bdev_lvol_remove_xattr \
  --name "replica-lvol-uuid" \
  --xattr_name "csi.generation"

# Re-attach volume to re-initialize
```

## Log Messages Reference

| Message | Meaning | Action |
|---------|---------|--------|
| `📊 [GEN_TRACK] Checking generations...` | Reading generation from all replicas | Normal operation |
| `✅ All replicas are in sync` | All generations match (healthy) | None - proceed normally |
| `⚠️ Detected N out-of-sync replicas` | Some replicas have stale generation | Volume works in degraded mode |
| `📈 New generation: N` | Incremented generation successfully | Normal - generation updated |
| `⚠️ Failed to increment generation` | Non-fatal error updating generation | Logged only, volume still attaches |
| `ℹ️ No generation xattr (uninitialized)` | New replica without generation yet | Normal for first attach |

## Testing Scenarios

### Test 1: First Attach (Normal)

```bash
# Create PVC
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-gen-pvc
spec:
  storageClassName: spdk-replicated
  accessModes: ["ReadWriteOnce"]
  resources:
    requests:
      storage: 1Gi
EOF

# Create pod
kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: test-gen-pod
spec:
  containers:
  - name: test
    image: busybox
    command: ["sleep", "3600"]
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: test-gen-pvc
EOF

# Check logs
kubectl logs -n flint-system spdk-node-<node> -c node-agent
# Look for: "📈 New generation: 1 (from 0)"
```

### Test 2: Re-attach (Generation Increment)

```bash
# Delete pod
kubectl delete pod test-gen-pod

# Recreate pod
kubectl apply -f pod.yaml

# Check logs
kubectl logs -n flint-system spdk-node-<node> -c node-agent
# Look for: "✅ All replicas are in sync"
# Look for: "📈 New generation: 2 (from 1)"
```

### Test 3: Simulate Stale Replica

```bash
# SSH to node with replica
ssh node-1

# Find replica lvol UUID
kubectl get pv <pv-name> -o yaml | grep lvol-uuid

# Reset generation on one replica
rpc.py bdev_lvol_remove_xattr \
  --name "<replica-uuid>" \
  --xattr_name "csi.generation"

# Re-attach volume
kubectl delete pod test-gen-pod
kubectl apply -f pod.yaml

# Check logs
kubectl logs -n flint-system spdk-node-<node> -c node-agent
# Look for: "⚠️ WARNING: Detected 1 out-of-sync replicas"
```

## API Reference

### Rust API

```rust
use spdk_csi_driver::generation_tracking::*;

// Create new generation
let gen = GenerationMetadata::new(10, "node-1");

// Serialize to base64
let b64 = gen.pack_base64();

// Deserialize from base64
let gen2 = GenerationMetadata::unpack_base64(&b64)?;

// Compare generations
let comparison = compare_generations(vec![
    Some(gen1),
    Some(gen2),
    None,  // uninitialized
]);

if comparison.is_consistent() {
    println!("All in sync!");
} else {
    println!("Stale replicas: {:?}", comparison.stale_replicas);
}
```

### Driver Methods

```rust
// Read generation from replica
let gen = driver.read_replica_generation("node-1", "lvol-uuid").await?;

// Write generation to replica
driver.write_replica_generation("node-1", "lvol-uuid", &gen).await?;

// Check all replicas
let result = driver.check_replica_generations(&replicas).await?;

// Increment generation on attach
let new_gen = driver.increment_replica_generations(&replicas, "node-1").await?;
```

## Performance Impact

- **Volume Attach Latency**: +10-50ms (one-time cost per attach)
  - Generation read: ~5ms per replica
  - Generation write: ~5ms per replica
  
- **Data Path**: **0ms** (no impact on reads/writes)

- **Metadata Size**: 24 bytes per replica (negligible)

## Best Practices

1. ✅ **Let it work automatically** - No configuration needed
2. ✅ **Monitor logs** - Watch for stale replica warnings
3. ✅ **Plan for rebuilds** - Manual process currently required
4. ✅ **Test failure scenarios** - Verify behavior with node failures
5. ⚠️ **Don't manually modify** - Use RPC commands only for testing

## Future Enhancements

Coming in future releases:

- 🚧 **Automatic replica rebuild** - Sync stale replicas automatically
- 🚧 **Admin CLI** - Manage generations from command line
- 🚧 **Generation history** - Track generation changes over time
- 🚧 **Metadata v2** - Add CRC32 and versioning

## Related Documentation

- **GENERATION_TRACKING_IMPLEMENTATION.md** - Full implementation details
- **XATTR_GENERATION_TRACKING.md** - SPDK xattr design document
- **lvol-xattr-rpc.patch** - SPDK patch for xattr support

## Support

For issues or questions:
1. Check logs: `kubectl logs -n flint-system spdk-node-<node> -c node-agent`
2. Review documentation in this directory
3. File issue with full logs and error messages
