// Canvas + drawer wiring, at jsdom depth: nodes render, selection opens the
// drawer with entity details, Escape and ✕ close it. Geometry/zoom behavior
// is React Flow's own and is not asserted here. fireEvent, not user-event:
// a real mousedown sequence reaches d3-zoom's pane handler, which reads
// event.view.document — always null in jsdom.
import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { VolumeTopologyGraph } from './VolumeTopologyGraph';
import { TopologyDrawer } from './TopologyDrawer';
import { joinMembers } from './buildTopology';
import { transformBackendData } from '../../../hooks/useDashboardData';
import type { Disk, Volume } from '../../../hooks/useDashboardData';
import { makeConsumerRaid, makeDashboardData, makeDisk, makeVolume } from '../../../test/fixtures';

// The hr-e2e fixture volume, assembled on runj-aws-1 with its second leg
// on runj-aws-2 — through the real wire transform.
function mkVolume(): Volume {
  const v = transformBackendData(
    makeDashboardData({ volumes: [makeVolume({ consumer_raids: [makeConsumerRaid()] })] })
  ).volumes[0];
  if (!v) throw new Error('fixture produced no volume');
  return v;
}

const disks: Disk[] = [];

describe('VolumeTopologyGraph', () => {
  it('renders every layer of the data path', () => {
    render(<VolumeTopologyGraph volume={mkVolume()} disks={disks} />);
    expect(screen.getByText('Consumer')).toBeInTheDocument();
    expect(screen.getByText('hr-e2e')).toBeInTheDocument();
    // The consumer card and the local leg both name runj-aws-1
    expect(screen.getAllByText('runj-aws-1').length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText('runj-aws-2')).toBeInTheDocument();
    expect(screen.getByText('2/2 bases configured')).toBeInTheDocument();
  });

  it('opens the drawer with replica details on member click, closes on Escape', () => {
    render(<VolumeTopologyGraph volume={mkVolume()} disks={disks} />);

    fireEvent.click(screen.getByText('runj-aws-2'));
    expect(screen.getByRole('dialog', { name: /runj-aws-2/ })).toBeInTheDocument();
    // The base bdev SPDK reports for that slot, and the sync record
    expect(screen.getByText('nvme_pvc-6ff1cf70-hr-e2e_1n1')).toBeInTheDocument();
    expect(screen.getByText('Sync state')).toBeInTheDocument();
    expect(screen.getByText('NVMe-oF from runj-aws-1')).toBeInTheDocument();

    // At body, not document: React Flow's own document-level key handlers
    // call target.hasAttribute, which Document doesn't implement in jsdom.
    fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('renders — not "Invalid Date" — when a provisioned entry has no PVC timestamp', () => {
    const volume = mkVolume();
    const disk = transformBackendData(
      makeDashboardData({
        disks: [
          makeDisk({
            provisioned_volumes: [
              {
                // Statically provisioned PV: backend sends an empty timestamp
                provisioned_at: '',
                replica_type: 'replica',
                size: 1024,
                status: 'healthy',
                volume_id: volume.id,
                volume_name: volume.name,
              },
            ],
          }),
        ],
      })
    ).disks[0];
    if (!disk) throw new Error('fixture produced no disk');

    const join = joinMembers(volume, [disk])[0]!;
    render(
      <TopologyDrawer
        volume={volume}
        detail={{ kind: 'disk', disk, join }}
        onClose={() => {}}
      />
    );
    expect(screen.getByText('—')).toBeInTheDocument();
    expect(screen.queryByText(/Invalid Date/)).not.toBeInTheDocument();
  });

  it('opens the educational drawer from the About button', () => {
    render(<VolumeTopologyGraph volume={mkVolume()} disks={disks} />);

    fireEvent.click(screen.getByRole('button', { name: /about this topology/i }));
    expect(screen.getByRole('dialog', { name: 'About this topology' })).toBeInTheDocument();
    expect(screen.getByText('How flint mirrors a volume')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Close details' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });
});
