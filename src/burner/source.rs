//! Where the sectors of a burn, read or verify come from: an image file, a device, zeros
//! (Discovery test burn), a 2352-byte-per-sector `.bin`, or an image that is built on the fly.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::JoinHandle;

use super::ringbuf::{ring, RingReader, RingStats};
use super::MediaInfo;
use super::udf::SECTOR;

const S: usize = SECTOR as usize;

/// Random access to 2048-byte sectors. `buf` is a multiple of 2048 bytes; the read starts at
/// `lba` and either fills `buf` or fails.
pub trait SectorSource: Send {
    fn sectors(&self) -> u64;
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Can be read again from the start (needed to verify a burn against the source).
    fn rewindable(&self) -> bool {
        true
    }

    /// The ring buffer inside the source, if it has one (its fill level is the main buffer).
    fn ring_stats(&self) -> Option<RingStats> {
        None
    }
}

impl SectorSource for Box<dyn SectorSource> {
    fn sectors(&self) -> u64 {
        (**self).sectors()
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read(lba, buf)
    }

    fn rewindable(&self) -> bool {
        (**self).rewindable()
    }

    fn ring_stats(&self) -> Option<RingStats> {
        (**self).ring_stats()
    }
}

/// A source as a sequential byte stream (what a burner pulls), read in chunks of 128 sectors.
pub struct SourceReader {
    src: Box<dyn SectorSource>,
    next: u64,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
}

impl SourceReader {
    pub fn new(src: Box<dyn SectorSource>) -> Self {
        SourceReader { src, next: 0, buf: vec![0; 128 * S], pos: 0, len: 0 }
    }

    /// Gives the source back (to verify against it).
    pub fn into_source(self) -> Box<dyn SectorSource> {
        self.src
    }
}

impl Read for SourceReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.len {
            let n = (self.src.sectors().saturating_sub(self.next)).min(128) as usize;
            if n == 0 {
                return Ok(0);
            }
            self.src.read(self.next, &mut self.buf[..n * S])?;
            (self.pos, self.len, self.next) = (0, n * S, self.next + n as u64);
        }
        let n = out.len().min(self.len - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn check(buf: &[u8]) {
    assert!(buf.len() % S == 0, "sector reads are multiples of {S} bytes");
}

/// An image file (or a raw device opened as a file). A last partial sector reads zero-padded.
pub struct FileSource {
    file: File,
    sectors: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let sectors = file.metadata()?.len().div_ceil(SECTOR);
        Ok(FileSource { file, sectors })
    }

    /// A device or file whose size the OS cannot tell (`sectors` from READ CAPACITY).
    pub fn with_sectors(file: File, sectors: u64) -> Self {
        FileSource { file, sectors }
    }
}

impl SectorSource for FileSource {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        check(buf);
        self.file.seek(SeekFrom::Start(lba * SECTOR))?;
        let mut got = 0;
        while got < buf.len() {
            let n = self.file.read(&mut buf[got..])?;
            if n == 0 {
                if lba * SECTOR + got as u64 >= self.sectors * SECTOR {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "hinter dem Ende des Images"));
                }
                buf[got..].fill(0); // image ends inside the last sector
                break;
            }
            got += n;
        }
        Ok(())
    }
}

/// `n` sectors of zeros: the Discovery test burn.
#[derive(Debug)]
pub struct ZeroSource(pub u64);

impl SectorSource for ZeroSource {
    fn sectors(&self) -> u64 {
        self.0
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        check(buf);
        if lba + (buf.len() / S) as u64 > self.0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "hinter dem Ende"));
        }
        buf.fill(0);
        Ok(())
    }
}

/// Discovery test burn: zeros for `requested` sectors, or for the whole free capacity of the
/// medium. A blank, writable disc is required.
pub fn discovery(media: &MediaInfo, requested: Option<u64>) -> Result<ZeroSource, String> {
    if !media.present {
        return Err("no disc in the drive".into());
    }
    if !media.writable || !media.blank {
        return Err("The Discovery test needs a blank, writable disc".into());
    }
    let sectors = requested.unwrap_or(media.free_sectors);
    if sectors == 0 || sectors > media.free_sectors {
        return Err(format!("{sectors} sectors do not fit on the disc ({} free)", media.free_sectors));
    }
    Ok(ZeroSource(sectors))
}

/// Raw 2352-byte sectors (`MODE1/2352` `.bin`): the 16-byte sync/header and the 288 bytes of
/// EDC/ECC are dropped, leaving the 2048 user data bytes.
pub struct Bin2352Source {
    file: File,
    sectors: u64,
}

