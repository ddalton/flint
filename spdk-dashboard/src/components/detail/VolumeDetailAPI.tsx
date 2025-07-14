import React, { useState, useEffect, useCallback } from 'react';
import { 
  Database, Activity, Settings, Cable, Shield, Network, 
  RefreshCw, AlertTriangle, CheckCircle, Clock, Info, X
} from 'lucide-react';
import type { Volume } from '../../hooks/useDashboardData';

interface VolumeDetailAPIProps {
  volumeId: string;
  volumeName: string;
  volumeData?: Volume;
  onClose: () => void;
}

interface VolumeDetails {
  volume: any;
  raidDetails: any;
  nvmeofDetails: any; // Changed from vhostDetails
  metrics: any;
}

export const VolumeDetailAPI: React.FC<VolumeDetailAPIProps> = ({ 
  volumeName, 
  volumeData,
  onClose 
}) => {
  const [details, setDetails] = useState<VolumeDetails | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState('overview');
  const [refreshing, setRefreshing] = useState(false);

  const fetchVolumeDetails = useCallback(async () => {
    try {
      setRefreshing(true);
      setError(null);

      // Use the real volume data that was passed in
      if (volumeData) {
        setDetails({
          volume: volumeData, // Use the real volume data
          raidDetails: volumeData.raid_status || null,
          nvmeofDetails: volumeData.nvmeof_targets || null,
          metrics: null
        });
      } else {
        // Fallback to API or mock data only if no volumeData provided
        throw new Error('No volume data provided');
      }

    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch volume details');
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, [volumeData]);

  useEffect(() => {
    fetchVolumeDetails();
  }, [fetchVolumeDetails]);

  const handleRefresh = () => {
    fetchVolumeDetails();
  };

  if (loading && !details) {
    return (
      <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
        <div className="bg-white rounded-lg p-8 max-w-md w-full mx-4">
          <div className="flex items-center justify-center">
            <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
            <span className="ml-3 text-lg">Loading volume details...</span>
          </div>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
        <div className="bg-white rounded-lg p-8 max-w-md w-full mx-4">
          <div className="text-center">
            <AlertTriangle className="w-12 h-12 text-red-500 mx-auto mb-4" />
            <h3 className="text-lg font-semibold text-gray-900 mb-2">Error Loading Volume</h3>
            <p className="text-sm text-gray-600 mb-4">{error}</p>
            <div className="flex gap-3 justify-center">
              <button
                onClick={handleRefresh}
                className="px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700"
              >
                Retry
              </button>
              <button
                onClick={onClose}
                className="px-4 py-2 bg-gray-300 text-gray-700 rounded-md hover:bg-gray-400"
              >
                Close
              </button>
            </div>
          </div>
        </div>
      </div>
    );
  }

  const tabs = [
    { id: 'overview', name: 'Overview', icon: Database },
    { id: 'raid', name: 'RAID Status', icon: Shield },
    { id: 'nvmeof', name: 'NVMe-oF', icon: Network }, // Changed from VHost-NVMe
    { id: 'replicas', name: 'Replicas', icon: Network }
  ];

  const renderOverviewTab = () => {
    const volume = details?.volume;
    if (!volume) return <div>No volume data available</div>;

    return (
      <div className="space-y-6">
        <div className="bg-gray-50 rounded-lg p-6">
          <h4 className="text-lg font-semibold mb-4">Volume Summary</h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
            <div>
              <p className="text-sm text-gray-600">Name</p>
              <p className="font-medium">{volume.name}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Size</p>
              <p className="font-medium">{volume.size}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">State</p>
              <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                volume.state === 'Healthy' ? 'bg-green-100 text-green-800' :
                volume.state === 'Degraded' ? 'bg-yellow-100 text-yellow-800' :
                'bg-red-100 text-red-800'
              }`}>
                {volume.state}
              </span>
            </div>
            <div>
              <p className="text-sm text-gray-600">Replicas</p>
              <p className="font-medium">{volume.active_replicas}/{volume.replicas}</p>
            </div>
          </div>
        </div>

        <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
          <div className="bg-white border rounded-lg p-4">
            <div className="flex items-center">
              <Database className="w-8 h-8 text-blue-600 mr-3" />
              <div>
                <p className="text-sm font-medium">Volume Health</p>
                <p className={`text-lg font-bold ${
                  volume.state === 'Healthy' ? 'text-green-600' :
                  volume.state === 'Degraded' ? 'text-yellow-600' :
                  'text-red-600'
                }`}>
                  {volume.state}
                </p>
              </div>
            </div>
          </div>

          <div className="bg-white border rounded-lg p-4">
            <div className="flex items-center">
              <Network className="w-8 h-8 text-purple-600 mr-3" />
              <div>
                <p className="text-sm font-medium">Active Replicas</p>
                <p className="text-lg font-bold text-purple-600">
                  {volume.active_replicas}/{volume.replicas}
                </p>
              </div>
            </div>
          </div>

          <div className="bg-white border rounded-lg p-4">
            <div className="flex items-center">
              <Network className="w-8 h-8 text-indigo-600 mr-3" />
              <div>
                <p className="text-sm font-medium">NVMe-oF</p>
                <p className={`text-lg font-bold ${
                  volume.nvmeof_enabled ? 'text-green-600' : 'text-gray-400'
                }`}>
                  {volume.nvmeof_enabled ? 'Enabled' : 'Disabled'}
                </p>
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  };

  const renderRaidTab = () => {
    const raidData = details?.raidDetails;
    if (!raidData?.raid_bdevs?.length) {
      return (
        <div className="text-center py-8">
          <Shield className="w-12 h-12 text-gray-400 mx-auto mb-4" />
          <p className="text-gray-600">No RAID information available</p>
        </div>
      );
    }

    return (
      <div className="space-y-6">
        {raidData.raid_bdevs.map((raidBdev: any, index: number) => (
          <div key={index} className="bg-gray-50 rounded-lg p-6">
            <div className="flex items-center justify-between mb-4">
              <h4 className="text-lg font-semibold flex items-center gap-2">
                <Shield className="w-5 h-5 text-blue-600" />
                RAID Device: {raidBdev.name}
              </h4>
              <span className={`px-3 py-1 rounded-full text-sm font-medium ${
                raidBdev.state === 'online' ? 'bg-green-100 text-green-800' :
                raidBdev.state === 'degraded' ? 'bg-yellow-100 text-yellow-800' :
                'bg-red-100 text-red-800'
              }`}>
                {raidBdev.state?.toUpperCase()}
              </span>
            </div>

            <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-6">
              <div>
                <p className="text-sm text-gray-600">RAID Level</p>
                <p className="font-medium">RAID-{raidBdev.raid_level}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Total Members</p>
                <p className="font-medium">{raidBdev.num_base_bdevs}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Operational</p>
                <p className="font-medium">{raidBdev.num_base_bdevs_operational}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Node</p>
                <p className="font-medium">{raidBdev.node}</p>
              </div>
            </div>

            {raidBdev.rebuild_info && (
              <div className="mb-6 p-4 bg-orange-50 border border-orange-200 rounded-lg">
                <h5 className="font-medium text-orange-800 mb-2 flex items-center gap-2">
                  <Settings className="w-4 h-4 animate-spin" />
                  Rebuild in Progress
                </h5>
                <div className="space-y-2">
                  <div className="flex justify-between text-sm">
                    <span>Progress:</span>
                    <span className="font-medium">{raidBdev.rebuild_info.progress_percentage?.toFixed(1)}%</span>
                  </div>
                  <div className="w-full bg-gray-200 rounded-full h-2">
                    <div 
                      className="bg-orange-500 h-2 rounded-full transition-all duration-300" 
                      style={{ width: `${raidBdev.rebuild_info.progress_percentage || 0}%` }}
                    />
                  </div>
                </div>
              </div>
            )}
          </div>
        ))}
      </div>
    );
  };

  const renderNvmeofTab = () => { // Renamed from renderVHostTab
    const nvmeofData = details?.nvmeofDetails;
    if (!nvmeofData?.nvmeof_subsystems?.length) {
      return (
        <div className="text-center py-8">
          <Network className="w-12 h-12 text-gray-400 mx-auto mb-4" />
          <p className="text-gray-600">No NVMe-oF subsystems found</p>
        </div>
      );
    }

    return (
      <div className="space-y-6">
        {nvmeofData.nvmeof_subsystems.map((subsystem: any, index: number) => (
          <div key={index} className="bg-gray-50 rounded-lg p-6">
            <div className="flex items-center justify-between mb-4">
              <h4 className="text-lg font-semibold flex items-center gap-2">
                <Network className="w-5 h-5 text-indigo-600" />
                Subsystem: {subsystem.nqn}
              </h4>
              <span className={`px-3 py-1 rounded-full text-sm font-medium ${
                subsystem.state === 'active' ? 'bg-green-100 text-green-800' : 'bg-gray-100 text-gray-800'
              }`}>
                {subsystem.state ? subsystem.state.toUpperCase() : 'UNKNOWN'}
              </span>
            </div>

            <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
              <div>
                <p className="text-sm text-gray-600">Node</p>
                <p className="font-medium">{subsystem.node}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Subtype</p>
                <p className="font-medium">{subsystem.subtype?.toUpperCase()}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Allow Any Host</p>
                <p className="font-medium">{subsystem.allow_any_host ? 'Yes' : 'No'}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Namespaces</p>
                <p className="font-medium">{subsystem.namespaces?.length || 0}</p>
              </div>
            </div>
          </div>
        ))}
      </div>
    );
  };

  const renderReplicasTab = () => {
    const volume = details?.volume;
    if (!volume?.replica_statuses?.length) {
      return (
        <div className="text-center py-8">
          <Network className="w-12 h-12 text-gray-400 mx-auto mb-4" />
          <p className="text-gray-600">No replica information available</p>
        </div>
      );
    }

    return (
      <div className="space-y-4">
        {volume.replica_statuses.map((replica: any, index: number) => (
          <div key={index} className="bg-gray-50 rounded-lg p-6">
            <div className="flex items-center justify-between mb-4">
              <h4 className="text-lg font-semibold">Replica on {replica.node}</h4>
              <span className={`px-3 py-1 rounded-full text-sm font-medium ${
                replica.status === 'healthy' ? 'bg-green-100 text-green-800' :
                replica.status === 'rebuilding' ? 'bg-orange-100 text-orange-800' :
                'bg-red-100 text-red-800'
              }`}>
                {replica.status}
              </span>
            </div>

            <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
              <div>
                <p className="text-sm text-gray-600">Access Method</p>
                <p className="font-medium">{replica.access_method}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Local NVMe</p>
                <p className="font-medium">{replica.is_local ? 'Yes' : 'No'}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">RAID Slot</p>
                <p className="font-medium">{replica.raid_member_slot ?? 'N/A'}</p>
              </div>
              <div>
                <p className="text-sm text-gray-600">Last I/O</p>
                <p className="font-medium text-xs">
                  {replica.last_io_timestamp ? new Date(replica.last_io_timestamp).toLocaleString() : 'N/A'}
                </p>
              </div>
            </div>

            {replica.nvmf_target && (
              <div className="mt-4 p-3 bg-white rounded border">
                <h5 className="font-medium mb-2">NVMe-oF Target</h5>
                <div className="text-sm space-y-1">
                  <div>IP: {replica.nvmf_target.target_ip}:{replica.nvmf_target.target_port}</div>
                  <div>NQN: {replica.nvmf_target.nqn}</div>
                  <div>Transport: {replica.nvmf_target.transport_type}</div>
                </div>
              </div>
            )}
          </div>
        ))}
      </div>
    );
  };

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div className="bg-white rounded-lg max-w-6xl w-full max-h-[90vh] mx-4 flex flex-col">
        {/* Header */}
        <div className="flex items-center justify-between p-6 border-b">
          <div className="flex items-center gap-3">
            <Database className="w-6 h-6 text-blue-600" />
            <h2 className="text-xl font-semibold">Volume Details: {volumeName}</h2>
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={handleRefresh}
              disabled={refreshing}
              className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md disabled:opacity-50"
            >
              <RefreshCw className={`w-5 h-5 ${refreshing ? 'animate-spin' : ''}`} />
            </button>
            <button
              onClick={onClose}
              className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md"
            >
              <X className="w-5 h-5" />
            </button>
          </div>
        </div>

        {/* Tabs */}
        <div className="border-b">
          <nav className="flex space-x-8 px-6">
            {tabs.map((tab) => (
              <button
                key={tab.id}
                onClick={() => setActiveTab(tab.id)}
                className={`flex items-center gap-2 py-4 px-1 border-b-2 font-medium text-sm ${
                  activeTab === tab.id
                    ? 'border-blue-500 text-blue-600'
                    : 'border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300'
                }`}
              >
                <tab.icon className="w-4 h-4" />
                {tab.name}
              </button>
            ))}
          </nav>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-auto p-6">
          {activeTab === 'overview' && renderOverviewTab()}
          {activeTab === 'raid' && renderRaidTab()}
          {activeTab === 'nvmeof' && renderNvmeofTab()}
          {activeTab === 'replicas' && renderReplicasTab()}
        </div>
      </div>
    </div>
  );
};
