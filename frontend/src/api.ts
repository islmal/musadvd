// Typed wrappers around the Tauri commands in src-tauri/src/main.rs.
import type { CustomLayout, EditorTitle, LayoutElement } from "./layout";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Disc choice; `custom` uses the target size from the settings. */
/** `avchd5`/`avchd9`: Blu-ray folder structure on a DVD-5/DVD-9. */
export type Disc = "dvd5" | "dvd9" | "bd25" | "bd50" | "avchd5" | "avchd9" | { custom: number };

export interface VideoInfo {
  codec: string;
  width: number;
  height: number;
  displayAspect: number;
  fps: number;
  interlaced: boolean;
  dvdAspect: string;
  /** Blu-ray stream copy of the untrimmed, uncropped file: average kbit/s, or why it is re-encoded. */
  bdCopyKbps: number | null;
  bdCopyIssue: string | null;
  /** The Blu-ray format the file already has; a copy needs the title to keep it. */
  bdFormat: { resolution: "1080" | "720"; rate: string } | null;
}

export interface AudioTrack {
  index: number;
  codec: string;
  channels: number;
  channelLayout: string;
  language: string | null;
  languageName: string;
  /** Track name by the label mode setting (computed by the engine). */
  displayLabel: string;
  title: string | null;
  default: boolean;
  ac3Kbps: number;
  dvdLayout: string;
  /** What happens to the track per disc format and audio mode. */
  plans: AudioPlans;
}

/** Bitrate, layout and whether the track is copied, for one disc format and audio mode. */
export interface AudioChoice {
  kbps: number;
  /** `displayLabel` plus this plan's channel layout: the name on the disc menu. */
  label: string;
  layout: string;
  copy: boolean;
  codec: "ac3" | "dts" | "lpcm";
}
export interface AudioModes {
  auto: AudioChoice;
  ac3: AudioChoice;
  lpcm: AudioChoice;
}
export interface AudioPlans {
  dvd: AudioModes;
  bluray: AudioModes;
  avchd: AudioModes;
}

