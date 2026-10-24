//! Synthetic disc content for the burner tests.

use std::path::PathBuf;

use super::udf::SECTOR;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cx_burner_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("VIDEO_TS")).unwrap();
    dir
}

/// A folder with a `VIDEO_TS` directory of small files with pseudo-random content.
pub fn synthetic_video_ts(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let fill = |name: &str, len: usize, seed: u8| {
        let data: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect();
        std::fs::write(dir.join("VIDEO_TS").join(name), data).unwrap();
    };
    fill("VIDEO_TS.IFO", 3000, 1);
    fill("VIDEO_TS.BUP", 3000, 1);
    fill("VIDEO_TS.VOB", 5000, 2);
    fill("VTS_01_0.IFO", 4100, 3);
    fill("VTS_01_0.BUP", 4100, 3);
    fill("VTS_01_1.VOB", 20_000, 4);
    fill("VTS_01_2.VOB", 2048, 5);
    dir
}

/// A cell of a synthetic titleset; sectors are relative to the start of the title VOBs.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub first: u32,
    pub last: u32,
    /// First byte of the cell playback entry (bit 3 seamless, bit 2 interleaved).
    pub flags: u8,
}

fn put_be32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// IFO of one titleset: sector 0 header, sector 1 PGCIT with one PGC (title number `ttn`),
/// sector 2 cell address table. The title VOBs start right behind the three IFO sectors.
fn vts_ifo(ttn: u8, cells: &[Cell]) -> Vec<u8> {
    let s = SECTOR as usize;
    let mut ifo = vec![0u8; 3 * s];
    ifo[..12].copy_from_slice(b"DVDVIDEO-VTS");
    put_be32(&mut ifo, 0xC4, 3); // VTS_VOBS
    put_be32(&mut ifo, 0xCC, 1); // VTS_PGCIT
    put_be32(&mut ifo, 0xE0, 2); // VTS_C_ADT
    // PGCIT: one search pointer, PGC right behind it.
    ifo[s..s + 2].copy_from_slice(&1u16.to_be_bytes());
    put_be32(&mut ifo, s + 8, ((0x80 | ttn as u32) << 24) | 0x0001);
    put_be32(&mut ifo, s + 12, 16);
    let pgc = s + 16;
    ifo[pgc + 2] = 1; // programs
    ifo[pgc + 3] = cells.len() as u8;
    ifo[pgc + 0xE8..pgc + 0xEA].copy_from_slice(&0xECu16.to_be_bytes());
    for (i, c) in cells.iter().enumerate() {
        let e = pgc + 0xEC + 24 * i;
        ifo[e] = c.flags;
        put_be32(&mut ifo, e + 8, c.first);
        put_be32(&mut ifo, e + 16, c.last);
        put_be32(&mut ifo, e + 20, c.last);
    }
    // C_ADT: 12 bytes per cell.
    let adt = 2 * s;
    ifo[adt..adt + 2].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    put_be32(&mut ifo, adt + 4, 7 + 12 * cells.len() as u32);
    for (i, c) in cells.iter().enumerate() {
        let e = adt + 8 + 12 * i;
        ifo[e..e + 2].copy_from_slice(&1u16.to_be_bytes());
        ifo[e + 2] = i as u8 + 1;
        put_be32(&mut ifo, e + 4, c.first);
        put_be32(&mut ifo, e + 8, c.last);
    }
    ifo
}

/// A folder with a complete VIDEO_TS: a VMG with a TT_SRPT and one titleset per entry of
/// `titlesets` (title number, cells). The title VOB of each titleset is zero-filled up to its
/// last cell sector.
pub fn synthetic_dvd(tag: &str, titlesets: &[(u8, Vec<Cell>)]) -> PathBuf {
    let dir = temp_dir(tag);
    let video_ts = dir.join("VIDEO_TS");
    let s = SECTOR as usize;
    let mut vmg = vec![0u8; 2 * s];
    vmg[..12].copy_from_slice(b"DVDVIDEO-VMG");
    put_be32(&mut vmg, 0xC4, 1);
    vmg[s..s + 2].copy_from_slice(&(titlesets.len() as u16).to_be_bytes());
    for (i, (ttn, _)) in titlesets.iter().enumerate() {
        let e = s + 8 + 12 * i;
        vmg[e + 6] = i as u8 + 1;
        vmg[e + 7] = *ttn;
    }
    std::fs::write(video_ts.join("VIDEO_TS.IFO"), &vmg).unwrap();
    std::fs::write(video_ts.join("VIDEO_TS.BUP"), &vmg).unwrap();
    for (i, (ttn, cells)) in titlesets.iter().enumerate() {
        let n = i + 1;
        let ifo = vts_ifo(*ttn, cells);
        std::fs::write(video_ts.join(format!("VTS_{n:02}_0.IFO")), &ifo).unwrap();
        std::fs::write(video_ts.join(format!("VTS_{n:02}_0.BUP")), &ifo).unwrap();
        let sectors = cells.iter().map(|c| c.last + 1).max().unwrap_or(1) as usize;
        std::fs::write(video_ts.join(format!("VTS_{n:02}_1.VOB")), vec![0u8; sectors * s]).unwrap();
    }
    dir
}
