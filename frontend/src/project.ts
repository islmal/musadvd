// `.musadvd` project files: format, validation, dialogs and the recovery autosave. The state <-> file
// conversion lives in main.ts (it owns the `Title` type); everything that can be checked without the
// state is here.
import { open, save } from "@tauri-apps/plugin-dialog";
import { autosavePath, loadProject, saveProject, type Crop, type SubtitleStyle } from "./api";
import { t as tr } from "./i18n";
import { AFTER_PLAYBACK, type AfterPlayback, type CustomLayout } from "./layout";
import { modal } from "./studio/ui";

export const PROJECT_FORMAT = "musadvd-project";
export const PROJECT_VERSION = 1;
const EXT = "musadvd";

// Same allow-lists as main.ts `loadSettings`.
const DISCS = ["dvd5", "dvd9", "bd25", "bd50", "avchd5", "avchd9", "custom"] as const;
const COPY_MODES = ["encode", "auto", "strict"] as const;
const BD_AUDIO_MODES = ["auto", "encode", "strict"] as const;

export type ProjectDisc = (typeof DISCS)[number];

export interface ProjectExternalSubtitle {
  path: string;
  language: string | null;
  title: string | null;
  forced: boolean;
  enabled: boolean;
}

export interface ProjectTitle {
  path: string;
  audio: number[];
  subs: number[];
  /** Display (= disc) order of all audio / embedded subtitle stream indices; omitted = source order. */
  audioOrder?: number[];
  subOrder?: number[];
  /** Disc order of embedded ("s") and external ("x") subtitles; omitted = embedded first. */
  subKinds?: string;
  label: string;
  chapters: number[];
  chapterNames?: (string | null)[];
  trimStart: number | null;
  trimEnd: number | null;
  crop: Crop | null;
  mergeWithPrevious: boolean;
  audioNames: Record<number, string>;
  bdResolution: "1080" | "720" | null;
  bdRate: string | null;
  bdVideo: "encode" | "copy" | null;
  dvdVideo?: "encode" | "auto" | "copy" | null;
  bdQuality: "low" | "high" | null;
  denoise: number | null;
  brightness: number | null;
  contrast: number | null;
  saturation: number | null;
  subtitleStyle: SubtitleStyle;
  /** Omitted when "default". */
  afterPlayback?: AfterPlayback;
  externalSubtitles?: ProjectExternalSubtitle[];
}

export interface ProjectFile {
  format: typeof PROJECT_FORMAT;
  version: number;
  savedAt: string;
  disc: ProjectDisc;
  tvStandard: "PAL" | "NTSC" | null;
  /** DVD video mode; null = not in the file, so the user's setting stays untouched. */
  dvdVideo: "encode" | "auto" | "strict" | null;
  twoPass: boolean;
  motion: boolean;
  bdVideo: (typeof COPY_MODES)[number];
  bdAudio: (typeof BD_AUDIO_MODES)[number];
  template: string;
  /** DVD slot's menu mode; the Blu-ray slot's is `menuModeBd`. */
  menuMode: "template" | "custom";
  menuModeBd: "template" | "custom";
  customLayout: CustomLayout | null;
  customLayoutBd: CustomLayout | null;
  /** `titlesKey` the layouts were made for (null = unknown). */
  layoutKey: string | null;
  layoutKeyBd: string | null;
  /** Whether the custom layout was explicitly edited by the user (missing in legacy v1 = true when layout is non-null). */
  layoutEdited?: boolean;
  layoutEditedBd?: boolean;
  menuTitle: string;
  menuTitleAuto: boolean;
  output: string;
  titles: ProjectTitle[];
  /** Project settings (`PROJECT_SETTING_KEYS` of the app settings); absent in older files. */
  settings?: Record<string, string | number | boolean>;
}

