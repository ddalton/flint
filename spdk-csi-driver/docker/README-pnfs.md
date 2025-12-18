# pNFS Docker Images

## Overview

The pNFS implementation uses **separate Docker images** to avoid impacting the existing CSI driver build times.

## Images

### 1. Dockerfile.csi (Existing - Unchanged)

**Builds**:
- `csi-driver` binary
- `flint-nfs-server` binary (standalone NFS)

**Build Time**: ~5-10 minutes (unchanged)

**Usage**: Existing CSI driver and standalone NFS server

✅ **No changes** - pNFS does not affect this build

### 2. Dockerfile.pnfs-mds (New)

**Builds**:
- `flint-pnfs-mds` binary only

**Build Time**: ~5-10 minutes (separate from CSI)

**Image**: `flint/pnfs-mds:latest`

**Usage**: pNFS Metadata Server

### 3. Dockerfile.pnfs-ds (New)

**Builds**:
- `flint-pnfs-ds` binary only

**Build Time**: ~5-10 minutes (separate from CSI)

**Image**: `flint/pnfs-ds:latest`

**Usage**: pNFS Data Server

---

## Build Commands

### Build CSI Driver (Existing - Unchanged)

```bash
# Build existing CSI driver image
docker build -f docker/Dockerfile.csi -t flint/csi-driver:latest .

# Build time: Same as before (no impact)
```

### Build pNFS Images (New - Separate)

```bash
# Build MDS image
docker build -f docker/Dockerfile.pnfs-mds -t flint/pnfs-mds:latest .

# Build DS image
docker build -f docker/Dockerfile.pnfs-ds -t flint/pnfs-ds:latest .

# Both can be built in parallel
docker build -f docker/Dockerfile.pnfs-mds -t flint/pnfs-mds:latest . &
docker build -f docker/Dockerfile.pnfs-ds -t flint/pnfs-ds:latest . &
wait
```

---

## CI/CD Integration

### Separate Build Pipelines

```yaml
# .github/workflows/build-csi.yaml (existing - unchanged)
name: Build CSI Driver
on:
  push:
    paths:
      - 'spdk-csi-driver/src/main.rs'
      - 'spdk-csi-driver/src/nfs_main.rs'
      - 'spdk-csi-driver/src/nfs/**'
      # NOTE: pNFS paths NOT included
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Build CSI Driver
        run: docker build -f docker/Dockerfile.csi -t flint/csi-driver:${{ github.sha }} .

# .github/workflows/build-pnfs.yaml (new - separate)
name: Build pNFS
on:
  push:
    paths:
      - 'spdk-csi-driver/src/pnfs/**'
      - 'spdk-csi-driver/src/nfs_mds_main.rs'
      - 'spdk-csi-driver/src/nfs_ds_main.rs'
      - 'spdk-csi-driver/proto/pnfs_control.proto'
jobs:
  build-mds:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Build MDS
        run: docker build -f docker/Dockerfile.pnfs-mds -t flint/pnfs-mds:${{ github.sha }} .
  
  build-ds:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Build DS
        run: docker build -f docker/Dockerfile.pnfs-ds -t flint/pnfs-ds:${{ github.sha }} .
```

**Result**: 
- CSI driver builds only when CSI code changes
- pNFS builds only when pNFS code changes
- No cross-impact

---

## Build Time Comparison

### Before pNFS (Existing)

```
Dockerfile.csi:
  • Builds: csi-driver + flint-nfs-server
  • Time: ~5-10 minutes
  • Frequency: On every push
```

### With pNFS (Separate Images)

```
Dockerfile.csi (unchanged):
  • Builds: csi-driver + flint-nfs-server
  • Time: ~5-10 minutes (SAME AS BEFORE)
  • Frequency: Only when CSI/NFS code changes

Dockerfile.pnfs-mds (new):
  • Builds: flint-pnfs-mds only
  • Time: ~5-10 minutes
  • Frequency: Only when pNFS code changes

Dockerfile.pnfs-ds (new):
  • Builds: flint-pnfs-ds only
  • Time: ~5-10 minutes
  • Frequency: Only when pNFS code changes
```

✅ **Zero impact** on existing CSI build times!

---

## Alternative: Single Multi-Binary Image (Not Recommended)

You could build all binaries in one image:

```dockerfile
# Dockerfile.all (NOT RECOMMENDED)
RUN cargo build --release --bin csi-driver
RUN cargo build --release --bin flint-nfs-server
RUN cargo build --release --bin flint-pnfs-mds  # ← Adds build time
RUN cargo build --release --bin flint-pnfs-ds   # ← Adds build time

COPY --from=builder /app/target/release/csi-driver /usr/local/bin/
COPY --from=builder /app/target/release/flint-nfs-server /usr/local/bin/
COPY --from=builder /app/target/release/flint-pnfs-mds /usr/local/bin/
COPY --from=builder /app/target/release/flint-pnfs-ds /usr/local/bin/
```

