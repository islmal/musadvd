//! Offline image tools of the Burn Studio: volume label read/patch and the DVD-Video layer break
//! inspector. (`burner::tools`, not the process helper `crate::tools`.)
//!
//! The label patcher edits an existing `.iso` in place and touches exactly the fields our two
//! writers (`udf.rs` UDF 1.02 + ISO 9660, `udf250.rs` UDF 2.50) put the label in, fixing the
//! descriptor CRC and tag checksum afterwards, so the result equals an image built with the new
//! label (tested byte for byte).

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::isoread::{IsoError, IsoImage};
use super::label::sanitize_volume_label;
use super::layerbreak::{fits, Capacities};
use super::udf::{crc_itu, dstring, put16, ImageError, ImagePlan, SECTOR};

const S: usize = SECTOR as usize;
const ANCHOR: u64 = 256;

#[derive(Debug)]
pub enum LabelError {
    Io(PathBuf, io::Error),
    /// Neither an ISO 9660 nor a UDF volume descriptor was found.
    NotRecognised,
    Corrupt(&'static str),
}

impl fmt::Display for LabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LabelError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            LabelError::NotRecognised => write!(f, "not an ISO 9660 or UDF image"),
            LabelError::Corrupt(m) => write!(f, "corrupt image: {m}"),
        }
    }
}

impl std::error::Error for LabelError {}

struct Img<'a> {
    f: File,
    path: &'a Path,
    sectors: u64,
}

impl<'a> Img<'a> {
    fn open(path: &'a Path, write: bool) -> Result<Self, LabelError> {
        let err = |e| LabelError::Io(path.to_path_buf(), e);
        let f = OpenOptions::new().read(true).write(write).open(path).map_err(err)?;
        let sectors = f.metadata().map_err(err)?.len() / SECTOR;
        Ok(Img { f, path, sectors })
    }

    fn read(&mut self, lba: u64) -> Result<Vec<u8>, LabelError> {
        let mut b = vec![0u8; S];
        let err = |e| LabelError::Io(self.path.to_path_buf(), e);
        self.f.seek(SeekFrom::Start(lba * SECTOR)).map_err(err)?;
        self.f.read_exact(&mut b).map_err(err)?;
        Ok(b)
    }

    fn write(&mut self, lba: u64, b: &[u8]) -> Result<(), LabelError> {
        let err = |e| LabelError::Io(self.path.to_path_buf(), e);
        self.f.seek(SeekFrom::Start(lba * SECTOR)).map_err(err)?;
        self.f.write_all(b).map_err(err)
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Decoded UDF dstring (compression 8 or 16) at `at`, `len` bytes long.
fn read_dstring(b: &[u8], at: usize, len: usize) -> String {
    let used = b[at + len - 1] as usize;
    if used < 2 || used > len - 1 {
        return String::new();
    }
    let body = &b[at + 1..at + used];
    match b[at] {
        8 => body.iter().map(|&c| c as char).collect(),
        16 => char::decode_utf16(body.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]]))).map(|r| r.unwrap_or('?')).collect(),
        _ => String::new(),
    }
}

fn set_dstring(b: &mut [u8], at: usize, len: usize, text: &str) {
    b[at..at + len].fill(0);
    dstring(b, at, len, text);
}

/// Recomputes the descriptor CRC and the tag checksum after a change.
fn retag(b: &mut [u8]) {
    let crc_len = u16::from_le_bytes([b[10], b[11]]) as usize;
    let crc = crc_itu(&b[16..16 + crc_len]);
    put16(b, 8, crc);
    let sum: u32 = b[..16].iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, v)| *v as u32).sum();
    b[4] = (sum % 256) as u8;
}

