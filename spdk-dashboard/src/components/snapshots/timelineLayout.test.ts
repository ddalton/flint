// Geometry contracts for the snapshot timeline: honest domains, collision-
// free epoch bucketing, and cluster merges that never drop a marker.
import { describe, expect, it } from 'vitest';
import {
  computeDomain,
  xScale,
  pxToTime,
  timeTicks,
  bucketEpochs,
  clusterMarkers,
  relTime,
  hitTestBrush,
  applyBrushDrag,
  nudgeWindow,
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

// The brush is pure geometry: pointer px in, zoom window (or null = full
// view) out. The component only wires DOM events onto these.
describe('brush geometry', () => {
  const domain = { min: NOW - 300_000, max: NOW };
  const win = { min: NOW - 200_000, max: NOW - 100_000 }; // px 300..600 at width 900

  it('pxToTime inverts xScale and clamps to the strip', () => {
    expect(pxToTime(450, domain, 900)).toBe(NOW - 150_000);
    expect(pxToTime(-50, domain, 900)).toBe(domain.min);
    expect(pxToTime(2000, domain, 900)).toBe(domain.max);
  });

  it('hit-tests edge handles, the window body, and empty strip', () => {
    expect(hitTestBrush(300, win, domain, 900)).toBe('resize-start');
    expect(hitTestBrush(606, win, domain, 900)).toBe('resize-end');
    expect(hitTestBrush(450, win, domain, 900)).toBe('move');
    expect(hitTestBrush(100, win, domain, 900)).toBe('create');
    expect(hitTestBrush(450, null, domain, 900)).toBe('create');
  });

  it('creates a window from a drag in either direction', () => {
    const w = applyBrushDrag('create', 300, 600, null, domain, 900)!;
    expect(w.min).toBeCloseTo(NOW - 200_000);
    expect(w.max).toBeCloseTo(NOW - 100_000);
    const rev = applyBrushDrag('create', 600, 300, null, domain, 900)!;
    expect(rev.min).toBeCloseTo(w.min);
    expect(rev.max).toBeCloseTo(w.max);
  });

  it('treats a tiny create-drag as a click, which clears the zoom', () => {
    expect(applyBrushDrag('create', 300, 302, null, domain, 900)).toBeNull();
  });

  it('never creates a sub-minimum span — a fast flick still yields a usable window', () => {
    const w = applyBrushDrag('create', 300, 304, null, domain, 900)!;
    expect(w.max - w.min).toBeGreaterThanOrEqual(1_000);
  });

  it('move preserves the span and clamps at the domain edges', () => {
    const moved = applyBrushDrag('move', 400, 490, win, domain, 900)!;
    expect(moved.max - moved.min).toBe(100_000);
    expect(moved.min).toBeCloseTo(win.min + 30_000); // 90px * 333.3ms/px
    const hard = applyBrushDrag('move', 400, 2_000, win, domain, 900)!;
    expect(hard.max).toBe(domain.max);
    expect(hard.max - hard.min).toBe(100_000);
  });

  it('resizes clamp to the domain and respect the minimum span', () => {
    const wider = applyBrushDrag('resize-end', 600, 750, win, domain, 900)!;
    expect(wider.min).toBe(win.min);
    expect(wider.max).toBeCloseTo(NOW - 50_000);
    // Dragging the end handle left past the start collapses to min span,
    // never inverts.
    const collapsed = applyBrushDrag('resize-end', 600, 100, win, domain, 900)!;
    expect(collapsed.max - collapsed.min).toBe(1_000);
    const past = applyBrushDrag('resize-start', 300, -500, win, domain, 900)!;
    expect(past.min).toBe(domain.min);
  });

  it('keyboard nudges pan and zoom; zooming out past the domain dissolves to full view', () => {
    const left = nudgeWindow(win, domain, 'left')!;
    expect(left.min).toBeCloseTo(win.min - 10_000);
    expect(left.max - left.min).toBe(100_000);

    const pinned = nudgeWindow({ min: NOW - 100_000, max: NOW }, domain, 'right')!;
    expect(pinned.max).toBe(domain.max); // already at the live edge

    const zoomedIn = nudgeWindow(win, domain, 'in')!;
    expect(zoomedIn.max - zoomedIn.min).toBeCloseTo(75_000);

    expect(nudgeWindow({ min: NOW - 280_000, max: NOW - 10_000 }, domain, 'out')).toBeNull();
  });
});

describe('relTime', () => {
  it('renders humane relative phrasing for tooltips', () => {
    expect(relTime(NOW - 14_000, NOW)).toBe('14s ago');
    expect(relTime(NOW - 134_000, NOW)).toBe('2m 14s ago');
    expect(relTime(NOW - 3 * 3_600_000 - 120_000, NOW)).toBe('3h 2m ago');
  });
});
