import React, { useState, useEffect, useCallback } from 'react';
import { apiFetch } from '../../api/client';
import {
  Database, Shield, Network, Activity,
  RefreshCw, AlertTriangle, X, HardDrive
} from 'lucide-react';
import type { ConsumerRaid, ReplicaStatus, Volume, SpdkVolumeDetails } from '../../hooks/useDashboardData';
import { SyncStateIndicator } from '../ui/SyncStateIndicator';
import { Skeleton } from '../ui/Skeleton';
import { ProgressBar } from '../ui/ProgressBar';
import { useEvents } from '../../hooks/useEvents';
import { EventTimelinePanel, HotRejoinWindowsPanel } from '../events/EventPanels';
import { Button, IconButton } from '../ui/Button';

// 2b volume detail: `volumeData` is LIVE — the parent derives it from the
// polled dashboard query each render, so the replica table and consumer-raid
// panel track a drill at the 2a adaptive cadence with no manual refresh.
// Only the SPDK details are a one-shot fetch, keyed by volume id.

interface VolumeDetailAPIProps {
  volumeId: string;
  volumeName: string;
  volumeData?: Volume;
  onClose: () => void;
}

const formatTime = (ts: string | null | undefined) => {
  if (!ts) return '—';
  const d = new Date(ts);
  return isNaN(d.getTime()) ? ts : d.toLocaleString();
};

// Shorten "epoch-pvc-c6896f1f-…-1264" to its distinguishing tail; full name
// stays in the title attribute.
const shortEpoch = (epoch: string | null | undefined) => {
  if (!epoch) return '—';
  const tail = epoch.split('-').at(-1) ?? '';
  return /^\d+$/.test(tail) ? `#${tail}` : epoch;
};

