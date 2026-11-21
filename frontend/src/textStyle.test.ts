import { describe, expect, it } from "vitest";
import type { ButtonNav } from "./api";
import { hexToRgba, resolveNav, textBoxCss, textGlyphCss } from "./textStyle";
import { applyStyleToSimilar, computeNavigationFallback, emptyMenu, scaleMenuToBluray, text, type CustomLayout, type CustomMenu, type TextElement } from "./layout";

const T = (extra: Partial<TextElement> = {}) => text(0, 0, 100, 40, "x", extra);

describe("hexToRgba", () => {
  it("handles #abc, #aabbcc and invalid input", () => {
    expect(hexToRgba("#abc", 0.5)).toBe("rgba(170, 187, 204, 0.5)");
    expect(hexToRgba("#aabbcc", 1)).toBe("rgba(170, 187, 204, 1)");
    expect(hexToRgba("nope", 1)).toBe("rgba(0, 0, 0, 1)");
    expect(hexToRgba("#fff", 7)).toBe("rgba(255, 255, 255, 1)");
  });
});

describe("textGlyphCss", () => {
  it("defaults reproduce the old shadow", () => {
    expect(textGlyphCss(T({ shadow: true })).textShadow).toBe("2px 2px 0px rgba(0, 0, 0, 0.67)");
    expect(textGlyphCss(T({ shadow: false })).textShadow).toBe("none");
  });
  it("applies blur, colour and opacity", () => {
    const css = textGlyphCss(T({ shadow: true, shadowX: 1, shadowY: 3, shadowBlur: 4, shadowColor: "#ff0000", shadowOpacity: 0.5 }));
    expect(css.textShadow).toBe("1px 3px 4px rgba(255, 0, 0, 0.5)");
  });
  it("draws the outline only when width > 0", () => {
    expect(textGlyphCss(T()).webkitTextStroke).toBeUndefined();
    const css = textGlyphCss(T({ outlineWidth: 2, outlineColor: "#00ff00" }));
    expect(css.webkitTextStroke).toBe("4px #00ff00");
    expect(css.paintOrder).toBe("stroke fill");
  });
});

describe("textBoxCss", () => {
  it("is transparent by default", () => {
    expect(textBoxCss(T())).toMatchObject({ backgroundColor: "transparent", borderRadius: "0px / 0px", padding: "0px" });
  });
  it("fills with rgba, radius and padding", () => {
    expect(textBoxCss(T({ bgColor: "#000", bgOpacity: 0.4, rx: 8, ry: 4, padding: 6 }))).toMatchObject({
      backgroundColor: "rgba(0, 0, 0, 0.4)",
      borderRadius: "8px / 4px",
      padding: "6px",
      boxSizing: "border-box",
    });
  });
});

const btn = (id: string, x: number, y: number, extra: Partial<TextElement> = {}) =>
  T({ id, x, y, action: { type: "title", title: 1 } as never, ...extra });
const menuOf = (...els: TextElement[]): CustomMenu => ({ ...emptyMenu("m", "root"), elements: els });

describe("resolveNav", () => {
  const auto: ButtonNav[] = [
    { id: "a", up: "a", down: "b", left: "a", right: "a" },
    { id: "b", up: "a", down: "b", left: "b", right: "b" },
  ];
  it("override wins, invalid/self ignored, null keeps auto", () => {
    const m = menuOf(btn("a", 0, 0, { nav: { up: "b", down: "a", left: "ghost", right: null } }), btn("b", 0, 100));
    const r = resolveNav(m, auto);
    expect(r[0]).toEqual({ id: "a", up: "b", down: "b", left: "a", right: "a" });
    expect(r[1]).toEqual(auto[1]);
  });
  it("computeNavigationFallback applies overrides", () => {
    const plain = computeNavigationFallback(menuOf(btn("a", 0, 0), btn("b", 0, 100), btn("c", 0, 200)));
    expect(plain[0].down).toBe("b");
    const m = menuOf(btn("a", 0, 0, { nav: { down: "c" } }), btn("b", 0, 100), btn("c", 0, 200));
    expect(computeNavigationFallback(m)[0].down).toBe("c");
  });
});

describe("style propagation and BD scaling", () => {
  it("applyStyleToSimilar copies the new text fields but not nav/autoExecute", () => {
    const mk = (extra: Partial<TextElement>) => ({ ...emptyMenu("s", "settings"), elements: [btn("t", 0, 0, extra)] });
    const a = mk({ bgColor: "#123456", rx: 5, outlineWidth: 2, nav: { up: "z" }, autoExecute: true });
    const b = mk({ padding: 9 });
    const layout = { menus: [a, b] } as unknown as CustomLayout;
    expect(applyStyleToSimilar(layout, a.id)).toBe(1);
    const e = b.elements[0] as TextElement;
    expect(e).toMatchObject({ bgColor: "#123456", rx: 5, outlineWidth: 2 });
    expect(e.padding).toBeUndefined();
    expect(e.nav).toBeUndefined();
    expect(e.autoExecute).toBeUndefined();
  });
  it("BD scaling multiplies the new geometry by the height ratio", () => {
    const e = T({ rx: 4, ry: 4, padding: 8, shadowX: 2, shadowY: 2, shadowBlur: 3, outlineWidth: 1 });
    const m = menuOf(e);
    scaleMenuToBluray(m, { width: 720, height: 576 }, { width: 1920, height: 1080 });
    const k = 1080 / 576;
    expect(e).toMatchObject({ rx: Math.round(4 * k), ry: Math.round(4 * k), padding: Math.round(8 * k), shadowX: Math.round(2 * k), shadowBlur: Math.round(3 * k), outlineWidth: Math.round(k) });
  });
});
