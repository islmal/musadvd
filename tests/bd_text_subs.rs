//! Text subtitles on Blu-ray: SRT cues rendered to a PGS `.sup` through the functions the BD
//! builder uses, then muxed natively. Regression for "invalid .sup format": a busy cue whose
//! object RLE exceeds one 64 KiB segment used to get a truncated segment length.
//! The mux step is skipped (with a message) when ffmpeg is missing.

use std::path::Path;

use musadvd::bd_clip::{Codec, PGS_PID};
use musadvd::bd_mux::{create_skeleton, mux_title, validate_sup, MuxJob, SubSpec, VideoSpec};
use musadvd::textsub::{apply_offset, parse_srt, Canvas, SubtitleStyle};
use musadvd::tools::Toolchain;

/// One plain cue and one huge cue: 8 lines of 30 characters, each in its own colour, at 200 %.
fn srt() -> String {
    let mut big = String::new();
    for line in 0..8 {
        for i in 0..30u32 {
            let c = line * 30 + i;
            big.push_str(&format!("<font color=\"#{:02x}{:02x}{:02x}\">{}</font>", (c * 37) % 256, (c * 91) % 256, (c * 53) % 256, ['W', 'M', '#', '@', '&'][c as usize % 5]));
        }
        big.push('\n');
    }
    format!("1\n00:00:00,200 --> 00:00:00,800\nHello <i>world</i>\n\n2\n00:00:01,000 --> 00:00:01,800\n{big}\n")
}

fn sup() -> Vec<u8> {
    let (cues, skipped) = parse_srt(&srt());
    assert_eq!((cues.len(), skipped), (2, 0));
    let st = SubtitleStyle { font_size_pct: Some(200.0), lang: Some("eng".into()), ..Default::default() };
    let cues = apply_offset(cues, st.offset_ms);
    musadvd::pgs::build_sup(&cues, &Canvas::hd(1920, 1080).with_style(&st), (24000, 1001))
}

/// Sequence flags of every ODS segment.
fn ods_flags(sup: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 13 <= sup.len() {
        let len = u16::from_be_bytes([sup[i + 11], sup[i + 12]]) as usize;
        if sup[i + 10] == 0x15 {
            v.push(sup[i + 16]);
        }
        i += 13 + len;
    }
    v
}

#[test]
fn large_cue_is_fragmented_and_parses() {
    let sup = sup();
    validate_sup(&sup).unwrap();
    // The plain cue fits one segment; the huge one is split (first 0x80 … last 0x40).
    let flags = ods_flags(&sup);
    assert_eq!(flags.first(), Some(&0xC0));
    assert_eq!(flags[1], 0x80, "huge cue is fragmented: {flags:02x?}");
    assert_eq!(flags.last(), Some(&0x40));
    let decoded = musadvd::subpic::decode_pgs(&sup).unwrap();
    assert_eq!(decoded.pictures.len(), 2);
}

#[test]
fn large_cue_muxes_into_bdmv() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = match rt.block_on(Toolchain::detect()) {
        Ok(t) => t,
        Err(e) => return eprintln!("skipping: {e}"),
    };
    let dir = std::env::temp_dir().join(format!("convertx_bd_text_subs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sup_path = dir.join("s1.sup");
    std::fs::write(&sup_path, sup()).unwrap();
    let bdmv = dir.join("BD").join("BDMV");
    create_skeleton(&bdmv).unwrap();
    let input = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tsmuxer/p24_ac3");
    let job = MuxJob {
        mkv: input.join("in.mkv"),
        video: VideoSpec { fps: (24000, 1001), width: 1920, interlaced: false, copy_vbv: None },
        audio: vec![(Codec::Ac3, *b"deu")],
        subs: vec![SubSpec { sup: sup_path, lang: *b"jpn", ig: false }],
        chapters: vec![0.0],
        number: 0,
        still: false,
        bdmv: bdmv.clone(),
        duration_secs: 2.0,
    };
    rt.block_on(mux_title(&tools, &job)).unwrap();

    let m2ts = std::fs::read(bdmv.join("STREAM/00000.m2ts")).unwrap();
    let pg_packets = m2ts.chunks(192).filter(|p| u16::from_be_bytes([p[5], p[6]]) & 0x1FFF == PGS_PID).count();
    // Both cues and their clear sets; the huge object alone needs hundreds of packets.
    assert!(pg_packets > 400, "PGS packets: {pg_packets}");
    // Clip info: the PG stream entry (PID, coding info length, type 0x90, language).
    let clpi = std::fs::read(bdmv.join("CLIPINF/00000.clpi")).unwrap();
    let pid = PGS_PID.to_be_bytes();
    assert!(
        clpi.windows(7).any(|w| w[..2] == pid && w[3] == 0x90 && &w[4..7] == b"jpn"),
        "clpi lists the PG stream"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
