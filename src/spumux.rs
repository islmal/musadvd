//! Origin: behaviour of dvdauthor's spumux (Copyright (C) 2002, 2003 Scott Smith, Jan Panteltje, GPL-2.0-or-later); see THIRD_PARTY_NOTICES.md.
//! Native replacement for `spumux -m dvd` on bitmap subpictures: encodes palette PNGs into DVD
//! subpicture units (RLE fields + SP_DCSQ command sequences) and multiplexes them as private
//! stream 1 (`0xBD`) PES packets into an MPEG-2 program stream.
//!
//! Behaviour was derived from `spumux.exe` (dvdauthor 0.7.2+) and is verified against its recorded
//! output byte for byte in `tests/spumux_native.rs`:
//! - every subpicture is preceded by a `dvdauthor-data` padding sector (stream id, PTS, end PTS and
//!   the CLUT as Y/Cr/Cb) that `dvdauthor` reads instead of an SPU-internal palette;
//! - the SPU sectors enter the stream at the first pack whose SCR reaches `PTS - 150 ms`;
//! - every pack header is rewritten so the SCR never falls below the previous one plus one sector.
//!
//! Text subtitles are rendered to bitmaps by [`crate::textsub`] and enter here like any other bitmap
//! stream; there is no external `spumux` in the product any more.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const SECTOR: usize = 2048;
/// 27 MHz ticks one sector takes at the DVD mux rate (10.08 Mbit/s).
const SECTOR_TICKS: u64 = 43_886;
/// Subpictures enter the stream this many 90 kHz ticks before their PTS.
const LEAD: i64 = 13_500;
/// Largest encoded SPU `spumux` accepts (its work buffer minus the trailing sequence).
const MAX_SPU: usize = 0xcfe3;
/// Payload bytes of one sector behind the 14-byte pack header and a 6-byte PES start.
const PAYLOAD: usize = SECTOR - 20;
/// Mux rate (units of 50 byte/s) of a DVD until the first pack header says otherwise.
const DEFAULT_MUX_RATE: u32 = 0x6270;

#[derive(Debug)]
pub enum SpumuxError {
    Io(io::Error),
    Xml(String),
    Image { path: PathBuf, reason: String },
    DuplicateStream(u8),
    /// Input spumux itself would reject or that this module does not reproduce (fatal: there is no
    /// external tool to fall back to).
    Unsupported(String),
    /// The button group search exceeded its iteration limit (number of buttons).
    TooComplex(usize),
}

impl fmt::Display for SpumuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpumuxError::Io(e) => write!(f, "I/O error: {e}"),
            SpumuxError::Xml(m) => write!(f, "Invalid subtitle XML: {m}"),
            SpumuxError::Image { path, reason } => write!(f, "Subtitle image {}: {reason}", path.display()),
            SpumuxError::DuplicateStream(id) => write!(f, "Substream 0x{id:02x} already exists in the video"),
            SpumuxError::Unsupported(m) => write!(f, "not supported: {m}"),
            SpumuxError::TooComplex(n) => write!(f, "button group search exceeded its limit ({n} buttons)"),
        }
    }
}

impl std::error::Error for SpumuxError {}

impl From<io::Error> for SpumuxError {
    fn from(e: io::Error) -> Self {
        SpumuxError::Io(e)
    }
}