fn tag_id(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn pad_ascii(b: &mut [u8], at: usize, len: usize, s: &str) {
    b[at..at + len].fill(b' ');
    let n = s.len().min(len);
    b[at..at + n].copy_from_slice(&s.as_bytes()[..n]);
}

/// ISO 9660 descriptors (`PVD` and Joliet `SVD`): `(sector, is_joliet)`.
fn iso_descriptors(img: &mut Img) -> Result<Vec<(u64, bool)>, LabelError> {
    let mut out = Vec::new();
    for lba in 16..img.sectors.min(48) {
        let b = img.read(lba)?;
        if &b[1..6] != b"CD001" || b[0] == 255 {
            break;
        }
        match b[0] {
            1 => out.push((lba, false)),
            2 if matches!(&b[88..91], b"%/@" | b"%/C" | b"%/E") => out.push((lba, true)),
            _ => {}
        }
    }
    Ok(out)
}

/// The volume label: ISO 9660 primary descriptor, else the UDF primary volume descriptor.
pub fn read_label(iso: &Path) -> Result<String, LabelError> {
    let mut img = Img::open(iso, false)?;
    if let Some(&(lba, _)) = iso_descriptors(&mut img)?.iter().find(|d| !d.1) {
        let b = img.read(lba)?;
        return Ok(String::from_utf8_lossy(&b[40..72]).trim_end().to_string());
    }
    for pvd in udf_descriptors(&mut img)? {
        if tag_id(&pvd.1) == 1 {
            return Ok(read_dstring(&pvd.1, 24, 32));
        }
    }
    Err(LabelError::NotRecognised)
}

/// Descriptors `(sector, data)` of the main volume descriptor sequence.
fn udf_descriptors(img: &mut Img) -> Result<Vec<(u64, Vec<u8>)>, LabelError> {
    if img.sectors <= ANCHOR {
        return Ok(vec![]);
    }
    let avdp = img.read(ANCHOR)?;
    if tag_id(&avdp) != 2 {
        return Ok(vec![]);
    }
    let (len, loc) = (le32(&avdp, 16) as u64 / SECTOR, le32(&avdp, 20) as u64);
    let mut out = Vec::new();
    for lba in loc..(loc + len).min(img.sectors) {
        let b = img.read(lba)?;
        if tag_id(&b) == 8 {
            break;
        }
        out.push((lba, b));
    }
    Ok(out)
}

type VdsSeq = Vec<(u64, Vec<u8>)>;

/// Both volume descriptor sequences (main, reserve) from the anchor.
fn vds_sequences(img: &mut Img) -> Result<Vec<VdsSeq>, LabelError> {
    if img.sectors <= ANCHOR {
        return Ok(vec![]);
    }
    let avdp = img.read(ANCHOR)?;
    if tag_id(&avdp) != 2 {
        return Ok(vec![]);
    }
    let mut seqs = Vec::new();
    for at in [16, 24] {
        let (len, loc) = (le32(&avdp, at) as u64 / SECTOR, le32(&avdp, at + 4) as u64);
        let mut seq = Vec::new();
        for lba in loc..(loc + len).min(img.sectors) {
            let b = img.read(lba)?;
            if tag_id(&b) == 8 {
                break;
            }
            seq.push((lba, b));
        }
        seqs.push(seq);
    }
    Ok(seqs)
}

/// First allocation descriptor `(length, position)` of the file entry at `lba`.
fn first_extent(img: &mut Img, lba: u64) -> Result<(u32, u32), LabelError> {
    let b = img.read(lba)?;
    let (ea, ad) = match tag_id(&b) {
        261 => (168, 176),
        266 => (208, 216),
        _ => return Err(LabelError::Corrupt("metadata file entry missing")),
    };
    let at = ad + le32(&b, ea) as usize;
    if le32(&b, ea + 4) < 8 || at + 8 > S {
        return Err(LabelError::Corrupt("metadata file entry without extent"));
    }
    Ok((le32(&b, at), le32(&b, at + 4)))
}

/// Sectors of the File Set Descriptor(s) the first logical volume descriptor points to.
fn fsd_sectors(img: &mut Img, seq: &[(u64, Vec<u8>)]) -> Result<Vec<u64>, LabelError> {
    let Some(lvd) = seq.iter().find(|d| tag_id(&d.1) == 6) else { return Ok(vec![]) };
    let pstart = seq
        .iter()
        .find(|d| tag_id(&d.1) == 5)
        .map(|d| le32(&d.1, 188) as u64)
        .ok_or(LabelError::Corrupt("Partition Descriptor missing"))?;
    let lvd = &lvd.1;
    let block = le32(lvd, 252) as u64;
    let part_ref = u16::from_le_bytes([lvd[256], lvd[257]]) as usize;
    // Partition maps start at 440: type 1 = physical (6 bytes), type 2 = metadata (64 bytes).
    let (mut at, mut maps) = (440usize, Vec::new());
    for _ in 0..le32(lvd, 268) {
        let (ty, len) = (*lvd.get(at).ok_or(LabelError::Corrupt("Partitionstabelle"))?, *lvd.get(at + 1).unwrap_or(&0) as usize);
        maps.push((ty, at));
        at += len.max(1);
    }
    let &(ty, map) = maps.get(part_ref).ok_or(LabelError::Corrupt("Partitionsreferenz"))?;
    if ty == 1 {
        return Ok(vec![pstart + block]);
    }
    // Metadata partition: FSD at `block` of the main metadata file and of its mirror.
    let mut out = Vec::new();
    for off in [40, 44] {
        let efe = le32(lvd, map + off) as u64;
        let (_, pos) = first_extent(img, pstart + efe)?;
        out.push(pstart + pos as u64 + block);
    }
    Ok(out)
}

/// Writes `new` (already sanitised) into every label field; returns the previous label.
fn patch_all(img: &mut Img, new: &str) -> Result<String, LabelError> {
    let mut old = None;
    let mut found = false;

    for (lba, joliet) in iso_descriptors(img)? {
        let mut b = img.read(lba)?;
        if joliet {
            let ucs: Vec<u8> = new.chars().take(16).flat_map(|c| (c as u16).to_be_bytes()).collect();
            for (i, pair) in b[40..72].as_chunks_mut::<2>().0.iter_mut().enumerate() {
                pair.copy_from_slice(ucs.get(2 * i..2 * i + 2).unwrap_or(&[0, 0x20]));
            }
        } else {
            let before = String::from_utf8_lossy(&b[40..72]).trim_end().to_string();
            // Volume set identifier: changed only if it carries the label (as our writer does).
            if String::from_utf8_lossy(&b[190..318]).trim_end() == before {
                pad_ascii(&mut b, 190, 128, new);
            }
            pad_ascii(&mut b, 40, 32, new);
            old = Some(before);
        }
        img.write(lba, &b)?;
        found = true;
    }

    let seqs = vds_sequences(img)?;
    for seq in &seqs {
        for (lba, d) in seq {
            let mut d = d.clone();
            match tag_id(&d) {
                1 => {
                    old.get_or_insert_with(|| read_dstring(&d, 24, 32));
                    set_dstring(&mut d, 24, 32, new);
                    // Volume set identifier: 16 hex digits (date, size) then the label.
                    let keep: String = read_dstring(&d, 72, 128).chars().take(16).collect();
                    set_dstring(&mut d, 72, 128, &format!("{keep}{new}"));
                }
                4 => set_dstring(&mut d, 116, 128, new),
                6 => set_dstring(&mut d, 84, 128, new),
                _ => continue,
            }
            retag(&mut d);
            img.write(*lba, &d)?;
            found = true;
        }
    }
    if let Some(main) = seqs.first() {
        for lba in fsd_sectors(img, main)? {
            let mut d = img.read(lba)?;
            if tag_id(&d) != 256 {
                return Err(LabelError::Corrupt("File Set Descriptor not found"));
            }
            set_dstring(&mut d, 112, 128, new);
            set_dstring(&mut d, 304, 32, new);
            retag(&mut d);
            img.write(lba, &d)?;
        }
    }
    if !found {
        return Err(LabelError::NotRecognised);
    }
    Ok(old.unwrap_or_default())
}

/// Changes the volume label of `iso` in place (the label is sanitised like a new image's) and
/// returns `(old label, new label)`.
pub fn patch_label(iso: &Path, new: &str) -> Result<(String, String), LabelError> {
    let new = sanitize_volume_label(new);
    let mut img = Img::open(iso, true)?;
    let old = patch_all(&mut img, &new)?;
    img.f.flush().map_err(|e| LabelError::Io(iso.to_path_buf(), e))?;
    Ok((old, new))
}

// ---- DVD-Video layer break candidates -------------------------------------------------------

/// A possible layer break: the start of a title cell.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Candidate {
    pub vts: u32,
    /// Title number within the titleset (from the PGC entry).
    pub ttn: u32,
    pub pgc: u32,
    pub cell: u32,
    pub start_lba: u32,
    pub end_lba: u32,
    pub seamless: bool,
    pub interleaved: bool,
    /// Sectors on layer 0 / layer 1 with this break (padding included).
    pub l0_sectors: u32,
    pub l1_sectors: u32,
    pub split_percent: f64,
    /// Zero sectors needed in front of the titleset to align the break to 16 sectors.
    pub padding_sectors: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CandidateOptions {
    /// List cells that are not seamlessly linked first.
    pub prefer_non_seamless: bool,
    pub show_interleaved: bool,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn be16(b: &[u8], at: usize) -> Option<usize> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes([s[0], s[1]]) as usize)
}

