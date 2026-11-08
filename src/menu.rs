//! Menu renderer: turns a template page into the still menu background and the two
//! 4-colour subpicture masks spumux needs.
//!
//! - `menu_bg.png`: 720x576 RGB(A) still. Background, bitmaps and texts of all *visible*
//!   buttons, composited back to front like the template's block order.
//! - `menu_highlight.png` / `menu_select.png`: 8-bit palette PNGs. Palette index
//!   0 = background (transparent), 1 = pattern, 2 = emphasis1, 3 = emphasis2, with the colours
//!   and alpha of the button's colour scheme (`selection_*` resp. `action_*`).
//!   Text buttons light up the glyphs (pattern) and their outline (emphasis1); image buttons use
//!   their `*_overlay.gif` mask, else the bitmap's alpha, else the full rectangle.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::fmt;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use ab_glyph::{point, Font, FontArc, Glyph, GlyphId, PxScale, ScaleFont};
use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};

use crate::author::{
    self, AuthorError, ClipSource, DvdLayout, MenuButton, MenuKind, MenuLinks, MenuSpec, TitleEntry,
    TitleSpec, TitlesetSpec, TvStandard,
};
use crate::encoder::VideoSlot;
use crate::menu_anim::{self, Intro, Pixels, Sprite};
use crate::template::{ColorScheme, DelphiColor, Graphic, Page, Template, TextStyle};

#[derive(Debug)]
pub enum MenuError {
    Io(std::io::Error),
    Image(image::ImageError),
    Png(png::EncodingError),
    Author(AuthorError),
    NoPage(String),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MenuError::Io(e) => write!(f, "I/O error: {e}"),
            MenuError::Image(e) => write!(f, "image error: {e}"),
            MenuError::Png(e) => write!(f, "PNG encoding error: {e}"),
            MenuError::Author(e) => write!(f, "{e}"),
            MenuError::NoPage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MenuError {}

impl From<std::io::Error> for MenuError {
    fn from(e: std::io::Error) -> Self {
        MenuError::Io(e)
    }
}
impl From<image::ImageError> for MenuError {
    fn from(e: image::ImageError) -> Self {
        MenuError::Image(e)
    }
}
impl From<png::EncodingError> for MenuError {
    fn from(e: png::EncodingError) -> Self {
        MenuError::Png(e)
    }
}
impl From<AuthorError> for MenuError {
    fn from(e: AuthorError) -> Self {
        MenuError::Author(e)
    }
}

// ---------------------------------------------------------------------------
// Palette image (subpicture mask)
// ---------------------------------------------------------------------------

pub const IDX_BACKGROUND: u8 = 0;
pub const IDX_PATTERN: u8 = 1;
pub const IDX_EMPHASIS1: u8 = 2;
pub const IDX_EMPHASIS2: u8 = 3;

/// 8-bit indexed image with a 4-entry RGBA palette.
#[derive(Debug, Clone)]
pub struct IndexedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub palette: [[u8; 4]; 4],
}

impl IndexedImage {
    pub fn new(width: u32, height: u32, palette: [[u8; 4]; 4]) -> Self {
        IndexedImage { width, height, pixels: vec![IDX_BACKGROUND; (width * height) as usize], palette }
    }

    pub fn get(&self, x: u32, y: u32) -> u8 {
        self.pixels[(y * self.width + x) as usize]
    }

    fn set(&mut self, x: u32, y: u32, v: u8) {
        self.pixels[(y * self.width + x) as usize] = v;
    }

    /// Writes an 8-bit palette PNG (PLTE + tRNS) as spumux expects.
    pub fn write_png(&self, path: &Path) -> Result<(), MenuError> {
        let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), self.width, self.height);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(self.palette.iter().flat_map(|c| [c[0], c[1], c[2]]).collect::<Vec<u8>>());
        enc.set_trns(self.palette.iter().map(|c| c[3]).collect::<Vec<u8>>());
        let mut w = enc.write_header()?;
        w.write_image_data(&self.pixels)?;
        w.finish()?;
        Ok(())
    }
}

/// Palette for a colour scheme: (background, pattern, emphasis1, emphasis2).
fn scheme_palette(colors: &[Option<DelphiColor>; 4], fallback: [[u8; 4]; 4]) -> [[u8; 4]; 4] {
    let mut out = fallback;
    for (slot, c) in out.iter_mut().zip(colors) {
        if let Some(rgba) = c.as_ref().and_then(DelphiColor::rgba) {
            *slot = rgba;
        }
    }
    out
}

const DEFAULT_SELECTION: [[u8; 4]; 4] = [[0, 0, 0, 0], [255, 255, 0, 200], [255, 255, 0, 150], [255, 255, 0, 80]];
const DEFAULT_ACTION: [[u8; 4]; 4] = [[0, 0, 0, 0], [255, 128, 0, 200], [255, 128, 0, 150], [255, 128, 0, 80]];

// ---------------------------------------------------------------------------
// Fonts and text
// ---------------------------------------------------------------------------

/// DejaVu Sans bundled font data (from `assets/fonts/DejaVuSans.ttf`).
static BUNDLED_DEJAVU_FONT: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

fn bundled_font() -> FontArc {
    static FONT: std::sync::OnceLock<FontArc> = std::sync::OnceLock::new();
    FONT.get_or_init(|| FontArc::try_from_slice(BUNDLED_DEJAVU_FONT).expect("bundled DejaVu Sans font is valid")).clone()
}

/// System font directories on Windows and Unix/macOS, cached and filtered to existing directories.
fn font_dirs() -> &'static [PathBuf] {
    static DIRS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Some(windir) = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot")) {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        } else if cfg!(windows) {
            dirs.push(PathBuf::from(r"C:\Windows\Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
        }
        // Common Unix / macOS font directories
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
            dirs.push(home.join("Library/Fonts"));
        }
        dirs.push(PathBuf::from("/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.retain(|d| d.is_dir());
        dirs
    })
}

/// Candidate font files for a family (regular, bold, italic, bold italic) in system font directories.
fn font_candidates(style: &TextStyle) -> Vec<PathBuf> {
    let family = style.font_name.trim().to_ascii_lowercase();
    let variants: Option<[&str; 4]> = match family.as_str() {
        "arial" => Some(["arial.ttf", "arialbd.ttf", "ariali.ttf", "arialbi.ttf"]),
        "arial rounded mt bold" => Some(["ARLRDBD.TTF"; 4]),
        "arial narrow" => Some(["ARIALN.TTF", "ARIALNB.TTF", "ARIALNI.TTF", "ARIALNBI.TTF"]),
        "arial black" => Some(["ariblk.ttf"; 4]),
        "tahoma" => Some(["tahoma.ttf", "tahomabd.ttf", "tahoma.ttf", "tahomabd.ttf"]),
        "verdana" => Some(["verdana.ttf", "verdanab.ttf", "verdanai.ttf", "verdanaz.ttf"]),
        "times new roman" => Some(["times.ttf", "timesbd.ttf", "timesi.ttf", "timesbi.ttf"]),
        "georgia" => Some(["georgia.ttf", "georgiab.ttf", "georgiai.ttf", "georgiaz.ttf"]),
        "trebuchet ms" => Some(["trebuc.ttf", "trebucbd.ttf", "trebucit.ttf", "trebucbi.ttf"]),
        "segoe ui" => Some(["segoeui.ttf", "segoeuib.ttf", "segoeuii.ttf", "segoeuiz.ttf"]),
        "comic sans ms" => Some(["comic.ttf", "comicbd.ttf", "comici.ttf", "comicz.ttf"]),
        "courier new" => Some(["cour.ttf", "courbd.ttf", "couri.ttf", "courbi.ttf"]),
        "impact" => Some(["impact.ttf"; 4]),
        _ => None,
    };
    let variant = (style.bold as usize) | ((style.italic as usize) << 1);
    let dirs = font_dirs();
    let mut out = Vec::new();
    if let Some(f) = &style.font_file {
        out.push(PathBuf::from(f));
        for dir in dirs {
            out.push(dir.join(f));
        }
    }
    for dir in dirs {
        if let Some(v) = variants {
            out.push(dir.join(v[variant]));
            out.push(dir.join(v[0]));
        }
        out.push(dir.join(format!("{}.ttf", family.replace(' ', ""))));
        out.push(dir.join("arial.ttf"));
    }
    out
}

