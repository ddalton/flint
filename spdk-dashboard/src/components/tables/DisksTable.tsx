import React from 'react';
import type { Disk } from '../../hooks/useDashboardData';

interface DisksTableProps {
  disks: Disk[];
  stats: {
    totalDisks: number;
    healthyDisks: number;
    formattedDisks: number;
  };
}

export const DisksTable: React.FC<DisksTableProps> = ({ disks, stats }) => {
  return (
    <div>
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4 mb-6">
        <div className="bg-gray-50 rounded-lg p-4">
          <h3 className="text-lg font-semibold">Total Disks</h3>
          <p className="text-3xl font-bold text-blue-600">{stats.totalDisks}</p>
        </div>
        <div className="bg-gray-50 rounded-lg p-4">
          <h3 className="text-lg font-semibold">Healthy Disks</h3>
          <p className="text-3xl font-bold text-green-600">{stats.healthyDisks}</p>
        </div>
        <div className="bg-gray-50 rounded-lg p-4">
          <h3 className="text-lg font-semibold">LVS Initialized</h3>
          <p className="text-3xl font-bold text-blue-600">{stats.formattedDisks}</p>
        </div>
      </div>
      
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Disk ID</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Node</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Model</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Capacity</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Free Space</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Status</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">LVS Initialized</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Performance</th>
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {disks.map((disk) => (
              <tr key={disk.id} className="hover:bg-gray-50">
                <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{disk.id}</td>
                <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.node}</td>
                <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.model}</td>
                <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.capacity_gb}GB</td>
                <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.free_space}GB</td>
                <td className="px-6 py-4 whitespace-nowrap">
                  <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                    disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                  }`}>
                    {disk.healthy ? 'Healthy' : 'Unhealthy'}
                  </span>
                </td>
                <td className="px-6 py-4 whitespace-nowrap">
                  <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                    disk.lvol_store_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                  }`}>
                    {disk.lvol_store_initialized ? 'Yes' : 'No'}
                  </span>
                </td>
                <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                  <div>
                    <div>R: {disk.read_iops.toLocaleString()} IOPS</div>
                    <div>W: {disk.write_iops.toLocaleString()} IOPS</div>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
};