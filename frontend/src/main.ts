import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getVersion } from "@tauri-apps/api/app";
import { openSettings } from "./settings";
import { openBurner } from "./burn";
import { chapterSegments, disposePlayer, renderEditTab } from "./edit";
import { emitTo, listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { EditorInit } from "./menuEditor";
import { FONTS, cleanTitle, isButton, isMoving, madeForOtherTitles, menuClipSecs, remapLayoutByPaths, titlesKey, type CustomLayout, type EditorTitle } from "./layout";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { clearAutosave, createAutosave, fingerprint, layoutFiles, pickOpenPath, pickSavePath, readAutosave, readProject, showMissing, writeProject, PROJECT_FORMAT, PROJECT_VERSION, type ProjectFile, type ProjectTitle } from "./project";
import { confirmPreflight } from "./preflight";
import { check } from "@tauri-apps/plugin-updater";
import { moveItem, shapeMismatch } from "./order";
import { openUpdateDialog } from "./updateDialog";
import {
  checkLayout,
  calculateProject,
  cancelConversion,
  onBuildCancelled,
  defaultOutputDir,
  listTemplates,
  onBuildDone,
  onBuildEvent,
  probeFile,
  listSystemFonts,
  missingPaths,
  startConversion,
  getSettings,
  settingsInfo,
  availableLanguages,
  saveSettings as storeSettings,
  startupFiles,
  startupBurn,
  templatePreview,
  type AppSettings,
  type AudioChoice,
  type AudioTrack,
  type BuildEvent,
  type CalculationResult,
  type ConversionDone,
  type Crop,
  type Eta,
  formatRemaining,
  type Disc,
  type SubtitleStyle,
  type SystemFont,
  type TemplateInfo,
  type VideoMetadata,
} from "./api";
import { applyStatic, fmtNumber, loadLanguage, t as tr } from "./i18n";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const MAX_AUDIO = 8;
const VIDEO_EXTENSIONS = ["mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "mts", "mpg", "mpeg", "vob", "wmv", "webm", "flv", "divx", "ogm"];

interface Title {
  id: number;
  meta: VideoMetadata;
  /** ffprobe stream indices of checked audio tracks. */
  audio: Set<number>;
  /** ffprobe stream indices of enabled subtitles. */
  subs: Set<number>;
  label: string;
  /** Chapter starts in seconds of the source file (first is 0). */
  chapters: number[];
  /** Used section in seconds of the source (null = file start / end). */
  trimStart: number | null;
  trimEnd: number | null;
  /** Picture edges cut away (source pixels). */
  crop: Crop | null;
  /** Plays together with the previous title behind one menu entry (ignored for the first title). */
  mergeWithPrevious: boolean;
  /** Own audio track names by ffprobe stream index (empty = generated label). */
  audioNames: Record<number, string>;
  /** Blu-ray overrides (null = project setting / automatic). */
  bdResolution: BdRes | null;
  bdRate: string | null;
  bdVideo: "encode" | "copy" | null;
  /** Share of the disc bitrate (null = normal). */
  bdQuality: "low" | "high" | null;
  /** Picture filters: denoise (hqdn3d luma, null = off) and eq adjustments (null = neutral). */
  denoise: number | null;
  brightness: number | null;
  contrast: number | null;
  saturation: number | null;
  /** Look and timing of this title's text subtitles (empty = defaults). */
  subtitleStyle: SubtitleStyle;
}

type BdRes = "1080" | "720";
/** Frame rates a Blu-ray title may have per frame size (engine `BdRate::allowed`). */
const BD_RATES: Record<BdRes, string[]> = { "1080": ["23.976", "24", "25i", "29.97i"], "720": ["23.976", "24", "50", "59.94"] };

type Tab = "audio" | "subtitles" | "chapters" | "edit" | "video" | "template";

type DiscChoice = "dvd5" | "dvd9" | "bd25" | "bd50" | "avchd5" | "avchd9" | "custom";

const state = {
  titles: [] as Title[],
  selected: null as number | null,
  tab: "audio" as Tab,
  disc: "dvd5" as DiscChoice,
  settings: null as AppSettings | null,
  twoPass: false,
  motion: false,
  /** Blu-ray/AVCHD video: re-encode, copy compliant streams, or copy all (engine `BdVideoMode`). */
  bdVideo: "encode" as BdVideo,
  /** Blu-ray/AVCHD audio: copy compliant tracks and encode the rest, always encode, or copy all. */
  bdAudio: "auto" as BdAudio,
  templates: [] as TemplateInfo[],
  template: "",
  /** Menus from a ConvertX template or from the menu editor (the editor is the default; templates need ConvertXToDVD). */
  menuMode: "custom" as "template" | "custom",
  customLayout: null as CustomLayout | null,
  /** `titlesKey` of the videos the custom layout was made for (null = unknown). */
  customLayoutKey: null as string | null,
  /** Problems of the custom layout with the current titles (empty = ready). */
  layoutErrors: [] as string[],
  /** Heading of the main menu = project name (also the name of the DVD folder). */
  menuTitle: "",
  /** The heading follows the first video's name until the user types their own. */
  menuTitleAuto: true,
  output: "",
  calc: null as CalculationResult | null,
  probing: 0,
  converting: false,
  /** One-step undo of "In separate Titel aufteilen". */
  splitUndo: null as { original: Title; ids: number[]; index: number } | null,
};
let nextId = 1;

type BdVideo = "encode" | "auto" | "strict";
type BdAudio = "auto" | "encode" | "strict";
const SETTINGS_KEY = "musadvd-gui-settings";
const LAYOUT_KEY = "musadvd-menu-layout";

/** DVD and Blu-ray menus are designed separately (4:3 TV frame vs 16:9 HD frame). */
type Medium = "dvd" | "bd";
type MenuSlot = { layout: CustomLayout | null; key: string | null; mode: "template" | "custom" };
const mediumOf = (d: DiscChoice): Medium => (["bd25", "bd50", "avchd5", "avchd9"].includes(d) ? "bd" : "dvd");
const slots: Record<Medium, MenuSlot> = {
  dvd: { layout: null, key: null, mode: "custom" },
  bd: { layout: null, key: null, mode: "custom" },
};
/** localStorage key suffix of a medium's layout (the DVD keys predate the Blu-ray ones). */
const slotKey = (m: Medium) => (m === "bd" ? `${LAYOUT_KEY}-bd` : LAYOUT_KEY);

/** Files the live menu state under `from` and loads the one of `to` (called on disc changes). */
function switchMedium(from: Medium, to: Medium) {
  if (from === to) return;
  slots[from] = { layout: state.customLayout, key: state.customLayoutKey, mode: state.menuMode };
  const next = slots[to];
  state.customLayout = next.layout;
  state.customLayoutKey = next.key;
  state.menuMode = next.mode;
  state.layoutErrors = [];
}

function loadSettings() {
  try {
    const s = JSON.parse(localStorage.getItem(SETTINGS_KEY) ?? "{}");
    if (["dvd5", "dvd9", "bd25", "bd50", "avchd5", "avchd9", "custom"].includes(s.disc)) state.disc = s.disc;
    state.twoPass = !!s.twoPass;
    state.motion = !!s.motion;
    if (["encode", "auto", "strict"].includes(s.bdVideo)) state.bdVideo = s.bdVideo;
    if (["encode", "auto", "strict"].includes(s.bdAudio)) state.bdAudio = s.bdAudio;
    if (typeof s.template === "string") state.template = s.template;
    if (typeof s.output === "string") state.output = s.output;
    slots.dvd.mode = s.menuMode === "template" ? "template" : "custom";
    slots.bd.mode = s.menuModeBd === "template" ? "template" : "custom";
    for (const m of ["dvd", "bd"] as const) {
      const layout = localStorage.getItem(slotKey(m));
      if (layout) slots[m].layout = JSON.parse(layout) as CustomLayout;
      slots[m].key = localStorage.getItem(`${slotKey(m)}-titles`);
    }
    const live = slots[mediumOf(state.disc)];
    [state.menuMode, state.customLayout, state.customLayoutKey] = [live.mode, live.layout, live.key];
  } catch {
    /* storage unavailable: defaults */
  }
}

function saveSettings() {
  try {
    // The heading is project-specific and therefore not remembered between sessions.
    const { disc, twoPass, motion, bdVideo, bdAudio, template, output } = state;
    slots[mediumOf(disc)] = { layout: state.customLayout, key: state.customLayoutKey, mode: state.menuMode };
    localStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({ disc, twoPass, motion, bdVideo, bdAudio, template, output, menuMode: slots.dvd.mode, menuModeBd: slots.bd.mode }),
    );
    for (const m of ["dvd", "bd"] as const) {
      const { layout, key } = slots[m];
      if (layout) localStorage.setItem(slotKey(m), JSON.stringify(layout));
      else localStorage.removeItem(slotKey(m));
      if (layout && key !== null) localStorage.setItem(`${slotKey(m)}-titles`, key);
      else localStorage.removeItem(`${slotKey(m)}-titles`);
    }
  } catch {
    /* ignore */
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

type Child = Node | string | null | undefined | false;

/** Creates an element; strings become text nodes (never parsed as HTML). */
function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<Record<string, unknown>> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = String(v);
    else if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v as EventListener);
    else if (k in el && typeof v !== "string") (el as unknown as Record<string, unknown>)[k] = v;
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) if (c !== null && c !== undefined && c !== false) el.append(c);
  return el;
}

function fmtTime(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  const hh = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  return `${hh}:${String(mm).padStart(2, "0")}:${String(ss).padStart(2, "0")}`;
}

/** Parses `h:mm:ss`, `mm:ss` or seconds; null if invalid. */
function parseTime(text: string): number | null {
  const parts = text.trim().replace(",", ".").split(":");
  if (parts.length === 0 || parts.length > 3) return null;
  let total = 0;
  for (const p of parts) {
    if (!/^\d+(\.\d+)?$/.test(p)) return null;
    total = total * 60 + Number(p);
  }
  return total;
}

const fmtNum = (n: number, digits = 0) =>
  fmtNumber(n, digits);

const selectedTitle = () => state.titles.find((t) => t.id === state.selected) ?? null;

/** Disc for the engine; "custom" uses the target size from the settings. */
function currentDisc(): Disc {
  if (state.disc === "custom") return { custom: state.settings?.target_size_mb ?? 4300 };
  return state.disc;
}

/** A target built like a Blu-ray (BDMV): BD-25/50 and AVCHD. Without editor menus it autoplays. */
const isBluray = () => mediumOf(state.disc) === "bd";

/** AVCHD ("BD5/BD9"): the Blu-ray structure on a DVD. */
const isAvchd = () => state.disc === "avchd5" || state.disc === "avchd9";

/** Disc format the audio plans are looked up by. */
const planKey = (): "dvd" | "bluray" | "avchd" => (isAvchd() ? "avchd" : isBluray() ? "bluray" : "dvd");

const isTrimmed = (t: Title) => t.trimStart !== null || t.trimEnd !== null;

/** Runtime of the used section. */
const effDuration = (t: Title) => (t.trimEnd ?? t.meta.durationSecs) - (t.trimStart ?? 0);

/** Chapter starts relative to the used section (the engine counts from the section's start). */
function effChapters(t: Title): number[] {
  const all = [...t.chapters].sort((a, b) => a - b);
  if (!isTrimmed(t)) return all;
  const start = t.trimStart ?? 0;
  const end = t.trimEnd ?? t.meta.durationSecs;
  return [0, ...all.filter((c) => c - start > 0.5 && c < end - 1).map((c) => c - start)];
}

/** What happens to an audio track with the current disc and settings (copy, AC-3 or LPCM). */
function audioChoice(t: Title, a: AudioTrack): AudioChoice {
  const mode = state.settings?.audio_mode ?? "auto";
  if (isBluray()) {
    const plans = a.plans[planKey()];
    // Strict keeps the copy plan so the start gate can flag what cannot be copied.
    if (state.bdAudio === "strict" && !bdAudioForced(t)) return plans.auto;
    return audioCopies(t, a) ? plans.auto : mode === "lpcm" ? plans.lpcm : plans.ac3;
  }
  // Normalised and trimmed tracks are always re-encoded (the engine does the same).
  const forced = mode === "auto" && (!!state.settings?.loudnorm || isTrimmed(t));
  return a.plans[planKey()][forced ? "ac3" : mode];
}

