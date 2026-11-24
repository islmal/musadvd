// Typed wrappers of the Burn Studio commands and events (src-tauri/src/burn_studio.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AppSettings, DriveInfo, MediaInfo } from "../api";

export type { DriveInfo, MediaInfo };

// ---- settings (mirror of src/config.rs `BurnStudioSettings`) ---------------------------------

export interface PerOp {
  read: boolean;
  write: boolean;
  verify: boolean;
  erase: boolean;
}

export interface StudioSettings {
  general: {
    prevent_standby: boolean;
    on_complete: string;
    warnings: { overburn: boolean; dvd_r_dl_layer_break: boolean; no_ifo_update: boolean; seamless_layer_break: boolean };
    md5_calculate: boolean;
    md5_compare: boolean;
    lb_check_seamless: boolean;
    lb_show_interleaved: boolean;
  };
  read: {
    split: string;
    layout_file: string;
    sw_retries: number;
    hw_retries: number;
    ignore_read_errors: boolean;
    priority: string;
    batch_eject_next: boolean;
  };
  build: {
    split: string;
    layout_file: string;
    start_lba: number;
    min_sectors: number;
    fs: string;
    dedupe: boolean;
    filter: { skip_hidden: boolean; skip_system: boolean; skip_reparse: boolean; skip_in_use: boolean };
    log_skipped: boolean;
    dvd: { create_audio_ts: boolean; fix_vts_sectors: boolean; ifo_bup_ecc_padding: boolean; uppercase: boolean; read_dvd_text: boolean };
    bd: { create_certificate: boolean; reset_cpi: boolean; read_bdmt: boolean };
    prompts: string;
  };
  write: {
    mode: string;
    write_type: string;
    link_size: number;
    auto_book_type: boolean;
    opc: boolean;
    dvdpr_reserve_track: boolean;
    dvdpr_min_radius: boolean;
    dvdpr_dl_only_l0: boolean;
    fastwrite: boolean;
    bdr_verify_not_required: boolean;
    burn_proof: boolean;
    lock_volume: boolean;
    cycle_tray_before_verify: boolean;
    verify: boolean;
    layer_break: string;
    user_l0_sectors: number;
    prefer_no_spare: boolean;
    sw_retries: number;
    overburn: string;
    test_mode: boolean;
    copies: number;
    delete_image_after: boolean;
    eject_after: boolean;
  };
  verify: { ignore_non_user_data: boolean; ignore_miscompares: boolean; sw_retries: number; hw_retries: number };
  graph: { capture: boolean; capture_cpu: boolean; sample_ms: number; template: string; folder: string };
  io: {
    backend: string;
    transport: string;
    ring_mib: number;
    recovery_main_low: number;
    recovery_main_high: number;
    recovery_device_low: number;
    transfer_kib: number;
    buffered_io_read: boolean;
    buffered_io_write: boolean;
    lock_tray: PerOp;
    eject_after: PerOp;
    device_sort: string;
  };
  auto_speed: { media_id: string; drive: string; speed: string }[];
}

export type FullSettings = AppSettings & { burn_studio: StudioSettings };

// ---- DTOs ------------------------------------------------------------------------------------

export interface Profile {
  code: number;
  name: string;
  current: boolean;
}

export interface DriveDetails {
  vendor: string;
  model: string;
  firmware: string;
  profiles: Profile[];
  current_profile: number | null;
  current_profile_name: string;
  ready: boolean;
  write_speeds_kbps: number[];
  book_type: string | null;
  media_id: string | null;
  layer_break: number | null;
  disc_status: string | null;
  erasable: boolean | null;
  capacity_sectors: number | null;
  notes: string[];
}

export interface DriveView {
  media: MediaInfo | null;
  mediaError: string | null;
  /** What the `auto` write speed resolves to for the inserted disc ("6x"). */
  autoSpeed: string | null;
  details: DriveDetails | null;
  detailsError: string | null;
}

export type Content = "dvdvideo" | "bluray" | "data";

export interface PlanInfo {
  content: Content;
  contentName: string;
  label: string;
  sectors: number;
  bytes: number;
  layerBreak: number | null;
  skipped: { path: string; reason: string }[];
  warnings: string[];
  needsAnswer: Content | null;
}

export interface PathInfo {
  path: string;
  name: string;
  isDir: boolean;
  bytes: number;
  files: number;
  content: Content | null;
}

