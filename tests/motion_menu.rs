//! Motion menus: layer split and video slots on the real "Open Style" template, the ffmpeg
//! overlay pipeline, and a complete DVD build with looping menus.
//! Tests needing the templates or ffmpeg/dvdauthor are skipped (with a message) without them.

use std::path::{Path, PathBuf};
use std::process::Command;

use convertx_rust::author::{self, ChapterEntry, ClipSource, MenuKind, MenuLinks, TvStandard, PAL_HEIGHT, PAL_WIDTH};
use convertx_rust::calculator::DiscType;
use convertx_rust::dvd::{self, BuildOptions};
use convertx_rust::encoder::{self, MenuAudio, VideoSlot};
use convertx_rust::menu::{self, page_by_style, RenderOptions, UiLang};
use convertx_rust::template::Template;
use convertx_rust::tools::Toolchain;

const DIR: &str = r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style";

fn open_style() -> Option<Template> {
    let p = Path::new(DIR).join("Open_Style.ini");
    match Template::load(&p) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("skipping: {}: {e}", p.display());
            None
        }
    }
}

fn clip(start: f64) -> ClipSource {
    ClipSource { path: "movie.mkv".into(), start_secs: start, has_audio: true }
}

fn chapters(n: usize) -> Vec<ChapterEntry> {
    (0..n)
        .map(|i| ChapterEntry { label: format!("Kapitel {}", i + 1), thumbnail: None, clip: Some(clip(i as f64 * 60.0)) })
        .collect()
}

fn render_chapter_page(t: &Template, motion: bool, chapters: &[ChapterEntry]) -> menu::RenderedPage {
    let page = page_by_style(t, "MNUS_CHAPTER").expect("Open Style has a chapter page");
    let kind = MenuKind::Chapters(chapters);
    let links = MenuLinks { first: 1, root: None, chapters: Some(1), settings: None, main_menu: None, bonus: None, episodes: None, title1_chapters: None, title1_settings: None };
    let (buttons, _) = author::plan_buttons(page, &kind, 1, links, PAL_WIDTH, PAL_HEIGHT).unwrap();
    let opts = RenderOptions { motion, ui_lang: UiLang::DE, ..RenderOptions::still(Path::new(DIR)) };
    menu::render_page(t, page, &buttons, &opts).unwrap()
}

#[test]
fn chapter_page_slots_and_layers() {
    let Some(t) = open_style() else { return };
    let r = render_chapter_page(&t, true, &chapters(3));

    // One slot per visible chapter, at the template's GT_VIDEO coordinates (letterboxed).
    assert_eq!(r.video_slots.len(), 3);
    let geom: Vec<_> = r.video_slots.iter().map(|s| (s.x, s.y, s.width, s.height, s.letterbox)).collect();
    assert_eq!(geom, vec![(95, 106, 174, 139, true), (273, 106, 174, 139, true), (451, 106, 174, 139, true)]);
    assert_eq!(r.video_slots[1].clip.start_secs, 60.0, "chapter 2 clip starts at its chapter mark");

    // Frames, captions and icons are on the transparent foreground, above the clips.
    let fg = r.foreground.as_ref().expect("motion page has a foreground layer");
    assert_eq!(fg.dimensions(), (PAL_WIDTH, PAL_HEIGHT));
    let slot = &r.video_slots[0];
    let centre = fg.get_pixel(slot.x + slot.width / 2, slot.y + slot.height / 2);
    assert_eq!(centre[3], 0, "the frame's window must stay transparent so the clip shows through");
    let border = (0..6).map(|d| fg.get_pixel(slot.x + d, slot.y + slot.height / 2)[3]).max().unwrap();
    assert!(border > 0, "the frame (frame_grey.png) is drawn above the clip");
    // The page background stays on the opaque base layer.
    assert_eq!(r.background.get_pixel(5, 300)[3], 255);
    assert_eq!(fg.get_pixel(5, 300)[3], 0);
    // Highlight masks are unaffected.
    assert!(r.highlight.pixels.iter().any(|&p| p != 0));
}

#[test]
fn still_mode_is_unchanged() {
    let Some(t) = open_style() else { return };
    let r = render_chapter_page(&t, false, &chapters(3));
    assert!(r.foreground.is_none());
    assert!(r.video_slots.is_empty());
}

