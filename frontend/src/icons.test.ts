import { describe, expect, it } from "vitest";
import { STRETCH_ICONS, iconSvg } from "./icons";

describe("icons", () => {
  it("stretch icons are generated for the box, others ignore it", () => {
    for (const n of STRETCH_ICONS) {
      const s = iconSvg(n, "#fff", 200, 60);
      expect(s).toContain('viewBox="0 0 200.000 60.000"');
      expect(s).not.toContain("currentColor");
    }
    expect(iconSvg("frameRect", "#fff", 200, 60)).toContain('rx="2.400"');
    expect(iconSvg("home", "#fff", 200, 60)).toContain('viewBox="0 0 24 24"');
    expect(iconSvg("frameRect", "#fff")).toContain('viewBox="0 0 24 24"');
  });
});
