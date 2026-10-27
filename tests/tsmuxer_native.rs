//! The native BDAV muxer against tsMuxeR 2.7.0 (the oracle it replaced).
//!
//! `tests/fixtures/tsmuxer/<case>/` holds small inputs (`in.mkv`, `.sup` files) and what tsMuxeR
//! made of them: `clpi.bin`, `mpls.bin` and `m2ts.json` (a summary of the m2ts: PSI sections and,
//! per PID, the PES count, all PTS/DTS and the PES header bytes after the length field).
//! The tests mux the same inputs natively and require
//! * the playlist byte for byte,
//! * the clip info byte for byte except the packet count and the source packet numbers / end
//!   offsets of the EP map (they depend on how many bytes the video NAL units have: tsMuxeR also
//!   rewrote the SPS VUI), the EP map's PTS values are compared,
//! * the same summary of the m2ts.
//! The m2ts itself is checked on its own (packet structure, arrival times, PCR / PAT / PMT
//! cadence, seek points) and with ffprobe.
//!
//! Re-record with `TSMUXER_RECORD=bin/tsMuxeR.exe cargo test --test tsmuxer_native` (tsMuxeR
//! 2.7.0, github.com/justdan96/tsMuxer releases); `oracle.meta` of each case is the project file.
//! Skipped (with a message) when ffmpeg is missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use convertx_rust::bd_clip::Codec;
use convertx_rust::bd_mux::{create_skeleton, mux_title, MuxJob, SubSpec, VideoSpec};
use convertx_rust::tools::Toolchain;
use serde_json::{json, Value};

const PACKET: usize = 192;

struct Case {
    name: &'static str,
    /// Fixture directory holding the inputs.
    input: &'static str,
    fps: (u32, u32),
    width: u32,
    interlaced: bool,
    audio: &'static [(Codec, &'static str)],
    subs: &'static [(&'static str, &'static str)],
    chapters: &'static [f64],
    number: usize,
}

const CASES: &[Case] = &[
    Case { name: "p24_ac3", input: "p24_ac3", fps: (24000, 1001), width: 1920, interlaced: false, audio: &[(Codec::Ac3, "deu")], subs: &[], chapters: &[0.0, 0.5], number: 0 },
    Case {
        name: "i25_multi",
        input: "i25_multi",
        fps: (25, 1),
        width: 1920,
        interlaced: true,
        audio: &[(Codec::Ac3, "deu"), (Codec::Dts, "eng"), (Codec::Lpcm, "fra")],
        subs: &[("s1.sup", "deu"), ("s2.sup", "eng")],
        chapters: &[0.0, 0.7],
        number: 0,
    },
    Case { name: "p5994_lpcm51", input: "p5994_lpcm51", fps: (60000, 1001), width: 1280, interlaced: false, audio: &[(Codec::Lpcm, "eng")], subs: &[], chapters: &[], number: 0 },
    Case { name: "offset1", input: "p24_ac3", fps: (24000, 1001), width: 1920, interlaced: false, audio: &[(Codec::Ac3, "deu")], subs: &[], chapters: &[0.0, 0.5], number: 1 },
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tsmuxer")
}

