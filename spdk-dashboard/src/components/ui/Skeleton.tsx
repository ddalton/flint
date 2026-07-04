// Loading placeholders (design-system kit). Pulse respects
// prefers-reduced-motion; the shape hints at the content that will land.
export function Skeleton({ className = '' }: { className?: string }) {
  return (
    <div
      aria-hidden="true"
      className={`animate-pulse motion-reduce:animate-none rounded bg-gray-200 ${className}`}
    />
  );
}

// Suspense fallback while a lazy tab's chunk loads, and the standard
// full-view loading state.
export function TabSkeleton() {
  return (
    <div className="space-y-4" role="status" aria-label="Loading">
      <Skeleton className="h-8 w-64" />
      <Skeleton className="h-40 w-full" />
      <Skeleton className="h-40 w-full" />
    </div>
  );
}
