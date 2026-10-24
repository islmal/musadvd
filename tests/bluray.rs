//! Blu-ray output end to end: two clips (16:9 with 5.1 + stereo audio, an SRT and chapters; 4:3
//! with one stereo track) become ONE autoplaying playlist on a BD-25 (H.264 + AC-3 via tsMuxeR),
//! 720p NTSC, the `--single-video` check, and the UDF 2.50 image of the result.
//! Skipped (with a message) when ffmpeg or tsMuxeR are missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

use convertx_rust::author::{Resolution, TvStandard};
use convertx_rust::burner::udf250;
use convertx_rust::calculator::DiscType;
use convertx_rust::dvd::{self, BuildOptions, BuildReport, StreamSelection};
use convertx_rust::tools::Toolchain;

fn tools_pair() -> (tokio::runtime::Runtime, Toolchain) {
    tools().expect("tools were available a moment ago")
}

fn tools() -> Option<(tokio::runtime::Runtime, Toolchain)> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(Toolchain::detect()) {
        Ok(t) if t.tsmuxer.is_some() => Some((rt, t)),
        Ok(_) => {
            eprintln!("skipping: tsMuxeR not found");
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

/// `key=value` lines of the top-level streams selected by `select` (ffprobe also lists the
/// streams of the transport stream's programs, which would count everything twice).
fn probe(tools: &Toolchain, file: &Path, select: &str, entries: &str) -> String {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", select, "-show_entries", entries, "-of", "json"])
        .arg(file)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut text = String::new();
    for stream in json["streams"].as_array().into_iter().flatten() {
        for (k, v) in stream.as_object().unwrap() {
            text.push_str(&format!("{k}={}
", v.as_str().map_or_else(|| v.to_string(), str::to_string)));
        }
    }
    text
}

/// Clips in `dir`: `a.mkv` (12 s, 1280×720 25 fps, 5.1 + stereo AC-3, SRT) and `b.mkv` (6 s, 4:3).
fn make_clips(tools: &Toolchain, dir: &Path) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let srt = dir.join("a.srt");
    std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:03,000\nHallo Welt\n\n2\n00:00:04,000 --> 00:00:06,000\nZweite Zeile\n\n").unwrap();
    let (a, b) = (dir.join("a.mkv"), dir.join("b.mkv"));
    ffmpeg(
        tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=12", "-f", "lavfi", "-i", "sine=f=440:d=12,aformat=channel_layouts=5.1",
            "-f", "lavfi", "-i", "sine=f=660:d=12", "-i", srt.to_str().unwrap(), "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3",
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3", "-c:s", "srt", "-metadata:s:a:0", "language=ger",
            "-metadata:s:a:1", "language=eng", "-metadata:s:s:0", "language=ger", a.to_str().unwrap(),
        ],
    );
    ffmpeg(
        tools,
        &[
            "-f", "lavfi", "-i", "testsrc=s=640x480:r=25:d=6", "-f", "lavfi", "-i", "sine=f=330:d=6", "-c:v", "libx264", "-preset", "ultrafast",
            "-c:a", "ac3", "-metadata:s:a:0", "language=ger", b.to_str().unwrap(),
        ],
    );
    (a, b)
}

fn build(rt: &tokio::runtime::Runtime, opts: &BuildOptions) -> Result<BuildReport, dvd::BuildError> {
    rt.block_on(dvd::build_dvd(opts, &mut |_| {}))
}

/// The two-clip Blu-ray, built once for the tests that inspect it.
fn fixture() -> Option<&'static (PathBuf, BuildReport)> {
    static BD: OnceLock<Option<(PathBuf, BuildReport)>> = OnceLock::new();
    BD.get_or_init(|| {
        let (rt, tools) = tools()?;
        let dir = std::env::temp_dir().join(format!("cx_bluray_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = make_clips(&tools, &dir.join("clips"));
        let opts = BuildOptions {
            inputs: vec![a, b],
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd1080,
            fast_encode: true,
            project_name: Some("BDTEST".into()),
            selections: vec![
                StreamSelection { audio: Some(vec![1, 2]), subtitles: Some(vec![3]), label: None, chapters: Some(vec![0.0, 6.0]) },
                StreamSelection::default(),
            ],
            ..BuildOptions::default()
        };
        let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
        Some((dir, report))
    })
    .as_ref()
}

#[test]
fn two_clips_make_one_autoplay_playlist() {
    let Some((_, tools)) = tools() else { return };
    let Some((_, report)) = fixture() else { return };
    let bd = report.project_dir.as_path();
    assert_eq!(report.video_ts, bd.join("BDMV"));
    for f in ["BDMV/index.bdmv", "BDMV/MovieObject.bdmv", "BDMV/PLAYLIST/00000.mpls", "BDMV/CLIPINF/00000.clpi", "BDMV/STREAM/00000.m2ts"] {
        assert!(bd.join(f).is_file(), "{f} missing");
    }
    assert!(bd.join("CERTIFICATE").is_dir());
    assert_eq!((report.menu_pages, report.resolution), (0, Resolution::Hd1080));
    assert_eq!(report.titles.len(), 2);
    assert!(report.warnings.iter().any(|w| w.contains("kein Menü")) || report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(report.work_dir.is_none(), "work directory removed");

    // One m2ts for both titles: H.264 High@4.1 1080, two AC-3 tracks, one PGS track.
    let m2ts = bd.join("BDMV/STREAM/00000.m2ts");
    let v = probe(&tools, &m2ts, "v:0", "stream=codec_name,profile,level,width,height,r_frame_rate");
    for want in ["codec_name=h264", "profile=High", "level=41", "width=1920", "height=1080", "r_frame_rate=25/1"] {
        assert!(v.contains(want), "{want} in {v}");
    }
    let audio = probe(&tools, &m2ts, "a", "stream=codec_name,channels");
    assert_eq!(audio.matches("codec_name=ac3").count(), 2, "{audio}");
    assert!(audio.contains("channels=6") && audio.contains("channels=2"), "{audio}");
    assert_eq!(probe(&tools, &m2ts, "s", "stream=codec_name").matches("hdmv_pgs_subtitle").count(), 1);
    let dur: f64 = String::from_utf8_lossy(
        &Command::new(&tools.ffprobe).args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(&m2ts).output().unwrap().stdout,
    )
    .trim()
    .parse()
    .unwrap();
    assert!((dur - 18.0).abs() < 0.5, "12 s + 6 s, got {dur}");

    // B has one audio track only: the second one is silent while B plays, A's is audible.
    let max_volume = |from: f64| -> f64 {
        let out = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-nostats", "-ss", &from.to_string(), "-i"])
            .arg(&m2ts)
            .args(["-map", "0:a:1", "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stderr).into_owned();
        let at = log.find("max_volume:").unwrap_or_else(|| panic!("no volumedetect in {log}"));
        log[at + 11..].trim_start().split_whitespace().next().unwrap().parse().unwrap()
    };
    assert!(max_volume(1.0) > -40.0, "A's stereo track is audible");
    assert!(max_volume(13.5) < -80.0, "B's filler track is silent");

    // Chapter marks: one per chapter of all titles (2 + 1), at 0 / 6 s / end of A.
    let mpls = std::fs::read(bd.join("BDMV/PLAYLIST/00000.mpls")).unwrap();
    let marks = {
        let at = u32::from_be_bytes(mpls[0x0c..0x10].try_into().unwrap()) as usize;
        let n = u16::from_be_bytes(mpls[at + 4..at + 6].try_into().unwrap()) as usize;
        (0..n)
            .map(|i| u32::from_be_bytes(mpls[at + 6 + i * 14 + 4..at + 6 + i * 14 + 8].try_into().unwrap()) as f64 / 45_000.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(marks.len(), 3, "tsMuxeR adds no mark of its own: {marks:?}");
    let start = marks[0];
    assert!((marks[1] - start - 6.0).abs() < 0.2 && (marks[2] - start - 12.0).abs() < 0.2, "{marks:?}");
}

#[test]
fn hd720_ntsc_single_clip() {
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_720_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (a, _) = make_clips(&tools, &dir.join("clips"));
    let opts = BuildOptions {
        inputs: vec![a],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd720,
        tv_standard: TvStandard::Ntsc,
        fast_encode: true,
        selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), label: None, chapters: None }],
        ..BuildOptions::default()
    };
    let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
    let m2ts = report.project_dir.join("BDMV/STREAM/00000.m2ts");
    let v = probe(&tools, &m2ts, "v:0", "stream=width,height,r_frame_rate,profile,level");
    for want in ["width=1280", "height=720", "r_frame_rate=60000/1001", "profile=High", "level=41"] {
        assert!(v.contains(want), "{want} in {v}");
    }
    assert!(probe(&tools, &m2ts, "s", "stream=codec_name").trim().is_empty(), "no subtitles selected");
    assert_eq!(probe(&tools, &m2ts, "a", "stream=codec_name").matches("ac3").count(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// NVENC on (the app default): with a working GPU/driver the encode uses it, otherwise it falls back
/// to x264 with a warning; either way the result is the same High@4.1 stream.
#[test]
fn hw_encode_uses_nvenc_or_falls_back() {
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_nvenc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (a, _) = make_clips(&tools, &dir.join("clips"));
    let opts = BuildOptions {
        inputs: vec![a],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd720,
        fast_encode: true,
        bd_hw_encode: true,
        selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), label: None, chapters: None }],
        ..BuildOptions::default()
    };
    let (mut steps, mut warnings) = (Vec::new(), Vec::new());
    let report = rt
        .block_on(dvd::build_dvd(&opts, &mut |e| match e {
            dvd::Event::Step(s) => steps.push(s),
            dvd::Event::Warning(w) => warnings.push(w),
            _ => {}
        }))
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    let nvenc = rt.block_on(tools.probe_nvenc());
    match &nvenc {
        Ok(()) => assert!(steps.iter().any(|s| s.contains("NVENC (h264_nvenc)")), "{steps:?}"),
        Err(reason) => assert!(warnings.iter().any(|w| w.contains("x264") && w.contains(reason.as_str())), "{reason}: {warnings:?}"),
    }
    let m2ts = report.project_dir.join("BDMV/STREAM/00000.m2ts");
    let v = probe(&tools, &m2ts, "v:0", "stream=codec_name,width,height,profile,level");
    for want in ["codec_name=h264", "width=1280", "profile=High", "level=41"] {
        assert!(v.contains(want), "{want} in {v}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn wrong_options_fail_before_any_work() {
    let dir = std::env::temp_dir().join(format!("cx_bluray_opts_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let base = BuildOptions {
        inputs: vec!["a.mkv".into(), "b.mkv".into()],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd1080,
        ..BuildOptions::default()
    };
    // --single-video with two inputs.
    let err = build(&rt, &BuildOptions { bd_single_video: true, ..base.clone() }).unwrap_err().to_string();
    assert!(err.contains("Nur ein Video pro Blu-ray") && err.contains('2'), "{err}");
    // DVD resolutions on a BD and HD resolutions on a DVD.
    let err = build(&rt, &BuildOptions { resolution: Resolution::FullD1, ..base.clone() }).unwrap_err().to_string();
    assert!(err.contains("Full D1") && err.contains("1080p"), "{err}");
    let err = build(&rt, &BuildOptions { disc: DiscType::Dvd5, ..base.clone() }).unwrap_err().to_string();
    assert!(err.contains("1080p") && err.contains("nur für Blu-ray"), "{err}");
    let err = build(&rt, &BuildOptions { disc: DiscType::Dvd9, resolution: Resolution::Hd720, ..base }).unwrap_err().to_string();
    assert!(err.contains("720p"), "{err}");
    assert!(!dir.exists(), "nothing was created");
}

/// Asserts that `dst` holds the same directories and byte-identical files as `src`.
#[cfg(windows)]
fn compare_tree(src: &Path, dst: &Path, files: &mut usize) {
    assert!(dst.is_dir(), "{} missing on the disc", dst.display());
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.path().is_dir() {
            compare_tree(&e.path(), &to, files);
        } else {
            assert_eq!(std::fs::read(e.path()).unwrap(), std::fs::read(&to).unwrap(), "{} differs", to.display());
            *files += 1;
        }
    }
}

#[cfg(windows)]
struct Mounted(PathBuf);

#[cfg(windows)]
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Dismount-DiskImage -ImagePath '{}' | Out-Null", self.0.display())])
            .status();
    }
}

#[test]
fn udf_250_image_of_the_build() {
    let Some((_, tools)) = tools() else { return };
    let Some((dir, report)) = fixture() else { return };
    let iso = dir.join("bdtest.iso");
    let plan = udf250::create_bd_image(&report.project_dir, &iso, "BDTEST", &AtomicBool::new(false), |_| {}).unwrap();
    let img = std::fs::read(&iso).unwrap();
    assert_eq!(img.len() as u64, plan.size_bytes());
    // The volume descriptor: revision 2.50, physical + metadata partition map.
    let lvd = &img[35 * 2048..36 * 2048];
    assert_eq!(u16::from_le_bytes([lvd[216 + 24], lvd[216 + 25]]), 0x0250, "domain identifier suffix");
    assert_eq!(u32::from_le_bytes(lvd[268..272].try_into().unwrap()), 2, "two partition maps");
    assert_eq!((lvd[440], lvd[446]), (1, 2), "physical map, then metadata map (type 2)");
    assert_eq!(&lvd[451..451 + 23], b"*UDF Metadata Partition");

    // ffmpeg's bluray protocol (libbluray + libudfread) is a real UDF 2.50 reader. libbluray
    // ignores playlists under 3 minutes, so a longer, trivially compressible project is used;
    // the plain folder is the control that shows the reader works at all.
    let protocols = Command::new(&tools.ffmpeg).args(["-hide_banner", "-protocols"]).output().unwrap();
    if String::from_utf8_lossy(&protocols.stdout).lines().any(|l| l.trim() == "bluray") {
        let (rt, _) = tools_pair();
        let long = dir.join("long");
        ffmpeg(&tools, &["-f", "lavfi", "-i", "color=c=0x2060a0:s=1280x720:r=25:d=200", "-f", "lavfi", "-i", "sine=f=220:d=200", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3", long.with_extension("mkv").to_str().unwrap()]);
        let opts = BuildOptions {
            inputs: vec![long.with_extension("mkv")],
            disc: DiscType::Bd25,
            output: long.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            project_name: Some("LONG".into()),
            ..BuildOptions::default()
        };
        let long_report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
        let long_iso = dir.join("long.iso");
        udf250::create_bd_image(&long_report.project_dir, &long_iso, "LONG", &AtomicBool::new(false), |_| {}).unwrap();
        let streams = |source: &Path| {
            let url = format!("bluray:{}", source.to_string_lossy().replace('\\', "/"));
            let out = Command::new(&tools.ffprobe)
                .args(["-v", "error", "-playlist", "0", "-show_entries", "stream=codec_name", "-of", "csv=p=0", &url])
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let (ok, text, err) = streams(&long_report.project_dir);
        assert!(ok && text.contains("h264"), "libbluray cannot read the folder: {text} {err}");
        let (ok, text, err) = streams(&long_iso);
        assert!(ok && text.contains("h264"), "libbluray cannot read the UDF 2.50 image: {text} {err}");
    } else {
        eprintln!("bluray protocol not in this ffmpeg build: libbluray check skipped");
    }

    #[cfg(windows)]
    {
        // Windows' own UDF driver mounts the image; every file is byte-identical.
        let script = format!(
            "$ErrorActionPreference='Stop'; $d = Mount-DiskImage -ImagePath '{}' -StorageType ISO -PassThru; $v = $d | Get-Volume; Write-Output \"$($v.DriveLetter)|$($v.FileSystem)\"",
            iso.display()
        );
        let _mounted = Mounted(iso.clone());
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().unwrap();
        if !out.status.success() {
            eprintln!("Mount-DiskImage failed, mount check skipped: {}", String::from_utf8_lossy(&out.stderr));
            return;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (letter, fs) = text.trim().split_once('|').unwrap_or_else(|| panic!("unexpected output {text}"));
        assert_eq!(fs, "UDF", "mounted as {text}");
        let root = PathBuf::from(format!("{letter}:\\"));
        let mut files = 0;
        for top in ["BDMV", "CERTIFICATE"] {
            compare_tree(&report.project_dir.join(top), &root.join(top), &mut files);
        }
        assert!(files >= 5, "{files} files compared");
    }
}

// ---------------------------------------------------------------------------
// Menus from the menu editor (HDMV / IGS)
// ---------------------------------------------------------------------------

mod menus {
    use super::*;
    use convertx_rust::layout::{Action, Background, Canvas, Content, CustomLayout, CustomMenu, Element, Highlight, MenuRole, MenuSound};

    fn button(y: f64, text: &str, action: Action) -> Element {
        Element {
            id: String::new(),
            content: Content::Text {
                text: text.into(),
                font: "Arial".into(),
                size: 54.0,
                color: "#ffffff".into(),
                align: "left".into(),
                bold: false,
                italic: false,
                shadow: true,
                wrap: false,
            },
            x: 160.0,
            y,
            w: 900.0,
            h: 90.0,
            rotation: 0,
            opacity: 1.0,
            action: Some(action),
            highlight: None,
            group: None,
        }
    }

    fn menu(id: &str, role: MenuRole, title: Option<u32>, elements: Vec<Element>) -> CustomMenu {
        CustomMenu {
            id: id.into(),
            name: id.into(),
            role,
            title,
            background: Background { color: "#102040".into(), ..Background::default() },
            duration_secs: None,
            audio: MenuSound::None,
            elements,
        }
    }

    /// Title menu, root menu of title 1 and its settings menu.
    fn layout() -> CustomLayout {
        CustomLayout {
            version: 1,
            canvas: Canvas { width: 1920, height: 1080 },
            highlight: Highlight::default(),
            menus: vec![
                menu("main", MenuRole::Title, None, vec![
                    button(200.0, "Film 1", Action::PlayTitle { title: 1 }),
                    button(320.0, "Film 2", Action::PlayTitle { title: 2 }),
                    button(440.0, "Alle abspielen", Action::PlayAll),
                    button(560.0, "Kapitel 2", Action::PlayChapter { title: 1, chapter: 2 }),
                ]),
                menu("root", MenuRole::Root, Some(1), vec![
                    button(200.0, "Abspielen", Action::PlayTitle { title: 1 }),
                    button(320.0, "Einstellungen", Action::Menu { menu: "set".into() }),
                ]),
                menu("set", MenuRole::Settings, Some(1), vec![
                    button(200.0, "Ton 2", Action::SetAudio { source: 2 }),
                    button(320.0, "Untertitel an", Action::SetSubtitle { source: Some(3) }),
                    button(440.0, "Untertitel aus", Action::SetSubtitle { source: None }),
                    button(560.0, "Zurück", Action::Menu { menu: "root".into() }),
                ]),
            ],
        }
    }

    /// First segment type of every PES that starts on `pid`.
    fn first_segment_types(m2ts: &Path, pid: u16) -> Vec<u8> {
        let d = std::fs::read(m2ts).unwrap();
        d.chunks_exact(192)
            .filter_map(|pkt| {
                let p = &pkt[4..];
                let this = (u16::from_be_bytes([p[1], p[2]])) & 0x1FFF;
                (this == pid && p[1] & 0x40 != 0).then(|| {
                    let off = if p[3] & 0x20 != 0 { 5 + p[4] as usize } else { 4 };
                    p[off + 9 + p[off + 8] as usize]
                })
            })
            .collect()
    }

    fn options(dir: &Path, inputs: Vec<PathBuf>, layout: CustomLayout) -> BuildOptions {
        BuildOptions {
            inputs,
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            bd_hw_encode: false,
            project_name: Some("BDMENU".into()),
            custom_layout: Some(layout),
            selections: vec![
                StreamSelection { audio: Some(vec![1, 2]), subtitles: Some(vec![3]), label: None, chapters: Some(vec![0.0, 6.0]) },
                StreamSelection::default(),
            ],
            ..BuildOptions::default()
        }
    }

    #[test]
    fn layout_builds_menus_and_navigation() {
        let Some((rt, tools)) = tools() else { return };
        let dir = std::env::temp_dir().join(format!("cx_bdmenu_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = make_clips(&tools, &dir.join("clips"));
        let report = build(&rt, &options(&dir, vec![a, b], layout())).unwrap_or_else(|e| panic!("build failed: {e}"));
        let bd = report.project_dir.as_path();
        assert_eq!(report.menu_pages, 3);
        // Playlists / clips: 0..2 menus, 3 and 4 titles.
        for n in 0..5 {
            for f in [format!("PLAYLIST/{n:05}.mpls"), format!("CLIPINF/{n:05}.clpi"), format!("STREAM/{n:05}.m2ts")] {
                assert!(bd.join("BDMV").join(&f).is_file(), "{f} missing");
                if !f.starts_with("STREAM") {
                    assert!(bd.join("BDMV/BACKUP").join(&f).is_file(), "BACKUP/{f} missing");
                }
            }
        }
        assert!(!bd.join("BDMV/STREAM/00005.m2ts").exists());

        // Menus: 720p H.264 + AC-3 + an IG stream (PID 0x1400; ICS = 0x18, PDS 0x14, ODS 0x15).
        for n in 0..3 {
            let m2ts = bd.join(format!("BDMV/STREAM/{n:05}.m2ts"));
            let v = probe(&tools, &m2ts, "v:0", "stream=codec_name,width,height");
            assert!(v.contains("codec_name=h264") && v.contains("width=1280"), "{v}");
            let ids = probe(&tools, &m2ts, "", "stream=id");
            assert!(ids.contains("id=0x1400"), "menu {n} has no IG stream: {ids}");
            let segs = first_segment_types(&m2ts, 0x1400);
            assert_eq!(segs.first(), Some(&0x18), "ICS first: {segs:?}");
            assert!(segs.contains(&0x14) && segs.contains(&0x15) && segs.last() == Some(&0x80), "{segs:?}");
            assert!(first_segment_types(&m2ts, 0x1200).is_empty(), "no PG PID left");
            // CLPI and MPLS (and their BACKUP copies) say IG, not PG.
            for dir in ["BDMV", "BDMV/BACKUP"] {
                let clpi = std::fs::read(bd.join(dir).join(format!("CLIPINF/{n:05}.clpi"))).unwrap();
                assert!(clpi.windows(4).any(|w| w == [0x14, 0x00, 0x15, 0x91]) && !clpi.windows(4).any(|w| w == [0x12, 0x00, 0x15, 0x90]));
                let mpls = std::fs::read(bd.join(dir).join(format!("PLAYLIST/{n:05}.mpls"))).unwrap();
                let item = u32::from_be_bytes(mpls[8..12].try_into().unwrap()) as usize + 10;
                assert_eq!(mpls[item + 31], 1, "still mode of a still menu");
                let stn = item + 34;
                assert_eq!((mpls[stn + 6], mpls[stn + 7]), (0, 1), "no PG, one IG");
            }
        }
        // Titles keep their subtitle track as PGS; title 1 has chapters 0 / 6 s (two marks).
        let t1 = bd.join("BDMV/STREAM/00003.m2ts");
        assert_eq!(probe(&tools, &t1, "s", "stream=codec_name").matches("hdmv_pgs_subtitle").count(), 1);
        let mpls = std::fs::read(bd.join("BDMV/PLAYLIST/00003.mpls")).unwrap();
        let marks = u32::from_be_bytes(mpls[12..16].try_into().unwrap()) as usize;
        assert_eq!(u16::from_be_bytes([mpls[marks + 4], mpls[marks + 5]]), 2, "PlayListMark count of title 1");

        // index.bdmv: First Play -> object 0, Top Menu -> object 1, titles -> objects 5 and 6.
        let index = std::fs::read(bd.join("BDMV/index.bdmv")).unwrap();
        assert_eq!(&index[..8], b"INDX0200");
        let obj = |at: usize| u16::from_be_bytes([index[at + 6], index[at + 7]]);
        assert_eq!((obj(0x52), obj(0x5E)), (0, 1));
        assert_eq!(u16::from_be_bytes([index[0x6A], index[0x6B]]), 2);
        assert_eq!((obj(0x6C), obj(0x78)), (5, 6));
        // MovieObject.bdmv: 2 + 3 menus + 2 titles = 7 objects.
        let mobj = std::fs::read(bd.join("BDMV/MovieObject.bdmv")).unwrap();
        assert_eq!(u16::from_be_bytes([mobj[0x30], mobj[0x31]]), 7);
        assert_eq!(std::fs::read(bd.join("BDMV/BACKUP/MovieObject.bdmv")).unwrap(), mobj);
        assert!(bd.join("CERTIFICATE/BACKUP").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_layouts_fail_before_any_work() {
        let Some((rt, tools)) = tools() else { return };
        let dir = std::env::temp_dir().join(format!("cx_bdmenu_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, _) = make_clips(&tools, &dir.join("clips"));
        let mut l = layout();
        l.canvas = Canvas { width: 720, height: 576 };
        let err = build(&rt, &options(&dir, vec![a], l)).unwrap_err().to_string();
        assert!(err.contains("16:9"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
