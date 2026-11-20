//! Disc image writer: UDF 1.02 + ISO 9660 bridge (the format DVD players expect), for DVD-Video
//! (`plan_image`) and for any file tree (`plan_tree`, optionally with Joliet).
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

use std::collections::{HashMap, HashSet};
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
const ISO_DIRS: u32 = 68;
const ANCHOR: u32 = 256;
const PARTITION_START: u32 = 257;
/// Longest extent a UDF short_ad can describe (30-bit length, sector aligned).
pub(crate) const MAX_EXTENT: u64 = (1 << 30) - SECTOR;

#[derive(Debug)]
pub enum ImageError {
    Io { path: PathBuf, error: std::io::Error },
    NotDvdVideo(String),
    /// The folder is no (complete) `BDMV` structure.
    NotBluray(String),
    /// A file tree that cannot be laid out (empty path, name too long, conflicting entries).
    Invalid(String),
    Cancelled,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            ImageError::NotDvdVideo(m) => write!(f, "not a valid VIDEO_TS structure: {m}"),
            ImageError::NotBluray(m) => write!(f, "not a valid BDMV structure: {m}"),
            ImageError::Invalid(m) => write!(f, "invalid file structure: {m}"),
            ImageError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ImageError {}

pub(crate) fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ImageError + '_ {
    move |error| ImageError::Io { path: path.to_path_buf(), error }
}

/// Applied to a file's data while it is copied into the image: `(chunk, offset of the chunk in
/// the file)`. Used for the CPI reset of M2TS files.
pub type Transform = fn(&mut [u8], u64);

/// One file or directory of the image, with a `/`-separated path from the image root.
#[derive(Debug, Clone, Default)]
pub struct TreeEntry {
    pub path: String,
    pub source: PathBuf,
    pub size: u64,
    pub dir: bool,
    /// Replaces the file content (patched IFO/BUP); `size` is then the length of `data`.
    pub data: Option<Vec<u8>>,
    /// Zero sectors written in front of the file (layer break alignment).
    pub padding_before: u32,
    pub transform: Option<Transform>,
}

impl TreeEntry {
    pub fn dir(path: &str) -> Self {
        TreeEntry { path: path.into(), dir: true, ..Default::default() }
    }

    pub fn file(path: &str, source: PathBuf, size: u64) -> Self {
        TreeEntry { path: path.into(), source, size, ..Default::default() }
    }
}