/// Fonts loaded so far, shared by all (parallel) page renders: each TTF is read only once.
fn font_cache() -> &'static Mutex<HashMap<PathBuf, Option<FontArc>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<FontArc>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Loads (once) the font file at `path`.
fn load_font(path: &Path) -> Option<FontArc> {
    if let Some(cached) = font_cache().lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        return cached.clone();
    }
    // Load outside the lock; a concurrent duplicate load is harmless.
    let font = std::fs::read(path).ok().and_then(|d| FontArc::try_from_vec(d).ok());
    font_cache().lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_path_buf(), font.clone());
    font
}

/// The template font of `style` (first loadable candidate).
fn font_for(style: &TextStyle) -> Option<FontArc> {
    font_candidates(style).iter().find_map(|p| load_font(p))
}

/// Coverage (0..1) canvas in page coordinates.
pub(crate) struct Coverage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) data: Vec<f32>,
}

impl Coverage {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Coverage { width, height, data: vec![0.0; (width * height) as usize] }
    }

    pub(crate) fn at(&self, x: i64, y: i64) -> f32 {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            0.0
        } else {
            self.data[(y as u32 * self.width + x as u32) as usize]
        }
    }

    /// Bounding box `(x0, y0, x1, y1)` (exclusive) of all non-zero pixels.
    fn bounds(&self) -> Option<(i64, i64, i64, i64)> {
        let mut b: Option<(i64, i64, i64, i64)> = None;
        for (i, v) in self.data.iter().enumerate() {
            if *v > 0.0 {
                let (x, y) = ((i as u32 % self.width) as i64, (i as u32 / self.width) as i64);
                b = Some(match b {
                    None => (x, y, x + 1, y + 1),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)),
                });
            }
        }
        b
    }

    /// Morphological dilation with a disk of radius `r` (text outline).
    fn dilate(&self, r: u32) -> Coverage {
        let mut out = Coverage::new(self.width, self.height);
        let Some((bx0, by0, bx1, by1)) = self.bounds() else { return out };
        if r == 0 {
            out.data.clone_from(&self.data);
            return out;
        }
        let r = r as i64;
        let offsets: Vec<(i64, i64)> = (-r..=r)
            .flat_map(|dy| (-r..=r).map(move |dx| (dx, dy)))
            .filter(|(dx, dy)| dx * dx + dy * dy <= r * r + r)
            .collect();
        for y in (by0 - r).max(0)..(by1 + r).min(self.height as i64) {
            for x in (bx0 - r).max(0)..(bx1 + r).min(self.width as i64) {
                let v = offsets.iter().map(|(dx, dy)| self.at(x + dx, y + dy)).fold(0.0, f32::max);
                out.data[(y as u32 * self.width + x as u32) as usize] = v;
            }
        }
        out
    }
}

fn fallback_style(height: u32) -> TextStyle {
    TextStyle {
        name: String::new(),
        font_name: "Arial".into(),
        font_file: None,
        size_px: (height as f32 * 0.8).max(8.0),
        bold: false,
        italic: false,
        text: DelphiColor::parse("White"),
        outline: DelphiColor::parse("Black"),
        shadow: None,
        outline_size: 1,
        shadow_offset: 0,
        shadow_align: String::new(),
        align: "MIDDLE_CENTER".into(),
        left_margin: 0,
        right_margin: 0,
        vert_margin: 0,
    }
}

/// Smallest scale a text is shrunk to before it gets cut off with an ellipsis (default).
const MIN_TEXT_SCALE: f32 = 0.7;

/// What to do with menu texts that are too wide for their box ("Text zu lang").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextOverflow {
    /// Shrink moderately (to 70 %), then cut off with "…" (CLI default).
    #[default]
    Balanced,
    /// Shrink further (to 50 %) before cutting off.
    ReduceFontSize,
    /// Keep the font size, cut off with "…" right away.
    Truncate,
}

impl TextOverflow {
    fn min_scale(self) -> f32 {
        match self {
            TextOverflow::Balanced => MIN_TEXT_SCALE,
            TextOverflow::ReduceFontSize => 0.5,
            TextOverflow::Truncate => 1.0,
        }
    }
}
/// Fonts used for characters the template font does not have (e.g. Cyrillic in
/// "Arial Rounded MT Bold" or CJK ideographs), in order.
pub(crate) const FALLBACK_FONTS: &[&str] = &[
    "arial.ttf",
    "segoeui.ttf",
    "seguisym.ttf",
    // CJK fallbacks for Windows
    "msyh.ttc",
    "msgothic.ttc",
    "malgun.ttf",
    "simsun.ttc",
    "yugothr.ttc",
    // CJK fallbacks for Linux
    "NotoSansCJK-Regular.ttc",
    "wqy-zenhei.ttc",
    "wqy-microhei.ttc",
    "DroidSansFallbackFull.ttf",
    // CJK fallbacks for macOS
    "PingFang.ttc",
    "Hiragino Sans GB.ttc",
    "AppleSDGothicNeo.ttc",
];

/// Template font followed by system fallback fonts and the bundled DejaVu Sans.
fn font_chain(style: &TextStyle) -> Vec<FontArc> {
    let mut chain: Vec<FontArc> = font_for(style).into_iter().collect();
    let dirs = font_dirs();
    for f in FALLBACK_FONTS {
        if let Some(font) = dirs.iter().find_map(|dir| load_font(&dir.join(f))) {
            chain.push(font);
        }
    }
    // Always include the bundled DejaVu Sans as final fallback so text rendering
    // never fails completely if system fonts are missing.
    chain.push(bundled_font());
    chain
}

/// Font chain for native subtitle rendering: bundled DejaVu Sans first, followed by system fallbacks (CJK etc.).
pub(crate) fn subtitle_font_chain() -> Vec<FontArc> {
    static CHAIN: OnceLock<Vec<FontArc>> = OnceLock::new();
    CHAIN
        .get_or_init(|| {
            let mut chain = vec![bundled_font()];
            let dirs = font_dirs();
            for f in FALLBACK_FONTS {
                if let Some(font) = dirs.iter().find_map(|dir| load_font(&dir.join(f))) {
                    chain.push(font);
                }
            }
            chain
        })
        .clone()
}

/// One laid-out character: which font of the chain renders it, and its glyph there.
#[derive(Clone, Copy)]
struct Placed {
    font: usize,
    id: GlyphId,
}

/// Picks for every character the first font of the chain that has a glyph for it
/// (the primary font's `.notdef` box if none has).
fn place_chars(fonts: &[FontArc], text: &str) -> Vec<Placed> {
    text.chars()
        .map(|c| {
            fonts
                .iter()
                .enumerate()
                .map(|(i, f)| Placed { font: i, id: f.glyph_id(c) })
                .find(|p| p.id.0 != 0)
                .unwrap_or(Placed { font: 0, id: fonts[0].glyph_id(c) })
        })
        .collect()
}

/// Scale of font `f` for an em height of `em_px` pixels.
fn font_scale(f: &FontArc, em_px: f32) -> PxScale {
    let units = f.units_per_em().unwrap_or(1000.0);
    PxScale::from(em_px * f.height_unscaled() / units)
}

/// Advance width of `placed` at em height `em_px` (kerning within runs of the same font).
fn text_width(fonts: &[FontArc], placed: &[Placed], em_px: f32) -> f32 {
    let mut w = 0.0;
    let mut prev: Option<Placed> = None;
    for p in placed {
        let sf = fonts[p.font].as_scaled(font_scale(&fonts[p.font], em_px));
        if let Some(q) = prev.filter(|q| q.font == p.font) {
            w += sf.kern(q.id, p.id);
        }
        w += sf.h_advance(p.id);
        prev = Some(*p);
    }
    w
}

