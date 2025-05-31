import React, { useState, useEffect } from 'react';
import { PieChart, Pie, Cell, BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip as RechartsTooltip, Legend, ResponsiveContainer, LineChart, Line, ScatterChart, Scatter } from 'recharts';
import { RefreshCw, LogOut, Database, HardDrive, Server, AlertTriangle, CheckCircle, X, Settings, Zap, Activity, Monitor, ChevronDown, ChevronRight, Info, Network, Eye, EyeOff } from 'lucide-react';

// Mock authentication service (FIXED VERSION for local development)
const authService = {
  token: '', // Store in memory instead of localStorage
  
  login: async (username: string, password: string) => {
    await new Promise(resolve => setTimeout(resolve, 1000));
    if (username === 'admin' && password === 'spdk-admin-2025') {
      authService.token = 'mock-token';
      return { success: true };
    }
    throw new Error('Invalid credentials');
  },
  
  logout: () => {
    authService.token = '';
  },
  
  isAuthenticated: () => {
    return !!authService.token;
  }
};

// Mock data generation
const generateMockData = () => {
  const volumes = [];
  const disks = [];
  const nodes = ['node-a', 'node-b', 'node-c', 'node-d', 'node-e'];
  
  // Generate volumes
  for (let i = 1; i <= 15; i++) {
    const totalReplicas = Math.floor(Math.random() * 3 + 2);
    const selectedNodes = nodes.slice(0, totalReplicas);
    const isHealthy = Math.random() > 0.3;
    const isRebuilding = !isHealthy && Math.random() > 0.4;
    const hasLocalNVMe = Math.random() > 0.3;
    
    const replicaStatuses = selectedNodes.map((node, index) => {
      if (isHealthy) {
        return {
          node,
          status: 'healthy',
          isLocal: index === 0 && hasLocalNVMe,
          lastIOTimestamp: new Date(Date.now() - Math.random() * 3600000).toISOString(),
          nvmfTarget: index === 0 && hasLocalNVMe ? null : {
            nqn: `nqn.2025-05.io.spdk:vol-${i}-replica-${index}`,
            targetIP: `192.168.1.${100 + nodes.indexOf(node)}`,
            targetPort: '4420',
            transportType: 'TCP'
          }
        };
      } else {
        const isFailed = Math.random() > 0.6;
        const isRebuilding = !isFailed && Math.random() > 0.5;
        
        return {
          node,
          status: isFailed ? 'failed' : (isRebuilding ? 'rebuilding' : 'healthy'),
          isLocal: index === 0 && hasLocalNVMe,
          lastIOTimestamp: isFailed ? null : new Date(Date.now() - Math.random() * 3600000).toISOString(),
          rebuildProgress: isRebuilding ? Math.floor(Math.random() * 90 + 10) : null,
          rebuildTarget: isRebuilding ? nodes[Math.floor(Math.random() * nodes.length)] : null,
          nvmfTarget: index === 0 && hasLocalNVMe ? null : {
            nqn: `nqn.2025-05.io.spdk:vol-${i}-replica-${index}`,
            targetIP: `192.168.1.${100 + nodes.indexOf(node)}`,
            targetPort: '4420',
            transportType: 'TCP'
          }
        };
      }
    });
    
    if (isRebuilding && replicaStatuses.some(r => r.status === 'failed')) {
      const availableNodes = nodes.filter(n => !selectedNodes.includes(n));
      if (availableNodes.length > 0) {
        const targetNode = availableNodes[Math.floor(Math.random() * availableNodes.length)];
        replicaStatuses.push({
          node: targetNode,
          status: 'rebuilding',
          isLocal: false,
          lastIOTimestamp: new Date().toISOString(),
          rebuildProgress: Math.floor(Math.random() * 90 + 10),
          isNewReplica: true,
          nvmfTarget: {
            nqn: `nqn.2025-05.io.spdk:vol-${i}-rebuild-${Date.now()}`,
            targetIP: `192.168.1.${100 + nodes.indexOf(targetNode)}`,
            targetPort: '4420',
            transportType: 'TCP'
          }
        });
      }
    }
    
    const activeCount = replicaStatuses.filter(r => r.status === 'healthy' || r.status === 'rebuilding').length;
    
    volumes.push({
      id: `raid1-vol-${i}`,
      name: `volume-${i}`,
      size: `${Math.floor(Math.random() * 500 + 100)}GB`,
      state: isRebuilding ? 'Rebuilding' : (isHealthy ? 'Healthy' : 'Degraded'),
      replicas: totalReplicas,
      activeReplicas: activeCount,
      localNVMe: hasLocalNVMe,
      rebuildProgress: isRebuilding ? Math.floor(Math.random() * 90 + 10) : null,
      nodes: selectedNodes,
      replicaStatuses: replicaStatuses
    });
  }
  
  // Generate disks
  nodes.forEach((node, nodeIndex) => {
    for (let i = 1; i <= Math.floor(Math.random() * 4 + 2); i++) {
      const isFormatted = Math.random() > 0.3;
      const isHealthy = Math.random() > 0.1;
      const totalCapacity = Math.floor(Math.random() * 1000 + 500);
      const allocatedSpace = isFormatted ? Math.floor(Math.random() * (totalCapacity * 0.8)) : 0;
      
      const bringOnlineTime = new Date(Date.now() - Math.random() * 30 * 24 * 60 * 60 * 1000);
      
      disks.push({
        id: `${node}-nvme${i}`,
        node,
        pciAddr: `0000:${(nodeIndex * 10 + i).toString(16).padStart(2, '0')}:00.0`,
        capacity: totalCapacity,
        capacityGB: totalCapacity,
        allocatedSpace: allocatedSpace,
        freeSpace: totalCapacity - allocatedSpace,
        freeSpaceDisplay: `${totalCapacity - allocatedSpace}GB`,
        healthy: isHealthy,
        blobstoreInitialized: isFormatted,
        blobCount: isFormatted ? Math.floor(Math.random() * 5) : 0,
        model: `Samsung NVMe ${Math.floor(Math.random() * 3 + 1)}TB`,
        readIOPS: Math.floor(Math.random() * 50000 + 10000),
        writeIOPS: Math.floor(Math.random() * 40000 + 8000),
        readLatency: Math.floor(Math.random() * 100 + 20),
        writeLatency: Math.floor(Math.random() * 150 + 30),
        broughtOnline: bringOnlineTime.toISOString(),
        provisionedVolumes: []
      });
    }
  });
  
  // Assign volumes to disks
  volumes.forEach(volume => {
    volume.replicaStatuses.forEach(replica => {
      if (replica.status === 'healthy' || replica.status === 'rebuilding') {
        const disk = disks.find(d => d.node === replica.node);
        if (disk && disk.blobstoreInitialized) {
          const volumeSize = parseInt(volume.size.replace('GB', ''));
          disk.provisionedVolumes.push({
            volumeName: volume.name,
            volumeId: volume.id,
            size: volumeSize,
            provisionedAt: new Date(Date.now() - Math.random() * 20 * 24 * 60 * 60 * 1000).toISOString(),
            replicaType: replica.isLocal ? 'Local NVMe' : 'Remote',
            status: replica.status
          });
        }
      }
    });
  });
  
  return { volumes, disks, nodes };
};

