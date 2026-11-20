//! Template menu intro on Blu-ray: packet-level checks of the muxed menu clip.
//!
//! Spike 1a: a menu clip is `[intro 0..T | loop T..]` with a playlist mark at `T`, and the loop
//! restarts with `PlayPLatMark(pl, 1)`. The player then starts reading at the EP-map entry of that
//! mark, so the IG display set (muxed by the native BDAV muxer) must not begin
//! before that packet, and its composition must be presented exactly at `T`.
//! Skipped (with a message) when ffmpeg is missing.

use std::path::Path;

use musadvd::author::MenuFrame;
use musadvd::bd::BdRate;
use musadvd::bd_author::menu_job;
use musadvd::config::HwAccelMode;
use musadvd::encoder::{bd_custom_menu_args, BackgroundOverlay, CustomMenuMedia, MenuBackground};
use musadvd::igs::{self, IgButton, IgMenu, IgObject};
use musadvd::tools::Toolchain;

const IG_PID: u16 = 0x1400;
const VIDEO_PID: u16 = 0x1011;
const PCR_PID: u16 = 0x1001;

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

fn be(d: &[u8], at: usize, n: usize) -> u64 {
    d[at..at + n].iter().fold(0, |v, &b| v << 8 | b as u64)
}

/// One PES start in the m2ts: packet number (SPN), PID, PTS / DTS (90 kHz).
#[derive(Debug, Clone, Copy)]
struct PesStart {
    spn: usize,
    pid: u16,
    pts: Option<u64>,
    /// Arrival time stamp of the packet (27 MHz, 30 bits).
    ats: u64,
}

fn ts_time(b: &[u8]) -> u64 {
    ((b[0] as u64 >> 1) & 0x07) << 30 | (be(b, 1, 2) >> 1) << 15 | be(b, 3, 2) >> 1
}

fn pes_starts(m2ts: &[u8]) -> Vec<PesStart> {
    m2ts.chunks_exact(192)
        .enumerate()
        .filter_map(|(spn, pkt)| {
            let p = &pkt[4..];
            let pid = (be(p, 1, 2) & 0x1FFF) as u16;
            if p[1] & 0x40 == 0 || p[3] & 0x10 == 0 {
                return None;
            }
            let off = if p[3] & 0x20 != 0 { 5 + p[4] as usize } else { 4 };
            if p.get(off..off + 3) != Some(&[0, 0, 1]) {
                return None;
            }
            let flags = p[off + 7] >> 6;
            let pts = (flags & 2 != 0).then(|| ts_time(&p[off + 9..]));
            Some(PesStart { spn, pid, pts, ats: be(pkt, 0, 4) & 0x3FFF_FFFF })
        })
        .collect()
}

/// System time clock (27 MHz) at which packet `spn` arrives: the last PCR before it plus the ATS
/// distance (`--no-pcr-on-video-pid` puts the PCR on its own PID).
fn arrival_stc(m2ts: &[u8], spn: usize) -> u64 {
    let ats = |i: usize| be(m2ts, i * 192, 4) & 0x3FFF_FFFF;
    let (i, pcr) = (0..=spn)
        .rev()
        .find_map(|i| {
            let p = &m2ts[i * 192 + 4..i * 192 + 192];
            let pcr_flag = (be(p, 1, 2) & 0x1FFF) as u16 == PCR_PID && p[3] & 0x20 != 0 && p[4] > 0 && p[5] & 0x10 != 0;
            pcr_flag.then(|| (i, (be(p, 6, 6) >> 15) * 300 + (be(p, 6, 6) & 0x1FF)))
        })
        .expect("PCR before the packet");
    pcr + (ats(spn).wrapping_sub(ats(i)) & 0x3FFF_FFFF)
}

/// EP map of `pid` from a CLPI: `(PTS 90 kHz, SPN)` per entry point (layout: libbluray `clpi_parse.c`).
fn ep_map(clpi: &[u8], pid: u16) -> Vec<(u64, usize)> {
    let cpi = be(clpi, 16, 4) as usize;
    let ep_map = cpi + 6;
    let streams = clpi[ep_map + 1] as usize;
    for s in 0..streams {
        let e = ep_map + 2 + s * 12;
        if be(clpi, e, 2) as u16 != pid {
            continue;
        }
        let bits = be(clpi, e + 2, 6);
        let (n_coarse, n_fine) = ((bits >> 18 & 0xFFFF) as usize, (bits & 0x3FFFF) as usize);
        let start = ep_map + be(clpi, e + 8, 4) as usize;
        let fine_at = start + be(clpi, start, 4) as usize;
        let coarse: Vec<(usize, u64, u64)> = (0..n_coarse)
            .map(|i| {
                let c = be(clpi, start + 4 + i * 8, 8);
                ((c >> 46) as usize, c >> 32 & 0x3FFF, c & 0xFFFF_FFFF)
            })
            .collect();
        return (0..n_fine)
            .map(|j| {
                let f = be(clpi, fine_at + j * 4, 4);
                let (fpts, fspn) = (f >> 17 & 0x7FF, f & 0x1FFFF);
                let (_, cpts, cspn) = *coarse.iter().rev().find(|c| c.0 <= j).expect("coarse entry");
                (((cpts & !1) << 19) + (fpts << 9), ((cspn & !0x1FFFF) + fspn) as usize)
            })
            .collect();
    }
    panic!("PID {pid:#x} not in the EP map");
}

