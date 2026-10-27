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
