//! Text subtitles as Blu-ray Presentation Graphics: one epoch per cue (epoch-start display set with
//! the rendered text, clear set at the end), in the `.sup` layout of [`crate::igs`]. Segment
//! timestamps follow the decode-time rules tsMuxeR 2.7.0 applied to the same cues (plane clear
//! 32 MB/s, object decode 16 MB/s; verified against its output).

use crate::igs::{rgb_to_ycrcb, rle_encode, segment};
use crate::subpic::Picture;
use crate::textsub::{render_cue_on, Canvas, Cue};

/// Earliest cue start: the epoch's decode time must stay after zero.
const MIN_START: f64 = 0.1;

fn ticks(num: u64, rate: u64) -> u32 {
    (num * 90_000).div_ceil(rate) as u32
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
    for pic in pictures {
        let (img, x, y) = (&pic.image, pic.x, pic.y);
        let start = frame(pic.start.max(MIN_START));
        let end = frame(pic.end).max(start + 1);
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
        let dts = (start as u32).saturating_sub(plane + win + 1);

        let mut pds = vec![0, 0];
        // Index 0: transparent
        pds.extend([0, 16, 128, 128, 0]);
        for (i, c) in palette_colors.iter().enumerate() {
            let [y_c, cr, cb] = rgb_to_ycrcb(c[0], c[1], c[2]);
            pds.extend([(i as u8) + 1, y_c, cr, cb, c[3]]);
        }
        let mut ods = Vec::new();
        ods.extend(0u16.to_be_bytes());
        ods.extend([0, 0xC0]);
        ods.extend(&(rle.len() as u32 + 4).to_be_bytes()[1..]);
        ods.extend((iw as u16).to_be_bytes());
        ods.extend((ih as u16).to_be_bytes());
        ods.extend(&rle);

        out.extend(segment(start as u32, dts, 0x16, &pcs(0x80, &[(x as u16, y as u16)])));
        out.extend(segment(start as u32 - win, dts, 0x17, &wds(x as u16, y as u16, iw as u16, ih as u16)));
        out.extend(segment(dts, 0, 0x14, &pds));
        out.extend(segment(dts + obj, dts, 0x15, &ods));
        out.extend(segment(dts + obj, 0, 0x80, &[]));

        // Clear set: the window is released at `end`.
        let end = end as u32;
        out.extend(segment(end, end - win - 90, 0x16, &pcs(0, &[])));
        out.extend(segment(end - win, end - win - 90, 0x17, &wds(x as u16, y as u16, iw as u16, ih as u16)));
        out.extend(segment(end - 90, 0, 0x80, &[]));
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

    #[test]
    fn early_cues_start_after_zero_and_text_less_cues_are_skipped() {
        let canvas = Canvas::hd(1280, 720);
        let sup = build_sup(&[cue(0.0, 1.0, &["A"]), cue(2.0, 3.0, &[" "])], &canvas, (60000, 1001));
        let segs = parse(&sup);
        assert_eq!(segs.len(), 8);
        assert!(segs[0].2 > 0 && segs[0].1 > segs[0].2);
        assert_eq!(&segs[0].3[..5], &[0x05, 0x00, 0x02, 0xD0, 0x70]);
    }
}
