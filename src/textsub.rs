//! Text (SRT) subtitles rendered natively into DVD subpictures: white text with a black outline,
//! bottom centred. The pictures go through the same path as decoded bitmap subtitles
//! ([`crate::subpic::render`] → [`crate::spumux`]), so no external tool is involved.
//!
//! Geometry and colours were measured on the output of dvdauthor's `spumux` for the settings the
//! encoder used to pass (`fontsize=28`, margins 60/60/20/40): em ≈ 31 px, line pitch 34 px, 2 px
//! outline, hard (non-anti-aliased) edges, last baseline 13 px above the bottom margin, wrapping at
//! the width between the side margins, `<i>`/`<b>` ignored.
//!
//! The font is bundled (DejaVu Sans, `assets/fonts/`), with system fallback fonts for CJK and
//! characters missing in DejaVu Sans. RTL scripts (Hebrew, Arabic) are reordered visually using
//! `unicode-bidi`.

use ab_glyph::{point, Font, FontArc, GlyphId, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use rayon::prelude::*;
use unicode_bidi::BidiInfo;

use crate::author::TvStandard;
use crate::subpic::{Decoded, Picture};

const EM_PX: f32 = 31.0;
const LINE_PITCH: i32 = 34;
const OUTLINE: i32 = 2;
const SIDE_MARGIN: u32 = 60;
const BOTTOM_MARGIN: u32 = 40;
/// Last baseline above the bottom margin edge (room for descenders).
const BASELINE_ABOVE_BOX: i32 = 13;
/// The XML used to pass `movie-height` two lines short of the frame ("recommended for spumux").
const FRAME_TRIM: u32 = 2;

/// User styling and sync of a title's text subtitles (Subtitles tab). Every field left at its
/// default keeps today's measured spumux geometry, so the default output stays byte-identical.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubtitleStyle {
    /// Font family (curated name or an installed system family); `None` = bundled DejaVu Sans.
    pub font_family: Option<String>,
    /// Font size in percent of the default size (50–200).
    pub font_size_pct: Option<f32>,
    /// Outline width in pixels at DVD scale (0–6; scaled like the other metrics on HD).
    pub outline_px: Option<u32>,
    /// Distance of the last baseline box from the bottom edge, in percent of the frame height (0–30).
    pub bottom_margin_pct: Option<f32>,
    /// Shifts every cue by this many milliseconds (negative = earlier) before rendering.
    pub offset_ms: i64,
    /// Han-unification region of the track being rendered ([`crate::fonts::cjk_region`]; set per
    /// track by the builders, never saved): picks Japanese / Korean / Chinese glyph forms.
    #[serde(skip)]
    pub lang: Option<String>,
}

const FILL: Rgba<u8> = Rgba([255, 255, 255, 255]);
const EDGE: Rgba<u8> = Rgba([0, 0, 0, 255]);

/// Subtitle alignment within the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubAlignment {
    #[default]
    BottomCenter,
    BottomLeft,
    BottomRight,
    TopCenter,
    TopLeft,
    TopRight,
    MiddleCenter,
    MiddleLeft,
    MiddleRight,
}

/// One character with formatting properties.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StyledChar {
    pub c: char,
    pub italic: bool,
    pub bold: bool,
    pub color: Rgba<u8>,
}

/// Parses a color string into RGBA (`#rrggbb`, `#rgb`, or color names).
fn parse_color(s: &str) -> Option<Rgba<u8>> {
    let s = s.trim().trim_matches(|c| c == '\'' || c == '"');
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Rgba([r, g, b, 255]));
        } else if hex.len() == 3 {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            return Some(Rgba([r, g, b, 255]));
        }
    }
    match s.to_ascii_lowercase().as_str() {
        "yellow" => Some(Rgba([255, 255, 0, 255])),
        "white" => Some(Rgba([255, 255, 255, 255])),
        "cyan" | "aqua" => Some(Rgba([0, 255, 255, 255])),
        "green" | "lime" => Some(Rgba([0, 255, 0, 255])),
        "magenta" | "fuchsia" => Some(Rgba([255, 0, 255, 255])),
        "red" => Some(Rgba([255, 0, 0, 255])),
        "blue" => Some(Rgba([0, 0, 255, 255])),
        "black" => Some(Rgba([0, 0, 0, 255])),
        "gray" | "grey" => Some(Rgba([128, 128, 128, 255])),
        _ => None,
    }
}

/// Parses an SSA/ASS color `&HBBGGRR&` or `&HBBGGRR`.
fn parse_ass_color(s: &str) -> Option<Rgba<u8>> {
    let s = s.trim().trim_start_matches("&H").trim_start_matches("&h").trim_end_matches('&');
    if s.len() == 6 {
        let b = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let r = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Rgba([r, g, b, 255]))
    } else {
        None
    }
}

/// Scale of font `f` for an em height of `em_px` pixels.
fn font_scale(f: &FontArc, em_px: f32) -> PxScale {
    PxScale::from(em_px * f.height_unscaled() / f.units_per_em().unwrap_or(f.height_unscaled()))
}

/// One laid-out character: which font of the chain renders it, and its glyph there.
#[derive(Clone, Copy)]
struct Placed {
    font: usize,
    id: GlyphId,
}

fn place_chars(fonts: &[FontArc], text: &str) -> Vec<Placed> {
    text.chars()
        .map(|c| {
            fonts
                .iter()
                .enumerate()
                .find_map(|(i, f)| {
                    let id = f.glyph_id(c);
                    (id.0 != 0).then_some(Placed { font: i, id })
                })
                .unwrap_or(Placed { font: 0, id: fonts[0].glyph_id(c) })
        })
        .collect()
}

/// One subtitle: seconds from the title start and its text lines (markup preserved).
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub lines: Vec<String>,
}

/// `HH:MM:SS,mmm` (comma or dot) in seconds.
fn parse_time(s: &str) -> Option<f64> {
    let s = s.split_whitespace().next()?;
    let mut parts = s.split(':');
    let (h, m, rest) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (sec, frac) = rest.split_once([',', '.']).unwrap_or((rest, "0"));
    let frac: f64 = format!("0.{frac}").parse().ok()?;
    Some(h.trim().parse::<f64>().ok()? * 3600.0 + m.trim().parse::<f64>().ok()? * 60.0 + sec.trim().parse::<f64>().ok()? + frac)
}

