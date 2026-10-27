//! IFO / BUP writer for DVD-Video: video manager (`VIDEO_TS.IFO`) and video title sets
//! (`VTS_nn_0.IFO`), with all tables and program chains.
//!
//! A port of dvdauthor's `dvdifo.c` and `dvdpgc.c` (without jump pads and cell commands, which this
//! program never uses). The output must be byte-identical to dvdauthor 0.7.2+
//! (`tests/dvdauthor_native.rs`); the layout of every table is planned before writing, so the
//! files are deterministic.
//!
//! GPL-2.0 (derived from dvdauthor).

use crate::dvd_mux::{self as mux, CellKind, MuxError, Pgc, PgcSet, VobGroup, AD_SURROUND, AL_LANG, VA_16X9, VF_PAL, VW_CROP, VW_NOLETTERBOX, VW_NOPANSCAN};
use crate::dvdvm::{self, Context, Domain};

type Result<T> = std::result::Result<T, MuxError>;

/// The IFO of a titleset has at least this many sectors (IFO and BUP end up in different ECC blocks).
pub const MIN_IFO_SECTORS: i32 = 16;
const MAX_CELLS: usize = 4096;
const MAX_PGC_SIZE: usize = 236 + 128 * 8 + 256 + MAX_CELLS * (24 + 4);
/// Provider ID of the video manager.
pub const PROVIDER: &[u8] = b"DVDAuthor 0.7.2+";

fn w2(b: &mut [u8], v: u32) {
    b[0] = (v >> 8) as u8;
    b[1] = v as u8;
}

fn w4(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_be_bytes());
}

fn r4(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn r2(b: &[u8]) -> u32 {
    (b[0] as u32) << 8 | b[1] as u32
}

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(MuxError(msg.into()))
}

/// Everything the table writers need: the menus of the domain and (for a titleset) its titles.
pub struct Workset<'a> {
    pub menus: &'a PgcSet,
    pub titles: Option<&'a PgcSet>,
}

impl Workset<'_> {
    /// Compiler context for PGC `pgc` of `set` (or for the first-play PGC when `pgc` is `None`).
    pub fn context(&self, set: &PgcSet, pgc: Option<usize>) -> Context {
        Context {
            group_entries: Some(set.pgcs.iter().map(|p| p.entries).collect()),
            all_menu_entries: self.menus.pgcs.iter().map(|p| p.entries).collect(),
            pgc: pgc.map(|i| [set.pgcs[i].numchapters, set.pgcs[i].numprograms, set.pgcs[i].numcells]),
            titles: self.titles.map(|t| t.pgcs.iter().map(|p| p.numchapters).collect()),
        }
    }
}

fn domain_of(set: &PgcSet) -> Domain {
    set.kind
}

// ---------------------------------------------------------------------------
// PGCs
// ---------------------------------------------------------------------------

