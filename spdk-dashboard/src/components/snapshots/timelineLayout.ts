// Pure geometry for the snapshot timeline: time domain, tick generation,
// epoch density bucketing, and user-marker clustering. No React, no DOM —
// everything here is unit-tested directly.

export interface TimeDomain {
  /** ms epoch of the left edge */
  min: number;
  /** ms epoch of the right edge (the "now" anchor) */
  max: number;
}

/** Left-pad the earliest event by 5% of the span so markers never sit on the
 * card edge; right edge is always "now" (live anchor). Minimum span 60s so a
 * single fresh snapshot doesn't produce a degenerate axis. */
export function computeDomain(eventTimesMs: number[], nowMs: number): TimeDomain | null {
  const known = eventTimesMs.filter((t) => Number.isFinite(t));
  if (known.length === 0) return null;
  const earliest = Math.min(...known);
  const span = Math.max(nowMs - earliest, 60_000);
  return { min: nowMs - span * 1.05, max: nowMs };
}

export function xScale(tMs: number, domain: TimeDomain, width: number): number {
  const { min, max } = domain;
  if (max <= min) return width;
  return ((tMs - min) / (max - min)) * width;
}

const TICK_STEPS_MS = [
  1_000, 5_000, 15_000, 30_000,
  60_000, 2 * 60_000, 5 * 60_000, 10 * 60_000, 15 * 60_000, 30 * 60_000,
  3_600_000, 2 * 3_600_000, 6 * 3_600_000, 12 * 3_600_000, 24 * 3_600_000,
];

export interface AxisTick {
  x: number;
  label: string;
}

/** Round ticks at a step chosen so labels sit ~90px apart. Labels are
 * absolute wall-clock (HH:MM or HH:MM:SS under one minute) — absolute ticks
 * don't go stale on a live chart; relative phrasing belongs in tooltips. */
export function timeTicks(domain: TimeDomain, width: number, targetPx = 90): AxisTick[] {
  const span = domain.max - domain.min;
  if (span <= 0 || width <= 0) return [];
  const targetStep = span / Math.max(1, width / targetPx);
  const step = TICK_STEPS_MS.find((s) => s >= targetStep) ?? 24 * 3_600_000;
  const first = Math.ceil(domain.min / step) * step;
  const ticks: AxisTick[] = [];
  for (let t = first; t <= domain.max; t += step) {
    const d = new Date(t);
    const hh = String(d.getHours()).padStart(2, '0');
    const mm = String(d.getMinutes()).padStart(2, '0');
    const ss = String(d.getSeconds()).padStart(2, '0');
    ticks.push({ x: xScale(t, domain, width), label: step < 60_000 ? `${hh}:${mm}:${ss}` : `${hh}:${mm}` });
  }
  return ticks;
}

export interface EpochBucket<T> {
  x: number;
  widthPx: number;
  count: number;
  items: T[];
}

/** Fixed-width density buckets for the epoch ribbon (GitHub-contribution
 * idiom rotated to 1-D): overlap is impossible by construction, and a
 * bucket with one epoch renders as a slim tick. Items with unknown time are
 * the caller's problem (they are counted separately, never plotted). */
export function bucketEpochs<T>(
  items: { timeMs: number; item: T }[],
  domain: TimeDomain,
  width: number,
  bucketPx = 7,
): EpochBucket<T>[] {
  if (width <= 0) return [];
  const nBuckets = Math.max(1, Math.floor(width / bucketPx));
  const buckets = new Map<number, T[]>();
  for (const { timeMs, item } of items) {
    const x = xScale(timeMs, domain, width);
    if (x < 0 || x > width) continue;
    const idx = Math.min(nBuckets - 1, Math.floor((x / width) * nBuckets));
    const list = buckets.get(idx) ?? [];
    list.push(item);
    buckets.set(idx, list);
  }
  return Array.from(buckets.entries())
    .sort((a, b) => a[0] - b[0])
    .map(([idx, list]) => ({
      x: (idx / nBuckets) * width,
      widthPx: Math.max(3, bucketPx - 2),
      count: list.length,
      items: list,
    }));
}

export interface MarkerCluster<T> {
  x: number;
  items: T[];
}

/** Greedy left-to-right clustering of sparse markers: any marker within
 * `minGapPx` of the cluster's running centroid joins it (map cluster-marker
 * pattern). Sparse timelines come back unchanged as 1-item clusters. */
export function clusterMarkers<T>(
  items: { timeMs: number; item: T }[],
  domain: TimeDomain,
  width: number,
  minGapPx = 18,
): MarkerCluster<T>[] {
  const positioned = items
    .map(({ timeMs, item }) => ({ x: xScale(timeMs, domain, width), item }))
    .filter(({ x }) => x >= -minGapPx && x <= width + minGapPx)
    .sort((a, b) => a.x - b.x);
  const clusters: { xs: number[]; lastX: number; items: T[] }[] = [];
  for (const { x, item } of positioned) {
    const last = clusters[clusters.length - 1];
    if (last && x - last.lastX < minGapPx) {
      last.xs.push(x);
      last.lastX = x;
      last.items.push(item);
    } else {
      clusters.push({ xs: [x], lastX: x, items: [item] });
    }
  }
  return clusters.map(({ xs, items: clustered }) => ({
    x: xs.reduce((a, b) => a + b, 0) / xs.length,
    items: clustered,
  }));
}

/** "2m 14s ago" — used in tooltips/popovers only, never on the axis. */
export function relTime(tMs: number, nowMs: number): string {
  const diff = Math.max(0, nowMs - tMs);
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s ago`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m ago`;
  return `${Math.floor(h / 24)}d ago`;
}
