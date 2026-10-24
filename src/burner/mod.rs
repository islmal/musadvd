//! Burning the authored VIDEO_TS folder to DVD: volume label, disc image (UDF 1.02 + ISO 9660
//! bridge), dual-layer break, and the platform burners (IMAPI2 on Windows, growisofs/xorriso
//! elsewhere).

pub mod builder;
pub mod graph;
pub mod isoread;
pub mod label;
pub mod layerbreak;
pub mod mmcwrite;
pub mod power;
pub mod reader;
pub mod ringbuf;
pub mod sidecar;
pub mod source;
pub mod spti;
#[cfg(test)]
pub(crate) mod testutil;
pub mod tools;
pub mod udf;
pub mod udf250;
pub mod verifier;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};

use crate::eta::Medium;

pub use label::{sanitize_volume_label, DEFAULT_LABEL};

/// Sectors per second at 1x DVD speed (1,385,000 bytes/s).
pub const DVD_1X_SECTORS_PER_SEC: f64 = 1_385_000.0 / 2048.0;
/// Sectors per second at 1x BD speed (4,495,500 bytes/s).
pub const BD_1X_SECTORS_PER_SEC: f64 = 4_495_500.0 / 2048.0;

/// An optical drive that can write DVDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    /// Backend-specific id (IMAPI unique id, device node).
    pub id: String,
    /// Display name ("PIONEER BD-RW BDR-209M (F:)").
    pub name: String,
    /// Mount point / drive letter (`F:\`), empty when unknown.
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Dvd5,
    Dvd9,
    /// BD-R / BD-RE, single layer.
    Bd25,
    /// BD-R / BD-RE, dual layer.
    Bd50,
    Unknown,
}

impl MediaType {
    pub fn is_bd(self) -> bool {
        matches!(self, MediaType::Bd25 | MediaType::Bd50)
    }

    pub fn is_dual_layer(self) -> bool {
        matches!(self, MediaType::Dvd9 | MediaType::Bd50)
    }

    /// Sectors per second at 1x (IMAPI reports speeds in sectors/s).
    pub fn sectors_per_sec_1x(self) -> f64 {
        if self.is_bd() {
            BD_1X_SECTORS_PER_SEC
        } else {
            DVD_1X_SECTORS_PER_SEC
        }
    }

    pub fn medium(self) -> Medium {
        if self.is_bd() {
            Medium::Bd
        } else {
            Medium::Dvd
        }
    }
}

/// Requested write speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteSpeed {
    Max,
    /// Sectors per second (as IMAPI reports them).
    Specific(u32),
}

/// What is in the drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub present: bool,
    pub media_type: MediaType,
    /// Physical type for display ("DVD+R DL", "DVD-RW", ...).
    pub physical: String,
    pub blank: bool,
    pub rewritable: bool,
    /// The drive can write this medium.
    pub writable: bool,
    pub free_sectors: u64,
    pub total_sectors: u64,
    /// Supported write speeds in sectors per second, fastest first.
    pub speeds: Vec<u32>,
    /// DVD+R DL: the layer break can be set by us (other DL media use the drive default).
    pub layer_break_supported: bool,
}

impl MediaInfo {
    pub fn none() -> Self {
        MediaInfo {
            present: false,
            media_type: MediaType::Unknown,
            physical: String::new(),
            blank: false,
            rewritable: false,
            writable: false,
            free_sectors: 0,
            total_sectors: 0,
            speeds: vec![],
            layer_break_supported: false,
        }
    }

    /// The disc can take an image of `sectors` without erasing anything.
    pub fn can_take(&self, sectors: u64) -> bool {
        self.present && self.writable && self.blank && self.free_sectors >= sectors
    }
}

/// Single or dual layer BD from the capacity IMAPI reports (BD-R: 12,219,392 / 24,438,784
/// sectors, BD-RE: 11,826,176 / 23,652,352).
pub fn bd_media_type(total_sectors: u64) -> MediaType {
    if total_sectors > udf250::BD25_SECTORS {
        MediaType::Bd50
    } else {
        MediaType::Bd25
    }
}

/// What a finished disc folder needs from the medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiscContent {
    /// `BDMV` (Blu-ray) instead of `VIDEO_TS` (DVD).
    pub bluray: bool,
    pub sectors: u64,
    /// Needs a DVD-9 / BD-50.
    pub dual_layer: bool,
}