/// Fits `text` into `max_w`: shrinks it down to `min_scale`, then cuts it off with "…".
/// Returns the characters to draw and the em height.
fn fit_text(fonts: &[FontArc], text: &str, em_px: f32, max_w: f32, min_scale: f32) -> (Vec<Placed>, f32) {
    // Half a pixel of slack: shrinking to exactly `max_w` must not fail on float rounding.
    let max_w = max_w + 0.5;
    let placed = place_chars(fonts, text);
    let width = text_width(fonts, &placed, em_px);
    if width <= max_w {
        return (placed, em_px);
    }
    let shrink = (max_w / width).max(min_scale).min(1.0);
    let em = em_px * shrink;
    if text_width(fonts, &placed, em) <= max_w {
        return (placed, em);
    }
    let ellipsis = place_chars(fonts, "…");
    let chars: Vec<char> = text.chars().collect();
    let mut keep = placed.len();
    while keep > 0 {
        keep -= 1;
        // No dangling space before the ellipsis.
        let mut n = keep;
        while n > 0 && chars[n - 1].is_whitespace() {
            n -= 1;
        }
        let mut candidate: Vec<Placed> = placed[..n].to_vec();
        candidate.extend(&ellipsis);
        if text_width(fonts, &candidate, em) <= max_w {
            return (candidate, em);
        }
    }
    (ellipsis, em)
}

/// Rasterises a single line of `text` into the graphic's rectangle using the style's alignment
/// and margins. Characters missing in the template font come from fallback fonts; text wider
/// than the box is shrunk (see [`TextOverflow`]) and then cut off with "…".
fn rasterize_text(
    fonts: &[FontArc],
    style: &TextStyle,
    text: &str,
    g: &Graphic,
    page: (u32, u32),
    overflow: TextOverflow,
) -> Coverage {
    let (page_w, page_h) = page;
    let mut cov = Coverage::new(page_w, page_h);
    let inner_x0 = (g.x + style.left_margin) as f32;
    let inner_x1 = (g.x + g.width as i32 - style.right_margin) as f32;
    let inner_y0 = (g.y + style.vert_margin) as f32;
    let inner_y1 = (g.y + g.height as i32 - style.vert_margin) as f32;
    let inner_w = (inner_x1 - inner_x0).max(1.0);

    let (placed, em) = fit_text(fonts, text, style.size_px, inner_w, overflow.min_scale());
    let width = text_width(fonts, &placed, em);
    // Vertical metrics come from the template font, so mixed scripts share one baseline.
    let primary = fonts[0].as_scaled(font_scale(&fonts[0], em));
    let (ascent, descent) = (primary.ascent(), primary.descent());

    let align = style.align.to_ascii_uppercase();
    let x = if align.ends_with("RIGHT") {
        inner_x1 - width
    } else if align.ends_with("CENTER") {
        inner_x0 + (inner_w - width) / 2.0
    } else {
        inner_x0
    };
    let baseline = if align.starts_with("TOP") {
        inner_y0 + ascent
    } else if align.starts_with("BOTTOM") {
        inner_y1 + descent
    } else {
        inner_y0 + ((inner_y1 - inner_y0) - (ascent - descent)) / 2.0 + ascent
    };

    let mut caret = x;
    let mut prev: Option<Placed> = None;
    for p in placed {
        let font = &fonts[p.font];
        let scale = font_scale(font, em);
        let sf = font.as_scaled(scale);
        if let Some(q) = prev.filter(|q| q.font == p.font) {
            caret += sf.kern(q.id, p.id);
        }
        let glyph: Glyph = p.id.with_scale_and_position(scale, point(caret, baseline));
        caret += sf.h_advance(p.id);
        prev = Some(p);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let b = outlined.px_bounds();
            outlined.draw(|px, py, c| {
                let (x, y) = (b.min.x as i64 + px as i64, b.min.y as i64 + py as i64);
                if x >= 0 && y >= 0 && x < page_w as i64 && y < page_h as i64 {
                    let i = (y as u32 * page_w + x as u32) as usize;
                    cov.data[i] = cov.data[i].max(c.min(1.0));
                }
            });
        }
    }
    cov
}

/// Text style of a menu editor text box.
fn editor_style(family: &str, size_px: f32, bold: bool, italic: bool, align: &str, height: u32) -> TextStyle {
    TextStyle {
        font_name: family.to_string(),
        size_px: size_px.max(4.0),
        bold,
        italic,
        align: match align {
            "left" => "MIDDLE_LEFT",
            "right" => "MIDDLE_RIGHT",
            _ => "MIDDLE_CENTER",
        }
        .into(),
        left_margin: 0,
        right_margin: 0,
        vert_margin: 0,
        ..fallback_style(height)
    }
}

/// Line height of a menu editor text (1.2 em).
fn editor_line_height(size_px: f32) -> i32 {
    (size_px.max(4.0) * 1.2).round().max(1.0) as i32
}

/// Breaks `text` into lines of at most `max_w` pixels: at spaces, a word wider than a line at
/// characters. `\n` always starts a new line.
fn wrap_text(fonts: &[FontArc], text: &str, em_px: f32, max_w: f32) -> Vec<String> {
    let width = |s: &str| text_width(fonts, &place_chars(fonts, s), em_px);
    let max_w = max_w + 0.5;
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if width(&candidate) <= max_w {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if width(word) <= max_w {
                line = word.to_string();
                continue;
            }
            for c in word.chars() {
                let candidate = format!("{line}{c}");
                if width(&candidate) > max_w && !line.is_empty() {
                    out.push(std::mem::replace(&mut line, c.to_string()));
                } else {
                    line = candidate;
                }
            }
        }
        out.push(line);
    }
    out
}

/// The lines a menu editor text shows in its `width`×`height` box: one per `\n`, with `wrap`
/// also broken to the box width. Lines starting below the box are dropped; with `wrap` the
/// last shown line then ends with "…". (Without `wrap`, overlong lines are cut with "…" when
/// drawn.) The editor shows exactly these lines.
#[allow(clippy::too_many_arguments)]
pub fn text_lines(text: &str, family: &str, size_px: f32, bold: bool, italic: bool, wrap: bool, width: u32, height: u32) -> Vec<String> {
    let style = editor_style(family, size_px, bold, italic, "left", height);
    let fonts = font_chain(&style);
    let mut lines: Vec<String> = if wrap && !fonts.is_empty() {
        wrap_text(&fonts, text, style.size_px, width as f32)
    } else {
        text.split('\n').map(str::to_string).collect()
    };
    let line_h = editor_line_height(size_px);
    let visible = (height as i32 + line_h - 1) / line_h;
    if lines.len() > visible as usize {
        lines.truncate(visible.max(0) as usize);
        if wrap && !fonts.is_empty() {
            if let Some(last) = lines.last_mut() {
                // "…" at the end, dropping characters until it fits.
                let width_of = |s: &str| text_width(&fonts, &place_chars(&fonts, s), style.size_px);
                let mut base: Vec<char> = last.trim_end().chars().collect();
                loop {
                    let candidate = format!("{}…", base.iter().collect::<String>().trim_end());
                    if base.is_empty() || width_of(&candidate) <= width as f32 + 0.5 {
                        *last = candidate;
                        break;
                    }
                    base.pop();
                }
            }
        }
    }
    lines
}

/// Glyph coverage of `text` in a `width`x`height` box (menu editor texts): the lines of
/// [`text_lines`], line height 1.2 em, from the top, each aligned `left`, `center` or `right`.
/// Uses the same font chain (fallback fonts for missing characters) as template texts; lines
/// wider than the box are cut off with "…".
#[allow(clippy::too_many_arguments)]
pub(crate) fn text_coverage(
    text: &str,
    family: &str,
    size_px: f32,
    bold: bool,
    italic: bool,
    wrap: bool,
    align: &str,
    width: u32,
    height: u32,
) -> Coverage {
    let mut cov = Coverage::new(width.max(1), height.max(1));
    let style = editor_style(family, size_px, bold, italic, align, height);
    let fonts = font_chain(&style);
    if fonts.is_empty() {
        return cov;
    }
    let line_h = editor_line_height(size_px);
    for (i, line) in text_lines(text, family, size_px, bold, italic, wrap, width, height).iter().enumerate() {
        let y = i as i32 * line_h;
        if line.trim().is_empty() {
            continue;
        }
        let g = Graphic {
            block: 0,
            name: String::new(),
            value: line.to_string(),
            gfx_type: "GT_TEXT".into(),
            location: String::new(),
            param: 0,
            x: 0,
            y,
            width,
            height: line_h as u32,
            alpha: 255,
            style: String::new(),
            padding: String::new(),
            gfx_alpha: 255,
            block_rect: (0, 0, 0, 0),
            intro: Vec::new(),
            outro: Vec::new(),
        };
        let line_cov = rasterize_text(&fonts, &style, line, &g, (cov.width, cov.height), TextOverflow::Truncate);
        for (d, v) in cov.data.iter_mut().zip(line_cov.data) {
            *d = d.max(v);
        }
    }
    cov
}