/// One `<spu>` of a stream: when it shows, where, and which image.
#[derive(Debug, Clone, PartialEq)]
pub struct SpuDef {
    /// 90 kHz ticks.
    pub start: i64,
    pub end: Option<i64>,
    pub image: PathBuf,
    pub x: u32,
    pub y: u32,
    pub force: bool,
    /// Menu subpicture: highlight/select masks and buttons instead of `image`.
    pub menu: Option<MenuDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuDef {
    pub highlight: PathBuf,
    pub select: PathBuf,
    pub buttons: Vec<ButtonDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ButtonDef {
    pub name: String,
    pub rect: [i32; 4],
    /// Targets up, down, left, right; `None` lets spumux pick the nearest button in that direction.
    pub nav: [Option<String>; 4],
    /// Autoaction attribute: button automatically activates when highlighted.
    pub autoaction: bool,
}

/// Whether `xml` is a plain bitmap subtitle stream this module can mux (no text rendering, no
/// buttons). Handles both populated streams with `<spu>` tags and empty streams without subtitles.
pub fn handles_xml(xml: &str) -> bool {
    (xml.contains("<spu ") || (xml.contains("<subpictures") && xml.contains("<stream>")))
        && !xml.contains("<textsub")
        && !xml.contains("<button")
        && !xml.contains("highlight=")
}

/// Whether `xml` is a menu subpicture (highlight/select masks with buttons).
pub fn is_menu_xml(xml: &str) -> bool {
    xml.contains("<spu ") && !xml.contains("<textsub") && xml.contains("highlight=") && xml.contains("<button")
}

/// `HH:MM:SS.fff` (any number of fraction digits) in 90 kHz ticks.
fn parse_time(s: &str) -> Option<i64> {
    let mut parts = s.trim().split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
    let secs = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60 + whole.parse::<i64>().ok()?;
    let frac_ticks = if frac.is_empty() {
        0
    } else {
        let digits: i64 = frac.parse().ok()?;
        digits * 90_000 / 10_i64.pow(frac.len() as u32)
    };
    Some(secs * 90_000 + frac_ticks)
}

/// Attribute `name` of a tag body (`a="1" b="2"`), XML entities undone.
fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&key) {
        let at = from + i;
        // `xoffset=` must not match `offset=`.
        if at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace() {
            let rest = &tag[at + key.len()..];
            let v = &rest[..rest.find('"')?];
            return Some(v.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"));
        }
        from = at + key.len();
    }
    None
}

/// Parses the `<spu .../>` elements of a spumux XML as [`crate::subpic::render`] writes it
/// (images relative to `dir`).
pub fn parse_xml(xml: &str, dir: &Path) -> Result<Vec<SpuDef>, SpumuxError> {
    let bad = |m: &str| SpumuxError::Xml(m.to_string());
    let mut out = Vec::new();
    for segment in xml.split("<spu ").skip(1) {
        let close = segment.find('>').ok_or_else(|| bad("<spu> not closed"))?;
        let tag = &segment[..close];
        // `<spu .../>` has no body; otherwise the buttons run up to `</spu>`.
        let body = if tag.ends_with('/') { "" } else { segment[close..].split("</spu>").next().unwrap_or("") };
        let time = |name: &str| attr(tag, name).map(|v| parse_time(&v).ok_or_else(|| bad("time value"))).transpose();
        let num = |name: &str| attr(tag, name).map(|v| v.parse::<u32>().map_err(|_| bad("Zahl"))).transpose();
        let menu = match (attr(tag, "highlight"), attr(tag, "select")) {
            (Some(h), Some(s)) => {
                let mut buttons = Vec::new();
                for b in body.split("<button ").skip(1) {
                    let b = &b[..b.find('>').ok_or_else(|| bad("<button> not closed"))?];
                    let coord = |n: &str| attr(b, n).and_then(|v| v.parse::<i32>().ok()).ok_or_else(|| bad("Button-Koordinate"));
                    buttons.push(ButtonDef {
                        name: attr(b, "name").ok_or_else(|| bad("button without a name"))?,
                        rect: [coord("x0")?, coord("y0")?, coord("x1")?, coord("y1")?],
                        nav: ["up", "down", "left", "right"].map(|n| attr(b, n)),
                        autoaction: attr(b, "autoaction").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("autoaction")),
                    });
                }
                Some(MenuDef { highlight: dir.join(h), select: dir.join(s), buttons })
            }
            _ => None,
        };
        let image = match (&menu, attr(tag, "image")) {
            (Some(_), _) => PathBuf::new(),
            (None, i) => dir.join(i.ok_or_else(|| bad("image missing"))?),
        };
        out.push(SpuDef {
            start: time("start")?.ok_or_else(|| bad("start missing"))?,
            end: time("end")?,
            image,
            x: num("xoffset")?.unwrap_or(0),
            y: num("yoffset")?.unwrap_or(0),
            force: attr(tag, "force").is_some_and(|v| v == "yes"),
            menu,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Image -> SPU
// ---------------------------------------------------------------------------------------------

/// A loaded mask/subtitle picture. Like spumux the palette lists the *used* colours in order of
/// first appearance (raster scan), whatever the PNG's own palette order was.
struct Pic {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    palette: Vec<[u8; 4]>,
}

fn load_png(path: &Path) -> Result<Pic, SpumuxError> {
    let bad = |reason: String| SpumuxError::Image { path: path.to_path_buf(), reason };
    let file = std::fs::File::open(path)?;
    let mut decoder = png::Decoder::new(io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(|e| bad(e.to_string()))?;
    let info = reader.info();
    if info.color_type != png::ColorType::Indexed || info.bit_depth != png::BitDepth::Eight {
        return Err(SpumuxError::Unsupported(format!("{}: not an 8-bit palette image", path.display())));
    }
    let (plte, trns) = (info.palette.clone().unwrap_or_default(), info.trns.clone().unwrap_or_default());
    let mut buf = vec![0; reader.output_buffer_size().ok_or_else(|| bad("Image too large".into()))?];
    let frame = reader.next_frame(&mut buf).map_err(|e| bad(e.to_string()))?;
    let (width, height) = (frame.width, frame.height);
    if width > 720 || height > 576 {
        return Err(SpumuxError::Unsupported(format!("{}: larger than 720x576", path.display())));
    }
    let color_of = |i: u8| {
        let i = i as usize;
        let rgb = plte.get(i * 3..i * 3 + 3).map_or([0, 0, 0], |c| [c[0], c[1], c[2]]);
        [rgb[0], rgb[1], rgb[2], trns.get(i).copied().unwrap_or(255)]
    };
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut slot_of = [None; 256];
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for &p in &buf[..(width * height) as usize] {
        let slot = *slot_of[p as usize].get_or_insert_with(|| {
            let c = color_of(p);
            palette.iter().position(|k| *k == c).unwrap_or_else(|| {
                palette.push(c);
                palette.len() - 1
            })
        });
        pixels.push(slot as u8);
    }
    Ok(Pic { width, height, pixels, palette })
}

/// MSB-first bit writer over the SPU buffer.
struct Bits {
    buf: Vec<u8>,
    /// Free bits in the last byte (8 = byte aligned).
    free: u32,
}

impl Bits {
    fn put(&mut self, mut value: u32, mut n: u32) {
        while n > 0 {
            if self.free == 8 {
                self.buf.push(0);
            }
            let take = n.min(self.free);
            let chunk = (value >> (n - take)) & ((1 << take) - 1);
            *self.buf.last_mut().expect("pushed") |= (chunk << (self.free - take)) as u8;
            self.free -= take;
            n -= take;
            value &= (1 << n) - 1;
            if self.free == 0 {
                self.free = 8;
            }
        }
    }

    fn align(&mut self) {
        if self.free != 8 {
            self.put(0, self.free);
        }
    }

    /// One run of 1..=255 pixels in the shortest of the four DVD codes.
    fn run(&mut self, len: u32, color: u8) {
        let c = color as u32;
        match len {
            1..=3 => self.put(len << 2 | c, 4),
            4..=15 => self.put(len << 2 | c, 8),
            16..=63 => self.put(len << 2 | c, 12),
            _ => self.put(len << 2 | c, 16),
        }
    }

    /// One line; a trailing run of 64+ pixels becomes the "to end of line" code.
    fn line(&mut self, px: &[u8]) {
        if px.len() < 2 {
            self.run(1, px[0]);
        } else {
            let mut start = 0;
            for i in 0..px.len() - 1 {
                if px[i] == px[i + 1] {
                    continue;
                }
                let mut len = (i + 1 - start) as u32;
                while len >= 0x100 {
                    self.put(0x3fc | px[i] as u32, 16);
                    len -= 0xff;
                }
                self.run(len, px[i]);
                start = i + 1;
            }
            let (rest, color) = ((px.len() - start) as u32, px[px.len() - 1]);
            if rest < 0x40 {
                self.run(rest, color);
            } else {
                self.put(color as u32, 16);
            }
        }
        self.align();
    }
}

/// Palette slot of `rgb` in the SPU's CLUT (appended when new; the CLUT holds 16 entries).
fn clut_slot(clut: &mut Vec<[u8; 3]>, rgb: [u8; 3]) -> Option<u32> {
    match clut.iter().position(|c| *c == rgb) {
        Some(i) => Some(i as u32),
        None if clut.len() < 16 => {
            clut.push(rgb);
            Some(clut.len() as u32 - 1)
        }
        None => None,
    }
}

/// spumux's view of a bitmap: padded to even size, pixel values numbered by first appearance in
/// raster order, cropped to the visible pixels (even origin and size). Returns the new bitmap and
/// the shifted position; `None` with more than 4 distinct colours.
fn normalize(pic: &Pic, x: u32, y: u32) -> Option<(Prepared, u32, u32)> {
    let (w, h) = (((pic.width + 1) & !1) as usize, ((pic.height + 1) & !1) as usize);
    let at = |col: usize, row: usize| if col < pic.width as usize && row < pic.height as usize { pic.pixels[row * pic.width as usize + col] } else { 0 };
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut pixels = Vec::with_capacity(w * h);
    for row in 0..h {
        for col in 0..w {
            let c = pic.palette[at(col, row) as usize];
            let v = match colors.iter().position(|k| *k == c) {
                Some(v) => v,
                None if colors.len() < 4 => {
                    colors.push(c);
                    colors.len() - 1
                }
                None => return None,
            };
            pixels.push(v as u8);
        }
    }
    let mut palette = [[0u8; 4]; 4];
    palette[..colors.len()].copy_from_slice(&colors);
    let vis: Vec<bool> = pixels.iter().map(|&p| palette[p as usize][3] != 0).collect();
    Some(crop(pixels, w, h, palette, |i| vis[i], x, y))
}

/// Cuts `pixels` (`w`×`h`) down to the visible area with even origin and size, shifting `x`/`y`;
/// everything stays when nothing is visible.
fn crop(pixels: Vec<u8>, w: usize, h: usize, colors: [[u8; 4]; 4], visible: impl Fn(usize) -> bool, x: u32, y: u32) -> (Prepared, u32, u32) {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for i in (0..pixels.len()).filter(|&i| visible(i)) {
        let (col, row) = (i % w, i / w);
        (x0, y0, x1, y1) = (x0.min(col), y0.min(row), x1.max(col), y1.max(row));
    }
    if x0 == usize::MAX {
        return (Prepared { width: w as u32, height: h as u32, pixels, colors }, x, y);
    }
    let (x0, y0) = (x0 & !1, y0 & !1);
    let (cw, ch) = ((x1 + 2 - x0) & !1, (y1 + 2 - y0) & !1);
    let cropped = (y0..y0 + ch).flat_map(|row| pixels[row * w + x0..row * w + x0 + cw].iter().copied()).collect();
    (Prepared { width: cw as u32, height: ch as u32, pixels: cropped, colors }, x + x0 as u32, y + y0 as u32)
}

/// A picture ready for the SPU: pixel values 0..=3 and the colour of each (RGBA, alpha 0 = unused
/// or transparent).
struct Prepared {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    colors: [[u8; 4]; 4],
}

/// Encoded SPU; `clut` holds the colours registered so far and gets the visible ones of `bmp`
/// added. `None` when it does not fit `MAX_SPU` or needs more than 16 colours.
fn encode_spu(bmp: &Prepared, x: u32, y: u32, force: bool, dur: i64, clut: &mut Vec<[u8; 3]>) -> Option<Vec<u8>> {
    let w = bmp.width as usize;
    let mut bits = Bits { buf: vec![0, 0, 0, 0], free: 8 };
    let mut bottom = 0;
    for first in [0usize, 1] {
        if first == 1 {
            bottom = bits.buf.len() as u16;
        }
        for row in (first..bmp.height as usize).step_by(2) {
            let before = bits.buf.len();
            bits.line(&bmp.pixels[row * w..(row + 1) * w]);
            // spumux aborts the whole run on a row of 1440 bits or more; skipping that picture is gentler.
            if bits.buf.len() - before >= 180 {
                return None;
            }
        }
    }
    let mut out = bits.buf;
    // CLUT slots are handed out in pixel value order; SET_COLOR / SET_CONTR list values 3, 2, 1, 0.
    let mut slot = [0u32; 4];
    for (v, s) in slot.iter_mut().enumerate() {
        let c = bmp.colors[v];
        *s = if c[3] == 0 { 0 } else { clut_slot(clut, [c[0], c[1], c[2]])? };
    }
    let color_nibbles = [slot[3], slot[2], slot[1], slot[0]];
    let nib = |n: [u32; 4]| ((n[0] << 12 | n[1] << 8 | n[2] << 4 | n[3]) as u16).to_be_bytes();

    let dcsq = out.len();
    let (x1, y1) = (x + bmp.width - 1, y + bmp.height - 1);
    let mut cmds = vec![0, 0, 0, 0, if force { 0x00 } else { 0x01 }, 0x03];
    cmds.extend(nib(color_nibbles));
    cmds.push(0x04);
    cmds.extend(nib([3, 2, 1, 0].map(|v| (bmp.colors[v][3] >> 4) as u32)));
    cmds.push(0x05);
    cmds.extend([(x >> 4) as u8, (x << 4) as u8 | (x1 >> 8) as u8 & 0xf, x1 as u8]);
    cmds.extend([(y >> 4) as u8, (y << 4) as u8 | (y1 >> 8) as u8 & 0xf, y1 as u8]);
    cmds.push(0x06);
    cmds.extend(4u16.to_be_bytes());
    cmds.extend(bottom.to_be_bytes());
    cmds.push(0xff);
    // Delay 0; the next-sequence pointer is patched below.
    out.extend(&cmds);
    let first_next = dcsq + 2;
    if dur < 0 {
        out[first_next..first_next + 2].copy_from_slice(&(dcsq as u16).to_be_bytes());
    } else {
        let second = out.len();
        out[first_next..first_next + 2].copy_from_slice(&(second as u16).to_be_bytes());
        // Delays are in 1024/90000 s; longer ones chain dummy sequences of 0xffff.
        let mut delay = (dur + 0x200) >> 10;
        let mut at = second;
        while delay > 0xffff {
            out.extend(0xffffu16.to_be_bytes());
            out.extend(((at + 5) as u16).to_be_bytes());
            out.push(0xff);
            delay -= 0xffff;
            at += 5;
        }
        out.extend((delay as u16).to_be_bytes());
        out.extend((at as u16).to_be_bytes());
        out.extend([0x02, 0xff]);
    }
    if out.len() % 2 == 1 {
        out.push(0xff);
    }
    if out.len() > MAX_SPU {
        return None;
    }
    let (size, dcsq) = (out.len() as u16, dcsq as u16);
    out[0..2].copy_from_slice(&size.to_be_bytes());
    out[2..4].copy_from_slice(&dcsq.to_be_bytes());
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Menu masks: button groups
// ---------------------------------------------------------------------------------------------

const EMPTY: u32 = u32::MAX;

/// Hard cap on button-to-group assignments tried per menu (all attempts together). Menus that
/// group at once need one trial; this only stops pathological ones (3^N growth).
const MAX_GROUP_TRIALS: u64 = 2_000_000;

/// Everything `dvdauthor-data` needs about a menu subpicture besides the CLUT.
struct MenuData {
    /// Per group the four pixel value slots as packed (image, highlight, select) palette indices.
    groups: Vec<[u32; 4]>,
    hl_palette: Vec<[u8; 4]>,
    sel_palette: Vec<[u8; 4]>,
    buttons: Vec<ButtonOut>,
}

struct ButtonOut {
    name: String,
    group: u8,
    /// Absolute x0, y0, x1, y1.
    rect: [i32; 4],
    nav: [String; 4],
    autoaction: bool,
}

/// Angle (degrees) and distance of the lateral offset `lat` at depth `dist` (`FUN_1400067d0`).
fn bearing(a0: i32, a1: i32, b0: i32, b1: i32, dist: i32) -> (i32, i32) {
    if a1 <= b0 || b1 <= a0 {
        let d = dist + 1;
        let t = a0 - b1 + 1;
        let p = a1 - b0 - 1;
        let mut lat = if p.abs() <= t.abs() { p } else { t };
        let sign = if lat < 0 { -1.0 } else { 1.0 };
        lat = lat.abs();
        let angle = ((lat as f64 / d as f64).atan() * 57.295_779_513_082_32 * sign) as i32;
        (angle, ((lat * lat + d * d) as f64).sqrt() as i32)
    } else {
        (0, dist)
    }
}

/// Where `other` lies seen from `me` as (angle, distance), `None` when they overlap (`FUN_1400068a0`).
fn direction(other: [i32; 4], me: [i32; 4]) -> Option<(i32, i32)> {
    let [ox0, oy0, ox1, oy1] = other;
    let [sx0, sy0, sx1, sy1] = me;
    Some(if sy0 < oy1 {
        if oy0 < sy1 {
            if sx0 < ox1 {
                if ox0 < sx1 {
                    return None;
                }
                let (a, d) = bearing(oy0, oy1, sy0, sy1, ox0 - sx1);
                (a + 90, d)
            } else {
                let (a, d) = bearing(oy0, oy1, sy0, sy1, sx0 - ox1);
                (270 - a, d)
            }
        } else {
            let (a, d) = bearing(ox0, ox1, sx0, sx1, oy0 - sy1);
            (180 - a, d)
        }
    } else {
        bearing(ox0, ox1, sx0, sx1, sy0 - oy1)
    })
}

/// Nearest button within 90° of `toward` degrees, else the button itself (`FUN_1400069f0`).
fn default_target(buttons: &[ButtonDef], me: usize, toward: i32) -> String {
    let mut best: Option<(i32, i32, &str)> = None;
    for (i, other) in buttons.iter().enumerate() {
        let Some((angle, dist)) = (i != me).then(|| direction(other.rect, buttons[me].rect)).flatten() else { continue };
        let diff = (angle - toward).abs();
        if diff < 90 && best.is_none_or(|(d, l, _)| diff < d || (diff == d && dist < l)) {
            best = Some((diff, dist, &other.name));
        }
    }
    best.map_or(buttons[me].name.clone(), |b| b.2.to_string())
}

type ButtonColors = (Vec<[u32; 4]>, Vec<u8>);

/// Result of the button group search (`FUN_140007080`): per group the slots, per button its group.
fn pick_groups(
    packed: &dyn Fn(usize, usize, bool) -> u32,
    rects: &[[i32; 4]],
    pals: [&[[u8; 4]]; 3],
    groups: usize,
    use_img: bool,
    budget: &mut u64,
) -> Result<Option<ButtonColors>, SpumuxError> {
    // Distinct packed values under each button, at most four.
    let mut per_button: Vec<Vec<u32>> = Vec::new();
    for r in rects {
        let mut set: Vec<u32> = Vec::new();
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                let v = packed(x as usize, y as usize, use_img);
                if !set.contains(&v) {
                    if set.len() == 4 {
                        return Ok(None);
                    }
                    set.push(v);
                }
            }
        }
        per_button.push(set);
    }
    let limit = groups.checked_pow(rects.len() as u32).unwrap_or(usize::MAX).max(1);
    'trial: for trial in 0..limit {
        // spumux itself would run for up to groups^buttons trials; stop with an error instead.
        if *budget == 0 {
            return Err(SpumuxError::TooComplex(rects.len()));
        }
        *budget -= 1;
        let assign: Vec<usize> = (0..rects.len()).map(|i| groups.checked_pow(i as u32).map_or(0, |p| trial / p % groups)).collect();
        let mut sets: Vec<Vec<u32>> = vec![Vec::new(); groups];
        for (b, set) in per_button.iter().enumerate() {
            for &v in set {
                let s = &mut sets[assign[b]];
                if !s.contains(&v) {
                    if s.len() == 4 {
                        continue 'trial;
                    }
                    s.push(v);
                }
            }
        }
        let mut table = vec![[EMPTY; 4]; groups];
        if use_img {
            let mut count = pals[0].len();
            let mut list: Vec<u32> = (0..4).map(|i| i as u32).collect();
            for (g, s) in sets.iter().enumerate() {
                for e in list.iter_mut().take(count) {
                    *e &= 0xff;
                }
                for &v in s {
                    let k = v >> 16;
                    let j = match list[..count].iter().position(|&e| e == k) {
                        Some(j) => j,
                        None if count == 4 => continue 'trial,
                        None => {
                            count += 1;
                            count - 1
                        }
                    };
                    list[j] = k | 0x100;
                    table[g][j] = v;
                }
            }
        } else {
            for (g, s) in sets.iter().enumerate() {
                table[g][..s.len()].copy_from_slice(s);
            }
        }
        // A fully transparent combination goes to slot 0.
        for t in table.iter_mut() {
            for j in 0..4 {
                let v = t[j];
                if v == EMPTY {
                    if j != 0 {
                        let first = |p: &[[u8; 4]]| p.iter().position(|c| c[3] == 0);
                        let mut n = first(pals[0]).map_or(0, |i| (i as u32) << 16);
                        n |= first(pals[1]).map_or(0, |i| (i as u32) << 8);
                        n |= first(pals[2]).map_or(0, |i| i as u32);
                        t[j] = t[0];
                        t[0] = n;
                    }
                    break;
                }
                let transparent = pals[0].get((v >> 16 & 0xff) as usize).is_none_or(|c| c[3] == 0)
                    && pals[1].get((v >> 8 & 0xff) as usize).is_none_or(|c| c[3] == 0)
                    && pals[2].get((v & 0xff) as usize).is_none_or(|c| c[3] == 0);
                if transparent {
                    if j > 0 {
                        t[j] = t[0];
                        t[0] = v;
                    }
                    break;
                }
            }
        }
        return Ok(Some((table, assign.iter().map(|&g| g as u8 + 1).collect())));
    }
    Ok(None)
}

type PreparedMenu = (Prepared, u32, u32, Vec<[u8; 3]>, MenuData);

/// Mask pair + buttons → SPU picture and `dvdauthor-data` button info. `Ok(None)` when spumux
/// would reject the input (it then skips the subpicture).
fn prepare_menu(menu: &MenuDef, x: u32, y: u32) -> Result<Option<PreparedMenu>, SpumuxError> {
    let (hl, sel) = (load_png(&menu.highlight)?, load_png(&menu.select)?);
    if (hl.width, hl.height) != (sel.width, sel.height) || hl.width % 2 != 0 || hl.height % 2 != 0 {
        return Err(SpumuxError::Unsupported("Masks differ in size or are odd-sized".into()));
    }
    let (w, h) = (hl.width as usize, hl.height as usize);
    let img_palette = [[0u8; 4]];
    let packed = |px: usize, py: usize, use_img: bool| {
        let i = py * w + px;
        let _ = use_img; // the blank base picture only has colour 0
        (hl.pixels[i] as u32) << 8 | sel.pixels[i] as u32
    };
    for b in &menu.buttons {
        let r = b.rect;
        if r[0] != r[2] && r[1] != r[3] && (r[0] < 0 || r[2] < r[0] || r[2] > w as i32 || r[1] < 0 || r[3] < r[1] || r[3] > h as i32) {
            return Err(SpumuxError::Unsupported(format!("Button {} outside the picture", b.name)));
        }
    }
    let rects: Vec<[i32; 4]> = menu.buttons.iter().map(|b| b.rect).collect();
    let pals: [&[[u8; 4]]; 3] = [&img_palette, &hl.palette, &sel.palette];
    let mut budget = MAX_GROUP_TRIALS;
    let mut picked = None;
    'search: for use_img in [true, false] {
        for g in 1..=3 {
            if let Some(p) = pick_groups(&packed, &rects, pals, g, use_img, &mut budget)? {
                picked = Some(p);
                break 'search;
            }
        }
    }
    let Some((table, group_of)) = picked else { return Ok(None) };

    let mut map = vec![0xffu8; w * h];
    let mut base: [Option<[u8; 4]>; 4] = [None; 4];
    let blank = [0u8; 4];
    for (b, r) in rects.iter().enumerate() {
        for py in r[1]..r[3] {
            for px in r[0]..r[2] {
                let i = py as usize * w + px as usize;
                let v = packed(px as usize, py as usize, true);
                let Some(j) = table[group_of[b] as usize - 1].iter().position(|&s| s == v) else { return Ok(None) };
                if map[i] != 0xff && map[i] as usize != j {
                    return Ok(None);
                }
                map[i] = j as u8;
                base[j] = Some(blank);
            }
        }
    }
    for m in map.iter_mut().filter(|m| **m == 0xff) {
        let slot = match base.iter().position(|c| *c == Some(blank)) {
            Some(k) => k,
            None => match base.iter().position(Option::is_none) {
                Some(k) => {
                    base[k] = Some(blank);
                    k
                }
                None => return Ok(None),
            },
        };
        *m = slot as u8;
    }
    let colors = base.map(|c| c.unwrap_or([0; 4]));
    let (prep, sx, sy) = crop(map, w, h, colors, |i| hl.palette[hl.pixels[i] as usize][3] != 0 || sel.palette[sel.pixels[i] as usize][3] != 0, x, y);

    let mut clut = Vec::new();
    for c in hl.palette.iter().chain(&sel.palette).filter(|c| c[3] != 0) {
        if clut_slot(&mut clut, [c[0], c[1], c[2]]).is_none() {
            return Err(SpumuxError::Unsupported("more than 16 colours".into()));
        }
    }
    let buttons = menu
        .buttons
        .iter()
        .enumerate()
        .map(|(i, b)| ButtonOut {
            name: b.name.clone(),
            group: group_of[i],
            rect: [b.rect[0] + x as i32, b.rect[1] + y as i32, b.rect[2] + x as i32, b.rect[3] + y as i32],
            nav: [0, 1, 2, 3].map(|d| b.nav[d].clone().unwrap_or_else(|| default_target(&menu.buttons, i, [0, 180, 270, 90][d]))),
            autoaction: b.autoaction,
        })
        .collect();
    Ok(Some((prep, sx, sy, clut, MenuData { groups: table, hl_palette: hl.palette, sel_palette: sel.palette, buttons })))
}

// ---------------------------------------------------------------------------------------------
// Multiplexer
// ---------------------------------------------------------------------------------------------

/// What a run did, for the log.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Stats {
    pub added: u32,
    pub skipped: u32,
    /// First video PTS the subpictures were shifted by (90 kHz ticks).
    pub offset: i64,
}

struct Record {
    pts: i64,
    /// -1: no end time.
    dur: i64,
    x: u32,
    y: u32,
    force: bool,
    prep: Prepared,
    /// Colours registered by the images, in order.
    clut: Vec<[u8; 3]>,
    menu: Option<MenuData>,
}

struct Mux<W: Write> {
    out: W,
    written: u64,
    stream: u8,
    defs: std::vec::IntoIter<SpuDef>,
    head: Option<Record>,
    /// First video PTS; subpicture times are relative to it.
    offset: Option<i64>,
    /// SCR (27 MHz) of the pack being written.
    scr: u64,
    /// Lowest SCR the next pack may carry.
    next_min: u64,
    rate: u32,
    /// PES header size of the next SPU packet (12 = with P-STD buffer, 9 = PTS only, 4 = continuation).
    pes_header: usize,
    seen: [bool; 256],
    stats: Stats,
}

impl<W: Write> Mux<W> {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.written += bytes.len() as u64;
        self.out.write_all(bytes)
    }