/// A `BDMV` folder (not `VIDEO_TS`): recognised by its `index.bdmv`.
pub fn is_bdmv_folder(folder: &Path) -> bool {
    folder.join("index.bdmv").is_file()
}

impl DiscContent {
    pub fn medium(&self) -> Medium {
        if self.bluray {
            Medium::Bd
        } else {
            Medium::Dvd
        }
    }

    pub fn bytes(&self) -> f64 {
        self.sectors as f64 * 2048.0
    }

    /// Plans the image of a `VIDEO_TS` or `BDMV` folder.
    pub fn of_folder(folder: &Path) -> Result<DiscContent, String> {
        if is_bdmv_folder(folder) {
            let plan = udf250::plan_bd_image(bd_image_root(folder), "BLURAY").map_err(|e| e.to_string())?;
            let sectors = plan.total_sectors as u64;
            Ok(DiscContent { bluray: true, sectors, dual_layer: udf250::is_dual_layer(sectors) })
        } else {
            let plan = layerbreak::plan_disc(folder, "DVD_VIDEO", layerbreak::Capacities::DVD).map_err(|e| e.to_string())?;
            Ok(DiscContent { bluray: false, sectors: plan.image.total_sectors as u64, dual_layer: plan.layer_break.is_some() })
        }
    }
}

/// The image root of a `BDMV` folder: its parent (`BDMV` + `CERTIFICATE`).
pub fn bd_image_root(bdmv: &Path) -> &Path {
    bdmv.parent().unwrap_or(bdmv)
}

/// Why `media` cannot take `content`, or `None` when burning is possible – possibly after
/// erasing a rewritable disc (`!media.blank`). DVD content needs DVD media, BD content BD media.
pub fn media_problem(media: &MediaInfo, content: &DiscContent) -> Option<String> {
    let sectors = content.sectors;
    let size_gb = sectors as f64 * 2048.0 / 1e9;
    let capacity = if media.blank { media.free_sectors } else { media.total_sectors };
    let kind = if content.bluray { "Blu-ray" } else { "DVD" };
    if !media.present {
        Some("Keine Disc eingelegt.".to_string())
    } else if media.media_type == MediaType::Unknown {
        Some(format!("{} ist keine beschreibbare {kind}.", if media.physical.is_empty() { "Die Disc" } else { &media.physical }))
    } else if !media.writable {
        Some(format!("Die {} kann in diesem Laufwerk nicht beschrieben werden.", media.physical))
    } else if !media.blank && !media.rewritable {
        Some(format!("Die {} ist bereits beschrieben.", media.physical))
    } else if content.bluray != media.media_type.is_bd() {
        Some(if content.bluray {
            format!("Die Blu-ray braucht einen BD-R/BD-RE-Rohling – die eingelegte {} ist keine Blu-ray.", media.physical)
        } else {
            format!("Die DVD braucht einen DVD-Rohling – die eingelegte {} ist eine Blu-ray.", media.physical)
        })
    } else if content.dual_layer && !media.media_type.is_dual_layer() {
        Some(if content.bluray {
            format!("Die Blu-ray braucht {size_gb:.2} GB – bitte einen BD-50-Rohling (Dual Layer) einlegen.")
        } else {
            format!("Die DVD braucht {size_gb:.2} GB – bitte einen Double-Layer-Rohling (DVD-9) einlegen.")
        })
    } else if capacity < sectors {
        Some(format!("Nicht genug Platz: {size_gb:.2} GB benötigt, {:.2} GB verfügbar.", capacity as f64 * 2048.0 / 1e9))
    } else {
        None
    }
}

/// `<output>/<project>.iso` next to the project folder (`<output>/<project>/VIDEO_TS`),
/// numbered when the name is taken.
pub fn default_iso_path(video_ts: &Path) -> PathBuf {
    let project = video_ts.parent().unwrap_or(video_ts);
    let output = project.parent().unwrap_or(project);
    let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "DVD".into());
    let mut path = output.join(format!("{name}.iso"));
    let mut n = 2;
    while path.exists() {
        path = output.join(format!("{name} ({n}).iso"));
        n += 1;
    }
    path
}

/// Speed as "8x" (one decimal below 10x when not integral, e.g. "2.4x") for `media`.
pub fn speed_label(sectors_per_sec: u32, media: MediaType) -> String {
    let x = sectors_per_sec as f64 / media.sectors_per_sec_1x();
    let r = (x * 10.0).round() / 10.0;
    if (r - r.round()).abs() < 0.05 {
        format!("{}x", r.round() as u32)
    } else {
        format!("{r:.1}x")
    }
}

