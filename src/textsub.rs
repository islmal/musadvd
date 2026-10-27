//! Text (SRT) subtitles rendered natively into DVD subpictures: white text with a black outline,
//! bottom centred. The pictures go through the same path as decoded bitmap subtitles
//! ([`crate::subpic::render`] → [`crate::spumux`]), so no external tool is involved.
//!
//! Geometry and colours were measured on the output of dvdauthor's `spumux` for the settings the
//! encoder used to pass (`fontsize=28`, margins 60/60/20/40): em ≈ 31 px, line pitch 34 px, 2 px
//! outline, hard (non-anti-aliased) edges, last baseline 13 px above the bottom margin, wrapping at
//! the width between the side margins, `<i>`/`<b>` ignored.
//!
//! The font is bundled (DejaVu Sans, `assets/fonts/`), so the result does not depend on the
//! machine's fonts. shortcut: no shaping or bidi (spumux used fribidi), so Arabic/Hebrew cues
//! come out unjoined and in logical order; the build warns for such subtitle languages. Upgrade
//! with `unicode-bidi` + a shaper when RTL matters.

use std::sync::OnceLock;

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};
use rayon::prelude::*;

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

/// DejaVu Sans (free licence, `assets/fonts/DejaVu-LICENSE.txt`).
static FONT_DATA: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_DATA).expect("bundled font is valid"))
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

fn scaled(em: f32) -> ab_glyph::PxScaleFont<&'static FontRef<'static>> {
    let f = font();
    f.as_scaled(PxScale::from(em * f.height_unscaled() / f.units_per_em().unwrap_or(f.height_unscaled())))
}

fn text_width_em(text: &str, em: f32) -> f32 {
    let sf = scaled(em);
    let mut w = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            w += sf.kern(p, id);
        }
        w += sf.h_advance(id);
        prev = Some(id);
    }
    w
}

#[cfg(test)]
fn text_width(text: &str) -> f32 {
    text_width_em(text, EM_PX)
}

/// Greedy word wrap to `max_w` pixels (a single longer word stays on its own line).
pub fn wrap(text: &str, max_w: f32) -> Vec<String> {
    wrap_em(text, max_w, EM_PX)
}

fn wrap_em(text: &str, max_w: f32, em: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let joined = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if cur.is_empty() || text_width_em(&joined, em) <= max_w {
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
    let (fw, fh) = c.size;
    let lines: Vec<String> = cue_lines.iter().flat_map(|l| wrap_em(l, (fw - 2 * c.side) as f32, c.em)).collect();
    if lines.is_empty() {
        return None;
    }
    let sf = scaled(c.em);
    let last_baseline = (fh - c.trim - c.bottom) as i32 - c.baseline_above;
    let n = lines.len() as i32;
    let first_baseline = last_baseline - (n - 1) * c.pitch;

    // Coverage in frame coordinates, but only over the cue's bounding box.
    let widest = lines.iter().map(|l| text_width_em(l, c.em)).fold(0.0f32, f32::max);
    let x0 = (((fw as f32 - widest) / 2.0).floor() as i32 - c.outline - 2).max(0);
    let x1 = (((fw as f32 + widest) / 2.0).ceil() as i32 + c.outline + 2).min(fw as i32);
    let y0 = (first_baseline - sf.ascent().ceil() as i32 - c.outline - 1).max(0);
    let y1 = (last_baseline + (-sf.descent()).ceil() as i32 + c.outline + 1).min(fh as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut ink = vec![false; w * h];
    for (i, line) in lines.iter().enumerate() {
        let baseline = (first_baseline + i as i32 * c.pitch) as f32;
        let mut caret = ((fw as f32 - text_width_em(line, c.em)) / 2.0).floor();
        let mut prev = None;
        for c in line.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                caret += sf.kern(p, id);
            }
            let glyph = id.with_scale_and_position(sf.scale(), point(caret, baseline));
            caret += sf.h_advance(id);
            prev = Some(id);
            if let Some(o) = font().outline_glyph(glyph) {
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
}
