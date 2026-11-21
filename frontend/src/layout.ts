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
  | { type: "customVm"; script: string }
  /** Disc-wide audio choice: 1-based track position, applied to every title that has it; any menu. */
  | { type: "setGlobalAudio"; track: number }
  /** Disc-wide subtitle choice: 1-based track position, `null` = off; any menu. */
  | { type: "setGlobalSubtitle"; track: number | null };

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
  /** Manual D-pad targets (element ids of buttons on the same menu); null/omitted = automatic spatial routing. */
  nav?: NavOverride | null;
  /** Run the action as soon as the button gets focus (DVD autoaction / BD auto action flag). */
  autoExecute?: boolean;
  /** Selection indicator: id of a button on the same menu; this element is hidden and only shows
   *  (in that button's highlight colours) while the button is focused or activated. */
  indicatorFor?: string | null;
}

export interface NavOverride {
  up?: string | null;
  down?: string | null;
  left?: string | null;
  right?: string | null;
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
  /** Box fill behind the text; null/omitted = transparent. */
  bgColor?: string | null;
  /** Fill opacity 0..1 (default 1). */
  bgOpacity?: number;
  /** Corner radii of the fill, canvas units (default 0). */
  rx?: number;
  ry?: number;
  /** Inner padding between box edge and text, canvas units (default 0). */
  padding?: number;
  /** Drop shadow (used when `shadow` is true); defaults reproduce the old fixed shadow: #000000, 0.67, 2/2, blur 0. */
  shadowColor?: string;
  shadowOpacity?: number;
  shadowX?: number;
  shadowY?: number;
  shadowBlur?: number;
  /** Text outline; omitted or width 0 = none. Width in canvas units. */
  outlineColor?: string | null;
  outlineWidth?: number;
}

/** Source rectangle of a picture, normalized 0..1 of its natural size. */
export interface CropRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface ImageElement extends ElementBase {
  type: "image";
  src: string;
  fit: Fit;
  /** Non-destructive crop applied before fitting; null/omitted = whole picture. */
  crop?: CropRect | null;
}

/** Built-in vector symbols (`assets/icons/<name>.svg`, drawn in `currentColor`). */
export type IconName =
  | "next" | "prev" | "home" | "play" | "subtitles" | "audio" | "back"
  | "arrowLeft" | "arrowRight" | "chapters" | "settings" | "star"
  | "framePill" | "frameRect"
  | "pointer" | "bullet" | "underline" | "brackets";
export const ICON_NAMES: IconName[] = [
  "next", "prev", "home", "play", "subtitles", "audio", "back",
  "arrowLeft", "arrowRight", "chapters", "settings", "star",
  "framePill", "frameRect",
  "pointer", "bullet", "underline", "brackets",
];
/** Icons meant as selection indicators (default `indicatorFor` target when added). */
export const INDICATOR_ICONS: IconName[] = ["pointer", "bullet", "underline", "brackets"];

/** A built-in symbol, fitted (contain) into its box and filled with `color`. */
export interface IconElement extends ElementBase {
  type: "icon";
  icon: IconName;
  color: string;
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

export type LayoutElement = TextElement | ImageElement | VideoElement | RectElement | IconElement;

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
  /** What happens when the menu clip ends; null/omitted = "loop". */
  playMode?: MenuPlayMode | null;
  /** "onceActivate": seconds to hold the last frame before the start button runs (0..254, default 0). */
  timeoutSecs?: number | null;
  /** Editor-only sidebar group ("main", "episodes", "chapters", "settings"); null = derived from the role. */
  group?: string | null;
  elements: LayoutElement[];
}

/** `loop` = loop forever (today); `once` = play once and hold the last frame;
 *  `onceActivate` = play once, hold `timeoutSecs`, then run the menu's start button (first focused one). */
export type MenuPlayMode = "loop" | "once" | "onceActivate";

/** Per-title end action. `default` = the disc-wide flow (Play All / return settings). */
export type AfterPlayback = "default" | "next" | "mainMenu" | "titleMenu" | "loop";
export const AFTER_PLAYBACK: AfterPlayback[] = ["default", "next", "mainMenu", "titleMenu", "loop"];

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
  /** Draft metadata (ignored by the build): theme and settings-menu structure it was made with. */
  theme?: MenuTheme;
  settingsMode?: SettingsMode;
  menus: CustomMenu[];
}

/** `landing` = retail single-screen main menu (default); `classic` = 6-thumbnail grid. */
export type MenuTheme = "landing" | "classic";
/** `global` = one shared audio/subtitle menu for the disc; `perTitle` = one per title. */
export type SettingsMode = "global" | "perTitle";

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
  /** End action of the title (Test Remote preview); omitted = "default". */
  afterPlayback?: AfterPlayback;
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
 * Maps 1-based old title indices to 1-based new title indices by path,
 * consuming duplicate file paths 1-to-1 in order.
 */
function mapPathsOneToOne(oldPaths: string[], newPaths: string[]): { map: Map<number, number>; matchedNew: Set<number> } {
  const usedNew = new Set<number>();
  const map = new Map<number, number>();
  const matchedNew = new Set<number>();
  for (let i = 0; i < oldPaths.length; i++) {
    const newIdx = newPaths.findIndex((p, idx) => !usedNew.has(idx) && p === oldPaths[i]);
    if (newIdx >= 0) {
      usedNew.add(newIdx);
      map.set(i + 1, newIdx + 1);
      matchedNew.add(newIdx + 1);
    }
  }
  return { map, matchedNew };
}

/**
 * Re-maps a custom layout when titles are reordered or removed, based on their file paths.
 * Returns true if remapping succeeded for all referenced titles.
 */