type CellRow = (u32, u32, u32, u8, u32, u32);

/// Cells of one VTS IFO as `(ttn, pgc, cell, flags, first, last)` (sectors relative to the title
/// VOBs) plus the VTS_VOBS offset. Damaged tables end the walk silently.
fn walk_cells(ifo: &[u8]) -> Option<(u32, Vec<CellRow>)> {
    if !ifo.starts_with(b"DVDVIDEO-VTS") {
        return None;
    }
    let vobs = be32(ifo, 0xC4)?;
    let pgcit = be32(ifo, 0xCC)? as usize * S;
    let mut out = Vec::new();
    if vobs == 0 || pgcit == 0 {
        return Some((vobs, out));
    }
    for i in 0..be16(ifo, pgcit)? {
        let srp = pgcit + 8 + 8 * i;
        let ttn = (be32(ifo, srp)? >> 24) & 0x7F;
        let pgc = pgcit + be32(ifo, srp + 4)? as usize;
        let (n, table) = (*ifo.get(pgc + 3)? as usize, be16(ifo, pgc + 0xE8)?);
        if table == 0 {
            continue;
        }
        for c in 0..n {
            let e = pgc + table + 24 * c;
            out.push((ttn, i as u32 + 1, c as u32 + 1, *ifo.get(e)?, be32(ifo, e + 8)?, be32(ifo, e + 20)?));
        }
    }
    Some((vobs, out))
}

