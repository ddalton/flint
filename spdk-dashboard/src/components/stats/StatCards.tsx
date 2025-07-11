import React from 'react';
import { Database, CheckCircle, AlertTriangle, XCircle, Settings, Zap, Cable } from 'lucide-react';
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
      color: 'text-blue-600',
      bgColor: 'bg-blue-50',
      borderColor: 'border-blue-200'
    },
    {
      id: 'healthy' as VolumeFilter,
      title: 'Healthy',
      value: stats.healthyVolumes,
      icon: CheckCircle,
      color: 'text-green-600',
      bgColor: 'bg-green-50',
      borderColor: 'border-green-200',
      subtitle: 'All replicas operational'
    },
    {
      id: 'degraded' as VolumeFilter,
      title: 'Degraded',
      value: stats.degradedVolumes,
      icon: AlertTriangle,
      color: 'text-yellow-600',
      bgColor: 'bg-yellow-50',
      borderColor: 'border-yellow-200',
      subtitle: 'Reduced redundancy'
    },
    {
      id: 'failed' as VolumeFilter,
      title: 'Failed',
      value: stats.failedVolumes,
      icon: XCircle,
      color: 'text-red-600',
      bgColor: 'bg-red-50',
      borderColor: 'border-red-200',
      subtitle: 'Immediate attention needed'
    },
    {
      id: 'rebuilding' as VolumeFilter,
      title: 'With Rebuilding',
      value: stats.volumesWithRebuilding,
      icon: Settings,
      color: 'text-orange-600',
      bgColor: 'bg-orange-50',
      borderColor: 'border-orange-200',
      subtitle: 'Replica recovery active'
    },
    {
      id: 'local-nvme' as VolumeFilter,
      title: 'Local NVMe',
      value: stats.localNVMeVolumes,
      icon: Zap,
      color: 'text-blue-600',
      bgColor: 'bg-blue-50',
      borderColor: 'border-blue-200',
      subtitle: 'High performance storage'
    }
  ];

  return (
    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6 gap-4">
      {cards.map((card) => {
        const isActive = activeFilter === card.id;
        const Icon = card.icon;
        
        return (
          <button
            key={card.id}
            onClick={() => onFilterClick(card.id)}
            className={`bg-white rounded-lg shadow p-4 text-left transition-all duration-200 hover:shadow-lg hover:scale-105 ${
              isActive 
                ? `ring-2 ring-blue-500 ${card.bgColor} border-2 ${card.borderColor}` 
                : 'hover:bg-gray-50'
            }`}
          >
            <div className="flex items-center mb-2">
              <Icon className={`w-8 h-8 ${card.color} mr-3`} />
              <div className="flex-1">
                <p className="text-2xl font-bold text-gray-900">{card.value}</p>
                <p className="text-sm font-medium text-gray-700">{card.title}</p>
                {card.subtitle && (
                  <p className="text-xs text-gray-500 mt-1">{card.subtitle}</p>
                )}
              </div>
            </div>
            {isActive && card.id !== 'all' && (
              <p className="text-xs text-blue-600 font-medium">
                Click to clear filter
              </p>
            )}
          </button>
        );
      })}
    </div>
  );
};
