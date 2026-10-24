//! Read-only ISO 9660 access to an image file (or anything `Read + Seek`): directory listing,
//! file lookup, file contents and a sector-to-path map for mapping miscompares to files.
//!
//! Only the primary volume descriptor tree is read (8.3-style names of our own images, Joliet is
//! ignored). `shortcut:` UDF-only images (our Blu-ray images have no ISO 9660 side) are rejected;
//! add a UDF reader if one is ever needed.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};

use super::udf::SECTOR;

const FIRST_DESCRIPTOR: u64 = 16;
/// Directory nesting limit; ISO 9660 allows 8, the guard stops loops in damaged images.
const MAX_DEPTH: usize = 16;

#[derive(Debug)]
pub enum IsoError {
    Io(io::Error),
    /// No ISO 9660 primary volume descriptor at sector 16.
    NotIso,
    Corrupt(&'static str),
    NotFound(String),
}

impl fmt::Display for IsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IsoError::Io(e) => e.fmt(f),
            IsoError::NotIso => write!(f, "kein ISO-9660-Image (Primary Volume Descriptor fehlt)"),
            IsoError::Corrupt(m) => write!(f, "beschädigtes ISO-Image: {m}"),
            IsoError::NotFound(p) => write!(f, "{p} nicht im Image gefunden"),
        }
    }
}

impl std::error::Error for IsoError {}

impl From<io::Error> for IsoError {
    fn from(e: io::Error) -> Self {
        IsoError::Io(e)
    }
}

/// A file or directory of the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoEntry {
    /// Name without version suffix (`;1`) and trailing dot.
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// (first sector, bytes) per extent; more than one for multi-extent files.
    pub extents: Vec<(u32, u32)>,
}

impl IsoEntry {
    pub fn first_sector(&self) -> Option<u32> {
        self.extents.first().map(|e| e.0)
    }
}

pub struct IsoImage<R: Read + Seek> {
    r: R,
    ucs: bool,
    root: IsoEntry,
    pub label: String,
    pub total_sectors: u32,
}

fn ucs2(b: &[u8]) -> String {
    char::decode_utf16(b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]))).map(|r| r.unwrap_or('?')).collect()
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// One directory record at the start of `b`, `None` for the end-of-sector padding.
fn record(b: &[u8], ucs: bool) -> Result<Option<(IsoEntry, bool, usize)>, IsoError> {
    let len = b[0] as usize;
    if len == 0 {
        return Ok(None);
    }
    if len < 34 || len > b.len() || 33 + b[32] as usize > len {
        return Err(IsoError::Corrupt("ungültiger Verzeichniseintrag"));
    }
    let raw = &b[33..33 + b[32] as usize];
    let mut name = if ucs && raw.len() > 1 { ucs2(raw) } else { String::from_utf8_lossy(raw).into_owned() };
    if let Some(i) = name.rfind(';') {
        name.truncate(i);
    }
    if name.ends_with('.') {
        name.pop();
    }
    let size = le32(b, 10);
    let entry = IsoEntry { name, dir: b[25] & 2 != 0, size: size as u64, extents: vec![(le32(b, 2), size)] };
    let more = b[25] & 0x80 != 0;
    Ok(Some((entry, more, len)))
}

impl<R: Read + Seek> IsoImage<R> {
    pub fn open(r: R) -> Result<Self, IsoError> {
        Self::open_with(r, false)
    }

    /// Reads the Joliet tree (long Unicode names) instead of the primary one.
    pub fn open_joliet(r: R) -> Result<Self, IsoError> {
        Self::open_with(r, true)
    }

    fn open_with(mut r: R, joliet: bool) -> Result<Self, IsoError> {
        let end = r.seek(SeekFrom::End(0))?;
        let mut sector = vec![0u8; SECTOR as usize];
        // Descriptor set: 16.. until the primary descriptor or the terminator.
        let mut at = FIRST_DESCRIPTOR;
        loop {
            if (at + 1) * SECTOR > end {
                return Err(IsoError::NotIso);
            }
            r.seek(SeekFrom::Start(at * SECTOR))?;
            r.read_exact(&mut sector)?;
            if &sector[1..6] != b"CD001" || sector[0] == 255 {
                return Err(IsoError::NotIso);
            }
            if sector[0] == if joliet { 2 } else { 1 } {
                break;
            }
            at += 1;
        }
        let label = if joliet { ucs2(&sector[40..72]) } else { String::from_utf8_lossy(&sector[40..72]).into_owned() };
        let (root, _, _) = record(&sector[156..190], false)?.ok_or(IsoError::Corrupt("Wurzelverzeichnis fehlt"))?;
        Ok(IsoImage { r, ucs: joliet, root, label: label.trim_end().to_string(), total_sectors: le32(&sector, 80) })
    }