/// Command table + program map + cell tables of one PGC; returns the number of bytes used.
fn genpgc(buf: &mut Vec<u8>, ws: &Workset, set: &PgcSet, pgc: usize) -> Result<usize> {
    let va: &VobGroup = &set.vg;
    let p: &Pgc = &set.pgcs[pgc];
    let domain = domain_of(set);
    let ctx = ws.context(set, Some(pgc));

    buf[2] = p.numprograms as u8;
    buf[3] = p.numcells as u8;
    w4(&mut buf[4..], va.vd.bcd_time(set.pgc_pts_span(pgc)));
    for i in 0..va.numaudiotracks {
        if va.ad[i].aid != 0 {
            buf[12 + i * 2] = 0x80 | (va.ad[i].aid - 1) as u8;
        }
    }
    for i in 0..va.numsubpicturetracks {
        let mut e = false;
        for m in 0..4 {
            if p.subpmap[i][m] & 128 != 0 {
                buf[28 + i * 4 + m] = p.subpmap[i][m] & 127;
                e = true;
            }
        }
        if e {
            buf[28 + i * 4] |= 0x80;
        }
    }
    buf[163] = p.pauselen;
    for i in 0..16 {
        let c = if p.colors[i] < mux::COLOR_UNUSED { p.colors[i] } else { 0x108080 };
        w4(&mut buf[164 + i * 4..], c as u32);
    }

    let mut d = 0xEC;
    {
        let preptr = d + 8;
        let mut cd = preptr;
        if let Some(prei) = &p.prei {
            cd = dvdvm::compile(buf, preptr, cd, prei, &ctx, domain).map_err(|e| MuxError(format!("{e}\nERR:  in {domain:?} pgc {pgc}, <pre>")))?;
        } else if !p.buttons.is_empty() {
            buf[cd..cd + 8].copy_from_slice(&[0x56, 0x00, 0x00, 0x00, 4, 0x00, 0x00, 0x00]);
            cd += 8;
        }
        let postptr = cd;
        let nb = p.buttons.len();
        if nb > 0 {
            // Transfer sequence for button commands too long for the PCI packet: g14 = g15; g15 = 0.
            buf[cd..cd + 8].copy_from_slice(&[0x61, 0x00, 0x00, 0x0E, 0x00, 0x0F, 0x00, 0x00]);
            buf[cd + 8..cd + 16].copy_from_slice(&[0x71, 0x00, 0x00, 0x0F, 0x00, 0x00, 0x00, 0x00]);
            cd += 8 * (nb + 2);
        }
        if let Some(posti) = &p.posti {
            cd = dvdvm::compile(buf, postptr, cd, posti, &ctx, domain).map_err(|e| MuxError(format!("{e}\nERR:  in {domain:?} pgc {pgc}, <post>")))?;
        } else {
            buf[cd..cd + 8].copy_from_slice(&[0x30, 0x01, 0, 0, 0, 0, 0, 0]);
            cd += 8;
        }
        if nb > 0 {
            let buttonptr = cd;
            for (i, b) in p.buttons.iter().enumerate() {
                let cdd = dvdvm::compile(buf, postptr, cd, &b.commands, &ctx, domain).map_err(|e| MuxError(format!("{e}\nERR:  in {domain:?} pgc {pgc}, button {}", b.name)))?;
                if cdd == cd + 8 {
                    // Fits in one command: it went into the VOB's NAV packs; clear the trial compile.
                    buf[postptr + i * 8 + 16..postptr + i * 8 + 24].fill(0);
                    buf[cd..cd + 8].fill(0);
                } else {
                    let line = ((cd - postptr) / 8 + 1) as u8;
                    buf[postptr + i * 8 + 16..postptr + i * 8 + 24].copy_from_slice(&[0x00, 0xa1, 0x00, 0x0E, 0x00, (i + 1) as u8, 0x00, line]);
                    cd = cdd;
                }
            }
            if cd == buttonptr {
                buf[postptr..postptr + 16].fill(0);
            }
        }
        dvdvm::optimize(buf, postptr, postptr, &mut cd)?;
        let cellptr = cd;
        w2(&mut buf[228..], d as u32);
        if cd - d - 8 > 128 * 8 {
            return err("ERR:  Can only have 128 commands for pre, post, and cell commands.");
        }
        w2(&mut buf[d..], ((postptr - preptr) / 8) as u32);
        w2(&mut buf[d + 2..], ((cellptr - postptr) / 8) as u32);
        w2(&mut buf[d + 4..], ((cd - cellptr) / 8) as u32);
        w2(&mut buf[d + 6..], (cd - d - 1) as u32);
        d = cd;
    }

    if !p.sources.is_empty() {
        w2(&mut buf[230..], d as u32);
        let mut j = 1;
        for s in &p.sources {
            for c in &s.cells {
                if c.scellid == c.ecellid {
                    continue;
                }
                if c.kind != CellKind::Neither {
                    buf[d] = j as u8;
                    d += 1;
                }
                j += c.ecellid - c.scellid;
            }
        }
        d += d & 1;

        w2(&mut buf[232..], d as u32);
        let mut j: i32 = -1;
        let mut notseamless = true;
        for s in &p.sources {
            let vob = &va.vobs[s.vob];
            for c in &s.cells {
                for l in c.scellid..c.ecellid {
                    let id = vob.vobid * 256 + l;
                    for m in 0..va.numaudiotracks {
                        if j >= 0 && va.audch_of(m) >= 0 && va.audio_gap(j, id, va.audch_of(m) as usize) {
                            notseamless = true;
                        }
                    }
                    buf[d] = (if notseamless { 0 } else { 8 }) | if j + 1 != id { 2 } else { 0 };
                    notseamless = false;
                    j = id;

                    let mut vi = vob.find_cell_vobu(l) as usize;
                    let firsttime = vob.vobu[vi].sectpts[0] as i32 as mux::Pts;
                    w4(&mut buf[8 + d..], vob.vobu[vi].sector as u32);
                    vi = (vob.find_cell_vobu(l + 1) - 1) as usize;
                    if l == c.ecellid - 1 && c.pauselen > 0 {
                        buf[2 + d] = c.pauselen;
                        notseamless = true;
                    }
                    w4(&mut buf[4 + d..], va.vd.bcd_time(vob.vobu[vi].sectpts[1] - firsttime));
                    w4(&mut buf[16 + d..], vob.vobu[vi].sector as u32);
                    w4(&mut buf[20 + d..], vob.vobu[vi].lastsector as u32);
                    d += 24;
                }
            }
        }

        w2(&mut buf[234..], d as u32);
        for s in &p.sources {
            let vob = &va.vobs[s.vob];
            for c in &s.cells {
                for k in c.scellid..c.ecellid {
                    buf[1 + d] = vob.vobid as u8;
                    buf[3 + d] = k as u8;
                    d += 4;
                }
            }
        }
    }
    Ok(d)
}

