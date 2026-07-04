import { useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { AlertTriangle } from 'lucide-react';

// Destructive-action confirmation (design-system kit): one modal shell for
// every wipe/delete flow — same warning chrome, same typed-phrase gate,
// same keyboard behavior (Escape cancels, initial focus on Cancel so Enter
// can't blind-confirm).
export function ConfirmModal({
  title,
  subtitle,
  danger,
  confirmLabel,
  confirmPhrase,
  phraseHelp,
  busy = false,
  onConfirm,
  onCancel,
  children,
}: {
  title: string;
  subtitle?: string;
  // The red box: what exactly gets destroyed.
  danger: ReactNode;
  confirmLabel: string;
  // When set, the confirm button stays disabled until this is typed back.
  confirmPhrase?: string;
  phraseHelp?: ReactNode;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  children?: ReactNode;
}) {
  const [typed, setTyped] = useState('');
  const cancelRef = useRef<HTMLButtonElement>(null);
  const enabled = !busy && (!confirmPhrase || typed.trim() === confirmPhrase);

  useEffect(() => {
    cancelRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onCancel]);

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div
        role="alertdialog"
        aria-modal="true"
        aria-label={title}
        className="bg-white rounded-lg p-6 max-w-3xl w-full mx-4 max-h-[85vh] flex flex-col"
      >
        <div className="flex items-center gap-3 mb-4">
          <AlertTriangle aria-hidden="true" className="w-8 h-8 text-failed-600 flex-shrink-0" />
          <div>
            <h3 className="text-lg font-bold text-gray-900">{title}</h3>
            {subtitle && <p className="text-sm text-gray-600">{subtitle}</p>}
          </div>
        </div>

        <div className="p-3 bg-failed-50 border border-failed-200 rounded-lg mb-4">
          <div className="text-sm text-failed-800">{danger}</div>
        </div>

        {children}

        {confirmPhrase && (
          <div className="mb-4">
            <label className="block text-sm text-gray-700 mb-1">
              {phraseHelp ?? (
                <>Type <span className="font-mono font-semibold">{confirmPhrase}</span> to continue</>
              )}
            </label>
            <input
              type="text"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              className="w-full px-3 py-2 border border-gray-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-failed-500"
              autoComplete="off"
            />
          </div>
        )}

        <div className="flex justify-end gap-3">
          <button
            ref={cancelRef}
            onClick={onCancel}
            disabled={busy}
            className="px-4 py-2 text-sm font-medium rounded-md border border-gray-300 text-gray-700 hover:bg-gray-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            disabled={!enabled}
            className="px-4 py-2 text-sm font-medium rounded-md bg-failed-600 text-white hover:bg-failed-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-failed-600 disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
