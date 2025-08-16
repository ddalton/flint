import React, { useState, useEffect } from 'react';
import { Server, HardDrive, Network, ArrowRight, AlertTriangle, Info, Plus, RefreshCw, Zap } from 'lucide-react';

// Migration operation types
type MigrationType = 'node_migration' | 'member_migration' | 'member_addition';

// Target types for RAID operations
type TargetType = 'node' | 'local_disk' | 'internal_nvmeof' | 'external_nvmeof';

interface LocalDisk {
  id: string;
  node: string;
  pci_addr: string;
  capacity_gb: number;
  model: string;
  healthy: boolean;
  blobstore_initialized: boolean;
  available: boolean; // Not currently in use
}

interface NvmeofTarget {
  id: string;
  nqn: string;
  target_ip: string;
  target_port: number;
  transport: string;
  node: string;
  bdev_name: string;
  active: boolean;
  capacity_gb?: number;
  type: 'internal' | 'external';
}

interface RaidMember {
  slot: number;
  name: string;
  state: string;
  node?: string;
  disk_ref?: string;
  health_status: string;
}

interface RaidInfo {
  name: string;
  raid_level: number;
  state: string;
  members: RaidMember[];
  node: string;
}

interface EnhancedRaidMigrationDialogProps {
  isOpen: boolean;
  onClose: () => void;
  onConfirm: (operation: MigrationOperation) => Promise<void>;
  
  // Operation context
  migrationType: MigrationType;
  volumeId?: string;
  raidInfo?: RaidInfo;
  currentNode: string;
  
  // Available targets
  availableNodes: string[];
  availableDisks: LocalDisk[];
  availableNvmeofTargets: NvmeofTarget[];
  
  // UI state
  isLoading?: boolean;
}

interface MigrationOperation {
  type: MigrationType;
  volume_id?: string;
  raid_name?: string;
  
  // Target specification
  target_type: TargetType;
  target_node?: string;
  target_disk_id?: string;
  target_nvmeof_nqn?: string;
  
  // Member-specific operations
  member_slot?: number; // For member migration
  new_member_count?: number; // For member addition
  
  // Advanced options
  force?: boolean;
  preserve_data?: boolean;
}

