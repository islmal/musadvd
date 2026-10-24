//! Tauri desktop GUI for the convertx-rust engine.
//!
//! Commands (called from the frontend via `invoke`):
//! - `probe_file`         analyse a dropped video (ffprobe via `probe.rs`)
//! - `list_templates`     ConvertX menu templates found on this machine
//! - `template_preview`   template thumbnail as data URL
//! - `calculate_project`  bitrate and disc fill level for DVD5/DVD9/BD-25/BD-50
//! - `start_conversion`   run the full pipeline in the background; progress arrives as
//!                        `convert-event` events, the result as one `convert-done` event
//! - `default_output_dir` suggestion for the output folder
//! - burning (see `burn.rs`): `list_drives`, `disc_needs`, `probe_media`, `start_burn`,
//!   `cancel_burn`

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod burn;
mod burn_studio;
mod menu_editor;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use convertx_rust::calculator::{Clamp, DiscType, EncodingProfile, Quality};
use convertx_rust::config::{AppSettings, UNSUPPORTED_CHOICES};
use convertx_rust::dvd::{self, BuildError, BuildOptions, CancelToken, Event, StreamSelection, TitleLoad};
use convertx_rust::encoder::{ac3_settings, Aspect};
use convertx_rust::menu::{UiLang, DEFAULT_MOTION_SECS};
use convertx_rust::probe::{self, language_name, SubtitleKind};
use convertx_rust::template::Template;
use convertx_rust::tools::Toolchain;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::OnceCell;

/// Where ConvertXToDVD 7 installs its menu templates.
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
        }
    }

    fn settings(&self) -> AppSettings {
        self.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
    title: Option<String>,
    default: bool,
    /// AC-3 bitrate and layout on the DVD.
    ac3_kbps: u32,
    dvd_layout: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleTrack {
    index: u32,
    codec: String,
    /// `text` (rendered by spumux) or `bitmap` (PGS/VobSub converted, others unsupported).
    kind: String,
    language: Option<String>,
    language_name: String,
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

#[tauri::command]
async fn probe_file(state: State<'_, AppState>, path: String) -> Result<VideoMetadata, String> {
    let tools = toolchain(&state).await?;
    let p = PathBuf::from(&path);
    let info = probe::probe(&tools.ffprobe, &p).await.map_err(|e| e.to_string())?;
    let chapter_settings = state.settings().chapters();
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
        }),
        audio: info
            .audio
            .iter()
            .map(|a| {
                let (kbps, ch) = ac3_settings(a.channels);
                AudioTrack {
                    index: a.index,
                    codec: a.codec.clone(),
                    channels: a.channels,
                    channel_layout: a.channel_layout.clone(),
                    language_name: language_name(a.language.as_deref()),
                    language: a.language.clone(),
                    title: a.title.clone(),
                    default: a.default,
                    ac3_kbps: kbps,
                    dvd_layout: if ch == 6 { "5.1".into() } else { "Stereo".into() },
                }
            })
            .collect(),
        subtitles: info
            .subtitles
            .iter()
            .map(|s| SubtitleTrack {
                index: s.index,
                codec: s.codec.clone(),
                kind: if s.kind == SubtitleKind::Text { "text".into() } else { "bitmap".into() },
                language_name: language_name(s.language.as_deref()),
                language: s.language.clone(),
                title: s.title.clone(),
                forced: s.forced,
                supported: s.kind == SubtitleKind::Text || convertx_rust::subpic::supported(&s.codec),
            })
            .collect(),
        has_source_chapters: !info.chapters.is_empty(),
        chapters,
    })
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

fn template_root() -> PathBuf {
    std::env::var_os("CONVERTX_TEMPLATES").map_or_else(|| PathBuf::from(DEFAULT_TEMPLATE_ROOT), PathBuf::from)
}

