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
icons!("next", "prev", "home", "play", "subtitles", "audio", "back");

/// Rasterizes `name` in `color`, contain-fitted and centred into `w`x`h`; the symbol is squeezed
/// horizontally by 1 / `pixel_aspect` so it looks round on the TV.
pub fn render_icon(name: &str, color: [u8; 3], w: u32, h: u32, pixel_aspect: f64) -> Option<RgbaImage> {
    if w == 0 || h == 0 {
        return None;
    }
    let hex = format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]);
    let svg = icon_svg(name)?.replace("currentColor", &hex);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let pa = if pixel_aspect.is_finite() && pixel_aspect > 0.0 { pixel_aspect } else { 1.0 };
    // Square on the TV: width in frame pixels is the height divided by the pixel aspect.
    let f = (w as f32 * pa as f32 / size.width()).min(h as f32 / size.height());
    let (sx, sy) = (f / pa as f32, f);
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
