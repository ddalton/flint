import React, { useState, useMemo } from 'react';
import { 
  GitBranch, ChevronDown, ChevronRight, Database, Server, Layers, 
  HardDrive, BarChart3, TrendingUp, AlertCircle, Info, Zap
} from 'lucide-react';
import type { SnapshotTreeNode, SnapshotChainItem, VolumeStorageAnalytics } from './types';

interface EnhancedSnapshotsTreeViewProps {
  snapshotTree: Record<string, SnapshotTreeNode>;
  expandedVolumes: Set<string>;
  onToggleVolumeExpansion: (volumeId: string) => void;
  formatSize: (bytes: number) => string;
  formatTime: (timeString: string) => string;
  getSnapshotTypeIcon: (type: string) => React.ReactNode;
}

const SnapshotChainItem: React.FC<{
  item: SnapshotChainItem;
  depth: number;
  formatSize: (bytes: number) => string;
  isLast: boolean;
  parentLines: boolean[];
}> = ({ item, depth, formatSize, isLast, parentLines }) => {
  const [isExpanded, setIsExpanded] = useState(depth < 2); // Auto-expand first 2 levels
  
  const hasChildren = item.children.length > 0;
  const storageInfo = item.storage_info;
  const isActiveVolume = item.is_active_volume;
  
  const getStorageEfficiency = () => {
    if (!storageInfo) return 'N/A';
    const efficiency = (storageInfo.consumed_bytes / (storageInfo.cluster_size * storageInfo.allocated_clusters)) * 100;
    return `${efficiency.toFixed(1)}%`;
  };

  return (
    <div className="relative">
      {/* Connection Lines */}
      <div className="flex items-center">
        {/* Parent connection lines */}
        {parentLines.map((showLine, index) => (
          <div key={index} className="w-6 flex justify-center">
            {showLine && <div className="w-px h-8 bg-gray-300"></div>}
          </div>
        ))}
        
        {/* Current level connection */}
        {depth > 0 && (
          <div className="w-6 flex items-center justify-center">
            <div className={`w-px bg-gray-300 ${isLast ? 'h-4' : 'h-8'}`}></div>
            <div className="w-4 h-px bg-gray-300"></div>
          </div>
        )}
        
        {/* Expand/Collapse Button */}
        {hasChildren && (
          <button
            onClick={() => setIsExpanded(!isExpanded)}
            className="w-5 h-5 flex items-center justify-center rounded bg-gray-100 hover:bg-gray-200 mr-2"
          >
            {isExpanded ? (
              <ChevronDown className="w-3 h-3 text-gray-600" />
            ) : (
              <ChevronRight className="w-3 h-3 text-gray-600" />
            )}
          </button>
        )}
        
        {/* Node Content */}
        <div className={`flex-1 p-3 rounded-lg border-l-4 ${
          isActiveVolume 
            ? 'border-blue-500 bg-blue-50' 
            : 'border-green-500 bg-green-50'
        }`}>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              {isActiveVolume ? (
                <Database className="w-4 h-4 text-blue-600" />
              ) : (
                <Layers className="w-4 h-4 text-green-600" />
              )}
              <div>
                <span className="font-medium">
                  {item.snapshot_id || item.bdev_name}
                </span>
                {isActiveVolume && (
                  <span className="ml-2 px-2 py-1 bg-blue-100 text-blue-800 text-xs rounded-full">
                    Active Volume
                  </span>
                )}
                <div className="text-xs text-gray-600 mt-1">
                  Bdev: {item.bdev_name}
                </div>
              </div>
            </div>
            
            {/* Storage Information */}
            {storageInfo && (
              <div className="text-right">
                <div className="text-sm font-medium">
                  {formatSize(storageInfo.consumed_bytes)}
                </div>
                <div className="text-xs text-gray-500">
                  {storageInfo.allocated_clusters} clusters
                </div>
                <div className="text-xs text-gray-500">
                  Efficiency: {getStorageEfficiency()}
                </div>
              </div>
            )}
          </div>
          
          {/* Additional Details */}
          {storageInfo && (
            <div className="mt-2 p-2 bg-white rounded border text-xs">
              <div className="grid grid-cols-3 gap-2">
                <div>
                  <span className="text-gray-600">Cluster Size:</span>
                  <div className="font-mono">{formatSize(storageInfo.cluster_size)}</div>
                </div>
                <div>
                  <span className="text-gray-600">Allocated:</span>
                  <div className="font-mono">{storageInfo.allocated_clusters}</div>
                </div>
                <div>
                  <span className="text-gray-600">Consumed:</span>
                  <div className="font-mono">{formatSize(storageInfo.consumed_bytes)}</div>
                </div>
              </div>
            </div>
          )}
        </div>
      </div>
      
      {/* Children */}
      {hasChildren && isExpanded && (
        <div className="ml-2">
          {item.children.map((child, index) => (
            <SnapshotChainItem
              key={child.bdev_name}
              item={child}
              depth={depth + 1}
              formatSize={formatSize}
              isLast={index === item.children.length - 1}
              parentLines={[...parentLines, !isLast]}
            />
          ))}
        </div>
      )}
    </div>
  );
};