// Login Component
const LoginPage = ({ onLogin }) => {
  const [username, setUsername] = useState('admin');
  const [password, setPassword] = useState('spdk-admin-2025');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const handleSubmit = async (e) => {
    e.preventDefault();
    setLoading(true);
    setError('');
    
    try {
      await authService.login(username, password);
      onLogin();
    } catch (err) {
      setError(err.message);
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="min-h-screen flex items-center justify-center bg-gradient-to-br from-blue-600 to-blue-800">
      <div className="bg-white rounded-lg shadow-xl p-8 w-full max-w-md">
        <div className="text-center mb-8">
          <div className="mx-auto w-16 h-16 bg-blue-100 rounded-full flex items-center justify-center mb-4">
            <Database className="w-8 h-8 text-blue-600" />
          </div>
          <h1 className="text-3xl font-bold text-gray-900 mb-2">SPDK CSI Dashboard</h1>
          <p className="text-gray-600">Sign in to access the storage management console</p>
        </div>
        
        {error && (
          <div className="mb-4 p-3 bg-red-100 border border-red-400 text-red-700 rounded">
            {error}
          </div>
        )}
        
        <div onSubmit={handleSubmit}>
          <div className="mb-4">
            <label className="block text-gray-700 text-sm font-bold mb-2">
              Username
            </label>
            <input
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              required
            />
          </div>
          <div className="mb-6">
            <label className="block text-gray-700 text-sm font-bold mb-2">
              Password
            </label>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              required
            />
          </div>
          <button
            onClick={handleSubmit}
            disabled={loading}
            className="w-full bg-blue-600 text-white py-2 px-4 rounded-md hover:bg-blue-700 focus:outline-none focus:ring-2 focus:ring-blue-500 disabled:opacity-50 flex items-center justify-center"
          >
            {loading ? (
              <div className="animate-spin rounded-full h-5 w-5 border-b-2 border-white"></div>
            ) : (
              'Sign In'
            )}
          </button>
        </div>
        
        <div className="mt-4 p-3 bg-gray-50 rounded-md">
          <p className="text-sm text-gray-600">
            Default credentials: admin / spdk-admin-2025
          </p>
        </div>
      </div>
    </div>
  );
};

// Tooltip component for NVMe-oF target information
const NVMFTooltip = ({ target, children }) => {
  const [showTooltip, setShowTooltip] = useState(false);
  
  if (!target) return children;
  
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
            <div><strong>Target:</strong> {target.targetIP}:{target.targetPort}</div>
            <div><strong>Transport:</strong> {target.transportType}</div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};

// Dashboard Charts Components
const VolumeStatusChart = ({ volumes }) => {
  const statusCounts = volumes.reduce((acc, volume) => {
    acc[volume.state] = (acc[volume.state] || 0) + 1;
    return acc;
  }, {});

  const data = Object.entries(statusCounts).map(([status, count]) => ({
    name: status,
    value: count,
    color: status === 'Healthy' ? '#10b981' : status === 'Rebuilding' ? '#f59e0b' : '#ef4444'
  }));

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center mb-4">
        <Database className="w-6 h-6 text-blue-600 mr-2" />
        <h3 className="text-lg font-semibold">Volume Status Distribution</h3>
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

const DiskStatusChart = ({ disks }) => {
  const nodeData = disks.reduce((acc, disk) => {
    if (!acc[disk.node]) {
      acc[disk.node] = { total: 0, formatted: 0, healthy: 0 };
    }
    acc[disk.node].total++;
    if (disk.blobstoreInitialized) acc[disk.node].formatted++;
    if (disk.healthy) acc[disk.node].healthy++;
    return acc;
  }, {});

  const chartData = Object.entries(nodeData).map(([node, data]) => ({
    node,
    total: data.total,
    formatted: data.formatted,
    unformatted: data.total - data.formatted,
    unhealthy: data.total - data.healthy
  }));

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center mb-4">
        <HardDrive className="w-6 h-6 text-blue-600 mr-2" />
        <h3 className="text-lg font-semibold">NVMe Disk Status by Node</h3>
      </div>
      <ResponsiveContainer width="100%" height={300}>
        <BarChart data={chartData}>
          <CartesianGrid strokeDasharray="3 3" />
          <XAxis dataKey="node" />
          <YAxis />
          <RechartsTooltip />
          <Legend />
          <Bar dataKey="formatted" stackId="a" fill="#10b981" name="Formatted" />
          <Bar dataKey="unformatted" stackId="a" fill="#f59e0b" name="Unformatted" />
          <Bar dataKey="unhealthy" stackId="b" fill="#ef4444" name="Unhealthy" />
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
};

const RaidTopologyChart = ({ volumes }) => {
  const [selectedVolume, setSelectedVolume] = useState(volumes[0]?.id || '');
  
  const volume = volumes.find(v => v.id === selectedVolume);
  
  if (!volume) return null;

  const getReplicaStatusColor = (status) => {
    switch (status) {
      case 'healthy': return 'bg-green-100 text-green-800 border-green-200';
      case 'failed': return 'bg-red-100 text-red-800 border-red-200';
      case 'rebuilding': return 'bg-orange-100 text-orange-800 border-orange-200';
      default: return 'bg-gray-100 text-gray-800 border-gray-200';
    }
  };

  const getReplicaIcon = (replica) => {
    if (replica.status === 'failed') return <X className="w-4 h-4 text-red-600" />;
    if (replica.status === 'rebuilding') return <Settings className="w-4 h-4 text-orange-600 animate-spin" />;
    if (replica.isLocal) return <Zap className="w-4 h-4 text-blue-600" />;
    return <Network className="w-4 h-4 text-purple-600" />;
  };

  const getConnectionColor = (status) => {
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
        <h4 className="text-xl font-semibold mb-6">Volume: {volume.name}</h4>
        
        <div className="flex justify-center mb-8">
          <div className="text-center">
            <div className="w-20 h-20 bg-blue-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-blue-300">
              <Database className="w-10 h-10 text-blue-600" />
            </div>
            <p className="font-medium text-lg">Primary Volume</p>
            <p className="text-sm text-gray-600">{volume.name}</p>
            <p className="text-xs text-gray-500">{volume.size}</p>
          </div>
        </div>
        
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6 max-w-4xl mx-auto">
          {volume.replicaStatuses.map((replica, index) => (
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
                    {replica.isLocal ? (
                      <div className="flex items-center gap-1">
                        {getReplicaIcon(replica)}
                        <span className="font-medium">{replica.node}</span>
                      </div>
                    ) : (
                      <NVMFTooltip target={replica.nvmfTarget}>
                        <div className="flex items-center gap-1">
                          {getReplicaIcon(replica)}
                          <span className="font-medium">{replica.node}</span>
                          <Info className="w-3 h-3 text-gray-400" />
                        </div>
                      </NVMFTooltip>
                    )}
                  </div>
                  {replica.isNewReplica && (
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
                    <span className={`font-medium ${replica.isLocal ? 'text-blue-600' : 'text-purple-600'}`}>
                      {replica.isLocal ? 'Local NVMe' : 'NVMe-oF'}
                    </span>
                  </div>
                  
                  {replica.status === 'rebuilding' && replica.rebuildProgress && (
                    <div className="mt-2">
                      <div className="flex justify-between text-xs mb-1">
                        <span>Rebuild Progress:</span>
                        <span>{replica.rebuildProgress}%</span>
                      </div>
                      <div className="w-full bg-gray-200 rounded-full h-2">
                        <div 
                          className="bg-orange-500 h-2 rounded-full transition-all duration-300" 
                          style={{ width: `${replica.rebuildProgress}%` }}
                        />
                      </div>
                    </div>
                  )}
                  
                  {replica.lastIOTimestamp && (
                    <div className="flex justify-between">
                      <span>Last I/O:</span>
                      <span className="font-medium">
                        {new Date(replica.lastIOTimestamp).toLocaleTimeString()}
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
              volume.activeReplicas === volume.replicas 
                ? 'bg-green-100 text-green-800' 
                : volume.activeReplicas > 0
                ? 'bg-yellow-100 text-yellow-800'
                : 'bg-red-100 text-red-800'
            }`}
          >
            {volume.activeReplicas}/{volume.replicas} Active Replicas
          </span>
          
          {volume.localNVMe && (
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
          
          {volume.replicaStatuses.some(r => r.status === 'failed') && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-red-100 text-red-800 flex items-center gap-1">
              <AlertTriangle className="w-4 h-4" />
              {volume.replicaStatuses.filter(r => r.status === 'failed').length} Failed Replica(s)
            </span>
          )}
        </div>
        
        {volume.replicaStatuses.some(r => r.status === 'rebuilding') && (
          <div className="mt-6 p-4 bg-orange-50 rounded-lg border border-orange-200">
            <h5 className="font-medium text-orange-800 mb-2 flex items-center gap-2">
              <Settings className="w-5 h-5 animate-spin" />
              Active Rebuild Operations
            </h5>
            <div className="space-y-2 text-sm">
              {volume.replicaStatuses
                .filter(r => r.status === 'rebuilding')
                .map((replica, index) => (
                  <div key={index} className="text-left bg-white p-3 rounded border">
                    <div className="flex justify-between items-center mb-2">
                      <span className="font-medium">
                        {replica.isNewReplica ? `New replica on ${replica.node}` : `Rebuilding ${replica.node}`}
                      </span>
                      <span className="text-orange-600 font-medium">
                        {replica.rebuildProgress}%
                      </span>
                    </div>
                    <div className="w-full bg-gray-200 rounded-full h-2 mb-2">
                      <div 
                        className="bg-orange-500 h-2 rounded-full transition-all duration-300" 
                        style={{ width: `${replica.rebuildProgress}%` }}
                      />
                    </div>
                    <div className="text-xs text-gray-600">
                      {replica.isNewReplica 
                        ? `Replacing failed replica with new instance on ${replica.node}`
                        : `Synchronizing data to restore replica health`
                      }
                    </div>
                    {replica.nvmfTarget && (
                      <div className="text-xs text-gray-500 mt-1">
                        NVMe-oF Target: {replica.nvmfTarget.targetIP}:{replica.nvmfTarget.targetPort}
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
}) => {
  const [expandedDisks, setExpandedDisks] = useState(new Set());
  
  const toggleDiskExpansion = (diskId) => {
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
              <p className="text-lg font-bold">{nodeVolumes.filter(v => v.localNVMe).length}</p>
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
            NVMe Disks on {node}
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
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Status</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Brought Online</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Volumes</th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200">
              {nodeDisks.map((disk) => (
                <React.Fragment key={disk.id}>
                  <tr className="hover:bg-gray-50">
                    <td className="px-4 py-4">
                      <div>
                        <div className="text-sm font-medium text-gray-900">{disk.id}</div>
                        <div className="text-xs text-gray-500">{disk.pciAddr}</div>
                      </div>
                    </td>
                    <td className="px-4 py-4 text-sm text-gray-700">{disk.model}</td>
                    <td className="px-4 py-4">
                      <div className="text-sm">
                        <div className="font-medium">{disk.capacityGB}GB</div>
                        <div className="text-xs text-gray-500">{disk.freeSpace}GB free</div>
                      </div>
                    </td>
                    <td className="px-4 py-4">
                      <div className="text-sm">
                        <div className="flex items-center gap-2 mb-1">
                          <div className="w-20 bg-gray-200 rounded-full h-2">
                            <div 
                              className="bg-blue-500 h-2 rounded-full" 
                              style={{ width: `${(disk.allocatedSpace / disk.capacityGB) * 100}%` }}
                            />
                          </div>
                          <span className="text-xs text-gray-600">
                            {Math.round((disk.allocatedSpace / disk.capacityGB) * 100)}%
                          </span>
                        </div>
                        <div className="text-xs text-gray-500">
                          {disk.allocatedSpace}GB / {disk.capacityGB}GB used
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
                        <div>
                          <span className={`inline-flex px-2 py-1 text-xs rounded-full ${
                            disk.blobstoreInitialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                          }`}>
                            {disk.blobstoreInitialized ? 'Formatted' : 'Unformatted'}
                          </span>
                        </div>
                      </div>
                    </td>
                    <td className="px-4 py-4 text-sm text-gray-700">
                      <div>
                        <div className="font-medium">
                          {new Date(disk.broughtOnline).toLocaleDateString()}
                        </div>
                        <div className="text-xs text-gray-500">
                          {new Date(disk.broughtOnline).toLocaleTimeString()}
                        </div>
                        <div className="text-xs text-gray-400 mt-1">
                          {Math.floor((Date.now() - new Date(disk.broughtOnline)) / (1000 * 60 * 60 * 24))} days ago
                        </div>
                      </div>
                    </td>
                    <td className="px-4 py-4">
                      <div className="flex items-center gap-2">
                        <span className="text-sm text-gray-600">
                          {disk.provisionedVolumes.length} volumes
                        </span>
                        {disk.provisionedVolumes.length > 0 && (
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
                  
                  {expandedDisks.has(disk.id) && disk.provisionedVolumes.length > 0 && (
                    <tr>
                      <td colSpan="7" className="px-4 py-2 bg-gray-50">
                        <div className="space-y-3">
                          <h5 className="text-sm font-medium text-gray-700 flex items-center gap-2">
                            <Database className="w-4 h-4" />
                            Provisioned Volumes on {disk.id}
                          </h5>
                          <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                            {disk.provisionedVolumes.map((volume, idx) => (
                              <div key={idx} className="p-3 bg-white rounded border border-gray-200 shadow-sm">
                                <div className="flex items-center justify-between mb-2">
                                  <span className="font-medium text-gray-900">{volume.volumeName}</span>
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
                                    volume.replicaType === 'Local NVMe' 
                                      ? 'bg-blue-100 text-blue-700' 
                                      : 'bg-purple-100 text-purple-700'
                                  }`}>
                                    {volume.replicaType}
                                  </span>
                                  <span>
                                    {new Date(volume.provisionedAt).toLocaleDateString()}
                                  </span>
                                </div>
                                
                                <div className="text-xs text-gray-400">
                                  Volume ID: {volume.volumeId}
                                </div>
                                <div className="text-xs text-gray-400">
                                  Provisioned {Math.floor((Date.now() - new Date(volume.provisionedAt)) / (1000 * 60 * 60 * 24))} days ago
                                </div>
                                
                                {volume.replicaType === 'Local NVMe' && (
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
const Dashboard = ({ onLogout }) => {
  const [data, setData] = useState({ volumes: [], disks: [], nodes: [] });
  const [loading, setLoading] = useState(true);
  const [activeTab, setActiveTab] = useState('overview');
  const [autoRefresh, setAutoRefresh] = useState(true);

  useEffect(() => {
    const fetchData = () => {
      setLoading(true);
      setTimeout(() => {
        setData(generateMockData());
        setLoading(false);
      }, 1000);
    };

    fetchData();
    
    if (autoRefresh) {
      const interval = setInterval(fetchData, 30000);
      return () => clearInterval(interval);
    }
  }, [autoRefresh]);

  const handleRefresh = () => {
    setData(generateMockData());
  };

  const handleLogout = () => {
    authService.logout();
    onLogout();
  };

  const faultedVolumes = data.volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
  const rebuildingVolumes = data.volumes.filter(v => v.state === 'Rebuilding');
  const localNVMeVolumes = data.volumes.filter(v => v.localNVMe);
  const healthyDisks = data.disks.filter(d => d.healthy).length;
  const formattedDisks = data.disks.filter(d => d.blobstoreInitialized).length;

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
                  onChange={(e) => setAutoRefresh(e.target.checked)}
                  className="rounded"
                />
                Auto-refresh
              </label>
              
              <button
                onClick={handleRefresh}
                className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md"
              >
                <RefreshCw className="w-5 h-5" />
              </button>
              
              <button
                onClick={handleLogout}
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
                <p className="text-3xl font-bold text-gray-900">{data.volumes.length}</p>
                <p className="text-gray-600">Total Volumes</p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <AlertTriangle className="w-10 h-10 text-red-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{faultedVolumes.length}</p>
                <p className="text-gray-600">Faulted Volumes</p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <Settings className="w-10 h-10 text-orange-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{rebuildingVolumes.length}</p>
                <p className="text-gray-600">Rebuilding</p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center">
              <Zap className="w-10 h-10 text-green-600 mr-4" />
              <div>
                <p className="text-3xl font-bold text-gray-900">{localNVMeVolumes.length}</p>
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
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.activeReplicas}/{volume.replicas}</td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          {volume.localNVMe ? (
                            <CheckCircle className="w-5 h-5 text-green-500" />
                          ) : (
                            <X className="w-5 h-5 text-gray-400" />
                          )}
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          {volume.rebuildProgress ? (
                            <div className="flex items-center gap-2">
                              <div className="w-20 bg-gray-200 rounded-full h-2">
                                <div 
                                  className="bg-blue-600 h-2 rounded-full" 
                                  style={{ width: `${volume.rebuildProgress}%` }}
                                />
                              </div>
                              <span className="text-sm text-gray-600">{volume.rebuildProgress}%</span>
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
                    <p className="text-3xl font-bold text-blue-600">{data.disks.length}</p>
                  </div>
                  <div className="bg-gray-50 rounded-lg p-4">
                    <h3 className="text-lg font-semibold">Healthy Disks</h3>
                    <p className="text-3xl font-bold text-green-600">{healthyDisks}</p>
                  </div>
                  <div className="bg-gray-50 rounded-lg p-4">
                    <h3 className="text-lg font-semibold">Formatted Disks</h3>
                    <p className="text-3xl font-bold text-blue-600">{formattedDisks}</p>
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
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Formatted</th>
                        <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Performance</th>
                      </tr>
                    </thead>
                    <tbody className="bg-white divide-y divide-gray-200">
                      {data.disks.map((disk) => (
                        <tr key={disk.id} className="hover:bg-gray-50">
                          <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{disk.id}</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.node}</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.model}</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.capacityGB}GB</td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.freeSpace}GB</td>
                          <td className="px-6 py-4 whitespace-nowrap">
                            <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                              disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                            }`}>
                              {disk.healthy ? 'Healthy' : 'Unhealthy'}
                            </span>
                          </td>
                          <td className="px-6 py-4 whitespace-nowrap">
                            <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                              disk.blobstoreInitialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                            }`}>
                              {disk.blobstoreInitialized ? 'Yes' : 'No'}
                            </span>
                          </td>
                          <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                            <div>
                              <div>R: {disk.readIOPS.toLocaleString()} IOPS</div>
                              <div>W: {disk.writeIOPS.toLocaleString()} IOPS</div>
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
                  const totalCapacity = nodeDisks.reduce((sum, disk) => sum + disk.capacityGB, 0);
                  const totalAllocated = nodeDisks.reduce((sum, disk) => sum + disk.allocatedSpace, 0);
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

// Main App Component (at the very end)
const App = () => {
  const [isAuthenticated, setIsAuthenticated] = useState(authService.isAuthenticated());
  
  const handleLogin = () => {
    setIsAuthenticated(true);
  };
  
  const handleLogout = () => {
    setIsAuthenticated(false);
  };
  
  if (!isAuthenticated) {
    return <LoginPage onLogin={handleLogin} />;
  }
  
  return <Dashboard onLogout={handleLogout} />;
};

export default App;
