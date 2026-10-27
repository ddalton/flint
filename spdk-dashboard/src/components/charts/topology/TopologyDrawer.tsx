import React, { useEffect, useRef } from 'react';
import { X } from 'lucide-react';
import { MemberStateChip, VolumeStateChip } from '../../ui/Chip';
import { SyncStateIndicator } from '../../ui/SyncStateIndicator';
import type { ConsumerRaid, Volume } from '../../../hooks/useDashboardData';
import { primaryConsumerRaid } from '../../../hooks/useDashboardData';
import { memberVisual, type TopologyDetail } from './buildTopology';

// Details-on-demand (the Kiali pattern): everything that used to be inlined
// into the page — the consumer raid's base list, sync records, the
// explainers — lives here, opened by selecting a node or edge. The diagram
// itself stays quiet. Every row is a field the backend sends; nothing is
// derived from the pre-Tier-2 raid_status / rebuild_info model, which the
// live backend never populated.

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

// One consumer raid as SPDK reports it on its node.
function ConsumerRaidRows({ raid }: { raid: ConsumerRaid }) {
  return (
    <>
      <Row k="Node">{raid.node}</Row>
      <Row k="Raid bdev"><Mono>{raid.raid_name}</Mono></Row>
      <Row k="State"><MemberStateChip state={raid.state} /></Row>
      <Row k="Bases configured">
        {raid.num_base_bdevs_operational}/{raid.num_base_bdevs}
      </Row>
      <ul className="mt-2 space-y-1">
        {raid.base_bdevs.map((b, i) => (
          <li key={i} className="flex items-center gap-2 rounded bg-gray-50 px-2 py-1 text-xs">
            <span
              aria-hidden="true"
              className={`h-2 w-2 flex-shrink-0 rounded-full ${
                b.is_configured ? 'bg-healthy-500' : 'bg-failed-500'
              }`}
            />
            <span className="font-medium text-gray-700">slot {i}</span>
            {b.is_configured ? (
              <>
                <span className="text-gray-600">{b.replica_node ?? 'unmatched'}</span>
                <span className="ml-auto truncate font-mono text-gray-500" title={b.name ?? undefined}>
                  {b.name ?? '—'}
                </span>
              </>
            ) : (
              <span className="text-failed-700">
                {b.replica_node
                  ? `removed — the leg on ${b.replica_node} is out of the raid`
                  : 'failed slot — leg removed from the raid'}
              </span>
            )}
          </li>
        ))}
      </ul>
    </>
  );
}

function AboutContent() {
  return (
    <div className="space-y-3 text-sm text-gray-600">
      <div>
        <h5 className="mb-1 font-medium text-gray-800">How flint mirrors a volume</h5>
        <p className="text-xs">
          The node that stages the volume assembles an SPDK raid bdev (level 1,
          a mirror) over the replica legs: its own leg directly, the others
          over NVMe-oF. The raid is ephemeral and is re-assembled wherever the
          consumer lands; the replica lvols and the PV sync record are the
          source of truth.
        </p>
      </div>
      <div>
        <h5 className="mb-1 font-medium text-gray-800">Replica recovery</h5>
        <p className="text-xs">
          A leg that misses writes is marked <em>stale</em> and excluded. The
          controller catches it up from epoch snapshots to a <em>standby</em>,
          then hot-rejoins it into the live raid in a short quiesce window. No
          block-by-block rebuild runs, so there is no percentage to show:
          progress is the epoch lag counting down, and the engine's events
          (volume detail → Events) narrate each phase.
        </p>
      </div>
      <div>
        <h5 className="mb-1 font-medium text-gray-800">Reading this graph</h5>
        <ul className="list-inside list-disc space-y-0.5 text-xs">
          <li>Left to right follows the data path: consumer → raid bdev → legs → disks</li>
          <li>Solid leg edges are local to the consumer; dashed are NVMe-oF remotes</li>
          <li>Marching dashes mean the leg is not in sync (catch-up in flight)</li>
          <li>Click any node or edge for its details</li>
        </ul>
      </div>
    </div>
  );
}

function DetailBody({ volume, detail }: { volume: Volume; detail: TopologyDetail }) {
  const raid = primaryConsumerRaid(volume);
  switch (detail.kind) {
    case 'consumer':
      return (
        <>
          <Section title="Consumer">
            {raid ? (
              <>
                <Row k="Node">{raid.node}</Row>
                <Row k="Raid bdev"><Mono>{raid.raid_name}</Mono></Row>
                <Row k="Raid state"><MemberStateChip state={raid.state} /></Row>
                <Row k="Volume state"><VolumeStateChip state={volume.state} /></Row>
              </>
            ) : (
              <p className="text-sm text-gray-600">
                Not staged on any node — no raid is assembled for this volume.
              </p>
            )}
          </Section>
          <Section title="Replica nodes">
            <div className="flex flex-wrap gap-1">
              {volume.nodes.map(n => (
                <span key={n} className="rounded bg-gray-100 px-2 py-0.5 text-xs text-gray-600">
                  {n}
                </span>
              ))}
              {volume.nodes.length === 0 && (
                <span className="text-xs text-gray-500">none recorded</span>
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

    case 'raid':
      return (
        <>
          <Section title="Volume">
            <Row k="State"><VolumeStateChip state={volume.state} /></Row>
            <Row k="Size">{volume.size}</Row>
            <Row k="Replicas in sync">
              {volume.active_replicas}/{volume.replicas}
            </Row>
            <Row k="ID"><Mono>{volume.id}</Mono></Row>
          </Section>
          {volume.consumer_raids.length === 0 ? (
            <Section title="RAID bdev">
              <p className="text-sm text-gray-600">
                Not assembled on any node. When a workload stages this volume,
                the consumer node builds its raid from the replica legs and it
                appears here.
              </p>
            </Section>
          ) : (
            volume.consumer_raids.map(r => (
              <Section key={r.node} title={`Assembled on ${r.node}`}>
                <ConsumerRaidRows raid={r} />
              </Section>
            ))
          )}
          {volume.consumer_raids.length > 1 && (
            <Section title="Note">
              <p className="text-xs text-gray-600">
                More than one node has this raid assembled — normal only
                mid-cutover; the graph draws the first.
              </p>
            </Section>
          )}
        </>
      );

    case 'member': {
      const { join } = detail;
      const replica = join.replica;
      const { dashed } = memberVisual(join);
      return (
        <>
          {(join.member || join.raidPresent) && (
            <Section title="RAID member">
              <Row k="Slot">{join.slot ?? 'not assembled'}</Row>
              {join.member && (
                <>
                  <Row k="Configured">
                    {join.member.is_configured
                      ? 'yes'
                      : join.replica
                        ? `no — the leg on ${join.replica.node} was removed from the raid`
                        : 'no — failed slot'}
                  </Row>
                  <Row k="Bdev"><Mono>{join.member.name ?? '—'}</Mono></Row>
                  {join.member.uuid && <Row k="UUID"><Mono>{join.member.uuid}</Mono></Row>}
                </>
              )}
            </Section>
          )}
          {replica && (
            <Section title={`Replica on ${replica.node}`}>
              <Row k="Status"><MemberStateChip state={replica.status} /></Row>
              <Row k="Path">
                {join.consumerNode
                  ? dashed
                    ? `NVMe-oF from ${join.consumerNode}`
                    : 'local to the consumer'
                  : '— (no consumer)'}
              </Row>
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
          <AboutContent />
        </div>
      );
  }
}

const DETAIL_TITLES: Record<TopologyDetail['kind'], string> = {
  consumer: 'Consumer',
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
