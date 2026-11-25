//! Burn Studio operations (write, read, verify, build, Discovery) on top of the engine pieces:
//! one place for the Tauri commands and the CLI. Everything reports through [`Event`]s and ends
//! in an [`Outcome`]; nothing here knows about windows or terminals.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::builder::{self, BuildInput, BuildPlan};
use super::graph::{self, GraphKind, IbgFile, Sample, Sampler, TemplateContext};
use super::reader::{self, ReadError, ReadOptions, Split};
use super::sidecar;
use super::source::{Bin2352Source, FileSource, SectorSource, ZeroSource};
use super::spti::{DriveDetails, ScsiTransport};
use super::verifier::{self, VerifyError, VerifyOptions};
use super::{
    isoread, power, preferred_speed_for, speed_label, speed_x, system_burner, BurnEta, BurnError, BurnOptions, BurnPhase, DriveInfo,
    MediaInfo, Telemetry, TelemetryHook, WriteSpeed,
};
use crate::config::{BurnStudioSettings, StudioIo};

/// Progress of the running operation (the `studio-progress` payload).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// `preparing`, `formatting`, `writing`, `finalizing`, `verifying`, `reading`, `building`, `erasing`.
    pub phase: String,
    pub fraction: f64,
    pub lba: u64,
    pub total: u64,
    /// Current speed in x of the medium.
    pub speed_x: f64,
    /// Main (ring) buffer, percent.
    pub ring: u32,
    /// Device buffer, percent; `None` when the backend cannot tell.
    pub device: Option<u32>,
    pub eta_secs: Option<f64>,
    pub message: String,
}

pub enum Event {
    Progress(Progress),
    /// New graph samples (batched, at most four batches per second).
    Graph(Vec<Sample>),
    Log(String),
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

/// Settings, cancel flag and event sink of one operation.
#[derive(Clone)]
pub struct Ctx {
    pub settings: BurnStudioSettings,
    pub cancel: Arc<AtomicBool>,
    pub emit: Emit,
}

impl Ctx {
    pub fn log(&self, message: impl Into<String>) {
        (self.emit)(Event::Log(message.into()));
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// How an operation ended (the `studio-done` payload).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub ok: bool,
    pub cancelled: bool,
    pub message: String,
    /// Image files written (read / build).
    pub files: Vec<String>,
    pub md5: Option<String>,
    /// Sectors that could not be read or differed.
    pub bad_sectors: u64,
    /// Human readable details (miscompare list, skipped files), capped.
    pub details: Vec<String>,
    pub graph: Option<String>,
    pub layout_file: Option<String>,
}

impl Outcome {
    fn ok(message: impl Into<String>) -> Self {
        Outcome { ok: true, message: message.into(), ..Default::default() }
    }

