// The 2a control's accessibility contract: every sync state renders a
// labeled chip, numeric progress (epoch lag) is a real progressbar with
// aria-valuenow, and the sub-2s rejoin renders as an indeterminate window
// marker — exactly what the live drills exercised on runj.
import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import { SyncStateIndicator, VolumeSyncSummary } from './SyncStateIndicator';
import { makeReplica, makeSyncInfo, makeVolume } from '../../test/fixtures';
import { transformBackendData } from '../../hooks/useDashboardData';
import { makeDashboardData } from '../../test/fixtures';
import type { ReplicaSyncInfo } from '../../hooks/useDashboardData';

const sync = (overrides: Parameters<typeof makeSyncInfo>[0] = {}) =>
  makeSyncInfo(overrides) as ReplicaSyncInfo;

describe('SyncStateIndicator', () => {
  it('renders nothing without a sync record (single-replica volumes)', () => {
    const { container } = render(<SyncStateIndicator sync={null} />);
    expect(container).toBeEmptyDOMElement();
  });

  it('in_sync renders the chip alone — no progress control', () => {
    render(<SyncStateIndicator sync={sync()} />);
    expect(screen.getByText('in sync')).toBeInTheDocument();
    expect(screen.queryByRole('progressbar')).not.toBeInTheDocument();
  });

  it('stale with numeric lag exposes aria-valuenow on a progressbar', () => {
    render(
      <SyncStateIndicator
        sync={sync({ sync_state: 'stale', epoch_lag: 2 })}
        node="runj-aws-2"
      />
    );
    expect(screen.getByText('stale')).toBeInTheDocument();
    const bar = screen.getByRole('progressbar', { name: 'runj-aws-2 epoch catch-up' });
    expect(bar).toHaveAttribute('aria-valuenow', '2');
    expect(bar).toHaveTextContent('2 epochs behind');
  });

  it('unknowable lag renders as indeterminate catch-up (no aria-valuenow)', () => {
    render(<SyncStateIndicator sync={sync({ sync_state: 'standby', epoch_lag: null })} />);
    const bar = screen.getByRole('progressbar', { name: 'epoch catch-up' });
    expect(bar).not.toHaveAttribute('aria-valuenow');
    expect(bar).toHaveTextContent('catching up');
  });

  it('a live hot_rejoin marker takes over the chip as "rejoining"', () => {
    render(
      <SyncStateIndicator
        sync={sync({ sync_state: 'stale', hot_rejoin: 'epoch-hr-e2e-1262' })}
      />
    );
    expect(screen.getByText('rejoining')).toBeInTheDocument();
    const bar = screen.getByRole('progressbar', { name: 'hot rejoin' });
    expect(bar).toHaveAttribute(
      'aria-valuetext',
      'rejoin window in flight at epoch-hr-e2e-1262'
    );
    // The lag readout belongs to catch-up, not the in-flight window.
    expect(screen.queryByText(/behind|catching up/)).not.toBeInTheDocument();
  });
});

describe('VolumeSyncSummary', () => {
  const toVolume = (v: ReturnType<typeof makeVolume>) => {
    const volume = transformBackendData(makeDashboardData({ volumes: [v] })).volumes[0];
    if (!volume) throw new Error('fixture produced no volume');
    return volume;
  };

  it('renders a neutral dash for volumes without sync data', () => {
    const volume = toVolume(
      makeVolume({ replica_statuses: [makeReplica({ sync: null })] })
    );
    render(<VolumeSyncSummary volume={volume} />);
    expect(screen.getByText('—')).toBeInTheDocument();
  });

  it('collapses an all-in_sync volume to one compact chip', () => {
    render(<VolumeSyncSummary volume={toVolume(makeVolume())} />);
    expect(screen.getAllByText('in sync')).toHaveLength(1);
  });

  it('lists each degraded replica labeled by node', () => {
    const volume = toVolume(
      makeVolume({
        replica_statuses: [
          makeReplica(),
          makeReplica({
            node: 'runj-aws-2',
            sync: makeSyncInfo({ sync_state: 'stale', epoch_lag: 1 }),
          }),
          makeReplica({
            node: 'runj-aws-3',
            sync: makeSyncInfo({ sync_state: 'standby', epoch_lag: 2 }),
          }),
        ],
      })
    );
    render(<VolumeSyncSummary volume={volume} />);
    expect(screen.queryByText('in sync')).not.toBeInTheDocument();
    expect(screen.getByText('runj-aws-2:')).toBeInTheDocument();
    expect(screen.getByText('stale')).toBeInTheDocument();
    expect(screen.getByText('runj-aws-3:')).toBeInTheDocument();
    expect(screen.getByText('standby')).toBeInTheDocument();
  });
});
