//! DVD-Video disc image writer: UDF 1.02 + ISO 9660 bridge (the format DVD players expect).
//!
//! The image is planned first ([`plan_image`]): every file gets its sector address before
//! anything is written, so the layer break of a dual-layer disc can be computed on exact
//! addresses and padding can be inserted in front of a titleset. [`write_image`] then streams
//! the image to disk.
//!
//! Layout (2048-byte sectors):
//! ```text
//!   0..15   system area (zero)
//!   16      ISO 9660 primary volume descriptor
//!   17      ISO 9660 descriptor set terminator
//!   18..20  UDF volume recognition sequence (BEA01, NSR02, TEA01)
//!   32..47  UDF main volume descriptor sequence (PVD, IUVD, PD, LVD, USD, TD)
//!   48..63  UDF reserve volume descriptor sequence (copy)
//!   64..65  UDF logical volume integrity descriptor + terminator
//!   66..67  ISO 9660 path tables (L, M)
//!   68..    ISO 9660 directories (root, AUDIO_TS, VIDEO_TS)
//!   256     UDF anchor volume descriptor pointer
//!   257..   UDF partition: file set descriptor, file entries, directories, then the file
//!           data in DVD-Video order (VIDEO_TS.IFO/VOB/BUP, VTS_01_0.IFO, VTS_01_0.VOB,
//!           VTS_01_1..9.VOB, VTS_01_0.BUP, ...), contiguous, optional padding per titleset
//!   last    UDF anchor volume descriptor pointer
//! ```
//! ISO 9660 and UDF directory entries point to the same file data (bridge).

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const SECTOR: u64 = 2048;

const ISO_PVD: u32 = 16;
const MAIN_VDS: u32 = 32;
const RESERVE_VDS: u32 = 48;
const LVID: u32 = 64;
const PATH_TABLE_L: u32 = 66;
const PATH_TABLE_M: u32 = 67;
const ISO_DIRS: u32 = 68;
const ANCHOR: u32 = 256;
const PARTITION_START: u32 = 257;
/// Longest extent a UDF short_ad can describe (30-bit length, sector aligned).
const MAX_EXTENT: u64 = (1 << 30) - SECTOR;

#[derive(Debug)]
pub enum ImageError {
    Io { path: PathBuf, error: std::io::Error },
    NotDvdVideo(String),
    Cancelled,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            ImageError::NotDvdVideo(m) => write!(f, "keine gültige VIDEO_TS-Struktur: {m}"),
            ImageError::Cancelled => write!(f, "abgebrochen"),
        }
    }
}

impl std::error::Error for ImageError {}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ImageError + '_ {
    move |error| ImageError::Io { path: path.to_path_buf(), error }
}

/// One file of the VIDEO_TS directory as placed in the image.
#[derive(Debug, Clone)]
pub struct ImageFile {
    /// `VIDEO_TS.IFO`, `VTS_01_1.VOB`, ...
    pub name: String,
    pub source: PathBuf,
    pub size: u64,
    /// Replaces the file content (patched IFO/BUP after a layer-break shift).
    pub data: Option<Vec<u8>>,
    /// Zero sectors written in front of the file (layer break alignment).
    pub padding_before: u32,
    /// First sector of the file data (absolute), set by the layout.
    pub lba: u32,
}

impl ImageFile {
    pub fn sectors(&self) -> u32 {
        self.size.div_ceil(SECTOR) as u32
    }
}

/// A fully laid out image.
#[derive(Debug, Clone)]
pub struct ImagePlan {
    pub label: String,
    /// VIDEO_TS files in DVD-Video order.
    pub files: Vec<ImageFile>,
    /// Total sectors of the image.
    pub total_sectors: u32,
    // Layout details (absolute sectors / partition-relative blocks).
    video_ts_iso_sectors: u32,
    fe_first_block: u32,
    video_ts_dir_blocks: u32,
    data_first_block: u32,
}