/// A PGC that only links to PGC `pgc` (entry point of a menu with additional entry types).
fn jumppgc(buf: &mut [u8], pgc: usize) -> usize {
    let base = 0xEC;
    let mut offs = base + 8;
    if pgc > 0 {
        buf[offs..offs + 8].copy_from_slice(&[0x20, 0x04, 0, 0, 0, 0, 0, pgc as u8]);
    } else {
        buf[offs..offs + 8].copy_from_slice(&[0x30, 0x06, 0, 0, 0, 0, 0, 0]);
    }
    offs += 8;
    let ncmd = (offs - base) / 8 - 1;
    buf[0xE5] = base as u8;
    buf[base + 1] = ncmd as u8;
    w2(&mut buf[base + 6..], (7 + 8 * ncmd) as u32);
    offs
}

/// VMGM_LU / VTSM_LU / VTS_PGCI table with all PGCs.
fn createpgcgroup(ws: &Workset, set: &PgcSet) -> Result<Vec<u8>> {
    let is_vts = set.kind == Domain::Vts;
    let mut len = set.pgcs.len() + set.numentries as usize;
    for p in &set.pgcs {
        if p.entries != 0 {
            len -= 1;
        }
    }
    let mut out = vec![0u8; len * 8 + 8];
    w2(&mut out, len as u32);
    let mut pgcidx = 8;
    let mut nrtitles = 0;
    for (i, p) in set.pgcs.iter().enumerate() {
        if is_vts {
            if p.entries == 0 {
                nrtitles += 1;
                out[pgcidx] = 0x80 | nrtitles as u8;
            }
        } else {
            out[pgcidx] = 0;
            if let Some(j) = (2..8).find(|j| p.entries & (1 << j) != 0) {
                out[pgcidx] = 0x80 | j as u8;
            }
        }
        let at = out.len();
        w4(&mut out[pgcidx + 4..], at as u32);
        let mut buf = vec![0u8; MAX_PGC_SIZE + 1024];
        let n = genpgc(&mut buf, ws, set, i)?;
        out.extend_from_slice(&buf[..n]);
        pgcidx += 8;
    }
    for i in 2..8u32 {
        if set.allentries & (1 << i) == 0 {
            continue;
        }
        let mut j = set.pgcs.iter().position(|p| p.entries & (1 << i) != 0).unwrap_or(set.pgcs.len());
        if j < set.pgcs.len() && set.pgcs[j].entries & ((1 << i) - 1) == 0 {
            continue;
        }
        j += 1;
        out[pgcidx] = 0x80 | i as u8;
        let at = out.len();
        w4(&mut out[pgcidx + 4..], at as u32);
        let mut buf = vec![0u8; 0x200];
        let n = jumppgc(&mut buf, j);
        out.extend_from_slice(&buf[..n]);
        pgcidx += 8;
    }
    let end = out.len() as u32 - 1;
    w4(&mut out[4..], end);
    Ok(out)
}

