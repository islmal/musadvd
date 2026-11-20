//! The native text subtitle renderer (`textsub.rs`) against measurements of dvdauthor's `spumux`
//! on the same SRT (arial.ttf, `fontsize=28`, PAL, margins 60/60/20/40), taken once from
//! `spumux.exe` output composited by ffmpeg and checked in here as numbers. The glyph outlines
//! differ (DejaVu Sans vs. hinted Arial), so only placement, extent and ink amount are compared,
//! with tolerances.

use musadvd::author::TvStandard;
use musadvd::textsub::{parse_srt, render_cue};

/// (x0, y0, x1, y1 of the visible pixels, fill pixels, outline pixels) from spumux.
type Box4 = (u32, u32, u32, u32);
const REFERENCE: [(Box4, u32, u32); 3] =
    [((245, 497, 470, 528), 1344, 2546), ((283, 463, 432, 522), 1563, 3212), ((99, 395, 618, 522), 9428, 18718)];

const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nHallo Welt gjpq\n\n2\n00:00:04,000 --> 00:00:06,000\nZeile eins\nZeile zwei\n\n\
3\n00:00:07,000 --> 00:00:09,000\n<i>Kursiv</i> und <b>fett</b> und ein sehr langer Satz der garantiert umgebrochen werden muss weil er viel zu breit ist fuer den Bildschirm\n\n";

#[test]
fn placement_extent_and_ink_match_spumux() {
    let (cues, skipped) = parse_srt(SRT);
    assert_eq!((cues.len(), skipped), (3, 0));
    for (cue, (bbox, fill, outline)) in cues.iter().zip(REFERENCE) {
        let (img, ox, oy) = render_cue(TvStandard::Pal, &cue.lines).unwrap();
        let (mut x0, mut y0, mut x1, mut y1, mut f, mut o) = (u32::MAX, u32::MAX, 0, 0, 0u32, 0u32);
        for (x, y, p) in img.enumerate_pixels() {
            if p.0[3] == 0 {
                continue;
            }
            (x0, y0, x1, y1) = (x0.min(x + ox), y0.min(y + oy), x1.max(x + ox), y1.max(y + oy));
            if p.0[0] == 255 {
                f += 1;
            } else {
                o += 1;
            }
        }
        eprintln!("{:?}: native ({x0},{y0},{x1},{y1}) fill {f} outline {o}; spumux {bbox:?} fill {fill} outline {outline}", cue.lines);
        let (cx, rx) = ((x0 + x1) as i32 / 2, (bbox.0 + bbox.2) as i32 / 2);
        assert!((cx - rx).abs() <= 4, "horizontal centre {cx} vs {rx}");
        assert!((y1 as i32 - bbox.3 as i32).abs() <= 8, "bottom {y1} vs {}", bbox.3);
        let within = |a: u32, b: u32, pct: f64| (a as f64 - b as f64).abs() <= b as f64 * pct;
        assert!(within(f, fill, 0.35), "fill pixels {f} vs {fill}");
        assert!(within(o, outline, 0.35), "outline pixels {o} vs {outline}");
    }
}
