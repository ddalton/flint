import React from 'react';
import { Monitor, Database, HardDrive, Server, Settings, Camera, Cloud, Activity } from 'lucide-react';

interface Tab {
  id: string;
  name: string;
  icon: React.ComponentType<{ className?: string }>;
}

interface TabNavigationProps {
  activeTab: string;
  onTabChange: (tabId: string) => void;
  // Optional per-tab attention counts (e.g. uninitialized disks on
  // Disk Setup) — rendered as an amber pill after the tab name.
  badges?: Record<string, number | undefined>;
}

const tabs: Tab[] = [
  { id: 'overview', name: 'Overview', icon: Monitor },
  { id: 'volumes', name: 'Volumes', icon: Database },
  { id: 'disks', name: 'Disks', icon: HardDrive },
  { id: 'events', name: 'Events', icon: Activity },
  { id: 'snapshots', name: 'Snapshots', icon: Camera }, // New snapshots tab
  { id: 'disk-setup', name: 'Disk Setup', icon: Settings },
  { id: 'remote-storage', name: 'Remote Storage', icon: Cloud },
  { id: 'nodes', name: 'Nodes', icon: Server }
];

export const TabNavigation: React.FC<TabNavigationProps> = ({ activeTab, onTabChange, badges }) => {
  return (
    <div className="border-b border-gray-200">
      <nav className="-mb-px flex space-x-8 px-6">
        {tabs.map((tab) => {
          const badge = badges?.[tab.id];
          return (
            <button
              key={tab.id}
              onClick={() => onTabChange(tab.id)}
              className={`flex items-center gap-2 py-4 px-1 border-b-2 font-medium text-sm ${
                activeTab === tab.id
                  ? 'border-blue-500 text-blue-600'
                  : 'border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300'
              }`}
            >
              <tab.icon className="w-5 h-5" />
              {tab.name}
              {badge !== undefined && badge > 0 && (
                <span className="px-1.5 py-0.5 text-xs font-semibold bg-amber-100 text-amber-800 rounded-full">
                  {badge}
                </span>
              )}
            </button>
          );
        })}
      </nav>
    </div>
  );
};
