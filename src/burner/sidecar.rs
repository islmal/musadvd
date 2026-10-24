//! Layout files that travel with a disc image: `.dvd` (layer break), `.cue` (single data track)
//! and `.mds` (Media Descriptor).
//!
//! `.dvd` and `.cue` are plain text. `.mds` is binary and written from the published
//! cdemu/libmirage format notes as remembered, **not** checked against Alcohol/Daemon Tools:
//! our parser reads back exactly what our writer produces (round-trip tested), nothing more.

use std::fmt;
use std::io;
use std::path::Path;

/// Frames per second in a cue sheet MSF time.
const FRAMES_PER_SEC: u32 = 75;

#[derive(Debug, PartialEq, Eq)]
pub struct SidecarError(pub String);

impl fmt::Display for SidecarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SidecarError {}

fn bad(msg: &str) -> SidecarError {
    SidecarError(msg.to_string())
}

// ---- .dvd ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DvdLayout {
    /// Sectors on layer 0.
    pub layer_break: u32,
    /// Image file name, relative to the `.dvd` file.
    pub image: String,
}

pub fn dvd_text(l: &DvdLayout) -> String {
    format!("LayerBreak={}\r\n{}\r\n", l.layer_break, l.image)
}

/// Tolerant: LF or CRLF, any case and spacing around the key.
pub fn parse_dvd(text: &str) -> Result<DvdLayout, SidecarError> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().ok_or_else(|| bad("leere .dvd-Datei"))?;
    let (key, value) = first.split_once('=').ok_or_else(|| bad(".dvd-Datei: „LayerBreak=…“ fehlt"))?;
    if !key.trim().eq_ignore_ascii_case("LayerBreak") {
        return Err(bad(".dvd-Datei: „LayerBreak=…“ fehlt"));
    }
    let layer_break = value.trim().parse().map_err(|_| bad(".dvd-Datei: ungültiger LayerBreak-Wert"))?;
    let image = lines.next().ok_or_else(|| bad(".dvd-Datei: Image-Name fehlt"))?.to_string();
    Ok(DvdLayout { layer_break, image })
}

pub fn write_dvd(path: &Path, l: &DvdLayout) -> io::Result<()> {
    std::fs::write(path, dvd_text(l))
}

// ---- .cue ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueMode {
    Mode1_2048,
    Mode1_2352,
}

