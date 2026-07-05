import React, { useEffect, useRef } from 'react';
import { X } from 'lucide-react';
import { MemberStateChip, VolumeStateChip } from '../../ui/Chip';
import { ProgressBar } from '../../ui/ProgressBar';
import { SyncStateIndicator } from '../../ui/SyncStateIndicator';
import type { Volume } from '../../../hooks/useDashboardData';
import { raidLevelDisplayName, type TopologyDetail } from './buildTopology';

// Details-on-demand (the Kiali pattern): everything that used to be inlined
// into the page — NQNs, sync trees, rebuild blocks, the RAID/NVMe-oF
// explainers — lives here, opened by selecting a node or edge. The diagram
// itself stays quiet.

function Row({ k, children }: { k: string; children: React.ReactNode }) {
  return (
    <div className="flex justify-between gap-3 py-0.5 text-sm">
      <span className="flex-shrink-0 text-gray-500">{k}</span>
      <span className="min-w-0 text-right font-medium text-gray-800">{children}</span>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="border-t border-gray-100 px-4 py-3">
      <h4 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-gray-500">
        {title}
      </h4>
      {children}
    </div>
  );
}

function Mono({ children }: { children: React.ReactNode }) {
  return <span className="break-all font-mono text-xs">{children}</span>;
}

const RAID_PROTECTION_NOTES: Record<number, string[]> = {
  0: [
    'Data striped across all members',
    'Maximum performance, no redundancy',
    'A single member failure causes data loss',
  ],
  1: [
    'Data mirrored across all members',
    'Tolerates up to N−1 member failures',
    'Read performance scales with members',
    'Write performance limited by the slowest member',
  ],
  5: [
    'Data striped with distributed parity',
    'Tolerates a single member failure',
    'Parity overhead: 1/N capacity',
  ],
  6: [
    'Data striped with dual parity',
    'Tolerates up to 2 member failures',
    'Parity overhead: 2/N capacity',
  ],
};

