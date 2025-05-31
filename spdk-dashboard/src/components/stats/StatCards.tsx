import React from 'react';
import { Database, AlertTriangle, Settings, Zap } from 'lucide-react';
import type { VolumeFilter } from '../../hooks/useDashboardData';

interface StatCardsProps {
  stats: {
    totalVolumes: number;
    faultedVolumes: number;
    rebuildingVolumes: number;
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
      id: 'faulted' as VolumeFilter,
      title: 'Faulted Volumes',
      value: stats.faultedVolumes,
      icon: AlertTriangle,
      color: 'text-red-600',
      bgColor: 'bg-red-50',
      borderColor: 'border-red-200'
    },
    {
      id: 'rebuilding' as VolumeFilter,
      title: 'Rebuilding',
      value: stats.rebuildingVolumes,
      icon: Settings,
      color: 'text-orange-600',
      bgColor: 'bg-orange-50',
      borderColor: 'border-orange-200'
    },
    {
      id: 'local-nvme' as VolumeFilter,
      title: 'Local NVMe',
      value: stats.localNVMeVolumes,
      icon: Zap,
      color: 'text-green-600',
      bgColor: 'bg-green-50',
      borderColor: 'border-green-200'
    }
  ];

  return (
    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-6 mb-8">
      {cards.map((card) => {
        const isActive = activeFilter === card.id;
        const Icon = card.icon;
        
        return (
          <button
            key={card.id}
            onClick={() => onFilterClick(card.id)}
            className={`bg-white rounded-lg shadow p-6 text-left transition-all duration-200 hover:shadow-lg hover:scale-105 ${
              isActive 
                ? `ring-2 ring-blue-500 ${card.bgColor} border-2 ${card.borderColor}` 
                : 'hover:bg-gray-50'
            }`}
          >
            <div className="flex items-center">
              <Icon className={`w-10 h-10 ${card.color} mr-4`} />
              <div>
                <p className="text-3xl font-bold text-gray-900">{card.value}</p>
                <p className="text-gray-600">{card.title}</p>
                {isActive && card.id !== 'all' && (
                  <p className="text-xs text-blue-600 font-medium mt-1">
                    Click to clear filter
                  </p>
                )}
              </div>
            </div>
          </button>
        );
      })}
    </div>
  );
};