/// VMGM_PGCI_UT / VTSM_PGCI_UT (menus) or VTS_PGCI (titles), padded to whole sectors.
pub fn create_pgc(ws: &Workset, set: &PgcSet) -> Result<Vec<u8>> {
    let mut out;
    if set.kind != Domain::Vts {
        // One language unit ("en"); `numgroups` is 1 whenever the domain has menus.
        out = vec![0u8; 16];
        out[1] = 1;
        let ph = 16;
        out[8..10].copy_from_slice(b"en");
        out[11] = if set.kind == Domain::Vtsm { set.allentries as u8 } else { 0x80 };
        w4(&mut out[12..], ph as u32);
        out.extend(createpgcgroup(ws, set)?);
        let end = out.len() as u32 - 1;
        w4(&mut out[4..], end);
    } else {
        out = createpgcgroup(ws, set)?;
    }
    out.resize(out.len().div_ceil(2048) * 2048, 0);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

fn secunit(ns: i64) -> i64 {
    if ns == 0 {
        return 1;
    }
    (ns + 2040 - 1) / 2040
}

fn pgc_duration_seconds(titles: &PgcSet, c: usize) -> i64 {
    (titles.pgc_pts_span(c) - 1) / titles.vg.vd.ratedenom()
}

fn tmapt_block_size(titles: &PgcSet, pgc: usize) -> usize {
    let mut v = pgc_duration_seconds(titles, pgc);
    v /= secunit(v);
    (v * 4 + 4) as usize
}

fn size_tmapt(titles: &PgcSet) -> usize {
    (0..titles.pgcs.len()).map(|i| tmapt_block_size(titles, i)).sum::<usize>() + titles.pgcs.len() * 4 + 8
}

fn numsect_tmapt(titles: &PgcSet) -> usize {
    size_tmapt(titles).div_ceil(2048)
}

/// `globalfindvobu`: (sector, cell id) of the VOBU of PGC `ch` spanning `pts`.
fn global_find_vobu(set: &PgcSet, ch: usize, mut pts: i64) -> Option<(i32, i32)> {
    for s in &set.pgcs[ch].sources {
        let vob = &set.vg.vobs[s.vob];
        for c in &s.cells {
            let fv = vob.find_cell_vobu(c.scellid);
            if pts < 0 {
                let v = &vob.vobu[fv as usize];
                return Some((v.sector, v.vobcellid));
            }
            let mut span: i64 = 0;
            for ci in c.scellid..c.ecellid {
                span += vob.cell_pts(ci);
            }
            // `span` is an `int` in dvdauthor.
            let span = span as i32 as i64;
            if pts < span {
                let r = vob.find_vobu(pts + vob.vobu[fv as usize].sectpts[0], fv, vob.vobu.len() as i32 - 1);
                let v = &vob.vobu[r as usize];
                return Some((v.sector, v.vobcellid));
            }
            pts -= span;
        }
    }
    None
}

/// VTS_TMAPTI: time maps of all titles.
fn create_tmapt(titles: &PgcSet) -> Vec<u8> {
    let n = titles.pgcs.len();
    let mut out = vec![0u8; 8];
    w2(&mut out, n as u32);
    w4(&mut out[4..], (size_tmapt(titles) - 1) as u32);
    let mut mapblock = 8 + 4 * n;
    for i in 0..n {
        out.extend_from_slice(&(mapblock as u32).to_be_bytes());
        mapblock += tmapt_block_size(titles, i);
    }
    let denom = titles.vg.vd.ratedenom();
    for i in 0..n {
        let mut numtmapt = pgc_duration_seconds(titles, i);
        let units = secunit(numtmapt);
        numtmapt /= units;
        out.extend_from_slice(&[units as u8, 0, (numtmapt >> 8) as u8, numtmapt as u8]);
        if numtmapt > 0 {
            let mut vobu1 = global_find_vobu(titles, i, units * denom);
            for j in 0..numtmapt {
                let vobu2 = global_find_vobu(titles, i, (j + 2) * units * denom);
                let (sector, cell1) = vobu1.expect("time map entry inside the title");
                let mut e = sector as u32;
                if vobu2.map_or(true, |(_, c2)| c2 != cell1) {
                    e |= 0x8000_0000;
                }
                out.extend_from_slice(&e.to_be_bytes());
                vobu1 = vobu2;
            }
        }
    }
    let pad = (-(size_tmapt(titles) as i64)) & 2047;
    out.extend(std::iter::repeat(0u8).take(pad as usize));
    out
}

fn numsect_vobuad(vg: &VobGroup) -> i32 {
    (4 + mux::num_vobus(vg) as i32 * 4 + 2047) / 2048
}

/// VMGM_C_ADT / VTSM_C_ADT / VTS_C_ADT: addresses of all cells.
fn create_cell_address_table(vg: &VobGroup) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    let mut p = 8;
    let put = |buf: &mut Vec<u8>, at: usize, bytes: &[u8]| {
        if buf.len() < at + bytes.len() {
            buf.resize(at + bytes.len(), 0);
        }
        buf[at..at + bytes.len()].copy_from_slice(bytes);
    };
    for vob in &vg.vobs {
        let mut i = 0;
        while i < vob.vobu.len() {
            if i == 0 || vob.vobu[i].vobcellid != vob.vobu[i - 1].vobcellid {
                if i != 0 {
                    put(&mut buf, p + 8, &(vob.vobu[i - 1].lastsector as u32).to_be_bytes());
                    p += 12;
                }
                put(&mut buf, p, &((vob.vobu[i].vobcellid >> 8) as u16).to_be_bytes());
                put(&mut buf, p + 2, &[vob.vobu[i].vobcellid as u8]);
                put(&mut buf, p + 4, &(vob.vobu[i].sector as u32).to_be_bytes());
            }
            i += 1;
        }
        put(&mut buf, p + 8, &(vob.vobu[i - 1].lastsector as u32).to_be_bytes());
        p += 12;
    }
    w4(&mut buf[4..], (p - 1) as u32);
    w2(&mut buf, vg.vobs.len() as u32);
    buf.resize(p.div_ceil(2048) * 2048, 0);
    buf
}

