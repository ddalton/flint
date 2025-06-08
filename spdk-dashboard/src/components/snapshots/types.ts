// Enhanced types.ts - Updated with storage consumption and relationship data

export interface SnapshotDetails {
  snapshot_id: string;
  source_volume_id: string;
  creation_time: string;
  ready_to_use: boolean;
  size_bytes: number;
  snapshot_type: string;
  clone_source_snapshot_id?: string;
  replica_bdev_details: ReplicaBdevDetails[];
  // Enhanced storage information
  storage_consumption?: SnapshotStorageInfo;
  parent_snapshot_id?: string;
  child_snapshot_ids?: string[];
}

export interface SnapshotStorageInfo {
  consumed_bytes: number;
  cluster_size: number;
  allocated_clusters: number;
  compression_ratio?: number;
  deduplication_savings?: number;
  actual_storage_overhead: number; // Storage used beyond the logical volume size
}

export interface ReplicaBdevDetails {
  node: string;
  name: string;
  aliases: string[];
  driver: string;
  snapshot_source_bdev?: string;
  // Enhanced storage details per replica
  storage_info?: {
    consumed_bytes: number;
    cluster_size: number;
    allocated_clusters: number;
  };
}

export interface SnapshotTreeNode {
  volume_name: string;
  volume_id: string;
  volume_size: number;
  snapshot_chain: SnapshotChainInfo;
  // Enhanced storage analytics
  storage_analytics: VolumeStorageAnalytics;
}

export interface SnapshotChainInfo {
  active_lvol: string;
  chain_depth: number;
  snapshots: SnapshotChainItem[];
  error?: string;
}

export interface SnapshotChainItem {
  bdev_name: string;
  snapshot_id?: string;
  details: any;
  children: SnapshotChainItem[];
  storage_info?: {
    consumed_bytes: number;
    cluster_size: number;
    allocated_clusters: number;
  };
  // Relationship information
  parent_bdev?: string;
  creation_order?: number;
  is_active_volume?: boolean;
}

export interface VolumeStorageAnalytics {
  total_volume_size: number;
  actual_data_size: number;
  total_snapshot_overhead: number;
  snapshot_efficiency_ratio: number; // Actual overhead / logical size
  storage_breakdown: {
    active_volume_consumption: number;
    snapshot_consumption: number;
    metadata_overhead: number;
    free_space_in_volume: number;
  };
  recommendations: string[];
}

export interface ReplicaSnapshotInfo {
  node: string;
  bdev_name: string;
  source_bdev: string;
  disk: string;
  // Enhanced with storage details
  storage_consumed: number;
  storage_efficiency: number;
}

export interface SnapshotRelationshipMap {
  [snapshotId: string]: {
    parent?: string;
    children: string[];
    depth: number;
    branch: string; // main, branch-1, etc.
  };
}

export type SnapshotTypeFilter = 'all' | 'Bdev' | 'LvolClone' | 'External';
export type SnapshotViewMode = 'list' | 'tree' | 'topology' | 'storage';

// New view mode for storage analysis
export interface SnapshotStorageViewProps {
  snapshots: SnapshotDetails[];
  snapshotTree: Record<string, SnapshotTreeNode>;
  onSnapshotSelect: (snapshot: SnapshotDetails) => void;
  formatSize: (bytes: number) => string;
}