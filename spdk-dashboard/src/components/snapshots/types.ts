// types.ts - Snapshot-related TypeScript types

export interface SnapshotDetails {
  snapshot_id: string;
  source_volume_id: string;
  creation_time: string;
  ready_to_use: boolean;
  size_bytes: number;
  snapshot_type: string;
  clone_source_snapshot_id?: string;
  replica_bdev_details: ReplicaBdevDetails[];
}

export interface ReplicaBdevDetails {
  node: string;
  name: string;
  aliases: string[];
  driver: string;
  snapshot_source_bdev?: string;
}

export interface SnapshotTreeNode {
  volume_name: string;
  volume_id: string;
  volume_size: number;
  snapshots: SnapshotTreeItem[];
}

export interface SnapshotTreeItem {
  snapshot_id: string;
  snapshot_type: string;
  creation_time: string;
  ready_to_use: boolean;
  size_bytes: number;
  clone_source_snapshot_id?: string;
  replica_snapshots: ReplicaSnapshotInfo[];
  children: SnapshotTreeItem[];
}

export interface ReplicaSnapshotInfo {
  node: string;
  bdev_name: string;
  source_bdev: string;
  disk: string;
}

export type SnapshotTypeFilter = 'all' | 'Bdev' | 'LvolClone' | 'External';
export type SnapshotViewMode = 'list' | 'tree' | 'topology';