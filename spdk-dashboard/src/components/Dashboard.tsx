import React, { useState } from 'react';
import type { DashboardData, VolumeFilter } from '../hooks/useDashboardData';
import { DashboardHeader } from './layout/DashboardHeader';
import { StatCards } from './stats/StatCards';
import { TabNavigation } from './ui/TabNavigation';
import { VolumeStatusChart } from './charts/VolumeStatusChart';
import { DiskStatusChart } from './charts/DiskStatusChart';
import { RaidTopologyChart } from './charts/RaidTopologyChart';
import { VolumesTable } from './tables/VolumesTable';
import { DisksTable } from './tables/DisksTable';
import { NodeDetailView } from './nodes/NodeDetailView';

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
  };

  const handleClearFilter = () => {
    setVolumeFilter('all');
  };

  // Reset filter when changing tabs away from volumes
  const handleTabChange = (tabId: string) => {
    setActiveTab(tabId);
    if (tabId !== 'volumes') {
      setVolumeFilter('all');
    }
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
            volumes={data.volumes} 
            activeFilter={volumeFilter}
            onClearFilter={handleClearFilter}
          />
        );

      case 'disks':
        return (
          <DisksTable 
            disks={data.disks} 
            stats={{
              totalDisks: stats.totalDisks,
              healthyDisks: stats.healthyDisks,
              formattedDisks: stats.formattedDisks
            }}
          />
        );

      case 'nodes':
        return (
          <div className="space-y-6">
            {data.nodes.map((node) => {
              const nodeDisks = data.disks.filter(d => d.node === node);
              const nodeVolumes = data.volumes.filter(v => v.nodes.includes(node));
              const healthyDisks = nodeDisks.filter(d => d.healthy).length;
              const totalCapacity = nodeDisks.reduce((sum, disk) => sum + disk.capacity_gb, 0);
              const totalAllocated = nodeDisks.reduce((sum, disk) => sum + disk.allocated_space, 0);
              const totalFree = totalCapacity - totalAllocated;
              
              return (
                <NodeDetailView 
                  key={node} 
                  node={node}
                  nodeDisks={nodeDisks}
                  nodeVolumes={nodeVolumes}
                  healthyDisks={healthyDisks}
                  totalCapacity={totalCapacity}
                  totalAllocated={totalAllocated}
                  totalFree={totalFree}
                />
              );
            })}
          </div>
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