function AboutContent({ volume }: { volume: Volume }) {
  const raid = volume.raid_status;
  return (
    <div className="space-y-3 text-sm text-gray-600">
      {raid && (
        <div>
          <h5 className="mb-1 font-medium text-gray-800">{raidLevelDisplayName(raid.raid_level)}</h5>
          <ul className="list-inside list-disc space-y-0.5 text-xs">
            {(RAID_PROTECTION_NOTES[raid.raid_level] ?? ['Custom RAID configuration']).map(n => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        </div>
      )}
      {volume.ublk_device && (
        <div>
          <h5 className="mb-1 font-medium text-gray-800">ublk</h5>
          <p className="text-xs">
            The volume is exposed to the consumer as a kernel block device
            (module <Mono>ublk_drv</Mono>, kernel 6.0+); SPDK serves the I/O in
            userspace and handles all replica management transparently.
          </p>
        </div>
      )}
      <div>
        <h5 className="mb-1 font-medium text-gray-800">NVMe-oF</h5>
        <p className="text-xs">
          NVMe over Fabrics carries block I/O between nodes: remote replicas are
          reached through per-replica NVMe-oF targets (dashed edges). Clients
          connect by NQN; failover, rebuild, and recovery are transparent to the
          consumer.
        </p>
      </div>
      <div>
        <h5 className="mb-1 font-medium text-gray-800">Reading this graph</h5>
        <ul className="list-inside list-disc space-y-0.5 text-xs">
          <li>Left to right follows the data path: consumer → access device → RAID → members → disks</li>
          <li>Solid member edges are local; dashed are NVMe-oF remotes</li>
          <li>Marching dashes mean recovery is in flight (rebuild or epoch catch-up)</li>
          <li>Click any node or edge for its details</li>
        </ul>
      </div>
    </div>
  );
}

function DetailBody({ volume, detail }: { volume: Volume; detail: TopologyDetail }) {
  const raid = volume.raid_status;
  switch (detail.kind) {
    case 'app':
      return (
        <>
          <Section title="Consumer">
            <Row k="Access method">{volume.access_method}</Row>
            <Row k="Transport">{volume.transport_type}</Row>
            <Row k="Volume state"><VolumeStateChip state={volume.state} /></Row>
          </Section>
          <Section title="Attached on nodes">
            <div className="flex flex-wrap gap-1">
              {volume.nodes.map(n => (
                <span key={n} className="rounded bg-gray-100 px-2 py-0.5 text-xs text-gray-600">
                  {n}
                </span>
              ))}
              {volume.nodes.length === 0 && (
                <span className="text-xs text-gray-500">not attached</span>
              )}
            </div>
          </Section>
          {volume.pvc_info && (
            <Section title="Kubernetes PVC">
              <Row k="Name"><Mono>{volume.pvc_info.name}</Mono></Row>
              <Row k="Namespace">{volume.pvc_info.namespace}</Row>
              <Row k="StorageClass">{volume.pvc_info.storage_class}</Row>
            </Section>
          )}
        </>
      );

    case 'access':
      return (
        <>
          {volume.ublk_device && (
            <Section title="ublk block device">
              <Row k="Device path"><Mono>{volume.ublk_device.device_path}</Mono></Row>
              <Row k="ublk ID">{volume.ublk_device.id}</Row>
              <Row k="Kernel module"><Mono>ublk_drv</Mono></Row>
              <Row k="Min kernel">6.0+</Row>
            </Section>
          )}
          {volume.nvmeof_targets.length > 0 && (
            <Section title={`NVMe-oF targets (${volume.nvmeof_targets.length})`}>
              <div className="space-y-2">
                {volume.nvmeof_targets.map((t, i) => (
                  <div key={i} className="rounded bg-gray-50 p-2 text-xs">
                    <div className="mb-1"><Mono>{t.nqn}</Mono></div>
                    <div className="flex justify-between text-gray-600">
                      <span>
                        {t.target_ip}:{t.target_port} ({t.transport})
                      </span>
                      <span>
                        {t.connection_count} conn{t.connection_count === 1 ? '' : 's'} •{' '}
                        {t.active ? 'active' : 'inactive'}
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            </Section>
          )}
        </>
      );

    case 'raid':
      return (
        <>
          <Section title="Volume">
            <Row k="State"><VolumeStateChip state={volume.state} /></Row>
            <Row k="Size">{volume.size}</Row>
            <Row k="Replicas">
              {volume.active_replicas}/{volume.replicas} active
            </Row>
            <Row k="ID"><Mono>{volume.id}</Mono></Row>
          </Section>
          {raid && (
            <Section title={raidLevelDisplayName(raid.raid_level)}>
              <Row k="RAID state"><MemberStateChip state={raid.state} /></Row>
              <Row k="Members">
                {raid.operational_members}/{raid.num_members} operational
              </Row>
              <Row k="Discovered">{raid.discovered_members}</Row>
              <Row k="Auto-rebuild">{raid.auto_rebuild_enabled ? 'enabled' : 'disabled'}</Row>
              {raid.superblock_version != null && (
                <Row k="Superblock">v{raid.superblock_version}</Row>
              )}
            </Section>
          )}
          {raid?.rebuild_info && (
            <Section title="Active rebuild">
              <Row k="Direction">
                slot {raid.rebuild_info.source_slot} → slot {raid.rebuild_info.target_slot}
              </Row>
              <Row k="State">{raid.rebuild_info.state}</Row>
              <div className="mt-2">
                <ProgressBar
                  value={raid.rebuild_info.progress_percentage}
                  label="RAID rebuild progress"
                  valueText={`${raid.rebuild_info.progress_percentage.toFixed(1)}%`}
                  tone="warn"
                  className="w-full"
                />
                <p className="mt-1 text-xs tabular-nums text-gray-600">
                  {raid.rebuild_info.progress_percentage.toFixed(1)}% •{' '}
                  {raid.rebuild_info.blocks_remaining.toLocaleString()} of{' '}
                  {raid.rebuild_info.blocks_total.toLocaleString()} blocks remaining
                  {raid.rebuild_info.estimated_time_remaining
                    ? ` • ETA ${raid.rebuild_info.estimated_time_remaining}`
                    : ''}
                </p>
              </div>
            </Section>
          )}
        </>
      );

    case 'member': {
      const { join } = detail;
      const replica = join.replica;
      const rebuild = raid?.rebuild_info;
      return (
        <>
          {(join.member || join.raidPresent) && (
            <Section title="RAID member">
              <Row k="Slot">{join.slot ?? 'not assembled'}</Row>
              {join.member && (
                <>
                  <Row k="State"><MemberStateChip state={join.member.state} /></Row>
                  <Row k="Bdev"><Mono>{join.member.name}</Mono></Row>
                  {join.member.uuid && <Row k="UUID"><Mono>{join.member.uuid}</Mono></Row>}
                </>
              )}
            </Section>
          )}
          {replica && (
            <Section title={`Replica on ${replica.node}`}>
              <Row k="Status"><MemberStateChip state={replica.status} /></Row>
              <Row k="RAID state">{replica.raid_member_state}</Row>
              <Row k="Access">{replica.is_local ? 'local' : 'remote'} • {replica.access_method}</Row>
              {replica.is_new_replica && <Row k="Provisioned">new replica</Row>}
              {replica.last_io_timestamp && (
                <Row k="Last I/O">{new Date(replica.last_io_timestamp).toLocaleTimeString()}</Row>
              )}
            </Section>
          )}
          {replica?.sync && (
            <Section title="Sync state">
              <div className="mb-1">
                <SyncStateIndicator sync={replica.sync} />
              </div>
              <div className="text-xs text-gray-600">
                {replica.sync.since && <div>since {replica.sync.since}</div>}
                {replica.sync.last_epoch && (
                  <div>
                    last epoch <Mono>{replica.sync.last_epoch}</Mono>
                  </div>
                )}
                {replica.sync.reason && <div>{replica.sync.reason}</div>}
              </div>
            </Section>
          )}
          {replica?.nvmf_target && (
            <Section title="NVMe-oF target">
              <Row k="Address">
                {replica.nvmf_target.target_ip}:{replica.nvmf_target.target_port}
              </Row>
              <Row k="Transport">{replica.nvmf_target.transport_type}</Row>
              <div className="mt-1 rounded bg-gray-50 p-2">
                <Mono>{replica.nvmf_target.nqn}</Mono>
              </div>
            </Section>
          )}
          {(join.isRebuildTarget || join.isRebuildSource) && rebuild && (
            <Section title="Rebuild role">
              <Row k="Role">{join.isRebuildTarget ? 'target (being rebuilt)' : 'source'}</Row>
              {join.isRebuildTarget && (
                <div className="mt-2">
                  <ProgressBar
                    value={rebuild.progress_percentage}
                    label="member rebuild progress"
                    valueText={`${rebuild.progress_percentage.toFixed(1)}%`}
                    tone="warn"
                    className="w-full"
                  />
                  {rebuild.estimated_time_remaining && (
                    <p className="mt-1 text-xs text-gray-600">
                      ETA {rebuild.estimated_time_remaining}
                    </p>
                  )}
                </div>
              )}
            </Section>
          )}
        </>
      );
    }

    case 'disk': {
      const d = detail.disk;
      const provisioned = d.provisioned_volumes.find(pv => pv.volume_id === volume.id);
      return (
        <>
          <Section title="Disk">
            <Row k="Health"><MemberStateChip state={d.healthy ? 'healthy' : 'failed'} /></Row>
            <Row k="Node">{d.node}</Row>
            <Row k="Model">{d.model || '—'}</Row>
            <Row k="Type">{d.device_type}</Row>
            <Row k="PCI"><Mono>{d.pci_addr}</Mono></Row>
            <Row k="Capacity">{d.capacity_gb} GB</Row>
            <Row k="Free">{d.free_space_display}</Row>
            <Row k="Volumes hosted">{d.lvol_count}</Row>
          </Section>
          <Section title="Live I/O">
            <Row k="Read">{d.read_iops.toLocaleString()} IOPS • {d.read_latency}µs</Row>
            <Row k="Write">{d.write_iops.toLocaleString()} IOPS • {d.write_latency}µs</Row>
          </Section>
          {provisioned && (
            <Section title="This volume's replica">
              <Row k="Type">{provisioned.replica_type}</Row>
              <Row k="Status">{provisioned.status}</Row>
              <Row k="Provisioned">
                {/* Empty when the PV has no PVC (statically provisioned) */}
                {provisioned.provisioned_at
                  ? new Date(provisioned.provisioned_at).toLocaleString()
                  : '—'}
              </Row>
            </Section>
          )}
        </>
      );
    }

    case 'about':
      return (
        <div className="px-4 py-3">
          <AboutContent volume={volume} />
        </div>
      );
  }
}

const DETAIL_TITLES: Record<TopologyDetail['kind'], string> = {
  app: 'Consumer',
  access: 'Access layer',
  raid: 'RAID bdev',
  member: 'RAID member',
  disk: 'Backing disk',
  about: 'About this topology',
};

export function TopologyDrawer({
  volume,
  detail,
  onClose,
}: {
  volume: Volume;
  detail: TopologyDetail;
  onClose: () => void;
}) {
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    closeRef.current?.focus();
  }, [detail.kind]);

  const title =
    detail.kind === 'member'
      ? [
          detail.join.raidPresent ? DETAIL_TITLES.member : 'Replica',
          detail.join.replica?.node,
        ]
          .filter(Boolean)
          .join(' — ')
      : DETAIL_TITLES[detail.kind];

  return (
    <aside
      role="dialog"
      aria-label={title}
      className="absolute inset-y-0 right-0 z-10 flex w-80 flex-col border-l border-gray-200 bg-white shadow-xl"
    >
      <div className="flex items-center justify-between border-b border-gray-200 px-4 py-3">
        <h3 className="min-w-0 truncate text-sm font-semibold text-gray-800">{title}</h3>
        <button
          ref={closeRef}
          onClick={onClose}
          aria-label="Close details"
          className="rounded p-1 text-gray-400 hover:bg-gray-100 hover:text-gray-600"
        >
          <X className="h-4 w-4" />
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto pb-3">
        <DetailBody volume={volume} detail={detail} />
      </div>
    </aside>
  );
}
