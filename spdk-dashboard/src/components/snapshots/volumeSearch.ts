// Volume-search resolution for the snapshot timeline: the operator knows
// the PVC name ("ui-val-a"), the timeline keys on the PV id ("pvc-<uuid>").
// Accept an exact id, an exact PVC name, or a unique substring of either;
// anything ambiguous resolves to nothing and the candidates render as
// clickable suggestions.
export function volumeInputMatches(
  input: string,
  ids: string[],
  idToName: Record<string, string>,
): string[] {
  const t = input.trim().toLowerCase();
  if (!t) return [];
  return ids.filter(
    id => id.toLowerCase().includes(t) || (idToName[id] ?? '').toLowerCase().includes(t)
  );
}

export function resolveVolumeInput(
  input: string,
  ids: string[],
  idToName: Record<string, string>,
): string | null {
  if (ids.includes(input)) return input;
  const t = input.trim().toLowerCase();
  if (!t) return null;
  const byName = ids.filter(id => (idToName[id] ?? '').toLowerCase() === t);
  if (byName.length === 1) return byName[0] ?? null;
  const bySub = volumeInputMatches(input, ids, idToName);
  return bySub.length === 1 ? bySub[0] ?? null : null;
}
