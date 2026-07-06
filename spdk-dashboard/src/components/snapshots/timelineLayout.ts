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

/** Inverse of xScale, clamped to the domain — pointer px never maps outside
 * the strip's time range. */
export function pxToTime(px: number, domain: TimeDomain, width: number): number {
  if (width <= 0) return domain.max;
  const clamped = Math.min(Math.max(px, 0), width);
  return domain.min + ((domain.max - domain.min) * clamped) / width;
}

/** Keep a window inside the domain without changing its span (unless the
 * span itself exceeds the domain). */
export function clampWindow(win: TimeDomain, domain: TimeDomain): TimeDomain {
  const span = Math.min(win.max - win.min, domain.max - domain.min);
  const min = Math.max(domain.min, Math.min(win.min, domain.max - span));
  return { min, max: min + span };
}

export type BrushMode = 'create' | 'move' | 'resize-start' | 'resize-end';

/** Which drag a pointer-down starts: near a window edge resizes it, inside
 * the window pans it, anywhere else draws a new one. */
export function hitTestBrush(
  px: number,
  win: TimeDomain | null,
  domain: TimeDomain,
  width: number,
  handlePx = 8,
): BrushMode {
  if (!win) return 'create';
  const x0 = xScale(win.min, domain, width);
  const x1 = xScale(win.max, domain, width);
  if (Math.abs(px - x0) <= handlePx) return 'resize-start';
  if (Math.abs(px - x1) <= handlePx) return 'resize-end';
  if (px > x0 && px < x1) return 'move';
  return 'create';
}

/** A create-drag under this many px is a click, and a click clears the
 * zoom (the map/Grafana brush idiom). */
const BRUSH_CLICK_PX = 3;

/** One brush drag, as pure geometry: (mode, anchor px, current px, the
 * window at drag start) → the new window, or null for "full view". Spans
 * never collapse below minSpanMs and never leave the domain. */
export function applyBrushDrag(
  mode: BrushMode,
  anchorPx: number,
  currentPx: number,
  window0: TimeDomain | null,
  domain: TimeDomain,
  width: number,
  minSpanMs = 1_000,
): TimeDomain | null {
  const span = domain.max - domain.min;
  if (span <= 0 || width <= 0) return window0;
  switch (mode) {
    case 'create': {
      if (Math.abs(currentPx - anchorPx) <= BRUSH_CLICK_PX) return null;
      const a = pxToTime(anchorPx, domain, width);
      const b = pxToTime(currentPx, domain, width);
      let min = Math.min(a, b);
      let max = Math.max(a, b);
      if (max - min < minSpanMs) {
        const mid = (min + max) / 2;
        min = mid - minSpanMs / 2;
        max = mid + minSpanMs / 2;
      }
      return clampWindow({ min, max }, domain);
    }
    case 'move': {
      if (!window0) return null;
      const w = window0.max - window0.min;
      const dt = ((currentPx - anchorPx) / width) * span;
      const min = Math.max(domain.min, Math.min(window0.min + dt, domain.max - w));
      return { min, max: min + w };
    }
    case 'resize-start': {
      if (!window0) return null;
      const t = pxToTime(currentPx, domain, width);
      return {
        min: Math.min(Math.max(t, domain.min), window0.max - minSpanMs),
        max: window0.max,
      };
    }
    case 'resize-end': {
      if (!window0) return null;
      const t = pxToTime(currentPx, domain, width);
      return {
        min: window0.min,
        max: Math.max(Math.min(t, domain.max), window0.min + minSpanMs),
      };
    }
  }
}

/** Keyboard nudges for the brush window: pan by 10% of the window span,
 * zoom in/out by 25% around its centre. Zooming out past the full domain
 * dissolves the window — null means "back to full view". */
export function nudgeWindow(
  win: TimeDomain,
  domain: TimeDomain,
  op: 'left' | 'right' | 'in' | 'out',
  minSpanMs = 1_000,
): TimeDomain | null {
  const span = win.max - win.min;
  const mid = (win.min + win.max) / 2;
  switch (op) {
    case 'left':
    case 'right': {
      const dt = span * 0.1 * (op === 'left' ? -1 : 1);
      return clampWindow({ min: win.min + dt, max: win.max + dt }, domain);
    }
    case 'in': {
      const newSpan = Math.max(minSpanMs, span * 0.75);
      return clampWindow({ min: mid - newSpan / 2, max: mid + newSpan / 2 }, domain);
    }
    case 'out': {
      const newSpan = span / 0.75;
      if (newSpan >= domain.max - domain.min) return null;
      return clampWindow({ min: mid - newSpan / 2, max: mid + newSpan / 2 }, domain);
    }
  }
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
