//! Direct write engine ("SPTI Direkt", `io.backend = direct`): our own MMC write sequence over a
//! [`ScsiTransport`] instead of IMAPI's `IDiscFormat2Data`. The engine is generic over the
//! transport, so it is tested with a scripted fake drive; nothing in the automated tests touches a
//! real drive. Not exercised on real hardware yet (STATUS §5.3).
//!
//! Sequence: lock tray → GET CONFIGURATION (profile) → READ DISC INFORMATION / READ FORMAT
//! CAPACITIES (blank? capacity, overburn policy) → optional FORMAT UNIT (random-writable media)
//! → MODE SENSE/SELECT page 05 (DVD-R/-RW: write type, BUFE, test write, link size) → SET
//! STREAMING (speed) → optional OPC → layer break (SEND DISC STRUCTURE) → optional RESERVE TRACK →
//! WRITE(10) loop from the ring buffer (retries on recoverable sense, READ BUFFER CAPACITY for the
//! device buffer) → SYNCHRONIZE CACHE → CLOSE TRACK/SESSION → optional verify (READ(10)) → allow
//! removal → optional eject.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{sleep, JoinHandle};
use std::time::{Duration, Instant};

use super::ringbuf::{RingReader, RingStats};
use super::source::{spawn_ring_feed, SectorSource};
use super::spti::{self, DataDir, ScsiResult, ScsiTransport};
use super::{BurnError, BurnOptions, BurnPhase, BurnProgress, Telemetry};
use crate::config::{StudioIo, StudioWrite};

const S: usize = 2048;

/// What to do with an image larger than the disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overburn {
    /// Refuse; the UI asks the user and starts again with another policy.
    Prompt,
    /// Write past the nominal capacity (works only as far as the drive and disc allow).
    Overburn,
    /// Refuse with an explanation: cutting a file system image makes no sense.
    Truncate,
}

impl Overburn {
    pub fn parse(s: &str) -> Self {
        match s {
            "overburn" => Overburn::Overburn,
            "truncate" => Overburn::Truncate,
            _ => Overburn::Prompt,
        }
    }
}

/// Settings of the direct engine that `BurnOptions` does not carry (verify, eject, tray lock,
/// ring buffer, layer break and telemetry come from there).
#[derive(Debug, Clone)]
pub struct DirectOptions {
    /// SET STREAMING write speed in kB/s; `None` = leave the drive's default (maximum).
    pub speed_kbps: Option<u32>,
    /// Test write (DVD-R/-RW only): everything but the laser stays on.
    pub test_mode: bool,
    /// DVD-R/-RW: incremental instead of disc-at-once.
    pub incremental: bool,
    /// Mode page 05 link size, 0 = drive default.
    pub link_size: u8,
    pub opc: bool,
    pub reserve_track: bool,
    /// FORMAT UNIT before writing (BD-RE, DVD+RW, DVD-RW restricted overwrite).
    pub format: bool,
    pub prefer_no_spare: bool,
    pub overburn: Overburn,
    /// Sectors per WRITE(10), 0 = 32 (64 KiB).
    pub transfer_sectors: u32,
    /// Repeats of a command that fails with recoverable sense.
    pub sw_retries: u32,
    /// Pause between repeats and while the device buffer recovers.
    pub retry_pause: Duration,
    /// Poll READ BUFFER CAPACITY at most this often (telemetry / device recovery only).
    pub sample: Duration,
    /// Device buffer fill below which writing waits for the ring buffer (0 = off).
    pub device_low: u32,
    /// Lock the volume of the drive letter (SPTI) so Windows leaves the disc alone.
    pub lock_volume: bool,
}

impl Default for DirectOptions {
    fn default() -> Self {
        DirectOptions {
            speed_kbps: None,
            test_mode: false,
            incremental: false,
            link_size: 0,
            opc: false,
            reserve_track: false,
            format: false,
            prefer_no_spare: true,
            overburn: Overburn::Prompt,
            transfer_sectors: 0,
            sw_retries: 20,
            retry_pause: Duration::from_millis(500),
            sample: Duration::from_millis(100),
            device_low: 0,
            lock_volume: true,
        }
    }
}

