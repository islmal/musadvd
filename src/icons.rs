//! Built-in menu symbols (`assets/icons/*.svg`) rasterized with resvg.

use image::RgbaImage;
use resvg::{tiny_skia, usvg};

macro_rules! icons {
    ($($name:literal),*) => {
        pub const ICON_NAMES: &[&str] = &[$($name),*];
        /// Source SVG of a built-in icon (24x24 viewBox, drawn with `currentColor`).
        pub fn icon_svg(name: &str) -> Option<&'static str> {
            match name {
                $($name => Some(include_str!(concat!("../assets/icons/", $name, ".svg"))),)*
                _ => None,
            }
        }
    };
}
icons!(
    "next", "prev", "home", "play", "subtitles", "audio", "back", "arrowLeft", "arrowRight", "chapters", "settings", "star",
    "framePill", "frameRect", "pointer", "bullet", "underline", "brackets"
);

/// Icons that fill the whole element box (see `icon_svg_sized`) instead of being contain-fitted.
pub const STRETCH_ICONS: &[&str] = &["framePill", "frameRect", "underline", "brackets"];

/// Generated SVG (`viewBox="0 0 w h"`, `currentColor`) of a stretch icon for a `w`x`h` box, so
/// strokes and corner radii keep their proportions at any aspect. None for other icons.
/// The formulas are mirrored in `frontend/src/icons.ts` (`stretchSvg`); keep both in sync.
pub fn icon_svg_sized(name: &str, w: f32, h: f32) -> Option<String> {
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) || !STRETCH_ICONS.contains(&name) {
        return None;
    }
    let sw = (w.min(h) * 0.08).clamp(1.5, 12.0).min(w.min(h) / 2.0);
    let (hs, iw, ih) = (sw / 2.0, w - sw, h - sw);
    let body = match name {
        "framePill" => format!(r#"<rect x="{hs:.3}" y="{hs:.3}" width="{iw:.3}" height="{ih:.3}" rx="{:.3}" fill="none" stroke="currentColor" stroke-width="{sw:.3}"/>"#, ih / 2.0),
        "frameRect" => format!(r#"<rect x="{hs:.3}" y="{hs:.3}" width="{iw:.3}" height="{ih:.3}" rx="{hs:.3}" fill="none" stroke="currentColor" stroke-width="{sw:.3}"/>"#),
        "underline" => format!(r#"<rect x="0" y="{:.3}" width="{w:.3}" height="{sw:.3}" rx="{hs:.3}" fill="currentColor"/>"#, h - sw),
        _ => {
            let a = w.min(h) * 0.3;
            let (r, b) = (w - hs, h - hs);
            format!(r#"<path d="M{:.3} {hs:.3}H{hs:.3}V{b:.3}H{:.3}M{:.3} {hs:.3}H{r:.3}V{b:.3}H{:.3}" fill="none" stroke="currentColor" stroke-width="{sw:.3}"/>"#, hs + a, hs + a, r - a, r - a)
        }
    };
    Some(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:.3} {h:.3}" width="{w:.3}" height="{h:.3}">{body}</svg>"#))
}

/// Rasterizes `name` in `color`, contain-fitted and centred into `w`x`h`; the symbol is squeezed
/// horizontally by 1 / `pixel_aspect` so it looks round on the TV.
pub fn render_icon(name: &str, color: [u8; 3], w: u32, h: u32, pixel_aspect: f64) -> Option<RgbaImage> {
    if w == 0 || h == 0 {
        return None;
    }
    let hex = format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]);
    let pa = if pixel_aspect.is_finite() && pixel_aspect > 0.0 { pixel_aspect } else { 1.0 };
    // Stretch icons: generated for the square-on-TV box (w * pa by h), then squeezed back to w.
    let sized = icon_svg_sized(name, w as f32 * pa as f32, h as f32);
    let svg = match &sized {
        Some(s) => s.replace("currentColor", &hex),
        None => icon_svg(name)?.replace("currentColor", &hex),
    };
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let size = tree.size();
    // Square on the TV: width in frame pixels is the height divided by the pixel aspect.
    let f = (w as f32 * pa as f32 / size.width()).min(h as f32 / size.height());
    let (sx, sy) = if sized.is_some() { (1.0 / pa as f32, 1.0) } else { (f / pa as f32, f) };
    let (dw, dh) = (size.width() * sx, size.height() * sy);
    let mut pix = tiny_skia::Pixmap::new(w, h)?;
    let t = tiny_skia::Transform::from_scale(sx, sy).post_translate((w as f32 - dw) / 2.0, (h as f32 - dh) / 2.0);
    resvg::render(&tree, t, &mut pix.as_mut());
    // tiny-skia is premultiplied; demultiply into straight RGBA.
    let mut out = RgbaImage::new(w, h);
    for (o, p) in out.pixels_mut().zip(pix.pixels()) {
        let c = p.demultiply();
        *o = image::Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_and_renders() {
        for n in ICON_NAMES {
            let img = render_icon(n, [255, 0, 0], 64, 64, 1.0).unwrap_or_else(|| panic!("{n}"));
            assert_eq!(img.dimensions(), (64, 64));
            assert!(img.pixels().any(|p| p[3] > 200), "{n} has no solid pixel");
        }
        assert!(render_icon("nope", [0; 3], 8, 8, 1.0).is_none());
        assert!(icon_svg("nope").is_none());
    }

    #[test]
    fn stretch_icons_fill_the_box() {
        for n in STRETCH_ICONS {
            assert!(icon_svg_sized(n, 200.0, 60.0).is_some() && icon_svg(n).is_some(), "{n}");
        }
        assert!(icon_svg_sized("home", 200.0, 60.0).is_none() && icon_svg_sized("frameRect", 0.0, 5.0).is_none());
        let img = render_icon("frameRect", [255; 3], 200, 60, 1.0).unwrap();
        for (x, y) in [(100, 0), (100, 59), (0, 30), (199, 30)] {
            assert!(img.get_pixel(x, y)[3] > 200, "edge {x},{y}");
        }
        assert_eq!(img.get_pixel(100, 30)[3], 0);
        let u = render_icon("underline", [255; 3], 200, 60, 1.0).unwrap();
        assert!(u.enumerate_pixels().all(|(_, y, p)| p[3] == 0 || y >= 50));
        assert!(u.get_pixel(100, 58)[3] > 200 && u.get_pixel(2, 58)[3] > 100);
        let pill = render_icon("framePill", [255; 3], 200, 60, 1.0).unwrap();
        assert_eq!(pill.get_pixel(0, 0)[3], 0);
        let b = render_icon("brackets", [255; 3], 200, 60, 2.0).unwrap();
        assert!(b.get_pixel(1, 30)[3] > 200 && b.get_pixel(198, 30)[3] > 200 && b.get_pixel(100, 30)[3] == 0);
    }

    #[test]
    fn colour_size_and_centering() {
        let img = render_icon("home", [10, 200, 30], 120, 60, 1.0).unwrap();
        assert_eq!(img.dimensions(), (120, 60));
        let lit: Vec<(u32, u32)> = img.enumerate_pixels().filter(|(_, _, p)| p[3] > 200).map(|(x, y, _)| (x, y)).collect();
        assert!(!lit.is_empty());
        let p = img.enumerate_pixels().find(|(_, _, p)| p[3] == 255).unwrap().2;
        assert_eq!([p[0], p[1], p[2]], [10, 200, 30]);
        // Contain-fitted square in the middle of the wide box: nothing in the side margins.
        assert!(lit.iter().all(|(x, _)| (30..90).contains(x)), "{lit:?}");
        let cx = lit.iter().map(|(x, _)| *x as f64).sum::<f64>() / lit.len() as f64;
        assert!((cx - 60.0).abs() < 4.0, "{cx}");
    }

    #[test]
    fn pixel_aspect_squeezes_horizontally() {
        let img = render_icon("play", [255; 3], 100, 100, 2.0).unwrap();
        let xs: Vec<u32> = img.enumerate_pixels().filter(|(_, _, p)| p[3] > 50).map(|(x, _, _)| x).collect();
        let (lo, hi) = (*xs.iter().min().unwrap(), *xs.iter().max().unwrap());
        assert!(hi - lo < 60, "{lo}..{hi}");
    }
}
