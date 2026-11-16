//! Bitmap subtitles end to end with the real tools:
//! - PGS: a small `.sup` written here is muxed into Matroska by ffmpeg, converted by the build
//!   and must come out as a DVD subpicture stream at the right time and place (checked against
//!   ffmpeg's own rendering of the source).
//! - VobSub: a DVD built from an SRT (natively rendered subpictures) is used as the input of a
//!   second build; every subtitle must survive with its timing.
//! Skipped (with a message) when tools or templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use musadvd::dvd::{self, BuildOptions, BuildReport};
use musadvd::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn setup(name: &str) -> Option<(tokio::runtime::Runtime, Toolchain, PathBuf)> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = match rt.block_on(Toolchain::detect()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: {e}");
            return None;
        }
    };
    if !Path::new(TEMPLATE).is_file() {
        eprintln!("skipping: {TEMPLATE} not found");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("cx_bitmapsubs_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Some((rt, tools, dir))
}

fn ffmpeg(tools: &Toolchain, args: &[&str]) {
    let st = Command::new(&tools.ffmpeg).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

fn build(rt: &tokio::runtime::Runtime, input: PathBuf, out: PathBuf) -> BuildReport {
    let opts = BuildOptions {
        inputs: vec![input],
        template: TEMPLATE.into(),
        output: out,
        keep_work: true,
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        ..BuildOptions::default()
    };
    rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"))
}

/// `pts_time` of the subtitle packets of `file` (absolute) and the file's start time.
fn subtitle_packets(tools: &Toolchain, file: &Path) -> (Vec<f64>, f64) {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "s:0", "-show_entries", "packet=pts_time:format=start_time", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let nums: Vec<f64> = text.lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
    let (start, packets) = nums.split_last().expect("start time");
    (packets.to_vec(), *start)
}

/// One RGB pixel of a frame rendered by ffmpeg at `secs` (absolute time) – with the first
/// subtitle stream overlaid when `overlay`.
fn pixel(tools: &Toolchain, file: &Path, secs: f64, overlay: bool, x: u32, y: u32, png: &Path) -> [u8; 3] {
    let graph = if overlay { "[0:v][0:s:0]overlay," } else { "[0:v]" };
    let graph = format!("{graph}trim=start={secs},setpts=PTS-STARTPTS,scale=720:576");
    ffmpeg(tools, &["-i", file.to_str().unwrap(), "-filter_complex", &graph, "-frames:v", "1", "-update", "1", png.to_str().unwrap()]);
    let img = image::open(png).unwrap().to_rgb8();
    img.get_pixel(x, y).0
}

// ---- a minimal PGS writer --------------------------------------------------------------------

fn segment(pts: f64, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut s = b"PG".to_vec();
    s.extend(((pts * 90_000.0) as u32).to_be_bytes());
    s.extend(0u32.to_be_bytes());
    s.push(kind);
    s.extend((payload.len() as u16).to_be_bytes());
    s.extend_from_slice(payload);
    s
}

/// PGS run-length code of `n` pixels of palette entry `c`.
fn run(out: &mut Vec<u8>, n: usize, c: u8) {
    match (c, n) {
        (_, 0) => {}
        (0, n) if n < 64 => out.extend([0, n as u8]),
        (0, n) => out.extend([0, 0x40 | (n >> 8) as u8, n as u8]),
        (c, n) if n < 3 => out.extend(std::iter::repeat_n(c, n)),
        (c, n) if n < 64 => out.extend([0, 0x80 | n as u8, c]),
        (c, n) => out.extend([0, 0xC0 | (n >> 8) as u8, n as u8, c]),
    }
}

/// A `w`×`h` box: white (entry 1) inside a 4-pixel black (entry 2) border.
fn box_rle(w: usize, h: usize) -> Vec<u8> {
    let mut rle = Vec::new();
    for y in 0..h {
        if y < 4 || y >= h - 4 {
            run(&mut rle, w, 2);
        } else {
            run(&mut rle, 4, 2);
            run(&mut rle, w - 8, 1);
            run(&mut rle, 4, 2);
        }
        rle.extend([0, 0]);
    }
    rle
}

/// Display set showing object 1 (`w`×`h`) at (`x`, `y`) on a 1280×720 canvas from `pts`.
fn show(pts: f64, x: u16, y: u16, w: u16, h: u16, forced: bool, composition: u16) -> Vec<u8> {
    let mut pcs = vec![0x05, 0x00, 0x02, 0xD0, 0x10];
    pcs.extend(composition.to_be_bytes());
    pcs.extend([0x80, 0, 0, 1]);
    pcs.extend(1u16.to_be_bytes());
    pcs.extend([0, if forced { 0x40 } else { 0 }]);
    pcs.extend(x.to_be_bytes());
    pcs.extend(y.to_be_bytes());
    let mut wds = vec![1, 0];
    for v in [x, y, w, h] {
        wds.extend(v.to_be_bytes());
    }
    // Palette 0: 1 = white, 2 = black (BT.709 limited range).
    let pds = [0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 255];
    let rle = box_rle(w as usize, h as usize);
    let mut ods = vec![0, 1, 0, 0xC0];
    ods.extend(((rle.len() + 4) as u32).to_be_bytes()[1..].to_vec());
    ods.extend(w.to_be_bytes());
    ods.extend(h.to_be_bytes());
    ods.extend(rle);
    [segment(pts, 0x16, &pcs), segment(pts, 0x17, &wds), segment(pts, 0x14, &pds), segment(pts, 0x15, &ods), segment(pts, 0x80, &[])]
        .concat()
}

/// Display set clearing the screen at `pts`.
fn clear(pts: f64, composition: u16) -> Vec<u8> {
    let mut pcs = vec![0x05, 0x00, 0x02, 0xD0, 0x10];
    pcs.extend(composition.to_be_bytes());
    pcs.extend([0x00, 0, 0, 0]);
    [segment(pts, 0x16, &pcs), segment(pts, 0x17, &[0]), segment(pts, 0x80, &[])].concat()
}

#[test]
fn pgs_subtitles_become_dvd_subpictures() {
    let Some((rt, tools, dir)) = setup("pgs") else { return };
    let clip = dir.join("clip.mkv");
    ffmpeg(&tools, &["-f", "lavfi", "-i", "color=c=0x406080:size=1280x720:rate=25:duration=5", "-f", "lavfi", "-i", "sine=f=440:duration=5", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", clip.to_str().unwrap()]);
    let sup = dir.join("subs.sup");
    let data = [show(1.0, 480, 600, 320, 60, false, 0), clear(2.5, 1), show(3.0, 100, 100, 200, 40, true, 2), clear(3.6, 3)].concat();
    std::fs::write(&sup, data).unwrap();
    let source = dir.join("pgs.mkv");
    // -copyts: keep the subtitle times (ffmpeg would otherwise shift the .sup to its first packet).
    ffmpeg(&tools, &["-copyts", "-i", clip.to_str().unwrap(), "-i", sup.to_str().unwrap(), "-map", "0", "-map", "1", "-c", "copy", source.to_str().unwrap()]);

    let report = build(&rt, source.clone(), dir.join("out"));
    assert_eq!(report.titles[0].subtitles.len(), 1, "{:?}", report.titles[0].subtitles);
    let work = report.work_dir.clone().expect("kept work dir");
    let xml = std::fs::read_to_string(work.join("title_01_s1.xml")).unwrap();
    assert!(xml.contains(r#"start="00:00:01.00" end="00:00:02.50""#), "{xml}");
    assert!(xml.contains(r#"start="00:00:03.00" end="00:00:03.60""#) && xml.contains(r#"force="yes""#), "{xml}");
    let log = std::fs::read_to_string(work.join("title_01_s1.xml.log")).unwrap();
    assert!(log.contains("2 subtitles added, 0 subtitles skipped"), "{log}");
    assert!(!work.join("title_01_s1.sup").exists(), "extracted stream removed");

    // The DVD title carries the subpictures 1 s and 3 s after its start.
    let vob = report.video_ts.join("VTS_01_1.VOB");
    let (packets, start) = subtitle_packets(&tools, &vob);
    assert_eq!(packets.len(), 2, "{packets:?}");
    assert!((packets[0] - start - 1.0).abs() < 0.1 && (packets[1] - start - 3.0).abs() < 0.1, "{packets:?} (start {start})");

    // Same place as ffmpeg's own rendering of the PGS source: the white box centre at
    // (640, 630) of 1280×720 → (360, 504) in the 16:9 PAL frame; the background stays blue.
    let png = dir.join("frame.png");
    let src = pixel(&tools, &source, 1.5, true, 360, 504, &png);
    let dvd = pixel(&tools, &vob, start + 1.5, true, 360, 504, &png);
    let plain = pixel(&tools, &vob, start + 1.5, false, 360, 504, &png);
    assert!(src.iter().all(|&c| c > 200), "ffmpeg renders the source box white: {src:?}");
    assert!(dvd.iter().all(|&c| c > 160), "subpicture visible on the DVD: {dvd:?} (without: {plain:?})");
    assert!(plain[2] > plain[0], "no subtitle without the overlay: {plain:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn vobsub_subtitles_survive_a_second_build() {
    let Some((rt, tools, dir)) = setup("vobsub") else { return };
    let srt = dir.join("subs.srt");
    std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nErste Zeile\n\n2\n00:00:02,500 --> 00:00:03,500\nZweite Zeile\n\n3\n00:00:04,000 --> 00:00:04,800\nDritte\n").unwrap();
    let clip = dir.join("clip.mkv");
    ffmpeg(&tools, &["-f", "lavfi", "-i", "testsrc2=size=720x576:rate=25:duration=6", "-f", "lavfi", "-i", "sine=f=440:duration=6", "-i", srt.to_str().unwrap(), "-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-c:s", "srt", clip.to_str().unwrap()]);
    // First build: the native renderer turns the SRT into DVD subpictures (VobSub).
    let first = build(&rt, clip, dir.join("first"));
    let vob = first.video_ts.join("VTS_01_1.VOB");
    let (original, original_start) = subtitle_packets(&tools, &vob);
    assert_eq!(original.len(), 3);

    // Second build from that VOB: the VobSub stream is decoded and re-rendered.
    let second = build(&rt, vob, dir.join("second"));
    assert_eq!(second.titles[0].subtitles.len(), 1);
    let work = second.work_dir.clone().expect("kept work dir");
    let log = std::fs::read_to_string(work.join("title_01_s1.xml.log")).unwrap();
    assert!(log.contains("3 subtitles added, 0 subtitles skipped"), "{log}");
    let xml = std::fs::read_to_string(work.join("title_01_s1.xml")).unwrap();
    // Relative to the title start (spumux puts subpictures on frame boundaries, ±1 frame).
    assert!(xml.contains(r#"start="00:00:01.0"#) && xml.contains(r#"start="00:00:04.0"#), "{xml}");
    let (again, again_start) = subtitle_packets(&tools, &second.video_ts.join("VTS_01_1.VOB"));
    assert_eq!(again.len(), 3);
    for (a, b) in original.iter().zip(&again) {
        assert!(((a - original_start) - (b - again_start)).abs() < 0.1, "{original:?} vs {again:?}");
    }
    std::fs::remove_dir_all(&dir).ok();
}
