import React, { lazy, Suspense, useState, useMemo, useEffect } from 'react';
import { useSearchParams } from 'react-router';
import { CheckCircle, X, Filter, HardDrive, AlertTriangle, XCircle, RefreshCw, Info, ShieldAlert, Database } from 'lucide-react';
import type { Disk, Volume, VolumeFilter, DiskFilter } from '../../hooks/useDashboardData';
import { filterVolumesByType, isVolumeRecovering } from '../../hooks/useDashboardData';
import { volumeFilterDisplay, volumeStateStyle } from '../ui/status';
import { VolumeSyncSummary } from '../ui/SyncStateIndicator';
import { useOperations } from '../../contexts/OperationsContext';
import { Button } from '../ui/Button';
import { Pagination } from '../ui/Pagination';

// The detail modal is its own chunk — most sessions never open it.
const VolumeDetailAPI = lazy(() =>
  import('../detail/VolumeDetailAPI').then(m => ({ default: m.VolumeDetailAPI })));

interface VolumesTableProps {
  volumes: Volume[];
  disks?: Disk[];
  // The cluster's node names (the aggregate's `nodes`): a replica whose
  // node is not among them sits on a node that has left the cluster, which
  // the sync record alone does not say (dash1-aws-3 deleted, 2026-10-27:
  // the idle r3 volume kept its leg there "in_sync").
  nodes?: string[];
  activeFilter?: VolumeFilter;
  diskFilter?: DiskFilter;
  onClearFilter?: () => void;
  onClearDiskFilter?: () => void;
  onReplicaClick?: (volumeId: string, volumeName: string) => void;
}

