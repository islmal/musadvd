//! Linux/Unix burner using the command line tools growisofs (dvd+rw-tools) or xorriso.

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

/// Burner backed by growisofs (preferred: supports the DVD+R DL layer break) or xorriso.
pub struct CliBurner {
    growisofs: Option<PathBuf>,
    xorriso: Option<PathBuf>,
}

impl CliBurner {
    pub fn detect() -> Self {
        CliBurner { growisofs: which("growisofs"), xorriso: which("xorriso") }
    }

    fn xorriso(&self) -> Result<&Path, BurnError> {
        self.xorriso.as_deref().ok_or_else(|| BurnError::Unsupported("xorriso ist nicht installiert".into()))
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
        let out = blocking_tool_command(self.xorriso()?).args(["-outdev", drive_id, "-list_speeds", "-toc"]).output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Ok(parse_media(&text))
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
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Preparing, 0.0, "Laufwerk wird vorbereitet …"));
        let media = self.probe_media(drive_id).unwrap_or_else(|_| MediaInfo::none());
        let sectors = std::fs::metadata(iso_path)?.len().div_ceil(2048);
        if !media.present {
            return Err(BurnError::NoMedia);
        }
        if !media.blank && !options.erase {
            return Err(BurnError::Unsupported(format!("die {} enthält Daten und darf nicht gelöscht werden", media.physical)));
        }
        if !media.blank && media.physical.starts_with("DVD-RW") {
            let _ = progress_tx.send(BurnProgress::new(BurnPhase::Formatting, 0.0, "DVD-RW wird gelöscht …"));
            let st = blocking_tool_command(self.xorriso()?).args(["-outdev", drive_id, "-blank", "as_needed"]).status()?;
            if !st.success() {
                return Err(BurnError::Backend("DVD-RW konnte nicht gelöscht werden".into()));
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
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Formatting, 0.0, "Disc wird vorbereitet …"));
        // Both tools report progress on stderr; lines end in \r or \n.
        let stderr = child.stderr.take().unwrap();
        let tx = progress_tx.clone();
        let reader = std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
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
                    let _ = tx.send(BurnProgress::new(phase, if phase == BurnPhase::Finalizing { 0.0 } else { f }, "Daten werden geschrieben …"));
                } else if !line.trim().is_empty() {
                    tail = line;
                }
            }
            tail
        });
        let status = loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(BurnError::Cancelled);
            }
            if let Some(st) = child.try_wait()? {
                break st;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let tail = reader.join().unwrap_or_default();
        if !status.success() {
            return Err(BurnError::Backend(format!("Brennen fehlgeschlagen: {tail}")));
        }
        if options.verify {
            let _ = progress_tx.send(BurnProgress::new(BurnPhase::Verifying, 0.0, "Disc wird überprüft …"));
            let mut device = std::fs::File::open(drive_id)?;
            let tx = progress_tx.clone();
            verify_against_image(&mut device, iso_path, cancel, &mut |f| {
                let _ = tx.send(BurnProgress::new(BurnPhase::Verifying, f, "Disc wird überprüft …"));
            })?;
        }
        if options.eject {
            let _ = blocking_tool_command("eject").arg(drive_id).status();
        }
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Done, 1.0, "Fertig"));
        Ok(())
    }
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
}
