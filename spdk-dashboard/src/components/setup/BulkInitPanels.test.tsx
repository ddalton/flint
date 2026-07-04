// Integration of the Phase 4 migration: BulkConfirmModal (2d's safety rail)
// rebased onto the kit ConfirmModal must keep its contract — full disk
// manifest, excluded-with-reason list, and the typed-phrase gate above the
// threshold.
import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { BulkConfirmModal, type ExcludedDisk } from './BulkInitPanels';
import { confirmPhrase, TYPE_TO_CONFIRM_THRESHOLD, type BatchDisk } from './batchSetup';

const disk = (node: string, i: number): BatchDisk => ({
  key: `${node}:0000:00:${i}.0`,
  node,
  pci: `0000:00:${i}.0`,
  device: `nvme${i}n1`,
  model: 'Amazon EC2 NVMe Instance Storage',
  serial: `AWS${node}${i}`,
  sizeBytes: 118111600640,
});

describe('BulkConfirmModal (on the kit ConfirmModal)', () => {
  it('lists exactly what will be wiped and what was excluded, with reasons', () => {
    const excluded: ExcludedDisk[] = [
      { disk: disk('node-b', 9), reason: 'mounted without Force Unmount' },
    ];
    render(
      <BulkConfirmModal
        disks={[disk('node-a', 1), disk('node-a', 2)]}
        excluded={excluded}
        onConfirm={() => {}}
        onCancel={() => {}}
      />
    );

    expect(
      screen.getByRole('alertdialog', { name: 'Initialize 2 disks for SPDK' })
    ).toBeInTheDocument();
    expect(screen.getByText(/All data on the disks below will be destroyed/)).toBeInTheDocument();
    // The manifest: node, device, PCI, serial all visible per disk.
    expect(screen.getByText('0000:00:1.0')).toBeInTheDocument();
    expect(screen.getByText('0000:00:2.0')).toBeInTheDocument();
    expect(screen.getByText('AWSnode-a1')).toBeInTheDocument();
    // Excluded disks are named with the reason they will not ride along.
    expect(screen.getByText(/node-b:nvme9n1/)).toBeInTheDocument();
    expect(screen.getByText(/mounted without Force Unmount/)).toBeInTheDocument();
  });

  it('small batches confirm without a typed phrase', async () => {
    const onConfirm = vi.fn();
    render(
      <BulkConfirmModal
        disks={[disk('node-a', 1)]}
        excluded={[]}
        onConfirm={onConfirm}
        onCancel={() => {}}
      />
    );
    const confirm = screen.getByRole('button', { name: /Initialize 1 disk/ });
    expect(confirm).toBeEnabled();
    await userEvent.setup().click(confirm);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it(`batches above ${TYPE_TO_CONFIRM_THRESHOLD} disks demand the typed phrase`, async () => {
    const user = userEvent.setup();
    const onConfirm = vi.fn();
    const disks = Array.from({ length: TYPE_TO_CONFIRM_THRESHOLD + 2 }, (_, i) =>
      disk(`node-${i % 3}`, i)
    );
    render(
      <BulkConfirmModal disks={disks} excluded={[]} onConfirm={onConfirm} onCancel={() => {}} />
    );

    const confirm = screen.getByRole('button', { name: /Initialize 12 disks/ });
    expect(confirm).toBeDisabled();
    await user.click(confirm);
    expect(onConfirm).not.toHaveBeenCalled();

    await user.type(screen.getByRole('textbox'), confirmPhrase(disks.length));
    expect(confirm).toBeEnabled();
    await user.click(confirm);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });
});
