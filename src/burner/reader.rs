//! Disc → image: reads every sector of a [`SectorSource`] into an `.iso` (optionally split), with
//! software retries, zero-fill for unreadable sectors and an MD5 of what was written.
//! CSS/AACS are not touched: protected sectors that the drive refuses count as unreadable.

use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use md5::{Digest, Md5};

use super::sidecar::{write_cue, write_dvd, write_mds, CueMode, DvdLayout, Mds};
use super::source::SectorSource;
use super::udf::SECTOR;

const S: usize = SECTOR as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Split {
    #[default]
    None,
    /// 2 GiB parts.
    Gb2,
    /// Just below 4 GiB (the FAT32 file size limit).
    Gb4,
}

impl Split {
    pub fn parse(s: &str) -> Split {
        match s {
            "2gb" => Split::Gb2,
            "4gb" => Split::Gb4,
            _ => Split::None,
        }
    }

    /// Bytes per part, a multiple of the sector size.
    fn limit(self) -> Option<u64> {
        match self {
            Split::None => None,
            Split::Gb2 => Some(2 << 30),
            Split::Gb4 => Some((4 << 30) - SECTOR),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadOptions {
    /// Extra attempts for a chunk or sector that fails.
    pub sw_retries: u32,
    /// Zero-fill unreadable sectors and go on (they are listed in the report).
    pub ignore_errors: bool,
    pub split: Split,
    pub md5: bool,
    /// Sectors per read request.
    pub chunk_sectors: u32,
    /// `auto`, `none`, `dvd`, `mds` or `cue`: the layout file written next to the image.
    pub layout_file: String,
    /// Layer 0 sectors of a dual-layer disc (from the drive).
    pub layer_break: Option<u32>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions { sw_retries: 20, ignore_errors: false, split: Split::None, md5: false, chunk_sectors: 32, layout_file: "auto".into(), layer_break: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadReport {
    pub sectors: u64,
    /// Sectors that could not be read (zero-filled).
    pub bad: Vec<u64>,
    pub md5: Option<String>,
    /// The image file(s) in order.
    pub files: Vec<PathBuf>,
    pub layout_file: Option<PathBuf>,
}

#[derive(Debug)]
pub enum ReadError {
    Io(PathBuf, io::Error),
    Unreadable { lba: u64, message: String },
    Cancelled,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            ReadError::Unreadable { lba, message } => write!(f, "Sector {lba} unreadable ({message})"),
            ReadError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ReadError {}

#[derive(Debug, Clone, Copy)]
pub struct ReadProgress {
    pub lba: u64,
    pub total: u64,
    pub bad: usize,
}

/// Reads with retries; `Ok(Err(e))` = the data could not be read after all attempts.
fn attempt(src: &mut dyn SectorSource, lba: u64, buf: &mut [u8], retries: u32, cancel: &AtomicBool) -> Result<Result<(), io::Error>, ReadError> {
    let mut last = None;
    for _ in 0..=retries {
        if cancel.load(Ordering::Relaxed) {
            return Err(ReadError::Cancelled);
        }
        match src.read(lba, buf) {
            Ok(()) => return Ok(Ok(())),
            Err(e) => last = Some(e),
        }
    }
    Ok(Err(last.expect("at least one attempt")))
}

/// Reads a chunk; a failing chunk is retried sector by sector. Unreadable sectors are zero-filled
/// and pushed to `bad` when `ignore` is set, otherwise they are an error.
pub(crate) fn read_resilient(
    src: &mut dyn SectorSource,
    lba: u64,
    buf: &mut [u8],
    retries: u32,
    ignore: bool,
    bad: &mut Vec<u64>,
    cancel: &AtomicBool,
) -> Result<(), ReadError> {
    if attempt(src, lba, buf, retries, cancel)?.is_ok() {
        return Ok(());
    }
    for (i, sector) in buf.chunks_exact_mut(S).enumerate() {
        let at = lba + i as u64;
        if let Err(e) = attempt(src, at, sector, retries, cancel)? {
            if !ignore {
                return Err(ReadError::Unreadable { lba: at, message: e.to_string() });
            }
            sector.fill(0);
            bad.push(at);
        }
    }
    Ok(())
}

fn part_name(out: &Path, n: usize) -> PathBuf {
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = out.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    out.with_file_name(format!("{stem}_part{n}{ext}"))
}

/// Writes the layout file for `kind`; returns its path (none for `none`, split images and
/// `auto` on a single-layer disc).
pub fn write_layout(image: &Path, kind: &str, sectors: u64, layer_break: Option<u32>) -> io::Result<Option<PathBuf>> {
    let name = image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let kind = if kind == "auto" {
        if layer_break.is_some() { "dvd" } else { "none" }
    } else {
        kind
    };
    let side = image.with_extension(kind);
    match kind {
        "dvd" => match layer_break {
            Some(l0) => write_dvd(&side, &DvdLayout { layer_break: l0, image: name })?,
            None => return Ok(None), // a .dvd file only exists for dual-layer images
        },
        "mds" => write_mds(&side, &Mds { sectors: sectors as u32, layer_break, image: name })?,
        "cue" => write_cue(&side, &name, CueMode::Mode1_2048)?,
        _ => return Ok(None),
    }
    Ok(Some(side))
}

/// Output file that rolls over to `name_part2.iso`, ... at the split limit. Everything goes to
/// `*.part` files first: [`commit`](Self::commit) renames them, [`abort`](Self::abort) deletes them.
pub struct SplitWriter {
    out: PathBuf,
    limit: Option<u64>,
    total_parts: usize,
    files: Vec<(PathBuf, PathBuf)>, // (temporary, final)
    cur: Option<BufWriter<File>>,
    in_part: u64,
}

impl SplitWriter {
    /// `total_bytes` decides whether the name gets `_partN` suffixes at all.
    pub fn new(out: &Path, split: Split, total_bytes: u64) -> Self {
        let limit = split.limit();
        let total_parts = limit.map_or(1, |l| total_bytes.div_ceil(l).max(1) as usize);
        SplitWriter { out: out.to_path_buf(), limit, total_parts, files: vec![], cur: None, in_part: 0 }
    }

    fn next_file(&mut self) -> io::Result<()> {
        if let Some(mut w) = self.cur.take() {
            w.flush()?;
        }
        let target = if self.total_parts > 1 { part_name(&self.out, self.files.len() + 1) } else { self.out.clone() };
        let mut tmp = target.clone().into_os_string();
        tmp.push(".part");
        let tmp = PathBuf::from(tmp);
        self.cur = Some(BufWriter::with_capacity(1 << 20, File::create(&tmp)?));
        self.files.push((tmp, target));
        self.in_part = 0;
        Ok(())
    }

    /// Flushes and renames the parts; returns the final paths.
    pub fn commit(mut self) -> io::Result<Vec<PathBuf>> {
        if self.files.is_empty() {
            self.next_file()?; // an empty image is still a file
        }
        if let Some(mut w) = self.cur.take() {
            w.flush()?;
        }
        let mut out = Vec::new();
        for (tmp, fin) in &self.files {
            std::fs::rename(tmp, fin)?;
            out.push(fin.clone());
        }
        Ok(out)
    }

    /// Closes and deletes the temporary files.
    pub fn abort(mut self) {
        self.cur = None;
        for (tmp, _) in &self.files {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

impl Write for SplitWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.cur.is_none() || self.limit.is_some_and(|l| self.in_part >= l) {
            self.next_file()?;
        }
        let room = self.limit.map_or(data.len() as u64, |l| l - self.in_part).min(data.len() as u64) as usize;
        let n = self.cur.as_mut().expect("a part is open").write(&data[..room])?;
        self.in_part += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.cur.as_mut().map_or(Ok(()), Write::flush)
    }
}

/// Copies the whole source to `out` (`name_part1.iso`, ... when split). Temporary `*.part` files
/// are renamed on success and removed on failure or cancel.
pub fn read_disc(
    src: &mut dyn SectorSource,
    out: &Path,
    opts: &ReadOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ReadProgress),
) -> Result<ReadReport, ReadError> {
    let total = src.sectors();
    let mut parts = SplitWriter::new(out, opts.split, total * SECTOR);
    let io_err = |e| ReadError::Io(out.to_path_buf(), e);
    let result = (|| {
        let mut hasher = opts.md5.then(Md5::new);
        let mut bad = Vec::new();
        let chunk = opts.chunk_sectors.max(1) as u64;
        let mut buf = vec![0u8; chunk as usize * S];
        let mut lba = 0;
        while lba < total {
            let n = chunk.min(total - lba);
            let data = &mut buf[..n as usize * S];
            read_resilient(src, lba, data, opts.sw_retries, opts.ignore_errors, &mut bad, cancel)?;
            if let Some(h) = hasher.as_mut() {
                h.update(&*data);
            }
            parts.write_all(data).map_err(io_err)?;
            lba += n;
            progress(ReadProgress { lba, total, bad: bad.len() });
        }
        Ok((bad, hasher.map(|h| format!("{:x}", h.finalize()))))
    })();
    match result {
        Ok((bad, md5)) => {
            let files = parts.commit().map_err(io_err)?;
            let layout_file = if files.len() == 1 {
                write_layout(&files[0], &opts.layout_file, total, opts.layer_break).map_err(io_err)?
            } else {
                None
            };
            Ok(ReadReport { sectors: total, bad, md5, files, layout_file })
        }
        Err(e) => {
            parts.abort();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::source::fake::FakeDisc;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cx_reader_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn md5_of(data: &[u8]) -> String {
        format!("{:x}", Md5::digest(data))
    }

    #[test]
    fn reads_everything_and_hashes_it() {
        let dir = temp("plain");
        let mut disc = FakeDisc::pattern(100);
        let want = disc.data.clone();
        let mut last = None;
        let opts = ReadOptions { md5: true, chunk_sectors: 7, ..Default::default() };
        let report = read_disc(&mut disc, &dir.join("a.iso"), &opts, &AtomicBool::new(false), &mut |p| last = Some(p)).unwrap();
        assert_eq!(std::fs::read(dir.join("a.iso")).unwrap(), want);
        assert_eq!((report.sectors, report.bad.len()), (100, 0));
        assert_eq!(report.md5.unwrap(), md5_of(&want));
        assert_eq!(last.unwrap().lba, 100);
        assert!(report.layout_file.is_none(), "auto layout: none for a single-layer disc");
        assert!(!dir.join("a.iso.part").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn retries_recover_flaky_sectors() {
        let dir = temp("retry");
        let mut disc = FakeDisc::pattern(40);
        disc.fail.insert(13, 3); // fails three times, then reads
        let want = disc.data.clone();
        let opts = ReadOptions { sw_retries: 5, chunk_sectors: 8, ..Default::default() };
        let r = read_disc(&mut disc, &dir.join("a.iso"), &opts, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(r.bad.is_empty());
        assert_eq!(std::fs::read(dir.join("a.iso")).unwrap(), want);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unreadable_sectors_stop_or_are_zero_filled() {
        let dir = temp("bad");
        let mut disc = FakeDisc::pattern(40);
        disc.fail.insert(21, u32::MAX);
        let opts = ReadOptions { sw_retries: 2, chunk_sectors: 8, ..Default::default() };
        let err = read_disc(&mut disc, &dir.join("a.iso"), &opts, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(matches!(err, ReadError::Unreadable { lba: 21, .. }), "{err}");
        assert!(!dir.join("a.iso").exists() && !dir.join("a.iso.part").exists(), "nothing left behind");

        let mut disc = FakeDisc::pattern(40);
        disc.fail.insert(21, u32::MAX);
        disc.fail.insert(22, u32::MAX);
        let mut expect = disc.data.clone();
        expect[21 * S..23 * S].fill(0);
        let opts = ReadOptions { ignore_errors: true, ..opts };
        let r = read_disc(&mut disc, &dir.join("b.iso"), &opts, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(r.bad, [21, 22]);
        assert_eq!(std::fs::read(dir.join("b.iso")).unwrap(), expect, "the rest is intact, bad sectors are zero");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancel_removes_partial_output() {
        let dir = temp("cancel");
        let cancel = AtomicBool::new(false);
        let mut disc = FakeDisc::pattern(100);
        let opts = ReadOptions { chunk_sectors: 4, ..Default::default() };
        let err = read_disc(&mut disc, &dir.join("a.iso"), &opts, &cancel, &mut |p| {
            if p.lba >= 40 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap_err();
        assert!(matches!(err, ReadError::Cancelled));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn split_parts_concatenate_to_the_image() {
        let dir = temp("split");
        // 3 sectors per part, 8 sectors → 3 parts.
        let data = FakeDisc::pattern(8).data;
        let mut w = SplitWriter::new(&dir.join("a.iso"), Split::Gb2, 8 * SECTOR);
        w.limit = Some(3 * SECTOR);
        w.total_parts = 3;
        w.write_all(&data[..5 * S + 100]).unwrap();
        w.write_all(&data[5 * S + 100..]).unwrap();
        let files = w.commit().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].file_name().unwrap(), "a_part2.iso");
        let mut joined = Vec::new();
        for f in &files {
            let b = std::fs::read(f).unwrap();
            assert!(b.len() <= 3 * S);
            joined.extend(b);
        }
        assert_eq!(joined, data);
        // One part only: the plain name, nothing left over; abort deletes everything.
        let mut w = SplitWriter::new(&dir.join("b.iso"), Split::Gb2, 8 * SECTOR);
        w.write_all(&data).unwrap();
        assert_eq!(w.commit().unwrap(), [dir.join("b.iso")]);
        let mut w = SplitWriter::new(&dir.join("c.iso"), Split::None, 0);
        w.write_all(&[1]).unwrap();
        w.abort();
        assert!(!dir.join("c.iso").exists() && !dir.join("c.iso.part").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn layout_files_follow_the_option() {
        let dir = temp("layout");
        let img = dir.join("film.iso");
        assert_eq!(write_layout(&img, "auto", 100, None).unwrap(), None);
        assert_eq!(write_layout(&img, "none", 100, Some(50)).unwrap(), None);
        assert_eq!(write_layout(&img, "dvd", 100, None).unwrap(), None, ".dvd needs a layer break");
        let dvd = write_layout(&img, "auto", 100, Some(64)).unwrap().unwrap();
        assert_eq!(dvd.file_name().unwrap(), "film.dvd");
        assert_eq!(std::fs::read_to_string(&dvd).unwrap(), "LayerBreak=64\r\nfilm.iso\r\n");
        let mds = write_layout(&img, "mds", 100, Some(64)).unwrap().unwrap();
        let parsed = crate::burner::sidecar::parse_mds(&std::fs::read(mds).unwrap()).unwrap();
        assert_eq!((parsed.sectors, parsed.layer_break, parsed.image.as_str()), (100, Some(64), "film.iso"));
        let cue = write_layout(&img, "cue", 100, None).unwrap().unwrap();
        assert!(std::fs::read_to_string(cue).unwrap().contains("MODE1/2048"));
        assert_eq!(Split::parse("4gb").limit(), Some((4 << 30) - 2048));
        assert_eq!(Split::parse("x"), Split::None);
        std::fs::remove_dir_all(dir).ok();
    }
}
