// The redesigned per-volume snapshot timeline: two honest lanes (user
// VolumeSnapshots as markers, engine epochs as a density ribbon), pinned
// popover with the CR-path delete, admin-gated.
import { describe, expect, it, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { SnapshotTimelineView } from './SnapshotTimelineView';
import { server } from '../../test/server';
import { makeSnapshotTimeline } from '../../test/fixtures';
import { login, logout } from '../../api/client';

const VOLUME = 'pvc-93edc114-bec7-43a0-8273-5812c2c52d13';

const renderView = (volume = VOLUME) => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <SnapshotTimelineView
        selectedVolume={volume}
        onVolumeChange={() => {}}
        availableVolumes={[VOLUME]}
      />
    </QueryClientProvider>
  );
};

describe('SnapshotTimelineView', () => {
  beforeEach(() => logout());

  it('renders both lanes with counts, live replica chips, and the now anchor', async () => {
    await login('admin', 'right-password');
    renderView();

    expect(await screen.findByText(/User snapshots · 3/)).toBeInTheDocument();
    expect(screen.getByText(/Engine epochs · 6/)).toBeInTheDocument();
    expect(screen.getByText(/\+1 rotating/)).toBeInTheDocument();
    expect(screen.getByText('epoch #9')).toBeInTheDocument();
    expect(screen.getByText('runk-aws-1 · in_sync')).toBeInTheDocument();
    expect(screen.getByText('now')).toBeInTheDocument();

    // Three separately clickable user markers on the lane.
    expect(screen.getByRole('button', { name: 'User snapshot snap-demo-1' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'User snapshot snap-demo-3' })).toBeInTheDocument();
  });

  it('pins a detail popover on click and deletes through the VolumeSnapshot CR', async () => {
    await login('admin', 'right-password');
    const deleted: string[] = [];
    server.use(
      http.delete('/api/volumesnapshots/:namespace/:name', ({ params }) => {
        deleted.push(`${params.namespace}/${params.name}`);
        return HttpResponse.json({
          success: true,
          namespace: String(params.namespace),
          name: String(params.name),
        });
      })
    );
    const user = userEvent.setup();
    renderView();

    await user.click(await screen.findByRole('button', { name: 'User snapshot snap-demo-2' }));
    // Popover: real metadata, actions live here (not in a hover tooltip).
    const popover = await screen.findByRole('dialog', { name: 'Snapshot details' });
    expect(popover).toHaveTextContent('snap-demo-2');
    expect(popover).toHaveTextContent(/ago/);
    expect(popover).toHaveTextContent('runk-aws-1, runk-aws-2');

    await user.click(screen.getByRole('button', { name: /Delete/ }));
    // Destructive-kit confirm, then the CR-path DELETE.
    expect(await screen.findByRole('alertdialog')).toHaveTextContent(/VolumeSnapshot default\/snap-demo-2/);
    await user.click(screen.getByRole('button', { name: 'Delete snapshot' }));

    await waitFor(() => expect(deleted).toEqual(['default/snap-demo-2']));
    await waitFor(() =>
      expect(screen.queryByRole('dialog', { name: 'Snapshot details' })).not.toBeInTheDocument()
    );
  });

  it('disables Delete for viewers — reading is free, destruction is not', async () => {
    await login('viewer', 'right-password');
    const user = userEvent.setup();
    renderView();

    await user.click(await screen.findByRole('button', { name: 'User snapshot snap-demo-1' }));
    const del = await screen.findByRole('button', { name: /Delete/ });
    expect(del).toBeDisabled();
    expect(del).toHaveAttribute('title', 'Admin login required');
  });

  it('never offers CR deletion for orphaned SPDK snapshots', async () => {
    await login('admin', 'right-password');
    const base = makeSnapshotTimeline();
    server.use(
      http.get('/api/snapshots/timeline', () =>
        HttpResponse.json(
          makeSnapshotTimeline({
            events: [
              ...base.events.filter((e) => e.kind === 'epoch'),
              {
                id: `snap_${VOLUME}_99999`,
                kind: 'user',
                name: `snap_${VOLUME}_99999`,
                spdk_name: `snap_${VOLUME}_99999`,
                created_at: new Date(Date.now() - 30_000).toISOString(),
                size_bytes: 2147483648,
                ready: true,
                nodes: ['runk-aws-1'],
                orphan: true,
              },
            ],
          })
        )
      )
    );
    const user = userEvent.setup();
    renderView();

    await user.click(await screen.findByRole('button', { name: /User snapshot snap_/ }));
    const popover = await screen.findByRole('dialog', { name: 'Snapshot details' });
    expect(popover).toHaveTextContent('orphan');
    expect(screen.queryByRole('button', { name: /Delete/ })).not.toBeInTheDocument();
  });

  it('explains an empty volume instead of rendering a bare card', async () => {
    await login('admin', 'right-password');
    server.use(
      http.get('/api/snapshots/timeline', () =>
        HttpResponse.json(
          makeSnapshotTimeline({ events: [], untracked_epochs: 0, current_epoch: null })
        )
      )
    );
    renderView();
    expect(await screen.findByText('No Snapshot History Yet')).toBeInTheDocument();
  });
});
