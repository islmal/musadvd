//! Blu-ray menus from a [`CustomLayout`]: the HDMV navigation (movie objects) and the rendering of
//! every menu into a background clip description plus an Interactive Graphics stream.
//!
//! Disc structure: playlist `i` / clip `i` for menu `i` (layout order), then one playlist per
//! title. Movie objects: 0 First Play, 1 Top Menu, `2 + i` menu `i`, then one per title, then (only
//! if a page has an intro) the dispatcher.
//! Registers: `GPR1` chapter mark the next title object starts at, `GPR2` play-all flag,
//! `GPR6` scratch; with intros also `GPR3` entry (0 fresh, 1 paged: skip the intro), `GPR4`/`GPR5`
//! kind and index of the dispatcher's target, `GPR7` current page (hook for outros).
//! Only HDMV (no BD-J, no pop-up menu).

use std::path::{Path, PathBuf};

use image::{imageops, RgbaImage};

use crate::author::{self, ButtonAction, ClipSource, MenuButton, MenuFrame, Navigation, PlaybackOptions, Rect};
use crate::bd::BdRate;
use crate::encoder::{CustomMenuMedia, MenuBackground, MenuLayer};
use crate::hdmv::{self, Cmp, MovieObject, Op};
use crate::igs::{self, Command, IgButton, IgMenu, IgObject};
use crate::layout::{
    bd_button_rect, button_colors, clip_of, element_name, fit_image, frame_box, frame_font_size, is_unrotated,
    parse_color, rect_border, rect_image, rotate_coverage, rotate_rgba, video_clip, video_rotation_deg, Action,
    Canvas, Content, CustomLayout, CustomMenu, ElementKey, LayoutError, LayoutTitle, MenuRole, MenuSound,
};
use crate::menu;
use crate::menu_anim::{self, Intro, Pixels, Sprite};

const MARK: u32 = 1;
const FLAG: u32 = 2;
const ENTRY: u32 = 3;
const NEXT_KIND: u32 = 4;
const NEXT_INDEX: u32 = 5;
const TMP: u32 = 6;
const PAGE: u32 = 7;
/// Menu object a title's end returns to through the Top Menu title (0 = the first title menu):
/// movie titles may not use JumpObject, so they leave with JumpTitle 0 and Top Menu dispatches.
const RETURN_MENU: u32 = 8;

/// Audio / subtitle choice of a settings menu for entry `t`, applied by the title's objects right
/// before PlayPL: a SetStream in the menu is ignored by the PS4, whose check runs against the menu
/// playlist (one audio stream, no PG). 0 = no choice, else the stream number; subtitle "off" is
/// `SUB_OFF`.
fn audio_reg(t: u32) -> u32 {
    16 + 2 * (t - 1)
}
fn sub_reg(t: u32) -> u32 {
    audio_reg(t) + 1
}
const SUB_OFF: u32 = 0xFFFF;

/// Commands that apply the stored audio / subtitle choice of entry `t` (start of a title object).
fn apply_stream_choice(t: u32, audio: bool, sub: bool) -> Vec<Command> {
    let mut c = Vec::new();
    if audio {
        // The PG display flag is rewritten by SetStream: keep the player's current state.
        let a = Some(audio_reg(t));
        c.extend([
            hdmv::compare(Op::Gpr(audio_reg(t)), Cmp::Eq, Op::Imm(0)),
            hdmv::goto(8),
            hdmv::mov(TMP, Op::Psr(2)),
            hdmv::and(TMP, Op::Imm(0x8000_0000)),
            hdmv::compare(Op::Gpr(TMP), Cmp::Ne, Op::Imm(0)),
            hdmv::set_stream_regs(a, None, true),
            hdmv::compare(Op::Gpr(TMP), Cmp::Eq, Op::Imm(0)),
            hdmv::set_stream_regs(a, None, false),
        ]);
    }
    if sub {
        let at = c.len() as u32;
        c.extend([
            hdmv::compare(Op::Gpr(sub_reg(t)), Cmp::Eq, Op::Imm(SUB_OFF)),
            hdmv::set_stream(0),
            hdmv::compare(Op::Gpr(sub_reg(t)), Cmp::Eq, Op::Imm(0)),
            hdmv::goto(at + 6),
            hdmv::compare(Op::Gpr(sub_reg(t)), Cmp::Ne, Op::Imm(SUB_OFF)),
            hdmv::set_stream_regs(None, Some(sub_reg(t)), true),
        ]);
    }
    c
}

/// Targets of the dispatcher (`GPR4`).
const KIND_TITLE: u32 = 1;
const KIND_MENU: u32 = 2;
const KIND_PAGED: u32 = 3;

/// Menu clips of a still menu last this long (the playlist item then waits forever).
pub const STILL_SECS: f64 = 1.0;
/// Decoded IG objects must fit the 16 MiB object buffer.
const IG_OBJECT_BUDGET: u64 = 16 * 1024 * 1024;
/// Largest IG display set (`.sup` bytes) of a page with an intro. tsMuxeR has the set complete
/// 333 ms before the frame at the loop mark is shown (measured in `tests/menu_intro.rs`); 256 KiB
/// takes 131 ms at the 16 Mbit/s IG transport rate, so the buttons are there on that frame.
/// Real templates stay below 81 KB.
pub const IG_BUDGET: usize = 256 * 1024;

/// Properties of a menu the navigation depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuTraits {
    pub moving: bool,
    pub has_buttons: bool,
    /// The page opens with an intro animation (and is `moving`).
    pub intro: bool,
}

/// Menu id without its page suffix (`chap-1-p2` → `chap-1`): pages of one menu share it.
fn page_family(id: &str) -> &str {
    match id.rsplit_once("-p") {
        Some((head, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => id,
    }
}

/// Whether `menu` plays an intro: the layout allows animations and an element has keys.
pub fn has_intro(layout: &CustomLayout, menu: &CustomMenu) -> bool {
    layout.animations && menu.intro_secs.is_some_and(|s| s > 0.0) && menu.elements.iter().any(|e| !e.intro.is_empty())
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
                (None, Some(format!("Title {title} has no audio – the menu stays silent")))
            }
        }
    }
}

