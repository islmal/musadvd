//! Origin: derived from tsMuxer 2.7.0 (Apache-2.0, justdan96/tsMuxer); see THIRD_PARTY_NOTICES.md.
//! Clip information (`.clpi`) and playlist (`.mpls`) files of one muxed clip, in the layout
//! tsMuxeR 2.7.0 writes with `--blu-ray` (version `0200`; port of `CLPIParser::compose*` and
//! `MPLSParser::compose*`, field by field checked against its output).

pub const PMT_PID: u16 = 0x0100;
pub const PCR_PID: u16 = 0x1001;
pub const VIDEO_PID: u16 = 0x1011;
pub const AUDIO_PID: u16 = 0x1100;
pub const PGS_PID: u16 = 0x1200;
pub const IG_PID: u16 = 0x1400;

/// Source packets are 192 bytes (4-byte arrival time + 188).
pub const PACKET: usize = 192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Ac3,
    Dts,
    DtsHdHr,
    DtsHdMa,
    Lpcm,
    Pgs,
    Ig,
}

impl Codec {
    /// `stream_coding_type` (also the PMT stream type).
    pub fn coding_type(self) -> u8 {
        match self {
            Codec::H264 => 0x1B,
            Codec::Ac3 => 0x81,
            Codec::Dts => 0x82,
            Codec::DtsHdHr => 0x85,
            Codec::DtsHdMa => 0x86,
            Codec::Lpcm => 0x80,
            Codec::Pgs => 0x90,
            Codec::Ig => 0x91,
        }
    }

    fn is_audio(self) -> bool {
        matches!(self, Codec::Ac3 | Codec::Dts | Codec::DtsHdHr | Codec::DtsHdMa | Codec::Lpcm)
    }
}

/// One elementary stream of a clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub pid: u16,
    pub codec: Codec,
    /// ISO 639-2 tag (`und` for video).
    pub lang: [u8; 3],
    /// Video: `video_format << 4 | frame_rate_index`; audio: `presentation_type << 4 | sampling_frequency_index`.
    pub attr: u8,
    /// PMT descriptors after the program-level ones (ES info).
    pub es_info: Vec<u8>,
}

impl Stream {
    fn aspect_16_9() -> u8 {
        3
    }
}

/// `video_format` of the clip attributes.
pub fn video_format(width: u32, interlaced: bool) -> u8 {
    match (width, interlaced) {
        (1300.., true) => 4,
        (1300.., false) => 6,
        _ => 5,
    }
}

/// Frame rate index (BD: 1 = 24000/1001 … 7 = 60000/1001).
pub fn frame_rate_index(fps: (u32, u32)) -> u8 {
    crate::pgs::frame_rate_code(fps)
}

/// One entry of the EP map: an IDR picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpEntry {
    /// PTS (90 kHz).
    pub pts: u64,
    /// Source packet number of the first packet of the PES.
    pub spn: u32,
    /// Bytes from the picture's first packet to the end of its PES.
    pub len: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Clip {
    /// By ascending PID, as tsMuxeR keeps them.
    pub streams: Vec<Stream>,
    pub packets: u32,
    /// Presentation start / end (45 kHz).
    pub start: u32,
    pub end: u32,
    pub ep: Vec<EpEntry>,
}

fn be16(v: &mut Vec<u8>, x: u16) {
    v.extend(x.to_be_bytes());
}