/// Default speed: maximum for DVD-5; for DVD-9 about 4x (the slowest supported speed that is
/// at least 2.4x and at most 4x, otherwise the slowest offered).
pub fn default_speed(media: MediaType, speeds: &[u32]) -> WriteSpeed {
    if media != MediaType::Dvd9 || speeds.is_empty() {
        return WriteSpeed::Max;
    }
    let x = |s: u32| s as f64 / DVD_1X_SECTORS_PER_SEC;
    let in_range: Vec<u32> = speeds.iter().copied().filter(|&s| (2.3..=4.1).contains(&x(s))).collect();
    let pick = in_range.into_iter().max().or_else(|| speeds.iter().copied().min()).unwrap();
    WriteSpeed::Specific(pick)
}

/// Speed from the "Schreibgeschwindigkeit" setting: `auto` (DVD-5 maximum, DVD-9 about 4x),
/// `max`, or a speed in x (`8`, `2.4`), mapped to the nearest supported speed not above it
/// (the slowest supported one when all are faster).
pub fn preferred_speed(media: MediaType, speeds: &[u32], pref: &str) -> WriteSpeed {
    let pref = pref.trim();
    if pref.eq_ignore_ascii_case("max") {
        return WriteSpeed::Max;
    }
    let pref = pref.trim_end_matches(['x', 'X']);
    let Ok(x) = pref.replace(',', ".").parse::<f64>() else { return default_speed(media, speeds) };
    if speeds.is_empty() {
        return WriteSpeed::Max;
    }
    let want = x * media.sectors_per_sec_1x() * 1.02; // tolerance: 8x is reported as 5410 or 5540
    let pick = speeds.iter().copied().filter(|&s| s as f64 <= want).max().or_else(|| speeds.iter().copied().min()).unwrap();
    WriteSpeed::Specific(pick)
}

/// Nominal speed in x of `speed` on `media` (`Max` = fastest supported, 16x DVD / 6x BD when
/// unknown).
pub fn speed_x(speed: WriteSpeed, speeds: &[u32], media: MediaType) -> f64 {
    let sps = match speed {
        WriteSpeed::Specific(s) => s,
        WriteSpeed::Max => match speeds.iter().max() {
            Some(&s) => s,
            None => return if media.is_bd() { 6.0 } else { 16.0 },
        },
    };
    sps as f64 / media.sectors_per_sec_1x()
}

/// Remaining time of image creation + burn + verification, from estimates for phases that have
/// not started and measured progress within the running one.
#[derive(Debug, Clone)]
pub struct BurnEta {
    medium: Medium,
    bytes: f64,
    speed_x: f64,
    verify: bool,
    dual_layer: bool,
    /// Phase in progress and when it started (seconds on the caller's clock).
    current: Option<(BurnPhase, f64)>,
}

impl BurnEta {
    pub fn new(medium: Medium, bytes: f64, speed_x: f64, verify: bool, dual_layer: bool) -> Self {
        BurnEta { medium, bytes, speed_x, verify, dual_layer, current: None }
    }

    fn estimate(&self, phase: BurnPhase) -> f64 {
        let write = crate::eta::burn_secs(self.medium, self.bytes, self.speed_x, false, false);
        match phase {
            BurnPhase::Preparing => crate::eta::image_secs(self.bytes),
            BurnPhase::Formatting => 5.0,
            // `burn_secs` without verification = writing + fixed lead-in/closing time.
            BurnPhase::Writing => write - 45.0,
            BurnPhase::Finalizing => 45.0 + if self.dual_layer { 60.0 } else { 0.0 },
            BurnPhase::Verifying if self.verify => self.bytes / self.medium.verify_bytes_per_sec(),
            BurnPhase::Verifying | BurnPhase::Done => 0.0,
        }
    }

