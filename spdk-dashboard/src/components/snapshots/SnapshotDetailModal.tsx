import React from 'react';
import {
  Shield, Info, CheckCircle, Server, X
} from 'lucide-react';
import type { SnapshotDetails } from './types';
import { Button, IconButton } from '../ui/Button';
import { Chip } from '../ui/Chip';

interface SnapshotDetailModalProps {
  snapshot: SnapshotDetails;
  onClose: () => void;
  formatSize: (bytes: number) => string;
  formatTime: (timeString: string) => string;
  getSnapshotTypeIcon: (type: string) => React.ReactNode;
}

export const SnapshotDetailModal: React.FC<SnapshotDetailModalProps> = ({
  snapshot,
  onClose,
  formatSize,
  formatTime,
  getSnapshotTypeIcon
}) => {
  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div className="bg-white rounded-lg max-w-4xl w-full max-h-[90vh] mx-4 flex flex-col">
        {/* Modal Header */}
        <div className="flex items-center justify-between p-6 border-b">
          <div className="flex items-center gap-3">
            {getSnapshotTypeIcon(snapshot.snapshot_type)}
            <div>
              <h2 className="text-section">Snapshot Details</h2>
              <p className="text-gray-600">{snapshot.snapshot_id}</p>
            </div>
          </div>
          <IconButton icon={X} aria-label="Close" onClick={onClose} />
        </div>

        {/* Modal Content */}
        <div className="flex-1 overflow-auto p-6">
          <div className="space-y-6">
            {/* Basic Information */}
            <div className="bg-gray-50 rounded-lg p-4">
              <h3 className="text-section mb-4">Snapshot Information</h3>
              <div className="grid grid-cols-2 gap-4">
                <div>
                  <span className="text-sm font-medium text-gray-600">Snapshot ID:</span>
                  <p className="font-mono text-sm">{snapshot.snapshot_id}</p>
                </div>
                <div>
                  <span className="text-sm font-medium text-gray-600">Source Volume:</span>
                  <p className="font-mono text-sm">{snapshot.source_volume_id}</p>
                </div>
                <div>
                  <span className="text-sm font-medium text-gray-600">Type:</span>
                  <p className="text-sm">{snapshot.snapshot_type}</p>
                </div>
                <div>
                  <span className="text-sm font-medium text-gray-600">Size:</span>
                  <p className="text-sm">{formatSize(snapshot.size_bytes)}</p>
                </div>
                <div>
                  <span className="text-sm font-medium text-gray-600">Created:</span>
                  <p className="text-sm">{formatTime(snapshot.creation_time)}</p>
                </div>
                <div>
                  <span className="text-sm font-medium text-gray-600">Status:</span>
                  <Chip
                    label={snapshot.ready_to_use ? 'Ready' : 'Creating'}
                    chip={snapshot.ready_to_use
                      ? 'bg-healthy-100 text-healthy-800 border-healthy-200'
                      : 'bg-degraded-100 text-degraded-800 border-degraded-200'}
                  />
                </div>
              </div>
            </div>

            {/* Replica Architecture Details */}
            <div>
              <h3 className="text-section mb-4 flex items-center gap-2">
                <Shield className="w-5 h-5 text-indigo-600" />
                Replica Snapshot Architecture
              </h3>
              
              <div className="mb-4 p-4 bg-brand-50 border border-brand-200 rounded-lg">
                <div className="flex items-start gap-3">
                  <Info className="w-5 h-5 text-brand-600 mt-0.5 flex-shrink-0" />
                  <div className="text-sm text-brand-800">
                    <p className="font-medium mb-2">High Availability Design</p>
                    <p>
                      This snapshot maintains the same redundancy as the source volume by creating 
                      individual snapshots on each replica node. This approach eliminates single points 
                      of failure and ensures data recovery is possible even if multiple nodes fail.
                    </p>
                  </div>
                </div>
              </div>

              <div className="space-y-4">
                {snapshot.replica_bdev_details.map((replica, index) => (
                  <div key={`${replica.node}-${replica.name}`} 
                       className="border border-gray-200 rounded-lg p-4">
                    <div className="flex items-center justify-between mb-4">
                      <div className="flex items-center gap-3">
                        {/* raw on purpose: replica-identity palette (1=blue,
                            2=green, 3=purple), a data-series coding, not status */}
                        <div className={`w-8 h-8 rounded-full flex items-center justify-center text-white text-sm font-medium ${
                          index === 0 ? 'bg-blue-600' :
                          index === 1 ? 'bg-green-600' :
                          'bg-purple-600'
                        }`}>
                          {index + 1}
                        </div>
                        <div>
                          <h4 className="font-medium text-gray-900 flex items-center gap-2">
                            <Server className="w-4 h-4 text-gray-600" />
                            {replica.node}
                          </h4>
                          <p className="text-sm text-gray-600">Replica Snapshot {index + 1}</p>
                        </div>
                      </div>
                      <span className="text-xs bg-gray-100 text-gray-800 px-2 py-1 rounded-full">
                        {replica.driver}
                      </span>
                    </div>

                    <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
                      <div>
                        <span className="text-sm font-medium text-gray-600">Snapshot Bdev Name:</span>
                        <div className="mt-1 p-2 bg-gray-100 rounded font-mono text-sm">
                          {replica.name}
                        </div>
                      </div>
                      <div>
                        <span className="text-sm font-medium text-gray-600">Source Bdev:</span>
                        <div className="mt-1 p-2 bg-gray-100 rounded font-mono text-sm">
                          {replica.snapshot_source_bdev || 'N/A'}
                        </div>
                      </div>
                    </div>

                    {replica.aliases.length > 0 && (
                      <div className="mt-4">
                        <span className="text-sm font-medium text-gray-600">Aliases:</span>
                        <div className="mt-2 flex flex-wrap gap-2">
                          {replica.aliases.map((alias, aliasIndex) => (
                            <span key={aliasIndex} 
                                  className="px-2 py-1 bg-gray-200 text-gray-800 text-xs rounded font-mono">
                              {alias}
                            </span>
                          ))}
                        </div>
                      </div>
                    )}
                  </div>
                ))}
              </div>
            </div>

            {/* Recovery Information */}
            <div className="bg-healthy-50 border border-healthy-200 rounded-lg p-4">
              <h4 className="font-medium text-healthy-800 mb-2 flex items-center gap-2">
                <CheckCircle className="w-4 h-4" />
                Recovery Capabilities
              </h4>
              <div className="text-sm text-healthy-700 space-y-2">
                <p>
                  • <strong>Node Failure Tolerance:</strong> Data can be recovered even if {snapshot.replica_bdev_details.length - 1} out of {snapshot.replica_bdev_details.length} nodes fail
                </p>
                <p>
                  • <strong>Consistency:</strong> All replica snapshots were created atomically at the same point in time
                </p>
                <p>
                  • <strong>Independent Recovery:</strong> Each replica snapshot can be restored independently on any compatible node
                </p>
                <p>
                  • <strong>Geographic Distribution:</strong> Replica snapshots can be distributed across different availability zones
                </p>
              </div>
            </div>

            {/* Clone Information */}
            {snapshot.clone_source_snapshot_id && (
              <div className="bg-degraded-50 border border-degraded-200 rounded-lg p-4">
                <h4 className="font-medium text-degraded-800 mb-2">Clone Information</h4>
                <p className="text-sm text-degraded-700">
                  This snapshot was created as a clone from snapshot: <span className="font-mono">{snapshot.clone_source_snapshot_id}</span>
                </p>
              </div>
            )}
          </div>
        </div>

        {/* Modal Footer. No Delete/Clone here: this view is SPDK-level and
            has no VolumeSnapshot CR reference — user snapshots are deleted
            from the timeline view, whose CR-path delete keeps the CR and
            the SPDK content in step. */}
        <div className="flex items-center justify-end gap-3 p-6 border-t">
          <Button onClick={onClose}>Close</Button>
        </div>
      </div>
    </div>
  );
};