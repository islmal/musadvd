//! Bitmap subtitles (Blu-ray PGS, DVD VobSub) → DVD subpictures.
//!
//! The source stream is extracted by ffmpeg (PGS as a raw `.sup`) or ffprobe (VobSub packets as
//! a hex dump, see [`parse_vobsub_probe`]) and decoded here. Every displayed picture is mapped
//! from the subtitle canvas onto the picture area of the DVD frame, reduced to transparent + 3
//! colours (the DVD subpicture limit) and written as a palette PNG; [`render`] returns the
//! spumux XML (`<spu image=…>`) that the encoder's spumux pipeline muxes like a text subtitle.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use image::{imageops, imageops::FilterType, Rgba, RgbaImage};

use crate::author::{xml_escape, TvStandard};
use crate::menu::IndexedImage;

/// PGS/VobSub timestamps tick at 90 kHz.
const CLOCK: f64 = 90_000.0;
/// Display time of a last picture that is never cleared.
const LAST_PICTURE_SECS: f64 = 5.0;
/// Pictures shorter than this are dropped (spumux needs increasing, non-empty intervals).
const MIN_PICTURE_SECS: f64 = 0.04;

/// Bitmap subtitle codecs this module converts.
pub fn supported(codec: &str) -> bool {
    matches!(codec, "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" | "dvb_teletext")
}

/// A `.sup` stream with one empty display set (presentation composition with no objects, then
/// END) for a canvas of `width`×`height`: a Blu-ray subtitle track without any picture.
pub fn empty_pgs(width: u16, height: u16) -> Vec<u8> {
    let seg = |kind: u8, payload: &[u8]| {
        let mut s = b"PG".to_vec();
        s.extend([0u8; 8]); // PTS and DTS 0
        s.push(kind);
        s.extend((payload.len() as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    };
    let mut pcs = Vec::new();
    pcs.extend(width.to_be_bytes());
    pcs.extend(height.to_be_bytes());
    // Frame rate code 0x10 (24 fps), composition number 1, epoch start, no palette update,
    // palette 0, zero objects.
    pcs.extend([0x10, 0, 1, 0x80, 0, 0, 0]);
    let mut out = seg(0x16, &pcs);
    out.extend(seg(0x80, &[]));
    out
}

#[derive(Debug)]
pub enum SubpicError {
    /// The subtitle data could not be parsed.
    Format(String),
    Io { path: PathBuf, error: std::io::Error },
}

impl fmt::Display for SubpicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubpicError::Format(m) => write!(f, "Bild-Untertitel: {m}"),
            SubpicError::Io { path, error } => write!(f, "Bild-Untertitel {}: {error}", path.display()),
        }
    }
}

impl std::error::Error for SubpicError {}

fn bad(m: impl Into<String>) -> SubpicError {
    SubpicError::Format(m.into())
}

/// One displayed subtitle picture on the source canvas.
#[derive(Debug, Clone)]
pub struct Picture {
    pub start: f64,
    pub end: f64,
    /// Shown even when subtitles are off (forced subtitles).
    pub forced: bool,
    /// Top-left corner on the canvas.
    pub x: u32,
    pub y: u32,
    pub image: RgbaImage,
}

/// A decoded subtitle stream.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// Coordinate space of the pictures (PGS: video size; VobSub: 720×576/480).
    pub canvas: (u32, u32),
    pub pictures: Vec<Picture>,
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// Limited-range Y'CbCr (BT.709 for HD canvases, BT.601 otherwise) to RGB.
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8, hd: bool) -> [u8; 3] {
    let y = (y as f64 - 16.0) * 255.0 / 219.0;
    let cb = (cb as f64 - 128.0) * 255.0 / 224.0;
    let cr = (cr as f64 - 128.0) * 255.0 / 224.0;
    let (r, g, b) = if hd {
        (y + 1.5748 * cr, y - 0.187324 * cb - 0.468124 * cr, y + 1.8556 * cb)
    } else {
        (y + 1.402 * cr, y - 0.344136 * cb - 0.714136 * cr, y + 1.772 * cb)
    };
    let c = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    [c(r), c(g), c(b)]
}

// ---------------------------------------------------------------------------------------------
// PGS (HDMV Presentation Graphic Stream)
// ---------------------------------------------------------------------------------------------

/// One composition object of a PGS presentation segment.
#[derive(Debug, Clone)]
struct PgsPlacement {
    object: u16,
    forced: bool,
    x: u32,
    y: u32,
    /// (x, y, w, h) inside the object.
    crop: Option<(u32, u32, u32, u32)>,
}

#[derive(Debug, Clone)]
struct Pcs {
    pts: f64,
    palette: u8,
    objects: Vec<PgsPlacement>,
}

#[derive(Debug, Clone, Default)]
struct PgsObject {
    width: u32,
    height: u32,
    rle: Vec<u8>,
}

/// Decodes PGS run-length data into `width × height` palette indices.
pub fn decode_pgs_rle(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut px: Vec<u8> = Vec::with_capacity(w * h);
    let mut line = 0;
    let mut i = 0;
    let next = |i: &mut usize| -> u8 {
        let v = data.get(*i).copied().unwrap_or(0);
        *i += 1;
        v
    };
    while i < data.len() && line < h {
        let b = next(&mut i);
        if b != 0 {
            px.push(b);
            continue;
        }
        let b2 = next(&mut i);
        if b2 == 0 {
            // End of line: pad short lines, cut overlong ones.
            line += 1;
            px.resize(line * w, 0);
            continue;
        }
        let low = (b2 & 0x3F) as usize;
        let (len, color) = match b2 >> 6 {
            0 => (low, 0),
            1 => ((low << 8) | next(&mut i) as usize, 0),
            2 => (low, next(&mut i)),
            _ => ((low << 8) | next(&mut i) as usize, next(&mut i)),
        };
        px.extend(std::iter::repeat_n(color, len));
    }
    px.resize(w * h, 0);
    px
}

