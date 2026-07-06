// Fleet helpers: problems-first ordering and health/onboarding facets.
import { describe, expect, it } from 'vitest';
import {
  compareFleetNodes,
  sortFleetNodes,
  matchesFacet,
  facetCounts,
  type FleetNode,
} from './useNodesFleet';
import { makeNodeSummary } from '../test/fixtures';

const node = (overrides: Partial<FleetNode>): FleetNode =>
  ({ ...makeNodeSummary(), ...overrides }) as FleetNode;

describe('fleet node helpers', () => {
  const ok = node({ name: 'zz-ok', health: 'ok' });
  const warning = node({ name: 'aa-warn', health: 'warning', replicas_out_of_sync: 1 });
  const critical = node({ name: 'mm-crit', health: 'critical', disks_healthy: 0 });
  const uninit = node({ name: 'bb-uninit', health: 'ok', disks_uninitialized: 2 });

  it('orders problems first, then by name', () => {
    const sorted = [ok, uninit, warning, critical].sort(compareFleetNodes);
    expect(sorted.map(n => n.name)).toEqual(['mm-crit', 'aa-warn', 'bb-uninit', 'zz-ok']);
  });

  it('sorts by the selected key with name tiebreak', () => {
    const big = node({ name: 'big', capacity_gb: 5000 });
    expect(sortFleetNodes([ok, big], 'capacity')[0]?.name).toBe('big');
    expect(sortFleetNodes([warning, critical, ok], 'name').map(n => n.name)).toEqual([
      'aa-warn',
      'mm-crit',
      'zz-ok',
    ]);
  });

  it('facets split health buckets and the onboarding bucket', () => {
    const all = [ok, warning, critical, uninit];
    expect(facetCounts(all)).toEqual({ all: 4, critical: 1, warning: 1, ok: 2, uninitialized: 1 });
    expect(all.filter(n => matchesFacet(n, 'warning'))).toEqual([warning]);
    expect(all.filter(n => matchesFacet(n, 'uninitialized'))).toEqual([uninit]);
    expect(all.filter(n => matchesFacet(n, 'all'))).toHaveLength(4);
  });
});
