import { useQuery } from '@tanstack/react-query';
import { apiFetch } from '../api/client';
import type { components } from '../api/schema';
import { nodeHealthStyle, type NodeHealth } from '../components/ui/status';

type Schemas = components['schemas'];

// Wire type aliased from the generated OpenAPI schema; health stays
// narrowed to the literal union the backend emits.
export type FleetNode = Omit<Schemas['NodeSummary'], 'health'> & {
  health: NodeHealth;
};
export type NodesData = Omit<Schemas['NodesResponse'], 'nodes'> & {
  nodes: FleetNode[];
};

const fetchNodes = async (): Promise<NodesData> => {
  const response = await apiFetch('/api/nodes');
  const contentType = response.headers.get('content-type') || '';
  if (!response.ok || contentType.indexOf('application/json') === -1) {
    throw new Error(
      response.ok
        ? 'Received non-JSON response from backend'
        : `Backend error (HTTP ${response.status})`
    );
  }
  return response.json();
};

// Same 30s cadence as the aggregate; both serve slices of the same
// server-side cached fan-out, so they agree within a cache TTL.
export const useNodesFleet = () =>
  useQuery({
    queryKey: ['nodes'],
    queryFn: fetchNodes,
    refetchInterval: 30_000,
  });

// Problems first, then stable by name.
export function compareFleetNodes(a: FleetNode, b: FleetNode): number {
  return (
    nodeHealthStyle(a.health).priority - nodeHealthStyle(b.health).priority ||
    a.name.localeCompare(b.name)
  );
}

export type FleetSort = 'problems' | 'name' | 'capacity' | 'volumes';

export function sortFleetNodes(nodes: FleetNode[], sort: FleetSort): FleetNode[] {
  const sorted = [...nodes];
  switch (sort) {
    case 'name':
      sorted.sort((a, b) => a.name.localeCompare(b.name));
      break;
    case 'capacity':
      sorted.sort((a, b) => b.capacity_gb - a.capacity_gb || a.name.localeCompare(b.name));
      break;
    case 'volumes':
      sorted.sort((a, b) => b.volumes_total - a.volumes_total || a.name.localeCompare(b.name));
      break;
    default:
      sorted.sort(compareFleetNodes);
  }
  return sorted;
}

// Facets are the fleet view's primary navigation: health buckets plus the
// onboarding bucket (uninitialized non-system disks, which is work to do
// but deliberately NOT a health condition).
export type FleetFacet = 'all' | NodeHealth | 'uninitialized';

export function matchesFacet(node: FleetNode, facet: FleetFacet): boolean {
  if (facet === 'all') return true;
  if (facet === 'uninitialized') return node.disks_uninitialized > 0;
  return node.health === facet;
}

export function facetCounts(nodes: FleetNode[]): Record<FleetFacet, number> {
  return {
    all: nodes.length,
    critical: nodes.filter(n => n.health === 'critical').length,
    warning: nodes.filter(n => n.health === 'warning').length,
    ok: nodes.filter(n => n.health === 'ok').length,
    uninitialized: nodes.filter(n => n.disks_uninitialized > 0).length,
  };
}