    /// Remaining seconds after a progress report at `now_secs`.
    pub fn update(&mut self, phase: BurnPhase, fraction: f64, now_secs: f64) -> f64 {
        const ORDER: [BurnPhase; 5] =
            [BurnPhase::Preparing, BurnPhase::Formatting, BurnPhase::Writing, BurnPhase::Finalizing, BurnPhase::Verifying];
        if phase == BurnPhase::Done {
            return 0.0;
        }
        let start = match self.current {
            Some((p, t)) if p == phase => t,
            _ => {
                self.current = Some((phase, now_secs));
                now_secs
            }
        };
        let estimate = self.estimate(phase);
        let this = match crate::eta::extrapolate(now_secs - start, fraction) {
            // Phases with real progress: measured; finalizing has none, so count down.
            Some(measured) if phase != BurnPhase::Finalizing => measured,
            _ => (estimate - (now_secs - start)).min(estimate * (1.0 - fraction)).max(0.0),
        };
        let pos = ORDER.iter().position(|p| *p == phase).unwrap_or(0);
        this + ORDER[pos + 1..].iter().map(|&p| self.estimate(p)).sum::<f64>()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BurnPhase {
    Preparing,
    Formatting,
    Writing,
    Finalizing,
    Verifying,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BurnProgress {
    pub phase: BurnPhase,
    /// 0..1 within the phase.
    pub fraction: f64,
    pub message: String,
}

impl BurnProgress {
    pub fn new(phase: BurnPhase, fraction: f64, message: impl Into<String>) -> Self {
        BurnProgress { phase, fraction: fraction.clamp(0.0, 1.0), message: message.into() }
    }
}

/// How to burn.
#[derive(Debug, Clone, Default)]
pub struct BurnOptions {
    /// Layer 0 sector count for dual-layer media (from the image plan).
    pub layer_break: Option<u32>,
    /// Read the disc back and compare it with the image.
    pub verify: bool,
    /// Eject when done.
    pub eject: bool,
    /// A rewritable disc with old content may be erased/overwritten.
    pub erase: bool,
    /// Ring buffer between the source and the burner in MiB (`burn_source`; 0 = 40).
    pub ring_mib: u32,
    /// Buffer recovery: pause below `recovery_low` % until `recovery_high` % (0 = off).
    pub recovery_low: u32,
    pub recovery_high: u32,
    /// No standby while burning.
    pub prevent_standby: bool,
    /// Lock the tray while burning and verifying.
    pub lock_tray: bool,
    /// Receives the position and buffer levels while burning (graph telemetry).
    pub telemetry: Option<TelemetryHook>,
}

/// A reading of the burn in progress (what the graph samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Telemetry {
    /// Sector last written (IMAPI events) or handed to the burner.
    pub lba: u64,
    /// Fill level of our ring buffer; 0 without one.
    pub ring_percent: u32,
    /// Device buffer; `None` when the backend cannot tell (IMAPI).
    pub device_percent: Option<u32>,
}

#[derive(Clone)]
pub struct TelemetryHook(pub std::sync::Arc<dyn Fn(Telemetry) + Send + Sync>);

impl fmt::Debug for TelemetryHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TelemetryHook")
    }
}

#[derive(Debug)]
pub enum BurnError {
    NoMedia,
    Unsupported(String),
    NotEnoughSpace { needed: u64, free: u64 },
    Cancelled,
    VerifyFailed { sector: u64 },
    /// Writing the disc image failed.
    Image(String),
    Io(std::io::Error),
    Backend(String),
}