fn tools() -> Option<(tokio::runtime::Runtime, Toolchain)> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(Toolchain::detect()) {
        Ok(t) => Some((rt, t)),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("convertx_tsmuxer_{}_{n}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lang(l: &str) -> [u8; 3] {
    l.as_bytes().try_into().unwrap()
}

fn job(c: &Case, dir: &Path) -> MuxJob {
    let input = fixtures().join(c.input);
    let bdmv = dir.join("BD").join("BDMV");
    create_skeleton(&bdmv).unwrap();
    MuxJob {
        mkv: input.join("in.mkv"),
        video: VideoSpec { fps: c.fps, width: c.width, interlaced: c.interlaced },
        audio: c.audio.iter().map(|&(codec, l)| (codec, lang(l))).collect(),
        subs: c.subs.iter().map(|&(f, l)| SubSpec { sup: input.join(f), lang: lang(l), ig: false }).collect(),
        chapters: c.chapters.to_vec(),
        number: c.number,
        still: false,
        bdmv,
        duration_secs: 2.0,
    }
}

fn mux(rt: &tokio::runtime::Runtime, tools: &Toolchain, c: &Case) -> PathBuf {
    let dir = scratch(c.name);
    let j = job(c, &dir);
    rt.block_on(mux_title(tools, &j)).unwrap_or_else(|e| panic!("{}: {e}", c.name));
    j.bdmv
}

// ---------------------------------------------------------------------------------------------
// Reading files
// ---------------------------------------------------------------------------------------------

fn be(d: &[u8], at: usize, n: usize) -> u64 {
    d[at..at + n].iter().fold(0, |v, &b| v << 8 | b as u64)
}

fn ts_time(b: &[u8]) -> u64 {
    ((b[0] as u64 >> 1) & 7) << 30 | be(b, 1, 2) >> 1 << 15 | be(b, 3, 2) >> 1
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Pkt<'a> {
    pid: u16,
    pusi: bool,
    cc: u8,
    /// Payload (after the adaptation field), empty without one.
    payload: &'a [u8],
    af: Option<&'a [u8]>,
    ats: u64,
}

fn packets(m2ts: &[u8]) -> Vec<Pkt<'_>> {
    assert_eq!(m2ts.len() % PACKET, 0);
    m2ts.chunks_exact(PACKET)
        .map(|p| {
            let t = &p[4..];
            assert_eq!(t[0], 0x47);
            let (afc, mut start) = ((t[3] >> 4) & 3, 4);
            let af = (afc & 2 != 0).then(|| {
                let af = &t[5..5 + t[4] as usize];
                start = 5 + t[4] as usize;
                af
            });
            Pkt { pid: (be(t, 1, 2) & 0x1FFF) as u16, pusi: t[1] & 0x40 != 0, cc: t[3] & 15, payload: if afc & 1 != 0 { &t[start..] } else { &[] }, af, ats: be(p, 0, 4) & 0x3FFF_FFFF }
        })
        .collect()
}

/// Summary of an m2ts as recorded in `m2ts.json`.
fn summarize(m2ts: &[u8]) -> Value {
    let pkts = packets(m2ts);
    let section = |pid: u16| {
        let p = pkts.iter().find(|p| p.pid == pid && p.pusi).expect("section present");
        let body = &p.payload[1 + p.payload[0] as usize..];
        let len = (be(body, 1, 2) & 0xFFF) as usize + 3;
        hex(&body[..len])
    };
    let mut streams = serde_json::Map::new();
    let mut pids: Vec<u16> = pkts.iter().map(|p| p.pid).filter(|&p| p >= 0x1000 && p != 0x1001 && p != 0x1FFF).collect();
    pids.sort_unstable();
    pids.dedup();
    for pid in pids {
        let starts: Vec<&Pkt> = pkts.iter().filter(|p| p.pid == pid && p.pusi).collect();
        let stamps: Vec<Value> = starts
            .iter()
            .map(|p| {
                let (flags, hl) = (p.payload[7], p.payload[8] as usize);
                let pts = (flags & 0x80 != 0).then(|| ts_time(&p.payload[9..]));
                let dts = (flags & 0x40 != 0).then(|| ts_time(&p.payload[14..]));
                json!([pts, dts, hl])
            })
            .collect();
        let first = starts[0].payload;
        streams.insert(format!("{pid:#06x}"), json!({ "pes": starts.len(), "stamps": stamps, "first_header": hex(&first[3..4]) + &hex(&first[6..9 + first[8] as usize]) }));
    }
    json!({ "pat": section(0), "pmt": section(0x100), "sit": section(0x1F), "streams": streams })
}