/// Recursively lists `dir` as tree entries below `rel` (the directory itself included).
pub fn scan(dir: &Path, rel: &str, out: &mut Vec<TreeEntry>) -> Result<(), ImageError> {
    out.push(TreeEntry { source: dir.to_path_buf(), ..TreeEntry::dir(rel) });
    for entry in std::fs::read_dir(dir).map_err(io(dir))? {
        let entry = entry.map_err(io(dir))?;
        let path = entry.path();
        let meta = std::fs::metadata(&path).map_err(io(&path))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{rel}/{name}");
        if meta.is_dir() {
            scan(&path, &rel, out)?;
        } else {
            out.push(TreeEntry::file(&rel, path, meta.len()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct TreeOptions {
    /// Add a Joliet tree (long Unicode names for Windows) next to ISO 9660 level 1.
    pub joliet: bool,
    /// First sector the file data may start at (0 = right behind the file system structures).
    pub start_lba: u32,
    /// Pad the image with zero sectors up to this size.
    pub min_sectors: u32,
    /// Files of equal size and MD5 share one extent.
    pub dedupe: bool,
}

/// One file of the image as placed in it.
#[derive(Debug, Clone)]
pub struct ImageFile {
    /// `VIDEO_TS.IFO`, `VTS_01_1.VOB`, ...
    pub name: String,
    /// Path from the image root (`VIDEO_TS/VIDEO_TS.IFO`).
    pub path: String,
    pub source: PathBuf,
    pub size: u64,
    /// Replaces the file content (patched IFO/BUP after a layer-break shift).
    pub data: Option<Vec<u8>>,
    /// Zero sectors written in front of the file (layer break alignment).
    pub padding_before: u32,
    /// First sector of the file data (absolute), set by the layout.
    pub lba: u32,
    /// Index of the earlier file whose extent this one shares (dedupe); it stores no data itself.
    pub duplicate_of: Option<usize>,
    pub transform: Option<Transform>,
}

impl ImageFile {
    pub fn sectors(&self) -> u32 {
        self.size.div_ceil(SECTOR) as u32
    }

    pub fn is_duplicate(&self) -> bool {
        self.duplicate_of.is_some()
    }
}

#[derive(Debug, Clone)]
struct Node {
    name: String,
    parent: usize,
    dir: bool,
    children: Vec<usize>,
    /// Index into `ImagePlan::files`.
    file: Option<usize>,
    unique: u32,
    /// Partition block of the file entry.
    fe: u32,
    /// Directories: first partition block of the FIDs and their length in bytes.
    data: u32,
    dir_bytes: u64,
}

/// One ISO 9660 tree (the primary one or the Joliet one).
#[derive(Debug, Clone)]
struct IsoSide {
    joliet: bool,
    /// Identifier per node (without version); `None` = not part of this tree.
    names: Vec<Option<Vec<u8>>>,
    /// Children in this tree per directory node, sorted by identifier.
    kids: Vec<Vec<usize>>,
    /// Directories in path table order, and each node's position in it.
    dirs: Vec<usize>,
    dir_pos: Vec<usize>,
    dir_lba: Vec<u32>,
    dir_sectors: Vec<u32>,
    pt_size: u32,
    pt_l: u32,
    pt_m: u32,
}

impl IsoSide {
    fn pt_sectors(&self) -> u32 {
        self.pt_size.div_ceil(SECTOR as u32).max(1)
    }

    fn area_sectors(&self) -> u32 {
        2 * self.pt_sectors() + self.dir_sectors.iter().sum::<u32>()
    }
}

/// A fully laid out image.
#[derive(Debug, Clone)]
pub struct ImagePlan {
    pub label: String,
    /// Files in the order the caller gave them (their data follows in this order).
    pub files: Vec<ImageFile>,
    /// Total sectors of the image.
    pub total_sectors: u32,
    /// Things left out of the ISO 9660 side (the UDF side always has everything).
    pub warnings: Vec<String>,
    nodes: Vec<Node>,
    files_node: Vec<usize>,
    /// Nodes in pre-order (parents first, children sorted by name).
    order: Vec<usize>,
    partition_start: u32,
    sides: Vec<IsoSide>,
    next_unique: u64,
    dir_count: u32,
}

impl ImagePlan {
    /// The file called `name` (or at `name` as a path), case-insensitive.
    pub fn file(&self, name: &str) -> Option<&ImageFile> {
        self.files.iter().find(|f| f.name.eq_ignore_ascii_case(name) || f.path.eq_ignore_ascii_case(name))
    }

    pub fn size_bytes(&self) -> u64 {
        self.total_sectors as u64 * SECTOR
    }

    /// First sector of the UDF partition.
    pub fn partition_start(&self) -> u32 {
        self.partition_start
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
            None if required => return Err(ImageError::NotDvdVideo(format!("{name} missing"))),
            None => {}
        }
    }
    if let Some(extra) = present.keys().next() {
        return Err(ImageError::NotDvdVideo(format!("unexpected file {extra}")));
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
    plan_image_with(video_ts, label, padding, overrides, true)
}

/// [`plan_image`] with the choice to leave out the (empty) `AUDIO_TS` directory.
pub fn plan_image_with(
    video_ts: &Path,
    label: &str,
    padding: &HashMap<u32, u32>,
    overrides: &HashMap<String, Vec<u8>>,
    audio_ts: bool,
) -> Result<ImagePlan, ImageError> {
    let mut entries = vec![TreeEntry::dir("VIDEO_TS")];
    if audio_ts {
        entries.insert(0, TreeEntry::dir("AUDIO_TS"));
    }
    for (name, source, size) in dvd_file_order(video_ts)? {
        let vts = name.strip_prefix("VTS_").and_then(|r| r[..2].parse::<u32>().ok());
        let padding_before = match vts {
            Some(n) if name.ends_with("_0.IFO") => padding.get(&n).copied().unwrap_or(0),
            _ => 0,
        };
        let data = overrides.get(&name).cloned();
        let size = data.as_ref().map_or(size, |d| d.len() as u64);
        entries.push(TreeEntry { data, padding_before, ..TreeEntry::file(&format!("VIDEO_TS/{name}"), source, size) });
    }
    plan_tree(label, &entries, &TreeOptions::default())
}

/// Encoded UDF file name: compression id 8 (one byte per character) or 16 (UTF-16BE).
fn fid_name(name: &str) -> Vec<u8> {
    if name.chars().all(|c| (c as u32) < 256) {
        std::iter::once(8).chain(name.chars().map(|c| c as u8)).collect()
    } else {
        std::iter::once(16).chain(name.encode_utf16().flat_map(u16::to_be_bytes)).collect()
    }
}

fn invalid(msg: String) -> ImageError {
    ImageError::Invalid(msg)
}
type NodeTree = (Vec<Node>, Vec<ImageFile>, Vec<usize>);

fn build_nodes(entries: &[TreeEntry]) -> Result<NodeTree, ImageError> {
    let new_node = |name: &str, parent: usize, dir: bool| Node {
        name: name.to_string(),
        parent,
        dir,
        children: vec![],
        file: None,
        unique: 0,
        fe: 0,
        data: 0,
        dir_bytes: 0,
    };
    let mut nodes = vec![new_node("", 0, true)];
    let mut files = Vec::new();
    let mut files_node = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::from([(String::new(), 0)]);
    for e in entries {
        let parts: Vec<&str> = e.path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(invalid("empty path".into()));
        }
        let (mut cur, mut prefix) = (0usize, String::new());
        for (i, part) in parts.iter().enumerate() {
            if fid_name(part).len() > 255 {
                return Err(invalid(format!("name too long: {part}")));
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let last = i + 1 == parts.len();
            let want_dir = !last || e.dir;
            match index.get(&prefix) {
                Some(&n) if nodes[n].dir == want_dir && !(last && !e.dir) => cur = n,
                Some(_) => return Err(invalid(format!("duplicate or conflicting path {prefix}"))),
                None => {
                    nodes.push(new_node(part, cur, want_dir));
                    let n = nodes.len() - 1;
                    nodes[cur].children.push(n);
                    index.insert(prefix.clone(), n);
                    cur = n;
                }
            }
        }
        if !e.dir {
            let size = e.data.as_ref().map_or(e.size, |d| d.len() as u64);
            nodes[cur].file = Some(files.len());
            files_node.push(cur);
            files.push(ImageFile {
                name: parts[parts.len() - 1].to_string(),
                path: prefix,
                source: e.source.clone(),
                size,
                data: e.data.clone(),
                padding_before: e.padding_before,
                lba: 0,
                duplicate_of: None,
                transform: e.transform,
            });
        }
    }
    for n in 0..nodes.len() {
        let mut children = std::mem::take(&mut nodes[n].children);
        children.sort_by(|&a, &b| nodes[a].name.as_bytes().cmp(nodes[b].name.as_bytes()));
        nodes[n].children = children;
    }
    Ok((nodes, files, files_node))
}

fn file_md5(f: &ImageFile) -> Result<[u8; 16], ImageError> {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    match &f.data {
        Some(d) => h.update(d),
        None => {
            let mut src = File::open(&f.source).map_err(io(&f.source))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = src.read(&mut buf).map_err(io(&f.source))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
        }
    }
    Ok(h.finalize().into())
}

/// Marks files with the same size and MD5 as copies of the first one.
fn mark_duplicates(files: &mut [ImageFile]) -> Result<(), ImageError> {
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size > 0 && f.transform.is_none() {
            by_size.entry(f.size).or_default().push(i);
        }
    }
    for idxs in by_size.values().filter(|v| v.len() > 1) {
        let mut seen: HashMap<[u8; 16], usize> = HashMap::new();
        for &i in idxs {
            let hash = file_md5(&files[i])?;
            match seen.get(&hash) {
                Some(&first) => {
                    files[i].duplicate_of = Some(first);
                    files[i].padding_before = 0;
                }
                None => {
                    seen.insert(hash, i);
                }
            }
        }
    }
    Ok(())
}

// ---- ISO 9660 naming -------------------------------------------------------------------------

/// Longest extent one ISO 9660 directory record can describe (multiple of the sector size).
const ISO_EXTENT: u64 = 0xFFFF_F800;
/// Deepest directory level of ISO 9660 (the root is level 1).
const ISO_MAX_DEPTH: usize = 8;

fn d_chars(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii_uppercase() || u.is_ascii_digit() || u == '_' {
                u
            } else {
                '_'
            }
        })
        .take(max)
        .collect()
}

/// Level 1 identifier: 8.3 for files (`NAME.EXT`, a bare `NAME.` without extension), 8 for
/// directories; `used` holds the identifiers of the directory so far (collisions get `~N`).
fn level1_ident(name: &str, dir: bool, used: &HashSet<Vec<u8>>) -> Vec<u8> {
    let (stem, ext) = if dir { (name, "") } else { name.rsplit_once('.').unwrap_or((name, "")) };
    let ext = d_chars(ext, 3);
    let make = |base: &str| if dir { base.to_string() } else { format!("{base}.{ext}") };
    let base = d_chars(stem, 8);
    let base = if base.is_empty() { "_".to_string() } else { base };
    let first = make(&base);
    if !used.contains(first.as_bytes()) {
        return first.into_bytes();
    }
    for n in 1.. {
        let suffix = format!("~{n}");
        let cut: String = base.chars().take(8 - suffix.len().min(7)).collect();
        let candidate = make(&format!("{cut}{suffix}"));
        if !used.contains(candidate.as_bytes()) {
            return candidate.into_bytes();
        }
    }
    unreachable!()
}

/// Joliet identifier: UCS-2BE, at most 64 characters (the extension survives truncation).
fn joliet_ident(name: &str, used: &HashSet<Vec<u8>>) -> Vec<u8> {
    let clean: String = name.chars().map(|c| if c.is_control() || "*/:;?\\".contains(c) { '_' } else { c }).collect();
    let fit = |s: &str, tail: &str| -> Vec<u8> {
        let room = 64 - tail.encode_utf16().count();
        let mut units: Vec<u16> = Vec::new();
        for c in s.chars() {
            let mut b = [0u16; 2];
            if units.len() + c.len_utf16() > room {
                break;
            }
            units.extend(c.encode_utf16(&mut b).iter());
        }
        units.extend(tail.encode_utf16());
        units.iter().flat_map(|u| u.to_be_bytes()).collect()
    };
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.encode_utf16().count() <= 8 => (s.to_string(), format!(".{e}")),
        _ => (clean.clone(), String::new()),
    };
    let first = fit(&stem, &ext);
    if !used.contains(&first) {
        return first;
    }
    for n in 1.. {
        // The suffix goes into the tail, so truncating the stem cannot cut it off.
        let candidate = fit(&stem, &format!("~{n}{ext}"));
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn make_side(nodes: &[Node], joliet: bool, warnings: &mut Vec<String>) -> IsoSide {
    let n = nodes.len();
    let mut side = IsoSide {
        joliet,
        names: vec![None; n],
        kids: vec![Vec::new(); n],
        dirs: vec![0],
        dir_pos: vec![0; n],
        dir_lba: vec![0; n],
        dir_sectors: vec![1; n],
        pt_size: 0,
        pt_l: 0,
        pt_m: 0,
    };
    side.names[0] = Some(vec![0]);
    let mut depth = vec![0usize; n];
    depth[0] = 1;
    let mut i = 0;
    while i < side.dirs.len() {
        let d = side.dirs[i];
        i += 1;
        let mut used: HashSet<Vec<u8>> = HashSet::new();
        let mut kids: Vec<(Vec<u8>, usize)> = Vec::new();
        for &c in &nodes[d].children {
            if nodes[c].dir && depth[d] + 1 > ISO_MAX_DEPTH {
                warnings.push(format!(
                    "{}: folder '{}' is deeper than {ISO_MAX_DEPTH} levels and is missing in the ISO 9660 view (UDF has it)",
                    if joliet { "Joliet" } else { "ISO 9660" },
                    nodes[c].name
                ));
                continue;
            }
            let ident = if joliet { joliet_ident(&nodes[c].name, &used) } else { level1_ident(&nodes[c].name, nodes[c].dir, &used) };
            used.insert(ident.clone());
            kids.push((ident, c));
        }
        kids.sort();
        for (ident, c) in kids {
            side.names[c] = Some(ident);
            side.kids[d].push(c);
            if nodes[c].dir {
                depth[c] = depth[d] + 1;
                side.dir_pos[c] = side.dirs.len();
                side.dirs.push(c);
            }
        }
    }
    side
}

/// Extents `(first sector, bytes)` of a file in directory records.
fn iso_extents(lba: u32, size: u64) -> Vec<(u32, u32)> {
    if size == 0 {
        return vec![(lba, 0)];
    }
    let mut out = Vec::new();
    let (mut at, mut left) = (lba as u64, size);
    while left > 0 {
        let len = left.min(ISO_EXTENT);
        out.push((at as u32, len as u32));
        at += len / SECTOR;
        left -= len;
    }
    out
}

/// Bytes of the directory `d` of an ISO tree (records never cross a sector boundary).
fn iso_dir(nodes: &[Node], files: &[ImageFile], side: &IsoSide, d: usize, ts: &Timestamp) -> Vec<u8> {
    let dir_rec = |n: usize, name: &[u8]| iso_record_flags(side.dir_lba[n], side.dir_sectors[n] * SECTOR as u32, 2, name, ts);
    let mut out = dir_rec(d, &[0]);
    out.extend(dir_rec(nodes[d].parent, &[1]));
    let push = |out: &mut Vec<u8>, r: Vec<u8>| {
        let used = out.len() % SECTOR as usize;
        if used + r.len() > SECTOR as usize {
            out.extend(vec![0u8; SECTOR as usize - used]);
        }
        out.extend(r);
    };
    for &c in &side.kids[d] {
        let ident = side.names[c].as_ref().expect("kids have identifiers");
        if nodes[c].dir {
            let r = dir_rec(c, ident);
            push(&mut out, r);
            continue;
        }
        let f = &files[nodes[c].file.expect("file node")];
        let name: Vec<u8> = if side.joliet { ident.clone() } else { [&ident[..], b";1"].concat() };
        let extents = iso_extents(f.lba, f.size);
        for (k, (lba, len)) in extents.iter().enumerate() {
            let flags = if k + 1 < extents.len() { 0x80 } else { 0 };
            push(&mut out, iso_record_flags(*lba, *len, flags, &name, ts));
        }
    }
    out
}

/// ISO 9660 path table of one tree.
fn path_table(nodes: &[Node], side: &IsoSide, big_endian: bool) -> Vec<u8> {
    let mut t = Vec::new();
    for &d in &side.dirs {
        let name = side.names[d].as_ref().expect("directories have identifiers");
        let parent = if d == 0 { 1 } else { side.dir_pos[nodes[d].parent] as u16 + 1 };
        let lba = side.dir_lba[d];
        t.push(name.len() as u8);
        t.push(0);
        t.extend(if big_endian { lba.to_be_bytes() } else { lba.to_le_bytes() });
        t.extend(if big_endian { parent.to_be_bytes() } else { parent.to_le_bytes() });
        t.extend(name);
        if name.len() % 2 == 1 {
            t.push(0);
        }
    }
    t
}

/// Lays out `entries` (any depth; parent directories are created as needed) as a UDF 1.02
/// image with an ISO 9660 level 1 (and optionally Joliet) side pointing at the same data.
/// Files keep the order of `entries`; directory contents are sorted by name.
pub fn plan_tree(label: &str, entries: &[TreeEntry], opts: &TreeOptions) -> Result<ImagePlan, ImageError> {
    let (mut nodes, mut files, files_node) = build_nodes(entries)?;
    if opts.dedupe {
        mark_duplicates(&mut files)?;
    }
    let mut order = Vec::with_capacity(nodes.len());
    let mut stack = vec![0usize];
    while let Some(n) = stack.pop() {
        order.push(n);
        stack.extend(nodes[n].children.iter().rev());
    }

    // Unique IDs: root 0, other directories 16.., then the files in the given order.
    let dir_nodes: Vec<usize> = order.iter().copied().filter(|&n| nodes[n].dir).collect();
    let mut unique = 16u32;
    for &n in dir_nodes.iter().skip(1) {
        nodes[n].unique = unique;
        unique += 1;
    }
    let dir_count = 1 + (unique - 16);
    for &n in &files_node {
        nodes[n].unique = unique;
        unique += 1;
    }

    // Partition blocks: 0 FSD, 1 TD, then per directory its FE and FID stream (pre-order), then
    // one FE per file, then the file data.
    let mut block = 2u32;
    for &n in &dir_nodes {
        let bytes = 40 + nodes[n].children.iter().map(|&c| fid_len(&nodes[c].name) as u64).sum::<u64>();
        nodes[n].fe = block;
        nodes[n].dir_bytes = bytes;
        nodes[n].data = block + 1;
        block += 1 + bytes.div_ceil(SECTOR) as u32;
    }
    for &n in &files_node {
        nodes[n].fe = block;
        block += 1;
    }
    let data_first_block = block;

    // ISO 9660 area: path tables and directories of each tree (right after the logical volume
    // integrity descriptor, or behind the anchor when that does not fit before it).
    let mut warnings = Vec::new();
    let mut sides = vec![make_side(&nodes, false, &mut warnings)];
    if opts.joliet {
        sides.push(make_side(&nodes, true, &mut warnings));
    }
    let probe = Timestamp { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
    for side in &mut sides {
        side.pt_size = path_table(&nodes, side, false).len() as u32;
        for d in side.dirs.clone() {
            let len = iso_dir(&nodes, &files, side, d, &probe).len() as u64;
            side.dir_sectors[d] = len.div_ceil(SECTOR).max(1) as u32;
        }
    }
    let area: u32 = sides.iter().map(IsoSide::area_sectors).sum();
    let iso_start = if ISO_DIRS - 2 + area <= ANCHOR { ISO_DIRS - 2 } else { ANCHOR + 1 };
    let partition_start = (iso_start + area).max(PARTITION_START);
    let mut at = iso_start;
    for side in &mut sides {
        side.pt_l = at;
        side.pt_m = at + side.pt_sectors();
        at += 2 * side.pt_sectors();
        for d in side.dirs.clone() {
            side.dir_lba[d] = at;
            at += side.dir_sectors[d];
        }
    }

    let mut sector = (partition_start + data_first_block).max(opts.start_lba);
    for i in 0..files.len() {
        match files[i].duplicate_of {
            Some(first) => files[i].lba = files[first].lba,
            None => {
                sector += files[i].padding_before;
                files[i].lba = sector;
                sector += files[i].sectors();
            }
        }
    }
    // Last sector: second anchor.
    let total_sectors = (sector + 1).max(opts.min_sectors);
    Ok(ImagePlan {
        label: label.to_string(),
        files,
        total_sectors,
        warnings,
        nodes,
        files_node,
        order,
        partition_start,
        sides,
        next_unique: unique as u64,
        dir_count,
    })
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

pub(crate) fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put64(b: &mut [u8], at: usize, v: u64) {
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
    tag_v(d, id, location, len, 2); // UDF 1.02 / ECMA-167 2nd edition
}

/// [`tag`] with an explicit descriptor version (3 = ECMA-167 3rd edition, UDF 2.xx).
pub(crate) fn tag_v(d: &mut [u8], id: u16, location: u32, len: usize, version: u16) {
    put16(d, 0, id);
    put16(d, 2, version);
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

    pub(crate) fn udf(&self, b: &mut [u8], at: usize) {
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
pub(crate) fn dstring(b: &mut [u8], at: usize, len: usize, text: &str) {
    if text.is_empty() {
        return;
    }
    let bytes: Vec<u8> = text.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'_' }).take(len - 2).collect();
    b[at] = 8;
    b[at + 1..at + 1 + bytes.len()].copy_from_slice(&bytes);
    b[at + len - 1] = (bytes.len() + 1) as u8;
}

/// charspec "OSTA Compressed Unicode".
pub(crate) fn charspec(b: &mut [u8], at: usize) {
    b[at] = 0;
    let s = b"OSTA Compressed Unicode";
    b[at + 1..at + 1 + s.len()].copy_from_slice(s);
}

/// EntityID with identifier and suffix.
pub(crate) fn regid(b: &mut [u8], at: usize, ident: &str, suffix: &[u8]) {
    b[at] = 0;
    b[at + 1..at + 1 + ident.len()].copy_from_slice(ident.as_bytes());
    b[at + 24..at + 24 + suffix.len()].copy_from_slice(suffix);
}

const UDF_REV: [u8; 2] = [0x02, 0x01]; // 0x0102 little endian
pub(crate) const IMPL_ID: &str = "*MusaDVD";

fn domain_regid(b: &mut [u8], at: usize) {
    regid(b, at, "*OSTA UDF Compliant", &[UDF_REV[0], UDF_REV[1], 0]);
}
pub(crate) fn impl_regid(b: &mut [u8], at: usize) {
    regid(b, at, IMPL_ID, &[]);
}

fn iso_record_len(name: &[u8]) -> usize {
    let len = 33 + name.len();
    len + (len % 2)
}

fn iso_record_flags(lba: u32, size: u32, flags: u8, name: &[u8], ts: &Timestamp) -> Vec<u8> {
    let len = iso_record_len(name);
    let mut r = vec![0u8; len];
    r[0] = len as u8;
    both32(&mut r, 2, lba);
    both32(&mut r, 10, size);
    ts.iso7(&mut r, 18);
    r[25] = flags;
    both16(&mut r, 28, 1);
    r[32] = name.len() as u8;
    r[33..33 + name.len()].copy_from_slice(name);
    r
}

/// Length of a UDF File Identifier Descriptor for `name` (padded to 4).
fn fid_len(name: &str) -> usize {
    (38 + fid_name(name).len()).div_ceil(4) * 4
}

/// UDF File Identifier Descriptor. `parent` = the parent entry (no name).
fn fid(name: Option<&str>, dir: bool, icb_block: u32, unique: u32, tag_block: u32) -> Vec<u8> {
    let enc = name.map(fid_name).unwrap_or_default();
    let l_fi = enc.len();
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
    d[38..38 + l_fi].copy_from_slice(&enc);
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
pub(crate) fn extents(first_block: u32, size: u64) -> Vec<(u32, u64)> {
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
pub(crate) struct SectorWriter<W: Write> {
    pub(crate) out: W,
    pub(crate) sector: u32,
    pub(crate) path: PathBuf,
}

impl<W: Write> SectorWriter<W> {
    pub(crate) fn zeros_until(&mut self, target: u32) -> Result<(), ImageError> {
        assert!(target >= self.sector, "layout went backwards: at {} want {target}", self.sector);
        let zero = [0u8; SECTOR as usize];
        while self.sector < target {
            self.out.write_all(&zero).map_err(io(&self.path))?;
            self.sector += 1;
        }
        Ok(())
    }

    /// Writes `data` at sector `at`, padded to whole sectors.
    pub(crate) fn put(&mut self, at: u32, data: &[u8]) -> Result<(), ImageError> {
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

/// Text field of a volume descriptor: spaces, then the text (UCS-2BE in a Joliet descriptor).
fn vd_text(b: &mut [u8], at: usize, len: usize, s: &str, joliet: bool) {
    if joliet {
        for pair in b[at..at + len].as_chunks_mut::<2>().0.iter_mut() {
            pair.copy_from_slice(&[0, b' ']);
        }
        for (i, c) in s.chars().take(len / 2).enumerate() {
            b[at + 2 * i..at + 2 * i + 2].copy_from_slice(&(c as u16).to_be_bytes());
        }
    } else {
        b[at..at + len].fill(b' ');
        let n = s.len().min(len);
        b[at..at + n].copy_from_slice(&s.as_bytes()[..n]);
    }
}

/// ISO 9660 primary (`si` = 0) or Joliet supplementary volume descriptor.
fn iso_volume_descriptor(plan: &ImagePlan, si: usize, ts: &Timestamp) -> Vec<u8> {
    let side = &plan.sides[si];
    let label = &plan.label;
    let mut vd = vec![0u8; SECTOR as usize];
    vd[0] = if side.joliet { 2 } else { 1 };
    vd[1..6].copy_from_slice(b"CD001");
    vd[6] = 1;
    let text = |vd: &mut [u8], at, len, s: &str| vd_text(vd, at, len, s, side.joliet);
    text(&mut vd, 8, 32, "");
    text(&mut vd, 40, 32, label);
    both32(&mut vd, 80, plan.total_sectors);
    if side.joliet {
        vd[88..91].copy_from_slice(b"%/E");
    }
    both16(&mut vd, 120, 1);
    both16(&mut vd, 124, 1);
    both16(&mut vd, 128, SECTOR as u16);
    both32(&mut vd, 132, side.pt_size);
    put32(&mut vd, 140, side.pt_l);
    vd[148..152].copy_from_slice(&side.pt_m.to_be_bytes());
    let root = iso_record_flags(side.dir_lba[0], side.dir_sectors[0] * SECTOR as u32, 2, &[0], ts);
    vd[156..156 + 34].copy_from_slice(&root);
    text(&mut vd, 190, 128, label);
    text(&mut vd, 318, 128, "");
    text(&mut vd, 446, 128, "");
    text(&mut vd, 574, 128, "MUSADVD");
    text(&mut vd, 702, 37, "");
    text(&mut vd, 739, 37, "");
    text(&mut vd, 776, 37, "");
    ts.iso17(&mut vd, 813);
    ts.iso17(&mut vd, 830);
    vd[847..863].fill(b'0');
    vd[864..880].fill(b'0');
    vd[881] = 1;
    vd
}

/// Writes the planned image to `output`; `progress` gets 0..1 while the file data is copied.
/// Stops with [`ImageError::Cancelled`] once `cancel` is set (the partial file stays).
pub fn write_image(
    plan: &ImagePlan,
    output: &Path,
    ts: &Timestamp,
    cancel: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<(), ImageError> {
    let file = File::create(output).map_err(io(output))?;
    write_to(plan, BufWriter::with_capacity(4 << 20, file), output, ts, cancel, progress)
}

/// Writes the planned image to any sink, strictly front to back (so it can feed a pipe).
/// `path` only names the sink in error messages.
pub fn write_to<W: Write>(
    plan: &ImagePlan,
    out: W,
    path: &Path,
    ts: &Timestamp,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f64),
) -> Result<(), ImageError> {
    let mut w = SectorWriter { out, sector: 0, path: path.to_path_buf() };
    let n = plan.total_sectors;
    let p = plan.partition_start;
    let partition_len = n - 1 - p;
    let label = &plan.label;

    // ---- ISO 9660 volume descriptors, then the UDF volume recognition sequence --------------
    w.put(ISO_PVD, &iso_volume_descriptor(plan, 0, ts))?;
    let mut next = ISO_PVD + 1;
    if plan.sides.len() > 1 {
        w.put(next, &iso_volume_descriptor(plan, 1, ts))?;
        next += 1;
    }
    let mut term = vec![0u8; SECTOR as usize];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    w.put(next, &term)?;
    for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
        let mut v = vec![0u8; SECTOR as usize];
        v[1..6].copy_from_slice(*id);
        v[6] = 1;
        w.put(next + 1 + i as u32, &v)?;
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
    put64(&mut d, 40, plan.next_unique); // next unique id
    put32(&mut d, 72, 1);
    put32(&mut d, 76, 46);
    put32(&mut d, 80, 0); // free space
    put32(&mut d, 84, partition_len);
    impl_regid(&mut d, 88);
    put32(&mut d, 120, plan.files.len() as u32);
    put32(&mut d, 124, plan.dir_count);
    put16(&mut d, 128, 0x0102);
    put16(&mut d, 130, 0x0102);
    put16(&mut d, 132, 0x0102);
    tag(&mut d, 9, LVID, 134);
    w.put(LVID, &d)?;
    let mut d = vec![0u8; 512];
    tag(&mut d, 8, LVID + 1, 512);
    w.put(LVID + 1, &d)?;

    // ---- ISO 9660 path tables and directories (before the anchor if they fit there) ----------
    let iso_area = |w: &mut SectorWriter<W>| -> Result<(), ImageError> {
        for side in &plan.sides {
            w.put(side.pt_l, &path_table(&plan.nodes, side, false))?;
            w.put(side.pt_m, &path_table(&plan.nodes, side, true))?;
            for &dir in &side.dirs {
                w.put(side.dir_lba[dir], &iso_dir(&plan.nodes, &plan.files, side, dir, ts))?;
            }
        }
        Ok(())
    };
    let iso_before_anchor = plan.sides[0].pt_l < ANCHOR;
    if iso_before_anchor {
        iso_area(&mut w)?;
    }

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
    if !iso_before_anchor {
        iso_area(&mut w)?;
    }

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
    put32(&mut d, 404, plan.nodes[0].fe);
    domain_regid(&mut d, 416);
    tag(&mut d, 256, 0, 512);
    w.put(p, &d)?;
    let mut d = vec![0u8; 512];
    tag(&mut d, 8, 1, 512);
    w.put(p + 1, &d)?;

    // Directories in pre-order: file entry, then the FID stream (parent entry first).
    for &dir in plan.order.iter().filter(|&&n| plan.nodes[n].dir) {
        let node = &plan.nodes[dir];
        let parent = &plan.nodes[node.parent];
        let mut stream = fid(None, true, parent.fe, parent.unique, node.data);
        for &c in &node.children {
            let child = &plan.nodes[c];
            // Tag location: the block where the descriptor starts.
            let block = node.data + (stream.len() as u64 / SECTOR) as u32;
            stream.extend(fid(Some(&child.name), child.dir, child.fe, child.unique, block));
        }
        debug_assert_eq!(stream.len() as u64, node.dir_bytes);
        let links = 1 + node.children.iter().filter(|&&c| plan.nodes[c].dir).count() as u16;
        let fe = file_entry(true, links, node.dir_bytes, &[(node.data, node.dir_bytes)], node.unique as u64, node.fe, ts);
        w.put(p + node.fe, &fe)?;
        w.put(p + node.data, &stream)?;
    }

    // One File Entry per file; duplicates point at the extent of their original.
    for (f, &node) in plan.files.iter().zip(&plan.files_node) {
        let node = &plan.nodes[node];
        let fe = file_entry(false, 1, f.size, &extents(f.lba - p, f.size), node.unique as u64, node.fe, ts);
        w.put(p + node.fe, &fe)?;
    }

    // ---- File data ------------------------------------------------------------------------------
    let total_bytes: u64 = plan.files.iter().filter(|f| !f.is_duplicate()).map(|f| f.size).sum::<u64>().max(1);
    let mut copied = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    for f in plan.files.iter().filter(|f| !f.is_duplicate()) {
        w.zeros_until(f.lba)?;
        match &f.data {
            Some(data) => {
                w.put(f.lba, data)?;
                copied += f.size;
            }
            None => {
                let mut src = File::open(&f.source).map_err(io(&f.source))?;
                let (mut left, mut offset) = (f.size, 0u64);
                while left > 0 {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(ImageError::Cancelled);
                    }
                    let want = left.min(buf.len() as u64) as usize;
                    let got = src.read(&mut buf[..want]).map_err(io(&f.source))?;
                    if got == 0 {
                        return Err(ImageError::Io {
                            path: f.source.clone(),
                            error: std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "File became shorter while writing"),
                        });
                    }
                    if let Some(transform) = f.transform {
                        transform(&mut buf[..got], offset);
                    }
                    w.out.write_all(&buf[..got]).map_err(io(path))?;
                    left -= got as u64;
                    offset += got as u64;
                    copied += got as u64;
                    progress(copied as f64 / total_bytes as f64);
                }
                let rem = f.size % SECTOR;
                if rem != 0 {
                    w.out.write_all(&vec![0u8; (SECTOR - rem) as usize]).map_err(io(path))?;
                }
                w.sector += f.sectors();
            }
        }
    }
    // Closing anchor in the last sector.
    w.put(n - 1, &anchor(n - 1))?;
    debug_assert_eq!(w.sector, n);
    w.out.flush().map_err(io(path))?;
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

#[cfg(test)]
mod tree_tests {
    use super::*;
    use crate::burner::isoread::{IsoError, IsoImage};
    use std::io::Cursor;

    const TS: Timestamp = Timestamp { year: 2026, month: 10, day: 24, hour: 12, minute: 0, second: 0 };

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cx_tree_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn image(plan: &ImagePlan) -> Vec<u8> {
        let mut out = Vec::new();
        write_to(plan, &mut out, Path::new("<memory>"), &TS, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(out.len() as u64, plan.size_bytes(), "image has exactly the planned size");
        out
    }

    fn names(iso: &mut IsoImage<Cursor<Vec<u8>>>, dir: &str) -> Vec<String> {
        iso.list(dir).unwrap().into_iter().map(|e| e.name).collect()
    }

    fn memfile(path: &str, data: &[u8]) -> TreeEntry {
        TreeEntry { data: Some(data.to_vec()), ..TreeEntry::file(path, PathBuf::new(), 0) }
    }

    #[test]
    fn deep_tree_with_joliet_names_collisions_and_dedupe() {
        let dir = temp("tree");
        let content = [
            ("Dokumente/Ärger über Größe.txt", vec![1u8; 9]),
            ("verylongfilename1.txt", b"one".to_vec()),
            ("verylongfilename2.txt", b"two".to_vec()),
            ("a/b/c/d/e/f/g/h/deep.txt", b"deep".to_vec()),
            ("copy1.bin", vec![7u8; 5000]),
            ("sub/copy2.bin", vec![7u8; 5000]),
        ];
        let entries: Vec<TreeEntry> = content
            .iter()
            .enumerate()
            .map(|(i, (rel, c))| {
                let p = dir.join(format!("src{i}"));
                std::fs::write(&p, c).unwrap();
                TreeEntry::file(rel, p, c.len() as u64)
            })
            .collect();
        let opts = TreeOptions { joliet: true, dedupe: true, ..Default::default() };
        let plan = plan_tree("DATENTEST", &entries, &opts).unwrap();

        // Dedupe: the copy shares the extent of the first file and saves its sectors.
        assert_eq!(plan.files[5].duplicate_of, Some(4));
        assert_eq!(plan.files[5].lba, plan.files[4].lba);
        let plain = plan_tree("DATENTEST", &entries, &TreeOptions { joliet: true, ..Default::default() }).unwrap();
        assert_eq!(plain.total_sectors - plan.total_sectors, 3, "5000 bytes = 3 sectors saved");

        // `h` is the 9th level: left out of both ISO trees, with one warning each.
        assert_eq!(plan.warnings.iter().filter(|w| w.contains("deeper")).count(), 2, "{:?}", plan.warnings);

        let mut iso = IsoImage::open(Cursor::new(image(&plan))).unwrap();
        assert_eq!(iso.label, "DATENTEST");
        assert_eq!(names(&mut iso, ""), ["A", "COPY1.BIN", "DOKUMENT", "SUB", "VERYLONG.TXT", "VERYLO~1.TXT"]);
        assert_eq!(iso.read_file("VERYLONG.TXT").unwrap(), b"one");
        assert_eq!(iso.read_file("verylo~1.txt").unwrap(), b"two", "collision gets ~1");
        assert_eq!(iso.read_file("SUB/COPY2.BIN").unwrap(), vec![7u8; 5000], "duplicate reads the shared extent");
        let umlaut = names(&mut iso, "DOKUMENT");
        assert_eq!(umlaut.len(), 1);
        assert!(umlaut[0].ends_with(".TXT") && umlaut[0].is_ascii(), "{umlaut:?}");
        assert!(iso.find("A/B/C/D/E/F/G").is_ok(), "level 8 is the last one");
        assert!(matches!(iso.find("A/B/C/D/E/F/G/H"), Err(IsoError::NotFound(_))));

        let mut joliet = IsoImage::open_joliet(Cursor::new(image(&plan))).unwrap();
        assert_eq!(joliet.label, "DATENTEST");
        assert_eq!(names(&mut joliet, ""), ["Dokumente", "a", "copy1.bin", "sub", "verylongfilename1.txt", "verylongfilename2.txt"]);
        assert_eq!(joliet.read_file("Dokumente/Ärger über Größe.txt").unwrap(), vec![1u8; 9]);
        assert_eq!(joliet.read_file("sub/copy2.bin").unwrap(), vec![7u8; 5000]);
        assert!(matches!(joliet.find("a/b/c/d/e/f/g/h"), Err(IsoError::NotFound(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn long_joliet_names_are_cut_keeping_the_extension() {
        let long = format!("{}.mkv", "x".repeat(100));
        let near = format!("{}.mkv", "x".repeat(99));
        let plan = plan_tree("L", &[memfile(&long, b"1"), memfile(&near, b"2")], &TreeOptions { joliet: true, ..Default::default() }).unwrap();
        let mut joliet = IsoImage::open_joliet(Cursor::new(image(&plan))).unwrap();
        let got = names(&mut joliet, "");
        assert_eq!(got.len(), 2);
        for n in &got {
            assert!(n.encode_utf16().count() <= 64 && n.ends_with(".mkv"), "{n}");
        }
        assert_ne!(got[0], got[1], "cut names stay unique");
    }

    #[test]
    fn minimum_size_and_start_sector() {
        let opts = TreeOptions { min_sectors: 600, start_lba: 400, ..Default::default() };
        let plan = plan_tree("X", &[memfile("f", &[1; 10])], &opts).unwrap();
        assert_eq!((plan.files[0].lba, plan.total_sectors), (400, 600));
        let img = image(&plan);
        assert_eq!(&img[400 * 2048..400 * 2048 + 10], &[1u8; 10]);
        for at in [256usize, 599] {
            assert_eq!(u16::from_le_bytes([img[at * 2048], img[at * 2048 + 1]]), 2, "anchor at {at}");
        }
        // Without options the image is just as big as its content needs.
        let small = plan_tree("X", &[memfile("f", &[1; 10])], &TreeOptions::default()).unwrap();
        assert!(small.total_sectors < 300);
    }

    #[test]
    fn files_over_4_gib_use_several_iso_extents() {
        let big = 5u64 << 30;
        let plan = plan_tree("BIG", &[TreeEntry::file("big.bin", PathBuf::from("nicht-vorhanden"), big)], &TreeOptions::default()).unwrap();
        assert!(plan.total_sectors as u64 > big / SECTOR, "5 GiB are planned");
        let side = &plan.sides[0];
        let dir = iso_dir(&plan.nodes, &plan.files, side, 0, &TS);
        // After "." and ".." (34 bytes each): two records of 0xFFFFF800 bytes and the rest.
        let (mut at, mut recs) = (68usize, Vec::new());
        while at < dir.len() && dir[at] != 0 {
            recs.push((dir[at + 25], u32::from_le_bytes(dir[at + 10..at + 14].try_into().unwrap())));
            at += dir[at] as usize;
        }
        assert_eq!(recs.len(), 2, "5 GiB = 4 GiB - 2 KiB + the remainder");
        assert_eq!(recs[0], (0x80, 0xFFFF_F800));
        assert_eq!(recs[1], (0, (big - 0xFFFF_F800) as u32));
        assert_eq!(iso_extents(100, 0xFFFF_F800 * 2).len(), 2);
        assert_eq!(iso_extents(100, 0), [(100, 0)]);
        // UDF side: extents are below 1 GiB each.
        assert!(extents(0, big).len() >= 5);
    }

    #[test]
    fn iso_area_that_does_not_fit_moves_behind_the_anchor() {
        let entries: Vec<TreeEntry> = (0..9000).map(|i| memfile(&format!("f{i:05}.bin"), &[])).collect();
        let plan = plan_tree("MANY", &entries, &TreeOptions::default()).unwrap();
        assert_eq!(plan.sides[0].pt_l, ANCHOR + 1);
        assert!(plan.partition_start() > 257 + 200);
        let img = image(&plan);
        assert_eq!(u16::from_le_bytes([img[256 * 2048], img[256 * 2048 + 1]]), 2, "anchor stays at 256");
        let mut iso = IsoImage::open(Cursor::new(img)).unwrap();
        assert_eq!(iso.list("").unwrap().len(), 9000);
        assert!(iso.find("F08999.BIN").is_ok());
        // A small tree keeps the classic layout.
        let small = plan_tree("S", &entries[..10], &TreeOptions::default()).unwrap();
        assert_eq!((small.sides[0].pt_l, small.partition_start()), (66, 257));
    }

    #[test]
    fn rejects_broken_trees() {
        let dup = [memfile("a/b", b"1"), memfile("a", b"2")];
        assert!(matches!(plan_tree("X", &dup, &TreeOptions::default()), Err(ImageError::Invalid(_))));
        assert!(plan_tree("X", &[memfile("", b"1")], &TreeOptions::default()).is_err());
        let long = "y".repeat(300);
        assert!(plan_tree("X", &[memfile(&long, b"1")], &TreeOptions::default()).is_err());
        // Non-Latin-1 names use 16-bit characters in UDF but stay valid.
        assert!(plan_tree("X", &[memfile("日本語.txt", b"1")], &TreeOptions { joliet: true, ..Default::default() }).is_ok());
    }

    #[test]
    fn level1_names() {
        let used = HashSet::new();
        assert_eq!(level1_ident("VIDEO_TS.IFO", false, &used), b"VIDEO_TS.IFO");
        assert_eq!(level1_ident("Film Nr. 1.mpeg", false, &used), b"FILM_NR_.MPE");
        assert_eq!(level1_ident("README", false, &used), b"README.", "no extension keeps the dot");
        assert_eq!(level1_ident("Meine Bilder", true, &used), b"MEINE_BI");
        let used: HashSet<Vec<u8>> = [b"AB.TXT".to_vec(), b"AB~1.TXT".to_vec()].into();
        assert_eq!(level1_ident("ab.txt", false, &used), b"AB~2.TXT");
    }
}
