// Canvas + drawer wiring, at jsdom depth: nodes render, selection opens the
// drawer with entity details, Escape and ✕ close it. Geometry/zoom behavior
// is React Flow's own and is not asserted here. fireEvent, not user-event:
// a real mousedown sequence reaches d3-zoom's pane handler, which reads
// event.view.document — always null in jsdom.
import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { VolumeTopologyGraph } from './VolumeTopologyGraph';
import type { Disk, ReplicaStatus, Volume } from '../../../hooks/useDashboardData';

function mkVolume(): Volume {
  const replica = (r: Partial<ReplicaStatus>): ReplicaStatus =>
    ({
      access_method: 'local_nvme',
      is_local: true,
      node: 'worker-1',
      raid_member_state: 'online',
      rebuild_progress: null,
      status: 'online',
      ...r,
    }) as ReplicaStatus;

  return {
    access_method: 'ublk',
    active_replicas: 2,
    consumer_raids: [],
    id: 'vol-1',
    local_nvme: true,
    name: 'pvc-demo',
    nodes: ['worker-1'],
    nvmeof_enabled: false,
    nvmeof_targets: [],
    replicas: 2,
    size: '10 GiB',
    spdk_validation_status: { has_spdk_backing: true, validation_severity: 'info' },
    state: 'Healthy',
    target_port: 4420,
    transport_type: 'tcp',
    ublk_device: { id: 42, device_path: '/dev/ublkb42' },
    raid_status: {
      auto_rebuild_enabled: true,
      discovered_members: 2,
      members: [
        { name: 'bdev-a', slot: 0, state: 'online' },
        { name: 'bdev-b', slot: 1, state: 'online' },
      ],
      num_members: 2,
      operational_members: 2,
      raid_level: 1,
      state: 'online',
    },
    replica_statuses: [
      replica({ node: 'worker-1', raid_member_slot: 0 }),
      replica({
        node: 'worker-2',
        raid_member_slot: 1,
        is_local: false,
        access_method: 'nvmeof',
        nvmf_target: {
          nqn: 'nqn.2024-01.io.flint:vol-1:w2',
          target_ip: '10.0.0.2',
          target_port: '4420',
          transport_type: 'TCP',
        },
      }),
    ],
  } as Volume;
}

const disks: Disk[] = [];

describe('VolumeTopologyGraph', () => {
  it('renders every layer of the data path', () => {
    render(<VolumeTopologyGraph volume={mkVolume()} disks={disks} />);
    expect(screen.getByText('Consumer')).toBeInTheDocument();
    expect(screen.getByText('ublk block device')).toBeInTheDocument();
    expect(screen.getByText('pvc-demo')).toBeInTheDocument();
    expect(screen.getByText('worker-1')).toBeInTheDocument();
    expect(screen.getByText('worker-2')).toBeInTheDocument();
    expect(screen.getByText('2/2 members operational')).toBeInTheDocument();
  });

  it('opens the drawer with replica details on member click, closes on Escape', () => {
    render(<VolumeTopologyGraph volume={mkVolume()} disks={disks} />);

    fireEvent.click(screen.getByText('worker-2'));
    expect(screen.getByRole('dialog', { name: /worker-2/ })).toBeInTheDocument();
    expect(screen.getByText('nqn.2024-01.io.flint:vol-1:w2')).toBeInTheDocument();

    // At body, not document: React Flow's own document-level key handlers
    // call target.hasAttribute, which Document doesn't implement in jsdom.
    fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('opens the educational drawer from the About button', () => {
    render(<VolumeTopologyGraph volume={mkVolume()} disks={disks} />);

    fireEvent.click(screen.getByRole('button', { name: /about this topology/i }));
    expect(screen.getByRole('dialog', { name: 'About this topology' })).toBeInTheDocument();
    expect(screen.getByText('RAID-1 (Mirroring)')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Close details' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });
});
