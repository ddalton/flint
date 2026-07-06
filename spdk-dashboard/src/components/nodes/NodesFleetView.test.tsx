// Fleet-scale nodes tab: health facets + status-cell heatmap over the
// /api/nodes rollup, problems-first rows, drill-in detail via ?node=.
import { describe, expect, it, beforeEach } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { NodesFleetView } from './NodesFleetView';
import { OperationsProvider } from '../../contexts/OperationsContext';
import { transformBackendData } from '../../hooks/useDashboardData';
import { makeDashboardData } from '../../test/fixtures';
import { login, logout } from '../../api/client';

const renderView = () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={client}>
        <OperationsProvider>
          <NodesFleetView data={transformBackendData(makeDashboardData())} />
        </OperationsProvider>
      </QueryClientProvider>
    </MemoryRouter>
  );
};

describe('NodesFleetView', () => {
  beforeEach(() => logout());

  it('renders facet counts, one heatmap cell per node, problems first', async () => {
    await login('admin', 'right-password');
    renderView();

    // Facets from the fixture fleet: 3 nodes, 1 warning, 2 ok, 1 with
    // uninitialized disks.
    expect(await screen.findByRole('button', { name: 'All · 3' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Warning · 1' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Ready · 2' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Uninit. disks · 1' })).toBeInTheDocument();

    // Heatmap: one status cell per node.
    expect(screen.getByRole('button', { name: 'runj-aws-1: Ready' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'runj-aws-2: Warning' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'runj-aws-3: Ready' })).toBeInTheDocument();

    // Problems first: the warning node's row leads the list.
    const rows = screen.getAllByRole('button', { expanded: false });
    const rowNames = rows
      .map(r => r.textContent ?? '')
      .filter(t => t.includes('runj-aws'));
    expect(rowNames[0]).toContain('runj-aws-2');
  });

  it('filters the list by health facet', async () => {
    await login('admin', 'right-password');
    const user = userEvent.setup();
    renderView();

    await user.click(await screen.findByRole('button', { name: 'Warning · 1' }));

    expect(screen.getByText('Showing 1 of 3 nodes')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'runj-aws-1: Ready' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'runj-aws-2: Warning' })).toBeInTheDocument();
  });

  it('drills into a node row and shows the aggregate disk detail', async () => {
    await login('admin', 'right-password');
    const user = userEvent.setup();
    renderView();

    const row = (await screen.findAllByRole('button', { expanded: false })).find(b =>
      (b.textContent ?? '').includes('runj-aws-1')
    );
    expect(row).toBeDefined();
    await user.click(row!);

    expect(row).toHaveAttribute('aria-expanded', 'true');
    const detail = screen.getByText(/NVMe Disks & Logical Volume Stores on runj-aws-1/);
    expect(detail).toBeInTheDocument();
    // The aggregate-fed disk table shows the fixture disk on that node.
    const table = screen.getByRole('table');
    expect(within(table).getAllByText(/0000:00:1f\.0/).length).toBeGreaterThan(0);
  });
});