/** Blu-ray: normalised and trimmed titles never copy audio, whatever the audio setting. */
const bdAudioForced = (t: Title) => !!state.settings?.loudnorm || isTrimmed(t);

/** Blu-ray/AVCHD: is this track copied (losslessly repacked) rather than encoded? */
function audioCopies(t: Title, a: AudioTrack): boolean {
  if (!isBluray()) return audioChoice(t, a).copy;
  const auto = a.plans[planKey()].auto;
  if (state.bdAudio === "encode" || bdAudioForced(t) || !auto.copy) return false;
  // An explicit AC-3 / LPCM setting keeps its format; only strict copies any compliant codec.
  const mode = state.settings?.audio_mode ?? "auto";
  return state.bdAudio === "strict" || mode === "auto" || auto.codec === mode;
}

/** Menu/GUI label of an audio track: the user's own name (plus channel layout) or the engine's label. */
function audioLabel(t: Title, a: AudioTrack): string {
  const own = t.audioNames[a.index];
  if (!own) return audioChoice(t, a).label;
  return state.settings?.track_label_mode === "generic" ? own : `${own} (${audioChoice(t, a).layout})`;
}

const CODEC_NAMES = { ac3: "AC-3", dts: "DTS", lpcm: "LPCM" } as const;

/** The frame menus are designed for: `PAL` / `NTSC`, or `BD1080` / `BD720` on a Blu-ray. */
function frameName(): string {
  if (isBluray()) return state.settings?.bd_resolution === "720p" ? "BD720" : "BD1080";
  return state.settings?.tv_standard ?? "PAL";
}

/** The setting "only one video per Blu-ray" is on and violated. */
const singleVideoViolated = () => isBluray() && !!state.settings?.bd_single_video && state.titles.length > 1;

interface MenuVideoCount {
  videos: number;
  menus: number;
  total: number;
  overLimit: boolean;
}

function getMenuAndVideoCounts(): MenuVideoCount {
  const videos = state.titles.length;
  let menus = 0;
  if (state.menuMode === "custom") {
    menus = state.customLayout ? state.customLayout.menus.filter((m) => m.role === "title").length : 0;
  } else {
    if (videos === 0) {
      menus = 0;
    } else if (videos === 1 && state.settings?.no_title_menu_for_single_title) {
      menus = 0;
    } else {
      const info = state.templates.find((t) => t.path === state.template);
      const perPage = info?.titlesPerPage && info.titlesPerPage > 0 ? info.titlesPerPage : 0;
      menus = perPage > 0 ? Math.max(1, Math.ceil(videos / perPage)) : 1;
    }
  }
  const total = videos + menus;
  const overLimit = !isBluray() && total > 99;
  return { videos, menus, total, overLimit };
}

function updateCustomDiscLabel() {
  const mb = state.settings?.target_size_mb ?? 4300;
  $("disc-custom").textContent = tr("main.benutzerdefiniertMb", { fmtNum: fmtNum(mb) });
}

function audioKbps(t: Title): number {
  return t.meta.audio.filter((a) => t.audio.has(a.index)).reduce((sum, a) => sum + audioChoice(t, a).kbps, 0);
}

const RELEASES = "https://github.com/islmal/musadvd/releases/latest";

/** Linux (.deb) has no in-app installer, so it only gets a notice: one anonymous GET of the latest GitHub release, shown in the status bar when newer. Silent on any failure. */
async function notifyNewRelease(current: string) {
  try {
    const r = await fetch("https://api.github.com/repos/islmal/musadvd/releases/latest", { headers: { Accept: "application/vnd.github+json" } });
    if (!r.ok) return;
    const latest = String((await r.json()).tag_name ?? "").replace(/^v/, "");
    const num = (s: string) => s.split(".").map((n) => parseInt(n, 10) || 0);
    const [a, b] = [num(latest), num(current)];
    const newer = a.some((n, i) => n !== (b[i] ?? 0) && n > (b[i] ?? 0) && a.slice(0, i).every((m, j) => m === (b[j] ?? 0)));
    if (latest && newer) status(tr("main.updateAvailable", { version: latest, url: RELEASES }));
  } catch {
    /* offline or rate-limited: no notice */
  }
}

function status(text: string, kind: "info" | "error" = "info") {
  const bar = $("status-bar");
  bar.textContent = text;
  bar.dataset.kind = kind;
}

// ---------------------------------------------------------------------------
// Adding / removing titles
// ---------------------------------------------------------------------------

async function addFiles(paths: string[]) {
  state.splitUndo = null;
  const videos = paths.filter((p) => VIDEO_EXTENSIONS.includes(p.split(".").pop()?.toLowerCase() ?? ""));
  if (videos.length === 0) {
    status(tr("main.keineUnterstuetztenVideodateien"), "error");
    return;
  }
  state.probing += videos.length;
  renderFooter();
  for (const path of videos) {
    status(tr("main.analysiere", { path }));
    try {
      const meta = await probeFile(path);
      const title: Title = {
        id: nextId++,
        meta,
        audio: new Set(meta.audio.slice(0, MAX_AUDIO).map((a) => a.index)),
        subs: new Set(meta.subtitles.filter((s) => s.supported).map((s) => s.index)),
        label: cleanTitle(meta.fileName),
        chapters: meta.chapters.map((c) => c.startSecs),
        trimStart: null,
        trimEnd: null,
        crop: null,
        mergeWithPrevious: false,
        audioNames: {},
        bdResolution: null,
        bdRate: null,
        bdVideo: null,
        bdQuality: null,
        denoise: null,
        brightness: null,
        contrast: null,
        saturation: null,
        subtitleStyle: {},
      };
      state.titles.push(title);
      if (state.selected === null) state.selected = title.id;
      status(tr("main.hinzugefuegt", { fileName: meta.fileName }));
    } catch (e) {
      status(`${path}: ${e}`, "error");
    } finally {
      state.probing--;
      renderAll();
      scheduleCalc();
    }
  }
}

async function addViaDialog() {
  const picked = await open({
    multiple: true,
    title: tr("main.videosHinzufuegen"),
    filters: [{ name: tr("main.videos"), extensions: VIDEO_EXTENSIONS }],
  });
  if (picked) await addFiles(Array.isArray(picked) ? picked : [picked]);
}

function removeSelected() {
  state.splitUndo = null;
  const i = state.titles.findIndex((t) => t.id === state.selected);
  if (i < 0) return;
  const oldGroupPaths = editorTitles().map((t) => t.path);
  state.titles.splice(i, 1);
  normalizeMerge();
  state.selected = state.titles[Math.min(i, state.titles.length - 1)]?.id ?? null;
  if (state.customLayout) {
    const newGroupPaths = editorTitles().map((t) => t.path);
    remapLayoutByPaths(state.customLayout, oldGroupPaths, newGroupPaths);
    state.customLayoutKey = titlesKey(editorTitles());
    void refreshLayoutCheck();
  }
  renderAll();
  scheduleCalc();
}

/** The first title has no predecessor to merge with. */
function normalizeMerge() {
  if (state.titles[0]) state.titles[0].mergeWithPrevious = false;
}

/** Titles grouped by menu entry: a title plus the directly following ones merged into it. */
function titleGroups(): Title[][] {
  const groups: Title[][] = [];
  state.titles.forEach((t, i) => {
    if (i > 0 && t.mergeWithPrevious) groups[groups.length - 1].push(t);
    else groups.push([t]);
  });
  return groups;
}

function toggleMerge(t: Title) {
  state.splitUndo = null;
  const oldGroupPaths = editorTitles().map((t) => t.path);
  t.mergeWithPrevious = !t.mergeWithPrevious;
  if (state.customLayout) {
    const newGroupPaths = editorTitles().map((t) => t.path);
    remapLayoutByPaths(state.customLayout, oldGroupPaths, newGroupPaths);
    state.customLayoutKey = titlesKey(editorTitles());
    void refreshLayoutCheck();
  }
  renderAll();
  scheduleCalc();
}

function moveSelected(delta: number) {
  state.splitUndo = null;
  const i = state.titles.findIndex((t) => t.id === state.selected);
  const j = i + delta;
  if (i < 0 || j < 0 || j >= state.titles.length) return;
  const oldGroupPaths = editorTitles().map((t) => t.path);
  [state.titles[i], state.titles[j]] = [state.titles[j], state.titles[i]];
  normalizeMerge();
  if (state.customLayout) {
    const newGroupPaths = editorTitles().map((t) => t.path);
    remapLayoutByPaths(state.customLayout, oldGroupPaths, newGroupPaths);
    state.customLayoutKey = titlesKey(editorTitles());
    void refreshLayoutCheck();
  }
  renderAll();
  scheduleCalc();
}

/** Moves a title to index `to` (drag and drop). A moved title never silently merges into its new predecessor. */
function moveTitle(from: number, to: number) {
  if (from === to || !state.titles[from]) return;
  state.splitUndo = null;
  const oldGroupPaths = editorTitles().map((t) => t.path);
  moveItem(state.titles, from, to);
  state.titles[to].mergeWithPrevious = false;
  normalizeMerge();
  if (state.customLayout) {
    const newGroupPaths = editorTitles().map((t) => t.path);
    remapLayoutByPaths(state.customLayout, oldGroupPaths, newGroupPaths);
    state.customLayoutKey = titlesKey(editorTitles());
    void refreshLayoutCheck();
  }
  renderAll();
  scheduleCalc();
}

/** Merged members whose audio/subtitle track counts differ from the group's first title, as id -> that title (mirrors `dvd::check_group_shapes`). */
function mergeMismatches(): Map<number, { audio: number; subs: number; first: Title }> {
  const groups = titleGroups();
  const counts = groups.map((g) =>
    g.map((t) => ({
      audio: t.meta.audio.filter((a) => t.audio.has(a.index)).length,
      subs: t.meta.subtitles.filter((s) => t.subs.has(s.index) && s.supported).length,
    })),
  );
  const bad = new Map<number, { audio: number; subs: number; first: Title }>();
  shapeMismatch(counts).forEach((flags, g) =>
    flags.forEach((f, m) => {
      if (f) bad.set(groups[g][m].id, { ...counts[g][m], first: groups[g][0] });
    }),
  );
  return bad;
}

/** Index of the split titles in `state.titles` while the undo is still valid, else -1. */
function splitUndoIndex(): number {
  const u = state.splitUndo;
  if (!u) return -1;
  const ok = u.ids.every((id, k) => state.titles[u.index + k]?.id === id);
  return ok ? u.index : -1;
}

/** One title becomes one title per chapter segment: same source, different trim (nothing is cut on disk). */
async function splitByChapters(t: Title) {
  const dur = t.meta.durationSecs;
  const segs = chapterSegments(t.chapters, t.trimStart ?? 0, t.trimEnd ?? dur);
  const i = state.titles.findIndex((x) => x.id === t.id);
  if (segs.length < 2 || i < 0) return;
  const text = tr("main.inTitelAufteilenMit", { label: t.label, length: segs.length });
  if (!(await ask(text, { title: tr("main.titelAufteilen"), kind: "info" }))) return;
  const news: Title[] = segs.map((s, k) => ({
    id: nextId++,
    meta: t.meta,
    audio: new Set(t.audio),
    subs: new Set(t.subs),
    label: t.meta.chapters.find((c) => Math.abs(c.startSecs - s.chapter) < 0.5)?.title || tr("main.kapitel", { label: t.label, k: k + 1 }),
    chapters: [0],
    trimStart: s.start > 0.05 ? s.start : null,
    trimEnd: s.end < dur - 0.05 ? s.end : null,
    crop: t.crop ? { ...t.crop } : null,
    mergeWithPrevious: false,
    audioNames: { ...t.audioNames },
    bdResolution: t.bdResolution,
    bdRate: t.bdRate,
    bdVideo: t.bdVideo,
    bdQuality: t.bdQuality,
    denoise: t.denoise,
    brightness: t.brightness,
    contrast: t.contrast,
    saturation: t.saturation,
    subtitleStyle: { ...t.subtitleStyle },
  }));
  news[0].mergeWithPrevious = t.mergeWithPrevious;
  state.titles.splice(i, 1, ...news);
  if (state.settings?.bd_single_video) {
    state.settings.bd_single_video = false;
    saveSettings();
  }
  state.splitUndo = { original: t, ids: news.map((n) => n.id), index: i };
  state.selected = news[0].id;
  renderAll();
  scheduleCalc();
  status(tr("main.inTitelAufgeteilt", { length: news.length }));
}

