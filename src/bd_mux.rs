//! Origin: derived from tsMuxer 2.7.0 (Apache-2.0, justdan96/tsMuxer); see THIRD_PARTY_NOTICES.md.
//! Native BDAV muxer: H.264 + audio + PGS/IGS of one title or menu into an `.m2ts` with its clip
//! and playlist files. Replaces tsMuxeR 2.7.0 (Apache-2.0; derived from its scheduling, see
//! STATUS.md): access units are written in DTS order, with a PCR packet (PID 0x1001) every few
//! thousand ticks of 90 kHz, PAT/PMT alongside and a SIT about once a second; the 27 MHz arrival
//! time stamps (ATS) are interpolated between PCR packets. The PCR starts 500 ms before the first
//! picture's PTS (tsMuxeR sent a PCR every 7000 ticks, here it is at least every 3000).
//!
//! ffmpeg is the demuxer: it copies the title's `.mkv` into an MPEG-TS pipe (one PES per picture
//! or audio frame, LPCM as `pcm_bluray`), this module re-packetises that. Video timestamps are
//! regenerated on the exact frame grid (ffmpeg's come from millisecond Matroska stamps).
//!
//! shortcut: the video NAL units pass through as ffmpeg's `h264_mp4toannexb` writes them (SPS/PPS
//! before every IDR); tsMuxeR additionally rewrote the SPS VUI and the HRD SEI. x264 and NVENC with
//! the BD options already emit both SEI. T-STD buffer behaviour is not simulated.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::bd_clip::{self, Clip, Codec, EpEntry, Playlist, Stream};
use crate::tools::{ToolError, Toolchain};

/// PTS of the first picture (600 s, like tsMuxeR): leaves room for the PCR and earlier DTS.
const BASE_PTS: i64 = 54_000_000;
/// A PCR packet at least every 3000 ticks (33 ms; at every picture in practice): tsMuxeR's 7000
/// left up to 105 ms between two PCRs at 23.976 fps, the standard allows 100 ms.
const PCR_DELTA: i64 = 3000;
/// PAT / PMT with a PCR at least every 5000 ticks, so never more than 100 ms apart.
const PSI_DELTA: i64 = 5000;
const SIT_INTERVAL: i64 = 76_900;
const VBV_LEN: i64 = 45_000;
/// Files are padded to 32 packets (6144 bytes).
const ALIGN_PACKETS: usize = 32;
const ATS_MASK: u64 = 0x3FFF_FFFF;

/// Service information table with the AVC peak-rate descriptor, as tsMuxeR writes it.
const SIT: [u8; 28] = [
    0x7F, 0xF0, 0x19, 0xFF, 0xFF, 0xC1, 0x00, 0x00, 0xF0, 0x0A, 0x63, 0x08, 0xC1, 0xD4, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01, 0x80, 0x00, 0x03, 0x00,
    0x38, 0x6D,
];

/// CRC-32/MPEG-2 (PSI sections).
pub fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { crc << 1 ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

// ---------------------------------------------------------------------------------------------
// Access units
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video { idr: bool },
    /// AC-3 / DTS: private stream 1 with the PES extension 0x71.
    CoreAudio,
    /// LPCM and subtitle segments: private stream 1.
    Private,
}

/// One PES worth of data with its (90 kHz) time stamps.
#[derive(Debug, Clone)]
pub struct Au {
    pid: u16,
    kind: Kind,
    pts: i64,
    dts: i64,
    /// Duration in 90 kHz ticks (0 for subtitle segments).
    dur: i64,
    payload: Vec<u8>,
}

fn pes_bytes(au: &Au) -> Vec<u8> {
    let (sid, ext) = match au.kind {
        Kind::Video { .. } => (0xE0, false),
        Kind::CoreAudio => (0xFD, true),
        Kind::Private => (0xBD, false),
    };
    let with_dts = au.dts != au.pts;
    let stamp = |marker: u8, t: i64| {
        let t = t as u64 & 0x1_FFFF_FFFF;
        [marker | (t >> 29) as u8 & 0x0E | 1, (t >> 22) as u8, (t >> 14) as u8 & 0xFE | 1, (t >> 7) as u8, (t << 1) as u8 | 1]
    };
    let mut hdr_data = Vec::new();
    hdr_data.extend(stamp(if with_dts { 0x31 } else { 0x21 }, au.pts));
    if with_dts {
        hdr_data.extend(stamp(0x11, au.dts));
    }
    if ext {
        hdr_data.extend([0x01, 0x81, 0x71]);
    }
    let total = 3 + hdr_data.len() + au.payload.len();
    let mut v = vec![0, 0, 1, sid];
    v.extend((if total <= 0xFFFF { total as u16 } else { 0 }).to_be_bytes());
    v.extend([0x84, (if with_dts { 0xC0 } else { 0x80 }) | u8::from(ext), hdr_data.len() as u8]);
    v.extend(hdr_data);
    v.extend(&au.payload);
    v
}

// ---------------------------------------------------------------------------------------------
// Packetiser
// ---------------------------------------------------------------------------------------------

struct Packetizer<W: Write> {
    out: W,
    streams: Vec<Stream>,
    cc: HashMap<u16, u8>,
    /// Packets since the last PCR packet, still without arrival time.
    pending: Vec<[u8; bd_clip::PACKET]>,
    written: u32,
    first: Option<(i64, i64)>, // (min DTS, fixed PCR offset)
    last_pcr: Option<i64>,
    last_pmt: i64,
    last_sit: i64,
    prev27: i64,
    end_dts: i64,
    ep: Vec<EpEntry>,
}