#[tauri::command]
fn list_templates() -> Result<Vec<TemplateInfo>, String> {
    let root = template_root();
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
                thumbnail: t.thumbnail.as_ref().map(|th| folder.join(th).to_string_lossy().into_owned()),
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
fn template_preview(path: String) -> Result<String, String> {
    let p = Path::new(&path);
    let root = template_root();
    // Only serve images from the template folders.
    if !p.starts_with(&root) {
        return Err("preview outside the template folder".into());
    }
    let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
    let mime = match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
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
        "convertx-frame-{}-{}.jpg",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    let args = convertx_rust::encoder::thumbnail_args(Path::new(&path), at_secs.max(0.0), &out);
    let result = tools.ffmpeg(&args, 0.0, |_| {}).await.map_err(|e| e.to_string());
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    result?;
    let bytes = bytes.map_err(|e| e.to_string())?;
    Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
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
fn get_settings(state: State<'_, AppState>) -> AppSettings {
    state.settings()
}

#[tauri::command]
fn default_settings() -> AppSettings {
    AppSettings::default()
}

/// Stores the settings (sanitized) and returns what was actually saved.
#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: AppSettings) -> Result<AppSettings, String> {
    let settings = settings.sanitized();
    settings.save(&state.settings_path).map_err(|e| e.to_string())?;
    *state.settings.lock().unwrap_or_else(|e| e.into_inner()) = settings.clone();
    Ok(settings)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInfo {
    path: String,
    /// Problem while loading the file at startup (corrupt file was backed up).
    load_problem: Option<String>,
    /// Choices that are stored but not implemented by the engine yet.
    unsupported: Vec<String>,
    cpu_count: usize,
}

#[tauri::command]
fn settings_info(state: State<'_, AppState>) -> SettingsInfo {
    SettingsInfo {
        path: state.settings_path.to_string_lossy().into_owned(),
        load_problem: state.settings_note.lock().unwrap_or_else(|e| e.into_inner()).take(),
        unsupported: UNSUPPORTED_CHOICES.iter().map(|c| c.to_string()).collect(),
        cpu_count: dvd::available_cpus(),
    }
}

#[tauri::command]
fn default_output_dir() -> String {
    let base = std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("."), PathBuf::from);
    base.join("Videos").join("ConvertX DVD").to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// calculate_project
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSettings {
    disc: DiscType,
    titles: Vec<TitleLoad>,
    motion_menu: bool,
    motion_duration: Option<f64>,
    /// Blu-ray menus from the editor: clip length of every menu in seconds (1 for stills).
    #[serde(default)]
    bd_menu_secs: Vec<f64>,
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

/// Live estimate with the encoding profile from the global settings.
#[tauri::command]
fn calculate_project(state: State<'_, AppState>, settings: ProjectSettings) -> Result<CalculationResult, String> {
    project_calculation(settings, state.settings().profile())
}

/// Size of an encoded Blu-ray menu clip per second.
const BD_MENU_MB_PER_SEC: f64 = 1.1;

fn project_calculation(settings: ProjectSettings, profile: EncodingProfile) -> Result<CalculationResult, String> {
    let target_mb = settings.disc.target_mb();
    // A Blu-ray has menus only from the editor (8 Mbit/s H.264 + AC-3 + muxing ≈ 1.1 MB/s).
    let menu_mb = if settings.disc.is_bluray() {
        settings.bd_menu_secs.iter().sum::<f64>() * BD_MENU_MB_PER_SEC
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
    let plan = dvd::bitrate_plan(settings.disc, &settings.titles, menu_mb, profile).map_err(|e| e.to_string())?;
    let titles = settings
        .titles
        .iter()
        .map(|t| {
            let v = plan.video_kbps.min(settings.disc.limits().max_total_kbps.saturating_sub(t.audio_kbps));
            TitleCalc { video_kbps: v, quality_percent: quality(v, settings.disc), quality: Quality::for_disc(v, settings.disc) }
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
        total_secs: plan.duration_secs,
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
    custom_layout: Option<convertx_rust::layout::CustomLayout>,
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
        return Err("Kein Ausgabeordner gewählt.".into());
    }
    if state.converting.swap(true, Ordering::SeqCst) {
        return Err("Es läuft bereits eine Konvertierung.".into());
    }
    let mut opts = BuildOptions {
        inputs: config.inputs.iter().map(|i| PathBuf::from(&i.path)).collect(),
        disc: config.disc,
        template: PathBuf::from(&config.template),
        output: PathBuf::from(&config.output),
        menu_title: config.menu_title.filter(|t| !t.trim().is_empty()),
        custom_layout: config.custom_layout,
        ui_lang: UiLang::De,
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
            })
            .collect(),
        ..BuildOptions::default()
    };
    // Global settings: worker pool size, priority, menu/playback options, languages, ...
    state.settings().apply_to(&mut opts);
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
                message: format!("Laufzeitumgebung konnte nicht starten: {e}"),
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
                        let _ = app.emit("convert-cancelled", "Konvertierung abgebrochen");
                        return;
                    }
                    Ok(r) => ConversionDone {
                        ok: true,
                        message: format!(
                            "{} fertig: {:.0} MB von {} MB",
                            if opts.disc.is_bluray() { "Blu-ray" } else { "DVD" },
                            r.size_mb,
                            r.target_mb
                        ),
                        video_ts: Some(r.video_ts.to_string_lossy().into_owned()),
                        project_name: r.project_dir.file_name().map(|n| n.to_string_lossy().into_owned()),
                        size_mb: Some(r.size_mb),
                        target_mb,
                        warnings: r.warnings,
                    },
                    Err(e) => ConversionDone {
                        ok: false,
                        message: e.to_string(),
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

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .manage(burn::BurnState::default())
        .manage(burn_studio::StudioState::default())
        .setup(|app| {
            // `--burn-studio`: start with the Burn Studio window only (the main window stays hidden).
            if std::env::args().any(|a| a == "--burn-studio") {
                tauri::WebviewWindowBuilder::new(app, "burn-studio", tauri::WebviewUrl::App("burnStudio.html".into()))
                    .title("Brennstudio – ConvertX Rust")
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
            probe_file,
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
            cancel_conversion,
            burn::list_drives,
            burn::disc_needs,
            burn::probe_media,
            burn::start_burn,
            burn::cancel_burn,
            burn::startup_burn,
            burn::open_folder,
            title_frame,
            menu_editor::check_layout,
            menu_editor::text_lines,
            menu_editor::load_layout,
            menu_editor::save_layout,
            menu_editor::image_preview,
            burn_studio::studio_cancel,
            burn_studio::studio_drives,
            burn_studio::studio_drive_info,
            burn_studio::studio_eject,
            burn_studio::studio_load,
            burn_studio::studio_lock,
            burn_studio::studio_build_plan,
            burn_studio::studio_inspect,
            burn_studio::studio_write_image,
            burn_studio::studio_write_files,
            burn_studio::studio_discovery,
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
        .expect("error while running the ConvertX GUI");
}

#[cfg(test)]
mod tests {
    use super::*;
    use convertx_rust::calculator::MAX_VIDEO_KBPS;

    #[test]
    fn calculation_for_two_titles() {
        let r = project_calculation(ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![
                TitleLoad { duration_secs: 5400.0, audio_kbps: 448 + 192, subtitle_streams: 1 },
                TitleLoad { duration_secs: 1800.0, audio_kbps: 192, subtitle_streams: 0 },
            ],
            motion_menu: false,
            motion_duration: None,
            bd_menu_secs: vec![],
        }, EncodingProfile::Auto)
        .unwrap();
        assert_eq!(r.target_mb, 4300);
        assert_eq!(r.clamp, "none");
        assert!(r.fits);
        assert!(r.fill_percent > 90.0 && r.fill_percent <= 100.0, "{}", r.fill_percent);
        assert_eq!(r.titles.len(), 2);
        assert!(r.titles.iter().all(|t| t.quality_percent > 0.0 && t.quality_percent < 100.0));
    }

    #[test]
    fn calculation_too_long_for_dvd5() {
        let r = project_calculation(ProjectSettings {
            disc: DiscType::Dvd5,
            titles: vec![TitleLoad { duration_secs: 8.0 * 3600.0, audio_kbps: 448, subtitle_streams: 0 }],
            motion_menu: true,
            motion_duration: Some(25.0),
            bd_menu_secs: vec![],
        }, EncodingProfile::Auto)
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
            titles: vec![TitleLoad { duration_secs: 1800.0, audio_kbps: 192, subtitle_streams: 0 }],
            motion_menu: false,
            motion_duration: None,
            bd_menu_secs: vec![],
        };
        let auto = project_calculation(project(), EncodingProfile::Auto).unwrap();
        let lp = project_calculation(project(), EncodingProfile::Lp).unwrap();
        assert_eq!((auto.video_kbps, lp.video_kbps), (MAX_VIDEO_KBPS, 4000));
        assert_eq!(lp.clamp, "max");
        assert!(lp.estimated_mb < auto.estimated_mb);
    }

    #[test]
    fn blu_ray_calculation_uses_bd_limits() {
        let project = |disc| ProjectSettings {
            disc,
            titles: vec![TitleLoad { duration_secs: 2.0 * 3600.0, audio_kbps: 448, subtitle_streams: 0 }],
            motion_menu: true,
            motion_duration: Some(25.0),
            bd_menu_secs: vec![],
        };
        let r = project_calculation(project(DiscType::Bd25), EncodingProfile::Auto).unwrap();
        assert_eq!(r.target_mb, 22_900);
        assert_eq!((r.menu_mb, r.clamp.as_str()), (0.0, "none"), "no menus without an editor layout");
        // Menus from the editor: a still (1 s) and a 20 s loop.
        let with_menus = ProjectSettings { bd_menu_secs: vec![1.0, 20.0], ..project(DiscType::Bd25) };
        let m = project_calculation(with_menus, EncodingProfile::Auto).unwrap();
        assert!((m.menu_mb - 21.0 * BD_MENU_MB_PER_SEC).abs() < 1e-9);
        assert!(r.fits && (20_000..35_000).contains(&r.video_kbps), "{}", r.video_kbps);
        assert_eq!(r.titles[0].quality, Quality::Good);
        assert!(r.titles[0].quality_percent > 50.0 && r.titles[0].quality_percent < 100.0);
        // The profile ceilings are the BD ones.
        let lp = project_calculation(project(DiscType::Bd50), EncodingProfile::Lp).unwrap();
        assert_eq!((lp.video_kbps, lp.clamp.as_str()), (12_000, "max"));
        // A 12 h project does not fit a BD-25 even at 4000 kbps and is rated poor.
        let long = ProjectSettings { titles: vec![TitleLoad { duration_secs: 12.0 * 3600.0, audio_kbps: 448, subtitle_streams: 0 }], ..project(DiscType::Bd25) };
        let r = project_calculation(long, EncodingProfile::Auto).unwrap();
        assert_eq!((r.clamp.as_str(), r.fits), ("min", false));
        assert_eq!((r.titles[0].quality, r.titles[0].quality_percent), (Quality::Poor, 0.0));
    }

    #[test]
    fn empty_project() {
        let r = project_calculation(ProjectSettings { disc: DiscType::Dvd9, titles: vec![], motion_menu: false, motion_duration: None, bd_menu_secs: vec![] }, EncodingProfile::Auto).unwrap();
        assert_eq!(r.target_mb, 7950);
        assert!(r.fits);
    }

    #[test]
    fn preview_is_confined_to_templates() {
        assert!(template_preview(r"C:\Windows\win.ini".into()).is_err());
    }
}
