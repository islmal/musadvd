import { describe, expect, it } from "vitest";
import { alphaBounds, clampCrop, croppedAspect, trimmedBox } from "./imageCrop";
import { iconSvg } from "./icons";
import { ICON_NAMES } from "./layout";

/** w x h RGBA buffer, opaque inside [x0,x1) x [y0,y1) with alpha `a`. */
function buffer(w: number, h: number, x0: number, y0: number, x1: number, y1: number, a = 255): Uint8ClampedArray {
  const d = new Uint8ClampedArray(w * h * 4);
  for (let y = y0; y < y1; y++) for (let x = x0; x < x1; x++) d[(y * w + x) * 4 + 3] = a;
  return d;
}

describe("alphaBounds", () => {
  it("finds the margins", () => {
    expect(alphaBounds(buffer(10, 10, 2, 3, 8, 6), 10, 10)).toEqual({ x: 0.2, y: 0.3, w: 0.6, h: 0.3 });
  });
  it("is null for a fully transparent picture", () => {
    expect(alphaBounds(new Uint8ClampedArray(4 * 4 * 4), 4, 4)).toBeNull();
  });
  it("ignores alpha at or below the threshold", () => {
    expect(alphaBounds(buffer(4, 4, 1, 1, 3, 3, 8), 4, 4)).toBeNull();
    expect(alphaBounds(buffer(4, 4, 1, 1, 3, 3, 9), 4, 4)).toEqual({ x: 0.25, y: 0.25, w: 0.5, h: 0.5 });
    expect(alphaBounds(buffer(4, 4, 1, 1, 3, 3, 50), 4, 4, 60)).toBeNull();
  });
});

describe("crop geometry", () => {
  it("computes the cropped aspect", () => {
    expect(croppedAspect(200, 100)).toBe(2);
    expect(croppedAspect(200, 100, { x: 0, y: 0, w: 0.5, h: 1 })).toBe(1);
  });
  it("keeps the centre and the larger side of the box", () => {
    const b = trimmedBox({ x: 100, y: 100, w: 200, h: 100 }, 200, 100, { x: 0, y: 0, w: 0.5, h: 1 });
    expect(b).toEqual({ x: 100, y: 50, w: 200, h: 200 });
    expect(b.x + b.w / 2).toBe(200);
    expect(b.y + b.h / 2).toBe(150);
    expect(b.w / b.h).toBeCloseTo(croppedAspect(200, 100, { x: 0, y: 0, w: 0.5, h: 1 }));
    const tall = trimmedBox({ x: 0, y: 0, w: 50, h: 100 }, 100, 100, null);
    expect(tall.h).toBe(100);
    expect(tall.w).toBe(100);
  });
  it("clamps into the picture", () => {
    expect(clampCrop({ x: -0.2, y: 0.9, w: 0.5, h: 0.5 })).toEqual({ x: 0, y: 0.5, w: 0.5, h: 0.5 });
    expect(clampCrop({ x: 0.5, y: 0.5, w: 2, h: 0 })).toEqual({ x: 0, y: 0.5, w: 1, h: 0.01 });
  });
});

describe("iconSvg", () => {
  it("replaces every currentColor", () => {
    for (const name of ICON_NAMES) {
      const svg = iconSvg(name, "#FF0000");
      expect(svg).not.toContain("currentColor");
      expect(svg).toContain("#FF0000");
    }
  });
});