impl ImagePlan {
    pub fn file(&self, name: &str) -> Option<&ImageFile> {
        self.files.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn size_bytes(&self) -> u64 {
        self.total_sectors as u64 * SECTOR
    }
}

/// VIDEO_TS files in the order DVD-Video requires (and dvdauthor's IFO addresses assume).
pub fn dvd_file_order(video_ts: &Path) -> Result<Vec<(String, PathBuf, u64)>, ImageError> {
    let mut present: HashMap<String, (PathBuf, u64)> = HashMap::new();
    for entry in std::fs::read_dir(video_ts).map_err(io(video_ts))? {
        let entry = entry.map_err(io(video_ts))?;
        let meta = entry.metadata().map_err(io(&entry.path()))?;
        if meta.is_file() {
            let name = entry.file_name().to_string_lossy().to_ascii_uppercase();
            present.insert(name, (entry.path(), meta.len()));
        }
    }
    // (name, required) in disc order.
    let mut wanted: Vec<(String, bool)> =
        vec![("VIDEO_TS.IFO".into(), true), ("VIDEO_TS.VOB".into(), false), ("VIDEO_TS.BUP".into(), true)];
    for vts in 1..=99u32 {
        if !present.contains_key(&format!("VTS_{vts:02}_0.IFO")) {
            continue;
        }
        wanted.push((format!("VTS_{vts:02}_0.IFO"), true));
        wanted.push((format!("VTS_{vts:02}_0.VOB"), false));
        for part in 1..=9 {
            wanted.push((format!("VTS_{vts:02}_{part}.VOB"), false));
        }
        wanted.push((format!("VTS_{vts:02}_0.BUP"), true));
    }
    let mut order = Vec::new();
    for (name, required) in wanted {
        match present.remove(&name) {
            Some((p, size)) => order.push((name, p, size)),
            None if required => return Err(ImageError::NotDvdVideo(format!("{name} fehlt"))),
            None => {}
        }
    }
    if let Some(extra) = present.keys().next() {
        return Err(ImageError::NotDvdVideo(format!("unerwartete Datei {extra}")));
    }
    Ok(order)
}

/// Plans an image of `video_ts` with `label`. `padding` maps a titleset number to zero
/// sectors inserted in front of its `VTS_xx_0.IFO`; `overrides` replaces file contents.
pub fn plan_image(
    video_ts: &Path,
    label: &str,
    padding: &HashMap<u32, u32>,
    overrides: &HashMap<String, Vec<u8>>,
) -> Result<ImagePlan, ImageError> {
    let files: Vec<ImageFile> = dvd_file_order(video_ts)?
        .into_iter()
        .map(|(name, source, size)| {
            let vts = name.strip_prefix("VTS_").and_then(|r| r[..2].parse::<u32>().ok());
            let padding_before = match vts {
                Some(n) if name.ends_with("_0.IFO") => padding.get(&n).copied().unwrap_or(0),
                _ => 0,
            };
            let data = overrides.get(&name).cloned();
            let size = data.as_ref().map_or(size, |d| d.len() as u64);
            ImageFile { name, source, size, data, padding_before, lba: 0 }
        })
        .collect();
    Ok(layout(label, files))
}

/// Size of the ISO 9660 VIDEO_TS directory in sectors (records never cross a sector).
fn iso_dir_sectors(names: &[&str]) -> u32 {
    let mut sectors = 1;
    let mut used = 34 + 34; // "." and ".."
    for n in names {
        let len = iso_record_len(&iso_name(n));
        if used + len > SECTOR as usize {
            sectors += 1;
            used = 0;
        }
        used += len;
    }
    sectors
}

fn layout(label: &str, mut files: Vec<ImageFile>) -> ImagePlan {
    let names: Vec<String> = files.iter().map(|f| f.name.clone()).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let video_ts_iso_sectors = iso_dir_sectors(&name_refs);
    assert!(ISO_DIRS + 2 + video_ts_iso_sectors <= ANCHOR, "ISO directories must end before sector 256");

    // Partition blocks: 0 FSD, 1 TD, 2 root FE, 3 root dir, 4 AUDIO_TS FE, 5 AUDIO_TS dir,
    // 6 VIDEO_TS FE, 7.. VIDEO_TS dir, then one FE per file, then the file data.
    let dir_bytes: u64 = 40 + files.iter().map(|f| fid_len(&f.name) as u64).sum::<u64>();
    let video_ts_dir_blocks = dir_bytes.div_ceil(SECTOR) as u32;
    let fe_first_block = 7 + video_ts_dir_blocks;
    let data_first_block = fe_first_block + files.len() as u32;

    let mut sector = PARTITION_START + data_first_block;
    for f in &mut files {
        sector += f.padding_before;
        f.lba = sector;
        sector += f.sectors();
    }
    // Last sector: second anchor.
    let total_sectors = sector + 1;
    ImagePlan {
        label: label.to_string(),
        files,
        total_sectors,
        video_ts_iso_sectors,
        fe_first_block,
        video_ts_dir_blocks,
        data_first_block,
    }
}

// ---------------------------------------------------------------------------
// Low-level encoders
// ---------------------------------------------------------------------------

/// CRC-ITU-T (polynomial 0x1021, initial value 0), as used by ECMA-167 descriptor tags.
pub fn crc_itu(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
/// ISO 9660 both-byte-order 16 bit.
fn both16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    b[at + 2..at + 4].copy_from_slice(&v.to_be_bytes());
}
/// ISO 9660 both-byte-order 32 bit.
fn both32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    b[at + 4..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// Fills the 16-byte descriptor tag at the start of `d` (`len` = descriptor length).
fn tag(d: &mut [u8], id: u16, location: u32, len: usize) {
    put16(d, 0, id);
    put16(d, 2, 2); // descriptor version (UDF 1.02 / ECMA-167 2nd edition)
    d[5] = 0;
    put16(d, 6, 0); // tag serial number
    let crc = crc_itu(&d[16..len]);
    put16(d, 8, crc);
    put16(d, 10, (len - 16) as u16);
    put32(d, 12, location);
    let sum: u32 = d[..16].iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, b)| *b as u32).sum();
    d[4] = (sum % 256) as u8;
}

