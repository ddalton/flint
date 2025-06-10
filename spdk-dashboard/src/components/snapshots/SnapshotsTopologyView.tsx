import React, { useMemo, useState } from 'react';
import { GitBranch, Database, Search, HardDrive, Clock, Server } from 'lucide-react';
import type { SnapshotDetails, ReplicaBdevDetails } from './types';

// New, self-contained SnapshotBlock component with its own tooltip logic
const SnapshotBlock = ({ snapshot, replica, timeRange, formatSize }: {
  snapshot: SnapshotDetails;
  replica: ReplicaBdevDetails;
  timeRange: { min: number; max: number; span: number };
  formatSize: (bytes: number) => string;
}) => {
  const [isHovered, setIsHovered] = useState(false);

  const snapshotTime = new Date(snapshot.creation_time).getTime();
  const leftPosition = timeRange.span > 0
    ? ((snapshotTime - timeRange.min) / timeRange.span) * 100
    : 50;

  const replicaInfo = snapshot.replica_bdev_details.find(r => r.name === replica.name);
  const size = replicaInfo?.storage_info?.consumed_bytes || 0;
  const width = Math.max(2, (size / 1e10) * 10); // Simple scaling for width

  return (
    <div
      className="absolute h-full flex items-center"
      style={{
        left: `${leftPosition}%`,
        transform: 'translateX(-50%)',
      }}
      onMouseEnter={() => setIsHovered(true)}
      onMouseLeave={() => setIsHovered(false)}
    >
      {/* Tooltip rendered within the component */}
      {isHovered && (
        <div
          className="absolute z-10 p-3 bg-white border border-gray-300 rounded-lg shadow-lg text-sm w-max"
          style={{
            bottom: '100%',
            left: '50%',
            transform: 'translateX(-50%)',
            marginBottom: '8px',
            pointerEvents: 'none', // Prevent the tooltip from interfering with mouse events
          }}
        >
          <p className="font-bold text-gray-800 mb-2">{`Replica: ${replica.name}`}</p>
          <div className="space-y-1">
            <div className="flex items-center gap-2">
              <Clock className="w-4 h-4 text-gray-500" />
              <span>{new Date(snapshot.creation_time).toLocaleString()}</span>
            </div>
            <div className="flex items-center gap-2">
              <HardDrive className="w-4 h-4 text-gray-500" />
              <span>{formatSize(replica.storage_info?.consumed_bytes || 0)}</span>
            </div>
            <div className="flex items-center gap-2">
              <Server className="w-4 h-4 text-gray-500" />
              <span>{`Node: ${replica.node}`}</span>
            </div>
          </div>
          <p className="text-xs text-gray-500 mt-2 pt-2 border-t">{`Snapshot ID: ${snapshot.snapshot_id}`}</p>
        </div>
      )}
      <div
        className="h-full bg-blue-500 rounded hover:bg-blue-700 cursor-pointer"
        style={{ width: `${width}px` }}
      />
    </div>
  );
};


interface SnapshotsTopologyViewProps {
  snapshots: SnapshotDetails[];
  formatSize: (bytes: number) => string;
  selectedVolume: string;
  onVolumeChange: (volumeId: string) => void;
  availableVolumes: string[];
}

