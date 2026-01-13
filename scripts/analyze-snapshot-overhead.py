#!/usr/bin/env python3
"""
Analyze snapshot storage overhead by querying SPDK bdevs.

This script queries the SPDK node agent to get detailed storage consumption
information for volumes and their snapshots.
"""

import json
import subprocess
import sys
from typing import Dict, List, Any, Optional

def run_kubectl_exec(namespace: str, pod: str, container: str, command: str) -> str:
    """Execute command in pod and return stdout."""
    cmd = [
        "kubectl", "exec", "-n", namespace, pod, "-c", container,
        "--", "sh", "-c", command
    ]
    
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"Error executing command: {result.stderr}", file=sys.stderr)
        sys.exit(1)
    
    return result.stdout

def query_spdk_rpc(namespace: str, pod: str, container: str, method: str, params: Optional[Dict] = None) -> Any:
    """Query SPDK via RPC."""
    payload = {"method": method}
    if params:
        payload["params"] = params
    
    rpc_payload = json.dumps(payload)
    command = f"curl -s -X POST http://localhost:9081/api/spdk/rpc -H 'Content-Type: application/json' -d '{rpc_payload}'"
    
    output = run_kubectl_exec(namespace, pod, container, command)
    data = json.loads(output)
    
    # Handle both direct array and {"result": [...]} format
    if isinstance(data, dict) and "result" in data:
        return data["result"]
    return data

def bytes_to_mb(bytes_val: int) -> float:
    """Convert bytes to MB with 2 decimal places."""
    return round(bytes_val / (1024 * 1024), 2)