/// UDF timestamp (local time, offset 0 = UTC).
#[derive(Debug, Clone, Copy)]
pub struct Timestamp {
    pub year: i16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Timestamp {
    /// Current UTC time.
    pub fn now() -> Self {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as i16;
        Timestamp { year, month, day, hour: (rem / 3600) as u8, minute: (rem / 60 % 60) as u8, second: (rem % 60) as u8 }
    }

    fn udf(&self, b: &mut [u8], at: usize) {
        put16(b, at, 0x1000); // type 1 (local time), offset 0
        put16(b, at + 2, self.year as u16);
        b[at + 4] = self.month;
        b[at + 5] = self.day;
        b[at + 6] = self.hour;
        b[at + 7] = self.minute;
        b[at + 8] = self.second;
    }

    /// ISO 9660 7-byte directory record date.
    fn iso7(&self, b: &mut [u8], at: usize) {
        b[at] = (self.year - 1900) as u8;
        b[at + 1] = self.month;
        b[at + 2] = self.day;
        b[at + 3] = self.hour;
        b[at + 4] = self.minute;
        b[at + 5] = self.second;
        b[at + 6] = 0;
    }

    /// ISO 9660 17-byte volume descriptor date.
    fn iso17(&self, b: &mut [u8], at: usize) {
        let s = format!("{:04}{:02}{:02}{:02}{:02}{:02}00", self.year, self.month, self.day, self.hour, self.minute, self.second);
        b[at..at + 16].copy_from_slice(s.as_bytes());
        b[at + 16] = 0;
    }
}

/// UDF dstring of `len` bytes (8-bit compressed unicode, last byte = used length).
fn dstring(b: &mut [u8], at: usize, len: usize, text: &str) {
    if text.is_empty() {
        return;
    }
    let bytes: Vec<u8> = text.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'_' }).take(len - 2).collect();
    b[at] = 8;
    b[at + 1..at + 1 + bytes.len()].copy_from_slice(&bytes);
    b[at + len - 1] = (bytes.len() + 1) as u8;
}

/// charspec "OSTA Compressed Unicode".
fn charspec(b: &mut [u8], at: usize) {
    b[at] = 0;
    let s = b"OSTA Compressed Unicode";
    b[at + 1..at + 1 + s.len()].copy_from_slice(s);
}

/// EntityID with identifier and suffix.
fn regid(b: &mut [u8], at: usize, ident: &str, suffix: &[u8]) {
    b[at] = 0;
    b[at + 1..at + 1 + ident.len()].copy_from_slice(ident.as_bytes());
    b[at + 24..at + 24 + suffix.len()].copy_from_slice(suffix);
}

const UDF_REV: [u8; 2] = [0x02, 0x01]; // 0x0102 little endian
const IMPL_ID: &str = "*convertx-rust";

fn domain_regid(b: &mut [u8], at: usize) {
    regid(b, at, "*OSTA UDF Compliant", &[UDF_REV[0], UDF_REV[1], 0]);
}
fn impl_regid(b: &mut [u8], at: usize) {
    regid(b, at, IMPL_ID, &[]);
}

/// ISO 9660 file name with version (`VIDEO_TS.IFO;1`).
fn iso_name(name: &str) -> Vec<u8> {
    format!("{name};1").into_bytes()
}

fn iso_record_len(name: &[u8]) -> usize {
    let len = 33 + name.len();
    len + (len % 2)
}

