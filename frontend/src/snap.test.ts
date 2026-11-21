import { describe, expect, it } from "vitest";
import { ACTION_SAFE, TITLE_SAFE, snapLines, snapMove, snapResize, snapToGrid } from "./snap";

const canvas = { width: 720, height: 576 };

describe("snap", () => {
  it("lines hold centre, both safe margins and neighbour edges", () => {
    const l = snapLines([{ x: 100, y: 200, w: 50, h: 40 }], canvas);
    const xs = l.x.map((s) => s.pos);
    expect(xs).toEqual(expect.arrayContaining([360, 720 * ACTION_SAFE, 720 * (1 - TITLE_SAFE), 100, 125, 150]));
    expect(l.y.map((s) => s.pos)).toEqual(expect.arrayContaining([288, 200, 220, 240]));
    expect(l.x.find((s) => s.pos === 360)?.kind).toBe("center");
  });

  it("snapMove centres a box and reports guides", () => {
    const r = snapMove({ x: 332, y: 100, w: 50, h: 20 }, snapLines([], canvas), 4);
    expect(r.dx).toBe(3);
    expect(r.dy).toBe(0);
    expect(r.guides).toEqual([{ axis: "x", pos: 360, kind: "center" }]);
  });

  it("snapMove snaps to a safe margin and a neighbour edge, picks the smallest", () => {
    const l = snapLines([{ x: 300, y: 300, w: 100, h: 50 }], canvas);
    expect(snapMove({ x: 38, y: 0, w: 10, h: 10 }, l, 4).dx).toBe(-2); // 36 = 5 % of 720
    expect(snapMove({ x: 0, y: 297, w: 10, h: 10 }, l, 4).dy).toBe(-2); // middle 302 -> neighbour top 300
  });

  it("snapMove misses beyond the threshold", () => {
    const r = snapMove({ x: 100, y: 100, w: 10, h: 10 }, snapLines([], canvas), 2);
    expect(r).toEqual({ dx: 0, dy: 0, guides: [] });
  });

  it("snapResize moves only the chosen edges", () => {
    const l = snapLines([], canvas);
    const r = snapResize({ x: 100, y: 100, w: 257, h: 50 }, { right: true }, l, 4);
    expect(r.box).toEqual({ x: 100, y: 100, w: 260, h: 50 });
    const r2 = snapResize({ x: 100, y: 100, w: 50, h: 50 }, { left: true }, l, 4);
    expect(r2.box).toEqual({ x: 100, y: 100, w: 50, h: 50 }); // nothing near x=100
    const r3 = snapResize({ x: 38, y: 100, w: 62, h: 50 }, { left: true }, l, 4);
    expect(r3.box).toEqual({ x: 36, y: 100, w: 64, h: 50 });
    expect(r3.guides[0].kind).toBe("safe");
  });

  it("snapToGrid rounds to the step", () => {
    expect(snapToGrid(13, 10)).toBe(10);
    expect(snapToGrid(15, 10)).toBe(20);
    expect(snapToGrid(7, 0)).toBe(7);
  });
});
