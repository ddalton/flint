# Docker Build Strategy Comparison

## Question: Single Image vs Separate Images?

Should we build MDS and DS in:
- **Option A**: Single Dockerfile.pnfs (both binaries)
- **Option B**: Separate Dockerfile.pnfs-mds and Dockerfile.pnfs-ds

---

## Build Time Analysis

### Shared Dependencies (99% Overlap)

Both MDS and DS binaries depend on:
- `tokio` (async runtime)
- `tonic` + `prost` (gRPC)
- `dashmap` (concurrent hashmap)
- `tracing` (logging)
- `serde` + `serde_yaml` (config)
- `bytes` (data handling)
- All of `src/pnfs/` (shared code)
- All of `src/nfs/` (base NFS code)

**Overlap**: ~99% of code and dependencies

### Option A: Single Dockerfile (Recommended) ✅

```dockerfile
RUN cargo build --release --bin flint-pnfs-mds --bin flint-pnfs-ds
```

**Build Process**:
```
1. Compile dependencies (5 min)
2. Compile src/pnfs/ (1 min)
3. Compile src/nfs/ (2 min) - shared
4. Link flint-pnfs-mds (30s)
5. Link flint-pnfs-ds (30s)

Total: ~9 minutes
```

### Option B: Separate Dockerfiles

```dockerfile
# Dockerfile.pnfs-mds
RUN cargo build --release --bin flint-pnfs-mds

# Dockerfile.pnfs-ds
RUN cargo build --release --bin flint-pnfs-ds
```

**Build Process** (if built separately):
```
MDS build:
  1. Compile dependencies (5 min)
  2. Compile src/pnfs/ (1 min)
  3. Compile src/nfs/ (2 min)
  4. Link flint-pnfs-mds (30s)
  Total: ~8.5 minutes

DS build:
  1. Compile dependencies (5 min) ← DUPLICATE!
  2. Compile src/pnfs/ (1 min)    ← DUPLICATE!
  3. Compile src/nfs/ (2 min)     ← DUPLICATE!
  4. Link flint-pnfs-ds (30s)
  Total: ~8.5 minutes

Combined: ~17 minutes (if serial) or ~9 minutes (if parallel)
```

**Comparison**:
- Single image: 9 minutes
- Separate images (parallel): 9 minutes (but 2x CPU usage)
- Separate images (serial): 17 minutes

✅ **Single image is more efficient!**

---

## Image Size Analysis

### Option A: Single Image

```
flint/pnfs:latest
  • Contains: both MDS and DS binaries
  • Size: ~70 MB total
    - Base: ~30 MB (Ubuntu)
    - MDS binary: ~20 MB
    - DS binary: ~20 MB
```

**When deploying MDS**: Uses 70 MB image (has unused DS binary)  
**When deploying DS**: Uses 70 MB image (has unused MDS binary)  
**Waste**: ~20 MB unused binary per deployment

### Option B: Separate Images

```
flint/pnfs-mds:latest
  • Contains: MDS binary only
  • Size: ~60 MB

flint/pnfs-ds:latest
  • Contains: DS binary only
  • Size: ~60 MB
```

**When deploying MDS**: Uses 60 MB (no waste)  
**When deploying DS**: Uses 60 MB (no waste)  
**Savings**: ~10 MB per pod

**With 200 DS pods + 1 MDS pod**:
- Single image: 201 × 70 MB = 14.07 GB
- Separate images: 200 × 60 MB + 1 × 60 MB = 12.06 GB
- **Savings**: 2 GB total

---

## Practical Considerations

### Build Time

**Single Dockerfile**: ✅ Faster (9 min)
- Dependencies compiled once
- Shared build cache
- More efficient CI/CD

**Separate Dockerfiles**: ⚠️ Slower or more CPU
- Must compile dependencies twice (unless parallel)
- Parallel: Same time but 2x CPU
- Serial: Nearly 2x time

### Deployment

**Single Image**:
```yaml
# MDS
spec:
  containers:
  - name: mds
    image: flint/pnfs:latest
    command: ["/usr/local/bin/flint-pnfs-mds"]

# DS
spec:
  containers:
  - name: ds
    image: flint/pnfs:latest
    command: ["/usr/local/bin/flint-pnfs-ds"]
```

