//! Global application settings ("Allgemeine Einstellungen"), persisted as JSON.
//!
//! Default location: `%APPDATA%\convertx-rust\settings.json`. Unknown fields are ignored and
//! missing fields take their defaults, so files from older or newer versions keep loading.
//! [`AppSettings::sanitized`] clamps numbers and replaces unknown choices with the defaults.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::author::{PlaybackOptions, Resolution, TvStandard};
use crate::encoder::{AudioMode, Deinterlace, MenuAudio, ScaleFilter, ScaleMode, VideoProcessing};
use crate::calculator::{DiscType, EncodingProfile};
use crate::dvd::{default_jobs, AspectPolicy, BuildOptions, ChapterSettings};
use crate::menu::TextOverflow;
use crate::tools::ProcessPriority;

/// Choices for the string settings (first entry = default).
pub const TEXT_OVERFLOW_ACTIONS: &[&str] = &["reduce_font_size", "truncate"];
pub const TV_STANDARDS: &[&str] = &["PAL", "NTSC"];
pub const DVD_RESOLUTIONS: &[&str] = &["Full D1", "Half D1", "SIF"];
pub const BD_RESOLUTIONS: &[&str] = &["1080p", "720p"];
pub const ASPECT_POLICIES: &[&str] = &["Auto", "4:3", "16:9"];
pub const ENCODING_PROFILES: &[&str] = &["Auto", "SP_Short", "MP_Medium", "LP_Long"];
pub const PROCESS_PRIORITIES: &[&str] = &["Normal", "BelowNormal", "Idle"];
pub const MENU_AUDIO_MODES: &[&str] = &["title", "silence", "file"];
/// How audio tracks are written: copy compliant AC-3/DTS, always AC-3, LPCM.
pub const AUDIO_MODES: &[&str] = &["auto", "ac3", "lpcm"];
/// "Videobearbeitung": how the picture is fitted into the frame, the swscale algorithm, deinterlacing.
pub const SCALE_MODES: &[&str] = &["letterbox", "panscan", "stretch"];
pub const SCALE_FILTERS: &[&str] = &["bicubic", "bilinear", "lanczos"];
pub const DEINTERLACE_MODES: &[&str] = &["auto", "yadif", "off"];
/// Write speed: `auto` = DVD-5 maximum, DVD-9 4x; otherwise maximum or a speed in x (mapped to
/// the nearest speed the drive offers).
pub const BURN_SPEEDS: &[&str] = &["auto", "max", "16", "12", "8", "6", "4", "2.4"];