/// Layer break candidates from titleset IFOs `(vts number, sector of the IFO in the image, bytes)`.
fn candidates(vts: &[(u32, u32, Vec<u8>)], total: u32, caps: Capacities, opts: CandidateOptions) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for (n, ifo_lba, ifo) in vts {
        let Some((vobs, cells)) = walk_cells(ifo) else { continue };
        for (ttn, pgc, cell, flags, first, last) in cells {
            let (seamless, interleaved) = (flags & 0x08 != 0, flags & 0x04 != 0);
            if interleaved && !opts.show_interleaved {
                continue;
            }
            let start = ifo_lba + vobs + first;
            let padding = (16 - start % 16) % 16;
            let (l0, all) = (start + padding, total + padding);
            if !fits(l0, all, caps) || out.iter().any(|c| c.start_lba == start) {
                continue;
            }
            out.push(Candidate {
                vts: *n,
                ttn,
                pgc,
                cell,
                start_lba: start,
                end_lba: ifo_lba + vobs + last,
                seamless,
                interleaved,
                l0_sectors: l0,
                l1_sectors: all - l0,
                split_percent: l0 as f64 * 100.0 / all as f64,
                padding_sectors: padding,
            });
        }
    }
    out.sort_by_key(|c| (opts.prefer_non_seamless && c.seamless, c.padding_sectors > 0, c.l0_sectors));
    out
}

/// Candidates for a planned (folder) image.
pub fn ifo_candidates(plan: &ImagePlan, caps: Capacities, opts: CandidateOptions) -> Result<Vec<Candidate>, ImageError> {
    let mut vts = Vec::new();
    for f in &plan.files {
        let Some(n) = f.name.strip_prefix("VTS_").filter(|_| f.name.ends_with("_0.IFO")).and_then(|r| r[..2].parse().ok()) else {
            continue;
        };
        let ifo = match &f.data {
            Some(d) => d.clone(),
            None => std::fs::read(&f.source).map_err(|error| ImageError::Io { path: f.source.clone(), error })?,
        };
        vts.push((n, f.lba, ifo));
    }
    Ok(candidates(&vts, plan.total_sectors, caps, opts))
}

