//! "Build image" mode: turns files and folders into a disc image plan (DVD-Video, Blu-ray or plain
//! data), applies the build settings and writes or streams the image.
//!
//! Content is detected like ImgBurn does: a `VIDEO_TS` folder with `VIDEO_TS.IFO` is DVD-Video
//! (UDF 1.02 bridge in disc order, layer break aware), a `BDMV` folder with `index.bdmv` is a
//! Blu-ray (UDF 2.50), anything else is a data tree (ISO 9660 + Joliet + UDF 1.02 by default).

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use md5::{Digest, Md5};

use crate::config::{StudioBuild, StudioFilter};

use super::label::sanitize_volume_label;
use super::layerbreak::{plan_disc_with, Capacities, LayerBreak, PlanError};
use super::reader::{write_layout, Split, SplitWriter};
use super::source::StreamSource;
use super::udf::{self, plan_image_with, plan_tree, scan, ImageError, ImagePlan, Timestamp, Transform, TreeEntry, TreeOptions, SECTOR};
use super::udf250::{self, BdImagePlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Content {
    DvdVideo,
    Bluray,
    Data,
}

impl Content {
    pub fn name(self) -> &'static str {
        match self {
            Content::DvdVideo => "DVD-Video",
            Content::Bluray => "Blu-ray",
            Content::Data => "Data",
        }
    }
}

/// Case-insensitive child lookup.
fn child(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).map(|e| e.path())
}

/// The `VIDEO_TS` folder for `root` (the folder itself or a child of it).
pub fn find_video_ts(root: &Path) -> Option<PathBuf> {
    let is_video_ts = |p: &Path| child(p, "VIDEO_TS.IFO").is_some();
    if root.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("VIDEO_TS")) && is_video_ts(root) {
        return Some(root.to_path_buf());
    }
    child(root, "VIDEO_TS").filter(|p| p.is_dir() && is_video_ts(p))
}

/// The folder containing `BDMV` (`root` itself, or its parent when `root` is the `BDMV` folder).
pub fn find_bd_root(root: &Path) -> Option<PathBuf> {
    let has_index = |bdmv: &Path| child(bdmv, "index.bdmv").is_some();
    if root.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("BDMV")) && has_index(root) {
        return root.parent().map(Path::to_path_buf);
    }
    child(root, "BDMV").filter(|p| p.is_dir() && has_index(p)).map(|_| root.to_path_buf())
}

pub fn detect(root: &Path) -> Content {
    if find_video_ts(root).is_some() {
        Content::DvdVideo
    } else if find_bd_root(root).is_some() {
        Content::Bluray
    } else {
        Content::Data
    }
}

/// How to treat a detected DVD-Video/Blu-ray folder: `answer` is the user's reply to the
/// question when the setting is `ask`.
pub fn resolve_content(detected: Content, prompts: &str, answer: Option<bool>) -> Result<Content, BuildError> {
    if detected == Content::Data {
        return Ok(Content::Data);
    }
    match (prompts, answer) {
        ("always", _) | ("ask", Some(true)) => Ok(detected),
        ("never", _) | ("ask", Some(false)) => Ok(Content::Data),
        _ => Err(BuildError::NeedsAnswer(detected)),
    }
}

// ---- collecting files ------------------------------------------------------------------------

/// A file or folder to put on the disc. A folder with an empty `dest` adds its *contents* to the
/// image root, otherwise it becomes the directory `dest`.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub dest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// File attributes the filters look at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Attrs {
    pub hidden: bool,
    pub system: bool,
    pub reparse: bool,
}

#[cfg(windows)]
fn attrs_of(meta: &std::fs::Metadata, _name: &str) -> Attrs {
    use std::os::windows::fs::MetadataExt;
    let a = meta.file_attributes();
    Attrs { hidden: a & 0x2 != 0, system: a & 0x4 != 0, reparse: a & 0x400 != 0 }
}

#[cfg(not(windows))]
fn attrs_of(meta: &std::fs::Metadata, name: &str) -> Attrs {
    Attrs { hidden: name.starts_with('.'), system: false, reparse: meta.file_type().is_symlink() }
}

pub fn skip_reason(a: Attrs, f: &StudioFilter) -> Option<&'static str> {
    if f.skip_hidden && a.hidden {
        Some("hidden")
    } else if f.skip_system && a.system {
        Some("system file")
    } else if f.skip_reparse && a.reparse {
        Some("Link (reparse point)")
    } else {
        None
    }
}