/// Choices that are stored but not implemented by the engine yet (none since Half D1, SIF and
/// the SP/MP/LP profiles were added; kept for the GUI's "noch nicht unterstützt" marks).
pub const UNSUPPORTED_CHOICES: &[&str] = &[];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    // Menu options
    pub auto_start_playback: bool,
    pub loop_playback: bool,
    pub play_sequentially: bool,
    pub skip_root_menu: bool,
    pub return_to_title_menu_after_title: bool,
    pub no_title_menu_for_single_title: bool,
    pub text_overflow_action: String,
    /// Sound of the motion menus: `title` (main menu silent, video menus with their video's
    /// sound), `silence`, `file` (main menu with `menu_audio_file`, video menus as `title`).
    pub menu_audio: String,
    /// Audio file for `menu_audio = "file"`.
    pub menu_audio_file: String,
    // Output & TV format
    pub tv_standard: String,
    pub dvd_resolution: String,
    /// Frame size of Blu-ray titles (`1080p` or `720p`); used when the disc is a BD.
    pub bd_resolution: String,
    /// Blu-ray: only one video per disc (no playlist of several videos).
    pub bd_single_video: bool,
    pub aspect_ratio_policy: String,
    // Encoding
    pub encoding_profile: String,
    /// Size used for the "Benutzerdefiniert" disc choice.
    pub target_size_mb: u64,
    pub max_concurrent_encodes: usize,
    pub process_priority: String,
    /// Decode the sources on the GPU (CUDA/D3D11VA/QSV) when available.
    pub hw_decode: bool,
    /// Blu-ray: encode with NVIDIA NVENC when available (falls back to x264).
    pub bd_hw_encode: bool,
    /// Draft encoding without trellis quantisation (about 40 % faster, slightly lower quality).
    pub fast_encode: bool,
    // Automatic chapters
    pub auto_chapter_enabled: bool,
    pub auto_chapter_interval_seconds: u32,
    pub auto_chapter_min_video_length_seconds: u32,
    /// `<c>` is replaced by the chapter number.
    pub chapter_naming_template: String,
    // Audio & subtitles
    /// ISO 639 code; empty = keep the source order.
    pub default_audio_language: String,
    pub default_subtitle_language: String,
    pub force_first_subtitle: bool,
    /// See [`AUDIO_MODES`].
    pub audio_mode: String,
    /// See [`SCALE_MODES`].
    pub scale_mode: String,
    /// See [`SCALE_FILTERS`].
    pub scale_filter: String,
    /// See [`DEINTERLACE_MODES`].
    pub deinterlace: String,
    /// EBU R128 loudness normalisation of all audio tracks (forces a re-encode).
    pub loudnorm: bool,
    // Burning
    /// See [`BURN_SPEEDS`].
    pub burn_speed: String,
    /// Read the disc back and compare it with the image.
    pub verify_after_burn: bool,
    /// Keep the .iso next to the DVD folder.
    pub keep_iso: bool,
    /// After a successful burn: delete the DVD folder, the image and the work directory.
    pub delete_after_burn: bool,
    /// Neither open Explorer on the finished DVD folder nor eject the disc when done.
    pub quiet_finish: bool,
    /// Erase/overwrite rewritable discs with old content without asking (also lets the
    /// auto-start countdown run for them).
    pub auto_erase_rewritable: bool,
    /// Default project folder and volume label (A-Z, 0-9, _; max. 32); empty = from the menu
    /// heading / first video.
    pub default_volume_label: String,
    /// Settings of the Burn Studio window (ImgBurn replacement).
    pub burn_studio: BurnStudioSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            auto_start_playback: false,
            loop_playback: false,
            play_sequentially: false,
            skip_root_menu: false,
            return_to_title_menu_after_title: true,
            no_title_menu_for_single_title: false,
            text_overflow_action: TEXT_OVERFLOW_ACTIONS[0].into(),
            menu_audio: MENU_AUDIO_MODES[0].into(),
            menu_audio_file: String::new(),
            tv_standard: TV_STANDARDS[0].into(),
            dvd_resolution: DVD_RESOLUTIONS[0].into(),
            bd_resolution: BD_RESOLUTIONS[0].into(),
            bd_single_video: false,
            aspect_ratio_policy: ASPECT_POLICIES[0].into(),
            encoding_profile: ENCODING_PROFILES[0].into(),
            target_size_mb: DiscType::Dvd5.target_mb() as u64,
            max_concurrent_encodes: default_jobs(),
            process_priority: PROCESS_PRIORITIES[0].into(),
            hw_decode: false,
            bd_hw_encode: true,
            fast_encode: false,
            auto_chapter_enabled: true,
            auto_chapter_interval_seconds: 300,
            auto_chapter_min_video_length_seconds: 900,
            chapter_naming_template: "Kapitel #<c>".into(),
            default_audio_language: "de".into(),
            default_subtitle_language: String::new(),
            force_first_subtitle: false,
            audio_mode: AUDIO_MODES[0].into(),
            scale_mode: SCALE_MODES[0].into(),
            scale_filter: SCALE_FILTERS[0].into(),
            deinterlace: DEINTERLACE_MODES[0].into(),
            loudnorm: false,
            burn_speed: BURN_SPEEDS[0].into(),
            verify_after_burn: true,
            keep_iso: false,
            delete_after_burn: false,
            quiet_finish: false,
            auto_erase_rewritable: false,
            default_volume_label: String::new(),
            burn_studio: BurnStudioSettings::default(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Burn Studio
// ---------------------------------------------------------------------------------------------

/// Choices of the Burn Studio string settings (first entry = default).
pub const ON_COMPLETE: &[&str] = &["nothing", "shutdown", "standby", "hibernate", "close"];
pub const SPLIT_MODES: &[&str] = &["none", "2gb", "4gb"];
pub const LAYOUT_FILES: &[&str] = &["auto", "none", "dvd", "mds", "cue"];
pub const READ_PRIORITIES: &[&str] = &["normal", "below", "idle"];
pub const BUILD_FS: &[&str] = &["iso_joliet_udf102", "udf102", "udf250"];
pub const PROMPT_MODES: &[&str] = &["ask", "always", "never"];
pub const WRITE_MODES: &[&str] = &["auto", "dvd", "bd"];
pub const WRITE_TYPES: &[&str] = &["dao", "incremental"];
pub const LAYER_BREAK_MODES: &[&str] = &["auto", "user"];
pub const OVERBURN_MODES: &[&str] = &["prompt", "overburn", "truncate"];
pub const IO_BACKENDS: &[&str] = &["auto", "imapi", "direct"];
pub const IO_TRANSPORTS: &[&str] = &["imapi_ex", "spti"];
pub const DEVICE_SORTS: &[&str] = &["letter", "name"];
pub const DEFAULT_GRAPH_TEMPLATE: &str = "%M_%F_%D_%T_%I_%S";

/// Confirmations the user can switch off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioWarnings {
    pub overburn: bool,
    pub dvd_r_dl_layer_break: bool,
    pub no_ifo_update: bool,
    pub seamless_layer_break: bool,
}