fn be32(v: &mut Vec<u8>, x: u32) {
    v.extend(x.to_be_bytes());
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

fn pad2(v: &mut Vec<u8>) {
    if !v.len().is_multiple_of(2) {
        v.push(0);
    }
}

/// Fills `at..at+4` with the current length as a start address / the length since `from`.
fn patch32(v: &mut [u8], at: usize, x: usize) {
    v[at..at + 4].copy_from_slice(&(x as u32).to_be_bytes());
}

/// Length field written before its content.
fn block32(v: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    let at = v.len();
    be32(v, 0);
    body(v);
    let len = v.len() - at - 4;
    patch32(v, at, len);
}

/// ISRC of the stream coding info: all `0` characters (tsMuxeR's default).
const ISRC: &[u8; 12] = b"000000000000";

fn coding_info(v: &mut Vec<u8>, s: &Stream) {
    let at = v.len();
    v.push(0);
    v.push(s.codec.coding_type());
    match s.codec {
        Codec::H264 => v.extend([s.attr, Stream::aspect_16_9() << 4, 0, 0]),
        c if c.is_audio() => {
            v.push(s.attr);
            v.extend(s.lang);
        }
        _ => {
            v.extend(s.lang);
            v.push(0);
        }
    }
    v.extend(ISRC);
    v.extend([0; 4]);
    v[at] = (v.len() - at - 1) as u8;
}

fn end_code(len: u32) -> u32 {
    const LIMITS: [u32; 6] = [131072, 262144, 393216, 589824, 917504, 1310720];
    if len == 0 {
        0
    } else {
        LIMITS.iter().position(|&l| len < l).map_or(7, |p| p as u32 + 1)
    }
}

/// `TS_recording_rate` (bytes/s): the 48 Mbit/s BD maximum.
const RECORDING_RATE: u32 = 48_000_000 / 8;

fn ep_map(v: &mut Vec<u8>, c: &Clip) {
    let start = v.len();
    let video: Vec<&Stream> = c.streams.iter().filter(|s| s.codec == Codec::H264).collect();
    v.push(0);
    v.push(video.len() as u8);
    // Coarse table: a new entry when the coarse PTS or the 128 K-packet window changes.
    let mut coarse: Vec<(u32, u32, u32)> = Vec::new();
    let (mut last_pts, mut last_spn) = (0u32, 0u32);
    for (i, e) in c.ep.iter().enumerate() {
        let pts = (e.pts >> 19) as u32;
        if coarse.is_empty() || pts != last_pts || last_spn & 0xFFFE_0000 != e.spn & 0xFFFE_0000 {
            coarse.push((pts, i as u32, e.spn));
        }
        (last_pts, last_spn) = (pts, e.spn);
    }
    let mut starts = Vec::new();
    for s in &video {
        be16(v, s.pid);
        // 10 reserved bits, EP_stream_type 1, coarse count (16), fine count (18).
        let word = (1u64 << 34) | (coarse.len() as u64) << 18 | c.ep.len() as u64;
        v.extend(&word.to_be_bytes()[2..]);
        starts.push(v.len());
        be32(v, 0);
    }
    pad2(v);
    for at in starts {
        let n = v.len() - start;
        patch32(v, at, n);
        let block = v.len();
        be32(v, 0);
        for &(pts, fine, spn) in &coarse {
            be32(v, fine << 14 | pts & 0x3FFF);
            be32(v, spn);
        }
        pad2(v);
        let n = v.len() - block;
        patch32(v, block, n);
        for e in &c.ep {
            // is_angle_change_point 0, I_end_position_offset, PTS_EP_fine, SPN_EP_fine.
            let fine = end_code(e.len) << 28 | (((e.pts >> 9) & 0x7FF) as u32) << 17 | e.spn & 0x1FFFF;
            be32(v, fine);
        }
        pad2(v);
    }
}

/// The `.clpi` file.
pub fn clpi(c: &Clip) -> Vec<u8> {
    let mut v = b"HDMV0200".to_vec();
    v.extend([0u8; 32]);
    block32(&mut v, |v| {
        be16(v, 0);
        v.extend([1, 1]); // clip_stream_type AV stream, application_type main TS
        be32(v, 0); // reserved, is_ATC_delta = 0
        be32(v, RECORDING_RATE);
        be32(v, c.packets);
        v.extend([0u8; 128]);
        be16(v, 30);
        v.push(0x80);
        v.extend(b"HDMV");
        v.extend([0u8; 25]);
    });
    pad4(&mut v);
    let seq = v.len();
    block32(&mut v, |v| {
        v.extend([0, 1]); // one ATC sequence
        be32(v, 0);
        v.extend([1, 0]); // one STC sequence
        be16(v, PCR_PID);
        be32(v, 0);
        be32(v, c.start);
        be32(v, c.end);
    });
    pad4(&mut v);
    let prog = v.len();
    block32(&mut v, |v| {
        v.extend([0, 1]);
        be32(v, 0);
        be16(v, PMT_PID);
        v.push(c.streams.len() as u8);
        v.push(0);
        for s in &c.streams {
            be16(v, s.pid);
            coding_info(v, s);
        }
    });
    pad4(&mut v);
    let cpi = v.len();
    block32(&mut v, |v| {
        be16(v, 1); // CPI_type
        ep_map(v, c);
    });
    pad4(&mut v);
    let mark = v.len();
    be32(&mut v, 0);
    let ext = v.len();
    be32(&mut v, 0);
    for (i, at) in [seq, prog, cpi, mark, ext].into_iter().enumerate() {
        patch32(&mut v, 8 + 4 * i, at);
    }
    v
}

/// One playlist with a single PlayItem over a whole clip.
#[derive(Debug, Clone)]
pub struct Playlist<'a> {
    /// Clip number (`NNNNN.clpi` / `.m2ts`).
    pub clip: usize,
    pub start: u32,
    pub end: u32,
    /// Chapter marks relative to the clip start, in seconds.
    pub marks: &'a [f64],
    /// The item waits forever at its end (menus).
    pub still: bool,
    pub streams: &'a [Stream],
}