impl CueMode {
    pub fn sector_size(self) -> u32 {
        match self {
            CueMode::Mode1_2048 => 2048,
            CueMode::Mode1_2352 => 2352,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub file: String,
    pub mode: CueMode,
    /// `INDEX 01` in frames.
    pub index01: u32,
}

pub fn cue_text(file: &str, mode: CueMode) -> String {
    let mode = match mode {
        CueMode::Mode1_2048 => "MODE1/2048",
        CueMode::Mode1_2352 => "MODE1/2352",
    };
    format!("FILE \"{file}\" BINARY\r\n  TRACK 01 {mode}\r\n    INDEX 01 00:00:00\r\n")
}

fn parse_msf(s: &str) -> Option<u32> {
    let mut it = s.split(':').map(|p| p.parse::<u32>().ok());
    let (m, sec, f) = (it.next()??, it.next()??, it.next()??);
    (it.next().is_none() && sec < 60 && f < FRAMES_PER_SEC).then_some((m * 60 + sec) * FRAMES_PER_SEC + f)
}

/// Accepts one BINARY file with one `MODE1/2048` or `MODE1/2352` track; audio or multi-track
/// sheets are rejected.
pub fn parse_cue(text: &str) -> Result<Cue, SidecarError> {
    let (mut file, mut mode, mut index01, mut tracks) = (None, None, None, 0);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match cmd.to_ascii_uppercase().as_str() {
            "FILE" => {
                if file.is_some() {
                    return Err(bad("Cue-Sheet mit mehreren Dateien wird nicht unterstützt"));
                }
                let (name, kind) = match rest.strip_prefix('"') {
                    Some(r) => r.split_once('"').ok_or_else(|| bad("Cue-Sheet: Dateiname ohne schließendes Anführungszeichen"))?,
                    None => rest.rsplit_once(char::is_whitespace).ok_or_else(|| bad("Cue-Sheet: FILE ohne Typ"))?,
                };
                if !kind.trim().eq_ignore_ascii_case("BINARY") {
                    return Err(bad("Cue-Sheet: nur BINARY-Dateien werden unterstützt"));
                }
                file = Some(name.trim().to_string());
            }
            "TRACK" => {
                tracks += 1;
                if tracks > 1 {
                    return Err(bad("Cue-Sheet mit mehreren Tracks wird nicht unterstützt"));
                }
                mode = Some(match rest.split_whitespace().nth(1).map(str::to_ascii_uppercase).as_deref() {
                    Some("MODE1/2048") => CueMode::Mode1_2048,
                    Some("MODE1/2352") => CueMode::Mode1_2352,
                    _ => return Err(bad("Cue-Sheet: nur Daten-Tracks (MODE1/2048, MODE1/2352) werden unterstützt")),
                });
            }
            "INDEX" => {
                let mut p = rest.split_whitespace();
                if p.next() == Some("01") {
                    index01 = Some(p.next().and_then(parse_msf).ok_or_else(|| bad("Cue-Sheet: ungültige Zeitangabe"))?);
                }
            }
            _ => {}
        }
    }
    match (file, mode, index01) {
        (Some(file), Some(mode), Some(index01)) => Ok(Cue { file, mode, index01 }),
        _ => Err(bad("Cue-Sheet: FILE, TRACK und INDEX 01 erforderlich")),
    }
}

pub fn write_cue(path: &Path, file: &str, mode: CueMode) -> io::Result<()> {
    std::fs::write(path, cue_text(file, mode))
}

// ---- .mds ----------------------------------------------------------------------------------

const MDS_SIGNATURE: &[u8; 16] = b"MEDIA DESCRIPTOR";
const MDS_HEADER: usize = 0x58;
const MDS_SESSION: usize = 0x20;
const MDS_TRACK: usize = 0x50;
const MDS_MEDIUM_DVD: u16 = 0x10;
/// First sector of the DVD data area (physical sector number 0x030000).
const DVD_DATA_START: u32 = 0x03_0000;
const PHYSICAL_FORMAT_LEN: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mds {
    /// Image size in 2048-byte sectors.
    pub sectors: u32,
    /// Sectors on layer 0 for a dual-layer disc.
    pub layer_break: Option<u32>,
    /// Image file name next to the `.mds`.
    pub image: String,
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

/// Single session, single MODE1/2048 track, DVD physical format structure (MMC format 0x00)
/// with the layer-0 end sector filled in for dual-layer images.
pub fn mds_bytes(m: &Mds) -> Vec<u8> {
    let session_at = MDS_HEADER;
    let track_at = session_at + MDS_SESSION;
    let extra_at = track_at + MDS_TRACK;
    let footer_at = extra_at + 8;
    let name_at = footer_at + 16;
    let name = format!("{}\0", m.image);
    let structures_at = (name_at + name.len()).next_multiple_of(4);
    let mut b = vec![0u8; structures_at + 4 + PHYSICAL_FORMAT_LEN];

    put(&mut b, 0, MDS_SIGNATURE);
    put(&mut b, 16, &[1, 5]);
    put(&mut b, 18, &MDS_MEDIUM_DVD.to_le_bytes());
    put(&mut b, 20, &1u16.to_le_bytes()); // sessions
    put(&mut b, 64, &(structures_at as u32).to_le_bytes()); // disc structures
    put(&mut b, 80, &(session_at as u32).to_le_bytes());

    put(&mut b, session_at, &0i32.to_le_bytes());
    put(&mut b, session_at + 4, &(m.sectors as i32 - 1).to_le_bytes());
    put(&mut b, session_at + 8, &1u16.to_le_bytes()); // session number
    b[session_at + 10] = 1; // blocks
    put(&mut b, session_at + 12, &1u16.to_le_bytes()); // first track
    put(&mut b, session_at + 14, &1u16.to_le_bytes()); // last track
    put(&mut b, session_at + 20, &(track_at as u32).to_le_bytes());

    b[track_at] = 0xAB; // MODE1, 2048 bytes per sector
    b[track_at + 2] = 0x14; // ADR/CTL data track
    b[track_at + 3] = 1; // track number
    b[track_at + 4] = 1;
    put(&mut b, track_at + 12, &(extra_at as u32).to_le_bytes());
    put(&mut b, track_at + 16, &2048u16.to_le_bytes());
    put(&mut b, track_at + 36, &1u32.to_le_bytes()); // files
    put(&mut b, track_at + 40, &(footer_at as u32).to_le_bytes());

    put(&mut b, extra_at + 4, &m.sectors.to_le_bytes()); // pregap 0, length
    put(&mut b, footer_at, &(name_at as u32).to_le_bytes());
    put(&mut b, name_at, name.as_bytes());

    put(&mut b, structures_at, &(PHYSICAL_FORMAT_LEN as u32).to_le_bytes());
    let phys = structures_at + 4;
    b[phys] = 0x01; // DVD-ROM book, part version 1
    b[phys + 2] = if m.layer_break.is_some() { 0x20 } else { 0 }; // two layers
    put(&mut b, phys + 5, &DVD_DATA_START.to_be_bytes()[1..]);
    put(&mut b, phys + 9, &(DVD_DATA_START + m.sectors - 1).to_be_bytes()[1..]);
    if let Some(l0) = m.layer_break {
        put(&mut b, phys + 13, &(DVD_DATA_START + l0 - 1).to_be_bytes()[1..]);
    }
    b
}

pub fn parse_mds(b: &[u8]) -> Result<Mds, SidecarError> {
    let u32_at = |at: usize| b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    let bad = || bad("ungültige .mds-Datei");
    if b.len() < MDS_HEADER || &b[..16] != MDS_SIGNATURE {
        return Err(bad());
    }
    if u16::from_le_bytes([b[18], b[19]]) != MDS_MEDIUM_DVD || u16::from_le_bytes([b[20], b[21]]) != 1 {
        return Err(SidecarError("nur .mds-Dateien mit einer DVD-Sitzung werden unterstützt".into()));
    }
    let session = u32_at(80).ok_or_else(bad)? as usize;
    let sectors = (u32_at(session + 4).ok_or_else(bad)? as i32 + 1) as u32;
    let track = u32_at(session + 20).ok_or_else(bad)? as usize;
    let footer = u32_at(track + 40).ok_or_else(bad)? as usize;
    let name_at = u32_at(footer).ok_or_else(bad)? as usize;
    let name = b.get(name_at..).and_then(|s| s.split(|&c| c == 0).next()).ok_or_else(bad)?;
    let phys = u32_at(64).ok_or_else(bad)? as usize + 4;
    let psn = |at: usize| b.get(at..at + 3).map(|s| u32::from_be_bytes([0, s[0], s[1], s[2]]));
    let two_layers = b.get(phys + 2).ok_or_else(bad)? & 0x60 != 0;
    let layer_break = if two_layers { Some(psn(phys + 13).ok_or_else(bad)? + 1 - DVD_DATA_START) } else { None };
    Ok(Mds { sectors, layer_break, image: String::from_utf8_lossy(name).into_owned() })
}

pub fn write_mds(path: &Path, m: &Mds) -> io::Result<()> {
    std::fs::write(path, mds_bytes(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dvd_round_trip_and_tolerance() {
        let l = DvdLayout { layer_break: 1_913_760, image: "film.iso".into() };
        let text = dvd_text(&l);
        assert_eq!(text, "LayerBreak=1913760\r\nfilm.iso\r\n");
        assert_eq!(parse_dvd(&text).unwrap(), l);
        assert_eq!(parse_dvd("  layerbreak = 1913760 \nfilm.iso").unwrap(), l, "LF, case, spaces");
        assert!(parse_dvd("").is_err());
        assert!(parse_dvd("LayerBreak=abc\nx.iso").is_err());
        assert!(parse_dvd("LayerBreak=5").is_err(), "image name missing");
        assert!(parse_dvd("Foo=5\nx.iso").is_err());
    }

    #[test]
    fn cue_round_trip_and_rejections() {
        for mode in [CueMode::Mode1_2048, CueMode::Mode1_2352] {
            let c = parse_cue(&cue_text("daten.bin", mode)).unwrap();
            assert_eq!(c, Cue { file: "daten.bin".into(), mode, index01: 0 });
        }
        let lf = "file \"my disc.iso\" binary\n track 01 mode1/2048\n  index 01 00:02:00\n";
        let c = parse_cue(lf).unwrap();
        assert_eq!((c.file.as_str(), c.index01), ("my disc.iso", 150), "2 s = 150 frames");
        assert_eq!(parse_msf("01:00:74"), Some(60 * 75 + 74));
        assert_eq!(parse_msf("00:60:00"), None);
        assert_eq!(parse_msf("00:00:75"), None);
        let audio = "FILE \"a.wav\" WAVE\n TRACK 01 AUDIO\n  INDEX 01 00:00:00";
        assert!(parse_cue(audio).unwrap_err().0.contains("BINARY"));
        let audio = "FILE \"a.bin\" BINARY\n TRACK 01 AUDIO\n  INDEX 01 00:00:00";
        assert!(parse_cue(audio).unwrap_err().0.contains("Daten-Tracks"));
        let two = format!("{}  TRACK 02 MODE1/2048\r\n", cue_text("a.bin", CueMode::Mode1_2048));
        assert!(parse_cue(&two).unwrap_err().0.contains("mehreren Tracks"));
        assert!(parse_cue("FILE \"a.bin\" BINARY\n").is_err());
        assert_eq!(CueMode::Mode1_2352.sector_size(), 2352);
    }

    #[test]
    fn mds_round_trip() {
        for layer_break in [None, Some(1_913_760)] {
            let m = Mds { sectors: 3_500_000, layer_break, image: "film.iso".into() };
            let bytes = mds_bytes(&m);
            assert_eq!(&bytes[..16], b"MEDIA DESCRIPTOR");
            assert_eq!(parse_mds(&bytes).unwrap(), m);
        }
        assert!(parse_mds(b"nope").is_err());
        let mut cd = mds_bytes(&Mds { sectors: 10, layer_break: None, image: "x.iso".into() });
        cd[18] = 0x01;
        assert!(parse_mds(&cd).unwrap_err().0.contains("DVD"));
        assert!(parse_mds(&cd[..100]).is_err(), "truncated file");
    }
}