// --- validation -------------------------------------------------------------------------------

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown, d = "") => (typeof v === "string" ? v : d);
const nstr = (v: unknown) => (typeof v === "string" ? v : null);
const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : null);
const pick = <T extends string>(v: unknown, list: readonly T[], d: T): T => (list.includes(v as T) ? (v as T) : d);
const ints = (v: unknown) => (Array.isArray(v) ? v.filter((n): n is number => Number.isInteger(n) && n >= 0) : []);
const clamp = (n: number | null, lo: number, hi: number) => (n === null ? undefined : Math.min(hi, Math.max(lo, n)));

function layoutOf(v: unknown): CustomLayout | null {
  return isObj(v) && Array.isArray(v.menus) && isObj(v.canvas) ? (v as unknown as CustomLayout) : null;
}

function styleOf(v: unknown): SubtitleStyle {
  if (!isObj(v)) return {};
  const s: SubtitleStyle = {};
  const family = nstr(v.fontFamily);
  if (family) s.fontFamily = family;
  const set = <K extends "fontSizePct" | "outlinePx" | "bottomMarginPct" | "offsetMs">(k: K, lo: number, hi: number) => {
    const n = clamp(num(v[k]), lo, hi);
    if (n !== undefined) s[k] = n;
  };
  set("fontSizePct", 50, 200);
  set("outlinePx", 0, 6);
  set("bottomMarginPct", 0, 30);
  set("offsetMs", -60000, 60000);
  return s;
}

function cropOf(v: unknown): Crop | null {
  if (!isObj(v)) return null;
  const [top, bottom, left, right] = [v.top, v.bottom, v.left, v.right].map(num);
  return top === null || bottom === null || left === null || right === null ? null : { top, bottom, left, right };
}

function externalSubsOf(v: unknown): ProjectExternalSubtitle[] | undefined {
  if (!Array.isArray(v)) return undefined;
  return v
    .filter((e): e is Record<string, unknown> => isObj(e) && typeof e.path === "string" && e.path.length > 0)
    .map((e) => ({
      path: String(e.path),
      language: nstr(e.language),
      title: nstr(e.title),
      forced: e.forced === true,
      enabled: e.enabled !== false,
    }));
}

function titleOf(v: unknown): ProjectTitle | null {
  if (!isObj(v) || typeof v.path !== "string" || !v.path) return null;
  const names: Record<number, string> = {};
  if (isObj(v.audioNames)) for (const [k, n] of Object.entries(v.audioNames)) if (typeof n === "string" && /^\d+$/.test(k)) names[Number(k)] = n;
  const rawChapters = Array.isArray(v.chapters) ? v.chapters : [];
  const rawChapterNames = Array.isArray(v.chapterNames) ? v.chapterNames : null;
  const chapters: number[] = [];
  const chapterNames: (string | null)[] | undefined = rawChapterNames ? [] : undefined;
  rawChapters.forEach((c, idx) => {
    if (typeof c === "number" && Number.isFinite(c) && c >= 0) {
      chapters.push(c);
      if (chapterNames && rawChapterNames) {
        const n = rawChapterNames[idx];
        chapterNames.push(typeof n === "string" && n.trim() ? n.trim() : null);
      }
    }
  });
  const externalSubtitles = externalSubsOf(v.externalSubtitles);
  const afterPlayback = pick(v.afterPlayback, AFTER_PLAYBACK, "default");
  return {
    path: v.path,
    audio: ints(v.audio),
    subs: ints(v.subs),
    ...(Array.isArray(v.audioOrder) ? { audioOrder: ints(v.audioOrder) } : {}),
    ...(Array.isArray(v.subOrder) ? { subOrder: ints(v.subOrder) } : {}),
    label: str(v.label),
    chapters,
    ...(chapterNames ? { chapterNames } : {}),
    trimStart: num(v.trimStart),
    trimEnd: num(v.trimEnd),
    crop: cropOf(v.crop),
    mergeWithPrevious: v.mergeWithPrevious === true,
    audioNames: names,
    bdResolution: v.bdResolution === "1080" || v.bdResolution === "720" ? v.bdResolution : null,
    bdRate: nstr(v.bdRate),
    bdVideo: v.bdVideo === "encode" || v.bdVideo === "copy" ? v.bdVideo : null,
    ...(v.dvdVideo === "encode" || v.dvdVideo === "auto" || v.dvdVideo === "copy" ? { dvdVideo: v.dvdVideo } : {}),
    bdQuality: v.bdQuality === "low" || v.bdQuality === "high" ? v.bdQuality : null,
    denoise: num(v.denoise),
    brightness: num(v.brightness),
    contrast: num(v.contrast),
    saturation: num(v.saturation),
    subtitleStyle: styleOf(v.subtitleStyle),
    ...(afterPlayback !== "default" ? { afterPlayback } : {}),
    ...(externalSubtitles ? { externalSubtitles } : {}),
    ...(typeof v.subKinds === "string" && /^[sx]*$/.test(v.subKinds) ? { subKinds: v.subKinds } : {}),
  };
}

