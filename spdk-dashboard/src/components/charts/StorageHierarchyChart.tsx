// StorageHierarchyChart.tsx - Correct SPDK Storage Topology Visualization
//
// Shows the correct hierarchy: Physical Disks → RAID → LVS → Logical Volume → ublk device

import React, { useState, useMemo } from 'react';
import { Volume, PhysicalDisk, SpdkRaid, LogicalVolumeStore, DashboardData } from '../../hooks/useDashboardData';
import { HardDisk, Shield, Database, Layers, MonitorCheck, AlertTriangle, CheckCircle2, Wifi, Globe } from 'lucide-react';

interface StorageHierarchyChartProps {
  data: DashboardData;
}

export const StorageHierarchyChart: React.FC<StorageHierarchyChartProps> = ({ data }) => {
  const [selectedVolume, setSelectedVolume] = useState<string | null>(
    data.volumes.length > 0 ? data.volumes[0].id : null
  );

  // Get hierarchy for selected volume
  const hierarchyData = useMemo(() => {
    if (!selectedVolume) return null;
    
    const volume = data.volumes.find(v => v.id === selectedVolume);
    if (!volume) return null;
    
    // Analyze replicas to understand local vs remote topology
    const localReplicas = volume.replica_statuses.filter(r => r.is_local);
    const remoteReplicas = volume.replica_statuses.filter(r => !r.is_local && r.nvmf_target);
    
    // For local access, find the LVS and RAID
    let localTopology = null;
    if (localReplicas.length > 0) {
      const lvs = data.logical_volume_stores.find(l => l.name === volume.lvs_name);
      if (lvs) {
        const raid = data.spdk_raids.find(r => r.name === lvs.base_raid);
        if (raid) {
          const physicalDisks = data.physical_disks.filter(d => 
            raid.member_disks.some(md => md.id === d.id) || 
            d.node === raid.node
          );
          localTopology = { lvs, raid, physicalDisks };
        }
      }
    }
    
    // For remote access, we show the NVMe-oF connections
    const remoteTopologies = remoteReplicas.map(replica => {
      // Try to find the remote LVS/RAID based on the replica node
      const remoteLvs = data.logical_volume_stores.find(l => l.node === replica.node);
      const remoteRaid = remoteLvs ? data.spdk_raids.find(r => r.name === remoteLvs.base_raid) : null;
      const remotePhysicalDisks = remoteRaid ? 
        data.physical_disks.filter(d => d.node === replica.node) : [];
      
      return {
        replica,
        remoteLvs,
        remoteRaid,
        remotePhysicalDisks
      };
    });
    
    return {
      volume,
      localTopology,
      remoteTopologies,
      isRemoteAccess: remoteReplicas.length > 0,
      isLocalAccess: localReplicas.length > 0
    };
  }, [selectedVolume, data]);

  if (!hierarchyData) {
    return (
      <div className="bg-white rounded-lg shadow p-6">
        <h3 className="text-lg font-semibold mb-4">Storage Hierarchy</h3>
        <div className="text-center py-8 text-gray-500">
          No volume selected or data incomplete
        </div>
      </div>
    );
  }

  const { volume, localTopology, remoteTopologies, isRemoteAccess, isLocalAccess } = hierarchyData;

  const getStateColor = (state: string, healthy?: boolean) => {
    if (healthy === false) return 'bg-red-100 text-red-800';
    if (state === 'online' || state === 'healthy' || state === 'Healthy') return 'bg-green-100 text-green-800';
    if (state === 'degraded') return 'bg-yellow-100 text-yellow-800';
    if (state === 'failed' || state === 'Failed') return 'bg-red-100 text-red-800';
    return 'bg-gray-100 text-gray-800';
  };

  return (
    <div className="bg-white rounded-lg shadow p-6">
      <div className="flex items-center justify-between mb-6">
        <h3 className="text-lg font-semibold">Storage Hierarchy</h3>
        
        {/* Volume Selector */}
        <div className="flex items-center gap-2">
          <label className="text-sm font-medium text-gray-700">Volume:</label>
          <select
            value={selectedVolume || ''}
            onChange={(e) => setSelectedVolume(e.target.value)}
            className="px-3 py-1 border border-gray-300 rounded-md text-sm"
          >
            {data.volumes.map(v => (
              <option key={v.id} value={v.id}>
                {v.name} ({v.size})
              </option>
            ))}
          </select>
        </div>
      </div>

      {/* Access Pattern Summary */}
      <div className="mb-6 p-4 bg-gradient-to-r from-blue-50 to-purple-50 rounded-lg border">
        <div className="flex items-center gap-2 mb-2">
          <Globe className="w-5 h-5 text-blue-600" />
          <h4 className="font-medium text-gray-900">Access Pattern</h4>
        </div>
        <div className="text-sm text-gray-700">
          {isLocalAccess && isRemoteAccess && (
            <span className="flex items-center gap-1">
              <CheckCircle2 className="w-4 h-4 text-green-600" />
              Hybrid: Local access + {remoteTopologies.length} remote NVMe-oF connection{remoteTopologies.length > 1 ? 's' : ''}
            </span>
          )}
          {isLocalAccess && !isRemoteAccess && (
            <span className="flex items-center gap-1">
              <CheckCircle2 className="w-4 h-4 text-green-600" />
              Local: Direct access to local logical volume
            </span>
          )}
          {!isLocalAccess && isRemoteAccess && (
            <span className="flex items-center gap-1">
              <Wifi className="w-4 h-4 text-purple-600" />
              Remote: All access via NVMe-oF ({remoteTopologies.length} connection{remoteTopologies.length > 1 ? 's' : ''})
            </span>
          )}
        </div>
      </div>

      {/* Local Storage Hierarchy */}
      {isLocalAccess && localTopology && (
        <div className="space-y-6">
          <div className="border-l-4 border-green-500 pl-4">
            <h4 className="font-medium text-gray-900 mb-4">Local Storage Hierarchy</h4>
            
            {/* 1. Physical Disks (Foundation) */}
            <div className="border border-gray-200 rounded-lg p-4 mb-4">
              <div className="flex items-center gap-2 mb-3">
                <HardDisk className="w-5 h-5 text-gray-600" />
                <h5 className="font-medium text-gray-900">Physical Disks</h5>
                <span className="text-sm text-gray-500">({localTopology.physicalDisks.length} disks on {localTopology.raid.node})</span>
              </div>
              
              <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
                {localTopology.physicalDisks.map(disk => (
                  <div key={disk.id} className="bg-gray-50 rounded p-3 border">
                    <div className="flex items-center justify-between mb-2">
                      <span className="font-medium text-sm">{disk.id}</span>
                      <span className={`px-2 py-1 text-xs rounded-full ${getStateColor('healthy', disk.healthy)}`}>
                        {disk.healthy ? 'Healthy' : 'Failed'}
                      </span>
                    </div>
                    <div className="text-xs text-gray-600 space-y-1">
                      <p>Capacity: {disk.capacity_gb}GB</p>
                      <p>Model: {disk.model}</p>
                      <p>PCI: {disk.pci_addr}</p>
                    </div>
                  </div>
                ))}
              </div>
            </div>

            {/* RAID Layer */}
            <div className="border border-blue-200 rounded-lg p-4 bg-blue-50 mb-4">
              <div className="flex items-center gap-2 mb-3">
                <Shield className="w-5 h-5 text-blue-600" />
                <h5 className="font-medium text-gray-900">RAID Array</h5>
                <span className={`px-2 py-1 text-xs rounded-full ${getStateColor(localTopology.raid.state)}`}>
                  {localTopology.raid.state.toUpperCase()}
                </span>
              </div>
              
              <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                <div>
                  <p className="text-xs text-gray-600">RAID Level</p>
                  <p className="font-medium">RAID-{localTopology.raid.raid_level}</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Capacity</p>
                  <p className="font-medium">{localTopology.raid.capacity_gb}GB</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Members</p>
                  <p className="font-medium">{localTopology.raid.operational_members}/{localTopology.raid.num_members}</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Node</p>
                  <p className="font-medium">{localTopology.raid.node}</p>
                </div>
              </div>
            </div>

            {/* LVS Layer */}
            <div className="border border-purple-200 rounded-lg p-4 bg-purple-50 mb-4">
              <div className="flex items-center gap-2 mb-3">
                <Database className="w-5 h-5 text-purple-600" />
                <h5 className="font-medium text-gray-900">Logical Volume Store</h5>
                <span className="text-sm text-gray-500">({localTopology.lvs.name})</span>
              </div>
              
              <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                <div>
                  <p className="text-xs text-gray-600">Capacity</p>
                  <p className="font-medium">{localTopology.lvs.capacity_gb}GB</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Used</p>
                  <p className="font-medium">{localTopology.lvs.used_gb}GB</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Utilization</p>
                  <p className="font-medium">{localTopology.lvs.utilization_pct}%</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Cluster Size</p>
                  <p className="font-medium">{localTopology.lvs.cluster_size}</p>
                </div>
              </div>
            </div>

            {/* Logical Volume */}
            <div className="border border-green-200 rounded-lg p-4 bg-green-50">
              <div className="flex items-center gap-2 mb-3">
                <Layers className="w-5 h-5 text-green-600" />
                <h5 className="font-medium text-gray-900">Logical Volume</h5>
                <span className={`px-2 py-1 text-xs rounded-full ${getStateColor(volume.state)}`}>
                  {volume.state}
                </span>
              </div>
              
              <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                <div>
                  <p className="text-xs text-gray-600">Name</p>
                  <p className="font-medium">{volume.name}</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Size</p>
                  <p className="font-medium">{volume.size}</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">UUID</p>
                  <p className="font-medium text-xs">{volume.lvol_uuid.slice(0, 8)}...</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Access</p>
                  <p className="font-medium text-green-700">Local</p>
                </div>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Remote Storage Hierarchies */}
      {isRemoteAccess && remoteTopologies.map((remoteTopology, index) => (
        <div key={index} className="space-y-6 mt-8">
          <div className="border-l-4 border-purple-500 pl-4">
            <h4 className="font-medium text-gray-900 mb-4">
              Remote Storage Hierarchy #{index + 1} 
              <span className="text-sm text-gray-500 ml-2">({remoteTopology.replica.node})</span>
            </h4>
            
            {/* Remote Physical Disks */}
            {remoteTopology.remotePhysicalDisks.length > 0 && (
              <div className="border border-gray-200 rounded-lg p-4 mb-4">
                <div className="flex items-center gap-2 mb-3">
                  <HardDisk className="w-5 h-5 text-gray-600" />
                  <h5 className="font-medium text-gray-900">Remote Physical Disks</h5>
                  <span className="text-sm text-gray-500">({remoteTopology.remotePhysicalDisks.length} disks on {remoteTopology.replica.node})</span>
                </div>
                
                <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
                  {remoteTopology.remotePhysicalDisks.map(disk => (
                    <div key={disk.id} className="bg-gray-50 rounded p-3 border">
                      <div className="flex items-center justify-between mb-2">
                        <span className="font-medium text-sm">{disk.id}</span>
                        <span className={`px-2 py-1 text-xs rounded-full ${getStateColor('healthy', disk.healthy)}`}>
                          {disk.healthy ? 'Healthy' : 'Failed'}
                        </span>
                      </div>
                      <div className="text-xs text-gray-600 space-y-1">
                        <p>Capacity: {disk.capacity_gb}GB</p>
                        <p>Model: {disk.model}</p>
                        <p>PCI: {disk.pci_addr}</p>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}

            {/* Remote RAID */}
            {remoteTopology.remoteRaid && (
              <div className="border border-blue-200 rounded-lg p-4 bg-blue-50 mb-4">
                <div className="flex items-center gap-2 mb-3">
                  <Shield className="w-5 h-5 text-blue-600" />
                  <h5 className="font-medium text-gray-900">Remote RAID Array</h5>
                  <span className={`px-2 py-1 text-xs rounded-full ${getStateColor(remoteTopology.remoteRaid.state)}`}>
                    {remoteTopology.remoteRaid.state.toUpperCase()}
                  </span>
                </div>
                
                <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                  <div>
                    <p className="text-xs text-gray-600">RAID Level</p>
                    <p className="font-medium">RAID-{remoteTopology.remoteRaid.raid_level}</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Capacity</p>
                    <p className="font-medium">{remoteTopology.remoteRaid.capacity_gb}GB</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Members</p>
                    <p className="font-medium">{remoteTopology.remoteRaid.operational_members}/{remoteTopology.remoteRaid.num_members}</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Node</p>
                    <p className="font-medium">{remoteTopology.remoteRaid.node}</p>
                  </div>
                </div>
              </div>
            )}

            {/* Remote LVS */}
            {remoteTopology.remoteLvs && (
              <div className="border border-purple-200 rounded-lg p-4 bg-purple-50 mb-4">
                <div className="flex items-center gap-2 mb-3">
                  <Database className="w-5 h-5 text-purple-600" />
                  <h5 className="font-medium text-gray-900">Remote Logical Volume Store</h5>
                  <span className="text-sm text-gray-500">({remoteTopology.remoteLvs.name})</span>
                </div>
                
                <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                  <div>
                    <p className="text-xs text-gray-600">Capacity</p>
                    <p className="font-medium">{remoteTopology.remoteLvs.capacity_gb}GB</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Used</p>
                    <p className="font-medium">{remoteTopology.remoteLvs.used_gb}GB</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Utilization</p>
                    <p className="font-medium">{remoteTopology.remoteLvs.utilization_pct}%</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Node</p>
                    <p className="font-medium">{remoteTopology.remoteLvs.node}</p>
                  </div>
                </div>
              </div>
            )}

            {/* Remote Logical Volume */}
            <div className="border border-green-200 rounded-lg p-4 bg-green-50 mb-4">
              <div className="flex items-center gap-2 mb-3">
                <Layers className="w-5 h-5 text-green-600" />
                <h5 className="font-medium text-gray-900">Remote Logical Volume</h5>
                <span className={`px-2 py-1 text-xs rounded-full ${getStateColor(remoteTopology.replica.status)}`}>
                  {remoteTopology.replica.status}
                </span>
              </div>
              
              <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                <div>
                  <p className="text-xs text-gray-600">Node</p>
                  <p className="font-medium">{remoteTopology.replica.node}</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">UUID</p>
                  <p className="font-medium text-xs">{remoteTopology.replica.lvol_uuid?.slice(0, 8) || 'N/A'}...</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Disk Ref</p>
                  <p className="font-medium">{remoteTopology.replica.disk_ref || 'N/A'}</p>
                </div>
                <div>
                  <p className="text-xs text-gray-600">Size</p>
                  <p className="font-medium">{remoteTopology.replica.replica_size ? Math.round(remoteTopology.replica.replica_size / 1024 / 1024 / 1024) + 'GB' : 'N/A'}</p>
                </div>
              </div>
            </div>

            {/* NVMe-oF Connection */}
            {remoteTopology.replica.nvmf_target && (
              <div className="border border-orange-200 rounded-lg p-4 bg-orange-50 mb-4">
                <div className="flex items-center gap-2 mb-3">
                  <Wifi className="w-5 h-5 text-orange-600" />
                  <h5 className="font-medium text-gray-900">NVMe-oF Export</h5>
                  <span className="text-sm text-gray-500">(Network Connectivity)</span>
                </div>
                
                <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                  <div>
                    <p className="text-xs text-gray-600">Target IP</p>
                    <p className="font-medium font-mono">{remoteTopology.replica.nvmf_target.target_ip}</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Port</p>
                    <p className="font-medium">{remoteTopology.replica.nvmf_target.target_port}</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">Transport</p>
                    <p className="font-medium">{remoteTopology.replica.nvmf_target.transport_type}</p>
                  </div>
                  <div>
                    <p className="text-xs text-gray-600">NQN</p>
                    <p className="font-medium text-xs">{remoteTopology.replica.nvmf_target.nqn.slice(-8)}...</p>
                  </div>
                </div>
              </div>
            )}
          </div>
        </div>
      ))}

      {/* Final ublk Device Layer */}
      <div className="mt-8 border border-indigo-200 rounded-lg p-4 bg-indigo-50">
        <div className="flex items-center gap-2 mb-3">
          <MonitorCheck className="w-5 h-5 text-indigo-600" />
          <h4 className="font-medium text-gray-900">ublk Device</h4>
          <span className="text-sm text-gray-500">(User Interface Layer)</span>
        </div>
        
        {volume.ublk_device ? (
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
            <div>
              <p className="text-xs text-gray-600">Device Path</p>
              <p className="font-medium font-mono">{volume.ublk_device.device_path}</p>
            </div>
            <div>
              <p className="text-xs text-gray-600">ublk ID</p>
              <p className="font-medium">{volume.ublk_device.id}</p>
            </div>
            <div>
              <p className="text-xs text-gray-600">Access Method</p>
              <p className="font-medium">{volume.access_method}</p>
            </div>
            <div>
              <p className="text-xs text-gray-600">Connection Type</p>
              <p className="font-medium">
                {isLocalAccess && !isRemoteAccess && "Local Direct"}
                {!isLocalAccess && isRemoteAccess && "Remote NVMe-oF"}
                {isLocalAccess && isRemoteAccess && "Hybrid"}
              </p>
            </div>
          </div>
        ) : (
          <div className="flex items-center gap-2 text-yellow-700">
            <AlertTriangle className="w-4 h-4" />
            <span className="text-sm">No ublk device configured</span>
          </div>
        )}
      </div>

      {/* Architecture Summary */}
      <div className="mt-6 p-4 bg-gray-50 rounded-lg">
        <h5 className="font-medium text-gray-900 mb-2">SPDK Storage Architecture Patterns:</h5>
        
        <div className="space-y-2 text-sm text-gray-600">
          <div className="flex items-start gap-2">
            <span className="text-green-600 font-medium">Local:</span>
            <span>Physical Disks → RAID → LVS → Logical Volume → ublk device</span>
          </div>
          
          <div className="flex items-start gap-2">
            <span className="text-purple-600 font-medium">Remote:</span>
            <span>Physical Disks → RAID → LVS → Logical Volume → NVMe-oF Export → Network → NVMe-oF Client → ublk device</span>
          </div>
          
          <div className="flex items-start gap-2">
            <span className="text-blue-600 font-medium">Hybrid:</span>
            <span>Combination of local and remote access patterns for high availability</span>
          </div>
        </div>
        
        <p className="text-xs text-gray-500 mt-3">
          When RAID disks are on remote hosts, ublk devices connect to remote logical volumes using NVMe-oF networking.
          This enables distributed storage with the performance benefits of local NVMe and the availability of remote replication.
        </p>
      </div>
    </div>
  );
};
