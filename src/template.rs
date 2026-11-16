//! Parser for ConvertX Delphi-style menu template INIs (`Templates\<Style>\*.ini`).
//!
//! Format quirks handled here:
//! - UTF-8 with BOM, CRLF line endings
//! - Delphi `TIniFile` semantics: section and key names are case-insensitive
//! - comma as decimal separator (`TimeStamp=2,999`)
//! - nested collections flattened into section paths
//!   (`[CXT\pages\Item0\buttons\Item3]`, each collection with a `Count=` key)
//! - colors as `#RRGGBB`, Delphi color names (`Black`) and an optional alpha (`#CCFF00@200`)

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use regex::Regex;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum TemplateError {
    Io(std::io::Error),
    MissingSection(String),
    MissingKey { section: String, key: String },
    InvalidValue { section: String, key: String, value: String },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::Io(e) => write!(f, "I/O error: {e}"),
            TemplateError::MissingSection(s) => write!(f, "missing section [{s}]"),
            TemplateError::MissingKey { section, key } => {
                write!(f, "missing key '{key}' in [{section}]")
            }
            TemplateError::InvalidValue { section, key, value } => {
                write!(f, "invalid value '{value}' for '{key}' in [{section}]")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

impl From<std::io::Error> for TemplateError {
    fn from(e: std::io::Error) -> Self {
        TemplateError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Value helpers (Delphi conventions)
// ---------------------------------------------------------------------------

/// Parses a Delphi float that may use `,` as decimal separator (`2,999` -> 2.999).
pub fn parse_delphi_float(s: &str) -> Option<f64> {
    s.trim().replace(',', ".").parse().ok()
}

/// Parses Delphi booleans (`TRUE`/`FALSE`, also `1`/`0`).
pub fn parse_delphi_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_uppercase().as_str() {
        "TRUE" | "1" => Some(true),
        "FALSE" | "0" => Some(false),
        _ => None,
    }
}

/// A template color: `#RRGGBB` or a Delphi color name, with optional `@alpha` (0..=255).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DelphiColor {
    /// Original textual value without the alpha suffix (e.g. `#CCFF00`, `Black`).
    pub name: String,
    /// Resolved RGB, `None` for color names we do not know.
    pub rgb: Option<(u8, u8, u8)>,
    /// Alpha, 255 (opaque) when no `@` suffix is present.
    pub alpha: u8,
}

impl DelphiColor {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (name, alpha) = match s.split_once('@') {
            Some((n, a)) => (n.trim(), a.trim().parse::<u8>().ok()?),
            None => (s, 255),
        };
        let rgb = if let Some(hex) = name.strip_prefix('#') {
            if hex.len() != 6 {
                return None;
            }
            let v = u32::from_str_radix(hex, 16).ok()?;
            Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
        } else {
            named_color(name)
        };
        Some(DelphiColor { name: name.to_string(), rgb, alpha })
    }

    /// RGBA, or `None` if the color name is unknown.
    pub fn rgba(&self) -> Option<[u8; 4]> {
        self.rgb.map(|(r, g, b)| [r, g, b, self.alpha])
    }
}

fn named_color(name: &str) -> Option<(u8, u8, u8)> {
    let n = name.to_ascii_lowercase();
    let n = n.strip_prefix("cl").unwrap_or(&n);
    Some(match n {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "lime" => (0, 255, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "aqua" => (0, 255, 255),
        "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "navy" => (0, 0, 128),
        "olive" => (128, 128, 0),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Raw INI document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct IniSection {
    /// Section name as written in the file (original case).
    pub name: String,
    entries: Vec<(String, String)>,
}

impl IniSection {
    /// Case-insensitive key lookup; the first occurrence wins (like `TIniFile.ReadString`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// A Delphi INI file, sections in file order with case-insensitive lookup.
#[derive(Debug, Clone, Default)]
pub struct IniDocument {
    sections: Vec<IniSection>,
    index: HashMap<String, usize>,
}

impl IniDocument {
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut doc = IniDocument::default();
        let mut current: Option<usize> = None;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let name = line[1..line.len() - 1].trim().to_string();
                let key = name.to_ascii_lowercase();
                // Duplicate sections merge into the first one, as TIniFile does.
                let idx = *doc.index.entry(key).or_insert_with(|| {
                    doc.sections.push(IniSection { name, entries: Vec::new() });
                    doc.sections.len() - 1
                });
                current = Some(idx);
                continue;
            }
            if let (Some(idx), Some((k, v))) = (current, line.split_once('=')) {
                doc.sections[idx].entries.push((k.trim().to_string(), v.to_string()));
            }
        }
        doc
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, TemplateError> {
        let bytes = std::fs::read(path)?;
        Ok(Self::parse(&String::from_utf8_lossy(&bytes)))
    }

    pub fn section(&self, name: &str) -> Option<&IniSection> {
        self.index.get(&name.to_ascii_lowercase()).map(|&i| &self.sections[i])
    }

    pub fn sections(&self) -> impl Iterator<Item = &IniSection> {
        self.sections.iter()
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?.get(key)
    }

    /// Indices `N` of all `<prefix>\ItemN` items, sorted ascending and deduplicated.
    ///
    /// Enumerates the sections actually present instead of trusting the `Count=` key,
    /// so hand-edited templates with stale counts still parse. An item counts as present
    /// if either `[<prefix>\ItemN]` or any `[<prefix>\ItemN\...]` sub-section exists.
    pub fn item_indices(&self, prefix: &str) -> Vec<usize> {
        let re = Regex::new(&format!(r"(?i)^{}\\Item(\d+)(?:\\|$)", regex::escape(prefix)))
            .expect("valid item regex");
        let mut out: Vec<usize> = self
            .sections
            .iter()
            .filter_map(|s| re.captures(&s.name)?.get(1)?.as_str().parse().ok())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

// ---------------------------------------------------------------------------
// Typed template model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Template {
    pub template_version: String,
    pub kind: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub thumbnail: Option<String>,
    pub titles_per_page: Option<u32>,
    pub chapters_per_page: Option<u32>,
    /// Text styles (`[CXT\TypeSettings\ItemN]`), referenced by [`Graphic::style`].
    pub text_styles: Vec<TextStyle>,
    pub pages: Vec<Page>,
}

/// A text style from `[CXT\TypeSettings\ItemN]` (+ `\Font`, `\Font\style`).
#[derive(Debug, Clone, Serialize)]
pub struct TextStyle {
    /// Full style key, e.g. `(PageStyle)MNUS_TITLE(PageStyle)(GfxType)GT_TEXT...`.
    pub name: String,
    pub font_name: String,
    pub font_file: Option<String>,
    /// Em height in pixels (`|Font\Height|`, falling back to `Font\size`).
    pub size_px: f32,
    pub bold: bool,
    pub italic: bool,
    pub text: Option<DelphiColor>,
    pub outline: Option<DelphiColor>,
    pub shadow: Option<DelphiColor>,
    pub outline_size: u32,
    pub shadow_offset: i32,
    pub shadow_align: String,
    /// e.g. `TOP_CENTER`, `MIDDLE_LEFT`.
    pub align: String,
    pub left_margin: i32,
    pub right_margin: i32,
    pub vert_margin: i32,
}

/// One graphic element of a page (`blocks\ItemN\Gfx\ItemM`), in page coordinates.
#[derive(Debug, Clone, Serialize)]
pub struct Graphic {
    pub block: usize,
    pub name: String,
    /// Image path (relative to the template dir) for `GT_BITMAP`, the text for `GT_TEXT`.
    pub value: String,
    /// `GT_BITMAP`, `GT_TEXT`, ...
    pub gfx_type: String,
    /// `ML_PAGE_BACKGROUND`, `ML_PLAY_EPISODE`, `ML_NEXT`, ...
    pub location: String,
    pub param: i32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub alpha: u8,
    /// Key into [`Template::text_styles`] for text graphics.
    pub style: String,
    /// `stretch` or `letterbox`.
    pub padding: String,
    /// The graphic's own `Alpha` (the block's alpha is not part of it).
    pub gfx_alpha: u8,
    /// Final box of the owning block (`Left`, `Top`, `Width`, `Height`) in page coordinates.
    pub block_rect: (i32, i32, u32, u32),
    /// Keyframes of the block's intro / outro (`blocks\ItemN\Anim\ItemK`), by time.
    pub intro: Vec<AnimKey>,
    pub outro: Vec<AnimKey>,
}

/// Value `Anim` keys use for "keep the block's own value".
const ANIM_INHERIT: i64 = 2147483647;

/// One keyframe of a block animation: the block's box and opacity `t` seconds into the phase.
/// Before the first key the value holds, between two keys it is linear.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnimKey {
    pub t: f64,
    /// Block box (inherited values are already replaced by the block's own).
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// `None` = the block's own alpha.
    pub alpha: Option<u8>,
}

impl Template {
    /// Looks up a text style by its full name.
    pub fn text_style(&self, name: &str) -> Option<&TextStyle> {
        self.text_styles.iter().find(|s| s.name == name)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub index: usize,
    pub name: String,
    /// Menu style, e.g. `MNUS_TITLE`, `MNUS_CHAPTER`, `MNUS_ROOT`.
    pub style: String,
    pub width: u32,
    pub height: u32,
    pub intro_duration: f64,
    pub main_duration: f64,
    pub outro_duration: f64,
    pub fill_color: Option<DelphiColor>,
    /// Background image/video paths (gfx with `Location=ML_PAGE_BACKGROUND`), relative to the template dir.
    pub backgrounds: Vec<String>,
    /// All graphics in drawing order (block order, back to front).
    pub graphics: Vec<Graphic>,
    pub buttons: Vec<Button>,
    pub color_schemes: Vec<ColorScheme>,
}

impl Page {
    /// The color scheme a button refers to (`color_scheme` is 1-based).
    pub fn color_scheme_of(&self, button: &Button) -> Option<&ColorScheme> {
        (button.color_scheme as usize).checked_sub(1).and_then(|i| self.color_schemes.get(i))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Button {
    pub index: usize,
    pub name: String,
    /// `TEXT` for text buttons, otherwise an overlay image path.
    pub value: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub is_default: bool,
    /// Button action/location, e.g. `ML_PLAY_EPISODE`, `ML_NEXT`, `ML_PREVIOUS`.
    pub location: String,
    /// Target page kind, e.g. `MNUP_NONE`.
    pub target: String,
    pub param: i32,
    /// 1-based index into [`Page::color_schemes`] (0 = none).
    pub color_scheme: u32,
    pub nav_h: i32,
    pub nav_v: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColorScheme {
    pub name: String,
    /// background, pattern, emphasis1, emphasis2 while highlighted.
    pub selection: [Option<DelphiColor>; 4],
    /// background, pattern, emphasis1, emphasis2 while activated.
    pub action: [Option<DelphiColor>; 4],
}

impl Template {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, TemplateError> {
        Self::from_ini(&IniDocument::load(path)?)
    }

    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        Self::from_ini(&IniDocument::parse(text))
    }

    pub fn from_ini(ini: &IniDocument) -> Result<Self, TemplateError> {
        let root = ini
            .section("CXT")
            .ok_or_else(|| TemplateError::MissingSection("CXT".into()))?;
        let loc = ini.section(r"CXT\localized_names\Item0");
        let loc_get = |k: &str| loc.and_then(|s| s.get(k)).map(str::to_string);

        let pages = ini
            .item_indices(r"CXT\pages")
            .into_iter()
            .map(|i| parse_page(ini, i))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Template {
            template_version: root.get("template_version").unwrap_or_default().to_string(),
            kind: root.get("Kind").unwrap_or_default().to_string(),
            name: loc_get("template_name"),
            description: loc_get("Description"),
            thumbnail: loc_get("Thumbnail"),
            titles_per_page: root.get("TitlesPerTitleMenuPage").and_then(|v| v.trim().parse().ok()).or_else(|| {
                let title_page = pages.iter().find(|p| p.style == "MNUS_TITLE").or_else(|| pages.first())?;
                let count = title_page.buttons.iter().filter(|b| {
                    let l = b.location.to_ascii_uppercase();
                    l == "ML_PLAY_EPISODE" || l == "ML_PLAY_SERIES_EPISODE" || l == "ML_PLAY_BONUS"
                }).count() as u32;
                (count > 0).then_some(count)
            }),
            chapters_per_page: root.get("ChaptersPerPage").and_then(|v| v.trim().parse().ok()),
            text_styles: parse_text_styles(ini),
            pages,
        })
    }
}

fn parse_text_styles(ini: &IniDocument) -> Vec<TextStyle> {
    let prefix = r"CXT\TypeSettings";
    ini.item_indices(prefix)
        .into_iter()
        .filter_map(|i| {
            let base = format!(r"{prefix}\Item{i}");
            let sec = ini.section(&base)?;
            let font = ini.section(&format!(r"{base}\Font"));
            let font_style = ini.section(&format!(r"{base}\Font\style"));
            let font_get = |k: &str| font.and_then(|f| f.get(k)).map(str::trim);
            let size_px = font_get("Height")
                .and_then(|h| h.parse::<f32>().ok())
                .map(f32::abs)
                .filter(|h| *h > 0.0)
                .or_else(|| font_get("size").and_then(|s| s.parse().ok()))
                .unwrap_or(24.0);
            let style_flag = |k: &str| font_style.is_some_and(|s| opt_bool(s, k, false));
            Some(TextStyle {
                name: opt_string(sec, "Name"),
                font_name: font_get("name").unwrap_or("Arial").to_string(),
                font_file: font_get("file_name").filter(|f| !f.is_empty()).map(str::to_string),
                size_px,
                bold: style_flag("fsBold"),
                italic: style_flag("fsItalic"),
                text: sec.get("text").and_then(DelphiColor::parse),
                outline: sec.get("outline").and_then(DelphiColor::parse),
                shadow: sec.get("shadow").and_then(DelphiColor::parse),
                outline_size: opt_num(sec, "OutlineSize", 0),
                shadow_offset: opt_num(sec, "ShadowOffset", 0),
                shadow_align: opt_string(sec, "ShadowAlign"),
                align: opt_string(sec, "Align"),
                left_margin: opt_num(sec, "LeftMargin", 0),
                right_margin: opt_num(sec, "RightMargin", 0),
                vert_margin: opt_num(sec, "VertMargin", 0),
            })
        })
        .collect()
}

fn required<'a>(sec: &'a IniSection, key: &str) -> Result<&'a str, TemplateError> {
    sec.get(key).ok_or_else(|| TemplateError::MissingKey {
        section: sec.name.clone(),
        key: key.to_string(),
    })
}

fn required_num<T: std::str::FromStr>(sec: &IniSection, key: &str) -> Result<T, TemplateError> {
    let v = required(sec, key)?;
    v.trim().parse().map_err(|_| TemplateError::InvalidValue {
        section: sec.name.clone(),
        key: key.to_string(),
        value: v.to_string(),
    })
}

fn opt_num<T: std::str::FromStr>(sec: &IniSection, key: &str, default: T) -> T {
    sec.get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn opt_float(sec: &IniSection, key: &str) -> f64 {
    sec.get(key).and_then(parse_delphi_float).unwrap_or(0.0)
}

fn opt_bool(sec: &IniSection, key: &str, default: bool) -> bool {
    sec.get(key).and_then(parse_delphi_bool).unwrap_or(default)
}

fn opt_string(sec: &IniSection, key: &str) -> String {
    sec.get(key).unwrap_or_default().to_string()
}

fn parse_page(ini: &IniDocument, i: usize) -> Result<Page, TemplateError> {
    let path = format!(r"CXT\pages\Item{i}");
    let sec = ini
        .section(&path)
        .ok_or_else(|| TemplateError::MissingSection(path.clone()))?;

    let buttons_prefix = format!(r"{path}\buttons");
    let buttons = ini
        .item_indices(&buttons_prefix)
        .into_iter()
        .filter_map(|b| ini.section(&format!(r"{buttons_prefix}\Item{b}")).map(|s| (b, s)))
        .map(|(b, s)| parse_button(b, s))
        .collect::<Result<Vec<_>, _>>()?;

    let schemes_prefix = format!(r"{buttons_prefix}\color_schemes");
    let color_schemes = ini
        .item_indices(&schemes_prefix)
        .into_iter()
        .filter_map(|c| ini.section(&format!(r"{schemes_prefix}\Item{c}")))
        .map(parse_color_scheme)
        .collect();

    // Graphics live in blocks\ItemN\Gfx\ItemM; Gfx Left/Top are relative to the block.
    let blocks_prefix = format!(r"{path}\blocks");
    let mut graphics = Vec::new();
    for b in ini.item_indices(&blocks_prefix) {
        let block_path = format!(r"{blocks_prefix}\Item{b}");
        let block = ini.section(&block_path);
        let block_num = |k: &str| block.map_or(0, |s| opt_num(s, k, 0));
        let block_alpha = block.map_or(255, |s| opt_num(s, "Alpha", 255u8));
        let block_rect = (block_num("Left"), block_num("Top"), block_num("Width") as u32, block_num("Height") as u32);
        let (intro, outro) = parse_anim(ini, &block_path, block_rect);
        let gfx_prefix = format!(r"{block_path}\Gfx");
        for g in ini.item_indices(&gfx_prefix) {
            let Some(gfx) = ini.section(&format!(r"{gfx_prefix}\Item{g}")) else { continue };
            let width = opt_num(gfx, "Width", block_num("Width") as u32);
            let height = opt_num(gfx, "Height", block_num("Height") as u32);
            graphics.push(Graphic {
                block: b,
                name: opt_string(gfx, "name"),
                value: gfx.get("value").unwrap_or_default().trim().to_string(),
                gfx_type: opt_string(gfx, "gfx_type"),
                location: opt_string(gfx, "Location"),
                param: opt_num(gfx, "param", 0),
                x: block_num("Left") + opt_num(gfx, "Left", 0),
                y: block_num("Top") + opt_num(gfx, "Top", 0),
                width,
                height,
                alpha: ((block_alpha as u32 * opt_num(gfx, "Alpha", 255u8) as u32) / 255) as u8,
                style: opt_string(gfx, "Style"),
                padding: opt_string(gfx, "PaddingMethod"),
                gfx_alpha: opt_num(gfx, "Alpha", 255u8),
                block_rect,
                intro: intro.clone(),
                outro: outro.clone(),
            });
        }
    }

    let mut backgrounds: Vec<String> = Vec::new();
    for g in &graphics {
        if g.location.eq_ignore_ascii_case("ML_PAGE_BACKGROUND")
            && !g.value.is_empty()
            && !backgrounds.contains(&g.value)
        {
            backgrounds.push(g.value.clone());
        }
    }

    Ok(Page {
        index: i,
        name: opt_string(sec, "name"),
        style: opt_string(sec, "style"),
        width: required_num(sec, "Width")?,
        height: required_num(sec, "Height")?,
        intro_duration: opt_float(sec, "intro_duration"),
        main_duration: opt_float(sec, "main_duration"),
        outro_duration: opt_float(sec, "outro_duration"),
        fill_color: sec.get("FillColor").and_then(DelphiColor::parse),
        backgrounds,
        graphics,
        buttons,
        color_schemes,
    })
}

/// Intro and outro keyframes of one block, sorted by time (stable, so equal times keep file order).
fn parse_anim(ini: &IniDocument, block_path: &str, (bx, by, bw, bh): (i32, i32, u32, u32)) -> (Vec<AnimKey>, Vec<AnimKey>) {
    let prefix = format!(r"{block_path}\Anim");
    let (mut intro, mut outro) = (Vec::new(), Vec::new());
    for k in ini.item_indices(&prefix) {
        let Some(sec) = ini.section(&format!(r"{prefix}\Item{k}")) else { continue };
        let pick = |key: &str, own: i32| match sec.get(key).and_then(|v| v.trim().parse::<i64>().ok()) {
            Some(v) if v != ANIM_INHERIT => v.clamp(-100_000, 100_000) as i32,
            _ => own,
        };
        let alpha = sec.get("alpha").and_then(|v| v.trim().parse::<i64>().ok()).filter(|&a| a != ANIM_INHERIT).map(|a| a.clamp(0, 255) as u8);
        let key = AnimKey {
            t: opt_float(sec, "TimeStamp").max(0.0),
            x: pick("left", bx),
            y: pick("top", by),
            w: pick("width", bw as i32),
            h: pick("height", bh as i32),
            alpha,
        };
        match sec.get("Origin").map(|o| o.trim().to_ascii_uppercase()).as_deref() {
            Some("MNUP_INTRO") => intro.push(key),
            Some("MNUP_OUTRO") => outro.push(key),
            _ => {}
        }
    }
    intro.sort_by(|a, b| a.t.total_cmp(&b.t));
    outro.sort_by(|a, b| a.t.total_cmp(&b.t));
    (intro, outro)
}

fn parse_button(index: usize, sec: &IniSection) -> Result<Button, TemplateError> {
    Ok(Button {
        index,
        name: opt_string(sec, "name"),
        value: opt_string(sec, "value"),
        x: required_num(sec, "Left")?,
        y: required_num(sec, "Top")?,
        width: required_num(sec, "Width")?,
        height: required_num(sec, "Height")?,
        visible: opt_bool(sec, "visible", true),
        is_default: opt_bool(sec, "Default", false),
        location: opt_string(sec, "location"),
        target: opt_string(sec, "target"),
        param: opt_num(sec, "param", 0),
        color_scheme: opt_num(sec, "color_scheme", 0),
        nav_h: opt_num(sec, "navh_index", -1),
        nav_v: opt_num(sec, "navv_index", -1),
    })
}

fn parse_color_scheme(sec: &IniSection) -> ColorScheme {
    let quad = |prefix: &str| {
        ["background", "pattern", "emphasis1", "emphasis2"]
            .map(|part| sec.get(&format!("{prefix}_{part}")).and_then(DelphiColor::parse))
    };
    ColorScheme {
        name: opt_string(sec, "Name"),
        selection: quad("selection"),
        action: quad("action"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}[CXT]\r\ntemplate_version=5.2.0.12\r\nKind=mtOpenStyle\r\n\r\n\
[CXT\\pages\\Item0]\r\nname=Menu\r\nstyle=MNUS_TITLE\r\nWidth=720\r\nHeight=576\r\nintro_duration=2,999\r\n\
[cxt\\PAGES\\item0\\buttons\\Item1]\r\nLeft=10\r\nTop=20\r\nWidth=30\r\nHeight=40\r\nlocation=ML_NEXT\r\ncolor_scheme=1\r\n\
[CXT\\pages\\Item0\\buttons\\Item0]\r\nLeft=1\r\nTop=2\r\nWidth=3\r\nHeight=4\r\nlocation=ML_PLAY_EPISODE\r\n\
[CXT\\pages\\Item0\\buttons\\color_schemes\\Item0]\r\nselection_pattern=#CCFF00@200\r\naction_background=Black@0\r\n\
[CXT\\pages\\Item0\\blocks\\Item0\\Gfx\\Item0]\r\nvalue=..\\bg.jpg\r\nLocation=ML_PAGE_BACKGROUND\r\n";

    #[test]
    fn anim_keys_inherit_the_block_and_split_intro_from_outro() {
        let ini = r"[CXT]
[CXT\pages\Item0]
Width=720
Height=576
[CXT\pages\Item0\blocks\Item0]
Left=50
Top=106
Width=68
Height=54
Alpha=200
[CXT\pages\Item0\blocks\Item0\Gfx\Item0]
Left=4
Top=5
Alpha=128
gfx_type=GT_BITMAP
[CXT\pages\Item0\blocks\Item0\Anim\Item1]
Origin=MNUP_INTRO
TimeStamp=2
alpha=2147483647
left=50
top=106
height=54
width=68
[CXT\pages\Item0\blocks\Item0\Anim\Item0]
Origin=MNUP_INTRO
TimeStamp=1
alpha=0
left=84
top=133
height=0
width=0
[CXT\pages\Item0\blocks\Item0\Anim\Item2]
Origin=MNUP_OUTRO
TimeStamp=0,5
alpha=2147483647
left=2147483647
top=2147483647
height=2147483647
width=2147483647
";
        let t = Template::parse(ini).unwrap();
        let g = &t.pages[0].graphics[0];
        assert_eq!((g.gfx_alpha, g.alpha, g.block_rect), (128, 100, (50, 106, 68, 54)));
        assert_eq!(g.intro.len(), 2);
        assert_eq!(g.intro[0], AnimKey { t: 1.0, x: 84, y: 133, w: 0, h: 0, alpha: Some(0) });
        assert_eq!(g.intro[1], AnimKey { t: 2.0, x: 50, y: 106, w: 68, h: 54, alpha: None });
        assert_eq!(g.outro, [AnimKey { t: 0.5, x: 50, y: 106, w: 68, h: 54, alpha: None }]);
    }

    #[test]
    fn decimal_comma() {
        assert_eq!(parse_delphi_float("2,999"), Some(2.999));
        assert_eq!(parse_delphi_float("25"), Some(25.0));
        assert_eq!(parse_delphi_float("x"), None);
    }

    #[test]
    fn colors() {
        let c = DelphiColor::parse("#CCFF00@200").unwrap();
        assert_eq!((c.rgb, c.alpha), (Some((0xCC, 0xFF, 0x00)), 200));
        let c = DelphiColor::parse("Black").unwrap();
        assert_eq!((c.rgb, c.alpha), (Some((0, 0, 0)), 255));
        assert!(DelphiColor::parse("#12@1").is_none());
    }

    #[test]
    fn parses_sample_with_bom_and_case_insensitive_sections() {
        let t = Template::parse(SAMPLE).unwrap();
        assert_eq!(t.template_version, "5.2.0.12");
        let p = &t.pages[0];
        assert_eq!((p.width, p.height), (720, 576));
        assert!((p.intro_duration - 2.999).abs() < 1e-9);
        assert_eq!(p.buttons.len(), 2);
        assert_eq!(p.buttons[0].location, "ML_PLAY_EPISODE");
        assert_eq!((p.buttons[1].x, p.buttons[1].y, p.buttons[1].width, p.buttons[1].height), (10, 20, 30, 40));
        assert_eq!(p.color_schemes[0].selection[1].as_ref().unwrap().alpha, 200);
        assert_eq!(p.backgrounds, vec![r"..\bg.jpg".to_string()]);
    }
}
