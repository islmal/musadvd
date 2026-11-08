// Speed dropdown choices for Burn Studio: merges fixed safe speeds with drive-reported ones.

const FIXED_SPEEDS = [12, 8, 4];

export function speedChoices(reportedX: number[]): [string, string][] {
  const merged: number[] = [...FIXED_SPEEDS];
  for (const r of reportedX) {
    if (r <= 0 || !Number.isFinite(r)) continue;
    const isNearFixed = FIXED_SPEEDS.some((f) => Math.abs(r - f) < 0.3);
    if (!isNearFixed) {
      if (!merged.some((m) => Math.abs(r - m) < 0.1)) {
        merged.push(r);
      }
    }
  }
  merged.sort((a, b) => b - a);

  return merged.map((x) => {
    const isIntegerLike = Math.abs(x - Math.round(x)) < 0.05;
    const value = isIntegerLike ? `${Math.round(x)}` : x.toFixed(1).replace(/\.0$/, "");
    const label = `${Math.abs(x - Math.round(x)) < 0.3 ? Math.round(x) : value}x`;
    return [value, label];
  });
}
