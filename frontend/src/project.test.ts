import { describe, expect, it } from "vitest";
import { fingerprint, layoutFiles, parseProject, PROJECT_FORMAT, PROJECT_VERSION } from "./project";
import type { CustomLayout } from "./layout";

const base = { format: PROJECT_FORMAT, version: PROJECT_VERSION };

describe("parseProject", () => {
  it("rejects foreign files", () => {
    expect(() => parseProject(null)).toThrow();
    expect(() => parseProject([])).toThrow();
    expect(() => parseProject({ format: "other", version: 1 })).toThrow();
  });
  it("rejects missing, old and newer versions", () => {
    expect(() => parseProject({ format: PROJECT_FORMAT })).toThrow();
    expect(() => parseProject({ format: PROJECT_FORMAT, version: 0 })).toThrow();
    expect(() => parseProject({ format: PROJECT_FORMAT, version: PROJECT_VERSION + 1 })).toThrow();
  });
  it("fills defaults for an empty project", () => {
    expect(parseProject(base)).toMatchObject({
      disc: "dvd5",
      tvStandard: null,
      twoPass: false,
      bdVideo: "encode",
      bdAudio: "auto",
      menuMode: "custom",
      menuTitleAuto: true,
      layoutEdited: false,
      titles: [],
    });
  });
  it("falls back for unknown enum values", () => {
    const p = parseProject({ ...base, disc: "floppy", bdVideo: "x", bdAudio: "y", tvStandard: "SECAM", menuMode: "?" });
    expect([p.disc, p.bdVideo, p.bdAudio, p.tvStandard, p.menuMode]).toEqual(["dvd5", "encode", "auto", null, "custom"]);
  });
  it("keeps only usable titles and sanitises their fields", () => {
    const p = parseProject({
      ...base,
      titles: [
        null,
        { path: "" },
        { path: 5 },
        {
          path: "a.mkv",
          audio: [0, -1, 1.5, 2],
          chapters: [0, "x", 10, Infinity, -3],
          chapterNames: [" One ", "", "Two", "dropped"],
          trimStart: NaN,
          crop: { top: 1, bottom: 2, left: 3 },
          audioNames: { "1": "Main", x: "bad", "2": 7 },
          bdResolution: "4k",
          subtitleStyle: { fontSizePct: 900, outlinePx: -2, offsetMs: 5, fontFamily: "" },
          externalSubtitles: [{ path: "a.srt" }, { path: "" }, { language: "de" }],
        },
      ],
    });
    expect(p.titles).toHaveLength(1);
    const t = p.titles[0];
    expect(t.audio).toEqual([0, 2]);
    expect(t.chapters).toEqual([0, 10]);
    expect(t.chapterNames).toEqual(["One", "Two"]); // names follow their raw chapter index
    expect(t.trimStart).toBeNull();
    expect(t.crop).toBeNull();
    expect(t.audioNames).toEqual({ 1: "Main" });
    expect(t.bdResolution).toBeNull();
    expect(t.subtitleStyle).toEqual({ fontSizePct: 200, outlinePx: 0, offsetMs: 5 });
    expect(t.externalSubtitles).toEqual([{ path: "a.srt", language: null, title: null, forced: false, enabled: true }]);
  });
  it("treats a layout as edited when the flag is missing", () => {
    const layout = { version: 1, canvas: { width: 720, height: 576 }, menus: [] };
    expect(parseProject({ ...base, customLayout: layout }).layoutEdited).toBe(true);
    expect(parseProject({ ...base, customLayout: layout, layoutEdited: false }).layoutEdited).toBe(false);
    expect(parseProject({ ...base, customLayout: { menus: 1 } }).customLayout).toBeNull();
  });
  it("round-trips through JSON", () => {
    const p = parseProject({ ...base, disc: "bd25", twoPass: true, titles: [{ path: "a.mkv", label: "A" }] });
    expect(parseProject(JSON.parse(JSON.stringify(p)))).toEqual(p);
  });
});

describe("fingerprint", () => {
  it("ignores the save timestamp only", () => {
    const a = parseProject({ ...base, savedAt: "1" });
    const b = parseProject({ ...base, savedAt: "2" });
    expect(fingerprint(a)).toBe(fingerprint(b));
    expect(fingerprint(a)).not.toBe(fingerprint({ ...b, output: "x" }));
  });
});

describe("layoutFiles", () => {
  it("lists local files once and skips embedded or remote sources", () => {
    const menu = {
      id: "m",
      name: "M",
      role: "title",
      background: { color: "#000", image: "bg.png", video: { path: "bg.mp4" }, blur: 0, gradientOverlay: "none" },
      audio: { type: "file", path: "snd.mp3" },
      elements: [
        { type: "image", src: "bg.png" },
        { type: "image", src: "data:image/png;base64,AAAA" },
        { type: "image", src: "https://example.com/x.png" },
        { type: "video", path: "clip.mp4" },
        { type: "video", path: "" },
      ],
    };
    const l = { version: 1, canvas: { width: 1, height: 1 }, menus: [menu] } as unknown as CustomLayout;
    expect(layoutFiles(l).sort()).toEqual(["bg.mp4", "bg.png", "clip.mp4", "snd.mp3"]);
    expect(layoutFiles(null)).toEqual([]);
  });
});
