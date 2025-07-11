import React, { useState } from 'react';
import type { NvmeofTargetInfo } from '../../hooks/useDashboardData';

interface VolumeAccessTooltipProps {
  targets: NvmeofTargetInfo[];
  raidLevel?: string;
  children: React.ReactNode;
}

export const VolumeAccessTooltip: React.FC<VolumeAccessTooltipProps> = ({ 
  targets,
  raidLevel,
  children 
}) => {
  const [showTooltip, setShowTooltip] = useState(false);
  
  if (!targets || targets.length === 0) return <>{children}</>;
  
  const primaryTarget = targets[0];

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
        <div className="absolute z-50 bottom-full left-1/2 transform -translate-x-1/2 mb-2 px-3 py-2 bg-gray-900 text-white text-xs rounded-lg shadow-lg whitespace-nowrap max-w-md">
          <div className="space-y-1 text-left">
            <div><strong>Access Method:</strong> NVMe-oF ({primaryTarget.transport})</div>
            <div><strong>NQN:</strong> {primaryTarget.nqn}</div>
            <div><strong>Target:</strong> {primaryTarget.target_ip}:{primaryTarget.target_port}</div>
            {raidLevel && (
              <div><strong>RAID Level:</strong> {raidLevel}</div>
            )}
            <div className="text-gray-300 mt-2">
              Volume exposed as an NVMe-oF target for network access.
            </div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};
