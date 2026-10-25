//! dvdauthor / spumux project generation.
//!
//! Division of labour between the two tools:
//! - `spumux` muxes the highlight/select PNGs into the menu MPEG as a subpicture stream and
//!   defines the button geometry (`x0 y0 x1 y1`) and remote navigation (`up down left right`).
//! - `dvdauthor` gets the menu VOBs, the button *commands* and the titles with chapter marks.
//!
//! Disc structure (one titleset per input video, so every title can have its own
//! aspect ratio and audio/subtitle layout):
//!
//! ```text
//! VMGM  title menu pages (all videos)          -> jump titleset N menu / jump title N
//! VTSM  root menu  (chapters, settings, back, play)
//!       chapter menu pages                     -> jump title 1 chapter N
//!       settings menu pages                    -> audio = N / subtitle = 64+N
//! VTS   title 1 of the titleset                -> call vmgm menu <its dispatcher>
//! ```
//!
//! The DVD VM allows at most 128 pre/post/cell commands per PGC, so nothing may grow with the
//! number of titles. Every title therefore gets its own command-only VMGM PGC (a "dispatcher",
//! numbered after the visible menu pages) with at most two commands:
//! `if (g1 eq 1) jump title N+1; jump menu <page listing title N>;`.
//! Back buttons in a titleset jump straight to the VMGM page that lists their title.
//!
//! General purpose register `g1`: play-all flag (1 = continue with the next title).

use std::fmt::{self, Write as _};
use std::path::PathBuf;

use regex::Regex;
use serde::Serialize;

use crate::template::Page;

/// DVD-Video limit of buttons per menu.
pub const MAX_BUTTONS: usize = 36;
/// DVD-Video limit of titles (and titlesets) per disc.
pub const MAX_TITLES: usize = 99;
/// DVD-Video limit of chapters per title.
pub const MAX_CHAPTERS: usize = 99;
/// DVD-Video limit of pre + post + cell commands per PGC.
pub const MAX_PGC_COMMANDS: usize = 128;
/// PAL frame size (the template pages are designed for it).
pub const PAL_WIDTH: u32 = 720;
pub const PAL_HEIGHT: u32 = 576;

/// TV standard of the whole disc: frame size and rate of titles and menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TvStandard {
    /// 720x576, 25 fps.
    #[default]
    Pal,
    /// 720x480, 29.97 fps (30000/1001).
    Ntsc,
}

/// The frame a menu is drawn on: size in pixels and the shape of one pixel (width / height on the
/// screen; 1.0 = square, as on Blu-ray).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuFrame {
    pub width: u32,
    pub height: u32,
    pub pixel_aspect: f64,
}

impl From<TvStandard> for MenuFrame {
    fn from(tv: TvStandard) -> Self {
        MenuFrame { width: tv.width(), height: tv.height(), pixel_aspect: tv.menu_pixel_aspect() }
    }
}

impl MenuFrame {
    pub fn square(width: u32, height: u32) -> Self {
        MenuFrame { width, height, pixel_aspect: 1.0 }
    }
}

impl TvStandard {
    pub fn width(self) -> u32 {
        720
    }

    pub fn height(self) -> u32 {
        match self {
            TvStandard::Pal => 576,
            TvStandard::Ntsc => 480,
        }
    }

    pub fn size(self) -> (u32, u32) {
        (self.width(), self.height())
    }

    /// Width of one frame pixel on a 4:3 screen relative to its height (PAL 16/15, NTSC 8/9):
    /// square-pixel pictures must be squeezed by its inverse to look undistorted in menus.
    pub fn menu_pixel_aspect(self) -> f64 {
        4.0 / 3.0 * self.height() as f64 / self.width() as f64
    }

    /// Frame rate as ffmpeg expects it.
    pub fn frame_rate(self) -> &'static str {
        match self {
            TvStandard::Pal => "25",
            TvStandard::Ntsc => "30000/1001",
        }
    }

    pub fn fps(self) -> f64 {
        match self {
            TvStandard::Pal => 25.0,
            TvStandard::Ntsc => 30000.0 / 1001.0,
        }
    }

    /// GOP length: DVD-Video allows at most 15 (PAL) / 18 (NTSC) frames; we use the common
    /// half-second GOP.
    pub fn gop(self) -> u32 {
        match self {
            TvStandard::Pal => 12,
            TvStandard::Ntsc => 15,
        }
    }

    /// ffmpeg `-target` preset.
    pub fn ffmpeg_target(self) -> &'static str {
        match self {
            TvStandard::Pal => "pal-dvd",
            TvStandard::Ntsc => "ntsc-dvd",
        }
    }

    /// dvdauthor `<video format=...>`.
    pub fn dvdauthor_format(self) -> &'static str {
        match self {
            TvStandard::Pal => "pal",
            TvStandard::Ntsc => "ntsc",
        }
    }

    /// spumux `<subpictures format=...>`.
    pub fn spumux_format(self) -> &'static str {
        match self {
            TvStandard::Pal => "PAL",
            TvStandard::Ntsc => "NTSC",
        }
    }
}

/// Frame size of the titles (menus are always Full D1). Half D1 and SIF trade picture detail for
/// a lower bitrate need; subpictures keep the full raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Resolution {
    /// 720×576 (PAL) / 720×480 (NTSC).
    #[default]
    FullD1,
    /// 352×576 / 352×480.
    HalfD1,
    /// 352×288 / 352×240.
    Sif,
    /// 1920×1080 (Blu-ray only).
    Hd1080,
    /// 1280×720 (Blu-ray only).
    Hd720,
}

impl Resolution {
    /// Coded frame size for `tv`.
    pub fn frame_size(self, tv: TvStandard) -> (u32, u32) {
        match self {
            Resolution::FullD1 => tv.size(),
            Resolution::HalfD1 => (352, tv.height()),
            Resolution::Sif => (352, tv.height() / 2),
            Resolution::Hd1080 => (1920, 1080),
            Resolution::Hd720 => (1280, 720),
        }
    }

    /// Blu-ray frame sizes; they ignore the TV standard.
    pub fn is_hd(self) -> bool {
        matches!(self, Resolution::Hd1080 | Resolution::Hd720)
    }

    /// Name as in the settings ("Full D1", "Half D1", "SIF").
    pub fn label(self) -> &'static str {
        match self {
            Resolution::FullD1 => "Full D1",
            Resolution::HalfD1 => "Half D1",
            Resolution::Sif => "SIF",
            Resolution::Hd1080 => "1080p",
            Resolution::Hd720 => "720p",
        }
    }

    /// 16:9 titles. Conservative choice: DVD-Video defines the 16:9 aspect for the 720/704
    /// wide frames, so the 352 wide sizes are encoded as 4:3 (wide sources get letterboxed).
    pub fn allows_wide(self) -> bool {
        matches!(self, Resolution::FullD1 | Resolution::Hd1080 | Resolution::Hd720)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthorError {
    InvalidChapter(String),
    TooManyButtons(usize),
    TooManyTitles(usize),
    NoTitles,
    /// A PGC would exceed the VM command limit (internal error, never expected).
    TooManyCommands { pgc: String, commands: usize },
}

impl fmt::Display for AuthorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthorError::InvalidChapter(c) => {
                write!(f, "invalid chapter mark '{c}' (expected [[h:]m:]s[.frac])")
            }
            AuthorError::TooManyButtons(n) => write!(f, "{n} buttons on one menu (DVD max {MAX_BUTTONS})"),
            AuthorError::TooManyTitles(n) => write!(f, "{n} titles (DVD max {MAX_TITLES})"),
            AuthorError::NoTitles => write!(f, "at least one title is required"),
            AuthorError::TooManyCommands { pgc, commands } => {
                write!(f, "{pgc}: {commands} VM commands (DVD max {MAX_PGC_COMMANDS})")
            }
        }
    }
}

