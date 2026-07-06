// The snapshots tab header chips count logical snapshots and per-node
// copies. Pre-merge, /api/snapshots returned one row per node copy, so
// "Total Snapshots" double-counted replicated snapshots and "Replica
// Snapshots" summed a field the endpoint never sent (always 0).
import { describe, expect, it, beforeEach } from 'vitest';
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

    // Fixture: one 2-copy snapshot + one 1-copy snapshot.
    const total = await screen.findByText('Total Snapshots');
    expect(total.previousElementSibling).toHaveTextContent(/^2$/);

    const copies = screen.getByText('Replica Snapshots');
    expect(copies.previousElementSibling).toHaveTextContent(/^3$/);

    const ready = screen.getByText('Ready to Use');
    expect(ready.previousElementSibling).toHaveTextContent(/^2$/);
  });
});