impl DirectOptions {
    pub fn from_settings(w: &StudioWrite, io: &StudioIo) -> Self {
        DirectOptions {
            test_mode: w.test_mode,
            incremental: w.write_type == "incremental",
            link_size: w.link_size.min(255) as u8,
            opc: w.opc,
            reserve_track: w.dvdpr_reserve_track,
            prefer_no_spare: w.prefer_no_spare,
            overburn: Overburn::parse(&w.overburn),
            transfer_sectors: io.transfer_kib / 2,
            sw_retries: w.sw_retries,
            device_low: io.recovery_device_low,
            lock_volume: w.lock_volume,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    DvdPlusR,
    DvdPlusRw,
    /// DVD-R / DVD-RW sequential / DVD-R DL.
    DvdMinus,
    /// DVD-RW restricted overwrite.
    DvdRwRo,
    BdR,
    BdRe,
}

impl Kind {
    fn of(profile: u16) -> Result<Kind, BurnError> {
        Ok(match profile {
            0x0000 => return Err(BurnError::NoMedia),
            0x001B | 0x002B => Kind::DvdPlusR,
            0x001A | 0x002A => Kind::DvdPlusRw,
            0x0011 | 0x0014 | 0x0015 | 0x0016 => Kind::DvdMinus,
            0x0013 => Kind::DvdRwRo,
            0x0041 | 0x0042 => Kind::BdR,
            0x0043 => Kind::BdRe,
            p => return Err(BurnError::Unsupported(format!("{} wird von SPTI Direkt nicht beschrieben", spti::profile_name(p)))),
        })
    }

    fn random_writable(self) -> bool {
        matches!(self, Kind::DvdPlusRw | Kind::DvdRwRo | Kind::BdRe)
    }
}

/// READ FORMAT CAPACITIES: (blocks, format type) of every descriptor; the first one is the
/// current or maximum capacity.
fn format_capacities(b: &[u8]) -> Vec<(u32, u8)> {
    let n = (*b.get(3).unwrap_or(&0) as usize).min(b.len().saturating_sub(4));
    b.get(4..4 + n).unwrap_or(&[]).chunks_exact(8).map(|d| (u32::from_be_bytes([d[0], d[1], d[2], d[3]]), d[4] >> 2)).collect()
}

/// The commands with retry, cancel and optional data.
struct Drive<'a> {
    t: &'a dyn ScsiTransport,
    o: &'a DirectOptions,
    cancel: &'a AtomicBool,
}

impl Drive<'_> {
    fn pause(&self) {
        if !self.o.retry_pause.is_zero() {
            sleep(self.o.retry_pause);
        }
    }

    /// Sends `cdb`, repeating it while the drive answers with recoverable sense.
    fn send(&self, what: &str, cdb: &[u8], dir: DataDir, buf: &mut [u8], timeout_s: u32) -> Result<ScsiResult, BurnError> {
        let mut tries = 0;
        loop {
            let r = self.t.exec(cdb, dir, buf, timeout_s)?;
            if r.ok() {
                return Ok(r);
            }
            if tries < self.o.sw_retries && r.sense.is_some_and(|s| s.recoverable()) {
                tries += 1;
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(BurnError::Cancelled);
                }
                self.pause();
                continue;
            }
            return r.into_result(what);
        }
    }

    fn run(&self, what: &str, cdb: &[u8]) -> Result<(), BurnError> {
        self.send(what, cdb, DataDir::None, &mut [], 30).map(|_| ())
    }

    /// A command that may take minutes (sync cache, close).
    fn run_long(&self, what: &str, cdb: &[u8]) -> Result<(), BurnError> {
        self.send(what, cdb, DataDir::None, &mut [], 1800).map(|_| ())
    }

    fn query(&self, what: &str, cdb: &[u8], alloc: usize) -> Result<Vec<u8>, BurnError> {
        let mut buf = vec![0u8; alloc];
        let r = self.send(what, cdb, DataDir::FromDevice, &mut buf, 30)?;
        buf.truncate(if r.transferred == 0 { alloc } else { r.transferred.min(alloc) });
        Ok(buf)
    }

    /// Device buffer fill in percent.
    fn device_percent(&self) -> Option<u32> {
        let mut buf = [0u8; 12];
        self.t.exec(&spti::read_buffer_capacity(12), DataDir::FromDevice, &mut buf, 10).ok().filter(|r| r.ok())?;
        let (len, free) = spti::parse_buffer_capacity(&buf)?;
        (len > 0).then(|| (len.saturating_sub(free) as u64 * 100 / len as u64) as u32)
    }

    /// Waits until the drive is ready again (after FORMAT UNIT).
    fn wait_ready(&self) -> Result<(), BurnError> {
        let start = Instant::now();
        loop {
            let r = self.t.exec(&spti::test_unit_ready(), DataDir::None, &mut [], 30)?;
            if r.ok() {
                return Ok(());
            }
            if !r.sense.is_some_and(|s| s.recoverable()) || start.elapsed() > Duration::from_secs(4 * 3600) {
                return r.into_result("Formatieren").map(|_| ());
            }
            if self.cancel.load(Ordering::Relaxed) {
                return Err(BurnError::Cancelled);
            }
            sleep(Duration::from_secs(1));
        }
    }
}

/// Where the sectors come from while writing: the source itself, or a ring filled by a thread.
struct Feed {
    reader: Option<RingReader>,
    source: Option<Box<dyn SectorSource>>,
    producer: Option<JoinHandle<Result<Box<dyn SectorSource>, String>>>,
    stats: Option<RingStats>,
}

impl Feed {
    fn new(source: Box<dyn SectorSource>, options: &BurnOptions) -> Feed {
        if let Some(stats) = source.ring_stats() {
            return Feed { reader: None, source: Some(source), producer: None, stats: Some(stats) }; // built on the fly: already buffered
        }
        let mib = if options.ring_mib == 0 { 40 } else { options.ring_mib } as usize;
        let f = spawn_ring_feed(source, mib << 20, options.recovery_low, options.recovery_high);
        Feed { reader: Some(f.reader), source: None, producer: Some(f.producer), stats: Some(f.stats) }
    }

    fn fill(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        match (&mut self.reader, &mut self.source) {
            (Some(r), _) => r.read_exact(buf),
            (None, Some(s)) => s.read(lba, buf),
            (None, None) => Err(io::Error::other("Quelle nicht mehr verfügbar")),
        }
    }

