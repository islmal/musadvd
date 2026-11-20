import { describe, expect, it } from "vitest";
import {
  applyStyleToSimilar,
  canvasSize,
  chapterLabel,
  cleanTitle,
  draftLayout,
  emptyMenu,
  isBdFrame,
  madeForOtherTitles,
  menuGroup,
  pixelAspectOf,
  referencedTitles,
  syncLayoutWithTitles,
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
    const l = draftLayout(titles, "PAL", "Disc", undefined, { theme: "classic" });
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

const tracked = (index: number, audio: number, subs: number, extra: Partial<EditorTitle> = {}): EditorTitle =>
  title(index, `t${index}.mkv`, {
    audioTracks: Array.from({ length: audio }, (_, k) => ({ source: k + 1, label: `A${k + 1}` })),
    subtitleTracks: Array.from({ length: subs }, (_, k) => ({ source: 10 + k, label: `S${k + 1}` })),
    ...extra,
  });

/** Every menu link resolves, every setGlobal* track is within `audio` / `subs`, every title ref exists. */
function expectConsistent(l: CustomLayout, n: number, audio = 0, subs = 0): void {
  const ids = new Set(l.menus.map((m) => m.id));
  expect(ids.size).toBe(l.menus.length);
  for (const m of l.menus) {
    expect(m.group).toBeTruthy();
    for (const e of m.elements) {
      const a = e.action;
      if (a?.type === "menu") expect(ids.has(a.menu)).toBe(true);
      if (a?.type === "setGlobalAudio") expect(a.track >= 1 && a.track <= audio).toBe(true);
      if (a?.type === "setGlobalSubtitle" && a.track !== null) expect(a.track >= 1 && a.track <= subs).toBe(true);
      if (a && "title" in a) expect(a.title >= 1 && a.title <= n).toBe(true);
    }
  }
  expect([...referencedTitles(l)].every((t) => t <= n)).toBe(true);
}

const elements = (l: CustomLayout, type: string) => l.menus.flatMap((m) => m.elements.filter((e) => e.type === type));
const actions = (m: { elements: LayoutElement[] }) => m.elements.map((e) => e.action?.type);
const isIcon = (e: LayoutElement, name: string) => e.type === "icon" && e.icon === name;

describe("landing draft", () => {
  it("builds a single title with Play, Chapters, key art and per-title settings", () => {
    const t = tracked(1, 2, 1, { chapters: 4 });
    const l = draftLayout([t], "PAL", "Film");
    expect(l.theme).toBe("landing");
    expect(l.settingsMode).toBe("perTitle");
    const main = l.menus[0];
    expect(menuGroup(main)).toBe("main");
    expect(main.elements[0]).toMatchObject({ type: "text", text: "Film", bold: true });
    expect(actions(main).filter((a) => a === "playTitle")).toHaveLength(1);
    expect(main.elements.some((e) => e.type === "rect" && e.borderWidth)).toBe(true);
    const art = main.elements.find((e) => e.type === "video");
    expect(art?.type === "video" && art.title).toBe(1);
    expect(art?.action).toBeFalsy();
    expect(l.menus.filter((m) => m.role === "title")).toHaveLength(1);
    const settings = l.menus.filter((m) => m.role === "settings");
    expect(settings).toHaveLength(1);
    expect(main.elements.some((e) => e.action?.type === "menu" && e.action.menu === settings[0].id)).toBe(true);
    expect(elements(l, "icon").length).toBeGreaterThan(0);
    expectConsistent(l, 1);
  });

  it("builds 3 titles with episodes and one global settings menu", () => {
    const ts = [tracked(1, 3, 2, { chapters: 3 }), tracked(2, 2, 4), tracked(3, 1, 0)];
    const l = draftLayout(ts, "PAL", "Show");
    expect(l.settingsMode).toBe("global");
    expect(l.menus.filter((m) => m.role === "settings")).toHaveLength(0);
    const groups = l.menus.map(menuGroup);
    expect(groups.filter((g) => g === "settings")).toHaveLength(1);
    expect(groups.filter((g) => g === "episodes")).toHaveLength(1);
    const g = l.menus.find((m) => menuGroup(m) === "settings")!;
    expect(g.role).toBe("title");
    expect(g.elements.filter((e) => e.action?.type === "setGlobalAudio")).toHaveLength(3);
    expect(g.elements.filter((e) => e.action?.type === "setGlobalSubtitle")).toHaveLength(5);
    expect(g.elements.some((e) => isIcon(e, "home"))).toBe(true);
    for (const m of l.menus.filter((x) => x.role === "root")) {
      expect(m.elements.some((e) => e.action?.type === "menu" && e.action.menu === g.id)).toBe(false);
    }
    expect(l.menus.filter((m) => m.role === "root").map((m) => m.title)).toEqual([1]);
    expect(actions(l.menus[0])).toContain("playAll");
    expect(l.menus[0].elements.some((e) => e.action?.type === "menu" && e.action.menu === g.id)).toBe(true);
    expectConsistent(l, 3, 3, 4);
  });

  it("paginates 24 titles over four episodes pages with icon arrows", () => {
    const ts = Array.from({ length: 24 }, (_, i) => tracked(i + 1, 1 + (i % 3), i % 5, { chapters: i === 0 ? 8 : 1 }));
    const l = draftLayout(ts, "PAL", "Series");
    const pages = l.menus.filter((m) => menuGroup(m) === "episodes");
    expect(pages).toHaveLength(4);
    pages.forEach((p, i) => {
      expect(p.elements.filter((e) => e.type === "video")).toHaveLength(6);
      expect(p.elements.some((e) => isIcon(e, "prev"))).toBe(i > 0);
      expect(p.elements.some((e) => isIcon(e, "next"))).toBe(i < 3);
      expect(p.elements.some((e) => isIcon(e, "home") && e.action?.type === "menu" && e.action.menu === l.menus[0].id)).toBe(true);
    });
    expect(l.menus.filter((m) => menuGroup(m) === "settings")).toHaveLength(1);
    expect(l.menus.filter((m) => m.role === "settings")).toHaveLength(0);
    const chapterPages = l.menus.filter((m) => menuGroup(m) === "chapters");
    expect(chapterPages).toHaveLength(2);
    expect(chapterPages[0].elements.some((e) => e.type === "text" && e.text === "›")).toBe(false);
    expect(chapterPages[0].elements.some((e) => isIcon(e, "next"))).toBe(true);
    expectConsistent(l, 24, 3, 4);
  });

  it("reproduces per-title settings in perTitle mode", () => {
    const ts = [tracked(1, 2, 0), tracked(2, 1, 1), tracked(3, 1, 0)];
    const l = draftLayout(ts, "PAL", "Show", undefined, { settingsMode: "perTitle" });
    const settings = l.menus.filter((m) => m.role === "settings");
    expect(settings.map((m) => m.title)).toEqual([1, 2]);
    expect(settings.every((m) => menuGroup(m) === "settings")).toBe(true);
    expect(elements(l, "text").some((e) => e.action?.type === "setAudio")).toBe(true);
    expect(elements(l, "text").some((e) => e.action?.type === "setGlobalAudio")).toBe(false);
    const ids = new Set(settings.map((m) => m.id));
    expect(l.menus[0].elements.some((e) => e.action?.type === "menu" && ids.has(e.action.menu))).toBe(false);
    expectConsistent(l, 3);
  });

  it("offers a global settings menu for a single title when asked", () => {
    const l = draftLayout([tracked(1, 2, 0)], "PAL", "Solo", undefined, { settingsMode: "global" });
    expect(l.menus.filter((m) => menuGroup(m) === "settings")).toHaveLength(1);
    expectConsistent(l, 1, 2, 0);
  });

  it("scales the key art without forcing 16:9 on Blu-ray", () => {
    const l = draftLayout([tracked(1, 1, 0), tracked(2, 1, 0)], "BD1080", "Disc");
    expect(l.canvas).toEqual({ width: 1920, height: 1080 });
    const art = l.menus[0].elements.find((e) => e.type === "video")!;
    expect(art.w).toBeGreaterThan(400);
    const icon = l.menus.find((m) => menuGroup(m) === "episodes")!.elements.find((e) => e.type === "icon")!;
    expect(icon.w).toBe(icon.h);
    expect(l.menus.every((m) => m.elements.every((e) => e.x >= 0 && e.x + e.w <= 1920 && e.y + e.h <= 1080))).toBe(true);
  });

  it("recolours icons with a preset", () => {
    const l = draftLayout([tracked(1, 1, 0), tracked(2, 1, 0)], "PAL", "Disc", "swiss");
    const icons = elements(l, "icon");
    expect(icons.length).toBeGreaterThan(0);
    expect(icons.every((e) => e.type === "icon" && e.color === "#111111")).toBe(true);
  });
});

describe("classic draft", () => {
  it("keeps the 6-thumbnail grid with text navigation and per-title settings", () => {
    const ts = Array.from({ length: 7 }, (_, i) => tracked(i + 1, i === 1 ? 2 : 1, 0));
    const l = draftLayout(ts, "PAL", "Disc", undefined, { theme: "classic" });
    expect(l.theme).toBe("classic");
    expect(l.settingsMode).toBe("perTitle");
    const pages = l.menus.filter((m) => m.role === "title");
    expect(pages).toHaveLength(2);
    expect(pages.every((m) => menuGroup(m) === "main")).toBe(true);
    expect(pages[0].elements.filter((e) => e.type === "video")).toHaveLength(6);
    expect(actions(pages[0])).toContain("playAll");
    expect(elements(l, "icon")).toHaveLength(0);
    expect(l.menus.filter((m) => m.role === "settings")).toHaveLength(1);
    expectConsistent(l, 7);
  });
});

describe("applyStyleToSimilar", () => {
  it("copies fonts, colours, highlight and background within the group only", () => {
    const l = draftLayout([tracked(1, 1, 0, { chapters: 8 }), tracked(2, 1, 0, { chapters: 8 })], "PAL", "Disc");
    const [a, b] = l.menus.filter((m) => menuGroup(m) === "chapters");
    const beforeOther = JSON.stringify(l.menus[0]);
    const geometry = () => b.elements.map((e) => [e.x, e.y, e.type === "text" ? e.text : ""]);
    const posB = geometry();
    for (const e of a.elements) {
      if (e.type === "text") Object.assign(e, { font: "Georgia", color: "#123456", shadow: false });
      if (e.type === "icon") e.color = "#654321";
    }
    a.elements.find((e) => e.action)!.highlight = { color: "#111111", selectColor: "#222222" };
    a.background.color = "#0A0A0A";
    a.background.gradientOverlay = "dim";
    expect(applyStyleToSimilar(l, a.id)).toBe(3);
    for (const e of b.elements) {
      if (e.type === "text") expect([e.font, e.color, e.shadow]).toEqual(["Georgia", "#123456", false]);
      if (e.type === "icon") expect(e.color).toBe("#654321");
      if (e.action) expect(e.highlight).toEqual({ color: "#111111", selectColor: "#222222" });
    }
    expect(b.background.color).toBe("#0A0A0A");
    expect(b.background.gradientOverlay).toBe("dim");
    expect(geometry()).toEqual(posB);
    expect(JSON.stringify(l.menus[0])).toBe(beforeOther);
    expect(applyStyleToSimilar(l, a.id)).toBe(0);
    expect(applyStyleToSimilar(l, "missing")).toBe(0);
  });
});

describe("syncLayoutWithTitles on a landing layout", () => {
  it("adds an episodes page instead of cluttering the landing screen", () => {
    const one = [tracked(1, 1, 0)];
    const l = draftLayout(one, "PAL", "Disc");
    const two = [...one, tracked(2, 1, 0)];
    const s = syncLayoutWithTitles(l, one.map((t) => t.path), two, "PAL", "Disc");
    const landing = s.menus[0];
    expect(landing.elements.filter((e) => e.type === "video" && e.action)).toHaveLength(0);
    const eps = s.menus.filter((m) => menuGroup(m) === "episodes");
    expect(eps).toHaveLength(1);
    expect(eps[0].elements.filter((e) => e.type === "video")).toHaveLength(2);
    expect(eps[0].elements.some((e) => isIcon(e, "home") && e.action?.type === "menu" && e.action.menu === landing.id)).toBe(true);
    const btn = landing.elements.find((e) => e.action?.type === "menu" && e.action.menu === eps[0].id);
    expect(btn?.type === "text" && btn.text).toBeTruthy();
    expect(actions(landing)).toContain("playAll");
    expect(actions(landing)).not.toContain("playTitle");
    expectConsistent(s, 2);
  });

  it("chains further episodes pages with icon arrows", () => {
    const one = [tracked(1, 1, 0)];
    const many = Array.from({ length: 8 }, (_, i) => tracked(i + 1, 1, 0));
    const s = syncLayoutWithTitles(draftLayout(one, "PAL", "Disc"), one.map((t) => t.path), many, "PAL", "Disc");
    const eps = s.menus.filter((m) => menuGroup(m) === "episodes");
    expect(eps).toHaveLength(2);
    expect(eps[0].elements.some((e) => isIcon(e, "next"))).toBe(true);
    expect(eps[1].elements.some((e) => isIcon(e, "prev"))).toBe(true);
    expect(s.menus[0].elements.filter((e) => e.type === "video" && e.action)).toHaveLength(0);
    expectConsistent(s, 8);
  });
});