/// Candidates for an existing `.iso`.
pub fn ifo_candidates_iso(iso: &Path, caps: Capacities, opts: CandidateOptions) -> Result<Vec<Candidate>, IsoError> {
    let mut img = IsoImage::open(File::open(iso)?)?;
    let mut vts = Vec::new();
    for e in img.list("VIDEO_TS")? {
        let Some(n) = e.name.strip_prefix("VTS_").filter(|_| e.name.ends_with("_0.IFO")).and_then(|r| r[..2].parse().ok()) else {
            continue;
        };
        let lba = e.first_sector().ok_or(IsoError::Corrupt("IFO without data"))?;
        vts.push((n, lba, img.read_file(&format!("VIDEO_TS/{}", e.name))?));
    }
    Ok(candidates(&vts, img.total_sectors, caps, opts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::testutil::{synthetic_dvd, synthetic_video_ts, Cell};
    use crate::burner::udf::{plan_image, write_image, Timestamp};
    use crate::burner::udf250::{plan_entries, write_bd_image, TreeEntry};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    const TS: Timestamp = Timestamp { year: 2026, month: 10, day: 24, hour: 9, minute: 30, second: 0 };

    fn dvd_image(dir: &Path, label: &str, name: &str) -> PathBuf {
        let plan = plan_image(&dir.join("VIDEO_TS"), label, &HashMap::new(), &HashMap::new()).unwrap();
        let out = dir.join(name);
        write_image(&plan, &out, &TS, &AtomicBool::new(false), |_| {}).unwrap();
        out
    }

    #[test]
    fn patched_dvd_image_equals_a_fresh_one() {
        let dir = synthetic_video_ts("label_dvd");
        let old = dvd_image(&dir, "ALT", "alt.iso");
        let fresh = dvd_image(&dir, "NEU_LABEL", "neu.iso");
        assert_eq!(read_label(&old).unwrap(), "ALT");
        let (was, now) = patch_label(&old, "neu label").unwrap();
        assert_eq!((was.as_str(), now.as_str()), ("ALT", "NEU_LABEL"), "sanitised like a new label");
        assert_eq!(read_label(&old).unwrap(), "NEU_LABEL");
        assert!(std::fs::read(&old).unwrap() == std::fs::read(&fresh).unwrap(), "byte-identical to a fresh image");
        // Shrinking works too (stale bytes of the longer label are cleared).
        patch_label(&old, "X").unwrap();
        let short = dvd_image(&dir, "X", "x.iso");
        assert!(std::fs::read(&old).unwrap() == std::fs::read(&short).unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn patched_bd_image_equals_a_fresh_one() {
        let dir = synthetic_video_ts("label_bd");
        let src = dir.join("VIDEO_TS").join("VTS_01_1.VOB");
        let entries = |name: &str| {
            vec![
                TreeEntry::dir("BDMV"),
                TreeEntry::file(&format!("BDMV/{name}"), src.clone(), std::fs::metadata(&src).unwrap().len()),
            ]
        };
        let build = |label: &str, out: &str| {
            let plan = plan_entries(label, &entries("index.bdmv")).unwrap();
            let out = dir.join(out);
            write_bd_image(&plan, &out, &TS, &AtomicBool::new(false), |_| {}).unwrap();
            out
        };
        let old = build("ALT", "alt.iso");
        let fresh = build("NEU_LABEL", "neu.iso");
        assert_eq!(read_label(&old).unwrap(), "ALT", "UDF label of an image without ISO 9660 side");
        patch_label(&old, "NEU_LABEL").unwrap();
        assert!(std::fs::read(&old).unwrap() == std::fs::read(&fresh).unwrap(), "byte-identical to a fresh image");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn joliet_descriptor_is_patched() {
        // Primary + Joliet descriptor only: UCS-2 label, `00 20` padding.
        let mut img = vec![0u8; 20 * S];
        for (i, ty) in [1u8, 2].iter().enumerate() {
            let at = (16 + i) * S;
            img[at] = *ty;
            img[at + 1..at + 6].copy_from_slice(b"CD001");
            img[at + 40..at + 72].fill(b' ');
            if *ty == 2 {
                img[at + 88..at + 91].copy_from_slice(b"%/E");
            }
        }
        let path = std::env::temp_dir().join(format!("cx_joliet_{}.iso", std::process::id()));
        std::fs::write(&path, &img).unwrap();
        patch_label(&path, "Film").unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(&out[16 * S + 40..16 * S + 44], b"FILM");
        assert_eq!(&out[17 * S + 40..17 * S + 48], &[0, b'F', 0, b'I', 0, b'L', 0, b'M']);
        assert_eq!(&out[17 * S + 48..17 * S + 52], &[0, 0x20, 0, 0x20]);
        std::fs::remove_file(&path).ok();
        let junk = std::env::temp_dir().join(format!("cx_junk_{}.iso", std::process::id()));
        std::fs::write(&junk, vec![0u8; 300 * S]).unwrap();
        assert!(matches!(patch_label(&junk, "x"), Err(LabelError::NotRecognised)));
        std::fs::remove_file(&junk).ok();
    }

    fn cell(first: u32, last: u32, flags: u8) -> Cell {
        Cell { first, last, flags }
    }

    #[test]
    fn candidates_follow_the_rules() {
        let cells = vec![cell(0, 499, 0), cell(500, 799, 0x08), cell(800, 999, 0), cell(1000, 1199, 0x04)];
        let dir = synthetic_dvd("candidates", &[(1, cells)]);
        let plan = plan_image(&dir.join("VIDEO_TS"), "T", &HashMap::new(), &HashMap::new()).unwrap();
        let total = plan.total_sectors;
        let caps = Capacities { single_layer: total - 1, layer: total };
        let vob = plan.file("VTS_01_0.IFO").unwrap().lba + 3;
        let opts = CandidateOptions { prefer_non_seamless: true, show_interleaved: false };

        let list = ifo_candidates(&plan, caps, opts).unwrap();
        assert!(!list.is_empty());
        for c in &list {
            let (l0, all) = (c.l0_sectors, total + c.padding_sectors);
            assert_eq!(l0 % 16, 0, "aligned: {c:?}");
            assert!(l0 >= all - l0 && c.l1_sectors == all - l0, "L0 holds at least as much as L1");
            assert_eq!(c.start_lba + c.padding_sectors, l0);
            assert!(!c.interleaved, "interleaved cells are hidden by default");
            assert_eq!((c.vts, c.ttn, c.pgc), (1, 1, 1));
        }
        assert!(list.iter().all(|c| c.start_lba != vob), "cell 1 is before the middle");
        let seamless_cell = list.iter().position(|c| c.cell == 2).expect("seamless cell is listed");
        let plain_cell = list.iter().position(|c| c.cell == 3).expect("plain cell is listed");
        assert!(list[seamless_cell].seamless && !list[plain_cell].seamless);
        assert!(plain_cell < seamless_cell, "non-seamless first when preferred");
        assert_eq!(list[plain_cell].start_lba, vob + 800);
        assert_eq!(list[plain_cell].end_lba, vob + 999);

        let natural = ifo_candidates(&plan, caps, CandidateOptions { prefer_non_seamless: false, ..opts }).unwrap();
        let first = natural.iter().find(|c| c.padding_sectors == 0).or(natural.first()).unwrap();
        assert!(first.start_lba <= list[plain_cell].start_lba, "ordered by position without the preference");
        let all = ifo_candidates(&plan, caps, CandidateOptions { show_interleaved: true, ..opts }).unwrap();
        assert!(all.iter().any(|c| c.cell == 4 && c.interleaved));

        // The same list comes out of the written image.
        let iso = dvd_image(&dir, "T", "t.iso");
        assert_eq!(ifo_candidates_iso(&iso, caps, opts).unwrap(), list);
        // An image that fits one layer easily still yields none when L1 would exceed L0's cap.
        let tight = Capacities { single_layer: 10, layer: 10 };
        assert!(ifo_candidates(&plan, tight, opts).unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
