//! Burning the authored VIDEO_TS folder to DVD: volume label, disc image (UDF 1.02 + ISO 9660
//! bridge), dual-layer break, and the platform burners (IMAPI2 on Windows, growisofs/xorriso
//! elsewhere).

pub mod label;
pub mod layerbreak;
pub mod udf;
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

pub use label::{sanitize_volume_label, DEFAULT_LABEL};

/// Sectors per second at 1x DVD speed (1,385,000 bytes/s).
pub const DVD_1X_SECTORS_PER_SEC: f64 = 1_385_000.0 / 2048.0;

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
    Unknown,
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

/// Why `media` cannot take a DVD of `sectors` (`dual_layer`: the content needs a DVD-9), or
/// `None` when burning is possible – possibly after erasing a rewritable disc (`!media.blank`).
pub fn media_problem(media: &MediaInfo, sectors: u64, dual_layer: bool) -> Option<String> {
    let size_gb = sectors as f64 * 2048.0 / 1e9;
    let capacity = if media.blank { media.free_sectors } else { media.total_sectors };
    if !media.present {
        Some("Keine Disc eingelegt.".to_string())
    } else if media.media_type == MediaType::Unknown {
        Some(format!("{} ist keine beschreibbare DVD.", if media.physical.is_empty() { "Die Disc" } else { &media.physical }))
    } else if !media.writable {
        Some(format!("Die {} kann in diesem Laufwerk nicht beschrieben werden.", media.physical))
    } else if !media.blank && !media.rewritable {
        Some(format!("Die {} ist bereits beschrieben.", media.physical))
    } else if dual_layer && media.media_type != MediaType::Dvd9 {
        Some(format!("Die DVD braucht {size_gb:.2} GB – bitte einen Double-Layer-Rohling (DVD-9) einlegen."))
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

/// Speed as "8x" (one decimal below 10x when not integral, e.g. "2.4x").
pub fn speed_label(sectors_per_sec: u32) -> String {
    let x = sectors_per_sec as f64 / DVD_1X_SECTORS_PER_SEC;
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
    let want = x * DVD_1X_SECTORS_PER_SEC * 1.02; // tolerance: 8x is reported as 5410 or 5540
    let pick = speeds.iter().copied().filter(|&s| s as f64 <= want).max().or_else(|| speeds.iter().copied().min()).unwrap();
    WriteSpeed::Specific(pick)
}

/// Nominal speed in x of `speed` (`Max` = fastest supported, 16x when unknown).
pub fn speed_x(speed: WriteSpeed, speeds: &[u32]) -> f64 {
    let sps = match speed {
        WriteSpeed::Specific(s) => s,
        WriteSpeed::Max => match speeds.iter().max() {
            Some(&s) => s,
            None => return 16.0,
        },
    };
    sps as f64 / DVD_1X_SECTORS_PER_SEC
}

/// Remaining time of image creation + burn + verification, from estimates for phases that have
/// not started and measured progress within the running one.
#[derive(Debug, Clone)]
pub struct BurnEta {
    bytes: f64,
    speed_x: f64,
    verify: bool,
    dual_layer: bool,
    /// Phase in progress and when it started (seconds on the caller's clock).
    current: Option<(BurnPhase, f64)>,
}

impl BurnEta {
    pub fn new(bytes: f64, speed_x: f64, verify: bool, dual_layer: bool) -> Self {
        BurnEta { bytes, speed_x, verify, dual_layer, current: None }
    }

    fn estimate(&self, phase: BurnPhase) -> f64 {
        let write = crate::eta::burn_secs(self.bytes, self.speed_x, false, false);
        match phase {
            BurnPhase::Preparing => crate::eta::image_secs(self.bytes),
            BurnPhase::Formatting => 5.0,
            // `burn_secs` without verification = writing + fixed lead-in/closing time.
            BurnPhase::Writing => write - 45.0,
            BurnPhase::Finalizing => 45.0 + if self.dual_layer { 60.0 } else { 0.0 },
            BurnPhase::Verifying if self.verify => self.bytes / (8.0 * crate::eta::DVD_1X_BYTES_PER_SEC),
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

/// One disc to burn from a finished VIDEO_TS folder (shared by the GUI and the CLI).
#[derive(Debug, Clone)]
pub struct BurnJob {
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

/// Writes the disc image of `job.video_ts`, burns it with `burner` (layer break from the image
/// plan) and removes the image unless it is to be kept after success. `cancel` stops image
/// writing and burning ([`BurnError::Cancelled`]).
pub fn burn_video_ts(
    burner: &dyn OpticalBurner,
    job: &BurnJob,
    cancel: &AtomicBool,
    mut on_update: impl FnMut(BurnUpdate) + Send,
) -> Result<BurnOutcome, BurnError> {
    let planned = layerbreak::plan_disc(&job.video_ts, &job.label, job.capacities).map_err(|e| BurnError::Image(e.to_string()))?;
    let dual_layer = planned.layer_break.is_some();
    let x = speed_x(job.speed, &job.media_speeds);
    let mut eta = BurnEta::new(planned.image.size_bytes() as f64, x, job.options.verify, dual_layer);
    let start = std::time::Instant::now();
    let mut emit = |progress: BurnProgress| {
        let remaining_secs = eta.update(progress.phase, progress.fraction, start.elapsed().as_secs_f64());
        on_update(BurnUpdate { progress, remaining_secs });
    };
    const IMAGE: &str = "Disc-Image wird erstellt …";
    emit(BurnProgress::new(BurnPhase::Preparing, 0.0, IMAGE));
    let mut last = -1.0;
    let result = create_disc_image(&job.video_ts, &job.iso, &job.label, job.capacities, cancel, |f| {
        if f - last >= 0.01 || f >= 1.0 {
            last = f;
            emit(BurnProgress::new(BurnPhase::Preparing, f, IMAGE));
        }
    })
    .map_err(|e| BurnError::Image(e.to_string()))
    .and_then(|plan| {
        if cancel.load(Ordering::SeqCst) {
            return Err(BurnError::Cancelled);
        }
        let options = BurnOptions { layer_break: plan.layer_break.map(|l| l.l0_sectors), ..job.options.clone() };
        let (tx, rx) = std::sync::mpsc::channel::<BurnProgress>();
        std::thread::scope(|scope| {
            scope.spawn(|| rx.into_iter().for_each(&mut emit));
            burner.burn_iso(&job.drive_id, &job.iso, job.speed, &options, cancel, tx)
        })
    });
    let keep = job.keep_iso && result.is_ok();
    if !keep {
        let _ = std::fs::remove_file(&job.iso);
        let _ = std::fs::remove_file(job.iso.with_extension("iso.part"));
    }
    match result {
        Ok(()) => Ok(BurnOutcome { dual_layer, iso: keep.then(|| job.iso.clone()) }),
        Err(_) if cancel.load(Ordering::SeqCst) => Err(BurnError::Cancelled),
        Err(e) => Err(e),
    }
}

/// Compares the first `len` bytes of `device` (raw disc) with `image`.
pub fn verify_against_image(
    device: &mut impl Read,
    image: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<(), BurnError> {
    let mut img = std::fs::File::open(image)?;
    let len = img.metadata()?.len();
    const CHUNK: usize = 1 << 20; // multiple of the sector size
    let (mut a, mut b) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut done = 0u64;
    while done < len {
        if cancel.load(Ordering::Relaxed) {
            return Err(BurnError::Cancelled);
        }
        let n = (len - done).min(CHUNK as u64) as usize;
        img.read_exact(&mut a[..n])?;
        // Raw devices only read whole sectors.
        let want = n.div_ceil(2048) * 2048;
        let mut got = 0;
        while got < want {
            let r = device.read(&mut b[got..want])?;
            if r == 0 {
                return Err(BurnError::VerifyFailed { sector: (done + got as u64) / 2048 });
            }
            got += r;
        }
        if let Some(pos) = a[..n].iter().zip(&b[..n]).position(|(x, y)| x != y) {
            return Err(BurnError::VerifyFailed { sector: (done + pos as u64) / 2048 });
        }
        done += n as u64;
        progress(done as f64 / len as f64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x(v: f64) -> u32 {
        (v * DVD_1X_SECTORS_PER_SEC).round() as u32
    }

    #[test]
    fn speed_labels() {
        assert_eq!(speed_label(x(1.0)), "1x");
        assert_eq!(speed_label(x(2.4)), "2.4x");
        assert_eq!(speed_label(x(16.0)), "16x");
        assert_eq!(speed_label(x(8.0) + 3), "8x");
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
        assert!((speed_x(WriteSpeed::Max, &speeds) - 16.0).abs() < 0.01);
        assert_eq!(speed_x(WriteSpeed::Max, &[]), 16.0);
        assert!((speed_x(WriteSpeed::Specific(x(4.0)), &speeds) - 4.0).abs() < 0.01);
    }

    #[test]
    fn burn_eta() {
        // 4.37 GB at 8x with verification.
        let bytes = 4.37e9;
        let mut e = BurnEta::new(bytes, 8.0, true, false);
        let total = crate::eta::image_secs(bytes) + 5.0 + crate::eta::burn_secs(bytes, 8.0, true, false);
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