export const VolumesTable: React.FC<VolumesTableProps> = ({ 
  disks,
  nodes,
  volumes,
  activeFilter,
  diskFilter,
  onClearFilter,
  onClearDiskFilter,
  onReplicaClick,
}) => {
  // Selection is by id in the URL (?volume=) — the detail modal derives its
  // volume from the live query data each render, so it tracks a drill at the
  // polling cadence instead of freezing a snapshot taken at click time, and
  // the open modal survives refresh / can be pasted as a link.
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedVolumeId = searchParams.get('volume');
  const setSelectedVolumeId = (id: string | null) => {
    setSearchParams(prev => {
      const next = new URLSearchParams(prev);
      if (id) next.set('volume', id);
      else next.delete('volume');
      return next;
    });
  };
  const { setDialogVisible } = useOperations();

  // Pagination state
  const [currentPage, setCurrentPage] = useState(1);
  const [pageSize, setPageSize] = useState(25);

  useEffect(() => {
    setDialogVisible(selectedVolumeId !== null);
  }, [selectedVolumeId, setDialogVisible]);

  const filteredVolumes = useMemo(() => {
    let result = volumes;

    // Apply volume filter first — the shared predicate (status/recovery
    // semantics identical to the stat cards). 'orphaned' matches nothing
    // here: orphans are per-disk lvols, listed on the Disks tab.
    if (activeFilter && activeFilter !== 'all') {
      result = filterVolumesByType(result, activeFilter);
    }

    // Apply disk filter if present
    if (diskFilter && disks) {
      // Find the disk object
      const selectedDisk = disks.find(d => d.id === diskFilter);
      if (selectedDisk) {
        result = result.filter(volume => {
          // Check if this volume is in the disk's provisioned_volumes
          return selectedDisk.provisioned_volumes.some(pv => 
            pv.volume_id === volume.id
          );
        });
      }
    }

    return result;
  }, [volumes, activeFilter, diskFilter, disks]);

  // Calculate pagination
  const totalPages = Math.ceil(filteredVolumes.length / pageSize);
  const paginatedVolumes = filteredVolumes.slice((currentPage - 1) * pageSize, currentPage * pageSize);

  // Reset to first page when filters change
  React.useEffect(() => {
    setCurrentPage(1);
  }, [activeFilter, diskFilter, pageSize]);

  const getFilterDisplayName = (filter: VolumeFilter) => volumeFilterDisplay(filter).name;

  const getVolumeStateInfo = volumeStateStyle;



  // Sort volumes by state priority (Failed -> Degraded -> Healthy)
  const sortedVolumes = paginatedVolumes.sort((a, b) => {
    const aInfo = getVolumeStateInfo(a.state);
    const bInfo = getVolumeStateInfo(b.state);
    return bInfo.priority - aInfo.priority; // Reverse sort for priority
  });

  const handleVolumeNameClick = (volume: Volume) => {
    setSelectedVolumeId(volume.id);
  };

  const selectedVolumeDetail =
    volumes.find(v => v.id === selectedVolumeId) ?? null;

  const goToPage = (page: number) => {
    setCurrentPage(Math.max(1, Math.min(totalPages, page)));
  };

  return (
    <div>
      {/* Page title — same convention as Disk Setup/Snapshots, so every
          tab announces itself (Volumes/Disks previously opened straight
          into filter chrome). */}
      <div className="mb-6 flex items-center gap-3">
        <Database className="w-8 h-8 text-brand-600" />
        <div>
          <h2 className="text-page-title text-gray-900">Volumes</h2>
          <p className="text-gray-600">PVC volumes and replica health across the cluster</p>
        </div>
      </div>
      <div className="space-y-3 mb-4">
        {activeFilter && activeFilter !== 'all' && (
          <div className="p-3 bg-brand-50 border border-brand-200 rounded-lg flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Filter className="w-4 h-4 text-brand-600" />
              <span className="text-sm font-medium text-brand-900">
                Filtered by: {getFilterDisplayName(activeFilter)}
              </span>
              <span className="text-sm text-brand-700">
                ({filteredVolumes.length} of {volumes.length} volumes)
              </span>
            </div>
            {onClearFilter && (
              <Button variant="link" icon={X} onClick={onClearFilter}>
                Clear Filter
              </Button>
            )}
          </div>
        )}

        {/* raw purple on purpose: decorative disk-filter accent, not the sync
            rejoining meaning purple aliases */}
        {diskFilter && (
          <div className="p-3 bg-purple-50 border border-purple-200 rounded-lg flex items-center justify-between">
            <div className="flex items-center gap-2">
              <HardDrive className="w-4 h-4 text-purple-600" />
              <span className="text-sm font-medium text-purple-900">
                Showing volumes on disk: {diskFilter}
              </span>
              <span className="text-sm text-purple-700">
                ({filteredVolumes.length} volumes on this disk)
              </span>
            </div>
            {onClearDiskFilter && (
              <Button
                variant="link"
                icon={X}
                className="text-purple-600 hover:text-purple-800"
                onClick={onClearDiskFilter}
              >
                Clear Disk Filter
              </Button>
            )}
          </div>
        )}
      </div>

      {/* Pagination Controls - Top */}
      {filteredVolumes.length > 0 && (
        <div className="bg-white rounded-lg shadow p-4 mb-4">
          <Pagination
            page={currentPage}
            pageCount={totalPages}
            onPage={goToPage}
            pageSize={pageSize}
            onPageSize={setPageSize}
            totalItems={filteredVolumes.length}
            itemNoun="volumes"
          />
        </div>
      )}

      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Volume Name</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Size</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">State</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Replicas</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Sync</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Nodes</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Actions</th>
              {diskFilter && (
                <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">On Disk</th>
              )}
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {sortedVolumes.length === 0 ? (
              <tr>
                <td colSpan={diskFilter ? 8 : 7} className="px-6 py-8 text-center text-gray-500">
                  {activeFilter === 'orphaned'
                    ? 'Orphaned SPDK volumes are listed per disk on the Disks tab.'
                    : activeFilter && activeFilter !== 'all'
                    ? `No volumes match the "${getFilterDisplayName(activeFilter)}" filter.`
                    : diskFilter
                    ? `No volumes found on disk "${diskFilter}".`
                    : 'No volumes found.'
                  }
                </td>
              </tr>
            ) : (
              sortedVolumes.map((volume) => {
                const stateInfo = getVolumeStateInfo(volume.state);
                const StateIcon = stateInfo.icon;
                const hasSyncData = volume.replica_statuses.some(r => r.sync != null);
                // What the missing replicas are: behind the record on a
                // replica set; on a single-replica volume either gone (the
                // lvol verify said no) or unverifiable (its node agent did
                // not answer — a csi-node restart read "failed, not in sync"
                // on dash1, 2026-10-27).
                const missing = volume.replicas - volume.active_replicas;
                const unverified =
                  !hasSyncData &&
                  (volume.replica_statuses.some(r => r.status === 'unknown') ||
                    volume.spdk_validation_status?.validation_severity === 'warning');
                const missingLabel = hasSyncData
                  ? `${missing} replica${missing !== 1 ? 's' : ''} not in sync`
                  : unverified
                    ? `${missing} replica${missing !== 1 ? 's' : ''} unverified`
                    : `${missing} replica${missing !== 1 ? 's' : ''} failed`;

                return (
                  <tr key={volume.id} className="hover:bg-gray-50">
                    <td className="px-6 py-4 whitespace-nowrap">
                      <Button
                        variant="link"
                        className="hover:underline"
                        onClick={() => handleVolumeNameClick(volume)}
                      >
                        {volume.name}
                      </Button>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.size}</td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <div className="flex items-center gap-2">
                        <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${stateInfo.badge}`}>
                          <StateIcon className="w-3 h-3 mr-1" />
                          {volume.state}
                        </span>

                        {/* No SPDK backing: a shield when the backend could
                            not verify the lvol (error red, warning amber); a
                            gray note when there is none by design — an NFS
                            share owns no SPDK volume (severity "info"). */}
                        {volume.spdk_validation_status && !volume.spdk_validation_status.has_spdk_backing && (
                          volume.spdk_validation_status.validation_severity === 'info' ? (
                            <span
                              className="text-gray-400"
                              title={volume.spdk_validation_status.validation_message || 'No SPDK volume'}
                            >
                              <Info className="w-4 h-4" />
                            </span>
                          ) : (
                            <span
                              className={volume.spdk_validation_status.validation_severity === 'warning'
                                ? 'text-warning-600'
                                : 'text-failed-500'}
                              title={volume.spdk_validation_status.validation_message || 'No SPDK backing found'}
                            >
                              <ShieldAlert className="w-4 h-4" />
                            </span>
                          )
                        )}

                        {stateInfo.tooltip && (
                          <span className="text-xs text-gray-400" title={stateInfo.tooltip}>
                            <Info className="w-3 h-3" />
                          </span>
                        )}
                      </div>
                    </td>

                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                      {volume.replicas === 0 ? (
                        // No SPDK legs at all (an NFS share): nothing to count.
                        <span
                          className="text-gray-400"
                          title={volume.spdk_validation_status?.validation_message || 'No SPDK replicas'}
                        >
                          —
                        </span>
                      ) : (
                        <>
                          <Button
                            variant="link"
                            className="hover:underline"
                            onClick={() => onReplicaClick?.(volume.id, volume.name)}
                            title={`Click to see disks with replicas for ${volume.name}`}
                          >
                            {volume.active_replicas}/{volume.replicas}
                          </Button>
                          {missing > 0 && (
                            <div
                              className={`text-xs mt-1 ${unverified ? 'text-gray-500' : 'text-failed-600'}`}
                              title={unverified ? (volume.spdk_validation_status?.validation_message ?? undefined) : undefined}
                            >
                              {missingLabel}
                            </div>
                          )}
                        </>
                      )}
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      {hasSyncData ? (
                        <VolumeSyncSummary volume={volume} />
                      ) : (
                        // Single-replica volumes carry no sync record.
                        <span className="text-gray-400 text-sm" title="Single-replica volume: no replication record">—</span>
                      )}
                    </td>
                    <td className="px-6 py-4">
                      <div className="flex flex-wrap gap-1">
                        {volume.nodes.map(node => {
                          const gone = !!nodes && node !== '' && !nodes.includes(node);
                          return gone ? (
                            <span
                              key={node}
                              className="inline-flex px-2 py-1 text-xs bg-failed-100 text-failed-800 rounded line-through"
                              title={`${node} is no longer in the cluster; the record still lists a leg there`}
                            >
                              {node}
                            </span>
                          ) : (
                            <span key={node} className="inline-flex px-2 py-1 text-xs bg-gray-100 text-gray-800 rounded">
                              {node}
                            </span>
                          );
                        })}
                      </div>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <div className="flex items-center gap-2">
                        <Button
                          size="sm"
                          icon={Info}
                          onClick={() => handleVolumeNameClick(volume)}
                        >
                          Details
                        </Button>
                      </div>
                    </td>
                    {diskFilter && (
                      <td className="px-6 py-4 whitespace-nowrap">
                        {/* raw purple on purpose: matches the disk-filter banner
                            accent above, not a status */}
                        <span className="inline-flex px-2 py-1 text-xs bg-purple-100 text-purple-800 rounded-full">
                          {diskFilter}
                        </span>
                      </td>
                    )}
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>

      {/* Pagination Controls - Bottom */}
      {totalPages > 1 && (
        <div className="bg-white rounded-lg shadow p-4 mt-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-700">
              Showing {((currentPage - 1) * pageSize) + 1} to {Math.min(currentPage * pageSize, filteredVolumes.length)} of {filteredVolumes.length} results
            </div>
            <div className="flex items-center gap-2">
              <Button size="sm" onClick={() => goToPage(1)} disabled={currentPage === 1}>
                First
              </Button>
              <Button size="sm" onClick={() => goToPage(currentPage - 1)} disabled={currentPage === 1}>
                Previous
              </Button>
              
              {/* Page numbers */}
              {Array.from({ length: Math.min(5, totalPages) }, (_, i) => {
                const pageNum = Math.max(1, Math.min(totalPages - 4, currentPage - 2)) + i;
                return (
                  <button
                    key={pageNum}
                    onClick={() => goToPage(pageNum)}
                    className={`px-3 py-1 text-sm border rounded ${
                      pageNum === currentPage
                        ? 'bg-brand-600 text-white border-brand-600'
                        : 'border-gray-300 hover:bg-gray-50'
                    }`}
                  >
                    {pageNum}
                  </button>
                );
              })}
              
              <Button size="sm" onClick={() => goToPage(currentPage + 1)} disabled={currentPage === totalPages}>
                Next
              </Button>
              <Button size="sm" onClick={() => goToPage(totalPages)} disabled={currentPage === totalPages}>
                Last
              </Button>
            </div>
          </div>
        </div>
      )}

      {/* Summary information */}
      {sortedVolumes.length > 0 && (
        <div className="mt-4 p-4 bg-gray-50 rounded-lg">
          <h4 className="text-sm font-medium text-gray-700 mb-2">Volume Summary</h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-xs">
            <div className="flex items-center gap-2">
              <CheckCircle className="w-4 h-4 text-healthy-600" />
              <span>{filteredVolumes.filter(v => v.state === 'Healthy').length} Healthy</span>
            </div>
            <div className="flex items-center gap-2">
              <AlertTriangle className="w-4 h-4 text-degraded-600" />
              <span>{filteredVolumes.filter(v => v.state === 'Degraded').length} Degraded</span>
            </div>
            <div className="flex items-center gap-2">
              <XCircle className="w-4 h-4 text-failed-600" />
              <span>{filteredVolumes.filter(v => v.state === 'Failed').length} Failed</span>
            </div>
            <div className="flex items-center gap-2">
              <RefreshCw className="w-4 h-4 text-rebuilding-600" />
              <span>{filteredVolumes.filter(isVolumeRecovering).length} Recovering</span>
            </div>
          </div>
          {totalPages > 1 && (
            <div className="mt-2 text-xs text-gray-500">
              Showing page {currentPage} of {totalPages} • {pageSize} volumes per page
            </div>
          )}
        </div>
      )}

      {/* Volume Detail Modal — volumeData re-derives from live query data */}
      {selectedVolumeDetail && (
        <Suspense fallback={null}>
        <VolumeDetailAPI
          key={selectedVolumeDetail.id}
          volumeId={selectedVolumeDetail.id}
          volumeName={selectedVolumeDetail.name}
          volumeData={selectedVolumeDetail}
          onClose={() => setSelectedVolumeId(null)}
        />
        </Suspense>
      )}

    </div>
  );
};