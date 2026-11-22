// Pure helpers for the title list. No imports and erasable TS only: scripts/check-order.mjs runs this file with Node.

/** Moves `arr[from]` so it ends up at index `to` (mutates). */
export function moveItem<T>(arr: T[], from: number, to: number): void {
  if (from === to) return;
  arr.splice(to, 0, ...arr.splice(from, 1));
}

/** Per group and member: true when its track counts differ from the group's first member (the engine's `check_group_shapes` rule). */
export function shapeMismatch(groups: { audio: number; subs: number }[][]): boolean[][] {
  return groups.map((g) => g.map((m) => m.audio !== g[0].audio || m.subs !== g[0].subs));
}

/**
 * Batch merge (mutates): gathers the items whose `id` is in `ids` into one block at the first
 * one's index and merges items 2..N into it. Others left with a new predecessor by the move are
 * unmerged, so they never silently join a different group.
 */
export function mergeBlock<T extends { id: number; mergeWithPrevious: boolean }>(arr: T[], ids: Set<number>): void {
  const picked = arr.filter((t) => ids.has(t.id));
  if (picked.length < 2) return;
  const prevOf = new Map(arr.map((t, i) => [t.id, arr[i - 1]?.id]));
  const first = arr.indexOf(picked[0]);
  const rest = arr.filter((t) => !ids.has(t.id));
  rest.splice(first, 0, ...picked);
  arr.splice(0, arr.length, ...rest);
  picked.forEach((t, k) => {
    if (k > 0) t.mergeWithPrevious = true;
  });
  arr.forEach((t, i) => {
    if (!ids.has(t.id) && prevOf.get(t.id) !== arr[i - 1]?.id) t.mergeWithPrevious = false;
  });
}
