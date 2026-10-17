//! Turns a tsMuxeR menu clip with a *disguised* PGS track into a clip with a real Interactive
//! Graphics stream: tsMuxeR only copies PGS segment types, so the ICS goes in as a PCS (0x16) on
//! PID 0x1200 and is relabelled here (segment type 0x18, PID 0x1400, stream type 0x91) in the
//! m2ts, its CLPI and its MPLS. Depends on tsMuxeR 2.7.0's output layout (one PES per segment,
//! PMT on PID 0x100, one PlayItem).

const PACKET: usize = 192;
const PG_PID: u16 = 0x1200;
const IG_PID: u16 = 0x1400;
const PMT_PID: u16 = 0x0100;

#[derive(Debug)]
pub struct PatchError(pub &'static str);

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Blu-ray menu patch: {}", self.0)
    }
}

impl std::error::Error for PatchError {}

type R = Result<(), PatchError>;

fn be16(d: &[u8], at: usize) -> usize {
    u16::from_be_bytes([d[at], d[at + 1]]) as usize
}
fn be32(d: &[u8], at: usize) -> usize {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]) as usize
}

/// CRC-32/MPEG-2 (PMT sections).
pub fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { crc << 1 ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// Rewrites the PG entry of one PMT section (`sec` starts at the table id).
fn patch_pmt(sec: &mut [u8]) -> R {
    if sec.len() < 12 || sec[0] != 2 {
        return Err(PatchError("PMT is not a PMT section"));
    }
    let end = 3 + (be16(sec, 1) & 0xFFF);
    if end > sec.len() {
        return Err(PatchError("PMT section crosses a packet"));
    }
    let mut i = 12 + (be16(sec, 10) & 0xFFF);
    while i + 5 <= end - 4 {
        if sec[i] == 0x90 && be16(sec, i + 1) & 0x1FFF == PG_PID as usize {
            sec[i] = 0x91;
            sec[i + 1] = 0xE0 | (IG_PID >> 8) as u8;
            sec[i + 2] = IG_PID as u8;
        }
        i += 5 + (be16(sec, i + 3) & 0xFFF);
    }
    let crc = crc32_mpeg(&sec[..end - 4]);
    sec[end - 4..end].copy_from_slice(&crc.to_be_bytes());
    Ok(())
}

/// `.m2ts`: PID 0x1200 → 0x1400 in every packet, ICS PCS → ICS, PMT stream type.
pub fn patch_m2ts(d: &mut [u8]) -> R {
    if d.len() % PACKET != 0 {
        return Err(PatchError("m2ts size is not a multiple of 192"));
    }
    for pkt in d.chunks_exact_mut(PACKET) {
        let p = &mut pkt[4..];
        let pid = (be16(p, 1) & 0x1FFF) as u16;
        let pusi = p[1] & 0x40 != 0;
        let off = if p[3] & 0x20 != 0 { 5 + p[4] as usize } else { 4 };
        if pid == PMT_PID && pusi {
            let at = off + 1 + p[off] as usize; // pointer field
            patch_pmt(&mut p[at..])?;
        } else if pid == PG_PID {
            p[1] = p[1] & 0xE0 | (IG_PID >> 8) as u8;
            p[2] = IG_PID as u8;
            if pusi {
                // PES header: 00 00 01 bd len(2) flags(2) header_len, then the first segment.
                let seg = off + 9 + p[off + 8] as usize;
                if p.get(off..off + 4) != Some(&[0, 0, 1, 0xBD]) || seg >= p.len() {
                    return Err(PatchError("unexpected PES layout"));
                }
                if p[seg] == 0x16 {
                    p[seg] = 0x18;
                }
            }
        }
    }
    Ok(())
}

/// Stream entries of the CLPI program info: PG 0x1200 / 0x90 → IG 0x1400 / 0x91.
pub fn patch_clpi(d: &mut [u8]) -> R {
    let ps = be32(d, 12);
    let mut at = ps + 5;
    let seqs = d.get(ps + 5).copied().ok_or(PatchError("CLPI too short"))?;
    at += 1;
    let mut found = false;
    for _ in 0..seqs {
        let streams = *d.get(at + 6).ok_or(PatchError("CLPI too short"))?;
        at += 8;
        for _ in 0..streams {
            let len = *d.get(at + 2).ok_or(PatchError("CLPI too short"))? as usize;
            if be16(d, at) == PG_PID as usize && d[at + 3] == 0x90 {
                d[at..at + 2].copy_from_slice(&IG_PID.to_be_bytes());
                d[at + 3] = 0x91;
                found = true;
            }
            at += 3 + len;
        }
    }
    if found { Ok(()) } else { Err(PatchError("CLPI has no PG stream")) }
}

/// Playlist of one menu clip: PG entry of the STN table → IG entry (`ig`); `still` makes the single
/// PlayItem wait forever at its end.
pub fn patch_mpls(d: &mut [u8], still: bool, ig: bool) -> R {
    let item = be32(d, 8) + 10;
    if d.len() < item + 60 || be16(d, be32(d, 8) + 6) != 1 {
        return Err(PatchError("playlist does not have exactly one PlayItem"));
    }
    if still {
        d[item + 31] = 1;
        d[item + 32..item + 34].fill(0);
    }
    if !ig {
        return Ok(());
    }
    let stn = item + 34;
    let (nv, na, npg, nig) = (d[stn + 4] as usize, d[stn + 5] as usize, d[stn + 6], d[stn + 7]);
    if npg != 1 || nig != 0 || d[stn + 8..stn + 11] != [0; 3] {
        return Err(PatchError("STN table: expected exactly one PG stream"));
    }
    let mut at = stn + 16;
    for k in 0..nv + na + 1 {
        let (elen, alen) = (d[at] as usize, d[at + 1 + d[at] as usize] as usize);
        if k == nv + na {
            if be16(d, at + 2) != PG_PID as usize || d[at + elen + 2] != 0x90 {
                return Err(PatchError("STN table: PG entry not where expected"));
            }
            d[at + 2..at + 4].copy_from_slice(&IG_PID.to_be_bytes());
            d[at + elen + 2] = 0x91;
        }
        at += elen + alen + 2;
    }
    d[stn + 6] = 0;
    d[stn + 7] = 1;
    Ok(())
}

/// Patches clip `clip` / playlist `pl` (5-digit names) of the `BDMV` folder, and their BACKUP
/// copies: `ig` = the clip carries a disguised IG track, `still` = the menu waits forever.
pub fn patch_menu(bdmv: &std::path::Path, clip: &str, pl: &str, still: bool, ig: bool) -> std::io::Result<()> {
    let io = |e: PatchError| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    if ig {
        let m2ts = bdmv.join(format!("STREAM/{clip}.m2ts"));
        let mut d = std::fs::read(&m2ts)?;
        patch_m2ts(&mut d).map_err(io)?;
        std::fs::write(&m2ts, d)?;
    }
    for dir in [bdmv.to_path_buf(), bdmv.join("BACKUP")] {
        if ig {
            let p = dir.join(format!("CLIPINF/{clip}.clpi"));
            let mut d = std::fs::read(&p)?;
            patch_clpi(&mut d).map_err(io)?;
            std::fs::write(&p, d)?;
        }
        let p = dir.join(format!("PLAYLIST/{pl}.mpls"));
        let mut d = std::fs::read(&p)?;
        patch_mpls(&mut d, still, ig).map_err(io)?;
        std::fs::write(&p, d)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Files written by tsMuxeR 2.7.0 for a menu clip (video, AC-3, one PGS track).
    const MPLS: &str = "4d504c53303230300000003a000000a60000000000000000000000000000000000000000000000000000000e000100000000000000000000400000000068000000010000006030303030304d325453000100019bfcc0019d5caa000000000000000000000000003e000001010100000000000000000009011011000000000000051b5100000009011100000000000000058131656e67090112000000000000000590656e670000000010000100010000019bfcc0ffff00000000";

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn mpls_pg_becomes_ig() {
        let mut d = unhex(MPLS);
        patch_mpls(&mut d, true, true).unwrap();
        let o = unhex(MPLS);
        let diff: Vec<usize> = (0..d.len()).filter(|&i| d[i] != o[i]).collect();
        // still mode, IG/PG counts, PID, coding type
        assert_eq!(diff, vec![0x63, 0x6c, 0x6d, 0x98, 0xa1]);
        assert_eq!((d[0x63], d[0x6c], d[0x6d], d[0x98], d[0xa1]), (1, 0, 1, 0x14, 0x91));
        assert!(patch_mpls(&mut d, false, true).is_err()); // no PG left
    }

    #[test]
    fn clpi_stream_entry() {
        let mut d = vec![0u8; 0x100];
        d[12..16].copy_from_slice(&0x40u32.to_be_bytes());
        let ps = 0x40;
        d[ps + 5] = 1; // one sequence
        d[ps + 6 + 6] = 2; // two streams
        let at = ps + 6 + 8;
        d[at..at + 3].copy_from_slice(&[0x10, 0x11, 3]); // video, 3 bytes of coding info
        d[at + 3] = 0x1B;
        let pg = at + 6;
        d[pg..pg + 4].copy_from_slice(&[0x12, 0, 5, 0x90]);
        patch_clpi(&mut d).unwrap();
        assert_eq!(&d[pg..pg + 4], &[0x14, 0, 5, 0x91]);
        assert_eq!(d[at + 3], 0x1B);
    }

    #[test]
    fn m2ts_packets() {
        let mut ts = vec![0u8; PACKET * 3];
        // PMT with a PG entry
        let sec = [0x02, 0xB0, 0x00, 0, 1, 0xC1, 0, 0, 0xF0, 0x01, 0xF0, 0x00, 0x90, 0xF2, 0x00, 0xF0, 0x00, 0, 0, 0, 0];
        let mut sec = sec.to_vec();
        sec[2] = (sec.len() - 3) as u8;
        ts[4..8].copy_from_slice(&[0x47, 0x41, 0x00, 0x10]);
        ts[8] = 0; // pointer
        ts[9..9 + sec.len()].copy_from_slice(&sec);
        // PES with a PCS on PG
        let p = PACKET + 4;
        ts[p..p + 4].copy_from_slice(&[0x47, 0x52, 0x00, 0x10]);
        ts[p + 4..p + 13].copy_from_slice(&[0, 0, 1, 0xBD, 0, 8, 0x84, 0x80, 0x05]);
        ts[p + 13..p + 18].copy_from_slice(&[0x21, 0, 1, 0, 1]);
        ts[p + 18] = 0x16;
        // continuation packet on PG
        ts[2 * PACKET + 4..2 * PACKET + 8].copy_from_slice(&[0x47, 0x12, 0x00, 0x11]);
        patch_m2ts(&mut ts).unwrap();
        assert_eq!(&ts[p..p + 3], &[0x47, 0x54, 0x00]);
        assert_eq!(ts[p + 18], 0x18);
        assert_eq!(ts[2 * PACKET + 5], 0x14);
        let s = &ts[9..9 + sec.len()];
        assert_eq!(&s[12..17], &[0x91, 0xF4, 0x00, 0xF0, 0x00]);
        let n = s.len();
        assert_eq!(crc32_mpeg(&s[..n - 4]).to_be_bytes(), s[n - 4..]);
    }
}