    fn cancelled() -> Self {
        Outcome { cancelled: true, message: "cancelled".into(), ..Default::default() }
    }
}

fn path_text(p: &Path) -> String {
    p.display().to_string()
}

// ---------------------------------------------------------------------------------------------
// Drives.

/// Finds the drive `want` names (id, drive letter or part of the name); `None` = the only one.
pub fn find_drive(want: Option<&str>) -> Result<DriveInfo, String> {
    let drives = system_burner().list_drives().map_err(|e| e.to_string())?;
    if drives.is_empty() {
        return Err("No DVD/Blu-ray burner found.".into());
    }
    let list = || drives.iter().map(|d| format!("  {} (--drive {})", d.name, drive_arg(d))).collect::<Vec<_>>().join("\n");
    let Some(want) = want else {
        return match &drives[..] {
            [d] => Ok(d.clone()),
            _ => Err(format!("Several drives – please choose one:\n{}", list())),
        };
    };
    let w = want.trim().trim_end_matches('\\').to_lowercase();
    let hits: Vec<_> = drives
        .iter()
        .filter(|d| d.id == want || (!w.is_empty() && (d.path.trim_end_matches('\\').to_lowercase() == w || d.name.to_lowercase().contains(&w))))
        .collect();
    match hits[..] {
        [d] => Ok(d.clone()),
        [] => Err(format!("Drive '{want}' not found. Available:\n{}", list())),
        _ => Err(format!("Several drives match '{want}':\n{}", list())),
    }
}

fn drive_arg(d: &DriveInfo) -> String {
    let letter = d.path.trim_end_matches('\\');
    if letter.is_empty() { d.id.clone() } else { letter.to_string() }
}

fn letter_of(drive: &DriveInfo) -> Result<char, BurnError> {
    power::drive_letter(&drive.path).ok_or_else(|| BurnError::Backend("The drive has no drive letter".into()))
}

/// The MMC transport the settings ask for (`io.transport`).
#[cfg(windows)]
pub fn transport(drive: &DriveInfo, io: &StudioIo) -> Result<Box<dyn ScsiTransport>, BurnError> {
    use super::spti::{ImapiExTransport, SptiTransport};
    if io.backend == "imapi" {
        return Err(BurnError::Unsupported("MMC queries are disabled with the 'IMAPI' backend".into()));
    }
    if io.transport == "spti" {
        Ok(Box::new(SptiTransport::open(letter_of(drive)?)?))
    } else {
        Ok(Box::new(ImapiExTransport::open(&drive.id)?))
    }
}

/// `SG_IO` on the drive's device node (`/dev/srN`); `io.backend`/`io.transport` are Windows choices.
#[cfg(target_os = "linux")]
pub fn transport(drive: &DriveInfo, _io: &StudioIo) -> Result<Box<dyn ScsiTransport>, BurnError> {
    Ok(Box::new(super::spti::SgIoTransport::open(&drive.id)?))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn transport(_drive: &DriveInfo, _io: &StudioIo) -> Result<Box<dyn ScsiTransport>, BurnError> {
    Err(BurnError::Unsupported("MMC access is only available on Windows and Linux".into()))
}

/// Read-only drive and media details (INQUIRY, profiles, media ID, book type, speeds).
pub fn drive_details(drive: &DriveInfo, io: &StudioIo) -> Result<DriveDetails, BurnError> {
    let t = transport(drive, io)?;
    Ok(super::spti::drive_info(&*t))
}

/// `eject` or `load` the tray.
pub fn tray(drive: &DriveInfo, action: &str) -> Result<(), BurnError> {
    let h = power::DriveHandle::open(letter_of(drive)?, true)?;
    match action {
        "eject" => h.eject(),
        "load" => h.load(),
        other => Err(BurnError::Unsupported(format!("tray action '{other}'"))),
    }
}

/// The disc in `drive` as a sector source (raw volume, size from READ CAPACITY).
pub fn open_disc(drive: &DriveInfo, io: &StudioIo) -> Result<Box<dyn SectorSource>, BurnError> {
    #[cfg(windows)]
    let device = format!(r"\\.\{}:", letter_of(drive)?);
    #[cfg(not(windows))]
    let device = drive.id.clone();
    let t = transport(drive, &StudioIo { backend: "auto".into(), ..io.clone() })?;
    open_disc_with(&*t, &device)
}

/// Opens `device` (raw volume / block device) with the sector count READ CAPACITY reports.
fn open_disc_with(t: &dyn ScsiTransport, device: &str) -> Result<Box<dyn SectorSource>, BurnError> {
    let cap = super::spti::query(t, "READ CAPACITY", &super::spti::read_capacity(), 8)?;
    let (last, _) = super::spti::parse_read_capacity(&cap).ok_or_else(|| BurnError::Backend("READ CAPACITY: unreadable answer".into()))?;
    Ok(Box::new(FileSource::with_sectors(power::open_raw_volume(device)?, last as u64 + 1)))
}

/// Queries the drive via MMC READ DISC STRUCTURE (format 0x01) whether the current disc is CSS/CPPM protected.
pub fn disc_css(drive: &DriveInfo, io: &StudioIo) -> Option<bool> {
    let t = transport(drive, &StudioIo { backend: "auto".into(), ..io.clone() }).ok()?;
    let cmd = super::spti::read_disc_structure(false, 0, 0, 0x01, 8);
    let resp = super::spti::query(&*t, "READ DISC STRUCTURE (Copyright)", &cmd, 8).ok()?;
    super::spti::parse_copyright_info(&resp)
}

/// Resolves a relative image target referenced by a sidecar (`.cue`, `.dvd`, `.mds`), confining it
/// to `sidecar_dir` and rejecting UNC/device, absolute, drive-letter, and traversal paths.
fn resolve_sidecar_path(sidecar_dir: &Path, target: &str) -> Result<PathBuf, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("sidecar target path is empty".into());
    }
    if crate::menu::is_unc_or_device_path(trimmed) || crate::menu::has_windows_drive_prefix(trimmed) {
        return Err(format!("sidecar target path must be relative: {trimmed}"));
    }
    let normalized = crate::menu::normalize_separators(trimmed);
    let mut rel = PathBuf::new();
    for comp in Path::new(&normalized).components() {
        match comp {
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::ParentDir => {
                return Err(format!("sidecar target path escapes sidecar directory: {trimmed}"));
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(s) => rel.push(s),
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(format!("sidecar target path is empty: {trimmed}"));
    }
    let canon_dir = std::fs::canonicalize(sidecar_dir)
        .map_err(|e| format!("{}: {e}", sidecar_dir.display()))?;
    let candidate = canon_dir.join(&rel);
    let canon_target = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("{}: {e}", candidate.display()))?;
    if !canon_target.starts_with(&canon_dir) {
        return Err(format!("sidecar target escapes sidecar directory: {trimmed}"));
    }
    Ok(canon_target)
}

/// An image to burn or compare with: `.iso`, a `.cue` (single data track, 2048 or 2352 bytes
/// per sector), or a `.dvd` / `.mds` layout file. Also returns the layer break found in a layout
/// file next to the image.
pub fn open_image(path: &Path) -> Result<(Box<dyn SectorSource>, Option<u32>), String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let open = |p: &Path| FileSource::open(p).map_err(|e| format!("{}: {e}", p.display()));
    let text = |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
    match ext.as_str() {
        "cue" => {
            let cue = sidecar::parse_cue(&text(path)?).map_err(|e| e.0)?;
            let bin = resolve_sidecar_path(dir, &cue.file)?;
            let src: Box<dyn SectorSource> = match cue.mode {
                sidecar::CueMode::Mode1_2048 => Box::new(open(&bin)?),
                sidecar::CueMode::Mode1_2352 => Box::new(Bin2352Source::open(&bin).map_err(|e| format!("{}: {e}", bin.display()))?),
            };
            Ok((src, None))
        }
        "dvd" => {
            let l = sidecar::parse_dvd(&text(path)?).map_err(|e| e.0)?;
            let img = resolve_sidecar_path(dir, &l.image)?;
            Ok((Box::new(open(&img)?), Some(l.layer_break).filter(|&b| b > 0)))
        }
        "mds" => {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let m = sidecar::parse_mds(&bytes).map_err(|e| e.0)?;
            let img = resolve_sidecar_path(dir, &m.image)?;
            Ok((Box::new(open(&img)?), m.layer_break))
        }
        _ => {
            let dvd = path.with_extension("dvd");
            let mds = path.with_extension("mds");
            let layer_break = std::fs::read_to_string(&dvd)
                .ok()
                .and_then(|t| sidecar::parse_dvd(&t).ok())
                .map(|l| l.layer_break)
                .filter(|&b| b > 0)
                .or_else(|| std::fs::read(&mds).ok().and_then(|b| sidecar::parse_mds(&b).ok()).and_then(|m| m.layer_break));
            Ok((Box::new(open(path)?), layer_break))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Graph recording and progress.

struct Live {
    lba: u64,
    main: u32,
    device: Option<u32>,
    speed: f64,
    batch: Vec<Sample>,
    flushed: Instant,
}

/// Samples the position of an operation (graph data) and turns it into progress events.
struct Recorder {
    sampler: Mutex<Sampler>,
    live: Mutex<Live>,
    emit: Emit,
}

impl Recorder {
    fn new(ctx: &Ctx, one_x: f64) -> Arc<Self> {
        let g = &ctx.settings.graph;
        Arc::new(Recorder {
            sampler: Mutex::new(Sampler::new(g.sample_ms, one_x, g.capture_cpu)),
            live: Mutex::new(Live { lba: 0, main: 0, device: None, speed: 0.0, batch: Vec::new(), flushed: Instant::now() }),
            emit: ctx.emit.clone(),
        })
    }

    fn feed(&self, lba: u64, main: u32, device: Option<u32>) {
        let sample = self.sampler.lock().unwrap_or_else(|e| e.into_inner()).record(lba, main, device.unwrap_or(0));
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        (live.lba, live.main, live.device) = (lba, main, device);
        if let Some(s) = sample {
            live.speed = s.speed;
            live.batch.push(s);
            if live.flushed.elapsed() >= Duration::from_millis(250) {
                live.flushed = Instant::now();
                (self.emit)(Event::Graph(std::mem::take(&mut live.batch)));
            }
        }
    }

    fn hook(self: &Arc<Self>) -> TelemetryHook {
        let me = self.clone();
        TelemetryHook(Arc::new(move |t: Telemetry| me.feed(t.lba, t.ring_percent, t.device_percent)))
    }

    fn progress(&self, phase: &str, fraction: f64, total: u64, eta_secs: Option<f64>, message: &str) {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let lba = if phase == "writing" && live.lba > 0 { live.lba } else { (fraction * total as f64) as u64 };
        (self.emit)(Event::Progress(Progress {
            phase: phase.into(),
            fraction,
            lba,
            total,
            speed_x: live.speed,
            ring: live.main,
            device: live.device,
            eta_secs,
            message: message.into(),
        }));
    }

    /// Sends the last samples and returns all of them.
    fn finish(&self) -> Vec<Sample> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if !live.batch.is_empty() {
            (self.emit)(Event::Graph(std::mem::take(&mut live.batch)));
        }
        self.sampler.lock().unwrap_or_else(|e| e.into_inner()).samples().to_vec()
    }
}

/// Everything the `.ibg` header says about one operation.
struct GraphInfo<'a> {
    kind: GraphKind,
    drive: &'a DriveInfo,
    details: Option<&'a DriveDetails>,
    media_physical: String,
    media_id: String,
    speed: String,
    volume: String,
    image: String,
    sectors: u64,
    layer_break: Option<u32>,
    test_mode: bool,
    seconds: u64,
}

fn save_graph(ctx: &Ctx, g: GraphInfo, samples: &[Sample], sample_ms: u32) -> Option<PathBuf> {
    if !ctx.settings.graph.capture || samples.len() < 2 {
        return None;
    }
    let (vendor, model, firmware) = g.details.map_or_else(
        || (String::new(), g.drive.name.clone(), String::new()),
        |d| (d.vendor.clone(), d.model.clone(), d.firmware.clone()),
    );
    let make_model = format!("{vendor} {model}").split_whitespace().collect::<Vec<_>>().join(" ");
    let when = graph::DateTime::now();
    let tc = TemplateContext {
        make_model: make_model.clone(),
        firmware: firmware.clone(),
        when,
        media_id: g.media_id.clone(),
        speed: g.speed.clone(),
        volume: g.volume.clone(),
    };
    let dir = graph::folder(&ctx.settings.graph.folder);
    std::fs::create_dir_all(&dir).ok()?;
    let mut path = dir.join(graph::file_name(&ctx.settings.graph.template, &tc));
    for n in 2.. {
        if !path.exists() {
            break;
        }
        path = dir.join(graph::file_name(&format!("{}_{n}", ctx.settings.graph.template), &tc));
    }
    let tag = match g.kind {
        GraphKind::Write => "WRITE",
        GraphKind::Read => "READ",
        GraphKind::Verify => "VERIFY",
    };
    let mut config: Vec<(String, String)> = vec![
        ("DATE".into(), when.ibg()),
        ("DEVICE".into(), g.drive.name.clone()),
        ("DEVICE_MAKEMODEL".into(), make_model),
        ("DEVICE_FIRMWAREVERSION".into(), firmware),
        ("DEVICE_DRIVELETTER".into(), g.drive.path.trim_end_matches(['\\', ':']).to_string()),
        ("MEDIA_TYPE".into(), g.media_physical.clone()),
        ("MEDIA_ID".into(), g.media_id),
        ("MEDIA_LAYER_BREAK".into(), g.layer_break.unwrap_or(0).to_string()),
        ("DATA_IMAGEFILE".into(), g.image),
        ("DATA_SECTORS".into(), g.sectors.to_string()),
        ("DATA_TYPE".into(), "MODE1/2048".into()),
        ("DATA_VOLUMEIDENTIFIER".into(), g.volume),
    ];
    if g.kind == GraphKind::Write {
        config.push(("WRITE_TEST_MODE".into(), if g.test_mode { "True" } else { "False" }.into()));
    }
    config.push((format!("{tag}_SPEED_SELECTED"), g.speed.to_uppercase()));
    config.extend(graph::speed_summary(samples, g.seconds).into_iter().map(|(k, v)| {
        // `TIME_TAKEN` is `WRITE_TIME_TAKEN` in the sample, the speeds are `WRITE_SPEED_*`.
        (format!("{tag}_{k}"), v)
    }));
    let file = IbgFile { kind: g.kind, config, sample_ms, samples: samples.to_vec() };
    std::fs::write(&path, graph::write_ibg(&file)).ok()?;
    Some(path)
}

/// Remaining time from the elapsed time and the fraction done.
fn remaining(start: Instant, fraction: f64) -> Option<f64> {
    (fraction > 0.01 && fraction < 1.0).then(|| start.elapsed().as_secs_f64() * (1.0 - fraction) / fraction)
}

fn phase_name(p: BurnPhase) -> &'static str {
    match p {
        BurnPhase::Preparing => "preparing",
        BurnPhase::Formatting => "formatting",
        BurnPhase::Writing => "writing",
        BurnPhase::Finalizing => "finalizing",
        BurnPhase::Verifying => "verifying",
        BurnPhase::Done => "done",
    }
}

fn one_x_for(sectors: u64) -> f64 {
    if sectors > 4_400_000 { super::BD_1X_SECTORS_PER_SEC } else { super::DVD_1X_SECTORS_PER_SEC }
}

// ---------------------------------------------------------------------------------------------
// Writing.

#[derive(Debug, Clone)]
pub struct WriteJob {
    pub drive: DriveInfo,
    /// `auto` (rules from the settings, then 8x DVD / 4x DVD-9 and Blu-ray), `max`, or a speed in x.
    pub speed: String,
    /// A rewritable disc with old content may be erased/overwritten.
    pub erase: bool,
    pub layer_break: Option<u32>,
    /// For the graph file only.
    pub volume: String,
    pub image: String,
}

/// Burn Studio's "Auto" without a matching rule: the same `auto` as the main burn step
/// (8x DVD, 4x DVD-9/Blu-ray, see [`super::default_speed`]).
pub const STUDIO_AUTO_SPEED: &str = "auto";

/// The speed wish after the auto-speed rules (first match on media ID and drive wins).
pub fn resolve_speed(settings: &BurnStudioSettings, drive: &DriveInfo, media_id: &str, wish: &str) -> String {
    if !(wish.is_empty() || wish.eq_ignore_ascii_case("auto")) {
        return wish.to_string();
    }
    let id = media_id.to_lowercase();
    let name = drive.name.to_lowercase();
    settings
        .auto_speed
        .iter()
        .find(|r| {
            let (m, d) = (r.media_id.trim().to_lowercase(), r.drive.trim().to_lowercase());
            (m.is_empty() || id.contains(&m)) && (d.is_empty() || name.contains(&d)) && !(m.is_empty() && d.is_empty())
        })
        .map_or(STUDIO_AUTO_SPEED.into(), |r| r.speed.clone())
}

fn burn_dispatch(
    ctx: &Ctx,
    drive: &DriveInfo,
    source: Box<dyn SectorSource>,
    speed: WriteSpeed,
    options: &BurnOptions,
    tx: mpsc::Sender<super::BurnProgress>,
) -> Result<(), BurnError> {
    #[cfg(windows)]
    {
        let use_direct = match ctx.settings.io.backend.as_str() {
            "direct" => true,
            "auto" => {
                let missing_write_feature = !super::windows::is_imapi_write_feature_current(&drive.id);
                if missing_write_feature {
                    ctx.log("Note: the drive reports no current IMAPI2 write feature page (USB bridge incompatibility). Switching to SPTI Direct automatically.");
                }
                missing_write_feature
            }
            _ => false,
        };
        if use_direct {
            let direct = super::mmcwrite::DirectOptions::from_settings(&ctx.settings.write, &ctx.settings.io);
            return super::mmcwrite::burn_direct(&drive.id, ctx.settings.io.transport == "spti", source, speed, options, &direct, &ctx.cancel, &tx);
        }
    }
    // CliBurner only burns files; sources (Discovery zeros, streamed builds) go straight through
    // MMC over SG_IO.
    #[cfg(target_os = "linux")]
    let r = {
        let t = super::spti::SgIoTransport::open(&drive.id)?;
        let direct = super::mmcwrite::DirectOptions::from_settings(&ctx.settings.write, &ctx.settings.io).with_speed(speed);
        super::mmcwrite::burn_direct_with(&t, source, options, &direct, &ctx.cancel, &tx)
    };
    #[cfg(not(target_os = "linux"))]
    let r = system_burner().burn_source(&drive.id, source, speed, options, &ctx.cancel, tx);
    r
}

/// Waits (cancellable) until a blank disc is in the drive.
fn wait_for_blank(ctx: &Ctx, drive: &DriveInfo, copy: u32) -> bool {
    ctx.log(format!("Please insert a blank disc for copy {copy} …"));
    let burner = system_burner();
    while !ctx.cancelled() {
        if burner.probe_media(&drive.id).is_ok_and(|m| m.present && m.blank) {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

/// Burns `make_source()` (called once per copy) to `job.drive`: speed rules, ring buffer,
/// telemetry graph (`.ibg`), copies with a disc change in between.
pub fn write(ctx: &Ctx, job: &WriteJob, make_source: &(dyn Fn() -> Result<Box<dyn SectorSource>, String> + Sync)) -> Result<Outcome, String> {
    let (s, w, io) = (&ctx.settings, &ctx.settings.write, &ctx.settings.io);
    let burner = system_burner();
    let copies = w.copies.max(1);
    let mut graphs = Vec::new();
    for copy in 1..=copies {
        if copy > 1 && !wait_for_blank(ctx, &job.drive, copy) {
            return Ok(Outcome::cancelled());
        }
        if copies > 1 {
            ctx.log(format!("Copy {copy} of {copies}"));
        }
        let media = burner.probe_media(&job.drive.id).map_err(|e| e.to_string())?;
        if !media.present {
            return Err(BurnError::NoMedia.to_string());
        }
        let details = drive_details(&job.drive, io).ok();
        let media_id = details.as_ref().and_then(|d| d.media_id.clone()).unwrap_or_default();
        let wish = resolve_speed(s, &job.drive, &media_id, &job.speed);
        let speed = preferred_speed_for(&media, &wish);
        let x = speed_x(speed, &media.speeds, media.media_type);
        let source = make_source()?;
        let sectors = source.sectors();
        let test_mode = w.test_mode && io.backend == "direct";
        let rec = Recorder::new(ctx, media.media_type.sectors_per_sec_1x());
        let user_break = (w.layer_break == "user" && w.user_l0_sectors > 0).then_some(w.user_l0_sectors);
        let options = BurnOptions {
            layer_break: user_break.or(job.layer_break),
            verify: w.verify && !test_mode,
            eject: copy < copies || io.eject_after.write,
            erase: job.erase && !media.blank,
            ring_mib: io.ring_mib,
            recovery_low: io.recovery_main_low,
            recovery_high: io.recovery_main_high,
            prevent_standby: s.general.prevent_standby,
            lock_tray: io.lock_tray.write,
            telemetry: Some(rec.hook()),
        };
        ctx.log(format!(
            "{}: {} at {}{}",
            job.drive.name,
            media.physical,
            match speed {
                WriteSpeed::Max => "max.".to_string(),
                WriteSpeed::Specific(sps) => speed_label(sps, media.media_type),
            },
            if test_mode { " (Testmodus)" } else { "" }
        ));
        let started = Instant::now();
        let (tx, rx) = mpsc::channel::<super::BurnProgress>();
        let result = std::thread::scope(|scope| {
            let rec = &rec;
            let medium = media.media_type.medium();
            let (verify, dual) = (options.verify, media.media_type.is_dual_layer());
            scope.spawn(move || {
                let mut eta = BurnEta::new(medium, sectors as f64 * 2048.0, x, verify, dual);
                for p in rx {
                    let left = eta.update(p.phase, p.fraction, started.elapsed().as_secs_f64());
                    rec.progress(phase_name(p.phase), p.fraction, sectors, Some(left), &p.message);
                }
            });
            burn_dispatch(ctx, &job.drive, source, speed, &options, tx)
        });
        let samples = rec.finish();
        let sample_ms = s.graph.sample_ms;
        let graph = save_graph(
            ctx,
            GraphInfo {
                kind: GraphKind::Write,
                drive: &job.drive,
                details: details.as_ref(),
                media_physical: media.physical.clone(),
                media_id,
                speed: match speed {
                    WriteSpeed::Max => "max".into(),
                    WriteSpeed::Specific(_) => format!("{}x", (x * 10.0).round() / 10.0),
                },
                volume: job.volume.clone(),
                image: job.image.clone(),
                sectors,
                layer_break: job.layer_break,
                test_mode,
                seconds: started.elapsed().as_secs(),
            },
            &samples,
            sample_ms,
        );
        graphs.extend(graph);
        match result {
            Ok(()) => {}
            Err(BurnError::Cancelled) => return Ok(Outcome::cancelled()),
            Err(_) if ctx.cancelled() => return Ok(Outcome::cancelled()),
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut o = Outcome::ok(format!("{} {} burned{}.", copies, if copies == 1 { "disc" } else { "discs" }, if w.verify { " and verified" } else { "" }));
    o.graph = graphs.last().map(|p| path_text(p));
    Ok(o)
}

/// Burns an image file (`.iso`, `.cue`, `.dvd`, `.mds`).
pub fn write_image(ctx: &Ctx, drive: &DriveInfo, image: &Path, speed: &str, erase: bool) -> Result<Outcome, String> {
    if super::css::image_scrambled(image) == Some(true) {
        ctx.log("Warning: this image contains CSS-encrypted sectors. A copy on a blank disc will not play.");
    }
    let (probe, layer_break) = open_image(image)?;
    let volume = isoread::DiscImage::open(File::open(image).map_err(|e| e.to_string())?).map(|i| i.label().to_string()).unwrap_or_default();
    drop(probe);
    let job = WriteJob { drive: drive.clone(), speed: speed.into(), erase, layer_break, volume, image: path_text(image) };
    write(ctx, &job, &|| open_image(image).map(|(s, _)| s))
}

/// Discovery test burn: zeros over `sectors` (default: the free capacity of the disc).
pub fn discovery(ctx: &Ctx, drive: &DriveInfo, speed: &str, sectors: Option<u64>) -> Result<Outcome, String> {
    let media = system_burner().probe_media(&drive.id).map_err(|e| e.to_string())?;
    let zeros = super::source::discovery(&media, sectors)?;
    let n = zeros.0;
    let job = WriteJob { drive: drive.clone(), speed: speed.into(), erase: false, layer_break: None, volume: "DISCOVERY".into(), image: "-==/\\/[DISCOVERY]\\/\\==-".into() };
    write(ctx, &job, &|| Ok(Box::new(ZeroSource(n))))
}

/// Builds the image from files and folders and burns it. Without verification the image is
/// built on the fly into the ring buffer; with it (the source must be read twice) it goes to a
/// temporary file first.
pub fn write_files(ctx: &Ctx, drive: &DriveInfo, plan: &BuildPlan, speed: &str, erase: bool) -> Result<Outcome, String> {
    let layer_break = plan.layer_break.map(|l| l.l0_sectors);
    let job = WriteJob { drive: drive.clone(), speed: speed.into(), erase, layer_break, volume: plan.label.clone(), image: "-==/\\/[BUILD IMAGE]\\/\\==-".into() };
    if !ctx.settings.write.verify {
        let io = &ctx.settings.io;
        return write(ctx, &job, &|| {
            Ok(Box::new(builder::stream_image(plan, io.ring_mib.max(1) as usize * (1 << 20), io.recovery_main_low, io.recovery_main_high, ctx.cancel.clone())))
        });
    }
    let tmp = std::env::temp_dir().join(format!("musadvd_studio_{}.iso", std::process::id()));
    ctx.log("Building the image as a temporary file first for verification …");
    let rec = Recorder::new(ctx, 1.0);
    let total = plan.total_sectors() as u64;
    let built = builder::build_to_file(plan, &tmp, Split::None, false, "none", &ctx.cancel, |f| rec.progress("building", f, total, None, "Building image …"));
    let result = match built {
        Ok(_) => write(ctx, &job, &|| FileSource::open(&tmp).map(|s| Box::new(s) as Box<dyn SectorSource>).map_err(|e| e.to_string())),
        Err(_) if ctx.cancelled() => Ok(Outcome::cancelled()),
        Err(e) => Err(e.to_string()),
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

// ---------------------------------------------------------------------------------------------
// Reading and verifying.

fn eject_if(ctx: &Ctx, drive: &DriveInfo, wanted: bool) {
    if wanted && !ctx.cancelled() {
        if let Err(e) = tray(drive, "eject") {
            ctx.log(format!("Eject failed: {e}"));
        }
    }
}

/// Blanks the CD-RW / DVD-RW in `drive`: `full` overwrites every sector, otherwise only the
/// lead-in/TOC (quick). The drive cannot abort a running BLANK, so the cancel flag is not polled.
pub fn erase(ctx: &Ctx, drive: &DriveInfo, full: bool) -> Result<(), String> {
    let t = transport(drive, &ctx.settings.io).map_err(|e| e.to_string())?;
    let _standby = ctx.settings.general.prevent_standby.then(power::StandbyGuard::new);
    let kind = if full { "Full" } else { "Quick" };
    ctx.log(format!("{kind} erase of the disc in {} started.", drive.name));
    let started = Instant::now();
    let message = format!("{kind} erase …");
    let emit = |fraction: f64| {
        (ctx.emit)(Event::Progress(Progress {
            phase: "erasing".into(),
            fraction,
            lba: 0,
            total: 0,
            speed_x: 0.0,
            ring: 0,
            device: None,
            eta_secs: remaining(started, fraction),
            message: message.clone(),
        }))
    };
    super::mmcwrite::blank_disc(&*t, full, &mut |f| emit(f as f64)).map_err(|e| e.to_string())?;
    ctx.log(format!("Disc erased in {} s.", started.elapsed().as_secs()));
    Ok(())
}

/// Reads the disc in `drive` into `out` (split and layout file from the settings).
pub fn read(ctx: &Ctx, drive: &DriveInfo, out: &Path) -> Result<Outcome, String> {
    let (s, r) = (&ctx.settings, &ctx.settings.read);
    if disc_css(drive, &s.io) == Some(true) {
        ctx.log("This disc is CSS protected. A copy as an image will not play from a blank disc.");
    }
    let details = drive_details(drive, &s.io).ok();
    let mut disc = open_disc(drive, &s.io).map_err(|e| e.to_string())?;
    let total = disc.sectors();
    let _lock = s.io.lock_tray.read.then(|| letter_of(drive).and_then(power::TrayLock::acquire).ok()).flatten();
    let _standby = s.general.prevent_standby.then(power::StandbyGuard::new);
    let opts = ReadOptions {
        sw_retries: r.sw_retries,
        ignore_errors: r.ignore_read_errors,
        split: Split::parse(&r.split),
        md5: s.general.md5_calculate,
        layout_file: r.layout_file.clone(),
        layer_break: details.as_ref().and_then(|d| d.layer_break),
        ..Default::default()
    };
    let rec = Recorder::new(ctx, one_x_for(total));
    let started = Instant::now();
    let result = reader::read_disc(&mut *disc, out, &opts, &ctx.cancel, &mut |p| {
        rec.feed(p.lba, 0, None);
        let f = p.lba as f64 / p.total.max(1) as f64;
        rec.progress("reading", f, p.total, remaining(started, f), &format!("Reading disc … ({} bad sectors)", p.bad));
    });
    let samples = rec.finish();
    match result {
        Ok(report) => {
            let graph = save_graph(
                ctx,
                GraphInfo {
                    kind: GraphKind::Read,
                    drive,
                    details: details.as_ref(),
                    media_physical: details.as_ref().map(|d| d.current_profile_name.clone()).unwrap_or_default(),
                    media_id: details.as_ref().and_then(|d| d.media_id.clone()).unwrap_or_default(),
                    speed: "max".into(),
                    volume: String::new(),
                    image: path_text(out),
                    sectors: total,
                    layer_break: opts.layer_break,
                    test_mode: false,
                    seconds: started.elapsed().as_secs(),
                },
                &samples,
                s.graph.sample_ms,
            );
            #[allow(clippy::drop_non_drop)] // the guard has a real Drop on Windows only
            drop(_lock);
            eject_if(ctx, drive, s.io.eject_after.read);
            let mut o = Outcome::ok(if report.bad.is_empty() {
                format!("{} sectors read.", report.sectors)
            } else {
                format!("{} sectors read, {} unreadable (filled with zeros).", report.sectors, report.bad.len())
            });
            o.files = report.files.iter().map(|p| path_text(p)).collect();
            o.md5 = report.md5;
            o.bad_sectors = report.bad.len() as u64;
            o.details = report.bad.iter().take(200).map(|l| format!("Sector {l} unreadable")).collect();
            o.graph = graph.map(|p| path_text(&p));
            o.layout_file = report.layout_file.map(|p| path_text(&p));
            Ok(o)
        }
        Err(ReadError::Cancelled) => Ok(Outcome::cancelled()),
        Err(e) => Err(e.to_string()),
    }
}

/// Compares the disc with `image`, or only checks that it can be read (`image` = `None`).
pub fn verify(ctx: &Ctx, drive: &DriveInfo, image: Option<&Path>) -> Result<Outcome, String> {
    let (s, v) = (&ctx.settings, &ctx.settings.verify);
    let details = drive_details(drive, &s.io).ok();
    let mut disc = open_disc(drive, &s.io).map_err(|e| e.to_string())?;
    let _lock = s.io.lock_tray.verify.then(|| letter_of(drive).and_then(power::TrayLock::acquire).ok()).flatten();
    let _standby = s.general.prevent_standby.then(power::StandbyGuard::new);
    let opts = VerifyOptions { sw_retries: v.sw_retries, ignore_miscompares: v.ignore_miscompares, md5: s.general.md5_calculate, ..Default::default() };
    let mut src = image.map(open_image).transpose()?.map(|(src, _)| src);
    let total = src.as_ref().map_or_else(|| disc.sectors(), |i| i.sectors());
    let rec = Recorder::new(ctx, one_x_for(total));
    let started = Instant::now();
    let mut progress = |f: f64| {
        rec.feed((f * total as f64) as u64, 0, None);
        rec.progress("verifying", f, total, remaining(started, f), "Verifying disc …");
    };
    let result = match src.as_mut() {
        Some(i) => verifier::verify(&mut *disc, i.as_mut(), &opts, &ctx.cancel, &mut progress),
        None => verifier::read_check(&mut *disc, &opts, &ctx.cancel, &mut progress),
    };
    let samples = rec.finish();
    let report = match result {
        Ok(r) => r,
        Err(VerifyError::Cancelled) => return Ok(Outcome::cancelled()),
        Err(e) => return Err(e.to_string()),
    };
    let graph = save_graph(
        ctx,
        GraphInfo {
            kind: GraphKind::Verify,
            drive,
            details: details.as_ref(),
            media_physical: details.as_ref().map(|d| d.current_profile_name.clone()).unwrap_or_default(),
            media_id: details.as_ref().and_then(|d| d.media_id.clone()).unwrap_or_default(),
            speed: "max".into(),
            volume: String::new(),
            image: image.map_or(String::new(), path_text),
            sectors: total,
            layer_break: None,
            test_mode: false,
            seconds: started.elapsed().as_secs(),
        },
        &samples,
        s.graph.sample_ms,
    );
    #[allow(clippy::drop_non_drop)] // the guard has a real Drop on Windows only
    drop(_lock);
    eject_if(ctx, drive, s.io.eject_after.verify);
    // Name the files of the first miscompares when the image is an ISO 9660 or UDF one.
    let names = image
        .and_then(|p| File::open(p).ok())
        .and_then(|f| isoread::DiscImage::open(f).ok())
        .and_then(|mut i| i.extent_map().ok())
        .map(|map| verifier::files_of(&report.miscompares, &map).into_iter().map(|(l, f)| (l, f.map(str::to_string))).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut details_out: Vec<String> = report
        .miscompares
        .iter()
        .take(200)
        .map(|l| match names.iter().find(|(n, _)| n == l).and_then(|(_, f)| f.as_ref()) {
            Some(f) => format!("Sector {l} differs ({f})"),
            None => format!("Sector {l} differs"),
        })
        .collect();
    details_out.extend(report.unreadable.iter().take(200).map(|l| format!("Sector {l} unreadable")));
    let bad = report.miscompare_count + report.unreadable.len() as u64;
    let mut o = if report.passed {
        Outcome::ok(if image.is_some() { "The disc matches the image." } else { "The disc is fully readable." })
    } else {
        Outcome { ok: false, message: format!("{} sectors differ, {} unreadable.", report.miscompare_count, report.unreadable.len()), ..Default::default() }
    };
    o.bad_sectors = bad;
    o.details = details_out;
    o.md5 = report.md5;
    o.graph = graph.map(|p| path_text(&p));
    Ok(o)
}

// ---------------------------------------------------------------------------------------------
// Building and after-completion actions.

/// Writes the planned image to `out` (split, MD5 and layout file from the settings).
pub fn build(ctx: &Ctx, plan: &BuildPlan, out: &Path) -> Result<Outcome, String> {
    let (s, b) = (&ctx.settings, &ctx.settings.build);
    let total = plan.total_sectors() as u64;
    let started = Instant::now();
    let rec = Recorder::new(ctx, 1.0);
    let built = builder::build_to_file(plan, out, Split::parse(&b.split), s.general.md5_calculate, &b.layout_file, &ctx.cancel, |f| {
        rec.progress("building", f, total, remaining(started, f), "Building image …");
    });
    match built {
        Ok(r) => {
            let mut o = Outcome::ok(format!("{} sectors ({:.2} GB) written.", r.sectors, r.sectors as f64 * 2048.0 / 1e9));
            o.files = r.files.iter().map(|p| path_text(p)).collect();
            o.md5 = r.md5;
            o.layout_file = r.layout_file.map(|p| path_text(&p));
            if b.log_skipped {
                o.details = plan.skipped.iter().map(|k| format!("skipped: {} ({})", k.path.display(), k.reason)).collect();
            }
            Ok(o)
        }
        Err(_) if ctx.cancelled() => Ok(Outcome::cancelled()),
        Err(e) => Err(e.to_string()),
    }
}

/// The settings' "when done" action (`shutdown`, `standby`, `hibernate`; `close` is the
/// window's business). Shutdown waits 60 s, `shutdown /a` takes it back.
pub fn on_complete(action: &str) {
    #[cfg(windows)]
    {
        use crate::tools::blocking_tool_command;
        let _ = match action {
            "shutdown" => blocking_tool_command("shutdown.exe").args(["/s", "/t", "60"]).spawn(),
            "hibernate" => blocking_tool_command("shutdown.exe").arg("/h").spawn(),
            "standby" => blocking_tool_command("rundll32.exe").args(["powrprof.dll,SetSuspendState", "0,1,0"]).spawn(),
            _ => return,
        };
    }
    #[cfg(not(windows))]
    let _ = action;
}

/// Media of the drive for the UI (details are optional: MMC may be switched off).
pub fn media_of(drive: &DriveInfo) -> Result<MediaInfo, String> {
    system_burner().probe_media(&drive.id).map_err(|e| e.to_string())
}

/// Used by the CLI to build the input of a plan from plain paths.
pub fn build_input(paths: &[PathBuf], label: &str) -> BuildInput {
    BuildInput {
        sources: paths.iter().map(|p| builder::Source { path: p.clone(), dest: String::new() }).collect(),
        label: label.into(),
        dvd_caps: Some(super::layerbreak::Capacities::DVD),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::preferred_speed;
    use crate::burner::MediaType;
    use crate::config::SpeedRule;

    fn drive(name: &str) -> DriveInfo {
        DriveInfo { id: "id".into(), name: name.into(), path: "F:\\".into() }
    }

    #[test]
    fn auto_speed_rules_match_media_then_drive_first_wins() {
        let rule = |m: &str, d: &str, s: &str| SpeedRule { media_id: m.into(), drive: d.into(), speed: s.into() };
        let s = BurnStudioSettings {
            auto_speed: vec![rule("CMC MAG", "", "8"), rule("", "pioneer", "4"), rule("", "", "2")],
            ..Default::default()
        };
        let d = drive("PIONEER BD-RW (F:)");
        assert_eq!(resolve_speed(&s, &d, "cmc mag-m01-00", "auto"), "8", "media ID first");
        assert_eq!(resolve_speed(&s, &d, "RITEK-S04-66", "auto"), "4", "then the drive");
        assert_eq!(resolve_speed(&s, &drive("LG"), "RITEK", "auto"), STUDIO_AUTO_SPEED, "an all-empty rule never matches");
        assert_eq!(resolve_speed(&s, &d, "CMC MAG", "12"), "12", "an explicit speed beats the rules");
        assert_eq!(resolve_speed(&BurnStudioSettings::default(), &d, "x", ""), STUDIO_AUTO_SPEED);
    }

    #[test]
    fn studio_auto_is_the_main_auto() {
        let x = |v: f64| (v * MediaType::Dvd5.sectors_per_sec_1x()).round() as u32;
        let s = BurnStudioSettings::default();
        let d = drive("DRIVE");
        let wish = resolve_speed(&s, &d, "ID", "auto");
        // DVD [16, 8, 6, 4, 2.4] -> 8x
        let speeds = [x(16.0), x(8.0), x(6.0), x(4.0), x(2.4)];
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, &wish), WriteSpeed::Specific(x(8.0)));
        assert_eq!(preferred_speed(MediaType::Dvd9, &speeds, &wish), WriteSpeed::Specific(x(4.0)));
        // DVD [16, 12] -> 12x (nothing up to 8x, so the slowest offered)
        assert_eq!(preferred_speed(MediaType::Dvd5, &[x(16.0), x(12.0)], &wish), WriteSpeed::Specific(x(12.0)));
        // Empty list -> the 8x target
        assert_eq!(preferred_speed(MediaType::Dvd5, &[], &wish), WriteSpeed::Specific(x(8.0)));
        // BD [12, 6, 4] -> 4x
        let bd_speeds = [26_341, 13_170, 8_780];
        assert_eq!(preferred_speed(MediaType::Bd25, &bd_speeds, &wish), WriteSpeed::Specific(8_780));
        // Wish max -> Max
        let wish_max = resolve_speed(&s, &d, "ID", "max");
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, &wish_max), WriteSpeed::Max);
        // A matching rule wins
        let rule_s = BurnStudioSettings {
            auto_speed: vec![SpeedRule { media_id: "ID".into(), drive: "".into(), speed: "4".into() }],
            ..Default::default()
        };
        let wish_rule = resolve_speed(&rule_s, &d, "ID", "auto");
        assert_eq!(preferred_speed(MediaType::Dvd5, &speeds, &wish_rule), WriteSpeed::Specific(x(4.0)));
    }

    #[test]
    fn images_open_through_their_sidecars() {
        let dir = std::env::temp_dir().join(format!("cx_studio_img_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("a.iso");
        std::fs::write(&iso, vec![0u8; 2048 * 10]).unwrap();
        let (src, lb) = open_image(&iso).unwrap();
        assert_eq!((src.sectors(), lb), (10, None));
        sidecar::write_dvd(&dir.join("a.dvd"), &sidecar::DvdLayout { layer_break: 6, image: "a.iso".into() }).unwrap();
        assert_eq!(open_image(&iso).unwrap().1, Some(6), "layout file next to the image");
        let (src, lb) = open_image(&dir.join("a.dvd")).unwrap();
        assert_eq!((src.sectors(), lb), (10, Some(6)));
        sidecar::write_cue(&dir.join("a.cue"), "a.iso", sidecar::CueMode::Mode1_2048).unwrap();
        assert_eq!(open_image(&dir.join("a.cue")).unwrap().0.sectors(), 10);
        assert!(open_image(&dir.join("fehlt.iso")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecars_reject_unsafe_paths_and_allow_local_and_subfolders() {
        let base = std::env::temp_dir().join(format!("cx_studio_sidecar_sec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        let sub = work.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let outside = base.join("outside.iso");
        let sibling = work.join("sibling.iso");
        let nested = sub.join("nested.iso");
        for p in [&outside, &sibling, &nested] {
            std::fs::write(p, vec![0u8; 2048 * 10]).unwrap();
        }
        let abs_outside = outside.to_string_lossy().into_owned();
        let bad_targets = [
            r"\\server\share\outside.iso",
            "//server/share/outside.iso",
            "../outside.iso",
            r"..\outside.iso",
            &abs_outside,
        ];

        // .cue
        sidecar::write_cue(&work.join("ok_sib.cue"), "sibling.iso", sidecar::CueMode::Mode1_2048).unwrap();
        sidecar::write_cue(&work.join("ok_sub.cue"), "sub/nested.iso", sidecar::CueMode::Mode1_2048).unwrap();
        assert_eq!(open_image(&work.join("ok_sib.cue")).unwrap().0.sectors(), 10);
        assert_eq!(open_image(&work.join("ok_sub.cue")).unwrap().0.sectors(), 10);
        for (i, target) in bad_targets.iter().enumerate() {
            let p = work.join(format!("bad_{i}.cue"));
            sidecar::write_cue(&p, target, sidecar::CueMode::Mode1_2048).unwrap();
            assert!(open_image(&p).is_err(), ".cue should reject {target:?}");
        }

        // .dvd
        sidecar::write_dvd(&work.join("ok_sib.dvd"), &sidecar::DvdLayout { layer_break: 5, image: "sibling.iso".into() }).unwrap();
        sidecar::write_dvd(&work.join("ok_sub.dvd"), &sidecar::DvdLayout { layer_break: 5, image: r"sub\nested.iso".into() }).unwrap();
        assert_eq!(open_image(&work.join("ok_sib.dvd")).unwrap().1, Some(5));
        assert_eq!(open_image(&work.join("ok_sub.dvd")).unwrap().1, Some(5));
        for (i, target) in bad_targets.iter().enumerate() {
            let p = work.join(format!("bad_{i}.dvd"));
            sidecar::write_dvd(&p, &sidecar::DvdLayout { layer_break: 5, image: (*target).into() }).unwrap();
            assert!(open_image(&p).is_err(), ".dvd should reject {target:?}");
        }

        // .mds
        sidecar::write_mds(&work.join("ok_sib.mds"), &sidecar::Mds { sectors: 10, layer_break: Some(5), image: "sibling.iso".into() }).unwrap();
        sidecar::write_mds(&work.join("ok_sub.mds"), &sidecar::Mds { sectors: 10, layer_break: Some(5), image: "sub/nested.iso".into() }).unwrap();
        assert_eq!(open_image(&work.join("ok_sib.mds")).unwrap().1, Some(5));
        assert_eq!(open_image(&work.join("ok_sub.mds")).unwrap().1, Some(5));
        for (i, target) in bad_targets.iter().enumerate() {
            let p = work.join(format!("bad_{i}.mds"));
            sidecar::write_mds(&p, &sidecar::Mds { sectors: 10, layer_break: Some(5), image: (*target).into() }).unwrap();
            assert!(open_image(&p).is_err(), ".mds should reject {target:?}");
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_without_a_drive_is_an_error_not_a_panic() {
        // Whatever the host has: an unknown drive id must fail cleanly (or find no media).
        let ctx = Ctx { settings: Default::default(), cancel: Arc::default(), emit: Arc::new(|_| {}) };
        let job = WriteJob { drive: drive("x"), speed: "auto".into(), erase: false, layer_break: None, volume: String::new(), image: String::new() };
        assert!(write(&ctx, &job, &|| Ok(Box::new(ZeroSource(1)))).is_err());
    }

    /// Answers READ CAPACITY only (last LBA `last`), like a drive behind SG_IO or SPTI.
    struct CapacityDrive(u32);

    impl ScsiTransport for CapacityDrive {
        fn exec(&self, cdb: &[u8], _dir: super::super::spti::DataDir, buf: &mut [u8], _t: u32) -> Result<super::super::spti::ScsiResult, BurnError> {
            assert_eq!(cdb[0], 0x25, "only READ CAPACITY expected");
            buf[..8].copy_from_slice(&[self.0.to_be_bytes(), 2048u32.to_be_bytes()].concat());
            Ok(super::super::spti::ScsiResult { transferred: 8, ..super::super::spti::ScsiResult::GOOD })
        }
    }

    /// The Read/Verify path on Linux: the device node sized by READ CAPACITY, then verified
    /// against the image it was written from (and a one-sector mismatch found).
    #[test]
    fn disc_opened_through_a_transport_verifies_against_its_image() {
        let dir = std::env::temp_dir().join(format!("musadvd_open_disc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (dev, iso) = (dir.join("sr0"), dir.join("image.iso"));
        let data: Vec<u8> = (0..20 * 2048).map(|i| (i / 2048) as u8).collect();
        std::fs::write(&dev, [&data[..], &[0xEE; 4096]].concat()).unwrap(); // lead-out padding past the capacity
        std::fs::write(&iso, &data).unwrap();
        let mut disc = open_disc_with(&CapacityDrive(19), dev.to_str().unwrap()).unwrap();
        assert_eq!(disc.sectors(), 20);
        let (mut image, _) = open_image(&iso).unwrap();
        let cancel = AtomicBool::new(false);
        let r = verifier::verify(&mut *disc, image.as_mut(), &VerifyOptions::default(), &cancel, &mut |_| {}).unwrap();
        assert!(r.passed);
        let mut bad = data.clone();
        bad[7 * 2048] ^= 1;
        std::fs::write(&iso, &bad).unwrap();
        let (mut image, _) = open_image(&iso).unwrap();
        let r = verifier::verify(&mut *disc, image.as_mut(), &VerifyOptions::default(), &cancel, &mut |_| {}).unwrap();
        assert_eq!((r.passed, r.miscompares.clone()), (false, vec![7]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
