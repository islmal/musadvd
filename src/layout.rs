//! Custom DVD menu layouts from the menu editor (JSON), as an alternative to ConvertX templates.
//!
//! A layout is a list of menus; every menu is either a **title menu** (VMGM, reached with the
//! remote's "Title" key; the first one is shown when the disc starts) or a **root menu** of one
//! title (that title's VTSM, reached with the "Menu" key during playback). Menus hold text, image
//! and video elements on a design canvas (normally the TV frame size). Any element can be a
//! button with an action: play a title, play a chapter, open another menu, play all.
//!
//! DVD constraints enforced by [`validate`]: rectangular hitboxes only (buttons cannot be
//! rotated), at most [`MAX_BUTTONS`] buttons per menu, no overlapping buttons, every PGC within
//! [`MAX_PGC_COMMANDS`] VM commands, all targets exist.
//!
//! Navigation across domains uses general registers:
//! - `g1` play-all flag (shared with the template path and the title-end dispatchers),
//! - `g2` dispatch code for a titleset's entry PGC: `1..=99` = chapter, `100 + j` = its j-th
//!   root menu; reset to 0 by every title and every visible root menu,
//! - `g3` code for the VMGM router (jumps a VTSM cannot do itself): `1000 + t` = play title t,
//!   `2000 + t` = enter titleset t's menus (with `g2` set by the caller).
//!
//! Rendering ([`build_custom_menus`]): static elements are drawn with the template renderer's
//! text/image code into frame-sized transparent PNG layers; video elements stay separate layers
//! so ffmpeg composites them in z-order ([`crate::encoder::custom_menu_args`]); highlight and
//! select masks are 4-colour palette PNGs (0 transparent, 1 pattern, 2 emphasis 1 (anti-aliased
//! text edges), 3 emphasis 2 unused).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use image::{imageops, imageops::FilterType, RgbaImage};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::author::{
    self, command_count, ButtonAction, ClipSource, MenuButton, MenuSpec, Navigation, Rect, TvStandard, MAX_BUTTONS,
    MAX_CHAPTERS, MAX_PGC_COMMANDS, MenuFrame,
};
use crate::encoder::{BackgroundOverlay, CustomMenuMedia, Fit, MenuBackground, MenuLayer};
use crate::menu::{self, IndexedImage, MenuPageFiles};

pub const LAYOUT_VERSION: u32 = 1;
/// Loop length of a moving custom menu when the layout does not say.
pub const DEFAULT_MENU_SECS: f64 = 20.0;
/// Command resetting the titleset dispatch register (title PGCs and visible root menus).
pub const RESET_DISPATCH: &str = "g2 = 0;";

// ---------------------------------------------------------------------------
// Model (JSON written by the menu editor)
// ---------------------------------------------------------------------------

fn version_1() -> u32 {
    LAYOUT_VERSION
}
fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomLayout {
    #[serde(default = "version_1")]
    pub version: u32,
    /// Coordinate space of all elements (the editor's stage).
    #[serde(default)]
    pub canvas: Canvas,
    #[serde(default)]
    pub highlight: Highlight,
    pub menus: Vec<CustomMenu>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas { width: 720, height: 576 }
    }
}

/// Button colours (subpicture palette).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Highlight {
    /// Selected button (`#RRGGBB`).
    pub color: String,
    /// Activated button.
    pub select_color: String,
    /// 0..1
    pub opacity: f32,
}

impl Default for Highlight {
    fn default() -> Self {
        Highlight { color: "#FFD200".into(), select_color: "#FF6A00".into(), opacity: 0.8 }
    }
}

/// Own colours of one button (instead of the layout's [`Highlight`]). A menu can show at most
/// [`MAX_COLOR_PAIRS`] different pairs: every pair needs its own pixel value of the 4-colour
/// subpicture (one value is transparent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonColors {
    pub color: String,
    pub select_color: String,
}

