// The redesigned per-volume snapshot timeline: two honest lanes (user
// VolumeSnapshots as markers, engine epochs as a density ribbon), pinned
// popover with the CR-path delete, admin-gated.
import { describe, expect, it, beforeEach } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { resolveVolumeInput, volumeInputMatches } from './volumeSearch';
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

  it('flags a ghost — CR-backed snapshot whose SPDK copies are all gone', async () => {
    await login('admin', 'right-password');
    const base = makeSnapshotTimeline();
    server.use(
      http.get('/api/snapshots/timeline', () =>
        HttpResponse.json({
          ...base,
          // snap-demo-2's copies were deleted out-of-band; the CR remains
          // and still claims ready.
          events: base.events.map((e) =>
            e.name === 'snap-demo-2' ? { ...e, nodes: [] } : e
          ),
        })
      )
    );
    const user = userEvent.setup();
    renderView();

    // Legend surfaces the exception; the marker names it for a11y.
    expect(await screen.findByText(/1 without copies/)).toBeInTheDocument();
    await user.click(
      screen.getByRole('button', { name: 'User snapshot snap-demo-2 (no copies)' })
    );

    const popover = await screen.findByRole('dialog', { name: 'Snapshot details' });
    expect(popover).toHaveTextContent('none');
    expect(popover).toHaveTextContent('No SPDK copies exist on any node');
    expect(popover).toHaveTextContent(/restore will fail/);
    // Clean-up path stays available: the CR delete is exactly the remedy.
    expect(within(popover).getByRole('button', { name: /Delete/ })).toBeEnabled();

    // Healthy snapshots are untouched.
    expect(
      screen.getByRole('button', { name: 'User snapshot snap-demo-1' })
    ).toBeInTheDocument();
  });

  it('surfaces a ghost hidden inside a collapsed cluster marker', async () => {
    // Three snapshots cut seconds apart collapse into one +N marker (the
    // live runl drill). The ghost must still read on the cluster, not only
    // in the legend — else a lane scan misses it.
    await login('admin', 'right-password');
    const now = Date.now();
    // Identical cut time → one x position → guaranteed single cluster,
    // independent of chart width (the live runl drill's 3-in-7s burst).
    const cutAt = new Date(now - 5000).toISOString();
    const burst = (n: number, nodes: string[]) => ({
      id: `c${n}`, kind: 'user' as const, name: `burst-${n}`, spdk_name: `snap_x_${n}`,
      created_at: cutAt, size_bytes: 1, ready: true, nodes,
      vs_namespace: 'default', vs_name: `burst-${n}`, vsc_name: `c${n}`, orphan: false,
    });
    server.use(
      http.get('/api/snapshots/timeline', () =>
        HttpResponse.json(
          makeSnapshotTimeline({
            now: new Date(now).toISOString(),
            current_epoch: null,
            events: [
              burst(1, ['runk-aws-1']),
              burst(2, []), // ghost: CR present, no copies
              burst(3, ['runk-aws-1']),
            ],
          })
        )
      )
    );
    const user = userEvent.setup();
    renderView();

    // The collapsed cluster marker itself announces the contained ghost.
    const cluster = await screen.findByRole('button', {
      name: '3 user snapshots (1 without copies)',
    });
    expect(cluster).toBeInTheDocument();
    expect(screen.getByText(/1 without copies/)).toBeInTheDocument();

    // Drilling in still reaches the ghost's warning + delete remedy.
    await user.click(cluster);
    const list = await screen.findByRole('dialog', { name: 'Snapshot details' });
    await user.click(within(list).getByRole('button', { name: /burst-2/ }));
    expect(list).toHaveTextContent('No SPDK copies exist on any node');
  });

  it('brush-zooms the lanes to a dragged window and resets from the chip', async () => {
    await login('admin', 'right-password');
    const user = userEvent.setup();
    renderView();
    await screen.findByText(/User snapshots · 3/);

    // Fixture: user snapshots at 280/160/45s ago; domain [now-325.5s, now]
    // at the 900px JSDOM fallback width puts them at x≈126/458/776. A drag
    // from 350 to 890 windows in demo-2 and demo-3, excludes demo-1.
    const brush = screen.getByTestId('timeline-brush');
    fireEvent.mouseDown(brush, { clientX: 350 });
    fireEvent.mouseMove(window, { clientX: 890 });
    fireEvent.mouseUp(window, { clientX: 890 });

    await waitFor(() =>
      expect(
        screen.queryByRole('button', { name: 'User snapshot snap-demo-1' })
      ).not.toBeInTheDocument()
    );
    expect(screen.getByRole('button', { name: 'User snapshot snap-demo-2' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'User snapshot snap-demo-3' })).toBeInTheDocument();
    // Legend counts stay full-history totals — zoom navigates, never hides.
    expect(screen.getByText(/User snapshots · 3/)).toBeInTheDocument();
    // Zoomed short of the live edge, the "now" pulse would be a lie.
    expect(screen.queryByText('now')).not.toBeInTheDocument();
    // The brush window is a keyboard-operable slider on the context strip.
    expect(screen.getByRole('slider', { name: 'Timeline zoom window' })).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Reset zoom' }));
    expect(
      await screen.findByRole('button', { name: 'User snapshot snap-demo-1' })
    ).toBeInTheDocument();
    expect(screen.getByText('now')).toBeInTheDocument();
  });

  it('clears the zoom from the keyboard (Escape on the window)', async () => {
    await login('admin', 'right-password');
    renderView();
    await screen.findByText(/User snapshots · 3/);

    const brush = screen.getByTestId('timeline-brush');
    fireEvent.mouseDown(brush, { clientX: 600 });
    fireEvent.mouseMove(window, { clientX: 800 });
    fireEvent.mouseUp(window, { clientX: 800 });

    const slider = await screen.findByRole('slider', { name: 'Timeline zoom window' });
    fireEvent.keyDown(slider, { key: 'Escape' });
    await waitFor(() =>
      expect(
        screen.queryByRole('slider', { name: 'Timeline zoom window' })
      ).not.toBeInTheDocument()
    );
    expect(screen.getByText('now')).toBeInTheDocument();
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


// Search resolution: operators type PVC names; the timeline keys on pv ids.
describe('resolveVolumeInput', () => {
  const ids = ['pvc-aaa-111', 'pvc-bbb-222'];
  const names = { 'pvc-aaa-111': 'workload-a', 'pvc-bbb-222': 'workload-b' };

  it('accepts an exact id', () => {
    expect(resolveVolumeInput('pvc-aaa-111', ids, names)).toBe('pvc-aaa-111');
  });

  it('resolves an exact PVC name to its id', () => {
    expect(resolveVolumeInput('workload-a', ids, names)).toBe('pvc-aaa-111');
  });

  it('resolves a unique substring of a name or id', () => {
    expect(resolveVolumeInput('load-b', ids, names)).toBe('pvc-bbb-222');
    expect(resolveVolumeInput('bbb', ids, names)).toBe('pvc-bbb-222');
  });

  it('refuses ambiguous input but exposes the candidates', () => {
    expect(resolveVolumeInput('workload', ids, names)).toBeNull();
    expect(volumeInputMatches('workload', ids, names)).toEqual(ids);
  });

  it('is empty-safe and works without a name map', () => {
    expect(resolveVolumeInput('', ids, names)).toBeNull();
    expect(resolveVolumeInput('aaa', ids, {})).toBe('pvc-aaa-111');
  });
});