fn shadow_offset(style: &TextStyle) -> (i64, i64) {
    let o = style.shadow_offset as i64;
    let a = style.shadow_align.to_ascii_uppercase();
    let dx = if a.ends_with("LEFT") { -o } else if a.ends_with("RIGHT") { o } else { 0 };
    let dy = if a.starts_with("TOP") { -o } else if a.starts_with("BOTTOM") { o } else { 0 };
    (dx, dy)
}

/// Blends `color` through `cov` (shifted by `dx`,`dy`) onto `img`, scaled by `alpha` (0..255).
pub(crate) fn blend_coverage(img: &mut RgbaImage, cov: &Coverage, color: [u8; 4], dx: i64, dy: i64, alpha: u8) {
    let a_scale = color[3] as f32 / 255.0 * alpha as f32 / 255.0;
    if a_scale <= 0.0 {
        return;
    }
    let Some((bx0, by0, bx1, by1)) = cov.bounds() else { return };
    let (w, h) = (img.width() as i64, img.height() as i64);
    for y in (by0 + dy).max(0)..(by1 + dy).min(h) {
        for x in (bx0 + dx).max(0)..(bx1 + dx).min(w) {
            let c = cov.at(x - dx, y - dy) * a_scale;
            if c > 0.0 {
                blend_pixel(img.get_pixel_mut(x as u32, y as u32), [color[0], color[1], color[2]], c);
            }
        }
    }
}

/// Source-over compositing with straight (non-premultiplied) alpha, so drawing onto a
/// transparent layer (motion menu foreground) keeps the original colours.
fn blend_pixel(dst: &mut Rgba<u8>, src: [u8; 3], a: f32) {
    let da = dst[3] as f32 / 255.0;
    let out_a = a + da * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for i in 0..3 {
        dst[i] = ((src[i] as f32 * a + dst[i] as f32 * da * (1.0 - a)) / out_a).round() as u8;
    }
    dst[3] = (out_a * 255.0).round() as u8;
}

// ---------------------------------------------------------------------------
// Bitmaps
// ---------------------------------------------------------------------------

/// Resolves a template-relative path such as `..\Resources\Backgrounds\bg.jpg`.
pub fn resolve_resource(template_dir: &Path, value: &str) -> PathBuf {
    let normalized: String = value
        .chars()
        .map(|c| if c == '\\' || c == '/' { std::path::MAIN_SEPARATOR } else { c })
        .collect();
    let p = Path::new(&normalized);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        template_dir.join(p)
    }
}

/// Loads an image and fits it into `w`x`h` (`stretch` or `letterbox`).
/// Returns the image plus its offset inside the box.
fn load_fitted(path: &Path, w: u32, h: u32, padding: &str) -> Result<(RgbaImage, u32, u32), MenuError> {
    let img = image::open(path)?.to_rgba8();
    if w == 0 || h == 0 {
        return Ok((RgbaImage::new(0, 0), 0, 0));
    }
    if padding.eq_ignore_ascii_case("letterbox") {
        let f = (w as f64 / img.width() as f64).min(h as f64 / img.height() as f64);
        let nw = ((img.width() as f64 * f).round() as u32).clamp(1, w);
        let nh = ((img.height() as f64 * f).round() as u32).clamp(1, h);
        let scaled = if (nw, nh) == img.dimensions() {
            img
        } else {
            imageops::resize(&img, nw, nh, FilterType::Lanczos3)
        };
        Ok((scaled, (w - nw) / 2, (h - nh) / 2))
    } else if img.dimensions() == (w, h) {
        Ok((img, 0, 0))
    } else {
        Ok((imageops::resize(&img, w, h, FilterType::Lanczos3), 0, 0))
    }
}

pub(crate) fn blit(dst: &mut RgbaImage, src: &RgbaImage, x: i64, y: i64, alpha: u8) {
    let (dw, dh) = dst.dimensions();
    for (sx, sy, p) in src.enumerate_pixels() {
        let (tx, ty) = (x + sx as i64, y + sy as i64);
        if tx < 0 || ty < 0 || tx >= dw as i64 || ty >= dh as i64 {
            continue;
        }
        let a = p[3] as f32 / 255.0 * alpha as f32 / 255.0;
        if a > 0.0 {
            blend_pixel(dst.get_pixel_mut(tx as u32, ty as u32), [p[0], p[1], p[2]], a);
        }
    }
}

// ---------------------------------------------------------------------------
// Page rendering
// ---------------------------------------------------------------------------

/// Result of rendering one menu page, already at the output frame size (PAL/NTSC).
pub struct RenderedPage {
    /// Still menu: the complete picture. Motion menu: only what lies below the video slots.
    pub background: RgbaImage,
    /// Motion menu: transparent layer with everything drawn above the video slots.
    pub foreground: Option<RgbaImage>,
    /// Motion menu: where the moving sample clips go (empty for a still menu).
    pub video_slots: Vec<VideoSlot>,
    pub highlight: IndexedImage,
    pub select: IndexedImage,
    /// `RenderOptions::intro`: the page's intro, if any graphic moves.
    pub intro: Option<Intro>,
    /// Page fill colour (the base the intro frames are composited on).
    pub fill: [u8; 3],
    pub warnings: Vec<String>,
}

pub use crate::i18n::UiLang;

/// Translates the fixed texts of the ConvertX templates (keys `engine.tpl.<English text>`); unknown
/// texts stay as they are.
pub fn localize(text: &str, lang: UiLang) -> &str {
    crate::i18n::tr_key(lang, &format!("engine.tpl.{}", text.trim())).unwrap_or(text)
}

pub struct RenderOptions<'a> {
    /// Directory containing the template .ini (resource paths are relative to it).
    pub template_dir: &'a Path,
    /// Replaces the template's `ML_PAGE_TITLE` text.
    pub heading: Option<&'a str>,
    /// Image for `GT_VIDEO` page backgrounds (e.g. the root menu's video window).
    pub background_video: Option<&'a Path>,
    pub ui_lang: UiLang,
    /// Draws the button rectangles into the background (layout debugging).
    pub debug_frames: bool,
    /// Motion menu: `GT_VIDEO` graphics become moving clips instead of still thumbnails.
    pub motion: bool,
    /// Motion menu clip for `GT_VIDEO` page backgrounds.
    pub background_clip: Option<&'a ClipSource>,
    /// Motion menu length; `None` uses the page's `main_duration` (25 s in the templates).
    pub motion_duration: Option<f64>,
    pub text_overflow: TextOverflow,
    /// Output frame (720x576 PAL / 720x480 NTSC); template pages are scaled to it.
    pub tv: TvStandard,
    /// Sound of this menu's motion pages (`None` = silent). The main menu is silent unless a
    /// menu audio file is set; a video's own menus use that video's sound.
    pub menu_audio: Option<&'a ClipSource>,
    /// Chapter menus: every page starts the sound at the first chapter it shows.
    pub audio_from_first_chapter: bool,
    /// Collects the page's `[MNUP_INTRO]` animation as sprites (see [`RenderedPage::intro`]).
    pub intro: bool,
}

impl<'a> RenderOptions<'a> {
    /// Still-menu options.
    pub fn still(template_dir: &'a Path) -> Self {
        RenderOptions {
            template_dir,
            heading: None,
            background_video: None,
            ui_lang: UiLang::default(),
            debug_frames: false,
            motion: false,
            background_clip: None,
            motion_duration: None,
            text_overflow: TextOverflow::default(),
            tv: TvStandard::default(),
            menu_audio: None,
            audio_from_first_chapter: false,
            intro: false,
        }
    }
}

/// Default motion menu length when neither the CLI nor the template sets one.
pub const DEFAULT_MOTION_SECS: f64 = 25.0;

/// Motion menu length for `page`: explicit value, else the template's `main_duration`, else 25 s.
pub fn motion_duration(page: &Page, explicit: Option<f64>) -> f64 {
    explicit
        .filter(|d| d.is_finite() && *d > 0.0)
        .or(Some(page.main_duration).filter(|d| *d > 0.0))
        .unwrap_or(DEFAULT_MOTION_SECS)
}

