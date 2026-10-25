//! Blu-ray menus from a [`CustomLayout`]: the HDMV navigation (movie objects) and the rendering of
//! every menu into a background clip description plus an Interactive Graphics stream.
//!
//! Disc structure: playlist `i` / clip `i` for menu `i` (layout order), then one playlist per
//! title. Movie objects: 0 First Play, 1 Top Menu, `2 + i` menu `i`, then one per title.
//! Registers: `GPR1` chapter mark the next title object starts at, `GPR2` play-all flag,
//! `GPR6` scratch. Only HDMV (no BD-J, no pop-up menu).

use std::path::{Path, PathBuf};

use image::RgbaImage;

use crate::author::{self, ButtonAction, ClipSource, MenuButton, MenuFrame, Navigation, PlaybackOptions, Rect};
use crate::bd::BdRate;
use crate::encoder::{CustomMenuMedia, MenuBackground, MenuLayer};
use crate::hdmv::{self, Cmp, MovieObject, Op};
use crate::igs::{self, Command, IgButton, IgMenu, IgObject};
use crate::layout::{
    bd_button_rect, button_colors, clip_of, element_name, fit_image, frame_box, frame_font_size, parse_color, rotate, Action,
    Content, CustomLayout, CustomMenu, LayoutError, LayoutTitle, MenuRole, MenuSound,
};
use crate::menu;

const MARK: u32 = 1;
const FLAG: u32 = 2;
const TMP: u32 = 6;

/// Menu clips of a still menu last this long (the playlist item then waits forever).
pub const STILL_SECS: f64 = 1.0;
/// Decoded IG objects must fit the 16 MiB object buffer.
const IG_OBJECT_BUDGET: u64 = 16 * 1024 * 1024;

/// Properties of a menu the navigation depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuTraits {
    pub moving: bool,
    pub has_buttons: bool,
}

/// Sound of a menu: the clip to use and a warning if a requested title has no audio.
fn menu_audio(menu: &CustomMenu, titles: &[LayoutTitle]) -> (Option<ClipSource>, Option<String>) {
    match &menu.audio {
        MenuSound::None => (None, None),
        MenuSound::File { path } => (Some(ClipSource { path: path.into(), start_secs: 0.0, has_audio: true }), None),
        MenuSound::Title { title, start_secs } => {
            let clip = clip_of(titles, *title, *start_secs);
            if clip.has_audio {
                (Some(clip), None)
            } else {
                (None, Some(format!("Titel {title} hat keinen Ton – Menü bleibt stumm")))
            }
        }
    }
}