pub fn traits(layout: &CustomLayout, titles: &[LayoutTitle], frame: MenuFrame) -> Vec<MenuTraits> {
    layout
        .menus
        .iter()
        .map(|m| {
            let intro = has_intro(layout, m);
            let moving = intro
                || m.background.video.is_some()
                || m.elements.iter().any(|e| matches!(e.content, Content::Video { .. }))
                || menu_audio(m, titles).0.is_some();
            let has_buttons =
                m.elements.iter().any(|e| e.action.is_some() && bd_button_rect(e, layout.canvas, frame).is_some());
            MenuTraits { moving, has_buttons, intro }
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
///
/// `titles` are the layout titles (one per menu entry); `groups` lists, per entry, the chapter
/// count of each merged file. Every file is an HDMV title of its own (title numbers run over all
/// files); a file that is not the last of its entry jumps on to the next one when it ends.
pub fn plan(layout: &CustomLayout, titles: &[LayoutTitle], traits: &[MenuTraits], opts: PlaybackOptions, groups: &[Vec<usize>]) -> BdNav {
    let m_count = layout.menus.len();
    let t_count: usize = groups.iter().map(Vec::len).sum();
    // With intros every button that leaves a menu goes through the dispatcher object (last), which
    // tells the next menu whether to play its intro.
    let intro = traits.iter().any(|t| t.intro);
    let dispatch_obj = (2 + m_count + t_count) as u32;
    // HDMV title number of the first file of each entry (index = entry number - 1).
    let first_of: Vec<u32> = groups
        .iter()
        .scan(1u32, |next, g| {
            let first = *next;
            *next += g.len() as u32;
            Some(first)
        })
        .collect();
    let first_file = |entry: u32| first_of.get(entry as usize - 1).copied().unwrap_or(1);
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
    let via_dispatcher = |kind: u32, index: u32| {
        vec![hdmv::mov(NEXT_KIND, Op::Imm(kind)), hdmv::mov(NEXT_INDEX, Op::Imm(index)), hdmv::jump_object(dispatch_obj)]
    };
    let to_title = |n: u32| if intro { via_dispatcher(KIND_TITLE, n) } else { vec![hdmv::jump_title(n)] };
    let go = |jump: Vec<Command>, mark: u32, flag: u32| [vec![hdmv::mov(MARK, Op::Imm(mark)), hdmv::mov(FLAG, Op::Imm(flag))], jump].concat();

    let mut commands = Vec::new();
    for m in &layout.menus {
        let mut per = Vec::new();
        for e in &m.elements {
            let Some(action) = &e.action else {
                per.push(None);
                continue;
            };
            let cmds = match action {
                Action::PlayAll => go(to_title(1), 0, 1),
                Action::PlayTitle { title } => go(to_title(first_file(*title as u32)), 0, 0),
                Action::PlayChapter { title, chapter } => {
                    // Chapters run across the merged files: find the file and its local mark.
                    let mut local = chapter - 1;
                    let mut file = 0;
                    for (k, &n) in groups.get(*title as usize - 1).into_iter().flatten().enumerate() {
                        if local < n as u32 {
                            file = k;
                            break;
                        }
                        local -= n as u32;
                    }
                    go(to_title(first_file(*title as u32) + file as u32), local, 0)
                }
                Action::Menu { menu, .. } if intro => {
                    let target = by_id(menu);
                    // Next / previous page of the same menu: no intro again. Role and title are not enough: a
                    // title's root and chapter menus share both but are different menus.
                    let (to, from) = (&layout.menus[target], m);
                    let paged = to.role == from.role && to.title == from.title && page_family(&to.id) == page_family(&from.id);
                    via_dispatcher(if paged { KIND_PAGED } else { KIND_MENU }, menu_obj(target))
                }
                Action::Menu { menu, .. } => vec![hdmv::jump_object(menu_obj(by_id(menu)))],
                Action::SetAudio { source } => {
                    let lt = m.title.and_then(|t| titles.get(t as usize - 1));
                    let n = lt.and_then(|lt| lt.audio_sources.iter().position(|s| s == source)).unwrap_or(0) as u32 + 1;
                    vec![hdmv::mov(audio_reg(m.title.unwrap_or(1)), Op::Imm(n))]
                }
                Action::SetSubtitle { source } => {
                    let lt = m.title.and_then(|t| titles.get(t as usize - 1));
                    let pg = match source.and_then(|s| lt.and_then(|lt| lt.subtitle_sources.iter().position(|x| *x == s))) {
                        Some(k) => k as u32 + 1,
                        None => SUB_OFF,
                    };
                    vec![hdmv::mov(sub_reg(m.title.unwrap_or(1)), Op::Imm(pg))]
                }
                Action::CustomVm { .. } => vec![],
            };
            per.push(Some(cmds));
        }
        commands.push(per);
    }

    let mut objects = Vec::new();
    // First Play.
    let start = if opts.auto_start { hdmv::jump_title(1) } else { hdmv::jump_title(0) };
    objects.push(MovieObject { commands: [vec![hdmv::mov(RETURN_MENU, Op::Imm(0))], go(vec![start], 0, 0)].concat(), resume: false });
    // Top Menu: always a fresh entry; a title's end may have left the menu to return to.
    let mut top = Vec::new();
    if intro {
        top.push(hdmv::mov(ENTRY, Op::Imm(0)));
    }
    let at = top.len() as u32;
    top.extend([
        hdmv::compare(Op::Gpr(RETURN_MENU), Cmp::Eq, Op::Imm(0)),
        hdmv::goto(at + 5),
        hdmv::mov(TMP, Op::Gpr(RETURN_MENU)),
        hdmv::mov(RETURN_MENU, Op::Imm(0)),
        hdmv::jump_object_op(Op::Gpr(TMP)),
        hdmv::jump_object(menu_obj(first_title_menu)),
    ]);
    objects.push(MovieObject { commands: top, resume: false });
    // Menus restart their clip if it ever ends (a still waits forever, but the player may still
    // end the item); the selected button lives in PSR10, so it survives the loop after the first
    // button was selected once.
    for (i, t) in traits.iter().enumerate() {
        let pl = i as u32;
        let cmds = if t.intro {
            // Fresh entry: intro + first loop, then loop from the mark behind the intro. After
            // paging: straight to the loop (the intro never plays twice in a row).
            let mut c = vec![hdmv::mov(PAGE, Op::Imm(i as u32))];
            if t.has_buttons {
                c.push(hdmv::set_button_page(1));
            }
            let at = c.len() as u32;
            c.extend([
                hdmv::compare(Op::Gpr(ENTRY), Cmp::Eq, Op::Imm(1)),
                hdmv::goto(at + 4),
                hdmv::play_pl(pl),
                hdmv::goto(at + 5),
                hdmv::mov(ENTRY, Op::Imm(0)),
                hdmv::play_pl_at_mark(Op::Imm(pl), Op::Imm(1)),
                hdmv::goto(at + 5),
            ]);
            c
        } else {
            let first = if t.has_buttons { vec![hdmv::set_button_page(1)] } else { vec![] };
            let at = first.len() as u32;
            [first, vec![hdmv::play_pl(pl), hdmv::goto(at)]].concat()
        };
        objects.push(MovieObject { commands: cmds, resume: false });
    }
    // Titles (one per file): play from the mark in GPR1, then what the playback options say.
    let g_count = groups.len();
    let mut t = 0usize;
    for (g, members) in groups.iter().enumerate() {
        let g = g + 1;
        for k in 0..members.len() {
            t += 1;
            let pl = title_playlist(m_count, t as u32);
            let used = |audio: bool| {
                layout.menus.iter().filter(|m| m.title.unwrap_or(1) == g as u32).flat_map(|m| &m.elements).any(|e| match e.action {
                    Some(Action::SetAudio { .. }) => audio,
                    Some(Action::SetSubtitle { .. }) => !audio,
                    _ => false,
                })
            };
            let mut c = apply_stream_choice(g as u32, used(true), used(false));
            c.extend([hdmv::play_pl_at_mark(Op::Imm(pl), Op::Gpr(MARK)), hdmv::mov(MARK, Op::Imm(0))]);
            if k + 1 < members.len() {
                c.push(hdmv::jump_title(t as u32 + 1));
                objects.push(MovieObject { commands: c, resume: false });
                continue;
            }
            let next = if g < g_count {
                Some(first_file(g as u32 + 1))
            } else if opts.loop_playback {
                Some(first_file(1))
            } else {
                None
            };
            let to_menu_idx = if !opts.return_to_title_menu { own_menu(g as u32) } else { None }.unwrap_or_else(|| main_menu(g as u32));
            // Back to a menu through the Top Menu title (see RETURN_MENU).
            let to_menu = if to_menu_idx == first_title_menu {
                vec![hdmv::jump_title(0)]
            } else {
                vec![hdmv::mov(RETURN_MENU, Op::Imm(menu_obj(to_menu_idx))), hdmv::jump_title(0)]
            };
            if opts.play_sequentially {
                match next {
                    Some(n) => c.push(hdmv::jump_title(n)),
                    None => c.extend([vec![hdmv::mov(FLAG, Op::Imm(0))], to_menu].concat()),
                }
            } else {
                let single_end = if opts.loop_playback { vec![hdmv::jump_title(first_file(g as u32))] } else { to_menu };
                match next {
                    // The compare guards one command only: skip over the whole single end with a Goto.
                    Some(n) => {
                        let at = c.len() as u32;
                        c.extend([hdmv::compare(Op::Gpr(FLAG), Cmp::Ne, Op::Imm(1)), hdmv::goto(at + 3), hdmv::jump_title(n)]);
                        c.extend(single_end);
                    }
                    None => c.extend([vec![hdmv::mov(FLAG, Op::Imm(0))], single_end].concat()),
                }
            }
            objects.push(MovieObject { commands: c, resume: false });
        }
    }
    if intro {
        // Only a paging jump keeps the entry flag; titles and other menus start fresh.
        objects.push(MovieObject {
            commands: vec![
                hdmv::mov(ENTRY, Op::Imm(0)),
                hdmv::compare(Op::Gpr(NEXT_KIND), Cmp::Eq, Op::Imm(KIND_TITLE)),
                hdmv::goto(6),
                hdmv::compare(Op::Gpr(NEXT_KIND), Cmp::Eq, Op::Imm(KIND_PAGED)),
                hdmv::mov(ENTRY, Op::Imm(1)),
                hdmv::jump_object_op(Op::Gpr(NEXT_INDEX)),
                hdmv::jump_title_op(Op::Gpr(NEXT_INDEX)),
            ],
            resume: false,
        });
    }
    BdNav { objects, title_objects: (1..=t_count).map(|t| title_obj(t) as u16).collect(), commands }
}

/// Navigation of a disc without menus: First Play starts title 1, every title object plays its
/// own playlist and enters the next title with `JumpTitle`; the last one just ends. Playlist `t-1`
/// is title `t`.
pub fn plan_without_menus(titles: usize) -> BdNav {
    let mut objects = vec![
        MovieObject { commands: vec![hdmv::jump_title(1)], resume: false },
        MovieObject { commands: vec![hdmv::jump_title(1)], resume: false },
    ];
    for t in 1..=titles {
        let mut commands = vec![hdmv::play_pl(t as u32 - 1)];
        if t < titles {
            commands.push(hdmv::jump_title(t as u32 + 1));
        }
        objects.push(MovieObject { commands, resume: false });
    }
    BdNav { objects, title_objects: (0..titles).map(|t| 2 + t as u16).collect(), commands: Vec::new() }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One menu ready for encoding and muxing.
#[derive(Debug, Clone)]
pub struct BdPage {
    pub media: CustomMenuMedia,
    pub audio: Option<ClipSource>,
    /// Length of the clip: the loop, plus the intro in front of it if there is one.
    pub secs: f64,
    pub moving: bool,
    /// `None` for a menu without buttons.
    pub igs: Option<IgMenu>,
    /// Intro frames to composite (`secs` then covers intro + loop).
    pub intro: Option<Intro>,
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

struct Palette {
    entries: Vec<[u8; 4]>,
    /// Alpha levels (`ALPHA_STEPS`, or fewer to shrink the display set).
    steps: u32,
}

impl Palette {
    /// Index of `[r, g, b, a]` (index 0 = transparent); `None` when all 255 colours are used.
    fn index(&mut self, px: [u8; 4]) -> Option<u8> {
        if px[3] == 0 {
            return Some(0);
        }
        let a = ((px[3] as u32 * self.steps + 127) / 255).max(1) * 255 / self.steps;
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
    /// The page plays its intro (see [`has_intro`]).
    intro: bool,
}

fn io_err(p: &Path, e: &dyn std::fmt::Display) -> LayoutError {
    LayoutError(vec![format!("{}: {e}", p.display())])
}

/// Highlight shape of an overlay mask (grey shape on black, transparent = off); `None` if nothing lights up.
fn mask_coverage(img: &RgbaImage) -> Option<menu::Coverage> {
    let mut cov = menu::Coverage::new(img.width(), img.height());
    let mut lit = false;
    for (i, p) in img.pixels().enumerate() {
        if p[3] >= 128 && (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3 > 16 {
            cov.data[i] = 1.0;
            lit = true;
        }
    }
    lit.then_some(cov)
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

/// One button's highlight pictures before they become IG objects.
struct Pending {
    rect: Rect,
    selected: RgbaImage,
    activated: RgbaImage,
    element: usize,
}

/// Bounding box `(x, y, w, h)` of the non-transparent pixels of both pictures.
fn opaque_bounds(a: &RgbaImage, b: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for img in [a, b] {
        for (x, y, p) in img.enumerate_pixels() {
            if p[3] != 0 {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
    }
    (x1 > x0).then(|| (x0, y0, x1 - x0, y1 - y0))
}

/// IG palette, objects and (possibly cropped) button rectangles of a page.
struct IgSet {
    pal: Palette,
    objects: Vec<IgObject>,
    rects: Vec<Rect>,
}

/// Palette-indexes and run-length encodes the button pictures: `steps` alpha levels, `crop` cuts every
/// object to its non-transparent bounding box.
fn ig_set(pending: &[Pending], steps: u32, crop: bool, menu_name: &str) -> Result<IgSet, LayoutError> {
    let mut pal = Palette { entries: Vec::new(), steps };
    let mut objects = Vec::new();
    let mut rects = Vec::new();
    let mut budget = 0u64;
    for (i, p) in pending.iter().enumerate() {
        let (mut rect, mut pics) = (p.rect, [p.selected.clone(), p.activated.clone()]);
        if let Some((x, y, w, h)) = crop.then(|| opaque_bounds(&p.selected, &p.activated)).flatten() {
            pics = pics.map(|img| imageops::crop_imm(&img, x, y, w, h).to_image());
            rect = Rect { x0: rect.x0 + x, y0: rect.y0 + y, x1: rect.x0 + x + w, y1: rect.y0 + y + h };
        }
        let (w, h) = (rect.x1 - rect.x0, rect.y1 - rect.y0);
        for (k, img) in pics.iter().enumerate() {
            let mut px = Vec::with_capacity((w * h) as usize);
            for q in img.pixels() {
                let Some(idx) = pal.index(q.0) else {
                    return Err(LayoutError(vec![format!("Menu '{menu_name}': too many different button colours for the Blu-ray palette (255)")]));
                };
                px.push(idx);
            }
            budget += w as u64 * h as u64;
            objects.push(IgObject { id: (2 * i + k) as u16, width: w as u16, height: h as u16, rle: igs::rle_encode(&px, w as usize, h as usize) });
        }
        rects.push(rect);
    }
    if budget > IG_OBJECT_BUDGET {
        return Err(LayoutError(vec![format!(
            "Menu '{menu_name}': the button graphics are too large for Blu-ray memory ({} MiB, max. 16) – use smaller buttons",
            budget / 1024 / 1024
        )]));
    }
    Ok(IgSet { pal, objects, rects })
}

/// Builds the IG menu with `build(alpha steps, crop)`, trying coarser versions (fewer anti-aliasing
/// levels, cropped objects: longer RLE runs) until the display set is within `limit` bytes. Returns the
/// first that fits, else the smallest, and whether one fit.
fn fit_budget(mut build: impl FnMut(u32, bool) -> Result<IgMenu, LayoutError>, limit: usize) -> Result<(IgMenu, bool), LayoutError> {
    let mut best: Option<(IgMenu, usize)> = None;
    for (steps, crop) in [(ALPHA_STEPS, false), (4, true), (2, true)] {
        let ig = build(steps, crop)?;
        let size = igs::build_sup(&ig).len();
        if size <= limit {
            return Ok((ig, true));
        }
        if best.as_ref().is_none_or(|b| size < b.1) {
            best = Some((ig, size));
        }
    }
    Ok((best.expect("at least one attempt").0, false))
}

/// Box of an element in frame pixels (keys scale like [`frame_box`], but stay fractional).
fn frame_key(k: &ElementKey, canvas: Canvas, frame: MenuFrame) -> ElementKey {
    let (sx, sy) = (frame.width as f64 / canvas.width as f64, frame.height as f64 / canvas.height as f64);
    ElementKey { x: k.x * sx, y: k.y * sy, w: k.w * sx, h: k.h * sy, ..*k }
}

/// Renders menu `index`: static layers (PNG), the IG stream data and the clip description. A page
/// with an intro also gets the sprites its intro frames are composited from.
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

    let mut sprites: Vec<Sprite> = Vec::new();
    let mut slots: Vec<usize> = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    for (n, e) in menu.elements.iter().enumerate() {
        let (x, y, w, h) = frame_box(e, canvas, frame);
        let unrotated = is_unrotated(e.rotation);
        let opacity = (e.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut text_cov: Option<(menu::Coverage, i64, i64)> = None;
        // Sprite of this element: its keys only apply to unrotated elements.
        let sprite = |pixels: Pixels, rx: i64, ry: i64, rw: u32, rh: u32| {
            let keys = if unrotated { e.intro.iter().map(|k| frame_key(k, canvas, frame)).collect() } else { Vec::new() };
            Sprite { pixels, rect: (rx as f64, ry as f64, rw as f64, rh as f64), opacity: e.opacity, keys }
        };
        let visible = e.opacity > 0.0 || !e.intro.is_empty();
        match &e.content {
            Content::Text { text, font, size, color, align, bold, italic, shadow, wrap } => {
                let cov = menu::text_coverage(text, font, frame_font_size(*size, canvas, frame), *bold, *italic, *wrap, align, w, h);
                let mut img = RgbaImage::new(w, h);
                let [r, g, b] = parse_color(color).unwrap_or([255, 255, 255]);
                if *shadow {
                    menu::blend_coverage(&mut img, &cov, [0, 0, 0, 170], 2, 2, 255);
                }
                menu::blend_coverage(&mut img, &cov, [r, g, b, 255], 0, 0, 255);
                let (img, rx, ry) = rotate_rgba(img, e.rotation, frame.pixel_aspect, (x, y));
                let (rw, rh) = img.dimensions();
                menu::blit(current.get_or_insert_with(|| RgbaImage::new(fw, fh)), &img, rx, ry, opacity);
                if d.intro && visible {
                    sprites.push(sprite(Pixels::Image(img), rx, ry, rw, rh));
                }
                text_cov = Some(rotate_coverage(cov, e.rotation, frame.pixel_aspect, (x, y)));
            }
            Content::Image { src, fit } => {
                let src_img = image::open(src).map_err(|err| io_err(Path::new(src), &err))?.to_rgba8();
                let fitted = fit_image(&src_img, w, h, *fit, frame.pixel_aspect, false);
                // An invisible image button is a template overlay mask: its bright, opaque pixels are the highlight.
                if e.action.is_some() && e.opacity == 0.0 {
                    text_cov = mask_coverage(&fitted).map(|cov| rotate_coverage(cov, e.rotation, frame.pixel_aspect, (x, y)));
                }
                let (img, rx, ry) = rotate_rgba(fitted, e.rotation, frame.pixel_aspect, (x, y));
                let (rw, rh) = img.dimensions();
                menu::blit(current.get_or_insert_with(|| RgbaImage::new(fw, fh)), &img, rx, ry, opacity);
                if d.intro && visible {
                    sprites.push(sprite(Pixels::Image(img), rx, ry, rw, rh));
                }
            }
            Content::Rect { color, border_color, border_width } => {
                let fill = parse_color(color).unwrap_or([255, 255, 255]);
                let raw = rect_image(fill, rect_border(border_color, border_width, canvas, frame, w, h), w, h, 255);
                let (img, rx, ry) = rotate_rgba(raw, e.rotation, frame.pixel_aspect, (x, y));
                let (rw, rh) = img.dimensions();
                menu::blit(current.get_or_insert_with(|| RgbaImage::new(fw, fh)), &img, rx, ry, opacity);
                if d.intro && visible {
                    sprites.push(sprite(Pixels::Image(img), rx, ry, rw, rh));
                }
            }
            Content::Video { title, start_secs, fit, path } => {
                let vid_rot = video_rotation_deg(e.rotation).unwrap_or(0);
                flush(&mut current, &mut layers)?;
                if d.intro && visible {
                    sprites.push(sprite(Pixels::Slot { index: slots.len(), width: w, height: h }, x, y, w, h));
                    slots.push(layers.len());
                }
                layers.push(MenuLayer::Video {
                    clip: video_clip(d.titles, *title, *start_secs, path.as_ref()),
                    x: x.max(0) as u32,
                    y: y.max(0) as u32,
                    width: w,
                    height: h,
                    rotation: vid_rot,
                    fit: *fit,
                    opacity: e.opacity,
                });
            }
        }
        if commands.get(n).is_none_or(Option::is_none) {
            continue;
        }
        let Some(rect) = bd_button_rect(e, canvas, frame) else {
            warnings.push(format!("{}: button lies outside the picture and was skipped", element_name(e, n)));
            continue;
        };
        let (hl, sel) = button_colors(layout, e);
        let (cov, origin) = match &text_cov {
            Some((c, ox, oy)) => (Some(c), (*ox, *oy)),
            None => (None, (x, y)),
        };
        pending.push(Pending {
            rect,
            selected: highlight_image(rect, cov, origin, hl, alpha),
            activated: highlight_image(rect, cov, origin, sel, alpha),
            element: n,
        });
    }
    flush(&mut current, &mut layers)?;

    // The intro: whole frames, ending where the loop (mark 1, IG composition) starts.
    let intro = d.intro.then(|| {
        let frames = menu_anim::intro_frames(menu.intro_secs.unwrap_or(0.0), rate);
        let ticks = menu_anim::intro_ticks(frames, rate);
        let end = sprites.iter().flat_map(|s| s.keys.iter().map(|k| k.t)).fold(0.0f64, f64::max);
        Intro {
            frames,
            secs: ticks as f64 / 90_000.0,
            ticks,
            end,
            sprites: std::mem::take(&mut sprites),
            slots: std::mem::take(&mut slots),
        }
    });

    // IG stream: palette, objects, buttons with their neighbours.
    let mut buttons: Vec<MenuButton> = pending
        .iter()
        .enumerate()
        .map(|(i, p)| MenuButton {
            name: format!("b{}", i + 1),
            template_index: p.element,
            location: "CUSTOM".into(),
            rect: p.rect,
            action: ButtonAction::Commands(String::new()),
            label: None,
            thumbnail: None,
            clip: None,
            nav: Navigation::default(),
        })
        .collect();
    author::compute_navigation(&mut buttons);
    let id_of = |name: &Option<String>| name.as_ref().and_then(|n| n[1..].parse::<u16>().ok()).unwrap_or(0);
    let make_ig = |steps: u32, crop: bool| -> Result<IgMenu, LayoutError> {
        let IgSet { pal, objects, rects } = ig_set(&pending, steps, crop, &menu.name)?;
        let ig_buttons: Vec<IgButton> = pending
            .iter()
            .enumerate()
            .map(|(i, p)| IgButton {
                id: i as u16 + 1,
                x: rects[i].x0 as u16,
                y: rects[i].y0 as u16,
                up: id_of(&buttons[i].nav.up),
                down: id_of(&buttons[i].nav.down),
                left: id_of(&buttons[i].nav.left),
                right: id_of(&buttons[i].nav.right),
                selected: (2 * i) as u16,
                activated: (2 * i + 1) as u16,
                commands: commands[p.element].clone().unwrap_or_default(),
            })
            .collect();
        let mut palette = vec![[16u8, 128, 128, 0]; 256];
        for (i, e) in pal.entries.iter().enumerate() {
            let [y, cr, cb] = igs::rgb_to_ycrcb(e[0], e[1], e[2]);
            palette[i + 1] = [y, cr, cb, e[3]];
        }
        Ok(IgMenu {
            width: fw as u16,
            height: fh as u16,
            frame_rate_code: frame_rate_code(rate),
            default_selected: if default_selected_none { None } else { Some(1) },
            palette,
            buttons: ig_buttons,
            objects,
            // The buttons appear with the first frame of the loop.
            pts: intro.as_ref().map_or(0, |i| i.ticks as u32),
        })
    };
    let igs = if pending.is_empty() {
        None
    } else if intro.is_some() {
        let (ig, fits) = fit_budget(make_ig, IG_BUDGET)?;
        if !fits {
            warnings.push("Button graphics are very large – buttons appear with a delay on some players".into());
        }
        Some(ig)
    } else {
        Some(make_ig(ALPHA_STEPS, false)?)
    };

    let background = match (&menu.background.video, &menu.background.image) {
        (Some(v), _) => MenuBackground::Video { clip: video_clip(d.titles, v.title, v.start_secs, v.path.as_ref()), blur: menu.background.blur },
        (None, Some(img)) => MenuBackground::Image { path: PathBuf::from(img), blur: menu.background.blur },
        (None, None) => MenuBackground::Color(parse_color(&menu.background.color).unwrap_or([0, 0, 0])),
    };
    let (audio, warn) = menu_audio(menu, d.titles);
    warnings.extend(warn);
    let moving = matches!(background, MenuBackground::Video { .. })
        || layers.iter().any(|l| matches!(l, MenuLayer::Video { .. }))
        || audio.is_some()
        || intro.is_some();
    let loop_secs = if moving { menu.duration_secs.unwrap_or(default_secs).clamp(1.0, 600.0) } else { STILL_SECS };
    // A page with an intro is one clip: the intro, then the loop.
    let secs = loop_secs + intro.as_ref().map_or(0.0, |i| i.secs);
    Ok(BdPage {
        media: CustomMenuMedia { background, overlay: menu.background.overlay, layers },
        audio,
        secs,
        moving,
        igs,
        intro,
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
            let d = Draw { layout, menu, titles, frame, intro: has_intro(layout, menu) };
            // Every menu selects its first button with SetButtonPage before its loop, so the ICS
            // has no default and a restarted clip keeps the focus (PSR10).
            let mut page = render_page(&d, &nav.commands[i], true, rate, out_dir, &format!("bm{:02}", i + 1), default_secs)?;
            page.warnings = page.warnings.into_iter().map(|w| format!("Menu '{}': {w}", menu.name)).collect();
            Ok(page)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Background, Canvas, Element, ElementKey, Highlight};

    fn title(chapters: usize) -> LayoutTitle {
        LayoutTitle {
            label: "T".into(),
            chapters,
            duration_secs: 100.0,
            clip: ClipSource { path: "a.mkv".into(), start_secs: 10.0, has_audio: true },
            audio_sources: vec![1, 2],
            subtitle_sources: vec![3],
            audio_labels: vec![],
            subtitle_labels: vec![],
        }
    }

    fn button(x: f64, y: f64, action: Action) -> Element {
        Element {
            id: String::new(),
            x,
            y,
            w: 400.0,
            h: 100.0,
            rotation: 0.0,
            opacity: 1.0,
            highlight: None,
            group: None,
            intro: Vec::new(),
            easter_egg: false,
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
            intro_secs: None,
            elements,
        }
    }

    fn layout() -> CustomLayout {
        CustomLayout {
            version: 1,
            canvas: Canvas { width: 1920, height: 1080 },
            highlight: Highlight::default(),
            animations: true,
            fast_play: None,
            menus: vec![
                menu("main", MenuRole::Title, None, vec![
                    button(100.0, 100.0, Action::PlayTitle { title: 1 }),
                    button(100.0, 300.0, Action::PlayChapter { title: 2, chapter: 3 }),
                    button(100.0, 500.0, Action::PlayAll),
                    button(100.0, 700.0, Action::menu("set")),
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
        let tr = [MenuTraits { moving: false, has_buttons: true, intro: false }, MenuTraits { moving: true, has_buttons: true, intro: false }];
        plan(&l, &t, &tr, opts, &[vec![5], vec![5]])
    }

    /// Entries `[a, b]` and `[c]`: three files, the second entry starts at title 3.
    fn merged_nav(opts: PlaybackOptions) -> BdNav {
        let l = layout();
        let t = [title(5), title(5)];
        let tr = [MenuTraits { moving: false, has_buttons: true, intro: false }, MenuTraits { moving: true, has_buttons: true, intro: false }];
        plan(&l, &t, &tr, opts, &[vec![2, 3], vec![5]])
    }

    #[test]
    fn merged_files_chain_and_resolve_chapters() {
        let n = merged_nav(PlaybackOptions::default());
        // 0 FP, 1 TM, 2/3 menus, 4/5/6 files.
        assert_eq!(n.objects.len(), 7);
        assert_eq!(n.title_objects, vec![4, 5, 6]);
        // The menu entry of the second group plays its first (only) file, HDMV title 3.
        assert_eq!(cmds(&n, 0, 0)[2], hdmv::jump_title(1));
        assert_eq!(cmds(&n, 0, 1), &[hdmv::mov(1, Op::Imm(2)), hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(3)]);
        // Member 1 ends with an unconditional jump to member 2 (mark reset as before).
        assert_eq!(tail(&n, 4)[..], [hdmv::jump_title(2)]);
        // Last member of group 1: play-all continues with the first file of group 2.
        assert_eq!(tail(&n, 5)[..], [hdmv::compare(Op::Gpr(2), Cmp::Ne, Op::Imm(1)), hdmv::goto(19), hdmv::jump_title(3), hdmv::jump_title(0)]);
        assert_eq!(tail(&n, 6)[..], [hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(0)]);

        let n = merged_nav(PlaybackOptions { play_sequentially: true, ..PlaybackOptions::default() });
        assert_eq!(tail(&n, 5)[..], [hdmv::jump_title(3)]);
        let n = merged_nav(PlaybackOptions { play_sequentially: true, loop_playback: true, ..PlaybackOptions::default() });
        assert_eq!(tail(&n, 6)[..], [hdmv::jump_title(1)], "the loop starts at the first file of entry 1");
        // Single-title loop repeats the whole entry, not just its last file.
        let n = merged_nav(PlaybackOptions { loop_playback: true, ..PlaybackOptions::default() });
        assert_eq!(tail(&n, 5)[..], [hdmv::compare(Op::Gpr(2), Cmp::Ne, Op::Imm(1)), hdmv::goto(19), hdmv::jump_title(3), hdmv::jump_title(1)]);
    }

    #[test]
    fn chapters_cross_member_boundaries() {
        let mut l = layout();
        l.menus[0].elements = vec![
            button(0.0, 0.0, Action::PlayChapter { title: 1, chapter: 2 }),
            button(0.0, 0.0, Action::PlayChapter { title: 1, chapter: 3 }),
            button(0.0, 0.0, Action::PlayChapter { title: 1, chapter: 5 }),
        ];
        let t = [title(5), title(5)];
        let tr = [MenuTraits { moving: false, has_buttons: true, intro: false }, MenuTraits { moving: true, has_buttons: true, intro: false }];
        let n = plan(&l, &t, &tr, PlaybackOptions::default(), &[vec![2, 3], vec![5]]);
        let go = |mark, title| [hdmv::mov(1, Op::Imm(mark)), hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(title)];
        assert_eq!(cmds(&n, 0, 0), &go(1, 1), "chapter 2 is the last mark of file 1");
        assert_eq!(cmds(&n, 0, 1), &go(0, 2), "chapter 3 is the first mark of file 2");
        assert_eq!(cmds(&n, 0, 2), &go(2, 2));
    }

    /// Top Menu object: `head`, then the return-menu dispatch, else menu object `first`.
    fn top_menu(head: &[Command], first: u32) -> Vec<Command> {
        let at = head.len() as u32;
        [
            head,
            &[
                hdmv::compare(Op::Gpr(8), Cmp::Eq, Op::Imm(0)),
                hdmv::goto(at + 5),
                hdmv::mov(6, Op::Gpr(8)),
                hdmv::mov(8, Op::Imm(0)),
                hdmv::jump_object_op(Op::Gpr(6)),
                hdmv::jump_object(first),
            ][..],
        ]
        .concat()
    }

    /// BD-ROM Part 3 forbids JumpObject in Movie titles (the PS4 crashes at the title's end): only
    /// First Play, Top Menu, menus and the dispatcher (all run in interactive titles) may use it.
    #[test]
    fn movie_titles_never_jump_to_objects() {
        let is_jump_object = |c: &Command| c[0] & 0x1F == 0x01 && c[1] & 0x0F == 0;
        let check = |n: &BdNav, what: &str| {
            for &o in &n.title_objects {
                assert!(!n.objects[o as usize].commands.iter().any(is_jump_object), "{what}: title object {o}");
            }
        };
        for bits in 0..16u8 {
            let opts = PlaybackOptions {
                return_to_title_menu: bits & 1 != 0,
                play_sequentially: bits & 2 != 0,
                loop_playback: bits & 4 != 0,
                auto_start: bits & 8 != 0,
            };
            check(&nav(opts), &format!("{opts:?}"));
            check(&merged_nav(opts), &format!("merged {opts:?}"));
            let l = intro_layout();
            let t = [title(5), title(5)];
            let tr = traits(&l, &t, MenuFrame::square(1920, 1080));
            check(&plan(&l, &t, &tr, opts, &[vec![5], vec![5]]), &format!("intro {opts:?}"));
        }
        check(&plan_without_menus(3), "no menus");
        assert!(is_jump_object(&hdmv::jump_object(2)) && is_jump_object(&hdmv::jump_object_op(Op::Gpr(6))));
        assert!(!is_jump_object(&hdmv::jump_title(0)) && !is_jump_object(&hdmv::goto(1)));
    }

    /// Commands of title object `o` after its PlayPLatMark and mark reset.
    fn tail(n: &BdNav, o: usize) -> &[Command] {
        let c = &n.objects[o].commands;
        &c[c.iter().position(|x| *x == hdmv::mov(1, Op::Imm(0))).unwrap() + 1..]
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
        // Subtitle off / on: settings buttons only store the choice (GPR16 audio / GPR17 subtitle of title 1).
        assert_eq!(cmds(&n, 1, 2), &[hdmv::mov(17, Op::Imm(0xFFFF))]);
        assert_eq!(cmds(&n, 1, 1), &[hdmv::mov(17, Op::Imm(1))]);
        // Audio source 2 = second audio track; keeps the display flag.
        let a = cmds(&n, 1, 0);
        assert_eq!(a, &[hdmv::mov(16, Op::Imm(2))]);
    }

    #[test]
    fn overlay_masks_light_their_bright_opaque_pixels() {
        let mut img = RgbaImage::new(4, 2);
        assert!(mask_coverage(&img).is_none(), "fully transparent: frame highlight instead");
        img.put_pixel(1, 0, image::Rgba([200, 200, 200, 255]));
        img.put_pixel(2, 0, image::Rgba([0, 0, 0, 255]));
        let cov = mask_coverage(&img).unwrap();
        assert_eq!((cov.at(1, 0), cov.at(2, 0), cov.at(0, 0)), (1.0, 0.0, 0.0));
    }

    #[test]
    fn menus_still_and_motion() {
        let n = nav(PlaybackOptions::default());
        // A still menu loops like a motion menu, in case the player ends its infinite still.
        assert_eq!(n.objects[2].commands, vec![hdmv::set_button_page(1), hdmv::play_pl(0), hdmv::goto(1)]);
        assert_eq!(n.objects[3].commands, vec![hdmv::set_button_page(1), hdmv::play_pl(1), hdmv::goto(1)]);
        // Disc start: the Top Menu title (never JumpObject out of First Play), with no menu to return to.
        assert_eq!(n.objects[0].commands[0], hdmv::mov(8, Op::Imm(0)));
        assert_eq!(n.objects[0].commands[3], hdmv::jump_title(0));
        // Top Menu: a pending return menu is cleared and entered, else the first title menu.
        assert_eq!(n.objects[1].commands, top_menu(&[], 2));
        let auto = nav(PlaybackOptions { auto_start: true, ..PlaybackOptions::default() });
        assert_eq!(auto.objects[0].commands[3], hdmv::jump_title(1));
    }

    #[test]
    fn title_end_behaviour() {
        let play = hdmv::play_pl_at_mark(Op::Imm(2), Op::Gpr(1));
        let clear = hdmv::mov(1, Op::Imm(0));
        // Default: title 1 -> if play-all flag, title 2; else back to the main menu.
        let n = nav(PlaybackOptions::default());
        // Title 1 has a settings menu: its stored audio / subtitle choice is applied first (the
        // PS4 ignores SetStream in the menu, whose playlist lacks the title's streams). Both
        // skips land on the next part; title 2 has no settings menu and starts with PlayPL.
        let pre = apply_stream_choice(1, true, true);
        assert_eq!((pre.len(), pre[1], pre[11]), (14, hdmv::goto(8), hdmv::goto(14)));
        assert_eq!((pre[0], pre[10]), (hdmv::compare(Op::Gpr(16), Cmp::Eq, Op::Imm(0)), hdmv::compare(Op::Gpr(17), Cmp::Eq, Op::Imm(0))));
        // Register-form SetStream: GPR16 holds the audio number, GPR17 the PG number.
        assert_eq!((pre[5], pre[7]), (hdmv::set_stream_regs(Some(16), None, true), hdmv::set_stream_regs(Some(16), None, false)));
        assert_eq!((pre[8], pre[9], pre[13]), (hdmv::compare(Op::Gpr(17), Cmp::Eq, Op::Imm(0xFFFF)), hdmv::set_stream(0), hdmv::set_stream_regs(None, Some(17), true)));
        assert_eq!(n.objects[5].commands[0], hdmv::play_pl_at_mark(Op::Imm(3), Op::Gpr(1)));
        assert_eq!(
            n.objects[4].commands,
            [
                apply_stream_choice(1, true, true),
                vec![
                    hdmv::play_pl_at_mark(Op::Imm(2), Op::Gpr(1)),
                    clear,
                    hdmv::compare(Op::Gpr(2), Cmp::Ne, Op::Imm(1)),
                    hdmv::goto(19),
                    hdmv::jump_title(2),
                    hdmv::jump_title(0)
                ]
            ]
            .concat()
        );
        // Last title: reset flag, menu.
        assert_eq!(tail(&n, 5)[..], [hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(0)]);
        let _ = play;
        // Loop, not sequential: repeat each title; play-all still advances.
        let n = nav(PlaybackOptions { loop_playback: true, ..PlaybackOptions::default() });
        assert_eq!(tail(&n, 5)[..], [hdmv::compare(Op::Gpr(2), Cmp::Ne, Op::Imm(1)), hdmv::goto(5), hdmv::jump_title(1), hdmv::jump_title(2)]);
        // Sequential: next title; the last one returns to the menu (or loops).
        let n = nav(PlaybackOptions { play_sequentially: true, ..PlaybackOptions::default() });
        assert_eq!(tail(&n, 4)[..], [hdmv::jump_title(2)]);
        assert_eq!(tail(&n, 5)[..], [hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(0)]);
        let n = nav(PlaybackOptions { play_sequentially: true, loop_playback: true, ..PlaybackOptions::default() });
        assert_eq!(tail(&n, 5)[..], [hdmv::jump_title(1)]);
        // Return to the title's own menu (title 1 has the settings menu, title 2 none).
        let n = nav(PlaybackOptions { return_to_title_menu: false, ..PlaybackOptions::default() });
        // Another menu than the first title menu goes through Top Menu with GPR8 = its object.
        assert_eq!(tail(&n, 4)[3..], [hdmv::mov(8, Op::Imm(3)), hdmv::jump_title(0)]);
        assert_eq!(tail(&n, 5)[..], [hdmv::mov(2, Op::Imm(0)), hdmv::jump_title(0)]);
    }

    #[test]
    fn rect_buttons_render_into_ig_objects() {
        let mut l = layout();
        let mut r = button(700.0, 500.0, Action::PlayTitle { title: 1 });
        r.content = Content::Rect { color: "#112233".into(), border_color: Some("#ff0000".into()), border_width: Some(3.0) };
        let before = l.menus[0].elements.len();
        l.menus[0].elements.push(r);
        let frame = MenuFrame::square(1920, 1080);
        let t = [title(5), title(5)];
        crate::layout::validate_bd(&l, &t, frame).unwrap();
        let tr = traits(&l, &t, frame);
        let nav = plan(&l, &t, &tr, PlaybackOptions::default(), &[vec![5], vec![5]]);
        let dir = std::env::temp_dir().join(format!("bdmenu-rect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let pages = render_pages(&l, &t, frame, rate, &nav, &dir, 10.0).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let ig = pages[0].igs.as_ref().unwrap();
        assert_eq!(ig.buttons.len(), 4 + 1, "the Rect button joins the {before} existing elements");
    }

    #[test]
    fn page_rendering_makes_ig_objects() {
        let l = layout();
        let frame = MenuFrame::square(1920, 1080);
        let t = [title(5), title(5)];
        crate::layout::validate_bd(&l, &t, frame).unwrap();
        let tr = traits(&l, &t, frame);
        assert_eq!(tr[0], MenuTraits { moving: false, has_buttons: true, intro: false });
        let nav = plan(&l, &t, &tr, PlaybackOptions::default(), &[vec![5], vec![5]]);
        let dir = std::env::temp_dir().join(format!("bdmenu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let pages = render_pages(&l, &t, frame, rate, &nav, &dir, 10.0).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let ig = pages[0].igs.as_ref().unwrap();
        assert_eq!(ig.buttons.len(), 4);
        assert_eq!(ig.objects.len(), 8);
        assert_eq!(ig.default_selected, None, "SetButtonPage selects the first button");
        assert_eq!(ig.frame_rate_code, 1);
        // Buttons are stacked vertically: down leads to the next, up from the first stays.
        assert_eq!((ig.buttons[0].up, ig.buttons[0].down), (1, 2));
        assert_eq!((ig.buttons[1].up, ig.buttons[1].down), (1, 3));
        assert_eq!(ig.buttons[0].commands.len(), 3);
        assert!(ig.palette[1][3] > 0 && ig.palette[0][3] == 0);
        assert_eq!(pages[0].secs, STILL_SECS);
        assert!(igs::build_sup(ig).len() > 100);
    }

    #[test]
    fn menu_less_navigation_chains_the_titles() {
        let n = plan_without_menus(3);
        assert_eq!(n.title_objects, vec![2, 3, 4]);
        assert_eq!(n.objects[0].commands, vec![hdmv::jump_title(1)]);
        assert_eq!(n.objects[2].commands, vec![hdmv::play_pl(0), hdmv::jump_title(2)]);
        assert_eq!(n.objects[4].commands, vec![hdmv::play_pl(2)], "the last title just ends");
    }
    /// "main" and "main-p2" are title pages (paging between them), "set" is a settings page; the first
    /// and the last have a decorative element that fades in.
    fn intro_layout() -> CustomLayout {
        let mut l = layout();
        l.menus.push(menu("main-p2", MenuRole::Title, None, vec![button(100.0, 100.0, Action::menu("main"))]));
        let mut fade = button(900.0, 900.0, Action::PlayAll);
        fade.action = None;
        fade.intro = vec![
            ElementKey { t: 0.0, x: 900.0, y: 900.0, w: 400.0, h: 100.0, opacity: 0.0 },
            ElementKey { t: 2.0, x: 900.0, y: 900.0, w: 400.0, h: 100.0, opacity: 1.0 },
        ];
        for i in [0, 2] {
            l.menus[i].elements.push(fade.clone());
            l.menus[i].intro_secs = Some(2.0);
        }
        l
    }

    fn intro_nav(l: &CustomLayout) -> (Vec<MenuTraits>, BdNav) {
        let t = [title(5), title(5)];
        let tr = traits(l, &t, MenuFrame::square(1920, 1080));
        let nav = plan(l, &t, &tr, PlaybackOptions::default(), &[vec![5], vec![5]]);
        (tr, nav)
    }

    #[test]
    fn intro_pages_enter_through_the_dispatcher() {
        let l = intro_layout();
        let (tr, n) = intro_nav(&l);
        assert_eq!(tr.iter().map(|t| (t.moving, t.intro)).collect::<Vec<_>>(), [(true, true), (false, false), (true, true)]);
        // 0 FP, 1 TM, 2..=4 menus, 5/6 titles, 7 dispatcher.
        assert_eq!(n.objects.len(), 8);
        assert_eq!(n.title_objects, vec![5, 6], "title numbers do not move");
        let to = |kind, index| [hdmv::mov(4, Op::Imm(kind)), hdmv::mov(5, Op::Imm(index)), hdmv::jump_object(7)];
        // Titles: mark and flag as before, then the dispatcher instead of JumpTitle.
        assert_eq!(cmds(&n, 0, 0), [&[hdmv::mov(1, Op::Imm(0)), hdmv::mov(2, Op::Imm(0))][..], &to(1, 1)].concat());
        // Settings are another menu (fresh entry), the next page of the same menu is paged.
        assert_eq!(cmds(&n, 0, 3), &to(2, 3));
        assert_eq!(cmds(&n, 2, 0), &to(3, 2));
        assert_eq!(n.objects[1].commands, top_menu(&[hdmv::mov(3, Op::Imm(0))], 2), "Top Menu always replays the intro");
        assert_eq!(
            n.objects[7].commands,
            vec![
                hdmv::mov(3, Op::Imm(0)),
                hdmv::compare(Op::Gpr(4), Cmp::Eq, Op::Imm(1)),
                hdmv::goto(6),
                hdmv::compare(Op::Gpr(4), Cmp::Eq, Op::Imm(3)),
                hdmv::mov(3, Op::Imm(1)),
                hdmv::jump_object_op(Op::Gpr(5)),
                hdmv::jump_title_op(Op::Gpr(5)),
            ]
        );
        // Intro page: remember the page, play intro + loop, or (paged) go straight to the loop.
        assert_eq!(
            n.objects[2].commands,
            vec![
                hdmv::mov(7, Op::Imm(0)),
                hdmv::set_button_page(1),
                hdmv::compare(Op::Gpr(3), Cmp::Eq, Op::Imm(1)),
                hdmv::goto(6),
                hdmv::play_pl(0),
                hdmv::goto(7),
                hdmv::mov(3, Op::Imm(0)),
                hdmv::play_pl_at_mark(Op::Imm(0), Op::Imm(1)),
                hdmv::goto(7),
            ]
        );
        // A still page without intro loops its clip.
        assert_eq!(n.objects[3].commands, vec![hdmv::set_button_page(1), hdmv::play_pl(1), hdmv::goto(1)]);
    }

    #[test]
    fn a_roots_chapter_menu_is_another_menu_not_a_page_of_it() {
        let mut l = intro_layout();
        // Root and chapter menus of a title share role and title, like the template translation makes them.
        l.menus.push(menu("root-1", MenuRole::Root, Some(1), vec![button(100.0, 100.0, Action::menu("chap-1-p1"))]));
        l.menus.push(menu("chap-1-p1", MenuRole::Root, Some(1), vec![button(100.0, 100.0, Action::menu("chap-1-p2"))]));
        l.menus.push(menu("chap-1-p2", MenuRole::Root, Some(1), vec![button(100.0, 100.0, Action::menu("root-1"))]));
        let (_, n) = intro_nav(&l);
        let to = |kind, index| [hdmv::mov(4, Op::Imm(kind)), hdmv::mov(5, Op::Imm(index)), hdmv::jump_object(10)];
        assert_eq!(cmds(&n, 3, 0), &to(2, 6), "root -> chapters: fresh entry, intro plays");
        assert_eq!(cmds(&n, 4, 0), &to(3, 7), "chapter page 1 -> 2: paged");
        assert_eq!(cmds(&n, 5, 0), &to(2, 5), "chapters -> root: fresh entry");
    }

    #[test]
    fn without_intros_the_navigation_is_unchanged() {
        let mut off = intro_layout();
        off.animations = false;
        let mut bare = intro_layout();
        for m in &mut bare.menus {
            m.intro_secs = None;
            m.elements.iter_mut().for_each(|e| e.intro.clear());
        }
        let (tr, n) = intro_nav(&off);
        assert!(tr.iter().all(|t| !t.intro));
        assert_eq!(n, intro_nav(&bare).1, "animations=false plays like a layout without animation data");
        assert_eq!(n.objects.len(), 7);
        assert_eq!(cmds(&n, 0, 0)[2], hdmv::jump_title(1));
    }

    #[test]
    fn intro_page_is_one_clip_with_the_buttons_at_the_loop_mark() {
        let l = intro_layout();
        let frame = MenuFrame::square(1920, 1080);
        let t = [title(5), title(5)];
        let (_, nav) = intro_nav(&l);
        let dir = std::env::temp_dir().join(format!("bdmenu-intro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let pages = render_pages(&l, &t, frame, rate, &nav, &dir, 10.0).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let intro = pages[0].intro.as_ref().expect("intro");
        // 2 s = 48 frames of 1001/24000 s.
        assert_eq!((intro.frames, intro.ticks), (48, 180_180));
        assert_eq!(pages[0].secs, 10.0 + intro.secs);
        assert_eq!(pages[0].igs.as_ref().unwrap().pts as u64, intro.ticks, "composition at exactly T");
        assert!(pages[0].moving && pages[2].intro.is_some() && pages[1].intro.is_none());
        assert_eq!(pages[1].igs.as_ref().unwrap().pts, 0);
        assert!(intro.sprites.iter().any(|s| !s.keys.is_empty()));
        assert!(intro.sprites.iter().all(|s| s.opacity > 0.0 || !s.keys.is_empty()));
        assert!(igs::build_sup(pages[0].igs.as_ref().unwrap()).len() <= IG_BUDGET);
        // Opt-out: same page, no intro, no extra length.
        let mut off = l.clone();
        off.animations = false;
        let (_, nav) = intro_nav(&off);
        let dir = std::env::temp_dir().join(format!("bdmenu-intro-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = render_pages(&off, &t, frame, rate, &nav, &dir, 10.0).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(plain[0].intro.is_none() && plain[0].igs.as_ref().unwrap().pts == 0 && plain[0].secs == STILL_SECS);
    }

    #[test]
    fn oversized_display_sets_shrink_and_are_flagged_when_they_cannot() {
        // Size of the fake set by (alpha steps, crop): coarser and cropped is smaller.
        let sizes = |steps: u32| match steps {
            16 => 4000,
            4 => 1500,
            _ => 700,
        };
        let build = |steps: u32, _crop: bool| {
            Ok(IgMenu {
                width: 8,
                height: 8,
                frame_rate_code: 1,
                default_selected: None,
                palette: vec![[16, 128, 128, 0]; 256],
                buttons: vec![],
                objects: vec![IgObject { id: 0, width: 1, height: 1, rle: vec![0u8; sizes(steps)] }],
                pts: 0,
            })
        };
        let size = |ig: &IgMenu| igs::build_sup(ig).len();
        let full = size(&build(16, false).unwrap());
        let (ig, fits) = fit_budget(build, full + 1).unwrap();
        assert!(fits && size(&ig) == full, "within the limit: the original");
        let (ig, fits) = fit_budget(build, full - 3000).unwrap();
        assert!(fits && size(&ig) < full, "shrunk to fit");
        let (ig, fits) = fit_budget(build, 10).unwrap();
        assert!(!fits && size(&ig) == full - 3300, "nothing fits: the smallest version is kept");
    }

    #[test]
    fn cropping_keeps_the_picture_and_moves_the_object() {
        let mut a = RgbaImage::new(10, 6);
        a.put_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
        a.put_pixel(5, 3, image::Rgba([255, 0, 0, 9]));
        let b = RgbaImage::new(10, 6);
        assert_eq!(opaque_bounds(&a, &b), Some((3, 2, 3, 2)));
        assert_eq!(opaque_bounds(&b, &b), None);
        let p = Pending { rect: Rect { x0: 100, y0: 50, x1: 110, y1: 56 }, selected: a, activated: b, element: 0 };
        let uncropped = ig_set(std::slice::from_ref(&p), 16, false, "m").unwrap();
        let cropped = ig_set(std::slice::from_ref(&p), 16, true, "m").unwrap();
        assert_eq!(cropped.rects[0], Rect { x0: 103, y0: 52, x1: 106, y1: 54 });
        assert_eq!(uncropped.rects[0], p.rect);
        assert_eq!((cropped.objects[0].width, cropped.objects[0].height), (3, 2));
    }

    #[test]
    fn rotated_text_and_rect_buttons_render_into_bd_igs() {
        let mut l = layout();
        let mut tbtn = button(400.0, 200.0, Action::PlayTitle { title: 1 });
        tbtn.w = 320.0;
        tbtn.h = 80.0;
        tbtn.rotation = 45.0;
        let mut rbtn = button(1100.0, 400.0, Action::PlayAll);
        rbtn.w = 240.0;
        rbtn.h = 80.0;
        rbtn.rotation = 30.0;
        rbtn.content = Content::Rect { color: "#224466".into(), border_color: Some("#FFFFFF".into()), border_width: Some(4.0) };
        l.menus[0].elements = vec![tbtn.clone(), rbtn.clone()];

        let frame = MenuFrame::square(1920, 1080);
        let t = [title(5), title(5)];
        crate::layout::validate_bd(&l, &t, frame).unwrap();

        let rect_t = bd_button_rect(&tbtn, l.canvas, frame).unwrap();
        assert!(rect_t.y0 < 200 && rect_t.y1 > 280, "BD button rect expands for 45° rotation: {rect_t:?}");

        let tr = traits(&l, &t, frame);
        let nav = plan(&l, &t, &tr, PlaybackOptions::default(), &[vec![5], vec![5]]);
        let dir = std::env::temp_dir().join(format!("bdmenu-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let pages = render_pages(&l, &t, frame, rate, &nav, &dir, 10.0).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let ig = pages[0].igs.as_ref().unwrap();
        assert_eq!(ig.buttons.len(), 2);
        assert_eq!((ig.buttons[0].x as u32, ig.buttons[0].y as u32), (rect_t.x0, rect_t.y0));
        assert!(igs::build_sup(ig).len() > 100);

        // Two rotated BD buttons that overlap in rotated AABB must fail validate_bd.
        let mut b2 = tbtn.clone();
        b2.y = 330.0;
        l.menus[0].elements.push(b2);
        let err = crate::layout::validate_bd(&l, &t, frame).unwrap_err().0.join("\n");
        assert!(err.contains("overlaps"), "{err}");
    }

}