    /// Next definition with its image loaded (`FUN_1400016d0`); unusable ones are skipped.
    fn fetch(&mut self) -> Result<Option<Record>, SpumuxError> {
        for def in self.defs.by_ref() {
            let pts = def.start + self.offset.filter(|o| *o > 0).unwrap_or(0);
            let dur = def.end.map_or(-1, |e| e - def.start);
            if def.end.is_some() && dur <= 0 {
                self.stats.skipped += 1;
                continue;
            }
            let (prep, x, y, clut, menu) = match &def.menu {
                Some(m) => match prepare_menu(m, def.x, def.y)? {
                    Some((prep, x, y, clut, data)) => (prep, x, y, clut, Some(data)),
                    None => {
                        self.stats.skipped += 1;
                        continue;
                    }
                },
                None => {
                    let pic = load_png(&def.image)?;
                    let Some((prep, x, y)) = normalize(&pic, def.x, def.y) else {
                        self.stats.skipped += 1;
                        continue;
                    };
                    // spumux enters every opaque colour of the picture into the CLUT while loading.
                    let mut clut = Vec::new();
                    for c in pic.palette.iter().filter(|c| c[3] != 0) {
                        if clut_slot(&mut clut, [c[0], c[1], c[2]]).is_none() {
                            return Err(SpumuxError::Unsupported("more than 16 colours".into()));
                        }
                    }
                    (prep, x, y, clut, None)
                }
            };
            return Ok(Some(Record { pts, dur, x, y, force: def.force, prep, clut, menu }));
        }
        Ok(None)
    }