/** Validates a loaded JSON value; throws an Error with a user-facing message for foreign or newer files. */
export function parseProject(raw: unknown): ProjectFile {
  if (!isObj(raw) || raw.format !== PROJECT_FORMAT) throw new Error(tr("project.notAProject"));
  if (typeof raw.version !== "number" || raw.version < 1 || raw.version > PROJECT_VERSION) {
    throw new Error(tr("project.badVersion", { version: String(raw.version) }));
  }
  const mode = (v: unknown) => (v === "template" ? "template" : "custom");
  const customLayout = layoutOf(raw.customLayout);
  const customLayoutBd = layoutOf(raw.customLayoutBd);
  const editedOf = (v: unknown, layout: CustomLayout | null): boolean => (typeof v === "boolean" ? v : layout !== null);
  return {
    format: PROJECT_FORMAT,
    version: raw.version,
    savedAt: str(raw.savedAt),
    disc: pick(raw.disc, DISCS, "dvd5"),
    tvStandard: raw.tvStandard === "PAL" || raw.tvStandard === "NTSC" ? raw.tvStandard : null,
    dvdVideo: raw.dvdVideo === "encode" || raw.dvdVideo === "auto" || raw.dvdVideo === "strict" ? raw.dvdVideo : null,
    twoPass: raw.twoPass === true,
    motion: raw.motion === true,
    bdVideo: pick(raw.bdVideo, COPY_MODES, "encode"),
    bdAudio: pick(raw.bdAudio, BD_AUDIO_MODES, "auto"),
    template: str(raw.template),
    menuMode: mode(raw.menuMode),
    menuModeBd: mode(raw.menuModeBd),
    customLayout,
    customLayoutBd,
    layoutKey: nstr(raw.layoutKey),
    layoutKeyBd: nstr(raw.layoutKeyBd),
    layoutEdited: editedOf(raw.layoutEdited, customLayout),
    layoutEditedBd: editedOf(raw.layoutEditedBd, customLayoutBd),
    menuTitle: str(raw.menuTitle),
    menuTitleAuto: raw.menuTitleAuto !== false,
    output: str(raw.output),
    titles: (Array.isArray(raw.titles) ? raw.titles : []).map(titleOf).filter((t): t is ProjectTitle => t !== null),
    // Plain values only; which keys apply and their types are checked against the live settings on load.
    ...(isObj(raw.settings)
      ? {
          settings: Object.fromEntries(
            Object.entries(raw.settings).filter(([, v]) => typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v))),
          ) as Record<string, string | number | boolean>,
        }
      : {}),
  };
}

/**
 * `current` with the project's saved settings applied: only `keys`, and only values of the same
 * type as the current one (the backend sanitises the rest on save).
 */
export function withProjectSettings<S extends object>(current: S, saved: Record<string, unknown> | undefined, keys: readonly string[]): S {
  const out = { ...current } as Record<string, unknown>;
  for (const k of keys) {
    if (saved && k in saved && typeof saved[k] === typeof out[k]) out[k] = saved[k];
  }
  return out as S;
}

