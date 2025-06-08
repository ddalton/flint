import React from 'react';
import { 
  Camera, Eye, Server, Shield, Info, CheckCircle 
} from 'lucide-react';
import type { SnapshotDetails } from './types';

interface SnapshotsListViewProps {
  snapshots: SnapshotDetails[];
  onSnapshotSelect: (snapshot: SnapshotDetails) => void;
  formatSize: (bytes: number) => string;
  formatTime: (timeString: string) => string;
  getSnapshotTypeIcon: (type: string) => React.ReactNode;
}

export const SnapshotsListView: React.FC<SnapshotsListViewProps> = ({
  snapshots,
  onSnapshotSelect,
  formatSize,
  formatTime,
  getSnapshotTypeIcon
}) => {
  if (snapshots.length === 0) {
    return (
      <div className="text-center py-12">
        <Camera className="w-16 h-16 text-gray-400 mx-auto mb-4" />
        <h3 className="text-lg font-medium text-gray-900 mb-2">No snapshots found</h3>
        <p className="text-gray-500">
          Try adjusting your filters to see more results or create some snapshots.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      {snapshots.map((snapshot) => (
        <div key={snapshot.snapshot_id} className="bg-white border border-gray-200 rounded-lg shadow-sm">
          {/* Snapshot Header */}
          <div className="p-6 border-b border-gray-200">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-3">
                {getSnapshotTypeIcon(snapshot.snapshot_type)}
                <div>
                  <h3 className="text-lg font-semibold text-gray-900">
                    {snapshot.snapshot_id}
                  </h3>
                  <p className="text-sm text-gray-600">
                    Source: {snapshot.source_volume_id}
                  </p>
                </div>
              </div>
              <div className="flex items-center gap-4">
                <div className="text-right">
                  <div className="text-sm font-medium text-gray-900">
                    {formatSize(snapshot.size_bytes)}
                  </div>
                  <div className="text-xs text-gray-500">
                    {formatTime(snapshot.creation_time)}
                  </div>
                </div>
                <div className={`px-3 py-1 rounded-full text-xs font-medium ${
                  snapshot.ready_to_use 
                    ? 'bg-green-100 text-green-800' 
                    : 'bg-yellow-100 text-yellow-800'
                }`}>
                  {snapshot.ready_to_use ? 'Ready' : 'Creating'}
                </div>
                <button
                  onClick={() => onSnapshotSelect(snapshot)}
                  className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md"
                >
                  <Eye className="w-5 h-5" />
                </button>
              </div>
            </div>
          </div>

          {/* Multi-Replica Architecture Display */}
          <div className="p-6">
            <div className="mb-4">
              <h4 className="text-sm font-semibold text-gray-700 mb-3 flex items-center gap-2">
                <Shield className="w-4 h-4 text-indigo-600" />
                Multi-Replica Snapshot Architecture
                <span className="text-xs text-gray-500">
                  ({snapshot.replica_bdev_details.length} replica snapshots)
                </span>
              </h4>
              <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mb-4">
                <div className="flex items-start gap-2">
                  <Info className="w-4 h-4 text-blue-600 mt-0.5 flex-shrink-0" />
                  <div className="text-sm text-blue-800">
                    <p className="font-medium mb-1">High Availability Snapshot</p>
                    <p>
                      This snapshot was created by taking individual snapshots of each volume replica 
                      across {snapshot.replica_bdev_details.length} nodes, ensuring no single point of failure.
                    </p>
                  </div>
                </div>
              </div>
            </div>

            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
              {snapshot.replica_bdev_details.map((replica, index) => (
                <div key={`${replica.node}-${replica.name}`} 
                     className="border border-gray-200 rounded-lg p-4 bg-gray-50">
                  <div className="flex items-center justify-between mb-3">
                    <div className="flex items-center gap-2">
                      <Server className="w-4 h-4 text-gray-600" />
                      <span className="font-medium text-gray-900">{replica.node}</span>
                    </div>
                    <span className="text-xs bg-indigo-100 text-indigo-800 px-2 py-1 rounded-full">
                      Replica {index + 1}
                    </span>
                  </div>
                  
                  <div className="space-y-2 text-sm">
                    <div>
                      <span className="text-gray-600">Snapshot Bdev:</span>
                      <div className="font-mono text-xs bg-white p-2 rounded border mt-1">
                        {replica.name}
                      </div>
                    </div>
                    
                    <div>
                      <span className="text-gray-600">Source Bdev:</span>
                      <div className="font-mono text-xs text-gray-700 mt-1">
                        {replica.snapshot_source_bdev || 'N/A'}
                      </div>
                    </div>
                    
                    <div>
                      <span className="text-gray-600">Driver:</span>
                      <span className="ml-2 text-xs bg-gray-200 text-gray-800 px-2 py-1 rounded">
                        {replica.driver}
                      </span>
                    </div>
                    
                    {replica.aliases.length > 0 && (
                      <div>
                        <span className="text-gray-600">Aliases:</span>
                        <div className="mt-1 space-y-1">
                          {replica.aliases.map((alias, aliasIndex) => (
                            <div key={aliasIndex} className="font-mono text-xs text-gray-700">
                              {alias}
                            </div>
                          ))}
                        </div>
                      </div>
                    )}
                  </div>
                </div>
              ))}
            </div>

            {/* Consistency Information */}
            <div className="mt-4 p-3 bg-green-50 border border-green-200 rounded-lg">
              <div className="flex items-center gap-2 mb-2">
                <CheckCircle className="w-4 h-4 text-green-600" />
                <span className="text-sm font-medium text-green-800">
                  Consistency Guarantee
                </span>
              </div>
              <p className="text-sm text-green-700">
                All replica snapshots were created atomically at {formatTime(snapshot.creation_time)} 
                ensuring data consistency across all {snapshot.replica_bdev_details.length} replicas.
              </p>
            </div>
          </div>
        </div>
      ))}
    </div>
  );
};