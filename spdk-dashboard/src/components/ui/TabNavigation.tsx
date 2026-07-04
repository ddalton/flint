import React from 'react';
import { Link, useSearchParams } from 'react-router';
import { Monitor, Database, HardDrive, Server, Settings, Camera, Activity } from 'lucide-react';
import { searchForTab, type TabId } from '../../routes';

interface Tab {
  id: TabId;
  name: string;
  icon: React.ComponentType<{ className?: string }>;
}

interface TabNavigationProps {
  activeTab: TabId;
  // Optional per-tab attention counts (e.g. uninitialized disks on
  // Disk Setup) — rendered as an amber pill after the tab name.
  badges?: Partial<Record<TabId, number | undefined>>;
}

const tabs: Tab[] = [
  { id: 'overview', name: 'Overview', icon: Monitor },
  { id: 'volumes', name: 'Volumes', icon: Database },
  { id: 'disks', name: 'Disks', icon: HardDrive },
  { id: 'events', name: 'Events', icon: Activity },
  { id: 'snapshots', name: 'Snapshots', icon: Camera },
  { id: 'disk-setup', name: 'Disk Setup', icon: Settings },
  { id: 'nodes', name: 'Nodes', icon: Server }
];

// Tabs are real links (deep-linkable, middle-clickable). Filter params
// persist across tabs; detail params (open modals) stay with their home tab
// — searchForTab implements that scoping.
export const TabNavigation: React.FC<TabNavigationProps> = ({ activeTab, badges }) => {
  const [searchParams] = useSearchParams();
  return (
    <div className="border-b border-gray-200">
      <nav className="-mb-px flex space-x-8 px-6">
        {tabs.map((tab) => {
          const badge = badges?.[tab.id];
          return (
            <Link
              key={tab.id}
              to={`/${tab.id}${searchForTab(searchParams, tab.id)}`}
              aria-current={activeTab === tab.id ? 'page' : undefined}
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
            </Link>
          );
        })}
      </nav>
    </div>
  );
};