const RAW: u64 = 2352;
const RAW_HEADER: u64 = 16;

impl Bin2352Source {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len % RAW != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Size is not a multiple of 2352 bytes"));
        }
        Ok(Bin2352Source { file, sectors: len / RAW })
    }
}

impl SectorSource for Bin2352Source {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        check(buf);
        let mut raw = [0u8; RAW as usize];
        for (i, out) in buf.chunks_exact_mut(S).enumerate() {
            let at = lba + i as u64;
            if at >= self.sectors {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "hinter dem Ende"));
            }
            self.file.seek(SeekFrom::Start(at * RAW))?;
            self.file.read_exact(&mut raw)?;
            out.copy_from_slice(&raw[RAW_HEADER as usize..RAW_HEADER as usize + S]);
        }
        Ok(())
    }
}

/// Sequential `Read` (a device handle, a pipe) as a source; only reads that continue where the
/// last one ended work.
pub struct ReadSource<R: Read + Send> {
    r: R,
    next: u64,
    sectors: u64,
}

impl<R: Read + Send> ReadSource<R> {
    pub fn new(r: R, sectors: u64) -> Self {
        ReadSource { r, next: 0, sectors }
    }
}

impl<R: Read + Send> SectorSource for ReadSource<R> {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn rewindable(&self) -> bool {
        false
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        check(buf);
        if lba != self.next {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "only sequential reading is possible"));
        }
        self.r.read_exact(buf)?;
        self.next += (buf.len() / S) as u64;
        Ok(())
    }
}

/// A source pumped through a ring buffer by its own thread (for a burner that pulls bytes). The
/// thread returns the source when it has delivered everything, so it can be verified against
/// afterwards; when the source fails it cancels the ring, so the consumer cannot take the short
/// stream for a complete one.
pub struct RingFeed {
    pub reader: RingReader,
    pub stats: RingStats,
    pub producer: JoinHandle<Result<Box<dyn SectorSource>, String>>,
}

pub fn spawn_ring_feed(source: Box<dyn SectorSource>, ring_bytes: usize, low_pct: u32, high_pct: u32) -> RingFeed {
    let (mut writer, reader, stats) = ring(ring_bytes, low_pct, high_pct);
    let on_error = stats.clone();
    let producer = std::thread::spawn(move || {
        let mut src = SourceReader::new(source);
        let mut chunk = vec![0u8; 1 << 20];
        let result = loop {
            match src.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    match writer.write_all(&chunk[..n]) {
                        Ok(()) => {}
                        // The consumer cancelled or dropped the ring (its own failure or the user): not this source's error.
                        Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe) => break Ok(()),
                        Err(e) => break Err(e.to_string()),
                    }
                }
                Err(e) => break Err(format!("Source unreadable: {e}")),
            }
        };
        match result {
            Ok(()) => Ok(src.into_source()),
            Err(e) => {
                on_error.cancel();
                Err(e)
            }
        }
    });
    RingFeed { reader, stats, producer }
}

/// An image produced by a thread while it is being consumed (build and burn without a temporary
/// `.iso`). Reads must be sequential; the ring buffer between the two shows as the main buffer.
pub struct StreamSource {
    reader: Option<RingReader>,
    stats: RingStats,
    next: u64,
    sectors: u64,
    worker: Option<JoinHandle<Result<(), String>>>,
}

impl StreamSource {
    /// Runs `produce` on a thread; it writes exactly `sectors` sectors into the sink.
    pub fn spawn<F>(sectors: u64, ring_bytes: usize, low_pct: u32, high_pct: u32, produce: F) -> Self
    where
        F: FnOnce(&mut dyn std::io::Write) -> Result<(), String> + Send + 'static,
    {
        let (mut writer, reader, stats) = ring(ring_bytes, low_pct, high_pct);
        let worker = std::thread::spawn(move || produce(&mut writer));
        StreamSource { reader: Some(reader), stats, next: 0, sectors, worker: Some(worker) }
    }

    pub fn stats(&self) -> RingStats {
        self.stats.clone()
    }

    /// Waits for the producer and returns its result (call after the last read).
    pub fn finish(mut self) -> Result<(), String> {
        let worker = self.worker.take().expect("finish is called once");
        self.reader = None; // a producer that is not done fails instead of blocking

        worker.join().unwrap_or_else(|_| Err("Cancelled in the image builder".into()))
    }
}

impl SectorSource for StreamSource {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn rewindable(&self) -> bool {
        false
    }

