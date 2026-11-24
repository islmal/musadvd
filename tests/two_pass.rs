//! 1-pass vs. 2-pass encoding: argument construction, real ffmpeg runs, and log cleanup.
//! The tests that run ffmpeg are skipped (with a message) when the tools are missing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use musadvd::author::TvStandard;
use musadvd::calculator::DiscType;
use musadvd::dvd::{self, BuildOptions};
use musadvd::encoder::{self, Aspect, EncodeJob, EncodePhase, MenuAudio, NULL_DEVICE};
use musadvd::menu::UiLang;
use musadvd::probe::{self, AudioStream, MediaInfo, VideoStream};
use musadvd::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn fake_info() -> MediaInfo {
    MediaInfo {
        path: "in.mkv".into(),
        duration_secs: 10.0,
        video: Some(VideoStream {
            index: 0,
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            display_aspect: 16.0 / 9.0,
            fps: 25.0,
            field_order: "progressive".into(),
            crop: None,
            hdr: false,
            profile: None,
            level: None,
            pix_fmt: String::new(),
            bit_rate: None,
        }),
        audio: vec![AudioStream {
            index: 1,
            codec: "aac".into(),
            channels: 2,
            channel_layout: "stereo".into(),
            language: None,
            title: None,
            default: true,
            bit_rate: None,
            sample_rate: None,
            profile: None,
            bits_per_raw_sample: None,
        }],
        subtitles: vec![],
        chapters: vec![],
        trim: None,
        audio_names: Default::default(),
    }
}

fn job<'a>(info: &'a MediaInfo, dir: &Path, two_pass: bool) -> EncodeJob<'a> {
    EncodeJob {
        info,
        audio: info.audio.iter().collect(),
        subtitles: vec![],
        video_kbps: 6000,
        aspect: Aspect::for_source(info.video.as_ref().unwrap().display_aspect),
        dir: dir.to_path_buf(),
        stem: "title_01".into(),
        two_pass,
        fast: false,
        tv: TvStandard::Pal,
        resolution: Default::default(),
        video: Default::default(),
        hwaccel: None,
        audio_mode: Default::default(),
        audio_modes: vec![],
        audio_filters: vec![],
        ac3_kbps: 0,
        copy_video: false,
        subtitle_style: Default::default(),
    }
}

/// Index of `needle` followed by `next` in `args`.
fn pair(args: &[String], needle: &str, next: &str) -> bool {
    args.windows(2).any(|w| w[0] == needle && w[1] == next)
}

#[test]
fn first_pass_is_video_only_to_null_device() {
    let info = fake_info();
    let j = job(&info, Path::new("w"), true);
    let a = strings(&j.first_pass_args());
    let prefix = Path::new("w").join("title_01_2pass").to_string_lossy().into_owned();

    assert!(pair(&a, "-pass", "1"), "{a:?}");
    assert!(pair(&a, "-passlogfile", &prefix), "{a:?}");
    assert!(a.contains(&"-an".to_string()));
    assert!(pair(&a, "-f", "rawvideo"));
    assert_eq!(a.last().unwrap(), NULL_DEVICE);
    assert_eq!(NULL_DEVICE, if cfg!(windows) { "NUL" } else { "/dev/null" });
    // No audio encoding and no real output files in the analysis run.
    assert!(!a.iter().any(|x| x == "ac3" || x == "dvd" || x.ends_with(".mpg")), "{a:?}");
    // Same rate control as the final run, otherwise the statistics do not apply.
    assert!(pair(&a, "-b:v", "6000k"));
    assert!(pair(&a, "-c:v", "mpeg2video"));
    // Pass 1 uses fast statistics flags regardless of `fast`, while keeping GOP/rate-control intact.
    for (flag, val) in [
        ("-mbd", "simple"),
        ("-trellis", "0"),
        ("-dia_size", "1"),
        ("-subq", "1"),
        ("-cmp", "sad"),
        ("-subcmp", "sad"),
        ("-mbcmp", "sad"),
        ("-g", "12"),
        ("-bf", "2"),
        ("-flags", "+cgop"),
        ("-sc_threshold", "1000000000"),
        ("-maxrate", "9588k"),
        ("-bufsize", "1835k"),
    ] {
        assert!(pair(&a, flag, val), "expected {flag} {val} in {a:?}");
    }
    assert!(!pair(&a, "-mbd", "rd"), "{a:?}");
    assert!(!pair(&a, "-trellis", "1"), "{a:?}");
    assert!(!a.iter().any(|x| x == "-intra_vlc" || x == "-motion_est"), "{a:?}");
}

