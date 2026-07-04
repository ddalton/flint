// Token invariants: the one-source-of-truth guarantees the consolidation
// exists to provide.
import { describe, expect, it } from 'vitest';
import {
  memberStateStyle,
  SYNC_STATE_STYLES,
  UNKNOWN_MEMBER_STATE,
  UNKNOWN_VOLUME_STATE,
  VOLUME_FILTER_DISPLAY,
  VOLUME_STATE_STYLES,
  volumeFilterDisplay,
  volumeStateStyle,
} from './status';

describe('volumeStateStyle', () => {
  it('ranks broken states above healthy for sorting', () => {
    expect(VOLUME_STATE_STYLES.Failed.priority).toBeGreaterThan(
      VOLUME_STATE_STYLES.Degraded.priority
    );
    expect(VOLUME_STATE_STYLES.Degraded.priority).toBeGreaterThan(
      VOLUME_STATE_STYLES.Healthy.priority
    );
  });

  it('degrades unknown states (e.g. the table-synthesized "Raw") to the gray token', () => {
    expect(volumeStateStyle('Raw')).toBe(UNKNOWN_VOLUME_STATE);
    expect(volumeStateStyle('')).toBe(UNKNOWN_VOLUME_STATE);
  });
});

describe('memberStateStyle', () => {
  it('is case-insensitive (SPDK reports member states in varying case)', () => {
    expect(memberStateStyle('ONLINE')).toBe(memberStateStyle('online'));
  });

  it('reuses the sync-state chips so the two renderings cannot drift', () => {
    expect(memberStateStyle('stale').chip).toBe(SYNC_STATE_STYLES.stale.chip);
    expect(memberStateStyle('standby').chip).toBe(SYNC_STATE_STYLES.standby.chip);
  });

  it('falls back to the unknown token for unmapped states', () => {
    expect(memberStateStyle('exploded')).toBe(UNKNOWN_MEMBER_STATE);
  });
});

describe('volumeFilterDisplay', () => {
  it('covers every filter value the URL accepts', () => {
    for (const filter of [
      'all',
      'orphaned',
      'healthy',
      'degraded',
      'failed',
      'faulted',
      'rebuilding',
      'local-nvme',
    ]) {
      expect(VOLUME_FILTER_DISPLAY[filter], `missing display for '${filter}'`).toBeDefined();
    }
  });

  it('defaults to All Volumes for absent or unknown filters', () => {
    expect(volumeFilterDisplay(undefined).name).toBe('All Volumes');
    expect(volumeFilterDisplay('junk').name).toBe('All Volumes');
    // 'all' reads naturally inline: "3 volumes", not "3 all volumes".
    expect(volumeFilterDisplay('all').short).toBe('');
  });
});
