//! Disc ↔ image comparison and plain readability checks, with the miscompares mapped back to
//! the files they fall into.

use std::io;
use std::sync::atomic::AtomicBool;

use md5::{Digest, Md5};

use super::isoread::path_at;
use super::reader::{read_resilient, ReadError};
use super::source::SectorSource;
use super::udf::SECTOR;

const S: usize = SECTOR as usize;
/// The report lists at most this many miscompared sectors (the count is exact).
pub const MAX_LISTED: usize = 1000;

#[derive(Debug, Clone)]
pub struct VerifyOptions {
    pub sw_retries: u32,
    /// Report miscompares but still count the verification as passed.
    pub ignore_miscompares: bool,
    /// MD5 of the data read from the disc.
    pub md5: bool,
    pub chunk_sectors: u32,
    /// Stop after the first chunk with a miscompare or an unreadable sector.
    pub stop_at_first: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        VerifyOptions { sw_retries: 20, ignore_miscompares: false, md5: false, chunk_sectors: 32, stop_at_first: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerifyReport {
    pub sectors: u64,
    /// First [`MAX_LISTED`] sectors whose content differs.
    pub miscompares: Vec<u64>,
    pub miscompare_count: u64,
    /// Sectors the disc could not deliver.
    pub unreadable: Vec<u64>,
    pub md5: Option<String>,
    pub passed: bool,
}

#[derive(Debug)]
pub enum VerifyError {
    /// The image (the reference) could not be read.
    Image(io::Error),
    Cancelled,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Image(e) => write!(f, "Image nicht lesbar: {e}"),
            VerifyError::Cancelled => write!(f, "abgebrochen"),
        }
    }
}

impl std::error::Error for VerifyError {}

fn finish(mut report: VerifyReport, opts: &VerifyOptions, hasher: Option<Md5>) -> VerifyReport {
    report.md5 = hasher.map(|h| format!("{:x}", h.finalize()));
    report.passed = report.unreadable.is_empty() && (opts.ignore_miscompares || report.miscompare_count == 0);
    report
}