impl<W: Write> Packetizer<W> {
    fn new(out: W, streams: Vec<Stream>) -> Self {
        Packetizer { out, streams, cc: HashMap::new(), pending: Vec::new(), written: 0, first: None, last_pcr: None, last_pmt: 0, last_sit: 0, prev27: 0, end_dts: 0, ep: Vec::new() }
    }

    fn count(&self) -> u32 {
        self.written + self.pending.len() as u32
    }

    fn push(&mut self, ts: [u8; 188]) {
        let mut p = [0u8; bd_clip::PACKET];
        p[4..].copy_from_slice(&ts);
        self.pending.push(p);
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let c = self.cc.entry(pid).or_insert(0);
        let v = *c;
        *c = (v + 1) & 15;
        v
    }

    /// A PSI section in one packet.
    fn section(&mut self, pid: u16, body: &[u8]) {
        let mut ts = [0xFFu8; 188];
        ts[..4].copy_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10 | self.next_cc(pid)]);
        ts[4] = 0;
        ts[5..5 + body.len()].copy_from_slice(body);
        self.push(ts);
    }

    fn pat(&mut self) {
        let mut s = vec![0x00, 0xB0, 0x11, 0x00, 0x00, 0xC1, 0x00, 0x00, 0x00, 0x00, 0xE0, 0x1F, 0x00, 0x01, 0xE1, 0x00];
        s.extend(crc32_mpeg(&s).to_be_bytes());
        self.section(0, &s);
    }

    fn pmt(&mut self) {
        let mut es = Vec::new();
        for s in &self.streams {
            es.push(s.codec.coding_type());
            es.extend((0xE000 | s.pid).to_be_bytes());
            es.extend((0xF000 | s.es_info.len() as u16).to_be_bytes());
            es.extend(&s.es_info);
        }
        const PROGRAM_INFO: [u8; 12] = [0x05, 0x04, b'H', b'D', b'M', b'V', 0x88, 0x04, 0x0F, 0xFF, 0xFC, 0xFC];
        let len = 9 + PROGRAM_INFO.len() + es.len() + 4;
        let mut s = vec![0x02, 0xB0 | (len >> 8) as u8, len as u8, 0x00, 0x01, 0xC1, 0x00, 0x00];
        s.extend((0xE000 | bd_clip::PCR_PID).to_be_bytes());
        s.extend((0xF000 | PROGRAM_INFO.len() as u16).to_be_bytes());
        s.extend(PROGRAM_INFO);
        s.extend(es);
        s.extend(crc32_mpeg(&s).to_be_bytes());
        self.section(bd_clip::PMT_PID, &s);
    }

    fn pat_pmt(&mut self, pcr: i64, force: bool) {
        if force || pcr - self.last_pmt >= PSI_DELTA {
            self.last_pmt = pcr;
            self.pat();
            self.pmt();
            if force || pcr - self.last_sit > SIT_INTERVAL {
                self.last_sit = pcr;
                self.section(0x1F, &SIT);
            }
        }
    }

    /// PCR packet; gives every packet since the previous PCR its arrival time.
    fn pcr(&mut self, pcr: i64) -> io::Result<()> {
        let pid = bd_clip::PCR_PID;
        let mut ts = [0xFFu8; 188];
        let cc = self.cc.get(&pid).copied().unwrap_or(0); // carries no payload: counter stays
        ts[..4].copy_from_slice(&[0x47, (pid >> 8) as u8, pid as u8, 0x20 | cc]);
        ts[4] = 183;
        ts[5] = 0x10;
        let base = pcr as u64;
        ts[6..12].copy_from_slice(&[(base >> 25) as u8, (base >> 17) as u8, (base >> 9) as u8, (base >> 1) as u8, ((base & 1) << 7) as u8 | 0x7E, 0]);
        self.push(ts);
        let hi = pcr * 300;
        let inc = (hi - self.prev27) as f64 / self.pending.len() as f64;
        let mut cur = self.prev27 as f64;
        for mut p in std::mem::take(&mut self.pending) {
            cur += inc;
            p[..4].copy_from_slice(&((cur.round() as i64 as u64 & ATS_MASK) as u32).to_be_bytes());
            self.out.write_all(&p)?;
            self.written += 1;
        }
        self.prev27 = hi;
        self.last_pcr = Some(pcr);
        Ok(())
    }

    /// The PES as TS packets of `pid`; returns their count.
    fn pes(&mut self, pid: u16, data: &[u8]) -> u32 {
        let mut n = 0;
        for (i, chunk) in data.chunks(184).enumerate() {
            let mut ts = [0xFFu8; 188];
            let cc = self.next_cc(pid);
            let (pusi, afc) = (if i == 0 { 0x40 } else { 0 }, if chunk.len() < 184 { 0x30 } else { 0x10 });
            ts[..4].copy_from_slice(&[0x47, pusi | (pid >> 8) as u8, pid as u8, afc | cc]);
            let stuffing = 184 - chunk.len();
            if stuffing == 0 {
                ts[4..].copy_from_slice(chunk);
            } else {
                ts[4] = (stuffing - 1) as u8;
                if stuffing > 1 {
                    ts[5] = 0;
                }
                ts[4 + stuffing..].copy_from_slice(chunk);
            }
            self.push(ts);
            n += 1;
        }
        n
    }

    fn au(&mut self, au: &Au) -> io::Result<()> {
        if self.first.is_none() {
            let fixed = au.pts - VBV_LEN;
            self.prev27 = (fixed - 100) * 300;
            self.first = Some((au.dts, fixed));
        }
        let (min_dts, fixed) = self.first.expect("set above");
        let pcr = au.dts - min_dts + fixed;
        match self.last_pcr {
            None => {
                self.pat_pmt(pcr, true);
                self.pcr(pcr)?;
            }
            Some(last) if pcr - last >= PCR_DELTA => {
                self.pat_pmt(pcr, false);
                self.pcr(pcr)?;
            }
            _ => {}
        }
        let spn = self.count();
        let n = self.pes(au.pid, &pes_bytes(au));
        if let Kind::Video { idr: true } = au.kind {
            if self.ep.last().is_none_or(|e| au.pts as u64 > e.pts) {
                self.ep.push(EpEntry { pts: au.pts as u64, spn, len: (n + 1) * bd_clip::PACKET as u32 });
            }
        }
        self.end_dts = au.dts + au.dur;
        Ok(())
    }

    /// Pads to a whole number of 6144-byte units and writes everything out; returns the packet
    /// count and the EP map.
    fn finish(mut self) -> io::Result<(u32, Vec<EpEntry>)> {
        let Some((min_dts, fixed)) = self.first else { return Ok((0, Vec::new())) };
        let nulls = (ALIGN_PACKETS - self.count() as usize % ALIGN_PACKETS) % ALIGN_PACKETS;
        for _ in 0..nulls {
            let mut ts = [0xFFu8; 188];
            ts[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10 | self.next_cc(0x1FFF)]);
            self.push(ts);
        }
        // The packets after the last PCR arrive up to the end of the last access unit.
        let hi = (self.end_dts - min_dts + fixed) * 300;
        let n = self.pending.len();
        if n > 0 {
            let inc = (hi - self.prev27) as f64 / n as f64;
            let mut cur = self.prev27 as f64;
            for mut p in std::mem::take(&mut self.pending) {
                cur += inc;
                p[..4].copy_from_slice(&((cur.round() as i64 as u64 & ATS_MASK) as u32).to_be_bytes());
                self.out.write_all(&p)?;
                self.written += 1;
            }
        }
        self.out.flush()?;
        Ok((self.written, self.ep))
    }
}

