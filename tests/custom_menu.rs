//! Custom menu layouts (menu editor JSON) end to end: rendering, the ffmpeg layer graph
//! (blurred background, moving and rotated elements, sound), spumux masks and the cross-domain
//! navigation (dispatchers, router) accepted by the real dvdauthor.
//! Skipped (with a message) when the tools are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use convertx_rust::author::TvStandard;
use convertx_rust::dvd::{self, BuildOptions};
use convertx_rust::layout::CustomLayout;
use convertx_rust::tools::Toolchain;

fn setup(name: &str) -> Option<(tokio::runtime::Runtime, Toolchain, PathBuf, Vec<PathBuf>)> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = match rt.block_on(Toolchain::detect()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: {e}");
            return None;
        }
    };
    let dir = std::env::temp_dir().join(format!("cx_custom_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let inputs = (1..=2)
        .map(|n| {
            let clip = dir.join(format!("film{n}.mkv"));
            let st = Command::new(&tools.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=6"])
                .args(["-f", "lavfi", "-i", &format!("sine=f={}:duration=6", 330 * n)])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
                .arg(&clip)
                .status()
                .unwrap();
            assert!(st.success());
            clip
        })
        .collect();
    // Background picture and a small logo.
    image::RgbImage::from_fn(320, 180, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 160])).save(dir.join("bg.png")).unwrap();
    image::RgbaImage::from_pixel(64, 32, image::Rgba([250, 40, 40, 255])).save(dir.join("logo.png")).unwrap();
    Some((rt, tools, dir, inputs))
}

fn layout_json(dir: &Path, canvas_h: u32) -> String {
    let p = |n: &str| dir.join(n).to_string_lossy().replace('\\', "\\\\");
    format!(
        r##"{{
  "version": 1,
  "canvas": {{"width": 720, "height": {canvas_h}}},
  "highlight": {{"color": "#00E0FF", "selectColor": "#FFFFFF", "opacity": 0.9}},
  "menus": [
    {{"id": "main", "name": "Hauptmenü", "role": "title", "durationSecs": 3,
      "background": {{"image": "{bg}", "blur": 6}},
      "audio": {{"type": "title", "title": 1}},
      "elements": [
        {{"type": "text", "text": "Mein Urlaub", "x": 40, "y": 30, "w": 640, "h": 60, "size": 44, "bold": true, "shadow": true}},
        {{"type": "image", "src": "{logo}", "x": 620, "y": 120, "w": 40, "h": 80, "rotation": 90, "fit": "stretch"}},
        {{"type": "video", "title": 1, "startSecs": 1, "x": 380, "y": 120, "w": 220, "h": 124,
          "action": {{"type": "menu", "menu": "extras1"}}}},
        {{"type": "text", "text": "Film 1", "x": 40, "y": 140, "w": 300, "h": 44, "align": "left",
          "action": {{"type": "playTitle", "title": 1}}}},
        {{"type": "text", "text": "Film 2", "x": 40, "y": 200, "w": 300, "h": 44, "align": "left",
          "action": {{"type": "playTitle", "title": 2}}}},
        {{"type": "text", "text": "Film 2, Kapitel 2", "x": 40, "y": 260, "w": 300, "h": 44, "align": "left",
          "action": {{"type": "playChapter", "title": 2, "chapter": 2}}}},
        {{"type": "text", "text": "Alles abspielen", "x": 40, "y": 320, "w": 300, "h": 44, "align": "left",
          "action": {{"type": "playAll"}}}},
        {{"type": "text", "text": "Seite 2 ›", "x": 500, "y": 400, "w": 180, "h": 40,
          "action": {{"type": "menu", "menu": "page2"}}}}
      ]}},
    {{"id": "page2", "name": "Seite 2", "role": "title",
      "background": {{"color": "#202848"}},
      "elements": [
        {{"type": "text", "text": "‹ Zurück", "x": 40, "y": 400, "w": 200, "h": 40, "action": {{"type": "menu", "menu": "main"}}}}
      ]}},
    {{"id": "root1", "name": "Film 1 – Menü", "role": "root", "title": 1,
      "background": {{"video": {{"title": 1}}, "blur": 12}},
      "durationSecs": 2,
      "elements": [
        {{"type": "text", "text": "Kapitel 2", "x": 40, "y": 100, "w": 300, "h": 44, "action": {{"type": "playChapter", "title": 1, "chapter": 2}}}},
        {{"type": "text", "text": "Film 2 abspielen", "x": 40, "y": 160, "w": 300, "h": 44, "action": {{"type": "playTitle", "title": 2}}}},
        {{"type": "text", "text": "Extras", "x": 40, "y": 220, "w": 300, "h": 44, "action": {{"type": "menu", "menu": "extras1"}}}},
        {{"type": "text", "text": "Hauptmenü", "x": 40, "y": 280, "w": 300, "h": 44, "action": {{"type": "menu", "menu": "main"}}}}
      ]}},
    {{"id": "extras1", "name": "Film 1 – Extras", "role": "root", "title": 1,
      "elements": [
        {{"type": "text", "text": "Zurück", "x": 40, "y": 100, "w": 300, "h": 44, "action": {{"type": "menu", "menu": "root1"}}}}
      ]}}
  ]
}}"##,
        bg = p("bg.png"),
        logo = p("logo.png"),
    )
}

