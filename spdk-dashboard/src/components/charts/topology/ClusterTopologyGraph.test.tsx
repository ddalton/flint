// Cluster canvas + drawer wiring and the cluster→volume drill-down handoff,
// exercised through the real EnhancedRaidTopologyChart toggle. Same jsdom
// rules as the volume graph tests: fireEvent, keyDown at body.
import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { EnhancedRaidTopologyChart } from '../EnhancedRaidTopologyChart';
import type { Disk, ReplicaStatus, Volume } from '../../../hooks/useDashboardData';

function mkReplica(node: string, overrides: Partial<ReplicaStatus> = {}): ReplicaStatus {
  return {
    node,
    status: 'healthy',
    ...overrides,
  } as ReplicaStatus;
}

function mkVolume(id: string, nodes: string[]): Volume {
  return {
    active_replicas: nodes.length,
    consumer_raids: [],
    id,
    name: id,
    nodes: [],
    replicas: nodes.length,
    size: '2 GiB',
    spdk_validation_status: { has_spdk_backing: true, validation_severity: 'info' },
    state: 'Healthy',
    replica_statuses: nodes.map(n => mkReplica(n)),
  } as Volume;
}

const disks: Disk[] = [];
const volumes = [mkVolume('pvc-alpha', ['w1', 'w2']), mkVolume('pvc-beta', ['w2', 'w3'])];

describe('cluster topology view', () => {
  it('toggles to the cluster altitude and renders node cards and links', () => {
    render(
      <EnhancedRaidTopologyChart volumes={volumes} disks={disks} nodeNames={['w1', 'w2', 'w3']} />
    );

    fireEvent.click(screen.getByRole('tab', { name: 'cluster' }));
    expect(screen.getByText('Cluster Topology')).toBeInTheDocument();
    expect(screen.getByText('w1')).toBeInTheDocument();
    expect(screen.getByText('w2')).toBeInTheDocument();
    expect(screen.getByText('w3')).toBeInTheDocument();
    // w2 hosts a replica of both volumes
    expect(screen.getByText(/2 replicas • 2 volumes/)).toBeInTheDocument();
  });

  it('opens the node drawer and drills through a volume row into the volume view', () => {
    render(
      <EnhancedRaidTopologyChart volumes={volumes} disks={disks} nodeNames={['w1', 'w2', 'w3']} />
    );

    fireEvent.click(screen.getByRole('tab', { name: 'cluster' }));
    fireEvent.click(screen.getByText('w2'));
    const drawer = screen.getByRole('dialog', { name: 'Node — w2' });
    expect(drawer).toBeInTheDocument();

    // Drill into pvc-beta → volume altitude with it selected
    fireEvent.click(screen.getByRole('button', { name: /pvc-beta/ }));
    expect(screen.getByText('Volume Topology')).toBeInTheDocument();
    expect(screen.getByText('2/2 replicas in sync')).toBeInTheDocument();
    // The volume graph shows pvc-beta's replicas (w2, w3 — not w1)
    expect(screen.queryByText('w1')).not.toBeInTheDocument();
  });

  it('renders the cluster view with zero volumes (fresh cluster)', () => {
    render(<EnhancedRaidTopologyChart volumes={[]} disks={disks} nodeNames={['w1']} />);
    fireEvent.click(screen.getByRole('tab', { name: 'cluster' }));
    expect(screen.getByText('w1')).toBeInTheDocument();
    expect(screen.getByText(/0 replicas • 0 volumes/)).toBeInTheDocument();
  });
});
