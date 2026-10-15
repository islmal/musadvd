// Custom menu layouts (same JSON as `src/layout.rs`) and a generator for a first draft.

export type Fit = "stretch" | "contain" | "cover";
/** `title` = main menu (VMGM); `root` and `settings` belong to one title (its VTSM). */
export type MenuRole = "title" | "root" | "settings";
export type Align = "left" | "center" | "right";
/** Shading over the background so texts stay legible. */
export type GradientOverlay = "none" | "bottomDark" | "vignette" | "dim";

export type Action =
  | { type: "playTitle"; title: number }
  | { type: "playChapter"; title: number; chapter: number }
  | { type: "menu"; menu: string }
  | { type: "playAll" }
  /** Source stream index (ffprobe) of the audio track; root/settings menus only. */
  | { type: "setAudio"; source: number }
  /** Source stream index of the subtitle, `null` = subtitles off. */
  | { type: "setSubtitle"; source: number | null };

interface ElementBase {
  id: string;
  x: number;
  y: number;
  w: number;
  h: number;
  /** Clockwise, 0/90/180/270; buttons stay at 0 (DVD hitboxes are axis-aligned rectangles). */
  rotation: number;
  opacity: number;
  action?: Action | null;
}

export interface TextElement extends ElementBase {
  type: "text";
  text: string;
  font: string;
  size: number;
  color: string;
  align: Align;
  bold: boolean;
  italic: boolean;
  shadow: boolean;
}

export interface ImageElement extends ElementBase {
  type: "image";
  src: string;
  fit: Fit;
}

export interface VideoElement extends ElementBase {
  type: "video";
  title: number;
  startSecs?: number | null;
  fit: Fit;
}

export type LayoutElement = TextElement | ImageElement | VideoElement;

export type MenuSound = { type: "none" } | { type: "title"; title: number; startSecs?: number | null } | { type: "file"; path: string };

export interface Background {
  color: string;
  image?: string | null;
  video?: { title: number; startSecs?: number | null } | null;
  blur: number;
  gradientOverlay?: GradientOverlay;
}

export interface CustomMenu {
  id: string;
  name: string;
  role: MenuRole;
  title?: number | null;
  background: Background;
  durationSecs?: number | null;
  audio: MenuSound;
  elements: LayoutElement[];
}

export interface CustomLayout {
  version: number;
  canvas: { width: number; height: number };
  highlight: { color: string; selectColor: string; opacity: number };
  menus: CustomMenu[];
}

/** A selected audio track or subtitle of a title: source stream index and a label. */
export interface TrackRef {
  source: number;
  label: string;
}

/** A project title as the editor sees it (`EditorTitle` in src-tauri; extra fields are ignored there). */
export interface EditorTitle {
  /** 1-based title number on the disc (= position in the project's title list). */
  index: number;
  label: string;
  path: string;
  chapters: number;
  durationSecs: number;
  /** Names of the chapters from the source (null = unnamed), in chapter order. */
  chapterNames?: (string | null)[];
  /** Chapter start times in seconds (same order as `chapterNames`). */
  chapterStarts?: number[];
  /** Audio tracks that go onto the disc. */
  audioTracks?: TrackRef[];
  /** Text subtitles that go onto the disc. */
  subtitleTracks?: TrackRef[];
}

export const OVERLAY_LABELS: [GradientOverlay, string][] = [
  ["none", "Keine"],
  ["bottomDark", "Nach unten abdunkeln"],
  ["vignette", "Vignette (Ecken dunkel)"],
  ["dim", "Insgesamt abdunkeln"],
];

/** Identifies the set of videos a layout was made for (paths in disc order). */
export function titlesKey(titles: EditorTitle[]): string {
  return titles.map((t) => t.path).join("\n");
}