/// The layer graphics are currently drawn on: the foreground once a motion slot was placed.
fn layer<'c>(bg: &'c mut RgbaImage, fg: &'c mut Option<RgbaImage>) -> &'c mut RgbaImage {
    match fg {
        Some(f) => f,
        None => bg,
    }
}

/// Template button a graphic belongs to: same location and `param` (a button's thumbnail,
/// frame and caption share them), falling back to an identical rectangle.
pub(crate) fn button_of_graphic(page: &Page, g: &Graphic) -> Option<usize> {
    let same_loc = |b: &&crate::template::Button| b.location.eq_ignore_ascii_case(&g.location);
    page.buttons
        .iter()
        .filter(same_loc)
        .find(|b| b.param == g.param)
        .or_else(|| {
            page.buttons
                .iter()
                .filter(same_loc)
                .find(|b| (b.x, b.y, b.width, b.height) == (g.x, g.y, g.width, g.height))
        })
        .map(|b| b.index)
}

/// Renders `page` with only `buttons` visible (see [`author::plan_buttons`]).
pub fn render_page(
    template: &Template,
    page: &Page,
    buttons: &[MenuButton],
    opts: &RenderOptions,
) -> Result<RenderedPage, MenuError> {
    let (pw, ph) = (page.width.max(1), page.height.max(1));
    let mut warnings = Vec::new();

    let fill = page.fill_color.as_ref().and_then(DelphiColor::rgba).unwrap_or([0, 0, 0, 255]);
    let mut bg = RgbaImage::from_pixel(pw, ph, Rgba([fill[0], fill[1], fill[2], 255]));

    // Text coverage per template button, reused for the highlight masks.
    let mut button_text: HashMap<usize, (Coverage, Coverage)> = HashMap::new();
    // Bitmap per template button (image + position), fallback mask source.
    let mut button_bitmap: HashMap<usize, (RgbaImage, i64, i64)> = HashMap::new();
    // Motion menu: everything after the first video slot is drawn onto a transparent layer
    // that ffmpeg puts above the clips (frames, captions and icons stay on top).
    let mut fg: Option<RgbaImage> = None;
    let mut slots: Vec<VideoSlot> = Vec::new();
    // Output frame (PAL 720x576 / NTSC 720x480); pages of another size are scaled to it.
    let (out_w, out_h) = opts.tv.size();
    let (sx, sy) = (out_w as f64 / pw as f64, out_h as f64 / ph as f64);
    let scale_slot = |s: &VideoSlot| VideoSlot {
        x: (s.x as f64 * sx).round() as u32,
        y: (s.y as f64 * sy).round() as u32,
        width: ((s.width as f64 * sx).round() as u32).max(2),
        height: ((s.height as f64 * sy).round() as u32).max(2),
        ..s.clone()
    };
    let to_frame_box = |x: f64, y: f64, w: f64, h: f64| (x * sx, y * sy, w * sx, h * sy);
    let mut sprites: Vec<Sprite> = Vec::new();

    for g in &page.graphics {
        let owner = button_of_graphic(page, g);
        let visible_button = owner.and_then(|o| buttons.iter().find(|b| b.template_index == o));
        if owner.is_some() && visible_button.is_none() {
            continue; // hidden button (no title / no next page)
        }
        if g.width == 0 || g.height == 0 {
            continue;
        }
        // Intro: the graphic is drawn a second time, unfaded, alone on a transparent page.
        let mut scratch = opts.intro.then(|| RgbaImage::new(pw, ph));
        macro_rules! paint {
            ($f:expr) => {{
                let f = $f;
                f(layer(&mut bg, &mut fg), g.alpha);
                if let Some(sc) = scratch.as_mut() {
                    f(sc, 255);
                }
            }};
        }
        let video_source = match visible_button {
            Some(b) => b.thumbnail.as_deref(),
            None if g.location.eq_ignore_ascii_case("ML_PAGE_BACKGROUND") => opts.background_video,
            None => None,
        };
        let clip_source = match visible_button {
            Some(b) => b.clip.as_ref(),
            None if g.location.eq_ignore_ascii_case("ML_PAGE_BACKGROUND") => opts.background_clip,
            None => None,
        };
        match g.gfx_type.to_ascii_uppercase().as_str() {
            "GT_VIDEO" if opts.motion && clip_source.is_some() => {
                slots.push(VideoSlot {
                    x: g.x.max(0) as u32,
                    y: g.y.max(0) as u32,
                    width: g.width,
                    height: g.height,
                    letterbox: g.padding.eq_ignore_ascii_case("letterbox"),
                    clip: clip_source.cloned().expect("checked above"),
                });
                fg.get_or_insert_with(|| RgbaImage::new(pw, ph));
                if opts.intro {
                    let fs = scale_slot(slots.last().expect("just pushed"));
                    sprites.push(Sprite {
                        pixels: Pixels::Slot { index: slots.len() - 1, width: fs.width, height: fs.height },
                        rect: (fs.x as f64, fs.y as f64, fs.width as f64, fs.height as f64),
                        opacity: g.alpha as f32 / 255.0,
                        keys: menu_anim::graphic_keys(g, (g.x as f64, g.y as f64, g.width as f64, g.height as f64), to_frame_box),
                    });
                }
            }
            "GT_VIDEO" => {
                let Some(path) = video_source else { continue };
                match load_fitted(path, g.width, g.height, &g.padding) {
                    Ok((img, ox, oy)) => {
                        let (x, y) = (g.x as i64 + ox as i64, g.y as i64 + oy as i64);
                        paint!(|d: &mut RgbaImage, a: u8| blit(d, &img, x, y, a));
                    }
                    Err(e) => warnings.push(format!("{}: {e}", path.display())),
                }
            }
            "GT_BITMAP" if !g.value.is_empty() => {
                let path = resolve_resource(opts.template_dir, &g.value);
                match load_fitted(&path, g.width, g.height, &g.padding) {
                    Ok((img, ox, oy)) => {
                        let (x, y) = (g.x as i64 + ox as i64, g.y as i64 + oy as i64);
                        paint!(|d: &mut RgbaImage, a: u8| blit(d, &img, x, y, a));
                        if let Some(o) = owner {
                            button_bitmap.insert(o, (img, x, y));
                        }
                    }
                    Err(e) => warnings.push(format!("{}: {e}", path.display())),
                }
            }
            "GT_TEXT" => {
                let text = match (visible_button.and_then(|b| b.label.as_deref()), g.location.as_str()) {
                    (Some(label), _) => label,
                    (None, loc) if loc.eq_ignore_ascii_case("ML_PAGE_TITLE") => {
                        opts.heading.unwrap_or(&g.value)
                    }
                    (None, _) => localize(&g.value, opts.ui_lang),
                };
                if text.trim().is_empty() {
                    continue;
                }
                let fallback;
                let style = match template.text_style(&g.style) {
                    Some(s) => s,
                    None => {
                        fallback = fallback_style(g.height);
                        &fallback
                    }
                };
                let fonts = font_chain(style);
                if fonts.is_empty() {
                    warnings.push(format!("no font found for '{}', text '{text}' skipped", style.font_name));
                    continue;
                }
                let cov = rasterize_text(&fonts, style, text, g, (pw, ph), opts.text_overflow);
                let outline = cov.dilate(style.outline_size);
                if let Some(shadow) = style.shadow.as_ref().and_then(DelphiColor::rgba) {
                    if style.shadow_offset != 0 {
                        let (dx, dy) = shadow_offset(style);
                        paint!(|d: &mut RgbaImage, a: u8| blend_coverage(d, &outline, shadow, dx, dy, a));
                    }
                }
                if style.outline_size > 0 {
                    if let Some(oc) = style.outline.as_ref().and_then(DelphiColor::rgba) {
                        paint!(|d: &mut RgbaImage, a: u8| blend_coverage(d, &outline, oc, 0, 0, a));
                    }
                }
                let tc = style.text.as_ref().and_then(DelphiColor::rgba).unwrap_or([255, 255, 255, 255]);
                paint!(|d: &mut RgbaImage, a: u8| blend_coverage(d, &cov, tc, 0, 0, a));
                if let Some(o) = owner {
                    button_text.insert(o, (cov, outline));
                }
            }
            _ => {}
        }
        if let Some(sprite) = scratch.and_then(|sc| intro_sprite(&sc, g, (sx, sy))) {
            sprites.push(sprite);
        }
    }

    // Subpicture masks in page coordinates.
    let scheme: Option<&ColorScheme> = buttons
        .iter()
        .filter_map(|mb| page.buttons.iter().find(|b| b.index == mb.template_index))
        .find_map(|b| page.color_scheme_of(b));
    let (sel_pal, act_pal) = match scheme {
        Some(s) => (scheme_palette(&s.selection, DEFAULT_SELECTION), scheme_palette(&s.action, DEFAULT_ACTION)),
        None => (DEFAULT_SELECTION, DEFAULT_ACTION),
    };
    let mut mask = IndexedImage::new(pw, ph, sel_pal);

    for mb in buttons {
        let Some(tb) = page.buttons.iter().find(|b| b.index == mb.template_index) else { continue };
        let x0 = tb.x.max(0) as u32;
        let y0 = tb.y.max(0) as u32;
        let x1 = (tb.x + tb.width as i32).clamp(0, pw as i32) as u32;
        let y1 = (tb.y + tb.height as i32).clamp(0, ph as i32) as u32;

        if let Some((text, outline)) = button_text.get(&tb.index) {
            let mut lit = false;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * pw + x) as usize;
                    if text.data[i] >= 0.5 {
                        mask.set(x, y, IDX_PATTERN);
                        lit = true;
                    } else if outline.data[i] >= 0.5 {
                        mask.set(x, y, IDX_EMPHASIS1);
                        lit = true;
                    }
                }
            }
            // An icon button whose caption lies outside its rectangle (Metallic root menu) has no
            // text pixels in the rectangle; real players then show no highlight at all. Fall
            // through to the icon / rectangle mask below.
            if lit {
                continue;
            }
        }

        // Image button: overlay GIF (template `value`) > bitmap alpha > full rectangle.
        let overlay = Some(tb.value.as_str())
            .filter(|v| !v.eq_ignore_ascii_case("TEXT") && v.contains('.'))
            .map(|v| resolve_resource(opts.template_dir, v))
            .filter(|p| p.is_file());
        let padding = page
            .graphics
            .iter()
            .find(|g| button_of_graphic(page, g) == Some(tb.index))
            .map_or("stretch", |g| g.padding.as_str());
        let overlay_img = match overlay {
            Some(p) => match load_fitted(&p, tb.width, tb.height, padding) {
                Ok((img, ox, oy)) => Some((img, tb.x as i64 + ox as i64, tb.y as i64 + oy as i64, true)),
                Err(e) => {
                    warnings.push(format!("{}: {e}", p.display()));
                    None
                }
            },
            None => None,
        };
        let source = overlay_img.or_else(|| button_bitmap.get(&tb.index).map(|(i, x, y)| (i.clone(), *x, *y, false)));
        match source {
            Some((img, ix, iy, is_overlay)) => {
                let mut lit = false;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let (sx, sy) = (x as i64 - ix, y as i64 - iy);
                        if sx < 0 || sy < 0 || sx >= img.width() as i64 || sy >= img.height() as i64 {
                            continue;
                        }
                        let p = img.get_pixel(sx as u32, sy as u32);
                        let on = if is_overlay {
                            // Overlay GIFs are grey masks on black; transparent = off.
                            p[3] >= 128 && (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3 > 16
                        } else {
                            p[3] >= 128
                        };
                        if on {
                            mask.set(x, y, IDX_PATTERN);
                            lit = true;
                        }
                    }
                }
                // Never leave a button without highlight pixels (nothing would light up).
                if !lit {
                    for y in y0..y1 {
                        for x in x0..x1 {
                            mask.set(x, y, IDX_PATTERN);
                        }
                    }
                }
            }
            None => {
                for y in y0..y1 {
                    for x in x0..x1 {
                        mask.set(x, y, IDX_PATTERN);
                    }
                }
            }
        }
    }

    if opts.debug_frames {
        for mb in buttons {
            if let Some(tb) = page.buttons.iter().find(|b| b.index == mb.template_index) {
                draw_frame(layer(&mut bg, &mut fg), tb.x, tb.y, tb.width, tb.height, Rgba([255, 255, 0, 255]));
            }
        }
    }

    // Scale to the output frame (PAL 720x576 / NTSC 720x480) if the page has another size.
    let to_frame = |img: RgbaImage| {
        if (pw, ph) == (out_w, out_h) {
            img
        } else {
            imageops::resize(&img, out_w, out_h, FilterType::Lanczos3)
        }
    };
    let background = to_frame(bg);
    let foreground = fg.map(to_frame);
    let video_slots: Vec<VideoSlot> = slots.iter().map(scale_slot).collect();
    let highlight = resize_nearest(&mask, out_w, out_h);
    let select = IndexedImage { palette: act_pal, ..highlight.clone() };

    // Subpicture pixels outside the spumux button rectangles are never shown; clear them so the
    // mask matches the (even-aligned) rectangles exactly.
    let mut highlight = highlight;
    let mut select = select;
    for img in [&mut highlight, &mut select] {
        for y in 0..img.height {
            for x in 0..img.width {
                if !buttons.iter().any(|b| b.rect.contains(x, y)) {
                    img.set(x, y, IDX_BACKGROUND);
                }
            }
        }
    }

    let intro = page_intro(sprites, video_slots.len(), page, opts.tv);
    Ok(RenderedPage { background, foreground, video_slots, highlight, select, intro, fill: [fill[0], fill[1], fill[2]], warnings })
}

