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

const FILL: Rgba<u8> = Rgba([255, 255, 255, 255]);
const EDGE: Rgba<u8> = Rgba([0, 0, 0, 255]);

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

/// One subtitle: seconds from the title start and its text lines (markup removed).
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

/// Removes `<…>` tags and `{\…}` override blocks.
fn strip_markup(line: &str) -> String {
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
/// because their time line could not be read.
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
        let lines: Vec<String> = lines.map(strip_markup).filter(|l| !l.is_empty()).collect();
        if !lines.is_empty() {
            cues.push(Cue { start, end, lines });
        }
    }
    (cues, skipped)
}

/// Frame and metrics of the rendering: the DVD values are measured (see the module docs), other
/// canvases scale them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Canvas {
    pub size: (u32, u32),
    em: f32,
    pitch: i32,
    outline: i32,
    side: u32,
    bottom: u32,
    baseline_above: i32,
    trim: u32,
}

impl Canvas {
    pub fn dvd(tv: TvStandard) -> Canvas {
        Canvas { size: tv.size(), em: EM_PX, pitch: LINE_PITCH, outline: OUTLINE, side: SIDE_MARGIN, bottom: BOTTOM_MARGIN, baseline_above: BASELINE_ABOVE_BOX, trim: FRAME_TRIM }
    }

    /// HD canvas: em 65 px at 1080 lines (what tsMuxeR was configured with), the rest scaled from the DVD metrics.
    pub fn hd(width: u32, height: u32) -> Canvas {
        let s = 65.0 * height as f32 / 1080.0 / EM_PX;
        let px = |v: f32| (v * s).round();
        Canvas { size: (width, height), em: EM_PX * s, pitch: px(LINE_PITCH as f32) as i32, outline: px(OUTLINE as f32) as i32, side: px(SIDE_MARGIN as f32) as u32, bottom: px(BOTTOM_MARGIN as f32) as u32, baseline_above: px(BASELINE_ABOVE_BOX as f32) as i32, trim: 0 }
    }
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

/// Renders one cue on a `tv` frame; returns the picture and its top-left corner in the frame, or
/// `None` when nothing is visible.
pub fn render_cue(tv: TvStandard, cue_lines: &[String]) -> Option<(RgbaImage, u32, u32)> {
    render_cue_on(&Canvas::dvd(tv), cue_lines)
}

/// [`render_cue`] on any [`Canvas`].
pub fn render_cue_on(c: &Canvas, cue_lines: &[String]) -> Option<(RgbaImage, u32, u32)> {
    let fonts = crate::menu::subtitle_font_chain();
    let (fw, fh) = c.size;
    let max_w = (fw - 2 * c.side) as f32;
    let lines: Vec<String> = cue_lines
        .iter()
        .flat_map(|l| wrap_em(&fonts, l, max_w, c.em))
        .map(|l| reorder_bidi(&l))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let sf0 = fonts[0].as_scaled(font_scale(&fonts[0], c.em));
    let last_baseline = (fh - c.trim - c.bottom) as i32 - c.baseline_above;
    let n = lines.len() as i32;
    let first_baseline = last_baseline - (n - 1) * c.pitch;

    // Coverage in frame coordinates, but only over the cue's bounding box.
    let widest = lines.iter().map(|l| text_width_em(&fonts, l, c.em)).fold(0.0f32, f32::max);
    let x0 = (((fw as f32 - widest) / 2.0).floor() as i32 - c.outline - 2).max(0);
    let x1 = (((fw as f32 + widest) / 2.0).ceil() as i32 + c.outline + 2).min(fw as i32);
    let y0 = (first_baseline - sf0.ascent().ceil() as i32 - c.outline - 1).max(0);
    let y1 = (last_baseline + (-sf0.descent()).ceil() as i32 + c.outline + 1).min(fh as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut ink = vec![false; w * h];
    for (i, line) in lines.iter().enumerate() {
        let baseline = (first_baseline + i as i32 * c.pitch) as f32;
        let mut caret = ((fw as f32 - text_width_em(&fonts, line, c.em)) / 2.0).floor();
        let placed = place_chars(&fonts, line);
        let mut prev: Option<Placed> = None;
        for &p in &placed {
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
                    let (x, y) = (b.min.x as i32 + px as i32 - x0, b.min.y as i32 + py as i32 - y0);
                    if cov >= 0.5 && x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        ink[y as usize * w + x as usize] = true;
                    }
                });
            }
        }
    }

    let mut img = RgbaImage::new(w as u32, h as u32);
    let r = c.outline;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let px = if ink[y as usize * w + x as usize] {
                FILL
            } else if (-r..=r).any(|dy| {
                (-r..=r).any(|dx| {
                    let (xx, yy) = (x + dx, y + dy);
                    dx * dx + dy * dy <= r * r + 1 && xx >= 0 && yy >= 0 && (xx as usize) < w && (yy as usize) < h && ink[yy as usize * w + xx as usize]
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
    let pictures = cues
        .par_iter()
        .filter_map(|c| {
            let (image, x, y) = render_cue(tv, &c.lines)?;
            Some(Picture { start: c.start, end: c.end, forced: false, x, y, image })
        })
        .collect();
    Decoded { canvas: tv.size(), pictures }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_srt_variants() {
        let src = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hallo</i>\r\n{\\an8}Welt\r\n\r\n2\r\n00:01:00.250 --> 01:00:00,000 X1:1\r\nNoch\r\n\r\nkaputt\r\nzeit --> irgendwann\r\nx\r\n\r\n4\r\n00:00:05,000 --> 00:00:06,000\r\n\r\n";
        let (cues, skipped) = parse_srt(src);
        assert_eq!(skipped, 1);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!(cues[0], Cue { start: 1.0, end: 2.5, lines: vec!["Hallo".into(), "Welt".into()] });
        assert_eq!((cues[1].start, cues[1].end), (60.25, 3600.0));
        // Blocks without an index line are accepted.
        let (c, s) = parse_srt("00:00:01,000 --> 00:00:02,000\nA\n");
        assert_eq!((c.len(), s), (1, 0));
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
}
