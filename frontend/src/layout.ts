import type { ButtonNav } from "./api";
import { t as tr } from "./i18n";
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
  | { type: "menu"; menu: string; transition?: TransitionVideo | null }
  | { type: "playAll" }
  /** Source stream index (ffprobe) of the audio track; root/settings menus only. */
  | { type: "setAudio"; source: number }
  /** Source stream index of the subtitle, `null` = subtitles off. */
  | { type: "setSubtitle"; source: number | null }
  /** Custom conditional VM script (e.g. "if (g4 == 1) link button 2; else jump menu 3;"). */
  | { type: "customVm"; script: string };

export type VideoAssetSource =
  | { source: "title"; title: number; startSecs?: number | null }
  | { source: "file"; path: string; inPointSecs?: number | null };

export interface TransitionVideo {
  video: VideoAssetSource;
  durationSecs: number;
  audio?: MenuSound;
}

export interface ElementKey {
  t: number;
  x: number;
  y: number;
  w: number;
  h: number;
  opacity: number;
}

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
  /** Own button colours (marked / activated); null = the layout's. At most 3 pairs per menu. */
  highlight?: { color: string; selectColor: string } | null;
  /** Editor-only grouping: elements sharing a value move together; ignored by the build. */
  group?: string | null;
  /** Intro animation keyframes. */
  intro?: ElementKey[];
  /** Hidden Easter Egg: Button hitbox and navigation active, but subpicture highlight is 100% transparent. */
  easterEgg?: boolean;
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
  /** Break lines at the box width (otherwise only at line feeds). */
  wrap?: boolean;
}

export interface ImageElement extends ElementBase {
  type: "image";
  src: string;
  fit: Fit;
}

export interface VideoElement extends ElementBase {
  type: "video";
  /** 1-based project title; ignored (0) when `path` is set. */
  title: number;
  startSecs?: number | null;
  fit: Fit;
  /** External video file instead of a title. */
  path?: string;
}

/** A filled box, optionally framed; a decoration unless it has an action. */
export interface RectElement extends ElementBase {
  type: "rect";
  color: string;
  borderColor?: string;
  /** Canvas units, 0..100; 0 / omitted = no border. */
  borderWidth?: number;
}

export type LayoutElement = TextElement | ImageElement | VideoElement | RectElement;

export type MenuSound = { type: "none" } | { type: "title"; title: number; startSecs?: number | null } | { type: "file"; path: string };

export interface Background {
  color: string;
  image?: string | null;
  /** A project title, or an external file when `path` is set (then `title` is ignored). */
  video?: { title: number; startSecs?: number | null; path?: string } | null;
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
  introSecs?: number | null;
  elements: LayoutElement[];
}

export type FastPlayMenuTarget = "vtsmRoot" | "vmgmMain";

export interface FastPlayConfig {
  enabled: boolean;
  timeoutSecs: number;
  targetTitle: number;
  menuTarget: FastPlayMenuTarget;
  splash?: Background | null;
  audio?: MenuSound;
}

export interface CustomLayout {
  version: number;
  canvas: { width: number; height: number };
  highlight: { color: string; selectColor: string; opacity: number };
  fastPlay?: FastPlayConfig | null;
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
  /** Source time of the menu clips: where the build starts them (trim start + 10 %, at most 5 min). */
  clipStartSecs?: number;
  /** Audio tracks that go onto the disc. */
  audioTracks?: TrackRef[];
  /** Text subtitles that go onto the disc. */
  subtitleTracks?: TrackRef[];
}

export const OVERLAY_LABELS: [GradientOverlay, string][] = [
  ["none", tr("layout.keine")],
  ["bottomDark", tr("layout.nachUntenAbdunkeln")],
  ["vignette", tr("layout.vignetteEckenDunkel")],
  ["dim", tr("layout.insgesamtAbdunkeln")],
];