/** Files a layout reads from disk (menu sound, background image/video, external video elements). */
export function layoutFiles(l: CustomLayout | null): string[] {
  if (!l) return [];
  const out: (string | null | undefined)[] = [];
  const bg = (b: CustomLayout["menus"][number]["background"] | null | undefined) => b && out.push(b.image, b.video?.path);
  const sound = (a: CustomLayout["menus"][number]["audio"] | undefined) => a?.type === "file" && out.push(a.path);
  for (const m of l.menus) {
    bg(m.background);
    sound(m.audio);
    for (const e of m.elements) {
      if (e.type === "image") out.push(e.src);
      else if (e.type === "video") out.push(e.path);
    }
  }
  bg(l.fastPlay?.splash);
  sound(l.fastPlay?.audio);
  return [...new Set(out.filter((p): p is string => !!p && !/^(data|https?|blob):/i.test(p)))];
}

/** Content identity without the timestamp: autosave skips states that equal the last saved file. */
export const fingerprint = (p: ProjectFile) => JSON.stringify({ ...p, savedAt: "" });

// --- dialogs ----------------------------------------------------------------------------------

export async function pickOpenPath(): Promise<string | null> {
  const p = await open({ multiple: false, title: tr("project.openTitle"), filters: [{ name: tr("project.filterName"), extensions: [EXT] }] });
  return typeof p === "string" ? p : null;
}

export async function pickSavePath(current: string | null, suggestedName: string): Promise<string | null> {
  const safe = suggestedName.replace(/[\\/:*?"<>|]+/g, " ").trim() || "project";
  const p = await save({
    title: tr("project.saveTitle"),
    defaultPath: current ?? `${safe}.${EXT}`,
    filters: [{ name: tr("project.filterName"), extensions: [EXT] }],
  });
  if (!p) return null;
  return p.toLowerCase().endsWith(`.${EXT}`) ? p : `${p}.${EXT}`;
}

/** Lists everything the project referenced that does not exist (titles/subtitles skipped, layout files flagged). */
export function showMissing(titlePaths: string[], layoutPaths: string[], externalSubPaths: string[] = []) {
  const body = document.createElement("div");
  for (const [heading, paths] of [
    [tr("project.missingTitles"), titlePaths],
    [tr("project.missingSubtitles"), externalSubPaths],
    [tr("project.missingLayoutFiles"), layoutPaths],
  ] as const) {
    if (paths.length === 0) continue;
    const ul = Object.assign(document.createElement("ul"), { className: "missing-list" });
    for (const p of paths) ul.append(Object.assign(document.createElement("li"), { textContent: p }));
    body.append(Object.assign(document.createElement("h3"), { textContent: heading }), ul);
  }
  modal(tr("project.missingHeading"), body, undefined, true);
}

// --- file access + autosave -------------------------------------------------------------------

export async function readProject(path: string): Promise<ProjectFile> {
  return parseProject(await loadProject(path));
}

export const writeProject = (path: string, p: ProjectFile) => saveProject(path, p);

/** The recovery file if it holds a project with titles; silent null otherwise. */
export async function readAutosave(): Promise<ProjectFile | null> {
  try {
    const p = parseProject(await loadProject(await autosavePath()));
    return p.titles.length > 0 ? p : null;
  } catch {
    return null;
  }
}

/** An empty project replaces the recovery file (no delete command): nothing is offered next start. */
export async function clearAutosave() {
  try {
    await saveProject(await autosavePath(), { format: PROJECT_FORMAT, version: PROJECT_VERSION, titles: [] });
  } catch (e) {
    console.warn("autosave clear failed", e);
  }
}

/** Debounced recovery writer: `touch()` after every change; `build` returns null while nothing should be written. */
export function createAutosave(build: () => ProjectFile | null, cleanPrint: () => string) {
  let timer: number | undefined;
  return () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(async () => {
      try {
        const p = build();
        if (!p || fingerprint(p) === cleanPrint()) return;
        await saveProject(await autosavePath(), p);
      } catch (e) {
        console.warn("autosave failed", e);
      }
    }, 5000);
  };
}
