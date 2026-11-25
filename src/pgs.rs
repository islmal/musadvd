//! Text subtitles as Blu-ray Presentation Graphics: overlapping pictures are partitioned into
//! non-overlapping display intervals (composited per interval so simultaneous cues stay visible
//! together), in the `.sup` layout of [`crate::igs`]. Segment timestamps follow the decode-time
//! rules tsMuxeR 2.7.0 applied to the same cues (plane clear 32 MB/s, object decode 16 MB/s;
//! verified against its output).

use std::borrow::Cow;

use crate::igs::{ods_segments, rgb_to_ycrcb, rle_encode, segment, IgObject};
use crate::subpic::Picture;
use crate::textsub::{render_cue_on, Canvas, Cue};

/// Earliest cue start: the epoch's decode time must stay after zero.
const MIN_START: f64 = 0.1;

fn ticks(num: u64, rate: u64) -> u32 {
    (num * 90_000).div_ceil(rate) as u32
}

struct Interval<'a> {
    start: u32,
    end: u32,
    active: Vec<&'a Picture>,
}

/// Composites `active` pictures onto their union bounding box, clamped to `canvas_size`.
fn composite_pictures(active: &[&Picture], canvas_size: (u32, u32)) -> Option<(image::RgbaImage, u32, u32)> {
    let (cw, ch) = canvas_size;
    let x0 = active.iter().map(|p| p.x).min()?;
    let y0 = active.iter().map(|p| p.y).min()?;
    let x1 = active.iter().map(|p| p.x + p.image.width()).max()?.min(cw);
    let y1 = active.iter().map(|p| p.y + p.image.height()).max()?.min(ch);
    let (bw, bh) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if bw == 0 || bh == 0 {
        return None;
    }
    let mut out = image::RgbaImage::new(bw, bh);
    for p in active {
        let ox = p.x - x0;
        let oy = p.y - y0;
        for (py, row) in p.image.rows().enumerate() {
            let dy = oy + py as u32;
            if dy >= bh {
                break;
            }
            for (px, &src) in row.enumerate() {
                let dx = ox + px as u32;
                if dx >= bw {
                    break;
                }
                let sa = src.0[3] as u32;
                if sa == 0 {
                    continue;
                }
                let dst = out.get_pixel_mut(dx, dy);
                let da = dst.0[3] as u32;
                if sa == 255 || da == 0 {
                    *dst = src;
                } else {
                    let inv = 255 - sa;
                    let out_a = sa + (da * inv + 127) / 255;
                    if out_a > 0 {
                        for c in 0..3 {
                            dst.0[c] = ((src.0[c] as u32 * sa * 255 + dst.0[c] as u32 * da * inv + out_a * 127) / (out_a * 255)).min(255) as u8;
                        }
                        dst.0[3] = out_a.min(255) as u8;
                    }
                }
            }
        }
    }
    Some((out, x0, y0))
}

/// Frame-rate code of a PCS (BD: 1 = 24000/1001 … 7 = 60000/1001).
pub fn frame_rate_code(fps: (u32, u32)) -> u8 {
    match fps {
        (24000, 1001) => 1,
        (24, 1) => 2,
        (25, 1) => 3,
        (30000, 1001) => 4,
        (50, 1) => 6,
        _ => 7,
    }
}