/// Playlist marks of the first play item, in 90 kHz clip time, and the item's IN time.
fn mpls_marks(mpls: &[u8]) -> (u64, Vec<u64>) {
    let pl = be(mpls, 8, 4) as usize;
    let item = pl + 10;
    let in_time = be(mpls, item + 14, 4) * 2;
    let marks = be(mpls, 12, 4) as usize;
    let n = be(mpls, marks + 4, 2) as usize;
    (in_time, (0..n).map(|i| be(mpls, marks + 6 + i * 14 + 4, 4) * 2).collect())
}

/// A realistic IG display set: two 1920×400 objects with poorly compressible content (~1.5 MB),
/// like the selected/activated states of a template's thumbnail grid.
/// `sparse`: like a real highlight state instead (12 thin frames around thumbnails, the rest
/// transparent).
fn ig_menu(rate_code: u8, pts: u32, sparse: bool) -> IgMenu {
    let (w, h) = (1920usize, 400usize);
    let mut seed = 0x1234_5678u32;
    let mut px = vec![0u8; w * h];
    for (i, p) in px.iter_mut().enumerate() {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let (x, y) = (i % w, i / w);
        let edge = (x % 320 < 6 || x % 320 > 300) && y % 200 < 180 || (y % 200 < 6 || y % 200 > 174) && x % 320 <= 306;
        *p = if sparse { if edge { 1 } else { 0 } } else { 1 + (seed >> 24) as u8 % 3 };
    }
    let rle = igs::rle_encode(&px, w, h);
    IgMenu {
        width: 1920,
        height: 1080,
        frame_rate_code: rate_code,
        default_selected: None,
        palette: vec![[16, 128, 128, 0], [235, 128, 128, 255], [81, 90, 240, 255], [145, 54, 34, 255]],
        buttons: vec![IgButton { id: 1, x: 0, y: 600, up: 1, down: 1, left: 1, right: 1, selected: 1, activated: 2, commands: vec![] }],
        objects: vec![
            IgObject { id: 1, width: w as u16, height: h as u16, rle: rle.clone() },
            IgObject { id: 2, width: w as u16, height: h as u16, rle },
        ],
        pts,
    }
}