    /// Hands the source back (to verify against it); a failed producer explains a short stream.
    fn finish(&mut self) -> Result<Box<dyn SectorSource>, BurnError> {
        self.reader = None;
        if let Some(h) = self.producer.take() {
            return h.join().map_err(|_| BurnError::Image("Abbruch im Bildlieferanten".into()))?.map_err(BurnError::Image);
        }
        self.source.take().ok_or_else(|| BurnError::Image("Bildquelle nicht mehr verfügbar".into()))
    }

    fn fail(&mut self, e: io::Error, cancel: &AtomicBool) -> BurnError {
        if cancel.load(Ordering::Relaxed) {
            return BurnError::Cancelled;
        }
        self.reader = None;
        match self.producer.take().map(JoinHandle::join) {
            Some(Ok(Err(why))) => BurnError::Image(why),
            _ => BurnError::Io(e),
        }
    }
}

fn progress(tx: &Sender<BurnProgress>, phase: BurnPhase, fraction: f64, message: &str) {
    let _ = tx.send(BurnProgress::new(phase, fraction, message));
}

/// Burns `source` through `t`. Blocks until the disc is done; the tray is unlocked again on every
/// exit. Verification needs a rewindable source.
pub fn burn_direct_with(
    t: &dyn ScsiTransport,
    source: Box<dyn SectorSource>,
    options: &BurnOptions,
    direct: &DirectOptions,
    cancel: &AtomicBool,
    tx: &Sender<BurnProgress>,
) -> Result<(), BurnError> {
    let verify = options.verify && !direct.test_mode;
    if verify && !source.rewindable() {
        return Err(BurnError::Unsupported("Die Überprüfung braucht eine Quelle, die erneut gelesen werden kann".into()));
    }
    let drive = Drive { t, o: direct, cancel };
    progress(tx, BurnPhase::Preparing, 0.0, "Laufwerk wird vorbereitet …");
    if options.lock_tray {
        drive.run("Sperren der Disc", &spti::prevent_allow_medium_removal(true))?;
    }
    let result = write_disc(&drive, source, options, verify, tx);
    if options.lock_tray {
        let _ = drive.run("Entsperren der Disc", &spti::prevent_allow_medium_removal(false));
    }
    result?;
    if options.eject {
        let _ = drive.run("Auswerfen", &spti::start_stop_unit(true));
    }
    progress(tx, BurnPhase::Done, 1.0, "Fertig");
    Ok(())
}

