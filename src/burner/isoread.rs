//! Read-only ISO 9660 and UDF (1.02 / 2.50) access to an image file (or anything `Read + Seek`):
//! directory listing, file lookup, file contents and a sector-to-path map for mapping miscompares
//! to files.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};

use super::udf::SECTOR;

const FIRST_DESCRIPTOR: u64 = 16;
const UDF_ANCHOR: u64 = 256;
/// Directory nesting limit; ISO 9660 allows 8, the guard stops loops in damaged images.
const MAX_DEPTH: usize = 16;

#[derive(Debug)]
pub enum IsoError {
    Io(io::Error),
    /// No ISO 9660 primary volume descriptor at sector 16.
    NotIso,
    /// No UDF volume recognition sequence / anchor volume descriptor pointer.
    NotUdf,
    Corrupt(&'static str),
    NotFound(String),
}

impl fmt::Display for IsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IsoError::Io(e) => e.fmt(f),
            IsoError::NotIso => write!(f, "not an ISO 9660 image (primary volume descriptor missing)"),
            IsoError::NotUdf => write!(f, "not a UDF image (anchor or volume descriptor missing)"),
            IsoError::Corrupt(m) => write!(f, "corrupt ISO image: {m}"),
            IsoError::NotFound(p) => write!(f, "{p} not found in the image"),
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
    file_len: u64,
    pub label: String,
    pub total_sectors: u32,
}