function undoSplit() {
  const i = splitUndoIndex();
  const u = state.splitUndo;
  if (!u || i < 0) return;
  state.titles.splice(i, u.ids.length, u.original);
  state.selected = u.original.id;
  state.splitUndo = null;
  renderAll();
  scheduleCalc();
}

/**
 * Pointer-event drag reordering (HTML5 DnD is swallowed by the webview's native file drop, see
 * `dragDropEnabled`). Listeners sit on the list because `runCalc` re-renders the cards mid-drag.
 */
function initTitleDrag() {
  const list = $("title-list");
  let down: { id: number; x: number; y: number } | null = null;
  let dragging = false;
  let drop: { id: number; after: boolean } | null = null;
  let suppressClick = false;

  const card = (id: number) => list.querySelector<HTMLElement>(`.title-item[data-id="${id}"]`);
  const clear = () => {
    list.querySelectorAll(".dragging, .drag-over-top, .drag-over-bottom").forEach((e) => e.classList.remove("dragging", "drag-over-top", "drag-over-bottom"));
    document.body.classList.remove("reordering");
  };
  const abort = () => {
    clear();
    down = null;
    dragging = false;
    drop = null;
  };

  list.addEventListener("pointerdown", (e) => {
    const el = (e.target as HTMLElement).closest<HTMLElement>(".title-item");
    if (e.button !== 0 || state.converting || !el || (e.target as HTMLElement).closest("button")) return;
    down = { id: Number(el.dataset.id), x: e.clientX, y: e.clientY };
    suppressClick = false;
  });
  list.addEventListener("pointermove", (e) => {
    if (!down) return;
    if (!dragging) {
      if (Math.hypot(e.clientX - down.x, e.clientY - down.y) < 5) return;
      dragging = true;
      list.setPointerCapture(e.pointerId);
      document.body.classList.add("reordering");
    }
    clear();
    document.body.classList.add("reordering");
    card(down.id)?.classList.add("dragging");
    const over = document.elementFromPoint(e.clientX, e.clientY)?.closest<HTMLElement>(".title-item");
    drop = null;
    if (!over || !list.contains(over)) return;
    const r = over.getBoundingClientRect();
    const after = e.clientY > r.top + r.height / 2;
    drop = { id: Number(over.dataset.id), after };
    over.classList.add(after ? "drag-over-bottom" : "drag-over-top");
  });
  list.addEventListener("pointerup", () => {
    const d = down;
    const target = drop;
    const was = dragging;
    abort();
    if (!was || !d) return;
    suppressClick = true; // the click that follows the pointerup must not select
    if (!target) return;
    const from = state.titles.findIndex((t) => t.id === d.id);
    const j = state.titles.findIndex((t) => t.id === target.id);
    if (from < 0 || j < 0) return;
    const at = j + (target.after ? 1 : 0);
    moveTitle(from, at > from ? at - 1 : at);
  });
  list.addEventListener("pointercancel", abort);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && down) abort();
  });
  list.addEventListener("click", (e) => {
    if (suppressClick) {
      e.stopPropagation();
      suppressClick = false;
    }
  }, true);
}

// ---------------------------------------------------------------------------
// Bitrate calculation (debounced)
// ---------------------------------------------------------------------------

let calcTimer: number | undefined;
let calcSeq = 0;

function scheduleCalc() {
  touchAutosave();
  window.clearTimeout(calcTimer);
  calcTimer = window.setTimeout(runCalc, 150);
}

const QUALITY_WEIGHT = { low: 0.75, high: 1.25 }; // keep in sync with dvd::QUALITY_LOW / QUALITY_HIGH

function calcInput() {
  return {
    disc: currentDisc(),
    titles: state.titles.map((t) => ({
      durationSecs: effDuration(t),
      audioKbps: audioKbps(t),
      subtitleStreams: t.subs.size,
      copyVideoKbps: bdCopy(t)?.kbps ?? null,
      weight: isBluray() && t.bdQuality ? QUALITY_WEIGHT[t.bdQuality] : 1,
    })),
    motionMenu: usesMotion(),
    motionDuration: null,
    bdMenuSecs: isBluray() && state.menuMode === "custom" && state.customLayout ? menuClipSecs(state.customLayout, 20) : [],
  };
}

/** "Fit to disc": switch the copied titles with the largest copy to re-encode, one at a time,
 * until the project fits; if it never does, the titles are put back. */
async function fitToDisc() {
  const copies = state.titles
    .filter((t) => bdCopy(t)?.kbps != null)
    .sort((a, b) => (bdCopy(b)!.kbps! * effDuration(b)) - (bdCopy(a)!.kbps! * effDuration(a)));
  const switched: Title[] = [];
  try {
    for (const t of copies) {
      t.bdVideo = "encode";
      switched.push(t);
      if ((await calculateProject(calcInput())).fits) break;
    }
    if (!(await calculateProject(calcInput())).fits) {
      for (const t of switched) t.bdVideo = null;
      status(tr("main.fitFailed"), "error");
    }
  } catch (e) {
    for (const t of switched) t.bdVideo = null;
    status(tr("main.berechnungFehlgeschlagen", { e }), "error");
  }
  renderAll();
  scheduleCalc();
}

async function runCalc() {
  const seq = ++calcSeq;
  try {
    const result = await calculateProject(calcInput());
    if (seq !== calcSeq) return; // a newer calculation is on its way
    state.calc = result;
  } catch (e) {
    state.calc = null;
    status(tr("main.berechnungFehlgeschlagen", { e }), "error");
  }
  renderTitleList();
  renderFooter();
}

// ---------------------------------------------------------------------------
// Rendering: left side
// ---------------------------------------------------------------------------

function copyBadge(t: Title) {
  const c = bdCopy(t);
  if (!c) return "";
  return c.issue
    ? h("span", { class: "badge copy-badge encode", title: c.issue }, tr("main.bdEncodeBadge"))
    : h("span", { class: "badge copy-badge copy", title: tr("main.bdCopyTip", { kbps: c.kbps ?? 0 }) }, tr("main.bdCopyBadge"));
}

/** Copy / Re-encode badge of one audio track on a Blu-ray (nothing on DVD). */
function audioBadge(t: Title, a: AudioTrack) {
  if (!isBluray()) return null;
  return audioCopies(t, a)
    ? h("span", { class: "badge copy-badge copy", title: tr("main.bdAudioCopyTip") }, tr("main.bdCopyBadge"))
    : h(
        "span",
        { class: "badge copy-badge encode", title: tr("main.bdAudioEncodeTip", { codec: CODEC_NAMES[audioChoice(t, a).codec] }) },
        tr("main.bdEncodeBadge"),
      );
}

const projectBdRes =(): BdRes => (state.settings?.bd_resolution === "720p" ? "720" : "1080");

const hasPictureFilters = (t: Title) =>
  (t.denoise ?? 0) > 0 ||
  Math.abs(t.brightness ?? 0) > 1e-4 ||
  Math.abs((t.contrast ?? 1) - 1) > 1e-4 ||
  Math.abs((t.saturation ?? 1) - 1) > 1e-4;

/** Blu-ray/AVCHD video copy verdict of a title (null = the video is encoded anyway). Mirrors
 * the build: the title must keep the source's own format; trim, crop and picture filters always
 * force a re-encode; the disc's bitrate limit is checked there. `strict`: a failed copy stops the build. */
function bdCopy(t: Title): { kbps: number | null; issue: string | null; strict: boolean } | null {
  const v = t.meta.video;
  const mode = t.bdVideo ?? state.bdVideo;
  if (!isBluray() || mode === "encode" || !v) return null;
  const f = isAvchd() ? v.avchdFormat : v.bdFormat;
  const [copyKbps, copyIssue] = isAvchd() ? [v.avchdCopyKbps, v.avchdCopyIssue] : [v.bdCopyKbps, v.bdCopyIssue];
  const res = t.bdResolution ?? projectBdRes();
  // An unset rate follows the TV standard and resolution settings, as in the build.
  const rate = t.bdRate || v.bdDefaultRates[`${state.settings?.tv_standard ?? "PAL"}${res}`];
  const changed = !!f && (res !== f.resolution || rate !== f.rate);
  const issue =
    copyIssue ??
    (changed
      ? tr("main.bdFormatChanged")
      : isTrimmed(t)
        ? tr("main.trimActive")
        : t.crop
          ? tr("main.cropActive")
          : hasPictureFilters(t)
            ? tr("main.pictureFiltersActive")
            : null);
  const strict = mode !== "auto";
  return issue ? { kbps: null, issue, strict } : { kbps: copyKbps, issue: null, strict };
}

function renderVideoTab(t: Title) {
  const v = t.meta.video;
  const res = t.bdResolution ?? projectBdRes();
  const changed = () => {
    renderTitleList();
    renderFooter();
    scheduleCalc();
    renderTabs();
  };
  const select = (id: string, value: string, options: [string, string][], on: (v: string) => void) =>
    h(
      "select",
      { id, onchange: (e: Event) => on((e.target as HTMLSelectElement).value) },
      ...options.map(([val, label]) => h("option", { value: val, selected: val === value }, label)),
    );
  const label = (r: string) => (r.endsWith("i") ? r : `${r}p`);
  const projectMode = tr(state.bdVideo === "encode" ? "app.bdVideoEncode" : state.bdVideo === "auto" ? "app.bdVideoAuto" : "app.bdVideoStrict");
  const c = bdCopy(t);
  const f = isAvchd() ? v?.avchdFormat : v?.bdFormat;
  const index = state.titles.indexOf(t);
  return h(
    "div",
    { class: "video-tab" },
    v
      ? h(
          "p",
          { class: "hint" },
          tr("main.bdSource", { codec: v.codec.toUpperCase(), w: v.width, h: v.height, fps: v.fps.toFixed(3), scan: v.interlaced ? "i" : "p" }),
          " · ",
          f ? tr("main.bdSourceFormat", { format: `${f.resolution} ${label(f.rate)}` }) : tr("main.bdNoFormat"),
        )
      : "",
    h(
      "div",
      { class: "row" },
      h("label", { for: "bd-res" }, tr("main.bdResolution")),
      select(
        "bd-res",
        t.bdResolution ?? "",
        [["", tr("main.projectDefault", { value: `${projectBdRes()}p` })], ["1080", "1080p"], ["720", "720p"]],
        (val) => {
          t.bdResolution = (val || null) as BdRes | null;
          // A rate the new size does not have falls back to automatic.
          if (t.bdRate && !BD_RATES[t.bdResolution ?? projectBdRes()].includes(t.bdRate)) t.bdRate = null;
          changed();
        },
      ),
    ),
    h(
      "div",
      { class: "row" },
      h("label", { for: "bd-rate" }, tr("main.bdRate")),
      select("bd-rate", t.bdRate ?? "", [["", tr("main.bdRateAuto")], ...BD_RATES[res].map((r): [string, string] => [r, label(r)])], (val) => {
        t.bdRate = val || null;
        changed();
      }),
    ),
    h(
      "div",
      { class: "row" },
      h("label", { for: "bd-title-video" }, tr("main.bdVideoTitle")),
      select(
        "bd-title-video",
        t.bdVideo ?? "",
        [["", tr("main.projectDefault", { value: projectMode })], ["encode", tr("app.bdVideoEncode")], ["copy", tr("main.bdVideoCopy")]],
        (val) => {
          t.bdVideo = (val || null) as Title["bdVideo"];
          changed();
        },
      ),
      copyBadge(t),
    ),
    h(
      "div",
      { class: "row" },
      h("label", { for: "bd-quality" }, tr("main.bdQuality")),
      select(
        "bd-quality",
        t.bdQuality ?? "",
        [["", tr("main.bdQualityNormal")], ["low", tr("main.bdQualityLow")], ["high", tr("main.bdQualityHigh")]],
        (val) => {
          t.bdQuality = (val || null) as Title["bdQuality"];
          changed();
        },
      ),
    ),
    c?.issue ? h("p", { class: "hint" }, tr("main.bdReencodeReason", { reason: c.issue })) : "",
    index === 0 ? h("p", { class: "hint" }, tr("main.bdMenuRateNote")) : "",
  );
}