// ---------------------------------------------------------------------------------------------
// ffmpeg's transport stream
// ---------------------------------------------------------------------------------------------

struct Pes {
    pts: Option<i64>,
    dts: Option<i64>,
    payload: Vec<u8>,
}

fn parse_pes(d: &[u8]) -> Option<Pes> {
    if d.len() < 9 || d[..3] != [0, 0, 1] {
        return None;
    }
    let ts = |b: &[u8]| ((b[0] as i64 >> 1 & 7) << 30) | (b[1] as i64) << 22 | (b[2] as i64 >> 1) << 15 | (b[3] as i64) << 7 | b[4] as i64 >> 1;
    let hl = d[8] as usize;
    let body = d.get(9 + hl..)?;
    Some(Pes { pts: (d[7] & 0x80 != 0 && hl >= 5).then(|| ts(&d[9..])), dts: (d[7] & 0x40 != 0 && hl >= 10).then(|| ts(&d[14..])), payload: body.to_vec() })
}

/// Splits an MPEG-TS stream into the PES packets of its elementary streams (PIDs 0x100..0x1000,
/// numbered from 0x100 as ffmpeg assigns them).
struct TsReader<R: Read> {
    r: R,
    open: HashMap<u16, Vec<u8>>,
    ready: VecDeque<(usize, Pes)>,
    eof: bool,
}

impl<R: Read> TsReader<R> {
    fn new(r: R) -> Self {
        TsReader { r, open: HashMap::new(), ready: VecDeque::new(), eof: false }
    }

    fn complete(&mut self, pid: u16, data: Vec<u8>) -> io::Result<()> {
        let pes = parse_pes(&data).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ffmpeg: invalid PES packet"))?;
        self.ready.push_back(((pid - 0x100) as usize, pes));
        Ok(())
    }

    fn next(&mut self) -> io::Result<Option<(usize, Pes)>> {
        while self.ready.is_empty() {
            if self.eof {
                return Ok(None);
            }
            let mut p = [0u8; 188];
            match self.r.read_exact(&mut p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    let mut rest: Vec<_> = std::mem::take(&mut self.open).into_iter().collect();
                    rest.sort_by_key(|(pid, _)| *pid);
                    for (pid, data) in rest {
                        self.complete(pid, data)?;
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
            if p[0] != 0x47 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "ffmpeg: transport stream out of sync"));
            }
            let pid = (p[1] as u16 & 0x1F) << 8 | p[2] as u16;
            if !(0x100..0x1000).contains(&pid) || p[3] & 0x10 == 0 {
                continue;
            }
            let start = if p[3] & 0x20 != 0 { 5 + p[4] as usize } else { 4 };
            let Some(payload) = p.get(start..) else { continue };
            if p[1] & 0x40 != 0 {
                if let Some(done) = self.open.insert(pid, payload.to_vec()) {
                    self.complete(pid, done)?;
                }
            } else if let Some(buf) = self.open.get_mut(&pid) {
                buf.extend(payload);
            }
        }
        Ok(self.ready.pop_front())
    }
}

// ---------------------------------------------------------------------------------------------
// Elementary stream details
// ---------------------------------------------------------------------------------------------

struct VideoClock {
    n: i64,
    pts0: Option<i64>,
    delay: i64,
    /// One frame in 90 kHz ticks.
    dur: f64,
}

