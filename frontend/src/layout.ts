// Custom menu layouts (same JSON as `src/layout.rs`) and a generator for a first draft.

export type Fit = "stretch" | "contain" | "cover";
export type MenuRole = "title" | "root";
export type Align = "left" | "center" | "right";

export type Action =
  | { type: "playTitle"; title: number }
  | { type: "playChapter"; title: number; chapter: number }
  | { type: "menu"; menu: string }
  | { type: "playAll" };

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
}

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

export function emptyMenu(name: string, role: MenuRole, title: number | null = null): CustomMenu {
  return {
    id: newId("m"),
    name,
    role,
    title,
    background: { color: "#14192B", image: null, video: null, blur: 0 },
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
 * A first draft for the project's actual titles: title menu pages with a video button (the
 * title's clip, plays it) and a caption with its label per title (6 per page), "Alle
 * abspielen", and per title with more than one chapter a root menu with one button per real
 * chapter (source chapter names where present; 8 per page), "Film abspielen" and "Hauptmenü".
 * Titles are addressed by their `index` (1-based disc title number).
 */
export function draftLayout(titles: EditorTitle[], tv: string, heading: string): CustomLayout {
  const canvas = canvasSize(tv);
  const { width: W, height: H } = canvas;
  const sy = H / 576;
  const menus: CustomMenu[] = [];
  const perPage = 6;
  const titlePages = Math.max(1, Math.ceil(titles.length / perPage));
  const rootIds = titles.map((t) => (t.chapters > 1 ? newId("m") : null));

  const pageMenus: CustomMenu[] = [];
  for (let p = 0; p < titlePages; p++) {
    pageMenus.push(emptyMenu(titlePages > 1 ? `Hauptmenü ${p + 1}` : "Hauptmenü", "title"));
  }
  pageMenus.forEach((menu, p) => {
    menu.elements.push(text(60, Math.round(36 * sy), W - 120, Math.round(56 * sy), heading || "DVD", { size: 40, bold: true }));
    const slice = titles.slice(p * perPage, (p + 1) * perPage);
    slice.forEach((t, i) => {
      const n = t.index;
      const col = i % 3;
      const row = Math.floor(i / 3);
      const x = 60 + col * 210;
      const y = Math.round((120 + row * 180) * sy);
      menu.elements.push({
        id: newId("v"),
        type: "video",
        title: n,
        startSecs: null,
        fit: "cover",
        x,
        y,
        w: 180,
        h: Math.round(112 * sy),
        rotation: 0,
        opacity: 1,
        action: { type: "playTitle", title: n },
      });
      const root = rootIds[p * perPage + i];
      menu.elements.push(
        text(x, y + Math.round(118 * sy), 180, Math.round(30 * sy), root ? `${t.label} · Kapitel` : t.label, {
          size: 18,
          action: root ? { type: "menu", menu: root } : null,
        }),
      );
    });
    const bottom = Math.round(490 * sy);
    if (titles.length > 1 && p === 0) {
      menu.elements.push(text(60, bottom, 220, Math.round(36 * sy), "Alle abspielen", { size: 22, align: "left", action: { type: "playAll" } }));
    }
    if (p > 0) {
      menu.elements.push(text(300, bottom, 140, Math.round(36 * sy), "‹ Zurück", { size: 22, action: { type: "menu", menu: pageMenus[p - 1].id } }));
    }
    if (p < titlePages - 1) {
      menu.elements.push(text(W - 220, bottom, 160, Math.round(36 * sy), "Weiter ›", { size: 22, align: "right", action: { type: "menu", menu: pageMenus[p + 1].id } }));
    }
  });
  menus.push(...pageMenus);

  titles.forEach((t, i) => {
    const rootId = rootIds[i];
    if (!rootId) return;
    const n = t.index;
    const perChapterPage = 8;
    const pages = Math.ceil(t.chapters / perChapterPage);
    const chapterMenus: CustomMenu[] = [];
    for (let p = 0; p < pages; p++) {
      const m = emptyMenu(pages > 1 ? `${t.label} – Kapitel ${p + 1}` : `${t.label} – Kapitel`, "root", n);
      if (p === 0) m.id = rootId;
      m.background = { color: "#14192B", image: null, video: { title: n, startSecs: null }, blur: 14 };
      chapterMenus.push(m);
    }
    const main = pageMenus[Math.floor(i / perPage)].id;
    chapterMenus.forEach((m, p) => {
      m.elements.push(text(60, Math.round(36 * sy), W - 120, Math.round(50 * sy), t.label, { size: 34, bold: true }));
      for (let c = p * perChapterPage; c < Math.min(t.chapters, (p + 1) * perChapterPage); c++) {
        const row = c - p * perChapterPage;
        m.elements.push(
          text(80, Math.round((110 + row * 42) * sy), 360, Math.round(36 * sy), chapterLabel(t, c + 1), {
            size: 22,
            align: "left",
            action: { type: "playChapter", title: n, chapter: c + 1 },
          }),
        );
      }
      const bottom = Math.round(490 * sy);
      m.elements.push(text(60, bottom, 200, Math.round(36 * sy), "Film abspielen", { size: 22, align: "left", action: { type: "playTitle", title: n } }));
      m.elements.push(text(270, bottom, 150, Math.round(36 * sy), "Hauptmenü", { size: 22, action: { type: "menu", menu: main } }));
      if (p > 0) {
        m.elements.push(text(430, bottom, 110, Math.round(36 * sy), "‹ Zurück", { size: 22, action: { type: "menu", menu: chapterMenus[p - 1].id } }));
      }
      if (p < pages - 1) {
        m.elements.push(text(W - 170, bottom, 110, Math.round(36 * sy), "Weiter ›", { size: 22, align: "right", action: { type: "menu", menu: chapterMenus[p + 1].id } }));
      }
    });
    menus.push(...chapterMenus);
  });

  return { version: 1, canvas, highlight: { color: "#FFD200", selectColor: "#FF6A00", opacity: 0.8 }, menus };
}