const VolumeStorageAnalytics: React.FC<{
  analytics: VolumeStorageAnalytics;
  formatSize: (bytes: number) => string;
}> = ({ analytics, formatSize }) => {
  const efficiency = analytics.snapshot_efficiency_ratio;
  const getEfficiencyColor = () => {
    if (efficiency < 0.1) return 'text-green-600';
    if (efficiency < 0.3) return 'text-yellow-600';
    return 'text-red-600';
  };

  const getEfficiencyIcon = () => {
    if (efficiency < 0.1) return <TrendingUp className="w-4 h-4 text-green-600" />;
    if (efficiency < 0.3) return <BarChart3 className="w-4 h-4 text-yellow-600" />;
    return <AlertCircle className="w-4 h-4 text-red-600" />;
  };

  return (
    <div className="bg-gray-50 rounded-lg p-4 mb-4">
      <h4 className="font-medium text-gray-800 mb-3 flex items-center gap-2">
        <HardDrive className="w-4 h-4" />
        Storage Analytics
      </h4>
      
      {/* Storage Breakdown */}
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-4">
        <div className="text-center">
          <div className="text-lg font-bold text-blue-600">
            {formatSize(analytics.total_volume_size)}
          </div>
          <div className="text-xs text-gray-600">Logical Size</div>
        </div>
        <div className="text-center">
          <div className="text-lg font-bold text-green-600">
            {formatSize(analytics.actual_data_size)}
          </div>
          <div className="text-xs text-gray-600">Actual Data</div>
        </div>
        <div className="text-center">
          <div className="text-lg font-bold text-orange-600">
            {formatSize(analytics.total_snapshot_overhead)}
          </div>
          <div className="text-xs text-gray-600">Snapshot Overhead</div>
        </div>
        <div className="text-center">
          <div className={`text-lg font-bold ${getEfficiencyColor()}`}>
            {(efficiency * 100).toFixed(1)}%
          </div>
          <div className="text-xs text-gray-600">Overhead Ratio</div>
        </div>
      </div>
      
      {/* Storage Efficiency Indicator */}
      <div className="mb-4">
        <div className="flex items-center gap-2 mb-2">
          {getEfficiencyIcon()}
          <span className="text-sm font-medium">Storage Efficiency</span>
        </div>
        <div className="w-full bg-gray-200 rounded-full h-3">
          <div 
            className={`h-3 rounded-full ${
              efficiency < 0.1 ? 'bg-green-500' :
              efficiency < 0.3 ? 'bg-yellow-500' : 'bg-red-500'
            }`}
            style={{ width: `${Math.min(efficiency * 100, 100)}%` }}
          />
        </div>
        <div className="text-xs text-gray-600 mt-1">
          Lower is better - showing snapshot overhead as % of logical volume size
        </div>
      </div>
      
      {/* Detailed Breakdown */}
      <div className="grid grid-cols-2 gap-4 text-sm">
        <div>
          <div className="flex justify-between">
            <span>Active Volume:</span>
            <span className="font-mono">{formatSize(analytics.storage_breakdown.active_volume_consumption)}</span>
          </div>
          <div className="flex justify-between">
            <span>Snapshots:</span>
            <span className="font-mono">{formatSize(analytics.storage_breakdown.snapshot_consumption)}</span>
          </div>
        </div>
        <div>
          <div className="flex justify-between">
            <span>Metadata:</span>
            <span className="font-mono">{formatSize(analytics.storage_breakdown.metadata_overhead)}</span>
          </div>
          <div className="flex justify-between">
            <span>Free Space:</span>
            <span className="font-mono">{formatSize(analytics.storage_breakdown.free_space_in_volume)}</span>
          </div>
        </div>
      </div>
      
      {/* Recommendations */}
      {analytics.recommendations.length > 0 && (
        <div className="mt-4 p-3 bg-blue-50 border border-blue-200 rounded">
          <div className="flex items-center gap-2 mb-2">
            <Info className="w-4 h-4 text-blue-600" />
            <span className="text-sm font-medium text-blue-800">Recommendations</span>
          </div>
          <ul className="text-sm text-blue-700 space-y-1">
            {analytics.recommendations.map((rec, index) => (
              <li key={index} className="flex items-start gap-1">
                <span className="text-blue-500 mt-1">•</span>
                <span>{rec}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
};

export const EnhancedSnapshotsTreeView: React.FC<EnhancedSnapshotsTreeViewProps> = ({
  snapshotTree,
  expandedVolumes,
  onToggleVolumeExpansion,
  formatSize,
  formatTime,
  getSnapshotTypeIcon
}) => {
  if (Object.entries(snapshotTree).length === 0) {
    return (
      <div className="text-center py-12">
        <GitBranch className="w-16 h-16 text-gray-400 mx-auto mb-4" />
        <h3 className="text-lg font-medium text-gray-900 mb-2">No snapshot tree available</h3>
        <p className="text-gray-500">Create some snapshots to see the hierarchy and storage relationships.</p>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {Object.entries(snapshotTree).map(([volumeId, volumeData]) => (
        <div key={volumeId} className="bg-white border border-gray-200 rounded-lg shadow-sm">
          {/* Volume Header with Storage Summary */}
          <div 
            className="p-4 border-b border-gray-200 cursor-pointer hover:bg-gray-50"
            onClick={() => onToggleVolumeExpansion(volumeId)}
          >
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-3">
                {expandedVolumes.has(volumeId) ? (
                  <ChevronDown className="w-5 h-5 text-gray-500" />
                ) : (
                  <ChevronRight className="w-5 h-5 text-gray-500" />
                )}
                <Database className="w-6 h-6 text-blue-600" />
                <div>
                  <h3 className="text-lg font-semibold text-gray-900">
                    {volumeData.volume_name}
                  </h3>
                  <p className="text-sm text-gray-600">
                    Volume: {formatSize(volumeData.volume_size)} • 
                    Chain Depth: {volumeData.snapshot_chain.chain_depth}
                  </p>
                </div>
              </div>
              
              {/* Quick Storage Summary */}
              <div className="flex items-center gap-4">
                {volumeData.storage_analytics && (
                  <>
                    <div className="text-right">
                      <div className="text-sm font-medium text-gray-900">
                        Snapshot Overhead: {formatSize(volumeData.storage_analytics.total_snapshot_overhead)}
                      </div>
                      <div className="text-xs text-gray-500">
                        {(volumeData.storage_analytics.snapshot_efficiency_ratio * 100).toFixed(1)}% of logical size
                      </div>
                    </div>
                    <div className="flex items-center">
                      {volumeData.storage_analytics.snapshot_efficiency_ratio < 0.1 ? (
                        <Zap className="w-5 h-5 text-green-500" title="Efficient storage usage" />
                      ) : volumeData.storage_analytics.snapshot_efficiency_ratio < 0.3 ? (
                        <BarChart3 className="w-5 h-5 text-yellow-500" title="Moderate overhead" />
                      ) : (
                        <AlertCircle className="w-5 h-5 text-red-500" title="High storage overhead" />
                      )}
                    </div>
                  </>
                )}
                <span className="text-sm text-gray-600">
                  {volumeData.snapshot_chain.snapshots.length} snapshot{volumeData.snapshot_chain.snapshots.length !== 1 ? 's' : ''}
                </span>
              </div>
            </div>
          </div>

          {/* Expanded Content */}
          {expandedVolumes.has(volumeId) && (
            <div className="p-4">
              {/* Storage Analytics */}
              {volumeData.storage_analytics && (
                <VolumeStorageAnalytics 
                  analytics={volumeData.storage_analytics}
                  formatSize={formatSize}
                />
              )}
              
              {/* Snapshot Chain Error */}
              {volumeData.snapshot_chain.error ? (
                <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
                  <div className="flex items-center gap-2 text-red-800">
                    <AlertCircle className="w-4 h-4" />
                    <span className="font-medium">Chain Trace Error</span>
                  </div>
                  <p className="text-sm text-red-700 mt-1">
                    {volumeData.snapshot_chain.error}
                  </p>
                </div>
              ) : volumeData.snapshot_chain.snapshots.length === 0 ? (
                <p className="text-gray-500 text-center py-8">No snapshots in chain for this volume</p>
              ) : (
                <div>
                  <h4 className="text-sm font-semibold text-gray-700 mb-4 flex items-center gap-2">
                    <GitBranch className="w-4 h-4 text-indigo-600" />
                    Snapshot Chain Hierarchy
                    <span className="text-xs text-gray-500">
                      (Showing storage consumption per snapshot)
                    </span>
                  </h4>
                  
                  {/* Active Volume Info */}
                  <div className="mb-4 p-3 bg-blue-50 border border-blue-200 rounded-lg">
                    <div className="flex items-center gap-2 mb-1">
                      <Database className="w-4 h-4 text-blue-600" />
                      <span className="text-sm font-medium text-blue-800">Active Volume (Head of Chain)</span>
                    </div>
                    <div className="text-xs text-blue-700">
                      <div>Logical Volume Object: {volumeData.snapshot_chain.active_lvol}</div>
                      <div>Total Chain Depth: {volumeData.snapshot_chain.chain_depth} levels</div>
                    </div>
                  </div>
                  
                  {/* Snapshot Chain Tree */}
                  <div className="bg-gray-50 rounded-lg p-4">
                    {volumeData.snapshot_chain.snapshots.map((snapshot, index) => (
                      <SnapshotChainItem
                        key={snapshot.bdev_name}
                        item={snapshot}
                        depth={0}
                        formatSize={formatSize}
                        isLast={index === volumeData.snapshot_chain.snapshots.length - 1}
                        parentLines={[]}
                      />
                    ))}
                  </div>
                </div>
              )}
            </div>
          )}
        </div>
      ))}
    </div>
  );
};