/// Decodes a raw `.sup` stream (segments with the 13-byte `PG` header).
pub fn decode_pgs(sup: &[u8]) -> Result<Decoded, SubpicError> {
    let mut canvas = (1920, 1080);
    let mut palettes: HashMap<u8, [[u8; 4]; 256]> = HashMap::new();
    let mut objects: HashMap<u16, PgsObject> = HashMap::new();
    let mut pending: Option<Pcs> = None;
    let mut shown: Option<Picture> = None;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 13 <= sup.len() {
        if &sup[pos..pos + 2] != b"PG" {
            return Err(bad(format!("kein PGS-Segment bei Byte {pos}")));
        }
        let pts = be32(sup, pos + 2).unwrap_or(0) as f64 / CLOCK;
        let kind = sup[pos + 10];
        let size = be16(sup, pos + 11).unwrap_or(0) as usize;
        let p = sup.get(pos + 13..pos + 13 + size).ok_or_else(|| bad("PGS-Segment abgeschnitten"))?;
        pos += 13 + size;
        match kind {
            // Presentation composition.
            0x16 => {
                let (Some(w), Some(h)) = (be16(p, 0), be16(p, 2)) else { continue };
                canvas = (w as u32, h as u32);
                let state = p.get(7).copied().unwrap_or(0);
                if state & 0x80 != 0 {
                    // Epoch start: earlier objects and palettes are gone.
                    objects.clear();
                    palettes.clear();
                }
                let palette = p.get(9).copied().unwrap_or(0);
                let count = p.get(10).copied().unwrap_or(0) as usize;
                let mut at = 11;
                let mut list = Vec::with_capacity(count);
                for _ in 0..count {
                    let (Some(object), Some(&flags), Some(x), Some(y)) = (be16(p, at), p.get(at + 3), be16(p, at + 4), be16(p, at + 6))
                    else {
                        break;
                    };
                    at += 8;
                    let crop = if flags & 0x80 != 0 {
                        let c = (be16(p, at), be16(p, at + 2), be16(p, at + 4), be16(p, at + 6));
                        at += 8;
                        match c {
                            (Some(a), Some(b), Some(c), Some(d)) => Some((a as u32, b as u32, c as u32, d as u32)),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    list.push(PgsPlacement { object, forced: flags & 0x40 != 0, x: x as u32, y: y as u32, crop });
                }
                pending = Some(Pcs { pts, palette, objects: list });
            }
            // Palette definition.
            0x14 => {
                let Some(&id) = p.first() else { continue };
                let hd = canvas.1 > 576;
                let pal = palettes.entry(id).or_insert([[0; 4]; 256]);
                for e in p.get(2..).unwrap_or(&[]).chunks_exact(5) {
                    let [r, g, b] = ycbcr_to_rgb(e[1], e[3], e[2], hd);
                    pal[e[0] as usize] = [r, g, b, e[4]];
                }
            }
            // Object definition (possibly split over several segments).
            0x15 => {
                let (Some(id), Some(&seq)) = (be16(p, 0), p.get(3)) else { continue };
                if seq & 0x80 != 0 {
                    let (Some(w), Some(h)) = (be16(p, 7), be16(p, 9)) else { continue };
                    objects.insert(id, PgsObject { width: w as u32, height: h as u32, rle: p.get(11..).unwrap_or(&[]).to_vec() });
                } else if let Some(o) = objects.get_mut(&id) {
                    o.rle.extend_from_slice(p.get(4..).unwrap_or(&[]));
                }
            }
            // End of display set: the composition takes effect.
            0x80 => {
                let Some(pcs) = pending.take() else { continue };
                if let Some(mut prev) = shown.take() {
                    prev.end = pcs.pts;
                    out.push(prev);
                }
                shown = compose_pgs(&pcs, &objects, palettes.get(&pcs.palette));
            }
            _ => {}
        }
    }
    if let Some(mut last) = shown {
        last.end = last.start + LAST_PICTURE_SECS;
        out.push(last);
    }
    out.retain(|p| p.end - p.start >= MIN_PICTURE_SECS);
    Ok(Decoded { canvas, pictures: out })
}

/// The picture of one composition (all its objects on one bounding box), `None` when empty.
fn compose_pgs(pcs: &Pcs, objects: &HashMap<u16, PgsObject>, palette: Option<&[[u8; 4]; 256]>) -> Option<Picture> {
    let palette = palette?;
    // (placement, decoded indices, visible rectangle in the object)
    let mut parts = Vec::new();
    for pl in &pcs.objects {
        let Some(o) = objects.get(&pl.object) else { continue };
        if o.width == 0 || o.height == 0 {
            continue;
        }
        let idx = decode_pgs_rle(&o.rle, o.width, o.height);
        let (cx, cy, cw, ch) = pl.crop.unwrap_or((0, 0, o.width, o.height));
        let cw = cw.min(o.width.saturating_sub(cx));
        let ch = ch.min(o.height.saturating_sub(cy));
        if cw > 0 && ch > 0 {
            parts.push((pl, o.width, idx, (cx, cy, cw, ch)));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let x0 = parts.iter().map(|p| p.0.x).min()?;
    let y0 = parts.iter().map(|p| p.0.y).min()?;
    let x1 = parts.iter().map(|p| p.0.x + p.3 .2).max()?;
    let y1 = parts.iter().map(|p| p.0.y + p.3 .3).max()?;
    let mut image = RgbaImage::new(x1 - x0, y1 - y0);
    for (pl, ow, idx, (cx, cy, cw, ch)) in &parts {
        for y in 0..*ch {
            for x in 0..*cw {
                let i = idx[((cy + y) * ow + cx + x) as usize];
                let c = palette[i as usize];
                if c[3] > 0 {
                    image.put_pixel(pl.x - x0 + x, pl.y - y0 + y, Rgba(c));
                }
            }
        }
    }
    let forced = pcs.objects.iter().all(|o| o.forced);
    Some(Picture { start: pcs.pts, end: f64::INFINITY, forced, x: x0, y: y0, image })
}

// ---------------------------------------------------------------------------------------------
// VobSub (DVD subpicture units)
// ---------------------------------------------------------------------------------------------

/// `palette:` (16 RGB colours) and `size:` from a VobSub `.idx` header (Matroska CodecPrivate).
pub fn parse_idx_header(text: &str) -> (Option<[[u8; 3]; 16]>, Option<(u32, u32)>) {
    let mut palette = None;
    let mut size = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "palette" => {
                let colors: Vec<[u8; 3]> = value
                    .split(',')
                    .filter_map(|c| u32::from_str_radix(c.trim(), 16).ok())
                    .map(|v| [(v >> 16) as u8, (v >> 8) as u8, v as u8])
                    .collect();
                if colors.len() >= 16 {
                    palette = colors[..16].try_into().ok();
                }
            }
            "size" => {
                size = value.trim().split_once('x').and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
            }
            _ => {}
        }
    }
    (palette, size)
}

/// Reads 4-bit nibbles of an SPU field.
struct Nibbles<'a> {
    data: &'a [u8],
    /// Position in nibbles.
    pos: usize,
}

impl Nibbles<'_> {
    fn next(&mut self) -> u16 {
        let b = self.data.get(self.pos / 2).copied().unwrap_or(0);
        let v = if self.pos % 2 == 0 { b >> 4 } else { b & 0x0F };
        self.pos += 1;
        v as u16
    }

    fn align(&mut self) {
        self.pos += self.pos % 2;
    }
}