export function remapLayoutByPaths(layout: CustomLayout, oldPaths: string[], newPaths: string[]): boolean {
  const { map } = mapPathsOneToOne(oldPaths, newPaths);
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

/** A built-in symbol; `color` defaults to white (the default text colour). */
export function icon(x: number, y: number, w: number, h: number, name: IconName, color = "#FFFFFF", extra: Partial<IconElement> = {}): IconElement {
  return { id: newId("i"), type: "icon", icon: name, color, x, y, w, h, rotation: 0, opacity: 1, action: null, ...extra };
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

  return resolveNav(
    menu,
    btns.map((b, i) => ({
      id: b.id,
      up: target(i, "up"),
      down: target(i, "down"),
      left: target(i, "left"),
      right: target(i, "right"),
    })),
  );
}

/** Applies manual `nav` overrides on top of an automatic graph; targets that are not buttons of the menu, or the button itself, are ignored. */
export function resolveNav(menu: CustomMenu, auto: ButtonNav[]): ButtonNav[] {
  const buttons = new Map(menu.elements.filter(isButton).map((e) => [e.id, e]));
  return auto.map((n) => {
    const o = buttons.get(n.id)?.nav;
    if (!o) return n;
    const pick = (dir: NavDir): string | null => {
      const v = o[dir];
      return v && v !== n.id && buttons.has(v) ? v : n[dir];
    };
    return { id: n.id, up: pick("up"), down: pick("down"), left: pick("left"), right: pick("right") };
  });
}

/** Menus that move (video elements, video background or sound) – they are encoded as loops. */
export function isMoving(m: CustomMenu): boolean {
  return !!m.background.video || m.audio.type !== "none" || m.elements.some((e) => e.type === "video");
}

interface SubmenuStyle {
  bgColor: string;
  flatBg: boolean;
  font: string;
  textColor: string;
  accentColor: string;
  italic: boolean;
  shadow: boolean;
}

/** How a submenu synthesis follows the draft: theme, settings structure, id of the title's settings menu. */
interface SubNav {
  theme?: MenuTheme;
  settingsMode?: SettingsMode;
  settingsId?: string;
}

/** Home icon at `homeX` plus round prev/next arrows on the right, as buttons on a 720-wide base canvas. */
function navIcons(W: number, y: number, h: number, homeX: number, home: string, prev: string | null, next: string | null): IconElement[] {
  const out = [icon(homeX, y, 44, h, "home", "#FFFFFF", { action: { type: "menu", menu: home } })];
  if (prev) out.push(icon(W - 172, y, 44, h, "prev", "#FFFFFF", { action: { type: "menu", menu: prev } }));
  if (next) out.push(icon(W - 104, y, 44, h, "next", "#FFFFFF", { action: { type: "menu", menu: next } }));
  return out;
}

/** Audio column (left) and subtitle column (right, "Off" first) of a settings menu; entries beyond the rows are dropped. */
function fillSettings(s: CustomMenu, Y: (v: number) => number, audio: [string, Action][], subs: [string, Action][], off: Action): void {
  const rows = 8;
  const step = Y(38);
  const top = Y(150);
  if (audio.length > 0) {
    s.elements.push(text(60, Y(110), 300, Y(32), tr("layout.tonspur"), { size: 22, bold: true, align: "left", color: "#FFD200" }));
    audio.slice(0, rows).forEach(([label, action], k) => {
      s.elements.push(text(60, top + k * step, 300, Y(34), label, { size: 20, align: "left", action }));
    });
  }
  if (subs.length > 0) {
    s.elements.push(text(380, Y(110), 280, Y(32), tr("layout.untertitel"), { size: 22, bold: true, align: "left", color: "#FFD200" }));
    const entries: [string, Action][] = [[tr("layout.aus"), off], ...subs];
    const twoColumns = entries.length > rows;
    entries.slice(0, rows * 2).forEach(([label, action], k) => {
      const col = Math.floor(k / rows);
      s.elements.push(text(twoColumns ? 380 + col * 145 : 380, top + (k % rows) * step, twoColumns ? 135 : 280, Y(34), label, { size: 20, align: "left", action }));
    });
  }
}

/** Synthesizes the VTSM root (chapter) pages and optional settings menu for one title. */
function buildTitleSubmenus(
  t: EditorTitle,
  mainMenuId: string,
  canvas: { width: number; height: number },
  tv: string,
  rootId: string,
  allocId: (prefix: string) => string = newId,
  style?: SubmenuStyle,
  nav: SubNav = {},
): CustomMenu[] {
  const landing = nav.theme === "landing";
  const global = nav.settingsMode === "global";
  const isBd = isBdFrame(tv) || canvas.width > 720;
  const baseCanvas = isBd ? { width: 720, height: 576 } : canvas;
  const { width: W, height: H } = baseCanvas;
  const sy = H / 576;
  const Y = (v: number) => Math.round(v * sy);
  const n = t.index;
  const settingsId = !global && needsSettings(t) ? (nav.settingsId ?? allocId("m")) : null;
  const shaded: Background = style?.flatBg
    ? { color: style.bgColor, image: null, video: null, blur: 0, gradientOverlay: "none" }
    : { color: style?.bgColor ?? "#14192B", image: null, video: { title: n, startSecs: null }, blur: 14 };

  const out: CustomMenu[] = [];
  const perChapterPage = 6;
  const pages = t.chapters > 1 ? Math.ceil(t.chapters / perChapterPage) : 1;
  const rootMenus: CustomMenu[] = [];
  for (let p = 0; p < pages; p++) {
    const name = t.chapters > 1 ? (pages > 1 ? tr("layout.kapitel3", { label: t.label, p: p + 1 }) : tr("layout.kapitel4", { label: t.label })) : tr("layout.menue2", { label: t.label });
    const m = emptyMenu(name, "root", n);
    m.id = p === 0 ? rootId : allocId("m");
    m.group = "chapters";
    m.background = style?.flatBg ? { ...shaded } : { ...shaded, gradientOverlay: "bottomDark" };
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
    const bottom = Y(490);
    const row: TextElement[] = [text(60, bottom, 170, Y(36), tr("layout.filmAbspielen"), { size: 20, align: "left", action: { type: "playTitle", title: n } })];
    if (settingsId) row.push(text(240, bottom, 160, Y(36), tr("layout.einstellungen"), { size: 20, action: { type: "menu", menu: settingsId } }));
    if (landing) {
      m.elements.push(...row, ...navIcons(W, bottom, Y(44), 420, mainMenuId, p > 0 ? rootMenus[p - 1].id : null, p < pages - 1 ? rootMenus[p + 1].id : null));
      return;
    }
    row.push(text(410, bottom, 130, Y(36), tr("layout.hauptmenue2"), { size: 20, action: { type: "menu", menu: mainMenuId } }));
    if (p > 0) row.push(text(550, bottom, 50, Y(36), "‹", { size: 24, action: { type: "menu", menu: rootMenus[p - 1].id } }));
    if (p < pages - 1) row.push(text(610, bottom, 50, Y(36), "›", { size: 24, action: { type: "menu", menu: rootMenus[p + 1].id } }));
    m.elements.push(...row);
  });
  out.push(...rootMenus);

  if (settingsId) {
    const s = emptyMenu(tr("layout.einstellungen2", { label: t.label }), "settings", n);
    s.id = settingsId;
    s.background = style?.flatBg ? { ...shaded } : { ...shaded, gradientOverlay: "dim" };
    s.elements.push(text(60, Y(36), W - 120, Y(50), tr("layout.einstellungen2", { label: t.label }), { size: 32, bold: true }));
    s.group = "settings";
    fillSettings(s, Y, (t.audioTracks ?? []).map((a): [string, Action] => [a.label, { type: "setAudio", source: a.source }]), (t.subtitleTracks ?? []).map((x): [string, Action] => [x.label, { type: "setSubtitle", source: x.source }]), { type: "setSubtitle", source: null });
    const bottom = Y(490);
    s.elements.push(text(60, bottom, 170, Y(36), tr("layout.zurueck"), { size: 20, align: "left", action: { type: "menu", menu: rootId } }));
    s.elements.push(text(240, bottom, 160, Y(36), tr("layout.filmAbspielen"), { size: 20, action: { type: "playTitle", title: n } }));
    if (landing) s.elements.push(icon(420, bottom, 44, Y(44), "home", "#FFFFFF", { action: { type: "menu", menu: mainMenuId } }));
    else s.elements.push(text(410, bottom, 130, Y(36), tr("layout.hauptmenue2"), { size: 20, action: { type: "menu", menu: mainMenuId } }));
    out.push(s);
  }

  for (const m of out) {
    if (style) {
      for (const e of m.elements) {
        if (e.type === "icon") e.color = style.textColor;
        if (e.type !== "text") continue;
        e.color = e.color === "#FFD200" ? style.accentColor : style.textColor;
        e.font = style.font;
        e.italic = style.italic;
        e.shadow = style.shadow;
      }
    }
    if (isBd) scaleMenuToBluray(m, baseCanvas, canvas);
    for (const e of m.elements) if (e.type === "text") e.size = fitSize(e.text, e.size, e.w, e.font, e.bold);
  }
  return out;
}

