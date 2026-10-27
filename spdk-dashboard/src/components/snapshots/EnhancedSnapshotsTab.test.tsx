// The snapshots tab header chips count logical snapshots and per-node
// copies. Pre-merge, /api/snapshots returned one row per node copy, so
// "Total Snapshots" double-counted replicated snapshots and "Replica
// Snapshots" summed a field the endpoint never sent (always 0).
import { describe, expect, it, beforeEach } from 'vitest';
import userEvent from '@testing-library/user-event';
import { render, screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { MemoryRouter } from 'react-router';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { EnhancedSnapshotsTab } from './EnhancedSnapshotsTab';
import { OperationsProvider } from '../../contexts/OperationsContext';
import { server } from '../../test/server';
import { makeSnapshotList } from '../../test/fixtures';
import { login, logout } from '../../api/client';

// The tab now reads the shared dashboard query for the timeline's PVC-name
// search map, so it needs a QueryClient like the other query-bearing views.
const renderTab = () => {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={queryClient}>
        <OperationsProvider>
          <EnhancedSnapshotsTab />
        </OperationsProvider>
      </QueryClientProvider>
    </MemoryRouter>
  );
};

describe('EnhancedSnapshotsTab header chips', () => {
  beforeEach(() => {
    logout();
    server.use(
      http.get('/api/snapshots', () => HttpResponse.json(makeSnapshotList())),
      http.get('/api/snapshots/tree', () => HttpResponse.json({}))
    );
  });

  it('counts logical snapshots once and replica copies per node', async () => {
    await login('admin', 'right-password');
    renderTab();

    // Fixture: two user snapshots (2 copies + 1 copy) and one engine epoch
    // (2 copies): the kinds are counted apart, copies across all of them.
    const users = await screen.findByText('User Snapshots');
    expect(users.previousElementSibling).toHaveTextContent(/^2$/);

    const epochs = screen.getByText('Engine Epochs');
    expect(epochs.previousElementSibling).toHaveTextContent(/^1$/);

    const copies = screen.getByText('Replica Snapshots');
    expect(copies.previousElementSibling).toHaveTextContent(/^5$/);

    const ready = screen.getByText('Ready to Use');
    expect(ready.previousElementSibling).toHaveTextContent(/^3$/);
  });

  it('labels estimated overhead and raises no alert on it', async () => {
    const volume = 'pvc-93edc114-bec7-43a0-8273-5812c2c52d13';
    server.use(
      http.get('/api/snapshots/tree', () =>
        HttpResponse.json({
          [volume]: {
            volume_id: volume,
            volume_name: 'r3-data',
            volume_size: 8589934592,
            snapshots: [],
            snapshot_chain: { active_lvol: `vol_${volume}`, chain_depth: 0, snapshots: [] },
            storage_analytics: {
              estimated: true,
              total_volume_size: 8589934592,
              actual_data_size: 8589934592,
              total_snapshot_overhead: 15913451520,
              snapshot_efficiency_ratio: 1.85,
              storage_breakdown: {
                active_volume_consumption: 8589934592,
                snapshot_consumption: 15913451520,
                metadata_overhead: 0,
                free_space_in_volume: 0,
              },
              recommendations: ['Overhead figures are estimates until every lvol\'s consumption is readable'],
            },
          },
        })
      )
    );
    await login('admin', 'right-password');
    renderTab();
    await screen.findByText('User Snapshots');
    expect(screen.getByText('Snapshot Overhead (est.)')).toBeInTheDocument();
    expect(screen.getByText('Overhead Ratio (est.)')).toBeInTheDocument();
    expect(screen.queryByText('Storage Efficiency Warning')).not.toBeInTheDocument();
    // Both the header note and the storage view's say so.
    expect(screen.getAllByText(/Overhead figures are estimates/).length).toBeGreaterThanOrEqual(1);
  });

  it('labels each row by kind and filters by it', async () => {
    await login('admin', 'right-password');
    const user = userEvent.setup();
    renderTab();
    await screen.findByText('User Snapshots');
    await user.click(screen.getByRole('button', { name: /List View/ }));
    expect(await screen.findAllByText('user snapshot')).toHaveLength(2);
    expect(screen.getAllByText('engine epoch')).toHaveLength(1);

    await user.click(screen.getByRole('button', { name: /Filters/ }));
    await user.selectOptions(screen.getByLabelText('Snapshot Type'), 'epoch');
    expect(screen.queryAllByText('user snapshot')).toHaveLength(0);
    expect(screen.getAllByText('engine epoch')).toHaveLength(1);
  });
});
