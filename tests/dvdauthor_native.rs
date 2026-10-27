//! The native DVD authoring (`author::author_dvd`: `dvdvm`, `dvd_mux`, `ifo`) must reproduce
//! dvdauthor 0.7.2+ byte for byte. The reference outputs were recorded once from `dvdauthor.exe`
//! and are checked in as MD5 + length per file (`tests/fixtures/dvdauthor/<case>.md5`, the IFOs
//! additionally as raw files for field diffs) next to the small input streams, so the tests need
//! no external tool.
//!
//! Re-recording (needs the dvdauthor build, e.g. `tools/oracle/build.sh`, and ffmpeg for missing
//! inputs): `DVDAUTHOR_RECORD=<path to dvdauthor.exe> cargo test --test dvdauthor_native`; this runs
//! the real tool, asserts native == reference and rewrites the fixtures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use convertx_rust::author::{
    self, ButtonAction, DvdLayout, MenuButton, MenuSpec, Navigation, PlaybackOptions, Rect, TitleSpec, TitlesetSpec, TvStandard, VobSpec,
};
use convertx_rust::menu::IndexedImage;
use convertx_rust::spumux;
use md5::{Digest, Md5};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn fixtures() -> PathBuf {
    root().join("tests/fixtures/dvdauthor")
}

fn record() -> Option<PathBuf> {
    std::env::var_os("DVDAUTHOR_RECORD").map(PathBuf::from)
}

