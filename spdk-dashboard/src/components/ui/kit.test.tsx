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
import { SegmentedControl } from './SegmentedControl';
import { Pagination } from './Pagination';
import { Grid, List } from 'lucide-react';

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

describe('SegmentedControl', () => {
  const options = [
    { value: 'list', label: 'List' },
    { value: 'tree', label: 'Tree' },
  ] as const;

  it('marks exactly the active segment pressed and switches on click', async () => {
    const onChange = vi.fn();
    render(
      <SegmentedControl
        aria-label="Snapshot view"
        options={[...options]}
        value="list"
        onChange={onChange}
      />
    );
    expect(screen.getByRole('group', { name: 'Snapshot view' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'List' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByRole('button', { name: 'Tree' })).toHaveAttribute('aria-pressed', 'false');
    await userEvent.setup().click(screen.getByRole('button', { name: 'Tree' }));
    expect(onChange).toHaveBeenCalledWith('tree');
  });

  it('iconOnly still names every segment (sr-only + title)', () => {
    render(
      <SegmentedControl
        aria-label="Disk view mode"
        iconOnly
        options={[
          { value: 'grid', label: 'Grid view', icon: Grid },
          { value: 'compact', label: 'Compact view', icon: List },
        ]}
        value="compact"
        onChange={() => {}}
      />
    );
    const grid = screen.getByRole('button', { name: 'Grid view' });
    expect(grid).toHaveAttribute('title', 'Grid view');
  });
});

describe('Pagination', () => {
  const props = {
    page: 1,
    pageCount: 3,
    onPage: vi.fn(),
    pageSize: 25,
    onPageSize: vi.fn(),
    totalItems: 60,
    itemNoun: 'disks',
  };

  it('shows the range, disables Previous on page 1, and pages forward', async () => {
    const onPage = vi.fn();
    render(<Pagination {...props} onPage={onPage} />);
    expect(screen.getByText('Showing 1-25 of 60 disks')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Next page' }));
    expect(onPage).toHaveBeenCalledWith(2);
  });

  it('keeps the size selector on a single page but drops the pager', () => {
    render(<Pagination {...props} pageCount={1} totalItems={10} />);
    expect(screen.getByLabelText('disks per page')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Next page' })).not.toBeInTheDocument();
  });

  it('renders nothing with zero rows', () => {
    const { container } = render(<Pagination {...props} totalItems={0} />);
    expect(container).toBeEmptyDOMElement();
  });
});