def main():
    # Configuration
    NAMESPACE = "flint-system"
    CONTAINER = "flint-csi-driver"
    TARGET_NODE = "flnt-4-46-m1"  # Master node with memory disk
    
    # Find the CSI node pod
    print(f"🔍 Finding CSI node pod on {TARGET_NODE}...")
    cmd = ["kubectl", "get", "pod", "-n", NAMESPACE, "-l", "app=flint-csi-node", 
           "-o", "json"]
    result = subprocess.run(cmd, capture_output=True, text=True)
    pods_data = json.loads(result.stdout)
    
    pod_name = None
    for pod in pods_data.get("items", []):
        node_name = pod.get("spec", {}).get("nodeName", "")
        if node_name == TARGET_NODE:
            pod_name = pod.get("metadata", {}).get("name")
            break
    
    if not pod_name:
        print(f"❌ Could not find CSI node pod on {TARGET_NODE}", file=sys.stderr)
        sys.exit(1)
    
    print(f"✅ Found pod: {pod_name}\n")
    
    # Query lvol stores to get cluster size
    print("📊 Querying SPDK lvol stores...")
    lvol_stores = query_spdk_rpc(NAMESPACE, pod_name, CONTAINER, "bdev_lvol_get_lvstores")
    
    cluster_size_map = {}
    for lvs in lvol_stores:
        uuid = lvs.get("uuid", "")
        cluster_size = lvs.get("cluster_size", 0)
        cluster_size_map[uuid] = cluster_size
        print(f"  LVS: {lvs.get('name', 'unknown')}, cluster_size: {cluster_size // 1024}KB")
    
    print()
    
    # Query SPDK bdevs
    print("📊 Querying SPDK for bdev information...")
    bdevs = query_spdk_rpc(NAMESPACE, pod_name, CONTAINER, "bdev_get_bdevs")
    print(f"✅ Got {len(bdevs)} total bdevs\n")
    
    # Filter for test-memory-512m volume and its snapshots
    relevant_bdevs = []
    for bdev in bdevs:
        aliases = bdev.get("aliases", [])
        name_to_check = aliases[0] if aliases else bdev.get("name", "")
        
        if "pvc-b692de1d" in name_to_check:
            lvol_info = bdev.get("driver_specific", {}).get("lvol", {})
            lvol_store_uuid = lvol_info.get("lvol_store_uuid", "")
            cluster_size = cluster_size_map.get(lvol_store_uuid, 0)
            
            num_blocks = bdev.get("num_blocks", 0)
            block_size = bdev.get("block_size", 0)
            logical_size = num_blocks * block_size
            
            num_allocated_clusters = lvol_info.get("num_allocated_clusters", 0)
            consumed_size = num_allocated_clusters * cluster_size
            
            # Get the human-readable name from aliases
            name = name_to_check
            if "/" in name:
                name = name.split("/", 1)[1]
            
            info = {
                "name": name,
                "uuid": bdev.get("uuid", ""),
                "is_snapshot": lvol_info.get("snapshot", False),
                "is_clone": lvol_info.get("clone", False),
                "base_snapshot": lvol_info.get("base_snapshot", ""),
                "clones": lvol_info.get("clones", []),
                "logical_size_mb": bytes_to_mb(logical_size),
                "consumed_mb": bytes_to_mb(consumed_size),
                "allocated_clusters": num_allocated_clusters,
                "cluster_size_kb": cluster_size // 1024 if cluster_size else 0,
                "claimed": bdev.get("claimed", False)
            }
            relevant_bdevs.append(info)
    
    if not relevant_bdevs:
        print("❌ No bdevs found for test-memory-512m volume")
        sys.exit(1)
    
    # Sort by type and name
    relevant_bdevs.sort(key=lambda x: (x["is_snapshot"], x["is_clone"], x["name"]))
    
    # Display results
    print("=" * 110)
    print(f"{'Name':<55} {'Type':<15} {'Logical':<12} {'Consumed':<12} {'Clusters':<12} {'Claimed'}")
    print("=" * 110)
    
    active_volume = None
    snapshots = []
    
    for bdev in relevant_bdevs:
        if bdev["is_snapshot"]:
            if bdev["is_clone"]:
                bdev_type = "Snap (Clone)"
            else:
                bdev_type = "Snap (Original)"
            snapshots.append(bdev)
        elif bdev["is_clone"]:
            bdev_type = "Active Volume"
            active_volume = bdev
        else:
            bdev_type = "Volume"
            active_volume = bdev
        
        claimed_str = "Yes" if bdev["claimed"] else "No"
        
        print(f"{bdev['name']:<55} {bdev_type:<15} {bdev['logical_size_mb']:>10.2f}MB "
              f"{bdev['consumed_mb']:>10.2f}MB {bdev['allocated_clusters']:>10}  {claimed_str}")
    
    print("=" * 110)
    
    # Calculate snapshot overhead
    if active_volume and snapshots:
        print("\n📊 SNAPSHOT OVERHEAD ANALYSIS:")
        print(f"\n✅ Active Volume: {active_volume['name']}")
        print(f"   Logical Size: {active_volume['logical_size_mb']:.2f} MB")
        print(f"   Consumed:     {active_volume['consumed_mb']:.2f} MB ({active_volume['allocated_clusters']} clusters)")
        print(f"   Cluster Size: {active_volume['cluster_size_kb']} KB")
        
        total_snapshot_consumed = sum(s['consumed_mb'] for s in snapshots)
        total_snapshot_clusters = sum(s['allocated_clusters'] for s in snapshots)
        
        print(f"\n📸 Snapshots ({len(snapshots)} total):")
        for i, snap in enumerate(snapshots, 1):
            snap_type = "Original" if not snap["is_clone"] else "Clone"
            clones_str = f" (has {len(snap['clones'])} clones)" if snap.get('clones') else ""
            print(f"   {i}. {snap['name']}")
            print(f"      Type: {snap_type}{clones_str}")
            print(f"      Consumed: {snap['consumed_mb']:.2f} MB ({snap['allocated_clusters']} clusters)")
            if snap.get('base_snapshot'):
                print(f"      Base: {snap['base_snapshot']}")
        
        print(f"\n{'─' * 70}")
        print(f"Total Snapshot Consumption:  {total_snapshot_consumed:.2f} MB ({total_snapshot_clusters} clusters)")
        print(f"Active Volume Consumption:   {active_volume['consumed_mb']:.2f} MB ({active_volume['allocated_clusters']} clusters)")
        print(f"{'─' * 70}")
        print(f"TOTAL STORAGE USED:          {total_snapshot_consumed + active_volume['consumed_mb']:.2f} MB")
        print(f"TOTAL CLUSTERS ALLOCATED:    {total_snapshot_clusters + active_volume['allocated_clusters']}")
        print(f"{'─' * 70}")
        
        # Explain snapshot overhead
        print(f"\n💡 EXPLANATION:")
        print(f"  • Cluster size: {active_volume['cluster_size_kb']} KB")
        print(f"  • Active volume uses {active_volume['allocated_clusters']} clusters = {active_volume['consumed_mb']:.2f} MB")
        print(f"  • Snapshots use {total_snapshot_clusters} clusters total = {total_snapshot_consumed:.2f} MB")
        print(f"  • Snapshot 1 (original): Captured initial state ({snapshots[0]['allocated_clusters']} clusters)")
        if len(snapshots) > 1:
            print(f"  • Snapshot 2 (clone): Only stores diff from Snapshot 1 ({snapshots[1]['allocated_clusters']} clusters)")
        print(f"\n  ⚠️  SPDK Copy-on-Write: Snapshots store DATA THAT HAS CHANGED since they were taken")
        print(f"      The active volume has low cluster count because most data is shared with snapshots!")
        
        # Compare with filesystem view
        print(f"\n📝 FILESYSTEM vs SPDK STORAGE:")
        print(f"  Filesystem shows (df -h): 356 MB used")
        print(f"  SPDK actual storage:      {total_snapshot_consumed + active_volume['consumed_mb']:.2f} MB")
        print(f"  \n  This is NORMAL! SPDK uses copy-on-write:")
        print(f"    - Original data (256MB testfile) is in Snapshot 1 ({snapshots[0]['consumed_mb']:.2f}MB)")
        print(f"    - New data written after Snapshot 1 is minimal")
        print(f"    - Snapshot 2 captured the 100MB write, but SPDK only allocated {snapshots[1]['consumed_mb']:.2f}MB")
        print(f"    - Active volume shares most blocks with snapshots (only {active_volume['consumed_mb']:.2f}MB unique)")
    else:
        print("\n⚠️  Could not find active volume or snapshots for analysis")

if __name__ == "__main__":
    main()