fn create_vobuad(vg: &VobGroup) -> Vec<u8> {
    let nv = mux::num_vobus(vg);
    let mut out = Vec::with_capacity(nv * 4 + 4);
    out.extend_from_slice(&((nv * 4 + 3) as u32).to_be_bytes());
    for vob in &vg.vobs {
        for v in &vob.vobu {
            out.extend_from_slice(&(v.sector as u32).to_be_bytes());
        }
    }
    let pad = (-(4 + nv as i64 * 4)) & 2047;
    out.extend(std::iter::repeat(0u8).take(pad as usize));
    out
}

/// VTS_PTT_SRPT with its VTS_PTT entries (chapters of every title).
fn create_ptt_srpt(t: &PgcSet) -> Vec<u8> {
    let mut b = vec![0u8; 2048];
    w2(&mut b, t.pgcs.len() as u32);
    let mut p = 8 + t.pgcs.len() * 4;
    assert!(p <= 2048, "title pointers must fit in the first sector");
    let put1 = |b: &mut Vec<u8>, at: usize, v: u8| {
        if b.len() <= at {
            b.resize(at + 1, 0);
        }
        b[at] = v;
    };
    for (j, pgc) in t.pgcs.iter().enumerate() {
        let mut pgm = 1u8;
        w4(&mut b[8 + j * 4..], p as u32);
        for s in &pgc.sources {
            for c in &s.cells {
                if c.scellid == c.ecellid {
                    continue;
                }
                match c.kind {
                    CellKind::ChapterProgram => {
                        put1(&mut b, 1 + p, (j + 1) as u8);
                        put1(&mut b, 3 + p, pgm);
                        p += 4;
                        pgm += 1;
                    }
                    CellKind::Program => pgm += 1,
                    CellKind::Neither => {}
                }
            }
        }
    }
    w4(&mut b[4..], (p - 1) as u32);
    b.resize(p.div_ceil(2048).max(1) * 2048, 0);
    b
}

