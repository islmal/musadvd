import { describe, expect, it } from "vitest";
import {
  canvasSize,
  chapterLabel,
  cleanTitle,
  draftLayout,
  emptyMenu,
  isBdFrame,
  madeForOtherTitles,
  pixelAspectOf,
  referencedTitles,
  remapLayoutByPaths,
  titlesKey,
  type CustomLayout,
  type EditorTitle,
  type LayoutElement,
} from "./layout";

const title = (index: number, path: string, extra: Partial<EditorTitle> = {}): EditorTitle => ({
  index,
  label: `T${index}`,
  path,
  chapters: 1,
  durationSecs: 60,
  ...extra,
});

/** A layout whose main menu has one button per entry that plays that title. */
function layoutPlaying(...refs: number[]): CustomLayout {
  const menu = emptyMenu("Main", "title");
  refs.forEach((n, i) =>
    menu.elements.push({
      id: `b${i}`,
      type: "text",
      x: 0,
      y: i * 10,
      w: 10,
      h: 10,
      value: "x",
      action: { type: "playTitle", title: n },
    } as unknown as LayoutElement),
  );
  return { version: 1, canvas: { width: 720, height: 576 }, highlight: { color: "#fff", selectColor: "#f00", opacity: 1 }, menus: [menu] };
}

const played = (l: CustomLayout) => l.menus[0].elements.map((e) => (e.action?.type === "playTitle" ? e.action.title : 0));

describe("cleanTitle", () => {
  it("strips the extension and rip tags", () => {
    expect(cleanTitle("My.Movie.2020.1080p.BluRay.x264.mkv")).toBe("My Movie 2020");
  });
  it("strips YouTube ids but keeps ordinary brackets", () => {
    expect(cleanTitle("Holiday Video [dQw4w9WgXcQ].mp4")).toBe("Holiday Video");
    expect(cleanTitle("Film [Commentary].mkv")).toBe("Film [Commentary]");
  });
  it("falls back to the original name instead of an empty label", () => {
    expect(cleanTitle("1080p.mkv")).toBe("1080p.mkv");
  });
  it("keeps names that are already clean", () => {
    expect(cleanTitle("Summer in Spain")).toBe("Summer in Spain");
  });
});

describe("canvas helpers", () => {
  it("picks the frame per standard", () => {
    expect(canvasSize("PAL")).toEqual({ width: 720, height: 576 });
    expect(canvasSize("NTSC")).toEqual({ width: 720, height: 480 });
    expect(canvasSize("BD1080")).toEqual({ width: 1920, height: 1080 });
    expect(isBdFrame("BD720")).toBe(true);
    expect(isBdFrame("PAL")).toBe(false);
  });
  it("uses a square pixel on Blu-ray and the 4:3 TV pixel on DVD", () => {
    expect(pixelAspectOf("BD1080", canvasSize("BD1080"))).toBe(1);
    expect(pixelAspectOf("PAL", canvasSize("PAL"))).toBeCloseTo(1.0667, 3);
  });
});

describe("title references", () => {
  it("collects referenced titles once", () => {
    expect([...referencedTitles(layoutPlaying(2, 3, 2))].sort()).toEqual([2, 3]);
  });
  it("builds a key from the paths in disc order", () => {
    expect(titlesKey([title(1, "a.mkv"), title(2, "b.mkv")])).toBe("a.mkv\nb.mkv");
  });
  it("remaps by path after a reorder", () => {
    const l = layoutPlaying(1, 2);
    expect(remapLayoutByPaths(l, ["a", "b"], ["b", "a"])).toBe(true);
    expect(played(l)).toEqual([2, 1]);
  });
  it("reports failure when a referenced title was removed", () => {
    expect(remapLayoutByPaths(layoutPlaying(2), ["a", "b"], ["a"])).toBe(false);
  });
});

describe("madeForOtherTitles", () => {
  const titles = [title(1, "a"), title(2, "b")];
  it("accepts the same list and a reordering", () => {
    expect(madeForOtherTitles(layoutPlaying(1, 2), titlesKey(titles), titles)).toBe(false);
    expect(madeForOtherTitles(layoutPlaying(1, 2), "b\na", titles)).toBe(false);
  });
  it("rejects other videos and references beyond the list", () => {
    expect(madeForOtherTitles(layoutPlaying(1, 2), "a\nc", titles)).toBe(true);
    expect(madeForOtherTitles(layoutPlaying(1, 3), null, titles)).toBe(true);
  });
  it("without a key every title must be referenced", () => {
    expect(madeForOtherTitles(layoutPlaying(1), null, titles)).toBe(true);
    expect(madeForOtherTitles(layoutPlaying(1, 2), null, titles)).toBe(false);
  });
});

describe("chapterLabel", () => {
  it("prefers the trimmed source name and falls back to the numbered label", () => {
    const t = title(1, "a", { chapterNames: [" Intro ", " ", null] });
    expect(chapterLabel(t, 1)).toBe("Intro");
    expect(chapterLabel(t, 2)).toBe(chapterLabel(undefined, 2));
    expect(chapterLabel(t, 3)).toBe(chapterLabel(undefined, 3));
  });
});

describe("draftLayout", () => {
  const titles = Array.from({ length: 8 }, (_, i) => title(i + 1, `v${i}.mkv`));
  it("paginates six titles per main menu", () => {
    const l = draftLayout(titles, "PAL", "Disc");
    expect(l.menus.filter((m) => m.role === "title")).toHaveLength(2);
    expect(l.canvas).toEqual({ width: 720, height: 576 });
  });
  it("only references titles that exist", () => {
    const l = draftLayout(titles, "NTSC", "Disc");
    expect(l.canvas.height).toBe(480);
    expect([...referencedTitles(l)].every((n) => n >= 1 && n <= titles.length)).toBe(true);
  });
  it("scales to the Blu-ray frame", () => {
    expect(draftLayout(titles.slice(0, 2), "BD1080", "Disc").canvas).toEqual({ width: 1920, height: 1080 });
  });
});
