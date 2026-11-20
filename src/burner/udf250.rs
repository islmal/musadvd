//! Blu-ray disc image writer: UDF 2.50 with a metadata partition (what a BD-ROM/BD-R carries).
//!
//! Read-only, single session, **no ISO 9660 bridge** (a BD uses UDF 2.50 only, and m2ts files
//! above 4 GiB would not fit ISO 9660 anyway). Like the DVD writer ([`super::udf`]) the image is
//! planned first ([`plan_bd_image`]): every directory and file gets its address, then
//! [`write_bd_image`] streams the image to disk.
//!
//! Layout (2048-byte sectors, `P` = 512 = start of the physical partition):
//! ```text
//!   0..15     system area (zero)
//!   16..18    volume recognition sequence BEA01, NSR03, TEA01
//!   32..37    main volume descriptor sequence (PVD, IUVD, PD, LVD, USD, TD)
//!   48..53    reserve volume descriptor sequence (copy)
//!   64..65    logical volume integrity descriptor + terminator
//!   256       anchor volume descriptor pointer
//!   P+0       Metadata File entry (EFE, file type 250)
//!   P+1       Metadata Mirror File entry (EFE, file type 251)
//!   P+32      metadata (copy 1): file set descriptor, then an EFE per directory/file and the
//!             directory contents (FIDs) – the logical blocks of the metadata partition
//!   ...       file data, every file starting on a 32-block boundary, in tree order
//!   ...       metadata (copy 2, the mirror; duplicate-metadata flag set)
//!   last      anchor volume descriptor pointer
//! ```
//! File data lives in the physical partition and is addressed with long_ads (partition
//! reference 0); directory entries and everything else sit in the metadata partition
//! (partition reference 1).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub use super::udf::{scan, TreeEntry};
use super::udf::{charspec, dstring, extents, impl_regid, io, put16, put32, put64, regid, tag_v, ImageError, SectorWriter, Timestamp, Transform, IMPL_ID, SECTOR};

/// Sectors of a BD-R/BD-RE single layer (12,219,392) and dual layer (24,438,784).
pub const BD25_SECTORS: u64 = 12_219_392;
pub const BD50_SECTORS: u64 = 24_438_784;

/// Allocation and alignment unit of the metadata partition (blocks).
const UNIT: u32 = 32;
const MAIN_VDS: u32 = 32;
const RESERVE_VDS: u32 = 48;
const LVID: u32 = 64;
const ANCHOR: u32 = 256;
const PARTITION_START: u32 = 512;
/// UDF revision 2.50.
const REV: u16 = 0x0250;
const REV_SUFFIX: [u8; 3] = [0x50, 0x02, 0];
const DESCRIPTOR_VERSION: u16 = 3;
const METADATA_MAP_ID: &str = "*UDF Metadata Partition";
/// Most extents one File Entry sector can describe (long_ads).
const MAX_EXTENTS: usize = (SECTOR as usize - 216) / 16;

fn tag(d: &mut [u8], id: u16, location: u32, len: usize) {
    tag_v(d, id, location, len, DESCRIPTOR_VERSION);
}

fn domain_regid(b: &mut [u8], at: usize) {
    regid(b, at, "*OSTA UDF Compliant", &REV_SUFFIX);
}

/// Whether an image of `sectors` needs a BD-50 (otherwise a BD-25 does).
pub fn is_dual_layer(sectors: u64) -> bool {
    sectors > BD25_SECTORS
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Node {
    name: String,
    source: PathBuf,
    size: u64,
    dir: bool,
    parent: usize,
    children: Vec<usize>,
    unique: u32,
    /// Block of the EFE in the metadata partition.
    efe: u32,
    /// Directory: block of its FIDs in the metadata partition; file: first block of its data in
    /// the physical partition.
    data: u32,
    /// Directory: length of its FIDs in bytes.
    dir_bytes: u64,
    transform: Option<Transform>,
}

/// A fully laid out Blu-ray image.
#[derive(Debug, Clone)]
pub struct BdImagePlan {
    pub label: String,
    nodes: Vec<Node>,
    /// Nodes in tree order (parents first, children sorted by name).
    order: Vec<usize>,
    meta_blocks: u32,
    /// Physical-partition blocks of the two metadata copies.
    meta1: u32,
    meta2: u32,
    partition_len: u32,
    pub total_sectors: u32,
}

impl BdImagePlan {
    pub fn size_bytes(&self) -> u64 {
        self.total_sectors as u64 * SECTOR
    }

    pub fn file_count(&self) -> usize {
        self.nodes.iter().filter(|n| !n.dir).count()
    }

    pub fn dir_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.dir).count()
    }

    /// Absolute first sector of the data of the file at `path` (`BDMV/index.bdmv`).
    pub fn file_sector(&self, path: &str) -> Option<u32> {
        let mut cur = 0;
        for part in path.split('/') {
            cur = *self.nodes[cur].children.iter().find(|&&c| self.nodes[c].name == part)?;
        }
        Some(PARTITION_START + self.nodes[cur].data)
    }
}