impl VideoClock {
    fn au(&mut self, pes: Pes, pid: u16) -> io::Result<Au> {
        let pts = pes.pts.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Video picture without PTS"))?;
        let first = self.n == 0;
        let pts0 = *self.pts0.get_or_insert(pts);
        if first {
            self.delay = ((pts - pes.dts.unwrap_or(pts)) as f64 / self.dur).round() as i64;
        }
        let idx = ((pts - pts0) as f64 / self.dur).round();
        let at = |frames: f64| BASE_PTS + (frames * self.dur).trunc() as i64;
        let (pts, dts) = (at(idx), at((self.n - self.delay) as f64));
        self.n += 1;
        let payload = if first { strip_user_data_sei(&pes.payload) } else { pes.payload };
        Ok(Au { pid, kind: Kind::Video { idr: is_idr(&payload) }, pts, dts, dur: self.dur as i64, payload })
    }
}

/// Positions of the NAL units (start of the header byte) of an Annex B access unit.
fn nal_starts(d: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < d.len() {
            if d[i + 2] > 1 {
                i += 3;
            } else if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
                i += 3;
                return Some(i);
            } else {
                i += 1;
            }
        }
        None
    })
}

/// An IDR picture with its SPS in the same access unit (a seek point).
fn is_idr(au: &[u8]) -> bool {
    let (mut sps, mut idr) = (false, false);
    for at in nal_starts(au) {
        match au[at] & 31 {
            7 => sps = true,
            5 => {
                idr = true;
                break;
            }
            1 => break,
            _ => {}
        }
    }
    sps && idr
}

/// Drops `user_data_unregistered` SEI (x264's version banner) from the first access unit; it
/// would precede the access unit delimiter.
fn strip_user_data_sei(au: &[u8]) -> Vec<u8> {
    let nals: Vec<usize> = nal_starts(au).collect();
    // Start of each NAL unit including its start code (4 bytes with the leading zero byte).
    let unit = |at: usize| at - 3 - usize::from(at >= 4 && au[at - 4] == 0);
    let mut out = Vec::with_capacity(au.len());
    for (i, &at) in nals.iter().enumerate() {
        let end = nals.get(i + 1).map_or(au.len(), |&n| unit(n));
        if !(au[at] & 31 == 6 && au.get(at + 1) == Some(&5)) {
            out.extend(&au[unit(at)..end]);
        }
    }
    out
}

/// Audio parameters read from the first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioInfo {
    channels: u8,
    rate: u32,
    /// PMT descriptor bytes after the program-level ones.
    descriptor_tail: [u8; 4],
}

fn bits(d: &[u8], at: usize, n: usize) -> u32 {
    (at..at + n).fold(0, |v, b| v << 1 | (d.get(b / 8).copied().unwrap_or(0) >> (7 - b % 8) & 1) as u32)
}

fn ac3_info(f: &[u8]) -> Option<(AudioInfo, u32)> {
    if f.len() < 8 || f[..2] != [0x0B, 0x77] {
        return None;
    }
    let (fscod, frmsizecod, bsid, bsmod, acmod) = (f[4] >> 6, f[4] & 0x3F, f[5] >> 3, f[5] & 7, f[6] >> 5);
    let mut at = 51;
    if acmod & 1 != 0 && acmod != 1 {
        at += 2;
    }
    if acmod & 4 != 0 {
        at += 2;
    }
    let dsurmod = if acmod == 2 {
        let v = bits(f, at, 2);
        at += 2;
        v
    } else {
        0
    };
    let lfe = bits(f, at, 1);
    let channels = [2, 1, 2, 3, 3, 4, 4, 5][acmod as usize] + lfe as u8;
    let rate = [48000, 44100, 32000][fscod.min(2) as usize];
    let tail = [fscod << 5 | bsid, (frmsizecod >> 1) << 2 | dsurmod as u8, bsmod << 5 | acmod << 1, 0];
    Some((AudioInfo { channels, rate, descriptor_tail: tail }, 1536))
}

fn dts_info(f: &[u8]) -> Option<(AudioInfo, u32)> {
    if f.len() < 16 || f[..4] != [0x7F, 0xFE, 0x80, 0x01] {
        return None;
    }
    let nblks = bits(f, 39, 7);
    let (amode, sfreq) = (bits(f, 60, 6), bits(f, 66, 4));
    let lfe = bits(f, 66 + 4 + 5 + 5 + 3 + 1 + 1, 2) != 0;
    const CH: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
    let rate = match sfreq {
        1 => 8000,
        2 => 16000,
        3 => 32000,
        6 => 11025,
        7 => 22050,
        8 => 44100,
        11 => 12000,
        12 => 24000,
        13 => 48000,
        _ => 48000,
    };
    Some((AudioInfo { channels: CH[amode as usize & 15] + u8::from(lfe), rate, descriptor_tail: [0; 4] }, (nblks + 1) * 32))
}

/// BD LPCM access unit: 4-byte header (size, channel assignment + rate, bits).
fn lpcm_info(f: &[u8]) -> Option<(AudioInfo, u32)> {
    if f.len() < 5 {
        return None;
    }
    let size = u16::from_be_bytes([f[0], f[1]]) as u32;
    let rate = match f[2] & 15 {
        4 => 96000,
        5 => 192000,
        _ => 48000,
    };
    const CH: [u8; 13] = [0, 1, 0, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8];
    let channels = *CH.get((f[2] >> 4) as usize)?;
    let bytes = match f[3] >> 6 {
        1 => 2,
        _ => 3,
    };
    let samples = size / (bytes * (channels as u32 + channels as u32 % 2));
    Some((AudioInfo { channels, rate, descriptor_tail: [f[2], 0, 0, 0] }, samples))
}

