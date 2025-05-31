import React from 'react';
import { CheckCircle, X } from 'lucide-react';
import type { Volume } from '../../hooks/useDashboardData';

interface VolumesTableProps {
  volumes: Volume[];
}

export const VolumesTable: React.FC<VolumesTableProps> = ({ volumes }) => {
  return (
    <div className="overflow-x-auto">
      <table className="min-w-full divide-y divide-gray-200">
        <thead className="bg-gray-50">
          <tr>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Volume Name</th>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Size</th>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">State</th>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Replicas</th>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Local NVMe</th>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Rebuild Progress</th>
            <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Nodes</th>
          </tr>
        </thead>
        <tbody className="bg-white divide-y divide-gray-200">
          {volumes.map((volume) => (
            <tr key={volume.id} className="hover:bg-gray-50">
              <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{volume.name}</td>
              <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.size}</td>
              <td className="px-6 py-4 whitespace-nowrap">
                <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                  volume.state === 'Healthy' ? 'bg-green-100 text-green-800' :
                  volume.state === 'Rebuilding' ? 'bg-yellow-100 text-yellow-800' : 'bg-red-100 text-red-800'
                }`}>
                  {volume.state}
                </span>
              </td>
              <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.active_replicas}/{volume.replicas}</td>
              <td className="px-6 py-4 whitespace-nowrap">
                {volume.local_nvme ? (
                  <CheckCircle className="w-5 h-5 text-green-500" />
                ) : (
                  <X className="w-5 h-5 text-gray-400" />
                )}
              </td>
              <td className="px-6 py-4 whitespace-nowrap">
                {volume.rebuild_progress ? (
                  <div className="flex items-center gap-2">
                    <div className="w-20 bg-gray-200 rounded-full h-2">
                      <div 
                        className="bg-blue-600 h-2 rounded-full" 
                        style={{ width: `${volume.rebuild_progress}%` }}
                      />
                    </div>
                    <span className="text-sm text-gray-600">{volume.rebuild_progress}%</span>
                  </div>
                ) : (
                  <span className="text-gray-400">-</span>
                )}
              </td>
              <td className="px-6 py-4">
                <div className="flex flex-wrap gap-1">
                  {volume.nodes.map(node => (
                    <span key={node} className="inline-flex px-2 py-1 text-xs bg-gray-100 text-gray-800 rounded">
                      {node}
                    </span>
                  ))}
                </div>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
};