export interface DraftOptions {
  /** Default `landing`. */
  theme?: MenuTheme;
  /** Default `global` for several titles on the landing theme, else `perTitle`. */
  settingsMode?: SettingsMode;
}

const ITEMS_PER_PAGE = 6;

/** Group of a menu in the editor sidebar: its own `group`, else derived from the role. */
export function menuGroup(m: CustomMenu): string {
  return m.group ?? (m.role === "root" ? "chapters" : m.role === "settings" ? "settings" : "main");
}

/**
 * A first draft for the project's actual titles (addressed by their 1-based `index`).
 * Landing theme (default): one retail main screen (heading, text button stack, key-art frame with
 * title 1's clip), paginated "episodes" pages (6 thumbnails + captions, round arrows, home icon),
 * and chapter pages per title with chapters. Classic theme: the 6-thumbnail grid as main menu
 * ("Alle abspielen", text arrows). Audio and subtitles: one disc-wide menu (`global`) or one
 * settings menu per title (`perTitle`). Every drafted menu carries its sidebar `group`.
 */
export function draftLayout(titles: EditorTitle[], tv: string, heading: string, preset?: MenuPreset, opts: DraftOptions = {}): CustomLayout {
  if (isBdFrame(tv)) return toBluray(draftLayout(titles, "PAL", heading, preset, opts), canvasSize(tv));
  const theme = opts.theme ?? "landing";
  const settingsMode = opts.settingsMode ?? (theme === "landing" && titles.length > 1 ? "global" : "perTitle");
  const global = settingsMode === "global" && titles.some(needsSettings);
  const canvas = canvasSize(tv);
  const { width: W, height: H } = canvas;
  const sy = H / 576;
  const Y = (v: number) => Math.round(v * sy);
  const landing = theme === "landing";
  const menus: CustomMenu[] = [];
  const pageCount = Math.max(1, Math.ceil(titles.length / ITEMS_PER_PAGE));
  // A title gets VTSM menus when it has chapters to choose or (per-title mode) streams to select.
  const needsRoot = (t: EditorTitle) => t.chapters > 1 || (settingsMode === "perTitle" && needsSettings(t));
  const rootIds = titles.map((t) => (needsRoot(t) ? newId("m") : null));
  const settingsIds = titles.map((t) => (settingsMode === "perTitle" && needsSettings(t) ? newId("m") : null));

  const landingMenu = landing ? emptyMenu(tr("layout.hauptmenue2"), "title") : null;
  const pageMenus: CustomMenu[] = [];
  for (let p = 0; p < pageCount; p++) {
    const name = landing
      ? pageCount > 1 ? tr("layout.episodesPage", { p: p + 1 }) : tr("layout.episodes")
      : pageCount > 1 ? tr("layout.hauptmenue", { p: p + 1 }) : tr("layout.hauptmenue2");
    pageMenus.push(emptyMenu(name, "title"));
  }
  const mainId = (landingMenu ?? pageMenus[0]).id;
  // The landing theme has episodes pages only for several titles.
  const pages = landing && titles.length < 2 ? [] : pageMenus;
  const settingsMenu = global ? emptyMenu(tr("layout.globalSettings"), "title") : null;

  const thumbs = (menu: CustomMenu, p: number) => {
    titles.slice(p * ITEMS_PER_PAGE, (p + 1) * ITEMS_PER_PAGE).forEach((t, i) => {
      const n = t.index;
      const x = 60 + (i % 3) * 210;
      const y = Y(120 + Math.floor(i / 3) * 180);
      menu.elements.push(video(x, y, 180, Y(112), n, null, { type: "playTitle", title: n }));
      const root = rootIds[p * ITEMS_PER_PAGE + i];
      const caption = root ? `${t.label} · ${t.chapters > 1 ? tr("layout.kapitel2") : tr("layout.menue")}` : t.label;
      menu.elements.push(text(x, y + Y(118), 180, Y(30), caption, { size: 18, action: root ? { type: "menu", menu: root } : null }));
    });
  };

  if (landingMenu) {
    const m = landingMenu;
    m.group = "main";
    // Left column: logo slot (the heading) over a clean button stack.
    m.elements.push(text(48, Y(60), 290, Y(120), heading || "DVD", { size: 40, bold: true, align: "left", wrap: true }));
    const buttons: [string, Action][] = [];
    if (titles.length === 1) buttons.push([tr("layout.play"), { type: "playTitle", title: titles[0].index }]);
    else if (titles.length > 1) buttons.push([tr("layout.alleAbspielen"), { type: "playAll" }]);
    if (titles.length > 1) buttons.push([tr("layout.episodes"), { type: "menu", menu: pageMenus[0].id }]);
    if (titles.length === 1 && rootIds[0] && titles[0].chapters > 1) buttons.push([tr("layout.kapitel2"), { type: "menu", menu: rootIds[0] }]);
    if (settingsMenu) buttons.push([tr("layout.subsAudio"), { type: "menu", menu: settingsMenu.id }]);
    else if (titles.length === 1 && settingsIds[0]) buttons.push([tr("layout.subsAudio"), { type: "menu", menu: settingsIds[0] }]);
    buttons.forEach(([label, action], i) => {
      m.elements.push(text(48, Y(210 + i * 54), 290, Y(42), label, { size: 26, align: "left", action }));
    });
    // Key art: a user-replaceable frame with title 1's clip inside.
    m.elements.push(rect(350, Y(70), 322, Y(430), "#0B0F1E", { borderColor: "#FFFFFF", borderWidth: 2 }));
    if (titles.length > 0) m.elements.push(video(358, Y(78), 306, Y(414), titles[0].index, null, null));
    menus.push(m);
  }

  pageMenus.forEach((menu, p) => {
    const last = p === pageCount - 1;
    if (landing) {
      if (pages.length === 0) return;
      menu.group = "episodes";
      menu.elements.push(text(60, Y(36), W - 120, Y(56), pageCount > 1 ? tr("layout.episodesPage", { p: p + 1 }) : tr("layout.episodes"), { size: 34, bold: true }));
      thumbs(menu, p);
      menu.elements.push(...navIcons(W, Y(490), Y(44), 60, mainId, p > 0 ? pageMenus[p - 1].id : null, last ? null : pageMenus[p + 1].id));
      return;
    }
    menu.group = "main";
    menu.elements.push(text(60, Y(36), W - 120, Y(56), heading || "DVD", { size: 40, bold: true }));
    thumbs(menu, p);
    const bottom = Y(490);
    if (titles.length > 1 && p === 0) {
      menu.elements.push(text(60, bottom, 220, Y(36), tr("layout.alleAbspielen"), { size: 22, align: "left", action: { type: "playAll" } }));
    }
    if (p > 0) {
      menu.elements.push(text(300, bottom, 140, Y(36), tr("layout.zurueck"), { size: 22, action: { type: "menu", menu: pageMenus[p - 1].id } }));
    }
    if (!last) {
      menu.elements.push(text(W - 220, bottom, 160, Y(36), tr("layout.weiter"), { size: 22, align: "right", action: { type: "menu", menu: pageMenus[p + 1].id } }));
    }
  });
  menus.push(...pages);

  if (settingsMenu) {
    const s = settingsMenu;
    s.group = "settings";
    s.elements.push(text(60, Y(36), W - 120, Y(50), tr("layout.globalSettings"), { size: 32, bold: true }));
    // The title with the most tracks defines the list; a track is addressed by its position.
    const richest = (count: (t: EditorTitle) => number) => titles.reduce((a, t) => (count(t) > count(a) ? t : a), titles[0]);
    const audio = richest((t) => t.audioTracks?.length ?? 0).audioTracks ?? [];
    const subs = richest((t) => t.subtitleTracks?.length ?? 0).subtitleTracks ?? [];
    fillSettings(
      s,
      Y,
      audio.length > 1 ? audio.map((a, k): [string, Action] => [a.label, { type: "setGlobalAudio", track: k + 1 }]) : [],
      subs.map((x, k): [string, Action] => [x.label, { type: "setGlobalSubtitle", track: k + 1 }]),
      { type: "setGlobalSubtitle", track: null },
    );
    s.elements.push(icon(60, Y(490), 44, Y(44), "home", "#FFFFFF", { action: { type: "menu", menu: mainId } }));
    menus.push(s);
  }

  titles.forEach((t, i) => {
    const rootId = rootIds[i];
    if (!rootId) return;
    const main = landing ? mainId : pageMenus[Math.floor(i / ITEMS_PER_PAGE)].id;
    menus.push(...buildTitleSubmenus(t, main, canvas, tv, rootId, newId, undefined, { theme, settingsMode, settingsId: settingsIds[i] ?? undefined }));
  });

  const layout: CustomLayout = { version: 1, canvas, highlight: { color: "#FFD200", selectColor: "#FF6A00", opacity: 0.8 }, theme, settingsMode, menus };
  if (preset) applyPreset(layout, preset);
  // After the preset: it may change the font, and thus the text width.
  for (const m of layout.menus) {
    for (const e of m.elements) if (e.type === "text") e.size = fitSize(e.text, e.size, e.w, e.font, e.bold);
  }
  return layout;
}

