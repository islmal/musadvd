//! "Menü Optionen" from the settings dialog, authored with the real dvdauthor:
//! auto start + sequential + loop on a two-title disc, and a single title without main menu
//! and without root menu. Skipped (with a message) when tools or templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use musadvd::author::PlaybackOptions;
use musadvd::config::AppSettings;
use musadvd::dvd::{self, BuildOptions};
use musadvd::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn setup(name: &str) -> Option<(tokio::runtime::Runtime, PathBuf, Vec<PathBuf>)> {
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
    let dir = std::env::temp_dir().join(format!("cx_playback_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clips = (1..=2)
        .map(|n| {
            let clip = dir.join(format!("clip{n}.mp4"));
            let st = Command::new(&tools.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=4"])
                .args(["-f", "lavfi", "-i", "sine=f=440:duration=4", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
                .arg(&clip)
                .status()
                .unwrap();
            assert!(st.success());
            clip
        })
        .collect();
    Some((rt, dir, clips))
}

fn build(rt: &tokio::runtime::Runtime, opts: &BuildOptions) -> String {
    let report = rt
        .block_on(dvd::build_dvd(opts, &mut |_| {}))
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    assert!(report.video_ts.join("VIDEO_TS.IFO").is_file());
    assert!(report.video_ts.join("VTS_01_1.VOB").is_file());
    report.authoring_xml
}

#[test]
fn auto_start_sequential_loop() {
    let Some((rt, dir, clips)) = setup("seq") else { return };
    let opts = BuildOptions {
        inputs: clips,
        template: TEMPLATE.into(),
        output: dir.join("out"),
        playback: PlaybackOptions { auto_start: true, play_sequentially: true, loop_playback: true, return_to_title_menu: true },
        ..BuildOptions::default()
    };
    let xml = build(&rt, &opts);
    // Without a menu title the project (folder and heading) is named after the first video.
    let out = dir.join("out");
    assert!(out.join("clip1").join("VIDEO_TS").join("VIDEO_TS.IFO").is_file());
    assert!(!has_work_dir(&out), "work directory removed after success");
    assert!(xml.contains("<fpc>g1 = 0; jump title 1;</fpc>"), "auto start");
    assert!(xml.contains("<pre>jump title 2;</pre>"), "title 1 -> title 2");
    assert!(xml.contains("<pre>jump title 1;</pre>"), "title 2 -> title 1 (loop)");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn single_title_without_title_and_root_menu() {
    let Some((rt, dir, mut clips)) = setup("single") else { return };
    clips.truncate(1);
    // Settings as saved by the dialog, applied the same way the GUI does it.
    let settings = AppSettings {
        no_title_menu_for_single_title: true,
        skip_root_menu: true,
        auto_chapter_interval_seconds: 30,
        auto_chapter_min_video_length_seconds: 0,
        force_first_subtitle: true,
        ..AppSettings::default()
    };
    let mut opts = BuildOptions { inputs: clips, template: TEMPLATE.into(), output: dir.join("out"), ..BuildOptions::default() };
    settings.apply_to(&mut opts);
    let xml = build(&rt, &opts);

    assert!(!xml.contains("entry=\"title\""), "no main menu");
    assert!(!xml.contains("ts01_root"), "no root menu");
    // Without root menu and with a single 4 s chapter there is no titleset menu either.
    assert!(xml.contains("<fpc>g1 = 0; jump title 1;</fpc>"), "{xml}");
    assert!(xml.contains("<post>exit;</post>"));
    assert!(!xml.contains("vmgm menu"), "no jumps into a missing main menu");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn single_title_with_own_menu_but_no_title_menu() {
    let Some((rt, dir, mut clips)) = setup("ownmenu") else { return };
    clips.truncate(1);
    let opts = BuildOptions {
        inputs: clips,
        template: TEMPLATE.into(),
        output: dir.join("out"),
        no_title_menu_for_single_title: true,
        // 4 s video with a chapter every second -> chapter menu exists.
        chapters: dvd::ChapterSettings { interval_secs: 1.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let xml = build(&rt, &opts);
    assert!(!xml.contains("entry=\"title\""));
    assert!(xml.contains("<fpc>g1 = 0; jump titleset 1 menu;</fpc>"), "disc starts in the title's own menu");
    assert!(xml.contains("<post>call menu;</post>"), "after playback back to the title's menu");
    assert!(!xml.contains("vmgm menu"), "back buttons to the missing main menu are hidden");
    std::fs::remove_dir_all(&dir).ok();
}

/// True if the output directory still holds a `<project>.musadvd-work` folder.
fn has_work_dir(out: &std::path::Path) -> bool {
    std::fs::read_dir(out).unwrap().filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".musadvd-work"))
}
