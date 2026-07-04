import React, { useState, useMemo, useRef, useEffect } from 'react';
import {
  Database, Activity, Zap, Network, AlertTriangle, Shield, Settings, Search, ChevronDown
} from 'lucide-react';
import type { Volume, Disk } from '../../hooks/useDashboardData';
import { VolumeTopologyGraph } from './topology/VolumeTopologyGraph';
import { raidLevelDisplayName } from './topology/buildTopology';

// The volume topology page: searchable volume picker + the data-path graph
// (topology/VolumeTopologyGraph) + a one-line status chip summary. Every
// detail that used to be stacked inline — member cards, replica cards, NQN
// dumps, rebuild banners, RAID/NVMe-oF explainers — lives in the graph's
// details drawer now (select a node/edge, or "About this topology").

interface EnhancedRaidTopologyChartProps {
  volumes: Volume[];
  disks: Disk[];
}

export const EnhancedRaidTopologyChart: React.FC<EnhancedRaidTopologyChartProps> = ({ volumes, disks }) => {
  const [selectedVolume, setSelectedVolume] = useState(volumes[0]?.id || '');
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
  const handleVolumeSelect = (volumeId: string) => {
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

  // Fall back to the first volume when the picked one disappears (deleted
  // between refreshes, or the list arrived after mount).
  const selectedVolumeInfo = volumes.find(v => v.id === selectedVolume) ?? volumes[0];

  if (!selectedVolumeInfo) {
    return (
      <div className="bg-white rounded-lg shadow-lg p-6">
        <div className="flex items-center mb-4">
          <Activity className="w-6 h-6 text-blue-600 mr-2" />
          <h3 className="text-lg font-semibold">Volume Topology</h3>
        </div>
        <p className="text-sm text-gray-500">No volumes to display yet.</p>
      </div>
    );
  }

  const raidStatus = selectedVolumeInfo.raid_status;
  const hasNvmeof = selectedVolumeInfo.nvmeof_enabled ||
                    (selectedVolumeInfo.nvmeof_targets && selectedVolumeInfo.nvmeof_targets.length > 0) ||
                    selectedVolumeInfo.access_method === 'nvmeof';

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
                        onClick={() => handleVolumeSelect(volume.id)}
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
                          {volume.replica_statuses.some(r => r.sync && r.sync.sync_state !== 'in_sync') && (
                            <div className="ml-3 flex-shrink-0" title="Replica recovery in progress">
                              <AlertTriangle className="w-4 h-4 text-amber-500" />
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
        </div>
      </div>

      {/* Missing live metrics: honest empty-state note, graph still renders
          from replica statuses. */}
      {!raidStatus && (
        <div className="mb-4 p-4 bg-yellow-50 border-l-4 border-yellow-400">
          <div className="flex">
            <div className="flex-shrink-0">
              <AlertTriangle className="h-5 w-5 text-yellow-400" />
            </div>
            <div className="ml-3">
              <p className="text-sm text-yellow-700">
                RAID status is not available for this volume; the graph shows
                replica placement only.
              </p>
            </div>
          </div>
        </div>
      )}

      <VolumeTopologyGraph volume={selectedVolumeInfo} disks={disks} />

      {/* Status Summary */}
      <div className="mt-4 flex justify-center gap-4 flex-wrap">
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
            {raidLevelDisplayName(raidStatus.raid_level)} {raidStatus.state.toUpperCase()}
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
            <Settings className="w-4 h-4" />
            RAID Rebuild in Progress ({raidStatus.rebuild_info.progress_percentage.toFixed(1)}%)
          </span>
        )}
      </div>
    </div>
  );
};