export interface BuildInput {
  sources: { path: string; dest: string }[];
  label: string;
  /** UDF volume identifier; empty = `label`. */
  udfLabel: string;
  answer: boolean | null;
  layerBreak: { l0Sectors: number; vts: number; padding: number } | null;
}

export interface Candidate {
  vts: number;
  ttn: number;
  pgc: number;
  cell: number;
  start_lba: number;
  end_lba: number;
  seamless: boolean;
  interleaved: boolean;
  l0_sectors: number;
  l1_sectors: number;
  split_percent: number;
  padding_sectors: number;
}

export interface Progress {
  phase: string;
  fraction: number;
  lba: number;
  total: number;
  speedX: number;
  ring: number;
  device: number | null;
  etaSecs: number | null;
  message: string;
}

export interface Sample {
  speed: number;
  lba: number;
  main: number;
  device: number;
  cpu: number;
}

export interface Outcome {
  ok: boolean;
  cancelled: boolean;
  message: string;
  files: string[];
  md5: string | null;
  badSectors: number;
  details: string[];
  graph: string | null;
  layoutFile: string | null;
}

export interface GraphFile {
  kind: "write" | "read" | "verify";
  config: [string, string][];
  sampleMs: number;
  samples: Sample[];
}

// ---- commands --------------------------------------------------------------------------------

export const drives = () => invoke<DriveInfo[]>("studio_drives");
export const driveInfo = (driveId: string) => invoke<DriveView>("studio_drive_info", { driveId });
export const eject = (driveId: string) => invoke<void>("studio_eject", { driveId });
export const load = (driveId: string) => invoke<void>("studio_load", { driveId });
export const lock = (driveId: string, on: boolean) => invoke<void>("studio_lock", { driveId, lock: on });
export const cancel = () => invoke<boolean>("studio_cancel");
export const buildPlan = (input: BuildInput) => invoke<PlanInfo>("studio_build_plan", { input });
export const inspect = (paths: string[]) => invoke<PathInfo[]>("studio_inspect", { paths });
export const imageScrambled = (image: string) => invoke<boolean | null>("studio_image_scrambled", { image });
export const writeImage = (driveId: string, image: string, speed: string, erase: boolean) =>
  invoke<void>("studio_write_image", { driveId, image, speed, erase });
export const writeFiles = (driveId: string, input: BuildInput, speed: string, erase: boolean) =>
  invoke<void>("studio_write_files", { driveId, input, speed, erase });
export const discovery = (driveId: string, speed: string, sectors: number | null) =>
  invoke<void>("studio_discovery", { driveId, speed, sectors });
export const erase = (driveId: string, full: boolean) => invoke<void>("studio_erase", { driveId, full });
export const readDisc = (driveId: string, output: string) => invoke<void>("studio_read", { driveId, output });
export const build = (input: BuildInput, output: string) => invoke<void>("studio_build", { input, output });
export const verify = (driveId: string, image: string | null) => invoke<void>("studio_verify", { driveId, image });
export const labelRead = (iso: string) => invoke<string>("studio_label_read", { iso });
export const labelPatch = (iso: string, label: string) => invoke<[string, string]>("studio_label_patch", { iso, label });
export const ifoCandidates = (source: string, showAll: boolean) => invoke<Candidate[]>("studio_ifo_candidates", { source, showAll });
export const sidecarWrite = (kind: "dvd" | "cue" | "mds", image: string, layerBreak: number | null) =>
  invoke<string>("studio_sidecar_write", { kind, image, layerBreak });
export const graphLoad = (path: string) => invoke<GraphFile>("studio_graph_load", { path });
export const graphFolder = () => invoke<string>("studio_graph_folder");
export const templatePreview = (template: string) => invoke<string>("studio_template_preview", { template });

// ---- events ----------------------------------------------------------------------------------

export const onProgress = (cb: (p: Progress) => void): Promise<UnlistenFn> => listen<Progress>("studio-progress", (e) => cb(e.payload));
export const onGraph = (cb: (s: Sample[]) => void): Promise<UnlistenFn> => listen<Sample[]>("studio-graph", (e) => cb(e.payload));
export const onLog = (cb: (m: string) => void): Promise<UnlistenFn> => listen<string>("studio-log", (e) => cb(e.payload));
export const onDone = (cb: (o: Outcome) => void): Promise<UnlistenFn> => listen<Outcome>("studio-done", (e) => cb(e.payload));
