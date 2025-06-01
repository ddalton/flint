import React, { useState } from 'react';
import { Database, Activity, X, Settings, Zap, Network, Info, AlertTriangle, Cable, Monitor } from 'lucide-react';
import { NVMFTooltip } from '../ui/NVMFTooltip';
import { VHostNvmeTooltip } from '../ui/VHostNvmeTooltip';
import type { Volume } from '../../hooks/useDashboardData';

interface RaidTopologyChartProps {
  volumes: Volume[];
}

export const RaidTopologyChart: React.FC<RaidTopologyChartProps> = ({ volumes }) => {
  const [selectedVolume, setSelectedVolume] = useState(volumes[0]?.id || '');
  
  const volume = volumes.find(v => v.id === selectedVolume);
  
  if (!volume) return null;

  const getReplicaStatusColor = (status: string) => {
    switch (status) {
      case 'healthy': return 'bg-green-100 text-green-800 border-green-200';
      case 'failed': return 'bg-red-100 text-red-800 border-red-200';
      case 'rebuilding': return 'bg-orange-100 text-orange-800 border-orange-200';
      default: return 'bg-gray-100 text-gray-800 border-gray-200';
    }
  };

  const getReplicaIcon = (replica: any) => {
    if (replica.status === 'failed') return <X className="w-4 h-4 text-red-600" />;
    if (replica.status === 'rebuilding') return <Settings className="w-4 h-4 text-orange-600 animate-spin" />;
    if (replica.is_local) return <Zap className="w-4 h-4 text-blue-600" />;
    return <Network className="w-4 h-4 text-purple-600" />;
  };

  const getConnectionColor = (status: string) => {
    switch (status) {
      case 'healthy': return '#10b981';
      case 'failed': return '#ef4444';
      case 'rebuilding': return '#f59e0b';
      default: return '#6b7280';
    }
  };

  // Check if volume has vhost-nvme configuration
  const hasVHostNvme = volume.vhost_socket || 
                       volume.vhost_enabled || 
                       volume.access_method === 'vhost-nvme' ||
                       volume.vhost_type === 'nvme';

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center">
          <Activity className="w-6 h-6 text-blue-600 mr-2" />
          <h3 className="text-lg font-semibold">RAID Topology Visualization</h3>
        </div>
        <select
          value={selectedVolume}
          onChange={(e) => setSelectedVolume(e.target.value)}
          className="px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 text-sm"
        >
          {volumes.map((vol) => (
            <option key={vol.id} value={vol.id}>
              {vol.name} ({vol.state})
            </option>
          ))}
        </select>
      </div>
      
      <div className="text-center">
        {/* VHost-NVMe Access Layer - Show only if volume has vhost-nvme */}
        {hasVHostNvme && (
          <>
            <div className="mb-6">
              <h4 className="text-lg font-semibold mb-4 text-gray-700">Application Access Layer</h4>
              <div className="flex justify-center items-center gap-4">
                <div className="text-center">
                  <div className="w-16 h-16 bg-purple-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-purple-300">
                    <Monitor className="w-8 h-8 text-purple-600" />
                  </div>
                  <p className="font-medium text-sm">Pod/Application</p>
                  <p className="text-xs text-gray-500">User Process</p>
                </div>
                
                <div className="flex items-center">
                  <div className="w-8 h-1 bg-purple-400"></div>
                  <Cable className="w-5 h-5 text-purple-600 mx-2" />
                  <div className="w-8 h-1 bg-purple-400"></div>
                </div>
                
                <VHostNvmeTooltip 
                  vhostSocket={volume.vhost_socket} 
                  vhostDevice={volume.vhost_device}
                  vhostType={volume.vhost_type}
                  nvmeNamespaces={volume.nvme_namespaces}
                >
                  <div className="text-center cursor-help">
                    <div className="w-16 h-16 bg-indigo-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-indigo-300">
                      <Cable className="w-8 h-8 text-indigo-600" />
                    </div>
                    <p className="font-medium text-sm">VHost-NVMe</p>
                    <p className="text-xs text-gray-500">Unix Socket</p>
                    <Info className="w-3 h-3 text-gray-400 mx-auto mt-1" />
                  </div>
                </VHostNvmeTooltip>
              </div>
            </div>

            {/* Connection from VHost-NVMe to SPDK Volume */}
            <div className="flex justify-center mb-6">
              <div className="w-1 h-8 bg-indigo-400"></div>
            </div>
          </>
        )}
        
        <h4 className="text-xl font-semibold mb-6">SPDK Logical Volume: {volume.name}</h4>
        
        <div className="flex justify-center mb-8">
          <div className="text-center">
            <div className="w-20 h-20 bg-blue-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-blue-300">
              <Database className="w-10 h-10 text-blue-600" />
            </div>
            <p className="font-medium text-lg">SPDK Logical Volume</p>
            <p className="text-sm text-gray-600">{volume.name}</p>
            <p className="text-xs text-gray-500">{volume.size}</p>
            
            {/* VHost-NVMe Information Panel - Show only if vhost-nvme is configured */}
            {hasVHostNvme && (
              <div className="mt-3 p-3 bg-indigo-50 rounded-lg border border-indigo-200 text-left max-w-xs mx-auto">
                <h5 className="font-medium text-indigo-800 mb-2 flex items-center gap-2">
                  <Cable className="w-4 h-4" />
                  VHost-NVMe Access
                </h5>
                <div className="space-y-1 text-xs">
                  {volume.vhost_socket && (
                    <div>
                      <span className="text-gray-600 block">Socket:</span>
                      <span className="font-mono text-indigo-700 text-xs break-all">
                        {volume.vhost_socket}
                      </span>
                    </div>
                  )}
                  {volume.vhost_device && (
                    <div>
                      <span className="text-gray-600 block">Device:</span>
                      <span className="font-mono text-indigo-700 text-xs break-all">
                        {volume.vhost_device}
                      </span>
                    </div>
                  )}
                  <div>
                    <span className="text-gray-600">Type:</span>
                    <span className="font-medium text-indigo-700 ml-1 uppercase">
                      {volume.vhost_type || 'NVMe'}
                    </span>
                  </div>
                  {volume.nvme_namespaces && volume.nvme_namespaces.length > 0 && (
                    <div>
                      <span className="text-gray-600 block">Namespaces:</span>
                      {volume.nvme_namespaces.map((ns, idx) => (
                        <div key={idx} className="text-indigo-700 ml-2">
                          NSID {ns.nsid}: {Math.round(ns.size / 1024 / 1024 / 1024)}GB
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </div>
            )}
          </div>
        </div>
        
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6 max-w-4xl mx-auto">
          {volume.replica_statuses.map((replica, index) => (
            <div key={`${replica.node}-${index}`} className="relative">
              <div className="absolute top-0 left-1/2 transform -translate-x-1/2 -translate-y-8">
                <div 
                  className="w-1 h-8"
                  style={{ backgroundColor: getConnectionColor(replica.status) }}
                />
              </div>
              
              <div className={`border-2 rounded-lg p-4 ${getReplicaStatusColor(replica.status)}`}>
                <div className="flex items-center justify-between mb-2">
                  <div className="flex items-center gap-2">
                    {replica.is_local ? (
                      <div className="flex items-center gap-1">
                        {getReplicaIcon(replica)}
                        <span className="font-medium">{replica.node}</span>
                      </div>
                    ) : (
                      <NVMFTooltip target={replica.nvmf_target}>
                        <div className="flex items-center gap-1">
                          {getReplicaIcon(replica)}
                          <span className="font-medium">{replica.node}</span>
                          <Info className="w-3 h-3 text-gray-400" />
                        </div>
                      </NVMFTooltip>
                    )}
                  </div>
                  {replica.is_new_replica && (
                    <span className="text-xs bg-blue-500 text-white px-2 py-1 rounded-full">
                      NEW
                    </span>
                  )}
                </div>
                
                <div className="text-xs space-y-1">
                  <div className="flex justify-between">
                    <span>Status:</span>
                    <span className="font-medium capitalize">{replica.status}</span>
                  </div>
                  
                  <div className="flex justify-between">
                    <span>Type:</span>
                    <span className={`font-medium ${replica.is_local ? 'text-blue-600' : 'text-purple-600'}`}>
                      {replica.is_local ? 'Local NVMe' : 'NVMe-oF'}
                    </span>
                  </div>
                  
                  {replica.status === 'rebuilding' && replica.rebuild_progress && (
                    <div className="mt-2">
                      <div className="flex justify-between text-xs mb-1">
                        <span>Rebuild Progress:</span>
                        <span>{replica.rebuild_progress}%</span>
                      </div>
                      <div className="w-full bg-gray-200 rounded-full h-2">
                        <div 
                          className="bg-orange-500 h-2 rounded-full transition-all duration-300" 
                          style={{ width: `${replica.rebuild_progress}%` }}
                        />
                      </div>
                    </div>
                  )}
                  
                  {replica.last_io_timestamp && (
                    <div className="flex justify-between">
                      <span>Last I/O:</span>
                      <span className="font-medium">
                        {new Date(replica.last_io_timestamp).toLocaleTimeString()}
                      </span>
                    </div>
                  )}
                  
                  {replica.status === 'failed' && (
                    <div className="mt-2 p-2 bg-red-50 rounded border border-red-200">
                      <div className="flex items-center gap-1 text-red-700">
                        <AlertTriangle className="w-3 h-3" />
                        <span className="text-xs font-medium">Replica Unreachable</span>
                      </div>
                      <div className="text-xs text-red-600 mt-1">
                        Connection lost - rebuild required
                      </div>
                    </div>
                  )}
                </div>
              </div>
            </div>
          ))}
        </div>
        
        <div className="mt-8 flex justify-center gap-4 flex-wrap">
          <span 
            className={`px-4 py-2 rounded-full text-sm font-medium ${
              volume.active_replicas === volume.replicas 
                ? 'bg-green-100 text-green-800' 
                : volume.active_replicas > 0
                ? 'bg-yellow-100 text-yellow-800'
                : 'bg-red-100 text-red-800'
            }`}
          >
            {volume.active_replicas}/{volume.replicas} Active Replicas
          </span>
          
          {volume.local_nvme && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-blue-100 text-blue-800 flex items-center gap-1">
              <Zap className="w-4 h-4" />
              High Performance Path
            </span>
          )}
          
          {hasVHostNvme && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-indigo-100 text-indigo-800 flex items-center gap-1">
              <Cable className="w-4 h-4" />
              VHost-NVMe Enabled
            </span>
          )}
          
          {volume.state === 'Rebuilding' && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-orange-100 text-orange-800 flex items-center gap-1">
              <Settings className="w-4 h-4 animate-spin" />
              Rebuild in Progress
            </span>
          )}
          
          {volume.replica_statuses.some(r => r.status === 'failed') && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-red-100 text-red-800 flex items-center gap-1">
              <AlertTriangle className="w-4 h-4" />
              {volume.replica_statuses.filter(r => r.status === 'failed').length} Failed Replica(s)
            </span>
          )}
        </div>
        
        {/* VHost-NVMe Technology Info - Show only if vhost-nvme is configured */}
        {hasVHostNvme && (
          <div className="mt-6 p-4 bg-gray-50 rounded-lg border border-gray-200">
            <h5 className="font-medium text-gray-800 mb-2 flex items-center gap-2">
              <Info className="w-5 h-5 text-blue-600" />
              VHost-NVMe Technology
            </h5>
            <div className="text-sm text-gray-600 text-left space-y-2">
              <p>
                <strong>VHost-NVMe</strong> provides a high-performance interface between user applications and SPDK storage.
                The volume is exposed as a virtual NVMe device through a Unix domain socket with native NVMe namespace support.
              </p>
              <div className="grid grid-cols-1 md:grid-cols-2 gap-4 mt-3">
                <div>
                  <h6 className="font-medium text-gray-700 mb-1">Benefits:</h6>
                  <ul className="text-xs space-y-1 text-gray-600">
                    <li>• Zero-copy data access</li>
                    <li>• Native NVMe command support</li>
                    <li>• Multiple namespace capability</li>
                    <li>• Bypasses kernel for I/O operations</li>
                    <li>• Ultra-low latency and high IOPS</li>
                  </ul>
                </div>
                <div>
                  <h6 className="font-medium text-gray-700 mb-1">Access Pattern:</h6>
                  <ul className="text-xs space-y-1 text-gray-600">
                    <li>• Pod mounts volume via vhost-nvme device</li>
                    <li>• Native NVMe namespace presentation</li>
                    <li>• SPDK handles all replica management</li>
                    <li>• Transparent failover on replica failure</li>
                    <li>• Automatic rebuild and recovery</li>
                  </ul>
                </div>
              </div>
              {volume.nvme_namespaces && volume.nvme_namespaces.length > 0 && (
                <div className="mt-3 p-2 bg-blue-50 rounded">
                  <h6 className="font-medium text-gray-700 mb-1">NVMe Namespaces:</h6>
                  <div className="text-xs space-y-1">
                    {volume.nvme_namespaces.map((ns, idx) => (
                      <div key={idx} className="flex justify-between">
                        <span>NSID {ns.nsid}:</span>
                        <span>{Math.round(ns.size / 1024 / 1024 / 1024)}GB ({ns.bdev_name})</span>
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
        
        {volume.replica_statuses.some(r => r.status === 'rebuilding') && (
          <div className="mt-6 p-4 bg-orange-50 rounded-lg border border-orange-200">
            <h5 className="font-medium text-orange-800 mb-2 flex items-center gap-2">
              <Settings className="w-5 h-5 animate-spin" />
              Active Rebuild Operations
            </h5>
            <div className="space-y-2 text-sm">
              {volume.replica_statuses
                .filter(r => r.status === 'rebuilding')
                .map((replica, index) => (
                  <div key={index} className="text-left bg-white p-3 rounded border">
                    <div className="flex justify-between items-center mb-2">
                      <span className="font-medium">
                        {replica.is_new_replica ? `New replica on ${replica.node}` : `Rebuilding ${replica.node}`}
                      </span>
                      <span className="text-orange-600 font-medium">
                        {replica.rebuild_progress}%
                      </span>
                    </div>
                    <div className="w-full bg-gray-200 rounded-full h-2 mb-2">
                      <div 
                        className="bg-orange-500 h-2 rounded-full transition-all duration-300" 
                        style={{ width: `${replica.rebuild_progress}%` }}
                      />
                    </div>
                    <div className="text-xs text-gray-600">
                      {replica.is_new_replica 
                        ? `Replacing failed replica with new instance on ${replica.node}`
                        : `Synchronizing data to restore replica health`
                      }
                    </div>
                    {replica.nvmf_target && (
                      <div className="text-xs text-gray-500 mt-1">
                        NVMe-oF Target: {replica.nvmf_target.target_ip}:{replica.nvmf_target.target_port}
                      </div>
                    )}
                    {hasVHostNvme && (
                      <div className="text-xs text-blue-600 mt-1">
                        VHost-NVMe access remains available during rebuild operations
                      </div>
                    )}
                  </div>
                ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
};