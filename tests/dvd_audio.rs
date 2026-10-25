//! DVD audio and editing options end to end: compliant AC-3/DTS is copied into the VOB (and accepted
//! by dvdauthor), LPCM mode falls back to AC-3 on DVD, a trimmed section has the right length, and
//! a crop changes the title's aspect. Skipped (with a message) when the tools or the ConvertX
//! templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use convertx_rust::calculator::DiscType;
use convertx_rust::dvd::{self, BuildOptions, BuildReport, Event, StreamSelection};
use convertx_rust::encoder::AudioMode;
use convertx_rust::probe::Crop;
use convertx_rust::tools::Toolchain;

const TEMPLATE: &str = r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn setup() -> Option<(tokio::runtime::Runtime, Toolchain)> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(Toolchain::detect()) {
        Ok(t) if Path::new(TEMPLATE).is_file() => Some((rt, t)),
        Ok(_) => {
            eprintln!("skipping: {TEMPLATE} not found");
            None
        }
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn ffmpeg(tools: &Toolchain, args: &[&str]) {
    let st = Command::new(&tools.ffmpeg).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

/// A 12 s 1280×720 clip with one 48 kHz audio track (`stereo` or `5.1`) in `codec` at `kbps`.
fn clip(tools: &Toolchain, dir: &Path, name: &str, codec: &str, layout: &str, kbps: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    let sine = format!("sine=f=440:d=12:sample_rate=48000,aformat=channel_layouts={layout}");
    ffmpeg(
        tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=12", "-f", "lavfi", "-i", &sine, "-c:v", "libx264", "-preset", "ultrafast", "-c:a",
            codec, "-strict", "-2", "-b:a", kbps, "-metadata:s:a:0", "language=ger", path.to_str().unwrap(),
        ],
    );
    path
}

fn build(rt: &tokio::runtime::Runtime, dir: &Path, name: &str, input: PathBuf, mode: AudioMode, sel: StreamSelection) -> (BuildReport, Vec<String>) {
    let opts = BuildOptions {
        inputs: vec![input],
        disc: DiscType::Dvd5,
        template: PathBuf::from(TEMPLATE),
        output: dir.join(name),
        fast_encode: true,
        no_title_menu_for_single_title: true,
        audio_mode: mode,
        selections: vec![sel],
        ..BuildOptions::default()
    };
    let mut warnings = Vec::new();
    let report = rt
        .block_on(dvd::build_dvd(&opts, &mut |e| {
            if let Event::Warning(w) = e {
                warnings.push(w);
            }
        }))
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    (report, warnings)
}

/// `codec,bit_rate,channels` of the audio streams in the title VOB.
fn audio_of(tools: &Toolchain, report: &BuildReport) -> Vec<String> {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=codec_name,bit_rate,channels", "-of", "csv=p=0"])
        .arg(report.video_ts.join("VTS_01_1.VOB"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).collect()
}

fn duration(tools: &Toolchain, report: &BuildReport) -> f64 {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(report.video_ts.join("VTS_01_1.VOB"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn only_audio() -> StreamSelection {
    StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), ..StreamSelection::default() }
}

#[test]
fn compliant_audio_is_copied_into_the_vob() {
    let Some((rt, tools)) = setup() else { return };
    let dir = std::env::temp_dir().join(format!("cx_dvdaudio_copy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ac3 = clip(&tools, &dir.join("clips"), "ac3.mkv", "ac3", "stereo", "256k");
    let dts = clip(&tools, &dir.join("clips"), "dts.mkv", "dca", "5.1", "768k");

    // Stereo AC-3 256 kbps: copied (same bitrate), dvdauthor accepts it.
    let (r, _) = build(&rt, &dir, "ac3", ac3.clone(), AudioMode::Auto, only_audio());
    let a = audio_of(&tools, &r);
    assert_eq!(a.len(), 1);
    assert!(a[0].starts_with("ac3,2,256000"), "copied, not re-encoded at 192k: {a:?}");
    assert!(r.authoring_xml.contains(r#"<audio format="ac3" lang="de"/>"#), "{}", r.authoring_xml);
    assert_eq!(r.titles[0].audio, vec!["Deutsch Stereo"]);

    // "Always AC-3" re-encodes (stereo -> 192 kbps).
    let (r, _) = build(&rt, &dir, "ac3b", ac3.clone(), AudioMode::Ac3, only_audio());
    assert!(audio_of(&tools, &r)[0].starts_with("ac3,2,192000"));

    // DTS core 5.1: copied, authored as dts.
    let (r, _) = build(&rt, &dir, "dts", dts, AudioMode::Auto, only_audio());
    let a = audio_of(&tools, &r);
    assert!(a[0].starts_with("dts,6,"), "{a:?}");
    assert!(r.authoring_xml.contains(r#"<audio format="dts" lang="de"/>"#), "{}", r.authoring_xml);
    assert!(r.plan.audio_kbps >= 700, "the real DTS bitrate enters the size plan: {}", r.plan.audio_kbps);

    // LPCM on DVD: ffmpeg's muxer cannot write usable DVD LPCM, so the title gets AC-3 and a warning.
    let (r, warnings) = build(&rt, &dir, "lpcm", ac3, AudioMode::Lpcm, only_audio());
    assert!(audio_of(&tools, &r)[0].starts_with("ac3,2,192000"));
    assert!(warnings.iter().any(|w| w.contains("LPCM")), "{warnings:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn trim_cuts_the_title_to_the_section() {
    let Some((rt, tools)) = setup() else { return };
    let dir = std::env::temp_dir().join(format!("cx_dvdaudio_trim_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = clip(&tools, &dir.join("clips"), "src.mkv", "ac3", "stereo", "192k");
    let sel = StreamSelection { start_secs: Some(2.0), end_secs: Some(8.0), ..only_audio() };
    let (r, _) = build(&rt, &dir, "trim", src.clone(), AudioMode::Auto, sel);
    let d = duration(&tools, &r);
    assert!((d - 6.0).abs() < 0.15, "6 s section, got {d}");
    assert!((r.titles[0].duration_secs - 6.0).abs() < 1e-6);
    // A bad section is rejected before any encoding.
    let opts = BuildOptions {
        inputs: vec![src],
        template: PathBuf::from(TEMPLATE),
        output: dir.join("bad"),
        selections: vec![StreamSelection { start_secs: Some(9.0), end_secs: Some(30.0), ..only_audio() }],
        ..BuildOptions::default()
    };
    let err = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_err().to_string();
    assert!(err.contains("Schnitt"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn crop_changes_the_aspect_of_the_title() {
    let Some((rt, tools)) = setup() else { return };
    let dir = std::env::temp_dir().join(format!("cx_dvdaudio_crop_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = clip(&tools, &dir.join("clips"), "src.mkv", "ac3", "stereo", "192k");
    let (plain, _) = build(&rt, &dir, "plain", src.clone(), AudioMode::Auto, only_audio());
    assert_eq!(plain.titles[0].aspect.as_str(), "16:9");
    // 160 px off each side of the 1280×720 picture leaves 960×720 = 4:3.
    let sel = StreamSelection { crop: Some(Crop { top: 0, bottom: 0, left: 160, right: 160 }), ..only_audio() };
    let (r, _) = build(&rt, &dir, "crop", src.clone(), AudioMode::Auto, sel);
    assert_eq!(r.titles[0].aspect.as_str(), "4:3");
    // Black bars are found by crop detection (here: the top/bottom of a letterboxed clip).
    let boxed = dir.join("clips").join("boxed.mkv");
    ffmpeg(
        &tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=8", "-vf", "drawbox=x=0:y=0:w=1280:h=90:color=black:t=fill,drawbox=x=0:y=630:w=1280:h=90:color=black:t=fill",
            "-c:v", "libx264", "-preset", "ultrafast", boxed.to_str().unwrap(),
        ],
    );
    let info = rt.block_on(convertx_rust::probe::probe(&tools.ffprobe, &boxed)).unwrap();
    let crop = rt.block_on(convertx_rust::encoder::detect_crop(&tools, &info)).unwrap();
    assert!((88..=92).contains(&crop.top) && (88..=92).contains(&crop.bottom), "{crop:?}");
    assert!(crop.left <= 2 && crop.right <= 2, "{crop:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Integrated loudness (LUFS) of the first audio stream of `file`, measured with loudnorm.
fn loudness(tools: &Toolchain, file: &Path) -> f64 {
    let out = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(file)
        .args(["-map", "0:a:0", "-af", "loudnorm=print_format=json", "-f", "null", "-"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    convertx_rust::encoder::parse_loudnorm_report(&log).unwrap_or_else(|| panic!("no loudnorm report in {log}")).integrated()
}

#[test]
fn loudnorm_brings_the_title_to_minus_23_lufs() {
    let Some((rt, tools)) = setup() else { return };
    let dir = std::env::temp_dir().join(format!("cx_dvdaudio_loud_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("clips")).unwrap();
    // A quiet 1 kHz tone with a speech-like envelope (about -38 LUFS).
    let src = dir.join("clips").join("quiet.mkv");
    ffmpeg(
        &tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=12", "-f", "lavfi", "-i",
            "sine=f=1000:d=12:sample_rate=48000,volume=0.02,aformat=channel_layouts=stereo", "-c:v", "libx264", "-preset", "ultrafast",
            "-c:a", "ac3", "-b:a", "192k", src.to_str().unwrap(),
        ],
    );
    let before = loudness(&tools, &src);
    assert!(before < -30.0, "{before}");
    let opts = BuildOptions {
        inputs: vec![src],
        disc: DiscType::Dvd5,
        template: PathBuf::from(TEMPLATE),
        output: dir.join("out"),
        fast_encode: true,
        no_title_menu_for_single_title: true,
        loudnorm: true,
        selections: vec![only_audio()],
        ..BuildOptions::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    let after = loudness(&tools, &report.video_ts.join("VTS_01_1.VOB"));
    assert!((after + 23.0).abs() < 2.0, "{before} LUFS became {after} LUFS");
    // Normalised tracks are re-encoded even though a stereo AC-3 would have been copied.
    assert!(audio_of(&tools, &report)[0].starts_with("ac3,2,192000"));
    std::fs::remove_dir_all(&dir).ok();
}
