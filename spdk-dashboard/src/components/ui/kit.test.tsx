// Design-system kit contracts: the ARIA shape of the progress control, the
// four-state AsyncView contract (incl. the never-blank-good-data rule), and
// the ConfirmModal typed-phrase gate.
import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ProgressBar } from './ProgressBar';
import { AsyncView } from './AsyncView';
import { ConfirmModal } from './ConfirmModal';
import { MemberStateChip, VolumeStateChip } from './Chip';

describe('ProgressBar', () => {
  it('exposes determinate progress via ARIA', () => {
    render(<ProgressBar value={3} max={10} label="epoch catch-up" valueText="3 of 10 epochs" />);
    const bar = screen.getByRole('progressbar', { name: 'epoch catch-up' });
    expect(bar).toHaveAttribute('aria-valuenow', '3');
    expect(bar).toHaveAttribute('aria-valuemax', '10');
    expect(bar).toHaveAttribute('aria-valuetext', '3 of 10 epochs');
  });

  it('indeterminate mode omits aria-valuenow', () => {
    render(<ProgressBar indeterminate label="rejoining" />);
    expect(screen.getByRole('progressbar', { name: 'rejoining' })).not.toHaveAttribute(
      'aria-valuenow'
    );
  });

  it('clamps out-of-range values', () => {
    render(<ProgressBar value={150} max={100} label="overfull" />);
    expect(screen.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '100');
  });
});

describe('AsyncView', () => {
  const view = (props: Partial<Parameters<typeof AsyncView<string[]>>[0]>) =>
    render(
      <AsyncView<string[]>
        loading={false}
        data={undefined}
        hasData={(d) => d.length > 0}
        emptyTitle="No volumes"
        emptyHint="Provision a PVC and it appears here."
        {...props}
      >
        {(data) => <ul>{data.map((x) => <li key={x}>{x}</li>)}</ul>}
      </AsyncView>
    );

  it('loading with no data renders the skeleton', () => {
    view({ loading: true });
    expect(screen.getByRole('status', { name: 'Loading' })).toBeInTheDocument();
  });

  it('error with no data renders an actionable error', async () => {
    const retry = vi.fn();
    view({ error: 'Backend error (HTTP 500)', onRetry: retry });
    expect(screen.getByRole('alert')).toHaveTextContent('Backend error (HTTP 500)');
    await userEvent.setup().click(screen.getByRole('button', { name: 'Retry' }));
    expect(retry).toHaveBeenCalled();
  });

  it('empty data explains what would populate the view', () => {
    view({ data: [] });
    expect(screen.getByText('No volumes')).toBeInTheDocument();
    expect(screen.getByText('Provision a PVC and it appears here.')).toBeInTheDocument();
  });

  it('an error never blanks out data already on screen — stale banner instead', () => {
    view({ data: ['vol-a'], error: 'Backend error (HTTP 500)' });
    expect(screen.getByText('vol-a')).toBeInTheDocument();
    expect(screen.getByRole('status')).toHaveTextContent('showing the last data received');
  });
});

describe('ConfirmModal', () => {
  it('gates confirm behind the typed phrase', async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    render(
      <ConfirmModal
        title="Initialize 12 disks for SPDK"
        danger="All data will be destroyed."
        confirmLabel="Initialize"
        confirmPhrase="initialize 12 disks"
        onConfirm={onConfirm}
        onCancel={() => {}}
      />
    );

    const confirm = screen.getByRole('button', { name: 'Initialize' });
    expect(confirm).toBeDisabled();
    await user.click(confirm);
    expect(onConfirm).not.toHaveBeenCalled();

    await user.type(screen.getByRole('textbox'), 'initialize 12 disks');
    expect(confirm).toBeEnabled();
    await user.click(confirm);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('Escape cancels and initial focus is on Cancel (Enter cannot blind-confirm)', async () => {
    const onCancel = vi.fn();
    render(
      <ConfirmModal
        title="Delete volume"
        danger="Gone forever."
        confirmLabel="Delete"
        onConfirm={() => {}}
        onCancel={onCancel}
      />
    );
    expect(screen.getByRole('button', { name: 'Cancel' })).toHaveFocus();
    await userEvent.setup().keyboard('{Escape}');
    expect(onCancel).toHaveBeenCalled();
  });
});

describe('status chips', () => {
  it('volume and member chips read from the shared tokens', () => {
    render(
      <>
        <VolumeStateChip state="Degraded" />
        <MemberStateChip state="stale" />
      </>
    );
    expect(screen.getByText('Degraded')).toBeInTheDocument();
    expect(screen.getByText('stale')).toBeInTheDocument();
  });
});