fn iso_record(lba: u32, size: u32, dir: bool, name: &[u8], ts: &Timestamp) -> Vec<u8> {
    let len = iso_record_len(name);
    let mut r = vec![0u8; len];
    r[0] = len as u8;
    both32(&mut r, 2, lba);
    both32(&mut r, 10, size);
    ts.iso7(&mut r, 18);
    r[25] = if dir { 2 } else { 0 };
    both16(&mut r, 28, 1);
    r[32] = name.len() as u8;
    r[33..33 + name.len()].copy_from_slice(name);
    r
}

/// Length of a UDF File Identifier Descriptor for `name` (padded to 4).
fn fid_len(name: &str) -> usize {
    (38 + 1 + name.len()).div_ceil(4) * 4
}

/// UDF File Identifier Descriptor. `parent` = the parent entry (no name).
fn fid(name: Option<&str>, dir: bool, icb_block: u32, unique: u32, tag_block: u32) -> Vec<u8> {
    let l_fi = name.map_or(0, |n| n.len() + 1);
    let len = (38 + l_fi).div_ceil(4) * 4;
    let mut d = vec![0u8; len];
    put16(&mut d, 16, 1); // file version
    d[18] = (if dir { 0x02 } else { 0 }) | (if name.is_none() { 0x08 } else { 0 });
    d[19] = l_fi as u8;
    // ICB long_ad: length, location (block, partition 0), implementation use (unique id).
    put32(&mut d, 20, SECTOR as u32);
    put32(&mut d, 24, icb_block);
    put16(&mut d, 28, 0);
    put32(&mut d, 32, unique);
    put16(&mut d, 36, 0); // L_IU
    if let Some(n) = name {
        d[38] = 8;
        d[39..39 + n.len()].copy_from_slice(n.as_bytes());
    }
    tag(&mut d, 257, tag_block, len);
    d
}

/// UDF File Entry (one block). `extents` = (partition block, byte length) pairs.
fn file_entry(dir: bool, links: u16, size: u64, extents: &[(u32, u64)], unique: u64, block: u32, ts: &Timestamp) -> Vec<u8> {
    let mut d = vec![0u8; SECTOR as usize];
    // ICB tag: strategy 4, max entries 1, file type, short_ad allocation descriptors.
    put16(&mut d, 20, 4);
    put16(&mut d, 24, 1);
    d[27] = if dir { 4 } else { 5 };
    put16(&mut d, 34, 0);
    put32(&mut d, 36, u32::MAX); // uid
    put32(&mut d, 40, u32::MAX); // gid
    put32(&mut d, 44, 0x14A5); // read + execute for owner, group, others
    put16(&mut d, 48, links);
    put64(&mut d, 56, size);
    let blocks: u64 = extents.iter().map(|(_, len)| len.div_ceil(SECTOR)).sum();
    put64(&mut d, 64, blocks);
    ts.udf(&mut d, 72);
    ts.udf(&mut d, 84);
    ts.udf(&mut d, 96);
    put32(&mut d, 108, 1); // checkpoint
    impl_regid(&mut d, 128);
    put64(&mut d, 160, unique);
    put32(&mut d, 168, 0); // L_EA
    let l_ad = extents.len() * 8;
    put32(&mut d, 172, l_ad as u32);
    for (i, (pos, len)) in extents.iter().enumerate() {
        put32(&mut d, 176 + 8 * i, *len as u32);
        put32(&mut d, 180 + 8 * i, *pos);
    }
    let len = 176 + l_ad;
    assert!(len <= SECTOR as usize, "too many extents for one file entry");
    tag(&mut d, 261, block, len);
    d
}

/// Splits a file into short_ad extents (each < 1 GiB).
fn extents(first_block: u32, size: u64) -> Vec<(u32, u64)> {
    if size == 0 {
        return vec![];
    }
    let mut out = Vec::new();
    let mut block = first_block;
    let mut left = size;
    while left > 0 {
        let len = left.min(MAX_EXTENT);
        out.push((block, len));
        block += (len / SECTOR) as u32;
        left -= len;
    }
    out
}

// ---------------------------------------------------------------------------
// Image writing
// ---------------------------------------------------------------------------

/// Writes sectors sequentially and checks that every structure lands where it was planned.
struct SectorWriter<W: Write> {
    out: W,
    sector: u32,
    path: PathBuf,
}

impl<W: Write> SectorWriter<W> {
    fn zeros_until(&mut self, target: u32) -> Result<(), ImageError> {
        assert!(target >= self.sector, "layout went backwards: at {} want {target}", self.sector);
        let zero = [0u8; SECTOR as usize];
        while self.sector < target {
            self.out.write_all(&zero).map_err(io(&self.path))?;
            self.sector += 1;
        }
        Ok(())
    }