/** Identifies the set of videos a layout was made for (paths in disc order). */
export function titlesKey(titles: EditorTitle[]): string {
  return titles.map((t) => t.path).join("\n");
}

/** Title numbers a layout refers to (menus, backgrounds, sound, videos, actions). */
export function referencedTitles(layout: CustomLayout): Set<number> {
  const refs = layout.menus.flatMap((m) => [
    m.title ?? 0,
    m.background.video?.path ? 0 : (m.background.video?.title ?? 0),
    m.audio.type === "title" ? m.audio.title : 0,
    ...m.elements.map((e) => (e.type === "video" && !e.path ? e.title : 0)),
    ...m.elements.map((e) => (e.action && "title" in e.action ? e.action.title : 0)),
  ]);
  return new Set(refs.filter((t) => t > 0));
}

/**
 * Remaps all 1-based title references in a custom layout using a mapping (oldTitle -> newTitle).
 * Used when titles are reordered or moved, preserving custom menus across title list edits.
 */
export function remapLayoutTitles(layout: CustomLayout, mapFn: (oldTitle: number) => number | undefined): void {
  for (const m of layout.menus) {
    if (typeof m.title === "number") {
      const next = mapFn(m.title);
      if (next !== undefined) m.title = next;
    }
    if (m.background.video && !m.background.video.path && typeof m.background.video.title === "number") {
      const next = mapFn(m.background.video.title);
      if (next !== undefined) m.background.video.title = next;
    }
    if (m.audio.type === "title" && typeof m.audio.title === "number") {
      const next = mapFn(m.audio.title);
      if (next !== undefined) m.audio.title = next;
    }
    for (const e of m.elements) {
      if (e.type === "video" && !e.path && typeof e.title === "number") {
        const next = mapFn(e.title);
        if (next !== undefined) e.title = next;
      }
      if (e.action && "title" in e.action && typeof e.action.title === "number") {
        const next = mapFn(e.action.title);
        if (next !== undefined) e.action.title = next;
      }
    }
  }
}

/**
 * Re-maps a custom layout when titles are reordered or removed, based on their file paths.
 * Returns true if remapping succeeded for all referenced titles.
 */
export function remapLayoutByPaths(layout: CustomLayout, oldPaths: string[], newPaths: string[]): boolean {
  const map = new Map<number, number>();
  for (let i = 0; i < oldPaths.length; i++) {
    const newIdx = newPaths.indexOf(oldPaths[i]);
    if (newIdx >= 0) {
      map.set(i + 1, newIdx + 1);
    }
  }
  remapLayoutTitles(layout, (oldTitle) => map.get(oldTitle));
  const refs = referencedTitles(layout);
  return [...refs].every((t) => t >= 1 && t <= newPaths.length);
}

/**
 * Whether `layout` was made for other videos than `titles` (a stored layout from an earlier
 * project). With a stored `key` the video list must match exactly (or be a reordering of the same videos);
 * for layouts without one (older versions) it must refer to every current title and to no title beyond them.
 */
export function madeForOtherTitles(layout: CustomLayout, key: string | null, titles: EditorTitle[]): boolean {
  const refs = referencedTitles(layout);
  if ([...refs].some((t) => t > titles.length)) return true;
  if (key !== null) {
    if (key === titlesKey(titles)) return false;
    const oldPaths = key.split("\n");
    const currentPaths = titles.map((t) => t.path);
    if (oldPaths.length === currentPaths.length && oldPaths.every((p) => currentPaths.includes(p))) {
      return false;
    }
    return true;
  }
  return titles.some((t) => !refs.has(t.index));
}

/** Label of chapter `n` (1-based): the source's chapter name or "Kapitel n". */
export function chapterLabel(t: EditorTitle | undefined, n: number): string {
  return t?.chapterNames?.[n - 1]?.trim() || tr("layout.kapitel", { n });
}

