import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  applyBrushDrag, bucketEpochs, hitTestBrush, nudgeWindow, xScale,
  type BrushMode, type TimeDomain,
} from './timelineLayout';

// Focus+context strip (the Grafana/finance-chart brush idiom): a miniature
// render of the FULL history that never zooms, on which the operator drags
// a window; the main lanes above become the focus view of that window.
// - Drag on empty strip → draw a new window. Drag the body → pan. Drag an
//   edge handle → resize. Click outside the window (or Reset) → full view.
// - The window is keyboard-operable (arrows pan, +/- zoom, Escape resets)
//   so zoom isn't mouse-only; all content stays reachable without it.
// Colors mirror the main lanes in SnapshotTimelineView (duplicated: importing
// them back from the component file would defeat fast-refresh).
const USER_COLOR = '#7c3aed';
const EPOCH_COLOR = '#3b82f6';
const NOW_COLOR = '#10b981';
const BRUSH_COLOR = '#2563eb'; // blue-600: selection chrome, not data

const LABEL_Y = 9;
const STRIP_TOP = 14;
const STRIP_H = 26;
const SVG_H = STRIP_TOP + STRIP_H + 2;

const fmtAbs = (t: number) => new Date(t).toLocaleTimeString();

interface Drag {
  mode: BrushMode;
  anchorPx: number;
  window0: TimeDomain | null;
}

