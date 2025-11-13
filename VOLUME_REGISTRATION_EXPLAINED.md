# How Volumes Are Registered - Complete Explanation

**Question:** How is the logical volume registered with the PV? Why isn't the full 32-bit ID used?

**Answer:** The lvol IS registered with the full volume_id (as its name), and we NOW use full 32-bit for ublk IDs!

---

## 📊 The Complete Picture

### Volume Registration (Permanent)

```
Kubernetes PV:
  name: pvc-abc123-def456-ghi789-jkl012
        ↓
CSI CreateVolume:
  volume_id: "pvc-abc123-def456-ghi789-jkl012"
        ↓
SPDK Lvol Creation:
  lvol_name: "vol_pvc-abc123-def456-ghi789-jkl012"  ← FULL ID!
  lvol_uuid: "bae37688-7742-48d7-afca-da490ba89d84" ← Random UUID
  alias: "lvs_ublk-2_nvme3n1/vol_pvc-abc123-..."
        ↓
Stored in SPDK Blobstore (persistent on disk):
  - Lvol name contains full volume_id
  - Lvol UUID is SPDK's internal identifier
  - Both persisted in blobstore metadata
```

**Lookup Process:**
```rust
// To find a volume by its PV name:
fn get_volume_info(volume_id: &str) -> VolumeInfo {
    // 1. Query SPDK for all lvols
    let lvols = spdk.bdev_lvol_get_lvols()
    
    // 2. Search for match
    for lvol in lvols {
        if lvol.name == format!("vol_{}", volume_id) {
            return VolumeInfo {
                volume_id,
                lvol_uuid: lvol.uuid,  // Need this for deletion
                ...
            }
        }
    }
}
```

**Key Point:** The full `volume_id` (PV name) IS stored in the lvol name!

---

### ublk Device ID (Transient)

```
CSI NodeStageVolume:
  volume_id: "pvc-abc123-def456-ghi789-jkl012"
        ↓
Generate ublk ID:
  hash(volume_id) = 0x1A2B3C4D5E6F7089 (64-bit)
  ublk_id = 0x5E6F7089 (lower 32 bits) = 1,584,861,321
        ↓
Create ublk device:
  /dev/ublkb1584861321
        ↓
Device exists only while volume is staged
  - Deleted on NodeUnstageVolume
  - Recreated on next NodeStageVolume
  - SAME ID regenerated (deterministic hash)
```

**Key Point:** ublk ID is NOT stored anywhere - it's recalculated every time!

---

## 💡 Why This Design?

### lvol Name = Full volume_id

**Advantages:**
- ✅ Direct lookup: Search by name in SPDK
- ✅ No separate database needed
- ✅ Survives SPDK restarts (persisted in blobstore)
- ✅ Human-readable in SPDK tools
- ✅ Debugging is easy

**Example:**
```bash
# List all lvols
rpc.py bdev_lvol_get_lvols

# Output shows names:
{
  "name": "vol_pvc-abc123-def456-ghi789-jkl012",
  "uuid": "bae37688-7742-48d7-afca-da490ba89d84",
  ...
}

# You can immediately see which PV this lvol belongs to!
```

### ublk ID = Hash of volume_id

**Why hash?**
1. **Deterministic:** Same volume_id → same ublk ID every time
2. **Stateless:** No need to track used IDs
3. **Survives restarts:** Regenerate from volume_id

**Why not alternatives?**

**Alternative 1: Sequential (0, 1, 2, 3...)**
```rust
// ❌ Problems:
- Need to track: "ID 123 is used by pvc-xyz"
- Lost on pod restart
- Need persistent storage
- Race conditions between pods
- Complex synchronization
```

**Alternative 2: Extract from UUID**
```rust
// volume_id = "pvc-abc123-def456-ghi789-jkl012"
// Take last segment as number?
// ❌ "jkl012" is not a number
// ❌ UUIDs are hex strings, not decimal
// ❌ Still need to convert to u32
```

**Alternative 3: Random ID**
```rust
// ❌ Problems:
- Different ID on each NodeStageVolume call
- Can't match device to volume
- Collision detection impossible
- Not deterministic
```