fn write_disc(
    drive: &Drive<'_>,
    source: Box<dyn SectorSource>,
    options: &BurnOptions,
    verify: bool,
    tx: &Sender<BurnProgress>,
) -> Result<(), BurnError> {
    let (o, cancel) = (drive.o, drive.cancel);
    let sectors = source.sectors();
    if sectors == 0 || sectors > u32::MAX as u64 {
        return Err(BurnError::Image("leeres oder zu großes Image".into()));
    }
    let cfg = drive.query("GET CONFIGURATION", &spti::get_configuration(0, 0, 8), 8)?;
    let profile = cfg.get(6..8).map_or(0, |p| u16::from_be_bytes([p[0], p[1]]));
    let kind = Kind::of(profile)?;
    if o.test_mode && kind != Kind::DvdMinus {
        return Err(BurnError::Unsupported(format!("Testschreiben gibt es nur auf DVD-R/-RW, nicht auf {}", spti::profile_name(profile))));
    }
    let info = drive.query("READ DISC INFORMATION", &spti::read_disc_information(34), 34)?;
    let blank = spti::parse_disc_information(&info).is_some_and(|i| i.status == 0);
    if !kind.random_writable() && !blank {
        return Err(BurnError::Unsupported(format!("die {} ist nicht leer", spti::profile_name(profile))));
    }
    let caps = format_capacities(&drive.query("READ FORMAT CAPACITIES", &spti::read_format_capacities(252), 252)?);
    let capacity = caps.first().map_or(0, |c| c.0 as u64);
    if sectors > capacity {
        match o.overburn {
            Overburn::Overburn => progress(tx, BurnPhase::Preparing, 0.0, "Warnung: Das Image ist größer als die Disc (Overburn)"),
            Overburn::Truncate => {
                return Err(BurnError::Unsupported(
                    "Das Image ist größer als die Disc; ein Dateisystem-Image kann nicht abgeschnitten werden – bitte kleineres Image oder größere Disc wählen".into(),
                ))
            }
            Overburn::Prompt => return Err(BurnError::NotEnoughSpace { needed: sectors, free: capacity }),
        }
    }
    let mut feed = Feed::new(source, options);

    if o.format && kind.random_writable() {
        let ftype = match kind {
            Kind::BdRe if o.prefer_no_spare => 0x31,
            Kind::BdRe => 0x30,
            Kind::DvdPlusRw => 0x26,
            _ => 0x13,
        };
        let blocks = caps.iter().skip(1).find(|c| c.1 == ftype).map(|c| c.0).ok_or_else(|| {
            BurnError::Unsupported(format!("das Laufwerk bietet das Format {ftype:#04X} für diese Disc nicht an"))
        })?;
        progress(tx, BurnPhase::Formatting, 0.0, "Disc wird formatiert …");
        let mut list = spti::format_list(ftype, blocks);
        drive.send("FORMAT UNIT", &spti::format_unit(), DataDir::ToDevice, &mut list, 7200)?;
        drive.wait_ready()?;
    }
    if kind == Kind::DvdMinus {
        set_write_parameters(drive)?;
    }
    if let Some(kbps) = o.speed_kbps {
        let mut d = spti::streaming_descriptor((sectors - 1) as u32, kbps, kbps);
        if drive.send("SET STREAMING", &spti::set_streaming(), DataDir::ToDevice, &mut d, 30).is_err() {
            progress(tx, BurnPhase::Preparing, 0.0, "Warnung: Das Laufwerk übernimmt die Schreibgeschwindigkeit nicht");
        }
    }
    if o.opc && !kind.random_writable() && drive.run("OPC", &spti::send_opc_information(true)).is_err() {
        progress(tx, BurnPhase::Preparing, 0.0, "Warnung: OPC-Kalibrierung vom Laufwerk abgelehnt");
    }
    if let Some(l0) = options.layer_break {
        if l0 as u64 >= sectors {
            return Err(BurnError::Image("der Layerwechsel liegt hinter dem Ende des Images".into()));
        }
        // shortcut: formats and the parameter layout follow MMC-5 but are unverified on real drives.
        let format = match profile {
            0x002B => Some(0x20),
            0x0015 => Some(0x23),
            0x0016 => Some(0x22),
            _ => None,
        };
        if let Some(f) = format {
            let mut p = [0u8; 12];
            p[1] = 10;
            p[8..12].copy_from_slice(&l0.to_be_bytes());
            drive.send("Layerwechsel", &spti::send_disc_structure(f, 12), DataDir::ToDevice, &mut p, 30)?;
        }
    }
    if o.reserve_track && (matches!(kind, Kind::DvdPlusR | Kind::BdR) || (kind == Kind::DvdMinus && o.incremental)) {
        drive.run("RESERVE TRACK", &spti::reserve_track(sectors as u32))?;
    }

    // WRITE(10) loop.
    let chunk = if o.transfer_sectors == 0 { 32 } else { o.transfer_sectors.min(64) } as u64;
    let mut buf = vec![0u8; chunk as usize * S];
    let (mut lba, mut last_permille, mut last_sample) = (0u64, u64::MAX, None::<Instant>);
    let sample_wanted = options.telemetry.is_some() || o.device_low > 0;
    let text = if o.test_mode { "Testschreiben …" } else { "Daten werden geschrieben …" };
    while lba < sectors {
        if cancel.load(Ordering::Relaxed) {
            let _ = drive.run_long("SYNCHRONIZE CACHE", &spti::synchronize_cache(false));
            return Err(BurnError::Cancelled);
        }
        let n = chunk.min(sectors - lba);
        let data = &mut buf[..n as usize * S];
        if let Err(e) = feed.fill(lba, data) {
            let _ = drive.run_long("SYNCHRONIZE CACHE", &spti::synchronize_cache(false));
            return Err(feed.fail(e, cancel));
        }
        drive
            .send(&format!("Schreibfehler bei Sektor {lba}"), &spti::write10(lba as u32, n as u16), DataDir::ToDevice, data, 60)
            .map_err(|e| if cancel.load(Ordering::Relaxed) { BurnError::Cancelled } else { e })?;
        lba += n;
        if sample_wanted && last_sample.is_none_or(|t| t.elapsed() >= o.sample) {
            last_sample = Some(Instant::now());
            let device = drive.device_percent();
            if let (Some(p), Some(stats)) = (device, &feed.stats) {
                // The device buffer ran low: give the ring buffer time to refill.
                let mut waited = 0;
                while o.device_low > 0 && p < o.device_low && stats.percent() < options.recovery_high && waited < 50 && !cancel.load(Ordering::Relaxed) {
                    drive.pause();
                    waited += 1;
                }
            }
            if let Some(hook) = &options.telemetry {
                (hook.0)(Telemetry { lba, ring_percent: feed.stats.as_ref().map_or(0, RingStats::percent), device_percent: device });
            }
        }
        let permille = lba * 1000 / sectors;
        if permille != last_permille {
            last_permille = permille;
            progress(tx, BurnPhase::Writing, lba as f64 / sectors as f64, text);
        }
    }

    progress(tx, BurnPhase::Finalizing, 0.0, "Disc wird abgeschlossen …");
    drive.run_long("SYNCHRONIZE CACHE", &spti::synchronize_cache(false))?;
    if !o.test_mode {
        // DVD-R/-RW in disc-at-once mode closes itself with the cache flush.
        let (track, session) = match kind {
            Kind::DvdPlusR | Kind::BdR => (true, true),
            Kind::DvdMinus => (o.incremental, o.incremental),
            Kind::DvdPlusRw => (false, true),
            Kind::DvdRwRo | Kind::BdRe => (false, false),
        };
        // shortcut: track number 1 for the track close is the MMC default but unverified on real drives.
        if track {
            drive.run_long("CLOSE TRACK", &spti::close_track_session(1, 1, false))?;
        }
        if session {
            drive.run_long("CLOSE SESSION", &spti::close_track_session(2, 0, false))?;
        }
    }
    if verify {
        let src = feed.finish()?;
        verify_disc(drive, src, sectors, tx)?;
    }
    Ok(())
}

