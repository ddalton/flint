import React from 'react';
import { Database, CheckCircle, AlertTriangle, XCircle, RefreshCw, Shield } from 'lucide-react';
import type { DashboardStats, VolumeFilter } from '../../hooks/useDashboardData';

interface StatCardsProps {
  stats: Pick<
    DashboardStats,
    | 'totalVolumes'
    | 'healthyVolumes'
    | 'degradedVolumes'
    | 'failedVolumes'
    | 'recoveringVolumes'
    | 'orphanedLvols'
  >;
  activeFilter?: VolumeFilter;
  onFilterClick: (filter: VolumeFilter) => void;
}

// The quick-filter cards. Every number is derived from what the backend
// actually emits: volume state, the per-replica sync record ("Recovering" =
// any replica not in_sync), and the per-disk orphan lists ("Orphaned" =
// SPDK lvols with no owning PV; the Disks tab lists and deletes them).
export const StatCards: React.FC<StatCardsProps> = ({ stats, activeFilter, onFilterClick }) => {
  const cards = [
    {
      id: 'all' as VolumeFilter,
      title: 'Total Volumes',
      value: stats.totalVolumes,
      icon: Database,
      color: 'text-brand-600',
      bgColor: 'bg-brand-50',
      borderColor: 'border-brand-200'
    },
    {
      id: 'healthy' as VolumeFilter,
      title: 'Healthy',
      value: stats.healthyVolumes,
      icon: CheckCircle,
      color: 'text-healthy-600',
      bgColor: 'bg-healthy-50',
      borderColor: 'border-healthy-200',
      subtitle: 'All replicas in sync'
    },
    {
      id: 'degraded' as VolumeFilter,
      title: 'Degraded',
      value: stats.degradedVolumes,
      icon: AlertTriangle,
      color: 'text-degraded-600',
      bgColor: 'bg-degraded-50',
      borderColor: 'border-degraded-200',
      subtitle: 'Reduced redundancy'
    },
    {
      id: 'failed' as VolumeFilter,
      title: 'Failed',
      value: stats.failedVolumes,
      icon: XCircle,
      color: 'text-failed-600',
      bgColor: 'bg-failed-50',
      borderColor: 'border-failed-200',
      subtitle: 'No replica in sync'
    },
    {
      id: 'rebuilding' as VolumeFilter,
      title: 'Recovering',
      value: stats.recoveringVolumes,
      icon: RefreshCw,
      color: 'text-rebuilding-600',
      bgColor: 'bg-rebuilding-50',
      borderColor: 'border-rebuilding-200',
      subtitle: 'Replicas catching up'
    },
    {
      id: 'orphaned' as VolumeFilter,
      title: 'Orphaned',
      value: stats.orphanedLvols,
      icon: Shield,
      color: 'text-warning-600',
      bgColor: 'bg-warning-50',
      borderColor: 'border-warning-200',
      subtitle: 'SPDK lvols with no owning PV'
    }
  ];

  return (
    <div className="grid grid-cols-2 md:grid-cols-3 lg:grid-cols-6 gap-4">
      {cards.map((card) => {
        const isActive = activeFilter === card.id;
        const Icon = card.icon;

        return (
          <button
            key={card.id}
            onClick={() => onFilterClick(card.id)}
            className={`bg-white rounded-lg shadow p-4 text-left transition-all duration-200 hover:shadow-lg hover:scale-105 ${
              isActive
                ? `ring-2 ring-brand-500 ${card.bgColor} border-2 ${card.borderColor}`
                : 'hover:bg-gray-50'
            }`}
          >
            <div className="flex items-center mb-2">
              <Icon className={`w-8 h-8 ${card.color} mr-3`} />
              <div className="flex-1">
                <p className="text-stat text-gray-900">{card.value}</p>
                <p className="text-sm font-medium text-gray-700">{card.title}</p>
                {card.subtitle && (
                  <p className="text-xs text-gray-500 mt-1">{card.subtitle}</p>
                )}
              </div>
            </div>
            {isActive && card.id !== 'all' && (
              <p className="text-xs text-brand-600 font-medium">
                Click to clear filter
              </p>
            )}
          </button>
        );
      })}
    </div>
  );
};
