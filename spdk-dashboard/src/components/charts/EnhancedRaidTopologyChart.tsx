import React, { useState, useMemo, useRef, useEffect } from 'react';
import { 
  Database, Activity, X, Settings, Zap, Network, Info, AlertTriangle, 
  Cable, Monitor, Shield, HardDrive, Clock, CheckCircle, Search, ChevronDown
} from 'lucide-react';
import { NVMFTooltip } from '../ui/NVMFTooltip';
import { VolumeAccessTooltip } from '../ui/VolumeAccessTooltip';
import type { Volume, RaidStatus, RaidMember, RebuildInfo } from '../../hooks/useDashboardData';

interface EnhancedRaidTopologyChartProps {
  volumes: Volume[];
}

export const EnhancedRaidTopologyChart: React.FC<EnhancedRaidTopologyChartProps> = ({ volumes }) => {
  const [selectedVolume, setSelectedVolume] = useState(volumes[0]?.id || '');
  const [showTechnicalDetails, setShowTechnicalDetails] = useState(false);

  const [searchTerm, setSearchTerm] = useState('');
  const [isDropdownOpen, setIsDropdownOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);
  
  // Filter volumes based on search term
  const filteredVolumes = useMemo(() => {
    if (!searchTerm.trim()) return volumes;
    
    const searchLower = searchTerm.toLowerCase();
    return volumes.filter(volume => 
      volume.name.toLowerCase().includes(searchLower) ||
      volume.id.toLowerCase().includes(searchLower) ||
      volume.state.toLowerCase().includes(searchLower) ||
      volume.nodes.some(node => node.toLowerCase().includes(searchLower))
    );
  }, [volumes, searchTerm]);

  // Handle clicking outside to close dropdown
  useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(event.target as Node)) {
        setIsDropdownOpen(false);
      }
    };

    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, []);

  // Handle volume selection
  const handleVolumeSelect = (volumeId: string, volumeName: string) => {
    setSelectedVolume(volumeId);
    setIsDropdownOpen(false);
    setSearchTerm('');
  };

  // Open dropdown and focus search input
  const openDropdown = () => {
    setIsDropdownOpen(true);
    setTimeout(() => {
      searchInputRef.current?.focus();
    }, 0);
  };

  const selectedVolumeInfo = volumes.find(v => v.id === selectedVolume);
  
  if (!selectedVolumeInfo) return null;

  const getRaidLevelDisplayName = (raidLevel: number): string => {
    switch (raidLevel) {
      case 0: return 'RAID-0 (Striping)';
      case 1: return 'RAID-1 (Mirroring)';
      case 5: return 'RAID-5 (Distributed Parity)';
      case 6: return 'RAID-6 (Dual Parity)';
      case 10: return 'RAID-10 (Striped Mirrors)';
      default: return `RAID-${raidLevel}`;
    }
  };

  const getRaidLevelDescription = (raidLevel: number): string => {
    switch (raidLevel) {
      case 0: return 'Data striped across all members for maximum performance';
      case 1: return 'Data mirrored across members for redundancy';
      case 5: return 'Data striped with distributed parity for fault tolerance';
      case 6: return 'Data striped with dual parity for enhanced fault tolerance';
      case 10: return 'Striped mirrors combining performance and redundancy';
      default: return 'Custom RAID configuration';
    }
  };

  const getRaidMemberStateColor = (state: string) => {
    switch (state.toLowerCase()) {
      case 'online': return 'bg-green-100 text-green-800 border-green-200';
      case 'degraded': return 'bg-yellow-100 text-yellow-800 border-yellow-200';
      case 'failed': return 'bg-red-100 text-red-800 border-red-200';
      case 'rebuilding': return 'bg-orange-100 text-orange-800 border-orange-200';
      case 'spare': return 'bg-blue-100 text-blue-800 border-blue-200';
      case 'removing': return 'bg-purple-100 text-purple-800 border-purple-200';
      default: return 'bg-gray-100 text-gray-800 border-gray-200';
    }
  };

  const getRaidMemberIcon = (state: string) => {
    switch (state.toLowerCase()) {
      case 'online': return <CheckCircle className="w-4 h-4 text-green-600" />;
      case 'failed': return <X className="w-4 h-4 text-red-600" />;
      case 'rebuilding': return <Settings className="w-4 h-4 text-orange-600 animate-spin" />;
      case 'degraded': return <AlertTriangle className="w-4 h-4 text-yellow-600" />;
      case 'spare': return <Shield className="w-4 h-4 text-blue-600" />;
      case 'removing': return <Clock className="w-4 h-4 text-purple-600" />;
      default: return <HardDrive className="w-4 h-4 text-gray-600" />;
    }
  };

  const getReplicaStatusColor = (status: string) => {
    switch (status) {
      case 'healthy': return 'bg-green-100 text-green-800 border-green-200';
      case 'failed': return 'bg-red-100 text-red-800 border-red-200';
      case 'rebuilding': return 'bg-orange-100 text-orange-800 border-orange-200';
      case 'degraded': return 'bg-yellow-100 text-yellow-800 border-yellow-200';
      default: return 'bg-gray-100 text-gray-800 border-gray-200';
    }
  };

  const getReplicaIcon = (replica: any) => {
    if (replica.status === 'failed') return <X className="w-4 h-4 text-red-600" />;
    if (replica.status === 'rebuilding') return <Settings className="w-4 h-4 text-orange-600 animate-spin" />;
    if (replica.is_local) return <Zap className="w-4 h-4 text-blue-600" />;
    return <Network className="w-4 h-4 text-purple-600" />;
  };

  // Check if volume has nvmeof configuration
  const hasNvmeof = selectedVolumeInfo.nvmeof_enabled || 
                    (selectedVolumeInfo.nvmeof_targets && selectedVolumeInfo.nvmeof_targets.length > 0) ||
                    selectedVolumeInfo.access_method === 'nvmeof';

  const raidStatus = selectedVolumeInfo.raid_status;

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center">
          <Activity className="w-6 h-6 text-blue-600 mr-2" />
          <h3 className="text-lg font-semibold">Volume Topology</h3>
        </div>
        <div className="flex items-center gap-4">
          {/* Searchable Volume Dropdown */}
          <div className="relative" ref={dropdownRef}>
            <button
              onClick={openDropdown}
              className="flex items-center gap-2 px-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 bg-white hover:bg-gray-50 min-w-[300px] text-left"
            >
              <Database className="w-4 h-4 text-gray-500" />
              <div className="flex-1 min-w-0">
                <div className="font-medium text-gray-900 truncate">
                  {selectedVolumeInfo?.name || 'Select Volume'}
                </div>
                <div className="text-xs text-gray-500 truncate">
                  {selectedVolumeInfo?.state} • {selectedVolumeInfo?.size}
                </div>
              </div>
              <ChevronDown className={`w-4 h-4 text-gray-400 transition-transform ${
                isDropdownOpen ? 'rotate-180' : ''
              }`} />
            </button>

            {/* Dropdown Menu */}
            {isDropdownOpen && (
              <div className="absolute z-50 mt-1 w-full bg-white border border-gray-300 rounded-md shadow-lg max-h-96 overflow-hidden">
                {/* Search Input */}
                <div className="p-3 border-b border-gray-200">
                  <div className="relative">
                    <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
                    <input
                      ref={searchInputRef}
                      type="text"
                      placeholder="Search volumes by name, ID, state, or node..."
                      value={searchTerm}
                      onChange={(e) => setSearchTerm(e.target.value)}
                      className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 text-sm"
                    />
                  </div>
                  {searchTerm && (
                    <div className="mt-2 text-xs text-gray-500">
                      {filteredVolumes.length} of {volumes.length} volumes
                    </div>
                  )}
                </div>

                {/* Volume List */}
                <div className="max-h-80 overflow-y-auto">
                  {filteredVolumes.length === 0 ? (
                    <div className="p-4 text-center text-gray-500 text-sm">
                      {searchTerm ? 'No volumes match your search' : 'No volumes available'}
                    </div>
                  ) : (
                    filteredVolumes.map((volume) => (
                      <button
                        key={volume.id}
                        onClick={() => handleVolumeSelect(volume.id, volume.name)}
                        className={`w-full px-4 py-3 text-left hover:bg-gray-50 border-b border-gray-100 last:border-b-0 transition-colors ${
                          selectedVolume === volume.id ? 'bg-blue-50 border-blue-200' : ''
                        }`}
                      >
                        <div className="flex items-center justify-between">
                          <div className="flex-1 min-w-0">
                            <div className="font-medium text-gray-900 truncate">
                              {volume.name}
                            </div>
                            <div className="text-xs text-gray-500 mt-1">
                              ID: {volume.id.length > 20 ? `${volume.id.substring(0, 20)}...` : volume.id}
                            </div>
                            <div className="flex items-center gap-2 mt-1">
                              <span className={`px-2 py-0.5 text-xs rounded-full ${
                                volume.state === 'Healthy' ? 'bg-green-100 text-green-700' :
                                volume.state === 'Degraded' ? 'bg-yellow-100 text-yellow-700' :
                                'bg-red-100 text-red-700'
                              }`}>
                                {volume.state}
                              </span>
                              <span className="text-xs text-gray-500">{volume.size}</span>
                              <span className="text-xs text-gray-500">
                                {volume.active_replicas}/{volume.replicas} replicas
                              </span>
                            </div>
                          </div>
                          {volume.rebuild_progress && (
                            <div className="ml-3 flex-shrink-0">
                              <Settings className="w-4 h-4 text-orange-500 animate-spin" />
                            </div>
                          )}
                        </div>
                        <div className="flex flex-wrap gap-1 mt-2">
                          {volume.nodes.slice(0, 3).map(node => (
                            <span key={node} className="px-2 py-0.5 text-xs bg-gray-100 text-gray-600 rounded">
                              {node}
                            </span>
                          ))}
                          {volume.nodes.length > 3 && (
                            <span className="px-2 py-0.5 text-xs bg-gray-100 text-gray-600 rounded">
                              +{volume.nodes.length - 3}
                            </span>
                          )}
                        </div>
                      </button>
                    ))
                  )}
                </div>

                {/* Quick Stats */}
                {volumes.length > 10 && (
                  <div className="p-3 bg-gray-50 border-t border-gray-200">
                    <div className="text-xs text-gray-600 space-y-1">
                      <div className="flex justify-between">
                        <span>Total Volumes:</span>
                        <span className="font-medium">{volumes.length}</span>
                      </div>
                      <div className="flex justify-between">
                        <span>Healthy:</span>
                        <span className="text-green-600 font-medium">
                          {volumes.filter(v => v.state === 'Healthy').length}
                        </span>
                      </div>
                      <div className="flex justify-between">
                        <span>With Issues:</span>
                        <span className="text-red-600 font-medium">
                          {volumes.filter(v => v.state !== 'Healthy').length}
                        </span>
                      </div>
                    </div>
                  </div>
                )}
              </div>
            )}
          </div>

          <button
            onClick={() => setShowTechnicalDetails(!showTechnicalDetails)}
            className={`px-3 py-2 text-sm rounded-md transition-colors ${
              showTechnicalDetails 
                ? 'bg-blue-600 text-white' 
                : 'bg-gray-100 text-gray-700 hover:bg-gray-200'
            }`}
          >
            {showTechnicalDetails ? 'Hide Details' : 'Show Technical Details'}
          </button>
        </div>
      </div>
      
      <div className="text-center relative">
        {/* NVMe-oF Access Layer */}
        {hasNvmeof && (
          <>
            <div className="mb-6">
              <h4 className="text-lg font-semibold mb-4 text-gray-700">Application Access Layer</h4>
              <div className="flex justify-center items-center gap-4">
                <div className="text-center">
                  <div className="w-16 h-16 bg-purple-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-purple-300">
                    <Monitor className="w-8 h-8 text-purple-600" />
                  </div>
                  <p className="font-medium text-sm">Pod/Application</p>
                  <p className="text-xs text-gray-500">User Process</p>
                </div>
                
                <div className="flex items-center">
                  <div className="w-8 h-1 bg-purple-400"></div>
                  <Cable className="w-5 h-5 text-purple-600 mx-2" />
                  <div className="w-8 h-1 bg-purple-400"></div>
                </div>
                
                <VolumeAccessTooltip 
                  targets={selectedVolumeInfo.nvmeof_targets} 
                  raidLevel={raidStatus ? getRaidLevelDisplayName(raidStatus.raid_level) : undefined}
                >
                  <div className="text-center cursor-help">
                    <div className="w-16 h-16 bg-indigo-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-indigo-300">
                      <Network className="w-8 h-8 text-indigo-600" />
                    </div>
                    <p className="font-medium text-sm">NVMe-oF</p>
                    <p className="text-xs text-gray-500">Network Fabric</p>
                    <Info className="w-3 h-3 text-gray-400 mx-auto mt-1" />
                  </div>
                </VolumeAccessTooltip>
              </div>
            </div>

            <div className="flex justify-center mb-6">
              <div className="w-1 h-8 bg-indigo-400"></div>
            </div>
          </>
        )}
        
        {/* SPDK RAID Layer */}
        <div className="mb-8">
          <h4 className="text-xl font-semibold mb-6">SPDK Storage Architecture: {selectedVolumeInfo.name}</h4>
          
          <div className="flex justify-center mb-8">
            <div className="text-center">
              <div className="w-20 h-20 bg-blue-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-blue-300">
                <Database className="w-10 h-10 text-blue-600" />
              </div>
              <p className="font-medium text-lg">
                {raidStatus ? 'SPDK RAID Bdev' : 'SPDK Bdev'}
              </p>
              <p className="text-sm text-gray-600">{selectedVolumeInfo.name}</p>
              <p className="text-xs text-gray-500">{selectedVolumeInfo.size}</p>
              
              {/* Enhanced RAID Status Display */}
              {raidStatus && (
                <div className="mt-3 p-3 bg-blue-50 rounded-lg border border-blue-200 text-left max-w-xs mx-auto">
                  <h5 className="font-medium text-blue-800 mb-2 flex items-center gap-2">
                    <Shield className="w-4 h-4" />
                    {getRaidLevelDisplayName(raidStatus.raid_level)}
                  </h5>
                  <div className="space-y-1 text-xs">
                    <div>
                      <span className="text-gray-600">State:</span>
                      <span className={`ml-1 font-medium ${
                        raidStatus.state === 'online' ? 'text-green-600' :
                        raidStatus.state === 'degraded' ? 'text-yellow-600' :
                        'text-red-600'
                      }`}>
                        {raidStatus.state.toUpperCase()}
                      </span>
                    </div>
                    <div>
                      <span className="text-gray-600">Members:</span>
                      <span className="ml-1 font-medium text-blue-700">
                        {raidStatus.operational_members}/{raidStatus.num_members} operational
                      </span>
                    </div>
                    {raidStatus.auto_rebuild_enabled && (
                      <div>
                        <span className="text-gray-600">Auto-rebuild:</span>
                        <span className="ml-1 font-medium text-green-600">Enabled</span>
                      </div>
                    )}
                    {raidStatus.superblock_version && (
                      <div>
                        <span className="text-gray-600">SB Version:</span>
                        <span className="ml-1 font-medium text-blue-700">{raidStatus.superblock_version}</span>
                      </div>
                    )}
                  </div>
                  <div className="text-xs text-gray-500 mt-2 p-2 bg-blue-50 rounded">
                    {getRaidLevelDescription(raidStatus.raid_level)}
                  </div>
                </div>
              )}
            </div>
          </div>
        </div>
        
        {/* Enhanced RAID Members Visualization */}
        {raidStatus && raidStatus.members && (
          <div className="mb-8">
            <h5 className="text-lg font-semibold mb-4 text-center text-gray-700">
              RAID Member Architecture ({getRaidLevelDisplayName(raidStatus.raid_level)})
            </h5>
            <div className="bg-gray-50 rounded-lg p-6">
              <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6 max-w-6xl mx-auto">
                {raidStatus.members.map((member: RaidMember, index: number) => {
                  const correspondingReplica = selectedVolumeInfo.replica_statuses.find(
                    r => r.raid_member_slot === member.slot
                  );
                  
                  return (
                    <div key={`${member.slot}-${index}`} className="text-center">
                      {/* RAID Member Header */}
                      <div className="mb-4">
                        <div className="w-12 h-12 bg-gray-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-gray-300">
                          <span className="font-bold text-gray-700">#{member.slot}</span>
                        </div>
                        <p className="font-medium text-gray-800">{member.node || 'Unknown Node'}</p>
                        <p className="text-xs text-gray-500">RAID Slot {member.slot}</p>
                      </div>
                      
                      {/* RAID Member Status Card */}
                      <div className={`border-2 rounded-lg p-4 ${getRaidMemberStateColor(member.state)}`}>
                        <div className="flex items-center justify-between mb-2">
                          <div className="flex items-center gap-2">
                            {getRaidMemberIcon(member.state)}
                            <span className="font-medium text-sm">{member.name}</span>
                          </div>
                          {member.is_configured && (
                            <span className="text-xs bg-blue-500 text-white px-2 py-1 rounded-full">
                              CFG
                            </span>
                          )}
                        </div>
                        
                        <div className="text-xs space-y-1">
                          <div className="flex justify-between">
                            <span>RAID State:</span>
                            <span className="font-medium capitalize">{member.state}</span>
                          </div>
                          
                          <div className="flex justify-between">
                            <span>Health:</span>
                            <span className={`font-medium ${
                              member.health_status === 'healthy' ? 'text-green-600' :
                              member.health_status === 'rebuilding' ? 'text-orange-600' :
                              'text-red-600'
                            }`}>
                              {member.health_status}
                            </span>
                          </div>
                          
                          {member.uuid && (
                            <div className="mt-2">
                              <span className="text-gray-600">UUID:</span>
                              <div className="font-mono text-xs break-all text-gray-500">
                                {member.uuid.substring(0, 8)}...
                              </div>
                            </div>
                          )}
                          
                          {correspondingReplica && (
                            <div className="mt-2 p-2 bg-white bg-opacity-50 rounded">
                              <div className="text-xs">
                                <div><strong>Access:</strong> {correspondingReplica.access_method}</div>
                                {correspondingReplica.nvmf_target && (
                                  <div><strong>NVMe-oF:</strong> {correspondingReplica.nvmf_target.target_ip}</div>
                                )}
                                {correspondingReplica.last_io_timestamp && (
                                  <div><strong>Last I/O:</strong> {new Date(correspondingReplica.last_io_timestamp).toLocaleTimeString()}</div>
                                )}
                              </div>
                            </div>
                          )}
                          
                          {member.state === 'rebuilding' && raidStatus.rebuild_info && 
                           raidStatus.rebuild_info.target_slot === member.slot && (
                            <div className="mt-2">
                              <div className="flex justify-between text-xs mb-1">
                                <span>Rebuild Progress:</span>
                                <span>{raidStatus.rebuild_info.progress_percentage.toFixed(1)}%</span>
                              </div>
                              <div className="w-full bg-gray-200 rounded-full h-2">
                                <div 
                                  className="bg-orange-500 h-2 rounded-full transition-all duration-300" 
                                  style={{ width: `${raidStatus.rebuild_info.progress_percentage}%` }}
                                />
                              </div>
                              {raidStatus.rebuild_info.estimated_time_remaining && (
                                <div className="text-xs text-orange-600 mt-1">
                                  ETA: {raidStatus.rebuild_info.estimated_time_remaining}
                                </div>
                              )}
                            </div>
                          )}
                        </div>
                      </div>
                    </div>
                  );
                })}
              </div>
              
              {/* RAID Technical Information */}
              {showTechnicalDetails && raidStatus && (
                <div className="mt-6 p-4 bg-white rounded-lg border border-gray-200">
                  <h6 className="font-medium text-gray-800 mb-2 flex items-center gap-2">
                    <Info className="w-5 h-5 text-blue-600" />
                    RAID Technical Details
                  </h6>
                  <div className="text-sm text-gray-600 grid grid-cols-1 md:grid-cols-2 gap-4">
                    <div>
                      <h6 className="font-medium text-gray-700 mb-1">Configuration:</h6>
                      <ul className="text-xs space-y-1">
                        <li>• RAID Level: {raidStatus.raid_level}</li>
                        <li>• Total Members: {raidStatus.num_members}</li>
                        <li>• Discovered: {raidStatus.discovered_members}</li>
                        <li>• Operational: {raidStatus.operational_members}</li>
                        <li>• Auto-rebuild: {raidStatus.auto_rebuild_enabled ? 'Enabled' : 'Disabled'}</li>
                      </ul>
                    </div>
                    <div>
                      <h6 className="font-medium text-gray-700 mb-1">Status:</h6>
                      <ul className="text-xs space-y-1">
                        <li>• State: {raidStatus.state.toUpperCase()}</li>
                        <li>• Superblock Version: {raidStatus.superblock_version || 'N/A'}</li>
                        <li>• Rebuild Active: {raidStatus.rebuild_info ? 'Yes' : 'No'}</li>
                        {raidStatus.rebuild_info && (
                          <>
                            <li>• Rebuild Target: Slot {raidStatus.rebuild_info.target_slot}</li>
                            <li>• Rebuild Source: Slot {raidStatus.rebuild_info.source_slot}</li>
                          </>
                        )}
                      </ul>
                    </div>
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
        
        {/* Network Replica Status (existing code enhanced) */}
        <div className="mb-8">
          <h5 className="text-lg font-semibold mb-4 text-center text-gray-700">Network Replica Status</h5>
          <div className="bg-gray-50 rounded-lg p-6">
            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6 max-w-4xl mx-auto">
              {selectedVolumeInfo.replica_statuses.map((replica, index) => (
                <div key={`${replica.node}-${index}`} className="text-center">
                  <div className="mb-4">
                    <div className="w-12 h-12 bg-gray-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-gray-300">
                      <Network className="w-6 h-6 text-gray-600" />
                    </div>
                    <p className="font-medium text-gray-800">{replica.node}</p>
                    <p className="text-xs text-gray-500">
                      Replica {replica.raid_member_slot !== undefined ? `(Slot ${replica.raid_member_slot})` : ''}
                    </p>
                  </div>
                  
                  <div className="flex justify-center mb-4">
                    {replica.is_local ? (
                      <div className="flex items-center gap-2 px-3 py-1 bg-blue-100 rounded-full">
                        <Zap className="w-4 h-4 text-blue-600" />
                        <span className="text-xs font-medium text-blue-700">Direct Access</span>
                      </div>
                    ) : (
                      <div className="flex items-center gap-2 px-3 py-1 bg-purple-100 rounded-full">
                        <Network className="w-4 h-4 text-purple-600" />
                        <span className="text-xs font-medium text-purple-700">NVMe-oF Network</span>
                      </div>
                    )}
                  </div>
                  
                  <div className={`border-2 rounded-lg p-4 ${getReplicaStatusColor(replica.status)}`}>
                    <div className="flex items-center justify-between mb-2">
                      <div className="flex items-center gap-1">
                        {getReplicaIcon(replica)}
                        {replica.is_local ? (
                          <span className="font-medium text-sm">Local Replica</span>
                        ) : (
                          <NVMFTooltip target={replica.nvmf_target}>
                            <div className="flex items-center gap-1">
                              <span className="font-medium text-sm">Remote Replica</span>
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
                        <span>RAID State:</span>
                        <span className="font-medium capitalize">{replica.raid_member_state}</span>
                      </div>
                      
                      <div className="flex justify-between">
                        <span>Access Method:</span>
                        <span className={`font-medium ${replica.is_local ? 'text-blue-600' : 'text-purple-600'}`}>
                          {replica.access_method}
                        </span>
                      </div>
                      
                      {showTechnicalDetails && replica.lvol_uuid && (
                        <div className="mt-2">
                          <span className="text-gray-600">LVol UUID:</span>
                          <div className="font-mono text-xs break-all text-gray-500">
                            {replica.lvol_uuid.substring(0, 8)}...
                          </div>
                        </div>
                      )}
                      
                      {!replica.is_local && replica.nvmf_target && (
                        <div className="mt-2 p-2 bg-white bg-opacity-50 rounded">
                          <div className="text-xs">
                            <div><strong>Target:</strong> {replica.nvmf_target.target_ip}:{replica.nvmf_target.target_port}</div>
                            <div><strong>NQN:</strong> <span className="font-mono text-xs break-all">{replica.nvmf_target.nqn}</span></div>
                            <div><strong>Transport:</strong> {replica.nvmf_target.transport_type}</div>
                          </div>
                        </div>
                      )}
                      
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
                    </div>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
        
        {/* Status Summary */}
        <div className="mt-8 flex justify-center gap-4 flex-wrap">
          <span 
            className={`px-4 py-2 rounded-full text-sm font-medium ${
              selectedVolumeInfo.active_replicas === selectedVolumeInfo.replicas 
                ? 'bg-green-100 text-green-800' 
                : selectedVolumeInfo.active_replicas > 0
                ? 'bg-yellow-100 text-yellow-800'
                : 'bg-red-100 text-red-800'
            }`}
          >
            {selectedVolumeInfo.active_replicas}/{selectedVolumeInfo.replicas} Active Replicas
          </span>
          
          {raidStatus && (
            <span className={`px-4 py-2 rounded-full text-sm font-medium flex items-center gap-1 ${
              raidStatus.state === 'online' ? 'bg-green-100 text-green-800' :
              raidStatus.state === 'degraded' ? 'bg-yellow-100 text-yellow-800' :
              'bg-red-100 text-red-800'
            }`}>
              <Shield className="w-4 h-4" />
              {getRaidLevelDisplayName(raidStatus.raid_level)} {raidStatus.state.toUpperCase()}
            </span>
          )}
          
          {selectedVolumeInfo.local_nvme && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-blue-100 text-blue-800 flex items-center gap-1">
              <Zap className="w-4 h-4" />
              High Performance Path
            </span>
          )}
          
          {hasNvmeof && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-indigo-100 text-indigo-800 flex items-center gap-1">
              <Network className="w-4 h-4" />
              NVMe-oF Enabled
            </span>
          )}
          
          {raidStatus?.rebuild_info && (
            <span className="px-4 py-2 rounded-full text-sm font-medium bg-orange-100 text-orange-800 flex items-center gap-1">
              <Settings className="w-4 h-4 animate-spin" />
              RAID Rebuild in Progress ({raidStatus.rebuild_info.progress_percentage.toFixed(1)}%)
            </span>
          )}
        </div>
        
        {/* Enhanced rebuild information */}
        {raidStatus?.rebuild_info && (
          <div className="mt-6 p-4 bg-orange-50 rounded-lg border border-orange-200">
            <h5 className="font-medium text-orange-800 mb-2 flex items-center gap-2">
              <Settings className="w-5 h-5 animate-spin" />
              Active RAID Rebuild Operation
            </h5>
            <div className="text-sm">
              <div className="flex justify-between items-center mb-2">
                <span className="font-medium">
                  Rebuilding Slot {raidStatus.rebuild_info.target_slot} from Slot {raidStatus.rebuild_info.source_slot}
                </span>
                <span className="text-orange-600 font-medium">
                  {raidStatus.rebuild_info.progress_percentage.toFixed(1)}%
                </span>
              </div>
              <div className="w-full bg-gray-200 rounded-full h-3 mb-2">
                <div 
                  className="bg-orange-500 h-3 rounded-full transition-all duration-300" 
                  style={{ width: `${raidStatus.rebuild_info.progress_percentage}%` }}
                />
              </div>
              <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-xs text-gray-600">
                <div>
                  <strong>State:</strong> {raidStatus.rebuild_info.state}
                </div>
                <div>
                  <strong>Blocks Remaining:</strong> {raidStatus.rebuild_info.blocks_remaining.toLocaleString()}
                </div>
                <div>
                  <strong>Total Blocks:</strong> {raidStatus.rebuild_info.blocks_total.toLocaleString()}
                </div>
                {raidStatus.rebuild_info.estimated_time_remaining && (
                  <div>
                    <strong>ETA:</strong> {raidStatus.rebuild_info.estimated_time_remaining}
                  </div>
                )}
              </div>
              {hasNvmeof && (
                <div className="text-xs text-blue-600 mt-2">
                  NVMe-oF access remains available during rebuild operations
                </div>
              )}
            </div>
          </div>
        )}
        
        {/* NVMe-oF Technology Info */}
        {hasNvmeof && (
          <div className="mt-6 p-4 bg-gray-50 rounded-lg border border-gray-200">
            <h5 className="font-medium text-gray-800 mb-2 flex items-center gap-2">
              <Info className="w-5 h-5 text-blue-600" />
              NVMe-oF Technology
            </h5>
            <div className="text-sm text-gray-600 text-left space-y-2">
              <p>
                <strong>NVMe-oF (NVMe over Fabrics)</strong> provides a high-performance interface for accessing storage over a network. 
                The {raidStatus ? getRaidLevelDisplayName(raidStatus.raid_level) : 'volume'} is exposed as one or more NVMe-oF targets.
              </p>
              <div className="grid grid-cols-1 md:grid-cols-2 gap-4 mt-3">
                <div>
                  <h6 className="font-medium text-gray-700 mb-1">Benefits:</h6>
                  <ul className="text-xs space-y-1 text-gray-600">
                    <li>• High throughput and low latency network access</li>
                    <li>• Efficiently utilizes RDMA or TCP/IP networks</li>
                    <li>• Scales to a large number of clients</li>
                    <li>• Bypasses kernel for I/O operations</li>
                    {raidStatus && <li>• Transparent RAID management</li>}
                  </ul>
                </div>
                <div>
                  <h6 className="font-medium text-gray-700 mb-1">Access Pattern:</h6>
                  <ul className="text-xs space-y-1 text-gray-600">
                    <li>• Client connects to NVMe-oF target via NQN</li>
                    <li>• Native NVMe namespace presentation</li>
                    <li>• SPDK handles all replica management</li>
                    <li>• Transparent failover on replica failure</li>
                    <li>• Automatic rebuild and recovery</li>
                    {raidStatus && <li>• {getRaidLevelDisplayName(raidStatus.raid_level)} fault tolerance</li>}
                  </ul>
                </div>
              </div>
              
              {selectedVolumeInfo.nvmeof_targets && selectedVolumeInfo.nvmeof_targets.length > 0 && (
                <div className="mt-3 p-2 bg-blue-50 rounded">
                  <h6 className="font-medium text-gray-700 mb-1">NVMe-oF Targets:</h6>
                  <div className="text-xs space-y-1">
                    {selectedVolumeInfo.nvmeof_targets.map((target, idx) => (
                      <div key={idx} className="flex justify-between">
                        <span>{target.nqn}:</span>
                        <span>{target.target_ip}:{target.target_port} ({target.transport})</span>
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
        
        {/* RAID Architecture Explanation */}
        {raidStatus && (
          <div className="mt-6 p-4 bg-white rounded-lg border border-gray-200">
            <h6 className="font-medium text-gray-800 mb-2 flex items-center gap-2">
              <Shield className="w-5 h-5 text-blue-600" />
              {getRaidLevelDisplayName(raidStatus.raid_level)} Architecture
            </h6>
            <div className="text-sm text-gray-600 grid grid-cols-1 md:grid-cols-2 gap-4">
              <div>
                <h6 className="font-medium text-gray-700 mb-1">Data Protection:</h6>
                <ul className="text-xs space-y-1">
                  {raidStatus.raid_level === 0 && (
                    <>
                      <li>• Data striped across all members</li>
                      <li>• Maximum performance, no redundancy</li>
                      <li>• Single member failure causes data loss</li>
                    </>
                  )}
                  {raidStatus.raid_level === 1 && (
                    <>
                      <li>• Data mirrored across all members</li>
                      <li>• Can tolerate up to N-1 member failures</li>
                      <li>• Read performance scales with members</li>
                      <li>• Write performance limited by slowest member</li>
                    </>
                  )}
                  {raidStatus.raid_level === 5 && (
                    <>
                      <li>• Data striped with distributed parity</li>
                      <li>• Can tolerate single member failure</li>
                      <li>• Good read performance, moderate write performance</li>
                      <li>• Parity overhead: 1/N capacity</li>
                    </>
                  )}
                  {raidStatus.raid_level === 6 && (
                    <>
                      <li>• Data striped with dual parity</li>
                      <li>• Can tolerate up to 2 member failures</li>
                      <li>• Enhanced fault tolerance</li>
                      <li>• Parity overhead: 2/N capacity</li>
                    </>
                  )}
                </ul>
              </div>
              <div>
                <h6 className="font-medium text-gray-700 mb-1">Current Status:</h6>
                <ul className="text-xs space-y-1">
                  <li>• Total Members: {raidStatus.num_members}</li>
                  <li>• Operational: {raidStatus.operational_members}</li>
                  <li>• Failed: {raidStatus.num_members - raidStatus.operational_members}</li>
                  <li>• Auto-rebuild: {raidStatus.auto_rebuild_enabled ? 'Enabled' : 'Disabled'}</li>
                  <li>• Rebuild Active: {raidStatus.rebuild_info ? 'Yes' : 'No'}</li>
                  {raidStatus.rebuild_info && (
                    <li>• Progress: {raidStatus.rebuild_info.progress_percentage.toFixed(1)}%</li>
                  )}
                </ul>
              </div>
            </div>
            
            {raidStatus.raid_level === 1 && (
              <div className="mt-3 p-2 bg-green-50 rounded text-xs text-green-700">
                <strong>RAID-1 Advantage:</strong> This volume can continue operating even if {raidStatus.num_members - 1} out of {raidStatus.num_members} members fail, 
                providing excellent fault tolerance for critical workloads.
              </div>
            )}
            
            {raidStatus.operational_members < raidStatus.num_members && (
              <div className="mt-3 p-2 bg-yellow-50 rounded text-xs text-yellow-700">
                <strong>Degraded Mode:</strong> {raidStatus.num_members - raidStatus.operational_members} member(s) are currently offline. 
                {raidStatus.auto_rebuild_enabled ? ' Auto-rebuild will begin when a replacement becomes available.' : ' Manual intervention may be required.'}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
};
