// Pure snapping math for the menu editor (canvas units, no DOM).

export interface Box { x: number; y: number; w: number; h: number }
export type SnapKind = "center" | "safe" | "element" | "grid";
export interface SnapLine { pos: number; kind: SnapKind }
export interface SnapLines { x: SnapLine[]; y: SnapLine[] }
export interface Guide { axis: "x" | "y"; pos: number; kind: SnapKind }

/** Safe-area margins as fractions of the canvas per side. */
export const ACTION_SAFE = 0.05;
export const TITLE_SAFE = 0.1;

/** Canvas centre, both safe margins, and left/centre/right + top/middle/bottom of every box. */
export function snapLines(boxes: Box[], canvas: { width: number; height: number }): SnapLines {
  const x: SnapLine[] = [{ pos: canvas.width / 2, kind: "center" }];
  const y: SnapLine[] = [{ pos: canvas.height / 2, kind: "center" }];
  for (const f of [ACTION_SAFE, TITLE_SAFE]) {
    x.push({ pos: canvas.width * f, kind: "safe" }, { pos: canvas.width * (1 - f), kind: "safe" });
    y.push({ pos: canvas.height * f, kind: "safe" }, { pos: canvas.height * (1 - f), kind: "safe" });
  }
  for (const b of boxes) {
    for (const v of [b.x, b.x + b.w / 2, b.x + b.w]) x.push({ pos: v, kind: "element" });
    for (const v of [b.y, b.y + b.h / 2, b.y + b.h]) y.push({ pos: v, kind: "element" });
  }
  return { x, y };
}

/** Smallest signed offset (and its line) taking any of `values` onto a line within `threshold`. */
function best(values: number[], lines: SnapLine[], threshold: number): { d: number; line: SnapLine } | null {
  let out: { d: number; line: SnapLine } | null = null;
  for (const v of values) {
    for (const line of lines) {
      const d = line.pos - v;
      if (Math.abs(d) <= threshold && (!out || Math.abs(d) < Math.abs(out.d))) out = { d, line };
    }
  }
  return out;
}

export function snapMove(box: Box, lines: SnapLines, threshold: number): { dx: number; dy: number; guides: Guide[] } {
  const bx = best([box.x, box.x + box.w / 2, box.x + box.w], lines.x, threshold);
  const by = best([box.y, box.y + box.h / 2, box.y + box.h], lines.y, threshold);
  const guides: Guide[] = [];
  if (bx) guides.push({ axis: "x", pos: bx.line.pos, kind: bx.line.kind });
  if (by) guides.push({ axis: "y", pos: by.line.pos, kind: by.line.kind });
  return { dx: bx?.d ?? 0, dy: by?.d ?? 0, guides };
}

/** Only the edges flagged as moving snap; the opposite edges stay put. */
export function snapResize(
  box: Box,
  edges: { left?: boolean; right?: boolean; top?: boolean; bottom?: boolean },
  lines: SnapLines,
  threshold: number,
): { box: Box; guides: Guide[] } {
  let { x, y, w, h } = box;
  const guides: Guide[] = [];
  const edge = (axis: "x" | "y", v: number) => {
    const b = best([v], lines[axis], threshold);
    if (b) guides.push({ axis, pos: b.line.pos, kind: b.line.kind });
    return b ? b.line.pos : v;
  };
  if (edges.left) { const l = edge("x", x); w += x - l; x = l; }
  if (edges.right) w = edge("x", x + w) - x;
  if (edges.top) { const t = edge("y", y); h += y - t; y = t; }
  if (edges.bottom) h = edge("y", y + h) - y;
  return { box: { x, y, w, h }, guides };
}

export function snapToGrid(v: number, step: number): number {
  return step > 0 ? Math.round(v / step) * step : v;
}