function renderTitleList() {
  const list = $("title-list");
  list.replaceChildren();
  $("dropzone").classList.toggle("compact", state.titles.length > 0);

  const mismatches = mergeMismatches();
  state.titles.forEach((t, i) => {
    const v = t.meta.video;
    const bad = mismatches.get(t.id);
    const calc = state.calc?.titles[i];
    const audioSummary =
      t.meta.audio
        .filter((a) => t.audio.has(a.index))
        .map((a) => audioLabel(t, a))
        .join(", ") || tr("main.keinTon");
    const subsSummary = t.subs.size > 0 ? tr("main.untertitel", { size: t.subs.size }) : "";
    const quality = calc ? calc.qualityPercent : 0;
    const qualityClass = calc ? calc.quality : "medium";

    list.append(
      h(
        "li",
        {
          class: `title-item${t.id === state.selected ? " selected" : ""}${i > 0 && t.mergeWithPrevious ? " merged" : ""}${bad ? " merge-bad" : ""}`,
          "data-id": t.id,
          tabIndex: 0,
          onclick: () => select(t.id),
          onkeydown: (e: KeyboardEvent) => {
            if (e.key === "Enter" || e.key === " ") select(t.id);
          },
        },
        h(
          "div",
          { class: "title-head" },
          h("span", { class: "title-no" }, String(i + 1)),
          h("span", { class: "title-name" }, t.label),
          copyBadge(t),
        ),
        h(
          "div",
          { class: "title-meta" },
          v ? `${v.width}×${v.height} · ${v.dvdAspect}${v.interlaced ? " · interlaced" : ""} · ` : "",
          fmtTime(effDuration(t)),
          tr("main.kapitel2", { effChapters: effChapters(t).length }),
          isTrimmed(t) ? tr("main.trimmed") : "",
          t.crop ? tr("main.cropped") : "",
        ),
        h("div", { class: "title-audio" }, audioSummary + subsSummary),
        bad ? h("div", { class: "title-warn" }, tr("main.mergeTrackMismatch", { audio: bad.audio, subs: bad.subs, first: bad.first.label })) : "",
        i > 0
          ? h(
              "button",
              {
                class: "btn small title-merge",
                title: tr("main.beideVideosLaufenHinter"),
                disabled: state.converting,
                onclick: (e: Event) => {
                  e.stopPropagation();
                  toggleMerge(t);
                },
              },
              t.mergeWithPrevious ? tr("main.trennen") : tr("main.mitVorherigemZusammenfuehren"),
            )
          : "",
        h(
          "div",
          {
            class: "quality",
            title: isAvchd()
              ? tr("main.videoqualitaetGruenUeber10")
              : isBluray()
              ? tr("main.videoqualitaetGruenUeber15")
              : tr("main.videoqualitaetGruenUeber2"),
          },
          h("div", { class: "quality-bar" }, h("div", { class: `quality-value ${qualityClass}`, style: `width:${quality}%` })),
          h("span", { class: "quality-text" }, calc ? `${fmtNum(calc.videoKbps)} kbps` : "…"),
        ),
      ),
    );
  });

  const hasSel = selectedTitle() !== null;
  ($("btn-remove") as HTMLButtonElement).disabled = !hasSel || state.converting;
  const idx = state.titles.findIndex((t) => t.id === state.selected);
  ($("btn-up") as HTMLButtonElement).disabled = idx <= 0;
  ($("btn-down") as HTMLButtonElement).disabled = idx < 0 || idx >= state.titles.length - 1;
}

/** First Blu-ray title with a selected audio track that strict mode could not copy (null = none). */
const bdAudioBlocked = () =>
  (isBluray() && state.bdAudio === "strict" &&
    state.titles.find((t) => t.meta.audio.some((a) => t.audio.has(a.index) && !audioCopies(t, a)))) ||
  null;

function renderFooter() {
  const calc = state.calc;
  const fill = $("fill-value");
  const text = $("fill-text");
  if (!calc || state.titles.length === 0) {
    fill.style.width = "0%";
    fill.className = "fill-value";
    text.textContent = state.probing > 0 ? tr("main.analysiere2") : tr("main.nochKeineVideos");
  } else {
    fill.style.width = `${Math.min(100, calc.fillPercent)}%`;
    fill.className = `fill-value ${calc.fits ? (calc.fillPercent > 97 ? "full" : "ok") : "over"}`;
    const base = `${fmtNum(calc.estimatedMb)} / ${fmtNum(calc.targetMb)} MB (${fmtNum(calc.fillPercent)} %)`;
    const hint =
      state.disc === "bd25"
        ? tr("main.bd50WaehlenOder")
        : state.disc === "bd50" || state.disc === "avchd9"
          ? tr("main.videosKuerzenOderTonspuren")
          : state.disc === "avchd5"
            ? tr("main.avchdDvd9Waehlen")
            : tr("main.dvd9WaehlenOder");
    const counts = getMenuAndVideoCounts();
    text.textContent = singleVideoViolated()
      ? tr("main.nurEinVideoPro")
      : bdAudioBlocked()
        ? tr("main.bdAudioStrictBlocked", { title: bdAudioBlocked()!.label || bdAudioBlocked()!.meta.fileName })
      : counts.overLimit
        ? tr("main.dvdMenuLimitFooter", {
            videos: counts.videos,
            menus: counts.menus,
            total: counts.total,
          })
        : calc.fits
          ? tr("main.videoKbps", { base, fmtNum: fmtNum(calc.videoKbps), calc: calc.clamp === "max" ? tr("main.maximum") : "" })
          : tr("main.doesNotFit", { base, hint });
  }
  const menusReady =
    state.menuMode === "template"
      ? !!state.template
      : !!state.customLayout && state.layoutErrors.length === 0 && !layoutIsStale();
  const counts = getMenuAndVideoCounts();
  const canConvert =
    state.titles.length > 0 &&
    state.probing === 0 &&
    !state.converting &&
    menusReady &&
    !singleVideoViolated() &&
    !counts.overLimit &&
    !state.titles.some((t) => bdCopy(t)?.strict && bdCopy(t)?.issue) &&
    !bdAudioBlocked() &&
    mergeMismatches().size === 0 &&
    !!state.output.trim() &&
    !!calc?.fits;
  // The switch only steers ConvertX template menus on DVD; custom and Blu-ray menus move on their own.
  const motionSwitch = document.getElementById("motion-menu")?.closest("label");
  if (motionSwitch) motionSwitch.hidden = state.menuMode !== "template" || isBluray();
  $("bd-video-row").hidden = !isBluray();
  $("bd-audio-row").hidden = !isBluray();
  ($("btn-convert") as HTMLButtonElement).disabled = !canConvert;
  ($("btn-split-undo") as HTMLButtonElement).hidden = splitUndoIndex() < 0 || state.converting;
  ($("btn-fit") as HTMLButtonElement).hidden = !(calc && !calc.fits) || state.converting || !state.titles.some((t) => bdCopy(t)?.kbps != null);
}

// ---------------------------------------------------------------------------
// Rendering: right side (tabs)
// ---------------------------------------------------------------------------

function select(id: number) {
  state.selected = id;
  renderAll();
}

function renderTabs() {
  // The Video tab holds Blu-ray-only options.
  const videoTab = document.querySelector<HTMLButtonElement>('.tab[data-tab="video"]');
  if (videoTab) videoTab.hidden = !isBluray();
  if (state.tab === "video" && !isBluray()) state.tab = "audio";
  document.querySelectorAll<HTMLButtonElement>(".tab").forEach((b) => {
    const active = b.dataset.tab === state.tab;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", String(active));
  });
  const body = $("tab-body");
  void disposePlayer();
  body.replaceChildren();
  const t = selectedTitle();

  if (state.tab === "template") {
    body.append(renderTemplateTab());
    return;
  }
  if (!t) {
    body.append(h("div", { class: "empty" }, tr("main.linksEinVideoHinzufuegen")));
    return;
  }

  body.append(
    h(
      "div",
      { class: "title-label-row" },
      h("label", { for: "title-label" }, tr("main.bezeichnungImMenue")),
      h("input", {
        id: "title-label",
        type: "text",
        value: t.label,
        oninput: (e: Event) => {
          t.label = (e.target as HTMLInputElement).value;
          renderTitleList();
        },
      }),
    ),
  );
  if (state.tab === "audio") body.append(renderAudioTab(t));
  if (state.tab === "subtitles") body.append(renderSubtitleTab(t));
  if (state.tab === "chapters") body.append(renderChapterTab(t));
  if (state.tab === "edit") body.append(renderEditTab(t, editHooks));
  if (state.tab === "video") body.append(renderVideoTab(t));
}

/** Connects the "Trim & picture" tab to the title list and the size estimate. */
const editHooks = {
  changed: () => {
    renderTitleList();
    renderFooter();
    scheduleCalc();
    if (state.menuMode === "custom") void refreshLayoutCheck();
  },
  /** Frame the picture is shown in: 16:9 on HD discs, otherwise the DVD aspect of the settings. */
  targetAspect: (dar: number) => {
    if (isBluray()) return 16 / 9;
    const policy = state.settings?.aspect_ratio_policy ?? "Auto";
    if (policy === "4:3") return 4 / 3;
    if (policy === "16:9") return 16 / 9;
    return dar >= (4 / 3 + 16 / 9) / 2 ? 16 / 9 : 4 / 3;
  },
  status,
  audioIndex: () => {
    const t = selectedTitle();
    return t?.meta.audio.find((a) => t.audio.has(a.index))?.index ?? null;
  },
  split: () => {
    const t = selectedTitle();
    if (t) void splitByChapters(t);
  },
  qualityContext: () => {
    const t = selectedTitle();
    const idx = t ? state.titles.indexOf(t) : -1;
    return {
      disc: currentDisc(),
      videoKbps: idx >= 0 ? (state.calc?.titles[idx]?.videoKbps ?? null) : null,
      audioKbps: t ? audioKbps(t) : 0,
      bdResolution: t && isBluray() ? t.bdResolution : null,
      bdRate: t && isBluray() ? t.bdRate : null,
    };
  },
};

function renderAudioTab(t: Title) {
  if (t.meta.audio.length === 0) return h("div", { class: "empty" }, tr("main.diesesVideoHatKeine"));
  const full = t.audio.size >= MAX_AUDIO;
  return h(
    "div",
    { class: "track-list" },
    h(
      "p",
      { class: "hint" },
      tr("main.passendeAc3Dts", { MAX_AUDIO }),
    ),
    ...t.meta.audio.map((a) => {
      const checked = t.audio.has(a.index);
      return h(
        "label",
        { class: "track" },
        h("input", {
          type: "checkbox",
          checked,
          disabled: (!checked && full) || (checked && t.audio.size === 1),
          onchange: (e: Event) => {
            if ((e.target as HTMLInputElement).checked) t.audio.add(a.index);
            else t.audio.delete(a.index);
            renderAll();
            scheduleCalc();
          },
        }),
        h(
          "div",
          { class: "track-main" },
          h("input", {
            class: "track-name",
            type: "text",
            maxLength: 60,
            value: t.audioNames[a.index] ?? "",
            placeholder: a.displayLabel,
            title: tr("main.renameTrack"),
            spellcheck: false,
            onchange: (e: Event) => {
              const v = (e.target as HTMLInputElement).value.trim();
              if (v) t.audioNames[a.index] = v;
              else delete t.audioNames[a.index];
              renderAll();
            },
          }),
          a.default ? h("span", { class: "badge" }, tr("main.standard")) : null,
          audioBadge(t, a),
        ),
        h("span", { class: "track-detail" }, audioDetail(t, a)),
      );
    }),
  );
}