struct AudioClock {
    codec: Codec,
    samples: u64,
    info: Option<AudioInfo>,
    /// LPCM: PCM bytes not yet cut into 5 ms frames, and the header bytes of the stream.
    pcm: Vec<u8>,
    header: [u8; 2],
    frames: u64,
}

impl AudioClock {
    fn new(codec: Codec) -> Self {
        AudioClock { codec, samples: 0, info: None, pcm: Vec::new(), header: [0; 2], frames: 0 }
    }

    fn au(&mut self, pes: Pes, pid: u16) -> io::Result<Vec<Au>> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("Audiospur 0x{pid:x}: unerwartetes Frame-Format"));
        if self.codec == Codec::Lpcm {
            let (info, _) = lpcm_info(&pes.payload).ok_or_else(bad)?;
            self.info.get_or_insert(info);
            self.header = [pes.payload[2], pes.payload[3]];
            self.pcm.extend(&pes.payload[4..]);
            return Ok(self.lpcm_frames(pid));
        }
        let (info, frame) = if self.codec == Codec::Ac3 { ac3_info(&pes.payload) } else { dts_info(&pes.payload) }.ok_or_else(bad)?;
        self.info.get_or_insert(info);
        let rate = self.info.map_or(48000, |i| i.rate) as u64;
        let pts = BASE_PTS + (self.samples * 90_000 / rate) as i64;
        self.samples += frame as u64;
        let dur = (self.samples * 90_000 / rate) as i64 - (pts - BASE_PTS);
        Ok(vec![Au { pid, kind: Kind::CoreAudio, pts, dts: pts, dur, payload: pes.payload }])
    }

    /// ffmpeg's LPCM frames are 7.5 ms; BD LPCM access units are 5 ms (240 samples at 48 kHz),
    /// cut from the contiguous sample bytes; a trailing partial frame is dropped.
    fn lpcm_frames(&mut self, pid: u16) -> Vec<Au> {
        let Some(info) = self.info else { return Vec::new() };
        let bytes = if self.header[1] >> 6 == 1 { 2 } else { 3 };
        let samples = info.rate / 200;
        let frame_len = (samples * bytes * (info.channels as u32 + info.channels as u32 % 2)) as usize;
        let mut out = Vec::new();
        while self.pcm.len() >= frame_len {
            let mut payload = Vec::with_capacity(4 + frame_len);
            payload.extend((frame_len as u16).to_be_bytes());
            // The first access unit carries the start flag (0x20 in the last header byte).
            payload.extend([self.header[0], self.header[1] & 0xC0 | if self.frames == 0 { 0x20 } else { 0 }]);
            payload.extend(self.pcm.drain(..frame_len));
            let pts = BASE_PTS + (self.frames * samples as u64 * 90_000 / info.rate as u64) as i64;
            let dur = ((self.frames + 1) * samples as u64 * 90_000 / info.rate as u64) as i64 - (pts - BASE_PTS);
            out.push(Au { pid, kind: Kind::Private, pts, dts: pts, dur, payload });
            self.frames += 1;
        }
        out
    }
}

fn audio_stream(pid: u16, codec: Codec, lang: [u8; 3], info: AudioInfo) -> Stream {
    let presentation = match info.channels {
        2 => 3,
        c if c > 3 => 6,
        c => c,
    };
    let rate_index = match info.rate {
        96000 => 4,
        192000 => 5,
        _ => 1,
    };
    let mut es_info = Vec::new();
    match codec {
        Codec::Ac3 => {
            es_info.extend([0x05, 0x04, b'A', b'C', b'-', b'3', 0x81, 0x04]);
            es_info.extend(info.descriptor_tail);
        }
        Codec::Lpcm => es_info.extend([0x05, 0x08, b'H', b'D', b'M', b'V', 0xFF, 0x80, presentation << 4 | rate_index, 0xFF]),
        _ => {}
    }
    Stream { pid, codec, lang, attr: presentation << 4 | rate_index, es_info }
}

// ---------------------------------------------------------------------------------------------
// Subtitle files
// ---------------------------------------------------------------------------------------------

/// Segments of a `.sup` stream as PES payloads (segment type, length, data) with their stamps.
fn sup_aus(sup: &[u8], pid: u16) -> io::Result<Vec<Au>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "Subtitle file: invalid .sup format");
    let mut aus = Vec::new();
    let mut i = 0;
    while i < sup.len() {
        let h = sup.get(i..i + 13).filter(|h| &h[..2] == b"PG").ok_or_else(bad)?;
        let n = |at: usize| u32::from_be_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]]) as i64;
        let len = u16::from_be_bytes([h[11], h[12]]) as usize;
        let data = sup.get(i + 10..i + 13 + len).ok_or_else(bad)?;
        let (pts, dts) = (BASE_PTS + n(2), if n(6) == 0 { BASE_PTS + n(2) } else { BASE_PTS + n(6) });
        aus.push(Au { pid, kind: Kind::Private, pts, dts, dur: 0, payload: data.to_vec() });
        i += 13 + len;
    }
    Ok(aus)
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct VideoSpec {
    pub fps: (u32, u32),
    pub width: u32,
    /// 1080i-flagged material (x264 `fake-interlaced`).
    pub interlaced: bool,
}

#[derive(Debug, Clone)]
pub struct SubSpec {
    pub sup: PathBuf,
    pub lang: [u8; 3],
    /// Interactive Graphics (menu) instead of Presentation Graphics.
    pub ig: bool,
}