impl Default for StudioWarnings {
    fn default() -> Self {
        StudioWarnings { overburn: true, dvd_r_dl_layer_break: true, no_ifo_update: true, seamless_layer_break: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioGeneral {
    pub prevent_standby: bool,
    /// See [`ON_COMPLETE`].
    pub on_complete: String,
    pub warnings: StudioWarnings,
    pub md5_calculate: bool,
    pub md5_compare: bool,
    /// Layer break candidates: prefer cells that are not seamless / show interleaved ones.
    pub lb_check_seamless: bool,
    pub lb_show_interleaved: bool,
}

impl Default for StudioGeneral {
    fn default() -> Self {
        StudioGeneral {
            prevent_standby: true,
            on_complete: ON_COMPLETE[0].into(),
            warnings: StudioWarnings::default(),
            md5_calculate: false,
            md5_compare: false,
            lb_check_seamless: true,
            lb_show_interleaved: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioRead {
    pub split: String,
    pub layout_file: String,
    pub sw_retries: u32,
    /// -1 = drive default.
    pub hw_retries: i32,
    pub ignore_read_errors: bool,
    pub priority: String,
    pub batch_eject_next: bool,
}

impl Default for StudioRead {
    fn default() -> Self {
        StudioRead {
            split: SPLIT_MODES[0].into(),
            layout_file: LAYOUT_FILES[0].into(),
            sw_retries: 20,
            hw_retries: -1,
            ignore_read_errors: false,
            priority: READ_PRIORITIES[0].into(),
            batch_eject_next: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioFilter {
    pub skip_hidden: bool,
    pub skip_system: bool,
    pub skip_reparse: bool,
    pub skip_in_use: bool,
}

impl Default for StudioFilter {
    fn default() -> Self {
        StudioFilter { skip_hidden: true, skip_system: true, skip_reparse: true, skip_in_use: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioDvdBuild {
    pub create_audio_ts: bool,
    pub fix_vts_sectors: bool,
    pub ifo_bup_ecc_padding: bool,
    pub uppercase: bool,
    pub read_dvd_text: bool,
}

impl Default for StudioDvdBuild {
    fn default() -> Self {
        StudioDvdBuild { create_audio_ts: true, fix_vts_sectors: true, ifo_bup_ecc_padding: false, uppercase: true, read_dvd_text: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioBdBuild {
    pub create_certificate: bool,
    pub reset_cpi: bool,
    pub read_bdmt: bool,
}

impl Default for StudioBdBuild {
    fn default() -> Self {
        StudioBdBuild { create_certificate: true, reset_cpi: false, read_bdmt: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioBuild {
    pub split: String,
    pub layout_file: String,
    /// 0 = automatic.
    pub start_lba: u32,
    pub min_sectors: u32,
    /// See [`BUILD_FS`].
    pub fs: String,
    pub dedupe: bool,
    pub filter: StudioFilter,
    pub log_skipped: bool,
    pub dvd: StudioDvdBuild,
    pub bd: StudioBdBuild,
    /// What to do when the content is recognised as DVD-Video / Blu-ray (see [`PROMPT_MODES`]).
    pub prompts: String,
}

impl Default for StudioBuild {
    fn default() -> Self {
        StudioBuild {
            split: SPLIT_MODES[0].into(),
            layout_file: LAYOUT_FILES[0].into(),
            start_lba: 0,
            min_sectors: 600,
            fs: BUILD_FS[0].into(),
            dedupe: true,
            filter: StudioFilter::default(),
            log_skipped: true,
            dvd: StudioDvdBuild::default(),
            bd: StudioBdBuild::default(),
            prompts: PROMPT_MODES[0].into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioWrite {
    pub mode: String,
    pub write_type: String,
    /// 0 = drive default.
    pub link_size: u32,
    /// Book type changing is vendor specific and not implemented; the flag is only stored.
    pub auto_book_type: bool,
    pub opc: bool,
    pub dvdpr_reserve_track: bool,
    pub dvdpr_min_radius: bool,
    pub dvdpr_dl_only_l0: bool,
    pub fastwrite: bool,
    pub bdr_verify_not_required: bool,
    pub burn_proof: bool,
    pub lock_volume: bool,
    pub cycle_tray_before_verify: bool,
    pub verify: bool,
    pub layer_break: String,
    pub user_l0_sectors: u32,
    pub prefer_no_spare: bool,
    pub sw_retries: u32,
    pub overburn: String,
    pub test_mode: bool,
    pub copies: u32,
    pub delete_image_after: bool,
    pub eject_after: bool,
}

impl Default for StudioWrite {
    fn default() -> Self {
        StudioWrite {
            mode: WRITE_MODES[0].into(),
            write_type: WRITE_TYPES[0].into(),
            link_size: 0,
            auto_book_type: false,
            opc: true,
            dvdpr_reserve_track: false,
            dvdpr_min_radius: false,
            dvdpr_dl_only_l0: false,
            fastwrite: false,
            bdr_verify_not_required: false,
            burn_proof: true,
            lock_volume: true,
            cycle_tray_before_verify: false,
            verify: true,
            layer_break: LAYER_BREAK_MODES[0].into(),
            user_l0_sectors: 0,
            prefer_no_spare: true,
            sw_retries: 20,
            overburn: OVERBURN_MODES[0].into(),
            test_mode: false,
            copies: 1,
            delete_image_after: false,
            eject_after: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioVerify {
    pub ignore_non_user_data: bool,
    pub ignore_miscompares: bool,
    pub sw_retries: u32,
    pub hw_retries: i32,
}

impl Default for StudioVerify {
    fn default() -> Self {
        StudioVerify { ignore_non_user_data: true, ignore_miscompares: false, sw_retries: 20, hw_retries: -1 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioGraph {
    pub capture: bool,
    pub capture_cpu: bool,
    pub sample_ms: u32,
    /// File name template, see `burner::graph::format_template`.
    pub template: String,
    /// Empty = `%APPDATA%\convertx-rust\graph`.
    pub folder: String,
}

impl Default for StudioGraph {
    fn default() -> Self {
        StudioGraph { capture: true, capture_cpu: true, sample_ms: 100, template: DEFAULT_GRAPH_TEMPLATE.into(), folder: String::new() }
    }
}

/// Per-operation flags (read, write, verify, erase).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PerOperation {
    pub read: bool,
    pub write: bool,
    pub verify: bool,
    pub erase: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioIo {
    pub backend: String,
    pub transport: String,
    pub ring_mib: u32,
    pub recovery_main_low: u32,
    pub recovery_main_high: u32,
    /// 0 = off.
    pub recovery_device_low: u32,
    /// KiB per transfer, 0 = automatic.
    pub transfer_kib: u32,
    pub buffered_io_read: bool,
    pub buffered_io_write: bool,
    pub lock_tray: PerOperation,
    pub eject_after: PerOperation,
    pub device_sort: String,
}

impl Default for StudioIo {
    fn default() -> Self {
        StudioIo {
            backend: IO_BACKENDS[0].into(),
            transport: IO_TRANSPORTS[0].into(),
            ring_mib: 40,
            recovery_main_low: 80,
            recovery_main_high: 95,
            recovery_device_low: 0,
            transfer_kib: 0,
            buffered_io_read: true,
            buffered_io_write: false,
            lock_tray: PerOperation { read: true, write: true, verify: true, erase: true },
            eject_after: PerOperation { read: false, write: true, verify: false, erase: false },
            device_sort: DEVICE_SORTS[0].into(),
        }
    }
}

/// Write speed wish for a medium (matched on media ID, then drive; first match wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpeedRule {
    pub media_id: String,
    /// Empty = any drive.
    pub drive: String,
    /// `max` or a speed in x.
    pub speed: String,
}

impl Default for SpeedRule {
    fn default() -> Self {
        SpeedRule { media_id: String::new(), drive: String::new(), speed: "max".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BurnStudioSettings {
    pub general: StudioGeneral,
    pub read: StudioRead,
    pub build: StudioBuild,
    pub write: StudioWrite,
    pub verify: StudioVerify,
    pub graph: StudioGraph,
    pub io: StudioIo,
    pub auto_speed: Vec<SpeedRule>,
}

impl BurnStudioSettings {
    /// Clamps numbers and replaces unknown choices with the defaults.
    pub fn sanitized(mut self) -> Self {
        let g = &mut self.general;
        g.on_complete = choice(&g.on_complete, ON_COMPLETE);

        let r = &mut self.read;
        r.split = choice(&r.split, SPLIT_MODES);
        r.layout_file = choice(&r.layout_file, LAYOUT_FILES);
        r.priority = choice(&r.priority, READ_PRIORITIES);
        r.sw_retries = r.sw_retries.min(100);
        r.hw_retries = r.hw_retries.clamp(-1, 100);

        let b = &mut self.build;
        b.split = choice(&b.split, SPLIT_MODES);
        b.layout_file = choice(&b.layout_file, LAYOUT_FILES);
        b.fs = choice(&b.fs, BUILD_FS);
        b.prompts = choice(&b.prompts, PROMPT_MODES);
        b.min_sectors = b.min_sectors.min(2_000_000);

        let w = &mut self.write;
        w.mode = choice(&w.mode, WRITE_MODES);
        w.write_type = choice(&w.write_type, WRITE_TYPES);
        w.layer_break = choice(&w.layer_break, LAYER_BREAK_MODES);
        w.overburn = choice(&w.overburn, OVERBURN_MODES);
        w.link_size = w.link_size.min(255);
        w.sw_retries = w.sw_retries.min(100);
        w.copies = w.copies.clamp(1, 99);
        if w.layer_break == "user" && w.user_l0_sectors == 0 {
            w.layer_break = LAYER_BREAK_MODES[0].into();
        }

        let v = &mut self.verify;
        v.sw_retries = v.sw_retries.min(100);
        v.hw_retries = v.hw_retries.clamp(-1, 100);

        let gr = &mut self.graph;
        gr.sample_ms = gr.sample_ms.clamp(50, 2000);
        if gr.template.trim().is_empty() {
            gr.template = DEFAULT_GRAPH_TEMPLATE.into();
        }
        gr.folder = gr.folder.trim().to_string();

        let io = &mut self.io;
        io.backend = choice(&io.backend, IO_BACKENDS);
        io.transport = choice(&io.transport, IO_TRANSPORTS);
        io.device_sort = choice(&io.device_sort, DEVICE_SORTS);
        io.ring_mib = io.ring_mib.clamp(20, 512);
        io.recovery_main_high = io.recovery_main_high.clamp(1, 100);
        io.recovery_main_low = io.recovery_main_low.min(io.recovery_main_high);
        io.recovery_device_low = io.recovery_device_low.min(100);
        if !matches!(io.transfer_kib, 32 | 64) {
            io.transfer_kib = 0;
        }

        for rule in &mut self.auto_speed {
            rule.media_id = rule.media_id.trim().to_string();
            rule.drive = rule.drive.trim().to_string();
            let sp = rule.speed.trim().to_ascii_lowercase();
            let valid = sp == "max" || sp.parse::<f32>().is_ok_and(|v| v > 0.0 && v <= 100.0);
            rule.speed = if valid { sp } else { "max".into() };
        }
        self.auto_speed.retain(|r| !r.media_id.is_empty());
        self
    }
}


#[derive(Debug)]
pub enum ConfigError {
    Io { path: PathBuf, error: std::io::Error },
    Json { path: PathBuf, error: serde_json::Error },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            ConfigError::Json { path, error } => write!(f, "{}: ungültige Einstellungen ({error})", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

fn choice(value: &str, allowed: &[&str]) -> String {
    allowed.iter().find(|a| a.eq_ignore_ascii_case(value)).unwrap_or(&allowed[0]).to_string()
}

/// Lower-case ISO 639 code (`de`, `ger`), or empty if not a plausible code.
fn language(value: &str) -> String {
    let v = value.trim().to_ascii_lowercase();
    if (2..=3).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase()) {
        v
    } else {
        String::new()
    }
}

impl AppSettings {
    /// Clamps numbers into sensible ranges and normalises the string choices.
    pub fn sanitized(mut self) -> Self {
        self.text_overflow_action = choice(&self.text_overflow_action, TEXT_OVERFLOW_ACTIONS);
        self.tv_standard = choice(&self.tv_standard, TV_STANDARDS);
        self.menu_audio = choice(&self.menu_audio, MENU_AUDIO_MODES);
        self.menu_audio_file = self.menu_audio_file.trim().to_string();
        if self.menu_audio == "file" && self.menu_audio_file.is_empty() {
            self.menu_audio = MENU_AUDIO_MODES[0].into();
        }
        self.dvd_resolution = choice(&self.dvd_resolution, DVD_RESOLUTIONS);
        self.bd_resolution = choice(&self.bd_resolution, BD_RESOLUTIONS);
        self.aspect_ratio_policy = choice(&self.aspect_ratio_policy, ASPECT_POLICIES);
        self.encoding_profile = choice(&self.encoding_profile, ENCODING_PROFILES);
        self.process_priority = choice(&self.process_priority, PROCESS_PRIORITIES);
        self.audio_mode = choice(&self.audio_mode, AUDIO_MODES);
        self.scale_mode = choice(&self.scale_mode, SCALE_MODES);
        self.scale_filter = choice(&self.scale_filter, SCALE_FILTERS);
        self.deinterlace = choice(&self.deinterlace, DEINTERLACE_MODES);
        self.target_size_mb = self.target_size_mb.clamp(100, 20_000);
        self.max_concurrent_encodes = self.max_concurrent_encodes.clamp(1, 64);
        self.auto_chapter_interval_seconds = self.auto_chapter_interval_seconds.clamp(30, 3600);
        self.auto_chapter_min_video_length_seconds = self.auto_chapter_min_video_length_seconds.min(24 * 3600);
        if !self.chapter_naming_template.contains("<c>") {
            self.chapter_naming_template = AppSettings::default().chapter_naming_template;
        }
        self.chapter_naming_template = self.chapter_naming_template.trim().chars().take(60).collect();
        self.default_audio_language = language(&self.default_audio_language);
        self.default_subtitle_language = language(&self.default_subtitle_language);
        self.burn_speed = choice(&self.burn_speed, BURN_SPEEDS);
        self.default_volume_label = if self.default_volume_label.trim().is_empty() {
            String::new()
        } else {
            crate::burner::sanitize_volume_label(self.default_volume_label.trim())
        };
        self.burn_studio = self.burn_studio.sanitized();
        self
    }

    /// `%APPDATA%\convertx-rust\settings.json` (or `./convertx-settings.json` without APPDATA).
    pub fn default_path() -> PathBuf {
        match std::env::var_os("APPDATA") {
            Some(dir) => PathBuf::from(dir).join("convertx-rust").join("settings.json"),
            None => PathBuf::from("convertx-settings.json"),
        }
    }

    /// Loads the settings; a missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<AppSettings>(&text)
                .map(AppSettings::sanitized)
                .map_err(|error| ConfigError::Json { path: path.to_path_buf(), error }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
            Err(error) => Err(ConfigError::Io { path: path.to_path_buf(), error }),
        }
    }

    /// Like [`load`](Self::load), but a corrupt file is renamed to `*.json.bak` and the defaults
    /// are returned together with a message, so the application always starts.
    pub fn load_or_default(path: &Path) -> (Self, Option<String>) {
        match Self::load(path) {
            Ok(s) => (s, None),
            Err(e @ ConfigError::Json { .. }) => {
                let backup = path.with_extension("json.bak");
                let moved = std::fs::rename(path, &backup).is_ok();
                let note = if moved {
                    format!("{e} – gesichert als {}, Standardwerte werden verwendet", backup.display())
                } else {
                    format!("{e} – Standardwerte werden verwendet")
                };
                (AppSettings::default(), Some(note))
            }
            Err(e) => (AppSettings::default(), Some(e.to_string())),
        }
    }

    /// Writes the settings atomically (temporary file + rename).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let io = |error| ConfigError::Io { path: path.to_path_buf(), error };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let json = serde_json::to_string_pretty(&self.clone().sanitized()).expect("settings serialize");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }

    /// The encoding profile setting as the engine's type.
    pub fn profile(&self) -> EncodingProfile {
        match self.encoding_profile.as_str() {
            "SP_Short" => EncodingProfile::Sp,
            "MP_Medium" => EncodingProfile::Mp,
            "LP_Long" => EncodingProfile::Lp,
            _ => EncodingProfile::Auto,
        }
    }

    pub fn playback(&self) -> PlaybackOptions {
        PlaybackOptions {
            auto_start: self.auto_start_playback,
            play_sequentially: self.play_sequentially,
            loop_playback: self.loop_playback,
            return_to_title_menu: self.return_to_title_menu_after_title,
        }
    }

    pub fn chapters(&self) -> ChapterSettings {
        ChapterSettings {
            interval_secs: if self.auto_chapter_enabled { self.auto_chapter_interval_seconds as f64 } else { 0.0 },
            min_video_secs: self.auto_chapter_min_video_length_seconds as f64,
            label_template: Some(self.chapter_naming_template.clone()),
        }
    }

    /// Transfers the settings the engine implements onto build options.
    pub fn apply_to(&self, opts: &mut BuildOptions) {
        opts.playback = self.playback();
        opts.skip_root_menu = self.skip_root_menu;
        opts.no_title_menu_for_single_title = self.no_title_menu_for_single_title;
        opts.text_overflow = if self.text_overflow_action == "truncate" {
            TextOverflow::Truncate
        } else {
            TextOverflow::ReduceFontSize
        };
        opts.aspect_policy = match self.aspect_ratio_policy.as_str() {
            "4:3" => AspectPolicy::Standard,
            "16:9" => AspectPolicy::Wide,
            _ => AspectPolicy::Auto,
        };
        opts.parallel_jobs = Some(self.max_concurrent_encodes);
        opts.process_priority = match self.process_priority.as_str() {
            "BelowNormal" => ProcessPriority::BelowNormal,
            "Idle" => ProcessPriority::Idle,
            _ => ProcessPriority::Normal,
        };
        opts.chapters = self.chapters();
        let lang = |l: &str| (!l.is_empty()).then(|| l.to_string());
        opts.preferred_audio_lang = lang(&self.default_audio_language);
        opts.preferred_subtitle_lang = lang(&self.default_subtitle_language);
        opts.force_first_subtitle = self.force_first_subtitle;
        opts.audio_mode = match self.audio_mode.as_str() {
            "ac3" => AudioMode::Ac3,
            "lpcm" => AudioMode::Lpcm,
            _ => AudioMode::Auto,
        };
        opts.video = VideoProcessing {
            scale: match self.scale_mode.as_str() {
                "panscan" => ScaleMode::PanScan,
                "stretch" => ScaleMode::Stretch,
                _ => ScaleMode::Letterbox,
            },
            filter: match self.scale_filter.as_str() {
                "bilinear" => ScaleFilter::Bilinear,
                "lanczos" => ScaleFilter::Lanczos,
                _ => ScaleFilter::Bicubic,
            },
            deinterlace: match self.deinterlace.as_str() {
                "yadif" => Deinterlace::Yadif,
                "off" => Deinterlace::Off,
                _ => Deinterlace::Auto,
            },
        };
        opts.loudnorm = self.loudnorm;
        opts.hw_decode = self.hw_decode;
        opts.bd_hw_encode = self.bd_hw_encode;
        opts.fast_encode = self.fast_encode;
        if opts.project_name.is_none() && !self.default_volume_label.is_empty() {
            opts.project_name = Some(self.default_volume_label.clone());
        }
        opts.tv_standard = if self.tv_standard == "NTSC" { TvStandard::Ntsc } else { TvStandard::Pal };
        // `opts.disc` has to be set before the settings are applied.
        opts.resolution = if opts.disc.is_bluray() {
            if self.bd_resolution == "720p" { Resolution::Hd720 } else { Resolution::Hd1080 }
        } else {
            match self.dvd_resolution.as_str() {
                "Half D1" => Resolution::HalfD1,
                "SIF" => Resolution::Sif,
                _ => Resolution::FullD1,
            }
        };
        opts.bd_single_video = self.bd_single_video;
        opts.encoding_profile = self.profile();
        opts.motion_audio = match self.menu_audio.as_str() {
            "silence" => MenuAudio::Silence,
            "file" if !self.menu_audio_file.is_empty() => MenuAudio::File(PathBuf::from(&self.menu_audio_file)),
            _ => MenuAudio::TitleAudio,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cx_config_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("settings.json")
    }

    #[test]
    fn defaults() {
        let s = AppSettings::default();
        assert_eq!(s.target_size_mb, 4300);
        assert_eq!(s.chapter_naming_template, "Kapitel #<c>");
        assert_eq!(s.default_audio_language, "de");
        assert!(s.return_to_title_menu_after_title);
        assert_eq!(s.max_concurrent_encodes, default_jobs(), "same default as the engine");
        assert_eq!(s.clone().sanitized(), s, "defaults are already valid");
    }

    #[test]
    fn roundtrip_and_partial_files() {
        let path = temp("roundtrip");
        let mut s = AppSettings::default();
        s.loop_playback = true;
        s.process_priority = "Idle".into();
        s.save(&path).unwrap();
        assert_eq!(AppSettings::load(&path).unwrap(), s);
        assert!(!path.with_extension("json.tmp").exists());

        // Older/newer files: missing fields get defaults, unknown fields are ignored.
        std::fs::write(&path, r#"{"loop_playback": true, "some_future_option": 42}"#).unwrap();
        let s = AppSettings::load(&path).unwrap();
        assert!(s.loop_playback);
        assert_eq!(s.target_size_mb, 4300);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_file_is_default() {
        assert_eq!(AppSettings::load(&temp("missing")).unwrap(), AppSettings::default());
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let path = temp("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let (s, note) = AppSettings::load_or_default(&path);
        assert_eq!(s, AppSettings::default());
        assert!(note.unwrap().contains("Standardwerte"));
        assert!(path.with_extension("json.bak").is_file());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn sanitizing() {
        let s = AppSettings {
            tv_standard: "secam".into(),
            aspect_ratio_policy: "16:9".into(),
            process_priority: "realtime".into(),
            max_concurrent_encodes: 0,
            target_size_mb: 5,
            auto_chapter_interval_seconds: 1,
            chapter_naming_template: "ohne Platzhalter".into(),
            default_audio_language: "Deutsch".into(),
            default_subtitle_language: " EN ".into(),
            menu_audio: "file".into(),
            menu_audio_file: "  ".into(),
            ..AppSettings::default()
        }
        .sanitized();
        assert_eq!(s.menu_audio, "title", "file mode without a file falls back");
        let old = AppSettings { menu_audio: "first_title".into(), ..AppSettings::default() }.sanitized();
        assert_eq!(old.menu_audio, "title", "value of older versions is migrated");
        assert_eq!(s.tv_standard, "PAL");
        assert_eq!(s.aspect_ratio_policy, "16:9");
        assert_eq!(s.process_priority, "Normal");
        assert_eq!(s.max_concurrent_encodes, 1);
        assert_eq!(s.target_size_mb, 100);
        assert_eq!(s.auto_chapter_interval_seconds, 30);
        assert_eq!(s.chapter_naming_template, "Kapitel #<c>");
        assert_eq!(s.default_audio_language, "");
        assert_eq!(s.default_subtitle_language, "en");
    }

    #[test]
    fn applies_to_build_options() {
        let s = AppSettings {
            play_sequentially: true,
            skip_root_menu: true,
            aspect_ratio_policy: "4:3".into(),
            max_concurrent_encodes: 3,
            process_priority: "BelowNormal".into(),
            auto_chapter_enabled: false,
            text_overflow_action: "truncate".into(),
            default_subtitle_language: "en".into(),
            ..AppSettings::default()
        };
        let mut o = BuildOptions::default();
        s.apply_to(&mut o);
        assert!(o.playback.play_sequentially && o.skip_root_menu);
        assert_eq!(o.aspect_policy, AspectPolicy::Standard);
        assert_eq!(o.parallel_jobs, Some(3));
        assert_eq!(o.process_priority, ProcessPriority::BelowNormal);
        assert_eq!(o.chapters.interval_secs, 0.0, "automatic chapters off");
        assert_eq!(o.text_overflow, TextOverflow::Truncate);
        assert_eq!(o.preferred_audio_lang.as_deref(), Some("de"));
        assert_eq!(o.preferred_subtitle_lang.as_deref(), Some("en"));
        assert_eq!(o.tv_standard, TvStandard::Pal);
        assert_eq!(o.motion_audio, MenuAudio::TitleAudio);

        let s = AppSettings {
            tv_standard: "NTSC".into(),
            menu_audio: "file".into(),
            menu_audio_file: r"C:\Musik\menu.mp3".into(),
            ..AppSettings::default()
        };
        s.apply_to(&mut o);
        assert_eq!(o.tv_standard, TvStandard::Ntsc);
        assert_eq!(o.motion_audio, MenuAudio::File(r"C:\Musik\menu.mp3".into()));
        let s = AppSettings { menu_audio: "silence".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!(o.motion_audio, MenuAudio::Silence);
        assert!(UNSUPPORTED_CHOICES.is_empty());
        assert_eq!((o.resolution, o.encoding_profile), (Resolution::FullD1, EncodingProfile::Auto), "defaults");
        let s = AppSettings { dvd_resolution: "Half D1".into(), encoding_profile: "LP_Long".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!((o.resolution, o.encoding_profile), (Resolution::HalfD1, EncodingProfile::Lp));
        let s = AppSettings { dvd_resolution: "SIF".into(), encoding_profile: "MP_Medium".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!((o.resolution, o.encoding_profile), (Resolution::Sif, EncodingProfile::Mp));
        assert_eq!(AppSettings { encoding_profile: "SP_Short".into(), ..AppSettings::default() }.profile(), EncodingProfile::Sp);
    }

    #[test]
    fn video_processing_settings() {
        let mut o = BuildOptions::default();
        AppSettings::default().apply_to(&mut o);
        assert_eq!(o.video, VideoProcessing::default());
        assert_eq!(o.video.filter, ScaleFilter::Bicubic);
        let s = AppSettings { scale_mode: "panscan".into(), scale_filter: "lanczos".into(), deinterlace: "off".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!(o.video, VideoProcessing { scale: ScaleMode::PanScan, filter: ScaleFilter::Lanczos, deinterlace: Deinterlace::Off });
        let s = AppSettings { scale_mode: "stretch".into(), deinterlace: "yadif".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!((o.video.scale, o.video.deinterlace), (ScaleMode::Stretch, Deinterlace::Yadif));
        let bad = AppSettings { scale_mode: "x".into(), scale_filter: "y".into(), deinterlace: "z".into(), ..AppSettings::default() }.sanitized();
        assert_eq!((bad.scale_mode.as_str(), bad.scale_filter.as_str(), bad.deinterlace.as_str()), ("letterbox", "bicubic", "auto"));
    }

    #[test]
    fn blu_ray_resolution_follows_the_disc() {
        let mut o = BuildOptions { disc: DiscType::Bd25, ..BuildOptions::default() };
        AppSettings::default().apply_to(&mut o);
        assert_eq!((o.resolution, o.bd_single_video), (Resolution::Hd1080, false));
        let s = AppSettings { bd_resolution: "720p".into(), bd_single_video: true, dvd_resolution: "SIF".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!((o.resolution, o.bd_single_video), (Resolution::Hd720, true));
        o.disc = DiscType::Dvd9;
        s.apply_to(&mut o);
        assert_eq!(o.resolution, Resolution::Sif, "DVD discs keep using the DVD resolution");
        // Unknown choices fall back; old files without the new fields load with defaults.
        assert_eq!(AppSettings { bd_resolution: "4k".into(), ..AppSettings::default() }.sanitized().bd_resolution, "1080p");
        let old: AppSettings = serde_json::from_str(r#"{"dvd_resolution":"Half D1"}"#).unwrap();
        assert_eq!((old.bd_resolution.as_str(), old.bd_single_video), ("1080p", false));
    }
}

#[cfg(test)]
mod burn_settings_tests {
    use super::*;

    #[test]
    fn burn_settings_are_sanitised_and_applied() {
        let s = AppSettings {
            burn_speed: "99x".into(),
            default_volume_label: "  Urlaub am Meer 2025!  ".into(),
            hw_decode: true,
            fast_encode: true,
            ..AppSettings::default()
        }
        .sanitized();
        assert_eq!(s.burn_speed, "auto", "unknown speed falls back to auto");
        assert_eq!(s.default_volume_label, "URLAUB_AM_MEER_2025_");
        assert_eq!(AppSettings { burn_speed: "2.4".into(), ..AppSettings::default() }.sanitized().burn_speed, "2.4");
        assert_eq!(AppSettings { default_volume_label: "   ".into(), ..AppSettings::default() }.sanitized().default_volume_label, "");

        let mut opts = BuildOptions::default();
        s.apply_to(&mut opts);
        assert!(opts.hw_decode && opts.fast_encode && opts.bd_hw_encode);
        assert!(AppSettings::default().bd_hw_encode, "NVENC is on by default");
        let old: AppSettings = serde_json::from_str(r#"{"hw_decode":true}"#).unwrap();
        assert!(old.bd_hw_encode, "settings files without the field keep the default");
        assert_eq!(opts.project_name.as_deref(), Some("URLAUB_AM_MEER_2025_"), "default project name");
        // An explicit project name wins.
        let mut opts = BuildOptions { project_name: Some("Mein Film".into()), ..BuildOptions::default() };
        s.apply_to(&mut opts);
        assert_eq!(opts.project_name.as_deref(), Some("Mein Film"));

        // Old settings files without the new fields keep loading with the defaults.
        let old: AppSettings = serde_json::from_str(r#"{"tv_standard":"NTSC"}"#).unwrap();
        assert_eq!(old.burn_speed, "auto");
        assert!(old.verify_after_burn && !old.delete_after_burn && !old.quiet_finish && !old.auto_erase_rewritable);
        assert!(!old.fast_encode, "full-quality encoding unless switched on");
    }
}

#[cfg(test)]
mod burn_studio_tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_round_trip() {
        let d = BurnStudioSettings::default();
        assert_eq!(d.clone().sanitized(), d);
        assert_eq!((d.io.ring_mib, d.build.min_sectors, d.graph.sample_ms), (40, 600, 100));
        let json = serde_json::to_string(&AppSettings::default()).unwrap();
        assert_eq!(serde_json::from_str::<AppSettings>(&json).unwrap(), AppSettings::default());
    }

    #[test]
    fn old_settings_without_burn_studio_load() {
        let s: AppSettings = serde_json::from_str(r#"{"tv_standard":"NTSC"}"#).unwrap();
        assert_eq!(s.burn_studio, BurnStudioSettings::default());
        let s: AppSettings = serde_json::from_str(r#"{"burn_studio":{"write":{"copies":3}}}"#).unwrap();
        assert_eq!((s.burn_studio.write.copies, s.burn_studio.write.verify), (3, true), "partial sections keep defaults");
    }

    #[test]
    fn unknown_choices_and_numbers_are_corrected() {
        let mut b = BurnStudioSettings::default();
        b.general.on_complete = "explode".into();
        b.write.overburn = "x".into();
        b.write.copies = 0;
        b.write.layer_break = "user".into(); // no sectors given
        b.io.ring_mib = 1;
        b.io.backend = "spti".into();
        b.io.transfer_kib = 48;
        b.io.recovery_main_low = 99;
        b.io.recovery_main_high = 50;
        b.graph.sample_ms = 5;
        b.graph.template = " ".into();
        b.read.hw_retries = -7;
        b.auto_speed = vec![
            SpeedRule { media_id: " RITEK-S04-66 ".into(), drive: String::new(), speed: "8".into() },
            SpeedRule { media_id: "CMC MAG-M01-00".into(), drive: String::new(), speed: "schnell".into() },
            SpeedRule { media_id: "  ".into(), drive: String::new(), speed: "4".into() },
        ];
        let s = b.sanitized();
        assert_eq!((s.general.on_complete.as_str(), s.write.overburn.as_str()), ("nothing", "prompt"));
        assert_eq!((s.write.copies, s.write.layer_break.as_str()), (1, "auto"));
        assert_eq!((s.io.ring_mib, s.io.backend.as_str(), s.io.transfer_kib), (20, "auto", 0));
        assert_eq!((s.io.recovery_main_low, s.io.recovery_main_high), (50, 50));
        assert_eq!((s.graph.sample_ms, s.graph.template.as_str()), (50, DEFAULT_GRAPH_TEMPLATE));
        assert_eq!(s.read.hw_retries, -1);
        assert_eq!(s.auto_speed.len(), 2, "rule without media ID dropped");
        assert_eq!((s.auto_speed[0].media_id.as_str(), s.auto_speed[0].speed.as_str()), ("RITEK-S04-66", "8"));
        assert_eq!(s.auto_speed[1].speed, "max", "unparsable speed");
    }
}