/// Reads the whole `disc` and compares it sector by sector with `image` (the first
/// `image.sectors()` sectors). `progress` gets the fraction done.
pub fn verify(
    disc: &mut dyn SectorSource,
    image: &mut dyn SectorSource,
    opts: &VerifyOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<VerifyReport, VerifyError> {
    let total = image.sectors();
    let mut report = VerifyReport { sectors: total, ..Default::default() };
    let mut hasher = opts.md5.then(Md5::new);
    let chunk = opts.chunk_sectors.max(1) as u64;
    let (mut a, mut b) = (vec![0u8; chunk as usize * S], vec![0u8; chunk as usize * S]);
    let mut lba = 0;
    while lba < total {
        let n = chunk.min(total - lba) as usize;
        image.read(lba, &mut a[..n * S]).map_err(VerifyError::Image)?;
        let mut bad = Vec::new();
        match read_resilient(disc, lba, &mut b[..n * S], opts.sw_retries, true, &mut bad, cancel) {
            Ok(()) => {}
            Err(ReadError::Cancelled) => return Err(VerifyError::Cancelled),
            Err(e) => return Err(VerifyError::Image(io::Error::other(e.to_string()))),
        }
        if let Some(h) = hasher.as_mut() {
            h.update(&b[..n * S]);
        }
        for i in 0..n {
            let at = lba + i as u64;
            if bad.contains(&at) {
                continue;
            }
            if a[i * S..(i + 1) * S] != b[i * S..(i + 1) * S] {
                report.miscompare_count += 1;
                if report.miscompares.len() < MAX_LISTED {
                    report.miscompares.push(at);
                }
            }
        }
        report.unreadable.extend(bad);
        lba += n as u64;
        progress(lba as f64 / total as f64);
        if opts.stop_at_first && (report.miscompare_count > 0 || !report.unreadable.is_empty()) {
            break;
        }
    }
    Ok(finish(report, opts, hasher))
}

/// Reads the whole `disc` without a reference ("verify readability").
pub fn read_check(disc: &mut dyn SectorSource, opts: &VerifyOptions, cancel: &AtomicBool, progress: &mut dyn FnMut(f64)) -> Result<VerifyReport, VerifyError> {
    let total = disc.sectors();
    let mut report = VerifyReport { sectors: total, ..Default::default() };
    let mut hasher = opts.md5.then(Md5::new);
    let chunk = opts.chunk_sectors.max(1) as u64;
    let mut buf = vec![0u8; chunk as usize * S];
    let mut lba = 0;
    while lba < total {
        let n = chunk.min(total - lba) as usize;
        match read_resilient(disc, lba, &mut buf[..n * S], opts.sw_retries, true, &mut report.unreadable, cancel) {
            Ok(()) => {}
            Err(ReadError::Cancelled) => return Err(VerifyError::Cancelled),
            Err(e) => return Err(VerifyError::Image(io::Error::other(e.to_string()))),
        }
        if let Some(h) = hasher.as_mut() {
            h.update(&buf[..n * S]);
        }
        lba += n as u64;
        progress(lba as f64 / total as f64);
    }
    Ok(finish(report, opts, hasher))
}

/// Pairs sectors with the file they belong to, from [`super::isoread::IsoImage::extent_map`].
pub fn files_of<'a>(lbas: &[u64], map: &'a [(u64, u64, String)]) -> Vec<(u64, Option<&'a str>)> {
    lbas.iter().map(|&l| (l, path_at(map, l))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burner::source::fake::FakeDisc;

    fn run(disc: &mut FakeDisc, image: &mut FakeDisc, opts: &VerifyOptions) -> VerifyReport {
        verify(disc, image, opts, &AtomicBool::new(false), &mut |_| {}).unwrap()
    }

    #[test]
    fn identical_discs_pass() {
        let (mut disc, mut image) = (FakeDisc::pattern(100), FakeDisc::pattern(100));
        let opts = VerifyOptions { md5: true, chunk_sectors: 9, ..Default::default() };
        let r = run(&mut disc, &mut image, &opts);
        assert!(r.passed && r.miscompare_count == 0 && r.unreadable.is_empty());
        assert_eq!(r.md5.unwrap(), format!("{:x}", Md5::digest(&disc.data)));
    }

    #[test]
    fn miscompares_and_unreadable_sectors_are_found() {
        let mut image = FakeDisc::pattern(100);
        let mut disc = FakeDisc::pattern(100);
        disc.corrupt.extend([5, 6, 77]);
        disc.fail.insert(40, u32::MAX);
        let opts = VerifyOptions { sw_retries: 1, chunk_sectors: 16, ..Default::default() };
        let r = run(&mut disc, &mut image, &opts);
        assert_eq!(r.miscompares, [5, 6, 77]);
        assert_eq!(r.miscompare_count, 3);
        assert_eq!(r.unreadable, [40]);
        assert!(!r.passed);
        // Ignoring miscompares does not hide unreadable sectors.
        disc.fail.clear();
        let ignore = VerifyOptions { ignore_miscompares: true, ..opts };
        assert!(run(&mut disc, &mut image, &ignore).passed);
        disc.fail.insert(40, u32::MAX);
        assert!(!run(&mut disc, &mut image, &ignore).passed);
    }

    #[test]
    fn stop_at_first_ends_after_the_first_bad_chunk() {
        let (mut disc, mut image) = (FakeDisc::pattern(100), FakeDisc::pattern(100));
        disc.corrupt.extend([5, 90]);
        let opts = VerifyOptions { chunk_sectors: 10, stop_at_first: true, ..Default::default() };
        let r = run(&mut disc, &mut image, &opts);
        assert_eq!(r.miscompares, [5], "sector 90 is never looked at");
        assert!(!r.passed);
    }

    #[test]
    fn flaky_sectors_are_retried() {
        let (mut disc, mut image) = (FakeDisc::pattern(30), FakeDisc::pattern(30));
        disc.fail.insert(12, 2);
        let r = run(&mut disc, &mut image, &VerifyOptions { sw_retries: 3, ..Default::default() });
        assert!(r.passed);
    }

    #[test]
    fn listing_is_capped_but_the_count_is_exact() {
        let (mut disc, mut image) = (FakeDisc::pattern(1500), FakeDisc::pattern(1500));
        disc.corrupt.extend(0..1500);
        let r = run(&mut disc, &mut image, &VerifyOptions::default());
        assert_eq!((r.miscompares.len(), r.miscompare_count), (MAX_LISTED, 1500));
    }

    #[test]
    fn cancel_and_image_errors() {
        let cancel = AtomicBool::new(true);
        let (mut disc, mut image) = (FakeDisc::pattern(10), FakeDisc::pattern(10));
        assert!(matches!(verify(&mut disc, &mut image, &VerifyOptions::default(), &cancel, &mut |_| {}), Err(VerifyError::Cancelled)));
        let mut image = FakeDisc::pattern(10);
        image.fail.insert(3, u32::MAX);
        assert!(matches!(
            verify(&mut disc, &mut image, &VerifyOptions::default(), &AtomicBool::new(false), &mut |_| {}),
            Err(VerifyError::Image(_))
        ));
    }

    #[test]
    fn readability_check_and_file_mapping() {
        let mut disc = FakeDisc::pattern(50);
        disc.fail.insert(7, u32::MAX);
        let r = read_check(&mut disc, &VerifyOptions { sw_retries: 0, ..Default::default() }, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!((r.unreadable.as_slice(), r.passed), (&[7u64][..], false));
        let map = vec![(0, 5, "A/X.BIN".to_string()), (10, 20, "B.BIN".to_string())];
        assert_eq!(files_of(&[3, 7, 10, 19, 20], &map), [(3, Some("A/X.BIN")), (7, None), (10, Some("B.BIN")), (19, Some("B.BIN")), (20, None)]);
    }
}