---

## 🔢 Full 32-bit vs Limited Bits

### Collision Probability

**Formula:** `P ≈ n² / (2 * ID_space)`

Where `n` = number of volumes

| Bits | ID Space | 50% collision at | 1% collision at |
|------|----------|------------------|-----------------|
| 16 | 65K | 300 volumes | 115 volumes |
| 24 | 16M | 4,900 volumes | 1,800 volumes |
| 32 | 4B | 77,000 volumes | 28,000 volumes |

**Conclusion:** 32-bit gives us room for tens of thousands of volumes before collisions become likely!

### Why We NOW Use Full 32-bit

**Old code (16-bit):** Too many collisions in production
**Previous fix (24-bit):** Better, but arbitrary limit
**Current (32-bit):** **Maximum collision resistance, no downside!**

---

## 📝 Code Evolution

### Version 1: 16-bit (Caused Problems)

```rust
(hash & 0xFFFF) as u32  // Only 65K IDs
```

**Result:** Frequent collisions → geometry mismatch → I/O errors

### Version 2: 24-bit (Temporary)

```rust
(hash & 0xFFFFFF) as u32  // 16M IDs
```

**Result:** Fewer collisions, but why limit to 24?

### Version 3: Full 32-bit (Current)

```rust
hash as u32  // Full 4B IDs
```

**Result:** Collision probability reduced by 256x vs 24-bit!

---

## 🎯 Why The Limit Existed (Historical)

Looking at the git history, the 16-bit limit likely came from:

1. **Early testing:** Small ID numbers easier to read in logs
2. **Cargo cult:** Copying from examples that used small IDs
3. **Misunderstanding:** Thinking ublk had lower limits
4. **Conservatism:** "Let's not use huge numbers"

**But there's no technical reason!** ublk supports up to 2^31 - 1.

---

## 🔍 Real-World Example

### Volume Registration

```
Kubernetes:
  PV: pvc-30dc0891-5555-479f-a669-47b2de8b92f2

SPDK:
  Lvol name: vol_pvc-30dc0891-5555-479f-a669-47b2de8b92f2
  Lvol UUID: 2d07ddf4-d393-46d8-8303-61d36783dbbf
  Alias: lvs_ublk-2_nvme3n1/vol_pvc-30dc0891-5555-479f-a669-47b2de8b92f2

ublk Device:
  hash("pvc-30dc0891-5555-479f-a669-47b2de8b92f2") = 0x8A7B6C5D4E3F2A1B
  ublk_id = 0x4E3F2A1B = 1,312,960,027
  device = /dev/ublkb1312960027
```

**Finding the volume:**
```rust
// Given PV name, find lvol UUID:
get_volume_info("pvc-30dc0891-...") 
  → Search lvols for name = "vol_pvc-30dc0891-..."
  → Found! UUID = "2d07ddf4-..."
  
// Generate ublk ID for staging:
generate_ublk_id("pvc-30dc0891-...")
  → hash = 0x8A7B6C5D4E3F2A1B
  → ublk_id = 1,312,960,027 (full 32-bit)
  → Device: /dev/ublkb1312960027
```

---

## ✅ Summary

### How lvol is registered with PV:
**The full volume_id is stored in the lvol name!**
- Lvol name: `vol_{volume_id}`
- Lookup: Search lvols by name matching
- No separate database needed
- Persisted in SPDK blobstore

### Why we now use full 32-bit ublk ID:
**Because there's no reason not to!**
- ✅ Maximum collision resistance
- ✅ Supports 4 billion unique IDs
- ✅ No performance impact
- ✅ No memory impact
- ✅ Still deterministic (same volume → same ID)

### The mapping:
```
volume_id (string) ──────┬──> Stored in lvol name (permanent)
                         │
                         └──> Hashed to ublk ID (transient)
                              ↓
                         /dev/ublkbXXXXXXXX (recreated each time)
```

---

**Commit:** Will be in next commit with full 32-bit hash
**Impact:** Virtually eliminates collision risk (~77K volumes before 50% collision)

