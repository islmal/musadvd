//! Reduced title resolutions and encoding profiles end to end: Half D1 (PAL, wide source →
//! 4:3 letterbox), SIF (NTSC), and LP switching a Full D1 project to Half D1 at a low bitrate.
//! Titles must come out at the reduced size, menus stay Full D1, dvdauthor accepts the mix.
//! Skipped (with a message) when tools or templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use musadvd::author::{Resolution, TvStandard};
use musadvd::calculator::{DiscType, EncodingProfile};
use musadvd::dvd::{self, BuildOptions, BuildReport};
use musadvd::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn setup(name: &str, size: &str, secs: u32) -> Option<(tokio::runtime::Runtime, Toolchain, PathBuf, PathBuf)> {
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
    let dir = std::env::temp_dir().join(format!("cx_res_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("clip.mkv");
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", &format!("testsrc2=size={size}:rate=25:duration={secs}")])
        .args(["-f", "lavfi", "-i", &format!("sine=f=440:duration={secs}")])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(st.success());
    Some((rt, tools, dir, clip))
}

/// `width,height,display_aspect_ratio` of the first video stream.
fn video_format(tools: &Toolchain, file: &Path) -> String {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,display_aspect_ratio", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().trim_end_matches(',').to_string()
}

fn build(rt: &tokio::runtime::Runtime, opts: &BuildOptions) -> (BuildReport, Vec<String>) {
    let mut steps = Vec::new();
    let report = rt
        .block_on(dvd::build_dvd(opts, &mut |e| {
            if let dvd::Event::Step(s) = e {
                steps.push(s);
            }
        }))
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    (report, steps)
}

#[test]
fn half_d1_pal_wide_source_becomes_4_3() {
    let Some((rt, tools, dir, clip)) = setup("half", "1280x720", 4) else { return };
    let opts = BuildOptions {
        inputs: vec![clip],
        template: TEMPLATE.into(),
        output: dir.join("out"),
        resolution: Resolution::HalfD1,
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let (report, steps) = build(&rt, &opts);
    assert_eq!(report.resolution, Resolution::HalfD1);
    assert!(report.warnings.iter().any(|w| w.contains("allows 4:3 only")), "{:?}", report.warnings);
    assert!(steps.iter().any(|s| s.contains("Half D1 352×576")), "{steps:?}");
    assert_eq!(report.titles[0].aspect.as_str(), "4:3");
    assert!(report.authoring_xml.contains(r#"<video format="pal" aspect="4:3"/>"#), "{}", report.authoring_xml);
    assert_eq!(video_format(&tools, &report.video_ts.join("VTS_01_1.VOB")), "352,576,4:3");
    // Menus keep the full frame.
    assert!(video_format(&tools, &report.video_ts.join("VIDEO_TS.VOB")).starts_with("720,576"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sif_ntsc() {
    let Some((rt, tools, dir, clip)) = setup("sif", "640x480", 4) else { return };
    let opts = BuildOptions {
        inputs: vec![clip],
        template: TEMPLATE.into(),
        output: dir.join("out"),
        tv_standard: TvStandard::Ntsc,
        resolution: Resolution::Sif,
        encoding_profile: EncodingProfile::Mp,
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let (report, steps) = build(&rt, &opts);
    assert_eq!(report.resolution, Resolution::Sif);
    assert!(report.warnings.iter().all(|w| !w.contains("erlaubt nur 4:3")), "4:3 source: no warning");
    assert_eq!(report.plan.video_kbps, 6000, "MP ceiling on a short project");
    assert!(steps.iter().any(|s| s.contains("profile MP ≤ 6000 kbps")), "{steps:?}");
    assert_eq!(video_format(&tools, &report.video_ts.join("VTS_01_1.VOB")), "352,240,4:3");
    assert!(video_format(&tools, &report.video_ts.join("VIDEO_TS.VOB")).starts_with("720,480"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lp_switches_full_d1_to_half_d1_on_a_small_target() {
    // 40 s on an 13 MB target leave roughly 1500–2000 kbps video (menus included): LP → Half D1.
    let Some((rt, tools, dir, clip)) = setup("lp", "720x576", 40) else { return };
    let opts = BuildOptions {
        inputs: vec![clip],
        disc: DiscType::Custom(13),
        template: TEMPLATE.into(),
        output: dir.join("out"),
        encoding_profile: EncodingProfile::Lp,
        chapters: dvd::ChapterSettings { interval_secs: 20.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let (report, steps) = build(&rt, &opts);
    let kbps = report.plan.video_kbps;
    assert!((1000..3000).contains(&kbps), "test setup must land below 3000 kbps: {kbps}");
    assert_eq!(report.resolution, Resolution::HalfD1);
    assert!(steps.iter().any(|s| s.starts_with("LP profile: below 3000 kbps")), "{steps:?}");
    assert_eq!(video_format(&tools, &report.video_ts.join("VTS_01_1.VOB")), "352,576,4:3");
    std::fs::remove_dir_all(&dir).ok();
}
