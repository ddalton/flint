import React, { useState } from 'react';
import type { NvmfTarget } from '../../hooks/useDashboardData';

interface NVMFTooltipProps {
  target: NvmfTarget | null;
  children: React.ReactNode;
}

export const NVMFTooltip: React.FC<NVMFTooltipProps> = ({ target, children }) => {
  const [showTooltip, setShowTooltip] = useState(false);
  
  if (!target) return <>{children}</>;
  
  return (
    <div className="relative inline-block">
      <div
        onMouseEnter={() => setShowTooltip(true)}
        onMouseLeave={() => setShowTooltip(false)}
        className="cursor-help"
      >
        {children}
      </div>
      {showTooltip && (
        <div className="absolute z-50 bottom-full left-1/2 transform -translate-x-1/2 mb-2 px-3 py-2 bg-gray-900 text-white text-xs rounded-lg shadow-lg whitespace-nowrap">
          <div className="space-y-1">
            <div><strong>NQN:</strong> {target.nqn}</div>
            <div><strong>Target:</strong> {target.target_ip}:{target.target_port}</div>
            <div><strong>Transport:</strong> {target.transport_type}</div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};