/// Extracts alignment tags (like `{\an8}`, `{\a6}`) from cue lines and returns the detected alignment
/// and the lines with alignment overrides stripped.
pub fn extract_alignment(lines: &[String]) -> (SubAlignment, Vec<String>) {
    let mut align = SubAlignment::BottomCenter;
    let mut out_lines = Vec::with_capacity(lines.len());
    for line in lines {
        let mut cur = String::with_capacity(line.len());
        let mut i = 0;
        let bytes = line.as_bytes();
        while i < bytes.len() {
            if bytes[i] == b'{' {
                if let Some(end) = line[i..].find('}') {
                    let tag = &line[i + 1..i + end];
                    let mut tag_rest = Vec::new();
                    for part in tag.split('\\') {
                        let trimmed = part.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match trimmed {
                            "an1" | "a1" => align = SubAlignment::BottomLeft,
                            "an2" | "a2" => align = SubAlignment::BottomCenter,
                            "an3" | "a3" => align = SubAlignment::BottomRight,
                            "an4" => align = SubAlignment::MiddleLeft,
                            "an5" => align = SubAlignment::MiddleCenter,
                            "an6" => align = SubAlignment::MiddleRight,
                            "an7" | "a9" => align = SubAlignment::TopLeft,
                            "an8" | "a6" | "a10" => align = SubAlignment::TopCenter,
                            "an9" | "a11" => align = SubAlignment::TopRight,
                            other => tag_rest.push(other),
                        }
                    }
                    if !tag_rest.is_empty() {
                        cur.push('{');
                        for (idx, r) in tag_rest.iter().enumerate() {
                            if idx > 0 || tag.starts_with('\\') {
                                cur.push('\\');
                            }
                            cur.push_str(r);
                        }
                        cur.push('}');
                    }
                    i += end + 1;
                    continue;
                }
            }
            let ch = line[i..].chars().next().unwrap();
            cur.push(ch);
            i += ch.len_utf8();
        }
        out_lines.push(cur);
    }
    (align, out_lines)
}

/// Parses a line into styled characters, interpreting formatting tags (`<i>`, `<b>`, `<font color="...">`, etc.).
pub fn parse_styled_line(line: &str) -> Vec<StyledChar> {
    let mut out = Vec::with_capacity(line.len());
    let mut italic = false;
    let mut bold = false;
    let mut color_stack: Vec<Rgba<u8>> = Vec::new();

    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for tc in chars.by_ref() {
                if tc == '>' {
                    break;
                }
                tag.push(tc);
            }
            let trimmed = tag.trim();
            let lower = trimmed.to_ascii_lowercase();
            if lower == "i" || lower == "em" {
                italic = true;
            } else if lower == "/i" || lower == "/em" {
                italic = false;
            } else if lower == "b" || lower == "strong" {
                bold = true;
            } else if lower == "/b" || lower == "/strong" {
                bold = false;
            } else if lower == "/font" {
                color_stack.pop();
            } else if lower.starts_with("font") {
                if let Some(col_idx) = lower.find("color") {
                    let rest = &trimmed[col_idx + 5..];
                    if let Some(eq_idx) = rest.find('=') {
                        let val = rest[eq_idx + 1..].trim();
                        let col_str = if val.starts_with('"') {
                            val.strip_prefix('"').and_then(|v| v.split('"').next()).unwrap_or("")
                        } else if val.starts_with('\'') {
                            val.strip_prefix('\'').and_then(|v| v.split('\'').next()).unwrap_or("")
                        } else {
                            val.split_whitespace().next().unwrap_or("")
                        };
                        if let Some(parsed) = parse_color(col_str) {
                            color_stack.push(parsed);
                        }
                    }
                }
            }
            continue;
        }

        if c == '{' {
            let mut tag = String::new();
            for tc in chars.by_ref() {
                if tc == '}' {
                    break;
                }
                tag.push(tc);
            }
            for part in tag.split('\\') {
                let part = part.trim();
                let lower = part.to_ascii_lowercase();
                if lower == "i1" || lower == "i" {
                    italic = true;
                } else if lower == "i0" {
                    italic = false;
                } else if lower == "b1" || lower == "b" {
                    bold = true;
                } else if lower == "b0" {
                    bold = false;
                } else if lower.starts_with("c&h") || lower.starts_with("1c&h") {
                    let s = if lower.starts_with("1c&h") { &part[2..] } else { &part[1..] };
                    if let Some(col) = parse_ass_color(s) {
                        color_stack.push(col);
                    }
                }
            }
            continue;
        }

        let cur_color = color_stack.last().copied().unwrap_or(FILL);
        match c {
            '\t' => out.push(StyledChar { c: ' ', italic, bold, color: cur_color }),
            c if c.is_control() => {}
            c => out.push(StyledChar { c, italic, bold, color: cur_color }),
        }
    }
    out
}

/// Removes `<…>` tags and `{\…}` override blocks.
pub fn strip_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let (mut tag, mut brace) = (false, false);
    for c in line.chars() {
        match c {
            '<' if !brace => tag = true,
            '>' if tag => tag = false,
            '{' if !tag => brace = true,
            '}' if brace => brace = false,
            _ if tag || brace => {}
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Parses SRT text. Returns the cues (text-less ones dropped) and the number of blocks skipped
/// because their time line could not be read. Markup is preserved in cue lines.
pub fn parse_srt(text: &str) -> (Vec<Cue>, usize) {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let (mut cues, mut skipped) = (Vec::new(), 0);
    for block in text.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| l.trim().is_empty());
        let Some(mut first) = lines.next() else { continue };
        // An index line precedes the times; some files omit it.
        if !first.contains("-->") {
            match lines.next() {
                Some(next) if next.contains("-->") => first = next,
                _ => {
                    skipped += 1;
                    continue;
                }
            }
        }
        let times = first.split_once("-->").and_then(|(a, b)| Some((parse_time(a)?, parse_time(b)?)));
        let Some((start, end)) = times else {
            skipped += 1;
            continue;
        };
        let lines: Vec<String> = lines
            .map(|l| l.trim().to_string())
            .filter(|l| !strip_markup(l).is_empty())
            .collect();
        if !lines.is_empty() {
            cues.push(Cue { start, end, lines });
        }
    }
    (cues, skipped)
}