    fn pack_header(&mut self, scr: u64) -> io::Result<()> {
        let (base, ext) = (scr / 300, scr % 300);
        let r = self.rate;
        let h = [
            0x00,
            0x00,
            0x01,
            0xba,
            0x44 | ((base >> 27) & 0x38) as u8 | ((base >> 28) & 3) as u8,
            (base >> 20) as u8,
            ((base >> 12) & 0xf8) as u8 | ((base >> 13) & 3) as u8 | 4,
            (base >> 5) as u8,
            (base << 3) as u8 | (ext >> 7) as u8 | 4,
            (ext * 2 + 1) as u8,
            (r >> 14) as u8,
            (r >> 6) as u8,
            (r * 4 + 3) as u8,
            0xf8,
        ];
        self.put(&h)
    }

    /// Writes a pack header for `self.scr` and advances the SCR bookkeeping; the header carries the
    /// SCR as it was *before* the clamp, like `spumux` does.
    fn next_pack(&mut self) -> io::Result<()> {
        self.pack_header(self.scr)?;
        self.scr = self.scr.max(self.next_min);
        self.next_min = self.scr + SECTOR_TICKS;
        Ok(())
    }

    /// Inserts every queued subpicture that is due (all of them when `force`).
    fn insert(&mut self, force: bool) -> Result<(), SpumuxError> {
        if self.head.is_none() {
            self.head = self.fetch()?;
        }
        while let Some(mut cur) = self.head.take() {
            let target = ((cur.pts - LEAD).max(0) as u64) * 300;
            if self.scr < target && !force {
                self.head = Some(cur);
                return Ok(());
            }
            let next = self.fetch()?;
            if let Some(n) = &next {
                if cur.dur != -1 && cur.pts + cur.dur + 0x59 >= n.pts {
                    cur.dur = -1; // overlaps the next one
                }
                if cur.dur == -1 {
                    // Shown until just before the next one.
                    if cur.pts + 0x5a >= n.pts {
                        self.stats.skipped += 1;
                        self.head = next;
                        continue;
                    }
                    cur.dur = n.pts - cur.pts - 0x5a;
                }
            }
            self.head = next;
            let Some(spu) = encode_spu(&cur.prep, cur.x, cur.y, cur.force, cur.dur, &mut cur.clut) else {
                self.stats.skipped += 1;
                continue;
            };
            self.stats.added += 1;
            self.scr = target;
            self.data_sector(&mut cur)?;
            self.spu_sectors(&spu, cur.pts)?;
        }
        Ok(())
    }