/// The graphic drawn alone on `scratch` (page size) as a sprite on the output frame: cropped to its
/// pixels, scaled by `scale`, with the intro keys of `g` (the box it is cropped to stands in for the
/// graphic's own, so a zoom about the block corner still lands right). `None` if nothing was drawn.
fn intro_sprite(scratch: &RgbaImage, g: &Graphic, (sx, sy): (f64, f64)) -> Option<Sprite> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in scratch.enumerate_pixels() {
        if p[3] > 0 {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let (fw, fh) = (((w as f64 * sx).round() as u32).max(1), ((h as f64 * sy).round() as u32).max(1));
    let crop = imageops::crop_imm(scratch, x0, y0, w, h).to_image();
    let pixels = if (fw, fh) == (w, h) { crop } else { imageops::resize(&crop, fw, fh, FilterType::Lanczos3) };
    let page_box = (x0 as f64, y0 as f64, w as f64, h as f64);
    let map = |x: f64, y: f64, w: f64, h: f64| (x * sx, y * sy, w * sx, h * sy);
    Some(Sprite {
        pixels: Pixels::Image(pixels),
        rect: (x0 as f64 * sx, y0 as f64 * sy, fw as f64, fh as f64),
        opacity: g.alpha as f32 / 255.0,
        keys: menu_anim::graphic_keys(g, page_box, map),
    })
}

/// The intro of a page, if some sprite moves: whole GOPs long, `slots` slot streams after the background.
fn page_intro(sprites: Vec<Sprite>, slots: usize, page: &Page, tv: TvStandard) -> Option<Intro> {
    let end = sprites.iter().flat_map(|s| s.keys.iter().map(|k| k.t)).reduce(f64::max)?;
    let rate = menu_anim::dvd_rate(tv);
    let frames = menu_anim::dvd_intro_frames(menu_anim::intro_length(end, page.intro_duration), tv);
    Some(Intro {
        frames,
        secs: frames as f64 * rate.fps.1 as f64 / rate.fps.0 as f64,
        ticks: menu_anim::intro_ticks(frames, rate),
        end,
        sprites,
        slots: (0..slots).collect(),
    })
}

fn resize_nearest(img: &IndexedImage, w: u32, h: u32) -> IndexedImage {
    if (img.width, img.height) == (w, h) {
        return img.clone();
    }
    let mut out = IndexedImage::new(w, h, img.palette);
    for y in 0..h {
        for x in 0..w {
            let sx = (x as u64 * img.width as u64 / w as u64) as u32;
            let sy = (y as u64 * img.height as u64 / h as u64) as u32;
            out.set(x, y, img.get(sx, sy));
        }
    }
    out
}

fn draw_frame(img: &mut RgbaImage, x: i32, y: i32, w: u32, h: u32, color: Rgba<u8>) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let mut put = |px: i32, py: i32| {
        if px >= 0 && py >= 0 && px < iw && py < ih {
            img.put_pixel(px as u32, py as u32, color);
        }
    };
    let (x1, y1) = (x + w as i32 - 1, y + h as i32 - 1);
    for px in x..=x1 {
        put(px, y);
        put(px, y1);
    }
    for py in y..=y1 {
        put(x, py);
        put(x1, py);
    }
}

