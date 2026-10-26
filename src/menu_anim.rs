//! Intro animation of a template menu page (Blu-ray and DVD): keyframe interpolation and the per-frame
//! compositing of the intro (see `docs/plans/archive/2026-10-26-template-menu-intro.md`).
//!
//! The intro is a stream of opaque RGBA frames that `encoder` overlays on the first `T` seconds of
//! the page's one continuous encode. A [`Sprite`] is one element of the page, drawn back to front
//! at the interpolated box and opacity onto the page background, whose frames (colour, picture or
//! clip, blurred and shaded like the page) arrive as raw RGBA from a helper ffmpeg. Nothing is
//! written to disk: sprites live in memory, moving slot videos arrive the same way.

use image::imageops::{self, FilterType};
use image::RgbaImage;

use crate::author::TvStandard;
use crate::bd::BdRate;
use crate::layout::ElementKey;
use crate::template::Graphic;

/// `(x, y, w, h)` in frame pixels.
pub type Rect = (f64, f64, f64, f64);

/// Largest scaled sprite side drawn (a zoom that big covers the frame anyway).
const MAX_SIDE: i64 = 8192;

#[derive(Debug, Clone)]
pub enum Pixels {
    /// Text or picture, rendered once at its final size.
    Image(RgbaImage),
    /// Frame `i` of the `index`-th slot stream: `width`×`height` RGBA, the slot's final size.
    Slot { index: usize, width: u32, height: u32 },
}

/// One element of an intro menu.
#[derive(Debug, Clone)]
pub struct Sprite {
    pub pixels: Pixels,
    /// Final box and opacity (used when there are no keys).
    pub rect: Rect,
    pub opacity: f32,
    /// Keys in frame pixels (see [`ElementKey`]); empty = the sprite does not move.
    pub keys: Vec<ElementKey>,
}

/// Everything the intro of one page needs besides the sprites' pixels.
#[derive(Debug, Clone)]
pub struct Intro {
    /// Whole frames of the intro; the loop starts with frame `frames`.
    pub frames: usize,
    /// `frames` / fps: where the loop (playlist mark 1, IG composition) starts.
    pub secs: f64,
    /// `secs` in 90 kHz ticks, exact.
    pub ticks: u64,
    /// Time of the last key: the last intro frame shows the state at this time.
    pub end: f64,
    pub sprites: Vec<Sprite>,
    /// Layer (index in the page's `MenuLayer`s) of each slot stream.
    pub slots: Vec<usize>,
}

/// Longest intro played (templates use 3 s).
const MAX_INTRO_SECS: f64 = 10.0;

/// Intro length of a page: its last key or its declared `intro_duration`, whichever is longer.
pub fn intro_length(last_key: f64, page_secs: f64) -> f64 {
    last_key.max(page_secs).clamp(0.5, MAX_INTRO_SECS)
}

/// Intro keys of a graphic whose final box is `rect` (page coordinates): the owning block's box
/// moves and fades per key, the graphic keeps its place inside the block (a zoom about the block's
/// own corner). `map` converts a page box to frame pixels. Empty when nothing moves or the box
/// cannot be mapped.
pub fn graphic_keys(g: &Graphic, rect: Rect, map: impl Fn(f64, f64, f64, f64) -> Rect) -> Vec<ElementKey> {
    let (bx, by, bw, bh) = (g.block_rect.0 as f64, g.block_rect.1 as f64, g.block_rect.2 as f64, g.block_rect.3 as f64);
    if g.intro.is_empty() || bw < 1.0 || bh < 1.0 {
        return Vec::new();
    }
    let final_opacity = g.alpha as f32 / 255.0;
    let keys: Vec<ElementKey> = g
        .intro
        .iter()
        .map(|k| {
            let (sx, sy) = (k.w as f64 / bw, k.h as f64 / bh);
            let (x, y, w, h) = map(k.x as f64 + (rect.0 - bx) * sx, k.y as f64 + (rect.1 - by) * sy, rect.2 * sx, rect.3 * sy);
            let opacity = k.alpha.map_or(final_opacity, |a| a as f32 / 255.0 * g.gfx_alpha as f32 / 255.0);
            ElementKey { t: k.t, x, y, w, h, opacity }
        })
        .collect();
    let (x, y, w, h) = map(rect.0, rect.1, rect.2, rect.3);
    let moves = keys.iter().any(|k| (k.x - x).abs() > 0.5 || (k.y - y).abs() > 0.5 || (k.w - w).abs() > 0.5 || (k.h - h).abs() > 0.5 || (k.opacity - final_opacity).abs() > 0.004);
    if moves { keys } else { Vec::new() }
}