/// Different button colour pairs one menu can use.
pub const MAX_COLOR_PAIRS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MenuRole {
    /// VMGM menu ("Title" key, disc start).
    Title,
    /// Root menu of one title (its VTSM, "Menu" key).
    Root,
    /// Audio/subtitle selection of one title (its VTSM, after its root menus).
    Settings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMenu {
    /// Unique id, referenced by `menu` actions.
    pub id: String,
    pub name: String,
    pub role: MenuRole,
    /// Root and settings menus: the 1-based title they belong to.
    #[serde(default)]
    pub title: Option<u32>,
    #[serde(default)]
    pub background: Background,
    /// Loop length of a moving menu (videos, video background or sound).
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub audio: MenuSound,
    /// Bottom to top.
    #[serde(default)]
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Background {
    /// `#RRGGBB`, used when there is neither image nor video.
    pub color: String,
    pub image: Option<String>,
    pub video: Option<VideoRef>,
    /// Gaussian blur sigma in pixels (0 = sharp, max. 50).
    pub blur: f32,
    /// Shading over the background so texts stay legible.
    #[serde(rename = "gradientOverlay")]
    pub overlay: BackgroundOverlay,
}

impl Default for Background {
    fn default() -> Self {
        Background { color: "#000000".into(), image: None, video: None, blur: 0.0, overlay: BackgroundOverlay::None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoRef {
    /// 1-based title.
    pub title: u32,
    /// Start in the title (default: 10 % of its runtime).
    #[serde(default)]
    pub start_secs: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MenuSound {
    #[default]
    None,
    Title { title: u32, #[serde(default)] start_secs: Option<f64> },
    File { path: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    #[serde(default)]
    pub id: String,
    #[serde(flatten)]
    pub content: Content,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Clockwise, multiple of 90; buttons must stay at 0.
    #[serde(default)]
    pub rotation: u32,
    #[serde(default = "one")]
    pub opacity: f32,
    #[serde(default)]
    pub action: Option<Action>,
    /// Button colours of this element; `None` = the layout's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<ButtonColors>,
    /// Editor-only grouping (elements sharing an id move together); ignored by the build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

fn default_font() -> String {
    "Arial".into()
}
fn default_size() -> f32 {
    32.0
}
fn white() -> String {
    "#FFFFFF".into()
}
fn center() -> String {
    "center".into()
}
fn cover() -> Fit {
    Fit::Cover
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Content {
    Text {
        text: String,
        #[serde(default = "default_font")]
        font: String,
        #[serde(default = "default_size")]
        size: f32,
        #[serde(default = "white")]
        color: String,
        /// `left`, `center`, `right`.
        #[serde(default = "center")]
        align: String,
        #[serde(default)]
        bold: bool,
        #[serde(default)]
        italic: bool,
        #[serde(default)]
        shadow: bool,
        /// Break lines at the box width (otherwise only at `\n`; overlong lines end with "…").
        #[serde(default)]
        wrap: bool,
    },
    Image {
        src: String,
        #[serde(default)]
        fit: Fit,
    },
    /// Moving clip of a title.
    Video {
        title: u32,
        #[serde(default)]
        start_secs: Option<f64>,
        #[serde(default = "cover")]
        fit: Fit,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Action {
    PlayTitle { title: u32 },
    PlayChapter { title: u32, chapter: u32 },
    Menu { menu: String },
    PlayAll,
    /// Select the audio track with this source stream index (ffprobe index of the input) – the
    /// build maps it to the DVD audio stream number. Root/settings menus only.
    SetAudio { source: u32 },
    /// Select the subtitle with this source stream index (`null` = subtitles off).
    SetSubtitle { source: Option<u32> },
}

impl CustomLayout {
    pub fn from_json(json: &str) -> Result<Self, LayoutError> {
        serde_json::from_str(json).map_err(|e| LayoutError(vec![format!("Menü-Layout ist kein gültiges JSON: {e}")]))
    }

    pub fn load(path: &Path) -> Result<Self, LayoutError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LayoutError(vec![format!("{}: {e}", path.display())]))?;
        Self::from_json(&text)
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// What the layout may refer to: the project's titles.
#[derive(Debug, Clone)]
pub struct LayoutTitle {
    pub label: String,
    /// Number of chapters of the title.
    pub chapters: usize,
    pub duration_secs: f64,
    /// Source file and default sample start (10 %).
    pub clip: ClipSource,
    /// Source stream indices of the audio tracks on the disc, in DVD stream order.
    pub audio_sources: Vec<u32>,
    /// Source stream indices of the subtitles on the disc, in DVD stream order.
    pub subtitle_sources: Vec<u32>,
}

/// All problems of a layout (German, for the UI).
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutError(pub Vec<String>);

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Menü-Layout: {}", self.0.join("; "))
    }
}

impl std::error::Error for LayoutError {}

/// `#RGB` / `#RRGGBB`.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().strip_prefix('#')?;
    let v = |t: &str| u8::from_str_radix(t, 16).ok();
    match h.len() {
        6 => Some([v(&h[0..2])?, v(&h[2..4])?, v(&h[4..6])?]),
        3 => Some([v(&h[0..1])? * 17, v(&h[1..2])? * 17, v(&h[2..3])? * 17]),
        _ => None,
    }
}

/// Button rectangle of `e` on the TV frame (even y, clipped).
pub(crate) fn button_rect(e: &Element, canvas: Canvas, frame: impl Into<MenuFrame>) -> Option<Rect> {
    let frame = frame.into();
    let (w, h) = (frame.width, frame.height);
    Rect::from_template(
        e.x.round() as i32,
        e.y.round() as i32,
        e.w.round().max(1.0) as u32,
        e.h.round().max(1.0) as u32,
        canvas.width,
        canvas.height,
        w,
        h,
    )
}

/// Button rectangle on a Blu-ray frame: the element's box, rounded and clipped (no field rule).
pub(crate) fn bd_button_rect(e: &Element, canvas: Canvas, frame: MenuFrame) -> Option<Rect> {
    let (x, y, w, h) = frame_box(e, canvas, frame);
    let clamp = |v: i64, max: u32| v.clamp(0, max as i64) as u32;
    let (x0, y0) = (clamp(x, frame.width), clamp(y, frame.height));
    let (x1, y1) = (clamp(x + w as i64, frame.width), clamp(y + h as i64, frame.height));
    (x1 > x0 && y1 > y0).then_some(Rect { x0, y0, x1, y1 })
}

pub(crate) fn overlaps(a: Rect, b: Rect) -> bool {
    a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1
}

pub(crate) fn element_name(e: &Element, n: usize) -> String {
    let kind = match &e.content {
        Content::Text { text, .. } => format!("Text „{}“", text.lines().next().unwrap_or("").chars().take(24).collect::<String>()),
        Content::Image { .. } => "Bild".into(),
        Content::Video { .. } => "Video".into(),
    };
    format!("Element {} ({kind})", n + 1)
}

/// (marked, activated) colours of a button.
pub(crate) fn button_colors(layout: &CustomLayout, e: &Element) -> ([u8; 3], [u8; 3]) {
    let (hl, sel) = match &e.highlight {
        Some(c) => (&c.color, &c.select_color),
        None => (&layout.highlight.color, &layout.highlight.select_color),
    };
    (parse_color(hl).unwrap_or([255, 210, 0]), parse_color(sel).unwrap_or([255, 106, 0]))
}

/// The different colour pairs of a menu's buttons, in order of first use.
fn color_pairs(layout: &CustomLayout, menu: &CustomMenu) -> Vec<([u8; 3], [u8; 3])> {
    let mut pairs = Vec::new();
    for e in menu.elements.iter().filter(|e| e.action.is_some()) {
        let p = button_colors(layout, e);
        if !pairs.contains(&p) {
            pairs.push(p);
        }
    }
    pairs
}

/// The lines a text element shows on `tv` (see [`menu::text_lines`]); `None` for other
/// elements. The menu editor displays exactly these.
pub fn element_text_lines(e: &Element, canvas: Canvas, tv: impl Into<MenuFrame> + Copy) -> Option<Vec<String>> {
    let Content::Text { text, font, size, bold, italic, wrap, .. } = &e.content else { return None };
    let (_, _, w, h) = frame_box(e, canvas, tv);
    let (bw, bh) = if e.rotation % 180 == 90 { (h, w) } else { (w, h) };
    Some(menu::text_lines(text, font, frame_font_size(*size, canvas, tv), *bold, *italic, *wrap, bw, bh))
}

/// Checks the layout against the DVD limits and the project's titles. Navigation command
/// counts are checked by [`plan_navigation`] (also called here).
pub fn validate(layout: &CustomLayout, titles: &[LayoutTitle], tv: TvStandard) -> Result<(), LayoutError> {
    validate_impl(layout, titles, tv.into(), false)
}

/// Blu-ray rules: 16:9 canvas, axis-aligned non-overlapping buttons, at most [`MAX_BD_BUTTONS`]
/// per menu; no colour-pair or VM command limits (the navigation is planned by `bd_menu`).
pub fn validate_bd(layout: &CustomLayout, titles: &[LayoutTitle], frame: MenuFrame) -> Result<(), LayoutError> {
    validate_impl(layout, titles, frame, true)
}

/// Buttons per Blu-ray menu (IG button ids are 16 bit; the ICS button count is limited to 255).
pub const MAX_BD_BUTTONS: usize = 255;

fn validate_impl(layout: &CustomLayout, titles: &[LayoutTitle], frame: MenuFrame, bd: bool) -> Result<(), LayoutError> {
    let medium = if bd { "Blu-ray" } else { "DVD" };
    let mut errs = Vec::new();
    let c = layout.canvas;
    if c.width < 16 || c.height < 16 {
        errs.push("Arbeitsfläche ist zu klein".to_string());
    }
    if bd && (c.width as u64 * 9 != c.height as u64 * 16) {
        errs.push(format!("Arbeitsfläche {}×{} ist nicht 16:9 (Blu-ray-Menüs sind 16:9)", c.width, c.height));
    }
    if layout.menus.is_empty() {
        errs.push("keine Menüs".to_string());
    }
    if !layout.menus.iter().any(|m| m.role == MenuRole::Title) {
        errs.push("mindestens ein Titelmenü ist nötig (es erscheint beim Einlegen der Disc)".to_string());
    }
    for (name, col) in [("Markierungsfarbe", &layout.highlight.color), ("Auswahlfarbe", &layout.highlight.select_color)] {
        if parse_color(col).is_none() {
            errs.push(format!("{name} „{col}“ ist keine Farbe (#RRGGBB)"));
        }
    }
    let mut ids = BTreeSet::new();
    for m in &layout.menus {
        if m.id.trim().is_empty() || !ids.insert(m.id.as_str()) {
            errs.push(format!("Menü „{}“: Id „{}“ fehlt oder ist doppelt", m.name, m.id));
        }
    }
    let title_ok = |t: u32| t >= 1 && (t as usize) <= titles.len();
    for m in &layout.menus {
        let menu = |msg: String| format!("Menü „{}“: {msg}", m.name);
        match (m.role, m.title) {
            (MenuRole::Root | MenuRole::Settings, Some(t)) if title_ok(t) => {}
            (MenuRole::Root, t) => errs.push(menu(format!("Root-Menü braucht einen gültigen Titel (ist {t:?})"))),
            (MenuRole::Settings, t) => errs.push(menu(format!("Einstellungsmenü braucht einen gültigen Titel (ist {t:?})"))),
            (MenuRole::Title, _) => {}
        }
        if parse_color(&m.background.color).is_none() {
            errs.push(menu(format!("Hintergrundfarbe „{}“ ist keine Farbe", m.background.color)));
        }
        if !(0.0..=50.0).contains(&m.background.blur) {
            errs.push(menu("Weichzeichner muss zwischen 0 und 50 liegen".into()));
        }
        if let Some(img) = &m.background.image {
            if !Path::new(img).is_file() {
                errs.push(menu(format!("Hintergrundbild nicht gefunden: {img}")));
            }
        }
        if let Some(v) = &m.background.video {
            if !title_ok(v.title) {
                errs.push(menu(format!("Hintergrundvideo: Titel {} existiert nicht", v.title)));
            }
        }
        match &m.audio {
            MenuSound::Title { title, .. } if !title_ok(*title) => errs.push(menu(format!("Ton: Titel {title} existiert nicht"))),
            MenuSound::File { path } if !Path::new(path).is_file() => errs.push(menu(format!("Tondatei nicht gefunden: {path}"))),
            _ => {}
        }
        if let Some(d) = m.duration_secs {
            if !(1.0..=600.0).contains(&d) {
                errs.push(menu("Dauer muss zwischen 1 und 600 Sekunden liegen".into()));
            }
        }
        let mut rects: Vec<(String, Rect)> = Vec::new();
        for (n, e) in m.elements.iter().enumerate() {
            let el = |msg: &str| menu(format!("{}: {msg}", element_name(e, n)));
            if e.w < 1.0 || e.h < 1.0 {
                errs.push(el("Breite und Höhe müssen mindestens 1 sein"));
            }
            if e.x < -0.5 || e.y < -0.5 || e.x + e.w > c.width as f64 + 0.5 || e.y + e.h > c.height as f64 + 0.5 {
                errs.push(el("liegt (teilweise) außerhalb der Arbeitsfläche"));
            }
            if e.rotation % 90 != 0 || e.rotation >= 360 {
                errs.push(el("Drehung nur in 90°-Schritten (0, 90, 180, 270)"));
            }
            if !(0.0..=1.0).contains(&e.opacity) {
                errs.push(el("Deckkraft muss zwischen 0 und 1 liegen"));
            }
            match &e.content {
                Content::Text { color, size, .. } => {
                    if parse_color(color).is_none() {
                        errs.push(el(&format!("Textfarbe „{color}“ ist keine Farbe")));
                    }
                    if !(4.0..=300.0).contains(size) {
                        errs.push(el("Schriftgröße muss zwischen 4 und 300 liegen"));
                    }
                }
                Content::Image { src, .. } => {
                    if !Path::new(src).is_file() {
                        errs.push(el(&format!("Bild nicht gefunden: {src}")));
                    }
                }
                Content::Video { title, .. } => {
                    if !title_ok(*title) {
                        errs.push(el(&format!("Titel {title} existiert nicht")));
                    }
                }
            }
            if let Some(colors) = &e.highlight {
                for (name, col) in [("Markierungsfarbe", &colors.color), ("Auswahlfarbe", &colors.select_color)] {
                    if parse_color(col).is_none() {
                        errs.push(el(&format!("{name} „{col}“ ist keine Farbe (#RRGGBB)")));
                    }
                }
            }
            let Some(action) = &e.action else { continue };
            if e.rotation != 0 {
                errs.push(el(&format!("Buttons können nicht gedreht werden ({medium}-Hitboxen sind achsenparallele Rechtecke)")));
            }
            match action {
                Action::PlayTitle { title } if !title_ok(*title) => errs.push(el(&format!("spielt Titel {title}, den es nicht gibt"))),
                Action::PlayChapter { title, chapter } => {
                    if !title_ok(*title) {
                        errs.push(el(&format!("spielt Titel {title}, den es nicht gibt")));
                    } else if *chapter < 1
                        || *chapter as usize > titles[*title as usize - 1].chapters
                        || *chapter as usize > MAX_CHAPTERS
                    {
                        errs.push(el(&format!(
                            "Kapitel {chapter} von Titel {title} existiert nicht ({} Kapitel)",
                            titles[*title as usize - 1].chapters
                        )));
                    }
                }
                Action::Menu { menu: target } if !layout.menus.iter().any(|x| &x.id == target) => {
                    errs.push(el(&format!("verweist auf ein unbekanntes Menü „{target}“")))
                }
                Action::SetAudio { .. } | Action::SetSubtitle { .. } => {
                    // Streams belong to a titleset: only its own root/settings menus can set them.
                    match m.title.filter(|&t| m.role != MenuRole::Title && title_ok(t)) {
                        None => errs.push(el("Ton-/Untertitelwahl geht nur in Root- und Einstellungsmenüs eines Titels")),
                        Some(t) => {
                            let lt = &titles[t as usize - 1];
                            match action {
                                Action::SetAudio { source } if !lt.audio_sources.contains(source) => errs.push(el(&format!(
                                    "Tonspur (Stream #{source}) ist bei Titel {t} nicht ausgewählt"
                                ))),
                                Action::SetSubtitle { source: Some(source) } if !lt.subtitle_sources.contains(source) => {
                                    errs.push(el(&format!("Untertitel (Stream #{source}) ist bei Titel {t} nicht ausgewählt")))
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
            if let Some(r) = if bd { bd_button_rect(e, c, frame) } else { button_rect(e, c, frame) } {
                for (other, o) in &rects {
                    if overlaps(r, *o) {
                        errs.push(el(&format!("überlappt mit {other} ({medium}-Buttons dürfen sich nicht überdecken)")));
                    }
                }
                rects.push((element_name(e, n), r));
            }
        }
        let max_buttons = if bd { MAX_BD_BUTTONS } else { MAX_BUTTONS };
        if rects.len() > max_buttons {
            errs.push(menu(format!("{} Buttons, eine {medium}-Menüseite erlaubt höchstens {max_buttons}", rects.len())));
        }
        let pairs = color_pairs(layout, m);
        if bd {
            // Real alpha in the IG palette: any number of colours, but one 256-entry palette.
        } else if pairs.len() > MAX_COLOR_PAIRS {
            errs.push(menu(format!(
                "{} verschiedene Button-Farben – eine DVD-Menüseite kann höchstens {MAX_COLOR_PAIRS} Farbpaare (markiert/aktiviert) zeigen",
                pairs.len()
            )));
        }
        // The pixel value of a pair is found from either colour, so colours cannot be shared.
        let shared = !bd && pairs.iter().enumerate().any(|(i, a)| pairs[i + 1..].iter().any(|b| a.0 == b.0 || a.1 == b.1));
        if shared {
            errs.push(menu(
                "Buttons mit gleicher Markierungsfarbe brauchen auch die gleiche Aktivierungsfarbe (und umgekehrt)".into(),
            ));
        }
    }
    if errs.is_empty() && !bd {
        if let Err(e) = plan_navigation(layout, titles) {
            errs.extend(e.0);
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(LayoutError(errs))
    }
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

/// Where a menu lives on the disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// VMGM menu PGC number.
    Vmgm(u32),
    /// Root menu of `title`: PGC number in its VTSM and the 1-based index among its root menus.
    Vtsm { title: u32, pgc: u32, index: u32 },
}

/// The navigation of a whole layout: where every menu goes and every command.
#[derive(Debug, Clone, PartialEq)]
pub struct NavigationPlan {
    /// Per menu (layout order).
    pub placement: Vec<Placement>,
    /// Per menu, per element: the button commands (`None` for plain elements).
    pub commands: Vec<Vec<Option<String>>>,
    /// Per title: the dispatcher PGC (VTSM menu 1) commands, if the titleset needs one.
    pub dispatchers: Vec<Option<String>>,
    /// VMGM router PGC number and commands, if any VTSM button leaves its titleset.
    pub router: Option<(u32, String)>,
    /// Per title: the VMGM menu its playback returns to.
    pub main_menu_pages: Vec<u32>,
    /// Number of visible VMGM menus.
    pub title_menus: u32,
}

const ROUTER_PLAY: u32 = 1000;
const ROUTER_ENTER: u32 = 2000;
const DISPATCH_MENU: u32 = 100;

/// Computes placements, button commands, dispatchers and the router for `titles` titles.
pub fn plan_navigation(layout: &CustomLayout, title_info: &[LayoutTitle]) -> Result<NavigationPlan, LayoutError> {
    let titles = title_info.len();
    // 1. Visible menus: title menus in the VMGM; root menus, then settings menus, in their
    //    title's VTSM (`roots` holds both, in that order).
    let title_menus: Vec<usize> = (0..layout.menus.len()).filter(|&i| layout.menus[i].role == MenuRole::Title).collect();
    let mut roots: Vec<Vec<usize>> = vec![Vec::new(); titles];
    for role in [MenuRole::Root, MenuRole::Settings] {
        for (i, m) in layout.menus.iter().enumerate() {
            if let (true, Some(t)) = (m.role == role, m.title) {
                if let Some(list) = roots.get_mut(t as usize - 1) {
                    list.push(i);
                }
            }
        }
    }
    let by_id: HashMap<&str, usize> = layout.menus.iter().enumerate().map(|(i, m)| (m.id.as_str(), i)).collect();
    let location = |i: usize| -> Option<u32> {
        // None = VMGM, Some(t) = VTSM of title t.
        match layout.menus[i].role {
            MenuRole::Title => None,
            MenuRole::Root | MenuRole::Settings => layout.menus[i].title,
        }
    };
    let root_index = |i: usize| -> u32 {
        let t = layout.menus[i].title.unwrap_or(1) as usize - 1;
        roots[t].iter().position(|&x| x == i).map_or(1, |p| p as u32 + 1)
    };

    // 2. Which titlesets need a dispatcher (entered from outside for a chapter or a non-first
    //    root menu), and whether VTSM buttons need the VMGM router.
    let mut codes: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); titles];
    let mut router_codes: BTreeSet<u32> = BTreeSet::new();
    let actions = || {
        layout
            .menus
            .iter()
            .enumerate()
            .flat_map(|(mi, m)| m.elements.iter().filter_map(move |e| e.action.as_ref().map(|a| (mi, a))))
    };
    for (mi, action) in actions() {
        let from = location(mi);
        match action {
            Action::PlayChapter { title, chapter } if from != Some(*title) => {
                codes[*title as usize - 1].insert(*chapter);
                if from.is_some() {
                    router_codes.insert(ROUTER_ENTER + title);
                }
            }
            Action::PlayTitle { title } if from.is_some() && from != Some(*title) => {
                router_codes.insert(ROUTER_PLAY + title);
            }
            Action::PlayAll if from.is_some() => {
                router_codes.insert(ROUTER_PLAY + 1);
            }
            Action::Menu { menu } => {
                let Some(&target) = by_id.get(menu.as_str()) else { continue };
                if let Some(t) = location(target) {
                    if from != Some(t) {
                        let j = root_index(target);
                        if j > 1 {
                            codes[t as usize - 1].insert(DISPATCH_MENU + j);
                        }
                        if from.is_some() {
                            router_codes.insert(ROUTER_ENTER + t);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 3. Placement: VTSM menu numbers start after the dispatcher.
    let has_dispatcher: Vec<bool> = codes.iter().map(|c| !c.is_empty()).collect();
    let mut placement = vec![Placement::Vmgm(0); layout.menus.len()];
    for (n, &i) in title_menus.iter().enumerate() {
        placement[i] = Placement::Vmgm(n as u32 + 1);
    }
    for (t, list) in roots.iter().enumerate() {
        for (j, &i) in list.iter().enumerate() {
            let pgc = j as u32 + 1 + has_dispatcher[t] as u32;
            placement[i] = Placement::Vtsm { title: t as u32 + 1, pgc, index: j as u32 + 1 };
        }
    }
    let router_no = (!router_codes.is_empty()).then_some(title_menus.len() as u32 + 1);

    // 4. Where each title's playback returns to: the first title menu that plays it.
    let main_menu_pages: Vec<u32> = (1..=titles as u32)
        .map(|t| {
            title_menus
                .iter()
                .enumerate()
                .find(|(_, &i)| {
                    layout.menus[i].elements.iter().any(|e| {
                        matches!(e.action, Some(Action::PlayTitle { title }) | Some(Action::PlayChapter { title, .. }) if title == t)
                    })
                })
                .map_or(1, |(n, _)| n as u32 + 1)
        })
        .collect();

    // 5. Button commands.
    let enter_titleset = |t: u32, g2: u32, from: Option<u32>| -> String {
        match (from, router_no) {
            (None, _) => format!("g2 = {g2}; jump titleset {t} menu;"),
            (Some(_), Some(r)) => format!("g2 = {g2}; g3 = {}; jump vmgm menu {r};", ROUTER_ENTER + t),
            (Some(_), None) => unreachable!("router exists for cross-titleset jumps"),
        }
    };
    let mut commands = Vec::new();
    for (mi, m) in layout.menus.iter().enumerate() {
        let from = location(mi);
        let mut per = Vec::new();
        for e in &m.elements {
            let Some(action) = &e.action else {
                per.push(None);
                continue;
            };
            let cmd = match action {
                Action::PlayAll => match (from, router_no) {
                    (None, _) => "g1 = 1; jump title 1;".to_string(),
                    (Some(_), Some(r)) => format!("g1 = 1; g3 = {}; jump vmgm menu {r};", ROUTER_PLAY + 1),
                    (Some(_), None) => unreachable!(),
                },
                Action::PlayTitle { title } => match (from, router_no) {
                    (None, _) => format!("g1 = 0; jump title {title};"),
                    (Some(t), _) if t == *title => "g1 = 0; jump title 1;".to_string(),
                    (Some(_), Some(r)) => format!("g1 = 0; g3 = {}; jump vmgm menu {r};", ROUTER_PLAY + title),
                    (Some(_), None) => unreachable!(),
                },
                Action::PlayChapter { title, chapter } if from == Some(*title) => {
                    format!("g1 = 0; jump title 1 chapter {chapter};")
                }
                Action::PlayChapter { title, chapter } => format!("g1 = 0; {}", enter_titleset(*title, *chapter, from)),
                Action::SetAudio { source } | Action::SetSubtitle { source: Some(source) } => {
                    let Placement::Vtsm { title, pgc, .. } = placement[mi] else { unreachable!("validated: VTSM menus only") };
                    let lt = &title_info[title as usize - 1];
                    if matches!(action, Action::SetAudio { .. }) {
                        let n = lt.audio_sources.iter().position(|s| s == source).unwrap_or(0);
                        format!("audio = {n}; jump menu {pgc};")
                    } else {
                        // Bit 6 switches the subpicture display on.
                        let n = lt.subtitle_sources.iter().position(|s| s == source).unwrap_or(0);
                        format!("subtitle = {}; jump menu {pgc};", n + 64)
                    }
                }
                Action::SetSubtitle { source: None } => {
                    let Placement::Vtsm { pgc, .. } = placement[mi] else { unreachable!("validated: VTSM menus only") };
                    format!("subtitle = 0; jump menu {pgc};")
                }
                Action::Menu { menu } => {
                    let target = by_id[menu.as_str()];
                    match (placement[target], from) {
                        (Placement::Vmgm(n), None) => format!("jump menu {n};"),
                        (Placement::Vmgm(n), Some(_)) => format!("jump vmgm menu {n};"),
                        (Placement::Vtsm { title, pgc, .. }, Some(t)) if t == title => format!("jump menu {pgc};"),
                        (Placement::Vtsm { title, index, .. }, _) => {
                            let g2 = if index > 1 { DISPATCH_MENU + index } else { 0 };
                            enter_titleset(title, g2, from)
                        }
                    }
                }
            };
            per.push(Some(cmd));
        }
        commands.push(per);
    }

    // 6. Dispatchers and router.
    let dispatchers: Vec<Option<String>> = (0..titles)
        .map(|t| {
            if codes[t].is_empty() {
                return None;
            }
            let mut c = String::new();
            for &code in &codes[t] {
                if code < DISPATCH_MENU {
                    c.push_str(&format!("if (g2 eq {code}) jump title 1 chapter {code}; "));
                } else {
                    let index = code - DISPATCH_MENU;
                    let pgc = 1 + index; // after the dispatcher
                    c.push_str(&format!("if (g2 eq {code}) jump menu {pgc}; "));
                }
            }
            // Nothing (or the "Menu" key during playback): the first root menu, else back to
            // the title menu that plays this title.
            if roots[t].is_empty() {
                c.push_str(&format!("jump vmgm menu {};", main_menu_pages[t]));
            } else {
                c.push_str("jump menu 2;");
            }
            Some(c)
        })
        .collect();
    let router = router_no.map(|r| {
        let mut c = String::new();
        for &code in &router_codes {
            if code >= ROUTER_ENTER {
                c.push_str(&format!("if (g3 eq {code}) jump titleset {} menu; ", code - ROUTER_ENTER));
            } else {
                c.push_str(&format!("if (g3 eq {code}) jump title {}; ", code - ROUTER_PLAY));
            }
        }
        c.push_str("jump menu 1;");
        (r, c)
    });

    // 7. VM limits.
    let mut errs = Vec::new();
    for (t, d) in dispatchers.iter().enumerate() {
        if let Some(d) = d {
            if command_count(d) > MAX_PGC_COMMANDS {
                errs.push(format!(
                    "Titel {}: {} Sprungziele von außen (Kapitel/Menüs) – mehr als {MAX_PGC_COMMANDS} DVD-Befehle",
                    t + 1,
                    command_count(d)
                ));
            }
        }
    }
    if let Some((_, r)) = &router {
        if command_count(r) > MAX_PGC_COMMANDS {
            errs.push(format!("zu viele titelübergreifende Sprünge aus Root-Menüs ({} DVD-Befehle, max. {MAX_PGC_COMMANDS})", command_count(r)));
        }
    }
    if !errs.is_empty() {
        return Err(LayoutError(errs));
    }
    Ok(NavigationPlan {
        placement,
        commands,
        dispatchers,
        router,
        main_menu_pages,
        title_menus: title_menus.len() as u32,
    })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Everything the build needs from a custom layout.
#[derive(Debug, Clone)]
pub struct CustomMenus {
    /// VMGM menu PGCs in order (visible title menus, then the router if any).
    pub vmgm_menus: Vec<MenuSpec>,
    /// Per title: its VTSM menu PGCs (dispatcher first if any, then its root menus).
    pub titleset_menus: Vec<Vec<MenuSpec>>,
    pub main_menu_pages: Vec<u32>,
    /// Visible pages to encode (spec + files), title menus first.
    pub pages: Vec<(MenuSpec, MenuPageFiles)>,
    /// `pre` commands for every title PGC.
    pub title_pre: String,
    pub warnings: Vec<String>,
}

/// Fits `img` into `w`x`h` frame pixels; `Contain` leaves transparent bars. Contain/cover keep
/// the picture's proportions on the TV: the axis that ends up horizontal (the height when
/// `sideways`, i.e. rotated by 90°/270° afterwards) is first scaled by 1 / `pixel_aspect`.
pub(crate) fn fit_image(img: &RgbaImage, w: u32, h: u32, fit: Fit, pixel_aspect: f64, sideways: bool) -> RgbaImage {
    let squeezed;
    let img = if fit == Fit::Stretch || (pixel_aspect - 1.0).abs() < 1e-6 {
        img
    } else {
        let (iw, ih) = img.dimensions();
        let f = 1.0 / pixel_aspect;
        let (nw, nh) = if sideways { (iw, ((ih as f64 * f).round() as u32).max(1)) } else { (((iw as f64 * f).round() as u32).max(1), ih) };
        squeezed = imageops::resize(img, nw, nh, FilterType::Lanczos3);
        &squeezed
    };
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 || w == 0 || h == 0 {
        return RgbaImage::new(w.max(1), h.max(1));
    }
    match fit {
        Fit::Stretch => imageops::resize(img, w, h, FilterType::Lanczos3),
        Fit::Contain => {
            let f = (w as f64 / iw as f64).min(h as f64 / ih as f64);
            let (nw, nh) = (((iw as f64 * f).round() as u32).clamp(1, w), ((ih as f64 * f).round() as u32).clamp(1, h));
            let scaled = imageops::resize(img, nw, nh, FilterType::Lanczos3);
            let mut out = RgbaImage::new(w, h);
            imageops::overlay(&mut out, &scaled, ((w - nw) / 2) as i64, ((h - nh) / 2) as i64);
            out
        }
        Fit::Cover => {
            let f = (w as f64 / iw as f64).max(h as f64 / ih as f64);
            let (nw, nh) = (((iw as f64 * f).ceil() as u32).max(w), ((ih as f64 * f).ceil() as u32).max(h));
            let scaled = imageops::resize(img, nw, nh, FilterType::Lanczos3);
            imageops::crop_imm(&scaled, (nw - w) / 2, (nh - h) / 2, w, h).to_image()
        }
    }
}

pub(crate) fn rotate(img: RgbaImage, rotation: u32) -> RgbaImage {
    match rotation % 360 {
        90 => imageops::rotate90(&img),
        180 => imageops::rotate180(&img),
        270 => imageops::rotate270(&img),
        _ => img,
    }
}

/// An element's box on the TV frame: `(x, y, w, h)`.
pub(crate) fn frame_box(e: &Element, canvas: Canvas, frame: impl Into<MenuFrame>) -> (i64, i64, u32, u32) {
    let frame = frame.into();
    let (fw, fh) = (frame.width, frame.height);
    let (sx, sy) = (fw as f64 / canvas.width as f64, fh as f64 / canvas.height as f64);
    let x = (e.x * sx).round() as i64;
    let y = (e.y * sy).round() as i64;
    let w = ((e.w * sx).round() as u32).max(1);
    let h = ((e.h * sy).round() as u32).max(1);
    (x, y, w, h)
}

/// Font size on the frame (texts scale with the canvas height).
pub(crate) fn frame_font_size(size: f32, canvas: Canvas, frame: impl Into<MenuFrame>) -> f32 {
    size * frame.into().height as f32 / canvas.height as f32
}

pub(crate) fn clip_of(titles: &[LayoutTitle], title: u32, start: Option<f64>) -> ClipSource {
    let t = &titles[title as usize - 1];
    match start {
        Some(s) => ClipSource { start_secs: s.clamp(0.0, (t.duration_secs - 1.0).max(0.0)), ..t.clip.clone() },
        None => t.clip.clone(),
    }
}

struct PageOutput {
    files: MenuPageFiles,
    buttons: Vec<MenuButton>,
    motion: bool,
    warnings: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn render_custom_page(
    layout: &CustomLayout,
    menu: &CustomMenu,
    commands: &[Option<String>],
    titles: &[LayoutTitle],
    tv: TvStandard,
    out_dir: &Path,
    prefix: &str,
    default_secs: f64,
) -> Result<PageOutput, LayoutError> {
    let io = |p: &Path, e: &dyn fmt::Display| LayoutError(vec![format!("{}: {e}", p.display())]);
    let (fw, fh) = tv.size();
    let canvas = layout.canvas;
    let mut warnings = Vec::new();
    let mut layers: Vec<MenuLayer> = Vec::new();
    let mut current: Option<RgbaImage> = None;
    let mut layer_no = 0;
    let mut flush = |current: &mut Option<RgbaImage>, layers: &mut Vec<MenuLayer>| -> Result<(), LayoutError> {
        if let Some(img) = current.take() {
            let path = out_dir.join(format!("{prefix}_l{layer_no}.png"));
            layer_no += 1;
            img.save(&path).map_err(|e| io(&path, &e))?;
            layers.push(MenuLayer::Image(path));
        }
        Ok(())
    };

    // Button colours: with one pair, pixel value 1 is the colour and 2 its half-transparent
    // edge (smooth text outlines); with several pairs every pair gets its own value 1..=3.
    let alpha = (layout.highlight.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    let pairs = color_pairs(layout, menu);
    let (highlight_palette, select_palette) = match pairs.as_slice() {
        [] | [_] => {
            let (hl, sel) = pairs.first().copied().unwrap_or_else(|| {
                (parse_color(&layout.highlight.color).unwrap_or([255, 210, 0]), parse_color(&layout.highlight.select_color).unwrap_or([255, 106, 0]))
            });
            let palette = |c: [u8; 3]| [[0, 0, 0, 0], [c[0], c[1], c[2], alpha], [c[0], c[1], c[2], alpha / 2], [0, 0, 0, 0]];
            (palette(hl), palette(sel))
        }
        _ => {
            let mut hl = [[0u8; 4]; 4];
            let mut sel = [[0u8; 4]; 4];
            for (i, (h, s)) in pairs.iter().take(MAX_COLOR_PAIRS).enumerate() {
                hl[i + 1] = [h[0], h[1], h[2], alpha];
                sel[i + 1] = [s[0], s[1], s[2], alpha];
            }
            (hl, sel)
        }
    };
    let smooth_edges = pairs.len() <= 1;
    let mut highlight = IndexedImage::new(fw, fh, highlight_palette);
    let mut select = IndexedImage::new(fw, fh, select_palette);
    let mut buttons: Vec<MenuButton> = Vec::new();

    for (n, e) in menu.elements.iter().enumerate() {
        let (x, y, w, h) = frame_box(e, canvas, tv);
        // Unrotated content size.
        let (bw, bh) = if e.rotation % 180 == 90 { (h, w) } else { (w, h) };
        let opacity = (e.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        // Glyph coverage of a text button, for its highlight shape.
        let mut text_cov = None;
        match &e.content {
            Content::Text { text, font, size, color, align, bold, italic, shadow, wrap } => {
                let cov = menu::text_coverage(text, font, frame_font_size(*size, canvas, tv), *bold, *italic, *wrap, align, bw, bh);
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
                let src_img = image::open(src).map_err(|err| io(Path::new(src), &err))?.to_rgba8();
                let img = rotate(fit_image(&src_img, bw, bh, *fit, tv.menu_pixel_aspect(), e.rotation % 180 == 90), e.rotation);
                menu::blit(current.get_or_insert_with(|| RgbaImage::new(fw, fh)), &img, x, y, opacity);
            }
            Content::Video { title, start_secs, fit } => {
                flush(&mut current, &mut layers)?;
                layers.push(MenuLayer::Video {
                    clip: clip_of(titles, *title, *start_secs),
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

        let Some(cmd) = commands.get(n).cloned().flatten() else { continue };
        let Some(rect) = button_rect(e, canvas, tv) else {
            warnings.push(format!("{}: Button liegt außerhalb des Bildes und wurde ausgelassen", element_name(e, n)));
            continue;
        };
        // Pixel value of this button's colour pair.
        let value = pairs.iter().position(|p| *p == button_colors(layout, e)).unwrap_or(0).min(MAX_COLOR_PAIRS - 1) as u8 + 1;
        // Highlight shape: the glyphs of a text button, a frame around everything else.
        match &text_cov {
            Some(cov) => {
                for py in 0..cov.height {
                    for px in 0..cov.width {
                        let v = cov.at(px as i64, py as i64);
                        let idx = if smooth_edges {
                            if v >= 0.5 { 1 } else if v >= 0.15 { 2 } else { 0 }
                        } else if v >= 0.35 {
                            value
                        } else {
                            0
                        };
                        let (tx, ty) = (x + px as i64, y + py as i64);
                        if idx > 0 && tx >= 0 && ty >= 0 && (tx as u32) < fw && (ty as u32) < fh {
                            let i = (ty as u32 * fw + tx as u32) as usize;
                            highlight.pixels[i] = idx;
                            select.pixels[i] = idx;
                        }
                    }
                }
            }
            None => {
                let t = ((rect.x1 - rect.x0).min(rect.y1 - rect.y0) / 20).clamp(2, 6);
                for py in rect.y0..rect.y1 {
                    for px in rect.x0..rect.x1 {
                        let edge = px < rect.x0 + t || px >= rect.x1 - t || py < rect.y0 + t || py >= rect.y1 - t;
                        if edge {
                            let i = (py * fw + px) as usize;
                            highlight.pixels[i] = value;
                            select.pixels[i] = value;
                        }
                    }
                }
            }
        }
        buttons.push(MenuButton {
            name: format!("b{}", buttons.len() + 1),
            template_index: n,
            location: "CUSTOM".into(),
            rect,
            action: ButtonAction::Commands(cmd),
            label: None,
            thumbnail: None,
            clip: None,
            nav: Navigation::default(),
        });
    }
    flush(&mut current, &mut layers)?;
    author::compute_navigation(&mut buttons);

    let background = match (&menu.background.video, &menu.background.image) {
        (Some(v), _) => MenuBackground::Video { clip: clip_of(titles, v.title, v.start_secs), blur: menu.background.blur },
        (None, Some(img)) => MenuBackground::Image { path: PathBuf::from(img), blur: menu.background.blur },
        (None, None) => MenuBackground::Color(parse_color(&menu.background.color).unwrap_or([0, 0, 0])),
    };
    let audio = match &menu.audio {
        MenuSound::None => None,
        MenuSound::File { path } => Some(ClipSource { path: path.into(), start_secs: 0.0, has_audio: true }),
        MenuSound::Title { title, start_secs } => {
            let clip = clip_of(titles, *title, *start_secs);
            if clip.has_audio {
                Some(clip)
            } else {
                warnings.push(format!("Titel {title} hat keinen Ton – Menü bleibt stumm"));
                None
            }
        }
    };
    let moving = matches!(background, MenuBackground::Video { .. })
        || layers.iter().any(|l| matches!(l, MenuLayer::Video { .. }))
        || audio.is_some();
    let secs = menu.duration_secs.unwrap_or(default_secs).clamp(1.0, 600.0);

    let hl_name = format!("{prefix}_hl.png");
    let sel_name = format!("{prefix}_sel.png");
    let xml_path = out_dir.join(format!("{prefix}_spumux.xml"));
    highlight.write_png(&out_dir.join(&hl_name)).map_err(|e| io(&out_dir.join(&hl_name), &e))?;
    select.write_png(&out_dir.join(&sel_name)).map_err(|e| io(&out_dir.join(&sel_name), &e))?;
    std::fs::write(&xml_path, author::spumux_xml(tv, &hl_name, &sel_name, &buttons)).map_err(|e| io(&xml_path, &e))?;
    let files = MenuPageFiles {
        background: out_dir.join(&hl_name),
        highlight: out_dir.join(&hl_name),
        select: out_dir.join(&sel_name),
        spumux_xml: xml_path,
        vob: format!("{prefix}.mpg"),
        foreground: None,
        video_slots: vec![],
        motion_secs: moving.then_some(secs),
        audio,
        custom: Some(CustomMenuMedia { background, overlay: menu.background.overlay, layers }),
    };
    Ok(PageOutput { files, buttons, motion: moving, warnings })
}

/// Validates the layout, plans its navigation and renders every menu into `out_dir`
/// (`cm<NN>_*` files). `default_secs` is the loop length of moving menus without their own.
pub fn build_custom_menus(
    layout: &CustomLayout,
    titles: &[LayoutTitle],
    tv: TvStandard,
    out_dir: &Path,
    default_secs: f64,
) -> Result<CustomMenus, LayoutError> {
    validate(layout, titles, tv)?;
    let nav = plan_navigation(layout, titles)?;
    let rendered: Vec<PageOutput> = layout
        .menus
        .par_iter()
        .enumerate()
        .map(|(i, m)| render_custom_page(layout, m, &nav.commands[i], titles, tv, out_dir, &format!("cm{:02}", i + 1), default_secs))
        .collect::<Result<_, _>>()?;

    let mut vmgm_menus = Vec::new();
    let mut titleset_menus: Vec<Vec<MenuSpec>> = vec![Vec::new(); titles.len()];
    for (t, d) in nav.dispatchers.iter().enumerate() {
        if let Some(d) = d {
            titleset_menus[t].push(MenuSpec::commands(d.clone(), Some("root")));
        }
    }
    let mut pages = Vec::new();
    let mut warnings = Vec::new();
    // Title menus first (VMGM order), then root menus per title (layout order within a title).
    let mut order: Vec<usize> = (0..layout.menus.len()).collect();
    order.sort_by_key(|&i| match nav.placement[i] {
        Placement::Vmgm(n) => (0, 0, n),
        Placement::Vtsm { title, pgc, .. } => (1, title, pgc),
    });
    for i in order {
        let PageOutput { files, buttons, motion, warnings: w } = rendered[i].clone_parts();
        warnings.extend(w.into_iter().map(|w| format!("Menü „{}“: {w}", layout.menus[i].name)));
        match nav.placement[i] {
            Placement::Vmgm(n) => {
                let spec = MenuSpec {
                    vob: files.vob.clone(),
                    buttons,
                    entry: (n == 1).then(|| "title".to_string()),
                    motion,
                    pre: None,
                };
                vmgm_menus.push(spec.clone());
                pages.push((spec, files));
            }
            Placement::Vtsm { title, .. } => {
                let list = &mut titleset_menus[title as usize - 1];
                let spec = MenuSpec {
                    vob: files.vob.clone(),
                    buttons,
                    entry: list.is_empty().then(|| "root".to_string()),
                    motion,
                    pre: Some(RESET_DISPATCH.to_string()),
                };
                list.push(spec.clone());
                pages.push((spec, files));
            }
        }
    }
    if let Some((_, r)) = &nav.router {
        vmgm_menus.push(MenuSpec::commands(r.clone(), None));
    }
    Ok(CustomMenus {
        vmgm_menus,
        titleset_menus,
        main_menu_pages: nav.main_menu_pages,
        pages,
        title_pre: RESET_DISPATCH.to_string(),
        warnings,
    })
}

impl PageOutput {
    fn clone_parts(&self) -> PageOutput {
        PageOutput { files: self.files.clone(), buttons: self.buttons.clone(), motion: self.motion, warnings: self.warnings.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(n: usize) -> Vec<LayoutTitle> {
        (1..=n)
            .map(|i| LayoutTitle {
                label: format!("Film {i}"),
                chapters: 4,
                duration_secs: 600.0,
                clip: ClipSource { path: format!("film{i}.mkv").into(), start_secs: 60.0, has_audio: true },
                audio_sources: vec![1, 2],
                subtitle_sources: vec![3],
            })
            .collect()
    }

    fn text(x: f64, y: f64, label: &str, action: Option<Action>) -> Element {
        Element {
            id: label.into(),
            content: Content::Text {
                text: label.into(),
                font: "Arial".into(),
                size: 32.0,
                color: "#FFFFFF".into(),
                align: "center".into(),
                bold: false,
                italic: false,
                shadow: false,
                wrap: false,
            },
            x,
            y,
            w: 300.0,
            h: 50.0,
            rotation: 0,
            opacity: 1.0,
            action,
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
            background: Background::default(),
            duration_secs: None,
            audio: MenuSound::None,
            elements,
        }
    }

    fn layout(menus: Vec<CustomMenu>) -> CustomLayout {
        CustomLayout { version: 1, canvas: Canvas::default(), highlight: Highlight::default(), menus }
    }

    #[test]
    fn json_round_trip() {
        let json = r##"{
          "version": 1,
          "canvas": {"width": 720, "height": 576},
          "menus": [
            {"id": "main", "name": "Hauptmenü", "role": "title",
             "background": {"color": "#102030", "blur": 6},
             "audio": {"type": "title", "title": 1, "startSecs": 5},
             "elements": [
               {"id": "t1", "type": "text", "text": "Film abspielen", "x": 60, "y": 400, "w": 300, "h": 50,
                "size": 30, "bold": true, "action": {"type": "playTitle", "title": 1}},
               {"id": "v1", "type": "video", "title": 1, "startSecs": 30, "x": 400, "y": 100, "w": 240, "h": 136,
                "action": {"type": "menu", "menu": "kap"}},
               {"id": "all", "type": "text", "text": "Alle", "x": 60, "y": 460, "w": 200, "h": 40,
                "action": {"type": "playAll"}}
             ]},
            {"id": "kap", "name": "Kapitel", "role": "root", "title": 1,
             "elements": [{"type": "text", "text": "Kapitel 2", "x": 60, "y": 100, "w": 300, "h": 50,
                           "action": {"type": "playChapter", "title": 1, "chapter": 2}}]}
          ]
        }"##;
        let l = CustomLayout::from_json(json).unwrap();
        assert_eq!(l.menus.len(), 2);
        assert_eq!(l.menus[0].audio, MenuSound::Title { title: 1, start_secs: Some(5.0) });
        assert!(matches!(&l.menus[0].elements[0].content, Content::Text { bold: true, size, .. } if *size == 30.0));
        assert!(matches!(&l.menus[0].elements[1].content, Content::Video { title: 1, start_secs: Some(s), fit: Fit::Cover } if *s == 30.0));
        assert_eq!(l.menus[1].elements[0].action, Some(Action::PlayChapter { title: 1, chapter: 2 }));
        let again = CustomLayout::from_json(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(again, l);
        validate(&l, &titles(1), TvStandard::Pal).unwrap();
        assert!(CustomLayout::from_json("{\"menus\": 3}").is_err());
    }

    #[test]
    fn validation_catches_dvd_limits() {
        let mut rotated = text(10.0, 10.0, "A", Some(Action::PlayTitle { title: 1 }));
        rotated.rotation = 90;
        let mut odd = text(10.0, 300.0, "B", None);
        odd.rotation = 45;
        let l = layout(vec![
            menu("m", MenuRole::Title, None, vec![
                rotated,
                odd,
                text(10.0, 100.0, "C", Some(Action::PlayChapter { title: 1, chapter: 9 })),
                text(10.0, 120.0, "D", Some(Action::Menu { menu: "nope".into() })),
                text(500.0, 500.0, "E", Some(Action::PlayTitle { title: 3 })),
            ]),
            menu("r", MenuRole::Root, None, vec![]),
        ]);
        let err = validate(&l, &titles(1), TvStandard::Pal).unwrap_err().0.join("\n");
        assert!(err.contains("nicht gedreht"), "{err}");
        assert!(err.contains("90°-Schritten"), "{err}");
        assert!(err.contains("Kapitel 9"), "{err}");
        assert!(err.contains("unbekanntes Menü"), "{err}");
        assert!(err.contains("Titel 3"), "{err}");
        assert!(err.contains("überlappt"), "C and D overlap: {err}");
        assert!(err.contains("Root-Menü braucht"), "{err}");
        assert!(err.contains("außerhalb"), "E reaches beyond 720x576: {err}");

        // 37 buttons on one page.
        let many: Vec<Element> =
            (0..37).map(|i| Element { w: 15.0, h: 10.0, ..text((i % 9) as f64 * 70.0, (i / 9) as f64 * 40.0, "x", Some(Action::PlayTitle { title: 1 })) }).collect();
        let err = validate(&layout(vec![menu("m", MenuRole::Title, None, many)]), &titles(1), TvStandard::Pal).unwrap_err();
        assert!(err.0.iter().any(|e| e.contains("höchstens 36")), "{err:?}");
        // Root menus only: no disc start menu.
        let err = validate(&layout(vec![menu("r", MenuRole::Root, Some(1), vec![])]), &titles(1), TvStandard::Pal).unwrap_err();
        assert!(err.0[0].contains("Titelmenü"));
    }

    #[test]
    fn navigation_commands_across_domains() {
        let l = layout(vec![
            menu("main", MenuRole::Title, None, vec![
                text(10.0, 10.0, "play 2", Some(Action::PlayTitle { title: 2 })),
                text(10.0, 70.0, "ch 3 of 1", Some(Action::PlayChapter { title: 1, chapter: 3 })),
                text(10.0, 130.0, "extras of 1", Some(Action::Menu { menu: "extra1".into() })),
                text(10.0, 190.0, "all", Some(Action::PlayAll)),
                text(10.0, 250.0, "page 2", Some(Action::Menu { menu: "main2".into() })),
            ]),
            menu("main2", MenuRole::Title, None, vec![text(10.0, 10.0, "play 1", Some(Action::PlayTitle { title: 1 }))]),
            menu("root1", MenuRole::Root, Some(1), vec![
                text(10.0, 10.0, "play", Some(Action::PlayTitle { title: 1 })),
                text(10.0, 70.0, "ch 2", Some(Action::PlayChapter { title: 1, chapter: 2 })),
                text(10.0, 130.0, "other film", Some(Action::PlayTitle { title: 2 })),
                text(10.0, 190.0, "back", Some(Action::Menu { menu: "main".into() })),
                text(10.0, 250.0, "extras", Some(Action::Menu { menu: "extra1".into() })),
                text(10.0, 310.0, "ch 1 of 2", Some(Action::PlayChapter { title: 2, chapter: 1 })),
            ]),
            menu("extra1", MenuRole::Root, Some(1), vec![]),
        ]);
        let nav = plan_navigation(&l, &titles(2)).unwrap();
        assert_eq!(nav.placement[0], Placement::Vmgm(1));
        assert_eq!(nav.placement[1], Placement::Vmgm(2));
        // Title 1 has a dispatcher (chapter 3 and its 2nd root menu are entered from the VMGM).
        assert_eq!(nav.placement[2], Placement::Vtsm { title: 1, pgc: 2, index: 1 });
        assert_eq!(nav.placement[3], Placement::Vtsm { title: 1, pgc: 3, index: 2 });
        assert_eq!(
            nav.dispatchers[0].as_deref(),
            Some("if (g2 eq 3) jump title 1 chapter 3; if (g2 eq 102) jump menu 3; jump menu 2;")
        );
        // Title 2 only gets a chapter jump from outside (root1): dispatcher without root menus.
        assert_eq!(nav.dispatchers[1].as_deref(), Some("if (g2 eq 1) jump title 1 chapter 1; jump vmgm menu 1;"));
        let c = |m: usize, e: usize| nav.commands[m][e].clone().unwrap();
        assert_eq!(c(0, 0), "g1 = 0; jump title 2;");
        assert_eq!(c(0, 1), "g1 = 0; g2 = 3; jump titleset 1 menu;");
        assert_eq!(c(0, 2), "g2 = 102; jump titleset 1 menu;");
        assert_eq!(c(0, 3), "g1 = 1; jump title 1;");
        assert_eq!(c(0, 4), "jump menu 2;");
        // In title 1's VTSM: own title/chapters directly, the rest via the VMGM router (menu 3).
        assert_eq!(c(2, 0), "g1 = 0; jump title 1;");
        assert_eq!(c(2, 1), "g1 = 0; jump title 1 chapter 2;");
        assert_eq!(c(2, 2), "g1 = 0; g3 = 1002; jump vmgm menu 3;");
        assert_eq!(c(2, 3), "jump vmgm menu 1;");
        assert_eq!(c(2, 4), "jump menu 3;");
        assert_eq!(c(2, 5), "g1 = 0; g2 = 1; g3 = 2002; jump vmgm menu 3;");
        assert_eq!(nav.router, Some((3, "if (g3 eq 1002) jump title 2; if (g3 eq 2002) jump titleset 2 menu; jump menu 1;".into())));
        // Playback returns to the first title menu that plays the title.
        assert_eq!(nav.main_menu_pages, vec![1, 1]);
        for cmd in nav.commands.iter().flatten().flatten() {
            assert!(command_count(cmd) <= MAX_PGC_COMMANDS);
        }
    }

    #[test]
    fn dispatcher_command_limit() {
        // 99 chapters of title 1 + 40 root menus targeted from the VMGM → > 128 commands.
        let mut ts = titles(1);
        ts[0].chapters = 99;
        let mut els = Vec::new();
        for ch in 1..=99u32 {
            els.push(Element { w: 8.0, h: 4.0, ..text(((ch - 1) % 30) as f64 * 20.0, ((ch - 1) / 30) as f64 * 10.0, "c", Some(Action::PlayChapter { title: 1, chapter: ch })) });
        }
        let mut menus = vec![menu("main", MenuRole::Title, None, els.clone())];
        for j in 0..40 {
            menus.push(menu(&format!("r{j}"), MenuRole::Root, Some(1), vec![]));
        }
        menus[0].elements.truncate(99);
        let mut l = layout(menus);
        for j in 1..40 {
            l.menus[0].elements.push(Element {
                w: 8.0,
                h: 4.0,
                ..text((j % 30) as f64 * 20.0, 100.0 + (j / 30) as f64 * 10.0, "m", Some(Action::Menu { menu: format!("r{j}") }))
            });
        }
        let err = plan_navigation(&l, &ts).unwrap_err();
        assert!(err.0[0].contains("128"), "{err:?}");
    }

    #[test]
    fn settings_menus_and_stream_selection() {
        let l = layout(vec![
            menu("main", MenuRole::Title, None, vec![text(10.0, 10.0, "Einstellungen", Some(Action::Menu { menu: "set1".into() }))]),
            menu("root1", MenuRole::Root, Some(1), vec![text(10.0, 10.0, "Einstellungen", Some(Action::Menu { menu: "set1".into() }))]),
            menu("set1", MenuRole::Settings, Some(1), vec![
                text(10.0, 10.0, "Englisch", Some(Action::SetAudio { source: 2 })),
                text(10.0, 70.0, "Deutsch", Some(Action::SetAudio { source: 1 })),
                text(10.0, 130.0, "UT an", Some(Action::SetSubtitle { source: Some(3) })),
                text(10.0, 190.0, "UT aus", Some(Action::SetSubtitle { source: None })),
                text(10.0, 250.0, "Zurück", Some(Action::Menu { menu: "root1".into() })),
            ]),
        ]);
        validate(&l, &titles(1), TvStandard::Pal).unwrap();
        let nav = plan_navigation(&l, &titles(1)).unwrap();
        // Settings come after the root menus; the VMGM enters them through the dispatcher.
        assert_eq!(nav.placement[1], Placement::Vtsm { title: 1, pgc: 2, index: 1 });
        assert_eq!(nav.placement[2], Placement::Vtsm { title: 1, pgc: 3, index: 2 });
        assert_eq!(nav.dispatchers[0].as_deref(), Some("if (g2 eq 102) jump menu 3; jump menu 2;"));
        assert_eq!(nav.commands[0][0].as_deref(), Some("g2 = 102; jump titleset 1 menu;"));
        assert_eq!(nav.commands[1][0].as_deref(), Some("jump menu 3;"));
        // Source stream 2 is DVD audio stream 1 (second track), subtitle source 3 is stream 0.
        let c = |e: usize| nav.commands[2][e].clone().unwrap();
        assert_eq!(c(0), "audio = 1; jump menu 3;");
        assert_eq!(c(1), "audio = 0; jump menu 3;");
        assert_eq!(c(2), "subtitle = 64; jump menu 3;");
        assert_eq!(c(3), "subtitle = 0; jump menu 3;");
        assert_eq!(c(4), "jump menu 2;");

        // Stream selection only in a title's own VTSM menus, and only for selected streams.
        let bad = layout(vec![
            menu("main", MenuRole::Title, None, vec![text(10.0, 10.0, "Ton", Some(Action::SetAudio { source: 1 }))]),
            menu("set1", MenuRole::Settings, Some(1), vec![text(10.0, 10.0, "Ton 9", Some(Action::SetAudio { source: 9 }))]),
            menu("set2", MenuRole::Settings, None, vec![]),
        ]);
        let err = validate(&bad, &titles(1), TvStandard::Pal).unwrap_err().0.join("\n");
        assert!(err.contains("nur in Root- und Einstellungsmenüs"), "{err}");
        assert!(err.contains("Stream #9"), "{err}");
        assert!(err.contains("Einstellungsmenü braucht"), "{err}");

        // JSON names.
        let json = r#"{"type": "setSubtitle", "source": null}"#;
        assert_eq!(serde_json::from_str::<Action>(json).unwrap(), Action::SetSubtitle { source: None });
        let bg: Background = serde_json::from_str(r#"{"gradientOverlay": "bottomDark"}"#).unwrap();
        assert_eq!(bg.overlay, BackgroundOverlay::BottomDark);
        assert_eq!(serde_json::from_str::<MenuRole>(r#""settings""#).unwrap(), MenuRole::Settings);
    }

    fn with_colors(mut e: Element, hl: &str, sel: &str) -> Element {
        e.highlight = Some(ButtonColors { color: hl.into(), select_color: sel.into() });
        e
    }

    #[test]
    fn word_wrap() {
        let sentence = "Ein ziemlich langer Satz, der nicht in eine Zeile passt";
        let mut e = text(0.0, 0.0, sentence, None);
        e.w = 260.0;
        e.h = 200.0;
        let canvas = Canvas::default();
        // Without wrap: one line (cut with "…" when drawn).
        assert_eq!(element_text_lines(&e, canvas, TvStandard::Pal).unwrap(), vec![sentence.to_string()]);
        let Content::Text { wrap, .. } = &mut e.content else { unreachable!() };
        *wrap = true;
        let lines = element_text_lines(&e, canvas, TvStandard::Pal).unwrap();
        assert!(lines.len() >= 3, "{lines:?}");
        assert_eq!(lines.join(" "), sentence, "only spaces become line breaks");
        // Too little height: the last visible line ends with "…".
        e.h = 76.0; // exactly two lines of 38 px
        let short = element_text_lines(&e, canvas, TvStandard::Pal).unwrap();
        assert_eq!(short.len(), 2, "{short:?}");
        assert!(short[1].ends_with('…'), "{short:?}");
        // A word wider than the box is broken at characters; \n still starts a new line.
        let Content::Text { text: t, .. } = &mut e.content else { unreachable!() };
        *t = "Donaudampfschifffahrtsgesellschaft\nB".into();
        e.w = 120.0;
        e.h = 400.0;
        let long = element_text_lines(&e, canvas, TvStandard::Pal).unwrap();
        assert!(long.len() >= 3 && long.last().unwrap() == "B", "{long:?}");
        assert_eq!(long[..long.len() - 1].concat(), "Donaudampfschifffahrtsgesellschaft");
        assert_eq!(element_text_lines(&text(0.0, 0.0, "x", None), canvas, TvStandard::Pal), Some(vec!["x".to_string()]));
        let mut img = text(0.0, 0.0, "", None);
        img.content = Content::Video { title: 1, start_secs: None, fit: Fit::Cover };
        assert_eq!(element_text_lines(&img, canvas, TvStandard::Pal), None);
    }

    #[test]
    fn button_color_pairs() {
        let ts = titles(1);
        let b = |y: f64, label: &str| text(10.0, y, label, Some(Action::PlayTitle { title: 1 }));
        // Default + 2 own pairs: allowed; a 4th pair is too many.
        let three = vec![b(10.0, "a"), with_colors(b(80.0, "b"), "#00FF00", "#008000"), with_colors(b(150.0, "c"), "#0000FF", "#000080")];
        assert!(validate(&layout(vec![menu("main", MenuRole::Title, None, three.clone())]), &ts, TvStandard::Pal).is_ok());
        let mut four = three.clone();
        four.push(with_colors(b(220.0, "d"), "#FF00FF", "#800080"));
        let err = validate(&layout(vec![menu("main", MenuRole::Title, None, four)]), &ts, TvStandard::Pal).unwrap_err();
        assert!(err.0.iter().any(|m| m.contains("4 verschiedene Button-Farben")), "{err:?}");
        // Same marked colour with another activated colour cannot be told apart.
        let shared = vec![b(10.0, "a"), with_colors(b(80.0, "b"), "#FFD200", "#00FF00")];
        let err = validate(&layout(vec![menu("main", MenuRole::Title, None, shared)]), &ts, TvStandard::Pal).unwrap_err();
        assert!(err.0.iter().any(|m| m.contains("gleiche Aktivierungsfarbe")), "{err:?}");
        let bad = vec![with_colors(b(10.0, "a"), "gelb", "#000000")];
        assert!(validate(&layout(vec![menu("main", MenuRole::Title, None, bad)]), &ts, TvStandard::Pal).is_err());
        // Colours on a non-button are ignored by the pair count.
        let l = layout(vec![menu("main", MenuRole::Title, None, vec![b(10.0, "a"), with_colors(text(10.0, 300.0, "Deko", None), "#123456", "#654321")])]);
        assert_eq!(color_pairs(&l, &l.menus[0]).len(), 1);

        // Rendering: every pair gets its own pixel value in both masks.
        let dir = std::env::temp_dir().join(format!("cx_layout_colors_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut picture = with_colors(b(300.0, ""), "#00FF00", "#008000");
        picture.content = Content::Image { src: String::new(), fit: Fit::Stretch };
        let png = dir.join("pic.png");
        RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 255])).save(&png).unwrap();
        picture.content = Content::Image { src: png.to_string_lossy().into_owned(), fit: Fit::Stretch };
        let l = layout(vec![menu("main", MenuRole::Title, None, vec![b(10.0, "Erster"), picture])]);
        let out = build_custom_menus(&l, &ts, TvStandard::Pal, &dir, 10.0).unwrap();
        let files = &out.pages[0].1;
        let hl = image::open(&files.highlight).unwrap().to_rgba8();
        let sel = image::open(&files.select).unwrap().to_rgba8();
        assert_eq!(hl.get_pixel(11, 301).0, [0, 255, 0, 204], "frame of the green button");
        assert_eq!(sel.get_pixel(11, 301).0, [0, 128, 0, 204]);
        let glyph = (10..310).flat_map(|x| (10..60).map(move |y| (x, y))).find(|&(x, y)| hl.get_pixel(x, y).0[3] > 0).expect("text glyphs");
        assert_eq!(hl.get_pixel(glyph.0, glyph.1).0, [255, 210, 0, 204], "default pair, no half-transparent edge");
        let colors: std::collections::BTreeSet<[u8; 4]> = hl.pixels().map(|p| p.0).filter(|p| p[3] > 0).collect();
        assert_eq!(colors.len(), 2, "one colour per pair: {colors:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pictures_keep_their_proportions_on_the_tv() {
        // A 16:9 picture "contain"-fitted into a 720×405 box: on a 4:3 PAL screen every pixel is
        // 16/15 wide, so the picture is 675 pixels wide (= 720 on screen) with bars at the sides.
        let img = RgbaImage::from_pixel(160, 90, image::Rgba([255, 0, 0, 255]));
        let pal = TvStandard::Pal.menu_pixel_aspect();
        assert!((pal - 16.0 / 15.0).abs() < 1e-9 && (TvStandard::Ntsc.menu_pixel_aspect() - 8.0 / 9.0).abs() < 1e-9);
        let fitted = fit_image(&img, 720, 405, Fit::Contain, pal, false);
        let visible = (0..720).filter(|&x| fitted.get_pixel(x, 200).0[3] > 0).count();
        assert!((visible as i32 - 675).abs() <= 2, "{visible}");
        // Rotated by 90°: the picture's height becomes the horizontal and is squeezed instead
        // (90 → 84 rows, then scaled ×405/160 like the width: 213 instead of 228 rows).
        let sideways = fit_image(&img, 405, 720, Fit::Contain, pal, true);
        let tall = (0..720).filter(|&y| sideways.get_pixel(200, y).0[3] > 0).count();
        assert!((tall as i32 - 213).abs() <= 2, "{tall}");
        // Stretch fills the box regardless.
        assert!(fit_image(&img, 720, 405, Fit::Stretch, pal, false).pixels().all(|p| p.0[3] == 255));
    }

    #[test]
    fn renders_layers_masks_and_specs() {
        let dir = std::env::temp_dir().join(format!("cx_layout_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("pic.png");
        RgbaImage::from_pixel(40, 20, image::Rgba([0, 200, 0, 255])).save(&png).unwrap();
        let mut picture = text(400.0, 40.0, "", Some(Action::Menu { menu: "root".into() }));
        picture.content = Content::Image { src: png.to_string_lossy().into_owned(), fit: Fit::Stretch };
        picture.w = 200.0;
        picture.h = 100.0;
        let mut video = text(60.0, 200.0, "", None);
        video.content = Content::Video { title: 1, start_secs: Some(42.0), fit: Fit::Cover };
        video.rotation = 90;
        let caption = text(60.0, 470.0, "Film abspielen", Some(Action::PlayTitle { title: 1 }));
        let l = layout(vec![
            menu("main", MenuRole::Title, None, vec![picture, video, caption]),
            menu("root", MenuRole::Root, Some(1), vec![text(60.0, 60.0, "Zurück", Some(Action::Menu { menu: "main".into() }))]),
        ]);
        let out = build_custom_menus(&l, &titles(1), TvStandard::Pal, &dir, 15.0).unwrap();
        assert_eq!(out.pages.len(), 2);
        assert_eq!(out.vmgm_menus.len(), 1, "no router needed");
        assert_eq!(out.titleset_menus[0].len(), 1, "no dispatcher: only the first root menu is entered");
        assert_eq!(out.titleset_menus[0][0].entry.as_deref(), Some("root"));
        assert_eq!(out.titleset_menus[0][0].pre.as_deref(), Some(RESET_DISPATCH));
        assert_eq!(out.vmgm_menus[0].entry.as_deref(), Some("title"));
        assert_eq!(out.title_pre, RESET_DISPATCH);

        // Main menu: image layer, video layer, caption layer; moving (video) for 15 s.
        let (spec, files) = &out.pages[0];
        assert!(spec.motion);
        assert_eq!(files.motion_secs, Some(15.0));
        let media = files.custom.as_ref().unwrap();
        assert_eq!(media.background, MenuBackground::Color([0, 0, 0]));
        assert_eq!(media.layers.len(), 3);
        assert!(matches!(&media.layers[1], MenuLayer::Video { x: 60, y: 200, width: 300, height: 50, rotation: 90, clip, .. } if clip.start_secs == 42.0));
        let MenuLayer::Image(under) = &media.layers[0] else { panic!() };
        let under = image::open(under).unwrap().to_rgba8();
        assert_eq!(under.dimensions(), (720, 576));
        assert_eq!(under.get_pixel(500, 90).0, [0, 200, 0, 255], "picture drawn");
        assert_eq!(under.get_pixel(10, 10).0[3], 0, "transparent elsewhere");

        // Buttons: picture (frame) and caption (glyphs), commands and spumux geometry.
        assert_eq!(spec.buttons.len(), 2);
        assert_eq!(spec.buttons[0].action, ButtonAction::Commands("g2 = 0; jump titleset 1 menu;".into()));
        assert_eq!(spec.buttons[1].action, ButtonAction::Commands("g1 = 0; jump title 1;".into()));
        assert_eq!(spec.buttons[0].rect, Rect { x0: 400, y0: 40, x1: 600, y1: 140 });
        let hl = image::open(&files.highlight).unwrap().to_rgba8();
        assert_eq!(hl.get_pixel(401, 90).0, [255, 210, 0, 204], "frame of the picture button");
        assert_eq!(hl.get_pixel(500, 90).0[3], 0, "inside of the frame stays clear");
        let text_px = (60..360).flat_map(|x| (470..520).map(move |y| (x, y))).filter(|&(x, y)| hl.get_pixel(x, y).0[3] > 0).count();
        assert!(text_px > 200, "caption glyphs in the mask: {text_px}");
        let xml = std::fs::read_to_string(&files.spumux_xml).unwrap();
        assert!(xml.contains(r#"name="b1" x0="400" y0="40" x1="600" y1="140""#), "{xml}");
        assert!(xml.contains(r#"down="b2""#) || xml.contains(r#"left="b2""#), "navigation computed: {xml}");

        // Root menu: still (no video, no sound).
        let (rspec, rfiles) = &out.pages[1];
        assert!(!rspec.motion && rfiles.motion_secs.is_none());
        assert_eq!(rspec.buttons[0].action, ButtonAction::Commands("jump vmgm menu 1;".into()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ntsc_scales_the_canvas() {
        let l = CustomLayout {
            canvas: Canvas { width: 720, height: 576 },
            ..layout(vec![menu("m", MenuRole::Title, None, vec![text(0.0, 288.0, "x", Some(Action::PlayTitle { title: 1 }))])])
        };
        let e = &l.menus[0].elements[0];
        assert_eq!(frame_box(e, l.canvas, TvStandard::Ntsc), (0, 240, 300, 42));
        assert_eq!(button_rect(e, l.canvas, TvStandard::Ntsc), Some(Rect { x0: 0, y0: 240, x1: 300, y1: 282 }));
        assert!((frame_font_size(36.0, l.canvas, TvStandard::Ntsc) - 30.0).abs() < 1e-4);
    }

    #[test]
    fn group_field_roundtrips_and_is_optional() {
        let json = r##"{"id":"a","type":"text","text":"x","x":0,"y":0,"w":10,"h":10,"group":"g1"}"##;
        let e: Element = serde_json::from_str(json).unwrap();
        assert_eq!(e.group.as_deref(), Some("g1"));
        let again: Element = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(again.group.as_deref(), Some("g1"));
        let plain: Element = serde_json::from_str(&json.replace(r#","group":"g1""#, "")).unwrap();
        assert_eq!(plain.group, None);
        assert!(!serde_json::to_string(&plain).unwrap().contains("group"));
    }
}
