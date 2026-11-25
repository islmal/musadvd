//! Tauri desktop GUI for the convertx-rust engine.
//!
//! Commands (called from the frontend via `invoke`):
//! - `probe_file`         analyse a dropped video (ffprobe via `probe.rs`)
//! - `list_templates`     ConvertX menu templates found on this machine
//! - `template_preview`   template thumbnail as data URL
//! - `calculate_project`  bitrate and disc fill level for DVD5/DVD9/BD-25/BD-50/AVCHD
//! - `preview_stream`, `preview_stop`  live player of the edit tab (see `preview.rs`)
//! - `source_frame`, `detect_crop`  crop inspector (frame preview, black bar detection)
//! - `start_conversion`   run the full pipeline in the background; progress arrives as
//!   `convert-event` events, the result as one `convert-done` event
//! - `default_output_dir` suggestion for the output folder
//! - burning (see `burn.rs`): `list_drives`, `disc_needs`, `probe_media`, `start_burn`,
//!   `cancel_burn`

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod burn;
mod burn_studio;
mod menu_editor;
mod preview;
mod self_update;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use musadvd::author::Resolution;
use musadvd::bd::{bd_frame_rate, BdRate};
use musadvd::calculator::{Clamp, DiscType, EncodingProfile, Quality};
use musadvd::chapters::{self, ImportedChapter};
use musadvd::config::AppSettings;
use musadvd::dvd::{self, BuildError, BuildOptions, CancelToken, Event, StreamSelection, TitleLoad};
use musadvd::encoder::{
    self, ac3_settings, max_video_rate, parse_psnr_average, plan_audio_kbps, psnr_verdict,
    quality_psnr_args, Aspect, AudioMode, AudioOut, AudioTarget, VBV_BUFFER_KBIT,
};
use musadvd::menu::{UiLang, DEFAULT_MOTION_SECS};
use musadvd::probe::{self, language_name, ExternalSubtitle, ExternalSubtitleInfo, SubtitleKind, TrackLabelMode};
use musadvd::template::Template;
use musadvd::tools::{blocking_tool_command, Toolchain};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::OnceCell;

/// Where ConvertXToDVD 7 installs its menu templates (Windows only; elsewhere the user sets `template_dir`).
const DEFAULT_TEMPLATE_ROOT: &str = r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates";
struct AppState {
    tools: OnceCell<Toolchain>,
    converting: Arc<AtomicBool>,
    /// Global settings ("Allgemeine Einstellungen") and where they are stored.
    settings: Mutex<AppSettings>,
    settings_path: PathBuf,
    /// Problem while loading the settings file (shown once in the GUI).
    settings_note: Mutex<Option<String>>,
    /// Cancel handle of the running conversion.
    cancel: Arc<Mutex<Option<CancelToken>>>,
    /// Startup hardware-acceleration auto-detection ran once.
    hw_accel_checked: OnceCell<()>,
}

impl AppState {
    fn new() -> Self {
        let settings_path = AppSettings::default_path();
        let (settings, note) = AppSettings::load_or_default(&settings_path);
        AppState {
            tools: OnceCell::new(),
            converting: Arc::default(),
            settings: Mutex::new(settings),
            settings_path,
            settings_note: Mutex::new(note),
            cancel: Arc::default(),
            hw_accel_checked: OnceCell::new(),
        }
    }