/// `codec_type:codec_name,width,height` of every stream.
fn streams(tools: &Toolchain, file: &Path) -> Vec<String> {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,width,height", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().trim_end_matches(',').to_string()).collect()
}

fn duration(tools: &Toolchain, file: &Path) -> f64 {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0.0)
}

#[test]
fn custom_layout_builds_a_dvd() {
    let Some((rt, tools, dir, inputs)) = setup("pal") else { return };
    let layout = CustomLayout::from_json(&layout_json(&dir, 576)).unwrap();
    let opts = BuildOptions {
        inputs,
        output: dir.join("out"),
        custom_layout: Some(layout),
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        keep_work: true,
        ..BuildOptions::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    let xml = &report.authoring_xml;

    // VMGM: 2 visible title menus + the router (menu 3), then the title-end dispatchers.
    // (Only "Film 2 abspielen" in root1 leaves its titleset.)
    assert!(xml.contains("<pre>if (g3 eq 1002) jump title 2; jump menu 1;</pre>"), "{xml}");
    // Title 1: dispatcher (Extras entered from the main menu) + 2 root menus.
    assert!(xml.contains("<pre>if (g2 eq 102) jump menu 3; jump menu 2;</pre>"), "{xml}");
    // Title 2: dispatcher for chapter 2 only, no root menu of its own.
    assert!(xml.contains("<pre>if (g2 eq 2) jump title 1 chapter 2; jump vmgm menu 1;</pre>"), "{xml}");
    assert!(xml.contains("g1 = 0; g2 = 2; jump titleset 2 menu;"), "{xml}");
    assert!(xml.contains("g1 = 0; g3 = 1002; jump vmgm menu 3;"), "{xml}");
    assert!(xml.contains("<pre>g2 = 0;</pre>"), "titles and root menus reset the dispatch register: {xml}");
    assert_eq!(report.menu_pages, 4);
    assert_eq!(report.motion_pages, 2, "main menu (video + sound) and root1 (video background)");

    // dvdauthor accepted everything: VMGM menu, both titlesets with menus.
    let vts = &report.video_ts;
    for f in ["VIDEO_TS.IFO", "VIDEO_TS.VOB", "VTS_01_0.IFO", "VTS_01_0.VOB", "VTS_01_1.VOB", "VTS_02_0.IFO", "VTS_02_1.VOB"] {
        assert!(vts.join(f).is_file(), "{f} missing");
    }
    // The main menu: MPEG-2 PAL, AC-3, a subpicture stream with the highlights, ~3 s loop + page 2.
    let s = streams(&tools, &vts.join("VIDEO_TS.VOB"));
    assert!(s.iter().any(|l| l == "mpeg2video,video,720,576"), "{s:?}");
    assert!(s.iter().any(|l| l.starts_with("ac3,audio")), "{s:?}");
    assert!(s.iter().any(|l| l.starts_with("dvd_subtitle,subtitle")), "{s:?}");
    let work = report.work_dir.as_ref().unwrap();
    let d = duration(&tools, &work.join("cm01.mpg"));
    assert!((2.5..4.0).contains(&d), "main menu loop length {d}");
    // The menu sound is title 1's audio from its default sample start.
    let sound = report.menu_sounds.iter().find(|(v, _)| v == "cm01.mpg").and_then(|(_, a)| a.clone()).expect("main menu sound");
    assert!(sound.path.ends_with("film1.mkv"));

    // Highlight mask of the main menu: 6 buttons, only palette colours of the layout.
    let spumux = std::fs::read_to_string(work.join("cm01_spumux.xml")).unwrap();
    assert_eq!(spumux.matches("<button ").count(), 6, "{spumux}");
    let hl = image::open(work.join("cm01_hl.png")).unwrap().to_rgba8();
    let colours: std::collections::BTreeSet<[u8; 4]> = hl.pixels().map(|p| p.0).collect();
    assert!(colours.len() <= 4, "{colours:?}");
    assert!(colours.contains(&[0, 224, 255, 230]), "{colours:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn custom_layout_ntsc_canvas() {
    let Some((rt, tools, dir, inputs)) = setup("ntsc") else { return };
    // Designed on an NTSC canvas: no scaling, 720x480 output.
    let layout = CustomLayout::from_json(&layout_json(&dir, 480)).unwrap();
    let opts = BuildOptions {
        inputs,
        output: dir.join("out"),
        custom_layout: Some(layout),
        tv_standard: TvStandard::Ntsc,
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    let s = streams(&tools, &report.video_ts.join("VIDEO_TS.VOB"));
    assert!(s.iter().any(|l| l == "mpeg2video,video,720,480"), "{s:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn invalid_layout_fails_before_encoding() {
    let Some((rt, _tools, dir, inputs)) = setup("invalid") else { return };
    let json = layout_json(&dir, 576).replace(r#""chapter": 2}}"#, r#""chapter": 42}}"#);
    let layout = CustomLayout::from_json(&json).unwrap();
    let opts = BuildOptions {
        inputs,
        output: dir.join("out"),
        custom_layout: Some(layout),
        chapters: dvd::ChapterSettings { interval_secs: 2.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let err = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_err().to_string();
    assert!(err.contains("Kapitel 42"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}