/// Video/audio/subpicture attribute block (`BuildAVInfo`).
fn build_av_info(buf: &mut [u8], va: &VobGroup) {
    let widescreen_bits = [0u32, 0x100, 0x200, 2];
    let vd = &va.vd;
    let v = (if vd.vmpeg == 2 { 0x4000 } else { 0 })
        | widescreen_bits[vd.vwidescreen as usize]
        | (if vd.vformat == VF_PAL { 0x1000 } else { 0 })
        | (if vd.vaspect == VA_16X9 { 0xc00 } else { 0x300 })
        | (if vd.vcaption & 1 != 0 { 0x80 } else { 0 })
        | (if vd.vcaption & 2 != 0 { 0x40 } else { 0 })
        | (((vd.vres - 1) as u32) << 3);
    w2(buf, v);
    buf[3] = va.numaudiotracks as u8;
    for i in 0..va.numaudiotracks {
        let a = &va.ad[i];
        buf[4 + i * 8] = ((a.aformat - 1) << 6) as u8;
        if a.alangpresent == AL_LANG {
            buf[4 + i * 8] |= 4;
            buf[6 + i * 8..8 + i * 8].copy_from_slice(&a.lang);
        }
        if a.adolby == AD_SURROUND {
            buf[4 + i * 8] |= 2;
            buf[11 + i * 8] = 8;
        }
        buf[5 + i * 8] = (((a.aquant - 1) << 6) | ((a.asample - 1) << 4) | (a.achannels - 1)) as u8;
        buf[9 + i * 8] = a.acontent as u8;
    }
    buf[0x55] = va.numsubpicturetracks as u8;
    for i in 0..va.numsubpicturetracks {
        let s = &va.sp[i];
        if s.slangpresent == AL_LANG {
            buf[0x56 + i * 6] = 1;
            buf[0x58 + i * 6..0x5a + i * 6].copy_from_slice(&s.lang);
        }
        buf[0x56 + i * 6 + 5] = s.scontent as u8;
    }
}

fn needmenus(menus: &PgcSet) -> bool {
    !menus.pgcs.is_empty()
}

// ---------------------------------------------------------------------------
// IFO files
// ---------------------------------------------------------------------------

/// `VTS_nn_0.IFO` (identical to the BUP) of a titleset.
pub fn write_vts_ifo(menus: &PgcSet, titles: &PgcSet) -> Result<Vec<u8>> {
    let ws = Workset { menus, titles: Some(titles) };
    let forcemenus = needmenus(menus);
    let mut buf = vec![0u8; 2048];
    buf[..12].copy_from_slice(b"DVDVIDEO-VTS");
    buf[33] = 0x11;
    w4(&mut buf[128..], 0x7ff);
    let mut nextsector: i32 = 1;

    let ptt = create_ptt_srpt(titles);
    w4(&mut buf[0xC8..], nextsector as u32);
    nextsector += (ptt.len() / 2048) as i32;

    let vts_pgc = create_pgc(&ws, titles)?;
    w4(&mut buf[0xCC..], nextsector as u32);
    nextsector += (vts_pgc.len() / 2048) as i32;

    let vtsm_pgc = if forcemenus {
        let p = create_pgc(&ws, menus)?;
        w4(&mut buf[0xD0..], nextsector as u32);
        nextsector += (p.len() / 2048) as i32;
        Some(p)
    } else {
        None
    };

    w4(&mut buf[0xD4..], nextsector as u32);
    nextsector += numsect_tmapt(titles) as i32;

    let (menu_adt, menu_admap) = if forcemenus {
        let adt = create_cell_address_table(&menus.vg);
        w4(&mut buf[0xD8..], nextsector as u32);
        nextsector += (adt.len() / 2048) as i32;
        w4(&mut buf[0xDC..], nextsector as u32);
        nextsector += numsect_vobuad(&menus.vg);
        (Some(adt), Some(create_vobuad(&menus.vg)))
    } else {
        (None, None)
    };

    let title_adt = create_cell_address_table(&titles.vg);
    w4(&mut buf[0xE0..], nextsector as u32);
    nextsector += (title_adt.len() / 2048) as i32;
    w4(&mut buf[0xE4..], nextsector as u32);
    nextsector += numsect_vobuad(&titles.vg);

    w4(&mut buf[28..], (nextsector - 1) as u32);
    if forcemenus {
        w4(&mut buf[0xC0..], nextsector as u32);
        nextsector += mux::voblen(&menus.vg);
    }
    w4(&mut buf[0xC4..], nextsector as u32);
    if !titles.pgcs.is_empty() {
        nextsector += mux::voblen(&titles.vg);
    }
    nextsector += r4(&buf[28..]) as i32;
    if nextsector < MIN_IFO_SECTORS - 1 {
        nextsector = MIN_IFO_SECTORS - 1;
    }
    w4(&mut buf[12..], nextsector as u32);

    if forcemenus {
        build_av_info(&mut buf[256..], &menus.vg);
    }
    build_av_info(&mut buf[512..], &titles.vg);

    let mut out = buf;
    out.extend(ptt);
    out.extend(vts_pgc);
    if let Some(p) = vtsm_pgc {
        out.extend(p);
    }
    out.extend(create_tmapt(titles));
    if let (Some(a), Some(m)) = (menu_adt, menu_admap) {
        out.extend(a);
        out.extend(m);
    }
    out.extend(title_adt);
    out.extend(create_vobuad(&titles.vg));
    Ok(out)
}

