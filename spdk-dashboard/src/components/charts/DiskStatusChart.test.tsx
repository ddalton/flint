// The per-node disk strip: one cell per disk colored by its volume-store
// state, a legend with counts — what the stacked bar chart said in 300 px.
import { describe, expect, it } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import { DiskStatusChart } from './DiskStatusChart';
import { transformBackendData } from '../../hooks/useDashboardData';
import { makeDashboardData, makeDisk } from '../../test/fixtures';

describe('DiskStatusChart', () => {
  it('draws one labelled cell per disk under its node, and counts per state', () => {
    const wire = makeDashboardData();
    wire.disks = [
      makeDisk(),
      makeDisk({ id: 'runj-aws-2:0000:00:1f.0', node: 'runj-aws-2' }),
      makeDisk({ id: 'runj-aws-3:0000:00:1f.0', node: 'runj-aws-3', blobstore_initialized: false }),
      makeDisk({ id: 'runj-aws-3:0000:00:04.0', node: 'runj-aws-3', is_system_disk: true }),
    ];
    render(<DiskStatusChart disks={transformBackendData(wire).disks} />);

    const rows = within(screen.getByRole('list', { name: 'Disks by node' })).getAllByRole('listitem');
    expect(rows.map(r => r.textContent)).toEqual([
      'runj-aws-11 disk',
      'runj-aws-21 disk',
      'runj-aws-32 disks',
    ]);
    expect(screen.getByRole('img', { name: 'runj-aws-3:0000:00:1f.0: Uninitialized' })).toBeInTheDocument();
    expect(screen.getByRole('img', { name: 'runj-aws-3:0000:00:04.0: System' })).toBeInTheDocument();
    expect(screen.getAllByRole('img', { name: /: Healthy$/ })).toHaveLength(2);
    const legend = screen.getByRole('list', { name: 'Legend' });
    expect(legend.textContent).toContain('Healthy· 2');
    expect(legend.textContent).toContain('Uninitialized· 1');
    expect(legend.textContent).toContain('System· 1');
  });

  it('says so when no disk has been discovered', () => {
    render(<DiskStatusChart disks={[]} />);
    expect(screen.getByText(/No disks discovered yet/)).toBeInTheDocument();
  });
});