/// Frame and metrics of the rendering: the DVD values are measured (see the module docs), other
/// canvases scale them.
#[derive(Debug, Clone, PartialEq)]
pub struct Canvas {
    pub size: (u32, u32),
    em: f32,
    pitch: i32,
    outline: i32,
    side: u32,
    bottom: u32,
    baseline_above: i32,
    trim: u32,
    /// Geometry scale relative to the DVD metrics (1.0 on DVD), for DVD-pixel style values.
    scale: f32,
    /// User-chosen font family (see [`SubtitleStyle::font_family`]).
    font: Option<String>,
    /// Track region (see [`SubtitleStyle::lang`]).
    lang: Option<String>,
}

impl Canvas {
    pub fn dvd(tv: TvStandard) -> Canvas {
        Canvas { size: tv.size(), em: EM_PX, pitch: LINE_PITCH, outline: OUTLINE, side: SIDE_MARGIN, bottom: BOTTOM_MARGIN, baseline_above: BASELINE_ABOVE_BOX, trim: FRAME_TRIM, scale: 1.0, font: None, lang: None }
    }

    /// HD canvas: em 65 px at 1080 lines (what tsMuxeR was configured with), the rest scaled from the DVD metrics.
    pub fn hd(width: u32, height: u32) -> Canvas {
        let s = 65.0 * height as f32 / 1080.0 / EM_PX;
        let px = |v: f32| (v * s).round();
        Canvas { size: (width, height), em: EM_PX * s, pitch: px(LINE_PITCH as f32) as i32, outline: px(OUTLINE as f32) as i32, side: px(SIDE_MARGIN as f32) as u32, bottom: px(BOTTOM_MARGIN as f32) as u32, baseline_above: px(BASELINE_ABOVE_BOX as f32) as i32, trim: 0, scale: s, font: None, lang: None }
    }
}

impl Canvas {
    /// Applies the user's size, outline, bottom margin and font; out-of-range values are clamped.
    /// The default style leaves every metric untouched.
    pub fn with_style(mut self, st: &SubtitleStyle) -> Canvas {
        if let Some(p) = st.font_size_pct.filter(|p| p.is_finite()) {
            let k = p.clamp(50.0, 200.0) / 100.0;
            self.em *= k;
            self.pitch = (self.pitch as f32 * k).round() as i32;
            self.baseline_above = (self.baseline_above as f32 * k).round() as i32;
        }
        if let Some(o) = st.outline_px {
            self.outline = (o.min(6) as f32 * self.scale).round() as i32;
        }
        if let Some(m) = st.bottom_margin_pct.filter(|m| m.is_finite()) {
            self.bottom = (self.size.1 as f32 * m.clamp(0.0, 30.0) / 100.0).round() as u32;
        }
        self.font = st.font_family.as_ref().map(|f| f.trim().to_string()).filter(|f| !f.is_empty());
        self.lang.clone_from(&st.lang);
        self
    }

    /// Font chain of this canvas: the chosen family and DejaVu Sans, then the fonts the track's
    /// language prefers for Han characters, then the Chinese-first system fallbacks.
    fn fonts(&self) -> Vec<FontArc> {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        let mut chain = crate::menu::subtitle_font_chain_for(self.font.as_deref());
        let preferred = self.lang.as_deref().map(crate::fonts::preferred_families).unwrap_or_default();
        let Some(&key) = preferred.first() else { return chain };
        static CACHE: OnceLock<Mutex<HashMap<&'static str, Vec<FontArc>>>> = OnceLock::new();
        let extra = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert_with(|| {
                // Collections (Noto CJK, msjh, mingliu, simsun, msgothic) hold several families:
                // load the face of the wanted one, not face 0.
                preferred
                    .iter()
                    .filter_map(|f| crate::fonts::find_family(f))
                    .filter_map(|f| ab_glyph::FontVec::try_from_vec_and_index(std::fs::read(&f.path).ok()?, f.index).ok())
                    .map(FontArc::new)
                    .collect()
            })
            .clone();
        // `subtitle_font_chain_for` = optional chosen font + `subtitle_font_chain` (DejaVu first).
        let at = chain.len() + 1 - crate::menu::subtitle_font_chain().len();
        chain.splice(at..at, extra);
        chain
    }
}

/// Shifts every cue by `offset_ms` (negative = earlier): cues ending at or before 0 are dropped,
/// starts are clamped to 0.
pub fn apply_offset(cues: Vec<Cue>, offset_ms: i64) -> Vec<Cue> {
    if offset_ms == 0 {
        return cues;
    }
    let d = offset_ms as f64 / 1000.0;
    cues.into_iter()
        .filter_map(|mut c| {
            c.start += d;
            c.end += d;
            if c.end <= 0.0 {
                return None;
            }
            c.start = c.start.max(0.0);
            Some(c)
        })
        .collect()
}

fn text_width_em(fonts: &[FontArc], text: &str, em: f32) -> f32 {
    let placed = place_chars(fonts, text);
    let mut w = 0.0;
    let mut prev: Option<Placed> = None;
    for &p in &placed {
        let sf = fonts[p.font].as_scaled(font_scale(&fonts[p.font], em));
        if let Some(q) = prev.filter(|q| q.font == p.font) {
            w += sf.kern(q.id, p.id);
        }
        w += sf.h_advance(p.id);
        prev = Some(p);
    }
    w
}

struct ArabicForms {
    isolated: char,
    final_form: char,
    initial: Option<char>,
    medial: Option<char>,
}