fn walk(dir: &Path, rel: &str, f: &StudioFilter, out: &mut Vec<TreeEntry>, skipped: &mut Vec<Skipped>) -> Result<(), ImageError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).map_err(udf::io(dir))?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(link_meta) = std::fs::symlink_metadata(&path) else {
            skipped.push(Skipped { path, reason: "unreadable".into() });
            continue;
        };
        if let Some(reason) = skip_reason(attrs_of(&link_meta, &name), f) {
            skipped.push(Skipped { path, reason: reason.into() });
            continue;
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                skipped.push(Skipped { path, reason: "unreadable".into() });
                continue;
            }
        };
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if meta.is_dir() {
            out.push(TreeEntry::dir(&rel));
            walk(&path, &rel, f, out, skipped)?;
        } else if f.skip_in_use && std::fs::File::open(&path).is_err() {
            skipped.push(Skipped { path, reason: "in Benutzung".into() });
        } else {
            out.push(TreeEntry::file(&rel, path, meta.len()));
        }
    }
    Ok(())
}

/// Lists everything below the sources as tree entries, leaving out what the filters exclude.
pub fn collect(sources: &[Source], filter: &StudioFilter) -> Result<(Vec<TreeEntry>, Vec<Skipped>), ImageError> {
    let (mut entries, mut skipped) = (Vec::new(), Vec::new());
    for s in sources {
        let meta = std::fs::metadata(&s.path).map_err(udf::io(&s.path))?;
        let name = s.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if meta.is_dir() {
            if !s.dest.is_empty() {
                entries.push(TreeEntry::dir(&s.dest));
            }
            walk(&s.path, &s.dest, filter, &mut entries, &mut skipped)?;
        } else {
            let dest = if s.dest.is_empty() { name } else { s.dest.clone() };
            entries.push(TreeEntry::file(&dest, s.path.clone(), meta.len()));
        }
    }
    Ok((entries, skipped))
}

// ---- Blu-ray specifics -----------------------------------------------------------------------

/// Clears the `copy_permission_indicator` (top two bits of the 4-byte `TP_extra_header`) of every
/// 192-byte source packet of an M2TS file. The `.clpi` copy of the flag is not touched.
pub fn reset_cpi(chunk: &mut [u8], offset: u64) {
    const PACKET: u64 = 192;
    let first = (PACKET - offset % PACKET) % PACKET;
    let mut i = first as usize;
    while i < chunk.len() {
        chunk[i] &= 0x3F;
        i += PACKET as usize;
    }
}

/// The disc name from `BDMV/META/DL/bdmt_eng.xml` (`<di:name>`).
pub fn read_bdmt_label(bd_root: &Path) -> Option<String> {
    let dl = child(&child(&child(bd_root, "BDMV")?, "META")?, "DL")?;
    let text = std::fs::read_to_string(child(&dl, "bdmt_eng.xml")?).ok()?;
    let start = text.find("<di:name>")? + "<di:name>".len();
    let name = text[start..start + text[start..].find("</di:name>")?].trim();
    (!name.is_empty()).then(|| name.to_string())
}

// ---- DVD-Video specifics ---------------------------------------------------------------------

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

/// Sets every title's start sector in the `TT_SRPT` of a VMG IFO/BUP to the planned distance
/// between `VIDEO_TS.IFO` and its titleset's IFO; returns how many entries changed.
pub fn fix_title_sectors(plan: &ImagePlan, vmg: &mut [u8]) -> usize {
    let (Some(vmg_file), true) = (plan.file("VIDEO_TS.IFO"), vmg.starts_with(b"DVDVIDEO-VMG")) else { return 0 };
    let Some(base) = be32(vmg, 0xC4).map(|s| s as usize * SECTOR as usize) else { return 0 };
    let Some(titles) = vmg.get(base..base + 2).map(|s| u16::from_be_bytes([s[0], s[1]]) as usize) else { return 0 };
    let mut changed = 0;
    for i in 0..titles {
        let e = base + 8 + 12 * i;
        let Some(vts) = vmg.get(e + 6).copied() else { break };
        let (Some(ifo), Some(current)) = (plan.file(&format!("VTS_{vts:02}_0.IFO")), be32(vmg, e + 8)) else { continue };
        let want = ifo.lba - vmg_file.lba;
        if want != current {
            vmg[e + 8..e + 12].copy_from_slice(&want.to_be_bytes());
            changed += 1;
        }
    }
    changed
}