export const EnhancedRaidMigrationDialog: React.FC<EnhancedRaidMigrationDialogProps> = ({
  isOpen,
  onClose,
  onConfirm,
  migrationType,
  volumeId,
  raidInfo,
  currentNode,
  availableNodes,
  availableDisks,
  availableNvmeofTargets,
  isLoading = false
}) => {
  const [targetType, setTargetType] = useState<TargetType>('node');
  const [selectedNode, setSelectedNode] = useState<string>('');
  const [selectedDisk, setSelectedDisk] = useState<string>('');
  const [selectedNvmeof, setSelectedNvmeof] = useState<string>('');
  const [selectedMemberSlot, setSelectedMemberSlot] = useState<number>(0);
  const [memberCount, setMemberCount] = useState<number>(1);
  const [preserveData, setPreserveData] = useState<boolean>(true);
  const [force, setForce] = useState<boolean>(false);
  const [isConfirming, setIsConfirming] = useState(false);

  // Check if this is a single-replica volume (cannot be migrated)
  const isSingleReplica = raidInfo && raidInfo.members.length === 1 && raidInfo.raid_level === 1;

  // Reset form when dialog opens
  useEffect(() => {
    if (isOpen) {
      setTargetType(migrationType === 'node_migration' ? 'node' : 'local_disk');
      setSelectedNode('');
      setSelectedDisk('');
      setSelectedNvmeof('');
      setSelectedMemberSlot(0);
      setMemberCount(1);
      setPreserveData(true);
      setForce(false);
      setIsConfirming(false);
    }
  }, [isOpen, migrationType]);

  // Filter available targets based on current node and requirements
  const validNodes = availableNodes.filter(node => node !== currentNode);
  const validDisks = availableDisks.filter(disk => 
    disk.available && disk.healthy && disk.blobstore_initialized &&
    (targetType === 'local_disk' ? disk.node !== currentNode : true)
  );
  const internalTargets = availableNvmeofTargets.filter(target => 
    target.type === 'internal' && target.active
  );
  const externalTargets = availableNvmeofTargets.filter(target => 
    target.type === 'external' && target.active
  );

  const getOperationTitle = () => {
    switch (migrationType) {
      case 'node_migration':
        return 'Migrate RAID Volume';
      case 'member_migration':
        return 'Migrate RAID Member';
      case 'member_addition':
        return 'Add RAID Members';
      default:
        return 'RAID Operation';
    }
  };

  const getOperationDescription = () => {
    switch (migrationType) {
      case 'node_migration':
        return `Migrate the entire RAID volume ${volumeId || ''} from ${currentNode} to another location. This moves all data and maintains volume availability.`;
      case 'member_migration':
        return `Replace a RAID member in ${raidInfo?.name || 'RAID array'} with a new disk or NVMe-oF target. This rebuilds the member while maintaining data integrity.`;
      case 'member_addition':
        return `Add new members to ${raidInfo?.name || 'RAID array'} to increase capacity or redundancy. New members will be synchronized with existing data.`;
      default:
        return 'Perform RAID operation with selected targets.';
    }
  };

  const isValidSelection = () => {
    switch (targetType) {
      case 'node':
        return selectedNode !== '';
      case 'local_disk':
        return selectedDisk !== '';
      case 'internal_nvmeof':
      case 'external_nvmeof':
        return selectedNvmeof !== '';
      default:
        return false;
    }
  };

  const handleConfirm = async () => {
    if (!isValidSelection()) return;

    setIsConfirming(true);
    try {
      const operation: MigrationOperation = {
        type: migrationType,
        volume_id: volumeId,
        raid_name: raidInfo?.name,
        target_type: targetType,
        preserve_data: preserveData,
        force: force
      };

      // Set target-specific fields
      switch (targetType) {
        case 'node':
          operation.target_node = selectedNode;
          break;
        case 'local_disk':
          operation.target_disk_id = selectedDisk;
          const selectedDiskObj = validDisks.find(d => d.id === selectedDisk);
          operation.target_node = selectedDiskObj?.node;
          break;
        case 'internal_nvmeof':
        case 'external_nvmeof':
          operation.target_nvmeof_nqn = selectedNvmeof;
          const selectedNvmeofObj = [...internalTargets, ...externalTargets].find(t => t.nqn === selectedNvmeof);
          operation.target_node = selectedNvmeofObj?.node;
          break;
      }

      // Set operation-specific fields
      if (migrationType === 'member_migration') {
        operation.member_slot = selectedMemberSlot;
      } else if (migrationType === 'member_addition') {
        operation.new_member_count = memberCount;
      }

      await onConfirm(operation);
    } finally {
      setIsConfirming(false);
    }
  };

  const renderTargetTypeSelection = () => (
    <div className="space-y-3">
      <label className="block text-sm font-medium text-gray-700">Target Type</label>
      <div className="grid grid-cols-1 gap-2">
        {/* Node Migration (only for node_migration type) */}
        {migrationType === 'node_migration' && (
          <label className="flex items-center gap-3 p-3 border rounded-lg cursor-pointer hover:bg-gray-50">
            <input
              type="radio"
              name="targetType"
              value="node"
              checked={targetType === 'node'}
              onChange={(e) => setTargetType(e.target.value as TargetType)}
              className="text-blue-600"
            />
            <Server className="w-5 h-5 text-gray-500" />
            <div>
              <div className="font-medium">Node Migration</div>
              <div className="text-sm text-gray-600">Move entire volume to another node</div>
            </div>
          </label>
        )}

        {/* Local Disk */}
        <label className="flex items-center gap-3 p-3 border rounded-lg cursor-pointer hover:bg-gray-50">
          <input
            type="radio"
            name="targetType"
            value="local_disk"
            checked={targetType === 'local_disk'}
            onChange={(e) => setTargetType(e.target.value as TargetType)}
            className="text-blue-600"
          />
          <HardDrive className="w-5 h-5 text-gray-500" />
          <div>
            <div className="font-medium">Local NVMe Disk</div>
            <div className="text-sm text-gray-600">Use local NVMe disk on cluster nodes</div>
          </div>
        </label>

        {/* Internal NVMe-oF */}
        <label className="flex items-center gap-3 p-3 border rounded-lg cursor-pointer hover:bg-gray-50">
          <input
            type="radio"
            name="targetType"
            value="internal_nvmeof"
            checked={targetType === 'internal_nvmeof'}
            onChange={(e) => setTargetType(e.target.value as TargetType)}
            className="text-blue-600"
          />
          <Network className="w-5 h-5 text-blue-500" />
          <div>
            <div className="font-medium">Internal NVMe-oF Target</div>
            <div className="text-sm text-gray-600">Use NVMe-oF target within cluster</div>
          </div>
        </label>

        {/* External NVMe-oF */}
        <label className="flex items-center gap-3 p-3 border rounded-lg cursor-pointer hover:bg-gray-50">
          <input
            type="radio"
            name="targetType"
            value="external_nvmeof"
            checked={targetType === 'external_nvmeof'}
            onChange={(e) => setTargetType(e.target.value as TargetType)}
            className="text-blue-600"
          />
          <Network className="w-5 h-5 text-purple-500" />
          <div>
            <div className="font-medium">External NVMe-oF Target</div>
            <div className="text-sm text-gray-600">Use external NVMe-oF storage system</div>
          </div>
        </label>
      </div>
    </div>
  );

  const renderTargetSelection = () => {
    switch (targetType) {
      case 'node':
        return (
          <div className="space-y-3">
            <label className="block text-sm font-medium text-gray-700">Target Node</label>
            {validNodes.length > 0 ? (
              <select
                value={selectedNode}
                onChange={(e) => setSelectedNode(e.target.value)}
                className="w-full border border-gray-300 rounded-lg px-3 py-2"
              >
                <option value="">Select target node...</option>
                {validNodes.map(node => (
                  <option key={node} value={node}>{node}</option>
                ))}
              </select>
            ) : (
              <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg text-sm text-yellow-800">
                No other nodes available for migration
              </div>
            )}
          </div>
        );

      case 'local_disk':
        return (
          <div className="space-y-3">
            <label className="block text-sm font-medium text-gray-700">Target Disk</label>
            {validDisks.length > 0 ? (
              <select
                value={selectedDisk}
                onChange={(e) => setSelectedDisk(e.target.value)}
                className="w-full border border-gray-300 rounded-lg px-3 py-2"
              >
                <option value="">Select target disk...</option>
                {validDisks.map(disk => (
                  <option key={disk.id} value={disk.id}>
                    {disk.id} ({disk.model}, {disk.capacity_gb}GB) - {disk.node}
                  </option>
                ))}
              </select>
            ) : (
              <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg text-sm text-yellow-800">
                No available local disks found
              </div>
            )}
          </div>
        );

      case 'internal_nvmeof':
        return (
          <div className="space-y-3">
            <label className="block text-sm font-medium text-gray-700">Internal NVMe-oF Target</label>
            {internalTargets.length > 0 ? (
              <select
                value={selectedNvmeof}
                onChange={(e) => setSelectedNvmeof(e.target.value)}
                className="w-full border border-gray-300 rounded-lg px-3 py-2"
              >
                <option value="">Select NVMe-oF target...</option>
                {internalTargets.map(target => (
                  <option key={target.nqn} value={target.nqn}>
                    {target.bdev_name} ({target.target_ip}:{target.target_port}) - {target.capacity_gb || '?'}GB
                  </option>
                ))}
              </select>
            ) : (
              <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg text-sm text-yellow-800">
                No internal NVMe-oF targets available
              </div>
            )}
          </div>
        );

      case 'external_nvmeof':
        return (
          <div className="space-y-3">
            <label className="block text-sm font-medium text-gray-700">External NVMe-oF Target</label>
            {externalTargets.length > 0 ? (
              <select
                value={selectedNvmeof}
                onChange={(e) => setSelectedNvmeof(e.target.value)}
                className="w-full border border-gray-300 rounded-lg px-3 py-2"
              >
                <option value="">Select external target...</option>
                {externalTargets.map(target => (
                  <option key={target.nqn} value={target.nqn}>
                    {target.bdev_name} ({target.target_ip}:{target.target_port}) - {target.capacity_gb || '?'}GB
                  </option>
                ))}
              </select>
            ) : (
              <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg text-sm text-yellow-800">
                No external NVMe-oF targets available
              </div>
            )}
          </div>
        );

      default:
        return null;
    }
  };

  const renderOperationOptions = () => {
    if (migrationType === 'member_migration' && raidInfo) {
      return (
        <div className="space-y-3">
          <label className="block text-sm font-medium text-gray-700">RAID Member to Replace</label>
          <select
            value={selectedMemberSlot}
            onChange={(e) => setSelectedMemberSlot(Number(e.target.value))}
            className="w-full border border-gray-300 rounded-lg px-3 py-2"
          >
            {raidInfo.members.map(member => (
              <option key={member.slot} value={member.slot}>
                Slot {member.slot}: {member.name} ({member.state})
              </option>
            ))}
          </select>
        </div>
      );
    }

    if (migrationType === 'member_addition') {
      return (
        <div className="space-y-3">
          <label className="block text-sm font-medium text-gray-700">Number of Members to Add</label>
          <select
            value={memberCount}
            onChange={(e) => setMemberCount(Number(e.target.value))}
            className="w-full border border-gray-300 rounded-lg px-3 py-2"
          >
            {[1, 2, 3, 4].map(count => (
              <option key={count} value={count}>{count} member{count > 1 ? 's' : ''}</option>
            ))}
          </select>
        </div>
      );
    }

    return null;
  };

  const renderAdvancedOptions = () => (
    <div className="space-y-3">
      <h4 className="text-sm font-medium text-gray-700">Advanced Options</h4>
      
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={preserveData}
          onChange={(e) => setPreserveData(e.target.checked)}
          className="text-blue-600"
        />
        <span className="text-sm">Preserve existing data during migration</span>
      </label>

      {migrationType !== 'node_migration' && (
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={force}
            onChange={(e) => setForce(e.target.checked)}
            className="text-red-600"
          />
          <span className="text-sm text-red-700">Force operation (skip safety checks)</span>
        </label>
      )}
    </div>
  );

  const getWarningMessage = () => {
    if (migrationType === 'member_migration') {
      return "RAID member migration will trigger a rebuild process. Ensure sufficient performance headroom during rebuild.";
    }
    if (migrationType === 'member_addition') {
      return "Adding RAID members will trigger resynchronization. Monitor system performance during this process.";
    }
    if (force) {
      return "Force mode bypasses safety checks. This may result in data loss if the operation fails.";
    }
    return null;
  };

  if (!isOpen) return null;

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div className="bg-white rounded-lg shadow-xl max-w-2xl w-full mx-4 max-h-[90vh] overflow-y-auto">
        {/* Header */}
        <div className="flex items-center justify-between p-6 border-b">
          <div className="flex items-center gap-3">
            <Zap className="w-6 h-6 text-blue-600" />
            <h2 className="text-lg font-semibold">{getOperationTitle()}</h2>
          </div>
          <button
            onClick={onClose}
            disabled={isConfirming}
            className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md disabled:opacity-50"
          >
            ×
          </button>
        </div>

        {/* Content */}
        <div className="p-6 space-y-6">
          <p className="text-gray-700">{getOperationDescription()}</p>

          {/* Single-replica warning */}
          {isSingleReplica && (
            <div className="bg-orange-50 border border-orange-200 rounded-lg p-4">
              <div className="flex items-start gap-3">
                <AlertTriangle className="w-5 h-5 text-orange-600 mt-0.5 flex-shrink-0" />
                <div>
                  <h4 className="font-medium text-orange-900 mb-1">Migration Not Available</h4>
                  <p className="text-sm text-orange-800">
                    This is a single-replica volume that uses direct LVS storage without RAID redundancy. 
                    Single-replica volumes cannot be migrated as they have no redundancy to maintain availability during the migration process.
                  </p>
                  <p className="text-sm text-orange-700 mt-2">
                    To enable migration, consider upgrading to a multi-replica configuration.
                  </p>
                </div>
              </div>
            </div>
          )}

          {/* Current RAID Info */}
          {raidInfo && (
            <div className="bg-gray-50 border border-gray-200 rounded-lg p-4">
              <h4 className="font-medium text-gray-900 mb-2">Current RAID Information</h4>
              <div className="grid grid-cols-2 gap-4 text-sm">
                <div>
                  <span className="text-gray-600">RAID Name:</span>
                  <span className="ml-2 font-medium">{raidInfo.name}</span>
                </div>
                <div>
                  <span className="text-gray-600">RAID Level:</span>
                  <span className="ml-2 font-medium">RAID-{raidInfo.raid_level}</span>
                </div>
                <div>
                  <span className="text-gray-600">State:</span>
                  <span className={`ml-2 px-2 py-1 rounded-full text-xs ${
                    raidInfo.state === 'online' ? 'bg-green-100 text-green-800' :
                    raidInfo.state === 'degraded' ? 'bg-yellow-100 text-yellow-800' :
                    'bg-red-100 text-red-800'
                  }`}>
                    {raidInfo.state.toUpperCase()}
                  </span>
                </div>
                <div>
                  <span className="text-gray-600">Members:</span>
                  <span className="ml-2 font-medium">{raidInfo.members.length}</span>
                </div>
              </div>
            </div>
          )}

          {/* Target Type Selection */}
          {!isSingleReplica && renderTargetTypeSelection()}

          {/* Target Selection */}
          {!isSingleReplica && renderTargetSelection()}

          {/* Operation-Specific Options */}
          {!isSingleReplica && renderOperationOptions()}

          {/* Advanced Options */}
          {!isSingleReplica && renderAdvancedOptions()}

          {/* Warning Message */}
          {getWarningMessage() && (
            <div className="bg-yellow-50 border border-yellow-200 rounded-lg p-3">
              <div className="flex items-start gap-2">
                <AlertTriangle className="w-4 h-4 text-yellow-600 mt-0.5 flex-shrink-0" />
                <p className="text-sm text-yellow-800">{getWarningMessage()}</p>
              </div>
            </div>
          )}

          {/* Info Message */}
          <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
            <div className="flex items-start gap-2">
              <Info className="w-4 h-4 text-blue-600 mt-0.5 flex-shrink-0" />
              <p className="text-sm text-blue-800">
                This operation uses SPDK JSON-RPC methods to perform the migration safely with minimal downtime.
              </p>
            </div>
          </div>
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-3 p-6 border-t bg-gray-50">
          <button
            onClick={onClose}
            disabled={isConfirming}
            className="px-4 py-2 text-gray-700 border border-gray-300 rounded-lg hover:bg-gray-100 disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            onClick={handleConfirm}
            disabled={isSingleReplica || !isValidSelection() || isConfirming || isLoading}
            className={`px-4 py-2 rounded-lg flex items-center gap-2 disabled:opacity-50 ${
              isSingleReplica 
                ? 'bg-gray-400 text-gray-600 cursor-not-allowed'
                : 'bg-blue-600 text-white hover:bg-blue-700'
            }`}
            title={isSingleReplica ? 'Migration not available for single-replica volumes' : ''}
          >
            {isConfirming ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                <span>Processing...</span>
              </>
            ) : (
              <>
                <RefreshCw className="w-4 h-4" />
                <span>{isSingleReplica ? 'Migration Not Available' : `Start ${getOperationTitle()}`}</span>
              </>
            )}
          </button>
        </div>
      </div>
    </div>
  );
};