fn arabic_forms(c: char) -> Option<ArabicForms> {
    match c {
        '\u{0621}' => Some(ArabicForms { isolated: '\u{FE80}', final_form: '\u{FE80}', initial: None, medial: None }),
        '\u{0622}' => Some(ArabicForms { isolated: '\u{FE81}', final_form: '\u{FE82}', initial: None, medial: None }),
        '\u{0623}' => Some(ArabicForms { isolated: '\u{FE83}', final_form: '\u{FE84}', initial: None, medial: None }),
        '\u{0624}' => Some(ArabicForms { isolated: '\u{FE85}', final_form: '\u{FE86}', initial: None, medial: None }),
        '\u{0625}' => Some(ArabicForms { isolated: '\u{FE87}', final_form: '\u{FE88}', initial: None, medial: None }),
        '\u{0626}' => Some(ArabicForms { isolated: '\u{FE89}', final_form: '\u{FE8A}', initial: Some('\u{FE8B}'), medial: Some('\u{FE8C}') }),
        '\u{0627}' => Some(ArabicForms { isolated: '\u{FE8D}', final_form: '\u{FE8E}', initial: None, medial: None }),
        '\u{0628}' => Some(ArabicForms { isolated: '\u{FE8F}', final_form: '\u{FE90}', initial: Some('\u{FE91}'), medial: Some('\u{FE92}') }),
        '\u{0629}' => Some(ArabicForms { isolated: '\u{FE93}', final_form: '\u{FE94}', initial: None, medial: None }),
        '\u{062A}' => Some(ArabicForms { isolated: '\u{FE95}', final_form: '\u{FE96}', initial: Some('\u{FE97}'), medial: Some('\u{FE98}') }),
        '\u{062B}' => Some(ArabicForms { isolated: '\u{FE99}', final_form: '\u{FE9A}', initial: Some('\u{FE9B}'), medial: Some('\u{FE9C}') }),
        '\u{062C}' => Some(ArabicForms { isolated: '\u{FE9D}', final_form: '\u{FE9E}', initial: Some('\u{FE9F}'), medial: Some('\u{FEA0}') }),
        '\u{062D}' => Some(ArabicForms { isolated: '\u{FEA1}', final_form: '\u{FEA2}', initial: Some('\u{FEA3}'), medial: Some('\u{FEA4}') }),
        '\u{062E}' => Some(ArabicForms { isolated: '\u{FEA5}', final_form: '\u{FEA6}', initial: Some('\u{FEA7}'), medial: Some('\u{FEA8}') }),
        '\u{062F}' => Some(ArabicForms { isolated: '\u{FEA9}', final_form: '\u{FEAA}', initial: None, medial: None }),
        '\u{0630}' => Some(ArabicForms { isolated: '\u{FEAB}', final_form: '\u{FEAC}', initial: None, medial: None }),
        '\u{0631}' => Some(ArabicForms { isolated: '\u{FEAD}', final_form: '\u{FEAE}', initial: None, medial: None }),
        '\u{0632}' => Some(ArabicForms { isolated: '\u{FEAF}', final_form: '\u{FEB0}', initial: None, medial: None }),
        '\u{0633}' => Some(ArabicForms { isolated: '\u{FEB1}', final_form: '\u{FEB2}', initial: Some('\u{FEB3}'), medial: Some('\u{FEB4}') }),
        '\u{0634}' => Some(ArabicForms { isolated: '\u{FEB5}', final_form: '\u{FEB6}', initial: Some('\u{FEB7}'), medial: Some('\u{FEB8}') }),
        '\u{0635}' => Some(ArabicForms { isolated: '\u{FEB9}', final_form: '\u{FEBA}', initial: Some('\u{FEBB}'), medial: Some('\u{FEBC}') }),
        '\u{0636}' => Some(ArabicForms { isolated: '\u{FEBD}', final_form: '\u{FEBE}', initial: Some('\u{FEBF}'), medial: Some('\u{FEC0}') }),
        '\u{0637}' => Some(ArabicForms { isolated: '\u{FEC1}', final_form: '\u{FEC2}', initial: Some('\u{FEC3}'), medial: Some('\u{FEC4}') }),
        '\u{0638}' => Some(ArabicForms { isolated: '\u{FEC5}', final_form: '\u{FEC6}', initial: Some('\u{FEC7}'), medial: Some('\u{FEC8}') }),
        '\u{0639}' => Some(ArabicForms { isolated: '\u{FEC9}', final_form: '\u{FECA}', initial: Some('\u{FECB}'), medial: Some('\u{FECC}') }),
        '\u{063A}' => Some(ArabicForms { isolated: '\u{FECD}', final_form: '\u{FECE}', initial: Some('\u{FECF}'), medial: Some('\u{FED0}') }),
        '\u{0640}' => Some(ArabicForms { isolated: '\u{0640}', final_form: '\u{0640}', initial: Some('\u{0640}'), medial: Some('\u{0640}') }),
        '\u{0641}' => Some(ArabicForms { isolated: '\u{FED1}', final_form: '\u{FED2}', initial: Some('\u{FED3}'), medial: Some('\u{FED4}') }),
        '\u{0642}' => Some(ArabicForms { isolated: '\u{FED5}', final_form: '\u{FED6}', initial: Some('\u{FED7}'), medial: Some('\u{FED8}') }),
        '\u{0643}' => Some(ArabicForms { isolated: '\u{FED9}', final_form: '\u{FEDA}', initial: Some('\u{FEDB}'), medial: Some('\u{FEDC}') }),
        '\u{0644}' => Some(ArabicForms { isolated: '\u{FEDD}', final_form: '\u{FEDE}', initial: Some('\u{FEDF}'), medial: Some('\u{FEE0}') }),
        '\u{0645}' => Some(ArabicForms { isolated: '\u{FEE1}', final_form: '\u{FEE2}', initial: Some('\u{FEE3}'), medial: Some('\u{FEE4}') }),
        '\u{0646}' => Some(ArabicForms { isolated: '\u{FEE5}', final_form: '\u{FEE6}', initial: Some('\u{FEE7}'), medial: Some('\u{FEE8}') }),
        '\u{0647}' => Some(ArabicForms { isolated: '\u{FEE9}', final_form: '\u{FEEA}', initial: Some('\u{FEEB}'), medial: Some('\u{FEEC}') }),
        '\u{0648}' => Some(ArabicForms { isolated: '\u{FEED}', final_form: '\u{FEEE}', initial: None, medial: None }),
        '\u{0649}' => Some(ArabicForms { isolated: '\u{FEEF}', final_form: '\u{FEF0}', initial: None, medial: None }),
        '\u{064A}' => Some(ArabicForms { isolated: '\u{FEF1}', final_form: '\u{FEF2}', initial: Some('\u{FEF3}'), medial: Some('\u{FEF4}') }),
        _ => None,
    }
}

fn lam_alef(alef: char, connects_prev: bool) -> Option<char> {
    match alef {
        '\u{0622}' => Some(if connects_prev { '\u{FEF6}' } else { '\u{FEF5}' }),
        '\u{0623}' => Some(if connects_prev { '\u{FEF8}' } else { '\u{FEF7}' }),
        '\u{0625}' => Some(if connects_prev { '\u{FEFA}' } else { '\u{FEF9}' }),
        '\u{0627}' => Some(if connects_prev { '\u{FEFC}' } else { '\u{FEFB}' }),
        _ => None,
    }
}