/// Decodes the interlaced 2-bit run-length picture of an SPU (even lines from `top`, odd lines
/// from `bottom`) into `width × height` pixel values 0..=3.
pub fn decode_spu_rle(spu: &[u8], top: usize, bottom: usize, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut px = vec![0u8; w * h];
    for (field, offset) in [(0, top), (1, bottom)] {
        let mut r = Nibbles { data: spu, pos: offset * 2 };
        for line in (field..h).step_by(2) {
            let mut x = 0;
            while x < w {
                if r.pos / 2 >= spu.len() {
                    break;
                }
                let mut v = r.next();
                if v < 0x4 {
                    v = (v << 4) | r.next();
                    if v < 0x10 {
                        v = (v << 4) | r.next();
                        if v < 0x40 {
                            v = (v << 4) | r.next();
                        }
                    }
                }
                let color = (v & 3) as u8;
                let run = match (v >> 2) as usize {
                    0 => w - x, // to the end of the line
                    n => n.min(w - x),
                };
                px[line * w + x..line * w + x + run].fill(color);
                x += run;
            }
            r.align();
        }
    }
    px
}

/// One parsed SPU: display times relative to its packet, colours and picture.
#[derive(Debug, Clone, PartialEq)]
struct Spu {
    start: Option<f64>,
    stop: Option<f64>,
    forced: bool,
    /// Palette entries and contrast (0..=15) of the pixel values 0..=3.
    colors: [u8; 4],
    alpha: [u8; 4],
    /// x1, y1, x2, y2 (inclusive).
    area: Option<(u32, u32, u32, u32)>,
    fields: Option<(usize, usize)>,
}

fn parse_spu(spu: &[u8]) -> Option<Spu> {
    let size = (be16(spu, 0)? as usize).min(spu.len());
    let mut off = be16(spu, 2)? as usize;
    let mut s = Spu { start: None, stop: None, forced: false, colors: [0, 1, 2, 3], alpha: [0, 15, 15, 15], area: None, fields: None };
    for _ in 0..64 {
        if off + 4 > size {
            break;
        }
        let delay = be16(spu, off)? as f64 * 1024.0 / CLOCK;
        let next = be16(spu, off + 2)? as usize;
        let mut p = off + 4;
        while p < size {
            let cmd = spu[p];
            p += 1;
            match cmd {
                0x00 => {
                    s.forced = true;
                    s.start = Some(delay);
                }
                0x01 => s.start = Some(delay),
                0x02 => s.stop = Some(delay),
                0x03 | 0x04 => {
                    let (b0, b1) = (*spu.get(p)?, *spu.get(p + 1)?);
                    let v = [b1 & 0x0F, b1 >> 4, b0 & 0x0F, b0 >> 4];
                    if cmd == 0x03 {
                        s.colors = v;
                    } else {
                        s.alpha = v;
                    }
                    p += 2;
                }
                0x05 => {
                    let c = spu.get(p..p + 6)?;
                    let x1 = ((c[0] as u32) << 4) | (c[1] as u32 >> 4);
                    let x2 = (((c[1] & 0x0F) as u32) << 8) | c[2] as u32;
                    let y1 = ((c[3] as u32) << 4) | (c[4] as u32 >> 4);
                    let y2 = (((c[4] & 0x0F) as u32) << 8) | c[5] as u32;
                    s.area = Some((x1, y1, x2, y2));
                    p += 6;
                }
                0x06 => {
                    s.fields = Some((be16(spu, p)? as usize, be16(spu, p + 2)? as usize));
                    p += 4;
                }
                _ => break, // 0xFF end of sequence (or an unknown command)
            }
        }
        if next <= off {
            break;
        }
        off = next;
    }
    Some(s)
}

/// Grey levels for VobSub streams without a palette (VOB files): pattern white, outline black,
/// anti-aliasing grey – the common DVD convention.
const NO_PALETTE: [[u8; 3]; 4] = [[0, 0, 0], [255, 255, 255], [0, 0, 0], [128, 128, 128]];

/// Decodes VobSub packets (`(pts in seconds, SPU)`) on a `canvas`; without `palette` the pixel
/// values get [`NO_PALETTE`].
pub fn decode_vobsub(packets: &[(f64, Vec<u8>)], palette: Option<&[[u8; 3]; 16]>, canvas: (u32, u32)) -> Decoded {
    let mut pictures: Vec<Picture> = Vec::new();
    for (pts, data) in packets {
        let Some(spu) = parse_spu(data) else { continue };
        let (Some((x1, y1, x2, y2)), Some((top, bottom)), Some(start)) = (spu.area, spu.fields, spu.start) else { continue };
        if x2 < x1 || y2 < y1 {
            continue;
        }
        let (w, h) = (x2 - x1 + 1, y2 - y1 + 1);
        let px = decode_spu_rle(data, top, bottom, w, h);
        let rgb = |v: usize| match palette {
            Some(p) => p[spu.colors[v] as usize & 15],
            None => NO_PALETTE[v],
        };
        let mut image = RgbaImage::new(w, h);
        for (i, &v) in px.iter().enumerate() {
            let a = spu.alpha[v as usize] * 17;
            if a > 0 {
                let [r, g, b] = rgb(v as usize);
                image.put_pixel(i as u32 % w, i as u32 / w, Rgba([r, g, b, a]));
            }
        }
        let start = pts + start;
        let end = spu.stop.map_or(f64::INFINITY, |s| pts + s);
        pictures.push(Picture { start, end, forced: spu.forced, x: x1, y: y1, image });
    }
    pictures.sort_by(|a, b| a.start.total_cmp(&b.start));
    // Pictures without a stop time last until the next one.
    for i in 0..pictures.len() {
        let next = pictures.get(i + 1).map(|p| p.start);
        let p = &mut pictures[i];
        if !p.end.is_finite() {
            p.end = next.unwrap_or(p.start + LAST_PICTURE_SECS);
        }
    }
    pictures.retain(|p| p.end - p.start >= MIN_PICTURE_SECS);
    Decoded { canvas, pictures }
}

