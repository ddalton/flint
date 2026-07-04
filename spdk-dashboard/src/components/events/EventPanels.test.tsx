// The 2c surfaces: completed windows measured against the 2s target and the
// category-filterable engine timeline.
import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { EventTimelinePanel, HotRejoinWindowsPanel } from './EventPanels';
import { makeEvent, makeWindow } from '../../test/fixtures';
import type { EngineEvent, HotRejoinWindow } from '../../hooks/useEvents';

const window_ = (overrides: Parameters<typeof makeWindow>[0] = {}) =>
  makeWindow(overrides) as HotRejoinWindow;
const event = (overrides: Parameters<typeof makeEvent>[0] = {}) =>
  makeEvent(overrides) as EngineEvent;

describe('HotRejoinWindowsPanel', () => {
  it('shows an explanatory empty state, not a blank table', () => {
    render(<HotRejoinWindowsPanel windows={[]} />);
    expect(
      screen.getByText(/No completed hot-rejoin windows in recent history/)
    ).toBeInTheDocument();
  });

  it('renders a within-target window with its step breakdown and estimator', () => {
    render(<HotRejoinWindowsPanel windows={[window_()]} />);

    const bar = screen.getByRole('progressbar');
    expect(bar).toHaveAttribute('aria-valuenow', '1730');
    expect(bar).toHaveAttribute('aria-valuetext', '1730ms of 2000ms target');
    expect(screen.getByText('1730ms')).toBeInTheDocument();
    expect(screen.getByText('inline')).toBeInTheDocument();
    expect(screen.getByText('26 MiB est.')).toBeInTheDocument();
    expect(screen.getByText(/quiesce 102ms · fenced_delta_copy 1416ms/)).toBeInTheDocument();
    expect(screen.queryByTitle('Over the 2000ms target')).not.toBeInTheDocument();
  });

  it('flags a window over the 2s target and clamps the bar', () => {
    render(<HotRejoinWindowsPanel windows={[window_({ window_ms: 4100, path: 'esnap' })]} />);

    const bar = screen.getByRole('progressbar');
    expect(bar).toHaveAttribute('aria-valuenow', '2000'); // clamped to target
    expect(bar).toHaveAttribute('aria-valuetext', '4100ms of 2000ms target');
    expect(screen.getByTitle('Over the 2000ms target')).toBeInTheDocument();
    expect(screen.getByText('esnap')).toBeInTheDocument();
  });
});

describe('EventTimelinePanel', () => {
  const events = [
    event(),
    event({ category: 'health', reason: 'VolumeDegraded', event_type: 'Warning' }),
    event({ category: 'catchup', reason: 'ReplicaCatchupStarted' }),
  ];

  it('lists all events with per-category counts', () => {
    render(<EventTimelinePanel events={events} />);
    expect(screen.getByText('All (3)')).toBeInTheDocument();
    expect(screen.getByText('Hot rejoin (1)')).toBeInTheDocument();
    expect(screen.getByText('Health (1)')).toBeInTheDocument();
    expect(screen.getByText('HotRejoinSucceeded')).toBeInTheDocument();
    expect(screen.getByText('VolumeDegraded')).toBeInTheDocument();
  });

  it('filters the timeline by category chip', async () => {
    const user = userEvent.setup();
    render(<EventTimelinePanel events={events} />);

    await user.click(screen.getByText('Health (1)'));

    expect(screen.getByText('VolumeDegraded')).toBeInTheDocument();
    expect(screen.queryByText('HotRejoinSucceeded')).not.toBeInTheDocument();

    await user.click(screen.getByText('All (3)'));
    expect(screen.getByText('HotRejoinSucceeded')).toBeInTheDocument();
  });

  it('offers chips only for categories actually present', () => {
    render(<EventTimelinePanel events={[event()]} />);
    expect(screen.getByText('Hot rejoin (1)')).toBeInTheDocument();
    expect(screen.queryByText(/Cutover/)).not.toBeInTheDocument();
  });
});