/// DVD-R/-RW mode page 05: write type, BUFE, test write, link size; multi-session off.
fn set_write_parameters(drive: &Drive<'_>) -> Result<(), BurnError> {
    let mut b = drive.query("MODE SENSE", &spti::mode_sense10(0x05, 64), 64)?;
    let at = 8 + b.get(6..8).map_or(0, |d| u16::from_be_bytes([d[0], d[1]]) as usize);
    if b.len() < at + 14 {
        return Err(BurnError::Backend("MODE SENSE: Schreibparameter-Seite zu kurz".into()));
    }
    b[0] = 0;
    b[1] = 0; // mode data length is reserved in MODE SELECT
    b[at] &= 0x3F; // PS bit
    b[at + 2] = 0x40 | if drive.o.test_mode { 0x10 } else { 0 } | if drive.o.incremental { 0 } else { 2 };
    b[at + 3] &= 0x0F;
    if drive.o.link_size > 0 {
        b[at + 5] = drive.o.link_size;
    }
    let len = b.len();
    drive.send("MODE SELECT", &spti::mode_select10(len as u16), DataDir::ToDevice, &mut b, 30)?;
    Ok(())
}

/// Reads the disc with READ(10) and compares it with the (re-read) source.
fn verify_disc(drive: &Drive<'_>, mut src: Box<dyn SectorSource>, sectors: u64, tx: &Sender<BurnProgress>) -> Result<(), BurnError> {
    progress(tx, BurnPhase::Verifying, 0.0, "Disc wird überprüft …");
    let chunk = 32u64;
    let (mut disc, mut want) = (vec![0u8; chunk as usize * S], vec![0u8; chunk as usize * S]);
    let mut lba = 0;
    while lba < sectors {
        if drive.cancel.load(Ordering::Relaxed) {
            return Err(BurnError::Cancelled);
        }
        let n = chunk.min(sectors - lba) as usize;
        drive.send(&format!("Lesefehler bei Sektor {lba}"), &spti::read10(lba as u32, n as u16), DataDir::FromDevice, &mut disc[..n * S], 60)?;
        src.read(lba, &mut want[..n * S]).map_err(|e| BurnError::Image(format!("Quelle nicht lesbar: {e}")))?;
        if let Some(i) = (0..n).find(|&i| disc[i * S..(i + 1) * S] != want[i * S..(i + 1) * S]) {
            return Err(BurnError::VerifyFailed { sector: lba + i as u64 });
        }
        lba += n as u64;
        progress(tx, BurnPhase::Verifying, lba as f64 / sectors as f64, "Disc wird überprüft …");
    }
    Ok(())
}

