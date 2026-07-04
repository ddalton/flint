// Geometry contracts for the snapshot timeline: honest domains, collision-
// free epoch bucketing, and cluster merges that never drop a marker.
import { describe, expect, it } from 'vitest';
import {
  computeDomain,
  xScale,
  timeTicks,
  bucketEpochs,
  clusterMarkers,
  relTime,
} from './timelineLayout';

const NOW = Date.UTC(2026, 6, 4, 21, 25, 0); // 2026-07-04T21:25:00Z

describe('computeDomain', () => {
  it('anchors the right edge at now and pads the left by 5%', () => {
    const earliest = NOW - 300_000;
    const domain = computeDomain([earliest, NOW - 100_000], NOW)!;
    expect(domain.max).toBe(NOW);
    expect(domain.min).toBe(NOW - 315_000); // 300s span * 1.05
  });

  it('returns null with no timed events (orphans only)', () => {
    expect(computeDomain([NaN], NOW)).toBeNull();
    expect(computeDomain([], NOW)).toBeNull();
  });

  it('enforces a 60s minimum span so one fresh snapshot gets a real axis', () => {
    const domain = computeDomain([NOW - 2_000], NOW)!;
    expect(domain.max - domain.min).toBeGreaterThanOrEqual(60_000);
  });
});

describe('xScale + timeTicks', () => {
  const domain = { min: NOW - 300_000, max: NOW };

  it('maps the domain edges onto [0, width]', () => {
    expect(xScale(domain.min, domain, 900)).toBe(0);
    expect(xScale(domain.max, domain, 900)).toBe(900);
    expect(xScale(NOW - 150_000, domain, 900)).toBeCloseTo(450);
  });

  it('emits round absolute ticks that fit the width', () => {
    const ticks = timeTicks(domain, 900);
    expect(ticks.length).toBeGreaterThanOrEqual(4);
    expect(ticks.length).toBeLessThanOrEqual(12);
    for (const t of ticks) {
      expect(t.x).toBeGreaterThanOrEqual(0);
      expect(t.x).toBeLessThanOrEqual(900);
      expect(t.label).toMatch(/^\d{2}:\d{2}(:\d{2})?$/);
    }
  });
});

describe('bucketEpochs', () => {
  const domain = { min: NOW - 300_000, max: NOW };

  it('keeps sparse epochs as distinct slim cells', () => {
    const items = [NOW - 250_000, NOW - 150_000, NOW - 50_000].map((t, i) => ({
      timeMs: t,
      item: i,
    }));
    const buckets = bucketEpochs(items, domain, 900);
    expect(buckets).toHaveLength(3);
    expect(buckets.every((b) => b.count === 1)).toBe(true);
  });

  it('merges dense periodic epochs — total is preserved, cells never overlap', () => {
    // 300 epochs over 5 minutes on a 300px-wide card: individual markers
    // would be sub-pixel soup; buckets must absorb them all.
    const items = Array.from({ length: 300 }, (_, i) => ({
      timeMs: NOW - i * 1_000,
      item: i,
    }));
    const buckets = bucketEpochs(items, domain, 300);
    const total = buckets.reduce((s, b) => s + b.count, 0);
    expect(total).toBe(300);
    expect(Math.max(...buckets.map((b) => b.count))).toBeGreaterThan(1);
    for (let i = 1; i < buckets.length; i++) {
      const prev = buckets[i - 1]!;
      expect(buckets[i]!.x).toBeGreaterThanOrEqual(prev.x + prev.widthPx - 0.001);
    }
  });

  it('drops out-of-domain items instead of plotting them at fake positions', () => {
    const buckets = bucketEpochs([{ timeMs: NOW - 900_000, item: 0 }], domain, 900);
    expect(buckets).toHaveLength(0);
  });
});

describe('clusterMarkers', () => {
  const domain = { min: NOW - 300_000, max: NOW };

  it('leaves well-spaced markers unclustered', () => {
    const items = [NOW - 250_000, NOW - 150_000, NOW - 50_000].map((t, i) => ({
      timeMs: t,
      item: `s${i}`,
    }));
    const clusters = clusterMarkers(items, domain, 900);
    expect(clusters).toHaveLength(3);
    expect(clusters.every((c) => c.items.length === 1)).toBe(true);
  });

  it('merges colliding markers into one +N cluster, preserving membership', () => {
    const items = [NOW - 150_000, NOW - 149_000, NOW - 148_000, NOW - 20_000].map(
      (t, i) => ({ timeMs: t, item: `s${i}` })
    );
    const clusters = clusterMarkers(items, domain, 900);
    expect(clusters).toHaveLength(2);
    expect(clusters[0]!.items).toEqual(['s0', 's1', 's2']);
    expect(clusters[1]!.items).toEqual(['s3']);
  });
});

describe('relTime', () => {
  it('renders humane relative phrasing for tooltips', () => {
    expect(relTime(NOW - 14_000, NOW)).toBe('14s ago');
    expect(relTime(NOW - 134_000, NOW)).toBe('2m 14s ago');
    expect(relTime(NOW - 3 * 3_600_000 - 120_000, NOW)).toBe('3h 2m ago');
  });
});