pub fn traits(layout: &CustomLayout, titles: &[LayoutTitle], frame: MenuFrame) -> Vec<MenuTraits> {
    layout
        .menus
        .iter()
        .map(|m| {
            let moving = m.background.video.is_some()
                || m.elements.iter().any(|e| matches!(e.content, Content::Video { .. }))
                || menu_audio(m, titles).0.is_some();
            let has_buttons =
                m.elements.iter().any(|e| e.action.is_some() && bd_button_rect(e, layout.canvas, frame).is_some());
            MenuTraits { moving, has_buttons }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct BdNav {
    pub objects: Vec<MovieObject>,
    /// Movie object of title `n` (index titles).
    pub title_objects: Vec<u16>,
    /// Per menu, per element: the commands of a button (`None` for plain elements).
    pub commands: Vec<Vec<Option<Vec<Command>>>>,
}

pub const FIRST_PLAY_OBJECT: u16 = 0;
pub const TOP_MENU_OBJECT: u16 = 1;

/// Playlist (= clip) number of title `t` (1-based) on a disc with `menus` menus.
pub fn title_playlist(menus: usize, t: u32) -> u32 {
    menus as u32 + t - 1
}

/// Plans the movie objects. `layout` must have passed [`crate::layout::validate_bd`].
pub fn plan(layout: &CustomLayout, titles: &[LayoutTitle], traits: &[MenuTraits], opts: PlaybackOptions) -> BdNav {
    let (m_count, t_count) = (layout.menus.len(), titles.len());
    let menu_obj = |i: usize| 2 + i as u32;
    let title_obj = |t: usize| (2 + m_count + t - 1) as u32;
    let first_title_menu = layout.menus.iter().position(|m| m.role == MenuRole::Title).unwrap_or(0);
    let by_id = |id: &str| layout.menus.iter().position(|m| m.id == id).unwrap_or(first_title_menu);
    // Where a title's playback returns to: the first title menu that plays it, else the first.
    let main_menu = |t: u32| -> usize {
        layout
            .menus
            .iter()
            .position(|m| {
                m.role == MenuRole::Title
                    && m.elements.iter().any(|e| {
                        matches!(e.action, Some(Action::PlayTitle { title }) | Some(Action::PlayChapter { title, .. }) if title == t)
                    })
            })
            .unwrap_or(first_title_menu)
    };
    // The title's own menu (root menus before settings menus), if any.
    let own_menu = |t: u32| -> Option<usize> {
        [MenuRole::Root, MenuRole::Settings]
            .iter()
            .find_map(|&r| layout.menus.iter().position(|m| m.role == r && m.title == Some(t)))
    };

    // Titles are entered with JumpTitle (not JumpObject): libbluray only switches its current title on
    // that, and VLC hides the time display while the current title is an interactive one (menu/FP).
    let go = |jump: Command, mark: u32, flag: u32| vec![hdmv::mov(MARK, Op::Imm(mark)), hdmv::mov(FLAG, Op::Imm(flag)), jump];

    let mut commands = Vec::new();
    for m in &layout.menus {
        let mut per = Vec::new();
        for e in &m.elements {
            let Some(action) = &e.action else {
                per.push(None);
                continue;
            };
            let cmds = match action {
                Action::PlayAll => go(hdmv::jump_title(1), 0, 1),
                Action::PlayTitle { title } => go(hdmv::jump_title(*title as u32), 0, 0),
                Action::PlayChapter { title, chapter } => go(hdmv::jump_title(*title as u32), chapter - 1, 0),
                Action::Menu { menu } => vec![hdmv::jump_object(menu_obj(by_id(menu)))],
                Action::SetAudio { source } => {
                    let lt = m.title.and_then(|t| titles.get(t as usize - 1));
                    let n = lt.and_then(|lt| lt.audio_sources.iter().position(|s| s == source)).unwrap_or(0) as u32 + 1;
                    // The display flag is rewritten by SetStream: keep the player's current state.
                    vec![
                        hdmv::mov(TMP, Op::Psr(2)),
                        hdmv::and(TMP, Op::Imm(0x8000_0000)),
                        hdmv::compare(Op::Gpr(TMP), Cmp::Ne, Op::Imm(0)),
                        hdmv::set_stream(hdmv::audio_stream(n) | hdmv::PG_DISPLAY),
                        hdmv::compare(Op::Gpr(TMP), Cmp::Eq, Op::Imm(0)),
                        hdmv::set_stream(hdmv::audio_stream(n)),
                    ]
                }
                Action::SetSubtitle { source } => {
                    let lt = m.title.and_then(|t| titles.get(t as usize - 1));
                    match source.and_then(|s| lt.and_then(|lt| lt.subtitle_sources.iter().position(|x| *x == s))) {
                        Some(k) => vec![hdmv::set_stream(hdmv::pg_stream_on(k as u32 + 1))],
                        None => vec![hdmv::set_stream(0)],
                    }
                }
            };
            per.push(Some(cmds));
        }
        commands.push(per);
    }

    let mut objects = Vec::new();
    // First Play.
    let start = if opts.auto_start { hdmv::jump_title(1) } else { hdmv::jump_object(menu_obj(first_title_menu)) };
    objects.push(MovieObject { commands: go(start, 0, 0), resume: false });
    // Top Menu.
    objects.push(MovieObject { commands: vec![hdmv::jump_object(menu_obj(first_title_menu))], resume: false });
    // Menus: a still shows its single clip forever; a motion menu restarts its clip (the selected
    // button lives in PSR10, so it survives the loop) after selecting the first button once.
    for (i, t) in traits.iter().enumerate() {
        let pl = i as u32;
        let cmds = if t.moving {
            let first = if t.has_buttons { vec![hdmv::set_button_page(1)] } else { vec![] };
            let at = first.len() as u32;
            [first, vec![hdmv::play_pl(pl), hdmv::goto(at)]].concat()
        } else {
            vec![hdmv::play_pl(pl)]
        };
        objects.push(MovieObject { commands: cmds, resume: false });
    }
    // Titles: play from the mark in GPR1, then what the playback options say.
    for t in 1..=t_count {
        let pl = title_playlist(m_count, t as u32);
        let next = if t < t_count {
            Some(t + 1)
        } else if opts.loop_playback {
            Some(1)
        } else {
            None
        };
        let to_menu_idx = if !opts.return_to_title_menu { own_menu(t as u32) } else { None }.unwrap_or_else(|| main_menu(t as u32));
        let to_menu = hdmv::jump_object(menu_obj(to_menu_idx));
        let mut c = vec![hdmv::play_pl_at_mark(Op::Imm(pl), Op::Gpr(MARK)), hdmv::mov(MARK, Op::Imm(0))];
        if opts.play_sequentially {
            match next {
                Some(n) => c.push(hdmv::jump_title(n as u32)),
                None => c.extend([hdmv::mov(FLAG, Op::Imm(0)), to_menu]),
            }
        } else {
            let single_end = if opts.loop_playback { hdmv::jump_title(t as u32) } else { to_menu };
            match next {
                Some(n) => c.extend([
                    hdmv::compare(Op::Gpr(FLAG), Cmp::Eq, Op::Imm(1)),
                    hdmv::jump_title(n as u32),
                    single_end,
                ]),
                None => c.extend([hdmv::mov(FLAG, Op::Imm(0)), single_end]),
            }
        }
        objects.push(MovieObject { commands: c, resume: false });
    }
    BdNav { objects, title_objects: (1..=t_count).map(|t| title_obj(t) as u16).collect(), commands }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One menu ready for encoding and muxing.
#[derive(Debug, Clone)]
pub struct BdPage {
    pub media: CustomMenuMedia,
    pub audio: Option<ClipSource>,
    pub secs: f64,
    pub moving: bool,
    /// `None` for a menu without buttons.
    pub igs: Option<IgMenu>,
    pub warnings: Vec<String>,
}

/// BD frame-rate code of the ICS video descriptor.
pub fn frame_rate_code(rate: BdRate) -> u8 {
    match rate.fps {
        (24000, 1001) => 1,
        (24, 1) => 2,
        (25, 1) => 3,
        (30000, 1001) => 4,
        (50, 1) => 6,
        _ => 7,
    }
}

/// Alpha steps of the highlight palette (text edges keep their anti-aliasing).
const ALPHA_STEPS: u32 = 16;

#[derive(Default)]
struct Palette {
    entries: Vec<[u8; 4]>,
}

impl Palette {
    /// Index of `[r, g, b, a]` (index 0 = transparent); `None` when all 255 colours are used.
    fn index(&mut self, px: [u8; 4]) -> Option<u8> {
        if px[3] == 0 {
            return Some(0);
        }
        let a = ((px[3] as u32 * ALPHA_STEPS + 127) / 255).max(1) * 255 / ALPHA_STEPS;
        let key = [px[0], px[1], px[2], a as u8];
        match self.entries.iter().position(|e| *e == key) {
            Some(i) => Some(i as u8 + 1),
            None if self.entries.len() < 255 => {
                self.entries.push(key);
                Some(self.entries.len() as u8)
            }
            None => None,
        }
    }
}

struct Draw<'a> {
    layout: &'a CustomLayout,
    menu: &'a CustomMenu,
    titles: &'a [LayoutTitle],
    frame: MenuFrame,
}

fn io_err(p: &Path, e: &dyn std::fmt::Display) -> LayoutError {
    LayoutError(vec![format!("{}: {e}", p.display())])
}

/// Highlight picture of one button state: glyph shape for text, frame for everything else.
fn highlight_image(rect: Rect, cov: Option<&menu::Coverage>, origin: (i64, i64), color: [u8; 3], alpha: u8) -> RgbaImage {
    let (w, h) = (rect.x1 - rect.x0, rect.y1 - rect.y0);
    let mut img = RgbaImage::new(w, h);
    match cov {
        Some(cov) => {
            for py in 0..h {
                for px in 0..w {
                    let v = cov.at(rect.x0 as i64 + px as i64 - origin.0, rect.y0 as i64 + py as i64 - origin.1);
                    if v > 0.05 {
                        img.put_pixel(px, py, image::Rgba([color[0], color[1], color[2], (v.min(1.0) * alpha as f32).round() as u8]));
                    }
                }
            }
        }
        None => {
            let t = (w.min(h) / 20).clamp(3, 8);
            for py in 0..h {
                for px in 0..w {
                    if px < t || px >= w - t.min(w) || py < t || py >= h - t.min(h) {
                        img.put_pixel(px, py, image::Rgba([color[0], color[1], color[2], alpha]));
                    }
                }
            }
        }
    }
    img
}

/// Renders menu `index`: static layers (PNG), the IG stream data and the clip description.
#[allow(clippy::too_many_arguments)]
fn render_page(
    d: &Draw,
    commands: &[Option<Vec<Command>>],
    default_selected_none: bool,
    rate: BdRate,
    out_dir: &Path,
    prefix: &str,
    default_secs: f64,
) -> Result<BdPage, LayoutError> {
    let (layout, menu, frame) = (d.layout, d.menu, d.frame);
    let canvas = layout.canvas;
    let (fw, fh) = (frame.width, frame.height);
    let mut warnings = Vec::new();
    let mut layers: Vec<MenuLayer> = Vec::new();
    let mut current: Option<RgbaImage> = None;
    let mut layer_no = 0;
    let mut flush = |current: &mut Option<RgbaImage>, layers: &mut Vec<MenuLayer>| -> Result<(), LayoutError> {
        if let Some(img) = current.take() {
            let path = out_dir.join(format!("{prefix}_l{layer_no}.png"));
            layer_no += 1;
            img.save(&path).map_err(|e| io_err(&path, &e))?;
            layers.push(MenuLayer::Image(path));
        }
        Ok(())
    };
    let alpha = (layout.highlight.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;

    struct Pending {
        rect: Rect,
        selected: RgbaImage,
        activated: RgbaImage,
        element: usize,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for (n, e) in menu.elements.iter().enumerate() {
        let (x, y, w, h) = frame_box(e, canvas, frame);
        let (bw, bh) = if e.rotation % 180 == 90 { (h, w) } else { (w, h) };
        let opacity = (e.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut text_cov = None;
        match &e.content {
            Content::Text { text, font, size, color, align, bold, italic, shadow, wrap } => {
                let cov = menu::text_coverage(text, font, frame_font_size(*size, canvas, frame), *bold, *italic, *wrap, align, bw, bh);
                let mut img = RgbaImage::new(bw, bh);
                let [r, g, b] = parse_color(color).unwrap_or([255, 255, 255]);
                if *shadow {
                    menu::blend_coverage(&mut img, &cov, [0, 0, 0, 170], 2, 2, 255);
                }
                menu::blend_coverage(&mut img, &cov, [r, g, b, 255], 0, 0, 255);
                let img = rotate(img, e.rotation);
                menu::blit(current.get_or_insert_with(|| RgbaImage::new(fw, fh)), &img, x, y, opacity);
                text_cov = Some(cov);
            }
            Content::Image { src, fit } => {
                let src_img = image::open(src).map_err(|err| io_err(Path::new(src), &err))?.to_rgba8();
                let img = rotate(fit_image(&src_img, bw, bh, *fit, frame.pixel_aspect, e.rotation % 180 == 90), e.rotation);
                menu::blit(current.get_or_insert_with(|| RgbaImage::new(fw, fh)), &img, x, y, opacity);
            }
            Content::Video { title, start_secs, fit } => {
                flush(&mut current, &mut layers)?;
                layers.push(MenuLayer::Video {
                    clip: clip_of(d.titles, *title, *start_secs),
                    x: x.max(0) as u32,
                    y: y.max(0) as u32,
                    width: w,
                    height: h,
                    rotation: e.rotation,
                    fit: *fit,
                    opacity: e.opacity,
                });
            }
        }
        if commands.get(n).is_none_or(Option::is_none) {
            continue;
        }
        let Some(rect) = bd_button_rect(e, canvas, frame) else {
            warnings.push(format!("{}: Button liegt außerhalb des Bildes und wurde ausgelassen", element_name(e, n)));
            continue;
        };
        let (hl, sel) = button_colors(layout, e);
        let cov = text_cov.as_ref();
        pending.push(Pending {
            rect,
            selected: highlight_image(rect, cov, (x, y), hl, alpha),
            activated: highlight_image(rect, cov, (x, y), sel, alpha),
            element: n,
        });
    }
    flush(&mut current, &mut layers)?;

    // IG stream: palette, objects, buttons with their neighbours.
    let mut pal = Palette::default();
    let mut objects = Vec::new();
    let mut budget = 0u64;
    let mut buttons: Vec<MenuButton> = Vec::new();
    for (i, p) in pending.iter().enumerate() {
        let (w, h) = (p.rect.x1 - p.rect.x0, p.rect.y1 - p.rect.y0);
        for (k, img) in [&p.selected, &p.activated].into_iter().enumerate() {
            let mut px = Vec::with_capacity((w * h) as usize);
            for q in img.pixels() {
                let Some(idx) = pal.index(q.0) else {
                    return Err(LayoutError(vec![format!("Menü „{}“: zu viele verschiedene Button-Farben für die Blu-ray-Palette (255)", menu.name)]));
                };
                px.push(idx);
            }
            budget += w as u64 * h as u64;
            objects.push(IgObject { id: (2 * i + k) as u16, width: w as u16, height: h as u16, rle: igs::rle_encode(&px, w as usize, h as usize) });
        }
        buttons.push(MenuButton {
            name: format!("b{}", i + 1),
            template_index: p.element,
            location: "CUSTOM".into(),
            rect: p.rect,
            action: ButtonAction::Commands(String::new()),
            label: None,
            thumbnail: None,
            clip: None,
            nav: Navigation::default(),
        });
    }
    if budget > IG_OBJECT_BUDGET {
        return Err(LayoutError(vec![format!(
            "Menü „{}“: die Button-Grafiken sind zu groß für den Blu-ray-Speicher ({} MiB, max. 16) – kleinere Buttons verwenden",
            menu.name,
            budget / 1024 / 1024
        )]));
    }
    author::compute_navigation(&mut buttons);
    let id_of = |name: &Option<String>| name.as_ref().and_then(|n| n[1..].parse::<u16>().ok()).unwrap_or(0);
    let ig_buttons: Vec<IgButton> = pending
        .iter()
        .enumerate()
        .map(|(i, p)| IgButton {
            id: i as u16 + 1,
            x: p.rect.x0 as u16,
            y: p.rect.y0 as u16,
            up: id_of(&buttons[i].nav.up),
            down: id_of(&buttons[i].nav.down),
            left: id_of(&buttons[i].nav.left),
            right: id_of(&buttons[i].nav.right),
            selected: (2 * i) as u16,
            activated: (2 * i + 1) as u16,
            commands: commands[p.element].clone().unwrap_or_default(),
        })
        .collect();
    let igs = (!ig_buttons.is_empty()).then(|| {
        let mut palette = vec![[16u8, 128, 128, 0]; 256];
        for (i, e) in pal.entries.iter().enumerate() {
            let [y, cr, cb] = igs::rgb_to_ycrcb(e[0], e[1], e[2]);
            palette[i + 1] = [y, cr, cb, e[3]];
        }
        IgMenu {
            width: fw as u16,
            height: fh as u16,
            frame_rate_code: frame_rate_code(rate),
            default_selected: if default_selected_none { None } else { Some(1) },
            palette,
            buttons: ig_buttons,
            objects,
            pts: 0,
        }
    });

    let background = match (&menu.background.video, &menu.background.image) {
        (Some(v), _) => MenuBackground::Video { clip: clip_of(d.titles, v.title, v.start_secs), blur: menu.background.blur },
        (None, Some(img)) => MenuBackground::Image { path: PathBuf::from(img), blur: menu.background.blur },
        (None, None) => MenuBackground::Color(parse_color(&menu.background.color).unwrap_or([0, 0, 0])),
    };
    let (audio, warn) = menu_audio(menu, d.titles);
    warnings.extend(warn);
    let moving = matches!(background, MenuBackground::Video { .. })
        || layers.iter().any(|l| matches!(l, MenuLayer::Video { .. }))
        || audio.is_some();
    let secs = if moving { menu.duration_secs.unwrap_or(default_secs).clamp(1.0, 600.0) } else { STILL_SECS };
    Ok(BdPage {
        media: CustomMenuMedia { background, overlay: menu.background.overlay, layers },
        audio,
        secs,
        moving,
        igs,
        warnings,
    })
}

/// Renders every menu of the layout into `out_dir` (`bm<NN>_*` layer files).
pub fn render_pages(
    layout: &CustomLayout,
    titles: &[LayoutTitle],
    frame: MenuFrame,
    rate: BdRate,
    nav: &BdNav,
    out_dir: &Path,
    default_secs: f64,
) -> Result<Vec<BdPage>, LayoutError> {
    layout
        .menus
        .iter()
        .enumerate()
        .map(|(i, menu)| {
            let d = Draw { layout, menu, titles, frame };
            let mut page = render_page(&d, &nav.commands[i], true, rate, out_dir, &format!("bm{:02}", i + 1), default_secs)?;
            // A still menu selects its first button through the ICS (no loop that could reset it).
            if !page.moving {
                if let Some(ig) = &mut page.igs {
                    ig.default_selected = Some(1);
                }
            }
            page.warnings = page.warnings.into_iter().map(|w| format!("Menü „{}“: {w}", menu.name)).collect();
            Ok(page)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Background, Canvas, Element, Highlight};

    fn title(chapters: usize) -> LayoutTitle {
        LayoutTitle {
            label: "T".into(),
            chapters,
            duration_secs: 100.0,
            clip: ClipSource { path: "a.mkv".into(), start_secs: 10.0, has_audio: true },
            audio_sources: vec![1, 2],
            subtitle_sources: vec![3],
        }
    }

    fn button(x: f64, y: f64, action: Action) -> Element {
        Element {
            id: String::new(),
            x,
            y,
            w: 400.0,
            h: 100.0,
            rotation: 0,
            opacity: 1.0,
            highlight: None,
            group: None,
            content: Content::Text {
                text: "Play".into(),
                font: "Arial".into(),
                size: 48.0,
                color: "#ffffff".into(),
                align: "center".into(),
                bold: false,
                italic: false,
                shadow: false,
                wrap: true,
            },
            action: Some(action),
        }
    }

    fn menu(id: &str, role: MenuRole, title: Option<u32>, elements: Vec<Element>) -> CustomMenu {
        CustomMenu {
            id: id.into(),
            name: id.into(),
            role,
            title,
            background: Background::default(),
            duration_secs: None,
            audio: MenuSound::None,
            elements,
        }
    }

    fn layout() -> CustomLayout {
        CustomLayout {
            version: 1,
            canvas: Canvas { width: 1920, height: 1080 },
            highlight: Highlight::default(),
            menus: vec![
                menu("main", MenuRole::Title, None, vec![
                    button(100.0, 100.0, Action::PlayTitle { title: 1 }),
                    button(100.0, 300.0, Action::PlayChapter { title: 2, chapter: 3 }),
                    button(100.0, 500.0, Action::PlayAll),
                    button(100.0, 700.0, Action::Menu { menu: "set".into() }),
                ]),
                menu("set", MenuRole::Settings, Some(1), vec![
                    button(100.0, 100.0, Action::SetAudio { source: 2 }),
                    button(100.0, 300.0, Action::SetSubtitle { source: Some(3) }),
                    button(100.0, 500.0, Action::SetSubtitle { source: None }),
                ]),
            ],
        }
    }

    fn nav(opts: PlaybackOptions) -> BdNav {
        let l = layout();
        let t = [title(5), title(5)];
        let tr = [MenuTraits { moving: false, has_buttons: true }, MenuTraits { moving: true, has_buttons: true }];
        plan(&l, &t, &tr, opts)
    }

    fn cmds(n: &BdNav, menu: usize, el: usize) -> &[Command] {
        n.commands[menu][el].as_deref().unwrap()
    }

    #[test]
    fn object_numbers_and_buttons() {
        let n = nav(PlaybackOptions::default());
        // 0 FP, 1 TM, 2/3 menus, 4/5 titles
        assert_eq!(n.objects.len(), 6);
        assert_eq!(n.title_objects, vec![4, 5]);
        // Titles are only ever entered via JumpTitle, never JumpObject (VLC timebar).
        let all = n.objects.iter().flat_map(|o| o.commands.iter()).chain(n.commands.iter().flatten().flatten().flatten());
        assert!(all.into_iter().all(|c| *c != hdmv::jump_object(4) && *c != hdmv::jump_object(5)));
        // Play title 1: GPR1 = 0, GPR2 = 0, jump object 4.
        assert_eq!(cmds(&n, 0, 0), &[hdmv::mov(1, Op::Imm(0)), hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(1)]);
        // Chapter 3 of title 2: mark index 2.
        assert_eq!(cmds(&n, 0, 1), &[hdmv::mov(1, Op::Imm(2)), hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(2)]);
        // Play all: flag 1, title 1.
        assert_eq!(cmds(&n, 0, 2), &[hdmv::mov(1, Op::Imm(0)), hdmv::mov(2, Op::Imm(1)), hdmv::jump_title(1)]);
        assert_eq!(cmds(&n, 0, 3), &[hdmv::jump_object(3)]);
        // Subtitle off / on.
        assert_eq!(cmds(&n, 1, 2), &[hdmv::set_stream(0)]);
        assert_eq!(cmds(&n, 1, 1), &[hdmv::set_stream(0xC001)]);
        // Audio source 2 = second audio track; keeps the display flag.
        let a = cmds(&n, 1, 0);
        assert_eq!(a.len(), 6);
        assert_eq!(a[3], hdmv::set_stream(0x8002_0000 | 0x4000));
        assert_eq!(a[5], hdmv::set_stream(0x8002_0000));
    }

    #[test]
    fn menus_still_and_motion() {
        let n = nav(PlaybackOptions::default());
        assert_eq!(n.objects[2].commands, vec![hdmv::play_pl(0)]);
        assert_eq!(n.objects[3].commands, vec![hdmv::set_button_page(1), hdmv::play_pl(1), hdmv::goto(1)]);
        // Disc start: first title menu; Top menu likewise.
        assert_eq!(n.objects[0].commands[2], hdmv::jump_object(2));
        assert_eq!(n.objects[1].commands, vec![hdmv::jump_object(2)]);
        let auto = nav(PlaybackOptions { auto_start: true, ..PlaybackOptions::default() });
        assert_eq!(auto.objects[0].commands[2], hdmv::jump_title(1));
    }

    #[test]
    fn title_end_behaviour() {
        let play = hdmv::play_pl_at_mark(Op::Imm(2), Op::Gpr(1));
        let clear = hdmv::mov(1, Op::Imm(0));
        // Default: title 1 -> if play-all flag, title 2; else back to the main menu.
        let n = nav(PlaybackOptions::default());
        assert_eq!(
            n.objects[4].commands,
            vec![
                hdmv::play_pl_at_mark(Op::Imm(2), Op::Gpr(1)),
                clear,
                hdmv::compare(Op::Gpr(2), Cmp::Eq, Op::Imm(1)),
                hdmv::jump_title(2),
                hdmv::jump_object(2)
            ]
        );
        // Last title: reset flag, menu.
        assert_eq!(n.objects[5].commands[2..], [hdmv::mov(2, Op::Imm(0)), hdmv::jump_object(2)]);
        let _ = play;
        // Loop, not sequential: repeat each title; play-all still advances.
        let n = nav(PlaybackOptions { loop_playback: true, ..PlaybackOptions::default() });
        assert_eq!(n.objects[5].commands[2..], [hdmv::compare(Op::Gpr(2), Cmp::Eq, Op::Imm(1)), hdmv::jump_title(1), hdmv::jump_title(2)]);
        // Sequential: next title; the last one returns to the menu (or loops).
        let n = nav(PlaybackOptions { play_sequentially: true, ..PlaybackOptions::default() });
        assert_eq!(n.objects[4].commands[2..], [hdmv::jump_title(2)]);
        assert_eq!(n.objects[5].commands[2..], [hdmv::mov(2, Op::Imm(0)), hdmv::jump_object(2)]);
        let n = nav(PlaybackOptions { play_sequentially: true, loop_playback: true, ..PlaybackOptions::default() });
        assert_eq!(n.objects[5].commands[2..], [hdmv::jump_title(1)]);
        // Return to the title's own menu (title 1 has the settings menu, title 2 none).
        let n = nav(PlaybackOptions { return_to_title_menu: false, ..PlaybackOptions::default() });
        assert_eq!(n.objects[4].commands[4], hdmv::jump_object(3));
        assert_eq!(n.objects[5].commands[3], hdmv::jump_object(2));
    }

    #[test]
    fn page_rendering_makes_ig_objects() {
        let l = layout();
        let frame = MenuFrame::square(1920, 1080);
        let t = [title(5), title(5)];
        crate::layout::validate_bd(&l, &t, frame).unwrap();
        let tr = traits(&l, &t, frame);
        assert_eq!(tr[0], MenuTraits { moving: false, has_buttons: true });
        let nav = plan(&l, &t, &tr, PlaybackOptions::default());
        let dir = std::env::temp_dir().join(format!("bdmenu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let pages = render_pages(&l, &t, frame, rate, &nav, &dir, 10.0).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let ig = pages[0].igs.as_ref().unwrap();
        assert_eq!(ig.buttons.len(), 4);
        assert_eq!(ig.objects.len(), 8);
        assert_eq!(ig.default_selected, Some(1));
        assert_eq!(ig.frame_rate_code, 1);
        // Buttons are stacked vertically: down leads to the next, up from the first stays.
        assert_eq!((ig.buttons[0].up, ig.buttons[0].down), (1, 2));
        assert_eq!((ig.buttons[1].up, ig.buttons[1].down), (1, 3));
        assert_eq!(ig.buttons[0].commands.len(), 3);
        assert!(ig.palette[1][3] > 0 && ig.palette[0][3] == 0);
        assert_eq!(pages[0].secs, STILL_SECS);
        assert!(igs::build_sup(ig).len() > 100);
    }
}