fn ucs2(b: &[u8]) -> String {
    char::decode_utf16(b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]]))).map(|r| r.unwrap_or('?')).collect()
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
        return Err(IsoError::Corrupt("invalid directory entry"));
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
        let (root, _, _) = record(&sector[156..190], false)?.ok_or(IsoError::Corrupt("root directory missing"))?;
        Ok(IsoImage {
            r,
            ucs: joliet,
            root,
            file_len: end,
            label: label.trim_end().to_string(),
            total_sectors: le32(&sector, 80),
        })
    }

    fn read_bytes(&mut self, lba: u32, len: u64) -> Result<Vec<u8>, IsoError> {
        let start = lba as u64 * SECTOR;
        if start.checked_add(len).is_none_or(|e| e > self.file_len) {
            return Err(IsoError::Corrupt("extent past end of image"));
        }
        let mut buf = vec![0u8; len as usize];
        self.r.seek(SeekFrom::Start(start))?;
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Entries of `dir` (without `.` and `..`); multi-extent files are merged.
    pub fn list_entry(&mut self, dir: &IsoEntry) -> Result<Vec<IsoEntry>, IsoError> {
        let (lba, len) = *dir.extents.first().ok_or(IsoError::Corrupt("directory without extent"))?;
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
        let mut visited = HashSet::new();
        self.walk(&root, "", 0, &mut visited, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk(
        &mut self,
        dir: &IsoEntry,
        prefix: &str,
        depth: usize,
        visited: &mut HashSet<u32>,
        out: &mut Vec<(u64, u64, String)>,
    ) -> Result<(), IsoError> {
        if depth > MAX_DEPTH {
            return Err(IsoError::Corrupt("directories nested too deeply"));
        }
        let lba = dir.first_sector().ok_or(IsoError::Corrupt("directory without extent"))?;
        if !visited.insert(lba) {
            return Err(IsoError::Corrupt("self-referencing or cyclic directory"));
        }
        for e in self.list_entry(dir)? {
            let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            if e.dir {
                self.walk(&e, &path, depth + 1, visited, out)?;
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

/// Path of the file holding `lba`, from a sorted [`IsoImage::extent_map`] or [`UdfImage::extent_map`].
pub fn path_at(map: &[(u64, u64, String)], lba: u64) -> Option<&str> {
    let i = map.partition_point(|e| e.0 <= lba);
    map[..i].last().filter(|e| lba < e.1).map(|e| e.2.as_str())
}

// ---------------------------------------------------------------------------------------------
// UDF 1.02 / 2.50 reader.

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn decode_osta(b: &[u8]) -> String {
    match b.first().copied() {
        Some(8) => b[1..].iter().map(|&c| c as char).collect(),
        Some(16) => ucs2(&b[1..]),
        _ => String::new(),
    }
}

fn decode_dstring(b: &[u8]) -> String {
    let Some(&len) = b.last() else { return String::new() };
    let len = len as usize;
    if len <= 1 || len >= b.len() {
        return String::new();
    }
    decode_osta(&b[..len])
}

#[derive(Debug, Clone)]
enum UdfPartition {
    Physical { start: u32, len: u32 },
    Metadata { extents: Vec<(u32, u32)> },
}

#[derive(Debug, Clone)]
struct UdfNode {
    entry: IsoEntry,
    icb: (u16, u32),
    inline: Option<Vec<u8>>,
}

/// Read-only UDF 1.02 / 2.50 image reader (supports both physical-only UDF and UDF 2.50
/// metadata partitions as used on Blu-ray / AVCHD).
pub struct UdfImage<R: Read + Seek> {
    r: R,
    file_len: u64,
    partitions: Vec<UdfPartition>,
    root: UdfNode,
    pub label: String,
    pub total_sectors: u32,
}

impl<R: Read + Seek> UdfImage<R> {
    pub fn open(mut r: R) -> Result<Self, IsoError> {
        let end = r.seek(SeekFrom::End(0))?;
        let mut sec = vec![0u8; SECTOR as usize];

        // 1. Volume Recognition Sequence (16..32): BEA01 + NSR02/NSR03.
        let mut has_nsr = false;
        for s in FIRST_DESCRIPTOR..32 {
            if (s + 1) * SECTOR > end {
                break;
            }
            r.seek(SeekFrom::Start(s * SECTOR))?;
            r.read_exact(&mut sec)?;
            if &sec[1..6] == b"NSR02" || &sec[1..6] == b"NSR03" {
                has_nsr = true;
                break;
            }
            if &sec[1..6] == b"TEA01" {
                break;
            }
        }
        if !has_nsr || (UDF_ANCHOR + 1) * SECTOR > end {
            return Err(IsoError::NotUdf);
        }

        // 2. Anchor Volume Descriptor Pointer at sector 256.
        r.seek(SeekFrom::Start(UDF_ANCHOR * SECTOR))?;
        r.read_exact(&mut sec)?;
        if le16(&sec, 0) != 2 {
            return Err(IsoError::NotUdf);
        }
        let main_len = le32(&sec, 16);
        let main_loc = le32(&sec, 20);
        let vds_sectors = (main_len as u64).div_ceil(SECTOR).clamp(1, 64);

        // 3. Volume Descriptor Sequence.
        let mut pvd_label = String::new();
        let mut lvd_label = String::new();
        let mut phys_parts: HashMap<u16, (u32, u32)> = HashMap::new();
        let mut raw_maps: Vec<RawMap> = Vec::new();
        let mut fsd_loc: Option<(u16, u32)> = None;

        for i in 0..vds_sectors {
            let lba = main_loc as u64 + i;
            if (lba + 1) * SECTOR > end {
                return Err(IsoError::Corrupt("VDS past end of image"));
            }
            r.seek(SeekFrom::Start(lba * SECTOR))?;
            r.read_exact(&mut sec)?;
            match le16(&sec, 0) {
                0 | 8 => break,
                1 => pvd_label = decode_dstring(&sec[24..56]),
                5 => {
                    let num = le16(&sec, 22);
                    let start = le32(&sec, 188);
                    let len = le32(&sec, 192);
                    phys_parts.insert(num, (start, len));
                }
                6 => {
                    lvd_label = decode_dstring(&sec[84..212]);
                    if le32(&sec, 212) as u64 != SECTOR {
                        return Err(IsoError::Corrupt("unsupported UDF block size"));
                    }
                    fsd_loc = Some((le16(&sec, 256), le32(&sec, 252)));
                    let num_maps = le32(&sec, 268) as usize;
                    let mut off = 440usize;
                    for _ in 0..num_maps {
                        if off + 2 > sec.len() {
                            return Err(IsoError::Corrupt("truncated partition map"));
                        }
                        let map_type = sec[off];
                        let map_len = sec[off + 1] as usize;
                        if map_len < 6 || off + map_len > sec.len() {
                            return Err(IsoError::Corrupt("invalid partition map"));
                        }
                        match map_type {
                            1 => raw_maps.push(RawMap::Physical { part_num: le16(&sec, off + 4) }),
                            2 if map_len >= 64 && sec[off + 5..].starts_with(b"*UDF Metadata Partition") => {
                                raw_maps.push(RawMap::Metadata {
                                    part_num: le16(&sec, off + 38),
                                    meta_file_blk: le32(&sec, off + 40),
                                });
                            }
                            _ => return Err(IsoError::Corrupt("unsupported UDF partition map")),
                        }
                        off += map_len;
                    }
                }
                _ => {}
            }
        }

        let (fsd_part, fsd_blk) = fsd_loc.ok_or(IsoError::NotUdf)?;
        if raw_maps.is_empty() {
            return Err(IsoError::NotUdf);
        }

        let mut partitions = Vec::with_capacity(raw_maps.len());
        for m in &raw_maps {
            match *m {
                RawMap::Physical { part_num } => {
                    let &(start, len) = phys_parts.get(&part_num).ok_or(IsoError::Corrupt("missing partition descriptor"))?;
                    partitions.push(UdfPartition::Physical { start, len });
                }
                RawMap::Metadata { part_num, meta_file_blk } => {
                    let &(start, len) = phys_parts.get(&part_num).ok_or(IsoError::Corrupt("missing metadata backing partition"))?;
                    let meta_parts = [UdfPartition::Physical { start, len }];
                    let (_dir, _size, extents, _inline) = read_fe(&mut r, end, &meta_parts, 0, meta_file_blk)?;
                    let sec_extents = extents.into_iter().map(|(lba, bytes)| (lba, bytes.div_ceil(SECTOR as u32))).collect();
                    partitions.push(UdfPartition::Metadata { extents: sec_extents });
                }
            }
        }

        // 4. File Set Descriptor.
        let fsd_lba = resolve_block(&partitions, fsd_part, fsd_blk)?;
        read_sector_into(&mut r, end, fsd_lba, &mut sec)?;
        if le16(&sec, 0) != 256 {
            return Err(IsoError::Corrupt("file set descriptor missing"));
        }
        let root_icb = (le16(&sec, 408), le32(&sec, 404));
        let (root_dir, root_size, root_extents, root_inline) = read_fe(&mut r, end, &partitions, root_icb.0, root_icb.1)?;
        if !root_dir {
            return Err(IsoError::Corrupt("UDF root is not a directory"));
        }
        let root = UdfNode {
            entry: IsoEntry { name: String::new(), dir: true, size: root_size, extents: root_extents },
            icb: root_icb,
            inline: root_inline,
        };
        let label = if !lvd_label.is_empty() { lvd_label } else { pvd_label };
        Ok(UdfImage {
            r,
            file_len: end,
            partitions,
            root,
            label: label.trim_end().to_string(),
            total_sectors: (end / SECTOR) as u32,
        })
    }

    fn read_node_bytes(&mut self, node: &UdfNode) -> Result<Vec<u8>, IsoError> {
        if let Some(inline) = &node.inline {
            return Ok(inline.clone());
        }
        let mut out = Vec::with_capacity(node.entry.size as usize);
        for &(lba, len) in &node.entry.extents {
            out.extend(read_bytes_raw(&mut self.r, self.file_len, lba, len as u64)?);
        }
        out.truncate(node.entry.size as usize);
        Ok(out)
    }

    fn list_node(&mut self, dir: &UdfNode) -> Result<Vec<UdfNode>, IsoError> {
        let data = self.read_node_bytes(dir)?;
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            if pos + 38 > data.len() || le16(&data, pos) != 257 {
                return Err(IsoError::Corrupt("invalid UDF file identifier descriptor"));
            }
            let chars = data[pos + 18];
            let l_fi = data[pos + 19] as usize;
            let icb_blk = le32(&data, pos + 24);
            let icb_part = le16(&data, pos + 28);
            let l_iu = le16(&data, pos + 36) as usize;
            let rec_len = (38 + l_iu + l_fi).div_ceil(4) * 4;
            if pos + rec_len > data.len() {
                return Err(IsoError::Corrupt("truncated UDF file identifier descriptor"));
            }
            let name_start = pos + 38 + l_iu;
            let name = decode_osta(&data[name_start..name_start + l_fi]);
            pos += rec_len;
            if chars & 0x0C != 0 {
                continue; // deleted or parent ("..")
            }
            let (is_dir, size, extents, inline) = read_fe(&mut self.r, self.file_len, &self.partitions, icb_part, icb_blk)?;
            out.push(UdfNode {
                entry: IsoEntry { name, dir: is_dir, size, extents },
                icb: (icb_part, icb_blk),
                inline,
            });
        }
        Ok(out)
    }

    fn find_node(&mut self, path: &str) -> Result<UdfNode, IsoError> {
        let mut cur = self.root.clone();
        for part in path.split(['/', '\\']).filter(|p| !p.is_empty()) {
            if !cur.entry.dir {
                return Err(IsoError::NotFound(path.to_string()));
            }
            cur = self
                .list_node(&cur)?
                .into_iter()
                .find(|n| n.entry.name.eq_ignore_ascii_case(part))
                .ok_or_else(|| IsoError::NotFound(path.to_string()))?;
        }
        Ok(cur)
    }

    pub fn list(&mut self, path: &str) -> Result<Vec<IsoEntry>, IsoError> {
        let dir = self.find_node(path)?;
        if !dir.entry.dir {
            return Err(IsoError::NotFound(path.to_string()));
        }
        Ok(self.list_node(&dir)?.into_iter().map(|n| n.entry).collect())
    }

    /// Entry at `path` (`BDMV/index.bdmv`, case-insensitive, empty = root).
    pub fn find(&mut self, path: &str) -> Result<IsoEntry, IsoError> {
        self.find_node(path).map(|n| n.entry)
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, IsoError> {
        let node = self.find_node(path)?;
        if node.entry.dir {
            return Err(IsoError::NotFound(path.to_string()));
        }
        self.read_node_bytes(&node)
    }

    /// Sorted `(first sector, sector after the last, path)` of every file extent.
    pub fn extent_map(&mut self) -> Result<Vec<(u64, u64, String)>, IsoError> {
        let mut out = Vec::new();
        let root = self.root.clone();
        let mut visited = HashSet::new();
        self.walk(&root, "", 0, &mut visited, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk(
        &mut self,
        dir: &UdfNode,
        prefix: &str,
        depth: usize,
        visited: &mut HashSet<(u16, u32)>,
        out: &mut Vec<(u64, u64, String)>,
    ) -> Result<(), IsoError> {
        if depth > MAX_DEPTH {
            return Err(IsoError::Corrupt("directories nested too deeply"));
        }
        if !visited.insert(dir.icb) {
            return Err(IsoError::Corrupt("self-referencing or cyclic directory"));
        }
        for n in self.list_node(dir)? {
            let path = if prefix.is_empty() { n.entry.name.clone() } else { format!("{prefix}/{}", n.entry.name) };
            if n.entry.dir {
                self.walk(&n, &path, depth + 1, visited, out)?;
            } else {
                for &(lba, len) in &n.entry.extents {
                    if len > 0 {
                        out.push((lba as u64, lba as u64 + (len as u64).div_ceil(SECTOR), path.clone()));
                    }
                }
            }
        }
        Ok(())
    }
}

enum RawMap {
    Physical { part_num: u16 },
    Metadata { part_num: u16, meta_file_blk: u32 },
}

fn resolve_block(partitions: &[UdfPartition], part_ref: u16, block: u32) -> Result<u32, IsoError> {
    match partitions.get(part_ref as usize) {
        Some(UdfPartition::Physical { start, len }) => {
            if block >= *len {
                Err(IsoError::Corrupt("UDF block past end of partition"))
            } else {
                Ok(*start + block)
            }
        }
        Some(UdfPartition::Metadata { extents }) => {
            let mut rem = block;
            for &(abs_start, count) in extents {
                if rem < count {
                    return Ok(abs_start + rem);
                }
                rem -= count;
            }
            Err(IsoError::Corrupt("UDF block past end of metadata partition"))
        }
        None => Err(IsoError::Corrupt("invalid UDF partition reference")),
    }
}

fn read_bytes_raw<R: Read + Seek>(r: &mut R, file_len: u64, lba: u32, len: u64) -> Result<Vec<u8>, IsoError> {
    let start = lba as u64 * SECTOR;
    if start.checked_add(len).is_none_or(|e| e > file_len) {
        return Err(IsoError::Corrupt("extent past end of image"));
    }
    let mut buf = vec![0u8; len as usize];
    r.seek(SeekFrom::Start(start))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_sector_into<R: Read + Seek>(r: &mut R, file_len: u64, lba: u32, buf: &mut [u8]) -> Result<(), IsoError> {
    let start = lba as u64 * SECTOR;
    if start.checked_add(SECTOR).is_none_or(|e| e > file_len) {
        return Err(IsoError::Corrupt("sector past end of image"));
    }
    r.seek(SeekFrom::Start(start))?;
    r.read_exact(buf)?;
    Ok(())
}

type ParsedFe = (bool, u64, Vec<(u32, u32)>, Option<Vec<u8>>);

fn read_fe<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
    partitions: &[UdfPartition],
    part_ref: u16,
    block: u32,
) -> Result<ParsedFe, IsoError> {
    let mut sec = vec![0u8; SECTOR as usize];
    read_sector_into(r, file_len, resolve_block(partitions, part_ref, block)?, &mut sec)?;
    let (ad_off, l_ad) = match le16(&sec, 0) {
        261 => (176 + le32(&sec, 168) as usize, le32(&sec, 172) as usize),
        266 => (216 + le32(&sec, 208) as usize, le32(&sec, 212) as usize),
        _ => return Err(IsoError::Corrupt("expected UDF file entry")),
    };
    if ad_off.checked_add(l_ad).is_none_or(|e| e > sec.len()) {
        return Err(IsoError::Corrupt("invalid UDF allocation descriptors"));
    }
    let dir = sec[27] == 4;
    let alloc_type = le16(&sec, 34) & 0x07;
    let info_len = le64(&sec, 56);
    let ads = &sec[ad_off..ad_off + l_ad];
    if alloc_type == 3 {
        let inline_len = (info_len as usize).min(ads.len());
        return Ok((dir, info_len, Vec::new(), Some(ads[..inline_len].to_vec())));
    }
    let mut extents = Vec::new();
    let mut cur_part = part_ref;
    let mut cur_buf = ads.to_vec();
    for _hop in 0..64 {
        let mut next: Option<(u16, u32, u32)> = None;
        match alloc_type {
            0 => {
                for c in cur_buf.as_chunks::<8>().0 {
                    let raw = le32(c, 0);
                    let len = raw & 0x3FFF_FFFF;
                    let ty = raw >> 30;
                    let blk = le32(c, 4);
                    if ty == 3 {
                        next = Some((cur_part, blk, len));
                        break;
                    } else if ty == 0 && len > 0 {
                        extents.push((resolve_block(partitions, cur_part, blk)?, len));
                    }
                }
            }
            1 => {
                for c in cur_buf.as_chunks::<16>().0 {
                    let raw = le32(c, 0);
                    let len = raw & 0x3FFF_FFFF;
                    let ty = raw >> 30;
                    let blk = le32(c, 4);
                    let p = le16(c, 8);
                    if ty == 3 {
                        next = Some((p, blk, len));
                        break;
                    } else if ty == 0 && len > 0 {
                        extents.push((resolve_block(partitions, p, blk)?, len));
                    }
                }
            }
            _ => return Err(IsoError::Corrupt("unsupported UDF allocation type")),
        }
        let Some((np, nblk, nlen)) = next else { break };
        cur_part = np;
        cur_buf = read_bytes_raw(r, file_len, resolve_block(partitions, np, nblk)?, nlen as u64)?;
    }
    Ok((dir, info_len, extents, None))
}

/// Unified reader for both ISO 9660 (+ bridge) and UDF-only disc images.
pub enum DiscImage<R: Read + Seek> {
    Iso(IsoImage<R>),
    Udf(UdfImage<R>),
}

impl<R: Read + Seek> DiscImage<R> {
    /// Opens an ISO 9660 image or, if no primary volume descriptor is present, a UDF-only image.
    pub fn open(mut r: R) -> Result<Self, IsoError> {
        let end = r.seek(SeekFrom::End(0))?;
        if end < (FIRST_DESCRIPTOR + 1) * SECTOR {
            return Err(IsoError::NotIso);
        }
        let mut sec = [0u8; SECTOR as usize];
        r.seek(SeekFrom::Start(FIRST_DESCRIPTOR * SECTOR))?;
        r.read_exact(&mut sec)?;
        r.seek(SeekFrom::Start(0))?;
        if &sec[1..6] == b"CD001" {
            Ok(DiscImage::Iso(IsoImage::open(r)?))
        } else {
            Ok(DiscImage::Udf(UdfImage::open(r)?))
        }
    }

    pub fn label(&self) -> &str {
        match self {
            DiscImage::Iso(i) => &i.label,
            DiscImage::Udf(u) => &u.label,
        }
    }

    pub fn total_sectors(&self) -> u32 {
        match self {
            DiscImage::Iso(i) => i.total_sectors,
            DiscImage::Udf(u) => u.total_sectors,
        }
    }

    pub fn list(&mut self, path: &str) -> Result<Vec<IsoEntry>, IsoError> {
        match self {
            DiscImage::Iso(i) => i.list(path),
            DiscImage::Udf(u) => u.list(path),
        }
    }

    pub fn find(&mut self, path: &str) -> Result<IsoEntry, IsoError> {
        match self {
            DiscImage::Iso(i) => i.find(path),
            DiscImage::Udf(u) => u.find(path),
        }
    }

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, IsoError> {
        match self {
            DiscImage::Iso(i) => i.read_file(path),
            DiscImage::Udf(u) => u.read_file(path),
        }
    }

    pub fn extent_map(&mut self) -> Result<Vec<(u64, u64, String)>, IsoError> {
        match self {
            DiscImage::Iso(i) => i.extent_map(),
            DiscImage::Udf(u) => u.extent_map(),
        }
    }
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

    fn mk_record(lba: u32, size: u32, flags: u8, name: &[u8]) -> Vec<u8> {
        let len = 33 + name.len() + (33 + name.len()) % 2;
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[25] = flags;
        r[32] = name.len() as u8;
        r[33..33 + name.len()].copy_from_slice(name);
        r
    }

    fn mk_dir_sector(self_lba: u32, parent_lba: u32, entries: &[Vec<u8>]) -> Vec<u8> {
        let mut dir = mk_record(self_lba, SECTOR as u32, 2, &[0]);
        dir.extend(mk_record(parent_lba, SECTOR as u32, 2, &[1]));
        for e in entries {
            dir.extend_from_slice(e);
        }
        dir
    }

    fn minimal_iso(sectors: usize, root_size: u32, dirs: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut img = vec![0u8; sectors * SECTOR as usize];
        let pvd = 16 * SECTOR as usize;
        img[pvd] = 1;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        let root = mk_record(18, root_size, 2, &[0]);
        img[pvd + 156..pvd + 156 + root.len()].copy_from_slice(&root);
        for (lba, data) in dirs {
            let off = *lba * SECTOR as usize;
            img[off..off + data.len()].copy_from_slice(data);
        }
        img
    }

    #[test]
    fn multi_extent_files_are_merged() {
        // Hand-made directory: file "BIG.BIN;1" in two extents (flag 0x80 on the first).
        let dir = mk_dir_sector(
            18,
            18,
            &[
                mk_record(100, 0xFFFF_F800, 0x80, b"BIG.BIN;1"),
                mk_record(300, 4096, 0, b"BIG.BIN;1"),
            ],
        );
        let img = minimal_iso(20, SECTOR as u32, &[(18, dir)]);
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        let e = iso.find("BIG.BIN").unwrap();
        assert_eq!(e.extents, [(100, 0xFFFF_F800), (300, 4096)]);
        assert_eq!(e.size, 0xFFFF_F800 + 4096);
    }

    #[test]
    fn direct_self_referencing_directory_rejected() {
        let dir18 = mk_dir_sector(18, 18, &[mk_record(18, SECTOR as u32, 2, b"LOOP")]);
        let img = minimal_iso(20, SECTOR as u32, &[(18, dir18)]);
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));
    }

    #[test]
    fn two_node_directory_cycle_rejected() {
        let dir18 = mk_dir_sector(18, 18, &[mk_record(19, SECTOR as u32, 2, b"A")]);
        let dir19 = mk_dir_sector(19, 18, &[mk_record(20, SECTOR as u32, 2, b"B")]);
        let dir20 = mk_dir_sector(20, 19, &[mk_record(19, SECTOR as u32, 2, b"A")]);
        let img = minimal_iso(22, SECTOR as u32, &[(18, dir18), (19, dir19), (20, dir20)]);
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));
    }

    #[test]
    fn branching_shared_directory_dag_rejected() {
        // Shared DAG: root 18 has multiple entries pointing to the same directory LBA 19.
        let dir18 = mk_dir_sector(
            18,
            18,
            &[
                mk_record(19, SECTOR as u32, 2, b"D1"),
                mk_record(19, SECTOR as u32, 2, b"D2"),
            ],
        );
        let dir19 = mk_dir_sector(19, 18, &[]);
        let img = minimal_iso(21, SECTOR as u32, &[(18, dir18), (19, dir19)]);
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));

        // Branching cyclic DAG (would trigger exponential reads without visited set).
        let cyclic18 = mk_dir_sector(
            18,
            18,
            &[
                mk_record(19, SECTOR as u32, 2, b"B1"),
                mk_record(19, SECTOR as u32, 2, b"B2"),
                mk_record(19, SECTOR as u32, 2, b"B3"),
                mk_record(19, SECTOR as u32, 2, b"B4"),
            ],
        );
        let cyclic19 = mk_dir_sector(
            19,
            18,
            &[
                mk_record(18, SECTOR as u32, 2, b"R1"),
                mk_record(18, SECTOR as u32, 2, b"R2"),
                mk_record(18, SECTOR as u32, 2, b"R3"),
                mk_record(18, SECTOR as u32, 2, b"R4"),
            ],
        );
        let img = minimal_iso(21, SECTOR as u32, &[(18, cyclic18), (19, cyclic19)]);
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));
    }

    #[test]
    fn out_of_bounds_directory_extent_rejected_without_oom() {
        let dir18 = mk_dir_sector(18, 18, &[]);
        let img_huge_root = minimal_iso(20, u32::MAX, &[(18, dir18)]);
        let mut iso = IsoImage::open(Cursor::new(img_huge_root)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));

        let dir18_huge_sub = mk_dir_sector(18, 18, &[mk_record(19, u32::MAX, 2, b"HUGE")]);
        let img_huge_sub = minimal_iso(20, SECTOR as u32, &[(18, dir18_huge_sub)]);
        let mut iso = IsoImage::open(Cursor::new(img_huge_sub)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));

        let dir18_oob_lba = mk_dir_sector(18, 18, &[mk_record(999, SECTOR as u32, 2, b"OOB")]);
        let img_oob_lba = minimal_iso(20, SECTOR as u32, &[(18, dir18_oob_lba)]);
        let mut iso = IsoImage::open(Cursor::new(img_oob_lba)).unwrap();
        assert!(matches!(iso.extent_map(), Err(IsoError::Corrupt(_))));
    }

    #[test]
    fn rejects_non_iso() {
        assert!(matches!(IsoImage::open(Cursor::new(vec![0u8; 40 * 2048])), Err(IsoError::NotIso)));
        assert!(matches!(IsoImage::open(Cursor::new(vec![0u8; 100])), Err(IsoError::NotIso)));
        assert!(matches!(UdfImage::open(Cursor::new(vec![0u8; 40 * 2048])), Err(IsoError::NotUdf)));
        assert!(matches!(UdfImage::open(Cursor::new(vec![0u8; 100])), Err(IsoError::NotUdf)));
    }

    #[test]
    fn reads_udf250_bluray_and_udf102_images() {
        use crate::burner::udf250::{plan_bd_image, write_bd_image};

        let dir = crate::burner::testutil::synthetic_video_ts("udf_read");
        // 1. UDF 1.02 side of a DVD-Video image.
        let video_ts = dir.join("VIDEO_TS");
        let dvd_plan = plan_image(&video_ts, "UDF102", &HashMap::new(), &HashMap::new()).unwrap();
        let dvd_iso = dir.join("dvd.iso");
        let ts = Timestamp { year: 2026, month: 11, day: 25, hour: 12, minute: 0, second: 0 };
        write_image(&dvd_plan, &dvd_iso, &ts, &AtomicBool::new(false), |_| {}).unwrap();
        let dvd_bytes = std::fs::read(&dvd_iso).unwrap();
        let mut udf102 = UdfImage::open(Cursor::new(dvd_bytes)).unwrap();
        assert_eq!(udf102.label, "UDF102");
        let want_vob = std::fs::read(video_ts.join("VTS_01_1.VOB")).unwrap();
        assert_eq!(udf102.read_file("VIDEO_TS/VTS_01_1.VOB").unwrap(), want_vob);
        let map102 = udf102.extent_map().unwrap();
        let vob_sec = udf102.find("VIDEO_TS/VTS_01_1.VOB").unwrap().first_sector().unwrap() as u64;
        assert_eq!(path_at(&map102, vob_sec), Some("VIDEO_TS/VTS_01_1.VOB"));

        // 2. UDF 2.50 (UDF-only, no ISO 9660 bridge) Blu-ray image.
        let bd_root = dir.join("bd");
        std::fs::create_dir_all(bd_root.join("BDMV").join("STREAM")).unwrap();
        std::fs::create_dir_all(bd_root.join("CERTIFICATE")).unwrap();
        std::fs::write(bd_root.join("BDMV").join("index.bdmv"), b"INDX0200").unwrap();
        let m2ts: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(bd_root.join("BDMV").join("STREAM").join("00000.m2ts"), &m2ts).unwrap();

        let bd_plan = plan_bd_image(&bd_root, "BLURAY_TEST").unwrap();
        let bd_iso = dir.join("bd.iso");
        write_bd_image(&bd_plan, &bd_iso, &ts, &AtomicBool::new(false), |_| {}).unwrap();
        let bd_bytes = std::fs::read(&bd_iso).unwrap();

        assert!(matches!(IsoImage::open(Cursor::new(&bd_bytes)), Err(IsoError::NotIso)));
        let mut disc = DiscImage::open(Cursor::new(&bd_bytes)).unwrap();
        assert_eq!(disc.label(), "BLURAY_TEST");
        let root_names: Vec<String> = disc.list("").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(root_names, ["BDMV", "CERTIFICATE"]);
        assert_eq!(disc.read_file("BDMV/index.bdmv").unwrap(), b"INDX0200");
        assert_eq!(disc.read_file("bdmv/stream/00000.m2ts").unwrap(), m2ts);
        assert!(matches!(disc.find("BDMV/MISSING"), Err(IsoError::NotFound(_))));
        let map = disc.extent_map().unwrap();
        let m2ts_sec = disc.find("BDMV/STREAM/00000.m2ts").unwrap().first_sector().unwrap() as u64;
        assert_eq!(path_at(&map, m2ts_sec), Some("BDMV/STREAM/00000.m2ts"));
        assert_eq!(path_at(&map, m2ts_sec + 2), Some("BDMV/STREAM/00000.m2ts"));

        std::fs::remove_dir_all(dir).ok();
    }
}