/// Frame rate of a DVD menu (no fake interlacing).
pub fn dvd_rate(tv: TvStandard) -> BdRate {
    BdRate { fps: if tv == TvStandard::Ntsc { (30000, 1001) } else { (25, 1) }, fake_interlaced: false }
}

/// DVD frames per GOP: the cell boundary of an intro must be a VOBU start, so the intro is a whole
/// number of GOPs (3 s fits both standards exactly).
pub fn dvd_gop(tv: TvStandard) -> usize {
    if tv == TvStandard::Ntsc { 18 } else { 15 }
}

/// Frames of an intro of about `secs` seconds on DVD, rounded up to whole GOPs.
pub fn dvd_intro_frames(secs: f64, tv: TvStandard) -> usize {
    let (r, gop) = (dvd_rate(tv), dvd_gop(tv));
    let exact = secs * r.fps.0 as f64 / r.fps.1 as f64;
    ((exact - 1e-6).ceil().max(1.0) as usize).div_ceil(gop) * gop
}

/// Whole frames of an intro of about `secs` seconds. Even if needed so that `frames / fps` is a
/// whole number of 90 kHz ticks (59.94 fps has 1501.5 ticks per frame).
pub fn intro_frames(secs: f64, rate: BdRate) -> usize {
    let (num, den) = (rate.fps.0 as u64, rate.fps.1 as u64);
    let mut n = ((secs * num as f64 / den as f64).round() as u64).max(2);
    while (n * den * 90_000) % num != 0 {
        n += 1;
    }
    n as usize
}

/// Duration of `frames` frames in 90 kHz ticks.
pub fn intro_ticks(frames: usize, rate: BdRate) -> u64 {
    frames as u64 * rate.fps.1 as u64 * 90_000 / rate.fps.0 as u64
}

/// Value of a key track at `t`: held before the first key and after the last, linear between.
/// `fallback` is used without keys.
pub fn lerp(keys: &[ElementKey], fallback: (Rect, f32), t: f64) -> (Rect, f32) {
    let at = |k: &ElementKey| ((k.x, k.y, k.w, k.h), k.opacity);
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return fallback };
    if t <= first.t {
        return at(first);
    }
    if t >= last.t {
        return at(last);
    }
    let i = keys.partition_point(|k| k.t <= t);
    let (a, b) = (&keys[i - 1], &keys[i]);
    let f = if b.t > a.t { (t - a.t) / (b.t - a.t) } else { 1.0 };
    let mix = |p: f64, q: f64| p + (q - p) * f;
    ((mix(a.x, b.x), mix(a.y, b.y), mix(a.w, b.w), mix(a.h, b.h)), mix(a.opacity as f64, b.opacity as f64) as f32)
}

/// Time of intro frame `k` of `n`: the last frame shows the state at `end`, so the seam to the
/// loop (which shows the final state) needs no extra step.
pub fn frame_time(k: usize, n: usize, end: f64) -> f64 {
    if n < 2 { end } else { end * k as f64 / (n - 1) as f64 }
}