/**
 * Copies the look of menu `menuId` (fonts, text and icon colours, bold/italic/shadow, button
 * highlight, background colour and overlay) to every other menu of its group; positions, texts,
 * actions and pictures stay. Returns how many menus changed.
 */
const TEXT_STYLE_KEYS = [
  "bgColor", "bgOpacity", "rx", "ry", "padding",
  "shadowColor", "shadowOpacity", "shadowX", "shadowY", "shadowBlur", "outlineColor", "outlineWidth",
] as const;

type TextKind = "heading" | "label" | "button";
const topTextY = (m: CustomMenu) => Math.min(...m.elements.filter((e) => e.type === "text" && !e.action).map((e) => e.y));
const textKindOf = (m: CustomMenu, e: TextElement): TextKind => (e.action ? "button" : e.y === topTextY(m) ? "heading" : "label");
const textsOf = (m: CustomMenu) => m.elements.filter((e): e is TextElement => e.type === "text");

/** Copies text look, icon colour and button highlight from `src` to `m` (no background, no geometry). */
function copyTypography(src: CustomMenu, m: CustomMenu): void {
  const srcText = new Map<TextKind, TextElement>();
  for (const e of textsOf(src)) if (!srcText.has(textKindOf(src, e))) srcText.set(textKindOf(src, e), e);
  const srcIcon = src.elements.find((e): e is IconElement => e.type === "icon");
  const srcButton = src.elements.find(isButton);
  for (const e of textsOf(m)) {
    const from = srcText.get(textKindOf(m, e));
    if (from) {
      Object.assign(e, { font: from.font, color: from.color, bold: from.bold, italic: from.italic, shadow: from.shadow });
      for (const k of TEXT_STYLE_KEYS) {
        if (from[k] === undefined) delete e[k];
        else Object.assign(e, { [k]: from[k] });
      }
    }
  }
  for (const e of m.elements) {
    if (e.type === "icon" && srcIcon) e.color = srcIcon.color;
    if (isButton(e) && srcButton) {
      if (srcButton.highlight) e.highlight = { ...srcButton.highlight };
      else delete e.highlight;
    }
  }
}

export function applyStyleToSimilar(layout: CustomLayout, menuId: string): number {
  const src = layout.menus.find((m) => m.id === menuId);
  if (!src) return 0;
  let changed = 0;
  for (const m of layout.menus) {
    if (m === src || menuGroup(m) !== menuGroup(src)) continue;
    const before = JSON.stringify(m);
    copyTypography(src, m);
    m.background.color = src.background.color;
    m.background.gradientOverlay = src.background.gradientOverlay;
    if (JSON.stringify(m) !== before) changed++;
  }
  return changed;
}

export type BatchScope = "group" | "all";
export interface BatchOptions {
  scope: BatchScope;
  background: boolean;
  typography: boolean;
  navIcons: boolean;
  positions: boolean;
}

/** Navigation slot of an element: an icon (or a plain arrow glyph text) with a menu action. */
export function navSlot(e: LayoutElement): "next" | "prev" | "home" | "back" | null {
  if (e.action?.type !== "menu") return null;
  if (e.type === "icon") {
    switch (e.icon) {
      case "next": case "arrowRight": return "next";
      case "prev": case "arrowLeft": return "prev";
      case "home": return "home";
      case "back": return "back";
      default: return null;
    }
  }
  if (e.type === "text") {
    const g = e.text.trim();
    if (/^[>›→▶]+$/.test(g)) return "next";
    if (/^[<‹←◀]+$/.test(g)) return "prev";
  }
  return null;
}

const firstNavBySlot = (m: CustomMenu) => {
  const map = new Map<string, LayoutElement>();
  for (const e of m.elements) {
    const s = navSlot(e);
    if (s && !map.has(s)) map.set(s, e);
  }
  return map;
};

/**
 * Copies the look of menu `sourceMenuId` to the other menus of its group (or all menus) by
 * category (background/audio/play mode, typography, nav icons, heading + nav positions).
 * Thumbnails, captions and other buttons never move. Returns how many menus changed.
 */