    /// The `dvdauthor-data` padding sector.
    fn data_sector(&mut self, cur: &mut Record) -> io::Result<()> {
        self.next_pack()?;
        // Group colour/contrast tables first: they may only reference CLUT entries that exist.
        let mut tables = Vec::new();
        if let Some(m) = &cur.menu {
            let entry = |pal: &[[u8; 4]], i: u32, clut: &mut Vec<[u8; 3]>| {
                let c = pal.get(i as usize).copied().unwrap_or([0; 4]);
                let slot = if c[3] == 0 { 0 } else { clut_slot(clut, [c[0], c[1], c[2]]).unwrap_or(0) };
                (slot as u16, (c[3] >> 4) as u16)
            };
            for g in &m.groups {
                // Lanes: highlight colours/contrast, select colours/contrast; slots 3..0, 4 bits each.
                let mut lanes = [0u16; 4];
                for &v in g.iter().rev() {
                    lanes = lanes.map(|l| l << 4);
                    if v != EMPTY {
                        let (hc, ha) = entry(&m.hl_palette, v >> 8 & 0xff, &mut cur.clut);
                        let (sc, sa) = entry(&m.sel_palette, v & 0xff, &mut cur.clut);
                        for (l, add) in lanes.iter_mut().zip([hc, ha, sc, sa]) {
                            *l |= add;
                        }
                    }
                }
                tables.push(lanes);
            }
        }
        let clut = &cur.clut;
        let mut d = b"dvdauthor-data\0".to_vec();
        d.extend([2, 1, self.stream]);
        d.extend((cur.pts as u32).to_be_bytes());
        d.extend(if cur.dur == -1 { u32::MAX } else { (cur.pts + cur.dur) as u32 }.to_be_bytes());
        d.extend([1, clut.len() as u8]);
        for c in clut {
            let [r, g, b] = c.map(|v| v as i32);
            d.extend([
                ((r * 257 + g * 504 + b * 98 + 16_500) / 1000) as u8,
                ((r * 439 - g * 368 + 128_500 - b * 71) / 1000) as u8,
                ((-r * 148 - g * 291 + 128_500 + b * 439) / 1000) as u8,
            ]);
        }
        if let Some(m) = &cur.menu {
            if !tables.is_empty() {
                d.extend([2, tables.len() as u8]);
                for lane in tables.iter().flatten() {
                    d.extend(lane.to_be_bytes());
                }
            }
            if !m.buttons.is_empty() {
                d.extend([3, m.buttons.len() as u8]);
                for b in &m.buttons {
                    d.extend(b.name.bytes().chain([0]));
                    // Two modifier bytes, then autoaction and group: the layout `dvd_mux` (dvdauthor) reads.
                    d.extend([0, 0, u8::from(b.autoaction), b.group]);
                    for v in b.rect {
                        d.extend((v as u16).to_be_bytes());
                    }
                    for n in &b.nav {
                        d.extend(n.bytes().chain([0]));
                    }
                }
            }
        }
        d.resize(PAYLOAD, 0xff);
        self.put(&[0, 0, 1, 0xbe])?;
        self.put(&(PAYLOAD as u16).to_be_bytes())?;
        self.put(&d)
    }

