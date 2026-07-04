// The accessible progress control (design-system kit; the 2a indicator set
// the ARIA contract). Replaces every hand-rolled `width: %` div. Motion with
// meaning: the bar moves only because the value moved; indeterminate work
// pulses instead of pretending to have a number.

export type ProgressTone = 'ok' | 'warn' | 'danger' | 'info';

const TONE_FILL: Record<ProgressTone, string> = {
  ok: 'bg-healthy-500',
  warn: 'bg-stale-500',
  danger: 'bg-failed-500',
  info: 'bg-brand-500',
};

export function ProgressBar({
  value,
  max = 100,
  label,
  valueText,
  tone = 'info',
  indeterminate = false,
  className = 'w-24',
}: {
  value?: number;
  max?: number;
  label: string;
  valueText?: string;
  tone?: ProgressTone;
  indeterminate?: boolean;
  className?: string;
}) {
  const clamped = indeterminate || value === undefined ? undefined : Math.min(Math.max(value, 0), max);
  const pct = clamped === undefined ? 100 : max > 0 ? (clamped / max) * 100 : 0;
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={max}
      {...(clamped !== undefined ? { 'aria-valuenow': clamped } : {})}
      {...(valueText ? { 'aria-valuetext': valueText } : {})}
      className={`bg-gray-200 rounded-full h-2 overflow-hidden ${className}`}
    >
      <div
        className={`h-2 rounded-full ${TONE_FILL[tone]} transition-[width] duration-300 motion-reduce:transition-none ${
          indeterminate ? 'animate-pulse motion-reduce:animate-none' : ''
        }`}
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}