// ---------------------------------------------------------------------------
// Rendering whole menus (all pages) to files
// ---------------------------------------------------------------------------

/// File suffix for menu page `n` (1-based): page 1 has none, so a single-page menu yields
/// exactly `menu_bg.png`, `menu_highlight.png`, ...
pub fn page_suffix(n: u32) -> String {
    if n == 1 { String::new() } else { format!("_{n}") }
}

/// Files of one rendered menu page, all inside the output directory.
#[derive(Debug, Clone)]
pub struct MenuPageFiles {
    pub background: PathBuf,
    pub highlight: PathBuf,
    pub select: PathBuf,
    /// spumux XML, referencing the PNGs by file name.
    pub spumux_xml: PathBuf,
    /// File name of the final (subpicture-muxed) menu MPEG, relative to the output dir.
    pub vob: String,
    /// Motion menu: transparent layer above the clips (`{prefix}_fg{s}.png`).
    pub foreground: Option<PathBuf>,
    /// Motion menu: the sample clips (empty for a still menu).
    pub video_slots: Vec<VideoSlot>,
    /// Motion menu loop length in seconds (`None` = still menu).
    pub motion_secs: Option<f64>,
    /// Sound of the motion loop (`None` = silence).
    pub audio: Option<ClipSource>,
    /// Custom (menu editor) page: background and layers for [`crate::encoder::custom_menu_args`];
    /// `background`/`foreground`/`video_slots` are unused then.
    pub custom: Option<crate::encoder::CustomMenuMedia>,
    /// Template intro (`RenderOptions::intro`): `motion_secs` then covers intro + page, and the
    /// first `secs` come from composited frames instead of the page files.
    pub intro: Option<Intro>,
    /// Page fill colour under the intro's sprites.
    pub intro_fill: [u8; 3],
}

impl MenuPageFiles {
    /// Encoded as a looping video: it has moving clips and/or its own sound.
    pub fn is_motion(&self) -> bool {
        self.motion_secs.is_some()
    }
}

/// A rendered menu with all its pages.
#[derive(Debug, Clone)]
pub struct RenderedMenu {
    pub pages: Vec<(MenuSpec, MenuPageFiles)>,
    pub warnings: Vec<String>,
}

/// Renders every page of a menu and writes `{prefix}_bg{s}.png`, `{prefix}_highlight{s}.png`,
/// `{prefix}_select{s}.png` and `spumux_{prefix}{s}.xml`; the menu MPEG is `{prefix}{s}.mpg`.
/// Motion pages (video slots with clips and `opts.motion`) additionally get `{prefix}_fg{s}.png`;
/// their `_bg` then holds only the layers below the clips.
/// `entry` is set on the first page's [`MenuSpec`].
#[allow(clippy::too_many_arguments)]
pub fn render_menu(
    template: &Template,
    page: &Page,
    kind: &MenuKind,
    links: MenuLinks,
    entry: Option<&str>,
    prefix: &str,
    out_dir: &Path,
    opts: &RenderOptions,
) -> Result<RenderedMenu, MenuError> {
    std::fs::create_dir_all(out_dir)?;
    // Pages are independent: render and write them in parallel, keep the page order.
    let rendered: Vec<((MenuSpec, MenuPageFiles), Vec<String>)> = (1..=author::page_count(page, kind))
        .into_par_iter()
        .map(|n| render_menu_page(template, page, kind, links, entry, prefix, out_dir, opts, n))
        .collect::<Result<_, _>>()?;
    let (pages, warnings): (Vec<_>, Vec<Vec<String>>) = rendered.into_iter().unzip();
    Ok(RenderedMenu { pages, warnings: warnings.into_iter().flatten().collect() })
}

/// Renders and writes page `n` of a menu (see [`render_menu`]).
#[allow(clippy::too_many_arguments)]
fn render_menu_page(
    template: &Template,
    page: &Page,
    kind: &MenuKind,
    links: MenuLinks,
    entry: Option<&str>,
    prefix: &str,
    out_dir: &Path,
    opts: &RenderOptions,
    n: u32,
) -> Result<((MenuSpec, MenuPageFiles), Vec<String>), MenuError> {
    let mut warnings = Vec::new();
    {
        let (out_w, out_h) = opts.tv.size();
        let (buttons, w) = author::plan_buttons(page, kind, n, links, out_w, out_h)?;
        warnings.extend(w.into_iter().map(|w| format!("{prefix} page {n}: {w}")));
        let rendered = render_page(template, page, &buttons, opts)?;
        warnings.extend(rendered.warnings.into_iter().map(|w| format!("{prefix} page {n}: {w}")));

        // Sound of this page: the menu's sound, for chapter pages from the first chapter shown.
        let audio = opts.menu_audio.filter(|_| opts.motion).map(|base| {
            let first_chapter = buttons
                .iter()
                .filter(|b| matches!(b.action, author::ButtonAction::PlayChapter(_)))
                .find_map(|b| b.clip.as_ref());
            match first_chapter {
                // The chapter's own file: chapters of merged videos come from different sources.
                Some(c) if opts.audio_from_first_chapter => c.clone(),
                _ => base.clone(),
            }
        });
        // Motion pages: moving clips, or (pages without video slots) a still image with sound.
        let motion_page = opts.motion && (!rendered.video_slots.is_empty() || audio.is_some());

        let s = page_suffix(n);
        let hl_name = format!("{prefix}_highlight{s}.png");
        let sel_name = format!("{prefix}_select{s}.png");
        let files = MenuPageFiles {
            background: out_dir.join(format!("{prefix}_bg{s}.png")),
            highlight: out_dir.join(&hl_name),
            select: out_dir.join(&sel_name),
            spumux_xml: out_dir.join(format!("spumux_{prefix}{s}.xml")),
            vob: format!("{prefix}{s}.mpg"),
            foreground: rendered.foreground.as_ref().map(|_| out_dir.join(format!("{prefix}_fg{s}.png"))),
            // An intro page is a video even as a still page (intro + 1 s held on its last frame).
            motion_secs: match &rendered.intro {
                Some(i) => Some(i.secs + if motion_page { motion_duration(page, opts.motion_duration) } else { 1.0 }),
                None => motion_page.then(|| motion_duration(page, opts.motion_duration)),
            },
            audio: audio.filter(|_| motion_page),
            video_slots: rendered.video_slots,
            custom: None,
            intro_fill: rendered.fill,
            intro: rendered.intro,
        };
        rendered.background.save(&files.background)?;
        if let (Some(fg), Some(path)) = (&rendered.foreground, &files.foreground) {
            fg.save(path)?;
        }
        rendered.highlight.write_png(&files.highlight)?;
        rendered.select.write_png(&files.select)?;
        let intro_secs = files.intro.as_ref().map(|i| i.secs);
        std::fs::write(&files.spumux_xml, author::spumux_xml(opts.tv, &hl_name, &sel_name, &buttons, intro_secs.unwrap_or(0.0)))?;
        let spec = MenuSpec {
            vob: files.vob.clone(),
            buttons,
            entry: if n == 1 { entry.map(str::to_string) } else { None },
            motion: motion_page,
            pre: None,
            intro_secs,
        };
        Ok(((spec, files), warnings))
    }
}

/// First template page with the given style (`MNUS_TITLE`, `MNUS_ROOT`, ...).
pub fn page_by_style<'t>(template: &'t Template, style: &str) -> Option<&'t Page> {
    template.pages.iter().find(|p| p.style.eq_ignore_ascii_case(style))
}

pub struct MenuSetOptions<'a> {
    pub template_path: &'a Path,
    /// Template page to use (index into `Template::pages`); default: first `MNUS_TITLE` page.
    pub page_index: Option<usize>,
    pub titles: Vec<TitleSpec>,
    pub menu_title: Option<&'a str>,
    pub output_dir: &'a Path,
    pub debug_frames: bool,
}

