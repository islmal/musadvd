//! Linux/Unix burner. On Linux it probes and writes natively over SG_IO with MusaDVD's own MMC
//! writer (the engine Burn Studio uses, which also blanks a used DVD-RW); growisofs (dvd+rw-tools)
//! or xorriso are only a fallback when the drive cannot be opened that way.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use crate::tools::blocking_tool_command;

use super::{
    verify_against_image, BurnError, BurnOptions, BurnPhase, BurnProgress, DriveInfo, MediaInfo, MediaType, OpticalBurner,
    WriteSpeed, DVD_1X_SECTORS_PER_SEC,
};

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|p| p.join(tool)).find(|p| p.is_file()))
}

/// Native SG_IO burner (Linux) with growisofs (preferred: DVD+R DL layer break) or xorriso as fallback.
pub struct CliBurner {
    growisofs: Option<PathBuf>,
    xorriso: Option<PathBuf>,
}

impl CliBurner {
    pub fn detect() -> Self {
        CliBurner { growisofs: which("growisofs"), xorriso: which("xorriso") }
    }

    fn xorriso(&self) -> Result<&Path, BurnError> {
        self.xorriso.as_deref().ok_or_else(|| BurnError::Unsupported("xorriso is not installed".into()))
    }
}

fn sys_attr(dev: &str, attr: &str) -> String {
    std::fs::read_to_string(format!("/sys/block/{dev}/device/{attr}")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Parses `xorriso -outdev DEV -list_speeds` / `-toc` output.
fn parse_media(text: &str) -> MediaInfo {
    let mut m = MediaInfo::none();
    for line in text.lines() {
        let (key, value) = match line.split_once(':') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match key {
            "Media current" => {
                m.present = !value.contains("is not present") && !value.is_empty();
                m.physical = value.split_whitespace().next().unwrap_or("").replace("_DL", " DL");
                let dvd = value.starts_with("DVD");
                let dual = value.contains("DL") || value.contains("Dual");
                m.rewritable = value.contains("RW") || value.contains("RAM");
                m.media_type = if !dvd { MediaType::Unknown } else if dual { MediaType::Dvd9 } else { MediaType::Dvd5 };
                m.layer_break_supported = value.starts_with("DVD+R") && dual;
            }
            "Media status" => {
                m.blank = value.contains("blank");
                m.writable = m.blank || value.contains("appendable") || m.rewritable;
            }
            "Media summary" => {
                // "... 0 data blocks, 0 data, 4489m free"
                if let Some(free) = value.split(',').map(str::trim).find(|p| p.ends_with("free")) {
                    let n = free.trim_end_matches("free").trim();
                    let mb: f64 = n.trim_end_matches(['m', 'k', 'g']).parse().unwrap_or(0.0);
                    let bytes = match n.chars().last() {
                        Some('g') => mb * 1024.0 * 1024.0 * 1024.0,
                        Some('m') => mb * 1024.0 * 1024.0,
                        Some('k') => mb * 1024.0,
                        _ => mb,
                    };
                    m.free_sectors = (bytes / 2048.0) as u64;
                }
            }
            "Write speed" => {
                // "Write speed  :  11080k , 8.0xD"
                if let Some(k) = value.split(',').next().and_then(|v| v.trim().trim_end_matches('k').parse::<f64>().ok()) {
                    m.speeds.push((k * 1000.0 / 2048.0).round() as u32);
                }
            }
            _ => {}
        }
    }
    m.total_sectors = m.total_sectors.max(m.free_sectors);
    m.speeds.sort_unstable_by(|a, b| b.cmp(a));
    m.speeds.dedup();
    m.writable &= m.media_type != MediaType::Unknown;
    m
}

/// "  123456/4567890 ( 2.7%) @3.9x" (growisofs) or "xorriso : UPDATE : Writing: ... 27.0% done".
fn parse_percent(line: &str) -> Option<f64> {
    let pct = line.split('%').next()?;
    let num: String = pct.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<Vec<_>>().into_iter().rev().collect();
    num.parse::<f64>().ok().filter(|p| (0.0..=100.0).contains(p)).map(|p| p / 100.0)
}

impl OpticalBurner for CliBurner {
    fn list_drives(&self) -> Result<Vec<DriveInfo>, BurnError> {
        let mut drives = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/block") else { return Ok(drives) };
        for e in entries.flatten() {
            let dev = e.file_name().to_string_lossy().into_owned();
            if !dev.starts_with("sr") {
                continue;
            }
            let name = format!("{} {}", sys_attr(&dev, "vendor"), sys_attr(&dev, "model"));
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            let id = format!("/dev/{dev}");
            drives.push(DriveInfo { name: if name.is_empty() { id.clone() } else { format!("{name} ({id})") }, path: id.clone(), id });
        }
        drives.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(drives)
    }

    fn probe_media(&self, drive_id: &str) -> Result<MediaInfo, BurnError> {
        #[cfg(target_os = "linux")]
        match native::probe(drive_id) {
            Ok(m) => return Ok(m),
            Err(e) if self.xorriso.is_none() => return Err(e),
            Err(_) => {} // e.g. no permission for SG_IO: ask xorriso
        }
        self.probe_media_cli(drive_id)
    }

    fn burn_iso(
        &self,
        drive_id: &str,
        iso_path: &Path,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        progress_tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        #[cfg(target_os = "linux")]
        {
            let has_tools = self.growisofs.is_some() || self.xorriso.is_some();
            match super::spti::SgIoTransport::open(drive_id) {
                Ok(t) => {
                    let source = Box::new(super::source::FileSource::open(iso_path)?);
                    return native::burn(&t, source, speed, options, cancel, &progress_tx);
                }
                Err(e) if !has_tools => return Err(e),
                Err(_) => {} // the drive cannot be opened for SG_IO: the tools may still reach it
            }
        }
        self.burn_iso_cli(drive_id, iso_path, speed, options, cancel, progress_tx)
    }

    #[cfg(target_os = "linux")]
    fn burn_source(
        &self,
        drive_id: &str,
        source: Box<dyn super::source::SectorSource>,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        progress_tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        let t = super::spti::SgIoTransport::open(drive_id)?;
        native::burn(&t, source, speed, options, cancel, &progress_tx)
    }
}

impl CliBurner {
    fn probe_media_cli(&self, drive_id: &str) -> Result<MediaInfo, BurnError> {
        let out = blocking_tool_command(self.xorriso()?).args(["-outdev", drive_id, "-list_speeds", "-toc"]).output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Ok(parse_media(&text))
    }

    fn burn_iso_cli(
        &self,
        drive_id: &str,
        iso_path: &Path,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        progress_tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Preparing, 0.0, "Preparing drive …"));
        let media = self.probe_media(drive_id).unwrap_or_else(|_| MediaInfo::none());
        let sectors = std::fs::metadata(iso_path)?.len().div_ceil(2048);
        if !media.present {
            return Err(BurnError::NoMedia);
        }
        if !media.blank && !options.erase {
            return Err(BurnError::Unsupported(format!("the {} contains data and must not be erased", media.physical)));
        }
        if !media.blank && media.physical.starts_with("DVD-RW") {
            let _ = progress_tx.send(BurnProgress::new(BurnPhase::Formatting, 0.0, "Erasing DVD-RW …"));
            let st = blocking_tool_command(self.xorriso()?).args(["-outdev", drive_id, "-blank", "as_needed"]).status()?;
            if !st.success() {
                return Err(BurnError::Backend("The DVD-RW could not be erased".into()));
            }
        }
        if media.blank && media.free_sectors > 0 && media.free_sectors < sectors {
            return Err(BurnError::NotEnoughSpace { needed: sectors, free: media.free_sectors });
        }
        let x = match speed {
            WriteSpeed::Max => None,
            WriteSpeed::Specific(s) => Some(s as f64 / DVD_1X_SECTORS_PER_SEC),
        };
        let mut cmd = if let Some(g) = &self.growisofs {
            let mut c = blocking_tool_command(g);
            c.arg("-dvd-compat");
            if let Some(x) = x {
                c.arg(format!("-speed={}", x.round().max(1.0)));
            }
            if let (Some(l0), true) = (options.layer_break, media.layer_break_supported) {
                c.arg(format!("-use-the-force-luke=break:{l0}"));
            }
            c.arg("-Z").arg(format!("{drive_id}={}", iso_path.display()));
            c
        } else {
            let mut c = blocking_tool_command(self.xorriso()?);
            c.args(["-as", "cdrecord", "-v", "-dao"]).arg(format!("dev={drive_id}"));
            if let Some(x) = x {
                c.arg(format!("speed={x:.1}"));
            }
            c.arg(iso_path);
            c
        };
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Formatting, 0.0, "Preparing disc …"));
        // growisofs writes progress to stdout/stderr depending on version; xorriso uses stderr.
        // Drain both streams so neither pipe can stall and live % updates always reach the UI.
        let spawn_reader = |stream: Box<dyn Read + Send>, tx: Sender<BurnProgress>| {
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream);
                let mut buf = Vec::new();
                let mut tail = String::new();
                loop {
                    buf.clear();
                    let mut byte = [0u8; 1];
                    while r.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                        if byte[0] == b'\r' || byte[0] == b'\n' {
                            break;
                        }
                        buf.push(byte[0]);
                    }
                    if buf.is_empty() && r.fill_buf().map(|b| b.is_empty()).unwrap_or(true) {
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if let Some(f) = parse_percent(&line) {
                        let phase = if f >= 0.999 { BurnPhase::Finalizing } else { BurnPhase::Writing };
                        let _ = tx.send(BurnProgress::new(phase, if phase == BurnPhase::Finalizing { 0.0 } else { f }, "Writing data …"));
                    } else if !line.trim().is_empty() {
                        tail = line;
                    }
                }
                tail
            })
        };
        let out_reader = spawn_reader(Box::new(child.stdout.take().unwrap()), progress_tx.clone());
        let err_reader = spawn_reader(Box::new(child.stderr.take().unwrap()), progress_tx.clone());
        let status = loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(BurnError::Cancelled);
            }
            if let Some(st) = child.try_wait()? {
                break st;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let out_tail = out_reader.join().unwrap_or_default();
        let err_tail = err_reader.join().unwrap_or_default();
        let tail = if err_tail.is_empty() { out_tail } else { err_tail };
        if !status.success() {
            return Err(BurnError::Backend(format!("Burning failed: {tail}")));
        }
        if options.verify {
            let _ = progress_tx.send(BurnProgress::new(BurnPhase::Verifying, 0.0, "Verifying disc …"));
            let mut device = open_for_verify(drive_id)?;
            let tx = progress_tx.clone();
            verify_against_image(&mut device, iso_path, cancel, &mut |f| {
                let _ = tx.send(BurnProgress::new(BurnPhase::Verifying, f, "Verifying disc …"));
            })?;
        }
        if options.eject {
            let _ = blocking_tool_command("eject").arg(drive_id).status();
        }
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Done, 1.0, "Done"));
        Ok(())
    }
}