    fn read_bytes(&mut self, lba: u32, len: u64) -> Result<Vec<u8>, IsoError> {
        let mut buf = vec![0u8; len as usize];
        self.r.seek(SeekFrom::Start(lba as u64 * SECTOR))?;
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Entries of `dir` (without `.` and `..`); multi-extent files are merged.
    pub fn list_entry(&mut self, dir: &IsoEntry) -> Result<Vec<IsoEntry>, IsoError> {
        let (lba, len) = *dir.extents.first().ok_or(IsoError::Corrupt("Verzeichnis ohne Extent"))?;
        let data = self.read_bytes(lba, (len as u64).div_ceil(SECTOR) * SECTOR)?;
        let mut out: Vec<IsoEntry> = Vec::new();
        let (mut pos, mut continued) = (0usize, false);
        while pos < len as usize {
            let Some((entry, more, rec_len)) = record(&data[pos..(pos / SECTOR as usize + 1) * SECTOR as usize], self.ucs)? else {
                pos = (pos / SECTOR as usize + 1) * SECTOR as usize; // records do not cross sectors
                continue;
            };
            pos += rec_len;
            if entry.name == "\0" || entry.name == "\u{1}" {
                continue;
            }
            if continued {
                let last = out.last_mut().expect("continuation follows a record");
                last.size += entry.size;
                last.extents.extend(entry.extents);
            } else {
                out.push(entry);
            }
            continued = more;
        }
        Ok(out)
    }

    pub fn list(&mut self, path: &str) -> Result<Vec<IsoEntry>, IsoError> {
        let dir = self.find(path)?;
        if !dir.dir {
            return Err(IsoError::NotFound(path.to_string()));
        }
        self.list_entry(&dir)
    }

    /// Entry at `path` (`VIDEO_TS/VIDEO_TS.IFO`, case-insensitive, empty = root).
    pub fn find(&mut self, path: &str) -> Result<IsoEntry, IsoError> {
        let mut cur = self.root.clone();
        for part in path.split(['/', '\\']).filter(|p| !p.is_empty()) {
            if !cur.dir {
                return Err(IsoError::NotFound(path.to_string()));
            }
            cur = self
                .list_entry(&cur)?
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or_else(|| IsoError::NotFound(path.to_string()))?;
        }
        Ok(cur)
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, IsoError> {
        let entry = self.find(path)?;
        if entry.dir {
            return Err(IsoError::NotFound(path.to_string()));
        }
        let mut out = Vec::with_capacity(entry.size as usize);
        for (lba, len) in entry.extents {
            out.extend(self.read_bytes(lba, len as u64)?);
        }
        Ok(out)
    }

    /// Sorted `(first sector, sector after the last, path)` of every file extent.
    pub fn extent_map(&mut self) -> Result<Vec<(u64, u64, String)>, IsoError> {
        let mut out = Vec::new();
        let root = self.root.clone();
        self.walk(&root, "", 0, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk(&mut self, dir: &IsoEntry, prefix: &str, depth: usize, out: &mut Vec<(u64, u64, String)>) -> Result<(), IsoError> {
        if depth > MAX_DEPTH {
            return Err(IsoError::Corrupt("Verzeichnisse zu tief verschachtelt"));
        }
        for e in self.list_entry(dir)? {
            let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            if e.dir {
                self.walk(&e, &path, depth + 1, out)?;
            } else {
                for (lba, len) in &e.extents {
                    if *len > 0 {
                        out.push((*lba as u64, *lba as u64 + (*len as u64).div_ceil(SECTOR), path.clone()));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Path of the file holding `lba`, from a sorted [`IsoImage::extent_map`].
pub fn path_at(map: &[(u64, u64, String)], lba: u64) -> Option<&str> {
    let i = map.partition_point(|e| e.0 <= lba);
    map[..i].last().filter(|e| lba < e.1).map(|e| e.2.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::testutil::synthetic_video_ts;
    use crate::burner::udf::{plan_image, write_image, Timestamp};
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;

    fn image(label: &str) -> (std::path::PathBuf, Vec<u8>) {
        let dir = synthetic_video_ts(label);
        let video_ts = dir.join("VIDEO_TS");
        let plan = plan_image(&video_ts, label, &HashMap::new(), &HashMap::new()).unwrap();
        let iso = dir.join("t.iso");
        let ts = Timestamp { year: 2026, month: 10, day: 24, hour: 1, minute: 2, second: 3 };
        write_image(&plan, &iso, &ts, &AtomicBool::new(false), |_| {}).unwrap();
        let bytes = std::fs::read(&iso).unwrap();
        (dir, bytes)
    }

    #[test]
    fn lists_finds_and_reads() {
        let (dir, bytes) = image("LESETEST");
        let mut iso = IsoImage::open(Cursor::new(bytes)).unwrap();
        assert_eq!(iso.label, "LESETEST");
        let root: Vec<String> = iso.list("").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(root, ["AUDIO_TS", "VIDEO_TS"]);
        let names: Vec<String> = iso.list("video_ts").unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"VIDEO_TS.IFO".to_string()) && names.contains(&"VTS_01_1.VOB".to_string()));
        let want = std::fs::read(dir.join("VIDEO_TS").join("VTS_01_1.VOB")).unwrap();
        assert_eq!(iso.read_file("VIDEO_TS/VTS_01_1.VOB").unwrap(), want);
        assert_eq!(iso.find("VIDEO_TS/VTS_01_1.VOB").unwrap().size, want.len() as u64);
        assert!(matches!(iso.find("VIDEO_TS/NOPE.IFO"), Err(IsoError::NotFound(_))));
        assert!(matches!(iso.read_file("VIDEO_TS"), Err(IsoError::NotFound(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn extent_map_maps_sectors_to_files() {
        let (dir, bytes) = image("MAPTEST");
        let mut iso = IsoImage::open(Cursor::new(bytes)).unwrap();
        let map = iso.extent_map().unwrap();
        assert_eq!(map.len(), 7, "one extent per synthetic file");
        let vob = iso.find("VIDEO_TS/VTS_01_1.VOB").unwrap();
        let first = vob.first_sector().unwrap() as u64;
        assert_eq!(path_at(&map, first), Some("VIDEO_TS/VTS_01_1.VOB"));
        assert_eq!(path_at(&map, first + 9), Some("VIDEO_TS/VTS_01_1.VOB"), "20000 bytes = 10 sectors");
        assert_ne!(path_at(&map, first + 10), Some("VIDEO_TS/VTS_01_1.VOB"));
        assert_eq!(path_at(&map, 3), None, "system area");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn multi_extent_files_are_merged() {
        // Hand-made directory: file "BIG.BIN;1" in two extents (flag 0x80 on the first).
        let mut img = vec![0u8; 20 * SECTOR as usize];
        let pvd = 16 * SECTOR as usize;
        img[pvd] = 1;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        let mk = |lba: u32, size: u32, flags: u8, name: &[u8]| {
            let len = 33 + name.len() + (33 + name.len()) % 2;
            let mut r = vec![0u8; len];
            r[0] = len as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[25] = flags;
            r[32] = name.len() as u8;
            r[33..33 + name.len()].copy_from_slice(name);
            r
        };
        let root = mk(18, 2048, 2, &[0]);
        img[pvd + 156..pvd + 156 + root.len()].copy_from_slice(&root);
        let mut dir = mk(18, 2048, 2, &[0]);
        dir.extend(mk(18, 2048, 2, &[1]));
        dir.extend(mk(100, 0xFFFF_F800, 0x80, b"BIG.BIN;1"));
        dir.extend(mk(300, 4096, 0, b"BIG.BIN;1"));
        img[18 * SECTOR as usize..18 * SECTOR as usize + dir.len()].copy_from_slice(&dir);
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        let e = iso.find("BIG.BIN").unwrap();
        assert_eq!(e.extents, [(100, 0xFFFF_F800), (300, 4096)]);
        assert_eq!(e.size, 0xFFFF_F800 + 4096);
    }

    #[test]
    fn rejects_non_iso() {
        assert!(matches!(IsoImage::open(Cursor::new(vec![0u8; 40 * 2048])), Err(IsoError::NotIso)));
        assert!(matches!(IsoImage::open(Cursor::new(vec![0u8; 100])), Err(IsoError::NotIso)));
    }
}