/// Opens the transport for `drive_id` and burns directly (Windows). `spti_transport`: SPTI
/// (`\\.\X:`) instead of IMAPI's `IDiscRecorder2Ex`, which holds exclusive access meanwhile.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub fn burn_direct(
    drive_id: &str,
    spti_transport: bool,
    source: Box<dyn SectorSource>,
    speed: super::WriteSpeed,
    options: &BurnOptions,
    direct: &DirectOptions,
    cancel: &AtomicBool,
    tx: &Sender<BurnProgress>,
) -> Result<(), BurnError> {
    use super::power::{DriveHandle, StandbyGuard};
    use super::spti::{ImapiExTransport, SptiTransport};
    use super::windows::{recorder, volume_paths, Com};

    let _standby = options.prevent_standby.then(StandbyGuard::new);
    let mut direct = direct.clone();
    if let (None, super::WriteSpeed::Specific(s)) = (direct.speed_kbps, speed) {
        direct.speed_kbps = Some(s * 2048 / 1000); // sectors/s → kB/s
    }
    if spti_transport {
        let letter = {
            let _com = Com::init()?;
            volume_paths(&recorder(drive_id)?).into_iter().next().and_then(|p| super::power::drive_letter(&p))
        }
        .ok_or_else(|| BurnError::Backend("Das Laufwerk hat keinen Laufwerksbuchstaben".into()))?;
        let t = SptiTransport::open(letter)?;
        let _volume = if direct.lock_volume { DriveHandle::open(letter, true).ok().filter(|h| h.lock_volume().is_ok()) } else { None };
        burn_direct_with(&t, source, options, &direct, cancel, tx)
    } else {
        let t = ImapiExTransport::open(drive_id)?;
        t.acquire_exclusive("convertx-rust")?;
        let r = burn_direct_with(&t, source, options, &direct, cancel, tx);
        t.release_exclusive();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::source::fake::FakeDisc;
    use crate::burner::spti::Sense;
    use crate::burner::TelemetryHook;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    /// Scripted drive: answers by opcode, records every CDB, keeps what was written.
    struct Fake<'a> {
        profile: u16,
        disc_status: u8,
        capacity: u32,
        /// (blocks, format type) offered by READ FORMAT CAPACITIES after the capacity descriptor.
        formats: Vec<(u32, u8)>,
        sent: RefCell<Vec<Vec<u8>>>,
        write_errors: RefCell<VecDeque<Sense>>,
        /// Raise the cancel flag after this many WRITE(10).
        cancel_after: Option<(usize, &'a AtomicBool)>,
        written: RefCell<Vec<u8>>,
        mode_select: RefCell<Vec<u8>>,
        format_list: RefCell<Vec<u8>>,
        layer_param: RefCell<Vec<u8>>,
        corrupt_read: Option<usize>,
    }

    impl<'a> Fake<'a> {
        fn new(profile: u16) -> Self {
            Fake {
                profile,
                disc_status: 0,
                capacity: 2_000_000,
                formats: vec![],
                sent: RefCell::new(vec![]),
                write_errors: RefCell::new(VecDeque::new()),
                cancel_after: None,
                written: RefCell::new(vec![]),
                mode_select: RefCell::new(vec![]),
                format_list: RefCell::new(vec![]),
                layer_param: RefCell::new(vec![]),
                corrupt_read: None,
            }
        }

        fn ops(&self) -> Vec<u8> {
            self.sent.borrow().iter().map(|c| c[0]).collect()
        }

        fn writes(&self) -> Vec<(u32, u16)> {
            self.sent.borrow().iter().filter(|c| c[0] == 0x2A).map(|c| (u32::from_be_bytes(c[2..6].try_into().unwrap()), u16::from_be_bytes([c[7], c[8]]))).collect()
        }
    }

    fn check(sense: Sense) -> ScsiResult {
        ScsiResult { status: 2, sense: Some(sense), transferred: 0 }
    }

    impl ScsiTransport for Fake<'_> {
        fn exec(&self, cdb: &[u8], _dir: DataDir, buf: &mut [u8], _t: u32) -> Result<ScsiResult, BurnError> {
            self.sent.borrow_mut().push(cdb.to_vec());
            let data: Vec<u8> = match cdb[0] {
                0x46 => {
                    let mut b = vec![0u8; 8];
                    b[3] = 4;
                    b[6..8].copy_from_slice(&self.profile.to_be_bytes());
                    b
                }
                0x51 => {
                    let mut b = vec![0u8; 34];
                    b[2] = self.disc_status;
                    b
                }
                0x23 => {
                    let mut b = vec![0u8; 4];
                    let mut d = vec![(self.capacity, 0x02u8)];
                    d.extend(self.formats.iter().map(|&(n, t)| (n, t << 2)));
                    for (n, t) in d {
                        b.extend(n.to_be_bytes());
                        b.extend([t, 0, 0, 0]);
                    }
                    b[3] = (b.len() - 4) as u8;
                    b
                }
                0x5A => {
                    let mut b = vec![0u8; 8 + 2 + 50];
                    b[8] = 0x85; // page 05 with the PS bit
                    b[9] = 0x32;
                    b[12] = 0x08;
                    b
                }
                0x5C => vec![0, 10, 0, 0, 0, 0x10, 0, 0, 0, 0x08, 0, 0], // 1 MiB, half free
                0x55 => {
                    *self.mode_select.borrow_mut() = buf.to_vec();
                    vec![]
                }
                0x04 => {
                    *self.format_list.borrow_mut() = buf.to_vec();
                    vec![]
                }
                0xBF => {
                    *self.layer_param.borrow_mut() = buf.to_vec();
                    vec![]
                }
                0x2A => {
                    if let Some(s) = self.write_errors.borrow_mut().pop_front() {
                        return Ok(check(s));
                    }
                    self.written.borrow_mut().extend_from_slice(buf);
                    if let Some((n, flag)) = self.cancel_after {
                        if self.writes().len() >= n {
                            flag.store(true, Ordering::SeqCst);
                        }
                    }
                    vec![]
                }
                0x28 => {
                    let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap()) as usize;
                    let w = self.written.borrow();
                    let end = (lba * S + buf.len()).min(w.len());
                    let part = &w[lba * S..end];
                    buf[..part.len()].copy_from_slice(part);
                    if self.corrupt_read.is_some_and(|s| s >= lba && s < lba + buf.len() / S) {
                        buf[(self.corrupt_read.unwrap() - lba) * S + 7] ^= 0xFF;
                    }
                    return Ok(ScsiResult { status: 0, sense: None, transferred: buf.len() });
                }
                _ => vec![],
            };
            let n = data.len().min(buf.len());
            if cdb[0] != 0x55 && cdb[0] != 0x04 && cdb[0] != 0xBF && cdb[0] != 0x2A {
                buf[..n].copy_from_slice(&data[..n]);
            }
            Ok(ScsiResult { status: 0, sense: None, transferred: n })
        }
    }

    fn opts() -> (BurnOptions, DirectOptions) {
        let b = BurnOptions { lock_tray: true, ring_mib: 1, ..BurnOptions::default() };
        let d = DirectOptions { retry_pause: Duration::ZERO, transfer_sectors: 32, ..DirectOptions::default() };
        (b, d)
    }

    fn burn(f: &Fake, sectors: u64, b: &BurnOptions, d: &DirectOptions, cancel: &AtomicBool) -> Result<(), BurnError> {
        let (tx, _rx) = channel();
        burn_direct_with(f, Box::new(FakeDisc::pattern(sectors)), b, d, cancel, &tx)
    }

    #[test]
    fn dvd_plus_r_sequence() {
        let f = Fake::new(0x1B);
        let (b, d) = opts();
        let d = DirectOptions { speed_kbps: Some(11080), opc: true, reserve_track: true, ..d };
        burn(&f, 100, &b, &d, &AtomicBool::new(false)).unwrap();
        // lock, config, disc info, capacities, streaming, OPC, reserve, 4 writes, sync, close track + session, unlock
        assert_eq!(f.ops(), [0x1E, 0x46, 0x51, 0x23, 0xB6, 0x54, 0x53, 0x2A, 0x2A, 0x2A, 0x2A, 0x35, 0x5B, 0x5B, 0x1E]);
        assert_eq!(f.writes(), [(0, 32), (32, 32), (64, 32), (96, 4)]);
        let sent = f.sent.borrow();
        assert_eq!(sent[0], spti::prevent_allow_medium_removal(true));
        assert_eq!(sent[6], spti::reserve_track(100));
        assert_eq!(sent[12], spti::close_track_session(1, 1, false));
        assert_eq!(sent[13], spti::close_track_session(2, 0, false));
        assert_eq!(sent[14], spti::prevent_allow_medium_removal(false));
        let expected: Vec<u8> = FakeDisc::pattern(100).data;
        assert_eq!(*f.written.borrow(), expected, "every sector arrives in order");
    }

    #[test]
    fn dvd_r_test_mode_sets_page_05_and_does_not_close() {
        let f = Fake::new(0x11);
        let (b, d) = opts();
        let d = DirectOptions { test_mode: true, link_size: 16, ..d };
        burn(&f, 50, &b, &d, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.ops(), [0x1E, 0x46, 0x51, 0x23, 0x5A, 0x55, 0x2A, 0x2A, 0x35, 0x1E]);
        let page = f.mode_select.borrow();
        assert_eq!(page.len(), 60);
        assert_eq!(&page[0..2], [0, 0], "mode data length cleared");
        assert_eq!(page[8], 0x05, "PS bit cleared");
        assert_eq!(page[10], 0x40 | 0x10 | 0x02, "BUFE + test write + disc-at-once");
        assert_eq!(page[13], 16, "link size");
    }

    #[test]
    fn dvd_r_incremental_closes_track_and_session() {
        let f = Fake::new(0x11);
        let (b, d) = opts();
        burn(&f, 10, &b, &DirectOptions { incremental: true, reserve_track: true, ..d }, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.ops(), [0x1E, 0x46, 0x51, 0x23, 0x5A, 0x55, 0x53, 0x2A, 0x35, 0x5B, 0x5B, 0x1E]);
        assert_eq!(f.mode_select.borrow()[10], 0x40, "incremental write type 0");
    }

    #[test]
    fn test_write_is_refused_on_other_media() {
        let f = Fake::new(0x1B);
        let (b, d) = opts();
        let e = burn(&f, 10, &b, &DirectOptions { test_mode: true, ..d }, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, BurnError::Unsupported(_)), "{e}");
        assert_eq!(f.ops(), [0x1E, 0x46, 0x1E], "nothing but lock, query and unlock");
    }

    #[test]
    fn bd_re_is_formatted_without_spare_areas() {
        let mut f = Fake::new(0x43);
        f.disc_status = 2;
        f.formats = vec![(11_826_176, 0x30), (11_900_000, 0x31)];
        let (b, d) = opts();
        burn(&f, 40, &b, &DirectOptions { format: true, ..d.clone() }, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.ops(), [0x1E, 0x46, 0x51, 0x23, 0x04, 0x00, 0x2A, 0x2A, 0x35, 0x1E], "format, wait for ready, write, no close");
        let list = f.format_list.borrow();
        assert_eq!(list[8], 0x31 << 2);
        assert_eq!(&list[4..8], 11_900_000u32.to_be_bytes());
        // With spare areas when the user does not prefer none.
        let f = Fake { formats: vec![(11_826_176, 0x30), (11_900_000, 0x31)], ..Fake::new(0x43) };
        burn(&f, 40, &b, &DirectOptions { format: true, prefer_no_spare: false, ..d.clone() }, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.format_list.borrow()[8], 0x30 << 2);
        // A drive that does not offer the format is an error before anything is written.
        let f = Fake::new(0x43);
        assert!(burn(&f, 40, &b, &DirectOptions { format: true, ..d }, &AtomicBool::new(false)).is_err());
        assert!(f.writes().is_empty());
    }

    #[test]
    fn non_blank_write_once_media_is_refused() {
        let mut f = Fake::new(0x1B);
        f.disc_status = 2;
        let (b, d) = opts();
        assert!(matches!(burn(&f, 10, &b, &d, &AtomicBool::new(false)), Err(BurnError::Unsupported(_))));
        assert!(f.writes().is_empty());
    }

    #[test]
    fn cancel_mid_write_flushes_and_unlocks() {
        let cancel = AtomicBool::new(false);
        let mut f = Fake::new(0x1B);
        f.cancel_after = Some((2, &cancel));
        let (b, d) = opts();
        let e = burn(&f, 200, &b, &d, &cancel).unwrap_err();
        assert!(matches!(e, BurnError::Cancelled), "{e}");
        assert_eq!(f.ops(), [0x1E, 0x46, 0x51, 0x23, 0x2A, 0x2A, 0x35, 0x1E], "two chunks, cache flush, no close, tray unlocked");
    }

    #[test]
    fn recoverable_sense_is_retried_and_hard_errors_stop() {
        let not_ready = Sense { key: 2, asc: 0x04, ascq: 0x08 };
        let f = Fake::new(0x1B);
        f.write_errors.borrow_mut().extend([not_ready, not_ready]);
        let (b, d) = opts();
        burn(&f, 40, &b, &d, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.writes(), [(0, 32), (0, 32), (0, 32), (32, 8)], "the first chunk is sent three times");
        assert_eq!(f.written.borrow().len(), 40 * S, "and lands once");
        // Retries are limited.
        let f = Fake::new(0x1B);
        f.write_errors.borrow_mut().extend([not_ready, not_ready]);
        let e = burn(&f, 40, &b, &DirectOptions { sw_retries: 1, ..d.clone() }, &AtomicBool::new(false)).unwrap_err();
        assert!(e.to_string().contains("Sektor 0"), "{e}");
        assert_eq!(f.ops().last(), Some(&0x1E), "tray unlocked after the error");
        // A write error is not retried.
        let f = Fake::new(0x1B);
        f.write_errors.borrow_mut().push_back(Sense { key: 3, asc: 0x0C, ascq: 0 });
        let e = burn(&f, 40, &b, &d, &AtomicBool::new(false)).unwrap_err();
        assert!(e.to_string().contains("Schreibfehler"), "{e}");
        assert_eq!(f.writes().len(), 1);
    }

    #[test]
    fn overburn_policy() {
        let mut f = Fake::new(0x1B);
        f.capacity = 50;
        let (b, d) = opts();
        let no = AtomicBool::new(false);
        assert!(matches!(burn(&f, 100, &b, &d, &no), Err(BurnError::NotEnoughSpace { needed: 100, free: 50 })));
        let e = burn(&f, 100, &b, &DirectOptions { overburn: Overburn::Truncate, ..d.clone() }, &no).unwrap_err();
        assert!(e.to_string().contains("abgeschnitten"), "{e}");
        assert!(f.writes().is_empty(), "refused before writing");
        burn(&f, 100, &b, &DirectOptions { overburn: Overburn::Overburn, ..d }, &no).unwrap();
        assert_eq!(f.written.borrow().len(), 100 * S);
    }

    #[test]
    fn dual_layer_break_is_sent_before_the_data() {
        let f = Fake::new(0x2B);
        let (b, d) = opts();
        let b = BurnOptions { layer_break: Some(40), ..b };
        burn(&f, 64, &b, &d, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.ops()[..6], [0x1E, 0x46, 0x51, 0x23, 0xBF, 0x2A]);
        let sent = f.sent.borrow();
        assert_eq!(sent[4], spti::send_disc_structure(0x20, 12));
        assert_eq!(&f.layer_param.borrow()[8..12], 40u32.to_be_bytes());
        drop(sent);
        let f = Fake::new(0x2B);
        let e = burn(&f, 30, &b, &opts().1, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(e, BurnError::Image(_)), "break behind the image end");
    }

    #[test]
    fn device_buffer_is_sampled_for_the_graph() {
        let f = Fake::new(0x1B);
        let (b, d) = opts();
        let seen = Arc::new(std::sync::Mutex::new(vec![]));
        let s2 = seen.clone();
        let b = BurnOptions { telemetry: Some(TelemetryHook(Arc::new(move |t| s2.lock().unwrap().push(t)))), ..b };
        burn(&f, 64, &b, &DirectOptions { sample: Duration::ZERO, ..d }, &AtomicBool::new(false)).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().map(|t| (t.lba, t.device_percent)).collect::<Vec<_>>(), [(32, Some(50)), (64, Some(50))]);
        assert!(f.ops().contains(&0x5C));
    }

    #[test]
    fn verify_reads_the_disc_back() {
        let (b, d) = opts();
        let b = BurnOptions { verify: true, ..b };
        let f = Fake::new(0x1B);
        burn(&f, 70, &b, &d, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.ops().iter().filter(|&&o| o == 0x28).count(), 3);
        let mut f = Fake::new(0x1B);
        f.corrupt_read = Some(40);
        assert!(matches!(burn(&f, 70, &b, &d, &AtomicBool::new(false)), Err(BurnError::VerifyFailed { sector: 40 })));
        // A test write leaves nothing to compare.
        let f = Fake::new(0x11);
        burn(&f, 10, &b, &DirectOptions { test_mode: true, ..d }, &AtomicBool::new(false)).unwrap();
        assert!(!f.ops().contains(&0x28));
    }

    #[test]
    fn eject_after_unlocking() {
        let f = Fake::new(0x1B);
        let (b, d) = opts();
        burn(&f, 10, &BurnOptions { eject: true, ..b }, &d, &AtomicBool::new(false)).unwrap();
        assert_eq!(f.ops()[f.ops().len() - 2..], [0x1E, 0x1B]);
        assert_eq!(*f.sent.borrow().last().unwrap(), spti::start_stop_unit(true));
    }

    #[test]
    fn options_follow_the_settings() {
        let w = StudioWrite { write_type: "incremental".into(), link_size: 300, overburn: "overburn".into(), test_mode: true, ..StudioWrite::default() };
        let io = StudioIo { transfer_kib: 64, recovery_device_low: 10, ..StudioIo::default() };
        let d = DirectOptions::from_settings(&w, &io);
        assert_eq!((d.incremental, d.link_size, d.overburn, d.test_mode), (true, 255, Overburn::Overburn, true));
        assert_eq!((d.transfer_sectors, d.device_low), (32, 10));
    }
}