/// One clip + playlist to write.
#[derive(Debug, Clone)]
pub struct MuxJob {
    /// Encoded title: video first, then the audio tracks in `audio` order.
    pub mkv: PathBuf,
    pub video: VideoSpec,
    pub audio: Vec<(Codec, [u8; 3])>,
    pub subs: Vec<SubSpec>,
    /// Chapter marks (seconds from the start); empty = one mark at the start.
    pub chapters: Vec<f64>,
    /// Clip / playlist number.
    pub number: usize,
    /// The playlist waits forever at its end (menu pages).
    pub still: bool,
    /// `BDMV` folder.
    pub bdmv: PathBuf,
    pub duration_secs: f64,
}

/// Creates the folders tsMuxeR created below `BDMV` (and the empty `CERTIFICATE`).
pub fn create_skeleton(bdmv: &Path) -> io::Result<()> {
    for d in ["AUXDATA", "BACKUP/BDJO", "BACKUP/CLIPINF", "BACKUP/JAR", "BACKUP/PLAYLIST", "BDJO", "CLIPINF", "JAR", "META", "PLAYLIST", "STREAM"] {
        std::fs::create_dir_all(bdmv.join(d))?;
    }
    Ok(())
}

impl MuxJob {
    /// ffmpeg arguments: stream copy of the title into an MPEG-TS pipe.
    pub fn ffmpeg_args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec!["-y".into(), "-i".into(), self.mkv.clone().into(), "-map".into(), "0:v:0".into()];
        for k in 0..self.audio.len() {
            a.extend(["-map".into(), format!("0:a:{k}").into()]);
        }
        a.extend(["-c", "copy"].map(OsString::from));
        for (k, (codec, _)) in self.audio.iter().enumerate() {
            if *codec == Codec::Lpcm {
                a.extend([format!("-c:a:{k}").into(), "pcm_bluray".into()]);
            }
        }
        a.extend(["-pes_payload_size", "0", "-f", "mpegts", "pipe:1"].map(OsString::from));
        a
    }

    /// Muxes the transport stream `ts` (ffmpeg's output for [`Self::ffmpeg_args`]) and writes the
    /// clip's `.m2ts`, `.clpi` and `.mpls` (and their `BACKUP` copies).
    pub fn mux(&self, ts: impl Read) -> io::Result<MuxSummary> {
        let n = self.number;
        let m2ts = self.bdmv.join(format!("STREAM/{n:05}.m2ts"));
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
        let dur = 90_000.0 * self.video.fps.1 as f64 / self.video.fps.0 as f64;

        // Subtitle segments are known up front.
        let mut streams: Vec<Stream> = Vec::new();
        let mut sub_aus: Vec<Au> = Vec::new();
        let (mut pg, mut ig) = (0u16, 0u16);
        for s in &self.subs {
            let (pid, codec) = if s.ig { (bd_clip::IG_PID + ig, Codec::Ig) } else { (bd_clip::PGS_PID + pg, Codec::Pgs) };
            if s.ig { ig += 1 } else { pg += 1 }
            sub_aus.extend(sup_aus(&std::fs::read(&s.sup)?, pid)?);
            streams.push(Stream { pid, codec, lang: s.lang, attr: 0, es_info: Vec::new() });
        }

        // Video and audio come from ffmpeg; their stream details are known after the first frames.
        let mut reader = TsReader::new(ts);
        let mut video = VideoClock { n: 0, pts0: None, delay: 0, dur };
        let mut audio: Vec<AudioClock> = self.audio.iter().map(|&(codec, _)| AudioClock::new(codec)).collect();
        let mut queues: Vec<VecDeque<Au>> = (0..1 + audio.len()).map(|_| VecDeque::new()).collect();
        let mut sub_queue: VecDeque<Au> = {
            sub_aus.sort_by_key(|a| (a.dts, a.pid));
            sub_aus.into()
        };
        let mut ended = false;
        let mut packetizer: Option<Packetizer<BufWriter<std::fs::File>>> = None;
        let mut max_idx_pts = 0i64;
        let mut ep: Vec<EpEntry> = Vec::new();
        let mut packets = 0u32;

        loop {
            if !ended {
                match reader.next()? {
                    Some((idx, pes)) if idx <= audio.len() => {
                        if idx == 0 {
                            let au = video.au(pes, bd_clip::VIDEO_PID)?;
                            max_idx_pts = max_idx_pts.max(au.pts);
                            queues[0].push_back(au);
                        } else {
                            queues[idx].extend(audio[idx - 1].au(pes, bd_clip::AUDIO_PID + idx as u16 - 1)?);
                        }
                    }
                    Some((idx, _)) => return Err(bad(format!("ffmpeg delivered an unexpected data stream {idx}"))),
                    None => ended = true,
                }
            }
            // Every running stream needs a queued unit before the earliest one can be written.
            while (ended || queues.iter().all(|q| !q.is_empty())) && (!queues.iter().all(VecDeque::is_empty) || !sub_queue.is_empty()) {
                if packetizer.is_none() {
                    if queues[0].is_empty() {
                        return Err(bad("ffmpeg delivered no video".into()));
                    }
                    // Stream list (and with it the PMT) is complete once all first frames are in.
                    let mut all = vec![video_stream(&self.video)];
                    for (k, a) in audio.iter().enumerate() {
                        let info = a.info.ok_or_else(|| bad(format!("Audio track {} is empty", k + 1)))?;
                        all.push(audio_stream(bd_clip::AUDIO_PID + k as u16, a.codec, self.audio[k].1, info));
                    }
                    all.extend(streams.iter().cloned());
                    all.sort_by_key(|s| s.pid);
                    streams = all;
                    if let Some(dir) = m2ts.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    packetizer = Some(Packetizer::new(BufWriter::with_capacity(1 << 22, std::fs::File::create(&m2ts)?), streams.clone()));
                }
                // Earliest decode time; ties by PID.
                let heads = queues.iter().enumerate().filter_map(|(i, q)| q.front().map(|a| (a.dts, a.pid, Some(i)))).chain(sub_queue.front().map(|a| (a.dts, a.pid, None)));
                let Some((_, _, from)) = heads.min_by_key(|&(d, p, _)| (d, p)) else { break };
                let au = match from {
                    Some(i) => queues[i].pop_front(),
                    None => sub_queue.pop_front(),
                }
                .expect("head exists");
                packetizer.as_mut().expect("created above").au(&au)?;
            }
            if ended && queues.iter().all(VecDeque::is_empty) && sub_queue.is_empty() {
                break;
            }
        }
        if let Some(p) = packetizer.take() {
            (packets, ep) = p.finish()?;
        }
        if packets == 0 {
            return Err(bad("ffmpeg delivered no data".into()));
        }

        // Clip and playlist: the clip ends one frame after the last picture's PTS.
        let frames = ((max_idx_pts - BASE_PTS) as f64 / dur).round() + 1.0;
        let (start, end) = ((BASE_PTS / 2) as u32, ((BASE_PTS as f64 + frames * dur) as i64 / 2) as u32);
        let clip = Clip { streams, packets, start, end, ep };
        let list = Playlist { clip: n, start, end, marks: &self.chapters, still: self.still, streams: &clip.streams };
        let (clpi, mpls) = (bd_clip::clpi(&clip), bd_clip::mpls(&list));
        for dir in [self.bdmv.clone(), self.bdmv.join("BACKUP")] {
            std::fs::create_dir_all(dir.join("CLIPINF"))?;
            std::fs::create_dir_all(dir.join("PLAYLIST"))?;
            std::fs::write(dir.join(format!("CLIPINF/{n:05}.clpi")), &clpi)?;
            std::fs::write(dir.join(format!("PLAYLIST/{n:05}.mpls")), &mpls)?;
        }
        Ok(MuxSummary { packets, pictures: video.n as u64, idr: clip.ep.len() })
    }
}

