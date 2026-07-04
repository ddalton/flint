import type { ReactNode } from 'react';
import { AlertTriangle, Inbox } from 'lucide-react';
import { TabSkeleton } from './Skeleton';

// The state contract (design-system kit): every data view renders exactly
// one of loading / error / empty / data — no view invents its own. Two
// honesty rules from the plan: an error NEVER blanks out data we already
// have (stale truth clearly labeled beats fresh fiction), and an empty
// state says what would populate it.
export function AsyncView<T>({
  loading,
  error,
  data,
  hasData,
  emptyTitle = 'Nothing here yet',
  emptyHint,
  onRetry,
  skeleton,
  children,
}: {
  loading: boolean;
  error?: string | null;
  data: T | undefined;
  // When data is present but semantically empty (e.g. zero rows).
  hasData?: (data: T) => boolean;
  emptyTitle?: string;
  emptyHint?: string;
  onRetry?: () => void;
  skeleton?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  const present = data !== undefined && (hasData ? hasData(data) : true);

  if (loading && !present) {
    return <>{skeleton ?? <TabSkeleton />}</>;
  }

  if (error && !present) {
    return (
      <div className="px-4 py-8 text-center" role="alert">
        <AlertTriangle aria-hidden="true" className="w-8 h-8 text-failed-500 mx-auto mb-3" />
        <p className="text-sm font-medium text-gray-900 mb-1">Could not load this view</p>
        <p className="text-sm text-gray-600 mb-4">{error}</p>
        {onRetry && (
          <button
            onClick={onRetry}
            className="px-3 py-1.5 text-sm font-medium rounded-md bg-brand-600 text-white hover:bg-brand-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600"
          >
            Retry
          </button>
        )}
      </div>
    );
  }

  if (!present) {
    return (
      <div className="px-4 py-8 text-center text-sm text-gray-500">
        <Inbox aria-hidden="true" className="w-8 h-8 text-gray-300 mx-auto mb-3" />
        <p className="font-medium text-gray-700 mb-1">{emptyTitle}</p>
        {emptyHint && <p>{emptyHint}</p>}
      </div>
    );
  }

  return (
    <>
      {error && (
        <div
          className="mb-3 px-3 py-2 rounded-md bg-stale-50 border border-stale-200 text-sm text-stale-800"
          role="status"
        >
          Refresh failed — showing the last data received. ({error})
        </div>
      )}
      {children(data as T)}
    </>
  );
}
