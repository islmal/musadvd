import type { CropRect } from "./layout";

/** Tight bounding box (normalized 0..1) of the pixels with alpha above `threshold`; null if none. */
export function alphaBounds(data: Uint8ClampedArray, w: number, h: number, threshold = 8): CropRect | null {
  let x0 = w;
  let y0 = h;
  let x1 = -1;
  let y1 = -1;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      if (data[(y * w + x) * 4 + 3] <= threshold) continue;
      if (x < x0) x0 = x;
      if (x > x1) x1 = x;
      if (y < y0) y0 = y;
      y1 = y;
    }
  }
  if (x1 < 0) return null;
  return { x: x0 / w, y: y0 / h, w: (x1 + 1 - x0) / w, h: (y1 + 1 - y0) / h };
}

/** Width : height of the (cropped) picture. */
export function croppedAspect(natW: number, natH: number, crop?: CropRect | null): number {
  return (natW * (crop?.w ?? 1)) / (natH * (crop?.h ?? 1));
}

/** The box around the same centre with its larger side kept and the other adapted to the cropped aspect. */
export function trimmedBox(e: { x: number; y: number; w: number; h: number }, natW: number, natH: number, crop: CropRect | null): { x: number; y: number; w: number; h: number } {
  const aspect = croppedAspect(natW, natH, crop);
  const cx = e.x + e.w / 2;
  const cy = e.y + e.h / 2;
  const [w, h] = e.w >= e.h ? [e.w, e.w / aspect] : [e.h * aspect, e.h];
  return { x: cx - w / 2, y: cy - h / 2, w, h };
}

/** Keeps the rectangle inside the picture and at least 1 % wide/high. */
export function clampCrop(c: CropRect): CropRect {
  const min = 0.01;
  const w = Math.min(1, Math.max(min, c.w));
  const h = Math.min(1, Math.max(min, c.h));
  return { x: Math.min(1 - w, Math.max(0, c.x)), y: Math.min(1 - h, Math.max(0, c.y)), w, h };
}