    fn spu_sectors(&mut self, spu: &[u8], pts: i64) -> io::Result<()> {
        let mut off = 0;
        while off < spu.len() {
            if self.pes_header != 12 {
                self.pes_header = if off == 0 { 9 } else { 4 };
            }
            let hdr = self.pes_header;
            let avail = PAYLOAD - hdr;
            let rest = spu.len() - off;
            let (chunk, stuff) = if rest > avail { (avail, 0) } else { (rest, if avail - rest < 8 { avail - rest } else { 0 }) };
            self.next_pack()?;
            let p = pts as u64;
            let mut h = Vec::with_capacity(hdr + stuff);
            match hdr {
                12 | 9 => {
                    let ext = hdr == 12;
                    h.extend([0x81, if ext { 0x81 } else { 0x80 }, (stuff + hdr - 4) as u8]);
                    h.extend([
                        0x21 | ((p >> 29) & 6) as u8,
                        (p >> 22) as u8,
                        (p >> 14) as u8 | 1,
                        (p >> 7) as u8,
                        (p << 1) as u8 | 1,
                    ]);
                    if ext {
                        h.extend([0x1e, 0x60, 0x3a]);
                    }
                }
                _ => h.extend([0x81, 0x00, stuff as u8]),
            }
            h.resize(hdr - 1 + stuff, 0xff);
            h.push(self.stream);
            self.put(&[0, 0, 1, 0xbd])?;
            self.put(&((hdr + stuff + chunk) as u16).to_be_bytes())?;
            self.put(&h)?;
            self.put(&spu[off..off + chunk])?;
            let used = 20 + hdr + stuff + chunk;
            if used != SECTOR {
                self.put(&[0, 0, 1, 0xbe])?;
                let pad = SECTOR - 6 - used;
                self.put(&(pad as u16).to_be_bytes())?;
                self.put(&vec![0xff; pad])?;
            }
            off += chunk;
            if off < spu.len() {
                self.pes_header = 4;
            }
        }
        Ok(())
    }

