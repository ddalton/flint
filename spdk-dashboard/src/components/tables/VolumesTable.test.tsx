// The volumes table against the wire shapes the live clusters produce: a
// replica-set volume and an NFS share (no SPDK legs at all — dash1, 2026-10-27).
import { describe, expect, it } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router';
import { VolumesTable } from './VolumesTable';
import { OperationsProvider } from '../../contexts/OperationsContext';
import { transformBackendData } from '../../hooks/useDashboardData';
import { makeDashboardData, makeReplica, makeVolume } from '../../test/fixtures';

const nfsShare = makeVolume({
  id: 'pvc-nfs-share',
  name: 'rwx-shared',
  state: 'Unknown',
  replicas: 0,
  active_replicas: 0,
  nodes: [],
  replica_statuses: [],
  consumer_raids: [],
  current_epoch: null,
  spdk_validation_status: {
    has_spdk_backing: false,
    validation_message: 'NFS share served from an emptyDir: no SPDK volume by design',
    validation_severity: 'info',
  },
});

// A single-replica volume whose node agent did not answer: the backend
// cannot verify the lvol (severity "warning"), which is not a failed leg.
const unverifiable = makeVolume({
  id: 'pvc-r1-unverified',
  name: 'r1-data',
  state: 'Unknown',
  replicas: 1,
  active_replicas: 0,
  nodes: ['runj-aws-2'],
  replica_statuses: [{ node: 'runj-aws-2', status: 'unknown', sync: null }],
  consumer_raids: [],
  current_epoch: null,
  spdk_validation_status: {
    has_spdk_backing: false,
    validation_message: 'Cannot verify: connection refused',
    validation_severity: 'warning',
  },
});

// A replica-set volume whose record still lists a leg on a node that has
// left the cluster (the fixture cluster has runj-aws-1..3).
const legOnGoneNode = makeVolume({
  id: 'pvc-gone-leg',
  name: 'r3-idle',
  nodes: ['runj-aws-1', 'runj-aws-9'],
  replica_statuses: [makeReplica(), makeReplica({ node: 'runj-aws-9' })],
});

const renderTable = () => {
  const data = transformBackendData(
    makeDashboardData({ volumes: [makeVolume(), nfsShare, unverifiable, legOnGoneNode] })
  );
  return render(
    <MemoryRouter>
      <OperationsProvider>
        <VolumesTable volumes={data.volumes} disks={data.disks} nodes={data.nodes} />
      </OperationsProvider>
    </MemoryRouter>
  );
};

describe('VolumesTable', () => {
  it('counts the legs of a replica-set volume', () => {
    renderTable();
    const row = screen.getByText('hr-e2e').closest('tr')!;
    expect(within(row).getByRole('button', { name: '2/2' })).toBeInTheDocument();
    expect(within(row).queryByText(/not in sync/)).not.toBeInTheDocument();
  });

  it('shows an NFS share as having no legs, with the note, not as a failed replica', () => {
    renderTable();
    const row = screen.getByText('rwx-shared').closest('tr')!;
    // No replica count to click, no "not in sync" line, no red shield.
    expect(within(row).queryByRole('button', { name: /\d+\/\d+/ })).not.toBeInTheDocument();
    expect(within(row).queryByText(/not in sync/)).not.toBeInTheDocument();
    const notes = within(row).getAllByTitle(/no SPDK volume by design/);
    expect(notes.length).toBeGreaterThanOrEqual(2); // the state marker and the replicas dash
    expect(within(row).getByText('Unknown')).toBeInTheDocument();
  });

  it('marks a leg whose node has left the cluster', () => {
    renderTable();
    const row = screen.getByText('r3-idle').closest('tr')!;
    const gone = within(row).getByText('runj-aws-9');
    expect(gone).toHaveAttribute('title', expect.stringContaining('no longer in the cluster'));
    expect(gone.className).toContain('line-through');
    expect(within(row).getByText('runj-aws-1').className).not.toContain('line-through');
  });

  it('calls an unverifiable single replica unverified, not failed or out of sync', () => {
    renderTable();
    const row = screen.getByText('r1-data').closest('tr')!;
    expect(within(row).getByRole('button', { name: '0/1' })).toBeInTheDocument();
    const note = within(row).getByText('1 replica unverified');
    expect(note).toHaveAttribute('title', 'Cannot verify: connection refused');
    expect(within(row).queryByText(/not in sync|failed/)).not.toBeInTheDocument();
    // The amber shield carries the backend's message too.
    expect(within(row).getAllByTitle('Cannot verify: connection refused')).toHaveLength(2);
  });
});