#[test]
fn hidden_chapters_get_no_slot() {
    let Some(t) = open_style() else { return };
    // 6 slots on the page, 2 chapters: only 2 clips.
    assert_eq!(render_chapter_page(&t, true, &chapters(2)).video_slots.len(), 2);
}

/// Chapter menus: every page starts the video's sound at the first chapter it shows.
#[test]
fn chapter_pages_start_sound_at_their_first_chapter() {
    let Some(t) = open_style() else { return };
    let page = page_by_style(&t, "MNUS_CHAPTER").unwrap();
    let chapters = chapters(8); // 6 per page -> page 2 starts with chapter 7 (360 s)
    let kind = MenuKind::Chapters(&chapters);
    let links = MenuLinks { first: 1, root: None, chapters: Some(1), settings: None, main_menu: None, bonus: None, episodes: None, title1_chapters: None, title1_settings: None };
    let sound = clip(30.0);
    let opts = RenderOptions {
        motion: true,
        menu_audio: Some(&sound),
        audio_from_first_chapter: true,
        ..RenderOptions::still(Path::new(DIR))
    };
    let out = std::env::temp_dir().join(format!("cx_chapter_sound_{}", std::process::id()));
    let menu = menu::render_menu(&t, page, &kind, links, Some("ptt"), "chap", &out, &opts).unwrap();
    let starts: Vec<f64> = menu.pages.iter().map(|(_, f)| f.audio.as_ref().unwrap().start_secs).collect();
    assert_eq!(starts, vec![0.0, 360.0]);
    assert!(menu.pages.iter().all(|(_, f)| f.audio.as_ref().unwrap().path == Path::new("movie.mkv")));

    // Without motion menus there is no sound at all (still menus).
    let still = RenderOptions { motion: false, ..opts };
    let menu = menu::render_menu(&t, page, &kind, links, Some("ptt"), "chap_still", &out, &still).unwrap();
    assert!(menu.pages.iter().all(|(_, f)| f.audio.is_none() && !f.is_motion()));
    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn motion_duration_from_template_or_cli() {
    let Some(t) = open_style() else { return };
    let page = page_by_style(&t, "MNUS_CHAPTER").unwrap();
    assert!(page.main_duration > 0.0);
    assert_eq!(menu::motion_duration(page, None), page.main_duration);
    assert_eq!(menu::motion_duration(page, Some(12.0)), 12.0);
    assert_eq!(menu::motion_duration(page, Some(0.0)), page.main_duration, "invalid override ignored");
}

// ---------------------------------------------------------------------------
// ffmpeg pipeline
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
    let d = std::env::temp_dir().join(format!("cx_motion_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(tools: &Toolchain, args: &[&str]) {
    let st = Command::new(&tools.ffmpeg).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

/// (codec names, container duration) of a media file.
fn probe(tools: &Toolchain, path: &Path) -> (Vec<String>, f64) {
    let run = |entries: &str| {
        let out = Command::new(&tools.ffprobe)
            .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().trim_end_matches(',').to_string()).collect::<Vec<_>>()
    };
    let duration = run("format=duration").first().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    (run("stream=codec_name"), duration)
}

#[test]
fn overlay_pipeline_produces_dvd_mpeg() {
    let Some((rt, tools)) = tools() else { return };
    let dir = temp_dir("pipeline");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let (base, fg, src) = (dir.join("bg.png"), dir.join("fg.png"), dir.join("src.mp4"));
    ffmpeg(&tools, &["-f", "lavfi", "-i", "color=c=navy:s=720x576", "-frames:v", "1", &s(&base)]);
    ffmpeg(&tools, &["-f", "lavfi", "-i", "color=c=white@0.0:s=720x576,format=rgba", "-frames:v", "1", &s(&fg)]);
    // Video only: the menu sound must fall back to silence.
    ffmpeg(&tools, &["-f", "lavfi", "-i", "testsrc2=s=640x360:r=25:d=2", "-c:v", "libx264", "-preset", "ultrafast", &s(&src)]);

    let slots = [
        VideoSlot { x: 95, y: 106, width: 174, height: 139, letterbox: true, clip: ClipSource { path: src.clone(), start_secs: 0.5, has_audio: false } },
        VideoSlot { x: 273, y: 106, width: 175, height: 139, letterbox: false, clip: ClipSource { path: src.clone(), start_secs: 0.0, has_audio: false } },
    ];
    let out = dir.join("menu_raw.mpg");
    // 4 s menu from a 2 s source: the clip has to loop.
    let args = encoder::motion_menu_args(TvStandard::Pal, &base, Some(&fg), &slots, 4.0, None, None, &out);
    rt.block_on(tools.ffmpeg(&args, 4.0, |_| {})).unwrap();

    let (codecs, duration) = probe(&tools, &out);
    assert!(codecs.contains(&"mpeg2video".to_string()), "{codecs:?}");
    assert!(codecs.contains(&"ac3".to_string()), "silent AC-3 track expected: {codecs:?}");
    assert!((3.5..=4.6).contains(&duration), "duration {duration}");
    let dims = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,r_frame_rate,display_aspect_ratio", "-of", "csv=p=0"])
        .arg(&out)
        .output()
        .unwrap();
    let dims = String::from_utf8_lossy(&dims.stdout).trim().trim_end_matches(',').to_string();
    assert_eq!(dims, "720,576,4:3,25/1");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_dvd_with_motion_menus() {
    let Some((rt, tools)) = tools() else { return };
    let template = Path::new(DIR).join("10_TitlesPerPageNoRoot.ini");
    if !template.is_file() {
        eprintln!("skipping: {} not found", template.display());
        return;
    }
    let dir = temp_dir("build");
    let clip = dir.join("clip.mp4");
    ffmpeg(&tools, &[
        "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=7",
        "-f", "lavfi", "-i", "sine=f=440:duration=7",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", &clip.to_string_lossy(),
    ]);

    let opts = BuildOptions {
        inputs: vec![clip],
        disc: DiscType::Dvd5,
        template,
        output: dir.join("out"),
        audio_langs: vec![],
        subtitle_langs: vec![],
        no_subtitles: true,
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() }, // 0, 2, 4 s -> chapter menu with 3 slots
        menu_title: None,
        ui_lang: UiLang::DE,
        keep_work: true, // the test inspects menu PNGs, MPEGs and logs
        two_pass: false,
        motion_menu: true,
        motion_duration: Some(3.0),
        motion_audio: MenuAudio::TitleAudio,
        selections: vec![],
        parallel_jobs: None,
        ..Default::default()
    };
    let report = rt
        .block_on(dvd::build_dvd(&opts, &mut |_| {}))
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    let work = report.work_dir.as_ref().expect("keep_work keeps the work directory");

    // Main menu of 10_TitlesPerPageNoRoot has no video slots and stays a still; the titleset's
    // root (video background) and chapter menu (thumbnails) are animated.
    assert_eq!(report.motion_pages, 2, "root + chapter menu");
    assert!(work.join("ts01_chap_fg.png").is_file());
    assert!(work.join("ts01_root_fg.png").is_file());
    assert!(!work.join("vmgm_fg.png").exists());

    let xml = &report.authoring_xml;
    assert!(xml.contains(r#"<vob file="vmgm.mpg" pause="inf"/>"#));
    assert!(xml.contains("<vob file=\"ts01_chap.mpg\"/>\n        <post>jump cell 1;</post>"), "{xml}");
    assert!(xml.contains("<vob file=\"ts01_root.mpg\"/>\n        <post>jump cell 1;</post>"));
    assert!(!xml.contains(r#"ts01_chap.mpg" pause"#));

    // The motion menu is a 3 s loop with sound and the spumux highlight subpicture.
    let (codecs, duration) = probe(&tools, &work.join("ts01_chap.mpg"));
    for c in ["mpeg2video", "ac3", "dvd_subtitle"] {
        assert!(codecs.contains(&c.to_string()), "{c} missing in {codecs:?}");
    }
    assert!((2.5..=3.6).contains(&duration), "duration {duration}");

    // dvdauthor accepted everything and wrote the menu domain VOB.
    assert!(report.video_ts.join("VTS_01_0.VOB").is_file());
    assert!(report.video_ts.join("VTS_01_1.VOB").is_file());
    let log = std::fs::read_to_string(work.join("dvdauthor.log")).unwrap();
    assert!(!log.contains("Discontinuity"), "audio of the motion menus must be continuous");
    std::fs::remove_dir_all(&dir).ok();
}