impl std::error::Error for AuthorError {}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TitleSpec {
    /// MPEG-2 program stream for this title.
    pub vob: String,
    /// Text shown on the menu button.
    pub label: String,
    /// Chapter marks in dvdauthor syntax (`0`, `5:00`, `1:02:03.5`).
    pub chapters: Vec<String>,
    /// Video aspect, `4:3` or `16:9`.
    pub aspect: String,
    /// ISO 639-1 language per audio stream (`None` = unknown), in stream order.
    pub audio_langs: Vec<Option<String>>,
    /// dvdauthor audio format per audio stream (`ac3`, `dts`, `pcm`); missing entries mean `ac3`.
    #[serde(default)]
    pub audio_formats: Vec<String>,
    /// ISO 639-1 language per subpicture stream, in stream id order.
    pub subtitle_langs: Vec<Option<String>>,
    /// Switch the first subtitle stream on when the title starts.
    #[serde(default)]
    pub force_subtitle: bool,
}

impl TitleSpec {
    /// Parses `FILE[@CHAPTERS]`, e.g. `movie.mpg@0,10:00,20:00`. The label is the file stem.
    pub fn parse_cli(s: &str) -> Result<Self, AuthorError> {
        let (vob, chapters) = match s.rsplit_once('@') {
            Some((v, c)) => (v, c.split(',').map(|c| c.trim().to_string()).collect()),
            None => (s, vec!["0".to_string()]),
        };
        let label = std::path::Path::new(vob)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| vob.to_string());
        let spec = TitleSpec { vob: vob.to_string(), label, chapters, ..TitleSpec::placeholder(1) };
        spec.validate()?;
        Ok(spec)
    }

    /// Placeholder title `title_NN.mpg` with a single chapter at 0.
    pub fn placeholder(n: usize) -> Self {
        TitleSpec {
            vob: format!("title_{n:02}.mpg"),
            label: format!("Title {n}"),
            chapters: vec!["0".to_string()],
            aspect: "16:9".to_string(),
            audio_langs: vec![],
            audio_formats: vec![],
            subtitle_langs: vec![],
            force_subtitle: false,
        }
    }

    pub fn validate(&self) -> Result<(), AuthorError> {
        let re = Regex::new(r"^\d+(:[0-5]?\d){0,2}(\.\d+)?$").expect("valid chapter regex");
        match self.chapters.iter().find(|c| !re.is_match(c)) {
            Some(bad) => Err(AuthorError::InvalidChapter(bad.clone())),
            None => Ok(()),
        }
    }
}

/// Formats seconds as a dvdauthor chapter mark (`h:mm:ss.fff`).
pub fn chapter_mark(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if frac == 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{h}:{m:02}:{s:02}.{frac:03}")
    }
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Domain {
    /// Video manager menus (main/title menu).
    Vmgm,
    /// Titleset menus (root, chapters, settings).
    Vtsm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ButtonAction {
    /// VMGM: open the root menu of titleset N.
    OpenTitleset(u32),
    /// Play a title: global title number in the VMGM, titleset-local number in a VTSM.
    PlayTitle(u32),
    PlayAll,
    /// Menu (PGC) number within the current domain.
    Menu(u32),
    /// VTSM: title 1 of the titleset, chapter N.
    PlayChapter(u32),
    /// 0-based audio stream.
    SetAudio(u32),
    /// 0-based subpicture stream, `None` switches subtitles off.
    SetSubtitle(Option<u32>),
    /// Back to the main (VMGM) menu page with this number.
    MainMenu(u32),
    Resume,
    /// Template action we do not implement; stays on the current menu.
    Unsupported(String),
    /// Ready-made VM commands (custom menu layouts compute their own navigation).
    Commands(String),
}

/// Where a button command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandContext {
    pub domain: Domain,
    /// Number of the menu PGC the button is on.
    pub menu: u32,
}

impl ButtonAction {
    /// dvdauthor command for this button.
    pub fn command(&self, ctx: CommandContext) -> String {
        let vmgm = ctx.domain == Domain::Vmgm;
        match self {
            ButtonAction::OpenTitleset(ts) => format!("g1 = 0; jump titleset {ts} menu;"),
            ButtonAction::PlayTitle(t) => format!("g1 = 0; jump title {t};"),
            ButtonAction::PlayAll => format!("g1 = {}; jump title 1;", vmgm as u8),
            ButtonAction::Menu(m) => format!("jump menu {m};"),
            ButtonAction::PlayChapter(c) => format!("g1 = 0; jump title 1 chapter {c};"),
            ButtonAction::SetAudio(a) => format!("audio = {a}; jump menu {};", ctx.menu),
            ButtonAction::SetSubtitle(Some(s)) => format!("subtitle = {}; jump menu {};", s + 64, ctx.menu),
            ButtonAction::SetSubtitle(None) => format!("subtitle = 0; jump menu {};", ctx.menu),
            ButtonAction::MainMenu(p) if vmgm => format!("jump menu {p};"),
            ButtonAction::MainMenu(p) => format!("jump vmgm menu {p};"),
            ButtonAction::Resume => "resume;".to_string(),
            ButtonAction::Unsupported(_) => format!("jump menu {};", ctx.menu),
            ButtonAction::Commands(c) => c.clone(),
        }
    }
}