#[test]
fn second_pass_reads_log_and_writes_audio() {
    let info = fake_info();
    let a = strings(&job(&info, Path::new("w"), true).encode_args("out.mpg"));
    let prefix = Path::new("w").join("title_01_2pass").to_string_lossy().into_owned();
    assert!(pair(&a, "-pass", "2"), "{a:?}");
    assert!(pair(&a, "-passlogfile", &prefix));
    assert!(pair(&a, "-c:a:0", "ac3"), "AC-3 audio is written in pass 2");
    assert!(pair(&a, "-mbd", "rd"), "{a:?}");
    assert!(pair(&a, "-trellis", "1"), "{a:?}");
    assert!(pair(&a, "-intra_vlc", "1"), "{a:?}");
    // One output: the DVD program stream (video + audio muxed directly).
    assert!(pair(&a, "-f", "dvd"));
    assert_eq!(a.last().unwrap(), "out.mpg");
    let pass_pos = a.iter().position(|x| x == "-pass").unwrap();
    assert!(pass_pos < a.len() - 1);

    // Draft mode (`fast: true`) keeps `-mbd rd` and omits `-trellis` / `-intra_vlc` on Pass 2,
    // while Pass 1 stays identical to full-quality Pass 1.
    let mut fast_job = job(&info, Path::new("w"), true);
    fast_job.fast = true;
    let fast_p2 = strings(&fast_job.encode_args("out.mpg"));
    assert!(pair(&fast_p2, "-mbd", "rd"), "{fast_p2:?}");
    assert!(!fast_p2.iter().any(|x| x == "-trellis" || x == "-intra_vlc"), "{fast_p2:?}");
    assert_eq!(strings(&fast_job.first_pass_args()), strings(&job(&info, Path::new("w"), true).first_pass_args()));
}

#[test]
fn single_pass_args_unchanged() {
    let info = fake_info();
    let one = strings(&job(&info, Path::new("w"), false).encode_args("out.mpg"));
    assert!(!one.iter().any(|x| x == "-pass" || x == "-passlogfile"), "{one:?}");
    // 2-pass final run = 1-pass run + exactly the four pass options.
    let two = strings(&job(&info, Path::new("w"), true).encode_args("out.mpg"));
    assert_eq!(two.len(), one.len() + 4);
}

