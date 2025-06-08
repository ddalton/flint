import React from 'react';
import { 
  GitBranch, ChevronDown, ChevronRight, Database, Server, Layers 
} from 'lucide-react';
import type { SnapshotTreeNode } from './types';

interface SnapshotsTreeViewProps {
  snapshotTree: Record<string, SnapshotTreeNode>;
  expandedVolumes: Set<string>;
  onToggleVolumeExpansion: (volumeId: string) => void;
  formatSize: (bytes: number) => string;
  formatTime: (timeString: string) => string;
  getSnapshotTypeIcon: (type: string) => React.ReactNode;
}

export const SnapshotsTreeView: React.FC<SnapshotsTreeViewProps> = ({
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
        <p className="text-gray-500">Create some snapshots to see the hierarchy.</p>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {Object.entries(snapshotTree).map(([volumeId, volumeData]) => (
        <div key={volumeId} className="bg-white border border-gray-200 rounded-lg shadow-sm">
          {/* Volume Header */}
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
                    Volume ID: {volumeData.volume_id} • Size: {formatSize(volumeData.volume_size)}
                  </p>
                </div>
              </div>
              <div className="flex items-center gap-4">
                <span className="text-sm text-gray-600">
                  {volumeData.snapshots.length} snapshot{volumeData.snapshots.length !== 1 ? 's' : ''}
                </span>
              </div>
            </div>
          </div>

          {/* Snapshots for this volume */}
          {expandedVolumes.has(volumeId) && (
            <div className="p-4">
              {volumeData.snapshots.length === 0 ? (
                <p className="text-gray-500 text-center py-8">No snapshots for this volume</p>
              ) : (
                <div className="space-y-4">
                  {volumeData.snapshots.map((snapshot) => (
                    <div key={snapshot.snapshot_id} 
                         className="border-l-4 border-blue-500 pl-4 bg-gray-50 p-4 rounded-r-lg">
                      <div className="flex items-center justify-between mb-3">
                        <div className="flex items-center gap-3">
                          {getSnapshotTypeIcon(snapshot.snapshot_type)}
                          <div>
                            <h4 className="font-medium text-gray-900">
                              {snapshot.snapshot_id}
                            </h4>
                            <p className="text-sm text-gray-600">
                              {formatTime(snapshot.creation_time)}
                            </p>
                          </div>
                        </div>
                        <div className={`px-3 py-1 rounded-full text-xs font-medium ${
                          snapshot.ready_to_use 
                            ? 'bg-green-100 text-green-800' 
                            : 'bg-yellow-100 text-yellow-800'
                        }`}>
                          {snapshot.ready_to_use ? 'Ready' : 'Creating'}
                        </div>
                      </div>

                      {/* Replica Snapshots */}
                      <div className="mt-3">
                        <h5 className="text-sm font-medium text-gray-700 mb-2 flex items-center gap-2">
                          <Layers className="w-4 h-4" />
                          Replica Snapshots ({snapshot.replica_snapshots.length})
                        </h5>
                        <div className="grid grid-cols-1 md:grid-cols-3 gap-2">
                          {snapshot.replica_snapshots.map((replica, index) => (
                            <div key={`${replica.node}-${replica.bdev_name}`}
                                 className="bg-white border border-gray-200 rounded p-3">
                              <div className="flex items-center gap-2 mb-2">
                                <Server className="w-3 h-3 text-gray-500" />
                                <span className="text-sm font-medium">{replica.node}</span>
                              </div>
                              <div className="text-xs space-y-1">
                                <div>
                                  <span className="text-gray-600">Bdev:</span>
                                  <div className="font-mono text-gray-800">{replica.bdev_name}</div>
                                </div>
                                <div>
                                  <span className="text-gray-600">Disk:</span>
                                  <span className="ml-1 text-gray-800">{replica.disk}</span>
                                </div>
                                <div>
                                  <span className="text-gray-600">Source:</span>
                                  <div className="font-mono text-gray-600 text-xs">{replica.source_bdev}</div>
                                </div>
                              </div>
                            </div>
                          ))}
                        </div>
                      </div>

                      {/* Snapshot Metadata */}
                      <div className="mt-3 p-3 bg-blue-50 border border-blue-200 rounded-lg">
                        <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-xs">
                          <div>
                            <span className="text-blue-700 font-medium">Size:</span>
                            <div className="text-blue-900">{formatSize(snapshot.size_bytes)}</div>
                          </div>
                          <div>
                            <span className="text-blue-700 font-medium">Type:</span>
                            <div className="text-blue-900">{snapshot.snapshot_type}</div>
                          </div>
                          <div>
                            <span className="text-blue-700 font-medium">Replicas:</span>
                            <div className="text-blue-900">{snapshot.replica_snapshots.length}</div>
                          </div>
                          <div>
                            <span className="text-blue-700 font-medium">Status:</span>
                            <div className="text-blue-900">{snapshot.ready_to_use ? 'Ready' : 'Creating'}</div>
                          </div>
                        </div>
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      ))}
    </div>
  );
};