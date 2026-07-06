import { describe, expect, it } from 'vitest';
import { parseTab, parseVolumeFilter, searchForTab } from './routes';

describe('parseTab', () => {
  it('maps the bare path to Overview and rejects unknown segments', () => {
    expect(parseTab(undefined)).toBe('overview');
    expect(parseTab('')).toBe('overview');
    expect(parseTab('volumes')).toBe('volumes');
    expect(parseTab('disk-setup')).toBe('disk-setup');
    expect(parseTab('bogus')).toBeNull();
  });
});

describe('searchForTab', () => {
  it('keeps filter context across tabs but drops foreign detail params', () => {
    const current = new URLSearchParams(
      'filter=degraded&disk=d1&replicas=v1&volume=pvc-1&snapshot=snap-1'
    );

    const toDisks = new URLSearchParams(searchForTab(current, 'disks'));
    expect(toDisks.get('filter')).toBe('degraded');
    expect(toDisks.get('disk')).toBe('d1');
    expect(toDisks.get('replicas')).toBe('v1');
    expect(toDisks.get('volume')).toBeNull();
    expect(toDisks.get('snapshot')).toBeNull();
  });

  it('keeps a detail param when navigating to its home tab', () => {
    const current = new URLSearchParams('volume=pvc-1&snapshot=snap-1');
    const toVolumes = new URLSearchParams(searchForTab(current, 'volumes'));
    expect(toVolumes.get('volume')).toBe('pvc-1');
    expect(toVolumes.get('snapshot')).toBeNull();
  });

  it('scopes the node drill-in param to the nodes tab', () => {
    const current = new URLSearchParams('filter=degraded&node=runk-aws-1');
    const toNodes = new URLSearchParams(searchForTab(current, 'nodes'));
    expect(toNodes.get('node')).toBe('runk-aws-1');
    expect(toNodes.get('filter')).toBe('degraded');
    const toVolumes = new URLSearchParams(searchForTab(current, 'volumes'));
    expect(toVolumes.get('node')).toBeNull();
  });

  it('returns an empty string when nothing survives', () => {
    expect(searchForTab(new URLSearchParams('volume=pvc-1'), 'events')).toBe('');
  });
});

describe('parseVolumeFilter', () => {
  it('accepts known filters and degrades unknown values to all', () => {
    expect(parseVolumeFilter('degraded')).toBe('degraded');
    expect(parseVolumeFilter('local-nvme')).toBe('local-nvme');
    expect(parseVolumeFilter(null)).toBe('all');
    expect(parseVolumeFilter('junk')).toBe('all');
  });
});
