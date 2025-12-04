# Delete Idempotency Test

Validates that CSI DeleteVolume is idempotent as required by the spec.

## CSI Spec Requirement

> "If the volume corresponding to the volume_id does not exist or the artifacts associated with the volume do not exist anymore, the Plugin MUST reply 0 OK."

## Test Scenario

1. Create PVC → Volume created
2. Delete PVC → DeleteVolume called (first time)
3. Verify volume deleted
4. **Delete same volume again** → DeleteVolume called (second time)
5. Should succeed without error (idempotent)

## Success Criteria

✅ First deletion completes  
✅ Second deletion succeeds (no error)  
✅ CSI driver handles duplicate delete gracefully