fn align(v: u32) -> u32 {
    v.div_ceil(UNIT) * UNIT
}

/// Encoded file name: compression id 8 (one byte per character) or 16 (UTF-16BE).
fn fid_name(name: &str) -> Vec<u8> {
    if name.chars().all(|c| (c as u32) < 256) {
        std::iter::once(8).chain(name.chars().map(|c| c as u8)).collect()
    } else {
        std::iter::once(16).chain(name.encode_utf16().flat_map(u16::to_be_bytes)).collect()
    }
}

/// Length of a File Identifier Descriptor, padded to 4 bytes.
fn fid_len(name: Option<&str>) -> usize {
    (38 + name.map_or(0, |n| fid_name(n).len())).div_ceil(4) * 4
}

/// Plans an image from `entries` (any depth; parent directories are created as needed).
pub fn plan_entries(label: &str, entries: &[TreeEntry]) -> Result<BdImagePlan, ImageError> {
    let bad = |m: String| ImageError::NotBluray(m);
    let mut nodes = vec![Node {
        name: String::new(),
        source: PathBuf::new(),
        size: 0,
        dir: true,
        parent: 0,
        children: vec![],
        unique: 0,
        efe: 0,
        data: 0,
        dir_bytes: 0,
        transform: None,
    }];
    let mut index: HashMap<String, usize> = HashMap::from([(String::new(), 0)]);
    for e in entries {
        let parts: Vec<&str> = e.path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(bad("empty path".into()));
        }
        let (mut cur, mut prefix) = (0usize, String::new());
        for (i, part) in parts.iter().enumerate() {
            if fid_name(part).len() > 255 {
                return Err(bad(format!("name too long: {part}")));
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let last = i + 1 == parts.len();
            let want_dir = !last || e.dir;
            match index.get(&prefix) {
                Some(&n) if nodes[n].dir == want_dir && !(last && !e.dir) => cur = n,
                Some(_) => return Err(bad(format!("duplicate or conflicting path {prefix}"))),
                None => {
                    nodes.push(Node {
                        name: part.to_string(),
                        source: PathBuf::new(),
                        size: 0,
                        dir: want_dir,
                        parent: cur,
                        children: vec![],
                        unique: 0,
                        efe: 0,
                        data: 0,
                        dir_bytes: 0,
                        transform: None,
                    });
                    let n = nodes.len() - 1;
                    nodes[cur].children.push(n);
                    index.insert(prefix.clone(), n);
                    cur = n;
                }
            }
        }
        if !e.dir {
            nodes[cur].source = e.source.clone();
            nodes[cur].size = e.size;
            nodes[cur].transform = e.transform;
            if extents(0, e.size).len() > MAX_EXTENTS {
                return Err(bad(format!("{} is too large", e.path)));
            }
        }
    }
    for n in 0..nodes.len() {
        let mut children = std::mem::take(&mut nodes[n].children);
        children.sort_by(|&a, &b| nodes[a].name.as_bytes().cmp(nodes[b].name.as_bytes()));
        nodes[n].children = children;
    }
    // Tree order.
    let mut order = Vec::with_capacity(nodes.len());
    let mut stack = vec![0usize];
    while let Some(n) = stack.pop() {
        order.push(n);
        stack.extend(nodes[n].children.iter().rev());
    }
    // Unique IDs: root 0, then 16.. (1-15 are reserved).
    for (pos, &n) in order.iter().enumerate().skip(1) {
        nodes[n].unique = 15 + pos as u32;
    }
    // Metadata partition: 0 FSD, 1 TD, then per node its EFE and (directories) its FIDs.
    let mut next = 2u32;
    for &n in &order {
        nodes[n].efe = next;
        next += 1;
        if nodes[n].dir {
            let bytes = fid_len(None) as u64 + nodes[n].children.iter().map(|&c| fid_len(Some(&nodes[c].name)) as u64).sum::<u64>();
            nodes[n].dir_bytes = bytes;
            nodes[n].data = next;
            next += bytes.div_ceil(SECTOR) as u32;
        }
    }
    let meta_blocks = align(next);
    // Physical partition: block 0/1 the metadata file entries, copy 1, file data, copy 2.
    let meta1 = UNIT;
    let mut cursor = meta1 + meta_blocks;
    for &n in &order {
        if !nodes[n].dir && nodes[n].size > 0 {
            nodes[n].data = cursor;
            cursor = align(cursor + nodes[n].size.div_ceil(SECTOR) as u32);
        }
    }
    let meta2 = cursor;
    let partition_len = meta2 + meta_blocks;
    Ok(BdImagePlan {
        label: label.to_string(),
        nodes,
        order,
        meta_blocks,
        meta1,
        meta2,
        partition_len,
        total_sectors: PARTITION_START + partition_len + 1,
    })
}

