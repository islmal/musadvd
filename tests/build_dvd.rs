//! End-to-end: generate two short clips with ffmpeg, build a DVD5 and check the VIDEO_TS.
//! Skipped (with a message) when ffmpeg/dvdauthor or the ConvertX templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use convertx_rust::calculator::DiscType;
use convertx_rust::encoder::MenuAudio;
use convertx_rust::dvd::{self, BuildOptions, Event, StreamSelection};
use convertx_rust::menu::UiLang;
use convertx_rust::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn ffmpeg(tools: &Toolchain, args: &[&str]) {
    let st = Command::new(&tools.ffmpeg).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

/// Stream codec names of a VOB, in order.
fn codecs(tools: &Toolchain, vob: &Path) -> Vec<String> {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(vob)
        .output()
        .unwrap();
    // ffprobe may append an empty side-data column ("mpeg2video,").
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().trim_end_matches(',').to_string()).collect()
}

#[test]
fn builds_dvd_with_submenus() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = match rt.block_on(Toolchain::detect()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    // With the bundled native builds present, they must win over anything on PATH.
    let bin = std::env::current_dir().unwrap().join("bin");
    if bin.join("ffmpeg.exe").is_file() {
        assert_eq!(tools.ffmpeg, bin.join("ffmpeg.exe"));
    }
    if !Path::new(TEMPLATE).is_file() {
        eprintln!("skipping: {TEMPLATE} not found");
        return;
    }

    let dir = std::env::temp_dir().join(format!("cx_build_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = |p: &Path| p.to_string_lossy().into_owned();

    // Clip 1: 16:9, 5.1 German + stereo English, German SRT, 3 chapters.
    std::fs::write(dir.join("de.srt"), "1\n00:00:01,000 --> 00:00:03,000\nHallo Ä Ö Ü\n").unwrap();
    std::fs::write(
        dir.join("meta.txt"),
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2000\ntitle=Eins\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=2000\nEND=4000\ntitle=Zwei\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=4000\nEND=6000\ntitle=Drei\n",
    )
    .unwrap();
    let clip1 = dir.join("clip one.mkv");
    ffmpeg(&tools, &[
        "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=6",
        "-f", "lavfi", "-i", "sine=f=440:duration=6,aformat=channel_layouts=5.1",
        "-f", "lavfi", "-i", "sine=f=880:duration=6",
        "-i", &s(&dir.join("de.srt")), "-i", &s(&dir.join("meta.txt")),
        "-map", "0", "-map", "1", "-map", "2", "-map", "3", "-map_chapters", "4",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-c:s", "srt",
        "-metadata:s:a:0", "language=ger", "-metadata:s:a:1", "language=eng", "-metadata:s:s:0", "language=ger",
        &s(&clip1),
    ]);
    // Clip 2: 4:3, mono, no chapters.
    let clip2 = dir.join("clip2.mp4");
    ffmpeg(&tools, &[
        "-f", "lavfi", "-i", "smptebars=size=640x480:rate=25:duration=4",
        "-f", "lavfi", "-i", "sine=f=330:duration=4",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", &s(&clip2),
    ]);

    let out = dir.join("out");
    let opts = BuildOptions {
        inputs: vec![clip1, clip2],
        disc: DiscType::Dvd5,
        template: PathBuf::from(TEMPLATE),
        output: out.clone(),
        audio_langs: vec![],
        subtitle_langs: vec![],
        no_subtitles: false,
        chapters: dvd::ChapterSettings { interval_secs: 300.0, ..Default::default() },
        menu_title: Some("Test".into()),
        ui_lang: UiLang::DE,
        keep_work: false,
        two_pass: false,
        motion_menu: false,
        motion_duration: None,
        motion_audio: MenuAudio::TitleAudio,
        selections: vec![],
        parallel_jobs: None,
        ..Default::default()
    };
    let mut events = Vec::new();
    let report = rt
        .block_on(dvd::build_dvd(&opts, &mut |e: Event| events.push(e)))
        .unwrap_or_else(|e| panic!("build failed: {e}"));

    // Structure: VMGM + two titlesets with menus.
    let vts = &report.video_ts;
    for f in ["VIDEO_TS.IFO", "VIDEO_TS.BUP", "VIDEO_TS.VOB", "VTS_01_0.IFO", "VTS_01_0.VOB", "VTS_01_1.VOB", "VTS_02_0.IFO", "VTS_02_1.VOB"] {
        assert!(vts.join(f).is_file(), "{f} missing");
    }
    assert!(report.size_mb > 1.0 && report.size_mb < report.target_mb as f64);

    // Title 1: video, two AC-3 tracks, one subpicture stream; title 2: video + one AC-3.
    let c1 = codecs(&tools, &vts.join("VTS_01_1.VOB"));
    assert_eq!(c1.iter().filter(|c| *c == "mpeg2video").count(), 1, "{c1:?}");
    assert_eq!(c1.iter().filter(|c| *c == "ac3").count(), 2, "{c1:?}");
    assert_eq!(c1.iter().filter(|c| *c == "dvd_subtitle").count(), 1, "{c1:?}");
    let c2 = codecs(&tools, &vts.join("VTS_02_1.VOB"));
    assert_eq!(c2.iter().filter(|c| *c == "ac3").count(), 1, "{c2:?}");

    assert_eq!(report.titles[0].aspect.as_str(), "16:9");
    assert_eq!(report.titles[1].aspect.as_str(), "4:3");
    assert_eq!(report.titles[0].chapters, 3);
    assert_eq!(report.titles[0].audio, vec!["Deutsch (5.1)", "Englisch (Stereo)"]);

    // Menus: main + title 1 (root, chapters, settings) + title 2 (root only: 1 chapter, 1 track).
    assert_eq!(report.menu_pages, 1 + 3 + 1);
    let xml = &report.authoring_xml;
    assert!(xml.contains("jump titleset 1 menu;"));
    assert!(xml.contains("g1 = 0; jump title 1 chapter 3;"));
    assert!(xml.contains("audio = 1; jump menu 3;"));
    assert!(xml.contains("subtitle = 64; jump menu 3;"));
    assert!(xml.contains("jump vmgm menu 1;"));
    // The project folder is named after the menu title ("Test").
    assert!(xml.contains("<!-- titleset 1: clip one -->"));
    assert_eq!(report.project_dir, out.join("Test"));
    assert_eq!(report.video_ts, out.join("Test").join("VIDEO_TS"));
    // The work directory (intermediate MPEGs, menu PNGs, logs) is removed after the build.
    assert!(report.work_dir.is_none());
    assert!(!out.join(".convertx-work").exists());
    assert_eq!(
        std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect::<Vec<_>>(),
        vec![std::ffi::OsString::from("Test")],
        "only the project folder is left"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Two merged inputs are one title: a single titleset whose only PGC plays both VOBs.
#[test]
fn merged_inputs_share_one_titleset() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let Ok(tools) = rt.block_on(Toolchain::detect()) else {
        eprintln!("skipping: tools missing");
        return;
    };
    if !Path::new(TEMPLATE).is_file() {
        eprintln!("skipping: {TEMPLATE} not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cx_merge_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clip = |name: &str, size: &str, secs: u32| {
        let path = dir.join(name);
        ffmpeg(&tools, &[
            "-f", "lavfi", "-i", &format!("testsrc2=size={size}:rate=25:duration={secs}"),
            "-f", "lavfi", "-i", &format!("sine=f=440:duration={secs}"),
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", path.to_str().unwrap(),
        ]);
        path
    };
    let opts = BuildOptions {
        inputs: vec![clip("a.mkv", "640x360", 3), clip("b.mkv", "640x480", 3)],
        disc: DiscType::Dvd5,
        template: PathBuf::from(TEMPLATE),
        output: dir.join("out"),
        chapters: dvd::ChapterSettings { interval_secs: 300.0, ..Default::default() },
        selections: vec![StreamSelection::default(), StreamSelection { merge_with_previous: true, ..Default::default() }],
        ..Default::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    let vts = &report.video_ts;
    assert!(vts.join("VTS_01_1.VOB").is_file());
    let extra: Vec<_> = std::fs::read_dir(vts).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("VTS_02_")).collect();
    assert!(extra.is_empty(), "a merged group must not create a second titleset: {extra:?}");
    let xml = &report.authoring_xml;
    assert_eq!(xml.matches("<titleset>").count(), 1);
    let titles = &xml[xml.find("<titles>").unwrap()..xml.find("</titles>").unwrap()];
    assert_eq!(titles.matches("<pgc>").count(), 1, "one title PGC");
    assert_eq!(xml.matches(r#"<vob file="title_"#).count(), 2, "{xml}");
    // Both members were encoded in the first member's 16:9 aspect.
    assert!(report.titles.iter().all(|t| t.aspect.as_str() == "16:9"));
    std::fs::remove_dir_all(&dir).ok();
}