    fn settings(&self) -> AppSettings {
        self.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// On startup, if the configured GPU vendor is unavailable on this machine, switch to the
    /// available one (or `None`) and persist it.
    async fn ensure_hw_accel_detected(&self) {
        self.hw_accel_checked
            .get_or_init(|| async {
                let current = self.settings().hw_accel_mode;
                if current == musadvd::config::HwAccelMode::None {
                    return;
                }
                let Ok(tools) = toolchain(self).await else {
                    return;
                };
                let resolved = tools.resolve_hw_accel(current).await;
                if resolved != current {
                    let mut guard = self.settings.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.hw_accel_mode == current {
                        guard.hw_accel_mode = resolved;
                        let _ = guard.save(&self.settings_path);
                    }
                }
            })
            .await;
    }
}

async fn toolchain(state: &AppState) -> Result<&Toolchain, String> {
    state
        .tools
        .get_or_try_init(|| async { Toolchain::detect().await.map_err(|e| e.to_string()) })
        .await
}

// ---------------------------------------------------------------------------
// probe_file
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoInfo {
    codec: String,
    width: u32,
    height: u32,
    display_aspect: f64,
    fps: f64,
    interlaced: bool,
    /// Aspect ratio the DVD title will get.
    dvd_aspect: String,
    /// Blu-ray/AVCHD stream copy of the untrimmed, uncropped file: its average kbit/s, or why
    /// the stream itself rules a copy out (the disc's bitrate limit is checked only by the build).
    bd_copy_kbps: Option<u32>,
    bd_copy_issue: Option<String>,
    /// DVD-Video copy of the untrimmed, uncropped file at Full D1 with the source aspect and no
    /// picture processing, keyed "PAL"/"NTSC" (each key is in exactly one of the two maps). The GUI
    /// adds the trim/crop/filter/resolution checks itself; the per-title bitrate limit
    /// (audio-dependent) is checked only by the build.
    dvd_copy_kbps: std::collections::BTreeMap<&'static str, u32>,
    dvd_copy_issue: std::collections::BTreeMap<&'static str, String>,
    /// The Blu-ray format the file already has (`1080`/`720` and a rate name): a copy needs the
    /// title to keep exactly this format.
    bd_format: Option<BdFormat>,
    /// The same three for AVCHD discs, which also take 1440×1080.
    avchd_copy_kbps: Option<u32>,
    avchd_copy_issue: Option<String>,
    avchd_format: Option<BdFormat>,
    /// Rate the build gives the title when none is chosen, per `<PAL|NTSC><1080|720>`: lets the
    /// GUI recheck the copy verdict when the TV standard or resolution setting changes.
    bd_default_rates: std::collections::BTreeMap<String, &'static str>,
}

#[derive(Serialize)]
struct BdFormat {
    resolution: &'static str,
    rate: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioTrack {
    /// ffprobe stream index (what `StreamSelection::audio` expects).
    index: u32,
    codec: String,
    channels: u32,
    channel_layout: String,
    language: Option<String>,
    language_name: String,
    /// Track name by the label mode setting (no channel suffix).
    display_label: String,
    title: Option<String>,
    default: bool,
    /// AC-3 bitrate and layout on the DVD.
    ac3_kbps: u32,
    dvd_layout: String,
    /// What happens to the track per disc format and audio mode (the live estimate and the
    /// track list pick the entry of the current settings).
    plans: AudioPlans,
}

/// Bitrate, layout and whether the track is copied, for one disc format and audio mode.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioChoice {
    kbps: u32,
    /// `display_label` plus this plan's channel layout (what the disc menu shows).
    label: String,
    layout: String,
    copy: bool,
    /// `ac3`, `dts` or `lpcm`.
    codec: String,
}

#[derive(Serialize)]
struct AudioModes {
    auto: AudioChoice,
    ac3: AudioChoice,
    lpcm: AudioChoice,
}

#[derive(Serialize)]
struct AudioPlans {
    dvd: AudioModes,
    bluray: AudioModes,
    avchd: AudioModes,
}

fn audio_choice(out: AudioOut, base: &str, mode: TrackLabelMode) -> AudioChoice {
    AudioChoice {
        kbps: out.kbps(),
        label: probe::with_layout(base, out.layout_label(), mode),
        layout: out.layout_label().into(),
        copy: matches!(out, AudioOut::Copy { .. }),
        codec: match out.kind() {
            musadvd::encoder::AudioKind::Ac3 => "ac3",
            musadvd::encoder::AudioKind::Dts
            | musadvd::encoder::AudioKind::DtsHdHr
            | musadvd::encoder::AudioKind::DtsHdMa => "dts",
            musadvd::encoder::AudioKind::Lpcm => "lpcm",
        }
        .into(),
    }
}

fn audio_plans(a: &probe::AudioStream, base: &str, mode: TrackLabelMode, ac3_kbps: u32) -> AudioPlans {
    // `auto` is the copy candidate; `ac3` / `lpcm` are the pure re-encode targets the GUI falls
    // back to (Blu-ray Auto with an explicit format copies only tracks already in that format).
    let encode = musadvd::encoder::BdAudioMode::Encode;
    let modes = |t| AudioModes {
        auto: audio_choice(plan_audio_kbps(a, t, AudioMode::Auto, false, ac3_kbps), base, mode),
        ac3: audio_choice(musadvd::encoder::plan_audio_with(a, t, AudioMode::Ac3, encode, false, ac3_kbps), base, mode),
        lpcm: audio_choice(musadvd::encoder::plan_audio_with(a, t, AudioMode::Lpcm, encode, false, ac3_kbps), base, mode),
    };
    AudioPlans { dvd: modes(AudioTarget::Dvd), bluray: modes(AudioTarget::Bluray), avchd: modes(AudioTarget::Avchd) }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleTrack {
    index: u32,
    codec: String,
    /// `text` (rendered natively) or `bitmap` (PGS/VobSub converted, others unsupported).
    kind: String,
    language: Option<String>,
    language_name: String,
    display_label: String,
    title: Option<String>,
    forced: bool,
    supported: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChapterInfo {
    start_secs: f64,
    title: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoMetadata {
    path: String,
    file_name: String,
    size_mb: f64,
    duration_secs: f64,
    video: Option<VideoInfo>,
    audio: Vec<AudioTrack>,
    subtitles: Vec<SubtitleTrack>,
    /// Chapters as they will be authored by default (source chapters or a 5-minute grid).
    chapters: Vec<ChapterInfo>,
    /// The file has its own chapter marks.
    has_source_chapters: bool,
}

type DvdCopyVerdicts = (std::collections::BTreeMap<&'static str, u32>, std::collections::BTreeMap<&'static str, String>);

/// Per-standard DVD stream-copy verdict of the file as it is (no trim/crop/filters).
fn dvd_copy_verdicts(info: &probe::MediaInfo) -> DvdCopyVerdicts {
    let mut out = DvdCopyVerdicts::default();
    let Some(v) = info.video.as_ref() else { return out };
    for (tv, name) in [(musadvd::author::TvStandard::Pal, "PAL"), (musadvd::author::TvStandard::Ntsc, "NTSC")] {
        match encoder::dvd_video_copy(
            info,
            tv,
            Resolution::FullD1,
            Aspect::for_source(v.display_aspect),
            encoder::VideoProcessing::default(),
            encoder::MAX_VIDEO_PEAK_KBPS,
        ) {
            Ok(c) => {
                out.0.insert(name, c.kbps);
            }
            Err(why) => {
                out.1.insert(name, why);
            }
        }
    }
    out
}

#[tauri::command]
async fn probe_file(state: State<'_, AppState>, path: String) -> Result<VideoMetadata, String> {
    let tools = toolchain(&state).await?;
    let p = PathBuf::from(&path);
    let info = probe::probe(&tools.ffprobe, &p).await.map_err(|e| e.to_string())?;
    let settings = state.settings();
    let label_mode = TrackLabelMode::from_setting(&settings.track_label_mode);
    let ui = UiLang::from_setting(&settings.ui_language);
    let audio_labels = probe::audio_labels(&info, label_mode, ui);
    let subtitle_labels = probe::subtitle_labels(&info, label_mode, ui);
    let chapter_settings = state.settings().chapters();
    // AVCHD also takes 1440×1080, so it gets its own native format and verdict.
    // Probe with default VideoProcessing (like `dvd_copy_verdicts`); the GUI checks forced
    // deinterlacing dynamically so toggling it in Settings updates badges without re-probing.
    let verdict = |avchd: bool| {
        let native = info.video.as_ref().and_then(|v| musadvd::bd::bd_native_format(v, avchd));
        let copy = musadvd::encoder::bd_stream_copy(&info, encoder::VideoProcessing::default(), u32::MAX).and_then(|kbps| match (&info.video, native) {
            (Some(v), None) => Err(musadvd::encoder::not_bd_format(v)),
            _ => Ok(kbps),
        });
        (native, copy)
    };
    let (bd_native, bd_copy) = verdict(false);
    let (avchd_native, avchd_copy) = verdict(true);
    let format = |n: Option<(musadvd::author::Resolution, musadvd::bd::BdRate)>| {
        n.map(|(res, rate)| BdFormat { resolution: if res == musadvd::author::Resolution::Hd720 { "720" } else { "1080" }, rate: rate.name() })
    };
    let dvd_copy = dvd_copy_verdicts(&info);
    let chapters = dvd::chapter_starts(&info, chapter_settings.interval_secs, chapter_settings.min_video_secs)
        .into_iter()
        .map(|(start_secs, title)| ChapterInfo { start_secs, title })
        .collect();
    Ok(VideoMetadata {
        file_name: p.file_name().map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned()),
        size_mb: std::fs::metadata(&p).map_or(0.0, |m| m.len() as f64 / 1024.0 / 1024.0),
        path,
        duration_secs: info.duration_secs,
        video: info.video.as_ref().map(|v| VideoInfo {
            codec: v.codec.clone(),
            width: v.width,
            height: v.height,
            display_aspect: v.display_aspect,
            fps: v.fps,
            interlaced: v.is_interlaced(),
            dvd_aspect: Aspect::for_source(v.display_aspect).as_str().into(),
            bd_copy_kbps: bd_copy.as_ref().ok().copied(),
            bd_copy_issue: bd_copy.as_ref().err().cloned(),
            dvd_copy_kbps: dvd_copy.0,
            dvd_copy_issue: dvd_copy.1,
            bd_default_rates: {
                use musadvd::author::Resolution::{Hd1080, Hd720};
                let mut m = std::collections::BTreeMap::new();
                for (tv, tv_name) in [(musadvd::author::TvStandard::Pal, "PAL"), (musadvd::author::TvStandard::Ntsc, "NTSC")] {
                    for (res, res_name) in [(Hd1080, "1080"), (Hd720, "720")] {
                        m.insert(format!("{tv_name}{res_name}"), musadvd::bd::bd_frame_rate(&[v.fps], tv, res).name());
                    }
                }
                m
            },
            bd_format: format(bd_native),
            avchd_copy_kbps: avchd_copy.as_ref().ok().copied(),
            avchd_copy_issue: avchd_copy.as_ref().err().cloned(),
            avchd_format: format(avchd_native),
        }),
        audio: info
            .audio
            .iter()
            .enumerate()
            .map(|(n, a)| {
                let (kbps, ch) = ac3_settings(a.channels, AudioTarget::Dvd, settings.audio_bitrate_kbps);
                AudioTrack {
                    index: a.index,
                    codec: a.codec.clone(),
                    channels: a.channels,
                    channel_layout: a.channel_layout.clone(),
                    language_name: language_name(a.language.as_deref(), ui),
                    display_label: audio_labels[n].clone(),
                    language: a.language.clone(),
                    title: a.title.clone(),
                    default: a.default,
                    ac3_kbps: kbps,
                    dvd_layout: if ch == 6 { "5.1".into() } else { "Stereo".into() },
                    plans: audio_plans(a, &audio_labels[n], label_mode, settings.audio_bitrate_kbps),
                }
            })
            .collect(),
        subtitles: info
            .subtitles
            .iter()
            .enumerate()
            .map(|(n, s)| SubtitleTrack {
                index: s.index,
                codec: s.codec.clone(),
                kind: if s.kind == SubtitleKind::Text { "text".into() } else { "bitmap".into() },
                language_name: language_name(s.language.as_deref(), ui),
                display_label: subtitle_labels[n].clone(),
                language: s.language.clone(),
                title: s.title.clone(),
                forced: s.forced,
                supported: s.kind == SubtitleKind::Text || musadvd::subpic::supported(&s.codec),
            })
            .collect(),
        has_source_chapters: !info.chapters.is_empty(),
        chapters,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BdCopyScanResult {
    issue: Option<String>,
    needs_timing: bool,
}

/// Scans a Blu-ray/AVCHD video stream-copy candidate through the disc's VBV buffer and keyframe
/// distance rules (`bd_mux::scan_video`), matching the build-time check.
#[tauri::command]
async fn scan_bd_copy(state: State<'_, AppState>, path: String, disc: DiscType, rate: String) -> Result<BdCopyScanResult, String> {
    if !disc.is_bluray() {
        return Err("not a Blu-ray or AVCHD disc".into());
    }
    let bd_rate = musadvd::bd::BdRate::from_name(&rate, musadvd::author::Resolution::Hd1080)
        .or_else(|| musadvd::bd::BdRate::from_name(&rate, musadvd::author::Resolution::Hd720))
        .ok_or_else(|| format!("unknown Blu-ray frame rate {rate}"))?;
    let tools = toolchain(&state).await?;
    let info = probe::probe(&tools.ffprobe, Path::new(&path)).await.map_err(|e| e.to_string())?;
    let v = info.video.as_ref().ok_or("no video stream")?;
    let limits = disc.limits();
    let vbv = (u64::from(limits.vbv_maxrate_kbps) * 1000, u64::from(limits.vbv_bufsize_kbit) * 1000);
    let args = musadvd::bd_mux::scan_args(info.input_args(), v.index);
    let (fps, keyint) = (bd_rate.fps, bd_rate.keyint());
    let scan = tools
        .ffmpeg_sink(&args, info.duration_secs, |_| {}, move |pipe| {
            musadvd::bd_mux::scan_video(std::io::BufReader::with_capacity(1 << 20, pipe), fps, vbv)
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(match scan {
        Ok(s) => BdCopyScanResult { issue: s.issue(keyint), needs_timing: s.needs_timing },
        Err(why) => BdCopyScanResult { issue: Some(why), needs_timing: false },
    })
}

/// Probes a standalone external subtitle file (`.srt`, `.ass`, `.ssa`, `.vtt`, `.sub`, `.sup`).
#[tauri::command]
fn probe_subtitle_file(state: State<'_, AppState>, path: String) -> Result<ExternalSubtitleInfo, String> {
    let settings = state.settings();
    let label_mode = TrackLabelMode::from_setting(&settings.track_label_mode);
    let ui = UiLang::from_setting(&settings.ui_language);
    probe::probe_external_subtitle_with_ui(Path::new(&path), label_mode, ui)
}

/// Reads and parses a chapter file (OGG/MKVToolNix, Matroska XML, FFMETADATA, plain/YouTube, CellTimes.txt).
#[tauri::command]
fn import_chapters_file(path: String, fps: Option<f64>) -> Result<Vec<ImportedChapter>, String> {
    chapters::read_chapters_file(Path::new(&path), fps)
}

/// Parses pasted chapter text in any supported chapter format.
#[tauri::command]
fn parse_chapters_text(text: String, fps: Option<f64>) -> Result<Vec<ImportedChapter>, String> {
    chapters::parse_chapters(&text, fps)
}

// ---------------------------------------------------------------------------
// list_templates / template_preview
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TemplateInfo {
    path: String,
    name: String,
    description: Option<String>,
    /// Template family folder (`Open Style`, `Movie`, ...).
    folder: String,
    thumbnail: Option<String>,
    titles_per_page: Option<u32>,
    /// Page styles the template itself contains (`MNUS_TITLE`, `MNUS_ROOT`, ...).
    page_styles: Vec<String>,
    /// Some page has video slots, so motion menus animate something.
    has_video_slots: bool,
}

/// Template folder: `CONVERTX_TEMPLATES`, else the `template_dir` setting, else the Windows default.
/// A folder that holds a `Templates` subfolder (the ConvertX install root) is accepted too.
fn template_root(settings: &AppSettings) -> Option<PathBuf> {
    let root = if let Some(dir) = std::env::var_os("CONVERTX_TEMPLATES") {
        PathBuf::from(dir)
    } else if !settings.template_dir.is_empty() {
        PathBuf::from(&settings.template_dir)
    } else if cfg!(windows) {
        PathBuf::from(DEFAULT_TEMPLATE_ROOT)
    } else {
        return None;
    };
    let nested = root.join("Templates");
    Some(if nested.is_dir() { nested } else { root })
}

#[tauri::command]
fn list_templates(state: State<'_, AppState>) -> Result<Vec<TemplateInfo>, String> {
    // No templates installed is normal (the GUI then offers only the menu editor), not an error.
    let Some(root) = template_root(&state.settings()).filter(|r| r.is_dir()) else { return Ok(Vec::new()) };
    let folders = std::fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut out = Vec::new();
    for folder in folders.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
        let folder_name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if folder_name.eq_ignore_ascii_case("Resources") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&folder) else { continue };
        for ini in files
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")))
        {
            let Ok(t) = Template::load(&ini) else { continue };
            let mut styles: Vec<String> = t.pages.iter().map(|p| p.style.clone()).collect();
            styles.dedup();
            out.push(TemplateInfo {
                name: t.name.clone().unwrap_or_else(|| ini.file_stem().unwrap_or_default().to_string_lossy().into_owned()),
                description: t.description.clone(),
                folder: folder_name.clone(),
                thumbnail: t.thumbnail.as_ref().and_then(|th| musadvd::menu::resolve_resource(&folder, th)).map(|p| p.to_string_lossy().into_owned()),
                titles_per_page: t.titles_per_page,
                has_video_slots: t.pages.iter().flat_map(|p| &p.graphics).any(|g| g.gfx_type.eq_ignore_ascii_case("GT_VIDEO")),
                page_styles: styles,
                path: ini.to_string_lossy().into_owned(),
            });
        }
    }
    out.sort_by(|a, b| (&a.folder, &a.name).cmp(&(&b.folder, &b.name)));
    Ok(out)
}

/// Template thumbnail (JPEG/PNG next to the .ini) as a data URL for an `<img>`.
#[tauri::command]
fn template_preview(state: State<'_, AppState>, path: String) -> Result<String, String> {
    preview_data_url(template_root(&state.settings()), &path)
}

fn preview_data_url(root: Option<PathBuf>, path: &str) -> Result<String, String> {
    if musadvd::menu::is_unc_or_device_path(path) {
        return Err("preview outside the template folder".into());
    }
    let p = Path::new(path);
    if path.trim().is_empty() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("preview outside the template folder".into());
    }
    let Some(root) = root else {
        return Err("preview outside the template folder".into());
    };
    let canon_root = std::fs::canonicalize(&root).map_err(|_| "preview outside the template folder".to_string())?;
    let canon_path = std::fs::canonicalize(p).map_err(|e| e.to_string())?;
    // Only serve images from the canonicalized template folders.
    if !canon_path.starts_with(&canon_root) {
        return Err("preview outside the template folder".into());
    }
    let bytes = std::fs::read(&canon_path).map_err(|e| e.to_string())?;
    let mime = match canon_path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        _ => "image/jpeg",
    };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// A frame of a video (menu editor: preview of video placeholders) as JPEG data URL.
#[tauri::command]
async fn title_frame(state: State<'_, AppState>, path: String, at_secs: f64) -> Result<String, String> {
    let tools = toolchain(&state).await?;
    let out = std::env::temp_dir().join(format!(
        "musadvd-frame-{}-{}.jpg",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    let args = musadvd::encoder::thumbnail_args(Path::new(&path), at_secs.max(0.0), &out);
    let result = tools.ffmpeg(&args, 0.0, |_| {}).await.map_err(|e| e.to_string());
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    result?;
    let bytes = bytes.map_err(|e| e.to_string())?;
    Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Audio peaks (0–255, `WAVEFORM_PEAKS_PER_SEC` per second) of `secs` of `path` from `start_secs`
/// (menu editor timeline). Piped from ffmpeg, nothing touches the disk.
#[tauri::command]
async fn audio_waveform(state: State<'_, AppState>, path: String, start_secs: f64, secs: f64) -> Result<Vec<u8>, String> {
    let tools = toolchain(&state).await?;
    let args = musadvd::encoder::waveform_args(Path::new(&path), start_secs.max(0.0), secs.clamp(1.0, 900.0));
    let pcm = tools.ffmpeg_output(&args).await.map_err(|e| e.to_string())?;
    Ok(musadvd::encoder::waveform_peaks(&pcm))
}

/// The whole picture of a video at `at_secs` as JPEG data URL, `width` pixels wide, in display
/// proportions (crop inspector).
#[tauri::command]
async fn source_frame(state: State<'_, AppState>, path: String, at_secs: f64, width: u32) -> Result<String, String> {
    let tools = toolchain(&state).await?;
    let out = std::env::temp_dir().join(format!(
        "musadvd-source-{}-{}.jpg",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    let args = musadvd::encoder::frame_args(Path::new(&path), at_secs.max(0.0), width.clamp(64, 1920), &out);
    let result = tools.ffmpeg(&args, 0.0, |_| {}).await.map_err(|e| e.to_string());
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    result?;
    let bytes = bytes.map_err(|e| e.to_string())?;
    Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Finds the black bars of a video (inside the section `start_secs`..`end_secs`, if given).
#[tauri::command]
async fn detect_crop(state: State<'_, AppState>, path: String, start_secs: Option<f64>, end_secs: Option<f64>) -> Result<probe::Crop, String> {
    let tools = toolchain(&state).await?;
    let mut info = probe::probe(&tools.ffprobe, Path::new(&path)).await.map_err(|e| e.to_string())?;
    probe::apply_trim(&mut info, start_secs, end_secs)?;
    musadvd::encoder::detect_crop(tools, &info).await.map_err(|e| e.to_string())
}

/// Detects scene changes inside `[start_secs, end_secs]` and returns chapter cut timestamps in seconds.
#[tauri::command]
async fn detect_scene_chapters(
    state: State<'_, AppState>,
    path: String,
    start_secs: Option<f64>,
    end_secs: Option<f64>,
) -> Result<Vec<f64>, String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("File not found: {}", p.display()));
    }
    let tools = toolchain(&state).await?;
    let info = probe::probe(&tools.ffprobe, &p).await.map_err(|e| e.to_string())?;
    if info.video.is_none() {
        return Err("The file has no video stream".to_string());
    }
    let ffmpeg = tools.ffmpeg.clone();
    let duration_secs = info.duration_secs;
    tokio::task::spawn_blocking(move || {
        probe::detect_scene_chapters(
            &ffmpeg,
            &p,
            start_secs,
            end_secs,
            duration_secs,
            probe::DEFAULT_SCENE_THRESHOLD,
            probe::MIN_SCENE_GAP_SECS,
        )
    })
    .await
    .map_err(|e| format!("Scene detection task panicked: {e}"))?
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QualityTestRequest {
    path: String,
    disc: DiscType,
    #[serde(default)]
    video_kbps: Option<u32>,
    #[serde(default)]
    audio_kbps: u32,
    #[serde(default)]
    start_secs: Option<f64>,
    #[serde(default)]
    trim_start: Option<f64>,
    #[serde(default)]
    trim_end: Option<f64>,
    #[serde(default)]
    crop: Option<probe::Crop>,
    #[serde(default)]
    denoise: Option<f32>,
    #[serde(default)]
    brightness: Option<f32>,
    #[serde(default)]
    contrast: Option<f32>,
    #[serde(default)]
    saturation: Option<f32>,
    #[serde(default)]
    bd_resolution: Option<String>,
    #[serde(default)]
    bd_rate: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct QualityTestResult {
    sample_path: String,
    psnr_db: f64,
    verdict: String,
}

fn quality_test_window(
    duration_secs: f64,
    trim_start: Option<f64>,
    trim_end: Option<f64>,
    start_secs: Option<f64>,
) -> (f64, f64, f64) {
    let total_dur = if duration_secs.is_finite() && duration_secs > 0.0 { duration_secs } else { 0.0 };
    let t0 = trim_start.filter(|x| x.is_finite() && *x >= 0.0).unwrap_or(0.0).min(total_dur);
    let t1 = trim_end.filter(|x| x.is_finite() && *x > t0).unwrap_or(total_dur).min(total_dur);
    let eff_dur = (t1 - t0).max(0.5);
    let sample_secs = eff_dur.min(10.0);
    let max_at = (t1 - sample_secs).max(t0);
    let at = match start_secs.filter(|s| s.is_finite() && *s >= 0.0) {
        Some(s) => s.clamp(t0, max_at),
        None => (t0 + eff_dur * 0.25).min(max_at),
    };
    (eff_dur, sample_secs, at)
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join(" | ")
}

fn dvd_quality_sample_spec(
    v: &probe::VideoStream,
    opts: &BuildOptions,
    vp: musadvd::encoder::VideoProcessing,
    video_kbps: u32,
    audio_kbps: u32,
    sample_path: &Path,
) -> (String, Vec<OsString>) {
    let res = dvd::effective_resolution(opts.resolution, opts.encoding_profile, video_kbps);
    let aspect = if !res.allows_wide() {
        Aspect::Standard
    } else {
        opts.aspect_policy.aspect_for(v.display_aspect)
    };
    let vf = encoder::video_filter(v, aspect, opts.tv_standard, res, vp);
    let maxrate = max_video_rate(audio_kbps).max(video_kbps);
    let (primaries, trc, matrix) = match opts.tv_standard {
        musadvd::author::TvStandard::Pal => ("bt470bg", "gamma28", "bt470bg"),
        musadvd::author::TvStandard::Ntsc => ("smpte170m", "smpte170m", "smpte170m"),
    };
    let gop = opts.tv_standard.gop().to_string();
    let mut args: Vec<OsString> = [
        "-c:v", "mpeg2video", "-threads", "0", "-pix_fmt", "yuv420p", "-r", opts.tv_standard.frame_rate(),
        "-g", &gop, "-bf", "2", "-dc", "9", "-qmin", "2", "-mbd", "rd", "-flags", "+cgop",
        "-sc_threshold", "1000000000",
    ]
    .map(OsString::from)
    .to_vec();
    if !opts.fast_encode {
        args.extend(["-trellis", "1", "-intra_vlc", "1"].map(OsString::from));
    }
    args.extend(["-color_primaries", primaries, "-color_trc", trc, "-colorspace", matrix].map(OsString::from));
    args.extend([
        "-b:v".into(),
        format!("{video_kbps}k").into(),
        "-maxrate".into(),
        format!("{maxrate}k").into(),
        "-bufsize".into(),
        format!("{VBV_BUFFER_KBIT}k").into(),
        "-aspect".into(),
        aspect.as_str().into(),
        "-an".into(),
        "-sn".into(),
        "-f".into(),
        "dvd".into(),
        sample_path.to_path_buf().into_os_string(),
    ]);
    (vf, args)
}

fn bd_quality_sample_spec(
    v: &probe::VideoStream,
    opts: &BuildOptions,
    request: &QualityTestRequest,
    vp: musadvd::encoder::VideoProcessing,
    video_kbps: u32,
    sample_path: &Path,
) -> Result<(String, Vec<OsString>), String> {
    let res = match request.bd_resolution.as_deref().filter(|s| !s.is_empty()) {
        None => opts.resolution,
        Some("1080") | Some("1080p") => Resolution::Hd1080,
        Some("720") | Some("720p") => Resolution::Hd720,
        Some(other) => return Err(format!("Unknown Blu-ray resolution: {other}")),
    };
    let rate = match request.bd_rate.as_deref().filter(|s| !s.is_empty()) {
        None => bd_frame_rate(&[v.fps], opts.tv_standard, res),
        Some(name) => BdRate::from_name(name, res)
            .ok_or_else(|| format!("{name} is not a Blu-ray frame rate at {}", res.label()))?,
    };
    let vf = encoder::bd_video_filter(v, rate, res, vp);
    let limits = request.disc.limits();
    let vbv_max = limits.vbv_maxrate_kbps.max(video_kbps);
    let vbv_buf = limits.vbv_bufsize_kbit;
    let preset = if opts.fast_encode { "veryfast" } else { "slow" };
    let mut x264 = String::from("slices=4:b-pyramid=strict:nal-hrd=vbr:aud=1:open-gop=0:weightp=1");
    if rate.fake_interlaced {
        x264.push_str(":fake-interlaced=1:pic-struct=1");
    }
    let mut args: Vec<OsString> = [
        "-c:v", "libx264", "-preset", preset, "-profile:v", "high", "-level:v", "4.1", "-bluray-compat", "1",
    ]
    .map(OsString::from)
    .to_vec();
    args.extend([
        "-b:v".into(),
        format!("{video_kbps}k").into(),
        "-maxrate".into(),
        format!("{vbv_max}k").into(),
        "-bufsize".into(),
        format!("{vbv_buf}k").into(),
        "-g".into(),
        rate.keyint().to_string().into(),
        "-bf".into(),
        "3".into(),
        "-x264-params".into(),
        x264.into(),
    ]);
    args.extend([
        "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
        "-an", "-sn", "-f", "matroska",
    ].map(OsString::from));
    args.push(sample_path.to_path_buf().into_os_string());
    Ok((vf, args))
}

async fn run_quality_test(
    ffmpeg: &Path,
    ffprobe: &Path,
    settings: &AppSettings,
    request: QualityTestRequest,
) -> Result<QualityTestResult, String> {
    let path = PathBuf::from(&request.path);
    if !path.is_file() {
        return Err(format!("File not found: {}", path.display()));
    }
    let mut info = probe::probe(ffprobe, &path).await.map_err(|e| e.to_string())?;
    probe::apply_crop(&mut info, request.crop.unwrap_or_default())?;
    let v = info.video.clone().ok_or_else(|| "The file has no video stream".to_string())?;

    let mut opts = BuildOptions { disc: request.disc, ..BuildOptions::default() };
    settings.apply_to(&mut opts);
    let vp = opts
        .video
        .with_picture(request.denoise, request.brightness, request.contrast, request.saturation);

    let (eff_dur, sample_secs, at) =
        quality_test_window(info.duration_secs, request.trim_start, request.trim_end, request.start_secs);
    let limits = request.disc.limits();
    let video_kbps = match request.video_kbps {
        Some(k) if k > 0 => k.clamp(limits.min_video_kbps, limits.max_video_kbps),
        _ => musadvd::calculator::calculate_capped(
            request.disc,
            eff_dur,
            request.audio_kbps,
            request.disc.default_mux_overhead_kbit(),
            opts.encoding_profile.max_video_kbps_for(request.disc),
        )
        .map_or(limits.min_video_kbps, |p| p.video_kbps),
    };

    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let ext = if request.disc.is_bluray() { "mkv" } else { "mpg" };
    let sample_path = std::env::temp_dir().join(format!("musadvd-qtest-{pid}-{nanos}.{ext}"));

    let (vf, codec_args) = if request.disc.is_bluray() {
        bd_quality_sample_spec(&v, &opts, &request, vp, video_kbps, &sample_path)?
    } else {
        dvd_quality_sample_spec(&v, &opts, vp, video_kbps, request.audio_kbps, &sample_path)
    };

    let mut encode_args: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-ss".into(),
        format!("{at:.3}").into(),
        "-t".into(),
        format!("{sample_secs:.3}").into(),
        "-i".into(),
        path.clone().into_os_string(),
        "-map".into(),
        format!("0:{}", v.index).into(),
        "-vf".into(),
        vf.clone().into(),
    ];
    encode_args.extend(codec_args);

    let ffmpeg_bin = ffmpeg.to_path_buf();
    let sample_for_job = sample_path.clone();
    let source_for_job = path;
    let v_index = v.index;

    tokio::task::spawn_blocking(move || -> Result<QualityTestResult, String> {
        let enc_out = blocking_tool_command(&ffmpeg_bin)
            .args(&encode_args)
            .output()
            .map_err(|e| format!("Failed to start ffmpeg: {e}"))?;
        if !enc_out.status.success() {
            let _ = std::fs::remove_file(&sample_for_job);
            let err = String::from_utf8_lossy(&enc_out.stderr);
            return Err(format!("Quality sample encode failed ({}): {}", enc_out.status, tail_lines(&err, 6)));
        }

        let psnr_args = quality_psnr_args(&sample_for_job, &source_for_job, v_index, at, sample_secs, &vf);
        let psnr_out = blocking_tool_command(&ffmpeg_bin)
            .args(&psnr_args)
            .output()
            .map_err(|e| {
                let _ = std::fs::remove_file(&sample_for_job);
                format!("Failed to start ffmpeg for PSNR: {e}")
            })?;
        let stderr = String::from_utf8_lossy(&psnr_out.stderr);
        if !psnr_out.status.success() {
            let _ = std::fs::remove_file(&sample_for_job);
            return Err(format!("Quality PSNR analysis failed ({}): {}", psnr_out.status, tail_lines(&stderr, 6)));
        }
        let psnr_db = parse_psnr_average(&stderr).ok_or_else(|| {
            let _ = std::fs::remove_file(&sample_for_job);
            "Failed to parse PSNR average from ffmpeg output".to_string()
        })?;
        let verdict = psnr_verdict(psnr_db).to_string();
        Ok(QualityTestResult {
            sample_path: sample_for_job.to_string_lossy().into_owned(),
            psnr_db: (psnr_db * 100.0).round() / 100.0,
            verdict,
        })
    })
    .await
    .map_err(|e| format!("Quality test task panicked: {e}"))?
}

/// Encodes up to 10 seconds of a title with the active settings and computes PSNR against the filtered source.
#[tauri::command]
async fn test_title_quality(state: State<'_, AppState>, request: QualityTestRequest) -> Result<QualityTestResult, String> {
    let tools = toolchain(&state).await?;
    let settings = state.settings();
    run_quality_test(&tools.ffmpeg, &tools.ffprobe, &settings, request).await
}

/// Files passed on the command line (videos dropped onto the .exe, "Open with").
#[tauri::command]
fn startup_files() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Cancels the running conversion: running ffmpeg/spumux/dvdauthor processes are killed and the
/// work directory is removed; `convert-cancelled` follows when that is done. Returns `false` if
/// no conversion is running.
#[tauri::command]
fn cancel_conversion(state: State<'_, AppState>) -> bool {
    match state.cancel.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
    state.ensure_hw_accel_detected().await;
    Ok(state.settings())
}

#[tauri::command]
async fn default_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
    let mut s = AppSettings::default();
    if let Ok(tools) = toolchain(&state).await {
        s.hw_accel_mode = tools.detect_hw_accel().await;
    }
    Ok(s)
}

/// Stores the settings (sanitized) and returns what was actually saved.
#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: AppSettings) -> Result<AppSettings, String> {
    let settings = settings.sanitized();
    settings.save(&state.settings_path).map_err(|e| e.to_string())?;
    *state.settings.lock().unwrap_or_else(|e| e.into_inner()) = settings.clone();
    musadvd::i18n::reload();
    Ok(settings)
}

#[derive(Serialize)]
struct LanguageOption {
    code: String,
    name: String,
}

#[tauri::command]
fn get_available_languages() -> Vec<LanguageOption> {
    musadvd::i18n::available_languages().into_iter().map(|(code, name)| LanguageOption { code, name }).collect()
}

/// A locale file fresh from disk (the GUI keeps its own copy).
#[tauri::command]
fn get_locale_data(lang: String) -> Result<std::collections::HashMap<String, String>, String> {
    musadvd::i18n::load(&lang)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInfo {
    path: String,
    /// Problem while loading the file at startup (corrupt file was backed up).
    load_problem: Option<String>,
    cpu_count: usize,
}

/// Absolute path of a bundled menu sound (`assets/menu-audio/<name>`), `None` if not shipped.
#[tauri::command]
fn bundled_menu_sound(name: String) -> Option<String> {
    // Plain file names only, so the frontend cannot reach outside the folder.
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return None;
    }
    musadvd::i18n::resource_dirs("menu-audio", "assets/menu-audio")
        .into_iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn settings_info(state: State<'_, AppState>) -> SettingsInfo {
    SettingsInfo {
        path: state.settings_path.to_string_lossy().into_owned(),
        load_problem: state.settings_note.lock().unwrap_or_else(|e| e.into_inner()).take(),
        cpu_count: dvd::available_cpus(),
    }
}

// ---------------------------------------------------------------------------
// Projects (.musadvd JSON) and system fonts
// ---------------------------------------------------------------------------

/// Writes a project file atomically (temp file + rename), so a failed save never leaves a
/// truncated project behind. The JSON shape and its version are owned by the frontend.
#[tauri::command]
fn save_project(path: String, project: serde_json::Value) -> Result<(), String> {
    let target = PathBuf::from(&path);
    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    let tmp = target.with_extension("musadvd.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("{path}: {e}"))?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{path}: {e}")
    })
}

#[tauri::command]
fn load_project(path: String) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
}

/// The subset of `paths` that is not an existing file.
#[tauri::command]
fn missing_paths(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().filter(|p| !Path::new(p).is_file()).collect()
}

/// Recovery autosave next to the settings file.
#[tauri::command]
fn autosave_path(state: State<'_, AppState>) -> String {
    let dir = state.settings_path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    dir.join("autosave.musadvd").to_string_lossy().into_owned()
}

/// Installed OS font families (sorted, deduplicated) with their font files.
#[tauri::command]
async fn list_system_fonts() -> Vec<musadvd::fonts::SystemFont> {
    tauri::async_runtime::spawn_blocking(|| musadvd::fonts::system_fonts().to_vec()).await.unwrap_or_default()
}

#[tauri::command]
fn default_output_dir() -> String {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("Videos").join("MusaDVD").to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// calculate_project
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectTitle {
    #[serde(flatten)]
    load: TitleLoad,
    /// Encoded/copied Blu-ray frame rate in fps (omitted = 23.976).
    #[serde(default)]
    fps: Option<f64>,
}

#[cfg(test)]
impl From<TitleLoad> for ProjectTitle {
    fn from(load: TitleLoad) -> Self {
        Self { load, fps: None }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSettings {
    disc: DiscType,
    titles: Vec<ProjectTitle>,
    motion_menu: bool,
    motion_duration: Option<f64>,
    /// Blu-ray menus from the editor: clip length of every menu in seconds (1 for stills).
    #[serde(default)]
    bd_menu_secs: Vec<f64>,
    /// Custom DVD/BD layout from the WYSIWYG editor.
    #[serde(default)]
    custom_layout: Option<musadvd::layout::CustomLayout>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TitleCalc {
    video_kbps: u32,
    /// Bar length: 0..100 between the minimum and maximum video bitrate of the disc (DVD
    /// 1000-8500, BD 4000-35,000).
    quality_percent: f64,
    /// Bar colour: `good`, `medium`, `poor` (DVD: > 2500 / 1200-2500 / < 1200 kbps, BD: > 15,000
    /// / 8000-15,000 / < 8000).
    quality: Quality,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalculationResult {
    target_mb: u32,
    video_kbps: u32,
    raw_video_kbps: f64,
    /// `none`, `min` (does not fit) or `max` (disc not full).
    clamp: String,
    estimated_mb: f64,
    fill_percent: f64,
    fits: bool,
    menu_mb: f64,
    total_secs: f64,
    titles: Vec<TitleCalc>,
}

fn quality(kbps: u32, disc: DiscType) -> f64 {
    let l = disc.limits();
    ((kbps as f64 - l.min_video_kbps as f64) / (l.max_video_kbps - l.min_video_kbps) as f64 * 100.0).clamp(0.0, 100.0)
}

/// Quality percent of H.264 video (Blu-ray / AVCHD) from its bits per pixel at `res` and `fps`,
/// so 720p is credited for its smaller frame and 50/59.94 fps clips are not overrated.
/// Points (bpp → %), linear in log(bpp) between them; the label bands are 40 fair / 65 good / 85 excellent.
fn h264_quality(kbps: u32, res: musadvd::author::Resolution, fps: f64) -> f64 {
    const POINTS: [(f64, f64); 5] = [(0.03, 0.0), (0.05, 40.0), (0.09, 65.0), (0.16, 85.0), (0.30, 100.0)];
    let (w, h) = res.frame_size(musadvd::author::TvStandard::Pal);
    let rate = if fps.is_finite() && fps > 0.0 { fps } else { 24000.0 / 1001.0 };
    let bpp = kbps as f64 * 1000.0 / (w as f64 * h as f64 * rate);
    if bpp <= POINTS[0].0 {
        return 0.0;
    }
    for pair in POINTS.windows(2) {
        let ((b0, p0), (b1, p1)) = (pair[0], pair[1]);
        if bpp <= b1 {
            return p0 + (p1 - p0) * (bpp / b0).ln() / (b1 / b0).ln();
        }
    }
    100.0
}

/// Quality band of a percent from [`h264_quality`] (same bands as the GUI's labels).
fn band(percent: f64) -> Quality {
    if percent >= 65.0 {
        Quality::Good
    } else if percent >= 40.0 {
        Quality::Medium
    } else {
        Quality::Poor
    }
}

/// Live estimate with the encoding profile from the global settings.
#[tauri::command]
fn calculate_project(state: State<'_, AppState>, settings: ProjectSettings) -> Result<CalculationResult, String> {
    let app = state.settings();
    let res = app.title_resolution(settings.disc);
    // Same hardware-encoder reserve as the build ([`dvd::hw_reserve_mb`]), from the same settings mapping.
    // Note: NVENC in practice lands ~3 % under its target bitrate (so AVCHD/BD discs often fill to ~95-96 %),
    // while `dvd::bitrate_plan_at` keeps the conservative 0.5 % reserve so we never over-promise disc space.
    let mut opts = BuildOptions { disc: settings.disc, ..BuildOptions::default() };
    app.apply_to(&mut opts);
    let reserve = dvd::hw_reserve_mb(settings.disc, opts.hw_accel);
    project_calculation_with(settings, app.profile(), res, reserve)
}

/// Size of a still Blu-ray menu clip (1 s static frame + AC-3 + IGS + M2TS overhead).
const BD_STILL_MENU_MB: f64 = 0.3;
/// Size of an encoded Blu-ray motion menu clip per second (8 Mbit/s H.264 + 192 kbit/s AC-3 + 8 % BDAV).
const BD_MENU_MB_PER_SEC: f64 = 1.1;

fn estimate_bd_menu_mb(menu_secs: &[f64]) -> f64 {
    menu_secs.iter().map(|&s| if s <= 1.0 { BD_STILL_MENU_MB } else { s * BD_MENU_MB_PER_SEC }).sum()
}

/// Full D1 bitrate with the same bits per pixel as `kbps` at the DVD title resolution, so the
/// quality rating credits Half D1 / SIF with spending the same bitrate on fewer pixels.
fn full_d1_equivalent_kbps(kbps: u32, disc: DiscType, res: musadvd::author::Resolution, profile: EncodingProfile) -> u32 {
    if disc.is_bluray() {
        return kbps;
    }
    let tv = musadvd::author::TvStandard::Pal; // the width/height ratios are the same for NTSC
    let (w, h) = dvd::effective_resolution(res, profile, kbps).frame_size(tv);
    let (fw, fh) = tv.size();
    (kbps as f64 * (fw * fh) as f64 / (w * h) as f64).round() as u32
}

/// [`project_calculation_with`] without a hardware-encoder reserve (the tests' fixed baseline).
#[cfg(test)]
fn project_calculation(settings: ProjectSettings, profile: EncodingProfile, res: musadvd::author::Resolution) -> Result<CalculationResult, String> {
    project_calculation_with(settings, profile, res, 0.0)
}

/// [`project_calculation`] keeping `reserve_mb` free (hardware-encoder reserve, see [`dvd::hw_reserve_mb`]).
fn project_calculation_with(settings: ProjectSettings, profile: EncodingProfile, res: musadvd::author::Resolution, reserve_mb: f64) -> Result<CalculationResult, String> {
    let target_mb = settings.disc.target_mb();
    // A Blu-ray has menus only from the editor (8 Mbit/s H.264 + AC-3 + muxing ≈ 1.1 MB/s).
    let menu_mb = if settings.disc.is_bluray() {
        estimate_bd_menu_mb(&settings.bd_menu_secs)
    } else if let Some(layout) = &settings.custom_layout {
        dvd::estimate_custom_layout_mb(layout, settings.motion_duration.unwrap_or(DEFAULT_MOTION_SECS))
    } else {
        dvd::estimate_menu_mb(settings.titles.len(), settings.motion_menu, settings.motion_duration.unwrap_or(DEFAULT_MOTION_SECS))
    };
    if settings.titles.is_empty() {
        return Ok(CalculationResult {
            target_mb,
            video_kbps: 0,
            raw_video_kbps: 0.0,
            clamp: "none".into(),
            estimated_mb: menu_mb,
            fill_percent: menu_mb / target_mb as f64 * 100.0,
            fits: true,
            menu_mb,
            total_secs: 0.0,
            titles: vec![],
        });
    }
    let loads: Vec<TitleLoad> = settings.titles.iter().map(|t| t.load).collect();
    let floor = dvd::min_video_kbps(settings.disc, res);
    let plan = dvd::bitrate_plan_at(settings.disc, &loads, menu_mb + reserve_mb, profile, floor).map_err(|e| e.to_string())?;
    let titles = settings
        .titles
        .iter()
        .map(|pt| {
            let t = &pt.load;
            let v = t.copy_video_kbps.unwrap_or_else(|| dvd::weighted_video_kbps(settings.disc, plan.video_kbps, t.weight, t.audio_kbps, floor));
            if settings.disc.is_bluray() {
                let p = h264_quality(v, res, pt.fps.unwrap_or(24000.0 / 1001.0));
                return TitleCalc { video_kbps: v, quality_percent: p, quality: band(p) };
            }
            // Copied video keeps its own resolution.
            let rated = if t.copy_video_kbps.is_some() { v } else { full_d1_equivalent_kbps(v, settings.disc, res, profile) };
            TitleCalc { video_kbps: v, quality_percent: quality(rated, settings.disc), quality: Quality::for_disc(rated, settings.disc) }
        })
        .collect();
    Ok(CalculationResult {
        target_mb,
        video_kbps: plan.video_kbps,
        raw_video_kbps: plan.raw_video_kbps,
        clamp: match plan.clamp {
            Clamp::None => "none",
            Clamp::Min => "min",
            Clamp::Max => "max",
        }
        .into(),
        estimated_mb: plan.estimated_mb,
        fill_percent: plan.estimated_mb / target_mb as f64 * 100.0,
        fits: plan.clamp != Clamp::Min && plan.fits(),
        menu_mb,
        total_secs: loads.iter().filter(|t| t.copy_video_kbps.is_none()).map(|t| t.duration_secs).sum(),
        titles,
    })
}

// ---------------------------------------------------------------------------
// start_conversion
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputConfig {
    path: String,
    /// ffprobe stream indices of the checked audio tracks.
    audio: Vec<u32>,
    /// ffprobe stream indices of the enabled subtitles.
    subtitles: Vec<u32>,
    label: Option<String>,
    chapters: Vec<f64>,
    /// Used section of the file in seconds of the source.
    #[serde(default)]
    start_secs: Option<f64>,
    #[serde(default)]
    end_secs: Option<f64>,
    #[serde(default)]
    crop: Option<probe::Crop>,
    #[serde(default)]
    merge_with_previous: bool,
    /// Own audio track names by ffprobe stream index.
    #[serde(default)]
    audio_names: std::collections::HashMap<u32, String>,
    /// Blu-ray overrides of this title (see `StreamSelection`).
    #[serde(default)]
    bd_resolution: Option<String>,
    #[serde(default)]
    bd_rate: Option<String>,
    #[serde(default)]
    bd_video: Option<String>,
    /// DVD-Video override of this title (see `StreamSelection`).
    #[serde(default)]
    dvd_video: Option<String>,
    #[serde(default)]
    bd_quality: Option<String>,
    /// End action of this title (see `StreamSelection`).
    #[serde(default)]
    after_playback: musadvd::author::AfterPlayback,
    /// Per-title picture adjustments (override project defaults when `Some`).
    #[serde(default)]
    denoise: Option<f32>,
    #[serde(default)]
    brightness: Option<f32>,
    #[serde(default)]
    contrast: Option<f32>,
    #[serde(default)]
    saturation: Option<f32>,
    /// Styling and timing offset of the title's text subtitles.
    #[serde(default)]
    subtitle_style: musadvd::textsub::SubtitleStyle,
    /// Standalone external subtitle files attached to this title.
    #[serde(default)]
    external_subtitles: Vec<ExternalSubtitle>,
    /// Disc order of all subtitles, embedded and external mixed ([`StreamSelection::subtitle_order`]).
    #[serde(default)]
    subtitle_order: Option<Vec<u32>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversionConfig {
    inputs: Vec<InputConfig>,
    disc: DiscType,
    template: String,
    output: String,
    two_pass: bool,
    motion_menu: bool,
    motion_duration: Option<f64>,
    menu_title: Option<String>,
    /// Menus from the menu editor instead of `template`.
    #[serde(default)]
    custom_layout: Option<musadvd::layout::CustomLayout>,
    /// Blu-ray/AVCHD video: `encode` (default), `auto` or `strict`.
    #[serde(default)]
    bd_video: String,
    /// Blu-ray/AVCHD audio: `auto` (default), `encode` or `strict`.
    #[serde(default)]
    bd_audio: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversionDone {
    ok: bool,
    message: String,
    video_ts: Option<String>,
    /// Name of the DVD folder (default volume label when burning).
    project_name: Option<String>,
    size_mb: Option<f64>,
    target_mb: u32,
    warnings: Vec<String>,
}

/// Starts the build in the background and returns immediately. Progress is emitted as
/// `convert-event` ([`Event`], JSON `{type, data}`), the result as `convert-done`.
#[tauri::command]
fn start_conversion(app: AppHandle, state: State<'_, AppState>, config: ConversionConfig) -> Result<(), String> {
    if config.inputs.is_empty() {
        return Err("Keine Videos im Projekt.".into());
    }
    if config.output.trim().is_empty() {
        return Err("No output folder selected.".into());
    }
    if state.converting.swap(true, Ordering::SeqCst) {
        return Err("A conversion is already running.".into());
    }
    let mut opts = BuildOptions {
        inputs: config.inputs.iter().map(|i| PathBuf::from(&i.path)).collect(),
        disc: config.disc,
        template: PathBuf::from(&config.template),
        output: PathBuf::from(&config.output),
        menu_title: config.menu_title.filter(|t| !t.trim().is_empty()),
        custom_layout: config.custom_layout,
        ui_lang: UiLang::default(),
        two_pass: config.two_pass,
        motion_menu: config.motion_menu,
        motion_duration: config.motion_duration,
        selections: config
            .inputs
            .iter()
            .map(|i| StreamSelection {
                audio: Some(i.audio.clone()),
                subtitles: Some(i.subtitles.clone()),
                label: i.label.clone(),
                chapters: Some(i.chapters.clone()),
                start_secs: i.start_secs,
                end_secs: i.end_secs,
                crop: i.crop,
                merge_with_previous: i.merge_with_previous,
                audio_names: i.audio_names.clone(),
                bd_resolution: i.bd_resolution.clone(),
                bd_rate: i.bd_rate.clone(),
                bd_video: i.bd_video.clone(),
                dvd_video: i.dvd_video.clone(),
                bd_quality: i.bd_quality.clone(),
                after_playback: i.after_playback,
                denoise: i.denoise,
                brightness: i.brightness,
                contrast: i.contrast,
                saturation: i.saturation,
                subtitle_style: i.subtitle_style.clone(),
                external_subtitles: i.external_subtitles.clone(),
                subtitle_order: i.subtitle_order.clone(),
            })
            .collect(),
        ..BuildOptions::default()
    };
    // Global settings: worker pool size, priority, menu/playback options, languages, ...
    state.settings().apply_to(&mut opts);
    opts.bd_video = musadvd::encoder::BdVideoMode::from_setting(&config.bd_video);
    opts.bd_audio = musadvd::encoder::BdAudioMode::from_setting(&config.bd_audio);
    let token = CancelToken::new();
    opts.cancel = token.clone();
    *state.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(token);
    let cancel_slot = state.cancel.clone();
    let target_mb = opts.disc.target_mb();
    let busy = state.converting.clone();

    // Own thread + runtime: the build future does not have to be `Send`, and long ffmpeg runs
    // never block Tauri's async runtime.
    std::thread::spawn(move || {
        let done = match tokio::runtime::Runtime::new() {
            Err(e) => ConversionDone {
                ok: false,
                message: format!("Runtime could not start: {e}"),
                video_ts: None,
                project_name: None,
                size_mb: None,
                target_mb,
                warnings: vec![],
            },
            Ok(rt) => {
                let events = app.clone();
                let mut on_event = move |e: Event| {
                    let _ = events.emit("convert-event", e);
                };
                match rt.block_on(dvd::build_dvd(&opts, &mut on_event)) {
                    Err(BuildError::Cancelled) => {
                        // All tools are stopped and the work directory is gone at this point.
                        cancel_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                        busy.store(false, Ordering::SeqCst);
                        let _ = app.emit("convert-cancelled", "Conversion cancelled");
                        return;
                    }
                    Ok(r) => ConversionDone {
                        ok: true,
                        message: musadvd::tf!(
                            opts.ui_lang,
                            "done_summary",
                            kind = match opts.disc {
                                d if d.is_avchd() => "AVCHD",
                                d if d.is_bluray() => "Blu-ray",
                                _ => "DVD",
                            },
                            mb = format!("{:.0}", r.size_mb),
                            target = r.target_mb
                        ),
                        video_ts: Some(r.video_ts.to_string_lossy().into_owned()),
                        project_name: r.project_dir.file_name().map(|n| n.to_string_lossy().into_owned()),
                        size_mb: Some(r.size_mb),
                        target_mb,
                        warnings: r.warnings,
                    },
                    Err(e) => ConversionDone {
                        ok: false,
                        message: e.message(opts.ui_lang),
                        video_ts: None,
                        project_name: None,
                        size_mb: None,
                        target_mb,
                        warnings: vec![],
                    },
                }
            }
        };
        cancel_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        busy.store(false, Ordering::SeqCst);
        let _ = app.emit("convert-done", done);
    });
    Ok(())
}

/// CLI dual mode (AppImage symlink named `musadvd`, `--cli`, or a CLI subcommand/flag as first
/// argument): returns the argv to hand to the CLI so GTK/WebKit are never initialised.
/// Windows ships `musadvd.exe` separately and the GUI exe has no console, so this is Unix-only.
#[cfg(unix)]
fn cli_invocation() -> Option<Vec<OsString>> {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // Inside an AppImage argv[0] is the mounted binary; the runtime exposes the invoked name as $ARGV0.
    let invoked = std::env::var_os("ARGV0").or_else(|| args.first().cloned()).map(PathBuf::from);
    let as_cli = invoked.as_deref().and_then(Path::file_name).is_some_and(|n| n == "musadvd");
    let first = args.get(1).and_then(|a| a.to_str()).unwrap_or("").to_owned();
    if first == "--cli" {
        args.remove(1);
        return Some(args);
    }
    let cli_flag = matches!(first.as_str(), "--help" | "-h" | "--version" | "-V");
    (as_cli || cli_flag || musadvd::cli::is_subcommand(&first)).then_some(args)
}

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    if let Some(args) = cli_invocation() {
        return musadvd::cli::run(args);
    }
    self_update::cleanup_leftovers();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new())
        .manage(burn::BurnState::default())
        .manage(burn_studio::StudioState::default());
    let builder = match preview::PreviewServer::start() {
        Ok(server) => builder.manage(server),
        Err(e) => {
            eprintln!("warning: preview server disabled: {e}");
            builder
        }
    };
    if let Err(err) = builder
        .setup(|app| {
            // `--burn-studio`: start with the Burn Studio window only (the main window stays hidden).
            if std::env::args().any(|a| a == "--burn-studio") {
                tauri::WebviewWindowBuilder::new(app, "burn-studio", tauri::WebviewUrl::App("burnStudio.html".into()))
                    .title("Burn Studio – MusaDVD")
                    .inner_size(1180.0, 800.0)
                    .min_inner_size(900.0, 620.0)
                    .build()?;
                if let Some(main) = app.get_webview_window("main") {
                    main.hide()?;
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // The menu editor is a second window: it must not outlive the main one.
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                if let Some(p) = window.app_handle().try_state::<Arc<preview::PreviewServer>>() {
                    p.stop();
                }
                if let Some(ed) = window.app_handle().get_webview_window("menu-editor") {
                    let _ = ed.destroy();
                }
                if let Some(studio) = window.app_handle().get_webview_window("burn-studio") {
                    let _ = studio.destroy();
                }
            // Started with `--burn-studio` the main window is hidden: closing the studio ends the app.
            if window.label() == "burn-studio" && matches!(event, tauri::WindowEvent::Destroyed) {
                let main_hidden = window.app_handle().get_webview_window("main").is_some_and(|m| !m.is_visible().unwrap_or(true));
                if main_hidden {
                    window.app_handle().exit(0);
                }
            }
            }
        })
        .invoke_handler(tauri::generate_handler![
            save_project,
            load_project,
            missing_paths,
            autosave_path,
            list_system_fonts,
            get_available_languages,
            get_locale_data,
            probe_file,
            scan_bd_copy,
            probe_subtitle_file,
            import_chapters_file,
            parse_chapters_text,
            list_templates,
            template_preview,
            calculate_project,
            start_conversion,
            default_output_dir,
            startup_files,
            get_settings,
            save_settings,
            default_settings,
            settings_info,
            bundled_menu_sound,
            cancel_conversion,
            burn::list_drives,
            burn::disc_needs,
            burn::probe_media,
            burn::start_burn,
            burn::cancel_burn,
            burn::startup_burn,
            burn::open_folder,
            self_update::update_install_mode,
            self_update::self_update,
            title_frame,
            audio_waveform,
            preview::preview_stream,
            preview::preview_stop,
            source_frame,
            detect_crop,
            detect_scene_chapters,
            test_title_quality,
            menu_editor::check_layout,
            menu_editor::text_lines,
            menu_editor::load_layout,
            menu_editor::import_convertx_template,
            menu_editor::save_layout,
            menu_editor::image_preview,
            menu_editor::save_clipboard_image,
            menu_editor::menu_navigation,
            burn_studio::studio_cancel,
            burn_studio::studio_drives,
            burn_studio::studio_drive_info,
            burn_studio::studio_eject,
            burn_studio::studio_load,
            burn_studio::studio_lock,
            burn_studio::studio_build_plan,
            burn_studio::studio_inspect,
            burn_studio::studio_image_scrambled,
            burn_studio::studio_write_image,
            burn_studio::studio_write_files,
            burn_studio::studio_discovery,
            burn_studio::studio_erase,
            burn_studio::studio_read,
            burn_studio::studio_build,
            burn_studio::studio_verify,
            burn_studio::studio_label_read,
            burn_studio::studio_label_patch,
            burn_studio::studio_ifo_candidates,
            burn_studio::studio_sidecar_write,
            burn_studio::studio_graph_load,
            burn_studio::studio_graph_folder,
            burn_studio::studio_template_preview
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("{err}");
        std::process::exit(1);
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use musadvd::calculator::MAX_VIDEO_KBPS;

    #[test]
    fn dvd_copy_verdict_per_standard() {
        let mut info = probe::MediaInfo {
            path: PathBuf::from("nonexistent_dvd_copy_test.mpg"),
            duration_secs: 10.0,
            video: Some(probe::VideoStream {
                index: 0,
                codec: "h264".into(),
                width: 720,
                height: 576,
                display_aspect: 16.0 / 9.0,
                fps: 25.0,
                field_order: "progressive".into(),
                crop: None,
                hdr: false,
                profile: None,
                level: None,
                pix_fmt: "yuv420p".into(),
                bit_rate: Some(6_000_000),
            }),
            audio: vec![],
            subtitles: vec![],
            chapters: vec![],
            trim: None,
            audio_names: Default::default(),
        };
        let (kbps, issue) = dvd_copy_verdicts(&info);
        assert!(kbps.is_empty());
        assert!(issue["PAL"].contains("needs MPEG-2") && issue["NTSC"].contains("needs MPEG-2"));

        info.video.as_mut().unwrap().codec = "mpeg2video".into();
        let (kbps, issue) = dvd_copy_verdicts(&info);
        assert_eq!(kbps.get("PAL"), Some(&6000));
        assert!(!issue.contains_key("PAL") && issue.contains_key("NTSC") && !kbps.contains_key("NTSC"));
    }

    #[test]
    fn calculation_for_two_titles() {
        let r = project_calculation(ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![
                TitleLoad { duration_secs: 5400.0, audio_kbps: 448 + 192, subtitle_streams: 1, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into(),
                TitleLoad { duration_secs: 1800.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into(),
            ],
            motion_menu: false,
            motion_duration: None,
            bd_menu_secs: vec![],
            custom_layout: None,
        }, EncodingProfile::Auto, musadvd::author::Resolution::FullD1)
        .unwrap();
        assert_eq!(r.target_mb, 4300);
        assert_eq!(r.clamp, "none");
        assert!(r.fits);
        assert!(r.fill_percent > 90.0 && r.fill_percent <= 100.0, "{}", r.fill_percent);
        assert_eq!(r.titles.len(), 2);
        assert!(r.titles.iter().all(|t| t.quality_percent > 0.0 && t.quality_percent < 100.0));
    }

    #[test]
    fn quality_rating_credits_lower_dvd_resolutions() {
        use musadvd::author::Resolution;
        // 4 h on a DVD-5: a low bitrate that rates far below "good" at Full D1.
        let project = || ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![TitleLoad { duration_secs: 4.0 * 3600.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into()],
            motion_menu: false,
            motion_duration: None,
            bd_menu_secs: vec![],
            custom_layout: None,
        };
        let calc = |res| project_calculation(project(), EncodingProfile::Auto, res).unwrap();
        let (full, half, sif) = (calc(Resolution::FullD1), calc(Resolution::HalfD1), calc(Resolution::Sif));
        // Same bitrate and size: the resolution only changes how the bits are spent.
        assert_eq!((full.video_kbps, full.estimated_mb), (half.video_kbps, half.estimated_mb));
        let q = |r: &CalculationResult| r.titles[0].quality_percent;
        assert!(q(&full) < q(&half) && q(&half) <= q(&sif), "{} {} {}", q(&full), q(&half), q(&sif));
        assert_eq!(full_d1_equivalent_kbps(2000, DiscType::Dvd5, Resolution::HalfD1, EncodingProfile::Auto), 4091);
        assert_eq!(full_d1_equivalent_kbps(2000, DiscType::Dvd5, Resolution::Sif, EncodingProfile::Auto), 8182);
        assert_eq!(full_d1_equivalent_kbps(2000, DiscType::Bd25, Resolution::HalfD1, EncodingProfile::Auto), 2000);
    }

    #[test]
    fn bluray_quality_rates_bits_per_pixel() {
        use musadvd::author::Resolution;
        // 2.1 Mbit/s at 720p23.976 (~0.095 bpp) is good H.264, not "0 % · Low".
        let p720 = h264_quality(2100, Resolution::Hd720, 23.976);
        assert!((65.0..85.0).contains(&p720), "{p720}");
        assert_eq!(band(p720), Quality::Good);
        // At 50 or 59.94 fps the same bitrate has less than half the bits per pixel.
        let p720_50 = h264_quality(2100, Resolution::Hd720, 50.0);
        let p720_60 = h264_quality(2100, Resolution::Hd720, 60000.0 / 1001.0);
        assert!(p720_50 < 40.0 && p720_60 < p720_50, "{p720_50} {p720_60}");
        assert_eq!(band(p720_50), Quality::Poor);
        // The same bitrate at 1080p has fewer bits per pixel.
        assert!(h264_quality(2100, Resolution::Hd1080, 23.976) < 40.0);
        // 15 Mbit/s 1080p (the old "good" mark) is excellent; the floors stay above zero.
        assert!(h264_quality(15_000, Resolution::Hd1080, 23.976) >= 85.0);
        assert!(h264_quality(1500, Resolution::Hd720, 23.976) > 0.0 && h264_quality(2500, Resolution::Hd1080, 23.976) > 0.0);
        assert_eq!(h264_quality(100, Resolution::Hd1080, 23.976), 0.0);
        assert_eq!(h264_quality(80_000, Resolution::Hd720, 23.976), 100.0);
    }

    #[test]
    fn calculation_too_long_for_dvd5() {
        let r = project_calculation(ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![TitleLoad { duration_secs: 8.0 * 3600.0, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into()],
            motion_menu: true,
            motion_duration: Some(25.0),
            bd_menu_secs: vec![],
            custom_layout: None,
        }, EncodingProfile::Auto, musadvd::author::Resolution::FullD1)
        .unwrap();
        assert_eq!(r.clamp, "min");
        assert!(!r.fits);
        assert!(r.fill_percent > 100.0);
        assert_eq!(r.titles[0].quality_percent, 0.0);
        assert_eq!(r.titles[0].quality, Quality::Poor);
    }

    #[test]
    fn calculation_follows_the_profile() {
        let project = || ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![TitleLoad { duration_secs: 1800.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into()],
            motion_menu: false,
            motion_duration: None,
            bd_menu_secs: vec![],
            custom_layout: None,
        };
        let auto = project_calculation(project(), EncodingProfile::Auto, musadvd::author::Resolution::FullD1).unwrap();
        let lp = project_calculation(project(), EncodingProfile::Lp, musadvd::author::Resolution::FullD1).unwrap();
        assert_eq!((auto.video_kbps, lp.video_kbps), (MAX_VIDEO_KBPS, 4000));
        assert_eq!(lp.clamp, "max");
        assert!(lp.estimated_mb < auto.estimated_mb);
    }

    #[test]
    fn blu_ray_calculation_uses_bd_limits() {
        let project = |disc| ProjectSettings {
            disc,
            titles: vec![TitleLoad { duration_secs: 2.0 * 3600.0, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into()],
            motion_menu: true,
            motion_duration: Some(25.0),
            bd_menu_secs: vec![],
            custom_layout: None,
        };
        use musadvd::author::Resolution::Hd1080;
        let r = project_calculation(project(DiscType::Bd25), EncodingProfile::Auto, Hd1080).unwrap();
        assert_eq!(r.target_mb, 22_900);
        assert_eq!((r.menu_mb, r.clamp.as_str()), (0.0, "none"), "no menus without an editor layout");
        // Menus from the editor: a still (1 s) and a 20 s loop.
        let with_menus = ProjectSettings { bd_menu_secs: vec![1.0, 20.0], ..project(DiscType::Bd25) };
        let m = project_calculation(with_menus, EncodingProfile::Auto, Hd1080).unwrap();
        assert!((m.menu_mb - (BD_STILL_MENU_MB + 20.0 * BD_MENU_MB_PER_SEC)).abs() < 1e-9);
        assert!(r.fits && (20_000..35_000).contains(&r.video_kbps), "{}", r.video_kbps);
        assert_eq!(r.titles[0].quality, Quality::Good);
        assert!(r.titles[0].quality_percent >= 85.0, "{}", r.titles[0].quality_percent);
        // The profile ceilings are the BD ones.
        let lp = project_calculation(project(DiscType::Bd50), EncodingProfile::Lp, Hd1080).unwrap();
        assert_eq!((lp.video_kbps, lp.clamp.as_str()), (12_000, "max"));
        // 24 h at 1080p does not fit a BD-25 even at the 2500 kbps floor.
        let long = ProjectSettings { titles: vec![TitleLoad { duration_secs: 24.0 * 3600.0, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into()], ..project(DiscType::Bd25) };
        let r = project_calculation(long, EncodingProfile::Auto, Hd1080).unwrap();
        assert_eq!((r.clamp.as_str(), r.fits, r.video_kbps), ("min", false, 2500));
        assert!(r.titles[0].quality_percent <= 45.0, "{}", r.titles[0].quality_percent);
        // A 50 fps 720p title rates lower than a 23.976 fps title at the same bitrate.
        let base_load = TitleLoad { duration_secs: 6.0 * 3600.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let q24 = project_calculation(ProjectSettings {
            disc: DiscType::Bd25,
            titles: vec![ProjectTitle { load: base_load, fps: Some(24000.0 / 1001.0) }],
            motion_menu: false, motion_duration: None, bd_menu_secs: vec![], custom_layout: None,
        }, EncodingProfile::Auto, musadvd::author::Resolution::Hd720).unwrap().titles[0].quality_percent;
        let q50 = project_calculation(ProjectSettings {
            disc: DiscType::Bd25,
            titles: vec![ProjectTitle { load: base_load, fps: Some(50.0) }],
            motion_menu: false, motion_duration: None, bd_menu_secs: vec![], custom_layout: None,
        }, EncodingProfile::Auto, musadvd::author::Resolution::Hd720).unwrap().titles[0].quality_percent;
        assert!(q50 < q24, "50 fps ({q50}) should rate lower than 23.976 fps ({q24})");
    }

    #[test]
    fn avchd_estimate_uses_avchd_limits() {
        let load = TitleLoad { duration_secs: 3600.0, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let r = project_calculation(
            ProjectSettings { disc: DiscType::Avchd5, titles: vec![load.into()], motion_menu: false, motion_duration: None, bd_menu_secs: vec![], custom_layout: None },
            EncodingProfile::Auto, musadvd::author::Resolution::FullD1,
        )
        .unwrap();
        assert_eq!(r.target_mb, 4300);
        assert!(r.fits && r.video_kbps > 5000 && r.video_kbps <= 15_000, "{}", r.video_kbps);
        // Ten hours do not fit a DVD-5 as AVCHD even at the 2000 kbps floor.
        let long = TitleLoad { duration_secs: 10.0 * 3600.0, ..load };
        let r = project_calculation(
            ProjectSettings { disc: DiscType::Avchd5, titles: vec![long.into()], motion_menu: false, motion_duration: None, bd_menu_secs: vec![], custom_layout: None },
            EncodingProfile::Auto, musadvd::author::Resolution::FullD1,
        )
        .unwrap();
        assert_eq!((r.clamp.as_str(), r.fits), ("min", false));
    }

    #[test]
    fn audio_plans_follow_disc_and_mode() {
        let ac3 = probe::AudioStream {
            index: 1,
            codec: "ac3".into(),
            channels: 6,
            channel_layout: "5.1".into(),
            language: None,
            title: None,
            default: false,
            bit_rate: Some(640_000),
            sample_rate: Some(48_000),
            profile: None,
            bits_per_raw_sample: None,
        };
        let p = audio_plans(&ac3, "Spanisch - Latino", TrackLabelMode::SmartMetadata, 0);
        // The label carries the plan's own layout: the DVD plan is 5.1 AC-3 too, LPCM on AVCHD is stereo.
        assert_eq!(p.dvd.auto.label, "Spanisch - Latino (5.1)");
        assert_eq!(p.avchd.lpcm.label, "Spanisch - Latino (Stereo)");
        // 640 kbps AC-3 is copied on Blu-ray/AVCHD but re-encoded to 448 kbps for the DVD.
        assert_eq!((p.dvd.auto.copy, p.dvd.auto.kbps), (false, 448));
        assert_eq!((p.bluray.auto.copy, p.bluray.auto.kbps, p.avchd.auto.copy), (true, 640, true));
        assert_eq!((p.bluray.ac3.copy, p.bluray.ac3.kbps), (false, 448));
        // LPCM: 6 channels x 24 bit on Blu-ray, stereo 16 bit on AVCHD, AC-3 on DVD.
        assert_eq!((p.bluray.lpcm.kbps, p.bluray.lpcm.codec.as_str()), (6912, "lpcm"));
        assert_eq!((p.avchd.lpcm.kbps, p.avchd.lpcm.layout.as_str()), (1536, "Stereo"));
        assert_eq!(p.dvd.lpcm.codec, "ac3");
    }

    #[test]
    fn empty_project() {
        let r = project_calculation(ProjectSettings { disc: DiscType::Dvd9, titles: vec![], motion_menu: false, motion_duration: None, bd_menu_secs: vec![], custom_layout: None }, EncodingProfile::Auto, musadvd::author::Resolution::FullD1).unwrap();
        assert_eq!(r.target_mb, 7950);
        assert!(r.fits);
    }

    #[test]
    fn dvd_calculation_uses_custom_layout_estimate() {
        let still_menu = musadvd::layout::CustomMenu { play_mode: None, timeout_secs: None,
            id: "m1".into(),
            name: "Main".into(),
            role: musadvd::layout::MenuRole::Title,
            title: None,
            background: musadvd::layout::Background::default(),
            duration_secs: None,
            audio: musadvd::layout::MenuSound::None,
            intro_secs: None,
            group: None,
            elements: vec![],
        };
        let l = musadvd::layout::CustomLayout { title_screen: None, chapter_menus: None, aspect: None,
            version: 1,
            canvas: musadvd::layout::Canvas::default(),
            highlight: musadvd::layout::Highlight::default(),
            animations: true,
            theme: None,
            settings_mode: None,
            menus: vec![still_menu],
            fast_play: None,
        };
        let s = ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![TitleLoad { duration_secs: 3600.0, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 }.into()],
            motion_menu: true,
            motion_duration: Some(25.0),
            bd_menu_secs: vec![],
            custom_layout: Some(l),
        };
        let r = project_calculation(s, EncodingProfile::Auto, musadvd::author::Resolution::FullD1).unwrap();
        // Custom layout has 1 still menu (0.15 MB), not the template estimate (7*0.15 + 2*5.5 = 12.05 MB)
        assert_eq!(r.menu_mb, 0.15);
    }

    #[test]
    fn preview_is_confined_to_templates() {
        assert!(preview_data_url(Some(PathBuf::from(DEFAULT_TEMPLATE_ROOT)), r"C:\Windows\win.ini").is_err());

        let base = std::env::temp_dir().join(format!("cx_gui_preview_sec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("Templates");
        let style = root.join("Style");
        let resources = root.join("Resources");
        let outside = base.join("outside");
        std::fs::create_dir_all(&style).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let thumb = style.join("thumb.png");
        let res_bg = resources.join("bg.jpg");
        let secret = base.join("secret.jpg");
        let outside_img = outside.join("other.png");
        for p in [&thumb, &res_bg, &secret, &outside_img] {
            std::fs::write(p, b"img").unwrap();
        }

        let valid = preview_data_url(Some(root.clone()), &thumb.to_string_lossy()).unwrap();
        assert!(valid.starts_with("data:image/png;base64,"));

        // Resolved ..\Resources thumbnail from list_templates must remain previewable without `\\?\` or `..`.
        let resolved = musadvd::menu::resolve_resource(&style, r"..\Resources\bg.jpg").unwrap();
        let res_url = preview_data_url(Some(root.clone()), &resolved.to_string_lossy()).unwrap();
        assert!(res_url.starts_with("data:image/jpeg;base64,"));

        assert!(preview_data_url(Some(root.clone()), &outside_img.to_string_lossy()).is_err());
        assert!(preview_data_url(Some(root.clone()), r"\\server\share\thumb.png").is_err());
        assert!(preview_data_url(Some(root.clone()), "//server/share/thumb.png").is_err());
        assert!(preview_data_url(Some(root.clone()), &format!("{}/../secret.jpg", root.display())).is_err());
        assert!(preview_data_url(Some(root), &format!("{}\\..\\secret.jpg", base.join("Templates").display())).is_err());
        assert!(preview_data_url(None, &thumb.to_string_lossy()).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn quality_test_window_and_serde() {
        assert_eq!(quality_test_window(120.0, None, None, None), (120.0, 10.0, 30.0));
        assert_eq!(quality_test_window(120.0, Some(10.0), Some(50.0), None), (40.0, 10.0, 20.0));
        assert_eq!(quality_test_window(120.0, Some(10.0), Some(50.0), Some(45.0)), (40.0, 10.0, 40.0));
        assert_eq!(quality_test_window(4.0, None, None, Some(2.0)), (4.0, 4.0, 0.0));
        assert_eq!(quality_test_window(20.0, Some(f64::NAN), Some(-1.0), Some(f64::INFINITY)), (20.0, 10.0, 5.0));

        let req: QualityTestRequest = serde_json::from_str(
            r#"{"path":"video.mkv","disc":"dvd5","audioKbps":192,"denoise":2.5,"bdResolution":"720"}"#,
        )
        .unwrap();
        assert_eq!(req.path, "video.mkv");
        assert_eq!(req.disc, DiscType::Dvd5);
        assert_eq!(req.audio_kbps, 192);
        assert_eq!(req.denoise, Some(2.5));
        assert_eq!(req.bd_resolution.as_deref(), Some("720"));
        assert_eq!((req.brightness, req.contrast, req.saturation), (None, None, None));

        let res = QualityTestResult {
            sample_path: "sample.mpg".into(),
            psnr_db: 38.42,
            verdict: "good".into(),
        };
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains(r#""sample_path":"sample.mpg""#), "{json}");
        assert!(json.contains(r#""psnr_db":38.42"#), "{json}");
        assert!(json.contains(r#""verdict":"good""#), "{json}");

        let input: InputConfig = serde_json::from_str(
            r#"{"path":"a.mkv","audio":[1],"subtitles":[],"label":null,"chapters":[],"contrast":1.15}"#,
        )
        .unwrap();
        assert_eq!((input.denoise, input.brightness, input.contrast, input.saturation), (None, None, Some(1.15), None));
        assert!(input.external_subtitles.is_empty());

        let with_ext: InputConfig = serde_json::from_str(
            r#"{"path":"a.mkv","audio":[1],"subtitles":[],"label":null,"chapters":[],"externalSubtitles":[{"path":"sub.en.srt","language":"eng","title":null,"forced":true}]}"#,
        )
        .unwrap();
        assert_eq!(with_ext.external_subtitles.len(), 1);
        assert_eq!(with_ext.external_subtitles[0].language.as_deref(), Some("eng"));
        assert!(with_ext.external_subtitles[0].forced);
    }

    #[test]
    fn external_subtitle_and_chapter_ipc_shapes() {
        let dir = std::env::temp_dir().join(format!("cx_gui_ext_ch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let srt = dir.join("film.deu.forced.ass");
        std::fs::write(&srt, "[Script Info]\n").unwrap();

        let info = probe::probe_external_subtitle_with_ui(&srt, TrackLabelMode::SmartMetadata, UiLang::EN).unwrap();
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["codec"], "ass");
        assert_eq!(json["kind"], "text");
        assert_eq!(json["language"], "deu");
        assert_eq!(json["languageName"], "German");
        assert_eq!(json["displayLabel"], "German (forced)");
        assert_eq!(json["forced"], true);
        assert_eq!(json["supported"], true);

        let parsed = parse_chapters_text("0:00 Intro\n5:00 Main\n".into(), None).unwrap();
        let ch_json = serde_json::to_value(&parsed).unwrap();
        assert_eq!(ch_json[0]["startSecs"], 0.0);
        assert_eq!(ch_json[0]["title"], "Intro");
        assert_eq!(ch_json[1]["startSecs"], 300.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quality_test_runner_evaluates_synthetic_clip() {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bin");
        let (ffmpeg, ffprobe) = (bin.join("ffmpeg.exe"), bin.join("ffprobe.exe"));
        if !ffmpeg.is_file() || !ffprobe.is_file() {
            eprintln!("skipped: bin/ffmpeg.exe or ffprobe.exe missing");
            return;
        }
        let dir = std::env::temp_dir().join(format!("cx-qtest-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("clip.mp4");
        let st = blocking_tool_command(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi", "-i", "testsrc2=s=640x360:r=25:d=2",
                "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(st.success(), "synthetic clip generation failed");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let dvd_res = rt
            .block_on(run_quality_test(
                &ffmpeg,
                &ffprobe,
                &AppSettings::default(),
                QualityTestRequest {
                    path: src.to_string_lossy().into_owned(),
                    disc: DiscType::Dvd5,
                    video_kbps: Some(4500),
                    audio_kbps: 192,
                    start_secs: Some(0.0),
                    trim_start: None,
                    trim_end: None,
                    crop: None,
                    denoise: Some(2.0),
                    brightness: Some(0.05),
                    contrast: Some(1.05),
                    saturation: Some(1.1),
                    bd_resolution: None,
                    bd_rate: None,
                },
            ))
            .unwrap();
        let dvd_sample = PathBuf::from(&dvd_res.sample_path);
        assert!(dvd_sample.is_file(), "missing DVD sample: {}", dvd_sample.display());
        assert!(std::fs::metadata(&dvd_sample).unwrap().len() > 0);
        assert!(dvd_res.psnr_db.is_finite() && dvd_res.psnr_db > 15.0, "psnr={}", dvd_res.psnr_db);
        assert!(["excellent", "good", "fair", "poor"].contains(&dvd_res.verdict.as_str()), "verdict={}", dvd_res.verdict);
        let _ = std::fs::remove_file(&dvd_sample);

        let bd_res = rt
            .block_on(run_quality_test(
                &ffmpeg,
                &ffprobe,
                &AppSettings { fast_encode: true, ..AppSettings::default() },
                QualityTestRequest {
                    path: src.to_string_lossy().into_owned(),
                    disc: DiscType::Bd25,
                    video_kbps: Some(8000),
                    audio_kbps: 448,
                    start_secs: None,
                    trim_start: None,
                    trim_end: None,
                    crop: None,
                    denoise: None,
                    brightness: None,
                    contrast: None,
                    saturation: None,
                    bd_resolution: Some("720".into()),
                    bd_rate: Some("50".into()),
                },
            ))
            .unwrap();
        let bd_sample = PathBuf::from(&bd_res.sample_path);
        assert!(bd_sample.is_file(), "missing BD sample: {}", bd_sample.display());
        assert!(std::fs::metadata(&bd_sample).unwrap().len() > 0);
        assert!(bd_res.psnr_db.is_finite() && bd_res.psnr_db > 15.0, "psnr={}", bd_res.psnr_db);
        assert!(["excellent", "good", "fair", "poor"].contains(&bd_res.verdict.as_str()), "verdict={}", bd_res.verdict);
        let _ = std::fs::remove_file(&bd_sample);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
