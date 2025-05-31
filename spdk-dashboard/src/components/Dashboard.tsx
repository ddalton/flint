import React, { useState } from 'react';
import type { DashboardData, VolumeFilter, DiskFilter } from '../hooks/useDashboardData';
import { DashboardHeader } from './layout/DashboardHeader';
import { StatCards } from './stats/StatCards';
import { TabNavigation } from './ui/TabNavigation';
import { VolumeStatusChart } from './charts/VolumeStatusChart';
import { DiskStatusChart } from './charts/DiskStatusChart';
import { RaidTopologyChart } from './charts/RaidTopologyChart';
import { VolumesTable } from './tables/VolumesTable';
import { DisksTable } from './tables/DisksTable';
import { FilteredNodesView } from './nodes/FilteredNodesView';

interface DashboardProps {
  data: DashboardData;
  loading: boolean;
  stats: {
    totalVolumes: number;
    faultedVolumes: number;
    rebuildingVolumes: number;
    localNVMeVolumes: number;
    totalDisks: number;
    healthyDisks: number;
    formattedDisks: number;
  };
  autoRefresh: boolean;
  onAutoRefreshChange: (enabled: boolean) => void;
  onRefresh: () => void;
  onLogout: () => void;
}

export const Dashboard: React.FC<DashboardProps> = ({
  data,
  loading,
  stats,
  autoRefresh,
  onAutoRefreshChange,
  onRefresh,
  onLogout
}) => {
  const [activeTab, setActiveTab] = useState('overview');
  const [volumeFilter, setVolumeFilter] = useState<VolumeFilter>('all');
  const [diskFilter, setDiskFilter] = useState<DiskFilter>(null);

  if (loading && data.volumes.length === 0) {
    return (
      <div className="flex justify-center items-center h-screen">
        <div className="animate-spin rounded-full h-16 w-16 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  const handleFilterClick = (filter: VolumeFilter) => {
    // If the same filter is clicked, clear it (toggle behavior)
    if (volumeFilter === filter) {
      setVolumeFilter('all');
    } else {
      setVolumeFilter(filter);
      // Automatically switch to volumes tab when a filter is applied
      if (filter !== 'all') {
        setActiveTab('volumes');
      }
    }
    // Clear disk filter when changing volume filter
    setDiskFilter(null);
  };

  const handleClearFilter = () => {
    setVolumeFilter('all');
  };

  const handleClearDiskFilter = () => {
    setDiskFilter(null);
  };

  const handleDiskClick = (diskId: string) => {
    // Set disk filter and switch to volumes tab
    setDiskFilter(diskId);
    setActiveTab('volumes');
  };

  // Don't reset filter when changing tabs - keep it persistent across all tabs
  const handleTabChange = (tabId: string) => {
    setActiveTab(tabId);
  };

  // Get volumes that are on the selected disk
  const getVolumesOnDisk = (diskId: string) => {
    const disk = data.disks.find(d => d.id === diskId);
    if (!disk) return [];
    
    // Return volumes that have replicas on this disk
    return data.volumes.filter(volume => 
      disk.provisioned_volumes.some(pv => pv.volume_id === volume.id)
    );
  };

  const renderTabContent = () => {
    switch (activeTab) {
      case 'overview':
        return (
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
            <VolumeStatusChart volumes={data.volumes} />
            <DiskStatusChart disks={data.disks} />
            <div className="lg:col-span-2">
              <RaidTopologyChart volumes={data.volumes} />
            </div>
          </div>
        );

      case 'volumes':
        return (
          <VolumesTable 
            volumes={diskFilter ? getVolumesOnDisk(diskFilter) : data.volumes}
            activeFilter={volumeFilter}
            diskFilter={diskFilter}
            onClearFilter={handleClearFilter}
            onClearDiskFilter={handleClearDiskFilter}
          />
        );

      case 'disks':
        return (
          <DisksTable 
            disks={data.disks} 
            volumes={data.volumes}
            stats={{
              totalDisks: stats.totalDisks,
              healthyDisks: stats.healthyDisks,
              formattedDisks: stats.formattedDisks
            }}
            volumeFilter={volumeFilter}
            onDiskClick={handleDiskClick}
          />
        );

      case 'nodes':
        return (
          <FilteredNodesView 
            data={data}
            activeFilter={volumeFilter}
            onClearFilter={handleClearFilter}
          />
        );

      default:
        return null;
    }
  };

  return (
    <div className="min-h-screen bg-gray-50">
      <DashboardHeader
        autoRefresh={autoRefresh}
        onAutoRefreshChange={onAutoRefreshChange}
        onRefresh={onRefresh}
        onLogout={onLogout}
      />

      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
        <StatCards 
          stats={stats} 
          activeFilter={volumeFilter}
          onFilterClick={handleFilterClick}
        />

        <div className="bg-white rounded-lg shadow mb-6">
          <TabNavigation 
            activeTab={activeTab} 
            onTabChange={handleTabChange} 
          />
          
          <div className="p-6">
            {renderTabContent()}
          </div>
        </div>
      </div>
    </div>
  );
};