export function applyStyleBatch(layout: CustomLayout, sourceMenuId: string, opts: BatchOptions): number {
  const src = layout.menus.find((m) => m.id === sourceMenuId);
  if (!src) return 0;
  const srcNav = firstNavBySlot(src);
  const srcHeading = textsOf(src).find((e) => !e.action && e.y === topTextY(src));
  const clone = <T>(v: T): T => (v === undefined ? v : JSON.parse(JSON.stringify(v)));
  let changed = 0;
  for (const m of layout.menus) {
    if (m === src || (opts.scope === "group" && menuGroup(m) !== menuGroup(src))) continue;
    const before = JSON.stringify(m);
    if (opts.background) {
      const bg = src.background;
      m.background = { ...m.background, color: bg.color, blur: bg.blur };
      for (const k of ["image", "video", "gradientOverlay"] as const) {
        if (bg[k] === undefined) delete m.background[k];
        else Object.assign(m.background, { [k]: clone(bg[k]) });
      }
      m.audio = clone(src.audio);
      for (const k of ["durationSecs", "playMode", "timeoutSecs"] as const) {
        if (src[k] === undefined) delete m[k];
        else Object.assign(m, { [k]: src[k] });
      }
    }
    if (opts.typography) copyTypography(src, m);
    if (opts.navIcons || opts.positions) {
      const heading = textsOf(m).find((e) => !e.action && e.y === topTextY(m));
      for (const e of m.elements) {
        const s = navSlot(e);
        const from = s ? srcNav.get(s) : undefined;
        if (!from) continue;
        if (opts.navIcons && e.type === "icon" && from.type === "icon") {
          const cx = e.x + e.w / 2;
          const cy = e.y + e.h / 2;
          Object.assign(e, { icon: from.icon, color: from.color, w: from.w, h: from.h, x: cx - from.w / 2, y: cy - from.h / 2 });
        }
        if (opts.positions) Object.assign(e, { x: from.x, y: from.y, w: from.w, h: from.h });
      }
      if (opts.positions && heading && srcHeading) Object.assign(heading, { x: srcHeading.x, y: srcHeading.y, w: srcHeading.w, h: srcHeading.h });
    }
    if (JSON.stringify(m) !== before) changed++;
  }
  return changed;
}

/** The button focused on entry: the first button (element order) that is not auto-execute. Mirrors Rust `focus_pre`. */
export function startButtonId(menu: CustomMenu): string | null {
  return menu.elements.find((e) => isButton(e) && !e.autoExecute)?.id ?? null;
}

export type AfterPlaybackStep = { type: "title"; title: number } | { type: "menu"; menuId: string } | { type: "none" };