/// Zeroes what depends on the video bytes: packet count, EP map source packet numbers and end offsets.
fn mask_clpi(c: &mut [u8]) {
    c[56..60].fill(0);
    let cpi = be(c, 16, 4) as usize;
    let map = cpi + 6;
    for k in 0..c[map + 1] as usize {
        let e = map + 2 + k * 12;
        let word = be(c, e + 2, 6);
        let (coarse, fine) = ((word >> 18 & 0xFFFF) as usize, (word & 0x3FFFF) as usize);
        let block = map + be(c, e + 8, 4) as usize;
        let fine_at = block + be(c, block, 4) as usize;
        for i in 0..coarse {
            c[block + 4 + i * 8 + 4..block + 4 + i * 8 + 8].fill(0);
        }
        for i in 0..fine {
            let at = fine_at + i * 4;
            let v = (be(c, at, 4) as u32) & 0x8FFE_0000;
            c[at..at + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------------

fn chapter_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// The tsMuxeR project of a case (file names relative to the fixture directory).
fn oracle_meta(c: &Case) -> String {
    let mut m = String::from("MUXOPT --no-pcr-on-video-pid --new-audio-pes --blu-ray --vbr");
    if !c.chapters.is_empty() {
        m += &format!(" --custom-chapters={}", c.chapters.iter().map(|&s| chapter_time(s)).collect::<Vec<_>>().join(";"));
    }
    m += " --vbv-len=500";
    if c.number > 0 {
        m += &format!(" --mplsOffset={0} --m2tsOffset={0}", c.number);
    }
    m += "\nV_MPEG4/ISO/AVC, \"in.mkv\", insertSEI, contSPS, track=1\n";
    for (k, &(codec, l)) in c.audio.iter().enumerate() {
        let name = match codec {
            Codec::Ac3 => "A_AC3",
            Codec::Dts => "A_DTS",
            _ => "A_LPCM",
        };
        let l = match l {
            "deu" => "deu",
            other => other,
        };
        m += &format!("{name}, \"in.mkv\", track={}, lang={l}\n", 2 + k);
    }
    for &(f, l) in c.subs {
        m += &format!("S_HDMV/PGS, \"{f}\", lang={l}\n");
    }
    m
}

fn record(oracle: &Path) {
    for c in CASES {
        let dir = scratch(&format!("rec_{}", c.name));
        let src = fixtures().join(c.input);
        for f in std::iter::once("in.mkv").chain(c.subs.iter().map(|s| s.0)) {
            std::fs::copy(src.join(f), dir.join(f)).unwrap();
        }
        let meta = oracle_meta(c);
        std::fs::write(dir.join("oracle.meta"), &meta).unwrap();
        let out = Command::new(oracle).args(["oracle.meta", "BD"]).current_dir(&dir).output().expect("run tsMuxeR");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        let bdmv = dir.join("BD/BDMV");
        let n = c.number;
        let case = fixtures().join(c.name);
        std::fs::create_dir_all(&case).unwrap();
        std::fs::write(case.join("oracle.meta"), meta).unwrap();
        std::fs::copy(bdmv.join(format!("CLIPINF/{n:05}.clpi")), case.join("clpi.bin")).unwrap();
        std::fs::copy(bdmv.join(format!("PLAYLIST/{n:05}.mpls")), case.join("mpls.bin")).unwrap();
        let summary = summarize(&std::fs::read(bdmv.join(format!("STREAM/{n:05}.m2ts"))).unwrap());
        std::fs::write(case.join("m2ts.json"), serde_json::to_string_pretty(&summary).unwrap()).unwrap();
        eprintln!("recorded {}", c.name);
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn matches_tsmuxer() {
    if let Some(oracle) = std::env::var_os("TSMUXER_RECORD") {
        record(Path::new(&oracle));
    }
    let Some((rt, tools)) = tools() else { return };
    for c in CASES {
        let bdmv = mux(&rt, &tools, c);
        let want = fixtures().join(c.name);
        let n = c.number;

        let mpls = std::fs::read(bdmv.join(format!("PLAYLIST/{n:05}.mpls"))).unwrap();
        assert_eq!(hex(&mpls), hex(&std::fs::read(want.join("mpls.bin")).unwrap()), "{}: playlist", c.name);
        // BACKUP copies are identical.
        assert_eq!(mpls, std::fs::read(bdmv.join(format!("BACKUP/PLAYLIST/{n:05}.mpls"))).unwrap());

        let mut clpi = std::fs::read(bdmv.join(format!("CLIPINF/{n:05}.clpi"))).unwrap();
        assert_eq!(clpi, std::fs::read(bdmv.join(format!("BACKUP/CLIPINF/{n:05}.clpi"))).unwrap());
        let mut oracle = std::fs::read(want.join("clpi.bin")).unwrap();
        assert_eq!(clpi.len(), oracle.len(), "{}: clip info length", c.name);
        mask_clpi(&mut clpi);
        mask_clpi(&mut oracle);
        assert_eq!(hex(&clpi), hex(&oracle), "{}: clip info", c.name);

        let got = summarize(&std::fs::read(bdmv.join(format!("STREAM/{n:05}.m2ts"))).unwrap());
        let recorded: Value = serde_json::from_str(&std::fs::read_to_string(want.join("m2ts.json")).unwrap()).unwrap();
        for key in ["pat", "pmt", "sit"] {
            assert_eq!(got[key], recorded[key], "{}: {key}", c.name);
        }
        let (g, r) = (got["streams"].as_object().unwrap(), recorded["streams"].as_object().unwrap());
        assert_eq!(g.keys().collect::<Vec<_>>(), r.keys().collect::<Vec<_>>(), "{}: PIDs", c.name);
        for (pid, want) in r {
            assert_eq!(g[pid]["pes"], want["pes"], "{}: {pid} PES count", c.name);
            assert_eq!(g[pid]["stamps"], want["stamps"], "{}: {pid} time stamps", c.name);
            assert_eq!(g[pid]["first_header"], want["first_header"], "{}: {pid} PES header", c.name);
        }
    }
}

#[test]
fn m2ts_is_well_formed() {
    let Some((rt, tools)) = tools() else { return };
    for c in CASES {
        let bdmv = mux(&rt, &tools, c);
        let m2ts = std::fs::read(bdmv.join(format!("STREAM/{:05}.m2ts", c.number))).unwrap();
        let clpi = std::fs::read(bdmv.join(format!("CLIPINF/{:05}.clpi", c.number))).unwrap();
        // 6144-byte units.
        assert_eq!(m2ts.len() % 6144, 0, "{}", c.name);
        let pkts = packets(&m2ts);
        assert_eq!(be(&clpi, 56, 4) as usize, pkts.len(), "{}: packet count in the clip info", c.name);

        // Arrival times never go backwards (30-bit clock, wraps).
        for w in pkts.windows(2) {
            assert!(w[1].ats.wrapping_sub(w[0].ats) & 0x3FFF_FFFF < 1 << 29, "{}: ATS goes backwards", c.name);
        }
        // Continuity counters count packets with payload; PCR packets repeat the last one.
        let mut last: std::collections::HashMap<u16, u8> = Default::default();
        for p in &pkts {
            if p.pid == 0x1FFF {
                continue;
            }
            if let Some(&prev) = last.get(&p.pid) {
                let want = if p.payload.is_empty() { prev } else { (prev + 1) & 15 };
                assert_eq!(p.cc, want, "{}: continuity of PID {:#x}", c.name, p.pid);
            }
            if !p.payload.is_empty() || !last.contains_key(&p.pid) {
                last.insert(p.pid, p.cc);
            }
        }
        // PCR: ATS equals the PCR, at most 100 ms apart; PAT and PMT follow within 100 ms.
        let pcr_of = |af: &[u8]| (be(af, 1, 5) >> 7) * 300 + (be(af, 5, 2) & 0x1FF);
        let (mut last_pcr, mut last_psi) = (None::<u64>, None::<u64>);
        let mut pmt_gap = 0;
        for p in &pkts {
            match p.pid {
                0x1001 => {
                    let pcr = pcr_of(p.af.expect("PCR packet has an adaptation field"));
                    assert_eq!(pcr & 0x3FFF_FFFF, p.ats, "{}: PCR vs ATS", c.name);
                    if let Some(l) = last_pcr {
                        assert!(pcr > l && pcr - l <= 2_700_000, "{}: PCR interval {}", c.name, pcr.wrapping_sub(l));
                    }
                    if let Some(l) = last_psi {
                        pmt_gap = pmt_gap.max(pcr - l);
                    }
                    last_pcr = Some(pcr);
                }
                0x100 => last_psi = last_pcr,
                _ => {}
            }
        }
        // PCR, then PMT of the same instant: the longest wait for a PMT stays below 100 ms.
        assert!(pmt_gap <= 2_700_000 * 2, "{}: PMT interval", c.name);

        // Every EP map entry is the first packet of an IDR picture's PES on the video PID.
        let spns = ep_spns(&clpi);
        assert!(spns.len() >= 2, "{}: EP map entries", c.name);
        for spn in spns {
            let p = &pkts[spn];
            assert!(p.pid == 0x1011 && p.pusi, "{}: SPN {spn} is not a video PES start", c.name);
            let mut au = p.payload[9 + p.payload[8] as usize..].to_vec();
            for q in pkts[spn + 1..].iter().take_while(|q| !(q.pid == 0x1011 && q.pusi)) {
                if q.pid == 0x1011 {
                    au.extend(q.payload);
                }
            }
            assert!(has_idr(&au), "{}: SPN {spn} has no IDR picture", c.name);
        }
        let _ = std::fs::remove_dir_all(bdmv.parent().unwrap().parent().unwrap());
    }
}

/// Source packet numbers of the video EP map (from the fine table).
fn ep_spns(c: &[u8]) -> Vec<usize> {
    let map = be(c, 16, 4) as usize + 6;
    let e = map + 2;
    let word = be(c, e + 2, 6);
    let (coarse, fine) = ((word >> 18 & 0xFFFF) as usize, (word & 0x3FFFF) as usize);
    let block = map + be(c, e + 8, 4) as usize;
    let fine_at = block + be(c, block, 4) as usize;
    let coarse_spn = |i: usize| be(c, block + 4 + i * 8 + 4, 4) as usize;
    // Fine entries hold the low 17 bits; the coarse entry the rest.
    let mut at_coarse = 0;
    (0..fine)
        .map(|i| {
            while at_coarse + 1 < coarse && be(c, block + 4 + (at_coarse + 1) * 8, 4) >> 14 <= i as u64 {
                at_coarse += 1;
            }
            (coarse_spn(at_coarse) & !0x1FFFF) | (be(c, fine_at + i * 4, 4) & 0x1FFFF) as usize
        })
        .collect()
}

fn has_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i..i + 3] == [0, 0, 1] {
            match au[i + 3] & 31 {
                5 => return true,
                1 => return false,
                _ => {}
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[test]
fn ffprobe_reads_the_clips_like_the_input() {
    let Some((rt, tools)) = tools() else { return };
    let probe = |file: &Path| -> Vec<(String, u64)> {
        let out = rt
            .block_on(tools.ffprobe_output(&["-v".into(), "error".into(), "-count_frames".into(), "-show_entries".into(), "stream=codec_name,nb_read_frames,nb_frames".into(), "-of".into(), "compact".into(), file.into()]))
            .unwrap();
        out.lines()
            .filter_map(|l| l.split_once("stream|").map(|x| x.1))
            .map(|l| {
                let field = |k: &str| l.split('|').find_map(|f| f.strip_prefix(k)).unwrap_or_default();
                (field("codec_name=").to_string(), field("nb_read_frames=").parse::<u64>().unwrap_or(0))
            })
            .collect()
    };
    for c in CASES.iter().filter(|c| c.number == 0) {
        let bdmv = mux(&rt, &tools, c);
        let got = probe(&bdmv.join("STREAM/00000.m2ts"));
        let want = probe(&fixtures().join(c.input).join("in.mkv"));
        // Video: same picture count. Audio: same codecs (LPCM becomes pcm_bluray).
        let video = |v: &[(String, u64)]| v.iter().find(|s| s.0 == "h264").cloned();
        assert_eq!(video(&got), video(&want), "{}", c.name);
        let codecs = |v: &[(String, u64)]| {
            let mut names: Vec<String> = v.iter().map(|s| s.0.replace("pcm_s24le", "pcm_bluray")).filter(|n| n != "hdmv_pgs_subtitle").collect();
            names.sort();
            names.dedup();
            names
        };
        assert_eq!(codecs(&got), codecs(&want), "{}", c.name);
        // The null decode reports no errors.
        let log = rt.block_on(tools.ffmpeg_report(&["-v".into(), "error".into(), "-i".into(), bdmv.join("STREAM/00000.m2ts").into(), "-f".into(), "null".into(), "-".into()])).unwrap();
        assert!(!log.to_lowercase().contains("error"), "{}: {log}", c.name);
    }
}