/** Title numbers a layout refers to (menus, backgrounds, sound, videos, actions). */
export function referencedTitles(layout: CustomLayout): Set<number> {
  const refs = layout.menus.flatMap((m) => [
    m.title ?? 0,
    m.background.video?.title ?? 0,
    m.audio.type === "title" ? m.audio.title : 0,
    ...m.elements.map((e) => (e.type === "video" ? e.title : 0)),
    ...m.elements.map((e) => (e.action && "title" in e.action ? e.action.title : 0)),
  ]);
  return new Set(refs.filter((t) => t > 0));
}

/**
 * Whether `layout` was made for other videos than `titles` (a stored layout from an earlier
 * project). With a stored `key` the video list must match exactly; for layouts without one
 * (older versions) it must refer to every current title and to no title beyond them.
 */
export function madeForOtherTitles(layout: CustomLayout, key: string | null, titles: EditorTitle[]): boolean {
  if (key !== null) return key !== titlesKey(titles);
  const refs = referencedTitles(layout);
  return [...refs].some((t) => t > titles.length) || titles.some((t) => !refs.has(t.index));
}

/** Label of chapter `n` (1-based): the source's chapter name or "Kapitel n". */
export function chapterLabel(t: EditorTitle | undefined, n: number): string {
  return t?.chapterNames?.[n - 1]?.trim() || `Kapitel ${n}`;
}

/** Whether a title needs an audio/subtitle selection menu. */
export function needsSettings(t: EditorTitle): boolean {
  return (t.audioTracks?.length ?? 0) > 1 || (t.subtitleTracks?.length ?? 0) > 0;
}

/** Fonts the renderer knows (files in %WINDIR%\Fonts, see menu.rs). */
export const FONTS = [
  "Arial",
  "Arial Black",
  "Arial Narrow",
  "Segoe UI",
  "Tahoma",
  "Verdana",
  "Trebuchet MS",
  "Georgia",
  "Times New Roman",
  "Comic Sans MS",
  "Courier New",
  "Impact",
];

export const MAX_BUTTONS = 36;

export function canvasSize(tv: string): { width: number; height: number } {
  return { width: 720, height: tv === "NTSC" ? 480 : 576 };
}

let counter = 0;
export function newId(prefix: string): string {
  counter += 1;
  return `${prefix}${Date.now().toString(36)}${counter}`;
}

export function text(x: number, y: number, w: number, h: number, value: string, extra: Partial<TextElement> = {}): TextElement {
  return {
    id: newId("t"),
    type: "text",
    text: value,
    font: "Arial",
    size: 28,
    color: "#FFFFFF",
    align: "center",
    bold: false,
    italic: false,
    shadow: true,
    x,
    y,
    w,
    h,
    rotation: 0,
    opacity: 1,
    action: null,
    ...extra,
  };
}

function video(x: number, y: number, w: number, h: number, title: number, startSecs: number | null, action: Action | null): VideoElement {
  return { id: newId("v"), type: "video", title, startSecs, fit: "cover", x, y, w, h, rotation: 0, opacity: 1, action };
}

export function emptyMenu(name: string, role: MenuRole, title: number | null = null): CustomMenu {
  return {
    id: newId("m"),
    name,
    role,
    title,
    background: { color: "#14192B", image: null, video: null, blur: 0, gradientOverlay: "none" },
    durationSecs: null,
    audio: { type: "none" },
    elements: [],
  };
}

export function isButton(e: LayoutElement): boolean {
  return !!e.action;
}

/** Menus that move (video elements, video background or sound) – they are encoded as loops. */
export function isMoving(m: CustomMenu): boolean {
  return !!m.background.video || m.audio.type !== "none" || m.elements.some((e) => e.type === "video");
}

/**
 * A first draft for the project's actual titles (addressed by their 1-based `index`):
 * - title menu pages: per title a video button (its clip, plays it) and a caption with its
 *   label (6 per page), "Alle abspielen", page arrows;
 * - per title with chapters: chapter pages (6 per page) with a moving thumbnail starting at
 *   each chapter's start and the chapter name below (both play the chapter), over the title's
 *   blurred clip darkened towards the bottom;
 * - per title with several audio tracks or subtitles: an "Einstellungen" menu with one button
 *   per track and subtitle (plus "Aus"), linked from the title's root menu.
 */
