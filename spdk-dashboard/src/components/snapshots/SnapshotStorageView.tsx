import React, { useMemo, useState } from 'react';
import { 
  BarChart3, PieChart, TrendingUp, AlertTriangle, 
  Info, HardDrive, Database, Layers, Search
} from 'lucide-react';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer, PieChart as RechartsPieChart, Cell, Pie } from 'recharts';
import type { SnapshotStorageViewProps } from './types';

interface StorageAnalysisCard {
  volumeId: string;
  volumeName: string;
  logicalSize: number;
  actualDataSize: number;
  snapshotOverhead: number;
  efficiency: number;
  snapshotCount: number;
  recommendations: string[];
}

export const SnapshotStorageView: React.FC<SnapshotStorageViewProps> = ({
  snapshots,
  snapshotTree,
  onSnapshotSelect,
  formatSize
}) => {
  const [sortBy, setSortBy] = useState<'overhead' | 'efficiency' | 'count'>('overhead');
  const [filterThreshold, setFilterThreshold] = useState<'all' | 'high' | 'medium' | 'low'>('all');
  const [searchTerm, setSearchTerm] = useState('');

  // Calculate storage analysis for each volume
  const storageAnalysis = useMemo((): StorageAnalysisCard[] => {
    const volumeMap = new Map<string, StorageAnalysisCard>();

    // Process each volume from the snapshot tree
    Object.entries(snapshotTree).forEach(([volumeId, volumeData]) => {
      const analytics = volumeData.storage_analytics;
      if (!analytics) return;

      const volumeSnapshots = snapshots.filter(s => s.source_volume_id === volumeId);
      
      const card: StorageAnalysisCard = {
        volumeId,
        volumeName: volumeData.volume_name || volumeId,
        logicalSize: volumeData.volume_size || 0,
        actualDataSize: analytics.actual_data_size || 0,
        snapshotOverhead: analytics.total_snapshot_overhead || 0,
        efficiency: analytics.snapshot_efficiency_ratio || 0,
        snapshotCount: volumeSnapshots.length,
        recommendations: analytics.recommendations || []
      };

      volumeMap.set(volumeId, card);
    });

    return Array.from(volumeMap.values());
  }, [snapshots, snapshotTree]);

  // Filter and sort the analysis
  const filteredAnalysis = useMemo(() => {
    let filtered = storageAnalysis;

    // Apply search filter
    if (searchTerm) {
      const searchLower = searchTerm.toLowerCase();
      filtered = filtered.filter(card => 
        card.volumeName.toLowerCase().includes(searchLower) ||
        card.volumeId.toLowerCase().includes(searchLower)
      );
    }

    // Apply efficiency threshold filter
    if (filterThreshold !== 'all') {
      filtered = filtered.filter(card => {
        switch (filterThreshold) {
          case 'high': return card.efficiency > 0.3; // >30% overhead
          case 'medium': return card.efficiency >= 0.1 && card.efficiency <= 0.3;
          case 'low': return card.efficiency < 0.1; // <10% overhead
          default: return true;
        }
      });
    }

    // Sort the results
    return filtered.sort((a, b) => {
      switch (sortBy) {
        case 'overhead':
          return b.snapshotOverhead - a.snapshotOverhead;
        case 'efficiency':
          return b.efficiency - a.efficiency; // Worst efficiency first
        case 'count':
          return b.snapshotCount - a.snapshotCount;
        default:
          return 0;
      }
    });
  }, [storageAnalysis, sortBy, filterThreshold, searchTerm]);

  // Calculate aggregate statistics
  const aggregateStats = useMemo(() => {
    const totalLogical = storageAnalysis.reduce((sum, card) => sum + (card.logicalSize || 0), 0);
    const totalActual = storageAnalysis.reduce((sum, card) => sum + (card.actualDataSize || 0), 0);
    const totalOverhead = storageAnalysis.reduce((sum, card) => sum + (card.snapshotOverhead || 0), 0);
    const totalSnapshots = storageAnalysis.reduce((sum, card) => sum + (card.snapshotCount || 0), 0);
    
    return {
      totalLogical,
      totalActual,
      totalOverhead,
      totalSnapshots,
      avgEfficiency: storageAnalysis.length > 0 
        ? storageAnalysis.reduce((sum, card) => sum + (card.efficiency || 0), 0) / storageAnalysis.length 
        : 0,
      inefficientVolumes: storageAnalysis.filter(card => (card.efficiency || 0) > 0.3).length,
      storageWasted: Math.max(0, totalOverhead - (totalLogical - totalActual)) // Overhead beyond data compression, never negative
    };
  }, [storageAnalysis]);

  // Prepare chart data
  const chartData = filteredAnalysis.map(card => ({
    name: card.volumeName,
    'Logical Size': Math.round((card.logicalSize || 0) / (1024 * 1024 * 1024)),
    'Actual Data': Math.round((card.actualDataSize || 0) / (1024 * 1024 * 1024)),
    'Snapshot Overhead': Math.round((card.snapshotOverhead || 0) / (1024 * 1024 * 1024)),
    'Efficiency %': Math.round((card.efficiency || 0) * 100),
    volumeId: card.volumeId
  }));

  const pieData = [
    { name: 'Actual Data', value: aggregateStats.totalActual, color: '#10b981' },
    { name: 'Snapshot Overhead', value: aggregateStats.totalOverhead, color: '#f59e0b' },
    { name: 'Free/Unallocated', value: Math.max(0, aggregateStats.totalLogical - aggregateStats.totalActual - aggregateStats.totalOverhead), color: '#6b7280' }
  ];

  const getEfficiencyColor = (efficiency: number) => {
    if (efficiency < 0.1) return 'text-green-600';
    if (efficiency < 0.3) return 'text-yellow-600';
    return 'text-red-600';
  };

  const getEfficiencyBadge = (efficiency: number) => {
    if (efficiency < 0.1) return 'bg-green-100 text-green-800';
    if (efficiency < 0.3) return 'bg-yellow-100 text-yellow-800';
    return 'bg-red-100 text-red-800';
  };

  return (
    <div className="space-y-6">
      {/* Overall Statistics */}
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-4">
        <div className="bg-white rounded-lg shadow p-6">
          <div className="flex items-center">
            <Database className="w-8 h-8 text-blue-600 mr-3" />
            <div>
              <p className="text-sm font-medium text-gray-600">Total Logical Storage</p>
              <p className="text-2xl font-bold text-gray-900">{formatSize(aggregateStats.totalLogical)}</p>
            </div>
          </div>
        </div>
        
        <div className="bg-white rounded-lg shadow p-6">
          <div className="flex items-center">
            <HardDrive className="w-8 h-8 text-green-600 mr-3" />
            <div>
              <p className="text-sm font-medium text-gray-600">Actual Data Used</p>
              <p className="text-2xl font-bold text-gray-900">{formatSize(aggregateStats.totalActual)}</p>
            </div>
          </div>
        </div>
        
        <div className="bg-white rounded-lg shadow p-6">
          <div className="flex items-center">
            <Layers className="w-8 h-8 text-orange-600 mr-3" />
            <div>
              <p className="text-sm font-medium text-gray-600">Snapshot Overhead</p>
              <p className="text-2xl font-bold text-gray-900">{formatSize(aggregateStats.totalOverhead)}</p>
              <p className="text-xs text-gray-500">
                {aggregateStats.totalLogical > 0 
                  ? ((aggregateStats.totalOverhead / aggregateStats.totalLogical) * 100).toFixed(1)
                  : '0.0'}% of logical
              </p>
            </div>
          </div>
        </div>
        
        <div className="bg-white rounded-lg shadow p-6">
          <div className="flex items-center">
            <BarChart3 className="w-8 h-8 text-purple-600 mr-3" />
            <div>
              <p className="text-sm font-medium text-gray-600">Average Efficiency</p>
              <p className={`text-2xl font-bold ${getEfficiencyColor(aggregateStats.avgEfficiency)}`}>
                {(aggregateStats.avgEfficiency * 100).toFixed(1)}%
              </p>
              <p className="text-xs text-gray-500">overhead ratio</p>
            </div>
          </div>
        </div>
      </div>

      {/* Efficiency Alerts */}
      {aggregateStats.inefficientVolumes > 0 && (
        <div className="bg-red-50 border border-red-200 rounded-lg p-4">
          <div className="flex items-center gap-2 mb-2">
            <AlertTriangle className="w-5 h-5 text-red-600" />
            <h3 className="font-medium text-red-800">Storage Efficiency Alert</h3>
          </div>
          <p className="text-sm text-red-700">
            {aggregateStats.inefficientVolumes} volume{aggregateStats.inefficientVolumes !== 1 ? 's have' : ' has'} high 
            snapshot overhead (&gt;30% of logical size). Consider cleaning up old snapshots or reviewing snapshot retention policies.
          </p>
          {aggregateStats.storageWasted > 0 && (
            <p className="text-sm text-red-700 mt-1">
              Estimated wasted storage: <strong>{formatSize(aggregateStats.storageWasted)}</strong>
            </p>
          )}
        </div>
      )}

      {/* Storage Distribution Charts */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {/* Overall Storage Distribution */}
        <div className="bg-white rounded-lg shadow p-6">
          <h3 className="text-lg font-semibold mb-4 flex items-center gap-2">
            <PieChart className="w-5 h-5 text-blue-600" />
            Overall Storage Distribution
          </h3>
          <ResponsiveContainer width="100%" height={300}>
            <RechartsPieChart>
              <Pie
                data={pieData}
                cx="50%"
                cy="50%"
                outerRadius={80}
                dataKey="value"
                label={({name, value}) => `${name}: ${formatSize(value)}`}
              >
                {pieData.map((entry, index) => (
                  <Cell key={`cell-${index}`} fill={entry.color} />
                ))}
              </Pie>
              <Tooltip formatter={(value) => formatSize(value as number)} />
            </RechartsPieChart>
          </ResponsiveContainer>
          <div className="mt-4 text-sm text-gray-600">
            <p>Shows how total logical storage is distributed between actual data, snapshot overhead, and free space.</p>
          </div>
        </div>

        {/* Volume Storage Comparison */}
        <div className="bg-white rounded-lg shadow p-6">
          <h3 className="text-lg font-semibold mb-4 flex items-center gap-2">
            <BarChart3 className="w-5 h-5 text-green-600" />
            Volume Storage Breakdown
          </h3>
          <ResponsiveContainer width="100%" height={300}>
            <BarChart data={chartData.slice(0, 10)}>
              <CartesianGrid strokeDasharray="3 3" />
              <XAxis 
                dataKey="name" 
                angle={-45}
                textAnchor="end"
                height={100}
                fontSize={12}
              />
              <YAxis label={{ value: 'Size (GB)', angle: -90, position: 'insideLeft' }} />
              <Tooltip />
              <Legend />
              <Bar dataKey="Actual Data" stackId="a" fill="#10b981" />
              <Bar dataKey="Snapshot Overhead" stackId="a" fill="#f59e0b" />
            </BarChart>
          </ResponsiveContainer>
          <div className="mt-4 text-sm text-gray-600">
            <p>Compares actual data usage vs snapshot overhead across volumes. Showing top 10 volumes.</p>
          </div>
        </div>
      </div>

      {/* Controls and Filters */}
      <div className="bg-white rounded-lg shadow p-4">
        <div className="flex items-center justify-between mb-4">
          <h3 className="text-lg font-semibold">Volume Storage Analysis</h3>
          <div className="flex items-center gap-4">
            {/* Search */}
            <div className="relative">
              <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
              <input
                type="text"
                placeholder="Search volumes..."
                value={searchTerm}
                onChange={(e) => setSearchTerm(e.target.value)}
                className="pl-10 pr-4 py-2 border border-gray-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
              />
            </div>

            {/* Efficiency Filter */}
            <select
              value={filterThreshold}
              onChange={(e) => setFilterThreshold(e.target.value as any)}
              className="border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
            >
              <option value="all">All Efficiency Levels</option>
              <option value="high">High Overhead (&gt;30%)</option>
              <option value="medium">Medium Overhead (10-30%)</option>
              <option value="low">Low Overhead (&lt;10%)</option>
            </select>

            {/* Sort */}
            <select
              value={sortBy}
              onChange={(e) => setSortBy(e.target.value as any)}
              className="border border-gray-300 rounded-md px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
            >
              <option value="overhead">Sort by Overhead</option>
              <option value="efficiency">Sort by Efficiency</option>
              <option value="count">Sort by Snapshot Count</option>
            </select>
          </div>
        </div>

        <div className="text-sm text-gray-600 mb-4">
          Showing {filteredAnalysis.length} of {storageAnalysis.length} volumes • 
          Total snapshots: {aggregateStats.totalSnapshots}
        </div>
      </div>

      {/* Detailed Volume Cards */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {filteredAnalysis.map((card) => (
          <div key={card.volumeId} className="bg-white rounded-lg shadow p-6">
            <div className="flex items-center justify-between mb-4">
              <div>
                <h4 className="text-lg font-semibold text-gray-900">{card.volumeName}</h4>
                <p className="text-sm text-gray-600">{card.volumeId}</p>
              </div>
              <div className={`px-3 py-1 rounded-full text-sm font-medium ${getEfficiencyBadge(card.efficiency)}`}>
                {(card.efficiency * 100).toFixed(1)}% overhead
              </div>
            </div>

            {/* Storage Breakdown */}
            <div className="space-y-3 mb-4">
              <div className="flex justify-between text-sm">
                <span className="text-gray-600">Logical Size:</span>
                <span className="font-medium">{formatSize(card.logicalSize)}</span>
              </div>
              <div className="flex justify-between text-sm">
                <span className="text-gray-600">Actual Data:</span>
                <span className="font-medium text-green-600">{formatSize(card.actualDataSize)}</span>
              </div>
              <div className="flex justify-between text-sm">
                <span className="text-gray-600">Snapshot Overhead:</span>
                <span className="font-medium text-orange-600">{formatSize(card.snapshotOverhead)}</span>
              </div>
              <div className="flex justify-between text-sm">
                <span className="text-gray-600">Snapshot Count:</span>
                <span className="font-medium">{card.snapshotCount}</span>
              </div>
            </div>

            {/* Efficiency Bar */}
            <div className="mb-4">
              <div className="flex items-center justify-between text-sm mb-1">
                <span className="text-gray-600">Storage Efficiency</span>
                <span className={`font-medium ${getEfficiencyColor(card.efficiency)}`}>
                  {card.efficiency < 0.1 ? 'Excellent' : 
                   card.efficiency < 0.3 ? 'Good' : 'Needs Attention'}
                </span>
              </div>
              <div className="w-full bg-gray-200 rounded-full h-2">
                <div 
                  className={`h-2 rounded-full ${
                    card.efficiency < 0.1 ? 'bg-green-500' :
                    card.efficiency < 0.3 ? 'bg-yellow-500' : 'bg-red-500'
                  }`}
                  style={{ width: `${Math.min(card.efficiency * 100, 100)}%` }}
                />
              </div>
            </div>

            {/* Recommendations */}
            {card.recommendations.length > 0 && (
              <div className="bg-blue-50 border border-blue-200 rounded p-3">
                <div className="flex items-center gap-2 mb-2">
                  <Info className="w-4 h-4 text-blue-600" />
                  <span className="text-sm font-medium text-blue-800">Recommendations</span>
                </div>
                <ul className="text-sm text-blue-700 space-y-1">
                  {card.recommendations.slice(0, 3).map((rec, index) => (
                    <li key={index} className="flex items-start gap-1">
                      <span className="text-blue-500 mt-1">•</span>
                      <span>{rec}</span>
                    </li>
                  ))}
                </ul>
              </div>
            )}

            {/* View Snapshots Button */}
            <button
              onClick={() => {
                const volumeSnapshots = snapshots.filter(s => s.source_volume_id === card.volumeId);
                if (volumeSnapshots.length > 0) {
                  onSnapshotSelect(volumeSnapshots[0]);
                }
              }}
              className="mt-4 w-full px-4 py-2 bg-blue-600 text-white rounded hover:bg-blue-700 text-sm font-medium"
            >
              View {card.snapshotCount} Snapshot{card.snapshotCount !== 1 ? 's' : ''}
            </button>
          </div>
        ))}
      </div>

      {filteredAnalysis.length === 0 && (
        <div className="text-center py-12">
          <BarChart3 className="w-16 h-16 text-gray-400 mx-auto mb-4" />
          <h3 className="text-lg font-medium text-gray-900 mb-2">No volumes match the current filters</h3>
          <p className="text-gray-500">Try adjusting your search or filter criteria.</p>
        </div>
      )}

      {/* Storage Optimization Tips */}
      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <h3 className="font-medium text-blue-900 mb-3 flex items-center gap-2">
          <TrendingUp className="w-5 h-5" />
          Storage Optimization Tips
        </h3>
        <div className="grid grid-cols-1 md:grid-cols-2 gap-4 text-sm text-blue-800">
          <div>
            <p className="font-medium mb-2">Snapshot Management:</p>
            <ul className="space-y-1">
              <li>• Delete unnecessary old snapshots regularly</li>
              <li>• Use automated snapshot retention policies</li>
              <li>• Consider snapshot consolidation for long chains</li>
            </ul>
          </div>
          <div>
            <p className="font-medium mb-2">Storage Efficiency:</p>
            <ul className="space-y-1">
              <li>• Monitor volumes with &gt;30% snapshot overhead</li>
              <li>• Review snapshot frequency for high-change workloads</li>
              <li>• Consider external backup for long-term retention</li>
            </ul>
          </div>
        </div>
      </div>
    </div>
  );
};