/** Re-probes the loaded files after the label mode changed: the engine owns the names. */
async function relabelTitles() {
  for (const t of state.titles) {
    try {
      const m = await probeFile(t.meta.path);
      t.meta.audio = m.audio;
      t.meta.subtitles = m.subtitles;
    } catch (e) {
      status(tr("main.spurnamenKonntenNichtAktualisiert", { e }), "error");
    }
  }
  renderAll();
}

function audioDetail(t: Title, a: AudioTrack): string {
  const c = audioChoice(t, a);
  const target = `${CODEC_NAMES[c.codec]} ${c.layout} ${fmtNum(c.kbps)} kbps`;
  return `#${a.index} · ${a.codec.toUpperCase()} ${a.channels} ch → ${audioCopies(t, a) ? tr("main.unveraendertKopiert", { target }) : target}`;
}

let fontsLoaded: Promise<SystemFont[]> | null = null;
const systemFonts = () => (fontsLoaded ??= listSystemFonts());

/** Font family choices: default, curated menu fonts, a divider, then every installed family (loaded once). */
function fontSelect(current: string | null, on: (family: string | null) => void) {
  const sel = h("select", { id: "sub-font", onchange: () => on(sel.value || null) }) as HTMLSelectElement;
  const add = (family: string) => sel.append(h("option", { value: family, selected: family === current }, family));
  sel.append(h("option", { value: "", selected: !current }, tr("main.subStyleFontDefault")));
  FONTS.forEach(add);
  // A font from another machine's project stays selectable.
  if (current && !FONTS.includes(current)) add(current);
  void systemFonts().then((all) => {
    const rest = all.map((f) => f.family).filter((f) => !FONTS.includes(f) && f !== current);
    if (rest.length === 0) return;
    sel.append(h("option", { disabled: true }, "------"));
    rest.forEach(add);
  });
  return sel;
}

/** Per-title styling of text subtitles; an empty field keeps the default. */
function subtitleStyleControls(t: Title) {
  const st = t.subtitleStyle;
  const field = (key: "fontSizePct" | "outlinePx" | "bottomMarginPct" | "offsetMs", label: string, min: number, max: number, step: number) =>
    h(
      "label",
      { class: "sub-style-field" },
      h("span", {}, label),
      h("input", {
        type: "number",
        min,
        max,
        step,
        value: st[key] ?? "",
        onchange: (e: Event) => {
          const raw = (e.target as HTMLInputElement).value.trim();
          const n = Number(raw);
          if (raw === "" || !Number.isFinite(n)) delete st[key];
          else st[key] = Math.min(max, Math.max(min, n));
          renderTabs();
          touchAutosave();
        },
      }),
    );
  return h(
    "div",
    { class: "sub-style" },
    h("h3", {}, tr("main.subStyleTitle")),
    h(
      "label",
      { class: "sub-style-field" },
      h("span", {}, tr("main.subStyleFont")),
      fontSelect(st.fontFamily ?? null, (family) => {
        if (family) st.fontFamily = family;
        else delete st.fontFamily;
        touchAutosave();
      }),
    ),
    field("fontSizePct", tr("main.subStyleSize"), 50, 200, 5),
    field("outlinePx", tr("main.subStyleOutline"), 0, 6, 1),
    field("bottomMarginPct", tr("main.subStyleMargin"), 0, 30, 1),
    field("offsetMs", tr("main.subStyleOffset"), -60000, 60000, 50),
    h(
      "button",
      {
        class: "btn small",
        type: "button",
        onclick: () => {
          t.subtitleStyle = {};
          renderTabs();
          touchAutosave();
        },
      },
      tr("main.subStyleReset"),
    ),
    h("p", { class: "hint" }, tr("main.subStyleHint")),
  );
}

function renderSubtitleTab(t: Title) {
  if (t.meta.subtitles.length === 0) return h("div", { class: "empty" }, tr("main.diesesVideoHatKeine2"));
  return h(
    "div",
    { class: "track-list" },
    h("p", { class: "hint" }, tr("main.textUntertitelSrtAss")),
    subtitleStyleControls(t),
    ...t.meta.subtitles.map((s) =>
      h(
        "label",
        { class: `track${s.supported ? "" : " disabled"}` },
        h(
          "span",
          { class: "switch" },
          h("input", {
            type: "checkbox",
            checked: t.subs.has(s.index),
            disabled: !s.supported,
            onchange: (e: Event) => {
              if ((e.target as HTMLInputElement).checked) t.subs.add(s.index);
              else t.subs.delete(s.index);
              renderAll();
              scheduleCalc();
            },
          }),
          h("span"),
        ),
        h(
          "div",
          { class: "track-main" },
          h("span", { class: "track-lang" }, s.displayLabel),
          s.forced ? h("span", { class: "badge" }, "Forced") : null,
        ),
        h("span", { class: "track-detail" }, `#${s.index} · ${s.codec}${s.supported ? "" : tr("main.nichtUnterstuetzt")}`),
      ),
    ),
  );
}

function renderChapterTab(t: Title) {
  const sorted = [...t.chapters].sort((a, b) => a - b);
  const sourceTitle = (secs: number) => t.meta.chapters.find((c) => Math.abs(c.startSecs - secs) < 0.5)?.title ?? null;
  const intervalInput = h("input", { type: "number", min: "1", max: "60", value: "5", class: "small" }) as HTMLInputElement;

  const update = (chapters: number[]) => {
    t.chapters = chapters;
    renderAll();
  };

  const rows = sorted.map((start, i) =>
    h(
      "tr",
      {},
      h("td", { class: "num" }, String(i + 1)),
      h(
        "td",
        {},
        i === 0
          ? h("span", { class: "mono" }, fmtTime(0))
          : h("input", {
              type: "text",
              class: "mono time",
              value: fmtTime(start),
              onchange: (e: Event) => {
                const input = e.target as HTMLInputElement;
                const v = parseTime(input.value);
                if (v === null || v <= 0 || v >= t.meta.durationSecs - 1) {
                  input.value = fmtTime(start);
                  status(tr("main.ungueltigeKapitelzeit"), "error");
                  return;
                }
                update(sorted.map((s, j) => (j === i ? v : s)));
              },
            }),
      ),
      h("td", {}, sourceTitle(start) ?? tr("main.kapitel3", { i: i + 1 })),
      h(
        "td",
        { class: "actions" },
        i === 0
          ? null
          : h("button", { class: "btn icon", title: tr("main.kapitelEntfernen"), onclick: () => update(sorted.filter((_, j) => j !== i)) }, "✕"),
      ),
    ),
  );

  return h(
    "div",
    { class: "chapters" },
    h(
      "div",
      { class: "chapter-tools" },
      h(
        "button",
        {
          class: "btn",
          onclick: () => {
            const last = sorted[sorted.length - 1] ?? 0;
            const next = Math.min(last + 300, t.meta.durationSecs - 2);
            if (next > last + 1) update([...sorted, next]);
          },
        },
        tr("main.kapitel4"),
      ),
      h("span", { class: "spacer" }),
      tr("main.alle"),
      intervalInput,
      tr("main.min"),
      h(
        "button",
        {
          class: "btn",
          onclick: () => {
            const step = Math.max(1, Number(intervalInput.value) || 5) * 60;
            const out = [0];
            for (let s = step; s < t.meta.durationSecs - 1; s += step) out.push(s);
            update(out.slice(0, 99));
          },
        },
        tr("main.rasterErzeugen"),
      ),
      t.meta.hasSourceChapters
        ? h("button", { class: "btn", onclick: () => update(t.meta.chapters.map((c) => c.startSecs)) }, tr("main.kapitelDerQuelle"))
        : null,
    ),
    h(
      "table",
      { class: "table" },
      h("thead", {}, h("tr", {}, h("th", {}, "#"), h("th", {}, tr("main.start")), h("th", {}, tr("main.titel")), h("th", {}))),
      h("tbody", {}, ...rows),
    ),
    h(
      "p",
      { class: "hint" },
      tr("main.vonHoechstens99Kapiteln", { length: sorted.length, fmtTime: fmtTime(t.meta.durationSecs) }),
      isTrimmed(t) ? tr("main.zeitenDerQuelldateiIm", { fmtTime: fmtTime(t.trimStart ?? 0), fmtTime2: fmtTime(t.trimEnd ?? t.meta.durationSecs), effChapters: effChapters(t).length }) : "",
    ),
  );
}

/** The project's titles as the menu editor sees them. */
function editorTitles(): EditorTitle[] {
  return titleGroups().map((members, i) => {
    const t = members[0];
    // Same chapter list the build uses (sorted starts), named like in the chapter tab. Merged
    // titles continue one after the other: every member's chapters are shifted by the runtime before it.
    let offset = 0;
    const starts: number[] = [];
    const names: (string | null)[] = [];
    for (const m of members) {
      const own = effChapters(m);
      starts.push(...own.map((s) => s + offset));
      names.push(...own.map((s) => m.meta.chapters.find((c) => Math.abs(c.startSecs - s - (m.trimStart ?? 0)) < 0.5)?.title ?? null));
      offset += effDuration(m);
    }
    return {
      index: i + 1,
      label: t.label.trim() || cleanTitle(t.meta.fileName),
      path: t.meta.path,
      chapters: Math.max(1, starts.length),
      durationSecs: offset,
      chapterNames: names,
      chapterStarts: starts,
      clipStartSecs: (t.trimStart ?? 0) + Math.min(effDuration(t) * 0.1, 300, Math.max(effDuration(t) - 1, 0)),
      // Tracks that go onto the disc (unsupported bitmap formats are skipped by the engine).
      audioTracks: t.meta.audio
        .filter((a) => t.audio.has(a.index))
        .map((a) => ({ source: a.index, label: audioLabel(t, a) })),
      subtitleTracks: t.meta.subtitles
        .filter((s) => t.subs.has(s.index) && s.supported)
        .map((s) => ({ source: s.index, label: s.displayLabel })),
    };
  });
}

/** The stored custom layout belongs to another set of videos (an earlier project). */
function layoutIsStale(): boolean {
  return !!state.customLayout && state.titles.length > 0 && madeForOtherTitles(state.customLayout, state.customLayoutKey, editorTitles());
}

/** Whether the menus move (size estimate): template motion switch or moving custom menus. */
function usesMotion(): boolean {
  return state.menuMode === "custom" ? !!state.customLayout?.menus.some(isMoving) : state.motion;
}

let layoutCheckSeq = 0;
/** Re-checks the custom layout against the current titles (they may have changed). */
async function refreshLayoutCheck() {
  if (state.menuMode !== "custom" || !state.customLayout) {
    state.layoutErrors = [];
    return;
  }
  const seq = ++layoutCheckSeq;
  const errors = await checkLayout(state.customLayout, editorTitles(), frameName()).catch((e) => [String(e)]);
  if (seq !== layoutCheckSeq) return;
  const changed = JSON.stringify(errors) !== JSON.stringify(state.layoutErrors);
  state.layoutErrors = errors;
  if (changed) {
    renderFooter();
    if (state.tab === "template") renderTabs();
  }
}

function editorInit(): EditorInit {
  return {
    layout: state.customLayout,
    titles: editorTitles(),
    tv: frameName(),
    heading: state.menuTitle.trim() || state.titles[0]?.label || (isBluray() ? tr("main.bluRay") : "DVD"),
    layoutTitlesKey: state.customLayoutKey,
  };
}

// The editor lives in its own window ("menu-editor", editor.html) and talks via events.
void listen("editor-ready", () => emitTo("menu-editor", "editor-init", editorInit()));
void listen<CustomLayout>("editor-apply", ({ payload }) => {
  state.customLayout = payload;
  state.customLayoutKey = titlesKey(editorTitles());
  state.menuMode = "custom";
  saveSettings();
  void refreshLayoutCheck();
  scheduleCalc();
  renderAll();
  status(tr("main.eigenesMenueUebernommen"));
});