export const TimelineBrush: React.FC<{
  /** Full-history domain — the strip always shows all of it. */
  domain: TimeDomain;
  /** Active zoom window; null = full view (no brush drawn). */
  zoom: TimeDomain | null;
  onZoomChange: (win: TimeDomain | null) => void;
  width: number;
  userTimesMs: number[];
  epochTimesMs: number[];
}> = ({ domain, zoom, onZoomChange, width, userTimesMs, epochTimesMs }) => {
  const svgRef = useRef<SVGSVGElement | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [focused, setFocused] = useState(false);

  const pxFromClientX = useCallback((clientX: number) => {
    const left = svgRef.current?.getBoundingClientRect().left ?? 0;
    return clientX - left;
  }, []);

  const onMouseDown = useCallback(
    (ev: React.MouseEvent<SVGSVGElement>) => {
      if (ev.button !== 0) return;
      ev.preventDefault();
      const px = pxFromClientX(ev.clientX);
      setDrag({ mode: hitTestBrush(px, zoom, domain, width), anchorPx: px, window0: zoom });
    },
    [pxFromClientX, zoom, domain, width]
  );

  // Window-level listeners while dragging: the pointer routinely leaves the
  // 26px strip mid-drag, and the brush must keep tracking it.
  useEffect(() => {
    if (!drag) return;
    const apply = (clientX: number) =>
      onZoomChange(
        applyBrushDrag(drag.mode, drag.anchorPx, pxFromClientX(clientX), drag.window0, domain, width)
      );
    const onMove = (ev: MouseEvent) => apply(ev.clientX);
    const onUp = (ev: MouseEvent) => {
      apply(ev.clientX);
      setDrag(null);
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
  }, [drag, domain, width, onZoomChange, pxFromClientX]);

  const onKeyDown = useCallback(
    (ev: React.KeyboardEvent<SVGGElement>) => {
      if (!zoom) return;
      let next: TimeDomain | null;
      switch (ev.key) {
        case 'ArrowLeft': next = nudgeWindow(zoom, domain, 'left'); break;
        case 'ArrowRight': next = nudgeWindow(zoom, domain, 'right'); break;
        case 'ArrowUp': case '+': case '=': next = nudgeWindow(zoom, domain, 'in'); break;
        case 'ArrowDown': case '-': next = nudgeWindow(zoom, domain, 'out'); break;
        case 'Escape': next = null; break;
        default: return;
      }
      ev.preventDefault();
      ev.stopPropagation();
      onZoomChange(next);
    },
    [zoom, domain, onZoomChange]
  );

  const buckets = useMemo(
    () => bucketEpochs(epochTimesMs.map((t) => ({ timeMs: t, item: t })), domain, width, 4),
    [epochTimesMs, domain, width]
  );
  const maxBucket = Math.max(1, ...buckets.map((b) => b.count));

  const x0 = zoom ? xScale(zoom.min, domain, width) : 0;
  const x1 = zoom ? xScale(zoom.max, domain, width) : width;

  return (
    <svg
      ref={svgRef}
      width={width}
      height={SVG_H}
      className="block cursor-crosshair"
      onMouseDown={onMouseDown}
      data-testid="timeline-brush"
      aria-hidden={false}
    >
      <text x={0} y={LABEL_Y} fontSize={10} fill="#6b7280" fontWeight={600}>
        FULL HISTORY
        <tspan fill="#9ca3af" fontWeight={400}>
          {' '}· drag to zoom{zoom ? ' · click outside the window to reset' : ''}
        </tspan>
      </text>

      {/* miniature full-domain render: epoch density + user snapshot ticks */}
      <rect x={0} y={STRIP_TOP} width={width} height={STRIP_H} rx={4} fill="#f3f4f6" />
      {buckets.map((b) => (
        <rect
          key={`bb-${b.x}`}
          x={b.x}
          y={STRIP_TOP + 2}
          width={b.widthPx}
          height={STRIP_H - 4}
          rx={1.5}
          fill={EPOCH_COLOR}
          fillOpacity={0.25 + 0.55 * (b.count / maxBucket)}
        />
      ))}
      {userTimesMs.filter(Number.isFinite).map((t, i) => {
        const x = xScale(t, domain, width);
        if (x < 0 || x > width) return null;
        return (
          <line
            key={`bu-${i}-${x}`}
            x1={x}
            y1={STRIP_TOP + 1}
            x2={x}
            y2={STRIP_TOP + STRIP_H - 1}
            stroke={USER_COLOR}
            strokeWidth={1.5}
          />
        );
      })}
      {/* now tick: the strip's right edge is always live */}
      <line
        x1={width - 1}
        y1={STRIP_TOP}
        x2={width - 1}
        y2={STRIP_TOP + STRIP_H}
        stroke={NOW_COLOR}
        strokeWidth={2}
      />

      {zoom && (
        <>
          {/* dim the out-of-window context; the window stays clear */}
          <rect x={0} y={STRIP_TOP} width={Math.max(0, x0)} height={STRIP_H} fill="#4b5563" fillOpacity={0.15} />
          <rect
            x={x1}
            y={STRIP_TOP}
            width={Math.max(0, width - x1)}
            height={STRIP_H}
            fill="#4b5563"
            fillOpacity={0.15}
          />
          <g
            role="slider"
            tabIndex={0}
            aria-label="Timeline zoom window"
            aria-valuemin={domain.min}
            aria-valuemax={domain.max}
            aria-valuenow={zoom.min}
            aria-valuetext={`${fmtAbs(zoom.min)} to ${fmtAbs(zoom.max)}`}
            className="focus:outline-none"
            onKeyDown={onKeyDown}
            onFocus={() => setFocused(true)}
            onBlur={() => setFocused(false)}
          >
            <rect
              x={x0}
              y={STRIP_TOP}
              width={Math.max(1, x1 - x0)}
              height={STRIP_H}
              rx={2}
              fill={BRUSH_COLOR}
              fillOpacity={0.08}
              stroke={BRUSH_COLOR}
              strokeWidth={focused ? 2.5 : 1.5}
              className="cursor-grab"
            />
            {[x0, x1].map((hx, i) => (
              <rect
                key={`bh-${i}`}
                x={hx - 2.5}
                y={STRIP_TOP + STRIP_H / 2 - 7}
                width={5}
                height={14}
                rx={2}
                fill="#ffffff"
                stroke={BRUSH_COLOR}
                strokeWidth={1.5}
                className="cursor-col-resize"
              />
            ))}
          </g>
        </>
      )}
    </svg>
  );
};
