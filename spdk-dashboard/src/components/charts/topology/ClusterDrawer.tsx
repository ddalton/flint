import { useEffect, useRef } from 'react';
import { ArrowRight, X } from 'lucide-react';
import { MemberStateChip, VolumeStateChip } from '../../ui/Chip';
import type { Volume } from '../../../hooks/useDashboardData';
import { nodeCapacity, type ClusterDetail } from './buildClusterTopology';

// Cluster-altitude details drawer. Volume rows are the click-through to
// the volume-level graph — the drill-down path between the two altitudes.

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

function VolumeList({
  volumes,
  onOpenVolume,
}: {
  volumes: Volume[];
  onOpenVolume: (volumeId: string) => void;
}) {
  return (
    <ul className="space-y-1">
      {volumes.map(v => (
        <li key={v.id}>
          <button
            onClick={() => onOpenVolume(v.id)}
            className="group flex w-full items-center justify-between gap-2 rounded px-2 py-1.5 text-left hover:bg-gray-50"
            title={`Open ${v.name} in the volume view`}
          >
            <span className="min-w-0 flex-1 truncate text-sm font-medium text-gray-800">
              {v.name}
            </span>
            <VolumeStateChip state={v.state} />
            <ArrowRight
              className="h-3.5 w-3.5 flex-shrink-0 text-gray-300 group-hover:text-gray-500"
              aria-hidden="true"
            />
          </button>
        </li>
      ))}
    </ul>
  );
}

function DetailBody({
  detail,
  onOpenVolume,
}: {
  detail: ClusterDetail;
  onOpenVolume: (volumeId: string) => void;
}) {
  switch (detail.kind) {
    case 'cluster-node': {
      const { node } = detail;
      const { totalGb, freeGb } = nodeCapacity(node);
      return (
        <>
          <Section title="Node">
            <Row k="Replicas hosted">{node.replicaCount}</Row>
            <Row k="Volumes">{node.volumes.length}</Row>
            {totalGb > 0 && (
              <Row k="Capacity">
                {Math.round(freeGb)} GB free of {Math.round(totalGb)} GB
              </Row>
            )}
            {node.info && (
              <Row k="Memory">
                {node.info.memory_used_mb.toLocaleString()}/
                {node.info.memory_total_mb.toLocaleString()} MB (
                {node.info.memory_utilization_pct.toFixed(0)}%)
              </Row>
            )}
          </Section>
          <Section title={`Disks (${node.disks.length})`}>
            {node.disks.length === 0 ? (
              <p className="text-xs text-gray-500">No SPDK-visible disks on this node.</p>
            ) : (
              <ul className="space-y-2">
                {node.disks.map(d => (
                  <li key={d.id} className="rounded bg-gray-50 p-2 text-xs">
                    <div className="mb-1 flex items-center justify-between gap-2">
                      <span className="min-w-0 truncate font-medium text-gray-800" title={d.id}>
                        {d.model || d.id}
                      </span>
                      <MemberStateChip
                        state={d.healthy ? (d.blobstore_initialized ? 'healthy' : 'spare') : 'failed'}
                        title={d.blobstore_initialized ? undefined : 'not initialized'}
                      />
                    </div>
                    <div className="flex justify-between text-gray-600 tabular-nums">
                      <span>{d.free_space_display} free</span>
                      <span>
                        R {d.read_iops.toLocaleString()} / W {d.write_iops.toLocaleString()} IOPS
                      </span>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </Section>
          <Section title={`Volumes on ${node.name} (${node.volumes.length})`}>
            {node.volumes.length === 0 ? (
              <p className="text-xs text-gray-500">No volume replicas on this node.</p>
            ) : (
              <VolumeList volumes={node.volumes} onOpenVolume={onOpenVolume} />
            )}
          </Section>
        </>
      );
    }

    case 'cluster-link': {
      const { link } = detail;
      return (
        <>
          <Section title="Replica link">
            <Row k="Between">
              {link.a} ↔ {link.b}
            </Row>
            <Row k="Shared volumes">{link.volumes.length}</Row>
            <Row k="Worst state"><VolumeStateChip state={link.worstState} /></Row>
          </Section>
          <Section title="Volumes spanning both nodes">
            <VolumeList volumes={link.volumes} onOpenVolume={onOpenVolume} />
          </Section>
        </>
      );
    }
  }
}

export function ClusterDrawer({
  detail,
  onClose,
  onOpenVolume,
}: {
  detail: ClusterDetail;
  onClose: () => void;
  onOpenVolume: (volumeId: string) => void;
}) {
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    closeRef.current?.focus();
  }, [detail.kind]);

  const title =
    detail.kind === 'cluster-node'
      ? `Node — ${detail.node.name}`
      : `Link — ${detail.link.a} ↔ ${detail.link.b}`;

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
        <DetailBody detail={detail} onOpenVolume={onOpenVolume} />
      </div>
    </aside>
  );
}
