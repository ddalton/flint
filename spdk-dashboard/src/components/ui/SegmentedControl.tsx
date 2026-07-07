import type { LucideIcon } from 'lucide-react';

// Segmented control (design-system kit): one exclusive-choice toggle for
// view switchers — the joined-button groups the Button sweep deliberately
// carved out. Exactly one segment is active; this is state, not an action,
// so segments carry aria-pressed and the group is labelled.

const FOCUS_RING =
  'focus-visible:outline focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-brand-600';

export interface SegmentedOption<T extends string> {
  value: T;
  label: string;
  icon?: LucideIcon;
}

export function SegmentedControl<T extends string>({
  options,
  value,
  onChange,
  'aria-label': ariaLabel,
  size = 'md',
  iconOnly = false,
}: {
  options: SegmentedOption<T>[];
  value: T;
  onChange: (value: T) => void;
  // Exclusive-choice groups always carry their accessible name.
  'aria-label': string;
  size?: 'sm' | 'md';
  // Compact icon segments: the label still names the segment (sr-only +
  // title), it just doesn't render visually.
  iconOnly?: boolean;
}) {
  const sizing = size === 'sm' ? 'px-3 py-1 text-sm' : 'px-4 py-2 text-sm font-medium';
  return (
    <div role="group" aria-label={ariaLabel} className="flex border border-gray-300 rounded-md overflow-hidden">
      {options.map(({ value: v, label, icon: Icon }, i) => (
        <button
          key={v}
          type="button"
          aria-pressed={v === value}
          onClick={() => onChange(v)}
          title={iconOnly ? label : undefined}
          className={`inline-flex items-center gap-2 ${sizing} ${FOCUS_RING} ${
            i > 0 ? 'border-l border-gray-300' : ''
          } ${v === value ? 'bg-brand-600 text-white' : 'bg-white text-gray-700 hover:bg-gray-50'}`}
        >
          {Icon && <Icon aria-hidden="true" className="w-4 h-4" />}
          {iconOnly ? <span className="sr-only">{label}</span> : label}
        </button>
      ))}
    </div>
  );
}
