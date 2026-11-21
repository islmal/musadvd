import type { TextElement } from "./layout";

// Lives in layout.ts (it needs isButton); re-exported so callers can import all text/nav helpers from here.
export { resolveNav } from "./layout";

/** "#rgb" / "#rrggbb" to `rgba(...)`; anything else falls back to black. */
export function hexToRgba(hex: string, alpha: number): string {
  let h = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex?.trim() ?? "")?.[1] ?? "000000";
  if (h.length === 3) h = [...h].map((c) => c + c).join("");
  const n = parseInt(h, 16);
  const a = Math.min(1, Math.max(0, Number.isFinite(alpha) ? alpha : 1));
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}

/** CSS for the element box: fill, rounded corners and padding (canvas units = px on the unscaled stage). */
export function textBoxCss(e: TextElement): Record<string, string> {
  return {
    backgroundColor: e.bgColor ? hexToRgba(e.bgColor, e.bgOpacity ?? 1) : "transparent",
    borderRadius: `${e.rx ?? 0}px / ${e.ry ?? 0}px`,
    padding: `${e.padding ?? 0}px`,
    boxSizing: "border-box",
  };
}

/** CSS for the glyphs: colour, shadow and outline. The stroke is centred on the edge, so double the width and paint it under the fill. */
export function textGlyphCss(e: TextElement): Record<string, string> {
  const css: Record<string, string> = {
    color: e.color,
    textShadow: e.shadow
      ? `${e.shadowX ?? 2}px ${e.shadowY ?? 2}px ${e.shadowBlur ?? 0}px ${hexToRgba(e.shadowColor ?? "#000000", e.shadowOpacity ?? 0.67)}`
      : "none",
  };
  if (e.outlineWidth && e.outlineWidth > 0) {
    css.webkitTextStroke = `${2 * e.outlineWidth}px ${e.outlineColor ?? "#000000"}`;
    css.paintOrder = "stroke fill";
  }
  return css;
}