fn video_stream(v: &VideoSpec) -> Stream {
    let attr = bd_clip::video_format(v.width, v.interlaced) << 4 | bd_clip::frame_rate_index(v.fps);
    let mut es_info = vec![0x05, 0x08, b'H', b'D', b'M', b'V', 0xFF, 0x1B, attr, 0x3F];
    es_info.shrink_to_fit();
    Stream { pid: bd_clip::VIDEO_PID, codec: Codec::H264, lang: *b"und", attr, es_info }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxSummary {
    pub packets: u32,
    pub pictures: u64,
    pub idr: usize,
}

/// Runs ffmpeg and muxes its output ([`MuxJob::mux`]); returns a log line.
pub async fn mux_title(tools: &Toolchain, job: &MuxJob) -> Result<String, ToolError> {
    let owned = job.clone();
    let summary = tools
        .ffmpeg_sink(&job.ffmpeg_args(), job.duration_secs, |_| {}, move |pipe| owned.mux(std::io::BufReader::with_capacity(1 << 20, pipe)))
        .await?;
    Ok(format!("{:05}.m2ts: {} packets, {} pictures, {} entry points\n", job.number, summary.packets, summary.pictures, summary.idr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376_E6E7);
        // The PAT of every tsMuxeR clip.
        let pat = unhex("00b0110000c100000000e01f0001e100");
        assert_eq!(crc32_mpeg(&pat).to_be_bytes(), [0x23, 0x5A, 0xAB, 0x82]);
    }

    #[test]
    fn pes_header_layouts() {
        let au = |kind, pts, dts| Au { pid: 0x1011, kind, pts, dts, dur: 0, payload: vec![1, 2, 3] };
        // Picture with PTS and DTS: flags 0x84 0xC0, 10 bytes of stamps, length without the first 6 bytes.
        let v = pes_bytes(&au(Kind::Video { idr: false }, 54_000_000, 53_992_493));
        assert_eq!(&v[..9], &[0, 0, 1, 0xE0, 0, 16, 0x84, 0xC0, 10]);
        assert_eq!(&v[9..14], &[0x31, 0x0C, 0xDF, 0xF3, 0x01]);
        assert_eq!(&v[14..19], &[0x11, 0x0C, 0xDF, 0xB8, 0x5B]);
        assert_eq!(&v[19..], &[1, 2, 3]);
        // Audio: private stream with the extension 0x71; PTS only.
        let a = pes_bytes(&au(Kind::CoreAudio, 54_000_000, 54_000_000));
        assert_eq!(&a[..9], &[0, 0, 1, 0xFD, 0, 14, 0x84, 0x81, 8]);
        assert_eq!(&a[14..17], &[0x01, 0x81, 0x71]);
        // Over 64 KiB the length stays 0.
        let big = Au { payload: vec![0; 70_000], ..au(Kind::Video { idr: false }, 0, 0) };
        assert_eq!(&pes_bytes(&big)[4..6], &[0, 0]);
    }

    #[test]
    fn audio_frame_headers() {
        // AC-3 stereo 192 kbit/s (first frame of tsMuxeR's test clip).
        let (info, samples) = ac3_info(&unhex("0b77fe95144043e106f46370")).unwrap();
        assert_eq!((info.channels, info.rate, samples, info.descriptor_tail), (2, 48000, 1536, [0x08, 0x28, 0x04, 0]));
        // Mono: acmod 1, no mix levels in front of lfeon.
        let (mono, _) = ac3_info(&unhex("0b77fe9514402f84")).unwrap();
        assert_eq!((mono.channels, mono.descriptor_tail[2]), (1, 0x02));
        // DTS core stereo, 512 samples per frame.
        let (dts, samples) = dts_info(&unhex("7ffe8001fc3c3ff0b5e001380003ef7fe006db15")).unwrap();
        assert_eq!((dts.channels, dts.rate, samples), (2, 48000, 512));
        // BD LPCM, 24 bit stereo: 4-byte header, 5 ms = 240 samples * 6 bytes.
        let mut f = vec![0x05, 0xA0, 0x31, 0xC0];
        f.resize(4 + 1440, 0);
        let (pcm, samples) = lpcm_info(&f).unwrap();
        assert_eq!((pcm.channels, pcm.rate, samples), (2, 48000, 240));
        assert!(ac3_info(&[0; 16]).is_none() && dts_info(&[0; 16]).is_none() && lpcm_info(&[0; 2]).is_none());
    }

    #[test]
    fn lpcm_is_cut_into_5_ms_frames() {
        let mut clock = AudioClock::new(Codec::Lpcm);
        // ffmpeg's 7.5 ms frames: 360 stereo samples of 24 bit.
        let frame = |fill: u8| {
            let mut p = vec![0x08, 0x70, 0x31, 0xC0];
            p.extend(vec![fill; 2160]);
            Pes { pts: Some(0), dts: None, payload: p }
        };
        let mut aus = clock.au(frame(1), 0x1100).unwrap();
        aus.extend(clock.au(frame(2), 0x1100).unwrap());
        // 720 samples = 3 frames of 240; the first one has the start flag.
        assert_eq!(aus.len(), 3);
        assert_eq!(&aus[0].payload[..4], &[0x05, 0xA0, 0x31, 0xE0]);
        assert_eq!(&aus[1].payload[..4], &[0x05, 0xA0, 0x31, 0xC0]);
        assert_eq!(aus.iter().map(|a| a.pts - BASE_PTS).collect::<Vec<_>>(), vec![0, 450, 900]);
        // The second frame's bytes continue the sample stream: frame 2 holds 120 old + 120 new samples.
        assert_eq!((aus[1].payload[4], aus[1].payload[4 + 720], aus[2].payload[4]), (1, 2, 2));
    }

    #[test]
    fn nal_helpers() {
        let sps = [0, 0, 0, 1, 0x67, 1, 2];
        let au = |nals: &[&[u8]]| nals.iter().flat_map(|n| n.iter().copied()).collect::<Vec<u8>>();
        let aud = [0, 0, 0, 1, 0x09, 0x10];
        let idr = [0, 0, 1, 0x65, 9];
        let p = [0, 0, 1, 0x41, 9];
        assert!(is_idr(&au(&[&aud, &sps, &idr])));
        // No SPS in front: not a usable seek point; a P slice ends the search.
        assert!(!is_idr(&au(&[&aud, &idr])) && !is_idr(&au(&[&aud, &p, &sps, &idr])));
        // x264's banner (SEI payload type 5) before the AUD is dropped, other SEI stay.
        let banner = [0, 0, 1, 0x06, 5, 2, 7, 7, 0x80];
        let timing = [0, 0, 1, 0x06, 1, 1, 0, 0x80];
        let stripped = strip_user_data_sei(&au(&[&banner, &aud, &timing, &idr]));
        assert_eq!(stripped, au(&[&aud, &timing, &idr]));
    }

    #[test]
    fn packetizer_pads_and_stamps() {
        let streams = vec![Stream { pid: bd_clip::VIDEO_PID, codec: Codec::H264, lang: *b"und", attr: 0x61, es_info: vec![] }];
        let mut out = Vec::new();
        let (packets, ep) = {
            let mut p = Packetizer::new(&mut out, streams);
            for i in 0..4i64 {
                let payload = [&[0, 0, 0, 1, 0x67][..], &[0, 0, 1, 0x65], &vec![0xAA; 1000 * (i as usize + 1)]].concat();
                let au = Au { pid: bd_clip::VIDEO_PID, kind: Kind::Video { idr: true }, pts: BASE_PTS + i * 3754, dts: BASE_PTS + i * 3754 - 7508, dur: 3754, payload };
                p.au(&au).unwrap();
            }
            p.finish().unwrap()
        };
        assert_eq!(out.len(), packets as usize * 192);
        assert_eq!(packets % 32, 0, "whole 6144-byte units");
        assert_eq!(ep.len(), 4);
        // PAT, PMT, SIT, PCR, then the first picture; its entry points right behind them.
        let pid = |n: usize| (u16::from(out[n * 192 + 5] & 0x1F) << 8) | u16::from(out[n * 192 + 6]);
        assert_eq!((0..5).map(pid).collect::<Vec<_>>(), vec![0, 0x100, 0x1F, 0x1001, 0x1011]);
        assert_eq!(ep[0], EpEntry { pts: BASE_PTS as u64, spn: 4, len: (6 + 1) * 192 });
        // Arrival times rise; the PCR packet's equals its PCR (the first PCR is 500 ms before the first PTS).
        let ats = |n: usize| u32::from_be_bytes(out[n * 192..n * 192 + 4].try_into().unwrap());
        assert!((1..packets as usize).all(|n| ats(n) >= ats(n - 1)));
        assert_eq!(ats(3) as i64, (BASE_PTS - VBV_LEN) * 300 & 0x3FFF_FFFF);
        // The padding is null packets on PID 0x1FFF.
        assert_eq!(pid(packets as usize - 1), 0x1FFF);
    }
}
