//! Origin: port of dvdauthor (Copyright (C) 2002 Scott Smith, GPL-2.0-or-later); see THIRD_PARTY_NOTICES.md.
//! DVD-Video VOB processing: scanning the program streams of titles and menus, marking cells and
//! programs, writing the output VOB files and filling in their NAV packs.
//!
//! A port of the parts of dvdauthor's `dvdvob.c` (`FindVobus`, `MarkChapters`, `FixVobus`) and
//! the attribute handling of `dvdauthor.c` (`setattr`, timing helpers) that this program uses. The
//! output must be byte-identical to dvdauthor 0.7.2+ (`tests/dvdauthor_native.rs`). The private
//! `dvdauthor-data` sectors written by [`crate::spumux`] (colour table, button colours, button
//! geometry) are consumed and removed here, exactly as dvdauthor does.
//!
//! GPL-2.0 (derived from dvdauthor).

// The index loops mirror dvdauthor's C 1:1 (byte-identical output, easy diffing upstream).
#![allow(clippy::needless_range_loop)]

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::dvdvm::{self, Program};

pub type Pts = i64;

pub const COLOR_UNUSED: i32 = 0x100_0000;
/// A VOB file is closed after this many sectors (just under 1 GiB).
pub const MAX_VOB_SECTORS: i32 = 524_272;

// Attribute codes (`da-internal.h`).
pub const VM_MPEG1: i32 = 1;
pub const VM_MPEG2: i32 = 2;
pub const VS_720H: i32 = 1;
pub const VS_704H: i32 = 2;
pub const VS_352H: i32 = 3;
pub const VS_352L: i32 = 4;
pub const VF_NTSC: i32 = 1;
pub const VF_PAL: i32 = 2;
pub const VA_4X3: i32 = 1;
pub const VA_16X9: i32 = 2;
pub const VW_NOLETTERBOX: i32 = 1;
pub const VW_NOPANSCAN: i32 = 2;
pub const VW_CROP: i32 = 3;
pub const VR_NTSCFILM: i32 = 1;
pub const VR_FILM: i32 = 2;
pub const VR_PAL: i32 = 3;
pub const VR_NTSC: i32 = 4;
pub const VR_30: i32 = 5;
pub const VR_PALFIELD: i32 = 6;
pub const VR_NTSCFIELD: i32 = 7;
pub const VR_60: i32 = 8;
pub const AF_AC3: i32 = 1;
pub const AF_MP2: i32 = 2;
pub const AF_PCM: i32 = 3;
pub const AF_DTS: i32 = 4;
pub const AQ_16: i32 = 1;
pub const AQ_20: i32 = 2;
pub const AQ_24: i32 = 3;
pub const AQ_DRC: i32 = 4;
pub const AD_SURROUND: i32 = 1;
pub const AL_NOLANG: i32 = 1;
pub const AL_LANG: i32 = 2;
pub const AS_48KHZ: i32 = 1;
pub const AS_96KHZ: i32 = 2;

const RATE_DENOM: [i64; 9] = [0, 90090, 90000, 90000, 90090, 90000, 90000, 90090, 90000];
const EVEN_RATE: [i64; 9] = [0, 24, 24, 25, 30, 30, 50, 60, 60];
/// Time steps of the DSI forward/backward VOBU pointers, in half seconds.
const TIMELINE: [Pts; 19] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 20, 60, 120, 240];
const FFREW_HALFSEC: Pts = 45_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxError(pub String);

impl std::fmt::Display for MuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MuxError {}

impl From<io::Error> for MuxError {
    fn from(e: io::Error) -> Self {
        MuxError(e.to_string())
    }
}

impl From<dvdvm::VmError> for MuxError {
    fn from(e: dvdvm::VmError) -> Self {
        MuxError(e.to_string())
    }
}

type Result<T> = std::result::Result<T, MuxError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(MuxError(msg.into()))
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoDesc {
    pub vmpeg: i32,
    pub vres: i32,
    pub vformat: i32,
    pub vaspect: i32,
    pub vwidescreen: i32,
    pub vframerate: i32,
    pub vcaption: i32,
}

impl VideoDesc {
    fn set_unset(field: &mut i32, v: i32) {
        if *field == 0 {
            *field = v;
        }
    }

    pub fn set_framerate(&mut self, mut rate: i32) {
        if self.vframerate == 0 && rate != VR_PAL && rate != VR_NTSC {
            rate = VR_NTSC;
        }
        Self::set_unset(&mut self.vframerate, rate);
    }

    /// `vobgroup_set_video_attr` for the keyword attributes.
    pub fn set_format(&mut self, pal: bool) {
        Self::set_unset(&mut self.vformat, if pal { VF_PAL } else { VF_NTSC });
    }

    pub fn set_aspect(&mut self, wide: bool) {
        Self::set_unset(&mut self.vaspect, if wide { VA_16X9 } else { VA_4X3 });
    }

    pub fn set_mpeg(&mut self, v: i32) {
        Self::set_unset(&mut self.vmpeg, v);
    }

    pub fn set_resolution(&mut self, h: i32, v: i32) {
        let r = if h > 704 {
            VS_720H
        } else if h > 352 {
            VS_704H
        } else if v >= 384 {
            VS_352H
        } else {
            VS_352L
        };
        Self::set_unset(&mut self.vres, r);
        if self.vformat == 0 {
            if v % 5 == 0 {
                self.vformat = VF_NTSC;
            } else if v % 9 == 0 {
                self.vformat = VF_PAL;
            }
        }
    }

    fn ratecode(&self) -> usize {
        if self.vframerate != 0 {
            self.vframerate as usize
        } else if self.vformat == VF_PAL {
            VR_PAL as usize
        } else {
            VR_NTSC as usize
        }
    }

    /// Clock units per second at the actual frame rate (90090 for the NTSC family).
    pub fn ratedenom(&self) -> i64 {
        RATE_DENOM[self.ratecode()]
    }

    /// Exact clock units per frame.
    pub fn framepts(&self) -> Pts {
        let rc = self.ratecode();
        RATE_DENOM[rc] / EVEN_RATE[rc]
    }

    /// 4-bit mask of the display modes a subpicture stream serves.
    pub fn subpmask(&self) -> i32 {
        let mut mask = if self.vaspect == VA_16X9 { 14 } else { 1 };
        match self.vwidescreen {
            VW_NOLETTERBOX => mask &= !4,
            VW_NOPANSCAN => mask &= !8,
            VW_CROP => mask |= 2,
            _ => {}
        }
        mask
    }