const RIP_TAGS = "2160p|1080p|720p|480p|4k|x264|x265|h264|h265|hevc|avc|aac|ac3|dts|webrip|web-dl|webdl|bluray|brrip|bdrip|dvdrip|hdtv|hdrip|10bit|remux";
const RIP_TAG_RE = new RegExp(`(^|[\\s._\\-\\[(])(?:${RIP_TAGS})(?=$|[\\s._\\-\\])])`, "gi");

/** Readable default caption from a file name: no extension, archive/YouTube ids or rip tags; never empty. */
export function cleanTitle(name: string): string {
  const base = name.trim();
  let s = base.replace(/\.(mkv|mp4|avi|mov|m4v|wmv|flv|webm|ts|m2ts|mpg|mpeg|vob|iso)$/i, "");
  // shortcut: an 11-char bracket needs a digit to count as a YouTube id, so "[Commentary]" survives.
  s = s.replace(/\s*\[(?:-?\d[\d_]*|(?=[^\]]*\d)[A-Za-z0-9_-]{11})\]/g, "").replace(/\s*\(\d{5,}\)/g, "");
  if (!/\s/.test(s)) s = s.replace(/[_.]+/g, " ");
  // Run twice: adjacent tags share their separator, which the first match consumes.
  s = s.replace(RIP_TAG_RE, "$1").replace(RIP_TAG_RE, "$1");
  s = s.replace(/[[(]\s*[\])]/g, "").replace(/\s+/g, " ").replace(/^[\s\-_.|]+|[\s\-_.|]+$/g, "");
  return s || base;
}