    /// First video PES: its PTS is the zero point of the subpicture times.
    fn note_video(&mut self, pes: &[u8]) {
        if self.offset.is_some() || pes.len() < 8 {
            return;
        }
        if pes[1] > 0x3f && pes[2] > 3 && pes[3] & 0xe1 == 0x21 && pes[5] & 1 != 0 && pes[7] & 1 != 0 {
            let pts = ((pes[3] as i64 & 0xe) << 29) | (pes[4] as i64) << 22 | ((pes[5] as i64 & 0xfe) << 14) | (pes[6] as i64) << 7 | (pes[7] as i64 >> 1);
            self.offset = Some(pts);
            self.stats.offset = pts;
            if let Some(h) = &mut self.head {
                if pts > 0 {
                    h.pts += pts;
                }
            }
        }
    }

    fn run(&mut self, input: impl Read) -> Result<(), SpumuxError> {
        let mut input = io::BufReader::with_capacity(1 << 20, input);
        self.head = self.fetch()?;
        let mut code = [0u8; 4];
        let mut payload = Vec::new();
        loop {
            if read_full(&mut input, &mut code)? != 4 {
                break;
            }
            match u32::from_be_bytes(code) {
                0x1ba => {
                    let mut h = [0u8; 10];
                    if read_full(&mut input, &mut h)? != 10 {
                        break;
                    }
                    let valid = h[8] & 3 == 3 && h[5] & 1 != 0 && h[4] & 4 != 0 && h[2] & 4 != 0 && h[0] & 0xc4 == 0x44;
                    if valid {
                        let base = ((h[0] as u64 & 0x38) << 27)
                            | ((h[0] as u64 & 3) << 28)
                            | (h[1] as u64) << 20
                            | ((h[2] as u64 & 0xf8) << 12)
                            | ((h[2] as u64 & 3) << 13)
                            | (h[3] as u64) << 5
                            | (h[4] as u64 >> 3);
                        self.scr = base * 300 + ((h[4] as u64 & 3) << 7 | h[5] as u64 >> 1);
                        if self.scr != 0 && self.offset.is_some() && self.written.is_multiple_of(SECTOR as u64) {
                            self.insert(false)?;
                        }
                        self.scr = self.scr.max(self.next_min);
                        self.next_min = self.scr + SECTOR_TICKS;
                        let rate = (h[8] as u32) >> 2 | (h[7] as u32) << 6 | (h[6] as u32) << 14;
                        self.pack_header(self.scr)?;
                        self.rate = rate;
                    } else {
                        self.scr = self.next_min;
                        self.pack_header(self.scr)?;
                    }
                }
                0x1b9 => self.put(&code)?,
                c @ 0x1bb..=0x1ef => {
                    let mut len = [0u8; 2];
                    if read_full(&mut input, &mut len)? != 2 {
                        break;
                    }
                    payload.resize(u16::from_be_bytes(len) as usize, 0);
                    if read_full(&mut input, &mut payload)? != payload.len() {
                        break;
                    }
                    self.put(&code)?;
                    self.put(&len)?;
                    self.put(&payload)?;
                    if c == 0x1e0 {
                        self.note_video(&payload);
                    } else if c == 0x1bd && payload.len() > 3 {
                        let at = payload[2] as usize + 3;
                        if let Some(&id) = payload.get(at) {
                            if id & 0xe0 != 0x80 {
                                if id == self.stream {
                                    return Err(SpumuxError::DuplicateStream(id));
                                }
                                self.seen[id as usize] = true;
                            }
                        }
                    }
                }
                _ => {
                    // Garbage: copy up to the next pack start.
                    self.put(&code)?;
                    let mut acc = 0u32;
                    let mut b = [0u8; 1];
                    while acc != 0x1ba {
                        if read_full(&mut input, &mut b)? != 1 {
                            break;
                        }
                        self.put(&b)?;
                        acc = acc << 8 | b[0] as u32;
                    }
                    if acc != 0x1ba {
                        break;
                    }
                    // The pack start was already copied: its header follows like a normal pack.
                    let mut h = [0u8; 10];
                    if read_full(&mut input, &mut h)? != 10 {
                        break;
                    }
                    self.put(&h)?;
                }
            }
        }
        self.insert(true)?;
        self.out.flush()?;
        Ok(())
    }
}

