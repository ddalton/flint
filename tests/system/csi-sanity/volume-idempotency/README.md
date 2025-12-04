# Volume Idempotency Test

This test validates that CSI CreateVolume and DeleteVolume operations are **idempotent** as required by the CSI specification.

## CSI Spec Requirement

From the CSI spec:
> "This RPC will be called multiple times by the CO for the same volume. The Plugin MUST handle this by returning the same response for multiple calls with the same name and parameters."

## Test Scenario

1. Create PVC → CreateVolume called
2. Verify volume bound
3. Use volume in Pod
4. Delete PVC → DeleteVolume called  
5. **Immediately recreate PVC with same name** → CreateVolume called again
6. Verify it succeeds (idempotency)

## Success Criteria

✅ First volume creation succeeds  
✅ Volume works (Pod can use it)  
✅ Second creation with same name succeeds  
✅ No errors from idempotent operations