export const SnapshotsTopologyView: React.FC<SnapshotsTopologyViewProps> = ({
  snapshots,
  formatSize,
  selectedVolume,
  onVolumeChange,
  availableVolumes,
}) => {
  const { replicasData, timeRange } = useMemo(() => {
    const data: Record<string, { replica: ReplicaBdevDetails, snapshots: SnapshotDetails[] }> = {};
    let minTime = Infinity;
    let maxTime = -Infinity;

    if (selectedVolume !== 'all' && availableVolumes.includes(selectedVolume)) {
      const volumeSnapshots = snapshots
        .filter(s => s.source_volume_id === selectedVolume)
        .sort((a, b) => new Date(a.creation_time).getTime() - new Date(b.creation_time).getTime());

      if (volumeSnapshots.length > 0) {
        minTime = new Date(volumeSnapshots[0].creation_time).getTime();
        maxTime = new Date(volumeSnapshots[volumeSnapshots.length - 1].creation_time).getTime();
      }

      volumeSnapshots.forEach(snapshot => {
        snapshot.replica_bdev_details.forEach((replica: ReplicaBdevDetails) => {
          if (!data[replica.name]) {
            data[replica.name] = { replica, snapshots: [] };
          }
          data[replica.name].snapshots.push(snapshot);
        });
      });
    }

    return {
      replicasData: Object.values(data),
      timeRange: { min: minTime, max: maxTime, span: maxTime - minTime },
    };
  }, [snapshots, selectedVolume, availableVolumes]);

  const isValidVolume = availableVolumes.includes(selectedVolume);

  return (
    <div className="space-y-6">
      <div className="bg-white border border-gray-200 rounded-lg shadow-sm p-6">
        <div className="flex items-center justify-between mb-4">
          <h3 className="text-lg font-semibold text-gray-900 flex items-center gap-2">
            <GitBranch className="w-5 h-5 text-blue-600" />
            Replica Snapshot Timeline
          </h3>
          <div className="relative flex items-center gap-2">
            <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
            <input
              id="volume-search"
              type="text"
              list="volume-list"
              value={selectedVolume === 'all' ? '' : selectedVolume}
              onChange={(e) => onVolumeChange(e.target.value === '' ? 'all' : e.target.value)}
              placeholder="Search for a volume..."
              className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
            />
            <datalist id="volume-list">
              {availableVolumes.map(volume => (
                <option key={volume} value={volume} />
              ))}
            </datalist>
          </div>
        </div>

        {!isValidVolume ? (
          <div className="text-center py-12">
            <Database className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <h3 className="text-lg font-medium text-gray-900 mb-2">
              {selectedVolume === 'all' ? "Please Select a Volume" : "Volume Not Found"}
            </h3>
            <p className="text-gray-500">
              {selectedVolume === 'all'
                ? "Start typing in the search box to find and select a volume."
                : `No volume matching "${selectedVolume}" was found. Please select one from the list.`}
            </p>
          </div>
        ) : replicasData.length === 0 ? (
          <div className="text-center py-12">
            <GitBranch className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <h3 className="text-lg font-medium text-gray-900 mb-2">No Replica Snapshot Data Available</h3>
            <p className="text-gray-500">The selected volume does not have any replica snapshots to display.</p>
          </div>
        ) : (
          <div className="space-y-4">
            {replicasData.map(({ replica, snapshots: replicaSnapshots }) => (
              <div key={replica.name} className="flex items-center gap-4">
                <div className="w-48 text-sm font-medium text-gray-700 truncate" title={replica.name}>
                  {replica.name}
                </div>
                <div className="flex-1 bg-gray-200 rounded h-6 relative">
                  {replicaSnapshots.map(snapshot => (
                    <SnapshotBlock
                      key={snapshot.snapshot_id}
                      snapshot={snapshot}
                      replica={replica}
                      timeRange={timeRange}
                      formatSize={formatSize}
                    />
                  ))}
                </div>
              </div>
            ))}
            {timeRange.span > 0 && (
                <div className="flex justify-between text-xs text-gray-500 mt-2 pl-52">
                    <span>{new Date(timeRange.min).toLocaleString()}</span>
                    <span>{new Date(timeRange.max).toLocaleString()}</span>
                </div>
            )}
          </div>
        )}
      </div>
      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <Database className="w-6 h-6 text-blue-600 mt-1 flex-shrink-0" />
          <div>
            <h4 className="font-medium text-blue-900 mb-2">About the Topology View</h4>
            <div className="text-sm text-blue-800 space-y-2">
              <p>
                This view provides a chronological representation of replica snapshots for a single volume. It helps visualize the creation of snapshots across different replicas over time. Each horizontal line represents a unique replica, showing all of its snapshots.
              </p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
