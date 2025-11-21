# Multi-Replica Volume Test

This test verifies the distributed RAID 1 multi-replica functionality:

## Test Scenario

1. Create a StorageClass with `numReplicas: "2"`
2. Create a PVC requesting 5Gi storage
3. Verify PVC is bound and PV has replica metadata
4. Deploy a Pod using the PVC
5. Verify RAID 1 is created on the Pod's node
6. Write test data to the volume
7. Verify data persistence
8. Clean up

## Expected Behavior

- 2 replicas created on different nodes
- PV volumeAttributes contains replica JSON
- RAID 1 bdev created with mixed local/remote access
- Data writes successfully
- Clean deletion of all replicas

## Success Criteria

- PVC binds successfully
- Pod runs and writes data
- Volume deletion cleans up all replicas