    fn ring_stats(&self) -> Option<RingStats> {
        Some(self.stats.clone())
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        check(buf);
        if lba != self.next {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "only sequential reading is possible"));
        }
        self.reader.as_mut().expect("reader is present until finish").read_exact(buf)?;
        self.next += (buf.len() / S) as u64;
        Ok(())
    }
}

impl Drop for StreamSource {
    fn drop(&mut self) {
        self.stats.cancel(); // unblock a producer whose consumer gave up
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// In-memory source whose reads can fail (a number of times, or always) or return damaged data.
    pub struct FakeDisc {
        pub data: Vec<u8>,
        /// lba → remaining failures (`u32::MAX` = always).
        pub fail: HashMap<u64, u32>,
        pub corrupt: HashSet<u64>,
        pub reads: u64,
    }

    impl FakeDisc {
        pub fn new(data: Vec<u8>) -> Self {
            FakeDisc { data, fail: HashMap::new(), corrupt: HashSet::new(), reads: 0 }
        }

        /// `n` sectors, sector `i` filled with `i % 251 + 1`.
        pub fn pattern(n: u64) -> Self {
            Self::new((0..n).flat_map(|i| vec![(i % 251) as u8 + 1; S]).collect())
        }
    }

    impl SectorSource for FakeDisc {
        fn sectors(&self) -> u64 {
            (self.data.len() / S) as u64
        }

        fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
            self.reads += 1;
            for i in 0..(buf.len() / S) as u64 {
                if let Some(left) = self.fail.get_mut(&(lba + i)) {
                    if *left > 0 {
                        if *left != u32::MAX {
                            *left -= 1;
                        }
                        return Err(io::Error::other(format!("Read error at sector {}", lba + i)));
                    }
                }
            }
            let at = lba as usize * S;
            let src = self.data.get(at..at + buf.len()).ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Ende"))?;
            buf.copy_from_slice(src);
            for (i, sector) in buf.chunks_exact_mut(S).enumerate() {
                if self.corrupt.contains(&(lba + i as u64)) {
                    sector[7] ^= 0xFF;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeDisc;
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cx_source_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn source_reader_streams_every_byte_in_order() {
        let disc = FakeDisc::pattern(300); // more than two chunks of 128 sectors
        let expect = disc.data.clone();
        let mut r = SourceReader::new(Box::new(disc));
        let mut got = Vec::new();
        let mut small = [0u8; 1000]; // odd read size across sector and chunk borders
        loop {
            let n = r.read(&mut small).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&small[..n]);
        }
        assert_eq!(got, expect);
        assert_eq!(r.into_source().sectors(), 300);
    }

    #[test]
    fn ring_feed_delivers_and_hands_the_source_back() {
        let disc = FakeDisc::pattern(40);
        let expect = disc.data.clone();
        let mut feed = spawn_ring_feed(Box::new(disc), 64 * 1024, 0, 100);
        let mut got = Vec::new();
        feed.reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, expect);
        let src = feed.producer.join().unwrap().unwrap();
        assert_eq!(src.sectors(), 40);
        assert!(src.rewindable());
    }

    #[test]
    fn ring_feed_cancels_the_ring_when_the_source_fails() {
        let mut disc = FakeDisc::pattern(300);
        disc.fail.insert(200, u32::MAX);
        let mut feed = spawn_ring_feed(Box::new(disc), 1 << 20, 0, 100);
        // (`read_to_end` would retry on Interrupted, so the abort is ConnectionAborted.)
        let mut buf = vec![0u8; 4096];
        let e = loop {
            match feed.reader.read(&mut buf) {
                Ok(0) => panic!("ended as if the image was complete"),
                Ok(_) => {}
                Err(e) => break e,
            }
        };
        assert_eq!(e.kind(), io::ErrorKind::ConnectionAborted, "the consumer sees an abort, not a short end");
        assert!(feed.producer.join().unwrap().err().unwrap().starts_with("Source unreadable"));
    }

    #[test]
    fn source_reader_passes_read_errors_on() {
        let mut disc = FakeDisc::pattern(10);
        disc.fail.insert(5, u32::MAX);
        let mut r = SourceReader::new(Box::new(disc));
        assert!(std::io::copy(&mut r, &mut std::io::sink()).is_err());
    }

    #[test]
    fn sequential_sources_are_not_rewindable() {
        assert!(ZeroSource(1).rewindable() && ZeroSource(1).ring_stats().is_none());
        assert!(!ReadSource::new(&b""[..], 0).rewindable());
        let s = StreamSource::spawn(1, 4096, 0, 100, |w| w.write_all(&[0; 2048]).map_err(|e| e.to_string()));
        assert!(!s.rewindable() && s.ring_stats().is_some());
    }

    #[test]
    fn file_source_reads_sectors_and_pads_the_tail() {
        let dir = temp("file");
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        std::fs::write(dir.join("a.iso"), &data).unwrap();
        let mut src = FileSource::open(&dir.join("a.iso")).unwrap();
        assert_eq!(src.sectors(), 3);
        let mut buf = vec![0u8; 2 * S];
        src.read(1, &mut buf).unwrap();
        assert_eq!(&buf[..S], &data[S..2 * S]);
        assert_eq!(&buf[S..S + 904], &data[2 * S..]);
        assert!(buf[S + 904..].iter().all(|&b| b == 0), "tail is zero-padded");
        assert!(src.read(3, &mut buf[..S]).is_err(), "past the end");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn zero_source_is_zero_and_bounded() {
        let mut z = ZeroSource(4);
        let mut buf = vec![1u8; 2 * S];
        z.read(2, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        assert!(z.read(3, &mut buf).is_err());
    }

    #[test]
    fn discovery_needs_blank_writable_media_and_fits() {
        let blank = MediaInfo { present: true, writable: true, blank: true, free_sectors: 2_295_104, total_sectors: 2_295_104, ..MediaInfo::none() };
        assert_eq!(discovery(&blank, None).unwrap().sectors(), 2_295_104);
        assert_eq!(discovery(&blank, Some(1000)).unwrap().sectors(), 1000);
        assert!(discovery(&blank, Some(2_295_105)).is_err());
        assert!(discovery(&blank, Some(0)).is_err());
        assert!(discovery(&MediaInfo::none(), None).unwrap_err().contains("no disc"));
        let used = MediaInfo { blank: false, ..blank };
        assert!(discovery(&used, None).unwrap_err().contains("blank"));
    }

    #[test]
    fn bin2352_drops_header_and_ecc() {
        let dir = temp("bin");
        let mut raw = Vec::new();
        for i in 0..3u8 {
            raw.extend([0xAA; 16]);
            raw.extend(vec![i + 1; S]);
            raw.extend([0xEE; 288]);
        }
        std::fs::write(dir.join("a.bin"), &raw).unwrap();
        let mut src = Bin2352Source::open(&dir.join("a.bin")).unwrap();
        assert_eq!(src.sectors(), 3);
        let mut buf = vec![0u8; 2 * S];
        src.read(1, &mut buf).unwrap();
        assert!(buf[..S].iter().all(|&b| b == 2) && buf[S..].iter().all(|&b| b == 3));
        std::fs::write(dir.join("bad.bin"), vec![0u8; 2352 + 5]).unwrap();
        assert!(Bin2352Source::open(&dir.join("bad.bin")).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sequential_sources_refuse_seeking() {
        let data: Vec<u8> = (0..3 * S).map(|i| (i / S) as u8).collect();
        let mut src = ReadSource::new(&data[..], 3);
        let mut buf = vec![0u8; S];
        src.read(0, &mut buf).unwrap();
        assert!(src.read(2, &mut buf).is_err());
        src.read(1, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn stream_source_delivers_what_the_producer_writes() {
        let mut src = StreamSource::spawn(50, 64 * 1024, 0, 100, |sink| {
            for i in 0..50u8 {
                sink.write_all(&vec![i; S]).map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        let mut buf = vec![0u8; 5 * S];
        for chunk in 0..10u64 {
            src.read(chunk * 5, &mut buf).unwrap();
            assert_eq!(buf[0] as u64, chunk * 5);
            assert_eq!(buf[5 * S - 1] as u64, chunk * 5 + 4);
        }
        assert!(src.read(5, &mut buf).is_err(), "no seeking");
        src.finish().unwrap();
    }

    #[test]
    fn stream_source_reports_producer_errors() {
        let mut src = StreamSource::spawn(10, 4096, 0, 100, |sink| {
            sink.write_all(&[1u8; S]).map_err(|e| e.to_string())?;
            Err("Source disappeared".to_string())
        });
        let mut buf = vec![0u8; S];
        src.read(0, &mut buf).unwrap();
        assert!(src.read(1, &mut buf).is_err(), "stream ends early");
        assert_eq!(src.finish().unwrap_err(), "Source disappeared");
    }

    #[test]
    fn fake_disc_fails_and_corrupts_on_demand() {
        let mut d = FakeDisc::pattern(4);
        d.fail.insert(2, 1);
        d.corrupt.insert(1);
        let mut buf = vec![0u8; S];
        assert!(d.read(2, &mut buf).is_err());
        d.read(2, &mut buf).unwrap();
        d.read(1, &mut buf).unwrap();
        assert_ne!(buf[7], 2);
    }
}