/// Bytes of an ffprobe `-show_data` hex dump (`"00000010: 0000 03fc …  ascii"`).
pub fn parse_hexdump(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(hex) = line.get(10..line.len().min(50)) else { continue };
        for group in hex.split_whitespace() {
            for pair in group.as_bytes().chunks(2) {
                if let Some(b) = std::str::from_utf8(pair).ok().and_then(|s| u8::from_str_radix(s, 16).ok()) {
                    out.push(b);
                }
            }
        }
    }
    out
}

/// A VobSub stream as read by ffprobe (see [`vobsub_probe_args`]).
#[derive(Debug, Clone, Default)]
pub struct VobsubProbe {
    /// (pts relative to the file start, SPU)
    pub packets: Vec<(f64, Vec<u8>)>,
    /// The stream's extradata (the `.idx` header in Matroska; empty in VOB files).
    pub extradata: Vec<u8>,
}

impl VobsubProbe {
    /// Keeps the packets shown within `start..end` seconds of the file and makes their times
    /// relative to `start` (the encode starts there).
    pub fn trimmed(mut self, start: f64, end: f64) -> VobsubProbe {
        self.packets.retain(|(t, _)| *t >= start && *t < end);
        for (t, _) in &mut self.packets {
            *t -= start;
        }
        self
    }
}

/// ffprobe arguments that dump the packets of stream `index` of `input`.
pub fn vobsub_probe_args(input: &Path, index: u32) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = ["-v", "error", "-select_streams"].map(Into::into).to_vec();
    a.push(index.to_string().into());
    a.extend(
        ["-show_entries", "packet=pts_time,data:stream=extradata:format=start_time", "-show_data", "-of", "json"]
            .map(Into::into),
    );
    a.push(input.as_os_str().to_owned());
    a
}