// The Burn Studio is its own window ("burn-studio", burnStudio.html): created or focused.
async function openStudio() {
  const existing = await WebviewWindow.getByLabel("burn-studio");
  if (existing) {
    await existing.setFocus();
    return;
  }
  const win = new WebviewWindow("burn-studio", {
    url: "burnStudio.html",
    title: tr("main.brennstudioConvertxRust"),
    width: 1180,
    height: 800,
    minWidth: 900,
    minHeight: 620,
  });
  void win.once("tauri://error", (ev) => status(tr("main.brennstudioKonnteNichtGeoeffnet", { ev: String(ev.payload) })));
}

async function openEditor() {
  const existing = await WebviewWindow.getByLabel("menu-editor");
  if (existing) {
    await existing.setFocus();
    return;
  }
  const win = new WebviewWindow("menu-editor", {
    url: "editor.html",
    title: tr("main.menueEditorConvertxRust"),
    width: 1280,
    height: 820,
    minWidth: 960,
    minHeight: 620,
  });
  void win.once("tauri://error", (ev) => status(tr("main.menueEditorKonnteNicht", { ev: String(ev.payload) })));
}

function renderCustomMenuTab(): HTMLElement {
  const l = state.customLayout;
  const wrap = h("div", { class: "custom-menu-tab" });
  if (!l) {
    wrap.append(
      h("p", { class: "hint" }, tr("main.nochKeinEigenesMenue")),
    );
  } else {
    const titleMenus = l.menus.filter((m) => m.role === "title").length;
    const rootMenus = l.menus.length - titleMenus;
    const buttons = l.menus.reduce((n, m) => n + m.elements.filter(isButton).length, 0);
    const moving = l.menus.filter(isMoving).length;
    const counts = getMenuAndVideoCounts();
    wrap.append(
      h(
        "div",
        { class: "template-card" },
        h(
          "div",
          { class: "template-info" },
          h("h3", {}, tr("main.eigenesMenue")),
          h("p", {}, tr("main.titelmenueSRootMenue", { titleMenus, rootMenus, buttons, moving })),
          h("p", {}, h("strong", {}, tr("main.startmenue")), l.menus.find((m) => m.role === "title")?.name ?? "–"),
          h(
            "p",
            { class: `menu-limit-info${counts.overLimit ? " warn" : ""}` },
            h("strong", {}, tr("main.dvdNavigation")),
            tr("main.dvdNavigationCounts", { videos: counts.videos, menus: counts.menus, total: counts.total }),
          ),
          layoutIsStale()
            ? h(
                "p",
                { class: "layout-errors" },
                tr("main.diesesLayoutWurdeFuer"),
              )
            : state.layoutErrors.length === 0
              ? h("p", { class: "ok-text" }, tr("main.passtZuDenAktuellen"))
              : h("ul", { class: "layout-errors" }, ...state.layoutErrors.slice(0, 6).map((e) => h("li", {}, e))),
        ),
      ),
    );
    if (counts.overLimit) {
      wrap.append(
        h(
          "div",
          { class: "menu-limit-warn" },
          h("strong", {}, "\u26A0 " + tr("main.dvdMenuLimitTitle")),
          h(
            "p",
            {},
            tr("main.dvdMenuLimitDesc", {
              videos: counts.videos,
              menus: counts.menus,
              total: counts.total,
            }),
          ),
        ),
      );
    }
  }
  wrap.append(
    h(
      "div",
      { class: "row" },
      h("button", { class: "btn primary", type: "button", disabled: state.titles.length === 0, onclick: openEditor }, l ? tr("main.menueEditorOeffnen") : tr("main.menueGestalten")),
      l
        ? h(
            "button",
            {
              class: "btn",
              type: "button",
              onclick: () => {
                state.customLayout = null;
                state.customLayoutKey = null;
                state.layoutErrors = [];
                saveSettings();
                renderAll();
              },
            },
            tr("main.eigenesMenueVerwerfen"),
          )
        : null,
    ),
  );
  if (state.titles.length === 0) wrap.append(h("p", { class: "hint" }, tr("main.zuerstVideosHinzufuegen")));
  return wrap;
}

function renderTemplateTab() {
  const outer = h("div", { class: "template-tab" });
  // Without ConvertXToDVD templates the menu editor is the only choice: no switch.
  if (state.templates.length === 0) return outer.append(renderCustomMenuTab()), outer;
  const modes: ["template" | "custom", string][] = [
    ["template", tr("main.convertxVorlage")],
    ["custom", tr("main.eigenesMenueEditor")],
  ];
  outer.append(
    h(
      "div",
      { class: "seg menu-mode", role: "radiogroup", "aria-label": tr("main.menueart") },
      ...modes.map(([mode, label]) =>
        h(
          "button",
          {
            type: "button",
            class: `btn${state.menuMode === mode ? " active" : ""}`,
            role: "radio",
            "aria-checked": String(state.menuMode === mode),
            onclick: () => {
              state.menuMode = mode;
              saveSettings();
              void refreshLayoutCheck();
              scheduleCalc();
              renderAll();
            },
          },
          label,
        ),
      ),
    ),
  );
  if (state.menuMode === "custom") outer.append(renderCustomMenuTab());
  else outer.append(renderTemplateChoice());
  return outer;
}

function renderTemplateChoice() {
  const wrap = h("div", { class: "template-tab" });
  if (state.templates.length === 0) {
    wrap.append(h("div", { class: "empty" }, tr("main.keineConvertxMenuevorlagenGefunden")));
    return wrap;
  }
  const select = h("select", {
    id: "template-select",
    onchange: (e: Event) => {
      state.template = (e.target as HTMLSelectElement).value;
      saveSettings();
      renderAll();
    },
  }) as HTMLSelectElement;
  const folders = [...new Set(state.templates.map((t) => t.folder))];
  for (const folder of folders) {
    const group = h("optgroup", { label: folder });
    for (const t of state.templates.filter((x) => x.folder === folder)) {
      group.append(h("option", { value: t.path, selected: t.path === state.template }, t.name));
    }
    select.append(group);
  }

  const info = state.templates.find((t) => t.path === state.template);
  const styles: Record<string, string> = {
    MNUS_TITLE: tr("main.titel"),
    MNUS_ROOT: "Root",
    MNUS_CHAPTER: tr("main.kapitel5"),
    MNUS_SETTINGS: tr("main.einstellungen"),
    MNUS_BONUS: tr("main.bonus"),
    MNUS_SERIES_EPISODES: tr("main.episoden"),
  };
  const preview = h("img", { class: "template-preview", alt: tr("main.vorschau") }) as HTMLImageElement;
  if (info?.thumbnail) {
    templatePreview(info.thumbnail)
      .then((url) => (preview.src = url))
      .catch(() => preview.remove());
  }

  wrap.append(
    h("p", { class: "hint" }, tr("main.dieMenuevorlageGiltFuer")),
    h("div", { class: "row" }, h("label", { for: "template-select" }, tr("main.vorlage")), select),
    h(
      "div",
      { class: "row" },
      h("label", { for: "menu-title" }, tr("main.ueberschriftHauptmenue")),
      h("input", {
        id: "menu-title",
        type: "text",
        placeholder: tr("main.nameDesErstenVideos"),
        value: state.menuTitle,
        oninput: (e: Event) => {
          state.menuTitle = (e.target as HTMLInputElement).value;
          // Typing makes it the user's own heading; an empty field follows the first video again.
          state.menuTitleAuto = state.menuTitle.trim() === "";
          if (state.menuTitleAuto) syncMenuTitle();
        },
      }),
    ),
    h("p", { class: "hint" }, tr("main.auchNameDesDvd")),
  );
  const counts = getMenuAndVideoCounts();
  if (info) {
    const pages = info.pageStyles.map((s) => styles[s] ?? s).join(", ");
    const hasSub = ["MNUS_ROOT", "MNUS_CHAPTER", "MNUS_SETTINGS"].every((s) => info.pageStyles.includes(s));
    wrap.append(
      h(
        "div",
        { class: "template-card" },
        preview,
        h(
          "div",
          { class: "template-info" },
          h("h3", {}, info.name),
          info.description ? h("p", {}, info.description) : null,
          h("p", {}, h("strong", {}, tr("main.seiten")), pages),
          info.titlesPerPage ? h("p", {}, h("strong", {}, tr("main.titelProSeite")), String(info.titlesPerPage)) : null,
          h("p", {}, h("strong", {}, tr("main.videoSlots")), info.hasVideoSlots ? tr("main.jaMotionMenuAnimiert") : "nein"),
          h(
            "p",
            { class: `menu-limit-info${counts.overLimit ? " warn" : ""}` },
            h("strong", {}, tr("main.dvdNavigation")),
            tr("main.dvdNavigationCounts", { videos: counts.videos, menus: counts.menus, total: counts.total }),
          ),
          hasSub ? null : h("p", { class: "hint" }, tr("main.fehlendeUntermenuesRootKapitel")),
        ),
      ),
    );
  }
  if (counts.overLimit) {
    wrap.append(
      h(
        "div",
        { class: "menu-limit-warn" },
        h("strong", {}, "\u26A0 " + tr("main.dvdMenuLimitTitle")),
        h(
          "p",
          {},
          tr("main.dvdMenuLimitDesc", {
            videos: counts.videos,
            menus: counts.menus,
            total: counts.total,
          }),
        ),
      ),
    );
  }
  return wrap;
}

/** ConvertX default: the project (heading and DVD folder) is named after the first video. */
function syncMenuTitle() {
  if (!state.menuTitleAuto) return;
  state.menuTitle = state.titles[0]?.label.trim() ?? "";
  const input = document.getElementById("menu-title") as HTMLInputElement | null;
  if (input && document.activeElement !== input) input.value = state.menuTitle;
}

function renderAll() {
  syncMenuTitle();
  renderTitleList();
  renderTabs();
  renderFooter();
  // Titles, chapters or labels may have changed: is the custom menu still valid?
  if (state.menuMode === "custom") void refreshLayoutCheck();
}

// ---------------------------------------------------------------------------
// Conversion overlay
// ---------------------------------------------------------------------------

const PHASE_ORDER = ["menus", "encoding", "authoring", "image", "burning"] as const;
const overlay = {
  phase: "menus" as (typeof PHASE_ORDER)[number],
  rawPhase: "probe",
  total: 0,
};

function setTotal(pct: number) {
  overlay.total = Math.max(overlay.total, Math.min(100, pct));
  $("total-bar").style.width = `${overlay.total}%`;
  $("total-pct").textContent = `${fmtNum(overlay.total)} %`;
}

function setPhase(name: (typeof PHASE_ORDER)[number]) {
  overlay.phase = name;
  const current = PHASE_ORDER.indexOf(name);
  document.querySelectorAll<HTMLLIElement>("#phases li").forEach((li) => {
    const i = PHASE_ORDER.indexOf(li.dataset.phase as (typeof PHASE_ORDER)[number]);
    li.className = i < current ? "done" : i === current ? "active" : "";
  });
}

function logLine(text: string, kind: "step" | "warn" | "error" | "ok" = "step") {
  const log = $("log");
  log.append(h("li", { class: kind }, text));
  while (log.children.length > 200) log.firstElementChild?.remove();
  log.scrollTop = log.scrollHeight;
}

function onEvent(e: BuildEvent) {
  switch (e.type) {
    case "phase": {
      const p = e.data;
      overlay.rawPhase = p.phase;
      if (p.phase === "probe") {
        setPhase("menus");
        setTotal(1);
      } else if (p.phase === "thumbnails" || p.phase === "menus") {
        setPhase("menus");
        setTotal(5);
      } else if (p.phase === "menuEncoding") {
        setPhase("menus");
        setTotal(8);
      } else if (p.phase === "encoding") {
        // Titles are encoded in parallel; `index` = titles already finished.
        setPhase("encoding");
        setTotal(15 + (80 * p.index) / Math.max(1, p.count));
      } else if (p.phase === "authoring") {
        setPhase("authoring");
        setTotal(95);
      }
      break;
    }
    case "step":
      $("step-label").textContent = e.data;
      logLine(e.data);
      break;
    case "progress": {
      const { label, fraction } = e.data;
      $("step-label").textContent = label;
      $("step-pct").textContent = `${fmtNum(fraction * 100)} %`;
      $("step-bar").style.width = `${fraction * 100}%`;
      if (overlay.rawPhase === "encoding") {
        // Duration-weighted progress over all titles (2-pass included), computed by the engine.
        setTotal(15 + 80 * fraction);
      } else if (overlay.rawPhase === "menuEncoding") {
        setTotal(8 + 7 * fraction);
      }
      break;
    }
    case "warning":
      logLine(e.data, "warn");
      break;
    case "eta":
      showEta(e.data);
      break;
  }
}

