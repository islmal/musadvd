import { describe, expect, it } from "vitest";
import {
  afterPlaybackStep,
  applyStyleBatch,
  applyStyleToSimilar,
  copyMenuClip,
  pasteMenuClip,
  navSlot,
  startButtonId,
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

describe("menu clipboard", () => {
  it("keeps a mock-up across a redraft: pasted over the new main menu, links relinked by name", () => {
    const eps = [1, 2].map((n) => title(n, `e${n}.mkv`));
    const old = draftLayout(eps, "PAL", "Disc");
    const mock = old.menus[0];
    mock.background.color = "#F5A800";
    const episodes = old.menus.find((m) => menuGroup(m) === "episodes")!;
    mock.elements.push({ ...(mock.elements[0] as LayoutElement), id: "custom", action: { type: "menu", menu: episodes.id } });
    mock.elements.push({ ...(mock.elements[0] as LayoutElement), id: "gone", action: { type: "menu", menu: "deleted" } });
    const ind = { ...(mock.elements[0] as LayoutElement), id: "ind", indicatorFor: "custom", action: null };
    mock.elements.push(ind);
    const clip = copyMenuClip(old, mock.id)!;

    const fresh = draftLayout(eps, "PAL", "Disc");
    const target = fresh.menus[0];
    const { menu, unlinked } = pasteMenuClip(fresh, clip, { replaceId: target.id });
    expect(unlinked).toBe(1);
    expect([menu.id, menu.role, menu.background.color, fresh.menus[0]]).toEqual([target.id, "title", "#F5A800", menu]);
    const custom = menu.elements[menu.elements.length - 3];
    const newEpisodes = fresh.menus.find((m) => menuGroup(m) === "episodes")!;
    expect(custom.action).toEqual({ type: "menu", menu: newEpisodes.id });
    expect(menu.elements[menu.elements.length - 2].action).toBeNull();
    expect(menu.elements[menu.elements.length - 1].indicatorFor).toBe(custom.id);
    expect(new Set(menu.elements.map((e) => e.id)).size).toBe(menu.elements.length);
    expect(menu.elements.some((e) => e.id === "custom")).toBe(false);

    const n = fresh.menus.length;
    const added = pasteMenuClip(fresh, clip, { insertAfter: 0 }).menu;
    expect([fresh.menus.length, fresh.menus[1], added.id === target.id]).toEqual([n + 1, added, false]);
  });
});

describe("draft menu choices", () => {
  const tracks = { audioTracks: [{ source: 1, label: "EN" }, { source: 2, label: "DE" }], subtitleTracks: [{ source: 3, label: "EN" }] };
  const eps = [1, 2, 3].map((n) => title(n, `e${n}.mkv`, { chapters: 4, ...tracks }));
  const actions = (l: CustomLayout) => l.menus.flatMap((m) => m.elements.map((e) => e.action?.type));

  it("defaults build title screen, chapter menus and the global settings menu", () => {
    const l = draftLayout(eps, "PAL", "Disc");
    expect(l.menus.map(menuGroup)).toEqual(expect.arrayContaining(["main", "episodes", "chapters", "settings"]));
    expect([l.titleScreen, l.chapterMenus]).toEqual([undefined, undefined]);
  });
  it("without a title screen the grid is the main menu and links the global settings", () => {
    const l = draftLayout(eps, "PAL", "Disc", undefined, { titleScreen: false });
    expect(l.menus.some((m) => menuGroup(m) === "episodes")).toBe(false);
    const settings = l.menus.find((m) => menuGroup(m) === "settings")!;
    expect(l.menus[0].elements.some((e) => e.action?.type === "menu" && e.action.menu === settings.id)).toBe(true);
    expect(l.titleScreen).toBe(false);
  });
  it("without chapter menus no chapter grids; per-title settings keep a root page", () => {
    const l = draftLayout(eps, "PAL", "Disc", undefined, { chapterMenus: false, settingsMode: "perTitle" });
    expect(actions(l)).not.toContain("playChapter");
    expect(l.menus.filter((m) => m.role === "root")).toHaveLength(3);
    expect(l.chapterMenus).toBe(false);
    expect(actions(draftLayout(eps, "PAL", "Disc", undefined, { chapterMenus: false, settingsMode: "global" }))).not.toContain("playChapter");
    expect(draftLayout(eps, "PAL", "Disc", undefined, { chapterMenus: false, settingsMode: "global" }).menus.some((m) => m.role === "root")).toBe(false);
  });
  it("settings none builds no settings menus at all", () => {
    for (const theme of ["landing", "classic"] as const) {
      const l = draftLayout(eps, "PAL", "Disc", undefined, { theme, settingsMode: "none" });
      expect(l.menus.some((m) => m.role === "settings" || menuGroup(m) === "settings")).toBe(false);
      expect(actions(l)).not.toEqual(expect.arrayContaining(["setGlobalAudio"]));
      expect(actions(l)).not.toContain("setAudio");
    }
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

describe("batch styling, start button and after-playback", () => {
  const base = { rotation: 0, opacity: 1 };
  const txt = (id: string, y: number, extra: Record<string, unknown> = {}) =>
    ({ ...base, id, type: "text", x: 10, y, w: 100, h: 20, text: id, font: "Arial", size: 20, color: "#FFFFFF", align: "center", bold: false, italic: false, shadow: true, ...extra }) as LayoutElement;
  const ico = (id: string, name: string, x: number, menu = "m1") =>
    ({ ...base, id, type: "icon", icon: name, color: "#FFFFFF", x, y: 400, w: 40, h: 40, action: { type: "menu", menu } }) as LayoutElement;
  const vid = (id: string) => ({ ...base, id, type: "video", title: 1, fit: "cover", x: 5, y: 100, w: 150, h: 90, action: { type: "playTitle", title: 1 } }) as LayoutElement;
  const menu = (id: string, group: string, role: "title" | "root" = "title") => {
    const m = emptyMenu(id, role);
    m.id = id;
    m.group = group;
    m.elements = [txt(id + "h", 5), vid(id + "v"), txt(id + "c", 200), ico(id + "n", "next", 600), ico(id + "p", "prev", 20)];
    return m;
  };
  const mk = () => ({ ...draftLayout([], "PAL", "x"), menus: [menu("a", "episodes"), menu("b", "episodes"), menu("c", "chapters")] }) as CustomLayout;
  const none = { scope: "group", background: false, music: false, typography: false, navIcons: false, positions: false } as const;

  it("scope group vs all, and each checkbox leaves the rest alone", () => {
    const l = mk();
    const [a, b, c] = l.menus;
    a.background.color = "#101010";
    a.audio = { type: "file", path: "x.opus" };
    a.playMode = "once";
    a.timeoutSecs = 3;
    expect(applyStyleBatch(l, "a", { ...none, background: true })).toBe(1);
    expect([b.background.color, b.audio, b.playMode, b.timeoutSecs]).toEqual(["#101010", { type: "none" }, "once", 3]);
    // Music alone: the track is copied (as an own copy), nothing else changes.
    b.background.color = "#202020";
    expect(applyStyleBatch(l, "a", { ...none, music: true })).toBe(1);
    expect([b.background.color, b.audio]).toEqual(["#202020", a.audio]);
    expect(b.audio).not.toBe(a.audio);
    b.background.color = "#101010";
    expect(c.background.color).not.toBe("#101010");
    expect(applyStyleBatch(l, "a", { ...none, scope: "all", background: true })).toBe(1);
    expect(c.playMode).toBe("once");
    // typography off: fonts untouched even though background copied
    a.elements.forEach((e) => e.type === "text" && (e.font = "Georgia"));
    expect(b.elements.find((e) => e.type === "text")).toMatchObject({ font: "Arial" });
    expect(applyStyleBatch(l, "a", { ...none, typography: true })).toBe(1);
    expect(b.elements.find((e) => e.type === "text")).toMatchObject({ font: "Georgia", text: "bh", y: 5 });
    expect(applyStyleBatch(l, "missing", { ...none, background: true })).toBe(0);
    // Selected scope: only the listed menus, across groups.
    c.background.color = "#000000";
    b.background.color = "#000000";
    a.background.color = "#ABCDEF";
    expect(applyStyleBatch(l, "a", { ...none, scope: "selected", menuIds: ["c"], background: true })).toBe(1);
    expect([b.background.color, c.background.color]).toEqual(["#000000", "#ABCDEF"]);
  });

  it("nav icons copy name/colour/size around the target centre; positions move only heading and nav", () => {
    const l = mk();
    const [a, b] = l.menus;
    const na = a.elements.find((e) => e.id === "an") as LayoutElement & { icon: string; color: string };
    Object.assign(na, { icon: "arrowRight", color: "#FF0000", w: 60, h: 60, x: 500, y: 300 });
    expect(applyStyleBatch(l, "a", { ...none, navIcons: true })).toBe(1);
    const nb = b.elements.find((e) => e.id === "bn") as typeof na;
    expect([nb.icon, nb.color, nb.w, nb.h]).toEqual(["arrowRight", "#FF0000", 60, 60]);
    expect([nb.x + 30, nb.y + 30]).toEqual([620, 420]); // old centre kept
    const pb = b.elements.find((e) => e.id === "bp")!;
    expect(pb).toMatchObject({ icon: "prev", x: 20, w: 40 }); // prev slot has same source look: unchanged geometry

    a.elements.find((e) => e.id === "ah")!.y = 50;
    a.elements.find((e) => e.id === "av")!.x = 77;
    a.elements.find((e) => e.id === "ac")!.x = 88;
    expect(applyStyleBatch(l, "a", { ...none, positions: true })).toBe(1);
    expect(b.elements.find((e) => e.id === "bh")!.y).toBe(50);
    expect(nb.x).toBe(500);
    expect(b.elements.find((e) => e.id === "bv")!.x).toBe(5);
    expect(b.elements.find((e) => e.id === "bc")!.x).toBe(10);
  });

  it("navSlot maps icons with a menu action", () => {
    expect(navSlot(ico("i", "arrowRight", 0))).toBe("next");
    expect(navSlot(ico("i", "prev", 0))).toBe("prev");
    expect(navSlot(ico("i", "home", 0))).toBe("home");
    expect(navSlot(ico("i", "back", 0))).toBe("back");
    expect(navSlot(ico("i", "star", 0))).toBeNull();
    expect(navSlot({ ...ico("i", "next", 0), action: null } as LayoutElement)).toBeNull();
    expect(navSlot(txt("t", 0, { text: "→", action: { type: "menu", menu: "m" } }))).toBe("next");
    expect(navSlot(txt("t", 0))).toBeNull();
  });

  it("startButtonId is the first non-autoExecute button", () => {
    const m = emptyMenu("s", "title");
    m.elements = [txt("a", 0), txt("b", 10, { action: { type: "playAll" }, autoExecute: true }), txt("c", 20, { action: { type: "playAll" } }), txt("d", 30, { action: { type: "playAll" } })];
    expect(startButtonId(m)).toBe("c");
    expect(startButtonId(emptyMenu("e", "title"))).toBeNull();
  });

  it("afterPlaybackStep covers the five modes", () => {
    const l = mk();
    const root = emptyMenu("r", "root");
    root.id = "r";
    root.title = 2;
    l.menus.push(root);
    // title 2 has a root menu; title 1 does not; main menu of both is "a" (plays title 1 only) -> else first menu
    const ts = (mode?: EditorTitle["afterPlayback"], i = 1) => title(i, `p${i}`, { afterPlayback: mode });
    const step = (modes: (EditorTitle["afterPlayback"])[], t: number) => afterPlaybackStep(l, modes.map((m, i) => ts(m, i + 1)), t);
    expect(step([undefined, "default"], 1)).toEqual({ type: "menu", menuId: "a" });
    expect(step(["loop"], 1)).toEqual({ type: "title", title: 1 });
    expect(step(["next", "default"], 1)).toEqual({ type: "title", title: 2 });
    expect(step(["default", "next"], 2)).toEqual({ type: "menu", menuId: "a" }); // last title -> main menu
    expect(step(["mainMenu"], 1)).toEqual({ type: "menu", menuId: "a" });
    expect(step(["titleMenu", "titleMenu"], 2)).toEqual({ type: "menu", menuId: "r" });
    expect(step(["titleMenu"], 1)).toEqual({ type: "menu", menuId: "a" }); // no root -> main
    expect(afterPlaybackStep({ ...l, menus: [] }, [ts()], 1)).toEqual({ type: "none" });
  });
});
