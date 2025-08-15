import React from 'react';
import { CheckCircle, XCircle, RefreshCw, AlertTriangle, HardDrive, Network, Trash2, Clock } from 'lucide-react';

export interface RaidMemberStatus {
  slot: number;
  name: string;
  state: 'online' | 'degraded' | 'failed' | 'rebuilding' | 'removing' | 'pending_removal';
  type: 'local_disk' | 'internal_nvmeof' | 'external_nvmeof';
  node?: string;
  disk_ref?: string;
  nvmeof_nqn?: string;
  health_status: 'healthy' | 'degraded' | 'failed';
  capacity_gb?: number;
  
  // Migration/cleanup status
  migration_status?: {
    is_being_replaced: boolean;
    replacement_progress?: number;
    new_member_id?: string;
    estimated_completion?: string;
  };
  
  // Cleanup tracking
  cleanup_status?: {
    marked_for_removal: boolean;
    data_migration_complete: boolean;
    safe_to_remove: boolean;
    removal_scheduled?: string;
  };
  
  // Performance metrics
  performance?: {
    read_iops: number;
    write_iops: number;
    read_latency_ms: number;
    write_latency_ms: number;
  };
}

interface RaidMemberStatusDisplayProps {
  raidName: string;
  raidLevel: number;
  members: RaidMemberStatus[];
  onReplaceMember?: (slot: number) => void;
  onRemoveMember?: (slot: number) => void;
  onAddMember?: () => void;
  showActions?: boolean;
}

