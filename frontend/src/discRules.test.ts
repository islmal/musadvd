import { describe, expect, it } from "vitest";
import { ac3EncodeKbps, autoMenuAspect, snapAc3Kbps, tvMismatch } from "./discRules";

describe("tvMismatch", () => {
  it("suggests NTSC for mostly 23.976/29.97/59.94 under PAL", () => {
    expect(tvMismatch([23.976, 29.97, 25], "PAL")).toBe("NTSC");
  });
  it("suggests PAL for mostly 25/50 under NTSC", () => {
    expect(tvMismatch([25, 50, 29.97], "NTSC")).toBe("PAL");
  });
  it("is null for a tie, a match or no titles", () => {
    expect(tvMismatch([25, 29.97], "PAL")).toBeNull();
    expect(tvMismatch([25, 25], "PAL")).toBeNull();
    expect(tvMismatch([], "PAL")).toBeNull();
  });
});

describe("autoMenuAspect", () => {
  it("auto follows the majority at Full D1", () => {
    expect(autoMenuAspect(["16:9", "16:9", "4:3"], "Full D1", "auto", false)).toBe("16:9");
    expect(autoMenuAspect(["16:9", "4:3"], "Full D1", "auto", false)).toBe("4:3");
  });
  it("a forced choice wins at Full D1 only", () => {
    expect(autoMenuAspect(["4:3"], "Full D1", "16:9", false)).toBe("16:9");
    expect(autoMenuAspect(["16:9"], "Half D1", "16:9", false)).toBe("4:3");
    expect(autoMenuAspect(["16:9"], "SIF", "auto", false)).toBe("4:3");
  });
  it("Blu-ray is 4:3 here (no DVD aspect)", () => {
    expect(autoMenuAspect(["16:9"], "Full D1", "auto", true)).toBe("4:3");
  });
});


describe("snapAc3Kbps", () => {
  it("snaps to the medium's set, ties to the lower rate", () => {
    expect(snapAc3Kbps(320, "dvd")).toBe(256);
    expect(snapAc3Kbps(640, "dvd")).toBe(448);
    expect(snapAc3Kbps(128, "bd")).toBe(192);
    expect(snapAc3Kbps(96, "bd")).toBe(192);
    expect(snapAc3Kbps(500, "bd")).toBe(512);
  });
  it("keeps 0 (auto)", () => expect(snapAc3Kbps(0, "dvd")).toBe(0));
});

describe("ac3EncodeKbps", () => {
  it("auto is 192 stereo / 448 for 5+ channels", () => {
    expect(ac3EncodeKbps(2, 0, "dvd")).toBe(192);
    expect(ac3EncodeKbps(6, 0, "bd")).toBe(448);
  });
  it("a setting wins and is snapped", () => expect(ac3EncodeKbps(6, 320, "dvd")).toBe(256));
});
