import type { LucideIcon } from 'lucide-react';
import { memberStateStyle, volumeStateStyle } from './status';

// Status chips (design-system kit): one pill rendering for every status in
// the app, colors and labels from status.ts — components never hand-pick
// status colors.

export function Chip({
  label,
  chip,
  icon: Icon,
  iconClass = '',
  title,
}: {
  label: string;
  chip: string;
  icon?: LucideIcon;
  iconClass?: string;
  title?: string;
}) {
  return (
    <span
      className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium border ${chip}`}
      title={title}
    >
      {Icon && <Icon aria-hidden="true" className={`w-3 h-3 ${iconClass}`} />}
      {label}
    </span>
  );
}

export function VolumeStateChip({ state, title }: { state: string; title?: string }) {
  const style = volumeStateStyle(state);
  return (
    <Chip
      label={state}
      chip={style.chip}
      icon={style.icon}
      title={title ?? style.tooltip}
    />
  );
}

export function MemberStateChip({ state, title }: { state: string; title?: string }) {
  const style = memberStateStyle(state);
  return (
    <Chip label={state} chip={style.chip} icon={style.icon} iconClass={style.iconClass} title={title} />
  );
}