/// `.sup` of `pictures` on canvas `(w, h)` for a video of `fps`; picture times snap to video frames.
pub fn build_sup_from_pictures(pictures: &[Picture], canvas_size: (u32, u32), fps: (u32, u32)) -> Vec<u8> {
    let (w, h) = canvas_size;
    let code = frame_rate_code(fps) << 4;
    let frame = |t: f64| ((t * fps.0 as f64 / fps.1 as f64).round() as u64 * fps.1 as u64 * 90_000).div_ceil(fps.0 as u64);
    let norm: Vec<(u32, u32, &Picture)> = pictures
        .iter()
        .filter(|p| p.image.width() > 0 && p.image.height() > 0 && p.x < w && p.y < h)
        .map(|p| {
            let start = frame(p.start.max(MIN_START)) as u32;
            let end = (frame(p.end) as u32).max(start + 1);
            (start, end, p)
        })
        .collect();
    if norm.is_empty() {
        return Vec::new();
    }

    let mut bounds: Vec<u32> = Vec::with_capacity(norm.len() * 2);
    for &(s, e, _) in &norm {
        bounds.push(s);
        bounds.push(e);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut intervals: Vec<Interval<'_>> = Vec::new();
    for win in bounds.windows(2) {
        let (t0, t1) = (win[0], win[1]);
        let active: Vec<&Picture> = norm.iter().filter(|&&(s, e, _)| s <= t0 && e >= t1).map(|&(_, _, p)| p).collect();
        if !active.is_empty() {
            intervals.push(Interval { start: t0, end: t1, active });
        }
    }

    let plane = ticks(w as u64 * h as u64, 32_000_000);
    let mut out = Vec::new();
    let mut number = 0u16;
    let mut pcs = |state: u8, objects: &[(u16, u16)]| {
        let mut p = Vec::new();
        p.extend((w as u16).to_be_bytes());
        p.extend((h as u16).to_be_bytes());
        p.extend([code]);
        p.extend(number.to_be_bytes());
        p.extend([state, 0, 0, objects.len() as u8]);
        for &(x, y) in objects {
            p.extend([0, 0, 0, 0]); // object 0, window 0, not cropped
            p.extend(x.to_be_bytes());
            p.extend(y.to_be_bytes());
        }
        number = number.wrapping_add(1);
        p
    };
    let wds = |x: u16, y: u16, ww: u16, wh: u16| {
        let mut p = vec![1, 0];
        for v in [x, y, ww, wh] {
            p.extend(v.to_be_bytes());
        }
        p
    };
    let mut prev_end_pts = 0u32;
    for (idx, iv) in intervals.iter().enumerate() {
        let (img, x, y): (Cow<'_, image::RgbaImage>, u32, u32) = if iv.active.len() == 1 {
            let p = iv.active[0];
            (Cow::Borrowed(&p.image), p.x, p.y)
        } else {
            let Some((img, x, y)) = composite_pictures(&iv.active, canvas_size) else {
                continue;
            };
            (Cow::Owned(img), x, y)
        };
        let (start, end) = (iv.start, iv.end);
        let (iw, ih) = (img.width() as usize, img.height() as usize);
        if iw == 0 || ih == 0 {
            continue;
        }

        // Collect distinct non-transparent colors (up to 254)
        let mut palette_colors: Vec<[u8; 4]> = Vec::new();
        for p in img.pixels() {
            if p.0[3] > 0 && !palette_colors.contains(&p.0) {
                palette_colors.push(p.0);
                if palette_colors.len() >= 254 {
                    break;
                }
            }
        }
        // Sort colors by brightness descending so white/bright fill is early
        palette_colors.sort_by_key(|c| std::cmp::Reverse(299 * c[0] as u32 + 587 * c[1] as u32 + 114 * c[2] as u32));

        let px: Vec<u8> = img
            .pixels()
            .map(|p| {
                if p.0[3] == 0 {
                    0
                } else if let Some(pos) = palette_colors.iter().position(|&c| c == p.0) {
                    (pos as u8) + 1
                } else {
                    1
                }
            })
            .collect();
        let rle = rle_encode(&px, iw, ih);
        let area = (iw * ih) as u64;
        let (win, obj) = (ticks(area, 32_000_000), ticks(area, 16_000_000));
        let dts = start.saturating_sub(plane + win + 1).max(prev_end_pts + 1).min(start - 1);
        let wds_pts = start.saturating_sub(win).max(dts);
        let end_pts = (dts + obj).min(start - 1);

        let mut pds = vec![0, 0];
        // Index 0: transparent
        pds.extend([0, 16, 128, 128, 0]);
        for (i, c) in palette_colors.iter().enumerate() {
            let [y_c, cr, cb] = rgb_to_ycrcb(c[0], c[1], c[2]);
            pds.extend([(i as u8) + 1, y_c, cr, cb, c[3]]);
        }
        // Busy cues (outlines, anti-aliasing, typesetting) easily exceed one 64 KiB segment.
        let ods = ods_segments(end_pts, dts, &IgObject { id: 0, width: iw as u16, height: ih as u16, rle });

        out.extend(segment(start, dts, 0x16, &pcs(0x80, &[(x as u16, y as u16)])));
        out.extend(segment(wds_pts, dts, 0x17, &wds(x as u16, y as u16, iw as u16, ih as u16)));
        out.extend(segment(dts, 0, 0x14, &pds));
        out.extend(ods.concat());
        out.extend(segment(end_pts, 0, 0x80, &[]));
        prev_end_pts = end_pts;

        // Clear set: only needed if there is a gap before the next interval (or at the end).
        let next_starts_at_end = intervals.get(idx + 1).is_some_and(|next| next.start == end);
        if !next_starts_at_end {
            let clear_end_pts = end.saturating_sub(90).max(prev_end_pts + 1).min(end);
            let clear_dts = end.saturating_sub(win + 90).max(prev_end_pts + 1).min(clear_end_pts);
            let clear_wds_pts = end.saturating_sub(win).max(clear_dts);
            out.extend(segment(end, clear_dts, 0x16, &pcs(0, &[])));
            out.extend(segment(clear_wds_pts, clear_dts, 0x17, &wds(x as u16, y as u16, iw as u16, ih as u16)));
            out.extend(segment(clear_end_pts, 0, 0x80, &[]));
            prev_end_pts = clear_end_pts;
        }
    }
    out
}

/// `.sup` of `cues` on `canvas` for a video of `fps`; cue times snap to video frames.
pub fn build_sup(cues: &[Cue], canvas: &Canvas, fps: (u32, u32)) -> Vec<u8> {
    let pictures: Vec<Picture> = cues
        .iter()
        .filter_map(|cue| {
            let (image, x, y) = render_cue_on(canvas, &cue.lines)?;
            Some(Picture { start: cue.start, end: cue.end, forced: false, x, y, image })
        })
        .collect();
    build_sup_from_pictures(&pictures, canvas.size, fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(type, pts, dts, payload)` of every segment.
    pub(crate) fn parse(sup: &[u8]) -> Vec<(u8, u32, u32, Vec<u8>)> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < sup.len() {
            assert_eq!(&sup[i..i + 2], b"PG");
            let n = |at: usize| u32::from_be_bytes(sup[i + at..i + at + 4].try_into().unwrap());
            let len = u16::from_be_bytes([sup[i + 11], sup[i + 12]]) as usize;
            v.push((sup[i + 10], n(2), n(6), sup[i + 13..i + 13 + len].to_vec()));
            i += 13 + len;
        }
        v
    }

    fn cue(start: f64, end: f64, lines: &[&str]) -> Cue {
        Cue { start, end, lines: lines.iter().map(|l| l.to_string()).collect() }
    }

    #[test]
    fn two_sets_per_cue_with_decode_times() {
        let canvas = Canvas::hd(1920, 1080);
        let sup = build_sup(&[cue(0.5, 1.2, &["Hallo Welt"])], &canvas, (24000, 1001));
        let segs = parse(&sup);
        assert_eq!(segs.iter().map(|s| s.0).collect::<Vec<_>>(), vec![0x16, 0x17, 0x14, 0x15, 0x80, 0x16, 0x17, 0x80]);
        let (pcs, wds, pds, ods, end) = (&segs[0], &segs[1], &segs[2], &segs[3], &segs[4]);
        // 0.5 s and 1.2 s snap to frames 12 and 29.
        assert_eq!(pcs.1, 45045);
        assert_eq!(segs[5].1, 108859);
        // Epoch start, one object; the window is cleared before the PCS is shown.
        assert_eq!(pcs.3[7], 0x80);
        assert_eq!(pcs.3[10], 1);
        assert!(wds.1 < pcs.1 && wds.2 == pcs.2);
        // Palette and object are decoded after the plane is cleared: 1920x1080 / 32 MB/s = 5832 ticks.
        assert_eq!(pds.1, pcs.2);
        assert!(pcs.1 - pcs.2 > 5832);
        assert!(ods.1 > pds.1 && end.1 == ods.1);
        // Clear set: normal state, no objects.
        assert_eq!((segs[5].3[7], segs[5].3[10]), (0, 0));
        assert_eq!(segs[7].1, segs[5].1 - 90);
        // Declared ODS length matches its data.
        let o = &ods.3;
        assert_eq!(u32::from_be_bytes([0, o[4], o[5], o[6]]) as usize, o.len() - 7);
        assert_eq!(o[3], 0xC0);
    }

    /// A 400x200 picture of per-pixel noise over 200 colours: about one RLE byte per pixel.
    fn noisy_picture(start: f64, end: f64) -> Picture {
        let mut seed = 1u32;
        let image = image::RgbaImage::from_fn(400, 200, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let v = (seed >> 16) % 200;
            image::Rgba([v as u8, 255 - v as u8, 40, 255])
        });
        Picture { start, end, forced: false, x: 100, y: 800, image }
    }

    #[test]
    fn objects_over_64k_are_fragmented_and_round_trip() {
        let pics = [noisy_picture(1.0, 2.0), noisy_picture(3.0, 4.0)];
        let sup = build_sup_from_pictures(&pics, (1920, 1080), (24000, 1001));
        crate::bd_mux::validate_sup(&sup).unwrap();
        let segs = parse(&sup);
        let ods: Vec<_> = segs.iter().filter(|s| s.0 == 0x15).collect();
        assert_eq!(ods.len(), 4, "two fragments per object");
        assert_eq!((ods[0].3[3], ods[1].3[3]), (0x80, 0x40));
        // Every fragment of an object shares the decode stamps.
        assert_eq!((ods[0].1, ods[0].2), (ods[1].1, ods[1].2));
        let decoded = crate::subpic::decode_pgs(&sup).unwrap();
        assert_eq!(decoded.pictures.len(), 2);
        for (d, p) in decoded.pictures.iter().zip(&pics) {
            assert_eq!((d.x, d.y, d.image.dimensions()), (p.x, p.y, p.image.dimensions()));
            // Colours pass through lossy YCrCb, so check the RLE round trip as a consistent colour map.
            let mut map = std::collections::HashMap::new();
            for (a, b) in p.image.pixels().zip(d.image.pixels()) {
                assert_eq!(*map.entry(a.0).or_insert(b.0), b.0);
            }
            assert_eq!(map.len(), 200);
        }
    }

    #[test]
    fn early_cues_start_after_zero_and_text_less_cues_are_skipped() {
        let canvas = Canvas::hd(1280, 720);
        let sup = build_sup(&[cue(0.0, 1.0, &["A"]), cue(2.0, 3.0, &[" "])], &canvas, (60000, 1001));
        let segs = parse(&sup);
        assert_eq!(segs.len(), 8);
        assert!(segs[0].2 > 0 && segs[0].1 > segs[0].2);
        assert_eq!(&segs[0].3[..5], &[0x05, 0x00, 0x02, 0xD0, 0x70]);
    }

    #[test]
    fn overlapping_cues_partition_and_composite_together() {
        let canvas = Canvas::hd(1920, 1080);
        let top = cue(1.0, 4.0, &["{\\an8}Top sign"]);
        let bottom = cue(2.0, 3.0, &["Bottom line"]);
        let sup = build_sup(&[top, bottom], &canvas, (24000, 1001));
        crate::bd_mux::validate_sup(&sup).unwrap();

        // DTS (or PTS when DTS == 0) must be non-decreasing across all segments.
        let segs = parse(&sup);
        let mut last_dts = 0u32;
        for s in &segs {
            let eff_dts = if s.2 == 0 { s.1 } else { s.2 };
            assert!(eff_dts > 0 && eff_dts >= last_dts, "non-monotonic dts: {eff_dts} < {last_dts}");
            last_dts = eff_dts;
        }

        let decoded = crate::subpic::decode_pgs(&sup).unwrap();
        assert_eq!(decoded.pictures.len(), 3, "expected 3 intervals: [1..2, 2..3, 3..4]");
        let (p0, p1, p2) = (&decoded.pictures[0], &decoded.pictures[1], &decoded.pictures[2]);
        assert!((p0.start - 1.0).abs() < 0.05 && (p0.end - 2.0).abs() < 0.05);
        assert!((p1.start - 2.0).abs() < 0.05 && (p1.end - 3.0).abs() < 0.05);
        assert!((p2.start - 3.0).abs() < 0.05 && (p2.end - 4.0).abs() < 0.05);

        // During [2..3], both top and bottom cues are visible in the same composited picture.
        assert!(p0.y < 300 && p0.image.height() < 200);
        assert_eq!((p2.x, p2.y, p2.image.dimensions()), (p0.x, p0.y, p0.image.dimensions()));
        assert!(p1.y <= p0.y && p1.y + p1.image.height() > 700, "composited picture spans top to bottom: y={}, h={}", p1.y, p1.image.height());
        let has_top = p1.image.rows().take(p0.image.height() as usize).any(|mut r| r.any(|px| px.0[3] > 0));
        let has_bottom = p1.image.rows().skip((p1.image.height() / 2) as usize).any(|mut r| r.any(|px| px.0[3] > 0));
        assert!(has_top && has_bottom, "both cues must have visible pixels in the overlap interval");
    }

    #[test]
    fn three_way_overlap_and_adjacent_cues_produce_monotonic_intervals() {
        let canvas = Canvas::hd(1920, 1080);
        let cues = [
            cue(1.0, 3.0, &["{\\an8}One"]),
            cue(1.5, 2.5, &["{\\an5}Two"]),
            cue(2.0, 4.0, &["Three"]),
            cue(4.0, 5.0, &["Four"]),
        ];
        let sup = build_sup(&cues, &canvas, (24000, 1001));
        crate::bd_mux::validate_sup(&sup).unwrap();
        let decoded = crate::subpic::decode_pgs(&sup).unwrap();
        assert_eq!(decoded.pictures.len(), 6);
        for w in decoded.pictures.windows(2) {
            assert!(w[0].end <= w[1].start + 1e-6, "pictures must not overlap: {:?} vs {:?}", (w[0].start, w[0].end), (w[1].start, w[1].end));
        }
    }

    #[test]
    fn identical_timestamps_composite_into_single_epoch() {
        let canvas = Canvas::hd(1920, 1080);
        let cues = [cue(1.0, 2.5, &["{\\an8}Top"]), cue(1.0, 2.5, &["Bottom"])];
        let sup = build_sup(&cues, &canvas, (24000, 1001));
        crate::bd_mux::validate_sup(&sup).unwrap();
        let decoded = crate::subpic::decode_pgs(&sup).unwrap();
        assert_eq!(decoded.pictures.len(), 1);
        assert!(decoded.pictures[0].y < 300 && decoded.pictures[0].y + decoded.pictures[0].image.height() > 700);
    }

    #[test]
    fn off_canvas_adjacent_picture_still_clears_previous_cue() {
        let on_canvas = Picture {
            start: 1.0,
            end: 2.0,
            forced: false,
            x: 100,
            y: 100,
            image: image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 255, 255, 255])),
        };
        let off_canvas = Picture {
            start: 2.0,
            end: 3.0,
            forced: false,
            x: 2000,
            y: 100,
            image: image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 255, 255, 255])),
        };
        let sup = build_sup_from_pictures(&[on_canvas, off_canvas], (1920, 1080), (24000, 1001));
        crate::bd_mux::validate_sup(&sup).unwrap();
        let decoded = crate::subpic::decode_pgs(&sup).unwrap();
        assert_eq!(decoded.pictures.len(), 1);
        assert!((decoded.pictures[0].end - 2.0).abs() < 0.05);
    }
}