/** Font size shrunk (not below 55 %) so `value` fits `width`; sizes stay untouched when it already fits. */
function fitSize(value: string, size: number, width: number, font: string, bold: boolean): number {
  let w = value.length * size * 0.55; // shortcut: rough estimate without a DOM (tests), canvas below is exact
  if (typeof document !== "undefined") {
    const ctx = document.createElement("canvas").getContext("2d");
    if (ctx) {
      ctx.font = `${bold ? "bold " : ""}${size}px "${font}"`;
      w = ctx.measureText(value).width;
    }
  }
  return w <= width ? size : Math.max(Math.floor(size * 0.55), Math.floor((size * width) / w));
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
/** Buttons per menu: 36 on a DVD, 255 on a Blu-ray. */
export const maxButtons = (tv: string) => (isBdFrame(tv) ? 255 : MAX_BUTTONS);

/** `tv` names the frame a layout is made for: `PAL`, `NTSC`, or `BD1080` / `BD720` (Blu-ray). */
export const isBdFrame = (tv: string) => tv.startsWith("BD");

export function canvasSize(tv: string): { width: number; height: number } {
  if (isBdFrame(tv)) return { width: 1920, height: 1080 };
  return { width: 720, height: tv === "NTSC" ? 480 : 576 };
}

/** Width of a canvas pixel on the screen relative to its height: 1 on Blu-ray, else the 4:3 TV's. */
export function pixelAspectOf(tv: string, canvas: { width: number; height: number }): number {
  return isBdFrame(tv) ? 1 : (4 / 3) * (canvas.height / canvas.width);
}

/** Length of every menu clip in seconds (a still is 1 s): what a Blu-ray's menus add to its size. */
export function menuClipSecs(layout: CustomLayout, motionSecs: number): number[] {
  return layout.menus.map((m) => (isMoving(m) ? (m.durationSecs ?? motionSecs) : 1));
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

export function rect(x: number, y: number, w: number, h: number, color: string, extra: Partial<RectElement> = {}): RectElement {
  return { id: newId("r"), type: "rect", color, x, y, w, h, rotation: 0, opacity: 1, action: null, ...extra };
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

type NavDir = "up" | "down" | "left" | "right";

/**
 * Spatial remote-control navigation fallback mirroring `author::compute_navigation`.
 * Used immediately on entering Remote Test mode while the authoritative Rust graph loads.
 */
export function computeNavigationFallback(menu: CustomMenu): ButtonNav[] {
  const btns = menu.elements.filter(isButton).map((e) => ({
    id: e.id,
    x0: e.x,
    y0: e.y,
    x1: e.x + e.w,
    y1: e.y + e.h,
    cx: e.x + e.w / 2,
    cy: e.y + e.h / 2,
  }));
  const axisGap = (a0: number, a1: number, b0: number, b1: number): number =>
    b0 >= a1 ? b0 - a1 : a0 >= b1 ? a0 - b1 : 0;

  const target = (from: number, dir: NavDir): string => {
    const a = btns[from];
    let bestCone: { score: number; idx: number } | null = null;
    let bestHalf: { score: number; idx: number } | null = null;
    for (let i = 0; i < btns.length; i++) {
      if (i === from) continue;
      const b = btns[i];
      let primary: number;
      let gap: number;
      let centerSecondary: number;
      if (dir === "up") {
        primary = a.cy - b.cy;
        gap = axisGap(a.x0, a.x1, b.x0, b.x1);
        centerSecondary = Math.abs(b.cx - a.cx);
      } else if (dir === "down") {
        primary = b.cy - a.cy;
        gap = axisGap(a.x0, a.x1, b.x0, b.x1);
        centerSecondary = Math.abs(b.cx - a.cx);
      } else if (dir === "left") {
        primary = a.cx - b.cx;
        gap = axisGap(a.y0, a.y1, b.y0, b.y1);
        centerSecondary = Math.abs(b.cy - a.cy);
      } else {
        primary = b.cx - a.cx;
        gap = axisGap(a.y0, a.y1, b.y0, b.y1);
        centerSecondary = Math.abs(b.cy - a.cy);
      }
      if (primary <= 0) continue;
      const score = primary + 2.0 * gap + 0.01 * centerSecondary;
      if (gap <= primary) {
        if (!bestCone || score < bestCone.score) bestCone = { score, idx: i };
      } else {
        if (!bestHalf || score < bestHalf.score) bestHalf = { score, idx: i };
      }
    }
    const chosen = bestCone ?? bestHalf;
    return btns[chosen ? chosen.idx : from].id;
  };

  return btns.map((b, i) => ({
    id: b.id,
    up: target(i, "up"),
    down: target(i, "down"),
    left: target(i, "left"),
    right: target(i, "right"),
  }));
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
export function draftLayout(titles: EditorTitle[], tv: string, heading: string, preset?: MenuPreset): CustomLayout {
  if (isBdFrame(tv)) return toBluray(draftLayout(titles, "PAL", heading, preset), canvasSize(tv));
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
    pageMenus.push(emptyMenu(titlePages > 1 ? tr("layout.hauptmenue", { p: p + 1 }) : tr("layout.hauptmenue2"), "title"));
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
      const caption = root ? `${t.label} · ${t.chapters > 1 ? tr("layout.kapitel2") : tr("layout.menue")}` : t.label;
      menu.elements.push(text(x, y + Y(118), 180, Y(30), caption, { size: 18, action: root ? { type: "menu", menu: root } : null }));
    });
    const bottom = Y(490);
    if (titles.length > 1 && p === 0) {
      menu.elements.push(text(60, bottom, 220, Y(36), tr("layout.alleAbspielen"), { size: 22, align: "left", action: { type: "playAll" } }));
    }
    if (p > 0) {
      menu.elements.push(text(300, bottom, 140, Y(36), tr("layout.zurueck"), { size: 22, action: { type: "menu", menu: pageMenus[p - 1].id } }));
    }
    if (p < titlePages - 1) {
      menu.elements.push(text(W - 220, bottom, 160, Y(36), tr("layout.weiter"), { size: 22, align: "right", action: { type: "menu", menu: pageMenus[p + 1].id } }));
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
      const name = t.chapters > 1 ? (pages > 1 ? tr("layout.kapitel3", { label: t.label, p: p + 1 }) : tr("layout.kapitel4", { label: t.label })) : tr("layout.menue2", { label: t.label });
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
      const row: TextElement[] = [text(60, bottom, 170, Y(36), tr("layout.filmAbspielen"), { size: 20, align: "left", action: { type: "playTitle", title: n } })];
      if (settingsId) row.push(text(240, bottom, 160, Y(36), tr("layout.einstellungen"), { size: 20, action: { type: "menu", menu: settingsId } }));
      row.push(text(410, bottom, 130, Y(36), tr("layout.hauptmenue2"), { size: 20, action: { type: "menu", menu: main } }));
      if (p > 0) row.push(text(550, bottom, 50, Y(36), "‹", { size: 24, action: { type: "menu", menu: rootMenus[p - 1].id } }));
      if (p < pages - 1) row.push(text(610, bottom, 50, Y(36), "›", { size: 24, action: { type: "menu", menu: rootMenus[p + 1].id } }));
      m.elements.push(...row);
    });
    menus.push(...rootMenus);

    // Settings menu: audio tracks left, subtitles (with "Aus") right.
    if (settingsId) {
      const s = emptyMenu(tr("layout.einstellungen2", { label: t.label }), "settings", n);
      s.id = settingsId;
      s.background = { ...shaded, gradientOverlay: "dim" };
      s.elements.push(text(60, Y(36), W - 120, Y(50), tr("layout.einstellungen2", { label: t.label }), { size: 32, bold: true }));
      const rows = 8;
      const step = Y(38);
      const top = Y(150);
      const audio = t.audioTracks ?? [];
      if (audio.length > 0) {
        s.elements.push(text(60, Y(110), 300, Y(32), tr("layout.tonspur"), { size: 22, bold: true, align: "left", color: "#FFD200" }));
        audio.slice(0, rows).forEach((a, k) => {
          s.elements.push(text(60, top + k * step, 300, Y(34), a.label, { size: 20, align: "left", action: { type: "setAudio", source: a.source } }));
        });
      }
      const subs = t.subtitleTracks ?? [];
      if (subs.length > 0) {
        s.elements.push(text(380, Y(110), 280, Y(32), tr("layout.untertitel"), { size: 22, bold: true, align: "left", color: "#FFD200" }));
        const entries: [string, number | null][] = [[tr("layout.aus"), null], ...subs.map((x): [string, number | null] => [x.label, x.source])];
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
      s.elements.push(text(60, bottom, 170, Y(36), tr("layout.zurueck"), { size: 20, align: "left", action: { type: "menu", menu: rootId } }));
      s.elements.push(text(240, bottom, 160, Y(36), tr("layout.filmAbspielen"), { size: 20, action: { type: "playTitle", title: n } }));
      s.elements.push(text(410, bottom, 130, Y(36), tr("layout.hauptmenue2"), { size: 20, action: { type: "menu", menu: main } }));
      menus.push(s);
    }
  });

  const layout: CustomLayout = { version: 1, canvas, highlight: { color: "#FFD200", selectColor: "#FF6A00", opacity: 0.8 }, menus };
  if (preset) applyPreset(layout, preset);
  // After the preset: it may change the font, and thus the text width.
  for (const m of layout.menus) {
    for (const e of m.elements) if (e.type === "text") e.size = fitSize(e.text, e.size, e.w, e.font, e.bold);
  }
  return layout;
}