/** Test Remote model of what follows the end of `title` (1-based); see `AfterPlayback`. */
export function afterPlaybackStep(layout: CustomLayout, titles: EditorTitle[], title: number): AfterPlaybackStep {
  const mode = titles[title - 1]?.afterPlayback ?? "default";
  const plays = (m: CustomMenu) => m.elements.some((e) => (e.action?.type === "playTitle" || e.action?.type === "playChapter") && e.action.title === title);
  const main = layout.menus.find((m) => m.role === "title" && plays(m)) ?? layout.menus[0];
  const mainStep: AfterPlaybackStep = main ? { type: "menu", menuId: main.id } : { type: "none" };
  switch (mode) {
    case "loop": return { type: "title", title };
    case "next": return titles[title] ? { type: "title", title: title + 1 } : mainStep;
    case "titleMenu": {
      const root = layout.menus.find((m) => m.role === "root" && m.title === title);
      return root ? { type: "menu", menuId: root.id } : mainStep;
    }
    default: return mainStep;
  }
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
      if (e.type === "icon") e.color = st.text;
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
export function scaleMenuToBluray(m: CustomMenu, fromCanvas: { width: number; height: number }, toCanvas: { width: number; height: number }): void {
  const sx = toCanvas.width / fromCanvas.width;
  const sy = toCanvas.height / fromCanvas.height;
  const vidW = Math.round((400 * toCanvas.width) / 1920);
  const vidH = Math.round((225 * toCanvas.height) / 1080);
  for (const e of m.elements) {
    e.x = Math.round(e.x * sx);
    e.y = Math.round(e.y * sy);
    e.w = Math.round(e.w * sx);
    e.h = Math.round(e.h * sy);
    if (e.type === "text") {
      e.size = Math.round(e.size * sy);
      for (const k of ["rx", "ry", "padding", "shadowX", "shadowY", "shadowBlur", "outlineWidth"] as const) {
        if (e[k]) e[k] = Math.round(e[k] * sy);
      }
    }
    // Thumbnail buttons become 16:9; an action-less video (landing key art) just scales.
    if (e.type === "video" && e.action) [e.w, e.h] = [vidW, vidH];
    if (e.type === "icon") {
      const side = Math.min(e.w, e.h);
      [e.x, e.y, e.w, e.h] = [e.x + Math.round((e.w - side) / 2), e.y + Math.round((e.h - side) / 2), side, side];
    }
    if (e.type === "rect" && e.borderWidth) e.borderWidth = Math.round(e.borderWidth * sy);
  }
  m.elements.forEach((e, i) => {
    const caption = m.elements[i + 1];
    if (e.type === "video" && caption?.type === "text" && caption.x === e.x) caption.y = e.y + e.h + 8;
  });
}

function toBluray(draft: CustomLayout, canvas: { width: number; height: number }): CustomLayout {
  for (const m of draft.menus) scaleMenuToBluray(m, draft.canvas, canvas);
  return { ...draft, canvas };
}

// ---------------------------------------------------------------------------
// Incremental layout sync (preserves user edits when titles change)
// ---------------------------------------------------------------------------

interface Box {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

interface TitleSlot {
  vid: Box;
  cap: Box;
  fontSize: number;
}

interface ExtractedStyle {
  bgColor: string;
  bgGradient: GradientOverlay;
  flatBg: boolean;
  font: string;
  textColor: string;
  accentColor: string;
  italic: boolean;
  shadow: boolean;
  heading: {
    x: number;
    y: number;
    w: number;
    h: number;
    font: string;
    size: number;
    color: string;
    align: Align;
    bold: boolean;
    italic: boolean;
    shadow: boolean;
  } | null;
}

function elementBox(e: LayoutElement): Box {
  const deg = ((e.rotation % 360) + 360) % 360;
  if (deg === 0) return { x0: e.x, y0: e.y, x1: e.x + e.w, y1: e.y + e.h };
  const rad = (deg * Math.PI) / 180;
  const cos = Math.abs(Math.cos(rad));
  const sin = Math.abs(Math.sin(rad));
  const cx = e.x + e.w / 2;
  const cy = e.y + e.h / 2;
  const halfW = (e.w * cos + e.h * sin) / 2;
  const halfH = (e.w * sin + e.h * cos) / 2;
  return { x0: cx - halfW, y0: cy - halfH, x1: cx + halfW, y1: cy + halfH };
}

function boxesConflict(a: Box, b: Box, margin = 4): boolean {
  return a.x0 < b.x1 + margin && b.x0 < a.x1 + margin && a.y0 < b.y1 + margin && b.y0 < a.y1 + margin;
}

function insideCanvas(b: Box, canvas: { width: number; height: number }): boolean {
  return b.x0 >= 0 && b.y0 >= 0 && b.x1 <= canvas.width && b.y1 <= canvas.height;
}

/** Bounding boxes to avoid when placing new buttons/captions on `menu`. */
function blockingBoxes(menu: CustomMenu, canvas: { width: number; height: number }): Box[] {
  const boxes: Box[] = [];
  for (const e of menu.elements) {
    if (isButton(e)) {
      boxes.push(elementBox(e));
    } else if ((e.type === "text" || e.type === "video") && (e.w <= canvas.width * 0.85 || e.h <= canvas.height * 0.85)) {
      boxes.push(elementBox(e));
    }
  }
  return boxes;
}

function extractLayoutStyle(layout: CustomLayout): ExtractedStyle {
  const firstTitle = layout.menus.find((m) => m.role === "title");
  const H = layout.canvas.height;
  const textEls = (firstTitle?.elements.filter((e): e is TextElement => e.type === "text") ?? []);
  const headingEl =
    textEls.find((e) => !e.action && e.y < H * 0.25) ??
    textEls.filter((e) => !e.action).sort((a, b) => b.size - a.size)[0] ??
    null;
  const bodyEl = textEls.find((e) => e !== headingEl) ?? headingEl;

  let accentColor = layout.highlight.color;
  const settingsMenu = layout.menus.find((m) => m.role === "settings");
  if (settingsMenu) {
    const subHead = settingsMenu.elements.find(
      (e): e is TextElement => e.type === "text" && !e.action && e.y >= H * 0.15 && e.color !== (bodyEl?.color ?? "#FFFFFF"),
    );
    if (subHead) accentColor = subHead.color;
  }

  const bg = firstTitle?.background;
  const bgColor = bg?.color ?? "#14192B";
  const bgGradient = bg?.gradientOverlay ?? "none";
  const flatBg = bgGradient === "none" && !bg?.video && bgColor !== "#14192B";

  return {
    bgColor,
    bgGradient,
    flatBg,
    font: bodyEl?.font ?? "Arial",
    textColor: bodyEl?.color ?? "#FFFFFF",
    accentColor,
    italic: bodyEl?.italic ?? false,
    shadow: bodyEl?.shadow ?? true,
    heading: headingEl
      ? {
          x: headingEl.x,
          y: headingEl.y,
          w: headingEl.w,
          h: headingEl.h,
          font: headingEl.font,
          size: headingEl.size,
          color: headingEl.color,
          align: headingEl.align,
          bold: headingEl.bold,
          italic: headingEl.italic,
          shadow: headingEl.shadow,
        }
      : null,
  };
}

/** Candidate 3x2 title slots for `page`, respecting existing thumbnail geometry if present. */
function candidateTitleSlots(page: CustomMenu, canvas: { width: number; height: number }, tv: string): TitleSlot[] {
  const W = canvas.width;
  const H = canvas.height;
  const sx = W / 720;
  const sy = H / 576;
  const isBd = isBdFrame(tv) || W > 720;

  const stdVidW = isBd ? Math.round((400 * W) / 1920) : Math.round(180 * sx);
  const stdVidH = isBd ? Math.round((225 * H) / 1080) : Math.round(112 * sy);
  const stdCapW = isBd ? Math.round(180 * sx) : stdVidW;
  const capH = Math.round(30 * sy);
  const fontSize = Math.round(18 * (isBd ? sy : 1));

  // If existing playTitle video thumbnails use a custom size, try that geometry first.
  const existingVids = page.elements.filter((e): e is VideoElement => e.type === "video" && e.action?.type === "playTitle");
  const dims: [number, number, number][] = [];
  if (existingVids.length > 0) {
    const widths = existingVids.map((v) => Math.round(v.w)).sort((a, b) => a - b);
    const heights = existingVids.map((v) => Math.round(v.h)).sort((a, b) => a - b);
    const medW = widths[Math.floor(widths.length / 2)];
    const medH = heights[Math.floor(heights.length / 2)];
    if (medW !== stdVidW || medH !== stdVidH) {
      dims.push([medW, medH, Math.max(medW, stdCapW)]);
    }
  }
  dims.push([stdVidW, stdVidH, stdCapW]);

  const slots: TitleSlot[] = [];
  for (const [vidW, vidH, capW] of dims) {
    const capGap = isBd ? 8 : Math.max(4, Math.round(6 * sy));
    for (let i = 0; i < 6; i++) {
      const x = Math.round((60 + (i % 3) * 210) * sx);
      const y = Math.round((120 + Math.floor(i / 3) * 180) * sy);
      const vid: Box = { x0: x, y0: y, x1: x + vidW, y1: y + vidH };
      const cap: Box = { x0: x, y0: y + vidH + capGap, x1: x + capW, y1: y + vidH + capGap + capH };
      slots.push({ vid, cap, fontSize });
    }
  }
  return slots;
}

/** Finds a collision-free position for a navigation button (`Next` / `Back`). */
function placeNavButton(
  page: CustomMenu,
  canvas: { width: number; height: number },
  tv: string,
  preferRight: boolean,
): { x: number; y: number; w: number; h: number; size: number } | null {
  const W = canvas.width;
  const H = canvas.height;
  const sx = W / 720;
  const sy = H / 576;
  const isBd = isBdFrame(tv) || W > 720;
  const w = Math.round((preferRight ? 160 : 140) * sx);
  const h = Math.round(36 * sy);
  const size = Math.round(22 * (isBd ? sy : 1));
  const blocked = blockingBoxes(page, canvas);

  const candidates: [number, number][] = preferRight
    ? [
        [W - Math.round(220 * sx), Math.round(490 * sy)],
        [Math.round(450 * sx), Math.round(490 * sy)],
        [Math.round(290 * sx), Math.round(490 * sy)],
        [Math.round(60 * sx), Math.round(490 * sy)],
        [W - Math.round(220 * sx), Math.round(36 * sy)],
      ]
    : [
        [Math.round(300 * sx), Math.round(490 * sy)],
        [Math.round(60 * sx), Math.round(490 * sy)],
        [W - Math.round(220 * sx), Math.round(490 * sy)],
        [Math.round(60 * sx), Math.round(36 * sy)],
      ];

  const stepX = Math.max(16, Math.round(20 * sx));
  const stepY = Math.max(16, Math.round(24 * sy));
  for (let y = Math.round(490 * sy); y >= Math.round(36 * sy); y -= stepY) {
    for (let x = W - w - Math.round(20 * sx); x >= Math.round(20 * sx); x -= stepX) {
      candidates.push([x, y]);
    }
  }

  for (const [x, y] of candidates) {
    const box: Box = { x0: x, y0: y, x1: x + w, y1: y + h };
    if (!insideCanvas(box, canvas)) continue;
    if (blocked.some((b) => boxesConflict(box, b, 4))) continue;
    return { x, y, w, h, size };
  }
  return null;
}

/**
 * Incrementally updates a user-edited custom menu layout when titles are added, removed,
 * reordered, merged, or split — preserving surviving menus, positions, styling, and custom elements.
 */
export function syncLayoutWithTitles(
  layout: CustomLayout,
  oldPaths: string[],
  newTitles: EditorTitle[],
  tv: string,
  heading: string,
): CustomLayout {
  const out = structuredClone(layout);
  const usedIds = new Set<string>();
  for (const m of out.menus) {
    usedIds.add(m.id);
    for (const e of m.elements) usedIds.add(e.id);
  }
  const allocId = (prefix: string): string => {
    let id = newId(prefix);
    while (usedIds.has(id)) id = newId(prefix);
    usedIds.add(id);
    return id;
  };

  // Step A: Map surviving old 1-based title indices to new 1-based indices (1-to-1 for duplicates).
  // Unmatched old titles map to 0 so they never collide with remapped surviving indices.
  const { map, matchedNew } = mapPathsOneToOne(oldPaths, newTitles.map((t) => t.path));
  const remapOrZero = (t: number) => map.get(t) ?? 0;
  remapLayoutTitles(out, remapOrZero);
  for (const m of out.menus) {
    for (const e of m.elements) {
      if (e.action?.type === "menu" && e.action.transition) {
        const trn = e.action.transition;
        if (trn.video.source === "title") {
          const next = remapOrZero(trn.video.title);
          if (next === 0) e.action.transition = null;
          else trn.video.title = next;
        }
        if (e.action.transition?.audio?.type === "title") {
          const next = remapOrZero(e.action.transition.audio.title);
          if (next === 0) e.action.transition.audio = { type: "none" };
          else e.action.transition.audio.title = next;
        }
      }
    }
  }
  if (out.fastPlay) {
    const nextTarget = map.get(out.fastPlay.targetTitle);
    out.fastPlay.targetTitle = nextTarget ?? (newTitles.length > 0 ? 1 : 0);
    if (out.fastPlay.splash?.video && !out.fastPlay.splash.video.path) {
      const next = remapOrZero(out.fastPlay.splash.video.title);
      if (next === 0) out.fastPlay.splash.video = null;
      else out.fastPlay.splash.video.title = next;
    }
    if (out.fastPlay.audio?.type === "title") {
      const next = remapOrZero(out.fastPlay.audio.title);
      if (next === 0) out.fastPlay.audio = { type: "none" };
      else out.fastPlay.audio.title = next;
    }
  }

  // Step B: Clean up removed titles and dangling menu/element references.
  const isValidTitle = (t: number | null | undefined): t is number =>
    typeof t === "number" && t >= 1 && t <= newTitles.length;
  const removedMenuIds = new Set<string>();
  out.menus = out.menus.filter((m) => {
    if ((m.role === "root" || m.role === "settings") && !isValidTitle(m.title)) {
      removedMenuIds.add(m.id);
      return false;
    }
    return true;
  });
  const validMenuIds = new Set(out.menus.map((m) => m.id));

  for (const m of out.menus) {
    if (m.background.video && !m.background.video.path && !isValidTitle(m.background.video.title)) {
      m.background.video = null;
    }
    if (m.audio.type === "title" && !isValidTitle(m.audio.title)) {
      m.audio = { type: "none" };
    }
    const removedVideos: VideoElement[] = [];
    const surviving: LayoutElement[] = [];
    for (const e of m.elements) {
      const badVideo = e.type === "video" && !e.path && !isValidTitle(e.title);
      const badPlayTitle = e.action?.type === "playTitle" && !isValidTitle(e.action.title);
      const badPlayChapter =
        e.action?.type === "playChapter" &&
        (!isValidTitle(e.action.title) || e.action.chapter < 1 || e.action.chapter > newTitles[e.action.title - 1].chapters);
      const badMenuLink = e.action?.type === "menu" && (removedMenuIds.has(e.action.menu) || !validMenuIds.has(e.action.menu));
      if (badVideo || badPlayTitle || badPlayChapter || badMenuLink) {
        if (e.type === "video") removedVideos.push(e);
        continue;
      }
      surviving.push(e);
    }
    // Remove unlinked companion captions positioned directly below removed video thumbnails.
    m.elements = surviving.filter((e) => {
      if (e.type !== "text" || e.action) return true;
      return !removedVideos.some(
        (v) =>
          Math.abs(e.x - v.x) <= 4 &&
          e.w >= v.w * 0.8 &&
          e.w <= v.w * 1.3 &&
          e.y >= v.y + v.h - 4 &&
          e.y <= v.y + v.h + Math.max(24, Math.round(v.h * 0.25)),
      );
    });
  }

  // Step C: Add new titles (or titles without any playTitle button / root menu) without touching user edits.
  if (newTitles.length === 0) return out;

  const style = extractLayoutStyle(out);
  const W = out.canvas.width;
  const H = out.canvas.height;
  const sx = W / 720;
  const sy = H / 576;
  const isBd = isBdFrame(tv) || W > 720;

  const landingTheme = out.theme === "landing";
  const landingMenu = landingTheme ? out.menus.find((m) => m.role === "title" && menuGroup(m) === "main") : undefined;
  const iconSide = Math.round(44 * Math.min(sx, sy));
  const navIcon = (name: IconName, x: number, target: string): IconElement =>
    icon(Math.round(x * sx), Math.round(490 * sy), iconSide, iconSide, name, style.textColor, { id: allocId("i"), action: { type: "menu", menu: target } });

  const createTitlePage = (pageNum: number): CustomMenu => {
    if (landingTheme) {
      // Landing theme: an episodes page (heading, home icon), styled like the landing screen.
      const label = pageNum > 1 ? tr("layout.episodesPage", { p: pageNum }) : tr("layout.episodes");
      const page = emptyMenu(label, "title");
      page.id = allocId("m");
      page.group = "episodes";
      page.background = { color: style.bgColor, image: null, video: null, blur: 0, gradientOverlay: style.bgGradient };
      const head = text(Math.round(60 * sx), Math.round(36 * sy), W - Math.round(120 * sx), Math.round(56 * sy), label, {
        id: allocId("t"),
        font: style.font,
        size: Math.round(34 * (isBd ? sy : 1)),
        color: style.textColor,
        bold: true,
        italic: style.italic,
        shadow: style.shadow,
      });
      head.size = fitSize(head.text, head.size, head.w, head.font, head.bold);
      page.elements.push(head);
      if (landingMenu) page.elements.push(navIcon("home", 60, landingMenu.id));
      return page;
    }
    const page = emptyMenu(tr("layout.hauptmenue", { p: pageNum }), "title");
    page.id = allocId("m");
    page.group = out.theme === "landing" ? "episodes" : "main";
    page.background = { color: style.bgColor, image: null, video: null, blur: 0, gradientOverlay: style.bgGradient };
    const hs = style.heading;
    const headingFitsAboveGrid = hs && hs.y + hs.h <= Math.round(112 * sy);
    const titleText = hs
      ? text(
          headingFitsAboveGrid ? hs.x : Math.round(60 * sx),
          headingFitsAboveGrid ? hs.y : Math.round(36 * sy),
          headingFitsAboveGrid ? hs.w : W - Math.round(120 * sx),
          headingFitsAboveGrid ? hs.h : Math.round(56 * sy),
          heading || "DVD",
          {
            font: hs.font,
            size: headingFitsAboveGrid ? hs.size : Math.round(40 * (isBd ? sy : 1)),
            color: hs.color,
            align: hs.align,
            bold: hs.bold,
            italic: hs.italic,
            shadow: hs.shadow,
          },
        )
      : text(
          Math.round(60 * sx),
          Math.round(36 * sy),
          W - Math.round(120 * sx),
          Math.round(56 * sy),
          heading || "DVD",
          {
            font: style.font,
            size: Math.round(40 * (isBd ? sy : 1)),
            color: style.textColor,
            bold: true,
            italic: style.italic,
            shadow: style.shadow,
          },
        );
    titleText.id = allocId("t");
    titleText.size = fitSize(titleText.text, titleText.size, titleText.w, titleText.font, titleText.bold);
    page.elements.push(titleText);
    return page;
  };

  if (!out.menus.some((m) => m.role === "title")) {
    out.menus.unshift(createTitlePage(1));
  }

  // A landing layout needs an episodes page as soon as there are several titles: the thumbnails
  // never go onto the landing screen. It gets an "Episodes" button under the existing stack.
  if (landingMenu && newTitles.length > 1 && !out.menus.some((m) => menuGroup(m) === "episodes")) {
    const page = createTitlePage(1);
    out.menus.splice(out.menus.indexOf(landingMenu) + 1, 0, page);
    const stack = landingMenu.elements.filter((e): e is TextElement => e.type === "text" && !!e.action && e.x < W / 2);
    const lowest = stack.reduce<TextElement | null>((a, e) => (!a || e.y > a.y ? e : a), null);
    if (lowest) {
      const y = lowest.y + Math.round(54 * sy);
      if (y + lowest.h <= H) {
        landingMenu.elements.push({ ...structuredClone(lowest), id: allocId("t"), y, text: tr("layout.episodes"), action: { type: "menu", menu: page.id } });
      }
    }
    const play = stack.find((e) => e.action?.type === "playTitle" && e.text === tr("layout.play"));
    if (play) {
      play.text = tr("layout.alleAbspielen");
      play.action = { type: "playAll" };
    }
  }

  const hasPlayTitle = (titleIdx: number) =>
    out.menus.some((m) => m.role === "title" && m.elements.some((e) => e.action?.type === "playTitle" && e.action.title === titleIdx));
  const hasRootMenu = (titleIdx: number) => out.menus.some((m) => m.role === "root" && m.title === titleIdx);

  // Layouts from before the themes are classic with per-title settings.
  const nav: SubNav = { theme: out.theme ?? "classic", settingsMode: out.settingsMode ?? "perTitle" };
  const globalSettings = nav.settingsMode === "global";
  const btnLimit = maxButtons(tv);
  const subStyle: SubmenuStyle = {
    bgColor: style.bgColor,
    flatBg: style.flatBg,
    font: style.font,
    textColor: style.textColor,
    accentColor: style.accentColor,
    italic: style.italic,
    shadow: style.shadow,
  };

  for (const t of newTitles) {
    const needsAdd = !matchedNew.has(t.index) || (!hasPlayTitle(t.index) && !hasRootMenu(t.index));
    if (!needsAdd) continue;

    const needsVtsm = t.chapters > 1 || (!globalSettings && needsSettings(t));
    const neededBtns = needsVtsm ? 2 : 1;

    // New thumbnails go to the last episodes page (landing) or title page (classic), never to
    // the landing screen or the global settings menu while another page exists.
    const findLastTitlePageIdx = () => {
      const last = (ok: (m: CustomMenu) => boolean) => out.menus.reduce((idx, m, i) => (m.role === "title" && ok(m) ? i : idx), -1);
      const episodes = last((m) => menuGroup(m) === "episodes");
      return episodes >= 0 ? episodes : last((m) => menuGroup(m) !== "settings");
    };

    const pickSlotOn = (page: CustomMenu): TitleSlot | null => {
      const titleEntries = page.elements.filter((e) => e.action?.type === "playTitle").length;
      const btnCount = page.elements.filter(isButton).length;
      if (titleEntries >= 6 || btnCount + neededBtns > btnLimit) return null;
      const blocked = blockingBoxes(page, out.canvas);
      for (const s of candidateTitleSlots(page, out.canvas, tv)) {
        if (!insideCanvas(s.vid, out.canvas) || !insideCanvas(s.cap, out.canvas)) continue;
        if (s.cap.y0 < s.vid.y1 + 4) continue;
        if (blocked.some((b) => boxesConflict(s.vid, b, 4) || boxesConflict(s.cap, b, 4))) continue;
        return s;
      }
      return null;
    };

    let pageIdx = findLastTitlePageIdx();
    let targetPage = out.menus[pageIdx];
    let slot = pickSlotOn(targetPage);

    if (!slot) {
      const titleCount = out.menus.filter((m) => m.role === "title" && (!landingTheme || menuGroup(m) === "episodes")).length;
      const newPage = createTitlePage(titleCount + 1);
      out.menus.splice(pageIdx + 1, 0, newPage);

      if (landingTheme) {
        targetPage.elements.push(navIcon("next", 616, newPage.id));
        newPage.elements.push(navIcon("prev", 548, targetPage.id));
      }
      if (!landingTheme && targetPage.elements.filter(isButton).length < btnLimit) {
        const nextPos = placeNavButton(targetPage, out.canvas, tv, true);
        if (nextPos) {
          const nextBtn = text(nextPos.x, nextPos.y, nextPos.w, nextPos.h, tr("layout.weiter"), {
            id: allocId("t"),
            font: style.font,
            size: fitSize(tr("layout.weiter"), nextPos.size, nextPos.w, style.font, false),
            color: style.textColor,
            align: "right",
            italic: style.italic,
            shadow: style.shadow,
            action: { type: "menu", menu: newPage.id },
          });
          targetPage.elements.push(nextBtn);
        }
      }

      const backPos = placeNavButton(newPage, out.canvas, tv, false);
      if (!landingTheme && backPos) {
        const backBtn = text(backPos.x, backPos.y, backPos.w, backPos.h, tr("layout.zurueck"), {
          id: allocId("t"),
          font: style.font,
          size: fitSize(tr("layout.zurueck"), backPos.size, backPos.w, style.font, false),
          color: style.textColor,
          align: "center",
          italic: style.italic,
          shadow: style.shadow,
          action: { type: "menu", menu: targetPage.id },
        });
        newPage.elements.push(backBtn);
      }

      targetPage = newPage;
      slot = pickSlotOn(targetPage);
    }

    if (!slot) continue;

    let rootId: string | null = null;
    if (needsVtsm) {
      rootId = allocId("m");
      const submenus = buildTitleSubmenus(t, targetPage.id, out.canvas, tv, rootId, allocId, subStyle, nav);
      out.menus.push(...submenus);
    }

    const vidEl = video(
      slot.vid.x0,
      slot.vid.y0,
      slot.vid.x1 - slot.vid.x0,
      slot.vid.y1 - slot.vid.y0,
      t.index,
      null,
      { type: "playTitle", title: t.index },
    );
    vidEl.id = allocId("v");

    const captionLabel = rootId ? `${t.label} · ${t.chapters > 1 ? tr("layout.kapitel2") : tr("layout.menue")}` : t.label;
    const capW = slot.cap.x1 - slot.cap.x0;
    const capEl = text(slot.cap.x0, slot.cap.y0, capW, slot.cap.y1 - slot.cap.y0, captionLabel, {
      id: allocId("t"),
      font: style.font,
      size: fitSize(captionLabel, slot.fontSize, capW, style.font, false),
      color: style.textColor,
      italic: style.italic,
      shadow: style.shadow,
      action: rootId ? { type: "menu", menu: rootId } : null,
    });

    targetPage.elements.push(vidEl, capEl);
  }

  return out;
}