/// What the video manager needs to know about a finished titleset (`toc_summary`).
#[derive(Debug, Clone)]
pub struct VtsDef {
    pub hasmenu: bool,
    pub numsectors: i32,
    pub vtscat: [u8; 4],
    pub vtssummary: Vec<u8>,
    pub numchapters: Vec<i32>,
}

/// Reads the summary back from a written `VTS_nn_0.IFO` (`ScanIfo`).
pub fn scan_ifo(ifo: &[u8]) -> VtsDef {
    let numtitles = r2(&ifo[2048..]) as usize;
    let mut numchapters = Vec::with_capacity(numtitles);
    let mut first = 8 + numtitles * 4;
    for i in 0..numtitles.saturating_sub(1) {
        let n = r4(&ifo[2048 + 12 + i * 4..]) as usize;
        numchapters.push(((n - first) / 4) as i32);
        first = n;
    }
    if numtitles > 0 {
        numchapters.push(((r4(&ifo[2048 + 4..]) as usize + 1 - first) / 4) as i32);
    }
    VtsDef {
        hasmenu: r4(&ifo[0xc0..]) != 0,
        numsectors: r4(&ifo[0xc..]) as i32 + 1,
        vtscat: ifo[0x22..0x26].try_into().expect("4 bytes"),
        vtssummary: ifo[0x100..0x400].to_vec(),
        numchapters,
    }
}

fn create_tt_srpt(ts: &[VtsDef], vtsstart: i32) -> Vec<u8> {
    let mut b = vec![0u8; 2048];
    let mut j = vtsstart;
    let mut tn = 0;
    let mut p = 8;
    for (i, v) in ts.iter().enumerate() {
        for (k, &ch) in v.numchapters.iter().enumerate() {
            if b.len() < p + 12 {
                b.resize(p + 12, 0);
            }
            b[p] = 0x3c;
            b[p + 1] = 1;
            w2(&mut b[p + 2..], ch as u32);
            b[p + 6] = (i + 1) as u8;
            b[p + 7] = (k + 1) as u8;
            w4(&mut b[p + 8..], j as u32);
            tn += 1;
            p += 12;
        }
        j += v.numsectors;
    }
    w2(&mut b, tn);
    w4(&mut b[4..], (p - 1) as u32);
    b.resize(p.div_ceil(2048) * 2048, 0);
    b
}

