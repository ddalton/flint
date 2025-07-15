import React, { useState, useEffect } from 'react';
import { Activity, TrendingUp, TrendingDown, BarChart3, Shield, Settings, X, AlertTriangle, HardDrive, Clock, CheckCircle } from 'lucide-react';
import type { RaidMember, ReplicaStatus, RaidStatus, Disk } from '../../hooks/useDashboardData';

// Add these new interfaces for throughput metrics
interface ThroughputMetrics {
  read_iops: AnimatedMetric;
  write_iops: AnimatedMetric;
  read_latency: AnimatedMetric;
  write_latency: AnimatedMetric;
}

interface AnimatedMetric {
  current: number;
  previous: number;
  trend: 'up' | 'down' | 'stable';
}

interface EnhancedRaidMemberCardProps {
  member: RaidMember;
  correspondingReplica?: ReplicaStatus;
  raidStatus: RaidStatus;
  disks: Disk[];
}

// Helper functions for styling, to be used in the component
const getRaidMemberStateColor = (state: string) => {
  switch (state.toLowerCase()) {
    case 'online': return 'bg-green-100 text-green-800 border-green-200';
    case 'degraded': return 'bg-yellow-100 text-yellow-800 border-yellow-200';
    case 'failed': return 'bg-red-100 text-red-800 border-red-200';
    case 'rebuilding': return 'bg-orange-100 text-orange-800 border-orange-200';
    case 'spare': return 'bg-blue-100 text-blue-800 border-blue-200';
    case 'removing': return 'bg-purple-100 text-purple-800 border-purple-200';
    default: return 'bg-gray-100 text-gray-800 border-gray-200';
  }
};

const getRaidMemberIcon = (state: string) => {
  switch (state.toLowerCase()) {
    case 'online': return <CheckCircle className="w-4 h-4 text-green-600" />;
    case 'failed': return <X className="w-4 h-4 text-red-600" />;
    case 'rebuilding': return <Settings className="w-4 h-4 text-orange-600 animate-spin" />;
    case 'degraded': return <AlertTriangle className="w-4 h-4 text-yellow-600" />;
    case 'spare': return <Shield className="w-4 h-4 text-blue-600" />;
    case 'removing': return <Clock className="w-4 h-4 text-purple-600" />;
    default: return <HardDrive className="w-4 h-4 text-gray-600" />;
  }
};


