import React, { useState } from 'react';
import type { VhostNvmeNamespace } from '../../hooks/useDashboardData';

interface VHostNvmeTooltipProps {
  vhostSocket?: string;
  vhostDevice?: string;
  vhostType?: string;
  nvmeNamespaces?: VhostNvmeNamespace[];
  children: React.ReactNode;
}

export const VHostNvmeTooltip: React.FC<VHostNvmeTooltipProps> = ({ 
  vhostSocket, 
  vhostDevice, 
  vhostType = 'nvme',
  nvmeNamespaces,
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
            {nvmeNamespaces && nvmeNamespaces.length > 0 && (
              <div className="mt-2">
                <div><strong>NVMe Namespaces:</strong></div>
                {nvmeNamespaces.map((ns, idx) => (
                  <div key={idx} className="ml-2 text-xs">
                    NSID {ns.nsid}: {Math.round(ns.size / 1024 / 1024 / 1024)}GB
                  </div>
                ))}
              </div>
            )}
            <div className="text-gray-300 mt-2">
              VHost-NVMe provides high-performance userspace NVMe access via Unix socket
            </div>
          </div>
          <div className="absolute top-full left-1/2 transform -translate-x-1/2 border-4 border-transparent border-t-gray-900"></div>
        </div>
      )}
    </div>
  );
};