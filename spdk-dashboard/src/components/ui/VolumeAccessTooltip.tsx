import React, { useState } from 'react';

interface VolumeAccessTooltipProps {
  ublkDevice?: {
    id: number;
    device_path: string;
  };
  raidLevel?: string;
  children: React.ReactNode;
}

export const VolumeAccessTooltip: React.FC<VolumeAccessTooltipProps> = ({ 
  ublkDevice,
  raidLevel,
  children 
}) => {
  const [showTooltip, setShowTooltip] = useState(false);

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
            <div><strong>Access Method:</strong> ublk (Userspace Block)</div>
            {ublkDevice && (
              <>
                <div><strong>Device Path:</strong> {ublkDevice.device_path}</div>
                <div><strong>ublk ID:</strong> {ublkDevice.id}</div>
              </>
            )}
            {raidLevel && (
              <div><strong>RAID Level:</strong> {raidLevel}</div>
            )}
            <div className="text-gray-300 mt-2">
              Volume exposed via ublk for high-performance local access.
              Direct userspace to kernel communication without network overhead.
            </div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};