/** "Estimated time remaining: ~14 min 20 s (Phase 2/5: Encoding videos - Video 2 of 4)". */
function showEta(eta: Eta) {
  const phase = tr("main.phase", { phaseNumber: eta.phaseNumber, phaseCount: eta.phaseCount, phaseLabel: eta.phaseLabel, eta: eta.detail ? ` – ${eta.detail}` : "" });
  const remaining =
    eta.remainingSecs === null
      ? tr("main.restzeitWirdBerechnet", { phase })
      : tr("main.geschaetzteRestzeit", { formatRemaining: formatRemaining(eta.remainingSecs), phase });
  const burn = eta.burnSecs !== null ? tr("main.danachFuerIsoBrennen", { formatRemaining: formatRemaining(eta.burnSecs) }) : "";
  $("eta-line").textContent = remaining + burn;
}

function onDone(d: ConversionDone) {
  state.converting = false;
  $("overlay-title").textContent = d.ok ? tr("main.dvdFertig") : tr("main.konvertierungFehlgeschlagen");
  if (d.ok) {
    setPhase("authoring");
    // The conversion is done; ISO and burning follow in the burn dialog.
    document.querySelectorAll<HTMLLIElement>("#phases li").forEach((li, i) => (li.className = i < 3 ? "done" : ""));
    setTotal(100);
    $("step-bar").style.width = "100%";
    $("step-label").textContent = d.videoTs ?? "";
    $("step-pct").textContent = "";
    logLine(d.message, "ok");
  } else {
    logLine(d.message, "error");
  }
  $("btn-close-overlay").hidden = false;
  $("btn-cancel").hidden = true;
  status(d.ok ? d.message : tr("main.fehler", { d: d.message.split("\n")[0] }), d.ok ? "info" : "error");
  renderAll();
  // Finished DVD: offer burning right away (the dialog's countdown starts only with a blank disc).
  lastDvd = d.ok && d.videoTs ? { videoTs: d.videoTs, projectName: d.projectName } : null;
  $("btn-burn").hidden = !lastDvd;
  $("eta-line").textContent = "";
  if (lastDvd) void openBurner(lastDvd.videoTs, lastDvd.projectName, state.settings, true);
}

/** The last successfully built DVD or Blu-ray (for "Auf Disc brennen"). */
let lastDvd: { videoTs: string; projectName: string | null } | null = null;

/** Backend confirmed the cancel: tools stopped, work directory removed. */
function onCancelled(message: string) {
  state.converting = false;
  $("overlay").hidden = true;
  resetCancelButton();
  status(tr("main.derTemporaereArbeitsordnerWurde", { message }));
  renderAll();
}

function resetCancelButton() {
  const b = $("btn-cancel") as HTMLButtonElement;
  b.disabled = false;
  b.textContent = tr("main.abbrechen");
}

async function requestCancel() {
  const b = $("btn-cancel") as HTMLButtonElement;
  if (b.disabled) return;
  b.disabled = true;
  b.textContent = tr("main.wirdAbgebrochen");
  $("step-label").textContent = tr("main.wirdAbgebrochen");
  try {
    // `false`: the conversion has just finished – its result arrives as `convert-done`.
    if (!(await cancelConversion())) resetCancelButton();
  } catch (e) {
    resetCancelButton();
    logLine(tr("main.abbrechenFehlgeschlagen", { e }), "error");
  }
}

/** Warnings shown before a conversion starts (see preflight.ts). */
function preflightInput() {
  return {
    fillPercent: state.calc?.fillPercent ?? null,
    clamp: state.calc?.clamp ?? null,
    titles: state.titles.map((t, i) => {
      const c = bdCopy(t);
      return {
        label: t.label.trim() || t.meta.fileName,
        copyFallback: c?.issue ?? null,
        copyWithDeinterlace: !!c && !c.issue && state.settings?.deinterlace === "yadif",
        quality: state.calc?.titles[i]?.quality ?? null,
      };
    }),
  };
}

async function convert() {
  if (state.converting) return;
  if (!(await confirmPreflight(preflightInput()))) return;
  const overlayEl = $("overlay");
  $("overlay-title").textContent = tr("main.konvertierungLaeuft");
  $("log").replaceChildren();
  $("btn-close-overlay").hidden = true;
  $("btn-burn").hidden = true;
  $("btn-cancel").hidden = false;
  resetCancelButton();
  $("step-bar").style.width = "0%";
  $("step-pct").textContent = "";
  $("step-label").textContent = tr("main.starte");
  $("eta-line").textContent = "";
  overlay.total = 0;
  setTotal(0);
  setPhase("menus");
  overlayEl.hidden = false;
  state.converting = true;
  renderAll();
  try {
    await startConversion({
      inputs: state.titles.map((t) => ({
        path: t.meta.path,
        audio: [...t.audio],
        subtitles: [...t.subs],
        label: t.label.trim() || null,
        chapters: effChapters(t),
        startSecs: t.trimStart,
        endSecs: t.trimEnd,
        crop: t.crop,
        mergeWithPrevious: t.mergeWithPrevious,
        audioNames: t.audioNames,
        bdResolution: isBluray() ? t.bdResolution : null,
        bdRate: isBluray() ? t.bdRate : null,
        bdVideo: isBluray() ? t.bdVideo : null,
        bdQuality: isBluray() ? t.bdQuality : null,
        denoise: t.denoise,
        brightness: t.brightness,
        contrast: t.contrast,
        saturation: t.saturation,
        subtitleStyle: t.subtitleStyle,
      })),
      disc: currentDisc(),
      template: state.menuMode === "template" ? state.template : "",
      output: state.output.trim(),
      twoPass: state.twoPass,
      motionMenu: state.motion,
      motionDuration: null,
      menuTitle: state.menuTitle.trim() || null,
      customLayout: state.menuMode === "custom" ? state.customLayout : null,
      bdVideo: isBluray() ? state.bdVideo : "encode",
      bdAudio: isBluray() ? state.bdAudio : "auto",
    });
  } catch (e) {
    onDone({ ok: false, message: String(e), videoTs: null, projectName: null, sizeMb: null, targetMb: 0, warnings: [] });
  }
}

// ---------------------------------------------------------------------------
// Projects (.musadvd): save, open, recovery autosave
// ---------------------------------------------------------------------------

let projectPath: string | null = null;
/** Fingerprint of the last saved/opened state; the autosave skips a state that equals it. */
let savedPrint = "";
let projectBusy = false;

const touchAutosave = createAutosave(
  () => (state.converting || projectBusy || state.titles.length === 0 ? null : snapshotProject()),
  () => savedPrint,
);

function snapshotProject(): ProjectFile {
  slots[mediumOf(state.disc)] = { layout: state.customLayout, key: state.customLayoutKey, mode: state.menuMode };
  const { disc, twoPass, motion, bdVideo, bdAudio, template, menuTitle, menuTitleAuto, output } = state;
  return {
    format: PROJECT_FORMAT,
    version: PROJECT_VERSION,
    savedAt: new Date().toISOString(),
    disc,
    tvStandard: state.settings?.tv_standard ?? null,
    twoPass,
    motion,
    bdVideo,
    bdAudio,
    template,
    menuMode: slots.dvd.mode,
    menuModeBd: slots.bd.mode,
    customLayout: slots.dvd.layout,
    customLayoutBd: slots.bd.layout,
    layoutKey: slots.dvd.key,
    layoutKeyBd: slots.bd.key,
    menuTitle,
    menuTitleAuto,
    output,
    titles: state.titles.map((t) => ({
      path: t.meta.path,
      audio: [...t.audio],
      subs: [...t.subs],
      label: t.label,
      chapters: [...t.chapters],
      trimStart: t.trimStart,
      trimEnd: t.trimEnd,
      crop: t.crop ? { ...t.crop } : null,
      mergeWithPrevious: t.mergeWithPrevious,
      audioNames: { ...t.audioNames },
      bdResolution: t.bdResolution,
      bdRate: t.bdRate,
      bdVideo: t.bdVideo,
      bdQuality: t.bdQuality,
      denoise: t.denoise,
      brightness: t.brightness,
      contrast: t.contrast,
      saturation: t.saturation,
      subtitleStyle: { ...t.subtitleStyle },
    })),
  };
}

/** A saved title on a freshly probed file: selections survive only for streams that still exist. */
function titleFromProject(p: ProjectTitle, meta: VideoMetadata, afterSkipped: boolean): Title {
  const audioIdx = new Set(meta.audio.map((a) => a.index));
  let audio = p.audio.filter((i) => audioIdx.has(i)).slice(0, MAX_AUDIO);
  if (audio.length === 0) audio = meta.audio.slice(0, MAX_AUDIO).map((a) => a.index);
  const subsOk = new Set(meta.subtitles.filter((s) => s.supported).map((s) => s.index));
  const dur = meta.durationSecs;
  const chapters = p.chapters.filter((c) => c < dur);
  return {
    ...p,
    id: nextId++,
    meta,
    audio: new Set(audio),
    subs: new Set(p.subs.filter((i) => subsOk.has(i))),
    label: p.label || cleanTitle(meta.fileName),
    chapters: chapters.length > 0 ? chapters : meta.chapters.map((c) => c.startSecs),
    // A changed source can be shorter than the saved section: use the whole file then.
    trimStart: p.trimStart !== null && p.trimStart < dur ? p.trimStart : null,
    trimEnd: p.trimEnd !== null && p.trimEnd <= dur ? p.trimEnd : null,
    mergeWithPrevious: p.mergeWithPrevious && !afterSkipped,
    audioNames: Object.fromEntries(Object.entries(p.audioNames).filter(([k]) => audioIdx.has(Number(k)))),
  };
}

/** Pushes the restored state into the footer controls. */
function syncControls() {
  ($("disc") as HTMLSelectElement).value = state.disc;
  ($("two-pass") as HTMLInputElement).checked = state.twoPass;
  ($("motion-menu") as HTMLInputElement).checked = state.motion;
  ($("bd-video") as HTMLSelectElement).value = state.bdVideo;
  ($("bd-audio") as HTMLSelectElement).value = state.bdAudio;
  ($("output") as HTMLInputElement).value = state.output;
}