fn connects_left(c: char) -> bool {
    arabic_forms(c).is_some_and(|f| f.initial.is_some())
}

fn connects_right(c: char) -> bool {
    arabic_forms(c).is_some()
}

/// Shapes Arabic characters into contextual Presentation Forms-B (isolated, initial, medial, final)
/// and Lam-Alef ligatures for cursive display.
pub fn shape_arabic(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\u{0644}' && i + 1 < chars.len() {
            let next = chars[i + 1];
            let connects_prev = i > 0 && connects_left(chars[i - 1]);
            if let Some(ligature) = lam_alef(next, connects_prev) {
                out.push(ligature);
                i += 2;
                continue;
            }
        }

        if let Some(forms) = arabic_forms(c) {
            let connects_prev = i > 0 && connects_left(chars[i - 1]);
            let connects_next = i + 1 < chars.len() && connects_right(chars[i + 1]) && forms.initial.is_some();
            let shaped = match (connects_prev, connects_next) {
                (true, true) => forms.medial.unwrap_or(forms.final_form),
                (false, true) => forms.initial.unwrap_or(forms.isolated),
                (true, false) => forms.final_form,
                (false, false) => forms.isolated,
            };
            out.push(shaped);
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Reorders a text line for visual LTR display (Unicode Bidirectional Algorithm for RTL scripts,
/// with Arabic cursive shaping).
pub fn reorder_bidi(text: &str) -> String {
    let shaped = shape_arabic(text);
    let bidi_info = BidiInfo::new(&shaped, None);
    if bidi_info.paragraphs.is_empty() {
        return shaped;
    }
    let mut out = String::with_capacity(shaped.len());
    for para in &bidi_info.paragraphs {
        let line = para.range.clone();
        out.push_str(&bidi_info.reorder_line(para, line));
    }
    out
}

#[cfg(test)]
fn text_width(text: &str) -> f32 {
    let fonts = crate::menu::subtitle_font_chain();
    text_width_em(&fonts, text, EM_PX)
}

/// Greedy word wrap to `max_w` pixels (a single longer word stays on its own line).
pub fn wrap(text: &str, max_w: f32) -> Vec<String> {
    let fonts = crate::menu::subtitle_font_chain();
    wrap_em(&fonts, text, max_w, EM_PX)
}

fn wrap_em(fonts: &[FontArc], text: &str, max_w: f32, em: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let joined = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if cur.is_empty() || text_width_em(fonts, &joined, em) <= max_w {
            cur = joined;
        } else {
            lines.push(std::mem::replace(&mut cur, word.to_string()));
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn wrap_styled_em(fonts: &[FontArc], chars: &[StyledChar], max_w: f32, em: f32) -> Vec<Vec<StyledChar>> {
    let mut lines = Vec::new();
    let mut cur: Vec<StyledChar> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].c.is_whitespace() {
            i += 1;
            continue;
        }
        let word_start = i;
        while i < chars.len() && !chars[i].c.is_whitespace() {
            i += 1;
        }
        let word = &chars[word_start..i];
        if cur.is_empty() {
            cur.extend_from_slice(word);
        } else {
            let mut test_line = cur.clone();
            test_line.push(StyledChar { c: ' ', italic: false, bold: false, color: FILL });
            test_line.extend_from_slice(word);
            let test_str: String = test_line.iter().map(|sc| sc.c).collect();
            if text_width_em(fonts, &test_str, em) <= max_w {
                cur = test_line;
            } else {
                lines.push(std::mem::replace(&mut cur, word.to_vec()));
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn reorder_styled_bidi(chars: &[StyledChar]) -> Vec<StyledChar> {
    let raw_text: String = chars.iter().map(|sc| sc.c).collect();
    let shaped = shape_arabic(&raw_text);
    let bidi_info = BidiInfo::new(&shaped, None);
    if bidi_info.paragraphs.is_empty() || !bidi_info.has_rtl() {
        return chars.to_vec();
    }
    let mut out = Vec::with_capacity(chars.len());
    for para in &bidi_info.paragraphs {
        let line = para.range.clone();
        let reordered_str = bidi_info.reorder_line(para, line);
        let base_style = chars.first().copied().unwrap_or(StyledChar { c: ' ', italic: false, bold: false, color: FILL });
        for c in reordered_str.chars() {
            out.push(StyledChar { c, italic: base_style.italic, bold: base_style.bold, color: base_style.color });
        }
    }
    out
}

/// Renders one cue on a `tv` frame; returns the picture and its top-left corner in the frame, or
/// `None` when nothing is visible.
pub fn render_cue(tv: TvStandard, cue_lines: &[String]) -> Option<(RgbaImage, u32, u32)> {
    render_cue_on(&Canvas::dvd(tv), cue_lines)
}

/// [`render_cue`] on any [`Canvas`].
pub fn render_cue_on(c: &Canvas, cue_lines: &[String]) -> Option<(RgbaImage, u32, u32)> {
    let fonts = c.fonts();
    let (fw, fh) = c.size;
    let max_w = (fw - 2 * c.side) as f32;
    let (alignment, cue_lines) = extract_alignment(cue_lines);
    let lines: Vec<Vec<StyledChar>> = cue_lines
        .iter()
        .flat_map(|l| {
            let styled = parse_styled_line(l);
            wrap_styled_em(&fonts, &styled, max_w, c.em)
        })
        .map(|l| reorder_styled_bidi(&l))
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let sf0 = fonts[0].as_scaled(font_scale(&fonts[0], c.em));
    let n = lines.len() as i32;
    let (first_baseline, last_baseline) = match alignment {
        SubAlignment::TopCenter | SubAlignment::TopLeft | SubAlignment::TopRight => {
            let first = (c.trim + c.bottom) as i32 + sf0.ascent().ceil() as i32;
            let last = first + (n - 1) * c.pitch;
            (first, last)
        }
        SubAlignment::MiddleCenter | SubAlignment::MiddleLeft | SubAlignment::MiddleRight => {
            let total_h = (n - 1) * c.pitch + sf0.ascent().ceil() as i32 + (-sf0.descent()).ceil() as i32;
            let y_mid = (fh / 2) as i32;
            let first = y_mid - total_h / 2 + sf0.ascent().ceil() as i32;
            let last = first + (n - 1) * c.pitch;
            (first, last)
        }
        _ => {
            let last = (fh - c.trim - c.bottom) as i32 - c.baseline_above;
            let first = last - (n - 1) * c.pitch;
            (first, last)
        }
    };

    let line_strings: Vec<String> = lines.iter().map(|l| l.iter().map(|sc| sc.c).collect()).collect();
    let line_widths: Vec<f32> = line_strings.iter().map(|s| text_width_em(&fonts, s, c.em)).collect();
    let widest = line_widths.iter().copied().fold(0.0f32, f32::max);

    let (x0, x1) = match alignment {
        SubAlignment::TopLeft | SubAlignment::BottomLeft | SubAlignment::MiddleLeft => {
            let x0 = (c.side as i32 - c.outline - 2).max(0);
            let x1 = ((c.side as f32 + widest).ceil() as i32 + c.outline + 2).min(fw as i32);
            (x0, x1)
        }
        SubAlignment::TopRight | SubAlignment::BottomRight | SubAlignment::MiddleRight => {
            let x0 = (((fw - c.side) as f32 - widest).floor() as i32 - c.outline - 2).max(0);
            let x1 = ((fw - c.side) as i32 + c.outline + 2).min(fw as i32);
            (x0, x1)
        }
        _ => {
            let x0 = (((fw as f32 - widest) / 2.0).floor() as i32 - c.outline - 2).max(0);
            let x1 = (((fw as f32 + widest) / 2.0).ceil() as i32 + c.outline + 2).min(fw as i32);
            (x0, x1)
        }
    };
    let y0 = (first_baseline - sf0.ascent().ceil() as i32 - c.outline - 1).max(0);
    let y1 = (last_baseline + (-sf0.descent()).ceil() as i32 + c.outline + 1).min(fh as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut ink: Vec<Option<Rgba<u8>>> = vec![None; w * h];
    for (i, line) in lines.iter().enumerate() {
        let baseline = (first_baseline + i as i32 * c.pitch) as f32;
        let lw = line_widths[i];
        let mut caret = match alignment {
            SubAlignment::TopLeft | SubAlignment::BottomLeft | SubAlignment::MiddleLeft => c.side as f32,
            SubAlignment::TopRight | SubAlignment::BottomRight | SubAlignment::MiddleRight => (fw - c.side) as f32 - lw,
            _ => ((fw as f32 - lw) / 2.0).floor(),
        };
        let line_str = &line_strings[i];
        let placed = place_chars(&fonts, line_str);
        let mut prev: Option<Placed> = None;
        for (idx, &p) in placed.iter().enumerate() {
            let sc = line[idx];
            let font = &fonts[p.font];
            let scale = font_scale(font, c.em);
            let sf = font.as_scaled(scale);
            if let Some(q) = prev.filter(|q| q.font == p.font) {
                caret += sf.kern(q.id, p.id);
            }
            let glyph = p.id.with_scale_and_position(scale, point(caret, baseline));
            caret += sf.h_advance(p.id);
            prev = Some(p);
            if let Some(o) = font.outline_glyph(glyph) {
                let b = o.px_bounds();
                o.draw(|px, py, cov| {
                    if cov < 0.5 {
                        return;
                    }
                    let mut gx = b.min.x as i32 + px as i32;
                    let gy = b.min.y as i32 + py as i32;
                    if sc.italic {
                        let dy = baseline - gy as f32;
                        gx += (dy * 0.213).round() as i32;
                    }
                    let x = gx - x0;
                    let y = gy - y0;
                    let d_max = if sc.bold { 1 } else { 0 };
                    for ddx in 0..=d_max {
                        let cur_x = x + ddx;
                        if cur_x >= 0 && y >= 0 && (cur_x as usize) < w && (y as usize) < h {
                            ink[y as usize * w + cur_x as usize] = Some(sc.color);
                        }
                    }
                });
            }
        }
    }

    let mut img = RgbaImage::new(w as u32, h as u32);
    let r = c.outline;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let idx = y as usize * w + x as usize;
            let px = if let Some(fill_color) = ink[idx] {
                fill_color
            } else if (-r..=r).any(|dy| {
                (-r..=r).any(|dx| {
                    let (xx, yy) = (x + dx, y + dy);
                    dx * dx + dy * dy <= r * r + 1 && xx >= 0 && yy >= 0 && (xx as usize) < w && (yy as usize) < h && ink[yy as usize * w + xx as usize].is_some()
                })
            }) {
                EDGE
            } else {
                continue;
            };
            img.put_pixel(x as u32, y as u32, px);
        }
    }
    Some((img, x0 as u32, y0 as u32))
}

/// Renders all cues in parallel into pictures on the DVD frame of `tv`.
pub fn render_cues(cues: &[Cue], tv: TvStandard) -> Decoded {
    render_cues_on(cues, tv, &Canvas::dvd(tv))
}

/// [`render_cues`] with the user's offset (applied first) and style.
pub fn render_cues_styled(cues: &[Cue], tv: TvStandard, style: &SubtitleStyle) -> Decoded {
    let shifted = apply_offset(cues.to_vec(), style.offset_ms);
    render_cues_on(&shifted, tv, &Canvas::dvd(tv).with_style(style))
}

fn render_cues_on(cues: &[Cue], tv: TvStandard, canvas: &Canvas) -> Decoded {
    let pictures = cues
        .par_iter()
        .filter_map(|c| {
            let (image, x, y) = render_cue_on(canvas, &c.lines)?;
            Some(Picture { start: c.start, end: c.end, forced: false, x, y, image })
        })
        .collect();
    Decoded { canvas: tv.size(), pictures }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_picks_its_own_han_glyphs() {
        // Only where both regional fonts are installed (Windows with Japanese and Chinese fonts).
        if crate::fonts::find_family("Yu Gothic").is_none() || crate::fonts::find_family("Microsoft YaHei").is_none() {
            return eprintln!("skipping: Yu Gothic / Microsoft YaHei not installed");
        }
        let draw = |lang: Option<&str>| {
            let c = Canvas::hd(1920, 1080).with_style(&SubtitleStyle { lang: lang.map(Into::into), ..Default::default() });
            render_cue_on(&c, &["線 直 骨".into()]).unwrap().0
        };
        let (ja, cn, none) = (draw(Some("ja")), draw(Some("zh-CN")), draw(None));
        assert_ne!(ja.as_raw(), cn.as_raw(), "Japanese and Chinese forms differ");
        // Untagged tracks keep today's chain, whose first CJK font is Microsoft YaHei.
        assert_eq!(cn.as_raw(), none.as_raw());
    }

    fn ink_rows(img: &RgbaImage, y: u32) -> (u32, u32) {
        let top = (0..img.height()).find(|&r| (0..img.width()).any(|x| img.get_pixel(x, r)[3] > 0)).unwrap();
        let bot = (0..img.height()).rev().find(|&r| (0..img.width()).any(|x| img.get_pixel(x, r)[3] > 0)).unwrap();
        (y + top, y + bot)
    }

    #[test]
    fn style_default_is_identity_and_applies() {
        let d = Canvas::dvd(TvStandard::Pal);
        assert_eq!(d.clone().with_style(&SubtitleStyle::default()), d);
        let l = vec!["Hallo".to_string()];
        let (a, _, ya) = render_cue_on(&d, &l).unwrap();
        let big = d.clone().with_style(&SubtitleStyle { font_size_pct: Some(150.0), ..Default::default() });
        let (b, _, yb) = render_cue_on(&big, &l).unwrap();
        let (ra, rb) = (ink_rows(&a, ya), ink_rows(&b, yb));
        assert!(rb.1 - rb.0 > ra.1 - ra.0);
        let up = d.clone().with_style(&SubtitleStyle { bottom_margin_pct: Some(20.0), ..Default::default() });
        let (c, _, yc) = render_cue_on(&up, &l).unwrap();
        assert!(ink_rows(&c, yc).1 < ra.1);
        let unk = d.with_style(&SubtitleStyle { font_family: Some("No Such Family 123".into()), ..Default::default() });
        assert!(render_cue_on(&unk, &l).is_some());
    }

    #[test]
    fn offset_shifts_and_drops() {
        let c = |s, e| Cue { start: s, end: e, lines: vec!["x".into()] };
        let out = apply_offset(vec![c(0.5, 1.0), c(1.0, 3.0), c(5.0, 6.0)], -1500);
        assert_eq!(out, vec![c(0.0, 1.5), c(3.5, 4.5)]);
        assert_eq!(apply_offset(vec![c(1.0, 2.0)], 500), vec![c(1.5, 2.5)]);
    }

    #[test]
    fn parses_srt_variants() {
        let src = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hallo</i>\r\n{\\an8}Welt\r\n\r\n2\r\n00:01:00.250 --> 01:00:00,000 X1:1\r\nNoch\r\n\r\nkaputt\r\nzeit --> irgendwann\r\nx\r\n\r\n4\r\n00:00:05,000 --> 00:00:06,000\r\n\r\n";
        let (cues, skipped) = parse_srt(src);
        assert_eq!(skipped, 1);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!(cues[0], Cue { start: 1.0, end: 2.5, lines: vec!["<i>Hallo</i>".into(), "{\\an8}Welt".into()] });
        assert_eq!((cues[1].start, cues[1].end), (60.25, 3600.0));
        assert_eq!(strip_markup(&cues[0].lines[0]), "Hallo");
        assert_eq!(strip_markup(&cues[0].lines[1]), "Welt");
        // Blocks without an index line are accepted.
        let (c, s) = parse_srt("00:00:01,000 --> 00:00:02,000\nA\n");
        assert_eq!((c.len(), s), (1, 0));
    }

    #[test]
    fn top_alignment_and_markup_rendering() {
        // Top alignment renders cue near the top of the frame
        let (img_top, x_top, y_top) = render_cue(TvStandard::Pal, &["{\\an8}Oben zentriert".into()]).unwrap();
        let (x0, y0, x1, _y1) = bbox(&img_top, x_top, y_top);
        assert!(y0 < 100, "Top aligned cue should be near the top: {y0}");
        let centre = (x0 + x1) as i32 / 2;
        assert!((centre - 720 / 2).abs() <= 5, "top centre: {centre}");

        // Normal bottom cue has y0 > 400
        let (img_bot, x_bot, y_bot) = render_cue(TvStandard::Pal, &["Unten zentriert".into()]).unwrap();
        let (_, y0_bot, _, _) = bbox(&img_bot, x_bot, y_bot);
        assert!(y0_bot > 400, "Bottom cue should be near the bottom: {y0_bot}");

        // Colored subtitle markup: yellow fill
        let (img_yellow, _, _) = render_cue(TvStandard::Pal, &["<font color=\"yellow\">Gelber Text</font>".into()]).unwrap();
        let yellow = Rgba([255, 255, 0, 255]);
        let yellow_count = img_yellow.pixels().filter(|p| **p == yellow).count();
        assert!(yellow_count > 100, "Yellow subtitle should have yellow ink: {yellow_count}");

        // Italic slant produces ink
        let (img_italic, _, _) = render_cue(TvStandard::Pal, &["<i>Kursiver Text</i>".into()]).unwrap();
        assert!(img_italic.pixels().any(|p| *p == FILL));

        // Bold text produces ink
        let (img_bold, _, _) = render_cue(TvStandard::Pal, &["<b>Fetter Text</b>".into()]).unwrap();
        assert!(img_bold.pixels().any(|p| *p == FILL));
    }

    #[test]
    fn wraps_to_the_safe_width_and_keeps_long_words() {
        let max = 600.0;
        let lines = wrap("Satz der garantiert umgebrochen werden muss weil er viel zu breit ist fuer den Bildschirm", max);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|l| text_width(l) <= max), "{lines:?}");
        assert_eq!(lines.join(" "), "Satz der garantiert umgebrochen werden muss weil er viel zu breit ist fuer den Bildschirm");
        let long = "W".repeat(60);
        assert_eq!(wrap(&long, max), vec![long]);
    }

    fn bbox(img: &RgbaImage, x: u32, y: u32) -> (u32, u32, u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for (px, py, p) in img.enumerate_pixels() {
            if p.0[3] > 0 {
                x0 = x0.min(px + x);
                y0 = y0.min(py + y);
                x1 = x1.max(px + x);
                y1 = y1.max(py + y);
            }
        }
        (x0, y0, x1, y1)
    }

    #[test]
    fn cue_is_centred_at_the_bottom_inside_the_frame() {
        for tv in [TvStandard::Pal, TvStandard::Ntsc] {
            let (fw, fh) = tv.size();
            let (img, x, y) = render_cue(tv, &["Hallo Welt gjpq".into()]).unwrap();
            let (x0, y0, x1, y1) = bbox(&img, x, y);
            assert!(x1 < fw && y1 < fh && x0 >= SIDE_MARGIN - 20, "{x0} {y0} {x1} {y1}");
            let centre = (x0 + x1) as i32 / 2;
            assert!((centre - fw as i32 / 2).abs() <= 3, "centre {centre}");
            assert!(y1 <= fh - FRAME_TRIM - BOTTOM_MARGIN + 2 && y0 > fh * 2 / 3, "{y0} {y1}");
            // Only the two cue colours, no anti-aliasing.
            assert!(img.pixels().all(|p| p.0[3] == 0 || *p == FILL || *p == EDGE));
        }
    }

    #[test]
    fn hd_cue_is_centred_at_the_bottom_and_scales_with_the_frame() {
        let lines = ["Hallo Welt gjpq".to_string()];
        let mut heights = Vec::new();
        for (w, h) in [(1920u32, 1080u32), (1280, 720)] {
            let (img, x, y) = render_cue_on(&Canvas::hd(w, h), &lines).unwrap();
            let (x0, y0, x1, y1) = bbox(&img, x, y);
            assert!(x1 < w && y1 < h && y0 > h * 3 / 4, "{x0} {y0} {x1} {y1}");
            assert!(((x0 + x1) as i32 / 2 - w as i32 / 2).abs() <= 3);
            assert!(img.pixels().all(|p| p.0[3] == 0 || *p == FILL || *p == EDGE));
            heights.push(y1 - y0);
        }
        // 65 px em at 1080 lines (what tsMuxeR was configured with), two thirds of it at 720.
        assert!(heights[0] > 45 && heights[0] * 2 > heights[1] * 3 - 6 && heights[0] * 2 < heights[1] * 3 + 12, "{heights:?}");
    }

    #[test]
    fn two_lines_stack_upwards_and_cyrillic_has_ink() {
        let (one, _, y1) = render_cue(TvStandard::Pal, &["Eins".into()]).unwrap();
        let (two, _, y2) = render_cue(TvStandard::Pal, &["Eins".into(), "Zwei".into()]).unwrap();
        assert!(y2 + 20 < y1 && two.height() > one.height());
        let (cy, _, _) = render_cue(TvStandard::Pal, &["Привет, мир".into()]).unwrap();
        assert!(cy.pixels().filter(|p| **p == FILL).count() > 200);
    }

    #[test]
    fn empty_and_wide_input_do_not_panic() {
        assert!(render_cue(TvStandard::Pal, &[]).is_none());
        assert!(render_cue(TvStandard::Pal, &["   ".into()]).is_none());
        let (img, x, _) = render_cue(TvStandard::Pal, &["W".repeat(80)]).unwrap();
        assert!(x + img.width() <= 720);
    }

    #[test]
    fn cjk_subtitles_render_with_fallback() {
        // CJK text falls back to system fonts (e.g. msyh, msgothic, malgun, etc.)
        let (img, x, y) = render_cue(TvStandard::Pal, &["你好，世界！ 日本語 한국어".into()]).unwrap();
        assert!(img.width() > 0 && img.height() > 0);
        let fill_count = img.pixels().filter(|p| **p == FILL).count();
        assert!(fill_count > 100, "CJK subtitle should render visible fill ink, got {fill_count}");
        let (x0, y0, x1, y1) = bbox(&img, x, y);
        assert!(x1 < 720 && y1 < 576, "{x0} {y0} {x1} {y1}");
    }

    #[test]
    fn rtl_hebrew_and_arabic_bidi_reorder() {
        // Pure Hebrew text is reversed in visual order
        let hebrew = "שלום";
        let reordered = reorder_bidi(hebrew);
        assert_ne!(reordered, hebrew);
        assert_eq!(reordered, "םולש");

        // Arabic cursive shaping and Lam-Alef ligature: "سلام" (Seen + Lam + Alef + Meem)
        // Seen (initial 0xFEB3), Lam-Alef (final ligature 0xFEFC), Meem (isolated 0xFEE1)
        let arabic = "سلام";
        let shaped = shape_arabic(arabic);
        assert_eq!(shaped, "\u{FEB3}\u{FEFC}\u{FEE1}");
        // reorder_bidi puts shaped Arabic into visual order for LTR rendering
        let reordered_ar = reorder_bidi(arabic);
        assert_eq!(reordered_ar, "\u{FEE1}\u{FEFC}\u{FEB3}");

        // Mixed LTR and RTL
        let mixed = "Hello שלום World";
        let reordered_mixed = reorder_bidi(mixed);
        assert!(reordered_mixed.starts_with("Hello "));
        assert!(reordered_mixed.ends_with(" World"));
        assert!(reordered_mixed.contains("םולש"));

        // Renders onto the frame with visible ink
        let (img, _, _) = render_cue(TvStandard::Pal, &[mixed.into(), arabic.into()]).unwrap();
        assert!(img.pixels().filter(|p| **p == FILL).count() > 100);
    }

    #[test]
    fn extract_alignment_preserves_utf8() {
        let cases = [
            ("äöüßÄÖÜ", SubAlignment::BottomCenter, "äöüßÄÖÜ"),
            ("Привет, мир!", SubAlignment::BottomCenter, "Привет, мир!"),
            ("Café résumé naïve", SubAlignment::BottomCenter, "Café résumé naïve"),
            ("你好", SubAlignment::BottomCenter, "你好"),
            ("שלום", SubAlignment::BottomCenter, "שלום"),
            ("{\\an8\\i1}Über Café — Привет!", SubAlignment::TopCenter, "{\\i1}Über Café — Привет!"),
            ("Unclosed {äöüß Привет 你好 שלום", SubAlignment::BottomCenter, "Unclosed {äöüß Привет 你好 שלום"),
        ];
        for (input, expected_align, expected_text) in cases {
            let (align, lines) = extract_alignment(&[input.to_string()]);
            assert_eq!(align, expected_align, "alignment for {input:?}");
            assert_eq!(lines, vec![expected_text.to_string()], "text for {input:?}");
        }
    }
}