/// Opens `drive_id` for read-back verification, retrying briefly if the drive is still
/// reloading/settling its TOC right after `growisofs` finishes (`ENOMEDIUM` = 123 on Linux).
fn open_for_verify(drive_id: &str) -> std::io::Result<std::fs::File> {
    let mut tried_close = false;
    for attempt in 0..16 {
        match std::fs::File::open(drive_id) {
            Ok(f) => return Ok(f),
            Err(e) if attempt < 15 && matches!(e.raw_os_error(), Some(123 | 16 | 11)) => {
                if !tried_close && e.raw_os_error() == Some(123) {
                    tried_close = true;
                    let _ = blocking_tool_command("eject").args(["-t", drive_id]).status();
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => return Err(e),
        }
    }
    std::fs::File::open(drive_id)
}

/// MMC over SG_IO: media probe from GET CONFIGURATION / READ DISC INFORMATION / GET PERFORMANCE
/// and writing through `mmcwrite` with Burn Studio's default write options.
#[cfg(target_os = "linux")]
mod native {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Sender;

    use super::super::mmcwrite::{burn_direct_with, DirectOptions};
    use super::super::source::SectorSource;
    use super::super::spti::{self, ScsiTransport, SgIoTransport};
    use super::super::{BurnError, BurnOptions, BurnProgress, MediaInfo, WriteSpeed};

    pub fn probe(drive_id: &str) -> Result<MediaInfo, BurnError> {
        Ok(probe_with(&SgIoTransport::open(drive_id)?))
    }

    pub fn probe_with(t: &dyn ScsiTransport) -> MediaInfo {
        let d = spti::drive_info(t);
        // Free blocks of the first track: the whole capacity of a blank disc.
        let free = spti::query(t, "READ TRACK INFORMATION", &spti::read_track_information(1, 1, 48), 48)
            .ok()
            .and_then(|b| b.get(16..20).map(|f| u32::from_be_bytes([f[0], f[1], f[2], f[3]]) as u64));
        super::media_from_mmc(&d, free)
    }

    pub fn burn(
        t: &dyn ScsiTransport,
        source: Box<dyn SectorSource>,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        tx: &Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        let s = crate::config::BurnStudioSettings::default();
        let direct = DirectOptions::from_settings(&s.write, &s.io).with_speed(speed);
        burn_direct_with(t, source, options, &direct, cancel, tx)
    }
}

/// [`MediaInfo`] from an MMC drive probe; `free_blocks` from READ TRACK INFORMATION.
fn media_from_mmc(d: &super::spti::DriveDetails, free_blocks: Option<u64>) -> MediaInfo {
    let profile = d.current_profile.unwrap_or(0);
    let mut m = MediaInfo::none();
    m.present = d.ready && profile != 0;
    if !m.present {
        return m;
    }
    // "DVD-RW (Restricted Overwrite)" is still a DVD-RW for the user.
    m.physical = d.current_profile_name.split(" (").next().unwrap_or_default().to_string();
    let bd = (0x40..=0x43).contains(&profile);
    let dvd_dl = matches!(profile, 0x15 | 0x16 | 0x2A | 0x2B);
    m.rewritable = matches!(profile, 0x12 | 0x13 | 0x14 | 0x1A | 0x2A | 0x43) || d.erasable == Some(true);
    m.blank = d.disc_status.as_deref() == Some("blank");
    m.writable = matches!(profile, 0x11 | 0x13..=0x16 | 0x1A | 0x1B | 0x2A | 0x2B | 0x41..=0x43);
    let capacity = free_blocks.filter(|&f| f > 0).or(d.capacity_sectors).unwrap_or(0);
    m.total_sectors = capacity.max(d.capacity_sectors.unwrap_or(0));
    m.free_sectors = if m.blank || matches!(profile, 0x13 | 0x1A | 0x43) { capacity } else { 0 };
    m.media_type = if bd {
        // BD-25 holds 12,219,392 sectors; anything clearly larger is dual layer.
        if m.total_sectors > 13_000_000 { MediaType::Bd50 } else { MediaType::Bd25 }
    } else if (0x10..=0x2B).contains(&profile) {
        if dvd_dl { MediaType::Dvd9 } else { MediaType::Dvd5 }
    } else {
        MediaType::Unknown
    };
    m.writable &= m.media_type != MediaType::Unknown;
    m.layer_break_supported = profile == 0x2B;
    // GET PERFORMANCE reports kB/s (1000 bytes); MediaInfo counts sectors per second.
    m.speeds = d.write_speeds_kbps.iter().map(|&k| (k as f64 * 1000.0 / 2048.0).round() as u32).collect();
    m.speeds.sort_unstable_by(|a, b| b.cmp(a));
    m.speeds.dedup();
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xorriso_report() {
        let text = "Drive current: -outdev '/dev/sr0'\nMedia current: DVD+R/DL\nMedia status : is blank\n\
                    Media summary: 0 sessions, 0 data blocks, 0 data, 8152m free\nWrite speed  :  11080k , 8.0xD\n\
                    Write speed  :   5540k , 4.0xD\n";
        let m = parse_media(text);
        assert!(m.present && m.blank && m.writable);
        assert_eq!(m.media_type, MediaType::Dvd9);
        assert_eq!(m.speeds, vec![5410, 2705]);
        assert_eq!(m.free_sectors, 8152 * 512);
    }

    #[test]
    fn percent() {
        assert_eq!(parse_percent(" 1234/5678 ( 27.0%) @3.9x, remaining 4:00"), Some(0.27));
        assert_eq!(parse_percent("no progress"), None);
    }

    #[test]
    fn media_from_mmc_probe() {
        use super::super::spti::DriveDetails;
        let blank_dvdr = DriveDetails {
            ready: true,
            current_profile: Some(0x1B),
            current_profile_name: "DVD+R".into(),
            disc_status: Some("blank".into()),
            erasable: Some(false),
            write_speeds_kbps: vec![22_160, 11_080, 5_540],
            ..Default::default()
        };
        let m = media_from_mmc(&blank_dvdr, Some(2_295_104));
        assert!(m.present && m.blank && m.writable && !m.rewritable);
        assert_eq!((m.media_type, m.free_sectors, m.total_sectors), (MediaType::Dvd5, 2_295_104, 2_295_104));
        assert_eq!(m.speeds, vec![10_820, 5_410, 2_705], "kB/s to sectors/s: 16x, 8x, 4x");
        assert!(!m.layer_break_supported);

        let dl = DriveDetails { current_profile: Some(0x2B), current_profile_name: "DVD+R DL".into(), ..blank_dvdr.clone() };
        let m = media_from_mmc(&dl, Some(4_173_824));
        assert_eq!(m.media_type, MediaType::Dvd9);
        assert!(m.layer_break_supported);

        let used_rw = DriveDetails {
            current_profile: Some(0x13),
            current_profile_name: "DVD-RW (Restricted Overwrite)".into(),
            disc_status: Some("finalized".into()),
            erasable: Some(true),
            capacity_sectors: Some(2_297_888),
            ..blank_dvdr.clone()
        };
        let m = media_from_mmc(&used_rw, None);
        assert_eq!(m.physical, "DVD-RW");
        assert!(m.rewritable && !m.blank && m.free_sectors == 2_297_888, "overwritable in place");

        let bd = DriveDetails { current_profile: Some(0x41), current_profile_name: "BD-R (SRM)".into(), ..blank_dvdr.clone() };
        assert_eq!(media_from_mmc(&bd, Some(12_219_392)).media_type, MediaType::Bd25);
        assert_eq!(media_from_mmc(&bd, Some(24_438_784)).media_type, MediaType::Bd50);

        let empty = DriveDetails { ready: false, current_profile: Some(0), ..Default::default() };
        assert!(!media_from_mmc(&empty, None).present);
    }
}