/// Button rectangle in output (PAL) pixels; `x1`/`y1` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl Rect {
    /// Converts a template rectangle into output coordinates: scales from page size to
    /// `out_w`x`out_h`, clips, and widens to even y values (subpicture fields are interlaced;
    /// spumux warns about odd y coordinates).
    #[allow(clippy::too_many_arguments)]
    pub fn from_template(
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        page_w: u32,
        page_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> Option<Rect> {
        let sx = out_w as f64 / page_w.max(1) as f64;
        let sy = out_h as f64 / page_h.max(1) as f64;
        let clamp = |v: f64, max: u32| v.round().clamp(0.0, max as f64) as u32;
        let x0 = clamp(x as f64 * sx, out_w);
        let x1 = clamp((x as f64 + w as f64) * sx, out_w);
        let y0 = clamp(y as f64 * sy, out_h) & !1;
        let y1 = (clamp((y as f64 + h as f64) * sy, out_h) + 1).min(out_h) & !1;
        (x1 > x0 && y1 > y0).then_some(Rect { x0, y0, x1, y1 })
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    fn center(&self) -> (f64, f64) {
        ((self.x0 + self.x1) as f64 / 2.0, (self.y0 + self.y1) as f64 / 2.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Navigation {
    pub up: Option<String>,
    pub down: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// A button that is actually shown on a rendered menu page.
#[derive(Debug, Clone, Serialize)]
pub struct MenuButton {
    /// spumux/dvdauthor button name (`b1`, `b2`, ...).
    pub name: String,
    /// Index into [`Page::buttons`].
    pub template_index: usize,
    pub location: String,
    pub rect: Rect,
    pub action: ButtonAction,
    /// Text to render on the button's text graphics instead of the template placeholder.
    pub label: Option<String>,
    /// Image for the button's `GT_VIDEO` graphics (title/chapter thumbnail).
    pub thumbnail: Option<PathBuf>,
    /// Moving preview for the button's `GT_VIDEO` graphics in motion menus.
    pub clip: Option<ClipSource>,
    pub nav: Navigation,
}

/// Where a motion menu's sample clip comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClipSource {
    pub path: PathBuf,
    /// Position in the source where the clip starts.
    pub start_secs: f64,
    /// The source has an audio track (usable as menu sound).
    pub has_audio: bool,
}

/// One entry of the main menu.
#[derive(Debug, Clone)]
pub struct TitleEntry {
    pub label: String,
    pub thumbnail: Option<PathBuf>,
    pub clip: Option<ClipSource>,
    /// Titleset number; `Some` opens the titleset menu, `None` plays the title directly.
    pub titleset_menu: Option<u32>,
}

/// One entry of a chapter menu.
#[derive(Debug, Clone)]
pub struct ChapterEntry {
    pub label: String,
    pub thumbnail: Option<PathBuf>,
    pub clip: Option<ClipSource>,
}

/// What a template page is used for.
#[derive(Debug, Clone, Copy)]
pub enum MenuKind<'a> {
    /// VMGM main menu listing all titles.
    Titles(&'a [TitleEntry]),
    /// VTSM hub: chapters, settings, back, play.
    Root,
    Chapters(&'a [ChapterEntry]),
    Settings { audio: &'a [String], subtitles: &'a [String] },
}

impl MenuKind<'_> {
    pub fn domain(&self) -> Domain {
        match self {
            MenuKind::Titles(_) => Domain::Vmgm,
            _ => Domain::Vtsm,
        }
    }
}

/// Menu numbers (PGC numbers within the domain) of the sibling menus.
#[derive(Debug, Clone, Copy, Default)]
pub struct MenuLinks {
    /// Menu number of page 1 of the menu being planned.
    pub first: u32,
    pub root: Option<u32>,
    pub chapters: Option<u32>,
    pub settings: Option<u32>,
    /// VMGM page that lists this title ("back to main menu"); `None` = the disc has no main
    /// menu, back buttons are hidden.
    pub main_menu: Option<u32>,
}

fn upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

fn count_loc(page: &Page, locs: &[&str]) -> usize {
    page.buttons.iter().filter(|b| locs.contains(&upper(&b.location).as_str())).count()
}

const TITLE_ITEMS: &[&str] = &["ML_PLAY_EPISODE", "ML_PLAY_SERIES_EPISODE", "ML_PLAY_BONUS"];

/// Number of item buttons (titles/chapters) a page shows.
pub fn items_per_page(page: &Page) -> usize {
    count_loc(page, TITLE_ITEMS) + count_loc(page, &["ML_PLAY_CHAPTER"])
}

/// Main menu page (1-based) on which title `index` (0-based) is listed.
pub fn title_page_of(page: &Page, index: usize) -> u32 {
    (index / count_loc(page, TITLE_ITEMS).max(1)) as u32 + 1
}

/// Number of menu pages needed for `kind` on `page`.
pub fn page_count(page: &Page, kind: &MenuKind) -> u32 {
    let pages = |items: usize, per: usize| items.div_ceil(per.max(1)).max(1);
    let n = match kind {
        MenuKind::Titles(t) => pages(t.len(), count_loc(page, TITLE_ITEMS)),
        MenuKind::Root => 1,
        MenuKind::Chapters(c) => pages(c.len(), count_loc(page, &["ML_PLAY_CHAPTER"])),
        MenuKind::Settings { audio, subtitles } => pages(audio.len(), count_loc(page, &["ML_SET_AUD"]))
            .max(pages(subtitles.len(), count_loc(page, &["ML_SET_STT"]))),
    };
    n as u32
}

/// Decides which template buttons are visible on page `page_no` (1-based) of a menu and what
/// they do. Item buttons beyond the available items are hidden, as are PREV on the first and
/// NEXT on the last page, and links to menus that do not exist.
///
/// Returns the buttons (with navigation computed) and warnings for unsupported actions.
pub fn plan_buttons(
    page: &Page,
    kind: &MenuKind,
    page_no: u32,
    links: MenuLinks,
    out_w: u32,
    out_h: u32,
) -> Result<(Vec<MenuButton>, Vec<String>), AuthorError> {
    let pages = page_count(page, kind);
    let menu_no = links.first + page_no - 1;
    let mut buttons = Vec::new();
    let mut warnings = Vec::new();

    for b in page.buttons.iter().filter(|b| b.visible) {
        let loc = upper(&b.location);
        // Index of the item this button shows, given `per` items of its kind per page.
        let item = |per: usize| ((page_no - 1) as usize) * per + b.param.max(0) as usize;
        let mut thumbnail = None;
        let mut clip = None;
        let mut label = None;

        let action = match (kind, loc.as_str()) {
            (MenuKind::Titles(titles), l) if TITLE_ITEMS.contains(&l) => {
                let i = item(count_loc(page, TITLE_ITEMS));
                let Some(t) = titles.get(i) else { continue };
                label = Some(t.label.clone());
                thumbnail = t.thumbnail.clone();
                clip = t.clip.clone();
                match t.titleset_menu {
                    Some(ts) => ButtonAction::OpenTitleset(ts),
                    None => ButtonAction::PlayTitle(i as u32 + 1),
                }
            }
            (MenuKind::Chapters(chapters), "ML_PLAY_CHAPTER") => {
                let i = item(count_loc(page, &["ML_PLAY_CHAPTER"]));
                let Some(c) = chapters.get(i) else { continue };
                label = Some(c.label.clone());
                thumbnail = c.thumbnail.clone();
                clip = c.clip.clone();
                ButtonAction::PlayChapter(i as u32 + 1)
            }
            (MenuKind::Settings { audio, .. }, "ML_SET_AUD") => {
                let i = item(count_loc(page, &["ML_SET_AUD"]));
                let Some(a) = audio.get(i) else { continue };
                label = Some(a.clone());
                ButtonAction::SetAudio(i as u32)
            }
            (MenuKind::Settings { subtitles, .. }, "ML_SET_STT") => {
                let i = item(count_loc(page, &["ML_SET_STT"]));
                let Some(s) = subtitles.get(i) else { continue };
                label = Some(s.clone());
                ButtonAction::SetSubtitle(Some(i as u32))
            }
            (MenuKind::Settings { subtitles, .. }, "ML_RESET_STT") => {
                if subtitles.is_empty() {
                    continue;
                }
                ButtonAction::SetSubtitle(None)
            }
            // Item buttons on a page used for another purpose.
            (_, "ML_PLAY_EPISODE" | "ML_PLAY_SERIES_EPISODE" | "ML_PLAY_BONUS" | "ML_PLAY_CHAPTER")
            | (_, "ML_SET_AUD" | "ML_SET_STT" | "ML_RESET_STT") => continue,
            (MenuKind::Titles(_), "ML_PLAY_ALL") => ButtonAction::PlayAll,
            (_, "ML_PLAY_ALL" | "ML_RESUME") => ButtonAction::PlayTitle(1),
            (_, "ML_PREV") if page_no > 1 => ButtonAction::Menu(menu_no - 1),
            (_, "ML_NEXT") if page_no < pages => ButtonAction::Menu(menu_no + 1),
            (_, "ML_PREV" | "ML_NEXT") => continue,
            (MenuKind::Titles(_), "ML_GO_TITLE_MENU" | "ML_BACK") => continue,
            (_, "ML_GO_TITLE_MENU") | (MenuKind::Root, "ML_BACK") => match links.main_menu {
                Some(p) => ButtonAction::MainMenu(p),
                None => continue, // the disc has no main menu
            },
            (_, "ML_BACK") => match links.root {
                Some(r) => ButtonAction::Menu(r),
                None => match links.main_menu {
                    Some(p) => ButtonAction::MainMenu(p),
                    None => continue,
                },
            },
            (_, "ML_GO_CHAPTER_MENU") => match links.chapters {
                Some(c) => ButtonAction::Menu(c),
                None => continue,
            },
            (_, "ML_GO_SETTINGS_MENU") => match links.settings {
                Some(s) => ButtonAction::Menu(s),
                None => continue,
            },
            (_, other) => {
                warnings.push(format!(
                    "button '{}': action {other} not supported, button stays on the menu",
                    b.name
                ));
                ButtonAction::Unsupported(other.to_string())
            }
        };
        let Some(rect) =
            Rect::from_template(b.x, b.y, b.width, b.height, page.width, page.height, out_w, out_h)
        else {
            warnings.push(format!("button '{}' lies outside the page, skipped", b.name));
            continue;
        };
        buttons.push(MenuButton {
            name: String::new(),
            template_index: b.index,
            location: b.location.clone(),
            rect,
            action,
            label,
            thumbnail,
            clip,
            nav: Navigation::default(),
        });
    }

    if buttons.len() > MAX_BUTTONS {
        return Err(AuthorError::TooManyButtons(buttons.len()));
    }
    // The template's default button becomes b1 (initially selected by the DVD player).
    if let Some(pos) = buttons
        .iter()
        .position(|mb| page.buttons.iter().any(|b| b.index == mb.template_index && b.is_default))
    {
        let default = buttons.remove(pos);
        buttons.insert(0, default);
    }
    for (i, b) in buttons.iter_mut().enumerate() {
        b.name = format!("b{}", i + 1);
    }
    compute_navigation(&mut buttons);
    Ok((buttons, warnings))
}

#[derive(Clone, Copy)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// Spatial remote-control navigation: for each direction pick the nearest button whose centre
/// lies within a 45° cone in that direction, falling back to the whole half-plane. Without any
/// candidate the button points to itself.
pub fn compute_navigation(buttons: &mut [MenuButton]) {
    let centers: Vec<(f64, f64)> = buttons.iter().map(|b| b.rect.center()).collect();
    let target = |from: usize, dir: Dir| -> usize {
        let (ax, ay) = centers[from];
        let mut best_cone: Option<(f64, usize)> = None;
        let mut best_half: Option<(f64, usize)> = None;
        for (i, &(bx, by)) in centers.iter().enumerate() {
            if i == from {
                continue;
            }
            let (primary, secondary) = match dir {
                Dir::Up => (ay - by, (bx - ax).abs()),
                Dir::Down => (by - ay, (bx - ax).abs()),
                Dir::Left => (ax - bx, (by - ay).abs()),
                Dir::Right => (bx - ax, (by - ay).abs()),
            };
            if primary <= 0.0 {
                continue;
            }
            let score = primary + 2.0 * secondary;
            let slot = if secondary <= primary { &mut best_cone } else { &mut best_half };
            if slot.is_none_or(|(s, _)| score < s) {
                *slot = Some((score, i));
            }
        }
        best_cone.or(best_half).map_or(from, |(_, i)| i)
    };
    let navs: Vec<Navigation> = (0..buttons.len())
        .map(|i| {
            let name = |d| Some(buttons[target(i, d)].name.clone());
            Navigation { up: name(Dir::Up), down: name(Dir::Down), left: name(Dir::Left), right: name(Dir::Right) }
        })
        .collect();
    for (b, nav) in buttons.iter_mut().zip(navs) {
        b.nav = nav;
    }
}

// ---------------------------------------------------------------------------
// XML generation
// ---------------------------------------------------------------------------

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One menu page (PGC).
#[derive(Debug, Clone, Serialize)]
pub struct MenuSpec {
    /// Menu MPEG with the spumux subpicture muxed in.
    pub vob: String,
    pub buttons: Vec<MenuButton>,
    /// dvdauthor `entry` attribute (`title` in the VMGM; `root`, `ptt`, `audio`, ... in a VTSM).
    pub entry: Option<String>,
    /// Motion menu: the VOB is a video loop replayed with `jump cell 1` instead of a still
    /// held with `pause="inf"`.
    pub motion: bool,
    /// Extra `pre` commands of this PGC (after the domain's own). With an empty `vob` the PGC
    /// is command-only (a dispatcher): it runs these commands and shows nothing.
    pub pre: Option<String>,
}

impl MenuSpec {
    /// A command-only PGC (no video, no buttons) running `commands`.
    pub fn commands(commands: impl Into<String>, entry: Option<&str>) -> Self {
        MenuSpec { vob: String::new(), buttons: vec![], entry: entry.map(str::to_string), motion: false, pre: Some(commands.into()) }
    }

    pub fn is_command_only(&self) -> bool {
        self.vob.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TitlesetSpec {
    /// Titleset menus; menu N is `menus[N-1]`.
    pub menus: Vec<MenuSpec>,
    pub title: TitleSpec,
    /// VMGM menu page that lists this title (where playback returns to).
    pub main_menu_page: u32,
}

/// What happens when the disc starts and when a title ends ("Menü Optionen").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlaybackOptions {
    /// Start title 1 when the disc is inserted instead of showing the main menu.
    pub auto_start: bool,
    /// After every title the next one plays (no menu in between).
    pub play_sequentially: bool,
    /// After the last title of a sequence start again with title 1; a single title repeats.
    pub loop_playback: bool,
    /// After a single title: back to the main menu page (`true`) or to the title's own menu.
    pub return_to_title_menu: bool,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        PlaybackOptions { auto_start: false, play_sequentially: false, loop_playback: false, return_to_title_menu: true }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DvdLayout {
    /// dvdauthor output directory.
    pub dest: String,
    /// Menu aspect ratio, `4:3` or `16:9`.
    pub menu_aspect: String,
    /// VMGM menu pages (main menu).
    pub vmgm_menus: Vec<MenuSpec>,
    /// One titleset per title; global title N is titleset N.
    pub titlesets: Vec<TitlesetSpec>,
    pub playback: PlaybackOptions,
    pub tv: TvStandard,
    /// Extra `pre` commands of every title PGC (custom layouts reset their dispatch register).
    pub title_pre: Option<String>,
}

/// Number of VM commands in a dvdauthor command string (statements end with `;`).
pub fn command_count(commands: &str) -> usize {
    commands.matches(';').count()
}

/// Writes a `<pre>`/`<post>` element after checking the VM command limit.
fn write_commands(x: &mut String, indent: &str, tag: &str, commands: &str, pgc: &str) -> Result<(), AuthorError> {
    let n = command_count(commands);
    if n > MAX_PGC_COMMANDS {
        return Err(AuthorError::TooManyCommands { pgc: pgc.to_string(), commands: n });
    }
    let _ = writeln!(x, "{indent}<{tag}>{}</{tag}>", xml_escape(commands));
    Ok(())
}

/// `pre` is put on every menu PGC (VMGM: reset the play-all flag).
fn write_menus(
    x: &mut String,
    indent: &str,
    tv: TvStandard,
    aspect: &str,
    menus: &[MenuSpec],
    domain: Domain,
    pre: &str,
) -> Result<(), AuthorError> {
    let _ = writeln!(x, "{indent}<menus>");
    let _ = writeln!(x, r#"{indent}  <video format="{}" aspect="{}"/>"#, tv.dvdauthor_format(), xml_escape(aspect));
    for (i, menu) in menus.iter().enumerate() {
        let menu_no = i as u32 + 1;
        if menu.buttons.len() > MAX_BUTTONS {
            return Err(AuthorError::TooManyButtons(menu.buttons.len()));
        }
        let entry = menu.entry.as_ref().map(|e| format!(r#" entry="{}""#, xml_escape(e))).unwrap_or_default();
        let _ = writeln!(x, "{indent}  <pgc{entry}>");
        let name = if menu.is_command_only() { format!("menu {menu_no} (dispatcher)") } else { menu.vob.clone() };
        let all_pre = match (&menu.pre, menu.is_command_only()) {
            // Dispatchers rely on the registers the caller set; no domain reset in front.
            (Some(own), true) => own.clone(),
            (Some(own), false) if !pre.is_empty() => format!("{pre} {own}"),
            (Some(own), false) => own.clone(),
            (None, _) => pre.to_string(),
        };
        if !all_pre.is_empty() {
            write_commands(x, &format!("{indent}    "), "pre", &all_pre, &name)?;
        }
        if menu.is_command_only() {
            let _ = writeln!(x, "{indent}  </pgc>");
            continue;
        }
        let ctx = CommandContext { domain, menu: menu_no };
        for b in &menu.buttons {
            let _ = writeln!(
                x,
                r#"{indent}    <button name="{}">{}</button>"#,
                xml_escape(&b.name),
                xml_escape(&b.action.command(ctx))
            );
        }
        if menu.motion {
            // Seamless loop: replaying the cell does not re-run the PGC's pre commands.
            let _ = writeln!(x, r#"{indent}    <vob file="{}"/>"#, xml_escape(&menu.vob));
            let _ = writeln!(x, "{indent}    <post>jump cell 1;</post>");
        } else {
            let _ = writeln!(x, r#"{indent}    <vob file="{}" pause="inf"/>"#, xml_escape(&menu.vob));
        }
        let _ = writeln!(x, "{indent}  </pgc>");
    }
    let _ = writeln!(x, "{indent}</menus>");
    Ok(())
}

/// VMGM menu number of the dispatcher PGC of title `title` (1-based).
pub fn dispatcher_menu(pages: usize, title: usize) -> usize {
    pages + title
}

/// Commands of title `title`'s dispatcher (runs when the title ends). At most 3 commands:
/// - sequential playback / play-all: next title (title 1 again after the last with looping),
/// - otherwise: repeat the title (looping), or back to its main menu page or its own menu.
fn dispatcher_commands(title: usize, titles: usize, page: u32, has_ts_menu: bool, opts: PlaybackOptions) -> String {
    let next = if title < titles {
        Some(title + 1)
    } else if opts.loop_playback {
        Some(1)
    } else {
        None
    };
    let to_menu = if !opts.return_to_title_menu && has_ts_menu {
        format!("jump titleset {title} menu;")
    } else {
        format!("jump menu {page};")
    };
    if opts.play_sequentially {
        return match next {
            Some(n) => format!("jump title {n};"),
            None => format!("g1 = 0; {to_menu}"),
        };
    }
    let single_end = if opts.loop_playback { format!("jump title {title};") } else { to_menu };
    match next {
        Some(n) => format!("if (g1 eq 1) jump title {n}; {single_end}"),
        None => format!("g1 = 0; {single_end}"),
    }
}

/// Post command of a title on a disc without main menu (single title): repeat, show the
/// title's own menu, or stop.
fn post_without_main_menu(has_ts_menu: bool, opts: PlaybackOptions) -> &'static str {
    if opts.loop_playback {
        "jump title 1;"
    } else if has_ts_menu {
        "call menu;"
    } else {
        "exit;"
    }
}

/// Generates the `dvdauthor -x` project XML (PAL).
pub fn dvdauthor_xml(layout: &DvdLayout) -> Result<String, AuthorError> {
    if layout.titlesets.is_empty() {
        return Err(AuthorError::NoTitles);
    }
    if layout.titlesets.len() > MAX_TITLES {
        return Err(AuthorError::TooManyTitles(layout.titlesets.len()));
    }
    for ts in &layout.titlesets {
        ts.title.validate()?;
    }

    let mut x = String::new();
    // `write!` into a String cannot fail.
    let _ = writeln!(x, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    // No `jumppad`: dvdauthor would add a jump table with entries per titleset to the VMGM,
    // which exceeds the 128-command limit with many titles. All our jumps (VMGM -> title or
    // titleset menu, VTSM -> VMGM menu, title -> call VMGM menu) are allowed without it.
    let _ = writeln!(x, r#"<dvdauthor dest="{}">"#, xml_escape(&layout.dest));
    let _ = writeln!(x, "  <vmgm>");
    let pages = layout.vmgm_menus.len();
    let titles = layout.titlesets.len();
    let opts = layout.playback;
    let tv = layout.tv;
    let first_has_menu = layout.titlesets.first().is_some_and(|ts| !ts.menus.is_empty());
    if pages == 0 {
        // No main menu: start the (single) title, or its own menu.
        let fpc = if first_has_menu && !opts.auto_start { "g1 = 0; jump titleset 1 menu;" } else { "g1 = 0; jump title 1;" };
        let _ = writeln!(x, "    <fpc>{fpc}</fpc>");
        // dvdauthor needs a video format for the VMGM even without any menu in it.
        let _ = writeln!(x, "    <menus>");
        let _ = writeln!(x, r#"      <video format="{}" aspect="{}"/>"#, tv.dvdauthor_format(), xml_escape(&layout.menu_aspect));
        let _ = writeln!(x, "    </menus>");
    } else {
        let fpc = if opts.auto_start { "g1 = 0; jump title 1;" } else { "g1 = 0; jump menu 1;" };
        let _ = writeln!(x, "    <fpc>{fpc}</fpc>");
        // Visible pages: only reset the play-all flag. Then one command-only dispatcher per title.
        let mut menus = String::new();
        write_menus(&mut menus, "    ", tv, &layout.menu_aspect, &layout.vmgm_menus, Domain::Vmgm, "g1 = 0;")?;
        let close = "    </menus>\n";
        let at = menus.rfind(close).expect("write_menus closes the menus element");
        menus.truncate(at);
        for (i, ts) in layout.titlesets.iter().enumerate() {
            let page = ts.main_menu_page.clamp(1, pages as u32);
            let _ = writeln!(menus, "      <!-- dispatcher for title {} -->", i + 1);
            let _ = writeln!(menus, "      <pgc>");
            let name = format!("VMGM dispatcher {}", i + 1);
            let commands = dispatcher_commands(i + 1, titles, page, !ts.menus.is_empty(), opts);
            write_commands(&mut menus, "        ", "pre", &commands, &name)?;
            let _ = writeln!(menus, "      </pgc>");
        }
        menus.push_str(close);
        x.push_str(&menus);
    }
    let _ = writeln!(x, "  </vmgm>");

    for (i, ts) in layout.titlesets.iter().enumerate() {
        let back = if pages == 0 {
            post_without_main_menu(!ts.menus.is_empty(), opts).to_string()
        } else {
            format!("call vmgm menu {};", dispatcher_menu(pages, i + 1))
        };
        let t = &ts.title;
        let _ = writeln!(x, "  <!-- titleset {}: {} -->", i + 1, xml_escape(&t.label).replace("--", "- -"));
        let _ = writeln!(x, "  <titleset>");
        if !ts.menus.is_empty() {
            write_menus(&mut x, "    ", tv, &layout.menu_aspect, &ts.menus, Domain::Vtsm, "")?;
        }
        let _ = writeln!(x, "    <titles>");
        let widescreen = if t.aspect == "16:9" { r#" widescreen="nopanscan""# } else { "" };
        let _ = writeln!(x, r#"      <video format="{}" aspect="{}"{widescreen}/>"#, tv.dvdauthor_format(), xml_escape(&t.aspect));
        for (n, lang) in t.audio_langs.iter().enumerate() {
            let lang = lang.as_ref().map(|l| format!(r#" lang="{}""#, xml_escape(l))).unwrap_or_default();
            let format = t.audio_formats.get(n).map_or("ac3", String::as_str);
            let _ = writeln!(x, r#"      <audio format="{}"{lang}/>"#, xml_escape(format));
        }
        for (id, lang) in t.subtitle_langs.iter().enumerate() {
            let lang = lang.as_ref().map(|l| format!(r#" lang="{}""#, xml_escape(l))).unwrap_or_default();
            if t.aspect == "16:9" {
                // One subpicture stream serves both the widescreen and the letterbox display mode.
                let _ = writeln!(x, r#"      <subpicture{lang}>"#);
                let _ = writeln!(x, r#"        <stream id="{id}" mode="widescreen"/>"#);
                let _ = writeln!(x, r#"        <stream id="{id}" mode="letterbox"/>"#);
                let _ = writeln!(x, "      </subpicture>");
            } else {
                let _ = writeln!(x, r#"      <subpicture{lang}/>"#);
            }
        }
        let _ = writeln!(x, "      <pgc>");
        let mut title_pre = layout.title_pre.clone().unwrap_or_default();
        if t.force_subtitle && !t.subtitle_langs.is_empty() {
            // Subtitle stream 0 with the display flag (bit 6).
            title_pre = format!("{title_pre} subtitle = 64;").trim().to_string();
        }
        if !title_pre.is_empty() {
            write_commands(&mut x, "        ", "pre", &title_pre, &t.vob)?;
        }
        let _ = writeln!(
            x,
            r#"        <vob file="{}" chapters="{}"/>"#,
            xml_escape(&t.vob),
            xml_escape(&t.chapters.join(","))
        );
        write_commands(&mut x, "        ", "post", &back, &t.vob)?;
        let _ = writeln!(x, "      </pgc>");
        let _ = writeln!(x, "    </titles>");
        let _ = writeln!(x, "  </titleset>");
    }
    let _ = writeln!(x, "</dvdauthor>");
    Ok(x)
}

/// Generates the `spumux` XML that muxes the highlight/select PNGs into a menu MPEG
/// and defines button geometry and navigation.
pub fn spumux_xml(tv: TvStandard, highlight_png: &str, select_png: &str, buttons: &[MenuButton]) -> String {
    let mut x = String::new();
    let _ = writeln!(x, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(x, r#"<subpictures format="{}">"#, tv.spumux_format());
    let _ = writeln!(x, "  <stream>");
    let _ = writeln!(
        x,
        r#"    <spu start="00:00:00.00" force="yes" highlight="{}" select="{}">"#,
        xml_escape(highlight_png),
        xml_escape(select_png)
    );
    for b in buttons {
        let r = b.rect;
        let _ = write!(
            x,
            r#"      <button name="{}" x0="{}" y0="{}" x1="{}" y1="{}""#,
            xml_escape(&b.name),
            r.x0,
            r.y0,
            r.x1,
            r.y1
        );
        for (attr, target) in [("up", &b.nav.up), ("down", &b.nav.down), ("left", &b.nav.left), ("right", &b.nav.right)] {
            if let Some(t) = target {
                let _ = write!(x, r#" {attr}="{}""#, xml_escape(t));
            }
        }
        let _ = writeln!(x, "/>");
    }
    let _ = writeln!(x, "    </spu>");
    let _ = writeln!(x, "  </stream>");
    let _ = writeln!(x, "</subpictures>");
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(name: &str, x0: u32, y0: u32, x1: u32, y1: u32) -> MenuButton {
        MenuButton {
            name: name.to_string(),
            template_index: 0,
            location: "ML_PLAY_EPISODE".into(),
            rect: Rect { x0, y0, x1, y1 },
            action: ButtonAction::PlayTitle(1),
            label: None,
            thumbnail: None,
            clip: None,
            nav: Navigation::default(),
        }
    }

    const VMGM1: CommandContext = CommandContext { domain: Domain::Vmgm, menu: 1 };
    const VTSM3: CommandContext = CommandContext { domain: Domain::Vtsm, menu: 3 };

    #[test]
    fn rect_is_even_and_clipped() {
        let r = Rect::from_template(587, 475, 80, 76, 720, 576, 720, 576).unwrap();
        assert_eq!(r, Rect { x0: 587, y0: 474, x1: 667, y1: 552 });
        let r = Rect::from_template(700, 570, 80, 76, 720, 576, 720, 576).unwrap();
        assert_eq!((r.x1, r.y1), (720, 576));
        assert!(Rect::from_template(800, 0, 10, 10, 720, 576, 720, 576).is_none());
        // Scaling from an HD page.
        let r = Rect::from_template(0, 0, 1920, 1080, 1920, 1080, 720, 576).unwrap();
        assert_eq!(r, Rect { x0: 0, y0: 0, x1: 720, y1: 576 });
    }

    #[test]
    fn navigation_list_with_bottom_row() {
        // Vertical list of two items, then a row of three icons below.
        let mut bs = vec![
            button("b1", 52, 108, 668, 134),
            button("b2", 52, 142, 668, 168),
            button("b3", 407, 474, 487, 552),
            button("b4", 497, 474, 577, 552),
            button("b5", 587, 474, 667, 552),
        ];
        compute_navigation(&mut bs);
        let nav = |i: usize| &bs[i].nav;
        assert_eq!(nav(0).down.as_deref(), Some("b2"));
        assert_eq!(nav(0).up.as_deref(), Some("b1"));
        assert_eq!(nav(1).up.as_deref(), Some("b1"));
        assert_eq!(nav(1).down.as_deref(), Some("b3"));
        assert_eq!(nav(2).right.as_deref(), Some("b4"));
        assert_eq!(nav(3).left.as_deref(), Some("b3"));
        assert_eq!(nav(4).right.as_deref(), Some("b5"));
        assert_eq!(nav(3).up.as_deref(), Some("b2"));
    }

    #[test]
    fn title_spec_cli() {
        let t = TitleSpec::parse_cli(r"C:\vids\Movie One.mpg@0,10:00,1:02:03.5").unwrap();
        assert_eq!(t.vob, r"C:\vids\Movie One.mpg");
        assert_eq!(t.label, "Movie One");
        assert_eq!(t.chapters, vec!["0", "10:00", "1:02:03.5"]);
        assert_eq!(TitleSpec::parse_cli("a.mpg").unwrap().chapters, vec!["0"]);
        assert!(matches!(TitleSpec::parse_cli("a.mpg@0,abc"), Err(AuthorError::InvalidChapter(_))));
        assert!(matches!(TitleSpec::parse_cli("a.mpg@1:75"), Err(AuthorError::InvalidChapter(_))));
    }

    #[test]
    fn chapter_marks() {
        assert_eq!(chapter_mark(0.0), "0:00:00");
        assert_eq!(chapter_mark(3723.5), "1:02:03.500");
        let t = TitleSpec { chapters: vec![chapter_mark(0.0), chapter_mark(612.04)], ..TitleSpec::placeholder(1) };
        assert!(t.validate().is_ok());
    }

    #[test]
    fn commands() {
        assert_eq!(ButtonAction::PlayTitle(3).command(VMGM1), "g1 = 0; jump title 3;");
        assert_eq!(ButtonAction::PlayAll.command(VMGM1), "g1 = 1; jump title 1;");
        assert_eq!(ButtonAction::OpenTitleset(2).command(VMGM1), "g1 = 0; jump titleset 2 menu;");
        assert_eq!(ButtonAction::Menu(2).command(VMGM1), "jump menu 2;");
        assert_eq!(ButtonAction::Menu(2).command(VTSM3), "jump menu 2;");
        assert_eq!(ButtonAction::PlayChapter(4).command(VTSM3), "g1 = 0; jump title 1 chapter 4;");
        assert_eq!(ButtonAction::SetAudio(1).command(VTSM3), "audio = 1; jump menu 3;");
        assert_eq!(ButtonAction::SetSubtitle(Some(0)).command(VTSM3), "subtitle = 64; jump menu 3;");
        assert_eq!(ButtonAction::SetSubtitle(None).command(VTSM3), "subtitle = 0; jump menu 3;");
        assert_eq!(ButtonAction::MainMenu(4).command(VTSM3), "jump vmgm menu 4;");
        assert_eq!(ButtonAction::MainMenu(2).command(VMGM1), "jump menu 2;");
        assert_eq!(ButtonAction::Unsupported("ML_X".into()).command(VTSM3), "jump menu 3;");
    }

    #[test]
    fn dvdauthor_xml_structure() {
        let mut b = button("b1", 0, 0, 10, 10);
        b.action = ButtonAction::OpenTitleset(1);
        let mut c = button("b1", 0, 0, 10, 10);
        c.action = ButtonAction::PlayChapter(2);
        let title = |vob: &str, aspect: &str| TitleSpec {
            vob: vob.into(),
            label: "A".into(),
            chapters: vec!["0".into(), "5:00".into()],
            aspect: aspect.into(),
            audio_langs: vec![Some("de".into()), None],
            audio_formats: vec!["ac3".into(), "dts".into()],
            subtitle_langs: vec![Some("en".into())],
            force_subtitle: false,
        };
        let layout = DvdLayout {
            dest: "DVD".into(),
            menu_aspect: "4:3".into(),
            playback: PlaybackOptions::default(),
            tv: TvStandard::Pal,
            title_pre: None,
            vmgm_menus: vec![
                MenuSpec { vob: "menu.mpg".into(), buttons: vec![b.clone()], entry: Some("title".into()), motion: false, pre: None },
                MenuSpec { vob: "menu_2.mpg".into(), buttons: vec![b], entry: None, motion: false, pre: None },
            ],
            titlesets: vec![
                TitlesetSpec {
                    menus: vec![MenuSpec { vob: "ts1_menu.mpg".into(), buttons: vec![c], entry: Some("root".into()), motion: true, pre: None }],
                    title: title("a&b.mpg", "16:9"),
                    main_menu_page: 1,
                },
                TitlesetSpec { menus: vec![], title: title("c.mpg", "4:3"), main_menu_page: 2 },
            ],
        };
        let x = dvdauthor_xml(&layout).unwrap();
        assert!(x.contains("<fpc>g1 = 0; jump menu 1;</fpc>"));
        assert!(x.contains(r#"<pgc entry="title">"#));
        // Visible pages only reset the play-all flag ...
        assert_eq!(x.matches("<pre>g1 = 0;</pre>").count(), 2);
        assert!(x.contains(r#"<button name="b1">g1 = 0; jump titleset 1 menu;</button>"#));
        // ... and each title has a tiny dispatcher PGC (menus 3 and 4) returning to its own page.
        assert!(x.contains("<pre>if (g1 eq 1) jump title 2; jump menu 1;</pre>"));
        assert!(x.contains("<pre>g1 = 0; jump menu 2;</pre>"));
        assert!(x.contains("<post>call vmgm menu 3;</post>"));
        assert!(x.contains("<post>call vmgm menu 4;</post>"));
        assert!(!x.contains("g2") && !x.contains("g3"), "no page/title registers any more");
        assert!(x.contains(r#"<pgc entry="root">"#));
        // Still menus pause forever, motion menus loop their cell.
        assert!(x.contains(r#"<vob file="menu.mpg" pause="inf"/>"#));
        assert!(x.contains(r#"<vob file="ts1_menu.mpg"/>"#));
        assert!(!x.contains(r#"<vob file="ts1_menu.mpg" pause"#));
        assert_eq!(x.matches("<post>jump cell 1;</post>").count(), 1);
        assert!(x.contains(r#"<button name="b1">g1 = 0; jump title 1 chapter 2;</button>"#));
        assert!(x.contains(r#"<video format="pal" aspect="16:9" widescreen="nopanscan"/>"#));
        assert!(x.contains(r#"<audio format="ac3" lang="de"/>"#));
        assert!(x.contains(r#"<audio format="dts"/>"#));
        assert!(x.contains(r#"<stream id="0" mode="widescreen"/>"#));
        assert!(x.contains(r#"<subpicture lang="en"/>"#));
        assert!(x.contains(r#"<vob file="a&amp;b.mpg" chapters="0,5:00"/>"#));
        assert_eq!(x.matches("<titleset>").count(), 2);

        let empty = DvdLayout { titlesets: vec![], ..layout };
        assert_eq!(dvdauthor_xml(&empty), Err(AuthorError::NoTitles));
    }

    /// Regression: 46 titles blew dvdauthor's limit of 128 commands per PGC.
    #[test]
    fn command_count_does_not_grow_with_titles() {
        let pages = 17; // 99 titles at 6 per page
        let mut b = button("b1", 0, 0, 10, 10);
        b.action = ButtonAction::PlayAll;
        let layout = DvdLayout {
            dest: "DVD".into(),
            menu_aspect: "4:3".into(),
            playback: PlaybackOptions::default(),
            tv: TvStandard::Pal,
            title_pre: None,
            vmgm_menus: (0..pages)
                .map(|i| MenuSpec { vob: format!("m{i}.mpg"), buttons: vec![b.clone()], entry: None, motion: false, pre: None })
                .collect(),
            titlesets: (1..=MAX_TITLES)
                .map(|n| TitlesetSpec {
                    menus: vec![],
                    title: TitleSpec::placeholder(n),
                    main_menu_page: ((n - 1) / 6 + 1) as u32,
                })
                .collect(),
        };
        let x = dvdauthor_xml(&layout).unwrap();
        let re = Regex::new(r"<(pre|post|fpc)>([^<]*)</(pre|post|fpc)>").unwrap();
        let max = re.captures_iter(&x).map(|c| command_count(&c[2])).max().unwrap();
        assert!(max <= 3, "largest command block has {max} commands");
        assert!(!x.contains("jumppad"), "jumppad makes dvdauthor generate per-titleset jump tables");
        // Last title ends play-all; title 7 returns to page 2; dispatchers follow the 17 pages.
        assert!(x.contains("<pre>g1 = 0; jump menu 17;</pre>"));
        assert!(x.contains("<pre>if (g1 eq 1) jump title 8; jump menu 2;</pre>"));
        assert!(x.contains(&format!("<post>call vmgm menu {};</post>", pages + 99)));
        assert_eq!(dispatcher_menu(pages, 1), 18);
    }

    fn dispatchers(titles: usize, opts: PlaybackOptions, ts_menus: bool) -> String {
        let mut b = button("b1", 0, 0, 10, 10);
        b.action = ButtonAction::PlayAll;
        let layout = DvdLayout {
            dest: "DVD".into(),
            menu_aspect: "4:3".into(),
            playback: opts,
            tv: TvStandard::Pal,
            title_pre: None,
            vmgm_menus: vec![MenuSpec { vob: "m.mpg".into(), buttons: vec![b.clone()], entry: Some("title".into()), motion: false, pre: None }],
            titlesets: (1..=titles)
                .map(|n| TitlesetSpec {
                    menus: if ts_menus {
                        vec![MenuSpec { vob: format!("ts{n}.mpg"), buttons: vec![b.clone()], entry: Some("root".into()), motion: false, pre: None }]
                    } else {
                        vec![]
                    },
                    title: TitleSpec::placeholder(n),
                    main_menu_page: 1,
                })
                .collect(),
        };
        dvdauthor_xml(&layout).unwrap()
    }

    #[test]
    fn playback_options() {
        let base = PlaybackOptions::default();
        // Default: play-all continues, single titles return to the main menu page.
        let x = dispatchers(2, base, true);
        assert!(x.contains("<fpc>g1 = 0; jump menu 1;</fpc>"));
        assert!(x.contains("<pre>if (g1 eq 1) jump title 2; jump menu 1;</pre>"));
        assert!(x.contains("<pre>g1 = 0; jump menu 1;</pre>"));

        // Auto start: the disc begins with title 1.
        let x = dispatchers(2, PlaybackOptions { auto_start: true, ..base }, true);
        assert!(x.contains("<fpc>g1 = 0; jump title 1;</fpc>"));

        // Sequential: always the next title, menu after the last one.
        let x = dispatchers(3, PlaybackOptions { play_sequentially: true, ..base }, true);
        assert!(x.contains("<pre>jump title 2;</pre>") && x.contains("<pre>jump title 3;</pre>"));
        assert!(x.contains("<pre>g1 = 0; jump menu 1;</pre>"));

        // Sequential + loop: back to title 1 after the last.
        let x = dispatchers(3, PlaybackOptions { play_sequentially: true, loop_playback: true, ..base }, true);
        assert_eq!(x.matches("<pre>jump title 1;</pre>").count(), 1);

        // Loop without sequence: single titles repeat, play-all wraps around.
        let x = dispatchers(2, PlaybackOptions { loop_playback: true, ..base }, true);
        assert!(x.contains("<pre>if (g1 eq 1) jump title 2; jump title 1;</pre>"));
        assert!(x.contains("<pre>if (g1 eq 1) jump title 1; jump title 2;</pre>"));

        // Not back to the title menu: the title's own (titleset) menu instead.
        let x = dispatchers(2, PlaybackOptions { return_to_title_menu: false, ..base }, true);
        assert!(x.contains("<pre>if (g1 eq 1) jump title 2; jump titleset 1 menu;</pre>"));
        // ... unless the titleset has no menu.
        let x = dispatchers(2, PlaybackOptions { return_to_title_menu: false, ..base }, false);
        assert!(x.contains("<pre>if (g1 eq 1) jump title 2; jump menu 1;</pre>"));
    }

    #[test]
    fn single_title_without_main_menu() {
        let ts = |menus: bool, force: bool| TitlesetSpec {
            menus: if menus {
                vec![MenuSpec { vob: "ts.mpg".into(), buttons: vec![], entry: Some("root".into()), motion: false, pre: None }]
            } else {
                vec![]
            },
            title: TitleSpec { force_subtitle: force, subtitle_langs: vec![Some("de".into())], ..TitleSpec::placeholder(1) },
            main_menu_page: 1,
        };
        let layout = |t: TitlesetSpec, playback: PlaybackOptions| DvdLayout {
            dest: "DVD".into(),
            menu_aspect: "4:3".into(),
            vmgm_menus: vec![],
            titlesets: vec![t],
            playback,
            tv: TvStandard::Pal,
            title_pre: None,
        };
        let base = PlaybackOptions::default();
        let x = dvdauthor_xml(&layout(ts(true, false), base)).unwrap();
        assert!(x.contains("<fpc>g1 = 0; jump titleset 1 menu;</fpc>"));
        assert!(x.contains("<post>call menu;</post>"));
        assert!(!x.contains("<menus>
      <video format=\"pal\" aspect=\"4:3\"/>
      <pgc entry=\"title\""));

        let x = dvdauthor_xml(&layout(ts(false, true), PlaybackOptions { loop_playback: true, ..base })).unwrap();
        assert!(x.contains("<fpc>g1 = 0; jump title 1;</fpc>"));
        assert!(x.contains("<post>jump title 1;</post>"));
        assert!(x.contains("<pre>subtitle = 64;</pre>"), "first subtitle forced on");

        let x = dvdauthor_xml(&layout(ts(false, false), base)).unwrap();
        assert!(x.contains("<post>exit;</post>"));
        assert!(!x.contains("subtitle = 64"));
    }

    #[test]
    fn ntsc_formats() {
        assert_eq!(TvStandard::Ntsc.size(), (720, 480));
        assert_eq!(TvStandard::Pal.size(), (720, 576));
        assert_eq!(TvStandard::Ntsc.frame_rate(), "30000/1001");
        assert!(TvStandard::Ntsc.gop() <= 18 && TvStandard::Pal.gop() <= 15);
        let layout = DvdLayout {
            dest: "DVD".into(),
            menu_aspect: "4:3".into(),
            vmgm_menus: vec![MenuSpec { vob: "m.mpg".into(), buttons: vec![], entry: Some("title".into()), motion: false, pre: None }],
            titlesets: vec![TitlesetSpec { menus: vec![], title: TitleSpec::placeholder(1), main_menu_page: 1 }],
            playback: PlaybackOptions::default(),
            tv: TvStandard::Ntsc,
            title_pre: None,
        };
        let x = dvdauthor_xml(&layout).unwrap();
        assert_eq!(x.matches(r#"<video format="ntsc""#).count(), 2, "menus and titles");
        assert!(!x.contains(r#"format="pal""#));
    }

    #[test]
    fn spumux_xml_buttons() {
        let mut bs = vec![button("b1", 52, 108, 668, 134), button("b2", 52, 142, 668, 168)];
        compute_navigation(&mut bs);
        let x = spumux_xml(TvStandard::Pal, "hl.png", "sel.png", &bs);
        assert!(x.contains(r#"<subpictures format="PAL">"#));
        assert!(spumux_xml(TvStandard::Ntsc, "hl.png", "sel.png", &bs).contains(r#"<subpictures format="NTSC">"#));
        assert!(x.contains(r#"highlight="hl.png" select="sel.png""#));
        assert!(x.contains(r#"<button name="b1" x0="52" y0="108" x1="668" y1="134" up="b1" down="b2" left="b1" right="b1"/>"#));
    }
}