/// Parses the JSON of [`vobsub_probe_args`]; times are made relative to the file start (like
/// ffmpeg's output timestamps).
pub fn parse_vobsub_probe(json: &str) -> Result<VobsubProbe, SubpicError> {
    #[derive(serde::Deserialize)]
    struct Out {
        #[serde(default)]
        packets: Vec<Packet>,
        #[serde(default)]
        streams: Vec<Stream>,
        format: Option<Format>,
    }
    #[derive(serde::Deserialize)]
    struct Packet {
        pts_time: Option<String>,
        data: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Stream {
        extradata: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Format {
        start_time: Option<String>,
    }
    let out: Out = serde_json::from_str(json).map_err(|e| bad(format!("unerwartete ffprobe-Ausgabe ({e})")))?;
    let num = |s: &Option<String>| s.as_deref().and_then(|v| v.parse::<f64>().ok());
    let start = out.format.as_ref().and_then(|f| num(&f.start_time)).unwrap_or(0.0);
    let packets = out
        .packets
        .iter()
        .filter_map(|p| Some(((num(&p.pts_time)? - start).max(0.0), parse_hexdump(p.data.as_deref()?))))
        .filter(|(_, d)| d.len() > 4)
        .collect();
    let extradata = out.streams.first().and_then(|s| s.extradata.as_deref()).map(parse_hexdump).unwrap_or_default();
    Ok(VobsubProbe { packets, extradata })
}

// ---------------------------------------------------------------------------------------------
// DVD output
// ---------------------------------------------------------------------------------------------

/// Where the subtitles go: the DVD frame (subpictures always use the full 720-wide raster) and
/// the rectangle the source picture occupies in it (letter-/pillarbox bars excluded).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub frame: (u32, u32),
    /// x, y, width, height in the frame.
    pub area: (u32, u32, u32, u32),
    /// The part of the canvas that holds the source picture: x, y, width, height (the whole
    /// canvas unless the video is a crop of it, e.g. a 1920×800 encode with a 1920×1080 canvas).
    pub source: (u32, u32, u32, u32),
}

impl Target {
    /// Target for a video of `video` pixels whose picture fills `area` of `tv`'s frame, with
    /// subtitles drawn on `canvas`.
    pub fn new(tv: TvStandard, area: (u32, u32, u32, u32), canvas: (u32, u32), video: (u32, u32)) -> Target {
        let (cw, ch) = canvas;
        let source = if video.0 == cw && video.1 > 0 && video.1 < ch {
            (0, (ch - video.1) / 2, cw, video.1)
        } else {
            (0, 0, cw, ch)
        };
        Target { frame: tv.size(), area, source }
    }

    /// The same target for a video that is cropped to `crop` (x, y, w, h in `video` pixels): the
    /// picture then shows only that part of the canvas.
    pub fn cropped(mut self, crop: (u32, u32, u32, u32), video: (u32, u32)) -> Target {
        let (sx, sy, sw, sh) = self.source;
        let (fx, fy) = (sw as f64 / video.0.max(1) as f64, sh as f64 / video.1.max(1) as f64);
        let at = |v: u32, f: f64| (v as f64 * f).round() as u32;
        self.source = (sx + at(crop.0, fx), sy + at(crop.1, fy), at(crop.2, fx).max(1), at(crop.3, fy).max(1));
        self
    }

    /// Position and size of a canvas rectangle in the frame (clamped into it).
    fn map(&self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32, u32, u32) {
        let (sx0, sy0, sw, sh) = self.source;
        let (ax, ay, aw, ah) = self.area;
        let fx = aw as f64 / sw.max(1) as f64;
        let fy = ah as f64 / sh.max(1) as f64;
        let nw = ((w as f64 * fx).round() as u32).clamp(1, self.frame.0);
        let nh = ((h as f64 * fy).round() as u32).clamp(1, self.frame.1);
        let nx = ax as f64 + (x as f64 - sx0 as f64) * fx;
        let ny = ay as f64 + (y as f64 - sy0 as f64) * fy;
        let nx = (nx.round().max(0.0) as u32).min(self.frame.0 - nw);
        let ny = (ny.round().max(0.0) as u32).min(self.frame.1 - nh);
        (nx, ny, nw, nh)
    }
}

fn luma(c: [u8; 3]) -> u32 {
    299 * c[0] as u32 + 587 * c[1] as u32 + 114 * c[2] as u32
}

fn dist(a: [u8; 3], b: [u8; 3]) -> u32 {
    a.iter().zip(&b).map(|(x, y)| (*x as i32 - *y as i32).pow(2) as u32).sum()
}

fn dist4(a: [u8; 4], b: [u8; 4]) -> u32 {
    let dr = a[0] as i32 - b[0] as i32;
    let dg = a[1] as i32 - b[1] as i32;
    let db = a[2] as i32 - b[2] as i32;
    let da = a[3] as i32 - b[3] as i32;
    (dr * dr + dg * dg + db * db + 2 * da * da) as u32
}

/// Up to 3 representative colours of `colors` (k-means, seeded with the darkest, middle and
/// brightest colour).
fn representatives(colors: &[[u8; 3]]) -> Vec<[u8; 3]> {
    let mut distinct: Vec<[u8; 3]> = colors.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() <= 3 {
        return distinct;
    }
    distinct.sort_by_key(|c| luma(*c));
    let mut centers = vec![distinct[0], distinct[distinct.len() / 2], distinct[distinct.len() - 1]];
    let step = (colors.len() / 20_000).max(1);
    for _ in 0..8 {
        let mut sums = [[0u64; 4]; 3];
        for c in colors.iter().step_by(step) {
            let k = (0..centers.len()).min_by_key(|&k| dist(*c, centers[k])).unwrap_or(0);
            for (s, v) in sums[k].iter_mut().zip(c) {
                *s += *v as u64;
            }
            sums[k][3] += 1;
        }
        for (center, s) in centers.iter_mut().zip(&sums) {
            if s[3] > 0 {
                *center = [(s[0] / s[3]) as u8, (s[1] / s[3]) as u8, (s[2] / s[3]) as u8];
            }
        }
    }
    centers
}

/// Chooses up to 3 palette colors with alpha preservation for anti-aliasing.
fn choose_reps(pixels: &[[u8; 4]]) -> Vec<[u8; 4]> {
    let mut distinct: Vec<[u8; 4]> = pixels.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() <= 3 {
        return distinct;
    }
    let opaque: Vec<[u8; 3]> = pixels.iter().filter(|p| p[3] >= 192).map(|p| [p[0], p[1], p[2]]).collect();
    let translucent: Vec<[u8; 4]> = pixels.iter().filter(|p| p[3] < 192).copied().collect();

    if translucent.is_empty() {
        return representatives(&opaque).into_iter().map(|c| [c[0], c[1], c[2], 255]).collect();
    }

    if opaque.is_empty() {
        distinct.sort_by_key(|p| p[3]);
        let mut centers = vec![distinct[0], distinct[distinct.len() / 2], distinct[distinct.len() - 1]];
        let step = (pixels.len() / 20_000).max(1);
        for _ in 0..8 {
            let mut sums = [[0u64; 5]; 3];
            for c in pixels.iter().step_by(step) {
                let k = (0..centers.len()).min_by_key(|&k| dist4(*c, centers[k])).unwrap_or(0);
                for (s, v) in sums[k][..4].iter_mut().zip(c) {
                    *s += *v as u64;
                }
                sums[k][4] += 1;
            }
            for (center, s) in centers.iter_mut().zip(&sums) {
                if s[4] > 0 {
                    *center = [(s[0] / s[4]) as u8, (s[1] / s[4]) as u8, (s[2] / s[4]) as u8, (s[3] / s[4]) as u8];
                }
            }
        }
        return centers;
    }

    let trans_count = translucent.len() as u64;
    let (sum_r, sum_g, sum_b, sum_a) = translucent.iter().fold((0u64, 0u64, 0u64, 0u64), |(r, g, b, a), p| {
        (r + p[0] as u64, g + p[1] as u64, b + p[2] as u64, a + p[3] as u64)
    });
    // Quantize RGB to multiples of 16 so cues share palette entries instead of diverging by single bits
    let quant = |v: u64| ((((v / trans_count) as u32 + 8) / 16 * 16).min(255)) as u8;
    let edge = [quant(sum_r), quant(sum_g), quant(sum_b), (sum_a / trans_count) as u8];

    let op_reps = representatives(&opaque);
    if op_reps.len() == 1 {
        let op = [op_reps[0][0], op_reps[0][1], op_reps[0][2], 255];
        let mut t_distinct: Vec<[u8; 4]> = translucent;
        t_distinct.sort_by_key(|p| p[3]);
        let t1 = t_distinct[0];
        let t2 = t_distinct[t_distinct.len() - 1];
        vec![op, t1, t2]
    } else {
        vec![
            [op_reps[0][0], op_reps[0][1], op_reps[0][2], 255],
            [op_reps[1][0], op_reps[1][1], op_reps[1][2], 255],
            edge,
        ]
    }
}

/// Transparent + up to 3 colors (alpha < 64 counts as transparent background), cropped to the
/// visible pixels. Semi-transparent anti-aliased edge alpha is preserved in the palette.
/// Returns the image and its offset inside `img`; `None` when nothing is visible.
pub fn quantize(img: &RgbaImage) -> Option<(IndexedImage, u32, u32)> {
    let visible = |p: &Rgba<u8>| p.0[3] >= 64;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    let mut pixels = Vec::new();
    for (x, y, p) in img.enumerate_pixels() {
        if visible(p) {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            pixels.push(p.0);
        }
    }
    if pixels.is_empty() {
        return None;
    }
    let reps = choose_reps(&pixels);
    let mut palette = [[0u8; 4]; 4];
    for (slot, c) in palette.iter_mut().skip(1).zip(reps.iter().cycle()) {
        *slot = *c;
    }
    let mut out = IndexedImage::new(x1 - x0 + 1, y1 - y0 + 1, palette);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let p = img.get_pixel(x, y);
            if visible(p) {
                let k = (0..reps.len()).min_by_key(|&k| dist4(p.0, reps[k])).unwrap_or(0);
                out.pixels[((y - y0) * out.width + x - x0) as usize] = k as u8 + 1;
            }
        }
    }
    Some((out, x0, y0))
}

