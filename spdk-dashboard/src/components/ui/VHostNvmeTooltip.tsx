import React, { useState } from 'react';

interface VHostNvmeTooltipProps {
  vhostSocket?: string;
  vhostDevice?: string;
  vhostType?: string;
  raidLevel?: string;
  children: React.ReactNode;
}

export const VHostNvmeTooltip: React.FC<VHostNvmeTooltipProps> = ({ 
  vhostSocket, 
  vhostDevice, 
  vhostType = 'nvme',
  raidLevel,
  children 
}) => {
  const [showTooltip, setShowTooltip] = useState(false);
  
  if (!vhostSocket) return <>{children}</>;
  
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
            <div><strong>VHost Type:</strong> {vhostType.toUpperCase()}</div>
            <div><strong>Socket:</strong> {vhostSocket}</div>
            {vhostDevice && (
              <div><strong>Device Path:</strong> {vhostDevice}</div>
            )}
            {raidLevel && (
              <div><strong>RAID Level:</strong> {raidLevel}</div>
            )}
            <div className="text-gray-300 mt-2">
              VHost-NVMe exposes the {raidLevel || 'RAID'} volume as a single NVMe namespace (NSID 1)
            </div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};