/// `VIDEO_TS.IFO` (identical to the BUP) of the video manager. `fpc` is the first-play program.
pub fn write_vmg_ifo(menus: &PgcSet, fpc: Option<&dvdvm::Program>, titlesets: &[VtsDef]) -> Result<Vec<u8>> {
    let ws = Workset { menus, titles: None };
    let forcemenus = needmenus(menus);
    let mut buf = vec![0u8; 2048];
    buf[..12].copy_from_slice(b"DVDVIDEO-VMG");
    buf[0x21] = 0x11;
    buf[0x27] = 1;
    buf[0x29] = 1;
    buf[0x2a] = 1;
    w2(&mut buf[0x3e..], titlesets.len() as u32);
    buf[0x40..0x40 + PROVIDER.len()].copy_from_slice(PROVIDER);
    buf[0x86] = 4;
    let mut nextsector: i32 = 1;

    w4(&mut buf[0xc4..], nextsector as u32);
    let tt = create_tt_srpt(titlesets, 0);
    nextsector += (tt.len() / 2048) as i32;

    let pgc = if forcemenus {
        let p = create_pgc(&ws, menus)?;
        w4(&mut buf[0xc8..], nextsector as u32);
        nextsector += (p.len() / 2048) as i32;
        Some(p)
    } else {
        None
    };

    w4(&mut buf[0xd0..], nextsector as u32);
    nextsector += (8 + titlesets.len() as i32 * 0x30c + 2047) / 2048;

    let (adt, admap) = if forcemenus {
        let adt = create_cell_address_table(&menus.vg);
        w4(&mut buf[0xd8..], nextsector as u32);
        nextsector += (adt.len() / 2048) as i32;
        w4(&mut buf[0xdc..], nextsector as u32);
        nextsector += numsect_vobuad(&menus.vg);
        (Some(adt), Some(create_vobuad(&menus.vg)))
    } else {
        (None, None)
    };

    w4(&mut buf[0x1c..], (nextsector - 1) as u32);
    let mut vtsstart = nextsector * 2;
    if forcemenus {
        w4(&mut buf[0xc0..], nextsector as u32);
        vtsstart += mux::voblen(&menus.vg);
    }
    if vtsstart < 2 * MIN_IFO_SECTORS {
        vtsstart = 2 * MIN_IFO_SECTORS;
    }
    w4(&mut buf[0xc..], (vtsstart - 1) as u32);

    if forcemenus {
        build_av_info(&mut buf[256..], &menus.vg);
    }

    buf[0x407] = (if menus.vg.vd.ratedenom() == 90090 { 3 } else { 1 }) << 6;
    buf[0x4e5] = 0xec;
    let mut offset = 0x4f4;
    let ninstr;
    if let Some(fpc) = fpc {
        let ctx = Context { group_entries: Some(menus.pgcs.iter().map(|p| p.entries).collect()), all_menu_entries: vec![], pgc: None, titles: None };
        let end = dvdvm::compile(&mut buf, offset, offset, fpc, &ctx, Domain::Vmgm).map_err(|e| MuxError(format!("{e}\nERR:  in FPC")))?;
        ninstr = (end - offset) / 8;
        if ninstr > 128 {
            return err("ERR:  FPC has more than 128 commands");
        }
    } else {
        let ins: [u8; 8] = if forcemenus {
            [0x30, 0x06, 0, 0, 0, 0x42, 0, 0]
        } else if titlesets.first().is_some_and(|v| v.hasmenu) {
            [0x30, 0x06, 0, 1, 1, 0x83, 0, 0]
        } else {
            [0x30, 0x02, 0, 0, 0, 0x01, 0, 0]
        };
        buf[offset..offset + 8].copy_from_slice(&ins);
        ninstr = 1;
    }
    buf[0x4ed] = ninstr as u8;
    w2(&mut buf[0x4f2..], 7 + ninstr as u32 * 8);
    let e = 0x4ec + r2(&buf[0x4f2..]);
    w2(&mut buf[0x82..], e);
    offset = 0;
    let _ = offset;
    buf.truncate(2048);
    let mut out = buf;

    out.extend(create_tt_srpt(titlesets, vtsstart));
    if let Some(p) = pgc {
        out.extend(p);
    }
    // VMG_VTS_ATRT
    let mut a = vec![0u8; 2048];
    let n = titlesets.len();
    let mut j = 8 + n * 4;
    w2(&mut a, n as u32);
    w4(&mut a[4..], (n * 0x30c + 8 - 1) as u32);
    for i in 0..n {
        w4(&mut a[8 + i * 4..], (j + i * 0x308) as u32);
    }
    out.extend_from_slice(&a[..j]);
    for v in titlesets {
        let mut t = vec![0u8; 0x308];
        w4(&mut t, 0x307);
        t[4..8].copy_from_slice(&v.vtscat);
        t[8..8 + 0x300].copy_from_slice(&v.vtssummary);
        out.extend(t);
        j += 0x308;
    }
    let pad = 2048 - (j & 2047);
    if pad < 2048 {
        out.extend(std::iter::repeat(0u8).take(pad));
    }
    if forcemenus {
        out.extend(adt.expect("menu address table"));
        out.extend(admap.expect("menu VOBU map"));
    }
    let _ = VW_CROP + VW_NOLETTERBOX + VW_NOPANSCAN + VF_PAL;
    Ok(out)
}
