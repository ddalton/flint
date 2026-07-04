// Router-shell integration: the tab is the path, filters are search params,
// unknown paths bounce home, and the state-aware landing (plan Decision 2)
// only ever fires on the bare "/" entry — never on an explicit deep link.
import { describe, expect, it } from 'vitest';
import { MemoryRouter, Navigate, Route, Routes } from 'react-router';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render, screen } from '@testing-library/react';
import { Dashboard } from './Dashboard';
import { OperationsProvider } from '../contexts/OperationsContext';
import { computeStats, transformBackendData, type DashboardData } from '../hooks/useDashboardData';
import { makeDashboardData, makeDisk } from '../test/fixtures';

const renderAt = (path: string, data: DashboardData) => {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <MemoryRouter initialEntries={[path]}>
      <QueryClientProvider client={queryClient}>
      <OperationsProvider autoRefresh={false} onAutoRefreshChange={() => {}}>
        <Routes>
          <Route
            path="/:tab?"
            element={
              <Dashboard
                data={data}
                loading={false}
                stats={computeStats(data)}
                autoRefresh={false}
                onAutoRefreshChange={() => {}}
                onRefresh={() => {}}
                onLogout={() => {}}
                connectionError={null}
              />
            }
          />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </OperationsProvider>
      </QueryClientProvider>
    </MemoryRouter>
  );
};

const provisioned = () => transformBackendData(makeDashboardData());

const fresh = () =>
  transformBackendData(
    makeDashboardData({
      volumes: [],
      disks: [makeDisk({ blobstore_initialized: false })],
    })
  );

describe('Dashboard URL state', () => {
  it('activates the tab named by the path', () => {
    renderAt('/volumes', provisioned());
    expect(screen.getByRole('link', { name: /Volumes/ })).toHaveAttribute(
      'aria-current',
      'page'
    );
    // The managed fixture volume is listed.
    expect(screen.getByText('hr-e2e')).toBeInTheDocument();
  });

  it('applies the volume filter from ?filter=', () => {
    renderAt('/volumes?filter=degraded', provisioned());
    expect(screen.getByText('Degraded Volumes')).toBeInTheDocument();
    expect(screen.getByText(/Active:/)).toBeInTheDocument();
  });

  it('bounces an unknown tab segment back to the landing entry', () => {
    renderAt('/no-such-tab', provisioned());
    expect(screen.getByRole('link', { name: /Overview/ })).toHaveAttribute(
      'aria-current',
      'page'
    );
  });

  it('lands a fresh cluster on Disk Setup from the bare entry point', () => {
    renderAt('/', fresh());
    expect(screen.getByRole('link', { name: /Disk Setup/ })).toHaveAttribute(
      'aria-current',
      'page'
    );
  });

  it('never hijacks an explicit deep link, even on a fresh cluster', () => {
    renderAt('/volumes', fresh());
    expect(screen.getByRole('link', { name: /Volumes/ })).toHaveAttribute(
      'aria-current',
      'page'
    );
  });

  it('keeps a provisioned cluster on Overview', () => {
    renderAt('/', provisioned());
    expect(screen.getByRole('link', { name: /Overview/ })).toHaveAttribute(
      'aria-current',
      'page'
    );
  });
});