/// Reads until `buf` is full or the input ends; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Muxes `defs` as subpicture stream `stream` (0..=31) into the program stream `input`, writing
/// the result to `output`.
pub fn mux(input: impl Read, output: impl Write, defs: Vec<SpuDef>, stream: u8) -> Result<Stats, SpumuxError> {
    let mut m = Mux {
        out: io::BufWriter::with_capacity(1 << 20, output),
        written: 0,
        stream: 0x20 + stream,
        defs: defs.into_iter(),
        head: None,
        offset: None,
        scr: 0,
        next_min: 0,
        rate: DEFAULT_MUX_RATE,
        pes_header: 12,
        seen: [false; 256],
        stats: Stats::default(),
    };
    m.run(input)?;
    Ok(m.stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twelve buttons with two private colour pairs each cannot share three groups: the search
    /// must stop at its budget instead of running through 3^12 assignments.
    #[test]
    fn group_search_stops_at_its_limit() {
        let rects: Vec<[i32; 4]> = (0..12).map(|i| [2 * i, 0, 2 * i + 2, 1]).collect();
        let pal = [[0u8; 4]; 1];
        let pals: [&[[u8; 4]]; 3] = [&pal, &pal, &pal];
        let packed = |x: usize, _y: usize, _img: bool| x as u32;
        let mut budget = 1000;
        let r = pick_groups(&packed, &rects, pals, 3, true, &mut budget);
        assert!(matches!(r, Err(SpumuxError::TooComplex(12))), "{:?}", r.map(|o| o.is_some()));
        // A solvable menu still succeeds within a tiny budget.
        let mut budget = 10;
        assert!(pick_groups(&packed, &rects[..2], pals, 1, true, &mut budget).unwrap().is_some());
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("00:00:01.50"), Some(135_000));
        assert_eq!(parse_time("01:02:03.004"), Some((3723 * 1000 + 4) * 90));
        assert_eq!(parse_time("00:00:00"), Some(0));
        assert_eq!(parse_time("1:2"), None);
    }

    #[test]
    fn xml_roundtrip() {
        let xml = r#"<subpictures format="PAL"><stream>
    <spu start="00:00:01.00" end="00:00:02.50" image="s/00001.png" xoffset="10" yoffset="20" force="yes"/>
    <spu start="00:00:03.00" image="s/00002.png" xoffset="1" yoffset="2"/>
</stream></subpictures>"#;
        assert!(handles_xml(xml));
        let d = parse_xml(xml, Path::new("w")).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].start, d[0].end, d[0].x, d[0].y, d[0].force), (90_000, Some(225_000), 10, 20, true));
        assert_eq!((d[1].end, d[1].force), (None, false));
        assert!(!handles_xml("<subpictures><stream><textsub/></stream></subpictures>"));

        let empty_xml = "<subpictures format=\"PAL\">\n  <stream>\n  </stream>\n</subpictures>";
        assert!(handles_xml(empty_xml));
        let empty_d = parse_xml(empty_xml, Path::new("w")).unwrap();
        assert!(empty_d.is_empty());
    }

    #[test]
    fn empty_subtitle_stream_muxes_without_error() {
        let empty_xml = "<subpictures format=\"PAL\">\n  <stream>\n  </stream>\n</subpictures>";
        let defs = parse_xml(empty_xml, Path::new("w")).unwrap();
        let mut out = Vec::new();
        let stats = mux(std::io::Cursor::new(vec![]), &mut out, defs, 0).unwrap();
        assert_eq!(stats.added, 0);
        assert_eq!(stats.skipped, 0);
    }

    #[test]
    fn menu_xml_button_autoaction() {
        let xml = r#"<subpictures><stream>
    <spu start="00:00:00" highlight="h.png" select="s.png">
        <button name="btn1" x0="10" y0="20" x1="50" y1="60" autoaction="1"/>
        <button name="btn2" x0="70" y0="20" x1="110" y1="60"/>
    </spu>
</stream></subpictures>"#;
        assert!(is_menu_xml(xml));
        let d = parse_xml(xml, Path::new("w")).unwrap();
        assert_eq!(d.len(), 1);
        let menu = d[0].menu.as_ref().unwrap();
        assert_eq!(menu.buttons.len(), 2);
        assert_eq!(menu.buttons[0].name, "btn1");
        assert!(menu.buttons[0].autoaction);
        assert_eq!(menu.buttons[1].name, "btn2");
        assert!(!menu.buttons[1].autoaction);
    }

    /// RLE + control sequence decode back to the source bitmap with the existing VobSub decoder.
    #[test]
    fn spu_decodes_back() {
        let (w, h) = (90u32, 7u32);
        let mut pixels = vec![0u8; (w * h) as usize];
        for y in 1..6 {
            for x in 3..80 {
                pixels[(y * w + x) as usize] = if (x / 7 + y) % 3 == 0 { 2 } else { 1 };
            }
        }
        pixels[0] = 3;
        let bmp = Pic { width: w, height: h, pixels: pixels.clone(), palette: vec![[0, 0, 0, 0], [255, 255, 255, 255], [200, 0, 0, 255], [0, 0, 255, 128]] };
        let (n, nx, ny) = normalize(&bmp, 100, 400).unwrap();
        let mut clut = Vec::new();
        let spu = encode_spu(&n, nx, ny, false, -1, &mut clut).unwrap();
        assert_eq!(u16::from_be_bytes([spu[0], spu[1]]) as usize, spu.len());
        assert_eq!(clut.len(), 3);
        // ... 06 <top=4> <bottom> ff, trailing pad byte at most.
        let at = spu.windows(3).position(|w| w == [0x06, 0, 4]).unwrap();
        let bottom = u16::from_be_bytes([spu[at + 3], spu[at + 4]]) as usize;
        assert_eq!(u16::from_be_bytes([spu[at + 1], spu[at + 2]]), 4);
        // The visible area (columns 0..=79, rows 0..=5) is kept; the corner pixel keeps the origin.
        assert_eq!((n.width, n.height, nx, ny), (80, 6, 100, 400));
        let decoded = crate::subpic::decode_spu_rle(&spu, 4, bottom, n.width, n.height);
        assert_eq!(decoded, n.pixels);
    }
}
