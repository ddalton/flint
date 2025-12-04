# CSI Sanity Tests for Flint

This directory contains setup for running the official [CSI Sanity Test Suite](https://github.com/kubernetes-csi/csi-test) against the Flint CSI driver.

## What is CSI Sanity?

CSI Sanity is the official Kubernetes CSI compliance test suite that validates:
- ✅ CSI spec compliance
- ✅ Idempotency of operations
- ✅ Error handling
- ✅ Edge cases and race conditions
- ✅ Required vs optional capabilities

## Prerequisites

1. **Install csi-sanity:**
```bash
go install github.com/kubernetes-csi/csi-test/cmd/csi-sanity@latest
```

2. **SPDK running on local node:**
- SPDK target daemon at `/var/tmp/spdk.sock`
- At least one initialized LVS (logical volume store)

## Running the Tests

### Option 1: Test Against Running Driver in Kubernetes

```bash
# Forward the CSI socket from a node pod
kubectl port-forward -n flint-system <node-pod-name> 10000:/csi/csi.sock

# In another terminal, run sanity tests
csi-sanity --csi.endpoint=unix:///tmp/csi.sock \
  --csi.stagingdir=/tmp/staging \
  --csi.mountdir=/tmp/mount \
  --csi.testvolumeparameters=/path/to/params.yaml
```

### Option 2: Test Against Local Driver Instance

```bash
# Start the driver locally in node mode
cd spdk-csi-driver
CSI_MODE=node \
CSI_ENDPOINT=unix:///tmp/csi.sock \
NODE_ID=test-node \
SPDK_RPC_URL=unix:///var/tmp/spdk.sock \
cargo run --bin csi-driver &

# Wait for it to start, then run sanity
csi-sanity --csi.endpoint=unix:///tmp/csi.sock \
  --csi.stagingdir=/tmp/staging \
  --csi.mountdir=/tmp/mount \
  --csi.testvolumesize=1073741824
```

## Test Parameters

Create `sanity-params.yaml`:

```yaml
# Volume parameters for test volumes
numReplicas: "1"
thinProvision: "true"
```

## Expected Results

The Flint driver should pass:
- ✅ **Identity Service** tests (GetPluginInfo, GetPluginCapabilities, Probe)
- ✅ **Controller Service** tests (CreateVolume, DeleteVolume, ControllerPublish/Unpublish)
- ✅ **Node Service** tests (NodeStage, NodePublish, NodeUnpublish, NodeUnstage)
- ✅ **Volume Expansion** tests (ControllerExpand, NodeExpand)
- ⚠️ **Snapshot tests** (CreateSnapshot, DeleteSnapshot) - if snapshot capability advertised

## Known Limitations

Some tests may be skipped or fail due to Flint-specific constraints:

1. **Topology tests**: Requires specific cluster configuration
2. **Access mode tests**: RWX not yet fully supported
3. **Volume cloning**: Requires source volume to exist

## Interpreting Results

Sanity tests output:
```
• Passed: Feature works correctly ✅
• Skipped: Feature not advertised in capabilities ⏭️
• Failed: CSI spec violation that needs fixing ❌
```

## Troubleshooting

### "dial unix /tmp/csi.sock: connect: no such file or directory"
- Driver not started or socket path incorrect
- Check CSI_ENDPOINT matches --csi.endpoint

### "rpc error: code = Unimplemented"
- Feature not implemented
- Check if capability is advertised in GetPluginCapabilities

### "context deadline exceeded"
- SPDK not responding
- Check SPDK is running: `ls -l /var/tmp/spdk.sock`

## References

- [CSI Sanity Test Suite](https://github.com/kubernetes-csi/csi-test)
- [CSI Spec](https://github.com/container-storage-interface/spec)
- [Flint Architecture](../../FLINT_CSI_ARCHITECTURE.md)