**Cons**:
- ❌ Slower builds (must build all 4 binaries)
- ❌ Larger image size
- ❌ Must rebuild everything on any change
- ❌ Can't build images in parallel

**Recommendation**: ❌ Don't do this!

---

## Image Sizes

### Estimated Sizes

```
flint/csi-driver:latest     ~100 MB (existing)
flint/pnfs-mds:latest       ~60 MB (MDS only)
flint/pnfs-ds:latest        ~60 MB (DS only)
```

**Total**: ~220 MB (vs ~160 MB if combined, but separate is better)

---

## Deployment

### Use Separate Images

**MDS Deployment**:
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: flint-pnfs-mds
spec:
  template:
    spec:
      containers:
      - name: mds
        image: flint/pnfs-mds:latest  # ← MDS-specific image
        ports:
        - containerPort: 2049
        - containerPort: 50051
```

**DS DaemonSet**:
```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: flint-pnfs-ds
spec:
  template:
    spec:
      containers:
      - name: ds
        image: flint/pnfs-ds:latest  # ← DS-specific image
        ports:
        - containerPort: 2049
```

**CSI Driver** (unchanged):
```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: flint-csi-driver
spec:
  template:
    spec:
      containers:
      - name: csi-driver
        image: flint/csi-driver:latest  # ← Existing image, unchanged
```

---

## Build Script

### Makefile for Convenience

```makefile
# Makefile
.PHONY: build-csi build-pnfs-mds build-pnfs-ds build-all

# Build existing CSI driver (unchanged)
build-csi:
	docker build -f docker/Dockerfile.csi -t flint/csi-driver:latest .

# Build pNFS MDS
build-pnfs-mds:
	docker build -f docker/Dockerfile.pnfs-mds -t flint/pnfs-mds:latest .

# Build pNFS DS
build-pnfs-ds:
	docker build -f docker/Dockerfile.pnfs-ds -t flint/pnfs-ds:latest .

# Build all pNFS images (in parallel)
build-pnfs: build-pnfs-mds build-pnfs-ds

# Build everything
build-all: build-csi build-pnfs

# Push images
push-csi:
	docker push flint/csi-driver:latest

push-pnfs:
	docker push flint/pnfs-mds:latest
	docker push flint/pnfs-ds:latest

push-all: push-csi push-pnfs
```

**Usage**:
```bash
# Build only CSI (fast, unchanged)
make build-csi

# Build only pNFS (separate)
make build-pnfs

# Build both in parallel
make build-all
```

---

## Advantages of Separate Dockerfiles

### ✅ Fast CSI Builds

**Existing Dockerfile.csi**:
- Builds only 2 binaries (csi-driver, flint-nfs-server)
- No pNFS code compiled
- Build time unchanged
- CI/CD pipelines unchanged

### ✅ Independent pNFS Builds

**New Dockerfiles**:
- Build only when pNFS code changes
- Can build MDS and DS in parallel
- Smaller individual images
- Faster iteration on pNFS development

### ✅ Deployment Flexibility

**Different deployment scenarios**:
- CSI only: Use existing image
- CSI + standalone NFS: Use existing image
- pNFS: Use MDS + DS images
- Mixed: Can deploy CSI and pNFS side-by-side

### ✅ Image Optimization

**Each image contains only what it needs**:
- CSI image: CSI driver + standalone NFS server
- MDS image: MDS binary + minimal deps
- DS image: DS binary + NFS utils

**No bloat**: Each image is minimal

---

## Summary

### Your Concern: Build Time Impact

✅ **ZERO IMPACT** on Dockerfile.csi build times

**How**:
- Separate Dockerfiles for pNFS
- Dockerfile.csi: **UNCHANGED**
- pNFS builds separately
- Can build in parallel

### Files Created

```
docker/
├── Dockerfile.csi         (existing - unchanged)
├── Dockerfile.spdk        (existing - unchanged)
├── Dockerfile.pnfs-mds    (new - MDS only)
├── Dockerfile.pnfs-ds     (new - DS only)
└── README-pnfs.md         (new - documentation)
```

### Build Commands

```bash
# CSI driver (unchanged, fast)
docker build -f docker/Dockerfile.csi -t flint/csi-driver:latest .

# pNFS images (separate, no impact on CSI)
docker build -f docker/Dockerfile.pnfs-mds -t flint/pnfs-mds:latest .
docker build -f docker/Dockerfile.pnfs-ds -t flint/pnfs-ds:latest .
```

---

**Result**: ✅ **Dockerfile.csi build time unchanged**, pNFS images build separately! 🚀