function ConsumerRaidCard({ raid }: { raid: ConsumerRaid }) {
  const degraded =
    raid.state === 'online' && raid.num_base_bdevs_operational < raid.num_base_bdevs;
  const chip =
    raid.state === 'online'
      ? degraded
        ? 'bg-amber-100 text-amber-800 border-amber-200'
        : 'bg-green-100 text-green-800 border-green-200'
      : 'bg-red-100 text-red-800 border-red-200';
  const label = degraded
    ? `degraded ${raid.num_base_bdevs_operational}/${raid.num_base_bdevs}`
    : raid.state === 'online'
    ? `online ${raid.num_base_bdevs_operational}/${raid.num_base_bdevs}`
    : raid.state;

  return (
    <div className="bg-gray-50 rounded-lg p-6">
      <div className="flex items-center justify-between mb-1">
        <h4 className="text-section flex items-center gap-2">
          <Shield className="w-5 h-5 text-blue-600" />
          Assembled on {raid.node}
        </h4>
        <span className={`px-3 py-1 rounded-full text-sm font-medium border ${chip}`}>
          {label}
        </span>
      </div>
      <p className="font-mono text-xs text-gray-500 mb-4 break-all">{raid.raid_name}</p>

      <div className="space-y-2">
        {raid.base_bdevs.map((member, i) => (
          <div key={i} className="flex items-center gap-2 bg-white border rounded px-3 py-2">
            <span
              aria-hidden="true"
              className={`w-2 h-2 rounded-full flex-shrink-0 ${
                member.is_configured ? 'bg-green-500' : 'bg-red-500'
              }`}
            />
            {member.is_configured ? (
              <>
                <span className="text-sm font-medium">
                  {member.replica_node ? `Replica on ${member.replica_node}` : 'Member'}
                </span>
                <span
                  className="ml-auto font-mono text-xs text-gray-500 truncate max-w-[50%]"
                  title={member.name ?? undefined}
                >
                  {member.name ?? '—'}
                </span>
              </>
            ) : (
              <span className="text-sm font-medium text-red-700">
                failed slot — leg removed from the raid
              </span>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

// The 2b per-replica table: node, sync state (2a indicator), epoch lag vs
// current_epoch, since/reason — straight from the PV replica-sync-state
// annotation the controller maintains.
function ReplicaSyncTable({ volume }: { volume: Volume }) {
  return (
    <div className="bg-white rounded-lg shadow overflow-hidden">
      <div className="px-4 py-3 border-b bg-gray-50 flex items-center gap-2">
        <Network className="w-5 h-5 text-gray-600" />
        <h4 className="font-semibold">Replicas</h4>
        <span className="ml-auto text-xs text-gray-500">
          current epoch{' '}
          <span className="font-mono" title={volume.current_epoch ?? undefined}>
            {shortEpoch(volume.current_epoch)}
          </span>
        </span>
      </div>
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              {['Node', 'Sync state', 'Last epoch', 'Since', 'Reason'].map((h) => (
                <th
                  key={h}
                  className="px-4 py-2 text-left text-xs font-medium text-gray-500 uppercase tracking-wider"
                >
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {volume.replica_statuses.map((replica) => (
              <tr key={replica.node} className="hover:bg-gray-50">
                <td className="px-4 py-3 whitespace-nowrap text-sm font-medium">
                  {replica.node}
                </td>
                <td className="px-4 py-3 whitespace-nowrap">
                  <SyncStateIndicator sync={replica.sync} />
                </td>
                <td
                  className="px-4 py-3 whitespace-nowrap font-mono text-sm text-gray-600"
                  title={replica.sync?.last_epoch ?? undefined}
                >
                  {shortEpoch(replica.sync?.last_epoch)}
                </td>
                <td className="px-4 py-3 whitespace-nowrap text-sm text-gray-500">
                  {formatTime(replica.sync?.since)}
                </td>
                <td className="px-4 py-3 text-sm text-gray-500">
                  {replica.sync?.reason ?? '—'}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

// Legacy per-replica cards for volumes without a sync record
// (single-replica volumes have none by design).
function LegacyReplicaCards({ replicas }: { replicas: ReplicaStatus[] }) {
  return (
    <div className="space-y-4">
      {replicas.map((replica, index) => (
        <div key={index} className="bg-gray-50 rounded-lg p-6">
          <div className="flex items-center justify-between mb-4">
            <h4 className="text-section">Replica on {replica.node}</h4>
            <span className={`px-3 py-1 rounded-full text-sm font-medium ${
              replica.status === 'healthy' || replica.status === 'active'
                ? 'bg-green-100 text-green-800'
                : replica.status === 'rebuilding'
                ? 'bg-orange-100 text-orange-800'
                : 'bg-red-100 text-red-800'
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
              <p className="font-medium text-xs">{formatTime(replica.last_io_timestamp)}</p>
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
}

// Per-volume embedding of the 2c panels (`/api/events?volume=`).
function VolumeEventsTab({ volumeId }: { volumeId: string }) {
  const { data, isLoading, isError, error } = useEvents(volumeId);

  if (isLoading) {
    return <div className="p-8 text-center text-gray-500">Loading volume events…</div>;
  }
  if (isError) {
    return (
      <div className="p-4 bg-red-50 border border-red-200 rounded-lg text-red-800 text-sm">
        Failed to load events: {(error as Error).message}
      </div>
    );
  }
  return (
    <div className="space-y-6">
      <HotRejoinWindowsPanel windows={data?.windows ?? []} showVolume={false} />
      <EventTimelinePanel events={data?.events ?? []} showVolume={false} />
    </div>
  );
}

export const VolumeDetailAPI: React.FC<VolumeDetailAPIProps> = ({
  volumeId,
  volumeName,
  volumeData,
  onClose
}) => {
  const [activeTab, setActiveTab] = useState('overview');
  const [spdkDetails, setSpdkDetails] = useState<SpdkVolumeDetails | undefined>(undefined);
  const [spdkLoading, setSpdkLoading] = useState(false);

  // One-shot per volume: keyed on the id, NOT on volumeData identity — the
  // live poll produces a fresh object every cycle and must not refetch this.
  const targetNode = volumeData?.nodes[0];
  const fetchSpdkDetails = useCallback(async () => {
    if (!targetNode) return;
    setSpdkLoading(true);
    try {
      const response = await apiFetch(`/api/volumes/${volumeId}/spdk?node=${targetNode}`);
      if (response.ok) {
        setSpdkDetails(await response.json());
      } else {
        console.warn('Failed to fetch SPDK details:', response.status);
      }
    } catch (spdkError) {
      console.warn('Error fetching SPDK details:', spdkError);
    } finally {
      setSpdkLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [volumeId]);

  useEffect(() => {
    fetchSpdkDetails();
  }, [fetchSpdkDetails]);

  if (!volumeData) {
    return (
      <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
        <div className="bg-white rounded-lg p-8 max-w-md w-full mx-4">
          <div className="text-center">
            <AlertTriangle className="w-12 h-12 text-red-500 mx-auto mb-4" />
            <h3 className="text-section text-gray-900 mb-2">Volume Not Available</h3>
            <p className="text-sm text-gray-600 mb-4">
              {volumeName} is no longer present in the dashboard data.
            </p>
            <Button onClick={onClose}>Close</Button>
          </div>
        </div>
      </div>
    );
  }

  const volume = volumeData;
  const consumerRaids = volume.consumer_raids ?? [];
  const hasSyncData = volume.replica_statuses.some((r) => r.sync != null);

  const tabs = [
    { id: 'overview', name: 'Overview', icon: Database },
    { id: 'replicas', name: 'Replicas', icon: Network },
    { id: 'raid', name: 'RAID', icon: Shield },
    { id: 'events', name: 'Events', icon: Activity },
    { id: 'spdk', name: 'SPDK Details', icon: HardDrive },
  ];

  const renderOverviewTab = () => (
    <div className="space-y-6">
      <div className="bg-gray-50 rounded-lg p-6">
        <h4 className="text-section mb-4">Volume Summary</h4>
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
            <p className="text-sm text-gray-600">Replicas in sync</p>
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
            <Activity className="w-8 h-8 text-indigo-600 mr-3" />
            <div>
              <p className="text-sm font-medium">Current Epoch</p>
              <p
                className="text-lg font-bold text-indigo-600 font-mono"
                title={volume.current_epoch ?? undefined}
              >
                {shortEpoch(volume.current_epoch)}
              </p>
            </div>
          </div>
        </div>

        <div className="bg-white border rounded-lg p-4">
          <div className="flex items-center">
            <Shield className="w-8 h-8 text-purple-600 mr-3" />
            <div>
              <p className="text-sm font-medium">Consumers</p>
              <p className="text-lg font-bold text-purple-600">
                {consumerRaids.length === 0
                  ? 'none'
                  : consumerRaids.map((r) => r.node).join(', ')}
              </p>
            </div>
          </div>
        </div>
      </div>

      {volume.pvc_info && (
        <div className="bg-white border rounded-lg p-4">
          <h4 className="text-sm font-semibold text-gray-700 mb-3">Kubernetes Claim</h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-sm">
            <div>
              <p className="text-gray-600">PVC</p>
              <p className="font-medium">{volume.pvc_info.name}</p>
            </div>
            <div>
              <p className="text-gray-600">Namespace</p>
              <p className="font-medium">{volume.pvc_info.namespace}</p>
            </div>
            <div>
              <p className="text-gray-600">Storage Class</p>
              <p className="font-medium">{volume.pvc_info.storage_class}</p>
            </div>
            <div>
              <p className="text-gray-600">Created</p>
              <p className="font-medium text-xs">{formatTime(volume.pvc_info.creation_timestamp)}</p>
            </div>
          </div>
        </div>
      )}
    </div>
  );

  const renderReplicasTab = () =>
    hasSyncData ? (
      <ReplicaSyncTable volume={volume} />
    ) : volume.replica_statuses.length > 0 ? (
      <LegacyReplicaCards replicas={volume.replica_statuses} />
    ) : (
      <div className="text-center py-8">
        <Network className="w-12 h-12 text-gray-400 mx-auto mb-4" />
        <p className="text-gray-600">No replica information available</p>
      </div>
    );

  const renderRaidTab = () =>
    consumerRaids.length > 0 ? (
      <div className="space-y-6">
        {consumerRaids.map((raid) => (
          <ConsumerRaidCard key={raid.node} raid={raid} />
        ))}
      </div>
    ) : (
      <div className="text-center py-8">
        <Shield className="w-12 h-12 text-gray-400 mx-auto mb-4" />
        <p className="text-gray-600">
          Not assembled on any node — the volume has no active consumer.
        </p>
        <p className="text-sm text-gray-500 mt-2">
          When a workload stages this volume, the consumer node assembles its
          raid from the replica legs and it appears here.
        </p>
      </div>
    );

  const renderSpdkTab = () => {
    if (spdkLoading) {
      return (
        <div className="flex items-center justify-center py-12">
          <div className="w-full space-y-3" role="status" aria-label="Loading volume details">
            <Skeleton className="h-6 w-48" />
            <Skeleton className="h-32 w-full" />
          </div>
          <span className="ml-3 text-gray-600">Loading SPDK details...</span>
        </div>
      );
    }

    const spdkData = spdkDetails;
    if (!spdkData) {
      return (
        <div className="text-center py-12">
          <HardDrive className="w-16 h-16 text-gray-400 mx-auto mb-4" />
          <h3 className="text-lg font-medium text-gray-900 mb-2">No SPDK Details Available</h3>
          <p className="text-gray-600 mb-4">Unable to retrieve SPDK logical volume information for this volume.</p>
          <Button variant="primary" icon={RefreshCw} onClick={fetchSpdkDetails}>
            Retry
          </Button>
        </div>
      );
    }

    return (
      <div className="space-y-6">
        {/* SPDK Volume Information */}
        <div className="bg-gradient-to-r from-blue-50 to-indigo-50 rounded-lg p-6 border border-blue-200">
          <h4 className="text-section mb-4 flex items-center gap-2">
            <HardDrive className="w-5 h-5 text-blue-600" />
            SPDK Logical Volume
          </h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
            <div>
              <p className="text-sm text-gray-600">Volume Name</p>
              <p className="font-mono text-sm bg-white px-2 py-1 rounded border">{spdkData.volume_name}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Volume UUID</p>
              <p className="font-mono text-xs bg-white px-2 py-1 rounded border break-all">{spdkData.volume_uuid}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Node</p>
              <p className="font-medium">{spdkData.node}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">SPDK Bdev Name</p>
              <p className="font-mono text-sm bg-white px-2 py-1 rounded border">{spdkData.bdev_name}</p>
            </div>
          </div>
        </div>

        {/* Volume Size and Allocation */}
        <div className="bg-white border rounded-lg p-6">
          <h4 className="text-section mb-4">Volume Allocation</h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-4">
            <div>
              <p className="text-sm text-gray-600">Size (GB)</p>
              <p className="text-lg font-bold text-blue-600">{(spdkData.size_gb || 0).toFixed(2)}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Size (Bytes)</p>
              <p className="font-mono text-sm">{(spdkData.size_bytes || 0).toLocaleString()}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Allocated Clusters</p>
              <p className="font-mono text-sm">{(spdkData.allocated_clusters || 0).toLocaleString()}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Cluster Size</p>
              <p className="font-mono text-sm">{((spdkData.cluster_size || 0) / 1024).toLocaleString()} KB</p>
            </div>
          </div>

          {/* Volume Properties */}
          <div className="grid grid-cols-3 gap-4">
            <div className="flex items-center gap-2">
              <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                spdkData.is_thin_provisioned ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
              }`}>
                {spdkData.is_thin_provisioned ? 'Thin Provisioned' : 'Thick Provisioned'}
              </span>
            </div>
            <div className="flex items-center gap-2">
              {spdkData.is_clone && (
                <span className="inline-flex px-2 py-1 text-xs font-semibold rounded-full bg-purple-100 text-purple-800">
                  Clone
                </span>
              )}
            </div>
            <div className="flex items-center gap-2">
              {spdkData.is_snapshot && (
                <span className="inline-flex px-2 py-1 text-xs font-semibold rounded-full bg-orange-100 text-orange-800">
                  Snapshot
                </span>
              )}
            </div>
          </div>
        </div>

        {/* LVS Information */}
        <div className="bg-white border rounded-lg p-6">
          <h4 className="text-section mb-4 flex items-center gap-2">
            <Database className="w-5 h-5 text-indigo-600" />
            Logical Volume Store (LVS)
          </h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-4">
            <div>
              <p className="text-sm text-gray-600">LVS Name</p>
              <p className="font-mono text-sm bg-gray-50 px-2 py-1 rounded border">{spdkData.lvs_name}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">LVS UUID</p>
              <p className="font-mono text-xs bg-gray-50 px-2 py-1 rounded border break-all">{spdkData.lvs_uuid}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Base Block Device</p>
              <p className="font-mono text-sm bg-gray-50 px-2 py-1 rounded border">{spdkData.lvs_base_bdev}</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Block Size</p>
              <p className="font-mono text-sm">{spdkData.lvs_block_size || 0} bytes</p>
            </div>
          </div>

          {/* LVS Capacity */}
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-4">
            <div>
              <p className="text-sm text-gray-600">Total Capacity</p>
              <p className="text-lg font-bold text-indigo-600">{(spdkData.lvs_capacity_gb || 0).toFixed(1)} GB</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Used Space</p>
              <p className="text-lg font-bold text-orange-600">{(spdkData.lvs_used_gb || 0).toFixed(1)} GB</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Free Space</p>
              <p className="text-lg font-bold text-green-600">{((spdkData.lvs_capacity_gb || 0) - (spdkData.lvs_used_gb || 0)).toFixed(1)} GB</p>
            </div>
            <div>
              <p className="text-sm text-gray-600">Utilization</p>
              <p className="text-lg font-bold text-gray-700">{(spdkData.lvs_utilization_pct || 0).toFixed(1)}%</p>
            </div>
          </div>

          {/* LVS Usage Bar */}
          <div className="mb-4">
            <div className="flex items-center justify-between mb-2">
              <span className="text-sm font-medium text-gray-700">LVS Space Usage</span>
              <span className="text-sm text-gray-500">{(spdkData.lvs_utilization_pct || 0).toFixed(1)}% used</span>
            </div>
            <ProgressBar
              value={Math.min(spdkData.lvs_utilization_pct || 0, 100)}
              label="LVS space usage"
              valueText={`${(spdkData.lvs_utilization_pct || 0).toFixed(1)}% used`}
              className="w-full"
            />
          </div>

          {/* Cluster Information */}
          <div className="grid grid-cols-3 gap-4 text-sm">
            <div className="bg-gray-50 p-3 rounded">
              <p className="text-gray-600">Total Clusters</p>
              <p className="font-mono font-semibold">{(spdkData.lvs_total_clusters || 0).toLocaleString()}</p>
            </div>
            <div className="bg-gray-50 p-3 rounded">
              <p className="text-gray-600">Free Clusters</p>
              <p className="font-mono font-semibold text-green-600">{(spdkData.lvs_free_clusters || 0).toLocaleString()}</p>
            </div>
            <div className="bg-gray-50 p-3 rounded">
              <p className="text-gray-600">Used Clusters</p>
              <p className="font-mono font-semibold text-orange-600">{((spdkData.lvs_total_clusters || 0) - (spdkData.lvs_free_clusters || 0)).toLocaleString()}</p>
            </div>
          </div>
        </div>

        {/* Additional Information */}
        <div className="bg-gray-50 rounded-lg p-4">
          <div className="flex items-center justify-between">
            <span className="text-sm text-gray-600">Last Updated</span>
            <span className="text-sm font-mono text-gray-800">
              {new Date(spdkData.last_updated).toLocaleString()}
            </span>
          </div>
          {spdkData.bdev_alias && (
            <div className="flex items-center justify-between mt-2">
              <span className="text-sm text-gray-600">SPDK Alias</span>
              <span className="text-sm font-mono text-gray-800">{spdkData.bdev_alias}</span>
            </div>
          )}
        </div>
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
            <h2 className="text-section">Volume Details: {volumeName}</h2>
            <span className="text-xs text-gray-400 hidden md:inline">live</span>
          </div>
          <IconButton icon={X} aria-label="Close" onClick={onClose} />
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
          {activeTab === 'replicas' && renderReplicasTab()}
          {activeTab === 'raid' && renderRaidTab()}
          {activeTab === 'events' && <VolumeEventsTab volumeId={volumeId} />}
          {activeTab === 'spdk' && renderSpdkTab()}
        </div>
      </div>
    </div>
  );
};
