import React from 'react';
import { Database, CheckCircle, AlertTriangle, XCircle, Settings, Zap, Shield } from 'lucide-react';
import type { VolumeFilter } from '../../hooks/useDashboardData';

interface StatCardsProps {
  stats: {
    totalVolumes: number;
    healthyVolumes: number;
    degradedVolumes: number;
    failedVolumes: number;
    faultedVolumes: number;
    volumesWithRebuilding: number;
    localNVMeVolumes: number;
    orphanedVolumes: number;
  };
  activeFilter?: VolumeFilter;
  onFilterClick: (filter: VolumeFilter) => void;
}

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
      subtitle: 'All replicas operational'
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
      subtitle: 'Immediate attention needed'
    },
    {
      id: 'rebuilding' as VolumeFilter,
      title: 'With Rebuilding',
      value: stats.volumesWithRebuilding,
      icon: Settings,
      color: 'text-rebuilding-600',
      bgColor: 'bg-rebuilding-50',
      borderColor: 'border-rebuilding-200',
      subtitle: 'Replica recovery active'
    },
    {
      id: 'local-nvme' as VolumeFilter,
      title: 'Local NVMe',
      value: stats.localNVMeVolumes,
      icon: Zap,
      color: 'text-brand-600',
      bgColor: 'bg-brand-50',
      borderColor: 'border-brand-200',
      subtitle: 'High performance storage'
    },
    {
      id: 'orphaned' as VolumeFilter,
      title: 'Orphaned',
      value: stats.orphanedVolumes,
      icon: Shield,
      color: 'text-warning-600',
      bgColor: 'bg-warning-50',
      borderColor: 'border-warning-200',
      subtitle: 'Raw SPDK volumes (needs cleanup)'
    }
  ];

  return (
    <div className="grid grid-cols-2 md:grid-cols-4 lg:grid-cols-7 xl:grid-cols-7 gap-4">
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