    /// Writes `data` at sector `at`, padded to whole sectors.
    fn put(&mut self, at: u32, data: &[u8]) -> Result<(), ImageError> {
        self.zeros_until(at)?;
        self.out.write_all(data).map_err(io(&self.path))?;
        let rem = data.len() as u64 % SECTOR;
        if rem != 0 {
            self.out.write_all(&vec![0u8; (SECTOR - rem) as usize]).map_err(io(&self.path))?;
        }
        self.sector += (data.len() as u64).div_ceil(SECTOR) as u32;
        Ok(())
    }
}

/// Writes the planned image to `output`; `progress` gets 0..1 while the file data is copied.
/// Stops with [`ImageError::Cancelled`] once `cancel` is set (the partial file stays).
pub fn write_image(
    plan: &ImagePlan,
    output: &Path,
    ts: &Timestamp,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f64),
) -> Result<(), ImageError> {
    let file = File::create(output).map_err(io(output))?;
    let mut w = SectorWriter { out: BufWriter::with_capacity(4 << 20, file), sector: 0, path: output.to_path_buf() };
    let n = plan.total_sectors;
    let p = PARTITION_START;
    let partition_len = n - 1 - p;
    let label = &plan.label;

    // Unique IDs: root 0, AUDIO_TS 16, VIDEO_TS 17, files 18.. (UDF reserves 1..15).
    let audio_uid = 16u32;
    let video_uid = 17u32;
    let file_uid = |i: usize| 18 + i as u32;

    // ---- ISO 9660 directory contents -------------------------------------------------------
    let root_lba = ISO_DIRS;
    let audio_lba = ISO_DIRS + 1;
    let video_lba = ISO_DIRS + 2;
    let video_size = plan.video_ts_iso_sectors * SECTOR as u32;
    let root_dir = {
        let mut d = Vec::new();
        d.extend(iso_record(root_lba, SECTOR as u32, true, &[0], ts));
        d.extend(iso_record(root_lba, SECTOR as u32, true, &[1], ts));
        d.extend(iso_record(audio_lba, SECTOR as u32, true, b"AUDIO_TS", ts));
        d.extend(iso_record(video_lba, video_size, true, b"VIDEO_TS", ts));
        d
    };
    let audio_dir = {
        let mut d = Vec::new();
        d.extend(iso_record(audio_lba, SECTOR as u32, true, &[0], ts));
        d.extend(iso_record(root_lba, SECTOR as u32, true, &[1], ts));
        d
    };
    let video_dir = {
        let mut d = Vec::new();
        d.extend(iso_record(video_lba, video_size, true, &[0], ts));
        d.extend(iso_record(root_lba, SECTOR as u32, true, &[1], ts));
        let mut sorted: Vec<&ImageFile> = plan.files.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for f in sorted {
            let r = iso_record(f.lba, f.size as u32, false, &iso_name(&f.name), ts);
            // Records must not cross a sector boundary.
            let used = d.len() % SECTOR as usize;
            if used + r.len() > SECTOR as usize {
                d.extend(vec![0u8; SECTOR as usize - used]);
            }
            d.extend(r);
        }
        d
    };
    let path_table = |big_endian: bool| -> Vec<u8> {
        let mut t = Vec::new();
        for (name, lba) in [(&b"\0"[..], root_lba), (&b"AUDIO_TS"[..], audio_lba), (&b"VIDEO_TS"[..], video_lba)] {
            t.push(name.len() as u8);
            t.push(0);
            t.extend(if big_endian { lba.to_be_bytes() } else { lba.to_le_bytes() });
            t.extend(if big_endian { 1u16.to_be_bytes() } else { 1u16.to_le_bytes() });
            t.extend(name);
            if name.len() % 2 == 1 {
                t.push(0);
            }
        }
        t
    };
    let path_table_size = path_table(false).len() as u32;

    // ---- ISO 9660 volume descriptors ---------------------------------------------------------
    let mut pvd = vec![0u8; SECTOR as usize];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    let pad = |b: &mut [u8], at: usize, len: usize, s: &str| {
        b[at..at + len].fill(b' ');
        b[at..at + s.len().min(len)].copy_from_slice(&s.as_bytes()[..s.len().min(len)]);
    };
    pad(&mut pvd, 8, 32, "");
    pad(&mut pvd, 40, 32, label);
    both32(&mut pvd, 80, n);
    both16(&mut pvd, 120, 1);
    both16(&mut pvd, 124, 1);
    both16(&mut pvd, 128, SECTOR as u16);
    both32(&mut pvd, 132, path_table_size);
    put32(&mut pvd, 140, PATH_TABLE_L);
    pvd[148..152].copy_from_slice(&PATH_TABLE_M.to_be_bytes());
    let root_record = iso_record(root_lba, SECTOR as u32, true, &[0], ts);
    pvd[156..156 + 34].copy_from_slice(&root_record);
    pad(&mut pvd, 190, 128, label);
    pad(&mut pvd, 318, 128, "");
    pad(&mut pvd, 446, 128, "");
    pad(&mut pvd, 574, 128, "CONVERTX-RUST");
    pad(&mut pvd, 702, 37, "");
    pad(&mut pvd, 739, 37, "");
    pad(&mut pvd, 776, 37, "");
    ts.iso17(&mut pvd, 813);
    ts.iso17(&mut pvd, 830);
    pvd[847..863].fill(b'0');
    pvd[864..880].fill(b'0');
    pvd[881] = 1;
    w.put(ISO_PVD, &pvd)?;

    let mut term = vec![0u8; SECTOR as usize];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    w.put(ISO_PVD + 1, &term)?;

    // ---- UDF volume recognition sequence -------------------------------------------------------
    for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
        let mut v = vec![0u8; SECTOR as usize];
        v[1..6].copy_from_slice(*id);
        v[6] = 1;
        w.put(18 + i as u32, &v)?;
    }

    // ---- UDF volume descriptor sequence (main + reserve) -------------------------------------
    let vds = |start: u32| -> Vec<Vec<u8>> {
        let mut seq = Vec::new();
        // Primary Volume Descriptor
        let mut d = vec![0u8; 512];
        put32(&mut d, 16, 0);
        put32(&mut d, 20, 0);
        dstring(&mut d, 24, 32, label);
        put16(&mut d, 56, 1);
        put16(&mut d, 58, 1);
        put16(&mut d, 60, 2);
        put16(&mut d, 62, 2);
        put32(&mut d, 64, 1);
        put32(&mut d, 68, 1);
        dstring(&mut d, 72, 128, &format!("{:08X}{:08X}{label}", ts.year as u32 * 10000 + ts.month as u32 * 100 + ts.day as u32, n));
        charspec(&mut d, 200);
        charspec(&mut d, 264);
        regid(&mut d, 344, IMPL_ID, &[]);
        ts.udf(&mut d, 376);
        impl_regid(&mut d, 388);
        tag(&mut d, 1, start, 512);
        seq.push(d);
        // Implementation Use Volume Descriptor
        let mut d = vec![0u8; 512];
        put32(&mut d, 16, 1);
        regid(&mut d, 20, "*UDF LV Info", &[UDF_REV[0], UDF_REV[1]]);
        charspec(&mut d, 52);
        dstring(&mut d, 116, 128, label);
        impl_regid(&mut d, 352);
        tag(&mut d, 4, start + 1, 512);
        seq.push(d);
        // Partition Descriptor (read-only, no space bitmaps)
        let mut d = vec![0u8; 512];
        put32(&mut d, 16, 2);
        put16(&mut d, 20, 1);
        put16(&mut d, 22, 0);
        regid(&mut d, 24, "+NSR02", &[]);
        put32(&mut d, 184, 1);
        put32(&mut d, 188, p);
        put32(&mut d, 192, partition_len);
        impl_regid(&mut d, 196);
        tag(&mut d, 5, start + 2, 512);
        seq.push(d);
        // Logical Volume Descriptor
        let mut d = vec![0u8; 446];
        put32(&mut d, 16, 3);
        charspec(&mut d, 20);
        dstring(&mut d, 84, 128, label);
        put32(&mut d, 212, SECTOR as u32);
        domain_regid(&mut d, 216);
        put32(&mut d, 248, SECTOR as u32); // FSD long_ad: length, block 0, partition 0
        put32(&mut d, 252, 0);
        put32(&mut d, 264, 6);
        put32(&mut d, 268, 1);
        impl_regid(&mut d, 272);
        put32(&mut d, 432, 2 * SECTOR as u32);
        put32(&mut d, 436, LVID);
        d[440] = 1; // type 1 partition map
        d[441] = 6;
        put16(&mut d, 442, 1);
        put16(&mut d, 444, 0);
        tag(&mut d, 6, start + 3, 446);
        seq.push(d);
        // Unallocated Space Descriptor
        let mut d = vec![0u8; 24];
        put32(&mut d, 16, 4);
        tag(&mut d, 7, start + 4, 24);
        seq.push(d);
        // Terminating Descriptor
        let mut d = vec![0u8; 512];
        tag(&mut d, 8, start + 5, 512);
        seq.push(d);
        seq
    };
    for (i, d) in vds(MAIN_VDS).iter().enumerate() {
        w.put(MAIN_VDS + i as u32, d)?;
    }
    for (i, d) in vds(RESERVE_VDS).iter().enumerate() {
        w.put(RESERVE_VDS + i as u32, d)?;
    }

    // ---- Logical Volume Integrity Descriptor -------------------------------------------------
    let mut d = vec![0u8; 134];
    ts.udf(&mut d, 16);
    put32(&mut d, 28, 1); // close
    put64(&mut d, 40, file_uid(plan.files.len()) as u64); // next unique id
    put32(&mut d, 72, 1);
    put32(&mut d, 76, 46);
    put32(&mut d, 80, 0); // free space
    put32(&mut d, 84, partition_len);
    impl_regid(&mut d, 88);
    put32(&mut d, 120, plan.files.len() as u32);
    put32(&mut d, 124, 3); // directories: root, AUDIO_TS, VIDEO_TS
    put16(&mut d, 128, 0x0102);
    put16(&mut d, 130, 0x0102);
    put16(&mut d, 132, 0x0102);
    tag(&mut d, 9, LVID, 134);
    w.put(LVID, &d)?;
    let mut d = vec![0u8; 512];
    tag(&mut d, 8, LVID + 1, 512);
    w.put(LVID + 1, &d)?;

    // ---- ISO 9660 path tables and directories --------------------------------------------------
    w.put(PATH_TABLE_L, &path_table(false))?;
    w.put(PATH_TABLE_M, &path_table(true))?;
    w.put(root_lba, &root_dir)?;
    w.put(audio_lba, &audio_dir)?;
    w.put(video_lba, &video_dir)?;

    // ---- Anchor ---------------------------------------------------------------------------------
    let anchor = |location: u32| {
        let mut d = vec![0u8; 512];
        put32(&mut d, 16, 16 * SECTOR as u32);
        put32(&mut d, 20, MAIN_VDS);
        put32(&mut d, 24, 16 * SECTOR as u32);
        put32(&mut d, 28, RESERVE_VDS);
        tag(&mut d, 2, location, 512);
        d
    };
    w.put(ANCHOR, &anchor(ANCHOR))?;

    // ---- UDF partition: file set, directories, file entries ------------------------------------
    // File Set Descriptor (block 0) + terminator (block 1)
    let mut d = vec![0u8; 512];
    ts.udf(&mut d, 16);
    put16(&mut d, 28, 3);
    put16(&mut d, 30, 3);
    put32(&mut d, 32, 1);
    put32(&mut d, 36, 1);
    charspec(&mut d, 48);
    dstring(&mut d, 112, 128, label);
    charspec(&mut d, 240);
    dstring(&mut d, 304, 32, label);
    put32(&mut d, 400, SECTOR as u32); // root ICB: block 2
    put32(&mut d, 404, 2);
    domain_regid(&mut d, 416);
    tag(&mut d, 256, 0, 512);
    w.put(p, &d)?;
    let mut d = vec![0u8; 512];
    tag(&mut d, 8, 1, 512);
    w.put(p + 1, &d)?;

    // Root directory: FE block 2, data block 3.
    let root_data = {
        let mut d = fid(None, true, 2, 0, 3);
        d.extend(fid(Some("AUDIO_TS"), true, 4, audio_uid, 3));
        d.extend(fid(Some("VIDEO_TS"), true, 6, video_uid, 3));
        d
    };
    w.put(p + 2, &file_entry(true, 3, root_data.len() as u64, &[(3, root_data.len() as u64)], 0, 2, ts))?;
    w.put(p + 3, &root_data)?;
    // AUDIO_TS: FE block 4, data block 5 (only the parent entry).
    let audio_data = fid(None, true, 2, 0, 5);
    w.put(p + 4, &file_entry(true, 1, audio_data.len() as u64, &[(5, audio_data.len() as u64)], audio_uid as u64, 4, ts))?;
    w.put(p + 5, &audio_data)?;
    // VIDEO_TS: FE block 6, data from block 7.
    let video_data = {
        let mut sorted: Vec<(usize, &ImageFile)> = plan.files.iter().enumerate().collect();
        sorted.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        let mut stream = Vec::new();
        stream.extend(fid(None, true, 2, 0, 7));
        for (i, f) in sorted {
            // Tag location: the block where the descriptor starts.
            let block = 7 + (stream.len() as u64 / SECTOR) as u32;
            stream.extend(fid(Some(&f.name), false, plan.fe_first_block + i as u32, file_uid(i), block));
        }
        stream
    };
    debug_assert!(video_data.len() as u64 <= plan.video_ts_dir_blocks as u64 * SECTOR);
    w.put(p + 6, &file_entry(true, 1, video_data.len() as u64, &[(7, video_data.len() as u64)], video_uid as u64, 6, ts))?;
    w.put(p + 7, &video_data)?;

    // One File Entry per file.
    for (i, f) in plan.files.iter().enumerate() {
        let block = plan.fe_first_block + i as u32;
        let fe = file_entry(false, 1, f.size, &extents(f.lba - p, f.size), file_uid(i) as u64, block, ts);
        w.put(p + block, &fe)?;
    }
    debug_assert_eq!(w.sector, p + plan.data_first_block);

    // ---- File data ------------------------------------------------------------------------------
    let total_bytes: u64 = plan.files.iter().map(|f| f.size).sum::<u64>().max(1);
    let mut copied = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    for f in &plan.files {
        w.zeros_until(f.lba)?;
        match &f.data {
            Some(data) => {
                w.put(f.lba, data)?;
                copied += f.size;
            }
            None => {
                let mut src = File::open(&f.source).map_err(io(&f.source))?;
                let mut left = f.size;
                while left > 0 {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(ImageError::Cancelled);
                    }
                    let want = left.min(buf.len() as u64) as usize;
                    let n = src.read(&mut buf[..want]).map_err(io(&f.source))?;
                    if n == 0 {
                        return Err(ImageError::Io {
                            path: f.source.clone(),
                            error: std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Datei wurde während des Schreibens kürzer"),
                        });
                    }
                    w.out.write_all(&buf[..n]).map_err(io(output))?;
                    left -= n as u64;
                    copied += n as u64;
                    progress(copied as f64 / total_bytes as f64);
                }
                let rem = f.size % SECTOR;
                if rem != 0 {
                    w.out.write_all(&vec![0u8; (SECTOR - rem) as usize]).map_err(io(output))?;
                }
                w.sector += f.sectors();
            }
        }
    }
    // Closing anchor in the last sector.
    w.put(n - 1, &anchor(n - 1))?;
    debug_assert_eq!(w.sector, n);
    w.out.flush().map_err(io(output))?;
    progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_vector() {
        // CRC-16/XMODEM (= CRC-ITU-T with initial value 0) check value.
        assert_eq!(crc_itu(b"123456789"), 0x31C3);
    }

    #[test]
    fn tag_checksum_and_crc() {
        let mut d = vec![0u8; 512];
        d[100] = 0xAB;
        tag(&mut d, 8, 1234, 512);
        assert_eq!(u16::from_le_bytes([d[0], d[1]]), 8);
        assert_eq!(u16::from_le_bytes([d[10], d[11]]), 496);
        assert_eq!(u32::from_le_bytes(d[12..16].try_into().unwrap()), 1234);
        assert_eq!(u16::from_le_bytes([d[8], d[9]]), crc_itu(&d[16..512]));
        let sum: u32 = d[..16].iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, b)| *b as u32).sum();
        assert_eq!(d[4] as u32, sum % 256);
    }

    #[test]
    fn dstrings_and_names() {
        let mut b = vec![0u8; 32];
        dstring(&mut b, 0, 32, "MY_DVD");
        assert_eq!(b[0], 8);
        assert_eq!(&b[1..7], b"MY_DVD");
        assert_eq!(b[31], 7);
        assert_eq!(iso_name("VIDEO_TS.IFO"), b"VIDEO_TS.IFO;1");
        assert_eq!(iso_record_len(b"VIDEO_TS.IFO;1"), 48);
        assert_eq!(fid_len("VIDEO_TS.IFO"), 52);
    }

    #[test]
    fn extents_split_below_one_gib() {
        assert!(extents(10, 0).is_empty());
        assert_eq!(extents(10, 5000), vec![(10, 5000)]);
        let e = extents(10, MAX_EXTENT + 4096);
        assert_eq!(e, vec![(10, MAX_EXTENT), (10 + (MAX_EXTENT / SECTOR) as u32, 4096)]);
    }

    #[test]
    fn timestamps() {
        let t = Timestamp::now();
        assert!(t.year >= 2024 && (1..=12).contains(&t.month) && (1..=31).contains(&t.day));
    }
}
