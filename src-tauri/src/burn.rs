//! Burning the finished VIDEO_TS to disc (after a successful conversion).
//!
//! Commands: `list_drives`, `disc_needs`, `probe_media`, `sanitize_label`, `start_burn`,
//! `cancel_burn`, `startup_burn`, `open_folder`. Progress arrives as `burn-progress`
//! ([`BurnStatus`]: phase, fraction, message and the remaining time), the result as one
//! `burn-done` event.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use convertx_rust::burner::layerbreak::{plan_disc, Capacities};
use convertx_rust::burner::{
    burn_video_ts, default_iso_path as iso_path, media_problem, preferred_speed, sanitize_volume_label, speed_label,
    speed_x, system_burner, BurnError, BurnJob, BurnOptions, BurnOutcome, BurnPhase, BurnProgress, DriveInfo, MediaInfo,
    WriteSpeed, DVD_1X_SECTORS_PER_SEC,
};
use convertx_rust::eta::{self, Eta, JobPhase};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// The engine's work directory next to the DVD folders and its marker file.
const WORK_DIR: &str = ".convertx-work";

#[derive(Default)]
pub struct BurnState {
    busy: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn list_drives() -> Result<Vec<DriveInfo>, String> {
    blocking(|| system_burner().list_drives().map_err(|e| e.to_string())).await
}

/// What the finished DVD needs from the disc.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscNeeds {
    pub sectors: u64,
    pub size_gb: f64,
    pub dual_layer: bool,
}

impl DiscNeeds {
    fn bytes(&self) -> f64 {
        self.sectors as f64 * 2048.0
    }
}

fn needs(video_ts: &Path) -> Result<DiscNeeds, String> {
    let plan = plan_disc(video_ts, "DVD_VIDEO", Capacities::DVD).map_err(|e| e.to_string())?;
    Ok(DiscNeeds {
        sectors: plan.image.total_sectors as u64,
        size_gb: plan.image.size_bytes() as f64 / 1e9,
        dual_layer: plan.layer_break.is_some(),
    })
}