export const RaidMemberStatusDisplay: React.FC<RaidMemberStatusDisplayProps> = ({
  raidName,
  raidLevel,
  members,
  onReplaceMember,
  onRemoveMember,
  onAddMember,
  showActions = true
}) => {
  const getMemberIcon = (member: RaidMemberStatus) => {
    if (member.migration_status?.is_being_replaced) {
      return <RefreshCw className="w-4 h-4 text-blue-600 animate-spin" />;
    }
    
    switch (member.state) {
      case 'online':
        return <CheckCircle className="w-4 h-4 text-green-600" />;
      case 'failed':
        return <XCircle className="w-4 h-4 text-red-600" />;
      case 'degraded':
        return <AlertTriangle className="w-4 h-4 text-yellow-600" />;
      case 'rebuilding':
        return <RefreshCw className="w-4 h-4 text-blue-600 animate-spin" />;
      case 'removing':
      case 'pending_removal':
        return <Trash2 className="w-4 h-4 text-orange-600" />;
      default:
        return <Clock className="w-4 h-4 text-gray-600" />;
    }
  };

  const getMemberTypeIcon = (type: string) => {
    switch (type) {
      case 'local_disk':
        return <HardDrive className="w-4 h-4 text-gray-600" />;
      case 'internal_nvmeof':
        return <Network className="w-4 h-4 text-blue-600" />;
      case 'external_nvmeof':
        return <Network className="w-4 h-4 text-purple-600" />;
      default:
        return <HardDrive className="w-4 h-4 text-gray-600" />;
    }
  };

  const getStateColor = (state: string) => {
    switch (state) {
      case 'online':
        return 'bg-green-100 text-green-800 border-green-200';
      case 'failed':
        return 'bg-red-100 text-red-800 border-red-200';
      case 'degraded':
        return 'bg-yellow-100 text-yellow-800 border-yellow-200';
      case 'rebuilding':
        return 'bg-blue-100 text-blue-800 border-blue-200';
      case 'removing':
      case 'pending_removal':
        return 'bg-orange-100 text-orange-800 border-orange-200';
      default:
        return 'bg-gray-100 text-gray-800 border-gray-200';
    }
  };

  const canAddMembers = () => {
    // RAID-1 can have additional mirrors, RAID-0 can be expanded
    return raidLevel === 0 || raidLevel === 1;
  };

  const canRemoveMembers = () => {
    // Must maintain minimum members (2 for RAID-1, 1 for RAID-0)
    const minMembers = raidLevel === 1 ? 2 : 1;
    const activeMembers = members.filter(m => 
      m.state !== 'failed' && m.state !== 'removing' && m.state !== 'pending_removal'
    ).length;
    return activeMembers > minMembers;
  };

  return (
    <div className="bg-white rounded-lg border">
      {/* Header */}
      <div className="p-4 border-b">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="text-lg font-semibold flex items-center gap-2">
              <HardDrive className="w-5 h-5 text-gray-600" />
              {raidName} Members
            </h3>
            <p className="text-sm text-gray-600">
              RAID-{raidLevel} • {members.length} members • 
              {members.filter(m => m.state === 'online').length} online
            </p>
          </div>
          
          {showActions && canAddMembers() && onAddMember && (
            <button
              onClick={onAddMember}
              className="px-3 py-1 text-sm bg-green-600 text-white rounded hover:bg-green-700 flex items-center gap-1"
            >
              <HardDrive className="w-4 h-4" />
              Add Member
            </button>
          )}
        </div>
      </div>

      {/* Members List */}
      <div className="divide-y">
        {members.map((member) => (
          <div key={member.slot} className="p-4">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-3">
                {getMemberIcon(member)}
                {getMemberTypeIcon(member.type)}
                <div>
                  <div className="flex items-center gap-2">
                    <span className="font-medium">Slot {member.slot}</span>
                    <span className="text-sm text-gray-600">{member.name}</span>
                    <span className={`px-2 py-1 text-xs rounded-full border ${getStateColor(member.state)}`}>
                      {member.state.toUpperCase()}
                    </span>
                  </div>
                  
                  <div className="text-xs text-gray-500 mt-1 space-x-3">
                    {member.node && (
                      <span>Node: {member.node}</span>
                    )}
                    {member.capacity_gb && (
                      <span>Capacity: {member.capacity_gb}GB</span>
                    )}
                    {member.type === 'internal_nvmeof' && (
                      <span>Internal NVMe-oF</span>
                    )}
                    {member.type === 'external_nvmeof' && (
                      <span>External NVMe-oF</span>
                    )}
                  </div>
                </div>
              </div>

              {/* Actions */}
              {showActions && (
                <div className="flex items-center gap-2">
                  {member.state === 'failed' || member.state === 'degraded' ? (
                    <button
                      onClick={() => onReplaceMember?.(member.slot)}
                      className="px-2 py-1 text-xs bg-blue-600 text-white rounded hover:bg-blue-700"
                    >
                      Replace
                    </button>
                  ) : member.state === 'online' && canRemoveMembers() ? (
                    <button
                      onClick={() => onRemoveMember?.(member.slot)}
                      className="px-2 py-1 text-xs bg-red-600 text-white rounded hover:bg-red-700"
                    >
                      Remove
                    </button>
                  ) : null}
                </div>
              )}
            </div>

            {/* Migration Progress */}
            {member.migration_status?.is_being_replaced && (
              <div className="mt-3 p-3 bg-blue-50 rounded-lg border border-blue-200">
                <div className="flex items-center justify-between mb-2">
                  <span className="text-sm font-medium text-blue-900">
                    Replacement in Progress
                  </span>
                  <span className="text-sm text-blue-700">
                    {member.migration_status.replacement_progress?.toFixed(1)}%
                  </span>
                </div>
                
                <div className="w-full bg-white rounded-full h-2 mb-2">
                  <div
                    className="bg-blue-500 h-2 rounded-full transition-all duration-300"
                    style={{ width: `${member.migration_status.replacement_progress || 0}%` }}
                  />
                </div>
                
                <div className="text-xs text-blue-600 space-y-1">
                  {member.migration_status.new_member_id && (
                    <div>New member: {member.migration_status.new_member_id}</div>
                  )}
                  {member.migration_status.estimated_completion && (
                    <div>ETA: {member.migration_status.estimated_completion}</div>
                  )}
                </div>
              </div>
            )}

            {/* Cleanup Status */}
            {member.cleanup_status && (
              <div className="mt-3 p-3 bg-orange-50 rounded-lg border border-orange-200">
                <div className="text-sm font-medium text-orange-900 mb-2">
                  Cleanup Status
                </div>
                
                <div className="grid grid-cols-2 gap-3 text-xs">
                  <div className="flex items-center gap-1">
                    <CheckCircle className={`w-3 h-3 ${
                      member.cleanup_status.data_migration_complete ? 'text-green-600' : 'text-gray-400'
                    }`} />
                    <span className={
                      member.cleanup_status.data_migration_complete ? 'text-green-700' : 'text-gray-500'
                    }>
                      Data Migration
                    </span>
                  </div>
                  
                  <div className="flex items-center gap-1">
                    <CheckCircle className={`w-3 h-3 ${
                      member.cleanup_status.safe_to_remove ? 'text-green-600' : 'text-gray-400'
                    }`} />
                    <span className={
                      member.cleanup_status.safe_to_remove ? 'text-green-700' : 'text-gray-500'
                    }>
                      Safe to Remove
                    </span>
                  </div>
                </div>
                
                {member.cleanup_status.removal_scheduled && (
                  <div className="mt-2 text-xs text-orange-600">
                    Scheduled removal: {member.cleanup_status.removal_scheduled}
                  </div>
                )}
              </div>
            )}

            {/* Performance Metrics */}
            {member.performance && member.state === 'online' && (
              <div className="mt-3 grid grid-cols-4 gap-4 text-xs">
                <div>
                  <span className="text-gray-600">Read IOPS:</span>
                  <div className="font-medium">{member.performance.read_iops.toLocaleString()}</div>
                </div>
                <div>
                  <span className="text-gray-600">Write IOPS:</span>
                  <div className="font-medium">{member.performance.write_iops.toLocaleString()}</div>
                </div>
                <div>
                  <span className="text-gray-600">Read Latency:</span>
                  <div className="font-medium">{member.performance.read_latency_ms.toFixed(1)}ms</div>
                </div>
                <div>
                  <span className="text-gray-600">Write Latency:</span>
                  <div className="font-medium">{member.performance.write_latency_ms.toFixed(1)}ms</div>
                </div>
              </div>
            )}
          </div>
        ))}
      </div>

      {/* Summary Footer */}
      <div className="p-4 border-t bg-gray-50">
        <div className="grid grid-cols-4 gap-4 text-sm">
          <div className="text-center">
            <div className="font-medium text-green-600">
              {members.filter(m => m.state === 'online').length}
            </div>
            <div className="text-gray-600">Online</div>
          </div>
          <div className="text-center">
            <div className="font-medium text-blue-600">
              {members.filter(m => m.state === 'rebuilding' || m.migration_status?.is_being_replaced).length}
            </div>
            <div className="text-gray-600">Rebuilding</div>
          </div>
          <div className="text-center">
            <div className="font-medium text-yellow-600">
              {members.filter(m => m.state === 'degraded').length}
            </div>
            <div className="text-gray-600">Degraded</div>
          </div>
          <div className="text-center">
            <div className="font-medium text-red-600">
              {members.filter(m => m.state === 'failed').length}
            </div>
            <div className="text-gray-600">Failed</div>
          </div>
        </div>
      </div>
    </div>
  );
};