// ---------------------------------------------------------------------------
// Real ffmpeg runs
// ---------------------------------------------------------------------------

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

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cx_2pass_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_clip(tools: &Toolchain, path: &Path, secs: u32) {
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=1280x720:rate=25:duration={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=f=440:duration={secs}"))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success());
}

/// Files in `dir` whose name mentions the 2-pass log prefix.
fn leftover_logs(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("2pass") || n.ends_with(".log.mbtree") || n.ends_with("-0.log"))
        .collect()
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[test]
fn encodes_one_pass_and_two_pass() {
    let Some((rt, tools)) = tools() else { return };
    let dir = temp_dir("encode");
    let clip = dir.join("clip.mp4");
    make_clip(&tools, &clip, 6);
    let info = rt.block_on(probe::probe(&tools.ffprobe, &clip)).unwrap();

    for two_pass in [false, true] {
        let out = dir.join(if two_pass { "two" } else { "one" });
        std::fs::create_dir_all(&out).unwrap();
        // A stale log from an aborted earlier run must not survive either.
        if two_pass {
            std::fs::write(out.join("title_01_2pass-0.log"), "stale").unwrap();
        }
        let j = job(&info, &out, two_pass);

        let mut phases = Vec::new();
        let mut log_seen_in_final = false;
        rt.block_on(encoder::run_encode(&tools, &j, |phase, _| {
            if phases.last() != Some(&phase) {
                phases.push(phase);
            }
            if phase == EncodePhase::Final && !j.passlog_files().is_empty() {
                log_seen_in_final = true;
            }
        }))
        .unwrap_or_else(|e| panic!("two_pass={two_pass}: {e}"));

        if two_pass {
            assert_eq!(phases, vec![EncodePhase::Analysis, EncodePhase::Final]);
            assert!(log_seen_in_final, "pass 1 must leave statistics for pass 2");
        } else {
            assert_eq!(phases, vec![EncodePhase::Single]);
        }
        assert!(file_size(&out.join(j.vob())) > 100_000, "two_pass={two_pass}: program stream too small");
        assert_eq!(leftover_logs(&out), Vec::<String>::new(), "two_pass={two_pass}");
        assert!(j.passlog_files().is_empty());
        // No elementary streams on disk any more.
        let names: Vec<String> =
            std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n.ends_with(".m2v") || n.ends_with(".ac3")), "{names:?}");

        // The result must be a DVD program stream with MPEG-2 video and AC-3 audio.
        let v = rt.block_on(probe::probe(&tools.ffprobe, &out.join(j.vob()))).unwrap();
        assert_eq!(v.video.as_ref().unwrap().codec, "mpeg2video");
        assert_eq!((v.video.as_ref().unwrap().width, v.video.as_ref().unwrap().height), (720, 576));
        assert_eq!(v.audio.first().map(|a| a.codec.as_str()), Some("ac3"));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_dvd_two_pass_leaves_no_logs() {
    let Some((rt, tools)) = tools() else { return };
    if !Path::new(TEMPLATE).is_file() {
        eprintln!("skipping: {TEMPLATE} not found");
        return;
    }
    let dir = temp_dir("build");
    let clip = dir.join("clip.mp4");
    make_clip(&tools, &clip, 5);

    let opts = BuildOptions {
        inputs: vec![clip],
        disc: DiscType::Dvd5,
        template: PathBuf::from(TEMPLATE),
        output: dir.join("out"),
        audio_langs: vec![],
        subtitle_langs: vec![],
        no_subtitles: false,
        chapters: dvd::ChapterSettings { interval_secs: 300.0, ..Default::default() },
        menu_title: None,
        ui_lang: UiLang::DE,
        keep_work: true, // even with kept intermediates the pass logs must be gone
        two_pass: true,
        motion_menu: false,
        motion_duration: None,
        motion_audio: MenuAudio::TitleAudio,
        selections: vec![],
        parallel_jobs: None,
        ..Default::default()
    };
    let mut steps = Vec::new();
    let mut encoding_progress = Vec::new();
    let report = rt
        .block_on(dvd::build_dvd(&opts, &mut |e| match e {
            dvd::Event::Step(s) => steps.push(s),
            dvd::Event::Progress { label, fraction } if label.starts_with("Encoding") => encoding_progress.push(fraction),
            _ => {}
        }))
        .unwrap_or_else(|e| panic!("build failed: {e}"));

    assert!(steps.iter().any(|s| s.contains("2-Pass")), "{steps:?}");
    // Aggregated (parallel-safe) encoding progress covers both passes and ends at 100 %.
    assert!(encoding_progress.windows(2).all(|w| w[0] <= w[1]), "{encoding_progress:?}");
    assert_eq!(encoding_progress.last().copied(), Some(1.0));
    assert!(report.video_ts.join("VTS_01_1.VOB").is_file());
    assert_eq!(leftover_logs(report.work_dir.as_ref().expect("keep_work keeps it")), Vec::<String>::new());
    std::fs::remove_dir_all(&dir).ok();
}