export function draftLayout(titles: EditorTitle[], tv: string, heading: string): CustomLayout {
  const canvas = canvasSize(tv);
  const { width: W, height: H } = canvas;
  const sy = H / 576;
  const Y = (v: number) => Math.round(v * sy);
  const menus: CustomMenu[] = [];
  const perPage = 6;
  const titlePages = Math.max(1, Math.ceil(titles.length / perPage));
  // A title gets VTSM menus when it has chapters to choose or streams to select.
  const rootIds = titles.map((t) => (t.chapters > 1 || needsSettings(t) ? newId("m") : null));

  const pageMenus: CustomMenu[] = [];
  for (let p = 0; p < titlePages; p++) {
    pageMenus.push(emptyMenu(titlePages > 1 ? `Hauptmenü ${p + 1}` : "Hauptmenü", "title"));
  }
  pageMenus.forEach((menu, p) => {
    menu.elements.push(text(60, Y(36), W - 120, Y(56), heading || "DVD", { size: 40, bold: true }));
    const slice = titles.slice(p * perPage, (p + 1) * perPage);
    slice.forEach((t, i) => {
      const n = t.index;
      const x = 60 + (i % 3) * 210;
      const y = Y(120 + Math.floor(i / 3) * 180);
      menu.elements.push(video(x, y, 180, Y(112), n, null, { type: "playTitle", title: n }));
      const root = rootIds[p * perPage + i];
      const caption = root ? `${t.label} · ${t.chapters > 1 ? "Kapitel" : "Menü"}` : t.label;
      menu.elements.push(text(x, y + Y(118), 180, Y(30), caption, { size: 18, action: root ? { type: "menu", menu: root } : null }));
    });
    const bottom = Y(490);
    if (titles.length > 1 && p === 0) {
      menu.elements.push(text(60, bottom, 220, Y(36), "Alle abspielen", { size: 22, align: "left", action: { type: "playAll" } }));
    }
    if (p > 0) {
      menu.elements.push(text(300, bottom, 140, Y(36), "‹ Zurück", { size: 22, action: { type: "menu", menu: pageMenus[p - 1].id } }));
    }
    if (p < titlePages - 1) {
      menu.elements.push(text(W - 220, bottom, 160, Y(36), "Weiter ›", { size: 22, align: "right", action: { type: "menu", menu: pageMenus[p + 1].id } }));
    }
  });
  menus.push(...pageMenus);

  titles.forEach((t, i) => {
    const rootId = rootIds[i];
    if (!rootId) return;
    const n = t.index;
    const main = pageMenus[Math.floor(i / perPage)].id;
    const settingsId = needsSettings(t) ? newId("m") : null;
    const shaded = { color: "#14192B", image: null, video: { title: n, startSecs: null }, blur: 14 };

    // Root menu pages: chapters with moving thumbnails (or just the buttons below).
    const perChapterPage = 6;
    const pages = t.chapters > 1 ? Math.ceil(t.chapters / perChapterPage) : 1;
    const rootMenus: CustomMenu[] = [];
    for (let p = 0; p < pages; p++) {
      const name = t.chapters > 1 ? (pages > 1 ? `${t.label} – Kapitel ${p + 1}` : `${t.label} – Kapitel`) : `${t.label} – Menü`;
      const m = emptyMenu(name, "root", n);
      if (p === 0) m.id = rootId;
      m.background = { ...shaded, gradientOverlay: "bottomDark" };
      rootMenus.push(m);
    }
    rootMenus.forEach((m, p) => {
      m.elements.push(text(60, Y(36), W - 120, Y(50), t.label, { size: 34, bold: true }));
      if (t.chapters > 1) {
        for (let c = p * perChapterPage; c < Math.min(t.chapters, (p + 1) * perChapterPage); c++) {
          const slot = c - p * perChapterPage;
          const x = 60 + (slot % 3) * 210;
          const y = Y(110 + Math.floor(slot / 3) * 170);
          const play: Action = { type: "playChapter", title: n, chapter: c + 1 };
          m.elements.push(video(x, y, 180, Y(101), n, t.chapterStarts?.[c] ?? null, play));
          m.elements.push(text(x, y + Y(105), 180, Y(30), chapterLabel(t, c + 1), { size: 18, action: play }));
        }
      }
      // Bottom row: play, settings, main menu, page arrows.
      const bottom = Y(490);
      const row: TextElement[] = [text(60, bottom, 170, Y(36), "Film abspielen", { size: 20, align: "left", action: { type: "playTitle", title: n } })];
      if (settingsId) row.push(text(240, bottom, 160, Y(36), "Einstellungen", { size: 20, action: { type: "menu", menu: settingsId } }));
      row.push(text(410, bottom, 130, Y(36), "Hauptmenü", { size: 20, action: { type: "menu", menu: main } }));
      if (p > 0) row.push(text(550, bottom, 50, Y(36), "‹", { size: 24, action: { type: "menu", menu: rootMenus[p - 1].id } }));
      if (p < pages - 1) row.push(text(610, bottom, 50, Y(36), "›", { size: 24, action: { type: "menu", menu: rootMenus[p + 1].id } }));
      m.elements.push(...row);
    });
    menus.push(...rootMenus);

    // Settings menu: audio tracks left, subtitles (with "Aus") right.
    if (settingsId) {
      const s = emptyMenu(`${t.label} – Einstellungen`, "settings", n);
      s.id = settingsId;
      s.background = { ...shaded, gradientOverlay: "dim" };
      s.elements.push(text(60, Y(36), W - 120, Y(50), `${t.label} – Einstellungen`, { size: 32, bold: true }));
      const rows = 8;
      const step = Y(38);
      const top = Y(150);
      const audio = t.audioTracks ?? [];
      if (audio.length > 0) {
        s.elements.push(text(60, Y(110), 300, Y(32), "Tonspur", { size: 22, bold: true, align: "left", color: "#FFD200" }));
        audio.slice(0, rows).forEach((a, k) => {
          s.elements.push(text(60, top + k * step, 300, Y(34), a.label, { size: 20, align: "left", action: { type: "setAudio", source: a.source } }));
        });
      }
      const subs = t.subtitleTracks ?? [];
      if (subs.length > 0) {
        s.elements.push(text(380, Y(110), 280, Y(32), "Untertitel", { size: 22, bold: true, align: "left", color: "#FFD200" }));
        const entries: [string, number | null][] = [["Aus", null], ...subs.map((x): [string, number | null] => [x.label, x.source])];
        // One column of 8, a second (narrower) one for more.
        const twoColumns = entries.length > rows;
        entries.slice(0, rows * 2).forEach(([label, source], k) => {
          const col = Math.floor(k / rows);
          const x = twoColumns ? 380 + col * 145 : 380;
          const w = twoColumns ? 135 : 280;
          s.elements.push(text(x, top + (k % rows) * step, w, Y(34), label, { size: 20, align: "left", action: { type: "setSubtitle", source } }));
        });
      }
      const bottom = Y(490);
      s.elements.push(text(60, bottom, 170, Y(36), "‹ Zurück", { size: 20, align: "left", action: { type: "menu", menu: rootId } }));
      s.elements.push(text(240, bottom, 160, Y(36), "Film abspielen", { size: 20, action: { type: "playTitle", title: n } }));
      s.elements.push(text(410, bottom, 130, Y(36), "Hauptmenü", { size: 20, action: { type: "menu", menu: main } }));
      menus.push(s);
    }
  });

  return { version: 1, canvas, highlight: { color: "#FFD200", selectColor: "#FF6A00", opacity: 0.8 }, menus };
}