export type MenuPreset = "swiss" | "cinema" | "midnight";
export const MENU_PRESETS: MenuPreset[] = ["swiss", "cinema", "midnight"];
/** Bundled menu sound per preset (file in `assets/menu-audio/`, shipped as `menu-audio/`). */
export const PRESET_SOUNDS: Record<MenuPreset, string> = {
  swiss: "sunlight-on-the-floorboards.opus",
  cinema: "porcelain-hours.opus",
  midnight: "view-from-the-berth.opus",
};

interface PresetStyle {
  bg: string;
  text: string;
  accent: string;
  font: string;
  bold: boolean;
  /** null = keep each text's own alignment. */
  align: Align | null;
  shadow: boolean;
  highlight: { color: string; selectColor: string; opacity: number };
  /** Decorative rects drawn behind the texts, for a canvas of `W` wide and `H` high. */
  decor: (W: number, H: number) => RectElement[];
}

const PRESET_STYLES: Record<MenuPreset, PresetStyle> = {
  swiss: {
    bg: "#F2F2F0",
    text: "#111111",
    accent: "#E4002B",
    font: "Arial",
    bold: true,
    align: "left",
    shadow: false,
    highlight: { color: "#E4002B", selectColor: "#111111", opacity: 0.8 },
    decor: (_W, H) => [rect(60, Math.round(H * 0.038), 90, Math.round(H * 0.014), "#E4002B")],
  },
  cinema: {
    bg: "#0B0B0D",
    text: "#E8C872",
    accent: "#D4AF37",
    font: "Georgia",
    bold: false,
    align: "center",
    shadow: true,
    highlight: { color: "#D4AF37", selectColor: "#FFF1B8", opacity: 0.8 },
    decor: (W, H) => {
      const bar = Math.round(H * 0.049);
      return [
        rect(0, 0, W, bar, "#000000"),
        rect(0, H - bar, W, bar, "#000000"),
        rect(0, bar, W, 2, "#D4AF37"),
        rect(0, H - bar - 2, W, 2, "#D4AF37"),
      ];
    },
  },
  midnight: {
    bg: "#0A1030",
    text: "#E8F1FF",
    accent: "#5CE1FF",
    font: "Segoe UI",
    bold: false,
    align: null,
    shadow: true,
    highlight: { color: "#00E5FF", selectColor: "#FFFFFF", opacity: 0.8 },
    decor: (W, H) => [rect(40, Math.round(H * 0.174), W - 80, Math.round(H * 0.66), "#1B2A5C", { opacity: 0.55, borderColor: "#5CE1FF", borderWidth: 2 })],
  },
};