// Enhanced RAID Member card with throughput metrics
const EnhancedRaidMemberCard: React.FC<EnhancedRaidMemberCardProps> = ({ member, correspondingReplica, raidStatus, disks }) => {
  const [metrics, setMetrics] = useState<ThroughputMetrics>({
    read_iops: { current: 0, previous: 0, trend: 'stable' },
    write_iops: { current: 0, previous: 0, trend: 'stable' },
    read_latency: { current: 0, previous: 0, trend: 'stable' },
    write_latency: { current: 0, previous: 0, trend: 'stable' }
  });

  // Simulate real-time metric updates (replace with actual data polling)
  useEffect(() => {
    const interval = setInterval(() => {
      if (correspondingReplica?.disk_ref) {
        // Get disk metrics from dashboard data
        const diskData = disks.find(d => d.id === correspondingReplica.disk_ref);
        if (diskData) {
          setMetrics(prev => ({
            read_iops: {
              current: diskData.read_iops,
              previous: prev.read_iops.current,
              trend: diskData.read_iops > prev.read_iops.current ? 'up' :
                     diskData.read_iops < prev.read_iops.current ? 'down' : 'stable'
            },
            write_iops: {
              current: diskData.write_iops,
              previous: prev.write_iops.current,
              trend: diskData.write_iops > prev.write_iops.current ? 'up' :
                     diskData.write_iops < prev.write_iops.current ? 'down' : 'stable'
            },
            read_latency: {
              current: diskData.read_latency,
              previous: prev.read_latency.current,
              trend: diskData.read_latency > prev.read_latency.current ? 'up' :
                     diskData.read_latency < prev.read_latency.current ? 'down' : 'stable'
            },
            write_latency: {
              current: diskData.write_latency,
              previous: prev.write_latency.current,
              trend: diskData.write_latency > prev.write_latency.current ? 'up' :
                     diskData.write_latency < prev.write_latency.current ? 'down' : 'stable'
            }
          }));
        }
      }
    }, 2000); // Update every 2 seconds

    return () => clearInterval(interval);
  }, [correspondingReplica?.disk_ref, disks]);

  const formatLatency = (latencyUs: number) => {
    if (latencyUs < 1000) return `${latencyUs}μs`;
    if (latencyUs < 1000000) return `${(latencyUs / 1000).toFixed(1)}ms`;
    return `${(latencyUs / 1000000).toFixed(2)}s`;
  };

  const getTrendIcon = (trend: string) => {
    switch (trend) {
      case 'up': return <TrendingUp className="w-3 h-3 text-green-500" />;
      case 'down': return <TrendingDown className="w-3 h-3 text-red-500" />;
      default: return <BarChart3 className="w-3 h-3 text-gray-400" />;
    }
  };

  const getMetricChangeColor = (trend: string, isLatency: boolean = false) => {
    if (isLatency) {
      return trend === 'up' ? 'text-red-500' : trend === 'down' ? 'text-green-500' : 'text-gray-500';
    }
    return trend === 'up' ? 'text-green-500' : trend === 'down' ? 'text-red-500' : 'text-gray-500';
  };

  return (
    <div className="text-center">
      {/* RAID Member Header */}
      <div className="mb-4">
        <div className="w-12 h-12 bg-gray-100 rounded-full flex items-center justify-center mx-auto mb-2 border-2 border-gray-300">
          <span className="font-bold text-gray-700">#{member.slot}</span>
        </div>
        <p className="font-medium text-gray-800">{member.node || 'Unknown Node'}</p>
        <p className="text-xs text-gray-500">RAID Slot {member.slot}</p>
      </div>

      {/* RAID Member Status Card */}
      <div className={`border-2 rounded-lg p-4 ${getRaidMemberStateColor(member.state)}`}>
        <div className="flex items-center justify-between mb-2">
          <div className="flex items-center gap-2">
            {getRaidMemberIcon(member.state)}
            <span className="font-medium text-sm">{member.name}</span>
          </div>
          {member.is_configured && (
            <span className="text-xs bg-blue-500 text-white px-2 py-1 rounded-full">
              CFG
            </span>
          )}
        </div>

        {/* Existing status information */}
        <div className="text-xs space-y-1">
          <div className="flex justify-between">
            <span>RAID State:</span>
            <span className="font-medium capitalize">{member.state}</span>
          </div>

          <div className="flex justify-between">
            <span>Health:</span>
            <span className={`font-medium ${
              member.health_status === 'healthy' ? 'text-green-600' :
              member.health_status === 'rebuilding' ? 'text-orange-600' :
              'text-red-600'
            }`}>
              {member.health_status}
            </span>
          </div>
        </div>

        {/* NEW: Real-time Throughput Metrics */}
        {correspondingReplica?.disk_ref && (
          <div className="mt-3 pt-3 border-t border-gray-200">
            <div className="flex items-center gap-1 mb-2">
              <Activity className="w-4 h-4 text-blue-500 animate-pulse" />
              <span className="text-xs font-medium text-gray-700">Live Metrics</span>
            </div>

            <div className="grid grid-cols-2 gap-2 text-xs">
              {/* Read IOPS */}
              <div className="bg-white bg-opacity-60 rounded p-2">
                <div className="flex items-center justify-between">
                  <span className="text-gray-600">Read IOPS</span>
                  {getTrendIcon(metrics.read_iops.trend)}
                </div>
                <div className={`font-mono font-bold transition-all duration-300 ${
                  getMetricChangeColor(metrics.read_iops.trend)
                }`}>
                  {metrics.read_iops.current.toLocaleString()}
                </div>
              </div>

              {/* Write IOPS */}
              <div className="bg-white bg-opacity-60 rounded p-2">
                <div className="flex items-center justify-between">
                  <span className="text-gray-600">Write IOPS</span>
                  {getTrendIcon(metrics.write_iops.trend)}
                </div>
                <div className={`font-mono font-bold transition-all duration-300 ${
                  getMetricChangeColor(metrics.write_iops.trend)
                }`}>
                  {metrics.write_iops.current.toLocaleString()}
                </div>
              </div>

              {/* Read Latency */}
              <div className="bg-white bg-opacity-60 rounded p-2">
                <div className="flex items-center justify-between">
                  <span className="text-gray-600">Read Lat</span>
                  {getTrendIcon(metrics.read_latency.trend)}
                </div>
                <div className={`font-mono font-bold transition-all duration-300 ${
                  getMetricChangeColor(metrics.read_latency.trend, true)
                }`}>
                  {formatLatency(metrics.read_latency.current)}
                </div>
              </div>

              {/* Write Latency */}
              <div className="bg-white bg-opacity-60 rounded p-2">
                <div className="flex items-center justify-between">
                  <span className="text-gray-600">Write Lat</span>
                  {getTrendIcon(metrics.write_latency.trend)}
                </div>
                <div className={`font-mono font-bold transition-all duration-300 ${
                  getMetricChangeColor(metrics.write_latency.trend, true)
                }`}>
                  {formatLatency(metrics.write_latency.current)}
                </div>
              </div>
            </div>

            {/* Activity Indicator */}
            <div className="mt-2 flex items-center justify-center">
              <div className={`w-2 h-2 rounded-full ${
                metrics.read_iops.current > 0 || metrics.write_iops.current > 0
                  ? 'bg-green-500 animate-pulse'
                  : 'bg-gray-300'
              }`} />
              <span className="text-xs text-gray-500 ml-1">
                {metrics.read_iops.current > 0 || metrics.write_iops.current > 0 ? 'Active' : 'Idle'}
              </span>
            </div>
          </div>
        )}

        {/* Existing replica information */}
        {correspondingReplica && (
          <div className="mt-2 p-2 bg-white bg-opacity-50 rounded">
            <div className="text-xs">
              <div><strong>Access:</strong> {correspondingReplica.access_method}</div>
              {correspondingReplica.nvmf_target && (
                <div><strong>NVMe-oF:</strong> {correspondingReplica.nvmf_target.target_ip}</div>
              )}
              {correspondingReplica.last_io_timestamp && (
                <div><strong>Last I/O:</strong> {new Date(correspondingReplica.last_io_timestamp).toLocaleTimeString()}</div>
              )}
            </div>
          </div>
        )}

        {/* Existing rebuild progress */}
        {member.state === 'rebuilding' && raidStatus.rebuild_info &&
         raidStatus.rebuild_info.target_slot === member.slot && (
          <div className="mt-2">
            <div className="flex justify-between text-xs mb-1">
              <span>Rebuild Progress:</span>
              <span>{raidStatus.rebuild_info.progress_percentage.toFixed(1)}%</span>
            </div>
            <div className="w-full bg-gray-200 rounded-full h-2">
              <div
                className="bg-orange-500 h-2 rounded-full transition-all duration-300"
                style={{ width: `${raidStatus.rebuild_info.progress_percentage}%` }}
              />
            </div>
            {raidStatus.rebuild_info.estimated_time_remaining && (
              <div className="text-xs text-orange-600 mt-1">
                ETA: {raidStatus.rebuild_info.estimated_time_remaining}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
};

interface RaidArrayPerformanceOverviewProps {
    raidStatus: RaidStatus;
    replicaStatuses: ReplicaStatus[];
    disks: Disk[];
}

// NEW: RAID Array Performance Overview
const RaidArrayPerformanceOverview: React.FC<RaidArrayPerformanceOverviewProps> = ({ replicaStatuses, disks }) => {
  const [arrayMetrics, setArrayMetrics] = useState({
    totalReadIOPS: 0,
    totalWriteIOPS: 0,
    readLatency: 0,
    writeLatency: 0,
    peakReadIOPS: 0,
    peakWriteIOPS: 0
  });

  useEffect(() => {
    const interval = setInterval(() => {
      // Calculate aggregate RAID array performance
      const memberDisks = replicaStatuses
        .filter(r => r.disk_ref)
        .map(r => disks.find(d => d.id === r.disk_ref))
        .filter((d): d is Disk => !!d);

      if (memberDisks.length > 0) {
        const totalReadIOPS = memberDisks.reduce((sum, disk) => sum + disk.read_iops, 0);
        const totalWriteIOPS = memberDisks.reduce((sum, disk) => sum + disk.write_iops, 0);
        const readLatency = memberDisks[0]?.read_latency ?? 0;
        const writeLatency = Math.max(...memberDisks.map(disk => disk.write_latency));

        setArrayMetrics(prev => ({
          totalReadIOPS,
          totalWriteIOPS,
          readLatency,
          writeLatency,
          peakReadIOPS: Math.max(prev.peakReadIOPS, totalReadIOPS),
          peakWriteIOPS: Math.max(prev.peakWriteIOPS, totalWriteIOPS)
        }));
      }
    }, 2000);

    return () => clearInterval(interval);
  }, [replicaStatuses, disks]);

  return (
    <div className="mb-6 p-4 bg-gradient-to-r from-blue-50 to-indigo-50 rounded-lg border border-blue-200">
      <h5 className="text-lg font-semibold mb-3 text-blue-800 flex items-center gap-2">
        <BarChart3 className="w-5 h-5" />
        RAID Array Performance
      </h5>

      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        <div className="text-center">
          <div className="text-2xl font-bold text-blue-600 animate-pulse">
            {arrayMetrics.totalReadIOPS.toLocaleString()}
          </div>
          <div className="text-sm text-gray-600">Total Read IOPS</div>
          <div className="text-xs text-gray-500">
            Peak: {arrayMetrics.peakReadIOPS.toLocaleString()}
          </div>
        </div>

        <div className="text-center">
          <div className="text-2xl font-bold text-green-600 animate-pulse">
            {arrayMetrics.totalWriteIOPS.toLocaleString()}
          </div>
          <div className="text-sm text-gray-600">Total Write IOPS</div>
          <div className="text-xs text-gray-500">
            Peak: {arrayMetrics.peakWriteIOPS.toLocaleString()}
          </div>
        </div>

        <div className="text-center">
          <div className="text-2xl font-bold text-orange-600">
            {arrayMetrics.readLatency < 1000
              ? `${Math.round(arrayMetrics.readLatency)}μs`
              : `${(arrayMetrics.readLatency / 1000).toFixed(1)}ms`
            }
          </div>
          <div className="text-sm text-gray-600">Read Latency</div>
        </div>

        <div className="text-center">
          <div className="text-2xl font-bold text-purple-600">
            {arrayMetrics.writeLatency < 1000
              ? `${Math.round(arrayMetrics.writeLatency)}μs`
              : `${(arrayMetrics.writeLatency / 1000).toFixed(1)}ms`
            }
          </div>
          <div className="text-sm text-gray-600">Write Latency</div>
        </div>
      </div>
    </div>
  );
};

export { EnhancedRaidMemberCard, RaidArrayPerformanceOverview };