#[test]
fn ig_display_set_starts_behind_the_entry_point_of_the_intro_mark() {
    let Some((rt, tools)) = tools() else { return };
    let base = std::env::temp_dir().join(format!("convertx_menu_intro_{}", std::process::id()));
    let mut failures = Vec::new();
    let rates = [(BdRate { fps: (25, 1), fake_interlaced: true }, 3u8), (BdRate { fps: (24000, 1001), fake_interlaced: false }, 1)];
    for ((rate, code), sparse) in rates.into_iter().flat_map(|r| [(r, false), (r, true)]) {
        let dir = base.join(format!("r{}_{sparse}", rate.fps.0));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (num, den) = (rate.fps.0 as u64, rate.fps.1 as u64);
        let frames = (3.0 * num as f64 / den as f64).round() as u64;
        // T in 90 kHz, exact for both rates (whole frames).
        let t90 = frames * den * 90_000 / num;
        let t = t90 as f64 / 90_000.0;

        let media = CustomMenuMedia { background: MenuBackground::Color([0x20, 0x40, 0x80]), overlay: BackgroundOverlay::None, layers: vec![] };
        let mkv = dir.join("menu.mkv");
        let mut args = bd_custom_menu_args(&MenuFrame::square(1920, 1080), rate, &media, t + 2.0, None, None, &mkv);
        let out = args.pop().unwrap();
        args.extend(["-force_key_frames".into(), format!("{t:.6}").into(), out]);
        rt.block_on(tools.ffmpeg(&args, t + 2.0, |_| {})).expect("menu clip");

        std::fs::write(dir.join("menu.sup"), igs::build_sup(&ig_menu(code, t90 as u32, sparse))).unwrap();
        let bdmv = dir.join("BD").join("BDMV");
        musadvd::bd_mux::create_skeleton(&bdmv).unwrap();
        let job = menu_job(mkv, Some(dir.join("menu.sup")), 0, Some(t), false, rate, 1920, &bdmv, t + 2.0);
        rt.block_on(musadvd::bd_mux::mux_title(&tools, &job)).expect("mux");

        let report = check_clip(&bdmv, IG_PID, t90);
        eprintln!("[{num}/{den} sparse={sparse}] {}", report.0);
        failures.extend(report.1.into_iter().map(|f| format!("{num}/{den} sparse={sparse}: {f}")));
    }
    let _ = std::fs::remove_dir_all(&base);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Returns a diagnostic line and the list of violated conditions.
fn check_clip(bdmv: &Path, ig_pid: u16, t90: u64) -> (String, Vec<String>) {
    check_clip_of(bdmv, 0, ig_pid, t90)
}

/// [`check_clip`] for clip / playlist number `n`.
fn check_clip_of(bdmv: &Path, n: usize, ig_pid: u16, t90: u64) -> (String, Vec<String>) {
    let m2ts = std::fs::read(bdmv.join(format!("STREAM/{n:05}.m2ts"))).unwrap();
    let clpi = std::fs::read(bdmv.join(format!("CLIPINF/{n:05}.clpi"))).unwrap();
    let mpls = std::fs::read(bdmv.join(format!("PLAYLIST/{n:05}.mpls"))).unwrap();
    let pes = pes_starts(&m2ts);
    let (in_time, marks) = mpls_marks(&mpls);
    let ep = ep_map(&clpi, VIDEO_PID);
    let mut bad = Vec::new();

    let Some(&mark) = marks.get(1) else {
        return (format!("marks {marks:?}"), vec!["no second playlist mark".into()]);
    };
    if mark - in_time != t90 {
        bad.push(format!("mark 1 at {} (clip +{}), expected +{t90}", mark, mark - in_time));
    }
    // The player starts at the last entry point at or before the mark.
    let (ep_pts, ep_spn) = *ep.iter().rev().find(|e| e.0 <= mark).expect("EP entry before mark 1");
    // EP-map PTS keep only bits 9..32: the entry must be the video PES presented exactly at the mark.
    let video = pes.iter().find(|p| p.pid == VIDEO_PID && p.spn == ep_spn).copied();
    if mark - ep_pts >= 512 || video.and_then(|v| v.pts) != Some(mark) {
        bad.push(format!("entry point before mark 1 is not the keyframe at T (EP pts {ep_pts}, PES there {video:?})"));
    }
    let ig: Vec<&PesStart> = pes.iter().filter(|p| p.pid == ig_pid).collect();
    let first_ig = ig.first().map(|p| p.spn);
    match first_ig {
        None => bad.push("no IG/PG PES in the clip".into()),
        Some(s) if s < ep_spn => bad.push(format!("IG display set starts at SPN {s}, before the mark's entry point SPN {ep_spn} ({} packets early)", ep_spn - s)),
        Some(_) => {}
    }
    if let Some(p) = ig.first() {
        if p.pts != Some(mark) {
            bad.push(format!("ICS PTS {:?} != mark {mark} (diff {:?})", p.pts, p.pts.map(|v| v as i64 - mark as i64)));
        }
    }
    // Arrival of the IG display set relative to the keyframe at T (27 MHz ATS, wraps after 2^30).
    let ms = |ats: u64| video.map_or(0.0, |v| (ats.wrapping_sub(v.ats) & 0x3FFF_FFFF) as f64 / 27_000.0);
    let ig_bytes = ig.len();
    let last_ig_packet = m2ts.chunks_exact(192).enumerate().filter(|(_, p)| (be(&p[4..], 1, 2) & 0x1FFF) as u16 == ig_pid).last();
    // Buttons are ready on the first loop frame only if the whole display set has arrived before
    // the frame at the mark is presented.
    if let Some((spn, _)) = last_ig_packet {
        let lead_ms = (mark as f64 * 300.0 - arrival_stc(&m2ts, spn) as f64) / 27_000.0;
        if lead_ms <= 0.0 {
            bad.push(format!("IG display set complete {:.1} ms after the frame at T is presented", -lead_ms));
        }
    }
    let present_ms = |spn: usize| (mark as f64 * 300.0 - arrival_stc(&m2ts, spn) as f64) / 27_000.0;
    let line = format!(
        "keyframe@T arrives {:.1} ms before presentation; IG complete {:.1} ms before | mark +{} | EP spn {ep_spn} | IG: {ig_bytes} PES, first spn {:?} (+{:.1} ms), last packet spn {:?} (+{:.1} ms) | clip {} packets",
        present_ms(ep_spn),
        last_ig_packet.map_or(f64::NAN, |(i, _)| present_ms(i)),
        mark - in_time,
        ig.first().map(|p| p.spn),
        ig.first().map_or(0.0, |p| ms(p.ats)),
        last_ig_packet.map(|(i, _)| i),
        last_ig_packet.map_or(0.0, |(_, p)| ms(be(p, 0, 4) & 0x3FFF_FFFF)),
        m2ts.len() / 192
    );
    (line, bad)
}

/// Largest IG display set a menu may have. Spike 1a: tsMuxeR has the set complete >= 333 ms before
/// the frame at T. 256 KiB = 2.1 Mbit needs 131 ms at the 16 Mbit/s IG transport rate and 66 ms at a
/// 32 Mbit/s decoder, i.e. at least 2.5x headroom. Real templates: max ~81 KB (2026-10-26).
const IG_BUDGET: usize = 256 * 1024;

/// Size of the IG display set (`.sup` bytes) of every page of every installed ConvertX template,
/// rendered for Blu-ray with one full page of every kind (10 titles, 6 chapters, 3 audio / 3
/// subtitle tracks).
#[test]
fn real_template_ig_display_sets_fit_the_budget() {
    use musadvd::author::{ClipSource, PlaybackOptions};
    use musadvd::bd_menu;
    use musadvd::encoder::Aspect;
    use musadvd::layout::LayoutTitle;
    use musadvd::template::Template;

    let root = Path::new(r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates");
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let titles: Vec<LayoutTitle> = (0..10)
        .map(|i| LayoutTitle {
            label: format!("Titel {}", i + 1),
            chapters: if i == 0 { 6 } else { 1 },
            duration_secs: 600.0,
            clip: ClipSource { path: "a.mkv".into(), start_secs: 10.0, has_audio: true },
            audio_sources: vec![1, 2, 3],
            subtitle_sources: vec![4, 5, 6],
            audio_labels: vec![],
            subtitle_labels: vec![],
        })
        .collect();
    let groups: Vec<Vec<usize>> = titles.iter().map(|t| vec![t.chapters]).collect();
    let out = std::env::temp_dir().join(format!("convertx_ig_sizes_{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
    let frame = MenuFrame::square(1920, 1080);
    let mut sizes: Vec<(String, usize)> = Vec::new();
    for dir in std::fs::read_dir(root).unwrap().flatten().filter(|d| d.path().is_dir()) {
        for ini in std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "ini")) {
            let tpl = Template::load(&ini).unwrap();
            if tpl.pages.is_empty() {
                continue;
            }
            let samples: Vec<Vec<f64>> = titles.iter().map(|t| (0..t.chapters).map(|c| c as f64 * 20.0).collect()).collect();
            let (layout, _) = musadvd::bd_template::layout_from_template(&tpl, &ini, &titles, &vec![None; titles.len()], &samples, Aspect::Wide, &musadvd::dvd::ChapterSettings::default(), musadvd::menu::UiLang::DE).unwrap();
            let traits = bd_menu::traits(&layout, &titles, frame);
            let nav = bd_menu::plan(&layout, &titles, &traits, PlaybackOptions::default(), &groups);
            let pages = bd_menu::render_pages(&layout, &titles, frame, rate, &nav, &out, 25.0).unwrap();
            for (menu, page) in layout.menus.iter().zip(&pages) {
                if let Some(ig) = &page.igs {
                    sizes.push((format!("{} / {}", ini.file_name().unwrap().to_string_lossy(), menu.id), igs::build_sup(ig).len()));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&out);
    sizes.sort_by_key(|s| std::cmp::Reverse(s.1));
    for (name, bytes) in sizes.iter().take(8) {
        eprintln!("{bytes:>9} B  {name}");
    }
    eprintln!("{} pages, median {} B", sizes.len(), sizes.get(sizes.len() / 2).map_or(0, |s| s.1));
    let over: Vec<_> = sizes.iter().filter(|s| s.1 > IG_BUDGET).collect();
    assert!(over.is_empty(), "IG display sets over {IG_BUDGET} B: {over:?}");
}

// ---------------------------------------------------------------------------
// A whole build with an intro: seam, slot video, marks, audio, no frame files
// ---------------------------------------------------------------------------

mod build {
    use std::path::PathBuf;
    use std::process::Command;

    use musadvd::author::Resolution;
    use musadvd::calculator::DiscType;
    use musadvd::dvd::{self, BuildOptions, BuildReport, StreamSelection};
    use musadvd::layout::{
        Action, Background, Canvas, Content, CustomLayout, CustomMenu, Element, ElementKey, Highlight, MenuRole, MenuSound,
    };

    use super::*;

    const INTRO: f64 = 2.0;
    /// The page's loop (without the intro).
    const LOOP: f64 = 4.0;
    /// Source time of the sound and of the slot video.
    const SOURCE_START: f64 = 1.0;
    /// Video slot on the 1920×1080 canvas.
    const SLOT: (f64, f64, f64, f64) = (960.0, 300.0, 800.0, 450.0);

    fn element(content: Content, (x, y, w, h): (f64, f64, f64, f64), action: Option<Action>, intro: Vec<ElementKey>) -> Element {
        Element { id: String::new(), content, x, y, w, h, rotation: 0.0, opacity: 1.0, action, highlight: None, group: None, intro, easter_egg: false }
    }

    fn text(s: &str) -> Content {
        Content::Text { text: s.into(), font: "Arial".into(), size: 80.0, color: "#FFFFFF".into(), align: "center".into(), bold: true, italic: false, shadow: false, wrap: false }
    }

    /// One title menu: a fading headline, the clip zooming in from its centre, a button.
    fn layout(animations: bool, with_keys: bool) -> CustomLayout {
        layout_on(animations, with_keys, Background { color: "#102040".into(), ..Background::default() })
    }

    fn layout_on(animations: bool, with_keys: bool, background: Background) -> CustomLayout {
        let key = |t: f64, (x, y, w, h): (f64, f64, f64, f64), opacity: f32| ElementKey { t, x, y, w, h, opacity };
        let headline = (100.0, 100.0, 700.0, 120.0);
        let keys = |v: Vec<ElementKey>| if with_keys { v } else { Vec::new() };
        CustomLayout {
            version: 1,
            canvas: Canvas { width: 1920, height: 1080 },
            highlight: Highlight::default(),
            animations,
            menus: vec![CustomMenu {
                id: "main".into(),
                name: "main".into(),
                role: MenuRole::Title,
                title: None,
                background,
                duration_secs: Some(LOOP),
                audio: MenuSound::Title { title: 1, start_secs: Some(SOURCE_START) },
                intro_secs: with_keys.then_some(INTRO),
                elements: vec![
                    element(text("Hallo Intro"), headline, None, keys(vec![key(0.0, headline, 0.0), key(INTRO, headline, 1.0)])),
                    element(
                        Content::Video { title: 1, start_secs: Some(SOURCE_START), fit: musadvd::encoder::Fit::Cover, path: None },
                        SLOT,
                        None,
                        keys(vec![key(0.0, (SLOT.0 + SLOT.2 / 2.0, SLOT.1 + SLOT.3 / 2.0, 0.0, 0.0), 1.0), key(INTRO, SLOT, 1.0)]),
                    ),
                    element(text("Start"), (100.0, 400.0, 400.0, 100.0), Some(Action::PlayTitle { title: 1 }), Vec::new()),
                ],
            }],
            fast_play: None,
        }
    }

    struct Fixture {
        dir: PathBuf,
        report: BuildReport,
    }

    fn make_clip(tools: &Toolchain, dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let clip = dir.join("clip.mkv");
        let st = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=40", "-f", "lavfi", "-i", "sine=f=330:d=40"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(st.success());
        clip
    }

    fn build(rt: &tokio::runtime::Runtime, tools: &Toolchain, name: &str, layout: CustomLayout, menu_animations: bool) -> Fixture {
        let dir = std::env::temp_dir().join(format!("cx_intro_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let clip = make_clip(tools, &dir.join("clips"));
        let opts = BuildOptions {
            inputs: vec![clip],
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            hw_accel: HwAccelMode::None,
            keep_work: true,
            menu_animations,
            project_name: Some("INTRO".into()),
            custom_layout: Some(layout),
            selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), ..StreamSelection::default() }],
            ..BuildOptions::default()
        };
        let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
        Fixture { dir, report }
    }

    /// All decoded frames of the clip, gray, `w`×`h`.
    fn gray_frames(tools: &Toolchain, m2ts: &Path, w: usize, h: usize) -> Vec<Vec<u8>> {
        let out = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(m2ts)
            .args(["-map", "0:v:0", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "gray", "-"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout.chunks_exact(w * h).map(<[u8]>::to_vec).collect()
    }

    fn pcm(tools: &Toolchain, input: &Path, seek: Option<(f64, f64)>, stream: &str) -> Vec<f32> {
        let mut cmd = Command::new(&tools.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if let Some((from, len)) = seek {
            cmd.args(["-ss", &format!("{from:.6}"), "-t", &format!("{len:.6}")]);
        }
        let out = cmd.arg("-i").arg(input).args(["-map", stream, "-ac", "1", "-ar", "48000", "-f", "f32le", "-"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
    }

    /// Mean absolute difference of two frames inside / outside the box (in frame fractions).
    fn region_diff(a: &[u8], b: &[u8], (w, h): (usize, usize), slot: (f64, f64, f64, f64), inside: bool) -> f64 {
        let (x0, y0) = ((slot.0 / 1920.0 * w as f64) as usize, (slot.1 / 1080.0 * h as f64) as usize);
        let (x1, y1) = (((slot.0 + slot.2) / 1920.0 * w as f64) as usize, ((slot.1 + slot.3) / 1080.0 * h as f64) as usize);
        // Keep a margin: the edge pixels differ with the scaler.
        let (mut sum, mut n) = (0u64, 0u64);
        for y in 0..h {
            for x in 0..w {
                let in_box = x + 4 >= x0 && x <= x1 + 4 && y + 4 >= y0 && y <= y1 + 4;
                let on_edge = in_box && !(x >= x0 + 4 && x + 4 <= x1 && y >= y0 + 4 && y + 4 <= y1);
                if (inside && in_box && !on_edge) || (!inside && !in_box) {
                    sum += (a[y * w + x] as i32 - b[y * w + x] as i32).unsigned_abs() as u64;
                    n += 1;
                }
            }
        }
        sum as f64 / n.max(1) as f64
    }

    fn correlation(a: &[f32], b: &[f32]) -> f64 {
        let n = a.len().min(b.len());
        let dot: f64 = (0..n).map(|i| a[i] as f64 * b[i] as f64).sum();
        let (na, nb): (f64, f64) = ((0..n).map(|i| (a[i] as f64).powi(2)).sum(), (0..n).map(|i| (b[i] as f64).powi(2)).sum());
        dot / (na.sqrt() * nb.sqrt()).max(1e-12)
    }

    #[test]
    fn page_with_intro_is_one_seamless_clip_with_a_mark_at_the_loop() {
        let Some((rt, tools)) = tools() else { return };
        let Fixture { dir, report } = build(&rt, &tools, "seam", layout(true, true), true);
        let source = dir.join("clips/clip.mkv");
        let bdmv = report.project_dir.join("BDMV");
        let m2ts = bdmv.join("STREAM/00000.m2ts");

        // Two marks: 0 and T, with T = whole frames of the menu rate; the IG set sits behind the mark.
        let mpls = std::fs::read(bdmv.join("PLAYLIST/00000.mpls")).unwrap();
        let (in_time, marks) = mpls_marks(&mpls);
        assert_eq!(marks.len(), 2, "{marks:?}");
        let t90 = marks[1] - in_time;
        let rate_out = Command::new(&tools.ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0"])
            .arg(&m2ts)
            .output()
            .unwrap();
        let rate_text = String::from_utf8_lossy(&rate_out.stdout).lines().next().unwrap_or("").trim().to_string();
        let (num, den) = rate_text.split_once('/').map_or_else(|| (rate_text.parse::<f64>().unwrap(), 1.0), |(n, d)| (n.parse::<f64>().unwrap(), d.parse::<f64>().unwrap()));
        let fps = num / den;
        let t = t90 as f64 / 90_000.0;
        let n = (t * fps).round() as usize;
        assert!((t - INTRO).abs() < 0.05 && (t * fps - n as f64).abs() < 1e-6, "T = {t} s = {n} frames at {fps} fps");
        let (ig_report, ig_bad) = check_clip(&bdmv, IG_PID, t90);
        assert!(ig_bad.is_empty(), "{ig_bad:?} ({ig_report})");

        // Seam: the last intro frame and the first loop frame agree outside the slot ...
        let (w, h) = (320, 180);
        let frames = gray_frames(&tools, &m2ts, w, h);
        assert!(frames.len() >= n + 4, "{} frames", frames.len());
        let outside = region_diff(&frames[n - 1], &frames[n], (w, h), SLOT, false);
        assert!(outside <= 2.0, "static areas differ by {outside} at the seam");
        // ... and the slot video goes on as if nothing happened (compare with the motion of its neighbours).
        let seam = region_diff(&frames[n - 1], &frames[n], (w, h), SLOT, true);
        let motion = region_diff(&frames[n - 2], &frames[n - 1], (w, h), SLOT, true).max(region_diff(&frames[n], &frames[n + 1], (w, h), SLOT, true));
        assert!(seam <= 2.0 * motion + 1.0, "slot video jumps at the seam: {seam} vs {motion}");
        // The intro really animates: it starts without the slot and the headline.
        assert!(region_diff(&frames[0], &frames[n - 1], (w, h), SLOT, true) > 5.0, "slot zooms in");

        // Audio: the clip ends in the sound at [T - x, T], and nothing jumps in the last x seconds.
        let audio = pcm(&tools, &m2ts, None, "0:a:0");
        let x = 0.5;
        let len = ((INTRO + LOOP) * 48_000.0) as usize;
        assert!(audio.len() >= len, "{} samples", audio.len());
        let tail = &audio[len - (x * 48_000.0) as usize..len];
        let jump = tail.windows(2).map(|p| (p[1] - p[0]).abs()).fold(0.0f32, f32::max);
        assert!(jump <= 0.05, "sample jump {jump} in the crossfade");
        let win = 960; // 20 ms
        let end = &audio[len - win..len];
        let reference = pcm(&tools, &source, Some((SOURCE_START + t - 0.02, 0.02)), "0:a:0");
        // The player's restart lands a few ms off the AC-3 frame grid: look for the best lag.
        let best = (-144i64..=144)
            .map(|lag| {
                let from = (len as i64 - win as i64 + lag).max(0) as usize;
                correlation(&audio[from..from + win], &reference)
            })
            .fold(f64::MIN, f64::max);
        eprintln!("seam: static {outside:.2}, slot {seam:.2} (neighbours {motion:.2}); audio jump {jump:.4}, corr {best:.3}; T = {t:.3} s = {n} frames");
        assert!(best >= 0.9, "last 20 ms do not match the sound at the mark (corr {best})");
        let _ = end;

        // No intro frames on disk: the work folder holds only the page layers, never a frame sequence.
        let work = report.work_dir.clone().expect("kept work dir");
        let mut names = Vec::new();
        let mut stack = vec![work];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                } else {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        let pngs = names.iter().filter(|n| n.ends_with(".png")).count();
        assert!(pngs <= 2, "page layers only, found {pngs} png files: {names:?}");
        assert!(names.iter().all(|n| !(n.ends_with(".bmp") || n.ends_with(".rgba") || n.ends_with(".raw") || n.ends_with(".yuv") || n.contains("intro"))), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Setting off = the layout without any animation data: same navigation, same playlists, no extra mark.
    /// The intro is drawn over the page's own background, whatever it is: a blurred, dimmed picture
    /// or a moving clip, delivered frame by frame by the helper pass.
    #[test]
    fn intro_frames_start_from_the_picture_or_clip_background() {
        use musadvd::encoder::BackgroundOverlay;
        use musadvd::layout::VideoRef;
        let Some((rt, tools)) = tools() else { return };
        let picture = std::env::temp_dir().join(format!("cx_intro_bgpic_{}.png", std::process::id()));
        let st = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=s=1280x720:r=1:d=1", "-frames:v", "1"])
            .arg(&picture)
            .status()
            .unwrap();
        assert!(st.success());
        let backgrounds = [
            ("pic", Background { image: Some(picture.to_string_lossy().into_owned()), blur: 6.0, overlay: BackgroundOverlay::Dim, ..Background::default() }, false),
            ("clip", Background { video: Some(VideoRef { title: 1, start_secs: Some(SOURCE_START), path: None }), blur: 3.0, overlay: BackgroundOverlay::Vignette, ..Background::default() }, true),
        ];
        for (name, background, moving) in backgrounds {
            let Fixture { dir, report } = build(&rt, &tools, name, layout_on(true, true, background), true);
            let bdmv = report.project_dir.join("BDMV");
            let m2ts = bdmv.join("STREAM/00000.m2ts");
            let (in_time, marks) = mpls_marks(&std::fs::read(bdmv.join("PLAYLIST/00000.mpls")).unwrap());
            assert_eq!(marks.len(), 2, "{name}: {marks:?}");
            let n = ((marks[1] - in_time) as f64 / 90_000.0 * 50.0).round() as usize;
            let (w, h) = (320, 180);
            let frames = gray_frames(&tools, &m2ts, w, h);
            assert!(frames.len() >= n + 4, "{name}: {} frames", frames.len());
            // Outside the slot the intro's last frame continues into the loop: the picture is still
            // the picture; a clip background moves like its neighbouring frames do.
            let outside = region_diff(&frames[n - 1], &frames[n], (w, h), SLOT, false);
            if moving {
                // A 25 fps clip on a 50 fps menu moves every other frame: compare with the largest
                // step of the frames around the seam.
                let step = |k: usize| region_diff(&frames[k], &frames[k + 1], (w, h), SLOT, false);
                let motion = [n - 4, n - 3, n - 2, n, n + 1, n + 2].into_iter().map(step).fold(0.0, f64::max);
                assert!(outside <= 1.5 * motion + 0.5, "{name}: background jumps at the seam: {outside} vs {motion}");
            } else {
                assert!(outside <= 2.0, "{name}: background differs by {outside} at the seam");
            }
            // The very first frame already shows the background (not a flat colour).
            let first = &frames[0];
            let mean = first.iter().map(|&v| v as u64).sum::<u64>() as f64 / first.len() as f64;
            let spread = first.iter().map(|&v| (v as f64 - mean).abs()).sum::<f64>() / first.len() as f64;
            assert!(spread > 3.0, "{name}: first intro frame is flat (spread {spread})");
            let _ = std::fs::remove_dir_all(&dir);
        }
        let _ = std::fs::remove_file(&picture);
    }

    /// A real ConvertX template: its pages open with the intro, the disc gets the dispatcher and the
    /// IG display set of every intro page starts at its loop mark.
    #[test]
    fn template_pages_play_their_intro_on_a_real_disc() {
        let tpl = Path::new(r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\Open_Style.ini");
        if !tpl.is_file() {
            eprintln!("skipping: {} not found", tpl.display());
            return;
        }
        let Some((rt, tools)) = tools() else { return };
        let dir = std::env::temp_dir().join(format!("cx_intro_tpl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let clip = make_clip(&tools, &dir.join("clips"));
        let opts = BuildOptions {
            inputs: vec![clip],
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            hw_accel: HwAccelMode::None,
            menu_animations: true,
            template: tpl.to_path_buf(),
            motion_duration: Some(3.0),
            project_name: Some("TPL".into()),
            selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), chapters: Some(vec![0.0, 10.0, 20.0]), ..StreamSelection::default() }],
            ..BuildOptions::default()
        };
        let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
        let bdmv = report.project_dir.join("BDMV");
        let pages = report.menu_pages;
        assert!(pages >= 3, "title, root and chapter page: {pages}");
        // Every template page opens with its intro: loop mark behind 0, display set at the mark.
        for n in 0..pages {
            let mpls = std::fs::read(bdmv.join(format!("PLAYLIST/{n:05}.mpls"))).unwrap();
            let (in_time, marks) = mpls_marks(&mpls);
            assert_eq!(marks.len(), 2, "menu {n}: {marks:?}");
            let (line, bad) = check_clip_of(&bdmv, n, IG_PID, marks[1] - in_time);
            assert!(bad.is_empty(), "menu {n}: {bad:?} ({line})");
        }
        // 2 + pages + 1 title + dispatcher.
        let mobj = std::fs::read(bdmv.join("MovieObject.bdmv")).unwrap();
        assert_eq!(u16::from_be_bytes([mobj[0x30], mobj[0x31]]) as usize, 2 + pages + 1 + 1);
        // The dispatcher is the last object: it ends in the register-operand jumps.
        use musadvd::hdmv::{jump_object_op, jump_title_op, Op};
        let at = |c: [u8; 12]| mobj.windows(12).any(|w| w == c);
        assert!(at(jump_object_op(Op::Gpr(5))) && at(jump_title_op(Op::Gpr(5))), "dispatcher jumps");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opting_out_builds_the_same_disc_as_a_layout_without_animation_data() {
        let Some((rt, tools)) = tools() else { return };
        let off = build(&rt, &tools, "off", layout(true, true), false);
        let bare = build(&rt, &tools, "bare", layout(true, false), true);
        for f in ["BDMV/index.bdmv", "BDMV/MovieObject.bdmv", "BDMV/PLAYLIST/00000.mpls", "BDMV/PLAYLIST/00001.mpls"] {
            let (a, b) = (off.report.project_dir.join(f), bare.report.project_dir.join(f));
            assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap(), "{f} differs");
        }
        let mpls = std::fs::read(off.report.project_dir.join("BDMV/PLAYLIST/00000.mpls")).unwrap();
        assert_eq!(mpls_marks(&mpls).1.len(), 1, "no loop mark without intro");
        let mobj = std::fs::read(off.report.project_dir.join("BDMV/MovieObject.bdmv")).unwrap();
        assert_eq!(u16::from_be_bytes([mobj[0x30], mobj[0x31]]), 4, "first play, top menu, menu, title: no dispatcher");
        for d in [off.dir, bare.dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

// ---------------------------------------------------------------------------
// DVD: template intro as a first cell without buttons
// ---------------------------------------------------------------------------

mod dvd {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use musadvd::author::TvStandard;
    use musadvd::calculator::DiscType;
    use musadvd::dvd::{self, BuildOptions, ChapterSettings};
    use musadvd::menu::UiLang;
    use musadvd::tools::Toolchain;

    const TEMPLATES: &str = r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style";

    /// One VOBU of a menu VOB: (VOB id, cell id, start PTM, hli_ss) read from its NAV pack.
    struct Vobu {
        vob: u16,
        cell: u8,
        ptm: u32,
        hli_ss: u16,
    }

    fn nav_packs(vob: &[u8]) -> Vec<Vobu> {
        let be = |s: &[u8], at: usize, n: usize| s[at..at + n].iter().fold(0u32, |v, &b| v << 8 | b as u32);
        vob.chunks_exact(2048)
            .filter(|s| s[0x26..0x2A] == [0, 0, 1, 0xBF] && s[0x2C] == 0x00 && s[0x400..0x404] == [0, 0, 1, 0xBF])
            .map(|s| Vobu { vob: be(s, 0x41F - 0, 2) as u16, cell: s[0x422], ptm: be(s, 0x39, 4), hli_ss: be(s, 0x8D, 2) as u16 & 3 })
            .collect()
    }

    fn run(name: &str, tv: TvStandard, motion: bool) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let Ok(tools) = rt.block_on(Toolchain::detect()) else { return eprintln!("skipping: tools missing") };
        let template = Path::new(TEMPLATES).join("Open_Style.ini");
        if !template.is_file() {
            return eprintln!("skipping: {} not found", template.display());
        }
        let dir: PathBuf = std::env::temp_dir().join(format!("cx_dvd_intro_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let st = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=6"])
            .args(["-f", "lavfi", "-i", "sine=f=440:duration=6", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
            .arg(&clip)
            .status()
            .unwrap();
        assert!(st.success());

        let opts = BuildOptions {
            inputs: vec![clip],
            disc: DiscType::Dvd5,
            template,
            output: dir.join("out"),
            no_subtitles: true,
            chapters: ChapterSettings { interval_secs: 2.0, ..Default::default() },
            ui_lang: UiLang::DE,
            keep_work: true,
            fast_encode: true,
            menu_animations: true,
            motion_menu: motion,
            motion_duration: Some(3.0),
            tv_standard: tv,
            ..Default::default()
        };
        let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
        let xml = &report.authoring_xml;
        assert!(xml.contains(r#"<cell start="0" end=""#), "the template has an intro: {xml}");
        assert_eq!(xml.contains("jump cell 2;"), motion, "{xml}");
        assert!(!motion || !xml.contains("jump cell 1;") || xml.contains("</vob>"));

        let mut checked = 0;
        for name in ["VIDEO_TS.VOB", "VTS_01_0.VOB"] {
            let bytes = std::fs::read(report.video_ts.join(name)).unwrap();
            let vobus = nav_packs(&bytes);
            assert!(!vobus.is_empty(), "{name}: no NAV packs found");
            let mut ids: Vec<u16> = vobus.iter().map(|v| v.vob).collect();
            ids.dedup();
            for id in ids {
                let cells: Vec<&Vobu> = vobus.iter().filter(|v| v.vob == id).collect();
                if !cells.iter().any(|v| v.cell == 2) {
                    continue; // a menu page without intro
                }
                let first = cells[0].ptm;
                let c2 = cells.iter().find(|v| v.cell == 2).unwrap();
                let t = (c2.ptm - first) as f64 / 90_000.0;
                assert!((t - 3.0).abs() < 0.1, "{name} vob {id}: cell 2 starts at {t} s");
                for v in &cells {
                    match v.cell {
                        1 => assert_eq!(v.hli_ss, 0, "{name} vob {id}: buttons during the intro"),
                        2 => assert_ne!(v.hli_ss, 0, "{name} vob {id}: no buttons in the page cell"),
                        c => panic!("cell {c}"),
                    }
                }
                checked += 1;
            }
        }
        assert!(checked > 0, "no intro menu found in the VOBs");
        let log = std::fs::read_to_string(report.work_dir.as_ref().unwrap().join("authoring.log")).unwrap();
        assert!(!log.contains("Discontinuity"), "{log}");
        if std::env::var_os("CX_KEEP").is_none() {
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn pal_still_menus_hold_the_page_after_the_intro() {
        run("pal", TvStandard::Pal, false);
    }

    #[test]
    fn ntsc_motion_menus_loop_the_page_cell() {
        run("ntsc", TvStandard::Ntsc, true);
    }
}