**Separate Images**:
```yaml
# MDS
spec:
  containers:
  - name: mds
    image: flint/pnfs-mds:latest

# DS
spec:
  containers:
  - name: ds
    image: flint/pnfs-ds:latest
```

**Advantage of single**: Always in sync (same image tag)  
**Advantage of separate**: Clearer which component

### Security

**Single Image**:
- MDS pod has DS binary (not used, but present)
- DS pod has MDS binary (not used, but present)
- Slightly larger attack surface

**Separate Images**:
- Each pod has only what it needs
- Principle of least privilege
- Smaller attack surface

**Impact**: Minimal (binaries are sandboxed anyway)

### Image Updates

**Single Image**:
- Change MDS code → Must rebuild entire image
- Change DS code → Must rebuild entire image
- **But**: Build is fast because shared compilation

**Separate Images**:
- Change MDS code → Only rebuild MDS image
- Change DS code → Only rebuild DS image
- **But**: Each build compiles dependencies separately

---

## Recommendation: **Single Dockerfile.pnfs** ✅

### Why Single Image is Better for Your Use Case

1. **Much Faster Builds** ✅
   ```
   Single: 9 minutes
   Separate (serial): 17 minutes
   Separate (parallel): 9 min but 2x CPU
   
   Winner: Single (same time, less CPU)
   ```

2. **Simpler CI/CD** ✅
   ```
   Single: One Docker build step
   Separate: Two Docker build steps
   
   Winner: Single
   ```

3. **Always in Sync** ✅
   ```
   Single: flint/pnfs:v1.0 has both MDS and DS
   Separate: flint/pnfs-mds:v1.0 + flint/pnfs-ds:v1.0 must match
   
   Winner: Single (no version skew possible)
   ```

4. **Shared Dependencies (99%)** ✅
   ```
   MDS and DS share nearly all code:
   - src/pnfs/ (all shared)
   - src/nfs/ (all shared)
   - Dependencies (all shared)
   
   Only difference: main.rs (110 lines each)
   
   Winner: Single (maximum build cache reuse)
   ```

5. **Image Size Trade-off** ⚠️
   ```
   Waste per deployment: 20 MB (unused binary)
   With 200 DS + 1 MDS: 4 GB total waste
   
   But: Saves build time on EVERY build
   Build time > Storage cost
   
   Winner: Single (build time is more valuable)
   ```

### Only Use Separate If:

- ❌ MDS and DS have different dependencies (they don't)
- ❌ Security requires minimal images (not really needed here)
- ❌ MDS and DS released separately (they're not)
- ❌ Image size is critical (it's not - 20 MB is nothing)

---

## Updated Recommendation

### 🎯 Use **Dockerfile.pnfs** (Single Image)

**Rationale**:
- ✅ Faster builds (shared compilation)
- ✅ Simpler CI/CD (one image)
- ✅ Always in sync (same tag)
- ✅ More efficient (build cache reuse)
- ✅ Negligible waste (20 MB per pod)

**Deployment**:
```yaml
# Both use same image, different entrypoints
apiVersion: apps/v1
kind: Deployment
metadata:
  name: flint-pnfs-mds
spec:
  template:
    spec:
      containers:
      - name: mds
        image: flint/pnfs:latest
        command: ["/usr/local/bin/flint-pnfs-mds"]
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: flint-pnfs-ds
spec:
  template:
    spec:
      containers:
      - name: ds
        image: flint/pnfs:latest  # Same image!
        command: ["/usr/local/bin/flint-pnfs-ds"]
```

---

## Let Me Update the Implementation

I'll remove the separate Dockerfiles and create a single optimized Dockerfile.pnfs:

**Benefits**:
- ✅ Single `docker build` command
- ✅ Faster builds (shared cache)
- ✅ Single image to push/pull
- ✅ Simpler operations

**Trade-off**:
- ⚠️ 20 MB larger per pod (acceptable)

Would you like me to replace the separate Dockerfiles with a single Dockerfile.pnfs?
