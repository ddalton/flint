import React, { useState } from 'react';
import { PieChart, Pie, Cell, BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip as RechartsTooltip, Legend, ResponsiveContainer } from 'recharts';
import { RefreshCw, LogOut, Database, HardDrive, Server, AlertTriangle, CheckCircle, X, Settings, Zap, Activity, Monitor, ChevronDown, ChevronRight, Info, Network } from 'lucide-react';
import type { DashboardData, Volume, Disk, NvmfTarget } from '../hooks/useDashboardData';

interface DashboardProps {
  data: DashboardData;
  loading: boolean;
  stats: {
    totalVolumes: number;
    faultedVolumes: number;
    rebuildingVolumes: number;
    localNVMeVolumes: number;
    totalDisks: number;
    healthyDisks: number;
    formattedDisks: number;
  };
  autoRefresh: boolean;
  onAutoRefreshChange: (enabled: boolean) => void;
  onRefresh: () => void;
  onLogout: () => void;
}

// Tooltip component for NVMe-oF target information
const NVMFTooltip = ({ target, children }: { target: NvmfTarget | null; children: React.ReactNode }) => {
  const [showTooltip, setShowTooltip] = useState(false);
  
  if (!target) return <>{children}</>;
  
  return (
    <div className="relative inline-block">
      <div
        onMouseEnter={() => setShowTooltip(true)}
        onMouseLeave={() => setShowTooltip(false)}
        className="cursor-help"
      >
        {children}
      </div>
      {showTooltip && (
        <div className="absolute z-50 bottom-full left-1/2 transform -translate-x-1/2 mb-2 px-3 py-2 bg-gray-900 text-white text-xs rounded-lg shadow-lg whitespace-nowrap">
          <div className="space-y-1">
            <div><strong>NQN:</strong> {target.nqn}</div>
            <div><strong>Target:</strong> {target.target_ip}:{target.target_port}</div>
            <div><strong>Transport:</strong> {target.transport_type}</div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};

// Dashboard Charts Components
const VolumeStatusChart = ({ volumes }: { volumes: Volume[] }) => {
  const statusCounts = volumes.reduce((acc, volume) => {
    acc[volume.state] = (acc[volume.state] || 0) + 1;
    return acc;
  }, {} as Record<string, number>);

  const data = Object.entries(statusCounts).map(([status, count]) => ({
    name: status,
    value: count,
    color: status === 'Healthy' ? '#10b981' : status === 'Rebuilding' ? '#f59e0b' : '#ef4444'
  }));

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center mb-4">
        <Database className="w-6 h-6 text-blue-600 mr-2" />
        <h3 className="text-lg font-semibold">PVC Volume Status Distribution</h3>
      </div>
      <ResponsiveContainer width="100%" height={300}>
        <PieChart>
          <Pie
            data={data}
            cx="50%"
            cy="50%"
            outerRadius={80}
            dataKey="value"
            label={({name, value}) => `${name}: ${value}`}
          >
            {data.map((entry, index) => (
              <Cell key={`cell-${index}`} fill={entry.color} />
            ))}
          </Pie>
          <RechartsTooltip />
        </PieChart>
      </ResponsiveContainer>
      
      <div className="mt-4 flex flex-wrap gap-2">
        {data.map((item) => (
          <span
            key={item.name}
            className="px-3 py-1 rounded-full text-sm text-white"
            style={{ backgroundColor: item.color }}
          >
            {item.name}: {item.value}
          </span>
        ))}
      </div>
    </div>
  );
};

const DiskStatusChart = ({ disks }: { disks: Disk[] }) => {
  const nodeData = disks.reduce((acc, disk) => {
    if (!acc[disk.node]) {
      acc[disk.node] = { total: 0, initialized: 0, healthy: 0 };
    }
    acc[disk.node].total++;
    if (disk.lvol_store_initialized) acc[disk.node].initialized++;
    if (disk.healthy) acc[disk.node].healthy++;
    return acc;
  }, {} as Record<string, { total: number; initialized: number; healthy: number }>);

  const chartData = Object.entries(nodeData).map(([node, data]) => ({
    node,
    total: data.total,
    initialized: data.initialized,
    uninitialized: data.total - data.initialized,
    unhealthy: data.total - data.healthy
  }));

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center mb-4">
        <HardDrive className="w-6 h-6 text-blue-600 mr-2" />
        <h3 className="text-lg font-semibold">NVMe Logical Volume Store Status by Node</h3>
      </div>
      <ResponsiveContainer width="100%" height={300}>
        <BarChart data={chartData}>
          <CartesianGrid strokeDasharray="3 3" />
          <XAxis dataKey="node" />
          <YAxis />
          <RechartsTooltip />
          <Legend />
          <Bar dataKey="initialized" stackId="a" fill="#10b981" name="LVS Initialized" />
          <Bar dataKey="uninitialized" stackId="a" fill="#f59e0b" name="Uninitialized" />
          <Bar dataKey="unhealthy" stackId="b" fill="#ef4444" name="Unhealthy" />
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
};

const RaidTopologyChart = ({ volumes }: { volumes: Volume[] }) => {
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
        <h4 className="text-xl font-semibold mb-6">PVC Volume: {volume.name}</h4>
        
        <div className="flex justify-center mb-8">
          <div className="text-center">
            <div className="w-20 h-20 bg-blue-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-blue-300">
              <Database className="w-10 h-10 text-blue-600" />
            </div>
            <p className="font-medium text-lg">SPDK Logical Volume</p>
            <p className="text-sm text-gray-600">{volume.name}</p>
            <p className="text-xs text-gray-500">{volume.size}</p>
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
                  </div>
                ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
};

// Enhanced Node Detail View Component
const NodeDetailView = ({ 
  node, 
  nodeDisks, 
  nodeVolumes, 
  healthyDisks, 
  totalCapacity, 
  totalAllocated, 
  totalFree 
}: {
  node: string;
  nodeDisks: Disk[];
  nodeVolumes: Volume[];
  healthyDisks: number;
  totalCapacity: number;
  totalAllocated: number;
  totalFree: number;
}) => {
  const [expandedDisks, setExpandedDisks] = useState(new Set<string>());
  
  const toggleDiskExpansion = (diskId: string) => {
    const newExpanded = new Set(expandedDisks);
    if (newExpanded.has(diskId)) {
      newExpanded.delete(diskId);
    } else {
      newExpanded.add(diskId);
    }
    setExpandedDisks(newExpanded);
  };

  return (
    <div className="bg-gray-50 rounded-lg p-6">
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center">
          <div className="w-12 h-12 bg-blue-100 rounded-full flex items-center justify-center mr-4">
            <Server className="w-7 h-7 text-blue-600" />
          </div>
          <div>
            <h3 className="text-xl font-semibold">{node}</h3>
            <p className="text-sm text-gray-600">
              {totalCapacity}GB total • {totalAllocated}GB allocated • {totalFree}GB free
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <span className="px-3 py-1 text-sm bg-green-100 text-green-800 rounded-full">
            Ready
          </span>
          <span className="px-3 py-1 text-sm bg-blue-100 text-blue-800 rounded-full">
            {nodeDisks.length} Disks
          </span>
        </div>
      </div>

      <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-6">
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <HardDrive className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">NVMe Disks</p>
              <p className="text-lg font-bold">{healthyDisks}/{nodeDisks.length}</p>
              <p className="text-xs text-gray-500">healthy</p>
            </div>
          </div>
        </div>
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <Database className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">Volumes</p>
              <p className="text-lg font-bold">{nodeVolumes.length}</p>
              <p className="text-xs text-gray-500">replicas</p>
            </div>
          </div>
        </div>
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <Zap className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">Local NVMe</p>
              <p className="text-lg font-bold">{nodeVolumes.filter(v => v.local_nvme).length}</p>
              <p className="text-xs text-gray-500">high perf</p>
            </div>
          </div>
        </div>
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <Activity className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">Utilization</p>
              <p className="text-lg font-bold">{Math.round((totalAllocated / totalCapacity) * 100)}%</p>
              <p className="text-xs text-gray-500">capacity used</p>
            </div>
          </div>
        </div>
      </div>

      <div className="bg-white rounded-lg overflow-hidden">
        <div className="px-6 py-4 bg-gray-100 border-b">
          <h4 className="text-lg font-semibold flex items-center gap-2">
            <HardDrive className="w-5 h-5" />
            NVMe Disks & Logical Volume Stores on {node}
          </h4>
        </div>
        <div className="overflow-x-auto">
          <table className="min-w-full divide-y divide-gray-200">
            <thead className="bg-gray-50">
              <tr>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Disk</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Model</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Capacity</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Allocation</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">LVS Status</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">LVS Initialized</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Logical Volumes</th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200">
              {nodeDisks.map((disk) => (
                <React.Fragment key={disk.id}>
                  <tr className="hover:bg-gray-50">
                    <td className="px-4 py-4">
                      <div>
                        <div className="text-sm font-medium text-gray-900">{disk.id}</div>
                        <div className="text-xs text-gray-500">{disk.pci_addr}</div>
                      </div>
                    </td>
                    <td className="px-4 py-4 text-sm text-gray-700">{disk.model}</td>
                    <td className="px-4 py-4">
                      <div className="text-sm">
                        <div className="font-medium">{disk.capacity_gb}GB</div>
                        <div className="text-xs text-gray-500">{disk.free_space}GB free</div>
                      </div>
                    </td>
                    <td className="px-4 py-4">
                      <div className="text-sm">
                        <div className="flex items-center gap-2 mb-1">
                          <div className="w-20 bg-gray-200 rounded-full h-2">
                            <div 
                              className="bg-blue-500 h-2 rounded-full" 
                              style={{ width: `${(disk.allocated_space / disk.capacity_gb) * 100}%` }}
                            />
                          </div>
                          <span className="text-xs text-gray-600">
                            {Math.round((disk.allocated_space / disk.capacity_gb) * 100)}%
                          </span>
                        </div>
                        <div className="text-xs text-gray-500">
                          {disk.allocated_space}GB / {disk.capacity_gb}GB used
                        </div>
                      </div>
                    </td>
                    <td className="px-4 py-4">
                      <div className="space-y-1">
                        <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                          disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                        }`}>
                          {disk.healthy ? 'Healthy' : 'Unhealthy'}
                        </span>
                      </div>
                    </td>
                    <td className="px-4 py-4">
                      <div>
                        <span className={`inline-flex px-2 py-1 text-xs rounded-full ${
                          disk.lvol_store_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                        }`}>
                          {disk.lvol_store_initialized ? 'Initialized' : 'Not Initialized'}
                        </span>
                        {disk.lvol_store_initialized && (
                          <div className="text-xs text-gray-500 mt-1">
                            {disk.lvol_count} logical volumes
                          </div>
                        )}
                      </div>
                    </td>
                    <td className="px-4 py-4">
                      <div className="flex items-center gap-2">
                        <span className="text-sm text-gray-600">
                          {disk.provisioned_volumes.length} volumes
                        </span>
                        {disk.provisioned_volumes.length > 0 && (
                          <button
                            onClick={() => toggleDiskExpansion(disk.id)}
                            className="p-1 text-gray-400 hover:text-gray-600 rounded"
                          >
                            {expandedDisks.has(disk.id) ? (
                              <ChevronDown className="w-4 h-4" />
                            ) : (
                              <ChevronRight className="w-4 h-4" />
                            )}
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                  
                  {expandedDisks.has(disk.id) && disk.provisioned_volumes.length > 0 && (
                    <tr>
                      <td colSpan={7} className="px-4 py-2 bg-gray-50">
                        <div className="space-y-3">
                          <h5 className="text-sm font-medium text-gray-700 flex items-center gap-2">
                            <Database className="w-4 h-4" />
                            Logical Volumes on {disk.id}
                          </h5>
                          <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                            {disk.provisioned_volumes.map((volume, idx) => (
                              <div key={idx} className="p-3 bg-white rounded border border-gray-200 shadow-sm">
                                <div className="flex items-center justify-between mb-2">
                                  <span className="font-medium text-gray-900">{volume.volume_name}</span>
                                  <div className="flex items-center gap-2">
                                    <span className="text-sm text-gray-600">{volume.size}GB</span>
                                    <span className={`px-2 py-1 text-xs rounded-full ${
                                      volume.status === 'healthy' ? 'bg-green-100 text-green-700' :
                                      volume.status === 'rebuilding' ? 'bg-orange-100 text-orange-700' :
                                      'bg-red-100 text-red-700'
                                    }`}>
                                      {volume.status}
                                    </span>
                                  </div>
                                </div>
                                
                                <div className="flex items-center justify-between text-xs text-gray-500 mb-2">
                                  <span className={`px-2 py-1 rounded ${
                                    volume.replica_type === 'Local NVMe' 
                                      ? 'bg-blue-100 text-blue-700' 
                                      : 'bg-purple-100 text-purple-700'
                                  }`}>
                                    {volume.replica_type}
                                  </span>
                                  <span>
                                    {new Date(volume.provisioned_at).toLocaleDateString()}
                                  </span>
                                </div>
                                
                                <div className="text-xs text-gray-400">
                                  Volume ID: {volume.volume_id}
                                </div>
                                <div className="text-xs text-gray-400">
                                  Provisioned {Math.floor((Date.now() - new Date(volume.provisioned_at).getTime()) / (1000 * 60 * 60 * 24))} days ago
                                </div>
                                
                                {volume.replica_type === 'Local NVMe' && (
                                  <div className="mt-2 p-2 bg-blue-50 rounded text-xs">
                                    <div className="flex items-center gap-1 text-blue-700 mb-1">
                                      <Zap className="w-3 h-3" />
                                      <span className="font-medium">High Performance Path</span>
                                    </div>
                                    <div className="text-blue-600">
                                      Direct NVMe access • Zero network latency
                                    </div>
                                  </div>
                                )}
                              </div>
                            ))}
                          </div>
                        </div>
                      </td>
                    </tr>
                  )}
                </React.Fragment>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
};

// Main Dashboard Component
export const Dashboard: React.FC<DashboardProps> = ({
  data,
  loading,
  stats,
  autoRefresh,
  onAutoRefreshChange,
  onRefresh,
  onLogout
}) => {
  const [activeTab, setActiveTab] = useState('overview');

  if (loading && data.volumes.length === 0) {
    return (
      <div className="flex justify-center items-center h-screen">
        <div className="animate-spin rounded-full h-16 w-16 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  return (
    <div className="min-h-screen bg-gray-50">
      <header className="bg-white shadow-sm border-b">
        <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
          <div className="flex justify-between items-center py-4">
            <div className="flex items-center">
              <Database className="w-8 h-8 text-blue-600 mr-3" />
              <h1 className="text-2xl font-bold text-gray-900">SPDK CSI Dashboard</h1>
            </div>
            
            <div className="flex items-center gap-4">
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={autoRefresh}
                  onChange={(e) => onAutoRefreshChange(e.target.checked)}
                  className="rounded"
                />
                Auto-refresh
              </label>
              
              <button
                onClick={onRefresh}
                className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md"
              >
                <RefreshCw className="w-5 h-5" />
              </button>
              
              <button
                onClick={onLogout}
                className="flex items-center gap-2 px-3 py-2 text-sm text-gray-700 hover:text-gray-900 hover:bg-gray-100 rounded-md"
              >
                <LogOut className="w-4 h-4" />
                Logout
              </button>
            </div>
          </div>
        </div>
      </header>

      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-6 mb-8">
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <Database className="w-10 h-10 text-blue-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{stats.totalVolumes}</p>
                <p className="text-gray-600">Total Volumes</p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <AlertTriangle className="w-10 h-10 text-red-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{stats.faultedVolumes}</p>
                <p className="text-gray-600">Faulted Volumes</p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <Settings className="w-10 h-10 text-orange-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{stats.rebuildingVolumes}</p>
                <p className="text-gray-600">Rebuilding</p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <Zap className="w-10 h-10 text-green-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{stats.localNVMeVolumes}</p>
                <p className="text-gray-600">Local NVMe</p>
              </div>
            </div>
          </div>
        </div>

        <div className="bg-white rounded-lg shadow mb-6">
          <div className="border-b border-gray-200">
            <nav className="-mb-px flex space-x-8 px-6">
              {[
                { id: 'overview', name: 'Overview', icon: Monitor },
                { id: 'volumes', name: 'Volumes', icon: Database },
                { id: 'disks', name: 'Disks', icon: HardDrive },
                { id: 'nodes', name: 'Nodes', icon: Server }
              ].map((tab) => (
                <button
                  key={tab.id}
                  onClick={() => setActiveTab(tab.id)}
                  className={`flex items-center gap-2 py-4 px-1 border-b-2 font-medium text-sm ${
                    activeTab === tab.id
                      ? 'border-blue-500 text-blue-600'
                      : 'border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300'
                  }`}
                >
                  <tab.icon className="w-5 h-5" />
                  {tab.name}
                </button>
              ))}
            </nav>
          </div>

          <div className="p-6">
            {activeTab === 'overview' && (
              <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
                <VolumeStatusChart volumes={data.volumes} />
                <DiskStatusChart disks={data.disks} />
                <div className="lg:col-span-2">
                  <RaidTopologyChart volumes={data.volumes} />
                </div>
              </div>
            )}

            {activeTab === 'volumes' && (
              <div className="overflow-x-auto">
                <table className="min-w-full divide-y divide-gray-200">
                  <thead className="bg-gray-50">
                    <tr>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Volume Name</th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Size</th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">State</th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Replicas</th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Local NVMe</th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Rebuild Progress</th>
                      <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Nodes</th>
                    </tr>
                  </thead>
                  <tbody className="bg-white divide-y divide-gray-200">
                    {data.volumes.map((volume) => (
                      <tr key={volume.id} className="hover:bg-gray-50">
                        <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{volume.name}</td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.size}</td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                            volume.state === 'Healthy' ? 'bg-green-100 text-green-800' :
                            volume.state === 'Rebuilding' ? 'bg-yellow-100 text-yellow-800' : 'bg-red-100 text-red-800'
                          }`}>
                            {volume.state}
                          </span>
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.active_replicas}/{volume.replicas}</td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          {volume.local_nvme ? (
                            <CheckCircle className="w-5 h-5 text-green-500" />
                          ) : (
                            <X className="w-5 h-5 text-gray-400" />
                          )}
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          {volume.rebuild_progress ? (
                            <div className="flex items-center gap-2">
                              <div className="w-20 bg-gray-200 rounded-full h-2">
                                <div 
                                  className="bg-blue-600 h-2 rounded-full" 
                                  style={{ width: `${volume.rebuild_progress}%` }}
                                />
                              </div>
                              <span className="text-sm text-gray-600">{volume.rebuild_progress}%</span>
                            </div>
                          ) : (
                            <span className="text-gray-400">-</span>
                          )}
                        </td>
                        <td className="px-6 py-4">
                          <div className="flex flex-wrap gap-1">
                            {volume.nodes.map(node => (
                              <span key={node} className="inline-flex px-2 py-1 text-xs bg-gray-100 text-gray-800 rounded">
                                {node}
                              </span>
                            ))}
                          </div>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}

            {activeTab === 'disks' && (
              <div>
                <div className="grid grid-cols-1 md:grid-cols-3 gap-4 mb-6">
                  <div className="bg-gray-50 rounded-lg p-4">
                    <h3 className="text-lg font-semibold">Total Disks</h3>
                    <p className="text-3xl font-bold text-blue-600">{stats.totalDisks}</p>
                  </div>
                  <div className="bg-gray-50 rounded-lg p-4">
                    <h3 className="text-lg font-semibold">Healthy Disks</h3>
                    <p className="text-3xl font-bold text-green-600">{stats.healthyDisks}</p>
                  </div>
                  <div className="bg-gray-50 rounded-lg p-4">
                    <h3 className="text-lg font-semibold">LVS Initialized</h3>
                    <p className="text-3xl font-bold text-blue-600">{stats.formattedDisks}</p>
                  </div>
                </div>
                
                <div className="overflow-x-auto">
                  <table className="min-w-full divide-y divide-gray-200">
                    <thead className="bg-gray-50">
                      <tr>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Disk ID</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Node</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Model</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Capacity</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Free Space</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Status</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">LVS Initialized</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Performance</th>
                      </tr>
                    </thead>
                    <tbody className="bg-white divide-y divide-gray-200">
                      {data.disks.map((disk) => (
                        <tr key={disk.id} className="hover:bg-gray-50">
                          <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{disk.id}</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.node}</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.model}</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.capacity_gb}GB</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.free_space}GB</td>
                          <td className="px-6 py-4 whitespace-nowrap">
                            <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                              disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                            }`}>
                              {disk.healthy ? 'Healthy' : 'Unhealthy'}
                            </span>
                          </td>
                          <td className="px-6 py-4 whitespace-nowrap">
                            <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                              disk.lvol_store_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                            }`}>
                              {disk.lvol_store_initialized ? 'Yes' : 'No'}
                            </span>
                          </td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                            <div>
                              <div>R: {disk.read_iops.toLocaleString()} IOPS</div>
                              <div>W: {disk.write_iops.toLocaleString()} IOPS</div>
                            </div>
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            )}

            {activeTab === 'nodes' && (
              <div className="space-y-6">
                {data.nodes.map((node) => {
                  const nodeDisks = data.disks.filter(d => d.node === node);
                  const nodeVolumes = data.volumes.filter(v => v.nodes.includes(node));
                  const healthyDisks = nodeDisks.filter(d => d.healthy).length;
                  const totalCapacity = nodeDisks.reduce((sum, disk) => sum + disk.capacity_gb, 0);
                  const totalAllocated = nodeDisks.reduce((sum, disk) => sum + disk.allocated_space, 0);
                  const totalFree = totalCapacity - totalAllocated;
                  
                  return (
                    <NodeDetailView 
                      key={node} 
                      node={node}
                      nodeDisks={nodeDisks}
                      nodeVolumes={nodeVolumes}
                      healthyDisks={healthyDisks}
                      totalCapacity={totalCapacity}
                      totalAllocated={totalAllocated}
                      totalFree={totalFree}
                    />
                  );
                })}
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
};
