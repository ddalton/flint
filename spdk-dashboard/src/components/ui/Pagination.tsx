import { ChevronLeft, ChevronRight } from 'lucide-react';
import { IconButton } from './Button';

// Pagination (design-system kit): one footer contract for every paginated
// table. Before this primitive, DisksTable and VolumesTable had already
// drifted (different size options, Previous/Next text vs chevrons, and
// VolumesTable hid the size selector whenever there was one page).
// Contract: range + size selector render whenever there are rows; the
// pager itself only when there is more than one page.

export function Pagination({
  page,
  pageCount,
  onPage,
  pageSize,
  onPageSize,
  pageSizes = [25, 50, 100],
  totalItems,
  itemNoun = 'rows',
  className = '',
}: {
  /** 1-based current page. */
  page: number;
  pageCount: number;
  onPage: (page: number) => void;
  pageSize: number;
  onPageSize: (size: number) => void;
  pageSizes?: number[];
  totalItems: number;
  /** Plural noun for the range caption ("disks", "volumes"). */
  itemNoun?: string;
  className?: string;
}) {
  if (totalItems <= 0) return null;
  const first = (page - 1) * pageSize + 1;
  const last = Math.min(page * pageSize, totalItems);
  return (
    <div className={`flex flex-wrap items-center justify-between gap-3 ${className}`}>
      <div className="flex items-center gap-2 text-sm text-gray-700">
        <span>
          Showing {first}-{last} of {totalItems} {itemNoun}
        </span>
        <select
          value={pageSize}
          onChange={(e) => onPageSize(Number(e.target.value))}
          aria-label={`${itemNoun} per page`}
          className="border border-gray-300 rounded px-2 py-1 text-sm"
        >
          {pageSizes.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
        <span>per page</span>
      </div>
      {pageCount > 1 && (
        <div className="flex items-center gap-2">
          <IconButton
            icon={ChevronLeft}
            aria-label="Previous page"
            onClick={() => onPage(page - 1)}
            disabled={page === 1}
            className="p-1"
            iconClass="w-4 h-4"
          />
          <span className="px-2 py-1 text-sm">
            {page} / {pageCount}
          </span>
          <IconButton
            icon={ChevronRight}
            aria-label="Next page"
            onClick={() => onPage(page + 1)}
            disabled={page === pageCount}
            className="p-1"
            iconClass="w-4 h-4"
          />
        </div>
      )}
    </div>
  );
}
