//! NTSC end to end (titles, still and motion menus, text subtitles) and the single menu sound
//! for all motion menu pages. Skipped (with a message) when tools or templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use musadvd::author::TvStandard;
use musadvd::dvd::{self, BuildOptions};
use musadvd::encoder::MenuAudio;
use musadvd::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn setup(name: &str, clips: usize) -> Option<(tokio::runtime::Runtime, Toolchain, PathBuf, Vec<PathBuf>)> {
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
    let dir = std::env::temp_dir().join(format!("cx_ntsc_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sub.srt"), "1\n00:00:01,000 --> 00:00:03,000\nNTSC subtitle\n").unwrap();
    let inputs = (1..=clips)
        .map(|n| {
            let clip = dir.join(format!("video{n}.mkv"));
            let st = Command::new(&tools.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=5"])
                .args(["-f", "lavfi", "-i", &format!("sine=f={}:duration=5", 300 * n)])
                .arg("-i")
                .arg(dir.join("sub.srt"))
                .args(["-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-c:s", "srt"])
                .arg(&clip)
                .status()
                .unwrap();
            assert!(st.success());
            clip
        })
        .collect();
    Some((rt, tools, dir, inputs))
}

/// `width,height,r_frame_rate` of the first video stream.
fn video_format(tools: &Toolchain, file: &Path) -> String {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,r_frame_rate", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().trim_end_matches(',').to_string()
}

#[test]
fn ntsc_disc() {
    let Some((rt, tools, dir, inputs)) = setup("disc", 1) else { return };
    let opts = BuildOptions {
        inputs,
        template: TEMPLATE.into(),
        output: dir.join("out"),
        tv_standard: TvStandard::Ntsc,
        motion_menu: true,
        motion_duration: Some(2.0),
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        keep_work: true,
        ..BuildOptions::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));

    let xml = &report.authoring_xml;
    assert!(xml.contains(r#"<video format="ntsc""#) && !xml.contains(r#"format="pal""#), "{xml}");

    // Title and menu VOBs: 720x480 at 29.97 fps.
    assert_eq!(video_format(&tools, &report.video_ts.join("VTS_01_1.VOB")), "720,480,30000/1001");
    assert_eq!(video_format(&tools, &report.video_ts.join("VTS_01_0.VOB")), "720,480,30000/1001", "titleset menus");
    assert_eq!(video_format(&tools, &report.video_ts.join("VIDEO_TS.VOB")), "720,480,30000/1001", "main menu");

    // Menu graphics, subpicture masks and spumux XML are NTSC-sized too.
    let work = report.work_dir.as_ref().unwrap();
    let bg = image::open(work.join("vmgm_bg.png")).unwrap();
    assert_eq!((bg.width(), bg.height()), (720, 480));
    let hl = image::open(work.join("ts01_chap_highlight.png")).unwrap();
    assert_eq!((hl.width(), hl.height()), (720, 480));
    let spu = std::fs::read_to_string(work.join("spumux_vmgm.xml")).unwrap();
    assert!(spu.contains(r#"<subpictures format="NTSC">"#));
    let sub = std::fs::read_to_string(work.join("title_01_s1.xml")).unwrap();
    assert!(sub.contains(r#"<subpictures format="NTSC">"#) && sub.contains("<spu "), "{sub}");
    // Natively rendered text subtitles sit inside the 480-line frame.
    for tag in sub.split("<spu ").skip(1) {
        let y: u32 = tag.split(r#"yoffset=""#).nth(1).and_then(|v| v.split('"').next()).and_then(|v| v.parse().ok()).unwrap();
        assert!(y > 300 && y < 480, "{tag}");
    }
    // Root + chapter menu (video slots) and the settings page (still image with the video's sound).
    assert_eq!(report.motion_pages, 3, "motion menus in NTSC");
    std::fs::remove_dir_all(&dir).ok();
}

/// Sound of every menu page: (VOB, source file name, start position).
fn sounds(report: &dvd::BuildReport) -> Vec<(String, Option<(String, f64)>)> {
    report
        .menu_sounds
        .iter()
        .map(|(vob, a)| {
            let src = a.as_ref().map(|c| (c.path.file_name().unwrap().to_string_lossy().into_owned(), c.start_secs));
            (vob.clone(), src)
        })
        .collect()
}

#[test]
fn menu_sound_per_video() {
    let Some((rt, tools, dir, inputs)) = setup("sound", 2) else { return };
    let mut opts = BuildOptions {
        inputs,
        template: TEMPLATE.into(),
        output: dir.join("out"),
        motion_menu: true,
        motion_duration: Some(2.0),
        chapters: dvd::ChapterSettings { interval_secs: 1.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    let s = sounds(&report);
    let get = |vob: &str| s.iter().find(|(v, _)| v == vob).unwrap_or_else(|| panic!("{vob} missing in {s:?}")).1.clone();

    // Main menu: silent by default.
    assert_eq!(get("vmgm.mpg"), None);
    // Each video's own menus play that video – never video 1 for video 2.
    let at_10_percent = |a: Option<(String, f64)>, file: &str| {
        let (f, start) = a.expect("sound");
        assert_eq!(f, file);
        assert!((start - 0.5).abs() < 0.05, "root starts at 10 % of the ~5 s video: {start}");
    };
    at_10_percent(get("ts01_root.mpg"), "video1.mkv");
    at_10_percent(get("ts02_root.mpg"), "video2.mkv");
    assert_eq!(get("ts02_set.mpg").map(|a| a.0), Some("video2.mkv".into()), "settings page has the video's sound too");
    // Chapter pages start at the first chapter they show (page 1: chapter 1 at 0 s).
    assert_eq!(get("ts01_chap.mpg"), Some(("video1.mkv".into(), 0.0)));
    assert_eq!(get("ts02_chap.mpg"), Some(("video2.mkv".into(), 0.0)));
    assert!(
        s.iter().all(|(v, a)| !v.starts_with("ts02") || a.as_ref().is_some_and(|a| a.0 == "video2.mkv")),
        "{s:?}"
    );

    // A global menu audio file only replaces the main menu's sound.
    let music = dir.join("menu-music.wav");
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=f=1000:duration=3"])
        .arg(&music)
        .status()
        .unwrap();
    assert!(st.success());
    opts.motion_audio = MenuAudio::File(music);
    opts.project_name = Some("mit Musik".into());
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    let s = sounds(&report);
    let vmgm = s.iter().find(|(v, _)| v == "vmgm.mpg").unwrap().1.clone();
    assert_eq!(vmgm, Some(("menu-music.wav".into(), 0.0)), "main menu loops the music file");
    let root2 = s.iter().find(|(v, _)| v == "ts02_root.mpg").unwrap().1.clone();
    assert_eq!(root2.map(|a| a.0), Some("video2.mkv".into()), "video menus keep their own sound");
    std::fs::remove_dir_all(&dir).ok();
}
