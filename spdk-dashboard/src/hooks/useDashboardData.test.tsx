// Phase 1 seams under test: the query hook keeps last-known-good data and
// surfaces honest errors (no mock fallback — Decision 1), the wire transform
// hardens partial payloads, and recovery detection drives the adaptive poll.
import { describe, expect, it } from 'vitest';
import React from 'react';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { http, HttpResponse } from 'msw';
import { server } from '../test/server';
import { makeDashboardData, makeDisk, makeReplica, makeSyncInfo, makeVolume } from '../test/fixtures';
import * as api from '../api/client';
import {
  computeStats,
  hasRecoveringReplicas,
  isReplicaRecovering,
  transformBackendData,
  useDashboardData,
  type ReplicaStatus,
} from './useDashboardData';

const createWrapper = () => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
};

describe('useDashboardData', () => {
  it('serves the transformed aggregate once authenticated', async () => {
    await api.login('admin', 'right-password');
    const { result } = renderHook(() => useDashboardData(false), {
      wrapper: createWrapper(),
    });

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.connectionError).toBeNull();
    expect(result.current.data.volumes).toHaveLength(1);
    expect(result.current.data.volumes[0]?.name).toBe('hr-e2e');
    expect(result.current.stats.totalVolumes).toBe(1);
    expect(result.current.stats.healthyVolumes).toBe(1);
    expect(result.current.stats.totalDisks).toBe(2);
    api.logout();
  });

  it('keeps last-known-good data and reports the error when the backend goes away', async () => {
    await api.login('admin', 'right-password');
    const { result } = renderHook(() => useDashboardData(false), {
      wrapper: createWrapper(),
    });
    await waitFor(() => expect(result.current.data.volumes).toHaveLength(1));

    server.use(
      http.get('/api/dashboard', () => HttpResponse.json({ error: 'boom' }, { status: 500 }))
    );
    await result.current.refreshData();

    await waitFor(() => expect(result.current.connectionError).toBe('Backend error (HTTP 500)'));
    // Stale truth clearly labeled beats fresh fiction: the data is still there.
    expect(result.current.data.volumes).toHaveLength(1);
    api.logout();
  });

  it('rejects a non-JSON response instead of rendering fiction', async () => {
    await api.login('admin', 'right-password');
    server.use(
      http.get('/api/dashboard', () => new HttpResponse('<html>proxy error</html>', {
        headers: { 'Content-Type': 'text/html' },
      }))
    );
    const { result } = renderHook(() => useDashboardData(false), {
      wrapper: createWrapper(),
    });

    await waitFor(() =>
      expect(result.current.connectionError).toBe('Received non-JSON response from backend')
    );
    expect(result.current.data.volumes).toHaveLength(0);
    api.logout();
  });
});

describe('transformBackendData', () => {
  it('hardens missing array fields on partial payloads', () => {
    // A backend mid-rollout can omit newer fields; the transform must not
    // let a missing array crash a table render.
    const wire = makeDashboardData();
    const wireVolume = wire.volumes[0] as Partial<(typeof wire.volumes)[number]>;
    const wireDisk = wire.disks[0] as Partial<(typeof wire.disks)[number]>;
    delete wireVolume.consumer_raids;
    delete wireVolume.nvmeof_targets;
    delete wireDisk.provisioned_volumes;

    const data = transformBackendData(wire);

    expect(data.volumes[0]?.consumer_raids).toEqual([]);
    expect(data.volumes[0]?.nvmeof_targets).toEqual([]);
    expect(data.disks[0]?.provisioned_volumes).toEqual([]);
  });

  it('derives disk capacity fields when the backend omits the GB projections', () => {
    const wire = makeDashboardData();
    wire.disks = [
      {
        ...makeDisk(),
        capacity: 4 * 1024 ** 3,
        capacity_gb: 0,
        free_space: 1,
        allocated_space: 0,
        free_space_display: '',
      },
    ];

    const disk = transformBackendData(wire).disks[0];

    expect(disk?.capacity_gb).toBe(4);
    expect(disk?.allocated_space).toBe(3);
    expect(disk?.free_space_display).toBe('1GB');
  });
});

describe('recovery detection (adaptive poll input)', () => {
  const asReplica = (r: ReturnType<typeof makeReplica>) => r as ReplicaStatus;

  it('a replica is recovering when the engine says non-in_sync', () => {
    expect(isReplicaRecovering(asReplica(makeReplica()))).toBe(false);
    expect(
      isReplicaRecovering(
        asReplica(makeReplica({ sync: makeSyncInfo({ sync_state: 'stale', epoch_lag: 2 }) }))
      )
    ).toBe(true);
    expect(
      isReplicaRecovering(
        asReplica(makeReplica({ sync: makeSyncInfo({ sync_state: 'standby', epoch_lag: 1 }) }))
      )
    ).toBe(true);
  });

  it('falls back to legacy rebuild markers when there is no sync record', () => {
    expect(isReplicaRecovering(asReplica(makeReplica({ sync: null })))).toBe(false);
    expect(
      isReplicaRecovering(asReplica(makeReplica({ sync: null, rebuild_progress: 40.0 })))
    ).toBe(true);
    expect(
      isReplicaRecovering(asReplica(makeReplica({ sync: null, status: 'rebuilding' })))
    ).toBe(true);
  });

  it('flags the cluster while any replica chases and clears once all are in_sync', () => {
    const healthy = transformBackendData(makeDashboardData());
    expect(hasRecoveringReplicas(healthy)).toBe(false);
    expect(hasRecoveringReplicas(undefined)).toBe(false);

    const degraded = transformBackendData(
      makeDashboardData({
        volumes: [
          makeVolume({
            replica_statuses: [
              makeReplica(),
              makeReplica({
                node: 'runj-aws-2',
                sync: makeSyncInfo({ sync_state: 'standby', epoch_lag: 3 }),
              }),
            ],
          }),
        ],
      })
    );
    expect(hasRecoveringReplicas(degraded)).toBe(true);
  });
});

describe('computeStats', () => {
  it('counts states, faulted union, and orphans from the aggregate', () => {
    const data = transformBackendData(
      makeDashboardData({
        volumes: [
          makeVolume(),
          makeVolume({ id: 'v2', name: 'v2', state: 'Degraded' }),
          makeVolume({ id: 'v3', name: 'v3', state: 'Failed' }),
          makeVolume({ id: 'v4', name: 'v4', local_nvme: true }),
        ],
        raw_volumes: [{}],
      })
    );

    const stats = computeStats(data);

    expect(stats.totalVolumes).toBe(5); // 4 managed + 1 orphan
    expect(stats.healthyVolumes).toBe(2);
    expect(stats.degradedVolumes).toBe(1);
    expect(stats.failedVolumes).toBe(1);
    expect(stats.faultedVolumes).toBe(2);
    expect(stats.localNVMeVolumes).toBe(1);
    expect(stats.orphanedVolumes).toBe(1);
    expect(stats.formattedDisks).toBe(2);
  });
});