    /// BCD `hhmmssff` of `num` clock units plus the frame rate bits (`buildtimeeven`).
    pub fn bcd_time(&self, num: Pts) -> u32 {
        let denom = RATE_DENOM[self.ratecode()];
        let (frate, rc): (i64, u32) = if denom == 90090 { (30, 3) } else { (25, 1) };
        let num = num + denom / (frate * 2) + 1;
        let sec = num / denom;
        let min = sec / 60;
        let tobcd = |v: i64| ((v / 10) * 16 + v % 10) as u32;
        let fr = tobcd((num % denom) * frate / denom);
        tobcd(min / 60) << 24 | tobcd(min % 60) << 16 | tobcd(sec % 60) << 8 | fr | rc << 6
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioDesc {
    pub aformat: i32,
    pub aquant: i32,
    pub adolby: i32,
    pub achannels: i32,
    pub alangpresent: i32,
    pub asample: i32,
    pub aid: i32,
    pub lang: [u8; 2],
    pub acontent: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubpDesc {
    pub slangpresent: i32,
    pub lang: [u8; 2],
    pub scontent: i32,
    /// Stream id per display mode (normal, widescreen, letterbox, panscan): `128 | id`, or 0.
    pub idmap: [u8; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Neither,
    /// Chapter and program start.
    ChapterProgram,
    Program,
}

#[derive(Debug, Clone)]
pub struct Cell {
    pub startpts: Pts,
    pub endpts: Pts,
    pub kind: CellKind,
    pub pauselen: u8,
    pub scellid: i32,
    pub ecellid: i32,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub fname: String,
    pub cells: Vec<Cell>,
    /// Index into [`VobGroup::vobs`].
    pub vob: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ButtonInfo {
    pub substreamid: i32,
    pub autoaction: bool,
    pub grp: i32,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub up: String,
    pub down: String,
    pub left: String,
    pub right: String,
}

#[derive(Debug, Clone)]
pub struct Button {
    pub name: String,
    pub commands: Program,
    pub streams: Vec<ButtonInfo>,
}

#[derive(Debug, Clone)]
pub struct Pgc {
    pub sources: Vec<Source>,
    pub buttons: Vec<Button>,
    pub numchapters: i32,
    pub numprograms: i32,
    pub numcells: i32,
    pub pauselen: u8,
    /// Menu entry types (bit `n` = entry `n`; 1 = "not a title" in a titleset).
    pub entries: u32,
    pub prei: Option<Program>,
    pub posti: Option<Program>,
    pub colors: [i32; 16],
    pub subpmap: [[u8; 4]; 32],
}

impl Pgc {
    pub fn new() -> Self {
        Pgc {
            sources: vec![],
            buttons: vec![],
            numchapters: 0,
            numprograms: 0,
            numcells: 0,
            pauselen: 0,
            entries: 0,
            prei: None,
            posti: None,
            colors: [COLOR_UNUSED; 16],
            subpmap: [[0; 4]; 32],
        }
    }
}

impl Default for Pgc {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Vobu {
    pub sector: i32,
    pub lastsector: i32,
    pub fsect: i32,
    pub fnum: i32,
    pub vobcellid: i32,
    pub firstvobuincell: i32,
    pub lastvobuincell: i32,
    pub hasseqend: bool,
    pub hasvideo: bool,
    pub videopts: [Pts; 2],
    pub sectpts: [Pts; 2],
    pub firstvideopts: Pts,
    pub numref: usize,
    pub firstifield: i32,
    pub numfields: i32,
    pub lastrefsect: [i32; 3],
    pub sectdata: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AudPts {
    pub pts: [Pts; 2],
    pub asect: i32,
}

#[derive(Debug, Clone, Default)]
pub struct AudChannel {
    pub audpts: Vec<AudPts>,
    pub ad: AudioDesc,
}

#[derive(Debug, Clone)]
pub struct Vob {
    pub fname: String,
    pub vobu: Vec<Vobu>,
    pub vobid: i32,
    pub numcells: i32,
    /// Index of the PGC that owns the VOB (colour table, buttons).
    pub pgc: usize,
    /// 0..31 audio (top two bits = format - 1, low three = stream), 32..63 subpicture streams.
    pub audch: Vec<AudChannel>,
    pub buttoncoli: [u8; 24],
}

impl Vob {
    fn new(fname: &str, pgc: usize) -> Self {
        Vob { fname: fname.to_string(), vobu: vec![], vobid: 0, numcells: 0, pgc, audch: vec![AudChannel::default(); 64], buttoncoli: [0; 24] }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VobGroup {
    pub vd: VideoDesc,
    pub ad: [AudioDesc; 8],
    pub numaudiotracks: usize,
    pub sp: [SubpDesc; 32],
    pub numsubpicturetracks: usize,
    pub vobs: Vec<Vob>,
}

/// A set of PGCs with their VOBs: the menus of a domain, or the titles of a titleset.
#[derive(Debug, Clone)]
pub struct PgcSet {
    pub kind: dvdvm::Domain,
    pub pgcs: Vec<Pgc>,
    pub allentries: u32,
    pub numentries: u32,
    pub vg: VobGroup,
}

impl PgcSet {
    pub fn new(kind: dvdvm::Domain) -> Self {
        PgcSet { kind, pgcs: vec![], allentries: 0, numentries: 0, vg: VobGroup::default() }
    }

    /// Creates the VOB entries for all sources (`pgcgroup_createvobs`).
    pub fn create_vobs(&mut self) {
        for p in 0..self.pgcs.len() {
            for s in 0..self.pgcs[p].sources.len() {
                let forcenew = !self.pgcs[p].buttons.is_empty();
                let fname = self.pgcs[p].sources[s].fname.clone();
                let reuse = (!forcenew)
                    .then(|| self.vg.vobs.iter().position(|v| v.fname == fname && self.pgcs[v.pgc].buttons.is_empty()))
                    .flatten();
                let v = reuse.unwrap_or_else(|| {
                    self.vg.vobs.push(Vob::new(&fname, p));
                    self.vg.vobs.len() - 1
                });
                self.pgcs[p].sources[s].vob = v;
            }
        }
    }

    /// `validatesummary`: entries bookkeeping and the chapter flag on the first cell.
    pub fn validate(&mut self) -> Result<()> {
        let allowed: u32 = match self.kind {
            dvdvm::Domain::Vtsm => 0xf8,
            dvdvm::Domain::Vmgm => 4,
            dvdvm::Domain::Vts => 0,
        };
        for (i, p) in self.pgcs.iter_mut().enumerate() {
            if self.allentries & p.entries != 0 {
                return err(format!("ERR:  Multiple definitions for an entry, 2nd occurrence in PGC #{i}"));
            }
            if self.kind != dvdvm::Domain::Vts && p.entries & !allowed != 0 {
                return err(format!("ERR:  Entry not allowed for menu type in PGC #{i}"));
            }
            self.allentries |= p.entries;
            let mut first = true;
            for s in &mut p.sources {
                if let Some(c) = s.cells.first_mut() {
                    if first {
                        c.kind = CellKind::ChapterProgram;
                        first = false;
                    }
                }
            }
        }
        for b in 0..8 {
            if self.allentries & (1 << b) != 0 {
                self.numentries += 1;
            }
        }
        Ok(())
    }

    /// `forceaddentry`: gives the first PGC `entry` unless the set has it already.
    pub fn force_entry(&mut self, entry: u32) {
        if self.pgcs.is_empty() {
            return;
        }
        if self.allentries & entry == 0 {
            self.pgcs[0].entries |= entry;
            self.allentries |= entry;
            self.numentries += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Timing helpers shared with the IFO writer
// ---------------------------------------------------------------------------

impl Vob {
    /// `findcellvobu`: index of the first VOBU of cell `cellid` (or `numvobus` behind the last).
    pub fn find_cell_vobu(&self, cellid: i32) -> i32 {
        let h = self.vobu.len() as i32 - 1;
        if h < 0 {
            return 0;
        }
        let cellid = (cellid & 255) | (self.vobid * 256);
        if cellid < self.vobu[0].vobcellid {
            return 0;
        }
        if cellid > self.vobu[h as usize].vobcellid {
            return h + 1;
        }
        let (mut l, mut h) = (0, h);
        while l < h {
            let m = (l + h) / 2;
            if cellid <= self.vobu[m as usize].vobcellid {
                h = m;
            } else {
                l = m + 1;
            }
        }
        l
    }

    pub fn cell_pts(&self, cellid: i32) -> Pts {
        let s = self.find_cell_vobu(cellid);
        let e = self.find_cell_vobu(cellid + 1);
        if s == e {
            return 0;
        }
        self.vobu[(e - 1) as usize].sectpts[1] - self.vobu[s as usize].sectpts[0]
    }

    /// `findvobu`: the VOBU within `l..=h` that spans `pts`.
    pub fn find_vobu(&self, pts: Pts, l: i32, h: i32) -> i32 {
        if h < l || pts < self.vobu[l as usize].sectpts[0] {
            return l - 1;
        }
        if pts >= self.vobu[h as usize].sectpts[1] {
            return h + 1;
        }
        let (mut l, mut h) = (l, h);
        while l < h {
            let m = (l + h + 1) / 2;
            if pts < self.vobu[m as usize].sectpts[0] {
                h = m - 1;
            } else {
                l = m;
            }
        }
        l
    }
}

impl PgcSet {
    /// `getptsspan`: playback duration of PGC `p`.
    pub fn pgc_pts_span(&self, p: usize) -> Pts {
        let mut span = 0;
        for s in &self.pgcs[p].sources {
            let vob = &self.vg.vobs[s.vob];
            for c in &s.cells {
                for ci in c.scellid..c.ecellid {
                    span += vob.cell_pts(ci);
                }
            }
        }
        span
    }
}

impl VobGroup {
    /// `getaudch`: index into `Vob::audch` for audio track `a`, or -1 when the track is unused.
    pub fn audch_of(&self, a: usize) -> i32 {
        if self.ad[a].aid == 0 {
            return -1;
        }
        self.ad[a].aid - 1 + (self.ad[a].aformat - 1) * 8
    }

    /// `calcaudiogap`: is there an audio discontinuity between cells `vcid0` and `vcid1`.
    pub fn audio_gap(&self, vcid0: i32, vcid1: i32, ach: usize) -> bool {
        if vcid0 == -1 || vcid1 == -1 || vcid1 == vcid0 + 1 {
            return false;
        }
        let v0 = &self.vobs[((vcid0 >> 8) - 1) as usize];
        let v1 = &self.vobs[((vcid1 >> 8) - 1) as usize];
        if (vcid1 & 255) == 1 && v0.numcells == (vcid0 & 255) {
            let has0 = |v: &Vob| !v.audch[ach].audpts.is_empty();
            if !has0(v1) {
                return false;
            }
            if !has0(v0) {
                return true;
            }
            let vid = |v: &Vob, w: usize| {
                let vi = if w == 1 { v.vobu.len() - 1 } else { 0 };
                v.vobu[vi].sectpts[w]
            };
            let aud = |v: &Vob, w: usize| {
                let ai = if w == 1 { v.audch[ach].audpts.len() - 1 } else { 0 };
                v.audch[ach].audpts[ai].pts[w]
            };
            let g1 = (vid(v0, 1) - aud(v0, 1)) as i32;
            let g2 = (vid(v1, 0) - aud(v1, 0)) as i32;
            return g1 != g2;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Low-level stream helpers
// ---------------------------------------------------------------------------

fn r2(b: &[u8]) -> i32 {
    (b[0] as i32) << 8 | b[1] as i32
}

fn r4(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn w2(b: &mut [u8], v: u32) {
    b[0] = (v >> 8) as u8;
    b[1] = v as u8;
}

fn w4(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_be_bytes());
}

/// Pack header SCR in 90 kHz units.
fn readscr(b: &[u8]) -> Pts {
    ((b[0] & 0x38) as Pts) << 27
        | ((b[0] & 3) as Pts) << 28
        | (b[1] as Pts) << 20
        | ((b[2] & 0xf8) as Pts) << 12
        | ((b[2] & 3) as Pts) << 13
        | (b[3] as Pts) << 5
        | ((b[4] & 0xf8) as Pts) >> 3
}

fn writescr(b: &mut [u8], scr: Pts) {
    b[0] = (((scr >> 27) & 0x38) | ((scr >> 28) & 3) | 68) as u8;
    b[1] = (scr >> 20) as u8;
    b[2] = (((scr >> 12) & 0xf8) | ((scr >> 13) & 3) | 4) as u8;
    b[3] = (scr >> 5) as u8;
    b[4] = (((scr << 3) & 0xf8) as u8) | (b[4] & 7);
}

fn readpts(b: &[u8]) -> Pts {
    let a1 = ((b[0] & 0xe) >> 1) as Pts;
    let a2 = (((b[1] as Pts) << 8) | b[2] as Pts) >> 1;
    let a3 = (((b[3] as Pts) << 8) | b[4] as Pts) >> 1;
    a1 << 30 | a2 << 15 | a3
}

fn writepts(b: &mut [u8], pts: Pts) {
    b[0] = (((pts >> 29) & 0xe) as u8) | (b[0] & 0xf1);
    w2(&mut b[1..], ((pts >> 14) | 1) as u32);
    w2(&mut b[3..], ((pts << 1) | 1) as u32);
}

const MPID_PACK: u8 = 0xba;
const MPID_SYSTEM: u8 = 0xbb;
const MPID_PRIVATE1: u8 = 0xbd;
const MPID_PAD: u8 = 0xbe;
const MPID_PRIVATE2: u8 = 0xbf;
const MPID_VIDEO_FIRST: u8 = 0xe0;

/// Adjusts the timestamps of a pack and its first packet by `tsoffs` (`transpose_ts`).
fn transpose_ts(buf: &mut [u8], tsoffs: Pts) {
    if buf[..4] != [0, 0, 1, MPID_PACK] {
        return;
    }
    let sysoffs = if buf[14..18] == [0, 0, 1, MPID_SYSTEM] { (r2(&buf[18..]) + 6) as usize } else { 0 };
    let scr = readscr(&buf[4..]) + tsoffs;
    writescr(&mut buf[4..], scr);
    let p = &buf[14 + sysoffs..];
    if p[..3] == [0, 0, 1] && (p[3] == MPID_PRIVATE1 || (0xc0..=0xef).contains(&p[3])) && (p[7] & 128) != 0 {
        let pts = readpts(&buf[23 + sysoffs..]) + tsoffs;
        writepts(&mut buf[23 + sysoffs..], pts);
        if buf[21 + sysoffs] & 64 != 0 {
            let dts = readpts(&buf[28 + sysoffs..]) + tsoffs;
            writepts(&mut buf[28 + sysoffs..], dts);
        }
    }
}

fn has_gop(buf: &[u8]) -> bool {
    if buf[14..18] == [0, 0, 1, MPID_SYSTEM] && buf[38..42] == [0, 0, 1, MPID_VIDEO_FIRST] {
        let mut i = 42;
        while i < 1024 {
            if buf[i..i + 4] == [0, 0, 1, 0xb8] {
                return true;
            }
            i += 4;
        }
    }
    false
}

fn mpa_valid(b: &[u8]) -> bool {
    let v = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    if v & 0xFFFE_0C00 != 0xFFFC_0400 {
        return false;
    }
    let t = (v >> 12) & 15;
    if t == 0 || t == 15 {
        return false;
    }
    (v & 3) != 2
}

fn mpa_len(b: &[u8]) -> i32 {
    const BITRATE: [i32; 16] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0];
    let padding = ((b[2] >> 1) & 1) as i32;
    3 * BITRATE[((b[2] >> 4) & 15) as usize] + padding
}

// ---------------------------------------------------------------------------
// Video stream scan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Vscani {
    lastrefsect: bool,
    firstgop: i32,
    firsttemporal: i32,
    lastadjust: bool,
}

fn closelastref(vi: &mut Vobu, vsi: &mut Vscani, cursect: i32) {
    if vsi.lastrefsect && vi.numref < 3 {
        vi.lastrefsect[vi.numref] = cursect;
        vi.numref += 1;
        vsi.lastrefsect = false;
    }
}

/// Examines one potential start code. May rewrite `buf[7]` (aspect ratio code).
fn scanvideoptr(vd: &mut VideoDesc, buf: &mut [u8], vi: &mut Vobu, prevbytesect: i32, vsi: &mut Vscani) {
    if buf[..3] != [0, 0, 1] {
        return;
    }
    match buf[3] {
        0x00 => {
            let ptype = (buf[5] >> 3) & 7;
            let temporal = ((buf[4] as i32) << 2) | (buf[5] >> 6) as i32;
            closelastref(vi, vsi, prevbytesect);
            if vsi.firsttemporal == -1 {
                vsi.firsttemporal = temporal;
            }
            vsi.lastadjust = temporal < vsi.firsttemporal;
            if ptype == 1 || ptype == 2 {
                vsi.lastrefsect = true;
            }
            if vd.vmpeg == VM_MPEG1 {
                vi.numfields += 2;
                if vsi.lastadjust && vsi.firstgop == 2 {
                    vi.firstifield += 2;
                }
            }
        }
        0xb3 => {
            closelastref(vi, vsi, prevbytesect);
            let hsize = ((buf[4] as i32) << 4) | (buf[5] >> 4) as i32;
            let vsize = (((buf[5] as i32) << 8) & 0xf00) | buf[6] as i32;
            let aspect = buf[7] >> 4;
            let framerate = (buf[7] & 0xf) as i32;
            vd.set_framerate(framerate);
            match framerate {
                VR_NTSCFILM | VR_NTSC | VR_NTSCFIELD => vd.set_format(false),
                VR_PAL | VR_PALFIELD => vd.set_format(true),
                _ => {}
            }
            vd.set_resolution(hsize, vsize);
            if vd.vmpeg == VM_MPEG1 {
                match aspect {
                    3 => {
                        vd.set_aspect(true);
                        vd.set_format(true);
                    }
                    6 => {
                        vd.set_aspect(true);
                        vd.set_format(false);
                    }
                    8 => {
                        vd.set_aspect(false);
                        vd.set_format(true);
                    }
                    12 => {
                        vd.set_aspect(false);
                        vd.set_format(false);
                    }
                    _ => {}
                }
                let mut newaspect = 3 + (vd.vaspect == VA_4X3) as u8 * 5 + (vd.vformat == VF_NTSC) as u8 * 3;
                if newaspect == 11 {
                    newaspect += 1;
                }
                buf[7] = (buf[7] & 0xf) | (newaspect << 4);
            } else if vd.vmpeg == VM_MPEG2 {
                if aspect == 2 {
                    vd.set_aspect(false);
                } else if aspect == 3 {
                    vd.set_aspect(true);
                }
                buf[7] = (buf[7] & 0xf) | (if vd.vaspect == VA_4X3 { 2 } else { 3 }) << 4;
            }
        }
        0xb5 => {
            vd.set_mpeg(VM_MPEG2);
            match buf[4] & 0xF0 {
                0x10 => closelastref(vi, vsi, prevbytesect),
                0x20 => {
                    closelastref(vi, vsi, prevbytesect);
                    match buf[4] & 0xE {
                        2 => vd.set_format(true),
                        4 => vd.set_format(false),
                        _ => {}
                    }
                }
                0x80 => {
                    let mut padj = 1;
                    if buf[6] & 3 == 3 {
                        padj += 1;
                    }
                    if buf[7] & 2 != 0 {
                        padj += 1;
                    }
                    vi.numfields += padj;
                    if vsi.lastadjust && vsi.firstgop == 2 {
                        vi.firstifield += padj;
                    }
                }
                _ => {}
            }
        }
        0xb7 => vi.hasseqend = true,
        0xb8 => {
            closelastref(vi, vsi, prevbytesect);
            if vsi.firstgop == 1 {
                vsi.firstgop = 2;
                vsi.firsttemporal = -1;
                vsi.lastadjust = false;
            } else if vsi.firstgop == 2 {
                vsi.firstgop = 0;
            }
        }
        _ => {}
    }
}

/// Scans the video packet of the sector in `buf` (which starts at the pack header).
fn scanvideoframe(vd: &mut VideoDesc, slide: &mut [u8; 15], buf: &mut [u8], vi: &mut Vobu, cursect: i32, prevsect: i32, vsi: &mut Vscani) {
    let f = 0x17 + buf[0x16] as usize;
    let l = 0x14 + buf[0x12] as usize * 256 + buf[0x13] as usize;
    if l < f + 8 {
        let n = l.saturating_sub(f);
        slide[7..7 + n].copy_from_slice(&buf[f..f + n]);
        for i in 0..n {
            scanvideoptr(vd, &mut slide[i..], vi, prevsect, vsi);
        }
        buf[f..f + n].copy_from_slice(&slide[7..7 + n]);
        slide[..7].fill(255);
        return;
    }
    loop {
        let mpf = vd.vmpeg;
        let old_vi = vi.clone();
        let old_vsi = *vsi;
        slide[7..15].copy_from_slice(&buf[f..f + 8]);
        for i in 0..=7 {
            scanvideoptr(vd, &mut slide[i..], vi, prevsect, vsi);
        }
        buf[f..f + 8].copy_from_slice(&slide[7..15]);
        let mut i = f + 1;
        while i + 7 < l {
            if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
                scanvideoptr(vd, &mut buf[i..], vi, cursect, vsi);
            }
            i += 1;
        }
        if vd.vmpeg == 0 {
            vd.set_mpeg(VM_MPEG1);
        }
        if mpf != vd.vmpeg {
            *vi = old_vi;
            *vsi = old_vsi;
            continue;
        }
        break;
    }
    slide[..7].copy_from_slice(&buf[l - 7..l]);
}

fn finishvideoscan(vd: &mut VideoDesc, slide: &mut [u8; 15], lastvi: &mut Vobu, prevsect: i32, vsi: &mut Vscani) {
    slide[7..14].fill(0);
    for i in 0..7 {
        scanvideoptr(vd, &mut slide[i..], lastvi, prevsect, vsi);
    }
    slide[..7].fill(255);
    closelastref(lastvi, vsi, prevsect);
}

// ---------------------------------------------------------------------------
// Subpicture colour remapping
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cr {
    Begin0,
    Begin1,
    Begin2,
    Begin3,
    Skip0,
    Skip1,
    NextOffs0,
    NextOffs1,
    Wait,
    Cmd,
    SkipWait,
    Col0,
    Col1,
    ChgArg,
    ChgLn0,
    ChgLn1,
    ChgLn2,
    ChgLn3,
    ChgPx0,
    ChgPx1,
    ChgPx2,
}

#[derive(Clone)]
struct ColorRemap {
    /// Bottom 24 bits YCbCr; bit 24 marks a colour that still has to be mapped to a palette index.
    newcolors: [i32; 16],
    state: Cr,
    curoffs: i32,
    maxlen: i32,
    nextoffs: i32,
    skip: i32,
    ln_ctli: i32,
    /// PGC whose colour table receives the colours.
    origmap: Option<usize>,
}

impl ColorRemap {
    fn new() -> Self {
        ColorRemap { newcolors: std::array::from_fn(|i| i as i32), state: Cr::Begin0, curoffs: 0, maxlen: 0, nextoffs: 0, skip: 0, ln_ctli: 0, origmap: None }
    }

    fn remapcolor(&mut self, colors: &mut [i32; 16], idx: usize) -> Result<u8> {
        if self.newcolors[idx] < 16 {
            return Ok(self.newcolors[idx] as u8);
        }
        let nc = self.newcolors[idx] & 0xff_ffff;
        if let Some(i) = colors.iter().position(|&c| c == nc) {
            self.newcolors[idx] = i as i32;
            return Ok(i as u8);
        }
        if let Some(i) = colors.iter().position(|&c| c == COLOR_UNUSED) {
            colors[i] = nc;
            self.newcolors[idx] = i as i32;
            return Ok(i as u8);
        }
        // When all 16 palette entries of the PGC are used, map to the closest
        // existing entry in YCbCr color space instead of failing the build.
        let (y0, c1_0, c2_0) = ((nc >> 16) & 0xff, (nc >> 8) & 0xff, nc & 0xff);
        let best = (0..16)
            .filter(|&i| colors[i] != COLOR_UNUSED)
            .min_by_key(|&i| {
                let c = colors[i] & 0xff_ffff;
                let (y, c1, c2) = ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff);
                let dy = y0 - y;
                let dc1 = c1_0 - c1;
                let dc2 = c2_0 - c2;
                dy * dy + dc1 * dc1 + dc2 * dc2
            })
            .unwrap_or(0);
        self.newcolors[idx] = best as i32;
        Ok(best as u8)
    }

    fn remapbyte(&mut self, colors: &mut [i32; 16], b: &mut u8) -> Result<()> {
        let lo = self.remapcolor(colors, (*b & 15) as usize)?;
        let hi = self.remapcolor(colors, (*b >> 4) as usize)?;
        *b = lo | (hi << 4);
        Ok(())
    }

    /// Interprets (part of) a subpicture unit to remap its colours; adds the display duration to
    /// `timespan`.
    fn process(&mut self, colors: &mut [i32; 16], data: &mut [u8], timespan: &mut Pts) -> Result<()> {
        let mut k = 0;
        while k < data.len() {
            let b0 = data[k];
            match self.state {
                Cr::Begin0 => {
                    self.curoffs = 0;
                    self.skip = 0;
                    self.maxlen = b0 as i32 * 256;
                    self.state = Cr::Begin1;
                }
                Cr::Begin1 => {
                    self.maxlen += b0 as i32;
                    self.state = Cr::Begin2;
                }
                Cr::Begin2 => {
                    self.nextoffs = b0 as i32 * 256;
                    self.state = Cr::Begin3;
                }
                Cr::Begin3 => {
                    self.nextoffs += b0 as i32;
                    self.state = Cr::Wait;
                }
                Cr::Wait | Cr::Skip0 => {
                    if self.state == Cr::Wait {
                        if self.curoffs == self.maxlen {
                            self.state = Cr::Begin0;
                            continue;
                        }
                        if self.curoffs != self.nextoffs {
                            self.curoffs += 1;
                            k += 1;
                            continue;
                        }
                        self.state = Cr::Skip0;
                    }
                    *timespan += 1024 * b0 as Pts * 256;
                    self.state = Cr::Skip1;
                }
                Cr::Skip1 => {
                    *timespan += 1024 * b0 as Pts;
                    self.state = Cr::NextOffs0;
                }
                Cr::NextOffs0 => {
                    self.nextoffs = b0 as i32 * 256;
                    self.state = Cr::NextOffs1;
                }
                Cr::NextOffs1 => {
                    self.nextoffs += b0 as i32;
                    self.state = Cr::Cmd;
                }
                Cr::SkipWait | Cr::Cmd => {
                    if self.state == Cr::SkipWait {
                        if self.skip != 0 {
                            self.skip -= 1;
                            self.curoffs += 1;
                            k += 1;
                            continue;
                        }
                        self.state = Cr::Cmd;
                    }
                    match b0 {
                        0..=2 => {}
                        3 => self.state = Cr::Col0,
                        4 => {
                            self.skip = 2;
                            self.state = Cr::SkipWait;
                        }
                        5 => {
                            self.skip = 6;
                            self.state = Cr::SkipWait;
                        }
                        6 => {
                            self.skip = 4;
                            self.state = Cr::SkipWait;
                        }
                        7 => {
                            self.skip = 2;
                            self.state = Cr::ChgArg;
                        }
                        255 => self.state = Cr::Wait,
                        x => return err(format!("ERR:  procremap encountered unknown subtitle command: {x}")),
                    }
                }
                Cr::Col0 => {
                    self.remapbyte(colors, &mut data[k])?;
                    self.state = Cr::Col1;
                }
                Cr::Col1 => {
                    self.remapbyte(colors, &mut data[k])?;
                    self.state = Cr::Cmd;
                }
                Cr::ChgArg => {
                    self.skip -= 1;
                    if self.skip == 0 {
                        self.state = Cr::ChgLn0;
                    }
                }
                Cr::ChgLn0 => {
                    self.ln_ctli = (b0 as i32) << 24;
                    self.state = Cr::ChgLn1;
                }
                Cr::ChgLn1 => {
                    self.ln_ctli |= (b0 as i32) << 16;
                    self.state = Cr::ChgLn2;
                }
                Cr::ChgLn2 => {
                    self.ln_ctli |= (b0 as i32) << 8;
                    self.state = Cr::ChgLn3;
                }
                Cr::ChgLn3 => {
                    self.ln_ctli |= b0 as i32;
                    if self.ln_ctli == 0x0fff_ffff {
                        self.state = Cr::Cmd;
                    } else {
                        self.ln_ctli >>= 12;
                        self.ln_ctli &= 0xf;
                        self.skip = 2;
                        self.state = Cr::ChgPx0;
                    }
                }
                Cr::ChgPx0 => {
                    self.skip -= 1;
                    if self.skip == 0 {
                        self.skip = 2;
                        self.state = Cr::ChgPx1;
                    }
                }
                Cr::ChgPx1 => {
                    self.remapbyte(colors, &mut data[k])?;
                    self.skip -= 1;
                    if self.skip == 0 {
                        self.skip = 2;
                        self.state = Cr::ChgPx2;
                    }
                }
                Cr::ChgPx2 => {
                    self.skip -= 1;
                    if self.skip == 0 {
                        self.ln_ctli -= 1;
                        if self.ln_ctli == 0 {
                            self.state = Cr::ChgLn0;
                        } else {
                            self.skip = 2;
                            self.state = Cr::ChgPx0;
                        }
                    }
                }
            }
            self.curoffs += 1;
            k += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Audio stream attributes
// ---------------------------------------------------------------------------

fn set_unset(field: &mut i32, v: i32) {
    if *field == 0 {
        *field = v;
    }
}

fn audio_scan_ac3(ad: &mut AudioDesc, buf: &[u8], sof: i32, len: i32) {
    if sof < 0 || sof + 8 > len || (sof as usize) + 8 > buf.len() {
        return;
    }
    let sof = sof as usize;
    if buf[sof] != 0x0b || buf[sof + 1] != 0x77 {
        return;
    }
    let mut parse = r4(&buf[sof + 4..]);
    set_unset(&mut ad.asample, AS_48KHZ);
    parse <<= 8;
    if parse >> 27 != 8 && parse >> 27 != 6 {
        return;
    }
    parse <<= 5;
    parse <<= 3;
    let acmod = parse >> 29;
    parse <<= 3;
    if (acmod & 1) != 0 && acmod != 1 {
        parse <<= 2;
    }
    if acmod & 4 != 0 {
        parse <<= 2;
    }
    if acmod == 2 {
        if parse >> 30 == 2 {
            set_unset(&mut ad.adolby, AD_SURROUND);
        }
        parse <<= 2;
    }
    let lfeon = parse >> 31;
    let mut nch = match acmod {
        0 | 2 => 2,
        1 => 1,
        3 | 4 => 3,
        5 | 6 => 4,
        _ => 5,
    };
    if lfeon != 0 {
        nch += 1;
    }
    set_unset(&mut ad.achannels, nch);
}

fn audio_scan_pcm(ad: &mut AudioDesc, buf: &[u8]) {
    match buf[1] >> 6 {
        0 => set_unset(&mut ad.aquant, AQ_16),
        1 => set_unset(&mut ad.aquant, AQ_20),
        2 => set_unset(&mut ad.aquant, AQ_24),
        _ => {}
    }
    set_unset(&mut ad.asample, if (buf[1] >> 4) & 1 == 1 { AS_96KHZ } else { AS_48KHZ });
    set_unset(&mut ad.achannels, (buf[1] & 7) as i32 + 1);
}

// ---------------------------------------------------------------------------
// Output files
// ---------------------------------------------------------------------------

/// Where the VOB files of one domain go: `<dir>/<base>_<n>.VOB`, or `<dir>/<base>` for the VMGM menu.
#[derive(Debug, Clone)]
pub struct VobFiles {
    pub dir: PathBuf,
    pub base: String,
    /// `true` for `VIDEO_TS.VOB` (no number in the name).
    pub single: bool,
}

impl VobFiles {
    pub fn path(&self, num: i32) -> PathBuf {
        if self.single {
            self.dir.join(&self.base)
        } else {
            self.dir.join(format!("{}_{}.VOB", self.base, num))
        }
    }
}

// ---------------------------------------------------------------------------
// FindVobus
// ---------------------------------------------------------------------------

/// Collects audio/video/subpicture information, remaps subpicture colours and writes the output
/// VOB files of a domain, complete except for the NAV packs (`FindVobus`). `files` is `None` for a
/// dry run. `kind` is the domain (menus count their VOB files from 0, titles from 1).
pub fn find_vobus(input_dir: &Path, files: Option<&VobFiles>, set: &mut PgcSet, log: &mut Vec<String>) -> Result<()> {
    let kind = set.kind;
    let mut cursect: i32 = 0;
    let mut fsect: i32 = -1;
    let mut outnum: i32 = match kind {
        dvdvm::Domain::Vts => 1,
        dvdvm::Domain::Vtsm => 0,
        dvdvm::Domain::Vmgm => -1,
    };
    let mut vobid = 0;
    let mut writer: Option<BufWriter<File>> = None;
    let mut slide = [255u8; 15];
    slide[7..].fill(0);
    let mut crs: Vec<ColorRemap> = vec![ColorRemap::new(); 32];

    for vnum in 0..set.vg.vobs.len() {
        let mut hadfirstvobu = false;
        let (mut backoffs, mut lastscr): (Pts, Pts) = (0, 0);
        let mut fill_in_vobus = false;
        let mut got_deferred = false;
        let mut deferred = [0u8; 2048];
        let mut prevvidsect: i32 = -1;
        let mut vsi = Vscani { lastrefsect: false, firstgop: 1, firsttemporal: 0, lastadjust: false };
        let mut mp2hdr: [(i32, [u8; 6]); 8] = [(0, [0; 6]); 8];
        for c in crs.iter_mut() {
            *c = ColorRemap::new();
        }
        vobid += 1;
        set.vg.vobs[vnum].vobid = vobid;
        let fname = set.vg.vobs[vnum].fname.clone();
        log.push(format!("STAT: Processing {fname}..."));
        let mut input = io::BufReader::new(File::open(input_dir.join(&fname)).map_err(|e| MuxError(format!("ERR:  cannot open {fname}: {e}")))?);
        let mut inoffset: u64 = 0;
        let mut buf = [0u8; 2048];

        loop {
            if fsect == MAX_VOB_SECTORS {
                if let Some(mut w) = writer.take() {
                    w.flush()?;
                }
                if outnum <= 0 {
                    return err(format!("ERR:  Menu VOB reached 1gb at inoffset {inoffset:#x}"));
                }
                outnum += 1;
                fsect = -1;
            }
            if got_deferred {
                buf = deferred;
            } else if let Err(e) = input.read_exact(&mut buf) {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    break;
                }
                return Err(e.into());
            }
            if buf[14..18] == [0, 0, 1, MPID_PAD] && buf[20..35] == *b"dvdauthor-data\0" {
                private_data(&buf, inoffset, vnum, set, &mut crs)?;
                inoffset += 2048;
                continue;
            }
            if (fill_in_vobus || !hadfirstvobu) && !got_deferred && has_gop(&buf) {
                if !hadfirstvobu {
                    log.push(format!("INFO: found video GOP at inoffset {inoffset:#x} without a preceding VOBU - creating VOBU"));
                }
                fill_in_vobus = true;
                deferred = buf;
                got_deferred = true;
                buf[41] = MPID_PRIVATE2;
                buf[42] = 0x03;
                buf[43] = 0xd4;
                buf[44] = 0x81;
                buf[45..].fill(0);
                buf[1026] = 1;
                buf[1027] = MPID_PRIVATE2;
                buf[1028] = 0x03;
                buf[1029] = 0xfa;
                buf[1030] = 0x81;
            } else if got_deferred {
                got_deferred = false;
            }
            if buf[..4] == [0, 0, 1, MPID_PACK] {
                let newscr = readscr(&buf[4..]);
                if hadfirstvobu && newscr == 0 && lastscr > 0 {
                    backoffs -= lastscr;
                    log.push(format!("WARN: SCR reset at inoffset {inoffset:#x}. New back offset = {backoffs}"));
                } else if newscr < lastscr {
                    return err(format!("ERR:  SCR moves backwards at inoffset {inoffset:#x}, remultiplex input: {newscr} < {lastscr}"));
                }
                lastscr = newscr;
                if !hadfirstvobu {
                    backoffs = newscr;
                }
            }
            transpose_ts(&mut buf, -backoffs);
            if fsect == -1 {
                fsect = 0;
                if let Some(f) = files {
                    writer = Some(BufWriter::with_capacity(16 * 2048, File::create(f.path(outnum))?));
                }
            }
            let thisvob = &mut set.vg.vobs[vnum];
            if buf[14..18] == [0, 0, 1, MPID_SYSTEM]
                && buf[38..42] == [0, 0, 1, MPID_PRIVATE2]
                && buf[1024..1028] == [0, 0, 1, MPID_PRIVATE2]
            {
                if !thisvob.vobu.is_empty() {
                    let last = thisvob.vobu.len() - 1;
                    finishvideoscan(&mut set.vg.vd, &mut slide, &mut thisvob.vobu[last], prevvidsect, &mut vsi);
                }
                hadfirstvobu = true;
                thisvob.vobu.push(Vobu {
                    sector: cursect,
                    fsect,
                    fnum: outnum,
                    firstvideopts: -1,
                    sectdata: buf[..0x26].to_vec(),
                    ..Vobu::default()
                });
                vsi.lastrefsect = false;
                vsi.firstgop = 1;
            }
            if !hadfirstvobu {
                log.push(format!("WARN: Skipping sector at inoffset {inoffset:#x}, waiting for first VOBU..."));
                inoffset += 2048;
                continue;
            }
            let vi_idx = thisvob.vobu.len() - 1;
            thisvob.vobu[vi_idx].lastsector = cursect;

            // Merge a trailing program-end code into a preceding padding packet.
            let (mut i, mut j) = (14usize, -1i32);
            while i <= 2044 {
                if buf[i..i + 3] == [0, 0, 1] {
                    if (MPID_PRIVATE1..=0xef).contains(&buf[i + 3]) {
                        j = i as i32;
                        i += 6 + r2(&buf[i + 4..]) as usize;
                        continue;
                    } else if buf[i + 3] == 0xb9 && j >= 14 && buf[j as usize + 3] == MPID_PAD {
                        let ju = j as usize;
                        let l = r2(&buf[ju + 4..]) + 4;
                        w2(&mut buf[ju + 4..], l as u32);
                        buf[i..i + 4].fill(0);
                    }
                }
                break;
            }

            let sysoffs = if buf[14..18] == [0, 0, 1, MPID_SYSTEM] { (r2(&buf[18..]) + 6) as usize } else { 0 };
            let pack = buf[..4] == [0, 0, 1, MPID_PACK];
            let stream = if pack { buf[17 + sysoffs] } else { 0 };
            if pack && buf[14 + sysoffs..17 + sysoffs] == [0, 0, 1] && stream == MPID_VIDEO_FIRST {
                let vi = &mut thisvob.vobu[vi_idx];
                vi.hasvideo = true;
                scanvideoframe(&mut set.vg.vd, &mut slide, &mut buf[sysoffs..], vi, cursect, prevvidsect, &mut vsi);
                if buf[21 + sysoffs] & 128 != 0 && vi.firstvideopts == -1 {
                    vi.firstvideopts = readpts(&buf[23 + sysoffs..]);
                }
                prevvidsect = cursect;
            }
            if pack && buf[14 + sysoffs..17 + sysoffs] == [0, 0, 1] && ((stream & 0xf8) == 0xc0 || stream == MPID_PRIVATE1) {
                let (mut pts0, mut pts1, mut backpts1): (Pts, Pts, Pts) = (0, 0, 0);
                let dptr = buf[22 + sysoffs] as usize + 23 + sysoffs;
                let endop = r2(&buf[18 + sysoffs..]) as usize + 20 + sysoffs;
                let haspts = buf[21 + sysoffs] & 128 != 0;
                let audch: i32;
                if stream == MPID_PRIVATE1 {
                    let sid = buf[dptr];
                    let offs = r2(&buf[dptr + 2..]);
                    let nrframes = buf[dptr + 1] as Pts;
                    match sid & 0xf8 {
                        0x20 | 0x28 | 0x30 | 0x38 => audch = sid as i32,
                        0x80 => {
                            pts1 += 2880 * nrframes;
                            audch = (sid & 7) as i32;
                            audio_scan_ac3(&mut thisvob.audch[audch as usize].ad, &buf[dptr + 4..], offs - 1, endop as i32 - (dptr as i32 + 4));
                        }
                        0x88 => audch = 24 | (sid & 7) as i32,
                        0xa0 => {
                            pts1 += 150 * nrframes;
                            audch = 16 | (sid & 7) as i32;
                            audio_scan_pcm(&mut thisvob.audch[audch as usize].ad, &buf[dptr + 4..]);
                        }
                        _ => audch = -1,
                    }
                } else {
                    let len = (endop - dptr) as i32;
                    let index = (stream & 7) as usize;
                    audch = 8 | index as i32;
                    let h = &mut mp2hdr[index];
                    h.1[3..6].copy_from_slice(&buf[dptr..dptr + 3]);
                    while h.0 + 4 <= len {
                        let hdr: [u8; 4] = if h.0 < 0 {
                            let o = (3 + h.0) as usize;
                            let mut a = [0u8; 4];
                            for (k, x) in a.iter_mut().enumerate() {
                                *x = if o + k < 6 { h.1[o + k] } else { buf[dptr + o + k - 6] };
                            }
                            a
                        } else {
                            let o = dptr + h.0 as usize;
                            [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
                        };
                        if !mpa_valid(&hdr) {
                            h.0 += 1;
                            continue;
                        }
                        if h.0 < 0 {
                            backpts1 += 2160;
                        } else {
                            pts1 += 2160;
                        }
                        h.0 += mpa_len(&hdr);
                    }
                    h.0 -= len;
                    let tail = dptr + len as usize - 3;
                    h.1[0..3].copy_from_slice(&buf[tail..tail + 3]);
                    set_unset(&mut thisvob.audch[audch as usize].ad.asample, AS_48KHZ);
                }
                if haspts {
                    pts0 = readpts(&buf[23 + sysoffs..]);
                    pts1 += pts0;
                } else if pts1 > 0 {
                    log.push(format!("WARN: Audio channel {audch} contains sync headers at inoffset {inoffset:#x} but has no PTS."));
                }
                if !(0..64).contains(&audch) {
                    log.push(format!("WARN: Invalid audio channel {audch} at inoffset {inoffset:#x}"));
                } else if haspts {
                    let ach = &mut thisvob.audch[audch as usize];
                    if let Some(prev) = ach.audpts.last_mut() {
                        if (audch & 0x38) == 0x18 {
                            prev.pts[1] = pts0;
                        } else {
                            prev.pts[1] += backpts1;
                        }
                        if prev.pts[1] < pts0 {
                            if audch < 32 {
                                log.push(format!("WARN: Discontinuity of {} in audio channel {audch} at inoffset {inoffset:#x}; please remultiplex input.", pts0 - prev.pts[1]));
                                prev.pts[1] = pts0;
                            }
                        } else if prev.pts[1] > pts0 {
                            // spumux rounds SP_DCSQ display delays to 1024-tick units, so a picture's
                            // end can overshoot the next one's start on the same stream, which
                            // replaces it anyway; only larger jumps are worth a warning.
                            if audch < 32 || prev.pts[1] - pts0 >= 1024 {
                                log.push(format!("WARN: pts for channel {audch} moves backwards by {} at inoffset {inoffset:#x}; please remultiplex input.", prev.pts[1] - pts0));
                            }
                            prev.pts[1] = pts0;
                        }
                    }
                    ach.audpts.push(AudPts { pts: [pts0, pts1], asect: cursect });
                }
            }
            if pack && buf[14 + sysoffs..17 + sysoffs] == [0, 0, 1] && stream == MPID_PRIVATE1 {
                let mut dptr = buf[22 + sysoffs] as usize + 23 + sysoffs;
                let ml = r2(&buf[18..]) as usize + 20 + sysoffs;
                let st = buf[dptr] as usize;
                dptr += 1;
                if st & 0xe0 == 0x20 {
                    let pgc = thisvob.pgc;
                    let colors = &mut set.pgcs[pgc].colors;
                    if let Some(last) = thisvob.audch[st].audpts.last_mut() {
                        let end = ml.min(2048);
                        crs[st & 31].process(colors, &mut buf[dptr..end], &mut last.pts[1])?;
                    }
                }
            }
            if let Some(w) = writer.as_mut() {
                w.write_all(&buf)?;
            }
            cursect += 1;
            fsect += 1;
            inoffset += 2048;
        }

        let thisvob = &mut set.vg.vobs[vnum];
        if !thisvob.vobu.is_empty() {
            let last = thisvob.vobu.len() - 1;
            finishvideoscan(&mut set.vg.vd, &mut slide, &mut thisvob.vobu[last], prevvidsect, &mut vsi);
            let vd = set.vg.vd;
            finish_vob_timing(&vd, thisvob, inoffset, log)?;
        }
    }
    if let Some(mut w) = writer.take() {
        w.flush()?;
    }
    if set.vg.vobs.iter().all(|v| v.vobu.is_empty()) {
        return err("ERR:  no VOBUs found.");
    }
    Ok(())
}

/// Consumes a `dvdauthor-data` sector from spumux (colour table, button colours and geometry).
fn private_data(buf: &[u8; 2048], inoffset: u64, vnum: usize, set: &mut PgcSet, crs: &mut [ColorRemap]) -> Result<()> {
    let mut i = 35;
    if buf[i] != 2 {
        return err(format!("ERR:  dvd info packet at inoffset {inoffset:#x} is unexpected version {}", buf[i]));
    }
    if buf[i + 1] != 1 {
        return err(format!("ERR:  unknown dvdauthor-data packet type at inoffset {inoffset:#x}: {}", buf[i + 1]));
    }
    let substreamid = (buf[i + 2] & 31) as usize;
    i += 3 + 8;
    let pgc = set.vg.vobs[vnum].pgc;
    while buf[i] != 0xff {
        match buf[i] {
            1 => {
                crs[substreamid].origmap = Some(pgc);
                let n = buf[i + 1] as usize;
                for j in 0..n {
                    crs[substreamid].newcolors[j] =
                        COLOR_UNUSED | (buf[i + 2 + 3 * j] as i32) << 16 | (buf[i + 3 + 3 * j] as i32) << 8 | buf[i + 4 + 3 * j] as i32;
                }
                for j in n..16 {
                    crs[substreamid].newcolors[j] = j as i32;
                }
                i += 2 + 3 * n;
            }
            2 => {
                let n = buf[i + 1] as usize;
                let vob = &mut set.vg.vobs[vnum];
                vob.buttoncoli[..n * 8].copy_from_slice(&buf[i + 2..i + 2 + n * 8]);
                let colors = &mut set.pgcs[pgc].colors;
                for j in 0..n {
                    for k in [0, 1, 4, 5] {
                        crs[substreamid].remapbyte(colors, &mut vob.buttoncoli[j * 8 + k])?;
                    }
                }
                i += 2 + 8 * n;
            }
            3 => {
                let nrbuttons = buf[i + 1] as usize;
                i += 2;
                for _ in 0..nrbuttons {
                    let bn = readpstr(buf, &mut i);
                    let Some(bidx) = set.pgcs[pgc].buttons.iter().position(|b| b.name == bn) else {
                        return err(format!("ERR:  Cannot find button '{bn}' as referenced by the subtitle at inoffset {inoffset:#x}"));
                    };
                    i += 2;
                    let mut bi = ButtonInfo { substreamid: substreamid as i32, autoaction: buf[i] != 0, ..ButtonInfo::default() };
                    i += 1;
                    bi.grp = buf[i] as i32;
                    bi.x1 = r2(&buf[i + 1..]);
                    bi.y1 = r2(&buf[i + 3..]);
                    bi.x2 = r2(&buf[i + 5..]);
                    bi.y2 = r2(&buf[i + 7..]);
                    i += 9;
                    bi.up = readpstr(buf, &mut i);
                    bi.down = readpstr(buf, &mut i);
                    bi.left = readpstr(buf, &mut i);
                    bi.right = readpstr(buf, &mut i);
                    let b = &mut set.pgcs[pgc].buttons[bidx];
                    if b.streams.len() < 3 {
                        b.streams.push(bi);
                    }
                }
            }
            x => return err(format!("ERR:  dvd info packet command within subtitle at inoffset {inoffset:#x}: {x}")),
        }
    }
    Ok(())
}

fn readpstr(b: &[u8], i: &mut usize) -> String {
    let end = b[*i..].iter().position(|&c| c == 0).map_or(b.len(), |p| *i + p);
    let s = String::from_utf8_lossy(&b[*i..end]).into_owned();
    *i = end + 1;
    s
}

/// `calcpts`: aligns `basepts` to whole fields; returns the aligned PTS.
fn calcpts(vd: &VideoDesc, cancomplain: bool, didcomplain: &mut bool, align: &mut Pts, basepts: Pts, mut nfields: i32, log: &mut Vec<String>) -> Pts {
    let fpts = vd.framepts();
    let bpframe = ((basepts * 2 - *align + fpts / 2) / fpts) as i32;
    if (*align + bpframe as Pts * fpts) / 2 != basepts {
        if !*didcomplain {
            if cancomplain {
                log.push("WARN: Video PTS does not line up on a multiple of a field.".into());
            }
            *didcomplain = true;
        }
        *align = basepts * 2;
    } else {
        nfields += bpframe;
    }
    (*align + nfields as Pts * fpts) / 2
}

/// Presentation times of all VOBUs of a finished VOB.
fn finish_vob_timing(vd: &VideoDesc, vob: &mut Vob, inoffset: u64, log: &mut Vec<String>) -> Result<()> {
    let n = vob.vobu.len();
    let mut finalaudiopts: Pts = -1;
    for ach in vob.audch.iter().take(32) {
        if let Some(last) = ach.audpts.last() {
            if last.pts[1] > finalaudiopts {
                finalaudiopts = last.pts[1];
            }
        }
    }
    for pass in 0..3 {
        let mut pts_align: Pts = -1;
        let mut complained = false;
        for j in 0..n {
            if !vob.vobu[j].hasvideo {
                continue;
            }
            if pts_align == -1 {
                pts_align = vob.vobu[j].firstvideopts * 2;
                if pass == 1 {
                    pts_align += 1;
                }
            }
            let (fv, fi, nf) = (vob.vobu[j].firstvideopts, vob.vobu[j].firstifield, vob.vobu[j].numfields);
            let p0 = calcpts(vd, pass == 2, &mut complained, &mut pts_align, fv, -fi, log);
            let p1 = calcpts(vd, pass == 2, &mut complained, &mut pts_align, fv, -fi + nf, log);
            vob.vobu[j].videopts = [p0, p1];
            if complained && pass < 2 {
                break;
            }
            vob.vobu[j].sectpts[0] = p0;
            vob.vobu[j].sectpts[1] = if j + 1 == n && finalaudiopts > p1 { finalaudiopts } else { p1 };
        }
        if !complained {
            break;
        }
    }
    // Guess the times of VOBUs without video.
    for i in 0..n {
        if vob.vobu[i].hasvideo {
            continue;
        }
        let sector = vob.vobu[i].sector;
        let mut firstaudiopts: Pts = -1;
        for ach in vob.audch.iter().take(32) {
            if let Some(a) = ach.audpts.iter().find(|a| a.asect >= sector) {
                if firstaudiopts == -1 || a.pts[0] < firstaudiopts {
                    firstaudiopts = a.pts[0];
                }
            }
        }
        if firstaudiopts == -1 {
            log.push(format!("WARN: Cannot detect pts for VOBU at inoffset {inoffset:#x} if there is no audio or video; using SCR instead."));
            firstaudiopts = readscr(&vob.vobu[i].sectdata[4..]) + 4 * 147;
        }
        if i == 0 {
            return err(format!("ERR:  Cannot infer pts for VOBU at inoffset {inoffset:#x} if there is no audio or video and it is the first VOBU."));
        }
        let frpts = vd.framepts();
        let prev0 = vob.vobu[i - 1].sectpts[0];
        let mut p = firstaudiopts - prev0;
        p += frpts - 1;
        p -= p % frpts;
        p += prev0;
        if p < vob.vobu[i - 1].sectpts[1] {
            return err(format!("ERR:  pts {firstaudiopts} rounded up to {p} lies within previous vobu {} at inoffset {inoffset:#x}", i - 1));
        }
        vob.vobu[i - 1].sectpts[1] = p;
        vob.vobu[i].sectpts[0] = p;
        let end = if i + 1 == n {
            if finalaudiopts > p {
                finalaudiopts
            } else {
                p + frpts
            }
        } else if vob.vobu[i + 1].hasvideo {
            vob.vobu[i + 1].sectpts[0]
        } else {
            continue;
        };
        if end <= p {
            return err(format!("ERR:  Audio and video are too poorly synchronised at inoffset {inoffset:#x}; you must remultiplex."));
        }
        vob.vobu[i].sectpts[1] = end;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MarkChapters
// ---------------------------------------------------------------------------

fn find_nearest_vobu(vob: &Vob, pts: Pts) -> i32 {
    let h = vob.vobu.len() as i32 - 1;
    if h < 0 {
        return -1;
    }
    let pts = pts + vob.vobu[0].sectpts[0];
    let mut i = vob.find_vobu(pts, 0, h);
    if i + 1 < vob.vobu.len() as i32 && i >= 0 && (pts - vob.vobu[(i + 1) as usize].sectpts[0]).abs() < (pts - vob.vobu[i as usize].sectpts[0]).abs() {
        i += 1;
    }
    i
}

/// Fills in cell ids of VOBUs, cells and programs (`MarkChapters`).
pub fn mark_chapters(set: &mut PgcSet, log: &mut Vec<String>) -> Result<()> {
    let PgcSet { pgcs, vg, .. } = set;
    for (pi, pgc) in pgcs.iter_mut().enumerate() {
        for (si, src) in pgc.sources.iter_mut().enumerate() {
            let vob = &mut vg.vobs[src.vob];
            for (ci, cell) in src.cells.iter_mut().enumerate() {
                let mut v = find_nearest_vobu(vob, cell.startpts);
                if v >= 0 && (v as usize) < vob.vobu.len() {
                    vob.vobu[v as usize].vobcellid = 1;
                }
                cell.scellid = v;
                if v >= 0 && (v as usize) < vob.vobu.len() && vob.vobu[v as usize].firstifield != 0 {
                    log.push(format!("WARN: GOP may not be closed on cell {} of source {} of pgc {}", ci + 1, src.fname, pi + 1));
                }
                let _ = si;
                if cell.endpts >= 0 {
                    v = find_nearest_vobu(vob, cell.endpts);
                    if v >= 0 && (v as usize) < vob.vobu.len() {
                        vob.vobu[v as usize].vobcellid = 1;
                    }
                } else {
                    v = vob.vobu.len() as i32;
                }
                cell.ecellid = v;
            }
        }
    }
    for vob in &mut vg.vobs {
        if vob.vobu.is_empty() {
            continue;
        }
        let mut cellvobu = 0;
        let mut cellid = 0;
        vob.vobu[0].vobcellid = 1;
        for j in 0..vob.vobu.len() {
            if vob.vobu[j].vobcellid != 0 {
                cellid += 1;
                cellvobu = j as i32;
            }
            vob.vobu[j].vobcellid = cellid + vob.vobid * 256;
            vob.vobu[j].firstvobuincell = cellvobu;
        }
        let mut cellvobu = vob.vobu.len() as i32 - 1;
        for j in (0..vob.vobu.len()).rev() {
            vob.vobu[j].lastvobuincell = cellvobu;
            if vob.vobu[j].firstvobuincell == j as i32 {
                cellvobu = j as i32 - 1;
            }
        }
        vob.numcells = cellid;
        if cellid >= 256 {
            return err(format!("ERR:  VOB {} has too many cells ({cellid}, 256 allowed)", vob.fname));
        }
    }
    for (pi, pgc) in pgcs.iter_mut().enumerate() {
        for src in &mut pgc.sources {
            let vob = &vg.vobs[src.vob];
            let nv = vob.vobu.len() as i32;
            for cell in &mut src.cells {
                let map = |id: i32| -> i32 {
                    if id < 0 {
                        1
                    } else if id < nv {
                        vob.vobu[id as usize].vobcellid & 255
                    } else {
                        vob.numcells + 1
                    }
                };
                cell.scellid = map(cell.scellid);
                cell.ecellid = map(cell.ecellid);
                pgc.numcells += cell.ecellid - cell.scellid;
                if cell.scellid != cell.ecellid && cell.kind != CellKind::Neither {
                    pgc.numprograms += 1;
                    if cell.kind == CellKind::ChapterProgram {
                        pgc.numchapters += 1;
                    }
                    if pgc.numprograms >= 256 {
                        return err(format!("ERR:  PGC {} has too many programs ({}, 256 allowed)", pi + 1, pgc.numprograms));
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// setattr
// ---------------------------------------------------------------------------

/// Matches the user's audio / subpicture declarations with what was found in the VOBs and fills
/// in defaults (`setattr`, without the diagnostics).
pub fn set_attributes(set: &mut PgcSet) -> Result<()> {
    let PgcSet { pgcs, vg, kind, .. } = set;
    let vd = &mut vg.vd;
    set_unset(&mut vd.vmpeg, VM_MPEG2);
    set_unset(&mut vd.vres, VS_720H);
    if vd.vformat == 0 {
        return err(format!("ERR:  no video format specified for {:?}", kind));
    }
    set_unset(&mut vd.vaspect, VA_4X3);
    if vd.vaspect == VA_4X3 {
        if vd.vwidescreen == VW_NOLETTERBOX || vd.vwidescreen == VW_NOPANSCAN {
            return err("ERR:  widescreen conversion should not be set to either noletterbox or nopanscan for 4:3 source material.");
        }
    } else if vd.vwidescreen == VW_CROP {
        return err("ERR:  widescreen conversion should not be set to crop for 16:9 source material.");
    }

    for i in 0..32usize {
        let id = (i >> 2) as i32 + 1;
        let afmt = (i & 3) as i32 + 1;
        let idx = (id - 1 + (afmt - 1) * 8) as usize;
        let Some(fad) = vg.vobs.iter().map(|v| &v.audch[idx]).find(|a| !a.audpts.is_empty()).map(|a| a.ad) else {
            continue;
        };
        if vg.ad[..vg.numaudiotracks].iter().any(|a| a.aformat == afmt && a.aid == id) {
            continue;
        }
        let mut best: i32 = -1;
        let mut matchidx: i32 = -1;
        let mut fnd = false;
        for j in 0..vg.numaudiotracks {
            let a = &vg.ad[j];
            if a.aid != 0 {
                continue;
            }
            let mut count = 0;
            let mut ok = true;
            for (mine, theirs) in [(a.aformat, afmt), (a.aquant, fad.aquant), (a.adolby, fad.adolby), (a.achannels, fad.achannels), (a.asample, fad.asample)] {
                if mine != 0 && theirs != 0 && mine != theirs {
                    ok = false;
                    break;
                }
                if mine == theirs {
                    count += 1;
                }
            }
            if !ok {
                continue;
            }
            if count > best {
                best = count;
                matchidx = j as i32;
            }
            fnd = true;
        }
        let j = if fnd {
            matchidx as usize
        } else {
            if vg.numaudiotracks >= 8 {
                return err("ERR:  too many audio streams");
            }
            vg.numaudiotracks += 1;
            vg.numaudiotracks - 1
        };
        let a = &mut vg.ad[j];
        a.aformat = afmt;
        a.aid = id;
        set_unset(&mut a.aquant, fad.aquant);
        set_unset(&mut a.adolby, fad.adolby);
        set_unset(&mut a.achannels, fad.achannels);
        set_unset(&mut a.asample, fad.asample);
    }
    for a in vg.ad[..vg.numaudiotracks].iter_mut() {
        set_unset(&mut a.aformat, AF_MP2);
        match a.aformat {
            AF_AC3 | AF_DTS => {
                set_unset(&mut a.aquant, AQ_DRC);
                set_unset(&mut a.achannels, 6);
            }
            AF_MP2 => {
                set_unset(&mut a.aquant, AQ_20);
                set_unset(&mut a.achannels, 2);
            }
            AF_PCM => {
                set_unset(&mut a.achannels, 2);
                set_unset(&mut a.aquant, AQ_16);
            }
            _ => {}
        }
        set_unset(&mut a.asample, AS_48KHZ);
    }

    let mask = vg.vd.subpmask();
    for pgc in pgcs.iter_mut() {
        let noinfer = (0..4).any(|m| pgc.subpmap[0][m] != 0);
        if !noinfer {
            for src in &pgc.sources.clone() {
                let vob = &vg.vobs[src.vob];
                for k in 0..32usize {
                    if vob.audch[k + 32].audpts.is_empty() {
                        continue;
                    }
                    if pgc.subpmap.iter().any(|row| row.iter().any(|&x| x == 128 + k as u8)) {
                        continue;
                    }
                    let mut used = false;
                    for l in 0..vg.numsubpicturetracks {
                        for m in 0..4 {
                            if vg.sp[l].idmap[m] == 128 + k as u8 && pgc.subpmap[l][m] == 0 {
                                pgc.subpmap[l][m] = 128 + k as u8;
                                used = true;
                            }
                        }
                    }
                    if used {
                        continue;
                    }
                    let mut l = 0;
                    while l < 32 {
                        let defined = (l < vg.numsubpicturetracks && vg.sp[l].idmap.iter().any(|&x| x != 0)) || pgc.subpmap[l].iter().any(|&x| x != 0);
                        if !defined {
                            break;
                        }
                        l += 1;
                    }
                    if l >= 32 {
                        return err("ERR:  no free subpicture slot");
                    }
                    for m in 0..4 {
                        pgc.subpmap[l][m] = if mask & (1 << m) != 0 { 128 + k as u8 } else { 127 };
                    }
                }
            }
        }
        // Remove illegal modes and fill the modes the mask asks for with the main stream.
        for l in 0..32 {
            let mut mainid: i32 = -1;
            for m in 0..4 {
                if mask & (1 << m) == 0 && pgc.subpmap[l][m] & 128 == 128 {
                    pgc.subpmap[l][m] = 127;
                }
                if pgc.subpmap[l][m] & 128 != 0 {
                    let id = (pgc.subpmap[l][m] & 127) as i32;
                    if mainid == -1 {
                        mainid = id;
                    } else if mainid >= 0 && mainid != id {
                        mainid = -2;
                    }
                }
            }
            if mainid == -1 {
                continue;
            }
            for m in 0..4 {
                if mask & (1 << m) != 0 && pgc.subpmap[l][m] & 128 == 0 && mainid >= 0 {
                    pgc.subpmap[l][m] = 128 + mainid as u8;
                }
            }
        }
    }
    for i in 0..32 {
        if pgcs.iter().any(|p| p.subpmap[i].iter().any(|&x| x != 0)) && i >= vg.numsubpicturetracks {
            vg.numsubpicturetracks = i + 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FixVobus
// ---------------------------------------------------------------------------

fn find_button(pg: &Pgc, dest: &str, dflt: i32) -> i32 {
    if dest.is_empty() {
        return dflt;
    }
    pg.buttons.iter().position(|b| b.name == dest).map_or(dflt, |i| i as i32 + 1)
}

/// Relative VOBU offset for a DSI pointer (`getsect`).
fn getsect(vob: &Vob, cur: i32, jump: i32, skip: bool, notfound: u32) -> u32 {
    let mut skipbit = 0u32;
    if skip {
        let (l, h) = if cur < jump { (cur + 1, jump - 1) } else { (jump + 1, cur - 1) };
        if (l..=h).any(|i| vob.vobu[i as usize].hasvideo) {
            skipbit = 0x4000_0000;
        }
    }
    if jump < 0 || jump >= vob.vobu.len() as i32 || vob.vobu[jump as usize].vobcellid != vob.vobu[cur as usize].vobcellid {
        return notfound | skipbit;
    }
    (vob.vobu[jump as usize].sector - vob.vobu[cur as usize].sector).unsigned_abs() | if vob.vobu[jump as usize].hasvideo { 0x8000_0000 } else { 0 } | skipbit
}

fn findnextvideo(vob: &Vob, cur: i32, dir: i32) -> i32 {
    if dir == 1 {
        ((cur + 1)..vob.vobu.len() as i32).find(|&i| vob.vobu[i as usize].hasvideo).unwrap_or(-1)
    } else {
        (0..cur).rev().find(|&i| vob.vobu[i as usize].hasvideo).unwrap_or(-1)
    }
}

fn findaudsect(vob: &Vob, aind: usize, pts0: Pts, pts1: Pts) -> i32 {
    let ach = &vob.audch[aind];
    let (mut l, mut h) = (0i32, ach.audpts.len() as i32 - 1);
    if h < l {
        return -1;
    }
    while h > l {
        let m = (l + h + 1) / 2;
        if pts0 < ach.audpts[m as usize].pts[0] {
            h = m - 1;
        } else {
            l = m;
        }
    }
    if ach.audpts[l as usize].pts[0] > pts1 {
        return -1;
    }
    ach.audpts[l as usize].asect
}

fn findvobubysect(vob: &Vob, sect: i32) -> i32 {
    let (mut l, mut h) = (0i32, vob.vobu.len() as i32 - 1);
    if h < 0 || sect < vob.vobu[0].sector {
        return -1;
    }
    while l < h {
        let m = (l + h + 1) / 2;
        if sect < vob.vobu[m as usize].sector {
            h = m - 1;
        } else {
            l = m;
        }
    }
    l
}

fn findspuidx(vob: &Vob, ach: usize, pts0: Pts) -> i32 {
    let (mut l, mut h) = (0i32, vob.audch[ach].audpts.len() as i32 - 1);
    if h < l {
        return -1;
    }
    while h > l {
        let m = (l + h + 1) / 2;
        if pts0 < vob.audch[ach].audpts[m as usize].pts[0] {
            h = m - 1;
        } else {
            l = m;
        }
    }
    l
}

/// Fills in the NAV packs (PCI and DSI) of every VOBU of the already written VOB files
/// (`FixVobus`). `ctx_for` gives the VM compiler context of a PGC (for button commands).
pub fn fix_vobus(files: &VobFiles, set: &PgcSet, ctx_for: &dyn Fn(usize) -> dvdvm::Context) -> Result<()> {
    let va = &set.vg;
    let mut cur: Option<(i32, File)> = None;
    let mut buf = [0u8; 2048];
    for vob in &va.vobs {
        let pg = &set.pgcs[vob.pgc];
        for (vobuindex, vobu) in vob.vobu.iter().enumerate() {
            let vobuindex = vobuindex as i32;
            if cur.as_ref().map(|c| c.0) != Some(vobu.fnum) {
                let f = std::fs::OpenOptions::new().write(true).open(files.path(vobu.fnum)).map_err(|e| MuxError(format!("ERR:  Error opening {}: {e}", files.path(vobu.fnum).display())))?;
                cur = Some((vobu.fnum, f));
            }
            buf[..0x26].copy_from_slice(&vobu.sectdata);
            w4(&mut buf[0x26..], 0x100 + MPID_PRIVATE2 as u32);
            w2(&mut buf[0x2a..], 0x3d4);
            buf[0x2c] = 0;
            buf[0x2d..0x400].fill(0);
            w4(&mut buf[0x400..], 0x100 + MPID_PRIVATE2 as u32);
            w2(&mut buf[0x404..], 0x3fa);
            buf[0x406] = 1;
            buf[0x407..0x7ff].fill(0);
            buf[0x7ff] = 0;
            let scr = readscr(&buf[4..]);

            w4(&mut buf[0x2d..], vobu.sector as u32);
            w4(&mut buf[0x39..], vobu.sectpts[0] as u32);
            w4(&mut buf[0x3d..], vobu.sectpts[1] as u32);
            if vobu.hasseqend {
                w4(&mut buf[0x41..], vobu.videopts[1] as u32);
            }
            let eltm = va.vd.bcd_time(vobu.sectpts[0] - vob.vobu[vobu.firstvobuincell as usize].sectpts[0]);
            w4(&mut buf[0x45..], eltm);

            // The buttons of a menu with an intro start with the page proper (cell 2): the intro cell's
            // NAV packs carry no highlight info (dvdauthor would copy it into every VOBU).
            let intro_cell = set.kind != dvdvm::Domain::Vts && vob.numcells > 1 && vobu.vobcellid & 255 == 1;
            if !pg.buttons.is_empty() && !intro_cell {
                let mask = va.vd.subpmask();
                let mut idmap = [0usize; 3];
                let mut nrgrps = 0usize;
                w2(&mut buf[0x9b..], 0);
                w4(&mut buf[0x8d..], 0);
                w2(&mut buf[0x8d..], 1);
                w4(&mut buf[0x8f..], vob.vobu[0].sectpts[0] as u32);
                w4(&mut buf[0x93..], u32::MAX);
                w4(&mut buf[0x97..], u32::MAX);
                for j in 0..4usize {
                    if mask & (1 << j) != 0 {
                        if nrgrps >= 3 {
                            return err("subpicture mask selects more than three display modes");
                        }
                        idmap[nrgrps] = j;
                        let v = r2(&buf[0x9b..]) + 0x1000 + (((1 << j) >> 1) << ((2 - nrgrps) * 4));
                        w2(&mut buf[0x9b..], v as u32);
                        nrgrps += 1;
                    }
                }
                buf[0x9e] = pg.buttons.len() as u8;
                buf[0x9f] = pg.buttons.len() as u8;
                buf[0xa3..0xa3 + 24].copy_from_slice(&vob.buttoncoli);
                for (grp, &idm) in idmap.iter().enumerate().take(nrgrps) {
                    let mut boffs = 0xbb + 18 * (grp * 36 / nrgrps);
                    let sid = (pg.subpmap[0][idm] & 127) as i32;
                    for (j, b) in pg.buttons.iter().enumerate() {
                        let Some(bi) = b.streams.iter().find(|s| s.substreamid == sid) else {
                            continue;
                        };
                        let nb = pg.buttons.len() as i32;
                        let j = j as i32;
                        let o = &mut buf[boffs..boffs + 18];
                        o[0] = (bi.grp * 64) as u8 | (bi.x1 >> 4) as u8;
                        o[1] = (bi.x1 << 4) as u8 | (bi.x2 >> 8) as u8;
                        o[2] = bi.x2 as u8;
                        o[3] = if bi.autoaction { 64 } else { 0 } | (bi.y1 >> 4) as u8;
                        o[4] = (bi.y1 << 4) as u8 | (bi.y2 >> 8) as u8;
                        o[5] = bi.y2 as u8;
                        o[6] = find_button(pg, &bi.up, if j == 0 { nb } else { j }) as u8;
                        o[7] = find_button(pg, &bi.down, if j + 1 == nb { 1 } else { j + 2 }) as u8;
                        o[8] = find_button(pg, &bi.left, if j == 0 { nb } else { j }) as u8;
                        o[9] = find_button(pg, &bi.right, if j + 1 == nb { 1 } else { j + 2 }) as u8;
                        let mut code = Vec::new();
                        let end = dvdvm::compile(&mut code, 0, 0, &b.commands, &ctx_for(vob.pgc), set.kind)?;
                        if end == 8 {
                            o[10..18].copy_from_slice(&code[..8]);
                        } else {
                            o[10..18].copy_from_slice(&[0x71, 0x01, 0x00, 0x0F, 0x00, (j + 1) as u8, 0x00, 0x0d]);
                        }
                        boffs += 18;
                    }
                }
            }

            w4(&mut buf[0x407..], scr as u32);
            w4(&mut buf[0x40b..], vobu.sector as u32);
            if vobu.numref > 0 {
                for j in 0..3 {
                    let r = vobu.lastrefsect[j.min(vobu.numref - 1)];
                    w4(&mut buf[0x413 + j * 4..], (r - vobu.sector) as u32);
                }
            }
            w2(&mut buf[0x41f..], (vobu.vobcellid >> 8) as u32);
            buf[0x422] = vobu.vobcellid as u8;
            let e = r4(&buf[0x45..]);
            w4(&mut buf[0x423..], e);
            w4(&mut buf[0x433..], vob.vobu[0].sectpts[0] as u32);
            w4(&mut buf[0x437..], vob.vobu[vob.vobu.len() - 1].sectpts[1] as u32);
            w4(&mut buf[0x4f1..], getsect(vob, vobuindex, findnextvideo(vob, vobuindex, 1), false, 0xbfff_ffff));
            w4(&mut buf[0x541..], getsect(vob, vobuindex, vobuindex + 1, false, 0x3fff_ffff));
            w4(&mut buf[0x545..], getsect(vob, vobuindex, vobuindex - 1, false, 0x3fff_ffff));
            w4(&mut buf[0x595..], getsect(vob, vobuindex, findnextvideo(vob, vobuindex, -1), false, 0xbfff_ffff));
            for j in 0..va.numaudiotracks {
                let mut s = va.audch_of(j);
                if s >= 0 {
                    s = findaudsect(vob, s as usize, vobu.sectpts[0], vobu.sectpts[1]);
                }
                let v: u32 = if s >= 0 {
                    let mut s = s - vobu.sector;
                    if !(-0x1fff..=0x1fff).contains(&s) {
                        s = 0;
                    }
                    if s < 0 {
                        (-s) as u32 | 0x8000
                    } else {
                        s as u32
                    }
                } else {
                    0x3fff
                };
                w2(&mut buf[0x599 + j * 2..], v);
            }
            for j in 0..va.numsubpicturetracks {
                let ach = &vob.audch[j | 32];
                let s: u32 = if !ach.audpts.is_empty() {
                    let id = findspuidx(vob, j | 32, vobu.sectpts[0]);
                    let target = if id >= 0 && ach.audpts[id as usize].pts[0] < vobu.sectpts[1] && ach.audpts[id as usize].pts[1] >= vobu.sectpts[0] {
                        findvobubysect(vob, ach.audpts[id as usize].asect)
                    } else if (id < 0 || ach.audpts[id as usize].pts[1] < vobu.sectpts[0])
                        && (id + 1 == ach.audpts.len() as i32 || ach.audpts[(id + 1) as usize].pts[0] >= vobu.sectpts[1])
                    {
                        vobuindex
                    } else {
                        // In dvdauthor C (dvdvob.c), id + 1 is guaranteed in-bounds by the preceding branch:
                        // findspuidx returns id in -1..len-1; the first branch handled id >= 0 with SPU overlapping this VOBU,
                        // and the else-if handled id + 1 == numaudpts or next SPU starting at/after this VOBU.
                        // Thus, (id + 1) as usize < ach.audpts.len() is guaranteed, with defensive fallback to 0.
                        findvobubysect(vob, ach.audpts.get((id + 1) as usize).map_or(0, |a| a.asect))
                    };
                    let back = target < vobuindex;
                    let mut s = getsect(vob, vobuindex, target, false, 0x7fff_ffff) & 0x7fff_ffff;
                    if s == 0 {
                        s = 0x7fff_ffff;
                    }
                    if s != 0x7fff_ffff && back {
                        s |= 0x8000_0000;
                    }
                    s
                } else {
                    0
                };
                w4(&mut buf[0x5a9 + j * 4..], s);
            }
            w4(&mut buf[0x40f..], (vobu.lastsector - vobu.sector) as u32);
            let (mut vff, mut vrew) = (vobuindex, vobuindex);
            for j in 0..19usize {
                let nff = vob.find_vobu(vobu.sectpts[0] + TIMELINE[j] * FFREW_HALFSEC, vobu.firstvobuincell, vobu.lastvobuincell);
                let nrew = vob.find_vobu(vobu.sectpts[0] - TIMELINE[j] * FFREW_HALFSEC, vobu.firstvobuincell, vobu.lastvobuincell);
                w4(&mut buf[0x53d - j * 4..], getsect(vob, vobuindex, nff, j >= 15 && nff > vff + 1, 0x3fff_ffff));
                w4(&mut buf[0x549 + j * 4..], getsect(vob, vobuindex, nrew, j >= 15 && nrew < vrew - 1, 0x3fff_ffff));
                vff = nff;
                vrew = nrew;
            }
            let f = &mut cur.as_mut().expect("output file is open").1;
            f.seek(SeekFrom::Start(vobu.fsect as u64 * 2048))?;
            f.write_all(&buf)?;
        }
    }
    Ok(())
}

/// Length in sectors of the VOB data of a set (`getvoblen`).
pub fn voblen(vg: &VobGroup) -> i32 {
    vg.vobs.iter().rev().find(|v| !v.vobu.is_empty()).map_or(0, |v| v.vobu[v.vobu.len() - 1].lastsector + 1)
}

pub fn num_vobus(vg: &VobGroup) -> usize {
    vg.vobs.iter().map(|v| v.vobu.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> VideoDesc {
        let mut vd = VideoDesc::default();
        vd.set_format(true);
        vd
    }

    #[test]
    fn bcd_time_and_frame_rate_bits() {
        // 1:01:01 and 0 frames, PAL (25 fps code 1 in the top bits of the frame byte).
        assert_eq!(pal().bcd_time(3661 * 90_000), 0x0101_0140);
        let mut ntsc = VideoDesc::default();
        ntsc.set_format(false);
        assert_eq!(ntsc.bcd_time(0) >> 6 & 3, 3);
    }

    /// Ten VOBUs of half a second, ten sectors each, video in all but the odd ones.
    fn vob() -> Vob {
        let mut v = Vob::new("x", 0);
        for i in 0..10 {
            v.vobu.push(Vobu {
                sector: i * 10,
                lastsector: i * 10 + 9,
                vobcellid: 257,
                hasvideo: i % 2 == 0,
                sectpts: [i as Pts * 45_000, (i as Pts + 1) * 45_000],
                ..Vobu::default()
            });
        }
        v.vobid = 1;
        v
    }

    #[test]
    fn vobu_search_and_dsi_offsets() {
        let v = vob();
        assert_eq!(v.find_vobu(3 * 45_000 + 5, 0, 9), 3);
        assert_eq!(v.find_vobu(-1, 0, 9), -1);
        assert_eq!(v.find_vobu(10 * 45_000, 0, 9), 10);
        // Next VOBU with video is two ahead: 20 sectors, video flag set.
        assert_eq!(getsect(&v, 0, findnextvideo(&v, 0, 1), false, 0xbfff_ffff), 20 | 0x8000_0000);
        // Past the end: the "not found" marker; skipping flag for a >1 VOBU jump over video.
        assert_eq!(getsect(&v, 9, 10, false, 0x3fff_ffff), 0x3fff_ffff);
        assert_eq!(getsect(&v, 0, 4, true, 0), 40 | 0x8000_0000 | 0x4000_0000);
        assert_eq!(getsect(&v, 4, 0, true, 0), 40 | 0x8000_0000 | 0x4000_0000);
    }

    #[test]
    fn vob_files_split_naming() {
        let f = VobFiles { dir: PathBuf::from("d"), base: "VTS_01".into(), single: false };
        assert_eq!(f.path(0), Path::new("d").join("VTS_01_0.VOB"));
        assert_eq!(f.path(2), Path::new("d").join("VTS_01_2.VOB"));
        let m = VobFiles { dir: PathBuf::from("d"), base: "VIDEO_TS.VOB".into(), single: true };
        assert_eq!(m.path(-1), Path::new("d").join("VIDEO_TS.VOB"));
        // A VOB file is closed just below 1 GiB of 2048-byte sectors.
        assert!(MAX_VOB_SECTORS as u64 * 2048 < 1 << 30);
    }

    #[test]
    fn scr_and_pts_round_trip() {
        let mut b = [0u8; 14];
        writescr(&mut b[4..], 123_456_789);
        assert_eq!(readscr(&b[4..]), 123_456_789);
        let mut p = [0x21u8, 0, 1, 0, 1];
        writepts(&mut p, 987_654_321);
        assert_eq!(readpts(&p), 987_654_321);
    }

    #[test]
    fn remapcolor_falls_back_to_closest_when_palette_full() {
        let mut cr = ColorRemap::new();
        let mut colors = [0i32; 16];
        for (i, c) in colors.iter_mut().enumerate() {
            *c = (i as i32 * 10) << 16 | 0x8080;
        }
        // Exact match
        cr.newcolors[0] = COLOR_UNUSED | (50 << 16) | 0x8080;
        assert_eq!(cr.remapcolor(&mut colors, 0).unwrap(), 5);

        // 17th color closest to Y = 50 (index 5)
        cr.newcolors[1] = COLOR_UNUSED | (52 << 16) | 0x8080;
        assert_eq!(cr.remapcolor(&mut colors, 1).unwrap(), 5);

        // 18th color closest to Y = 150 (index 15)
        cr.newcolors[2] = COLOR_UNUSED | (148 << 16) | 0x8080;
        assert_eq!(cr.remapcolor(&mut colors, 2).unwrap(), 15);
    }
}