/** Restyles a default draft in place: flat background, recoloured texts, decorative rects behind. */
function applyPreset(layout: CustomLayout, preset: MenuPreset): void {
  const st = PRESET_STYLES[preset];
  layout.highlight = { ...st.highlight };
  for (const m of layout.menus) {
    m.background = { color: st.bg, image: null, video: null, blur: 0, gradientOverlay: "none" };
    for (const e of m.elements) {
      if (e.type !== "text") continue;
      // The default's yellow marks the settings column headings.
      e.color = e.color === "#FFD200" ? st.accent : st.text;
      e.font = st.font;
      e.bold = e.bold || st.bold;
      e.shadow = st.shadow;
      if (st.align) e.align = st.align;
    }
    m.elements.unshift(...st.decor(layout.canvas.width, layout.canvas.height));
  }
}

/**
 * A 720×576 draft on a 16:9 Blu-ray canvas: positions and sizes scale with the canvas, text
 * sizes with its height, and video boxes become 16:9 (400×225) with the caption right below.
 */
function toBluray(draft: CustomLayout, canvas: { width: number; height: number }): CustomLayout {
  const sx = canvas.width / draft.canvas.width;
  const sy = canvas.height / draft.canvas.height;
  for (const m of draft.menus) {
    for (const e of m.elements) {
      e.x = Math.round(e.x * sx);
      e.y = Math.round(e.y * sy);
      e.w = Math.round(e.w * sx);
      e.h = Math.round(e.h * sy);
      if (e.type === "text") e.size = Math.round(e.size * sy);
      if (e.type === "video") [e.w, e.h] = [400, 225];
      if (e.type === "rect" && e.borderWidth) e.borderWidth = Math.round(e.borderWidth * sy);
    }
    m.elements.forEach((e, i) => {
      const caption = m.elements[i + 1];
      if (e.type === "video" && caption?.type === "text" && caption.x === e.x) caption.y = e.y + e.h + 8;
    });
  }
  return { ...draft, canvas };
}