#[derive(Debug)]
pub struct MenuSetSummary {
    pub page_index: usize,
    pub files: Vec<PathBuf>,
    pub menus: Vec<MenuSpec>,
    pub warnings: Vec<String>,
}

/// Renders the main menu pages for `titles` (no sub menus) and writes the PNGs, the spumux XMLs,
/// `dvdauthor_menu.xml` and a `build_dvd.cmd` into `output_dir`.
pub fn render_menu_set(opts: &MenuSetOptions) -> Result<MenuSetSummary, MenuError> {
    let template = Template::load(opts.template_path)
        .map_err(|e| MenuError::NoPage(format!("{}: {e}", opts.template_path.display())))?;
    let template_dir = opts.template_path.parent().unwrap_or(Path::new("."));

    let page = match opts.page_index {
        Some(i) => template.pages.iter().find(|p| p.index == i),
        None => page_by_style(&template, "MNUS_TITLE").or(template.pages.first()),
    }
    .ok_or_else(|| MenuError::NoPage("template has no such page".into()))?;

    let titles = if opts.titles.is_empty() {
        (1..=author::items_per_page(page).max(1)).map(TitleSpec::placeholder).collect()
    } else {
        opts.titles.clone()
    };
    let entries: Vec<TitleEntry> = titles
        .iter()
        .map(|t| TitleEntry { label: t.label.clone(), thumbnail: None, clip: None, titleset_menu: None })
        .collect();

    let render_opts = RenderOptions {
        heading: opts.menu_title,
        debug_frames: opts.debug_frames,
        ..RenderOptions::still(template_dir)
    };
    let links = MenuLinks { first: 1, ..MenuLinks::default() };
    let rendered = render_menu(
        &template,
        page,
        &MenuKind::Titles(&entries),
        links,
        Some("title"),
        "menu",
        opts.output_dir,
        &render_opts,
    )?;

    let mut files = Vec::new();
    let mut menus = Vec::new();
    for (spec, f) in rendered.pages {
        files.extend([f.background, f.highlight, f.select, f.spumux_xml]);
        menus.push(spec);
    }

    let layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: "4:3".into(),
        vmgm_menus: menus.clone(),
        playback: Default::default(),
        tv: TvStandard::Pal,
        title_pre: None,
        titlesets: titles
            .into_iter()
            .enumerate()
            .map(|(i, title)| TitlesetSpec { menus: vec![], title, main_menu_page: author::title_page_of(page, i) })
            .collect(),
    };
    let xml_path = opts.output_dir.join("dvdauthor_menu.xml");
    std::fs::write(&xml_path, author::dvdauthor_xml(&layout)?)?;
    files.push(xml_path);

    let script_path = opts.output_dir.join("build_dvd.cmd");
    let vobs: Vec<&str> = menus.iter().map(|m| m.vob.as_str()).collect();
    std::fs::write(&script_path, build_script(&vobs))?;
    files.push(script_path);

    Ok(MenuSetSummary { page_index: page.index, files, menus, warnings: rendered.warnings })
}

/// Windows batch script: still image -> MPEG-2 (ffmpeg), subpicture mux (spumux), dvdauthor.
fn build_script(menu_vobs: &[&str]) -> String {
    let mut s = String::from(
        "@echo off\r\nrem Requires ffmpeg, spumux and dvdauthor on PATH. Run from this directory.\r\n\
         setlocal\r\ncd /d \"%~dp0\"\r\n",
    );
    for vob in menu_vobs {
        let stem = vob.trim_end_matches(".mpg");
        let x = stem.strip_prefix("menu").unwrap_or("");
        s.push_str(&format!(
            "ffmpeg -y -loop 1 -i menu_bg{x}.png -f lavfi -i anullsrc=r=48000:cl=stereo -t 1 \
             -target pal-dvd -aspect 4:3 -c:a ac3 -b:a 192k {stem}_raw.mpg || exit /b 1\r\n\
             spumux -m dvd spumux_menu{x}.xml < {stem}_raw.mpg > {vob} || exit /b 1\r\n"
        ));
    }
    s.push_str("dvdauthor -x dvdauthor_menu.xml || exit /b 1\r\necho DVD structure written to DVD\\VIDEO_TS\r\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_png_roundtrip() {
        let dir = std::env::temp_dir().join(format!("cx_menu_png_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.png");
        let mut img = IndexedImage::new(4, 2, DEFAULT_SELECTION);
        img.set(1, 1, IDX_PATTERN);
        img.write_png(&path).unwrap();

        let dec = png::Decoder::new(std::io::BufReader::new(File::open(&path).unwrap()));
        let reader = dec.read_info().unwrap();
        let info = reader.info();
        assert_eq!(info.color_type, png::ColorType::Indexed);
        assert_eq!(info.palette.as_ref().unwrap().len(), 12);
        assert_eq!(info.trns.as_ref().unwrap().as_ref(), &[0, 200, 150, 80]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resource_paths() {
        let p = resolve_resource(Path::new("T"), r"..\Resources\bg.jpg");
        assert_eq!(p, Path::new("T").join("..").join("Resources").join("bg.jpg"));
    }

    fn rounded_style() -> TextStyle {
        TextStyle { font_name: "Arial Rounded MT Bold".into(), size_px: 27.0, ..fallback_style(40) }
    }

    /// Regression: Cyrillic titles rendered as boxes, "Arial Rounded MT Bold" has no Cyrillic.
    #[test]
    fn cyrillic_uses_fallback_font() {
        let fonts = font_chain(&rounded_style());
        if fonts.len() < 2 {
            eprintln!("skipping: template or fallback fonts not installed");
            return;
        }
        let placed = place_chars(&fonts, "Война A");
        assert!(placed.iter().all(|p| p.id.0 != 0), "every character has a real glyph");
        assert!(placed[0].font > 0, "Cyrillic comes from a fallback font");
        assert_eq!(placed[6].font, 0, "Latin stays in the template font");
    }

    /// CJK titles (Chinese, Japanese, Korean) fall back to system CJK fonts.
    #[test]
    fn cjk_uses_fallback_font() {
        let fonts = font_chain(&rounded_style());
        let placed = place_chars(&fonts, "中文 日本語 한국어");
        if fonts.len() > 1 {
            assert!(placed[0].id.0 != 0, "CJK character '中' has a resolved glyph");
        }
    }

    #[test]
    fn long_text_is_shrunk_then_ellipsized() {
        let fonts = font_chain(&rounded_style());
        if fonts.is_empty() {
            eprintln!("skipping: no fonts installed");
            return;
        }
        // Fits after shrinking: no ellipsis, at least 70 % size.
        let (placed, em) = fit_text(&fonts, "Episode 02", 27.0, text_width(&fonts, &place_chars(&fonts, "Episode 02"), 27.0) * 0.8, MIN_TEXT_SCALE);
        assert_eq!(placed.len(), 10);
        assert!(em >= 27.0 * MIN_TEXT_SCALE - 1e-3 && em < 27.0);

        let long = "Очень длинное название фильма про приключения в Санкт-Петербурге и Москве 2025 01";
        let (placed, em) = fit_text(&fonts, long, 27.0, 548.0, MIN_TEXT_SCALE);
        assert!((em - 27.0 * MIN_TEXT_SCALE).abs() < 1e-3, "shrunk only down to the minimum");
        assert!(placed.len() < long.chars().count(), "cut off");
        let ellipsis = place_chars(&fonts, "…")[0];
        assert_eq!(placed.last().unwrap().id, ellipsis.id);
        assert!(text_width(&fonts, &placed, em) <= 548.5);

        // "truncate": full size, cut off right away; "reduce_font_size": smaller, keeps more text.
        let (cut, em_cut) = fit_text(&fonts, long, 27.0, 548.0, TextOverflow::Truncate.min_scale());
        assert_eq!(em_cut, 27.0);
        let (reduced, em_red) = fit_text(&fonts, long, 27.0, 548.0, TextOverflow::ReduceFontSize.min_scale());
        assert!((em_red - 13.5).abs() < 1e-3);
        assert!(reduced.len() > placed.len() && placed.len() > cut.len());
    }

    #[test]
    fn suffixes() {
        assert_eq!(page_suffix(1), "");
        assert_eq!(page_suffix(3), "_3");
    }
}
