import type { LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';

// Standard panel (design-system kit): one card shell so every view shares
// the same chrome, header density, and radius.
export function Card({
  icon: Icon,
  title,
  subtitle,
  actions,
  children,
  className = '',
  bodyClassName = '',
}: {
  icon?: LucideIcon;
  title?: string;
  subtitle?: string;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
}) {
  return (
    <div className={`bg-white rounded-lg shadow overflow-hidden ${className}`}>
      {title && (
        <div className="px-4 py-3 border-b bg-gray-50 flex flex-wrap items-center gap-2">
          {Icon && <Icon aria-hidden="true" className="w-5 h-5 text-gray-600" />}
          <h3 className="font-semibold">{title}</h3>
          {subtitle && <span className="text-xs text-gray-500">{subtitle}</span>}
          {actions && <div className="ml-auto flex flex-wrap gap-1">{actions}</div>}
        </div>
      )}
      <div className={bodyClassName}>{children}</div>
    </div>
  );
}