/// Plans the image of the disc folder `root` (the folder containing `BDMV`, and `CERTIFICATE`
/// when present; other entries are not part of the disc).
pub fn plan_bd_image(root: &Path, label: &str) -> Result<BdImagePlan, ImageError> {
    let bdmv = root.join("BDMV");
    if !bdmv.join("index.bdmv").is_file() {
        return Err(ImageError::NotBluray("BDMV/index.bdmv missing".into()));
    }
    let mut entries = Vec::new();
    scan(&bdmv, "BDMV", &mut entries)?;
    let certificate = root.join("CERTIFICATE");
    if certificate.is_dir() {
        scan(&certificate, "CERTIFICATE", &mut entries)?;
    }
    plan_entries(label, &entries)
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// UDF File Identifier Descriptor with its ICB in the metadata partition (reference 1).
fn fid(name: Option<&str>, dir: bool, icb_block: u32, unique: u32, tag_block: u32) -> Vec<u8> {
    let enc = name.map(fid_name).unwrap_or_default();
    let len = fid_len(name);
    let mut d = vec![0u8; len];
    put16(&mut d, 16, 1); // file version
    d[18] = (if dir { 0x02 } else { 0 }) | (if name.is_none() { 0x08 } else { 0 });
    d[19] = enc.len() as u8;
    put32(&mut d, 20, SECTOR as u32);
    put32(&mut d, 24, icb_block);
    put16(&mut d, 28, 1);
    put32(&mut d, 32, unique);
    d[38..38 + enc.len()].copy_from_slice(&enc);
    tag(&mut d, 257, tag_block, len);
    d
}

/// Extended File Entry (one block). `ads` = encoded allocation descriptors (short_ad when
/// `long_ads` is false, long_ad otherwise).
#[allow(clippy::too_many_arguments)]
fn efe(file_type: u8, long_ads: bool, links: u16, size: u64, blocks: u64, ads: &[u8], unique: u64, block: u32, ts: &Timestamp) -> Vec<u8> {
    let mut d = vec![0u8; SECTOR as usize];
    put16(&mut d, 20, 4); // ICB strategy 4
    put16(&mut d, 24, 1); // max entries
    d[27] = file_type;
    put16(&mut d, 34, if long_ads { 1 } else { 0 }); // allocation type
    put32(&mut d, 36, u32::MAX); // uid
    put32(&mut d, 40, u32::MAX); // gid
    put32(&mut d, 44, 0x14A5); // read + execute for owner, group, others
    put16(&mut d, 48, links);
    put64(&mut d, 56, size); // information length
    put64(&mut d, 64, size); // object size
    put64(&mut d, 72, blocks);
    ts.udf(&mut d, 80);
    ts.udf(&mut d, 92);
    ts.udf(&mut d, 104);
    ts.udf(&mut d, 116);
    put32(&mut d, 128, 1); // checkpoint
    impl_regid(&mut d, 168);
    put64(&mut d, 200, unique);
    put32(&mut d, 208, 0); // L_EA
    put32(&mut d, 212, ads.len() as u32);
    d[216..216 + ads.len()].copy_from_slice(ads);
    tag(&mut d, 266, block, 216 + ads.len());
    d
}

fn short_ad(len: u64, block: u32) -> Vec<u8> {
    let mut v = (len as u32).to_le_bytes().to_vec();
    v.extend(block.to_le_bytes());
    v
}

/// long_ad of a file extent in the physical partition (reference 0).
fn long_ad(len: u64, block: u32) -> Vec<u8> {
    let mut v = (len as u32).to_le_bytes().to_vec();
    v.extend(block.to_le_bytes());
    v.extend([0u8; 8]); // partition reference 0, implementation use
    v
}

/// The metadata partition's content: FSD + terminator and, per node, its EFE and directory.
fn metadata_area(plan: &BdImagePlan, ts: &Timestamp) -> Vec<u8> {
    let mut m = vec![0u8; plan.meta_blocks as usize * SECTOR as usize];
    let put = |m: &mut Vec<u8>, block: u32, data: &[u8]| {
        let at = block as usize * SECTOR as usize;
        m[at..at + data.len()].copy_from_slice(data);
    };
    // File Set Descriptor (block 0) + terminator (block 1); the root ICB is block 2.
    let mut d = vec![0u8; 512];
    ts.udf(&mut d, 16);
    put16(&mut d, 28, 3);
    put16(&mut d, 30, 3);
    put32(&mut d, 32, 1);
    put32(&mut d, 36, 1);
    charspec(&mut d, 48);
    dstring(&mut d, 112, 128, &plan.label);
    charspec(&mut d, 240);
    dstring(&mut d, 304, 32, &plan.label);
    put32(&mut d, 400, SECTOR as u32);
    put32(&mut d, 404, plan.nodes[0].efe);
    put16(&mut d, 408, 1); // partition reference: metadata partition
    domain_regid(&mut d, 416);
    tag(&mut d, 256, 0, 512);
    put(&mut m, 0, &d);
    let mut d = vec![0u8; 512];
    tag(&mut d, 8, 1, 512);
    put(&mut m, 1, &d);

    for &i in &plan.order {
        let n = &plan.nodes[i];
        if n.dir {
            let parent = &plan.nodes[n.parent];
            let mut stream = fid(None, true, parent.efe, parent.unique, n.data);
            for &c in &n.children {
                let child = &plan.nodes[c];
                // Tag location: the block where the descriptor starts.
                let block = n.data + (stream.len() as u64 / SECTOR) as u32;
                stream.extend(fid(Some(&child.name), child.dir, child.efe, child.unique, block));
            }
            debug_assert_eq!(stream.len() as u64, n.dir_bytes);
            let links = 1 + n.children.iter().filter(|&&c| plan.nodes[c].dir).count() as u16;
            let blocks = n.dir_bytes.div_ceil(SECTOR);
            put(&mut m, n.efe, &efe(4, false, links, n.dir_bytes, blocks, &short_ad(n.dir_bytes, n.data), n.unique as u64, n.efe, ts));
            put(&mut m, n.data, &stream);
        } else {
            let ads: Vec<u8> = extents(n.data, n.size).iter().flat_map(|&(block, len)| long_ad(len, block)).collect();
            put(&mut m, n.efe, &efe(5, true, 1, n.size, n.size.div_ceil(SECTOR), &ads, n.unique as u64, n.efe, ts));
        }
    }
    m
}

/// Writes the planned image to `output`; `progress` gets 0..1 while the file data is copied.
/// Stops with [`ImageError::Cancelled`] once `cancel` is set (the partial file stays).
pub fn write_bd_image(
    plan: &BdImagePlan,
    output: &Path,
    ts: &Timestamp,
    cancel: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<(), ImageError> {
    let file = File::create(output).map_err(io(output))?;
    write_bd_to(plan, BufWriter::with_capacity(4 << 20, file), output, ts, cancel, progress)
}

/// Writes the planned image to any sink, strictly front to back (so it can feed a pipe).
/// `path` only names the sink in error messages.
pub fn write_bd_to<W: Write>(
    plan: &BdImagePlan,
    out: W,
    path: &Path,
    ts: &Timestamp,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f64),
) -> Result<(), ImageError> {
    let mut w = SectorWriter { out, sector: 0, path: path.to_path_buf() };
    let n = plan.total_sectors;
    let p = PARTITION_START;
    let label = &plan.label;
    let meta_bytes = plan.meta_blocks as u64 * SECTOR;

    // ---- Volume recognition sequence -------------------------------------------------------
    for (i, id) in [b"BEA01", b"NSR03", b"TEA01"].iter().enumerate() {
        let mut v = vec![0u8; SECTOR as usize];
        v[1..6].copy_from_slice(*id);
        v[6] = 1;
        w.put(16 + i as u32, &v)?;
    }

    // ---- Volume descriptor sequence (main + reserve) -------------------------------------------
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
        regid(&mut d, 20, "*UDF LV Info", &REV.to_le_bytes());
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
        regid(&mut d, 24, "+NSR03", &[]);
        put32(&mut d, 184, 1);
        put32(&mut d, 188, p);
        put32(&mut d, 192, plan.partition_len);
        impl_regid(&mut d, 196);
        tag(&mut d, 5, start + 2, 512);
        seq.push(d);
        // Logical Volume Descriptor with the physical and the metadata partition map
        let mut d = vec![0u8; 440 + 6 + 64];
        put32(&mut d, 16, 3);
        charspec(&mut d, 20);
        dstring(&mut d, 84, 128, label);
        put32(&mut d, 212, SECTOR as u32);
        domain_regid(&mut d, 216);
        put32(&mut d, 248, SECTOR as u32); // FSD long_ad: block 0 of the metadata partition
        put32(&mut d, 252, 0);
        put16(&mut d, 256, 1);
        put32(&mut d, 264, 70);
        put32(&mut d, 268, 2);
        impl_regid(&mut d, 272);
        put32(&mut d, 432, 2 * SECTOR as u32);
        put32(&mut d, 436, LVID);
        d[440] = 1; // type 1 partition map
        d[441] = 6;
        put16(&mut d, 442, 1);
        put16(&mut d, 444, 0);
        let m = &mut d[446..510];
        m[0] = 2; // type 2 partition map: metadata partition
        m[1] = 64;
        regid(m, 4, METADATA_MAP_ID, &REV_SUFFIX);
        put16(m, 36, 1);
        put16(m, 38, 0);
        put32(m, 40, 0); // Metadata File: EFE at block 0 of the physical partition
        put32(m, 44, 1); // Metadata Mirror File
        put32(m, 48, u32::MAX); // no Metadata Bitmap File (read-only)
        put32(m, 52, UNIT);
        put16(m, 56, UNIT as u16);
        m[58] = 1; // duplicate metadata: the mirror is a real copy
        tag(&mut d, 6, start + 3, 440 + 70);
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
    put64(&mut d, 40, 16 + plan.nodes.len() as u64); // next unique id
    put32(&mut d, 72, 1);
    put32(&mut d, 76, 46);
    put32(&mut d, 80, 0); // free space
    put32(&mut d, 84, plan.partition_len);
    impl_regid(&mut d, 88);
    put32(&mut d, 120, plan.file_count() as u32);
    put32(&mut d, 124, plan.dir_count() as u32);
    put16(&mut d, 128, REV);
    put16(&mut d, 130, REV);
    put16(&mut d, 132, REV);
    tag(&mut d, 9, LVID, 134);
    w.put(LVID, &d)?;
    let mut d = vec![0u8; 512];
    tag(&mut d, 8, LVID + 1, 512);
    w.put(LVID + 1, &d)?;

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

    // ---- Physical partition: metadata file entries, metadata, file data, mirror ---------------
    let meta_blocks = plan.meta_blocks as u64;
    w.put(p, &efe(250, false, 1, meta_bytes, meta_blocks, &short_ad(meta_bytes, plan.meta1), 1, 0, ts))?;
    w.put(p + 1, &efe(251, false, 1, meta_bytes, meta_blocks, &short_ad(meta_bytes, plan.meta2), 2, 1, ts))?;
    let meta = metadata_area(plan, ts);
    w.put(p + plan.meta1, &meta)?;

    let total_bytes: u64 = plan.nodes.iter().map(|n| n.size).sum::<u64>().max(1);
    let mut copied = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    for &i in &plan.order {
        let f = &plan.nodes[i];
        if f.dir || f.size == 0 {
            continue;
        }
        w.zeros_until(p + f.data)?;
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
        w.sector += f.size.div_ceil(SECTOR) as u32;
    }
    w.put(p + plan.meta2, &meta)?;
    // Closing anchor in the last sector.
    w.put(n - 1, &anchor(n - 1))?;
    debug_assert_eq!(w.sector, n);
    w.out.flush().map_err(io(path))?;
    progress(1.0);
    Ok(())
}

/// Plans and writes the image of the disc folder `root` to `output` (via `<output>.part`, renamed
/// on success; a failed or cancelled write leaves nothing behind).
pub fn create_bd_image(
    root: &Path,
    output: &Path,
    label: &str,
    cancel: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<BdImagePlan, ImageError> {
    let plan = plan_bd_image(root, label)?;
    let tmp = output.with_extension("iso.part");
    let written = write_bd_image(&plan, &tmp, &Timestamp::now(), cancel, progress)
        .and_then(|()| std::fs::rename(&tmp, output).map_err(|error| ImageError::Io { path: output.to_path_buf(), error }));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> TreeEntry {
        TreeEntry::file(path, PathBuf::from(path), size)
    }

    fn dir(path: &str) -> TreeEntry {
        TreeEntry::dir(path)
    }

    fn le16(d: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([d[at], d[at + 1]])
    }
    fn le32(d: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(d[at..at + 4].try_into().unwrap())
    }

    /// A small BDMV tree on disk.
    fn sample_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cx_udf250_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["BDMV/STREAM", "BDMV/PLAYLIST", "BDMV/AUXDATA", "CERTIFICATE/BACKUP"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("BDMV/index.bdmv"), b"INDX0200 some index data").unwrap();
        std::fs::write(root.join("BDMV/PLAYLIST/00000.mpls"), vec![7u8; 3000]).unwrap();
        std::fs::write(root.join("BDMV/STREAM/00000.m2ts"), (0..200_000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>()).unwrap();
        root
    }

    #[test]
    fn tags_use_descriptor_version_3() {
        let mut d = vec![0u8; 512];
        d[100] = 0xAB;
        tag(&mut d, 8, 1234, 512);
        assert_eq!((le16(&d, 0), le16(&d, 2)), (8, 3));
        assert_eq!(le32(&d, 12), 1234);
        assert_eq!(le16(&d, 8), super::super::udf::crc_itu(&d[16..512]));
        let sum: u32 = d[..16].iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, b)| *b as u32).sum();
        assert_eq!(d[4] as u32, sum % 256);
    }

    #[test]
    fn fid_padding_and_partition_reference() {
        // "index.bdmv" = 10 characters + compression id = 11 -> 38 + 11 = 49 -> 52 bytes.
        let f = fid(Some("index.bdmv"), false, 7, 20, 3);
        assert_eq!(f.len(), 52);
        assert_eq!((f[19], f[18]), (11, 0));
        assert_eq!((le32(&f, 24), le16(&f, 28), le32(&f, 32)), (7, 1, 20), "ICB in the metadata partition");
        assert_eq!(fid(None, true, 2, 0, 0).len(), 40);
        assert_eq!(fid(None, true, 2, 0, 0)[18], 0x0A);
        // Names outside Latin-1 use UTF-16.
        assert_eq!(fid_name("ä"), vec![8, 0xE4]);
        assert_eq!(fid_name("日"), vec![16, 0x65, 0xE5]);
    }

    #[test]
    fn efe_layout() {
        let ts = Timestamp::now();
        let ads = long_ad(5000, 99);
        let d = efe(5, true, 1, 5000, 3, &ads, 17, 42, &ts);
        assert_eq!(d.len(), 2048);
        assert_eq!((le16(&d, 0), le16(&d, 2), le32(&d, 12)), (266, 3, 42));
        assert_eq!((d[27], le16(&d, 34)), (5, 1), "file type and long_ad allocation");
        assert_eq!(u64::from_le_bytes(d[56..64].try_into().unwrap()), 5000);
        assert_eq!(u64::from_le_bytes(d[72..80].try_into().unwrap()), 3);
        assert_eq!(u64::from_le_bytes(d[200..208].try_into().unwrap()), 17);
        assert_eq!(le32(&d, 212), 16);
        assert_eq!((le32(&d, 216), le32(&d, 220), le16(&d, 224)), (5000, 99, 0), "partition reference 0 = physical");
        assert_eq!(le16(&d, 10) as usize, 216 + 16 - 16, "tag CRC length");
    }

    #[test]
    fn plan_is_aligned_and_ordered() {
        let plan = plan_entries(
            "BD",
            &[
                dir("BDMV"),
                dir("BDMV/AUXDATA"),
                entry("BDMV/STREAM/00001.m2ts", 100_000_000),
                entry("BDMV/STREAM/00000.m2ts", 5000),
                entry("BDMV/index.bdmv", 100),
                entry("BDMV/empty.bin", 0),
                dir("CERTIFICATE/BACKUP"),
            ],
        )
        .unwrap();
        assert_eq!((plan.dir_count(), plan.file_count()), (6, 4)); // root, BDMV, AUXDATA, STREAM, CERTIFICATE, BACKUP
        assert_eq!(plan.meta1 % 32, 0);
        assert_eq!(plan.meta2 % 32, 0);
        assert_eq!(plan.meta_blocks % 32, 0);
        for path in ["BDMV/STREAM/00000.m2ts", "BDMV/STREAM/00001.m2ts", "BDMV/index.bdmv"] {
            assert_eq!(plan.file_sector(path).unwrap() % 32, 0, "{path}");
        }
        // Sorted by name: 00000 before 00001; the metadata copies enclose the data.
        let (a, b) = (plan.file_sector("BDMV/STREAM/00000.m2ts").unwrap(), plan.file_sector("BDMV/STREAM/00001.m2ts").unwrap());
        assert!(a < b && b + 48829 <= PARTITION_START + plan.meta2);
        assert_eq!(plan.total_sectors, PARTITION_START + plan.partition_len + 1);
        // Unique ids: root 0, the rest from 16.
        assert_eq!(plan.nodes[0].unique, 0);
        let mut ids: Vec<u32> = plan.nodes.iter().map(|n| n.unique).collect();
        ids.sort();
        assert_eq!(ids[1], 16);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        // Conflicts are rejected.
        assert!(plan_entries("BD", &[entry("a/b", 1), entry("a/b/c", 1)]).is_err());
        assert!(plan_entries("BD", &[entry("a", 1), entry("a", 1)]).is_err());
    }

    #[test]
    fn big_file_is_split_into_extents() {
        let size = 3u64 << 30;
        let plan = plan_entries("BD", &[entry("BDMV/STREAM/00000.m2ts", size)]).unwrap();
        let first = plan.file_sector("BDMV/STREAM/00000.m2ts").unwrap() - PARTITION_START;
        let e = extents(first, size);
        assert_eq!(e.len(), 4, "3 GiB need 4 extents below 1 GiB");
        assert!(e.iter().all(|&(_, len)| len < 1 << 30 && len % SECTOR == 0 || len == e.last().unwrap().1));
        assert_eq!(e.iter().map(|x| x.1).sum::<u64>(), size);
        // The File Entry of such a file still fits one sector.
        let ads: Vec<u8> = e.iter().flat_map(|&(b, l)| long_ad(l, b)).collect();
        assert_eq!(ads.len(), 64);
        assert!(efe(5, true, 1, size, size / SECTOR, &ads, 16, 3, &Timestamp::now()).len() == 2048);
        // Partition end is aligned, so the image is about 3 GiB.
        assert!(plan.size_bytes() > size && plan.size_bytes() < size + (4 << 20));
        assert!(is_dual_layer(BD25_SECTORS + 1) && !is_dual_layer(BD25_SECTORS));
    }

    #[test]
    fn written_image_has_udf_250_structures() {
        let root = sample_tree("write");
        let iso = root.join("out.iso");
        let cancel = AtomicBool::new(false);
        let plan = create_bd_image(&root, &iso, "MY_BD", &cancel, |_| {}).unwrap();
        let img = std::fs::read(&iso).unwrap();
        assert_eq!(img.len() as u64, plan.size_bytes());
        let sec = |n: usize| &img[n * 2048..(n + 1) * 2048];
        // Volume recognition sequence.
        assert_eq!(&sec(16)[1..6], b"BEA01");
        assert_eq!(&sec(17)[1..6], b"NSR03");
        assert_eq!(&sec(18)[1..6], b"TEA01");
        // Anchors at 256 and at the last sector, descriptor version 3.
        for a in [256, img.len() / 2048 - 1] {
            assert_eq!((le16(sec(a), 0), le16(sec(a), 2), le32(sec(a), 12) as usize), (2, 3, a));
        }
        // Logical volume descriptor: domain id with revision 0x0250 and two partition maps.
        let lvd = sec(35);
        assert_eq!((le16(lvd, 0), le16(lvd, 2)), (6, 3));
        assert_eq!(&lvd[217..217 + 19], b"*OSTA UDF Compliant");
        assert_eq!(le16(lvd, 216 + 24), 0x0250);
        assert_eq!((le32(lvd, 264), le32(lvd, 268)), (70, 2));
        assert_eq!((lvd[440], lvd[441]), (1, 6), "type 1 (physical) map");
        let m = &lvd[446..510];
        assert_eq!((m[0], m[1]), (2, 64), "type 2 (metadata) map");
        assert_eq!(&m[5..28], METADATA_MAP_ID.as_bytes());
        assert_eq!(le16(m, 28), 0x0250);
        assert_eq!((le32(m, 40), le32(m, 44), le32(m, 48)), (0, 1, u32::MAX));
        assert_eq!((le32(m, 52), le16(m, 56), m[58]), (32, 32, 1));
        // FSD long_ad points into the metadata partition (reference 1).
        assert_eq!((le32(lvd, 252), le16(lvd, 256)), (0, 1));
        // Metadata file and mirror: EFEs of type 250/251 at the start of the partition.
        let p = PARTITION_START as usize;
        assert_eq!((le16(sec(p), 0), sec(p)[27]), (266, 250));
        assert_eq!((le16(sec(p + 1), 0), sec(p + 1)[27]), (266, 251));
        let (mf, mm) = (le32(sec(p), 220) as usize, le32(sec(p + 1), 220) as usize);
        assert_eq!(mf, plan.meta1 as usize);
        // The mirror is a byte-identical copy.
        let len = plan.meta_blocks as usize * 2048;
        assert_eq!(&img[(p + mf) * 2048..(p + mf) * 2048 + len], &img[(p + mm) * 2048..(p + mm) * 2048 + len]);
        // The FSD is the first block of the metadata; the data of a file is where the plan says.
        assert_eq!(le16(sec(p + mf), 0), 256);
        let at = plan.file_sector("BDMV/index.bdmv").unwrap() as usize;
        assert_eq!(&sec(at)[..24], b"INDX0200 some index data");
        let at = plan.file_sector("BDMV/STREAM/00000.m2ts").unwrap() as usize;
        assert_eq!(sec(at)[250], 250);
        // LVID: number of files/dirs and revisions.
        let lvid = sec(64);
        assert_eq!((le16(lvid, 0), le32(lvid, 120), le32(lvid, 124)), (9, 3, 7));
        assert_eq!((le16(lvid, 128), le16(lvid, 130), le16(lvid, 132)), (0x0250, 0x0250, 0x0250));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn requires_index_bdmv_and_cancel_leaves_nothing() {
        let root = sample_tree("cancel");
        let iso = root.join("c.iso");
        let cancel = AtomicBool::new(true);
        assert!(matches!(create_bd_image(&root, &iso, "X", &cancel, |_| {}), Err(ImageError::Cancelled)));
        assert!(!iso.exists() && !iso.with_extension("iso.part").exists());
        std::fs::remove_file(root.join("BDMV/index.bdmv")).unwrap();
        assert!(matches!(plan_bd_image(&root, "X"), Err(ImageError::NotBluray(_))));
        std::fs::remove_dir_all(&root).ok();
    }
}