#[tauri::command]
pub async fn disc_needs(video_ts: String) -> Result<DiscNeeds, String> {
    blocking(move || needs(Path::new(&video_ts))).await
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedOption {
    pub value: WriteSpeed,
    pub label: String,
    /// Image + burn time at this speed, without and with verification.
    pub secs: f64,
    pub secs_verify: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaView {
    pub media: MediaInfo,
    pub speeds: Vec<SpeedOption>,
    pub default_speed: WriteSpeed,
    /// Burning is not possible (shown instead of the burn button).
    pub problem: Option<String>,
    /// Burning is possible but needs confirmation (e.g. overwriting a rewritable disc).
    pub warning: Option<String>,
    /// The disc has old content that will be erased/overwritten.
    pub needs_erase: bool,
    /// Blank disc (or rewritable with automatic erasing) with enough room: the countdown may
    /// start the burn on its own.
    pub auto_ok: bool,
}

fn media_view(media: MediaInfo, needs: &DiscNeeds, speed_pref: &str, auto_erase: bool) -> MediaView {
    let option = |value: WriteSpeed, label: String| {
        let x = speed_x(value, &media.speeds);
        SpeedOption {
            value,
            label,
            secs: eta::image_and_burn_secs(needs.bytes(), x, false, needs.dual_layer),
            secs_verify: eta::image_and_burn_secs(needs.bytes(), x, true, needs.dual_layer),
        }
    };
    let mut speeds = vec![option(WriteSpeed::Max, "Maximal".into())];
    speeds.extend(media.speeds.iter().map(|&s| option(WriteSpeed::Specific(s), speed_label(s))));
    let default = preferred_speed(media.media_type, &media.speeds, speed_pref);
    let capacity = if media.blank { media.free_sectors } else { media.total_sectors };
    let problem = media_problem(&media, needs.sectors, needs.dual_layer);
    let needs_erase = problem.is_none() && !media.blank;
    let mut warning = None;
    if needs_erase {
        warning = Some(if auto_erase {
            format!("Die {} enthält Daten und wird automatisch gelöscht.", media.physical)
        } else {
            format!("Die {} enthält Daten – sie wird beim Brennen gelöscht.", media.physical)
        });
    } else if problem.is_none() && needs.dual_layer && !media.layer_break_supported {
        warning = Some(format!("{}: Der Layerwechsel wird vom Laufwerk festgelegt.", media.physical));
    }
    let room = capacity >= needs.sectors;
    let auto_ok = problem.is_none() && room && (media.blank || auto_erase);
    MediaView { media, speeds, default_speed: default, problem, warning, needs_erase, auto_ok }
}

#[tauri::command]
pub async fn probe_media(drive_id: String, video_ts: String, speed_pref: String, auto_erase: bool) -> Result<MediaView, String> {
    blocking(move || {
        let needs = needs(Path::new(&video_ts))?;
        let media = system_burner().probe_media(&drive_id).map_err(|e| e.to_string())?;
        Ok(media_view(media, &needs, &speed_pref, auto_erase))
    })
    .await
}

/// A DVD folder passed on the command line (`VIDEO_TS` or the folder containing it): the GUI
/// opens the burn dialog for it right away.
#[tauri::command]
pub fn startup_burn() -> Option<String> {
    std::env::args_os().skip(1).map(PathBuf::from).find_map(|p| {
        [p.clone(), p.join("VIDEO_TS")]
            .into_iter()
            .find(|d| d.join("VIDEO_TS.IFO").is_file())
            .map(|d| d.to_string_lossy().into_owned())
    })
}

#[tauri::command]
pub fn sanitize_label(text: String) -> String {
    sanitize_volume_label(&text)
}

/// Shows a folder in the file manager (Explorer on Windows).
#[tauri::command]
pub fn open_folder(path: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    if !path.is_dir() {
        return Err(format!("{} existiert nicht", path.display()));
    }
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    convertx_rust::tools::blocking_tool_command(program).arg(&path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnRequest {
    pub drive_id: String,
    pub video_ts: String,
    pub label: String,
    pub speed: WriteSpeed,
    #[serde(default = "yes")]
    pub verify: bool,
    #[serde(default)]
    pub keep_iso: bool,
    #[serde(default = "yes")]
    pub eject: bool,
    /// A rewritable disc with old content may be erased.
    #[serde(default)]
    pub erase: bool,
    /// After success: delete the DVD folder, image and work directory.
    #[serde(default)]
    pub delete_after: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnDone {
    pub ok: bool,
    pub cancelled: bool,
    pub message: String,
    /// The kept disc image.
    pub iso: Option<String>,
}

/// One `burn-progress` event: the backend's progress plus the remaining time.
#[derive(Debug, Clone, Serialize)]
pub struct BurnStatus {
    #[serde(flatten)]
    pub progress: BurnProgress,
    pub eta: Eta,
}

/// Deletes what the conversion produced for this disc: the project folder – only when it holds
/// nothing but `VIDEO_TS`/`AUDIO_TS` – and a left-over work directory next to it.
fn delete_dvd_output(video_ts: &Path) -> Result<(), String> {
    let project = video_ts.parent().ok_or("kein Projektordner")?;
    let foreign: Vec<String> = std::fs::read_dir(project)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.eq_ignore_ascii_case("VIDEO_TS") && !n.eq_ignore_ascii_case("AUDIO_TS"))
        .collect();
    if !foreign.is_empty() {
        return Err(format!("{} enthält weitere Dateien ({}) und wurde nicht gelöscht", project.display(), foreign.join(", ")));
    }
    std::fs::remove_dir_all(project).map_err(|e| format!("{}: {e}", project.display()))?;
    if let Some(output) = project.parent() {
        let work = output.join(WORK_DIR);
        // Only our own work directory (it carries a marker file of the same name).
        if work.join(WORK_DIR).is_file() {
            let _ = std::fs::remove_dir_all(work);
        }
    }
    Ok(())
}

/// Burns the request with the engine's [`burn_video_ts`] and emits `burn-progress`.
fn run_burn(app: &AppHandle, req: &BurnRequest, cancel: &AtomicBool) -> Result<BurnOutcome, BurnError> {
    let video_ts = PathBuf::from(&req.video_ts);
    let needs = needs(&video_ts).map_err(BurnError::Image)?;
    let media_speeds = system_burner().probe_media(&req.drive_id).map(|m| m.speeds).unwrap_or_default();
    let x = speed_x(req.speed, &media_speeds);
    let detail = format!(
        "{} bei {}{}",
        if needs.dual_layer { "DVD-9" } else { "DVD-5" },
        match req.speed {
            WriteSpeed::Max => format!("max. {}", speed_label((x * DVD_1X_SECTORS_PER_SEC).round() as u32)),
            WriteSpeed::Specific(s) => speed_label(s),
        },
        if req.verify { ", mit Überprüfung" } else { "" }
    );
    let job = BurnJob {
        iso: iso_path(&video_ts),
        video_ts,
        drive_id: req.drive_id.clone(),
        label: req.label.clone(),
        speed: req.speed,
        media_speeds,
        options: BurnOptions { layer_break: None, verify: req.verify, eject: req.eject, erase: req.erase },
        // Deleting the DVD afterwards takes the image with it.
        keep_iso: req.keep_iso && !req.delete_after,
        capacities: Capacities::DVD,
    };
    burn_video_ts(system_burner().as_ref(), &job, cancel, |u| {
        let phase = if u.progress.phase == BurnPhase::Preparing { JobPhase::Image } else { JobPhase::Burning };
        let eta = Eta::new(phase, Some(detail.clone()), Some(u.remaining_secs), None);
        let _ = app.emit("burn-progress", BurnStatus { progress: u.progress, eta });
    })
}

#[tauri::command]
pub fn start_burn(app: AppHandle, state: State<'_, BurnState>, request: BurnRequest) -> Result<(), String> {
    if !Path::new(&request.video_ts).join("VIDEO_TS.IFO").is_file() {
        return Err("Kein fertiger VIDEO_TS-Ordner gefunden.".into());
    }
    if state.busy.swap(true, Ordering::SeqCst) {
        return Err("Es wird bereits gebrannt.".into());
    }
    state.cancel.store(false, Ordering::SeqCst);
    let busy = state.busy.clone();
    let cancel = state.cancel.clone();
    std::thread::spawn(move || {
        let mut done = match run_burn(&app, &request, &cancel) {
            Ok(outcome) => BurnDone {
                ok: true,
                cancelled: false,
                message: if request.verify { "Die DVD wurde gebrannt und überprüft." } else { "Die DVD wurde gebrannt." }.into(),
                iso: outcome.iso.map(|p| p.to_string_lossy().into_owned()),
            },
            Err(BurnError::Cancelled) => {
                BurnDone { ok: false, cancelled: true, message: "Brennvorgang abgebrochen.".into(), iso: None }
            }
            Err(e) => BurnDone { ok: false, cancelled: false, message: e.to_string(), iso: None },
        };
        if done.ok && request.delete_after {
            done.message.push_str(&match delete_dvd_output(Path::new(&request.video_ts)) {
                Ok(()) => " DVD-Ordner und Arbeitsdateien wurden gelöscht.".to_string(),
                Err(e) => format!(" Hinweis: {e}."),
            });
        }
        busy.store(false, Ordering::SeqCst);
        let _ = app.emit("burn-done", done);
    });
    Ok(())
}

#[tauri::command]
pub fn cancel_burn(state: State<'_, BurnState>) -> bool {
    if state.busy.load(Ordering::SeqCst) {
        state.cancel.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use convertx_rust::burner::MediaType;

    fn media(blank: bool, rw: bool, t: MediaType, free: u64) -> MediaInfo {
        MediaInfo {
            present: true,
            media_type: t,
            physical: "DVD+R".into(),
            blank,
            rewritable: rw,
            writable: true,
            free_sectors: if blank { free } else { 0 },
            total_sectors: free,
            speeds: vec![10_820, 5_410, 2_705],
            layer_break_supported: true,
        }
    }

    #[test]
    fn auto_start_only_on_blank_media_with_room() {
        let small = DiscNeeds { sectors: 1_000_000, size_gb: 2.0, dual_layer: false };
        let v = media_view(media(true, false, MediaType::Dvd5, 2_295_104), &small, "auto", false);
        assert!(v.problem.is_none() && v.auto_ok && !v.needs_erase);
        assert_eq!(v.default_speed, WriteSpeed::Max);
        assert_eq!(v.speeds[0].label, "Maximal");
        assert_eq!(v.speeds[1].label, "16x");
        // Estimates: slower speeds take longer, verification adds time.
        assert!(v.speeds[3].secs > v.speeds[1].secs && v.speeds[1].secs_verify > v.speeds[1].secs);
        // Speed setting.
        let v8 = media_view(media(true, false, MediaType::Dvd5, 2_295_104), &small, "8", false);
        assert_eq!(v8.default_speed, WriteSpeed::Specific(5_410));

        let v = media_view(MediaInfo::none(), &small, "auto", false);
        assert!(v.problem.is_some() && !v.auto_ok);
        // Rewritable with content: allowed manually (warning), automatically only with auto-erase.
        let v = media_view(media(false, true, MediaType::Dvd5, 2_295_104), &small, "auto", false);
        assert!(v.problem.is_none() && v.warning.is_some() && v.needs_erase && !v.auto_ok);
        let v = media_view(media(false, true, MediaType::Dvd5, 2_295_104), &small, "auto", true);
        assert!(v.auto_ok && v.warning.as_deref().unwrap().contains("automatisch"));
        // Written DVD-R: impossible, even with auto-erase.
        assert!(media_view(media(false, false, MediaType::Dvd5, 2_295_104), &small, "auto", true).problem.is_some());
        // Too small.
        assert!(media_view(media(true, false, MediaType::Dvd5, 900_000), &small, "auto", false).problem.is_some());
        // DVD-9 content on a single-layer disc.
        let big = DiscNeeds { sectors: 3_000_000, size_gb: 6.1, dual_layer: true };
        assert!(media_view(media(true, false, MediaType::Dvd5, 2_295_104), &big, "auto", false).problem.is_some());
        let v = media_view(media(true, false, MediaType::Dvd9, 4_173_824), &big, "auto", false);
        assert!(v.auto_ok);
        assert_eq!(v.default_speed, WriteSpeed::Specific(2_705), "DVD-9 burns at 4x");
    }

    #[test]
    fn iso_next_to_project() {
        let dir = std::env::temp_dir().join(format!("cx_isopath_{}", std::process::id()));
        let vts = dir.join("Urlaub").join("VIDEO_TS");
        std::fs::create_dir_all(&vts).unwrap();
        assert_eq!(iso_path(&vts), dir.join("Urlaub.iso"));
        std::fs::write(dir.join("Urlaub.iso"), b"x").unwrap();
        assert_eq!(iso_path(&vts), dir.join("Urlaub (2).iso"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_only_pure_dvd_folders() {
        let dir = std::env::temp_dir().join(format!("cx_delete_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vts = dir.join("Film").join("VIDEO_TS");
        std::fs::create_dir_all(&vts).unwrap();
        std::fs::create_dir_all(dir.join("Film").join("AUDIO_TS")).unwrap();
        std::fs::write(vts.join("VIDEO_TS.IFO"), b"x").unwrap();
        let work = dir.join(WORK_DIR);
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(WORK_DIR), b"").unwrap();
        // A foreign file protects the folder.
        std::fs::write(dir.join("Film").join("notes.txt"), b"x").unwrap();
        assert!(delete_dvd_output(&vts).unwrap_err().contains("notes.txt"));
        assert!(vts.is_dir());
        std::fs::remove_file(dir.join("Film").join("notes.txt")).unwrap();
        delete_dvd_output(&vts).unwrap();
        assert!(!dir.join("Film").exists() && !work.exists());
        assert!(dir.is_dir(), "the output folder itself stays");
        std::fs::remove_dir_all(&dir).ok();
    }
}
