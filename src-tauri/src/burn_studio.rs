//! Commands and events of the Burn Studio window (`burnStudio.html`).
//!
//! One operation at a time (`StudioState`): it runs on its own thread and reports through the
//! events `studio-progress` ([`Progress`]), `studio-graph` (sample batches, at most 4/s),
//! `studio-log` (text) and one `studio-done` ([`Outcome`]). The settings come from the shared
//! `get_settings` / `save_settings` (`burn_studio` section).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use musadvd::burner::builder::{self, BuildError, BuildInput, BuildPlan, Content, Source};
use musadvd::burner::graph::{self, TemplateContext};
use musadvd::burner::layerbreak::{Capacities, LayerBreak};
use musadvd::burner::spti::DriveDetails;
use musadvd::burner::studio::{self, Ctx, Event, Outcome};
use musadvd::burner::tools::{self, Candidate, CandidateOptions};
use musadvd::burner::{power, sidecar, system_burner, DriveInfo, MediaInfo};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::AppState;

#[derive(Default)]
pub struct StudioState {
    busy: Arc<AtomicBool>,
    cancel: Mutex<Arc<AtomicBool>>,
    /// Keeps a tray lock alive (the lock lives as long as its handle): dropping the sender
    /// ends the holder thread.
    lock: Mutex<Option<Sender<()>>>,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

fn drive_by_id(id: &str) -> Result<DriveInfo, String> {
    system_burner().list_drives().map_err(|e| e.to_string())?.into_iter().find(|d| d.id == id).ok_or_else(|| "Drive not found.".to_string())
}

/// Runs `op` on a thread with the events wired to the window; `studio-done` ends it.
fn run(app: AppHandle, studio_state: &StudioState, app_state: &AppState, op: impl FnOnce(&Ctx) -> Result<Outcome, String> + Send + 'static) -> Result<(), String> {
    if studio_state.busy.swap(true, Ordering::SeqCst) {
        return Err("An operation is already running.".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *studio_state.cancel.lock().unwrap_or_else(|e| e.into_inner()) = cancel.clone();
    let busy = studio_state.busy.clone();
    let settings = app_state.settings().burn_studio.sanitized();
    let emitter = app.clone();
    let emit: studio::Emit = Arc::new(move |e| {
        let _ = match e {
            Event::Progress(p) => emitter.emit("studio-progress", p),
            Event::Graph(s) => emitter.emit("studio-graph", s),
            Event::Log(m) => emitter.emit("studio-log", m),
        };
    });
    std::thread::spawn(move || {
        let ctx = Ctx { settings, cancel, emit };
        let outcome = op(&ctx).unwrap_or_else(|message| Outcome { message, ..Default::default() });
        if outcome.ok {
            studio::on_complete(&ctx.settings.general.on_complete);
        }
        busy.store(false, Ordering::SeqCst);
        let _ = app.emit("studio-done", outcome);
    });
    Ok(())
}

#[tauri::command]
pub fn studio_cancel(state: State<'_, StudioState>) -> bool {
    let busy = state.busy.load(Ordering::SeqCst);
    if busy {
        state.cancel.lock().unwrap_or_else(|e| e.into_inner()).store(true, Ordering::SeqCst);
    }
    busy
}

// ---- drives ---------------------------------------------------------------------------------

#[tauri::command]
pub async fn studio_drives(state: State<'_, AppState>) -> Result<Vec<DriveInfo>, String> {
    let by_name = state.settings().burn_studio.io.device_sort == "name";
    blocking(move || {
        let mut drives = system_burner().list_drives().map_err(|e| e.to_string())?;
        drives.sort_by_key(|d| if by_name { d.name.to_lowercase() } else { d.path.to_lowercase() });
        Ok(drives)
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveView {
    media: Option<MediaInfo>,
    media_error: Option<String>,
    details: Option<DriveDetails>,
    details_error: Option<String>,
}

/// Media (IMAPI) and MMC details of a drive; each part may fail on its own.
#[tauri::command]
pub async fn studio_drive_info(state: State<'_, AppState>, drive_id: String) -> Result<DriveView, String> {
    let io = state.settings().burn_studio.io;
    blocking(move || {
        let drive = drive_by_id(&drive_id)?;
        let media = studio::media_of(&drive);
        let details = studio::drive_details(&drive, &io);
        Ok(DriveView {
            media: media.as_ref().ok().cloned(),
            media_error: media.err(),
            details: details.as_ref().ok().cloned(),
            details_error: details.err().map(|e| e.to_string()),
        })
    })
    .await
}

#[tauri::command]
pub async fn studio_eject(drive_id: String) -> Result<(), String> {
    blocking(move || studio::tray(&drive_by_id(&drive_id)?, "eject").map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn studio_load(drive_id: String) -> Result<(), String> {
    blocking(move || studio::tray(&drive_by_id(&drive_id)?, "load").map_err(|e| e.to_string())).await
}

/// Locks (`true`) or unlocks the tray of a drive until the next call or the end of the app.
#[tauri::command]
pub async fn studio_lock(state: State<'_, StudioState>, drive_id: String, lock: bool) -> Result<(), String> {
    let mut held = state.lock.lock().unwrap_or_else(|e| e.into_inner());
    held.take(); // dropping the sender releases the previous lock
    if !lock {
        return Ok(());
    }
    let drive = drive_by_id(&drive_id)?;
    let letter = power::drive_letter(&drive.path).ok_or("The drive has no drive letter.")?;
    let (tx, rx) = channel::<()>();
    let (ready_tx, ready_rx) = channel::<Result<(), String>>();
    std::thread::spawn(move || match power::TrayLock::acquire(letter) {
        // `_guard` (not `_`) keeps the lock until the arm ends, i.e. after the release signal.
        Ok(_guard) => {
            let _ = ready_tx.send(Ok(()));
            let _ = rx.recv(); // returns when the sender is dropped
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
        }
    });
    ready_rx.recv().map_err(|e| e.to_string())??;
    *held = Some(tx);
    Ok(())
}

// ---- build input ----------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDto {
    path: String,
    #[serde(default)]
    dest: String,
}

/// A layer break chosen in the candidate dialog.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakDto {
    l0_sectors: u32,
    vts: u32,
    padding: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInputDto {
    sources: Vec<SourceDto>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    udf_label: String,
    /// The answer to "build this folder as a disc?".
    answer: Option<bool>,
    layer_break: Option<BreakDto>,
}

impl BuildInputDto {
    fn input(&self) -> BuildInput {
        BuildInput {
            sources: self.sources.iter().map(|s| Source { path: PathBuf::from(&s.path), dest: s.dest.clone() }).collect(),
            label: self.label.clone(),
            udf_label: self.udf_label.clone(),
            answer: self.answer,
            dvd_caps: Some(Capacities::DVD),
            layer_break: self
                .layer_break
                .as_ref()
                .map(|b| LayerBreak { l0_sectors: b.l0_sectors, padding: (b.padding > 0).then_some((b.vts, b.padding)) }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedDto {
    path: String,
    reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanInfo {
    content: Content,
    content_name: &'static str,
    label: String,
    sectors: u32,
    bytes: u64,
    layer_break: Option<u32>,
    skipped: Vec<SkippedDto>,
    warnings: Vec<String>,
    /// Set when the folder looks like a disc and the settings ask what to do (answer and plan again).
    needs_answer: Option<Content>,
}

fn plan_of(input: &BuildInputDto, cfg: &musadvd::config::StudioBuild) -> Result<BuildPlan, BuildError> {
    builder::plan_build(&input.input(), cfg)
}

#[tauri::command]
pub async fn studio_build_plan(state: State<'_, AppState>, input: BuildInputDto) -> Result<PlanInfo, String> {
    let cfg = state.settings().burn_studio.sanitized().build;
    blocking(move || match plan_of(&input, &cfg) {
        Ok(p) => Ok(PlanInfo {
            content: p.content,
            content_name: p.content.name(),
            label: p.label.clone(),
            sectors: p.total_sectors(),
            bytes: p.total_sectors() as u64 * 2048,
            layer_break: p.layer_break.map(|l| l.l0_sectors),
            skipped: p.skipped.iter().map(|s| SkippedDto { path: s.path.display().to_string(), reason: s.reason.clone() }).collect(),
            warnings: p.warnings.clone(),
            needs_answer: None,
        }),
        Err(BuildError::NeedsAnswer(c)) => Ok(PlanInfo {
            content: c,
            content_name: c.name(),
            label: String::new(),
            sectors: 0,
            bytes: 0,
            layer_break: None,
            skipped: vec![],
            warnings: vec![],
            needs_answer: Some(c),
        }),
        Err(e) => Err(e.to_string()),
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathInfo {
    path: String,
    name: String,
    is_dir: bool,
    bytes: u64,
    files: u64,
    /// For a folder: what the layout looks like (`dvdvideo`, `bluray`, `data`).
    content: Option<Content>,
}

fn dir_size(dir: &Path, bytes: &mut u64, files: &mut u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path(), bytes, files),
            Ok(m) => {
                *bytes += m.len();
                *files += 1;
            }
            Err(_) => {}
        }
    }
}

/// Sizes and detected content of the paths the user added (the file list of the build view).
#[tauri::command]
pub async fn studio_inspect(paths: Vec<String>) -> Result<Vec<PathInfo>, String> {
    blocking(move || {
        Ok(paths
            .into_iter()
            .filter_map(|p| {
                let path = PathBuf::from(&p);
                let meta = std::fs::metadata(&path).ok()?;
                let (mut bytes, mut files) = (meta.len(), 1);
                if meta.is_dir() {
                    (bytes, files) = (0, 0);
                    dir_size(&path, &mut bytes, &mut files);
                }
                Some(PathInfo {
                    name: path.file_name().map_or(p.clone(), |n| n.to_string_lossy().into_owned()),
                    is_dir: meta.is_dir(),
                    content: meta.is_dir().then(|| builder::detect(&path)),
                    path: p,
                    bytes,
                    files,
                })
            })
            .collect())
    })
    .await
}

// ---- operations -----------------------------------------------------------------------------

/// Removes the image (and its layout files) when the setting `delete_image_after` asks for it.
fn delete_image(ctx: &Ctx, image: &Path, o: &mut Outcome) {
    if o.ok && ctx.settings.write.delete_image_after {
        let gone = std::fs::remove_file(image).is_ok();
        for ext in ["dvd", "mds", "cue"] {
            let _ = std::fs::remove_file(image.with_extension(ext));
        }
        o.message.push_str(if gone { " The image was deleted." } else { " The image could not be deleted." });
    }
}

#[tauri::command]
pub async fn studio_image_scrambled(image: String) -> Option<bool> {
    tokio::task::spawn_blocking(move || {
        musadvd::burner::css::image_scrambled(Path::new(&image))
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
pub fn studio_write_image(
    app: AppHandle,
    state: State<'_, StudioState>,
    app_state: State<'_, AppState>,
    drive_id: String,
    image: String,
    speed: String,
    erase: bool,
) -> Result<(), String> {
    let drive = drive_by_id(&drive_id)?;
    run(app, &state, &app_state, move |ctx| {
        let mut o = studio::write_image(ctx, &drive, Path::new(&image), &speed, erase)?;
        delete_image(ctx, Path::new(&image), &mut o);
        Ok(o)
    })
}

#[tauri::command]
pub fn studio_write_files(
    app: AppHandle,
    state: State<'_, StudioState>,
    app_state: State<'_, AppState>,
    drive_id: String,
    input: BuildInputDto,
    speed: String,
    erase: bool,
) -> Result<(), String> {
    let drive = drive_by_id(&drive_id)?;
    run(app, &state, &app_state, move |ctx| {
        let plan = plan_of(&input, &ctx.settings.build).map_err(|e| e.to_string())?;
        studio::write_files(ctx, &drive, &plan, &speed, erase)
    })
}

#[tauri::command]
pub fn studio_discovery(
    app: AppHandle,
    state: State<'_, StudioState>,
    app_state: State<'_, AppState>,
    drive_id: String,
    speed: String,
    sectors: Option<u64>,
) -> Result<(), String> {
    let drive = drive_by_id(&drive_id)?;
    run(app, &state, &app_state, move |ctx| studio::discovery(ctx, &drive, &speed, sectors))
}

#[tauri::command]
pub fn studio_erase(
    app: AppHandle,
    state: State<'_, StudioState>,
    app_state: State<'_, AppState>,
    drive_id: String,
    full: bool,
) -> Result<(), String> {
    let drive = drive_by_id(&drive_id)?;
    run(app, &state, &app_state, move |ctx| {
        studio::erase(ctx, &drive, full)?;
        Ok(studio::Outcome { ok: true, message: if full { "Disc erased (full)." } else { "Disc erased (quick)." }.into(), ..Default::default() })
    })
}

#[tauri::command]
pub fn studio_read(app: AppHandle, state: State<'_, StudioState>, app_state: State<'_, AppState>, drive_id: String, output: String) -> Result<(), String> {
    let drive = drive_by_id(&drive_id)?;
    run(app, &state, &app_state, move |ctx| studio::read(ctx, &drive, Path::new(&output)))
}

#[tauri::command]
pub fn studio_build(app: AppHandle, state: State<'_, StudioState>, app_state: State<'_, AppState>, input: BuildInputDto, output: String) -> Result<(), String> {
    run(app, &state, &app_state, move |ctx| {
        let plan = plan_of(&input, &ctx.settings.build).map_err(|e| e.to_string())?;
        studio::build(ctx, &plan, Path::new(&output))
    })
}

#[tauri::command]
pub fn studio_verify(app: AppHandle, state: State<'_, StudioState>, app_state: State<'_, AppState>, drive_id: String, image: Option<String>) -> Result<(), String> {
    let drive = drive_by_id(&drive_id)?;
    run(app, &state, &app_state, move |ctx| studio::verify(ctx, &drive, image.as_deref().map(Path::new)))
}

// ---- tools ----------------------------------------------------------------------------------

#[tauri::command]
pub async fn studio_label_read(iso: String) -> Result<String, String> {
    blocking(move || tools::read_label(Path::new(&iso)).map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn studio_label_patch(iso: String, label: String) -> Result<(String, String), String> {
    blocking(move || tools::patch_label(Path::new(&iso), &label).map_err(|e| e.to_string())).await
}

/// Layer break candidates of a DVD-Video `.iso` or folder.
#[tauri::command]
pub async fn studio_ifo_candidates(state: State<'_, AppState>, source: String, show_all: bool) -> Result<Vec<Candidate>, String> {
    let g = state.settings().burn_studio.general;
    let opts = CandidateOptions { prefer_non_seamless: g.lb_check_seamless, show_interleaved: g.lb_show_interleaved || show_all };
    blocking(move || {
        let path = PathBuf::from(&source);
        if path.is_dir() {
            let cfg = musadvd::config::StudioBuild { prompts: "always".into(), ..Default::default() };
            match builder::plan_build(&studio::build_input(&[path], ""), &cfg).map_err(|e| e.to_string())?.image {
                builder::BuiltImage::Udf(p) => tools::ifo_candidates(&p, Capacities::DVD, opts).map_err(|e| e.to_string()),
                builder::BuiltImage::Bd(_) => Err("Blu-ray images have no layer break.".into()),
            }
        } else {
            tools::ifo_candidates_iso(&path, Capacities::DVD, opts).map_err(|e| e.to_string())
        }
    })
    .await
}

/// Writes the `.dvd`, `.cue` or `.mds` next to `image`; returns the file.
#[tauri::command]
pub async fn studio_sidecar_write(kind: String, image: String, layer_break: Option<u32>) -> Result<String, String> {
    blocking(move || {
        let image = PathBuf::from(&image);
        let name = image.file_name().ok_or("Invalid image name")?.to_string_lossy().into_owned();
        let io = |e: std::io::Error| e.to_string();
        let out = match kind.as_str() {
            "dvd" => {
                let out = image.with_extension("dvd");
                let lb = layer_break.filter(|&l| l > 0).ok_or("The .dvd file needs a layer break.")?;
                sidecar::write_dvd(&out, &sidecar::DvdLayout { layer_break: lb, image: name }).map_err(io)?;
                out
            }
            "cue" => {
                let out = image.with_extension("cue");
                sidecar::write_cue(&out, &name, sidecar::CueMode::Mode1_2048).map_err(io)?;
                out
            }
            "mds" => {
                let out = image.with_extension("mds");
                let sectors = std::fs::metadata(&image).map_err(io)?.len().div_ceil(2048) as u32;
                sidecar::write_mds(&out, &sidecar::Mds { sectors, layer_break: layer_break.filter(|&l| l > 0), image: name }).map_err(io)?;
                out
            }
            other => return Err(format!("Unknown layout file '{other}'")),
        };
        Ok(out.display().to_string())
    })
    .await
}

// ---- graph ----------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphFile {
    kind: graph::GraphKind,
    config: Vec<(String, String)>,
    sample_ms: u32,
    samples: Vec<graph::Sample>,
}

#[tauri::command]
pub async fn studio_graph_load(path: String) -> Result<GraphFile, String> {
    blocking(move || {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        let f = graph::parse_ibg(&text)?;
        Ok(GraphFile { kind: f.kind, config: f.config, sample_ms: f.sample_ms, samples: f.samples })
    })
    .await
}

/// The folder graph files are stored in (for the "Graph laden" dialog).
#[tauri::command]
pub fn studio_graph_folder(state: State<'_, AppState>) -> String {
    graph::folder(&state.settings().burn_studio.graph.folder).display().to_string()
}

/// The graph file name a template produces (live preview in the settings).
#[tauri::command]
pub fn studio_template_preview(template: String) -> String {
    let c = TemplateContext {
        make_model: "PIONEER BD-RW BDR-209M".into(),
        firmware: "1.33".into(),
        when: graph::DateTime::now(),
        media_id: "CMC MAG-M01-00".into(),
        speed: "MAX".into(),
        volume: "MEIN_TEST".into(),
    };
    graph::file_name(&template, &c)
}