impl fmt::Display for BurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BurnError::NoMedia => write!(f, "keine Disc im Laufwerk"),
            BurnError::Unsupported(m) => write!(f, "nicht unterstützt: {m}"),
            BurnError::NotEnoughSpace { needed, free } => write!(
                f,
                "nicht genug Platz auf der Disc ({:.2} GB benötigt, {:.2} GB frei)",
                *needed as f64 * 2048.0 / 1e9,
                *free as f64 * 2048.0 / 1e9
            ),
            BurnError::Cancelled => write!(f, "abgebrochen"),
            BurnError::VerifyFailed { sector } => write!(f, "Überprüfung fehlgeschlagen: Sektor {sector} weicht vom Image ab"),
            BurnError::Image(m) => write!(f, "Disc-Image: {m}"),
            BurnError::Io(e) => write!(f, "{e}"),
            BurnError::Backend(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for BurnError {}

impl From<std::io::Error> for BurnError {
    fn from(e: std::io::Error) -> Self {
        BurnError::Io(e)
    }
}

/// A disc burner backend.
pub trait OpticalBurner: Send + Sync {
    fn list_drives(&self) -> Result<Vec<DriveInfo>, BurnError>;
    fn probe_media(&self, drive_id: &str) -> Result<MediaInfo, BurnError>;
    /// Burns `iso_path` and (with `options.verify`) reads it back. Blocks until done;
    /// `cancel` aborts as soon as the drive allows it.
    fn burn_iso(
        &self,
        drive_id: &str,
        iso_path: &Path,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        progress_tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError>;

    /// Burns the sectors of `source` (an image that is built on the fly, zeros, a `.bin`) through
    /// a ring buffer of `options.ring_mib`. Verification re-reads the source, so it needs a
    /// rewindable one. Backends that can only burn files report "nicht unterstützt".
    fn burn_source(
        &self,
        _drive_id: &str,
        _source: Box<dyn source::SectorSource>,
        _speed: WriteSpeed,
        _options: &BurnOptions,
        _cancel: &AtomicBool,
        _progress_tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        Err(BurnError::Unsupported("Brennen aus Quellen (nur unter Windows)".into()))
    }
}

/// The burner of this platform.
pub fn system_burner() -> Box<dyn OpticalBurner> {
    #[cfg(windows)]
    {
        Box::new(windows::ImapiBurner)
    }
    #[cfg(unix)]
    {
        Box::new(unix::CliBurner::detect())
    }
}

/// Writes the disc image of `video_ts` to `output` (label sanitised) and returns its plan,
/// including the layer break when the content needs a dual-layer disc.
pub fn create_disc_image(
    video_ts: &Path,
    output: &Path,
    label: &str,
    caps: layerbreak::Capacities,
    cancel: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<layerbreak::DiscPlan, layerbreak::PlanError> {
    let label = sanitize_volume_label(label);
    let plan = layerbreak::plan_disc(video_ts, &label, caps)?;
    let tmp = output.with_extension("iso.part");
    let written = udf::write_image(&plan.image, &tmp, &udf::Timestamp::now(), cancel, progress)
        .and_then(|()| std::fs::rename(&tmp, output).map_err(|error| udf::ImageError::Io { path: output.to_path_buf(), error }));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    Ok(plan)
}

/// One disc to burn from a finished VIDEO_TS or BDMV folder (shared by the GUI and the CLI).
#[derive(Debug, Clone)]
pub struct BurnJob {
    /// The `VIDEO_TS` or `BDMV` folder (a Blu-ray image is made of its parent: `BDMV` +
    /// `CERTIFICATE`).
    pub video_ts: PathBuf,
    pub drive_id: String,
    /// Where the disc image is written.
    pub iso: PathBuf,
    /// Volume label (sanitised when the image is written).
    pub label: String,
    pub speed: WriteSpeed,
    /// Speeds the medium offers (from [`OpticalBurner::probe_media`]), for the remaining time.
    pub media_speeds: Vec<u32>,
    pub options: BurnOptions,
    /// Keep the image after a successful burn (otherwise it is always removed).
    pub keep_iso: bool,
    pub capacities: layerbreak::Capacities,
}

/// Progress of [`burn_video_ts`] with the remaining time of image + burn + verification.
#[derive(Debug, Clone)]
pub struct BurnUpdate {
    pub progress: BurnProgress,
    pub remaining_secs: f64,
}

/// What [`burn_video_ts`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnOutcome {
    pub dual_layer: bool,
    /// The kept image (`keep_iso`).
    pub iso: Option<PathBuf>,
}

/// Writes the disc image of `job.video_ts`, burns it with `burner` (DVD: layer break from the
/// image plan; BD: UDF 2.50 image, no layer break) and removes the image unless it is to be kept
/// after success. `cancel` stops image writing and burning ([`BurnError::Cancelled`]).
pub fn burn_video_ts(
    burner: &dyn OpticalBurner,
    job: &BurnJob,
    cancel: &AtomicBool,
    mut on_update: impl FnMut(BurnUpdate) + Send,
) -> Result<BurnOutcome, BurnError> {
    let bluray = is_bdmv_folder(&job.video_ts);
    let label = sanitize_volume_label(&job.label);
    let (image_bytes, dual_layer) = if bluray {
        let plan = udf250::plan_bd_image(bd_image_root(&job.video_ts), &label).map_err(|e| BurnError::Image(e.to_string()))?;
        (plan.size_bytes() as f64, udf250::is_dual_layer(plan.total_sectors as u64))
    } else {
        let planned =
            layerbreak::plan_disc(&job.video_ts, &job.label, job.capacities).map_err(|e| BurnError::Image(e.to_string()))?;
        (planned.image.size_bytes() as f64, planned.layer_break.is_some())
    };
    let medium = if bluray { Medium::Bd } else { Medium::Dvd };
    let media_type = if bluray { MediaType::Bd25 } else { MediaType::Dvd5 };
    let x = speed_x(job.speed, &job.media_speeds, media_type);
    let mut eta = BurnEta::new(medium, image_bytes, x, job.options.verify, dual_layer);
    let start = std::time::Instant::now();
    let mut emit = |progress: BurnProgress| {
        let remaining_secs = eta.update(progress.phase, progress.fraction, start.elapsed().as_secs_f64());
        on_update(BurnUpdate { progress, remaining_secs });
    };
    const IMAGE: &str = "Disc-Image wird erstellt …";
    emit(BurnProgress::new(BurnPhase::Preparing, 0.0, IMAGE));
    let mut last = -1.0;
    let mut on_progress = |f: f64| {
        if f - last >= 0.01 || f >= 1.0 {
            last = f;
            emit(BurnProgress::new(BurnPhase::Preparing, f, IMAGE));
        }
    };
    // The layer break (DVD only) of the written image.
    let image: Result<Option<u32>, BurnError> = if bluray {
        udf250::create_bd_image(bd_image_root(&job.video_ts), &job.iso, &label, cancel, &mut on_progress)
            .map(|_| None)
            .map_err(|e| BurnError::Image(e.to_string()))
    } else {
        create_disc_image(&job.video_ts, &job.iso, &job.label, job.capacities, cancel, &mut on_progress)
            .map(|plan| plan.layer_break.map(|l| l.l0_sectors))
            .map_err(|e| BurnError::Image(e.to_string()))
    };
    // Until the image is renamed into place, a file at `job.iso` is not ours (CLI `--keep-iso PATH`).
    let image_written = image.is_ok();
    let result = image.and_then(|layer_break| {
        if cancel.load(Ordering::SeqCst) {
            return Err(BurnError::Cancelled);
        }
        let options = BurnOptions { layer_break, ..job.options.clone() };
        let (tx, rx) = std::sync::mpsc::channel::<BurnProgress>();
        std::thread::scope(|scope| {
            scope.spawn(|| rx.into_iter().for_each(&mut emit));
            burner.burn_iso(&job.drive_id, &job.iso, job.speed, &options, cancel, tx)
        })
    });
    let keep = job.keep_iso && result.is_ok();
    if !keep {
        if image_written {
            let _ = std::fs::remove_file(&job.iso);
        }
        let _ = std::fs::remove_file(job.iso.with_extension("iso.part"));
    }
    match result {
        Ok(()) => Ok(BurnOutcome { dual_layer, iso: keep.then(|| job.iso.clone()) }),
        Err(_) if cancel.load(Ordering::SeqCst) => Err(BurnError::Cancelled),
        Err(e) => Err(e),
    }
}

/// Compares the raw disc `device` (read front to back) with `image`; the first difference or
/// unreadable sector fails with its number.
pub fn verify_against_image(
    device: &mut (impl Read + Send),
    image: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<(), BurnError> {
    let mut img = source::FileSource::open(image)?;
    verify_against_source(device, &mut img, cancel, progress)
}

/// [`verify_against_image`] against any source that can be read from the start.
pub fn verify_against_source(
    device: &mut (impl Read + Send),
    image: &mut dyn source::SectorSource,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<(), BurnError> {
    let mut disc = source::ReadSource::new(device, image.sectors());
    // A sequential device cannot be read twice, so no retries here.
    let opts = verifier::VerifyOptions { sw_retries: 0, stop_at_first: true, chunk_sectors: 512, ..Default::default() };
    match verifier::verify(&mut disc, image, &opts, cancel, progress) {
        Ok(r) if r.passed => Ok(()),
        Ok(r) => Err(BurnError::VerifyFailed { sector: r.unreadable.iter().chain(&r.miscompares).copied().min().unwrap_or(0) }),
        Err(verifier::VerifyError::Cancelled) => Err(BurnError::Cancelled),
        Err(verifier::VerifyError::Image(e)) => Err(BurnError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x(v: f64) -> u32 {
        (v * DVD_1X_SECTORS_PER_SEC).round() as u32
    }

    #[test]
    fn speed_labels() {
        let l = |s: u32| speed_label(s, MediaType::Dvd5);
        assert_eq!(l(x(1.0)), "1x");
        assert_eq!(l(x(2.4)), "2.4x");
        assert_eq!(l(x(16.0)), "16x");
        assert_eq!(l(x(8.0) + 3), "8x");
        // BD speeds are multiples of 4.5 MB/s: 2195 sectors/s = 1x, 13,170 = 6x.
        assert_eq!(speed_label(2195, MediaType::Bd25), "1x");
        assert_eq!(speed_label(13_170, MediaType::Bd50), "6x");
    }

    #[test]
    fn default_speeds() {
        let speeds = [x(16.0), x(8.0), x(6.0), x(4.0), x(2.4)];
        assert_eq!(default_speed(MediaType::Dvd5, &speeds), WriteSpeed::Max);
        assert_eq!(default_speed(MediaType::Dvd9, &speeds), WriteSpeed::Specific(x(4.0)));
        assert_eq!(default_speed(MediaType::Dvd9, &[x(8.0), x(2.4)]), WriteSpeed::Specific(x(2.4)));
        assert_eq!(default_speed(MediaType::Dvd9, &[x(12.0), x(8.0)]), WriteSpeed::Specific(x(8.0)), "slowest offered");
        assert_eq!(default_speed(MediaType::Dvd9, &[]), WriteSpeed::Max);
    }

    #[test]
    fn speed_preferences() {
        let speeds = [x(16.0), x(8.0), x(4.0), x(2.4)];
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, "auto"), WriteSpeed::Max);
        assert_eq!(preferred_speed(MediaType::Dvd9, &speeds, "auto"), WriteSpeed::Specific(x(4.0)));
        assert_eq!(preferred_speed(MediaType::Dvd9, &speeds, "max"), WriteSpeed::Max);
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, "8"), WriteSpeed::Specific(x(8.0)));
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, "12x"), WriteSpeed::Specific(x(8.0)), "nearest below");
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, "2,4"), WriteSpeed::Specific(x(2.4)));
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, "1"), WriteSpeed::Specific(x(2.4)), "slowest offered");
        let dvd = MediaType::Dvd5;
        assert!((speed_x(WriteSpeed::Max, &speeds, dvd) - 16.0).abs() < 0.01);
        assert_eq!(speed_x(WriteSpeed::Max, &[], dvd), 16.0);
        assert!((speed_x(WriteSpeed::Specific(x(4.0)), &speeds, dvd) - 4.0).abs() < 0.01);
        // BD: speeds count in 4.5 MB/s steps; 12x = 26,341 sectors/s, "max" without a list is 6x.
        let bd_speeds = [26_341, 13_170, 8_780];
        assert!((speed_x(WriteSpeed::Max, &bd_speeds, MediaType::Bd25) - 12.0).abs() < 0.01);
        assert_eq!(speed_x(WriteSpeed::Max, &[], MediaType::Bd25), 6.0);
        assert_eq!(preferred_speed(MediaType::Bd25, &bd_speeds, "auto"), WriteSpeed::Max);
        assert_eq!(preferred_speed(MediaType::Bd50, &bd_speeds, "8"), WriteSpeed::Specific(13_170), "nearest below");
        assert_eq!(preferred_speed(MediaType::Bd50, &bd_speeds, "4x"), WriteSpeed::Specific(8_780));
        assert_eq!(preferred_speed(MediaType::Bd50, &bd_speeds, "1"), WriteSpeed::Specific(8_780), "slowest offered");
    }

    #[test]
    fn burn_eta() {
        // 4.37 GB at 8x with verification.
        let bytes = 4.37e9;
        let mut e = BurnEta::new(Medium::Dvd, bytes, 8.0, true, false);
        let total = crate::eta::image_secs(bytes) + 5.0 + crate::eta::burn_secs(Medium::Dvd, bytes, 8.0, true, false);
        let first = e.update(BurnPhase::Preparing, 0.0, 0.0);
        assert!((first - total).abs() < 1.0, "{first} vs {total}");
        // Writing half done after 200 s → 200 s left for writing, then closing + verify.
        e.update(BurnPhase::Writing, 0.0, 100.0);
        let r = e.update(BurnPhase::Writing, 0.5, 300.0);
        let after_writing = 45.0 + bytes / (8.0 * crate::eta::DVD_1X_BYTES_PER_SEC);
        assert!((r - (200.0 + after_writing)).abs() < 1.0, "{r}");
        // Finalizing counts down from its estimate.
        e.update(BurnPhase::Finalizing, 0.0, 500.0);
        let r = e.update(BurnPhase::Finalizing, 0.0, 520.0);
        assert!((r - (25.0 + bytes / (8.0 * crate::eta::DVD_1X_BYTES_PER_SEC))).abs() < 1.0, "{r}");
        assert_eq!(e.update(BurnPhase::Done, 1.0, 900.0), 0.0);
    }

    fn media(media_type: MediaType, physical: &str, blank: bool, rewritable: bool, sectors: u64) -> MediaInfo {
        MediaInfo {
            present: true,
            media_type,
            physical: physical.into(),
            blank,
            rewritable,
            writable: true,
            free_sectors: if blank { sectors } else { 0 },
            total_sectors: sectors,
            speeds: vec![],
            layer_break_supported: false,
        }
    }

    #[test]
    fn bd_media_by_capacity() {
        assert_eq!(bd_media_type(12_219_392), MediaType::Bd25);
        assert_eq!(bd_media_type(11_826_176), MediaType::Bd25, "BD-RE SL");
        assert_eq!(bd_media_type(12_219_393), MediaType::Bd50);
        assert_eq!(bd_media_type(23_652_352), MediaType::Bd50, "BD-RE DL");
        assert_eq!(bd_media_type(24_438_784), MediaType::Bd50);
        assert_eq!(bd_media_type(0), MediaType::Bd25);
    }

    #[test]
    fn media_problems_by_content_kind() {
        let dvd = DiscContent { bluray: false, sectors: 2_000_000, dual_layer: false };
        let bd25 = DiscContent { bluray: true, sectors: 10_000_000, dual_layer: false };
        let bd50 = DiscContent { bluray: true, sectors: 20_000_000, dual_layer: true };
        let dvd_r = media(MediaType::Dvd5, "DVD+R", true, false, 2_295_104);
        let bd_r = media(MediaType::Bd25, "BD-R", true, false, BD25);
        let bd_r_dl = media(MediaType::Bd50, "BD-R DL", true, false, udf250::BD50_SECTORS);
        // DVD content on BD-R and BD content on DVD+R are rejected, matching content is fine.
        assert!(media_problem(&dvd_r, &dvd).is_none() && media_problem(&bd_r, &bd25).is_none());
        assert!(media_problem(&bd_r, &dvd).unwrap().contains("Blu-ray"));
        assert!(media_problem(&dvd_r, &bd25).unwrap().contains("BD-R/BD-RE"));
        // BD-25 content on BD-50 is fine, BD-50 content on BD-25 is rejected.
        assert!(media_problem(&bd_r_dl, &bd25).is_none());
        let p = media_problem(&bd_r, &bd50).unwrap();
        assert!(p.contains("BD-50-Rohling"), "{p}");
        assert!(media_problem(&bd_r_dl, &bd50).is_none());
        // Not a writable Blu-ray.
        assert!(media_problem(&media(MediaType::Unknown, "CD-R", true, false, 1), &bd25).unwrap().contains("keine beschreibbare Blu-ray"));
        assert!(media_problem(&media(MediaType::Unknown, "CD-R", true, false, 1), &dvd).unwrap().contains("keine beschreibbare DVD"));
        // Rewritable BD-RE with content is allowed (it gets overwritten); a written BD-R is not.
        assert!(media_problem(&media(MediaType::Bd25, "BD-RE", false, true, BD25), &bd25).is_none());
        assert!(media_problem(&media(MediaType::Bd25, "BD-R", false, false, BD25), &bd25).unwrap().contains("bereits beschrieben"));
        // Too little room.
        assert!(media_problem(&media(MediaType::Bd25, "BD-R", true, false, 5_000_000), &bd25).unwrap().contains("Nicht genug Platz"));
    }

    const BD25: u64 = udf250::BD25_SECTORS;

    #[test]
    fn verify_detects_mismatch() {
        let dir = std::env::temp_dir().join(format!("cx_verify_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("a.iso");
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        std::fs::write(&img, &data).unwrap();
        let cancel = AtomicBool::new(false);
        let mut disc = data.clone();
        disc.resize(6144, 0); // device returns whole sectors
        verify_against_image(&mut &disc[..], &img, &cancel, &mut |_| {}).unwrap();
        disc[4097] ^= 1;
        assert!(matches!(
            verify_against_image(&mut &disc[..], &img, &cancel, &mut |_| {}),
            Err(BurnError::VerifyFailed { sector: 2 })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
