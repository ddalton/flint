import React from 'react';
import { Database, AlertTriangle, Settings, Zap } from 'lucide-react';

interface StatCardsProps {
  stats: {
    totalVolumes: number;
    faultedVolumes: number;
    rebuildingVolumes: number;
    localNVMeVolumes: number;
  };
}

export const StatCards: React.FC<StatCardsProps> = ({ stats }) => {
  return (
    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-6 mb-8">
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center">
          <Database className="w-10 h-10 text-blue-600 mr-4" />
          <div>
            <p className="text-3xl font-bold text-gray-900">{stats.totalVolumes}</p>
            <p className="text-gray-600">Total Volumes</p>
          </div>
        </div>
      </div>
      
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center">
          <AlertTriangle className="w-10 h-10 text-red-600 mr-4" />
          <div>
            <p className="text-3xl font-bold text-gray-900">{stats.faultedVolumes}</p>
            <p className="text-gray-600">Faulted Volumes</p>
          </div>
        </div>
      </div>
      
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center">
          <Settings className="w-10 h-10 text-orange-600 mr-4" />
          <div>
            <p className="text-3xl font-bold text-gray-900">{stats.rebuildingVolumes}</p>
            <p className="text-gray-600">Rebuilding</p>
          </div>
        </div>
      </div>
      
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center">
          <Zap className="w-10 h-10 text-green-600 mr-4" />
          <div>
            <p className="text-3xl font-bold text-gray-900">{stats.localNVMeVolumes}</p>
            <p className="text-gray-600">Local NVMe</p>
          </div>
        </div>
      </div>
    </div>
  );
};