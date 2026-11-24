import { describe, expect, it } from "vitest";
import { bitsPerPixel, dvdResOf, frameOf, qualityHint, rate, ratingKey } from "./quality";

describe("quality", () => {
  it("rates by threshold", () => {
    expect([100, 85, 84.9, 65, 64.9, 40, 39.9, 0].map(rate)).toEqual(["excellent", "excellent", "good", "good", "fair", "fair", "low", "low"]);
  });
  it("computes bits per pixel", () => {
    expect(bitsPerPixel(5000, frameOf("dvd", "full", "NTSC"))).toBeCloseTo(0.483, 3);
  });
  it("picks frames", () => {
    expect(frameOf("dvd", "half", "PAL")).toEqual({ w: 352, h: 576, fps: 25 });
    expect(frameOf("dvd", "sif", "NTSC")).toEqual({ w: 352, h: 240, fps: 29.97 });
    expect(frameOf("bluray", "full", "PAL", "720p").h).toBe(720);
    expect(frameOf("avchd", "full", "PAL").w).toBe(1920);
    expect(dvdResOf("Half D1")).toBe("half");
    expect(dvdResOf("SIF")).toBe("sif");
    expect(dvdResOf("Full D1")).toBe("full");
  });
  it("selects hints", () => {
    expect(qualityHint("good", "dvd", "full", false)).toBeNull();
    expect(qualityHint("low", "dvd", "full", false)).toBe("quality.tipHalfD1");
    expect(qualityHint("fair", "dvd", "full", true)).toBe("quality.tipHalfD1Only");
    expect(qualityHint("fair", "dvd", "half", false)).toBe("quality.tipSif");
    expect(qualityHint("fair", "dvd", "sif", false)).toBe("quality.tipLowerAudio");
    expect(qualityHint("low", "dvd", "sif", false)).toBe("quality.tipFewerTitles");
    expect(qualityHint("low", "bluray", "full", false)).toBe("quality.tipBiggerDisc");
    expect(ratingKey("fair")).toBe("quality.fair");
  });
});
