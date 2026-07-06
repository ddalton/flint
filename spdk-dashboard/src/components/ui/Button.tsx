import type { LucideIcon } from 'lucide-react';
import type { ButtonHTMLAttributes, ReactNode } from 'react';

// Buttons (design-system kit): one action rendering for the whole app —
// variant says what the action MEANS (primary/danger/...), size and focus
// ring come with it. Raw <button> styling in views is legacy to migrate.

export type ButtonVariant = 'primary' | 'secondary' | 'danger' | 'ghost' | 'link';
export type ButtonSize = 'sm' | 'md';

const VARIANT_CLASSES: Record<ButtonVariant, string> = {
  primary: 'bg-brand-600 text-white hover:bg-brand-700 rounded',
  secondary: 'bg-white text-gray-700 border border-gray-300 hover:bg-gray-50 rounded',
  danger: 'bg-failed-600 text-white hover:bg-failed-700 rounded',
  // Quiet chrome actions (toolbars, dismissals).
  ghost: 'text-gray-600 hover:text-gray-800 hover:bg-gray-100 rounded-md',
  // Inline text actions ("Select all…", "Clear"); no padding of its own.
  link: 'text-brand-600 hover:text-brand-800',
};

const SIZE_CLASSES: Record<ButtonSize, string> = {
  sm: 'px-3 py-1 text-sm',
  md: 'px-4 py-2 text-sm font-medium',
};

const FOCUS_RING =
  'focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600';

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  icon?: LucideIcon;
  iconClass?: string;
  children?: ReactNode;
}

export function Button({
  variant = 'secondary',
  size = 'md',
  icon: Icon,
  iconClass = '',
  className = '',
  type = 'button',
  children,
  ...rest
}: ButtonProps) {
  const sizing = variant === 'link' ? 'text-sm font-medium' : SIZE_CLASSES[size];
  return (
    <button
      type={type}
      className={`inline-flex items-center justify-center gap-2 ${VARIANT_CLASSES[variant]} ${sizing} ${FOCUS_RING} disabled:opacity-50 disabled:cursor-not-allowed transition-colors ${className}`}
      {...rest}
    >
      {Icon && <Icon aria-hidden="true" className={`w-4 h-4 ${iconClass}`} />}
      {children}
    </button>
  );
}

export interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  icon: LucideIcon;
  // Icon-only controls always carry their accessible name.
  'aria-label': string;
  iconClass?: string;
}

export function IconButton({
  icon: Icon,
  iconClass = '',
  className = '',
  type = 'button',
  ...rest
}: IconButtonProps) {
  return (
    <button
      type={type}
      className={`p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md ${FOCUS_RING} disabled:opacity-50 disabled:cursor-not-allowed transition-colors ${className}`}
      {...rest}
    >
      <Icon aria-hidden="true" className={`w-5 h-5 ${iconClass}`} />
    </button>
  );
}