/// IFO and BUP of a titleset share a 16-sector ECC block (a scratch on that block damages both).
fn ecc_shared(plan: &ImagePlan) -> Vec<String> {
    let mut out = Vec::new();
    for f in plan.files.iter().filter(|f| f.name.ends_with(".IFO") && f.name.starts_with("V")) {
        let bup = f.name.replace(".IFO", ".BUP");
        if let Some(b) = plan.file(&bup) {
            let last = f.lba + f.sectors().saturating_sub(1);
            if last / 16 == b.lba / 16 {
                out.push(format!("{} and {bup} share an ECC block", f.name));
            }
        }
    }
    out
}

// ---- the plan --------------------------------------------------------------------------------

#[derive(Debug)]
pub enum BuildError {
    Image(ImageError),
    Plan(PlanError),
    /// The folder looks like DVD-Video/Blu-ray and the setting asks the user what to do.
    NeedsAnswer(Content),
    Empty,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Image(e) => write!(f, "{e}"),
            BuildError::Plan(e) => write!(f, "{e}"),
            BuildError::NeedsAnswer(c) => write!(f, "The folder looks like {} – build it as a disc?", c.name()),
            BuildError::Empty => write!(f, "no files to burn"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<ImageError> for BuildError {
    fn from(e: ImageError) -> Self {
        BuildError::Image(e)
    }
}

impl From<PlanError> for BuildError {
    fn from(e: PlanError) -> Self {
        BuildError::Plan(e)
    }
}

/// A laid out image of either kind.
#[derive(Debug, Clone)]
pub enum BuiltImage {
    Udf(ImagePlan),
    Bd(BdImagePlan),
}

impl BuiltImage {
    pub fn total_sectors(&self) -> u32 {
        match self {
            BuiltImage::Udf(p) => p.total_sectors,
            BuiltImage::Bd(p) => p.total_sectors,
        }
    }

    /// Writes the image front to back into `out`.
    pub fn write_to<W: Write>(&self, out: W, name: &Path, ts: &Timestamp, cancel: &AtomicBool, progress: impl FnMut(f64)) -> Result<(), ImageError> {
        match self {
            BuiltImage::Udf(p) => udf::write_to(p, out, name, ts, cancel, progress),
            BuiltImage::Bd(p) => udf250::write_bd_to(p, out, name, ts, cancel, progress),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuildInput {
    pub sources: Vec<Source>,
    /// Empty = from the Blu-ray disc name or the default label.
    pub label: String,
    /// The user's answer when the detected content needs a question (`prompts = ask`).
    pub answer: Option<bool>,
    /// Dual-layer limits: DVD-Video content gets a layer break when it exceeds one layer.
    pub dvd_caps: Option<Capacities>,
    /// A layer break picked by the user (a candidate of the inspector).
    pub layer_break: Option<LayerBreak>,
}

#[derive(Debug)]
pub struct BuildPlan {
    pub content: Content,
    pub label: String,
    pub image: BuiltImage,
    pub layer_break: Option<LayerBreak>,
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<String>,
    /// Creation time written into the image; fixed at planning so every way of writing the plan
    /// (file, stream, second copy) yields the same bytes.
    pub timestamp: Timestamp,
}

impl BuildPlan {
    pub fn total_sectors(&self) -> u32 {
        self.image.total_sectors()
    }
}

pub fn plan_build(input: &BuildInput, cfg: &StudioBuild) -> Result<BuildPlan, BuildError> {
    let single_dir = (input.sources.len() == 1 && input.sources[0].path.is_dir()).then(|| input.sources[0].path.clone());
    let detected = single_dir.as_deref().map_or(Content::Data, detect);
    let content = resolve_content(detected, &cfg.prompts, input.answer)?;
    let label = |fallback: Option<String>| {
        let name = if input.label.trim().is_empty() { fallback.unwrap_or_default() } else { input.label.clone() };
        sanitize_volume_label(&name)
    };
    let mut warnings = Vec::new();
    let timestamp = Timestamp::now();
    match content {
        Content::DvdVideo => {
            let video_ts = find_video_ts(single_dir.as_deref().expect("detected from a folder")).expect("detected");
            let label = label(None);
            let (mut image, layer_break) = match input.dvd_caps {
                Some(caps) => {
                    let disc = plan_disc_with(&video_ts, &label, caps, cfg.dvd.create_audio_ts, input.layer_break)?;
                    (disc.image, disc.layer_break)
                }
                None => (plan_image_with(&video_ts, &label, &Default::default(), &Default::default(), cfg.dvd.create_audio_ts)?, None),
            };
            if cfg.dvd.fix_vts_sectors {
                image = fix_dvd_sectors(&video_ts, &label, image, layer_break, cfg.dvd.create_audio_ts, &mut warnings)?;
            }
            // shortcut: only detected, not separated by padding (option `ifo_bup_ecc_padding`).
            warnings.extend(ecc_shared(&image));
            Ok(BuildPlan { content, label, image: BuiltImage::Udf(image), layer_break, skipped: vec![], warnings, timestamp })
        }
        Content::Bluray => {
            let root = find_bd_root(single_dir.as_deref().expect("detected from a folder")).expect("detected");
            let label = label(cfg.bd.read_bdmt.then(|| read_bdmt_label(&root)).flatten());
            let mut entries = Vec::new();
            scan(&child(&root, "BDMV").expect("detected"), "BDMV", &mut entries)?;
            match child(&root, "CERTIFICATE") {
                Some(cert) => scan(&cert, "CERTIFICATE", &mut entries)?,
                None if cfg.bd.create_certificate => {
                    entries.push(TreeEntry::dir("CERTIFICATE"));
                    entries.push(TreeEntry::dir("CERTIFICATE/BACKUP"));
                }
                None => {}
            }
            if cfg.bd.reset_cpi {
                let reset: Transform = reset_cpi;
                for e in entries.iter_mut().filter(|e| !e.dir) {
                    let p = e.path.to_ascii_uppercase();
                    if p.starts_with("BDMV/STREAM/") && p.ends_with(".M2TS") {
                        e.transform = Some(reset);
                    }
                }
            }
            let image = udf250::plan_entries(&label, &entries)?;
            Ok(BuildPlan { content, label, image: BuiltImage::Bd(image), layer_break: None, skipped: vec![], warnings, timestamp })
        }
        Content::Data => {
            let (entries, skipped) = collect(&input.sources, &cfg.filter)?;
            if !entries.iter().any(|e| !e.dir) {
                return Err(BuildError::Empty);
            }
            let label = label(None);
            let image = if cfg.fs == "udf250" {
                BuiltImage::Bd(udf250::plan_entries(&label, &entries)?)
            } else {
                let opts = TreeOptions { joliet: cfg.fs == "iso_joliet_udf102", start_lba: cfg.start_lba, min_sectors: cfg.min_sectors, dedupe: cfg.dedupe };
                let plan = plan_tree(&label, &entries, &opts)?;
                warnings.extend(plan.warnings.iter().cloned());
                BuiltImage::Udf(plan)
            };
            Ok(BuildPlan { content, label, image, layer_break: None, skipped, warnings, timestamp })
        }
    }
}

/// Re-plans a DVD image with corrected `TT_SRPT` entries when the layout moved a titleset.
fn fix_dvd_sectors(
    video_ts: &Path,
    label: &str,
    image: ImagePlan,
    layer_break: Option<LayerBreak>,
    audio_ts: bool,
    warnings: &mut Vec<String>,
) -> Result<ImagePlan, BuildError> {
    let read = |name: &str| -> Result<Vec<u8>, ImageError> {
        let f = image.file(name).expect("checked by the planner");
        Ok(match &f.data {
            Some(d) => d.clone(),
            None => std::fs::read(&f.source).map_err(udf::io(&f.source))?,
        })
    };
    let mut overrides = std::collections::HashMap::new();
    let mut changed = 0;
    for name in ["VIDEO_TS.IFO", "VIDEO_TS.BUP"] {
        let mut data = read(name)?;
        changed += fix_title_sectors(&image, &mut data);
        overrides.insert(name.to_string(), data);
    }
    if changed == 0 {
        return Ok(image);
    }
    warnings.push(format!("{changed} title start sectors corrected in VIDEO_TS.IFO/BUP"));
    let padding: std::collections::HashMap<u32, u32> = layer_break.and_then(|lb| lb.padding).into_iter().collect();
    Ok(plan_image_with(video_ts, label, &padding, &overrides, audio_ts)?)
}

// ---- writing ---------------------------------------------------------------------------------

struct HashWriter<W: Write> {
    inner: W,
    md5: Option<Md5>,
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        if let Some(h) = self.md5.as_mut() {
            h.update(&data[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuildReport {
    pub sectors: u32,
    pub md5: Option<String>,
    pub files: Vec<PathBuf>,
    pub layout_file: Option<PathBuf>,
}

/// Writes the planned image to `out` (split if asked), with the MD5 of the image and the layout
/// file (`auto`, `none`, `dvd`, `mds`, `cue`) next to it.
#[allow(clippy::too_many_arguments)]
pub fn build_to_file(
    plan: &BuildPlan,
    out: &Path,
    split: Split,
    md5: bool,
    layout_file: &str,
    cancel: &AtomicBool,
    progress: impl FnMut(f64),
) -> Result<BuildReport, ImageError> {
    let sectors = plan.total_sectors();
    let writer = SplitWriter::new(out, split, sectors as u64 * SECTOR);
    let mut hw = HashWriter { inner: writer, md5: md5.then(Md5::new) };
    let result = plan.image.write_to(&mut hw, out, &plan.timestamp, cancel, progress);
    let HashWriter { inner, md5: hasher } = hw;
    match result {
        Ok(()) => {
            let files = inner.commit().map_err(udf::io(out))?;
            let layout = if files.len() == 1 {
                write_layout(&files[0], layout_file, sectors as u64, plan.layer_break.map(|l| l.l0_sectors)).map_err(udf::io(out))?
            } else {
                None
            };
            Ok(BuildReport { sectors, md5: hasher.map(|h| format!("{:x}", h.finalize())), files, layout_file: layout })
        }
        Err(e) => {
            inner.abort();
            Err(e)
        }
    }
}

/// Streams the image to a consumer while it is built (build and burn without an `.iso`).
/// `cancel` stops the builder thread.
pub fn stream_image(plan: &BuildPlan, ring_bytes: usize, low_pct: u32, high_pct: u32, cancel: Arc<AtomicBool>) -> StreamSource {
    let (image, ts) = (plan.image.clone(), plan.timestamp);
    let sectors = image.total_sectors() as u64;
    StreamSource::spawn(sectors, ring_bytes, low_pct, high_pct, move |sink| {
        image.write_to(sink, Path::new("<Puffer>"), &ts, &cancel, |_| {}).map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::isoread::IsoImage;
    use crate::burner::source::SectorSource;
    use crate::burner::testutil::{synthetic_dvd, synthetic_video_ts, Cell};
    use std::sync::atomic::Ordering;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cx_builder_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn input(path: &Path) -> BuildInput {
        BuildInput { sources: vec![Source { path: path.to_path_buf(), dest: String::new() }], ..Default::default() }
    }

    fn always() -> StudioBuild {
        StudioBuild { prompts: "always".into(), ..Default::default() }
    }

    #[test]
    fn detects_and_asks() {
        let dvd = synthetic_dvd("detect_dvd", &[(1, vec![Cell { first: 0, last: 9, flags: 0 }])]);
        assert_eq!(detect(&dvd), Content::DvdVideo);
        assert_eq!(detect(&dvd.join("VIDEO_TS")), Content::DvdVideo, "the VIDEO_TS folder itself");
        let data = temp("detect_data");
        std::fs::write(data.join("a.txt"), "x").unwrap();
        assert_eq!(detect(&data), Content::Data);
        let bd = temp("detect_bd");
        std::fs::create_dir_all(bd.join("BDMV")).unwrap();
        assert_eq!(detect(&bd), Content::Data, "no index.bdmv");
        std::fs::write(bd.join("BDMV").join("index.bdmv"), "x").unwrap();
        assert_eq!(detect(&bd), Content::Bluray);
        assert_eq!(find_bd_root(&bd.join("BDMV")), Some(bd.clone()));

        assert!(matches!(resolve_content(Content::DvdVideo, "ask", None), Err(BuildError::NeedsAnswer(Content::DvdVideo))));
        assert_eq!(resolve_content(Content::DvdVideo, "ask", Some(true)).unwrap(), Content::DvdVideo);
        assert_eq!(resolve_content(Content::Bluray, "ask", Some(false)).unwrap(), Content::Data);
        assert_eq!(resolve_content(Content::Bluray, "always", None).unwrap(), Content::Bluray);
        assert_eq!(resolve_content(Content::DvdVideo, "never", None).unwrap(), Content::Data);
        assert_eq!(resolve_content(Content::Data, "ask", None).unwrap(), Content::Data);
        for d in [dvd, data, bd] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn filters_decide_by_attributes() {
        let f = StudioFilter::default();
        assert_eq!(skip_reason(Attrs { hidden: true, ..Default::default() }, &f), Some("hidden"));
        assert_eq!(skip_reason(Attrs { system: true, ..Default::default() }, &f), Some("system file"));
        assert!(skip_reason(Attrs { reparse: true, ..Default::default() }, &f).unwrap().contains("Link"));
        assert_eq!(skip_reason(Attrs::default(), &f), None);
        let off = StudioFilter { skip_hidden: false, skip_system: false, skip_reparse: false, skip_in_use: false };
        assert_eq!(skip_reason(Attrs { hidden: true, system: true, reparse: true }, &off), None);
    }

    #[test]
    fn collects_trees_with_destinations() {
        let dir = temp("collect");
        std::fs::create_dir_all(dir.join("src").join("sub")).unwrap();
        std::fs::write(dir.join("src").join("a.txt"), "aa").unwrap();
        std::fs::write(dir.join("src").join("sub").join("b.txt"), "bbb").unwrap();
        std::fs::write(dir.join("loose.bin"), "l").unwrap();
        let sources = [
            Source { path: dir.join("src"), dest: String::new() },
            Source { path: dir.join("src"), dest: "Kopie".into() },
            Source { path: dir.join("loose.bin"), dest: String::new() },
        ];
        let (entries, skipped) = collect(&sources, &StudioFilter::default()).unwrap();
        assert!(skipped.is_empty());
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "sub", "sub/b.txt", "Kopie", "Kopie/a.txt", "Kopie/sub", "Kopie/sub/b.txt", "loose.bin"]);
        assert_eq!(entries.iter().find(|e| e.path == "sub/b.txt").unwrap().size, 3);
        assert!(collect(&[Source { path: dir.join("fehlt"), dest: String::new() }], &StudioFilter::default()).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn data_image_has_the_settings_applied() {
        let dir = temp("data");
        std::fs::write(dir.join("a.txt"), vec![5u8; 3000]).unwrap();
        std::fs::write(dir.join("b.txt"), vec![5u8; 3000]).unwrap();
        let cfg = StudioBuild { fs: "iso_joliet_udf102".into(), ..Default::default() };
        let mut inp = input(&dir);
        inp.label = "Meine Daten".into();
        let plan = plan_build(&inp, &cfg).unwrap();
        assert_eq!((plan.content, plan.label.as_str()), (Content::Data, "MEINE_DATEN"));
        assert!(plan.total_sectors() >= cfg.min_sectors, "minimum size of 600 sectors");
        let BuiltImage::Udf(ref p) = plan.image else { panic!("UDF 1.02 expected") };
        assert!(p.files[1].is_duplicate(), "dedupe is on by default");

        let out = dir.join("out.iso");
        let report = build_to_file(&plan, &out, Split::None, true, "auto", &AtomicBool::new(false), |_| {}).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(bytes.len() as u64, plan.total_sectors() as u64 * SECTOR);
        assert_eq!(report.md5.unwrap(), format!("{:x}", Md5::digest(&bytes)));
        let mut iso = IsoImage::open_joliet(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(iso.read_file("b.txt").unwrap(), vec![5u8; 3000]);

        // Same tree as UDF 2.50 (no ISO side) and without a file at all.
        let bd = plan_build(&inp, &StudioBuild { fs: "udf250".into(), ..Default::default() }).unwrap();
        assert!(matches!(bd.image, BuiltImage::Bd(_)));
        let empty = temp("data_empty");
        assert!(matches!(plan_build(&input(&empty), &cfg), Err(BuildError::Empty)));
        for d in [dir, empty] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn dvd_video_gets_a_layer_break_and_consistent_titles() {
        let cells = vec![Cell { first: 0, last: 499, flags: 0 }, Cell { first: 500, last: 799, flags: 0 }, Cell { first: 800, last: 999, flags: 0 }];
        let dir = synthetic_dvd("dvd_build", &[(1, cells)]);
        let plain = StudioBuild { dvd: crate::config::StudioDvdBuild { fix_vts_sectors: false, ..Default::default() }, ..always() };
        let plan = plan_build(&input(&dir), &plain).unwrap();
        assert_eq!(plan.content, Content::DvdVideo);
        assert!(plan.layer_break.is_none());
        let total = plan.total_sectors();

        let mut inp = input(&dir);
        inp.dvd_caps = Some(Capacities { single_layer: total - 1, layer: total });
        let dl = plan_build(&inp, &plain).unwrap();
        let lb = dl.layer_break.expect("needs two layers");
        assert_eq!(lb.l0_sectors % 16, 0);

        // A layer break chosen by the user is used as given.
        let forced = LayerBreak { l0_sectors: lb.l0_sectors, padding: lb.padding };
        inp.layer_break = Some(forced);
        assert_eq!(plan_build(&inp, &plain).unwrap().layer_break, Some(forced));
        inp.layer_break = Some(LayerBreak { l0_sectors: 16, padding: None });
        assert!(matches!(plan_build(&inp, &plain), Err(BuildError::Plan(_))), "break that cannot work is refused");

        // Without AUDIO_TS the directory is left out; the titles still point at their titlesets.
        let cfg = StudioBuild { dvd: crate::config::StudioDvdBuild { create_audio_ts: false, fix_vts_sectors: false, ..Default::default() }, ..always() };
        let plan = plan_build(&input(&dir), &cfg).unwrap();
        let BuiltImage::Udf(ref p) = plan.image else { panic!() };
        let vmg = p.file("VIDEO_TS.IFO").unwrap();
        let ifo = std::fs::read(&vmg.source).unwrap();
        let mut copy = ifo.clone();
        assert_eq!(fix_title_sectors(p, &mut copy), 1, "synthetic TT_SRPT has start sector 0");
        assert_eq!(be32(&copy, 2048 + 8 + 8), Some(p.file("VTS_01_0.IFO").unwrap().lba - vmg.lba));
        assert_eq!(fix_title_sectors(p, &mut copy), 0, "idempotent");
        assert_eq!(fix_title_sectors(p, &mut vec![0u8; 100]), 0, "not a VMG");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fix_vts_sectors_rewrites_the_image_tables() {
        let dir = synthetic_dvd("dvd_fix", &[(1, vec![Cell { first: 0, last: 99, flags: 0 }])]);
        let plan = plan_build(&input(&dir), &always()).unwrap();
        assert!(plan.warnings.iter().any(|w| w.contains("corrected")), "{:?}", plan.warnings);
        let BuiltImage::Udf(ref p) = plan.image else { panic!() };
        let f = p.file("VIDEO_TS.IFO").unwrap();
        let data = f.data.as_ref().expect("patched content");
        let want = p.file("VTS_01_0.IFO").unwrap().lba - f.lba;
        assert_eq!(be32(data, 2048 + 16), Some(want));
        assert_eq!(p.file("VIDEO_TS.BUP").unwrap().data.as_deref(), Some(&data[..]), "BUP gets the same table");
        let off = StudioBuild { dvd: crate::config::StudioDvdBuild { fix_vts_sectors: false, ..Default::default() }, ..always() };
        let plan = plan_build(&input(&dir), &off).unwrap();
        let BuiltImage::Udf(ref p) = plan.image else { panic!() };
        assert!(p.file("VIDEO_TS.IFO").unwrap().data.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn blu_ray_gets_certificate_cpi_reset_and_disc_name() {
        let dir = temp("bd");
        let bdmv = dir.join("BDMV");
        std::fs::create_dir_all(bdmv.join("STREAM")).unwrap();
        std::fs::create_dir_all(bdmv.join("META").join("DL")).unwrap();
        std::fs::write(bdmv.join("index.bdmv"), "INDX").unwrap();
        let mut m2ts = vec![0u8; 192 * 10];
        for p in 0..10 {
            m2ts[p * 192] = 0xC0 | p as u8; // CPI bits set, plus a marker in the low bits
        }
        std::fs::write(bdmv.join("STREAM").join("00000.m2ts"), &m2ts).unwrap();
        std::fs::write(bdmv.join("META").join("DL").join("bdmt_eng.xml"), "<x><di:name>Mein Film</di:name></x>").unwrap();

        let cfg = StudioBuild { bd: crate::config::StudioBdBuild { reset_cpi: true, ..Default::default() }, ..always() };
        let plan = plan_build(&input(&dir), &cfg).unwrap();
        assert_eq!((plan.content, plan.label.as_str()), (Content::Bluray, "MEIN_FILM"));
        let BuiltImage::Bd(ref bd) = plan.image else { panic!("UDF 2.50 expected") };
        let out = dir.join("bd.iso");
        build_to_file(&plan, &out, Split::None, false, "none", &AtomicBool::new(false), |_| {}).unwrap();
        let img = std::fs::read(&out).unwrap();
        let at = bd.file_sector("BDMV/STREAM/00000.m2ts").unwrap() as usize * SECTOR as usize;
        for p in 0..10 {
            assert_eq!(img[at + p * 192], p as u8, "packet {p}: CPI bits cleared, marker kept");
        }
        assert_eq!(&img[at + 1..at + 5], &[0, 0, 0, 0]);

        // Without the reset the bits survive.
        let plain = plan_build(&input(&dir), &always()).unwrap();
        let out2 = dir.join("bd2.iso");
        build_to_file(&plain, &out2, Split::None, false, "none", &AtomicBool::new(false), |_| {}).unwrap();
        let BuiltImage::Bd(ref bd2) = plain.image else { panic!() };
        let img2 = std::fs::read(&out2).unwrap();
        assert_eq!(img2[bd2.file_sector("BDMV/STREAM/00000.m2ts").unwrap() as usize * SECTOR as usize], 0xC0);

        let mut chunk = vec![0xFFu8; 400];
        reset_cpi(&mut chunk, 100); // packets start at 192 - 100 = 92 and 284
        assert_eq!(chunk.iter().enumerate().filter(|(_, &b)| b == 0x3F).map(|(i, _)| i).collect::<Vec<_>>(), [92, 284]);
        assert_eq!(read_bdmt_label(&dir), Some("Mein Film".into()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn writes_split_images_and_layout_files() {
        let dir = temp("split");
        std::fs::write(dir.join("big.bin"), vec![9u8; 700_000]).unwrap();
        let cfg = StudioBuild::default();
        let plan = plan_build(&input(&dir), &cfg).unwrap();
        let out = dir.join("out.iso");
        let report = build_to_file(&plan, &out, Split::Gb2, false, "auto", &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(report.files, [out.clone()], "small image: one file, plain name");
        // Layout file for a dual-layer build.
        let mut with_break = plan;
        with_break.layer_break = Some(LayerBreak { l0_sectors: 640, padding: None });
        let out2 = dir.join("dl.iso");
        let report = build_to_file(&with_break, &out2, Split::None, false, "auto", &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(std::fs::read_to_string(report.layout_file.unwrap()).unwrap(), "LayerBreak=640\r\ndl.iso\r\n");
        // Cancel leaves nothing behind.
        let cancelled = AtomicBool::new(true);
        let out3 = dir.join("c.iso");
        assert!(matches!(build_to_file(&with_break, &out3, Split::None, false, "none", &cancelled, |_| {}), Err(ImageError::Cancelled)));
        assert!(!out3.exists() && !dir.join("c.iso.part").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn streamed_image_equals_the_written_one() {
        let dir = synthetic_video_ts("stream");
        // The synthetic folder has no valid IFOs, so build it as plain data.
        let cfg = StudioBuild { prompts: "never".into(), ..Default::default() };
        let mut inp = input(&dir.join("VIDEO_TS"));
        inp.label = "STREAM".into();
        let plan = plan_build(&inp, &cfg).unwrap();
        let out = dir.join("a.iso");
        build_to_file(&plan, &out, Split::None, false, "none", &AtomicBool::new(false), |_| {}).unwrap();
        let want = std::fs::read(&out).unwrap();

        let mut src = stream_image(&plan, 256 * 1024, 20, 90, Arc::new(AtomicBool::new(false)));
        assert_eq!(src.sectors() * SECTOR, want.len() as u64);
        let mut got = vec![0u8; want.len()];
        for (i, chunk) in got.chunks_mut(32 * SECTOR as usize).enumerate() {
            src.read(i as u64 * 32, chunk).unwrap();
        }
        src.finish().unwrap();
        assert!(got == want, "streamed bytes differ from the written file");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stopping_the_stream_stops_the_builder() {
        let dir = temp("stream_stop");
        std::fs::write(dir.join("f.bin"), vec![1u8; 4 << 20]).unwrap();
        let plan = plan_build(&input(&dir), &StudioBuild::default()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let src = stream_image(&plan, 64 * 1024, 0, 100, cancel.clone());
        cancel.store(true, Ordering::SeqCst);
        assert!(src.finish().is_err(), "the builder reports the interruption");
        std::fs::remove_dir_all(dir).ok();
    }
}