/// spumux time stamp `hh:mm:ss.ss`.
pub fn spumux_time(secs: f64) -> String {
    let cs = (secs.max(0.0) * 100.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:02}", cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100)
}

/// Writes one palette PNG per picture into `dir/<folder>/` and returns the spumux XML for the
/// stream (images referenced relative to `dir`) and the number of subpictures.
pub fn render(decoded: &Decoded, target: &Target, tv: TvStandard, dir: &Path, folder: &str) -> Result<(String, usize), SubpicError> {
    let out_dir = dir.join(folder);
    std::fs::create_dir_all(&out_dir).map_err(|error| SubpicError::Io { path: out_dir.clone(), error })?;
    let mut pictures: Vec<&Picture> = decoded.pictures.iter().collect();
    pictures.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut xml = String::new();
    let _ = writeln!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(xml, r#"<subpictures format="{}">"#, tv.spumux_format());
    let _ = writeln!(xml, "  <stream>");
    let mut count = 0;
    for (i, p) in pictures.iter().enumerate() {
        // No overlaps: a picture ends when the next one starts.
        let end = pictures.get(i + 1).map_or(p.end, |n| p.end.min(n.start));
        if end - p.start < MIN_PICTURE_SECS {
            continue;
        }
        let (x, y, w, h) = target.map(p.x, p.y, p.image.width(), p.image.height());
        let scaled = if (w, h) == p.image.dimensions() { p.image.clone() } else { imageops::resize(&p.image, w, h, FilterType::Triangle) };
        let Some((img, dx, dy)) = quantize(&scaled) else { continue };
        count += 1;
        let name = format!("{folder}/{count:05}.png");
        let path = dir.join(&name);
        img.write_png(&path).map_err(|e| SubpicError::Io { path: path.clone(), error: std::io::Error::other(e.to_string()) })?;
        let _ = writeln!(
            xml,
            r#"    <spu start="{}" end="{}" image="{}" xoffset="{}" yoffset="{}"{}/>"#,
            spumux_time(p.start),
            spumux_time(end),
            xml_escape(&name),
            x + dx,
            y + dy,
            if p.forced { r#" force="yes""# } else { "" }
        );
    }
    let _ = writeln!(xml, "  </stream>");
    let _ = writeln!(xml, "</subpictures>");
    Ok((xml, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One PGS segment with the `PG` header.
    fn seg(pts: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = b"PG".to_vec();
        s.extend(pts.to_be_bytes());
        s.extend(0u32.to_be_bytes());
        s.push(kind);
        s.extend((payload.len() as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    fn pcs(objects: &[(u16, u16, u16, bool)]) -> Vec<u8> {
        let mut p = vec![0x07, 0x80, 0x04, 0x38, 0x10, 0, 1, if objects.is_empty() { 0 } else { 0x80 }, 0, 0, objects.len() as u8];
        for &(id, x, y, forced) in objects {
            p.extend(id.to_be_bytes());
            p.push(0);
            p.push(if forced { 0x40 } else { 0 });
            p.extend(x.to_be_bytes());
            p.extend(y.to_be_bytes());
        }
        p
    }

    #[test]
    fn empty_pgs_has_no_pictures() {
        let sup = empty_pgs(1920, 1080);
        assert_eq!(sup.len(), 13 + 11 + 13);
        let d = decode_pgs(&sup).unwrap();
        assert_eq!((d.canvas, d.pictures.len()), ((1920, 1080), 0));
    }

    #[test]
    fn pgs_rle() {
        // Line 1: 2 × colour 5, 3 × transparent (short form), colour 7 × 70 (long form);
        // line 2: colour 1, then end of line (padded).
        let data = [5, 5, 0, 0x03, 0, 0xC0, 70, 7, 0, 0, 1, 0, 0];
        let px = decode_pgs_rle(&data, 75, 2);
        assert_eq!(&px[..5], &[5, 5, 0, 0, 0]);
        assert!(px[5..75].iter().all(|&v| v == 7));
        assert_eq!(px[75], 1);
        assert!(px[76..].iter().all(|&v| v == 0));
        assert_eq!(decode_pgs_rle(&[0, 0x82, 9, 0, 0], 2, 1), vec![9, 9]);
    }

    #[test]
    fn pgs_display_sets() {
        // Object 1: 4×2, colour 1 (white) except one transparent pixel per line.
        let mut ods = vec![0, 1, 0, 0xC0];
        let rle = [1, 1, 1, 0, 0x01, 0, 0, 1, 1, 1, 0, 0x01, 0, 0];
        ods.extend(((rle.len() + 4) as u32).to_be_bytes()[1..].to_vec());
        ods.extend(4u16.to_be_bytes());
        ods.extend(2u16.to_be_bytes());
        ods.extend(rle);
        // Palette 0: entry 1 = white (Y 235, neutral chroma), opaque.
        let pds = [0, 0, 1, 235, 128, 128, 255];
        let mut sup = Vec::new();
        sup.extend(seg(90_000, 0x16, &pcs(&[(1, 100, 900, true)])));
        sup.extend(seg(90_000, 0x14, &pds));
        sup.extend(seg(90_000, 0x15, &ods));
        sup.extend(seg(90_000, 0x80, &[]));
        // Cleared at 3.5 s.
        sup.extend(seg(315_000, 0x16, &pcs(&[])));
        sup.extend(seg(315_000, 0x80, &[]));
        let d = decode_pgs(&sup).unwrap();
        assert_eq!(d.canvas, (1920, 1080));
        assert_eq!(d.pictures.len(), 1);
        let p = &d.pictures[0];
        assert_eq!((p.start, p.end, p.forced, p.x, p.y), (1.0, 3.5, true, 100, 900));
        assert_eq!(p.image.dimensions(), (4, 2));
        assert_eq!(p.image.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(p.image.get_pixel(3, 1).0[3], 0, "transparent pixel");
        // A stream that is never cleared gets a default duration; garbage is an error.
        sup.truncate(sup.len() - 2 * 13 - pcs(&[]).len());
        assert_eq!(decode_pgs(&sup).unwrap().pictures[0].end, 1.0 + LAST_PICTURE_SECS);
        assert!(decode_pgs(b"XX0000000000000").is_err());
    }

    /// Run-length codes of the 2-bit SPU format for `(run, value)`; run 0 = to the line end.
    fn spu_code(run: u16, v: u16) -> Vec<u8> {
        let code = (run << 2) | v;
        let nibbles = if run == 0 {
            4
        } else if run < 4 {
            1
        } else if run < 16 {
            2
        } else if run < 64 {
            3
        } else {
            4
        };
        (0..nibbles).rev().map(|i| ((code >> (4 * i)) & 0xF) as u8).collect()
    }

    fn pack(nibbles: &[u8]) -> Vec<u8> {
        nibbles.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect()
    }

    /// An SPU of `w`×2 pixels: line 0 value 1 for 3 pixels then 2 to the end, line 1 all 3.
    fn spu(w: u16) -> Vec<u8> {
        let mut top = spu_code(3, 1);
        top.extend(spu_code(0, 2));
        let top = pack(&top);
        let bottom = pack(&spu_code(0, 3));
        let mut d = vec![0, 0, 0, 0];
        let top_at = d.len();
        d.extend(&top);
        let bottom_at = d.len();
        d.extend(&bottom);
        let ctrl = d.len();
        let stop_ctrl = ctrl + 4 + 1 + 3 + 3 + 7 + 5 + 1;
        // Sequence 1 at delay 0: forced start, colours, contrast, area 10..10+w-1 × 20..21, fields.
        d.extend(0u16.to_be_bytes());
        d.extend((stop_ctrl as u16).to_be_bytes());
        d.push(0x00);
        d.extend([0x03, 0x32, 0x10]); // pixel 0→0, 1→1, 2→2, 3→3
        d.extend([0x04, 0xFF, 0xF0]); // pixel 0 transparent, others opaque
        let (x1, x2, y1, y2) = (10u16, 10 + w - 1, 20u16, 21u16);
        d.extend([0x05, (x1 >> 4) as u8, (((x1 & 0xF) << 4) | (x2 >> 8)) as u8, x2 as u8]);
        d.extend([(y1 >> 4) as u8, (((y1 & 0xF) << 4) | (y2 >> 8)) as u8, y2 as u8]);
        d.extend([0x06]);
        d.extend((top_at as u16).to_be_bytes());
        d.extend((bottom_at as u16).to_be_bytes());
        d.push(0xFF);
        assert_eq!(d.len(), stop_ctrl);
        // Sequence 2: stop after 100 ticks (1.1378 s).
        d.extend(100u16.to_be_bytes());
        d.extend((stop_ctrl as u16).to_be_bytes());
        d.extend([0x02, 0xFF]);
        let size = d.len() as u16;
        d[0..2].copy_from_slice(&size.to_be_bytes());
        d[2..4].copy_from_slice(&(ctrl as u16).to_be_bytes());
        d
    }

    #[test]
    fn spu_rle_and_control() {
        let data = spu(8);
        let s = parse_spu(&data).unwrap();
        assert!(s.forced);
        assert_eq!((s.start, s.colors, s.alpha), (Some(0.0), [0, 1, 2, 3], [0, 15, 15, 15]));
        assert!((s.stop.unwrap() - 100.0 * 1024.0 / 90_000.0).abs() < 1e-9);
        assert_eq!(s.area, Some((10, 20, 17, 21)));
        let (top, bottom) = s.fields.unwrap();
        let px = decode_spu_rle(&data, top, bottom, 8, 2);
        assert_eq!(px, vec![1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3]);
        // Long runs use more nibbles.
        let long = pack(&[spu_code(70, 1), spu_code(0, 0)].concat());
        assert_eq!(decode_spu_rle(&long, 0, 0, 80, 1)[..71], [vec![1u8; 70], vec![0]].concat()[..]);
    }

    #[test]
    fn vobsub_pictures() {
        let mut palette = [[0u8; 3]; 16];
        palette[1] = [255, 255, 0];
        palette[2] = [0, 0, 0];
        palette[3] = [10, 20, 30];
        let packets = vec![(2.0, spu(8)), (2.5, spu(4))];
        let d = decode_vobsub(&packets, Some(&palette), (720, 576));
        assert_eq!(d.pictures.len(), 2);
        let p = &d.pictures[0];
        assert_eq!((p.start, p.forced, p.x, p.y), (2.0, true, 10, 20));
        assert!((p.end - (2.0 + 100.0 * 1024.0 / 90_000.0)).abs() < 1e-9);
        assert_eq!(p.image.get_pixel(0, 0).0, [255, 255, 0, 255]);
        assert_eq!(p.image.get_pixel(0, 1).0, [10, 20, 30, 255]);
        // Without a palette: white pattern, black outline.
        let d = decode_vobsub(&packets[..1], None, (720, 576));
        assert_eq!(d.pictures[0].image.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(d.pictures[0].image.get_pixel(5, 0).0, [0, 0, 0, 255]);
    }

    #[test]
    fn idx_header_and_hexdump() {
        let idx = "# VobSub index file, v7\nsize: 720x480\npalette: 000000, ffffff, 808080, ff0000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0\n";
        let (palette, size) = parse_idx_header(idx);
        assert_eq!(size, Some((720, 480)));
        let palette = palette.unwrap();
        assert_eq!((palette[1], palette[3]), ([255, 255, 255], [255, 0, 0]));
        assert_eq!(parse_idx_header("nothing"), (None, None));

        let dump = "\n00000000: 090e 08f0 0000 0000 0000 0000 0000 0000  ................\n00000010: 0102 03                                  ...\n";
        let bytes = parse_hexdump(dump);
        assert_eq!(bytes.len(), 19);
        assert_eq!(&bytes[..4], &[0x09, 0x0e, 0x08, 0xf0]);
        assert_eq!(&bytes[16..], &[1, 2, 3]);

        let json = r#"{"packets":[{"pts_time":"2.407267","data":"\n00000000: 0001 0203 04"},{"pts_time":"3.0"}],
            "streams":[{"extradata":"\n"}],"format":{"start_time":"0.407267"}}"#;
        let p = parse_vobsub_probe(json).unwrap();
        assert_eq!(p.packets.len(), 1);
        assert!((p.packets[0].0 - 2.0).abs() < 1e-9, "relative to the file start");
        assert_eq!(p.packets[0].1, vec![0, 1, 2, 3, 4]);
        assert!(p.extradata.is_empty());
    }

    #[test]
    fn mapping_into_the_frame() {
        // 1920×1080 canvas, 16:9 video filling a PAL 16:9 frame.
        let t = Target::new(TvStandard::Pal, (0, 0, 720, 576), (1920, 1080), (1920, 1080));
        assert_eq!(t.map(960, 900, 400, 100), (360, 480, 150, 53));
        // Same canvas on a 4:3 letterboxed frame (picture 720×432 at y 72).
        let t = Target::new(TvStandard::Pal, (0, 72, 720, 432), (1920, 1080), (1920, 1080));
        assert_eq!(t.map(0, 1000, 1920, 80), (0, 472, 720, 32));
        // Video cropped to 1920×800 inside the 1080 canvas: the canvas rows 140..940 are the picture.
        let t = Target::new(TvStandard::Pal, (0, 0, 720, 576), (1920, 1080), (1920, 800));
        assert_eq!(t.source, (0, 140, 1920, 800));
        // A subtitle in the canvas' bottom bar is kept inside the frame.
        let (_, y, _, h) = t.map(0, 1000, 1920, 60);
        assert!(y + h <= 576, "{y}+{h}");
    }

    #[test]
    fn quantizing_and_xml() {
        let mut img = RgbaImage::new(10, 4);
        // White text, black outline, two grey shades, a half-transparent pixel and a faint one.
        for x in 2..8 {
            img.put_pixel(x, 1, Rgba([255, 255, 255, 255]));
            img.put_pixel(x, 2, Rgba([0, 0, 0, 255]));
        }
        img.put_pixel(2, 3, Rgba([120, 120, 120, 255]));
        img.put_pixel(3, 3, Rgba([130, 130, 130, 200]));
        img.put_pixel(9, 0, Rgba([255, 0, 0, 40]));
        let (q, dx, dy) = quantize(&img).unwrap();
        assert_eq!((dx, dy, q.width, q.height), (2, 1, 6, 3), "cropped to the visible pixels");
        assert_eq!(q.palette[0][3], 0, "index 0 is transparent");
        assert!(q.pixels.iter().all(|&v| v <= 3));
        let distinct: std::collections::BTreeSet<u8> = q.pixels.iter().copied().filter(|&v| v > 0).collect();
        assert_eq!(distinct.len(), 3, "white, black, grey");
        assert!(quantize(&RgbaImage::new(3, 3)).is_none());

        assert_eq!(spumux_time(3661.257), "01:01:01.26");
        assert_eq!(spumux_time(0.0), "00:00:00.00");

        let dir = std::env::temp_dir().join(format!("cx_subpic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pic = |start: f64, end: f64| Picture { start, end, forced: false, x: 2, y: 2, image: img.clone() };
        let decoded = Decoded { canvas: (720, 576), pictures: vec![pic(1.0, 5.0), pic(3.0, 4.0), pic(9.0, 9.01)] };
        let t = Target::new(TvStandard::Pal, (0, 0, 720, 576), (720, 576), (720, 576));
        let (xml, n) = render(&decoded, &t, TvStandard::Pal, &dir, "s1").unwrap();
        assert_eq!(n, 2, "too short picture dropped");
        assert!(xml.contains(r#"<spu start="00:00:01.00" end="00:00:03.00" image="s1/00001.png" xoffset="4" yoffset="3"/>"#), "{xml}");
        assert!(xml.contains(r#"start="00:00:03.00" end="00:00:04.00""#), "overlap trimmed: {xml}");
        assert!(dir.join("s1/00002.png").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crop_moves_the_subtitle_source_area() {
        // 1920x1080 video, 1920x1080 canvas, cropped to the 1920x800 middle part: subtitles at the
        // bottom of the canvas (y = 900) land at the bottom of the shown picture.
        let t = Target::new(TvStandard::Pal, (0, 72, 720, 432), (1920, 1080), (1920, 1080));
        let c = t.cropped((0, 140, 1920, 800), (1920, 1080));
        assert_eq!(c.source, (0, 140, 1920, 800));
        let (_, y, _, _) = c.map(0, 900, 100, 50);
        assert_eq!(y, 72 + ((900 - 140) as f64 * 432.0 / 800.0).round() as u32);
        // Without crop the same canvas row maps proportionally into the whole area.
        assert_ne!(t.map(0, 900, 100, 50).1, y);
    }

    #[test]
    fn vobsub_packets_are_trimmed() {
        let p = VobsubProbe { packets: vec![(5.0, vec![1; 8]), (12.0, vec![2; 8]), (30.0, vec![3; 8])], extradata: vec![] }.trimmed(10.0, 20.0);
        assert_eq!(p.packets.len(), 1);
        assert_eq!(p.packets[0].0, 2.0);
    }

    #[test]
    fn quantize_preserves_anti_aliased_alpha() {
        let mut img = RgbaImage::new(10, 10);
        // White text, black outline, anti-aliased edge at alpha 120
        for x in 3..7 {
            img.put_pixel(x, 3, Rgba([255, 255, 255, 255]));
            img.put_pixel(x, 4, Rgba([0, 0, 0, 255]));
            img.put_pixel(x, 5, Rgba([0, 0, 0, 120]));
        }
        let (q, _, _) = quantize(&img).unwrap();
        // Slot 0 is transparent (alpha 0)
        assert_eq!(q.palette[0][3], 0);
        // At least one palette slot should preserve the semi-transparent alpha around 120
        let has_translucent_slot = q.palette.iter().skip(1).any(|slot| slot[3] > 60 && slot[3] < 180);
        assert!(has_translucent_slot, "Quantized palette should preserve anti-aliased edge alpha: {:?}", q.palette);
    }
}
