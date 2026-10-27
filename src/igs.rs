//! Blu-ray Interactive Graphics (IGS) segment writer: ICS (one page, one BOG per button), PDS,
//! ODS and END, in the `.sup` layout (13-byte `PG` header per segment) that tsMuxeR copies as a
//! PGS track. Field layouts follow libbluray's `ig_decode.c`.

/// One HDMV navigation command (12 bytes, see `hdmv.rs`).
pub type Command = [u8; 12];

/// RLE-coded palette image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgObject {
    pub id: u16,
    pub width: u16,
    pub height: u16,
    pub rle: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgButton {
    pub id: u16,
    pub x: u16,
    pub y: u16,
    /// Neighbours; a button's own id means "stay".
    pub up: u16,
    pub down: u16,
    pub left: u16,
    pub right: u16,
    pub selected: u16,
    pub activated: u16,
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgMenu {
    pub width: u16,
    pub height: u16,
    /// Frame-rate code of the video (BD: 1 = 24000/1001, 2 = 24, 3 = 25, 4 = 30000/1001, 6 = 50, 7 = 60000/1001).
    pub frame_rate_code: u8,
    /// Button selected on entry; `None` = 0xFFFF (take it from PSR10, survives menu loops).
    pub default_selected: Option<u16>,
    /// 256 entries of `[Y, Cr, Cb, alpha]`.
    pub palette: Vec<[u8; 4]>,
    pub buttons: Vec<IgButton>,
    pub objects: Vec<IgObject>,
    /// PTS (90 kHz) of the segments; ODS/PDS are decoded before it.
    pub pts: u32,
}

/// Run-length encodes `w × h` palette indices (inverse of `subpic::decode_pgs_rle`).
pub fn rle_encode(px: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for line in px.chunks(w).take(h) {
        let mut i = 0;
        while i < line.len() {
            let c = line[i];
            let mut n = 1;
            while i + n < line.len() && line[i + n] == c && n < 16383 {
                n += 1;
            }
            i += n;
            match (c, n) {
                (0, 1..=63) => out.extend([0, n as u8]),
                (0, _) => out.extend([0, 0x40 | (n >> 8) as u8, n as u8]),
                (_, 1) => out.push(c),
                (_, 2) => out.extend([c, c]),
                (_, 3..=63) => out.extend([0, 0x80 | n as u8, c]),
                _ => out.extend([0, 0xC0 | (n >> 8) as u8, n as u8, c]),
            }
        }
        out.extend([0, 0]);
    }
    out
}

/// BT.709 limited-range `[Y, Cr, Cb]` of an sRGB colour.
pub fn rgb_to_ycrcb(r: u8, g: u8, b: u8) -> [u8; 3] {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    let q = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    [q(16.0 + 219.0 * y), q(128.0 + 224.0 * cr), q(128.0 + 224.0 * cb)]
}

/// One `.sup` segment; `dts` 0 = none.
pub(crate) fn segment(pts: u32, dts: u32, ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(13 + payload.len());
    s.extend(b"PG");
    s.extend(pts.to_be_bytes());
    s.extend(dts.to_be_bytes());
    s.push(ty);
    s.extend((payload.len() as u16).to_be_bytes());
    s.extend(payload);
    s
}

fn ods_segments(pts: u32, o: &IgObject) -> Vec<Vec<u8>> {
    // First fragment: id, version, sequence flag, u24 length (data + 4), w, h, data.
    const MAX: usize = 65535;
    let mut out = Vec::new();
    let total = o.rle.len() + 4;
    let first_room = MAX - 7 - 4;
    let (head, mut rest) = o.rle.split_at(o.rle.len().min(first_room));
    let mut p = Vec::new();
    p.extend(o.id.to_be_bytes());
    p.push(0);
    p.push(if rest.is_empty() { 0xC0 } else { 0x80 });
    p.extend(&(total as u32).to_be_bytes()[1..]);
    p.extend(o.width.to_be_bytes());
    p.extend(o.height.to_be_bytes());
    p.extend(head);
    out.push(segment(pts, 0, 0x15, &p));
    while !rest.is_empty() {
        let (chunk, r) = rest.split_at(rest.len().min(MAX - 4));
        rest = r;
        let mut p = Vec::new();
        p.extend(o.id.to_be_bytes());
        p.push(0);
        p.push(if rest.is_empty() { 0x40 } else { 0x00 });
        p.extend(chunk);
        out.push(segment(pts, 0, 0x15, &p));
    }
    out
}

fn ics_payload(m: &IgMenu) -> Vec<u8> {
    let mut page = Vec::new();
    page.push(0); // page id
    page.push(0); // version
    page.extend(0u64.to_be_bytes()); // uo mask
    page.extend([0, 0, 0, 0]); // in / out effects: no windows, no effects
    page.push(0); // animation frame rate
    page.extend(m.default_selected.unwrap_or(0xFFFF).to_be_bytes());
    page.extend(0xFFFFu16.to_be_bytes()); // default activated
    page.push(0); // palette id
    page.push(m.buttons.len() as u8); // one BOG per button
    for b in &m.buttons {
        page.extend(b.id.to_be_bytes()); // default valid button of the BOG
        page.push(1);
        page.extend(b.id.to_be_bytes());
        page.extend(b.id.to_be_bytes()); // numeric select value
        page.push(0); // auto action
        page.extend(b.x.to_be_bytes());
        page.extend(b.y.to_be_bytes());
        for n in [b.up, b.down, b.left, b.right] {
            page.extend(n.to_be_bytes());
        }
        page.extend(0xFFFFu16.to_be_bytes()); // normal: start / end / repeat (nothing)
        page.extend(0xFFFFu16.to_be_bytes());
        page.push(0);
        page.push(0xFF); // selected sound
        page.extend(b.selected.to_be_bytes());
        page.extend(b.selected.to_be_bytes());
        page.push(0);
        page.push(0xFF); // activated sound
        page.extend(b.activated.to_be_bytes());
        page.extend(b.activated.to_be_bytes());
        page.extend((b.commands.len() as u16).to_be_bytes());
        for c in &b.commands {
            page.extend(c);
        }
    }
    // interactive_composition: stream_model 0 (multiplexed) and ui_model 0 (always on) are one
    // zero byte, followed by the two 33-bit time-out PTS fields (0 = none).
    let mut ic = vec![0u8; 11];
    ic.extend([0, 0, 0]); // user_time_out_duration
    ic.push(1); // pages
    ic.extend(page);
    let mut p = Vec::new();
    p.extend(m.width.to_be_bytes());
    p.extend(m.height.to_be_bytes());
    p.push(m.frame_rate_code << 4);
    p.extend(0u16.to_be_bytes()); // composition number
    p.push(0x80); // epoch start
    p.push(0xC0); // first and last in sequence
    p.extend(&(ic.len() as u32).to_be_bytes()[1..]);
    p.extend(ic);
    p
}

/// The whole `.sup` for tsMuxeR as an `S_HDMV/PGS` track: ICS (disguised as PCS), PDS, one ODS group per object, END.
pub fn build_sup(m: &IgMenu) -> Vec<u8> {
    // 0x16 (PCS) instead of the real ICS type 0x18: tsMuxeR only passes PGS segment types;
    // `bd_patch` relabels it in the muxed clip.
    let mut out = segment(m.pts, 0, 0x16, &ics_payload(m));
    let mut pds = vec![0, 0];
    for (i, e) in m.palette.iter().enumerate().take(256) {
        pds.push(i as u8);
        pds.extend(e);
    }
    out.extend(segment(m.pts, 0, 0x14, &pds));
    for o in &m.objects {
        for s in ods_segments(m.pts, o) {
            out.extend(s);
        }
    }
    out.extend(segment(m.pts, 0, 0x80, &[]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subpic::decode_pgs_rle;

    #[test]
    fn rle_round_trip() {
        let (w, h) = (300usize, 4usize);
        let mut px = vec![0u8; w * h];
        for (i, p) in px.iter_mut().enumerate() {
            *p = match (i % w, i / w) {
                (0..=9, _) => 5,
                (10..=11, _) => 7,
                (12, _) => 9,
                (20..=99, 1) => 3,
                (100..=299, 2) => 4,
                _ => 0,
            };
        }
        assert_eq!(decode_pgs_rle(&rle_encode(&px, w, h), w as u32, h as u32), px);
    }

    #[test]
    fn long_runs_use_the_14_bit_form() {
        let px = vec![0u8; 20_000];
        let rle = rle_encode(&px, 20_000, 1);
        assert_eq!(rle, vec![0, 0x40 | (16383 >> 8) as u8, (16383 & 0xFF) as u8, 0, 0x40 | (3617 >> 8) as u8, (3617 & 0xFF) as u8, 0, 0]);
        assert_eq!(decode_pgs_rle(&rle, 20_000, 1), px);
    }

    fn menu() -> IgMenu {
        IgMenu {
            width: 1920,
            height: 1080,
            frame_rate_code: 1,
            default_selected: Some(1),
            palette: vec![[16, 128, 128, 0], [235, 128, 128, 255]],
            buttons: vec![
                IgButton { id: 1, x: 100, y: 100, up: 1, down: 2, left: 1, right: 1, selected: 1, activated: 1, commands: vec![[0; 12]] },
                IgButton { id: 2, x: 100, y: 300, up: 1, down: 2, left: 2, right: 2, selected: 1, activated: 1, commands: vec![] },
            ],
            objects: vec![IgObject { id: 1, width: 4, height: 2, rle: rle_encode(&[1; 8], 4, 2) }],
            pts: 90_000,
        }
    }

    /// Splits a `.sup` into `(type, payload)`, checking every header.
    fn parse(sup: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < sup.len() {
            assert_eq!(&sup[i..i + 2], b"PG");
            let len = u16::from_be_bytes([sup[i + 11], sup[i + 12]]) as usize;
            v.push((sup[i + 10], sup[i + 13..i + 13 + len].to_vec()));
            i += 13 + len;
        }
        v
    }

    #[test]
    fn segment_order_and_lengths() {
        let segs = parse(&build_sup(&menu()));
        assert_eq!(segs.iter().map(|s| s.0).collect::<Vec<_>>(), vec![0x16, 0x14, 0x15, 0x80]);
        let ics = &segs[0].1;
        assert_eq!(&ics[..5], &[0x07, 0x80, 0x04, 0x38, 0x10]);
        let ic_len = u32::from_be_bytes([0, ics[9], ics[10], ics[11]]) as usize;
        assert_eq!(ics.len(), 12 + ic_len);
        // PDS: id, version, 2 entries of 5 bytes.
        assert_eq!(segs[1].1.len(), 2 + 10);
        // ODS: id, version, flags, u24 length = data + 4, w, h.
        let ods = &segs[2].1;
        assert_eq!(ods[3], 0xC0);
        assert_eq!(u32::from_be_bytes([0, ods[4], ods[5], ods[6]]) as usize, ods.len() - 7);
        assert_eq!(&ods[7..11], &[0, 4, 0, 2]);
    }

    #[test]
    fn big_objects_are_fragmented() {
        let mut m = menu();
        m.objects[0].rle = vec![1; 150_000];
        let segs = parse(&build_sup(&m));
        let ods: Vec<_> = segs.iter().filter(|s| s.0 == 0x15).collect();
        assert_eq!(ods.len(), 3);
        assert_eq!(ods[0].1[3], 0x80);
        assert_eq!(ods[1].1[3], 0x00);
        assert_eq!(ods[2].1[3], 0x40);
        assert_eq!(ods.iter().map(|s| s.1.len()).sum::<usize>(), 150_000 + 11 + 4 + 4);
    }

    #[test]
    fn colour_conversion() {
        assert_eq!(rgb_to_ycrcb(0, 0, 0), [16, 128, 128]);
        assert_eq!(rgb_to_ycrcb(255, 255, 255), [235, 128, 128]);
    }
}