fn stream_entry(v: &mut Vec<u8>, s: &Stream) {
    v.push(9);
    v.push(1);
    be16(v, s.pid);
    v.extend([0u8; 6]);
    let at = v.len();
    v.push(0);
    v.push(s.codec.coding_type());
    match s.codec {
        Codec::H264 => v.extend([s.attr, 0, 0, 0]),
        c if c.is_audio() => {
            v.push(s.attr);
            v.extend(s.lang);
        }
        _ => {
            v.extend(s.lang);
            v.push(0);
        }
    }
    v[at] = (v.len() - at - 1) as u8;
}

/// The `.mpls` file. Chapter marks beyond the clip length are dropped.
pub fn mpls(p: &Playlist) -> Vec<u8> {
    let mut v = b"MPLS0200".to_vec();
    v.extend([0u8; 32]);
    block32(&mut v, |v| {
        v.extend([0, 1]); // playback type: sequential
        be16(v, 0);
        v.extend([0u8; 8]); // UO mask
        be16(v, 0x4000); // audio_mix_app_flag
    });
    pad2(&mut v);
    let list = v.len();
    block32(&mut v, |v| {
        be16(v, 0);
        be16(v, 1);
        be16(v, 0);
        let at = v.len();
        be16(v, 0);
        v.extend(format!("{:05}M2TS", p.clip).bytes());
        be16(v, 1); // connection_condition 1
        v.push(0); // ref_to_STC_id
        be32(v, p.start);
        be32(v, p.end);
        v.extend([0u8; 8]); // UO mask
        v.push(0); // PlayItem_random_access_flag
        // still_mode 2 = infinite (1 = timed, where still_time 0 is illegal and the PS4 ends the item).
        v.push(if p.still { 2 } else { 0 });
        be16(v, 0); // still_time
        // STN table
        let stn = v.len();
        be16(v, 0);
        be16(v, 0);
        let count = |c: Codec| p.streams.iter().filter(|s| s.codec == c).count() as u8;
        let audio = p.streams.iter().filter(|s| s.codec.is_audio()).count() as u8;
        v.extend([count(Codec::H264), audio, count(Codec::Pgs), count(Codec::Ig), 0, 0, 0, 0]);
        v.extend([0u8; 4]);
        let picks: [&dyn Fn(&Stream) -> bool; 4] =
            [&|s| s.codec == Codec::H264, &|s| s.codec.is_audio(), &|s| s.codec == Codec::Pgs, &|s| s.codec == Codec::Ig];
        for pick in picks {
            for s in p.streams.iter().filter(|s| pick(s)) {
                stream_entry(v, s);
            }
        }
        let len = v.len() - stn - 2;
        v[stn..stn + 2].copy_from_slice(&(len as u16).to_be_bytes());
        let len = v.len() - at - 2;
        v[at..at + 2].copy_from_slice(&(len as u16).to_be_bytes());
    });
    pad2(&mut v);
    let marks = v.len();
    block32(&mut v, |v| {
        let mut times: Vec<u32> =
            p.marks.iter().map(|m| (m * 45000.0) as u32).filter(|&m| m <= p.end - p.start).map(|m| m + p.start).collect();
        // Strict players need at least one entry mark, even if every chapter lies beyond the clip.
        if times.is_empty() {
            times.push(p.start);
        }
        be16(v, times.len() as u16);
        for t in times {
            v.extend([0, 1]); // mark_type: chapter
            be16(v, 0);
            be32(v, t);
            be16(v, 0xFFFF);
            be32(v, 0);
        }
    });
    pad2(&mut v);
    patch32(&mut v, 8, list);
    patch32(&mut v, 12, marks);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// tsMuxeR 2.7.0: 2 s of 1080p23.976 H.264 + stereo AC-3 ("deu"), chapters at 0 and 1 s.
    const MPLS: &str = "4d504c53303230300000003a000000960000000000000000000000000000000000000000000000000000000e000100000000000000000000400000000058000000010000005030303030304d325453000100019bfcc0019d5caa000000000000000000000000002e000001010000000000000000000009011011000000000000051b61000000090111000000000000000581316465750000001e000200010000019bfcc0ffff0000000000010000019cac88ffff00000000";

    fn video() -> Stream {
        Stream { pid: VIDEO_PID, codec: Codec::H264, lang: *b"und", attr: 0x61, es_info: vec![] }
    }

    fn audio() -> Stream {
        Stream { pid: AUDIO_PID, codec: Codec::Ac3, lang: *b"deu", attr: 0x31, es_info: vec![] }
    }

    #[test]
    fn playlist_matches_tsmuxer() {
        let streams = [video(), audio()];
        let p = Playlist { clip: 0, start: 27_000_000, end: 27_090_090, marks: &[0.0, 1.0], still: false, streams: &streams };
        let want = unhex(MPLS);
        let got = mpls(&p);
        assert_eq!(got.len(), want.len());
        assert_eq!(got.iter().zip(&want).position(|(a, b)| a != b), None, "{}", got.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }

    /// Same clip; the IDRs are at packets 4 and 1708.
    const CLPI: &str = "48444d5630323030000000dc000000f800000138000001680000016c000000000000000000000000000000b00000010100000000005b8d80000010400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001e8048444d5600000000000000000000000000000000000000000000000000000000160001000000000100100100000000019bfcc0019d5caa00000000003a000100000000010002001011151b613000003030303030303030303030300000000011001581316465753030303030303030303030300000000000000000002c0001000110110004000800020000000e00000014000000660000000400004067000006ac17f80004195806ac0000000000000000";

    #[test]
    fn clip_info_matches_tsmuxer() {
        let c = Clip {
            streams: vec![video(), audio()],
            packets: 4160,
            start: 27_000_000,
            end: 27_090_090,
            ep: vec![EpEntry { pts: 54_000_000, spn: 4, len: 192 * 200 }, EpEntry { pts: 54_090_090, spn: 1708, len: 192 * 50 }],
        };
        let want = unhex(CLPI);
        let got = clpi(&c);
        assert_eq!(got, want);
    }

    /// A menu clip (video, AC-3 "eng", IG) with the item waiting forever: tsMuxeR 2.7.0's playlist
    /// of the clip with a PGS stream, with the PG entry turned into an IG entry and the still
    /// mode set to infinite (`still_mode` 2, `still_time` 0).
    #[test]
    fn menu_playlist_with_ig_and_still() {
        let streams = [
            Stream { pid: VIDEO_PID, codec: Codec::H264, lang: *b"und", attr: 0x51, es_info: vec![] },
            Stream { pid: AUDIO_PID, codec: Codec::Ac3, lang: *b"eng", attr: 0x31, es_info: vec![] },
            Stream { pid: IG_PID, codec: Codec::Ig, lang: *b"eng", attr: 0, es_info: vec![] },
        ];
        let p = Playlist { clip: 0, start: 27_000_000, end: 27_090_090, marks: &[], still: true, streams: &streams };
        let want = unhex("4d504c53303230300000003a000000a60000000000000000000000000000000000000000000000000000000e000100000000000000000000400000000068000000010000006030303030304d325453000100019bfcc0019d5caa000000000000000000020000003e000001010001000000000000000009011011000000000000051b5100000009011100000000000000058131656e67090114000000000000000591656e670000000010000100010000019bfcc0ffff00000000");
        assert_eq!(hex(&mpls(&p)), hex(&want));
    }

    #[test]
    fn chapters_beyond_end_keep_one_entry_mark() {
        let streams = [video()];
        let p = Playlist { clip: 0, start: 27_000_000, end: 27_090_090, marks: &[100.0, 200.0], still: false, streams: &streams };
        let v = mpls(&p);
        let n = v.len();
        assert_eq!(u16::from_be_bytes([v[n - 16], v[n - 15]]), 1, "mark count");
        assert_eq!(v[n - 13], 1, "entry mark type");
        assert_eq!(u32::from_be_bytes(v[n - 10..n - 6].try_into().unwrap()), 27_000_000);
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn end_codes() {
        assert_eq!([end_code(0), end_code(1), end_code(131072), end_code(1310719), end_code(1310720)], [0, 1, 2, 6, 7]);
    }

    #[test]
    fn dts_hd_coding_types_in_clpi_and_mpls() {
        assert_eq!(Codec::DtsHdHr.coding_type(), 0x85);
        assert_eq!(Codec::DtsHdMa.coding_type(), 0x86);
        let streams = [
            video(),
            Stream { pid: AUDIO_PID, codec: Codec::DtsHdMa, lang: *b"eng", attr: 0x64, es_info: vec![] },
            Stream { pid: AUDIO_PID + 1, codec: Codec::DtsHdHr, lang: *b"deu", attr: 0x61, es_info: vec![] },
        ];
        let p = Playlist { clip: 0, start: 27_000_000, end: 27_090_090, marks: &[0.0], still: false, streams: &streams };
        let m = mpls(&p);
        // STN audio count = 2, and stream_entry coding bytes 0x86 (attr 0x64, "eng") and 0x85 (attr 0x61, "deu") appear.
        assert!(m.windows(5).any(|w| w == [0x86, 0x64, b'e', b'n', b'g']));
        assert!(m.windows(5).any(|w| w == [0x85, 0x61, b'd', b'e', b'u']));
        let c = clpi(&Clip { streams: streams.to_vec(), packets: 100, start: 27_000_000, end: 27_090_090, ep: vec![] });
        assert!(c.windows(5).any(|w| w == [0x86, 0x64, b'e', b'n', b'g']));
        assert!(c.windows(5).any(|w| w == [0x85, 0x61, b'd', b'e', b'u']));
    }
}