/** Pixels cut from the edges of the source picture. */
export interface Crop {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

export interface SubtitleTrack {
  index: number;
  codec: string;
  kind: "text" | "bitmap";
  language: string | null;
  languageName: string;
  displayLabel: string;
  title: string | null;
  forced: boolean;
  supported: boolean;
}

export interface ChapterInfo {
  startSecs: number;
  title: string | null;
}

export interface VideoMetadata {
  path: string;
  fileName: string;
  sizeMb: number;
  durationSecs: number;
  video: VideoInfo | null;
  audio: AudioTrack[];
  subtitles: SubtitleTrack[];
  chapters: ChapterInfo[];
  hasSourceChapters: boolean;
}

export interface TemplateInfo {
  path: string;
  name: string;
  description: string | null;
  folder: string;
  thumbnail: string | null;
  titlesPerPage: number | null;
  pageStyles: string[];
  hasVideoSlots: boolean;
}

export interface TitleLoad {
  durationSecs: number;
  audioKbps: number;
  subtitleStreams: number;
  /** Video copied at this kbit/s (fixed size on the disc). */
  copyVideoKbps?: number | null;
}

export interface ProjectSettings {
  disc: Disc;
  titles: TitleLoad[];
  motionMenu: boolean;
  motionDuration: number | null;
  /** Blu-ray menus from the editor: clip length of every menu in seconds. */
  bdMenuSecs?: number[];
}

export interface CalculationResult {
  targetMb: number;
  videoKbps: number;
  rawVideoKbps: number;
  clamp: "none" | "min" | "max";
  estimatedMb: number;
  fillPercent: number;
  fits: boolean;
  menuMb: number;
  totalSecs: number;
  /** quality: good > 2500 kbps, medium 1200-2500, poor < 1200 (engine thresholds). */
  titles: { videoKbps: number; qualityPercent: number; quality: "good" | "medium" | "poor" }[];
}

export interface InputConfig {
  path: string;
  audio: number[];
  subtitles: number[];
  label: string | null;
  chapters: number[];
  /** Used section in seconds of the source file (null = file start / end). */
  startSecs?: number | null;
  endSecs?: number | null;
  crop?: Crop | null;
  /** Play together with the previous input behind one menu entry. */
  mergeWithPrevious?: boolean;
  /** Own audio track names by ffprobe stream index. */
  audioNames?: Record<number, string>;
  /** Blu-ray overrides: `1080`/`720`, a rate name (`23.976`, `25i`, ...), `encode`/`copy`. */
  bdResolution?: string | null;
  bdRate?: string | null;
  bdVideo?: string | null;
}

export interface ConversionConfig {
  inputs: InputConfig[];
  disc: Disc;
  template: string;
  output: string;
  twoPass: boolean;
  motionMenu: boolean;
  motionDuration: number | null;
  menuTitle: string | null;
  /** Menus from the menu editor instead of `template`. */
  customLayout?: CustomLayout | null;
  /** Blu-ray/AVCHD video: re-encode, copy compliant streams, or copy all. */
  bdVideo?: "encode" | "auto" | "strict";
}

export type BuildPhase =
  | { phase: "probe" }
  | { phase: "thumbnails" }
  | { phase: "menus" }
  | { phase: "menuEncoding" }
  | { phase: "encoding"; index: number; count: number }
  | { phase: "authoring" };

/** `dvd::Event` as serialized by serde (`{type, data}`). */
export type BuildEvent =
  | { type: "phase"; data: BuildPhase }
  | { type: "step"; data: string }
  | { type: "progress"; data: { label: string; fraction: number } }
  | { type: "warning"; data: string }
  | { type: "eta"; data: Eta };

export type JobPhase = "menus" | "encoding" | "authoring" | "image" | "burning";

/** Remaining time of the whole job (engine: `eta::Eta`). */
export interface Eta {
  phase: JobPhase;
  phaseNumber: number;
  phaseCount: number;
  phaseLabel: string;
  detail: string | null;
  remainingSecs: number | null;
  /** Image + burn (+ verify) afterwards, at the default speed. */
  burnSecs: number | null;
}

/** "~14 min 20 s", "~45 s", "~1 h 05 min". */
export function formatRemaining(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  if (s >= 3600) return `~${Math.floor(s / 3600)} h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")} min`;
  if (s >= 60) return `~${Math.floor(s / 60)} min ${String(s % 60).padStart(2, "0")} s`;
  return `~${s} s`;
}

export interface ConversionDone {
  ok: boolean;
  message: string;
  videoTs: string | null;
  /** DVD folder name (default volume label). */
  projectName: string | null;
  sizeMb: number | null;
  targetMb: number;
  warnings: string[];
}

export const probeFile = (path: string) => invoke<VideoMetadata>("probe_file", { path });
export const listTemplates = () => invoke<TemplateInfo[]>("list_templates");
export const templatePreview = (path: string) => invoke<string>("template_preview", { path });
export const calculateProject = (settings: ProjectSettings) =>
  invoke<CalculationResult>("calculate_project", { settings });
export const startConversion = (config: ConversionConfig) => invoke<void>("start_conversion", { config });
export const defaultOutputDir = () => invoke<string>("default_output_dir");
export const startupFiles = () => invoke<string[]>("startup_files");

/** `config::AppSettings` (field names as in Rust). */
export interface AppSettings {
  auto_start_playback: boolean;
  loop_playback: boolean;
  play_sequentially: boolean;
  skip_root_menu: boolean;
  return_to_title_menu_after_title: boolean;
  no_title_menu_for_single_title: boolean;
  text_overflow_action: "reduce_font_size" | "truncate";
  menu_audio: "title" | "silence" | "file";
  menu_audio_file: string;
  template_dir: string;
  tv_standard: "PAL" | "NTSC";
  dvd_resolution: "Full D1" | "Half D1" | "SIF";
  bd_resolution: "1080p" | "720p";
  bd_single_video: boolean;
  aspect_ratio_policy: "Auto" | "4:3" | "16:9";
  encoding_profile: "Auto" | "SP_Short" | "MP_Medium" | "LP_Long";
  target_size_mb: number;
  max_concurrent_encodes: number;
  process_priority: "Normal" | "BelowNormal" | "Idle";
  auto_chapter_enabled: boolean;
  auto_chapter_interval_seconds: number;
  auto_chapter_min_video_length_seconds: number;
  chapter_naming_template: string;
  default_audio_language: string;
  default_subtitle_language: string;
  force_first_subtitle: boolean;
  hw_decode: boolean;
  hw_accel_mode: "none" | "nvidia" | "intel" | "amd";
  menu_animations: boolean;
  fast_encode: boolean;
  audio_mode: "auto" | "ac3" | "lpcm";
  ui_language: string;
  track_label_mode: "smart" | "language" | "generic";
  scale_mode: "letterbox" | "panscan" | "stretch";
  scale_filter: "bicubic" | "bilinear" | "lanczos";
  deinterlace: "auto" | "yadif" | "off";
  loudnorm: boolean;
  burn_speed: "auto" | "max" | "16" | "12" | "8" | "6" | "4" | "2.4";
  verify_after_burn: boolean;
  keep_iso: boolean;
  delete_after_burn: boolean;
  quiet_finish: boolean;
  auto_erase_rewritable: boolean;
  default_volume_label: string;
}

export interface SettingsInfo {
  path: string;
  loadProblem: string | null;
  /** Choices that are stored but not implemented by the engine yet. */
  unsupported: string[];
  cpuCount: number;
}

export const getSettings = () => invoke<AppSettings>("get_settings");
export const defaultSettings = () => invoke<AppSettings>("default_settings");
/** Saves and returns the sanitized settings. */
export const saveSettings = (settings: AppSettings) => invoke<AppSettings>("save_settings", { settings });
export const settingsInfo = () => invoke<SettingsInfo>("settings_info");
export const availableLanguages = () => invoke<{ code: string; name: string }[]>("get_available_languages");
export const localeData = (lang: string) => invoke<Record<string, string>>("get_locale_data", { lang });

export const onBuildEvent = (cb: (e: BuildEvent) => void): Promise<UnlistenFn> =>
  listen<BuildEvent>("convert-event", (e) => cb(e.payload));
export const onBuildDone = (cb: (d: ConversionDone) => void): Promise<UnlistenFn> =>
  listen<ConversionDone>("convert-done", (e) => cb(e.payload));
/** Stops the running conversion; `false` if none is running. */
export const cancelConversion = () => invoke<boolean>("cancel_conversion");
/** Sent once all tools are stopped and the work directory is removed. */
export const onBuildCancelled = (cb: (message: string) => void): Promise<UnlistenFn> =>
  listen<string>("convert-cancelled", (e) => cb(e.payload));

// ---------------------------------------------------------------------------
// Burning (src-tauri/src/burn.rs)
// ---------------------------------------------------------------------------

export interface DriveInfo {
  id: string;
  name: string;
  path: string;
}

export type MediaType = "dvd5" | "dvd9" | "bd25" | "bd50" | "unknown";
/** `"max"` or `{ specific: sectorsPerSecond }`. */
export type WriteSpeed = "max" | { specific: number };

export interface MediaInfo {
  present: boolean;
  media_type: MediaType;
  physical: string;
  blank: boolean;
  rewritable: boolean;
  writable: boolean;
  free_sectors: number;
  total_sectors: number;
  speeds: number[];
  layer_break_supported: boolean;
}

export interface SpeedOption {
  value: WriteSpeed;
  label: string;
  /** Image + burn time at this speed without / with verification. */
  secs: number;
  secsVerify: number;
}

export interface MediaView {
  media: MediaInfo;
  speeds: SpeedOption[];
  defaultSpeed: WriteSpeed;
  problem: string | null;
  warning: string | null;
  needsErase: boolean;
  autoOk: boolean;
}

export interface DiscNeeds {
  sectors: number;
  sizeGb: number;
  /** DVD-9 / BD-50 needed. */
  dualLayer: boolean;
  /** The content is a Blu-ray (BDMV). */
  bluray: boolean;
  /** A BDMV small enough for a DVD-9 (AVCHD): DVD media are fine. */
  fitsDvd: boolean;
}

export type BurnPhase = "preparing" | "formatting" | "writing" | "finalizing" | "verifying" | "done";

export interface BurnProgress {
  phase: BurnPhase;
  fraction: number;
  message: string;
  eta: Eta;
}

export interface BurnRequest {
  driveId: string;
  videoTs: string;
  label: string;
  speed: WriteSpeed;
  verify: boolean;
  keepIso: boolean;
  eject: boolean;
  erase: boolean;
  deleteAfter: boolean;
}

export interface BurnDone {
  ok: boolean;
  cancelled: boolean;
  message: string;
  iso: string | null;
}

export const listDrives = () => invoke<DriveInfo[]>("list_drives");
export const discNeeds = (videoTs: string) => invoke<DiscNeeds>("disc_needs", { videoTs });
export const probeMedia = (driveId: string, videoTs: string, speedPref: string, autoErase: boolean) =>
  invoke<MediaView>("probe_media", { driveId, videoTs, speedPref, autoErase });
export const openFolder = (path: string) => invoke<void>("open_folder", { path });
export const startBurn = (request: BurnRequest) => invoke<void>("start_burn", { request });
/** DVD folder from the command line whose burn dialog opens on start. */
export const startupBurn = () => invoke<string | null>("startup_burn");
export const cancelBurn = () => invoke<boolean>("cancel_burn");
export const onBurnProgress = (cb: (p: BurnProgress) => void): Promise<UnlistenFn> =>
  listen<BurnProgress>("burn-progress", (e) => cb(e.payload));
export const onBurnDone = (cb: (d: BurnDone) => void): Promise<UnlistenFn> =>
  listen<BurnDone>("burn-done", (e) => cb(e.payload));

// ---------------------------------------------------------------------------
// Menu editor (src-tauri/src/menu_editor.rs)
// ---------------------------------------------------------------------------

/** All problems of a layout for the project's titles (empty = ready). */
export const checkLayout = (layout: CustomLayout, titles: EditorTitle[], tv: string) =>
  invoke<string[]>("check_layout", { layout, titles, tv });
export const loadLayout = (path: string) => invoke<CustomLayout>("load_layout", { path });
/** A ConvertX template (.ini) converted into an editor layout, plus the converter's notes. */
export const importConvertxTemplate = (path: string, titles: EditorTitle[], lang: string) =>
  invoke<{ layout: CustomLayout; warnings: string[] }>("import_convertx_template", { path, titles, lang });
/** Lines of a text element exactly as the build draws them (word wrap, "…"). */
export const textLines = (element: LayoutElement, canvas: { width: number; height: number }, tv: string) =>
  invoke<string[]>("text_lines", { element, canvas, tv });
export const saveLayout = (path: string, layout: CustomLayout) => invoke<void>("save_layout", { path, layout });
/** A picture file as data URL. */
export const imagePreview = (path: string) => invoke<string>("image_preview", { path });
/** A frame of a video as data URL. */
export const titleFrame = (path: string, atSecs: number) => invoke<string>("title_frame", { path, atSecs });
/** Audio peaks 0–255, 40 per second (`encoder::WAVEFORM_PEAKS_PER_SEC`). */
export const audioWaveform = (path: string, startSecs: number, secs: number) => invoke<number[]>("audio_waveform", { path, startSecs, secs });
/** The whole picture of a video at `atSecs`, `width` px wide, as data URL (crop inspector). */
export const sourceFrame = (path: string, atSecs: number, width: number) => invoke<string>("source_frame", { path, atSecs, width });
/** Black bars of a video (inside the section, if given). */
export const detectCrop = (path: string, startSecs: number | null, endSecs: number | null) =>
  invoke<Crop>("detect_crop", { path, startSecs, endSecs });
/** Registers a video for the live player; returns the stream URL (append `?t=<secs>&a=<audio index>`). */
export const previewStream = (path: string, crop: Crop | null) => invoke<string>("preview_stream", { path, crop });
/** Kills the running preview ffmpeg. */
export const previewStop = () => invoke<void>("preview_stop");