fn ffmpeg() -> PathBuf {
    ["bin/ffmpeg.exe", "bin/ffmpeg"].iter().map(|p| root().join(p)).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// Input stream `name` (created with `ffmpeg_args` when recording and missing); the spumux
/// fixtures are shared.
fn input(name: &str, ffmpeg_args: &[&str]) -> PathBuf {
    for dir in [fixtures(), root().join("tests/fixtures/spumux")] {
        let p = dir.join(name);
        if p.is_file() {
            return p;
        }
    }
    let p = fixtures().join(name);
    assert!(record().is_some(), "missing fixture {name}");
    let st = Command::new(ffmpeg()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(ffmpeg_args).arg(&p).status().unwrap();
    assert!(st.success());
    p
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cx_dvdauthor_native_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn digest(bytes: &[u8]) -> String {
    let h: String = Md5::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    format!("{h} {}", bytes.len())
}

fn first_diff(a: &[u8], b: &[u8]) -> String {
    let n = a.len().min(b.len());
    match (0..n).find(|&i| a[i] != b[i]) {
        Some(i) => format!("first difference at byte {i:#x} (sector {}, offset {:#x}): expected {:02x} got {:02x}", i / 2048, i % 2048, a[i], b[i]),
        None => format!("equal prefix, lengths expected {} got {}", a.len(), b.len()),
    }
}

fn files_of(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    for e in std::fs::read_dir(dir.join("VIDEO_TS")).unwrap().flatten() {
        m.insert(e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap());
    }
    m
}

/// Runs the native authoring of `layout` in `work` and compares with the recorded reference.
fn check(name: &str, layout: &DvdLayout, work: &Path) {
    let native_dir = work.join("native");
    let log = author::author_dvd(layout, work, &native_dir).unwrap_or_else(|e| panic!("{name}: {e}"));
    let native = files_of(&native_dir);
    for (n, bytes) in &native {
        if let Some(base) = n.strip_suffix(".BUP") {
            assert_eq!(bytes, &native[&format!("{base}.IFO")], "{name}: {n} differs from its IFO");
        }
    }
    let md5_file = fixtures().join(format!("{name}.md5"));
    if let Some(exe) = record() {
        let mut reference_layout = layout.clone();
        reference_layout.dest = "ref".into();
        let xml = author::dvdauthor_xml(&reference_layout).unwrap();
        std::fs::write(work.join("p.xml"), &xml).unwrap();
        let out = Command::new(&exe).args(["-x", "p.xml"]).current_dir(work).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut reference = files_of(&work.join("ref"));
        for (n, bytes) in reference.iter_mut() {
            if n == "VIDEO_TS.VOB" || n.ends_with("_0.VOB") {
                strip_intro_buttons(bytes);
            }
        }
        assert_eq!(native.keys().collect::<Vec<_>>(), reference.keys().collect::<Vec<_>>(), "{name}: file lists differ\n{log:?}");
        let mut lines = String::new();
        let _ = std::fs::create_dir_all(fixtures().join(name));
        for (n, bytes) in &reference {
            assert!(native[n] == *bytes, "{name}: {n}: {}\n{log:?}", first_diff(bytes, &native[n]));
            lines.push_str(&format!("{n} {}\n", digest(bytes)));
            if n.ends_with(".IFO") {
                std::fs::write(fixtures().join(name).join(n), bytes).unwrap();
            }
        }
        std::fs::write(&md5_file, lines).unwrap();
    } else {
        let expected = std::fs::read_to_string(&md5_file).unwrap_or_else(|e| panic!("{}: {e}", md5_file.display()));
        let want: BTreeMap<&str, &str> = expected.lines().filter_map(|l| l.split_once(' ')).collect();
        assert_eq!(native.keys().map(String::as_str).collect::<Vec<_>>(), want.keys().copied().collect::<Vec<_>>(), "{name}: file lists differ");
        for (n, bytes) in &native {
            if digest(bytes) != want[n.as_str()] {
                let detail = std::fs::read(fixtures().join(name).join(n)).map(|r| first_diff(&r, bytes)).unwrap_or_default();
                panic!("{name}: {n} differs from the recorded dvdauthor output: {detail}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(work);
}

/// dvdauthor copies a menu's button info into the NAV pack of every VOBU, also those of an intro
/// cell (cell 1 of a VOB that has a cell 2); the native authoring leaves the highlight info of
/// those empty, so the reference is reduced the same way (the former post-pass of `dvd.rs`).
fn strip_intro_buttons(vob: &mut [u8]) {
    const HLI: std::ops::Range<usize> = 0x8D..0x8D + 189;
    let is_nav = |s: &[u8]| s[0x26..0x2A] == [0, 0, 1, 0xBF] && s[0x2C] == 0 && s[0x400..0x404] == [0, 0, 1, 0xBF];
    let id = |s: &[u8]| (u16::from_be_bytes([s[0x41F], s[0x420]]), s[0x422]);
    let with_intro: std::collections::HashSet<u16> = vob.chunks_exact(2048).filter(|s| is_nav(s)).map(id).filter(|&(_, cell)| cell > 1).map(|(v, _)| v).collect();
    for s in vob.chunks_exact_mut(2048).filter(|s| is_nav(s)) {
        let (v, cell) = id(s);
        if cell == 1 && with_intro.contains(&v) {
            s[HLI].fill(0);
        }
    }
}

fn copy_in(work: &Path, src: &Path, as_name: &str) {
    std::fs::copy(src, work.join(as_name)).unwrap();
}

fn title(files: &[(&str, &[&str])], aspect: &str, audio: &[(&str, Option<&str>)], subs: &[Option<&str>]) -> TitleSpec {
    TitleSpec {
        vobs: files.iter().map(|(f, c)| VobSpec { file: f.to_string(), chapters: c.iter().map(|s| s.to_string()).collect() }).collect(),
        label: "T".into(),
        aspect: aspect.into(),
        audio_langs: audio.iter().map(|a| a.1.map(str::to_string)).collect(),
        audio_formats: audio.iter().map(|a| a.0.to_string()).collect(),
        subtitle_langs: subs.iter().map(|s| s.map(str::to_string)).collect(),
        force_subtitle: false,
    }
}

fn layout(tv: TvStandard, vmgm_menus: Vec<MenuSpec>, titlesets: Vec<TitlesetSpec>, playback: PlaybackOptions) -> DvdLayout {
    DvdLayout { dest: "DVD".into(), menu_aspect: "4:3".into(), vmgm_menus, titlesets, playback, tv, title_pre: None }
}

fn plain(t: TitleSpec) -> TitlesetSpec {
    TitlesetSpec { menus: vec![], title: t, main_menu_page: 1 }
}

#[test]
fn single_title_pal_with_chapters() {
    let work = temp_dir("single");
    copy_in(&work, &input("in_audio_4s.mpg", &[]), "t1.mpg");
    let t = title(&[("t1.mpg", &["0", "0:02"])], "16:9", &[("ac3", Some("de"))], &[]);
    check("single", &layout(TvStandard::Pal, vec![], vec![plain(t)], PlaybackOptions::default()), &work);
}

#[test]
fn merged_title_of_two_vobs() {
    let work = temp_dir("merged");
    copy_in(&work, &input("in_audio_4s.mpg", &[]), "a.mpg");
    copy_in(&work, &input("in_audio_7s.mpg", &[]), "b.mpg");
    let t = title(&[("a.mpg", &["0", "0:02"]), ("b.mpg", &["0", "0:04"])], "4:3", &[("ac3", None)], &[]);
    check("merged", &layout(TvStandard::Pal, vec![], vec![plain(t)], PlaybackOptions { loop_playback: true, ..PlaybackOptions::default() }), &work);
}

#[test]
fn ntsc_title() {
    let work = temp_dir("ntsc");
    let src = input("in_ntsc_3s.mpg", &["-f", "lavfi", "-i", "testsrc=s=720x480:r=30000/1001", "-f", "lavfi", "-i", "sine=f=440:r=48000", "-t", "3", "-target", "ntsc-dvd", "-b:v", "300k", "-c:a", "ac3", "-b:a", "96k"]);
    copy_in(&work, &src, "t1.mpg");
    let t = title(&[("t1.mpg", &["0", "0:01"])], "4:3", &[("ac3", Some("en"))], &[]);
    check("ntsc", &layout(TvStandard::Ntsc, vec![], vec![plain(t)], PlaybackOptions::default()), &work);
}

// --- menus ---------------------------------------------------------------------------------

const SELECTION: [[u8; 4]; 4] = [[0, 0, 0, 0], [255, 255, 0, 200], [255, 255, 0, 150], [255, 255, 0, 80]];
const ACTION: [[u8; 4]; 4] = [[0, 0, 0, 0], [255, 128, 0, 200], [255, 128, 0, 150], [255, 128, 0, 80]];

fn button(name: &str, r: [u32; 4], action: ButtonAction) -> MenuButton {
    MenuButton {
        name: name.into(),
        template_index: 0,
        location: String::new(),
        rect: Rect { x0: r[0], y0: r[1], x1: r[2], y1: r[3] },
        action,
        label: None,
        thumbnail: None,
        clip: None,
        nav: Navigation::default(),
    }
}

/// 2×N grid of buttons.
fn grid(actions: Vec<ButtonAction>) -> Vec<MenuButton> {
    let mut v: Vec<MenuButton> = actions
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            let (col, row) = (i as u32 % 2, i as u32 / 2);
            button(&format!("b{}", i + 1), [80 + col * 300, 100 + row * 100, 340 + col * 300, 170 + row * 100], a)
        })
        .collect();
    author::compute_navigation(&mut v);
    v
}

fn mask(buttons: &[MenuButton], palette: [[u8; 4]; 4]) -> IndexedImage {
    let mut img = IndexedImage::new(720, 576, palette);
    for b in buttons {
        for y in b.rect.y0..b.rect.y1 {
            for x in b.rect.x0..b.rect.x1 {
                img.pixels[(y * 720 + x) as usize] = match (x / 3 + y / 2) % 5 {
                    0 | 1 => 1,
                    2 => 2,
                    _ => 0,
                };
            }
        }
    }
    img
}

/// Muxes the menu's subpicture into `src` as `out` in `work` (the pipeline's spumux step).
fn menu_vob(work: &Path, src: &Path, out: &str, tv: TvStandard, buttons: &[MenuButton], start: f64) {
    mask(buttons, SELECTION).write_png(&work.join(format!("{out}.hl.png"))).unwrap();
    mask(buttons, ACTION).write_png(&work.join(format!("{out}.sel.png"))).unwrap();
    let xml = author::spumux_xml(tv, &format!("{out}.hl.png"), &format!("{out}.sel.png"), buttons, start);
    let mut native = Vec::new();
    spumux::mux(std::fs::File::open(src).unwrap(), &mut native, spumux::parse_xml(&xml, work).unwrap(), 0).unwrap();
    std::fs::write(work.join(out), native).unwrap();
}

fn menu_spec(vob: &str, buttons: Vec<MenuButton>, entry: Option<&str>) -> MenuSpec {
    MenuSpec { vob: vob.into(), buttons, entry: entry.map(str::to_string), motion: false, pre: None, intro_secs: None }
}

fn menu_case(name: &str, playback: PlaybackOptions) {
    let work = temp_dir(name);
    let menu_src = input("in_menu.mpg", &[]);
    copy_in(&work, &input("in_audio_4s.mpg", &[]), "t1.mpg");
    copy_in(&work, &input("in_audio_7s.mpg", &[]), "t2.mpg");
    let main = grid(vec![ButtonAction::OpenTitleset(1), ButtonAction::OpenTitleset(2), ButtonAction::PlayAll]);
    menu_vob(&work, &menu_src, "main.vob", TvStandard::Pal, &main, 0.0);
    let mut sets = Vec::new();
    for n in 1..=2u32 {
        let root = grid(vec![ButtonAction::PlayTitle(1), ButtonAction::Menu(2), ButtonAction::MainMenu(1), ButtonAction::SetAudio(0)]);
        let chapters = grid(vec![ButtonAction::PlayChapter(1), ButtonAction::PlayChapter(2), ButtonAction::Menu(1), ButtonAction::SetSubtitle(None)]);
        menu_vob(&work, &menu_src, &format!("root{n}.vob"), TvStandard::Pal, &root, 0.0);
        menu_vob(&work, &menu_src, &format!("ch{n}.vob"), TvStandard::Pal, &chapters, 0.0);
        let t = title(&[(&format!("t{n}.mpg"), &["0", "0:02"])], "4:3", &[("ac3", Some("de"))], &[]);
        sets.push(TitlesetSpec {
            menus: vec![menu_spec(&format!("root{n}.vob"), root, Some("root")), menu_spec(&format!("ch{n}.vob"), chapters, Some("ptt"))],
            title: t,
            main_menu_page: 1,
        });
    }
    let l = layout(TvStandard::Pal, vec![menu_spec("main.vob", main, Some("title"))], sets, playback);
    check(name, &l, &work);
}

#[test]
fn still_menus_with_dispatchers() {
    menu_case("menus", PlaybackOptions::default());
}

#[test]
fn menus_with_playback_options() {
    menu_case("menus_seq", PlaybackOptions { auto_start: true, play_sequentially: true, loop_playback: true, return_to_title_menu: false });
}

#[test]
fn motion_menu_with_intro() {
    let work = temp_dir("motion");
    let menu_src = input("in_3s.mpg", &[]);
    copy_in(&work, &input("in_audio_4s.mpg", &[]), "t1.mpg");
    let main = grid(vec![ButtonAction::PlayTitle(1), ButtonAction::PlayAll]);
    menu_vob(&work, &menu_src, "main.vob", TvStandard::Pal, &main, 1.0);
    let mut spec = menu_spec("main.vob", main, Some("title"));
    spec.motion = true;
    spec.intro_secs = Some(1.0);
    let t = title(&[("t1.mpg", &["0"])], "4:3", &[("ac3", None)], &[]);
    check("motion", &layout(TvStandard::Pal, vec![spec], vec![plain(t)], PlaybackOptions::default()), &work);
}

#[test]
fn two_audio_and_two_subpicture_streams() {
    let work = temp_dir("av");
    let src = input(
        "in_ac3_dts_3s.mpg",
        &["-f", "lavfi", "-i", "testsrc=s=720x576:r=25", "-f", "lavfi", "-i", "sine=f=440:r=48000", "-f", "lavfi", "-i", "sine=f=660:r=48000", "-t", "3", "-map", "0:v", "-map", "1:a", "-map", "2:a", "-target", "pal-dvd", "-b:v", "300k", "-c:a:0", "ac3", "-b:a:0", "96k", "-c:a:1", "dts", "-strict", "-2", "-b:a:1", "384k"],
    );
    let pal = [[0, 0, 0, 0], [255, 255, 255, 255], [10, 10, 10, 255], [250, 200, 20, 255]];
    let mut stream = src.clone();
    for id in 0..2u8 {
        let mut img = IndexedImage::new(300, 40, pal);
        for y in 5..35 {
            for x in 10..290 {
                img.pixels[(y * 300 + x) as usize] = ((x / 7 + y / 3 + id as u32) % 4) as u8;
            }
        }
        img.write_png(&work.join(format!("s{id}.png"))).unwrap();
        let xml = format!(
            "<subpictures format=\"PAL\"><stream><spu start=\"00:00:00.500\" end=\"00:00:02.000\" image=\"s{id}.png\" xoffset=\"{}\" yoffset=\"400\"/></stream></subpictures>",
            60 + 40 * id
        );
        let mut out = Vec::new();
        spumux::mux(std::fs::File::open(&stream).unwrap(), &mut out, spumux::parse_xml(&xml, &work).unwrap(), id).unwrap();
        let name = work.join(format!("pass{id}.mpg"));
        std::fs::write(&name, out).unwrap();
        stream = name;
    }
    copy_in(&work, &stream, "t1.mpg");
    let t = title(&[("t1.mpg", &["0", "0:01"])], "16:9", &[("ac3", Some("de")), ("dts", Some("en"))], &[Some("de"), Some("en")]);
    check("av", &layout(TvStandard::Pal, vec![], vec![plain(t)], PlaybackOptions::default()), &work);
}