/// Draws `src` scaled into `rect` onto the opaque RGBA frame `dst` (`fw`×`fh`).
fn draw(dst: &mut [u8], fw: u32, fh: u32, src: &RgbaImage, rect: Rect, opacity: f32) {
    let op = (opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
    let (x0, y0) = (rect.0.round() as i64, rect.1.round() as i64);
    let (w, h) = (rect.2.round() as i64, rect.3.round() as i64);
    if op == 0 || w < 1 || h < 1 || w > MAX_SIDE || h > MAX_SIDE || x0 >= fw as i64 || y0 >= fh as i64 || x0 + w <= 0 || y0 + h <= 0 {
        return;
    }
    let scaled;
    let src = if src.dimensions() == (w as u32, h as u32) {
        src
    } else {
        scaled = imageops::resize(src, w as u32, h as u32, FilterType::Triangle);
        &scaled
    };
    let (cx0, cx1) = (x0.max(0), (x0 + w).min(fw as i64));
    let (cy0, cy1) = (y0.max(0), (y0 + h).min(fh as i64));
    let stride = src.width() as usize * 4;
    let raw = src.as_raw();
    for y in cy0..cy1 {
        let s_row = &raw[(y - y0) as usize * stride..][..stride];
        let d_row = &mut dst[(y as usize * fw as usize + cx0 as usize) * 4..][..(cx1 - cx0) as usize * 4];
        for (i, d) in d_row.chunks_exact_mut(4).enumerate() {
            let s = &s_row[((cx0 - x0) as usize + i) * 4..][..4];
            let a = (s[3] as u32 * op + 127) / 255;
            if a == 0 {
                continue;
            }
            for c in 0..3 {
                d[c] = ((s[c] as u32 * a + d[c] as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

/// Intro frame at time `t` (RGBA, opaque, `fw`×`fh`): the sprites back to front over `background`,
/// the current raw frame of the page background. `slots[i]` is the current raw frame of slot stream `i`.
pub fn composite_frame(fw: u32, fh: u32, background: &[u8], sprites: &[Sprite], slots: &[&[u8]], t: f64) -> Vec<u8> {
    let mut out = background.to_vec();
    out.resize(fw as usize * fh as usize * 4, 0);
    for s in sprites {
        let (rect, opacity) = lerp(&s.keys, (s.rect, s.opacity), t);
        match &s.pixels {
            Pixels::Image(img) => draw(&mut out, fw, fh, img, rect, opacity),
            Pixels::Slot { index, width, height } => {
                let Some(img) = slots.get(*index).and_then(|raw| RgbaImage::from_raw(*width, *height, raw.to_vec())) else { continue };
                draw(&mut out, fw, fh, &img, rect, opacity);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(t: f64, x: f64, w: f64, opacity: f32) -> ElementKey {
        ElementKey { t, x, y: 0.0, w, h: w, opacity }
    }

    #[test]
    fn keys_hold_before_the_first_and_after_the_last_and_blend_between() {
        let keys = [key(1.0, 10.0, 0.0, 0.0), key(2.0, 30.0, 40.0, 1.0)];
        let fb = ((0.0, 0.0, 1.0, 1.0), 0.5);
        assert_eq!(lerp(&[], fb, 1.0), fb);
        assert_eq!(lerp(&keys, fb, 0.0), ((10.0, 0.0, 0.0, 0.0), 0.0));
        assert_eq!(lerp(&keys, fb, 1.0), ((10.0, 0.0, 0.0, 0.0), 0.0));
        assert_eq!(lerp(&keys, fb, 1.5), ((20.0, 0.0, 20.0, 20.0), 0.5));
        assert_eq!(lerp(&keys, fb, 2.0), ((30.0, 0.0, 40.0, 40.0), 1.0));
        assert_eq!(lerp(&keys, fb, 9.0), ((30.0, 0.0, 40.0, 40.0), 1.0));
    }

    #[test]
    fn dvd_intro_is_whole_gops() {
        assert_eq!(dvd_intro_frames(3.0, TvStandard::Pal), 75);
        assert_eq!(dvd_intro_frames(3.0, TvStandard::Ntsc), 90);
        assert_eq!(dvd_intro_frames(3.1, TvStandard::Pal), 90);
        assert_eq!(dvd_intro_frames(0.5, TvStandard::Ntsc), 18);
        assert_eq!(intro_length(1.0, 3.0), 3.0);
        assert_eq!(intro_length(20.0, 0.0), 10.0);
    }

    #[test]
    fn frames_are_whole_ticks_and_the_last_one_reaches_the_end_state() {
        for (fps, want) in [((24000, 1001), 72), ((25, 1), 75), ((30000, 1001), 90), ((60000, 1001), 180)] {
            let rate = BdRate { fps, fake_interlaced: false };
            let n = intro_frames(3.0, rate);
            assert_eq!(n, want, "{fps:?}");
            assert_eq!(intro_ticks(n, rate) * fps.0 as u64, n as u64 * fps.1 as u64 * 90_000);
        }
        // 59.94 fps: an odd count would end between two ticks.
        assert_eq!(intro_frames(2.99, BdRate { fps: (60000, 1001), fake_interlaced: false }) % 2, 0);
        assert_eq!(frame_time(0, 72, 3.0), 0.0);
        assert_eq!(frame_time(71, 72, 3.0), 3.0);
    }

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba(px))
    }

    fn pixel(frame: &[u8], fw: u32, x: u32, y: u32) -> [u8; 4] {
        let i = (y * fw + x) as usize * 4;
        [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
    }

    #[test]
    fn composite_fades_in_from_black_and_ends_on_the_final_picture() {
        let fade = vec![key(0.0, 0.0, 4.0, 0.0), key(3.0, 0.0, 4.0, 1.0)];
        let sprites = [Sprite { pixels: Pixels::Image(solid(4, 4, [200, 100, 50, 255])), rect: (0.0, 0.0, 4.0, 4.0), opacity: 1.0, keys: fade }];
        let black = [0, 0, 0, 255].repeat(64);
        let first = composite_frame(8, 8, &black, &sprites, &[], frame_time(0, 72, 3.0));
        let last = composite_frame(8, 8, &black, &sprites, &[], frame_time(71, 72, 3.0));
        assert_eq!(pixel(&first, 8, 1, 1), [0, 0, 0, 255]);
        assert_eq!(pixel(&last, 8, 1, 1), [200, 100, 50, 255]);
        assert_eq!(pixel(&last, 8, 6, 6), [0, 0, 0, 255], "outside the sprite stays black");
        let page = [16, 32, 64, 255].repeat(64);
        let blue = composite_frame(8, 8, &page, &sprites, &[], 0.0);
        assert_eq!(pixel(&blue, 8, 1, 1), [16, 32, 64, 255], "the page background shows where nothing is drawn yet");
    }

    #[test]
    fn zero_size_and_off_frame_sprites_are_skipped_and_zoom_scales() {
        let zoom = vec![key(1.0, 4.0, 0.0, 1.0), key(2.0, 0.0, 8.0, 1.0)];
        let sprites = [Sprite { pixels: Pixels::Image(solid(8, 8, [255, 255, 255, 255])), rect: (0.0, 0.0, 8.0, 8.0), opacity: 1.0, keys: zoom }];
        let black = [0, 0, 0, 255].repeat(64);
        assert_eq!(pixel(&composite_frame(8, 8, &black, &sprites, &[], 0.5), 8, 4, 4), [0, 0, 0, 255]);
        let half = composite_frame(8, 8, &black, &sprites, &[], 1.5);
        assert_eq!(pixel(&half, 8, 3, 3), [255, 255, 255, 255]);
        assert_eq!(pixel(&half, 8, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&composite_frame(8, 8, &black, &sprites, &[], 2.0), 8, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn slot_frames_are_drawn_from_the_raw_stream() {
        let raw = [10u8, 10, 10, 255].repeat(2 * 2);
        let sprites = [Sprite { pixels: Pixels::Slot { index: 0, width: 2, height: 2 }, rect: (1.0, 1.0, 2.0, 2.0), opacity: 1.0, keys: vec![] }];
        let f = composite_frame(4, 4, &[0, 0, 0, 255].repeat(16), &sprites, &[&raw], 0.0);
        assert_eq!(pixel(&f, 4, 1, 1), [10, 10, 10, 255]);
        assert_eq!(pixel(&f, 4, 0, 0), [0, 0, 0, 255]);
    }
}