/** Replaces the current project with `data`; missing files are listed, never fatal. */
async function applyProject(data: ProjectFile, path: string | null) {
  projectBusy = true;
  try {
    const gone = new Set(
      await missingPaths([...data.titles.map((t) => t.path), ...layoutFiles(data.customLayout), ...layoutFiles(data.customLayoutBd)]).catch(() => [] as string[]),
    );
    const loaded: Title[] = [];
    const lostTitles: string[] = [];
    let afterSkipped = false;
    state.probing += data.titles.length;
    renderFooter();
    try {
      for (const p of data.titles) {
        if (gone.has(p.path)) {
          lostTitles.push(p.path);
          afterSkipped = true;
          continue;
        }
        status(tr("main.analysiere", { path: p.path }));
        try {
          loaded.push(titleFromProject(p, await probeFile(p.path), afterSkipped));
          afterSkipped = false;
        } catch (e) {
          lostTitles.push(`${p.path} (${e})`);
          afterSkipped = true;
        }
      }
    } finally {
      state.probing -= data.titles.length;
    }

    state.splitUndo = null;
    state.titles = loaded;
    state.selected = loaded[0]?.id ?? null;
    normalizeMerge();
    Object.assign(state, {
      disc: data.disc,
      twoPass: data.twoPass,
      motion: data.motion,
      bdVideo: data.bdVideo,
      bdAudio: data.bdAudio,
      menuTitle: data.menuTitle,
      menuTitleAuto: data.menuTitleAuto,
    });
    if (data.output) state.output = data.output;
    if (data.template && (state.templates.length === 0 || state.templates.some((x) => x.path === data.template))) state.template = data.template;
    // The layouts were made for these titles; with titles missing the stale check stays on the saved key.
    const key = (k: string | null, l: CustomLayout | null) => (l && loaded.length === data.titles.length ? titlesKey(editorTitles()) : k);
    const noTemplates = state.templates.length === 0;
    slots.dvd = { layout: data.customLayout, key: key(data.layoutKey, data.customLayout), mode: noTemplates ? "custom" : data.menuMode };
    slots.bd = { layout: data.customLayoutBd, key: key(data.layoutKeyBd, data.customLayoutBd), mode: noTemplates ? "custom" : data.menuModeBd };
    const live = slots[mediumOf(state.disc)];
    [state.menuMode, state.customLayout, state.customLayoutKey] = [live.mode, live.layout, live.key];
    state.layoutErrors = [];

    let note = "";
    if (data.tvStandard && state.settings && data.tvStandard !== state.settings.tv_standard) {
      try {
        state.settings = await storeSettings({ ...state.settings, tv_standard: data.tvStandard });
        note = " " + tr("project.tvApplied", { tv: data.tvStandard });
      } catch (e) {
        note = " " + String(e);
      }
    }
    saveSettings();
    syncControls();
    projectPath = path;
    renderAll();
    scheduleCalc();
    savedPrint = fingerprint(snapshotProject());
    if (path) void clearAutosave();
    const name = path ? (path.split(/[\\/]/).pop() ?? path) : tr("project.recovered");
    const layoutLost = [...layoutFiles(data.customLayout), ...layoutFiles(data.customLayoutBd)].filter((p) => gone.has(p));
    if (lostTitles.length > 0 || layoutLost.length > 0) {
      showMissing(lostTitles, layoutLost);
      status(tr("project.openedPartial", { name, count: loaded.length, missing: lostTitles.length + layoutLost.length }) + note, "error");
    } else {
      status(tr("project.opened", { name, count: loaded.length }) + note);
    }
  } finally {
    projectBusy = false;
  }
}

async function openProject() {
  if (state.converting || projectBusy) return;
  const path = await pickOpenPath();
  if (!path) return;
  if (state.titles.length > 0 && !(await ask(tr("project.replaceAsk"), { title: tr("project.replaceTitle"), kind: "warning" }))) return;
  try {
    await applyProject(await readProject(path), path);
  } catch (e) {
    status(tr("project.openFailed", { e: e instanceof Error ? e.message : String(e) }), "error");
  }
}

async function saveProjectFile(saveAs: boolean) {
  if (state.converting || projectBusy) return;
  const path = saveAs || !projectPath ? await pickSavePath(projectPath, state.menuTitle.trim() || state.titles[0]?.label || "") : projectPath;
  if (!path) return;
  try {
    const p = snapshotProject();
    await writeProject(path, p);
    projectPath = path;
    savedPrint = fingerprint(p);
    void clearAutosave();
    status(tr("project.saved", { path }));
  } catch (e) {
    status(tr("project.saveFailed", { e: e instanceof Error ? e.message : String(e) }), "error");
  }
}

/** Offers the recovery autosave of an interrupted session; declining discards it. */
async function offerRecovery() {
  const p = await readAutosave();
  if (!p) return;
  const when = p.savedAt ? new Date(p.savedAt).toLocaleString() : "";
  if (await ask(tr("project.restoreAsk", { count: p.titles.length, when }), { title: tr("project.restoreTitle"), kind: "info" })) {
    try {
      await applyProject(p, null);
    } catch (e) {
      status(tr("project.openFailed", { e: String(e) }), "error");
    }
  } else {
    void clearAutosave();
  }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

async function init() {
  loadSettings();

  $("btn-add").addEventListener("click", () => void addViaDialog());
  $("dropzone").addEventListener("click", () => void addViaDialog());
  $("btn-remove").addEventListener("click", removeSelected);
  $("btn-split-undo").addEventListener("click", undoSplit);
  $("btn-fit").addEventListener("click", () => void fitToDisc());
  $("btn-up").addEventListener("click", () => moveSelected(-1));
  $("btn-down").addEventListener("click", () => moveSelected(1));
  initTitleDrag();
  document.addEventListener("keydown", (e) => {
    if (e.key === "Delete" && !(e.target instanceof HTMLInputElement) && !state.converting) removeSelected();
  });
  $("btn-open-project").addEventListener("click", () => void openProject());
  $("btn-save-project").addEventListener("click", () => void saveProjectFile(false));
  document.addEventListener("keydown", (e) => {
    if (!(e.ctrlKey || e.metaKey) || e.altKey || state.converting || document.querySelector(".overlay:not([hidden])")) return;
    const k = e.key.toLowerCase();
    if (k !== "o" && k !== "s") return;
    e.preventDefault();
    if (k === "o") void openProject();
    else void saveProjectFile(e.shiftKey);
  });

  try {
    state.settings = await getSettings();
    const info = await settingsInfo();
    if (info.loadProblem) status(tr("main.einstellungen2", { loadProblem: info.loadProblem }), "error");
  } catch (e) {
    status(tr("main.einstellungenKonntenNichtGeladen", { e }), "error");
  }
  updateCustomDiscLabel();

  // Header language picker: same setting as Settings → Language, saved right away.
  const langPicker = $("ui-lang") as HTMLSelectElement;
  const switchLanguage = async (code: string) => {
    await loadLanguage(code);
    applyStatic();
    renderAll();
    langPicker.value = code;
    void relabelTitles();
  };
  void availableLanguages().then((list) => {
    langPicker.replaceChildren(...list.map((l) => new Option(l.name, l.code)));
    langPicker.value = state.settings?.ui_language ?? "en";
  });
  langPicker.addEventListener("change", async () => {
    if (!state.settings || state.converting) {
      langPicker.value = state.settings?.ui_language ?? "en";
      return;
    }
    try {
      state.settings = await storeSettings({ ...state.settings, ui_language: langPicker.value });
      await switchLanguage(state.settings.ui_language);
    } catch (e) {
      langPicker.value = state.settings.ui_language;
      status(String(e), "error");
    }
  });

  $("btn-studio").addEventListener("click", () => void openStudio());
  $("btn-settings").addEventListener("click", async () => {
    if (state.converting || !state.settings) return;
    const [info, languages] = await Promise.all([settingsInfo(), availableLanguages()]);
    openSettings(state.settings, info, languages, async (saved) => {
      const previousTemplateDir = state.settings?.template_dir;
      const langChanged = saved.ui_language !== state.settings?.ui_language;
      const relabel = langChanged || saved.track_label_mode !== state.settings?.track_label_mode;
      state.settings = saved;
      if (langChanged) await switchLanguage(saved.ui_language);
      else if (relabel) void relabelTitles();
      if (saved.template_dir !== previousTemplateDir) {
        await refreshTemplates();
        renderAll();
      }
      updateCustomDiscLabel();
      status(tr("main.einstellungenGespeichert"));
      void refreshLayoutCheck();
      scheduleCalc();
    });
  });

  const disc = $("disc") as HTMLSelectElement;
  disc.value = state.disc;
  disc.addEventListener("change", () => {
    const from = mediumOf(state.disc);
    state.disc = disc.value as DiscChoice;
    switchMedium(from, mediumOf(state.disc));
    saveSettings();
    void refreshLayoutCheck();
    scheduleCalc();
    renderAll();
  });
  const twoPass = $("two-pass") as HTMLInputElement;
  twoPass.checked = state.twoPass;
  twoPass.addEventListener("change", () => {
    state.twoPass = twoPass.checked;
    saveSettings();
  });
  const bdVideo = $("bd-video") as HTMLSelectElement;
  bdVideo.value = state.bdVideo;
  bdVideo.addEventListener("change", () => {
    state.bdVideo = bdVideo.value as BdVideo;
    saveSettings();
    scheduleCalc();
    renderAll();
  });
  const bdAudio = $("bd-audio") as HTMLSelectElement;
  bdAudio.value = state.bdAudio;
  bdAudio.addEventListener("change", () => {
    state.bdAudio = bdAudio.value as BdAudio;
    saveSettings();
    scheduleCalc();
    renderAll();
  });
  void getVersion()
    .then((v) => {
      $("app-version").textContent = `v${v}`;
      if (navigator.userAgent.includes("Linux")) void notifyNewRelease(v);
    })
    .catch(() => {});
  const motion = $("motion-menu") as HTMLInputElement;
  motion.checked = state.motion;
  motion.addEventListener("change", () => {
    state.motion = motion.checked;
    saveSettings();
    scheduleCalc();
  });

  const output = $("output") as HTMLInputElement;
  if (!state.output) state.output = await defaultOutputDir().catch(() => "");
  output.value = state.output;
  output.addEventListener("input", () => {
    state.output = output.value;
    saveSettings();
    renderFooter();
  });
  $("btn-output").addEventListener("click", async () => {
    const dir = await open({ directory: true, title: tr("main.ausgabeordnerWaehlen"), defaultPath: state.output || undefined });
    if (typeof dir === "string") {
      state.output = dir;
      output.value = dir;
      saveSettings();
      renderFooter();
    }
  });
  $("btn-convert").addEventListener("click", () => void convert());
  $("btn-close-overlay").addEventListener("click", () => ($("overlay").hidden = true));
  $("btn-burn").addEventListener("click", () => {
    if (lastDvd) void openBurner(lastDvd.videoTs, lastDvd.projectName, state.settings, false);
  });
  $("btn-cancel").addEventListener("click", () => void requestCancel());

  document.querySelectorAll<HTMLButtonElement>(".tab").forEach((b) =>
    b.addEventListener("click", () => {
      state.tab = b.dataset.tab as Tab;
      renderTabs();
    }),
  );

  // Native drag & drop delivers file paths (HTML5 drop events do not).
  const dropzone = $("dropzone");
  await getCurrentWebview().onDragDropEvent((e) => {
    const p = e.payload;
    if (p.type === "enter" || p.type === "over") dropzone.classList.add("hover");
    else dropzone.classList.remove("hover");
    if (p.type === "drop" && !state.converting) void addFiles(p.paths);
  });

  await onBuildEvent(onEvent);
  await onBuildDone(onDone);
  await onBuildCancelled(onCancelled);

  await refreshTemplates();
  renderAll();
  status(tr("main.bereitVideosPerDrag"));

  const initial = await startupFiles().catch(() => [] as string[]);
  if (initial.length > 0) await addFiles(initial);
  else void offerRecovery();
  // `musadvd-gui <DVD folder>`: burn an existing DVD folder.
  const burnDir = await startupBurn().catch(() => null);
  if (burnDir) {
    const parts = burnDir.split(/[\\/]/).filter(Boolean);
    lastDvd = { videoTs: burnDir, projectName: parts.length > 1 ? parts[parts.length - 2] : null };
    void openBurner(lastDvd.videoTs, lastDvd.projectName, state.settings, false);
  }
}

/** Looks up the ConvertX templates; without any, the menu editor becomes the only menu mode. */
async function refreshTemplates() {
  try {
    state.templates = await listTemplates();
  } catch (e) {
    state.templates = [];
    status(tr("main.menuevorlagen", { e }), "error");
  }
  if (state.templates.length === 0) {
    state.menuMode = "custom";
    slots.dvd.mode = "custom";
    slots.bd.mode = "custom";
    return;
  }
  if (!state.templates.some((t) => t.path === state.template)) {
    state.template =
      state.templates.find((t) => t.path.endsWith("10_TitlesPerPageNoRoot.ini"))?.path ??
      state.templates.find((t) => t.folder === "Open Style")?.path ??
      state.templates[0]?.path ??
      "";
  }
}

/** Offers a newer GitHub release; offline, dev builds and unsigned releases fail silently. */
async function checkForUpdate() {
  try {
    const update = await check();
    if (update) openUpdateDialog(update);
  } catch {
    // no update check possible, not worth bothering the user
  }
}

void init().then(checkForUpdate);
