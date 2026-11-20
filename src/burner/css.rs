//! CSS (Content Scramble System) detection on DVD-Video sectors and ISO images.
//!
//! Warns before burning an image that was copied with raw encrypted sectors (which cannot play on a
//! blank recordable disc). Decryption is explicitly out of scope.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::isoread::IsoImage;

const SECTOR_SIZE: usize = 2048;
const MAX_SAMPLES_PER_VOB: usize = 1024;

/// Returns true when `sector` is a DVD Program Stream sector whose PES packet has the CSS scrambling
/// bits set (PES scrambling control != 0).
///
/// Only checks content streams (video `0xE0..=0xEF`, audio `0xC0..=0xDF`, private stream 1 `0xBD`).
/// NAV packs, system headers and padding never count. Bounds-checked; never panics on short input.
pub fn pes_scrambled(sector: &[u8]) -> bool {
    if sector.len() < SECTOR_SIZE {
        return false;
    }
    // Pack start code 00 00 01 BA
    if sector[0..4] != [0x00, 0x00, 0x01, 0xBA] {
        return false;
    }
    // Pack stuffing length in the low 3 bits of byte 13
    let stuffing = (sector[13] & 0x07) as usize;
    let pes = 14 + stuffing;
    // PES packet header: 00 00 01 <stream_id> <packet_len:2> <flags:1>
    if pes + 7 > sector.len() {
        return false;
    }
    if sector[pes..pes + 3] != [0x00, 0x00, 0x01] {
        return false;
    }
    let stream_id = sector[pes + 3];
    let is_content = matches!(stream_id, 0xE0..=0xEF | 0xC0..=0xDF | 0xBD);
    if !is_content {
        return false;
    }
    // Byte 6 of the PES header contains the PES scrambling control in bits 5 and 4.
    let flags = sector[pes + 6];
    (flags & 0x30) != 0
}

fn is_title_vob(path: &str) -> bool {
    let p = path.to_ascii_uppercase();
    let file = p.split('/').next_back().unwrap_or(&p);
    let Some(stem) = file.strip_suffix(".VOB") else { return false };
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() != 3 || parts[0] != "VTS" {
        return false;
    }
    parts[2].parse::<u32>().is_ok_and(|m| m >= 1)
}

/// Checks whether an ISO image contains CSS-scrambled sectors in any of its title VOBs.
///
/// Returns:
/// - `Some(true)` if at least one scrambled sector was found.
/// - `Some(false)` if title VOBs were found and none of their sampled sectors were scrambled.
/// - `None` if the file cannot be inspected (not an ISO 9660 image, no title VOBs present, or I/O error).
pub fn image_scrambled(path: &Path) -> Option<bool> {
    let file = File::open(path).ok()?;
    let mut iso = IsoImage::open(file).ok()?;
    let map = iso.extent_map().ok()?;

    let mut reader = File::open(path).ok()?;
    let mut found_title_vob = false;
    let mut sector = [0u8; SECTOR_SIZE];

    for (start, end, file_path) in map {
        if !is_title_vob(&file_path) {
            continue;
        }
        let count = end.saturating_sub(start);
        if count == 0 {
            continue;
        }
        found_title_vob = true;

        let samples = (count as usize).min(MAX_SAMPLES_PER_VOB);
        let step = count as f64 / samples as f64;
        for i in 0..samples {
            let lba = start + (i as f64 * step) as u64;
            reader.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64)).ok()?;
            reader.read_exact(&mut sector).ok()?;
            if pes_scrambled(&sector) {
                return Some(true);
            }
        }
    }

    if found_title_vob {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;
    use crate::burner::udf::{plan_image, write_image, Timestamp};
    use crate::burner::testutil::synthetic_video_ts;

    fn make_pack(stream_id: u8, stuffing: usize, scrambling: u8) -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        s[0..4].copy_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
        s[4..13].fill(0x44); // dummy clock ref / mux rate
        s[13] = 0xF8 | (stuffing as u8 & 0x07);
        s[14..14 + stuffing].fill(0xFF); // stuffing bytes
        let pes = 14 + stuffing;
        s[pes..pes + 3].copy_from_slice(&[0x00, 0x00, 0x01]);
        s[pes + 3] = stream_id;
        s[pes + 4..pes + 6].copy_from_slice(&[0x07, 0xEC]); // packet length
        s[pes + 6] = 0x80 | ((scrambling & 0x03) << 4); // scrambling control bits
        s[pes + 7] = 0x80; // PTS flag
        s[pes + 8] = 0x05; // header data length
        s
    }

    #[test]
    fn pes_scrambled_detects_scrambled_content_streams() {
        // Video stream 0xE0, scrambling bits set -> true
        let s = make_pack(0xE0, 0, 2);
        assert!(pes_scrambled(&s));

        // Audio stream 0xC0 (MPEG) or 0xBD (AC-3/DTS/SPU) -> true
        assert!(pes_scrambled(&make_pack(0xC0, 0, 1)));
        assert!(pes_scrambled(&make_pack(0xBD, 0, 3)));

        // Stuffing bytes respected
        assert!(pes_scrambled(&make_pack(0xE0, 5, 2)));

        // Scrambling bits 0 -> false
        assert!(!pes_scrambled(&make_pack(0xE0, 0, 0)));

        // NAV pack (0xBF) or System header (0xBB) -> false even with bits set
        assert!(!pes_scrambled(&make_pack(0xBF, 0, 2)));
        assert!(!pes_scrambled(&make_pack(0xBB, 0, 2)));

        // Empty / short / all-zero sectors -> false
        assert!(!pes_scrambled(&[]));
        assert!(!pes_scrambled(&[0u8; 100]));
        assert!(!pes_scrambled(&[0u8; 2048]));
    }

    #[test]
    fn image_scrambled_scans_iso_title_vobs() {
        let dir = synthetic_video_ts("CSSTEST");
        let video_ts = dir.join("VIDEO_TS");

        let plan = plan_image(&video_ts, "CSSTEST", &HashMap::new(), &HashMap::new()).unwrap();
        let iso_path = dir.join("clean.iso");
        let ts = Timestamp { year: 2026, month: 11, day: 4, hour: 12, minute: 0, second: 0 };
        write_image(&plan, &iso_path, &ts, &AtomicBool::new(false), |_| {}).unwrap();

        // 1. Clean synthetic ISO -> Some(false)
        assert_eq!(image_scrambled(&iso_path), Some(false));

        // 2. Patched ISO with one scrambled sector in VTS_01_1.VOB -> Some(true)
        let mut iso_bytes = std::fs::read(&iso_path).unwrap();
        let mut iso = IsoImage::open(Cursor::new(&iso_bytes)).unwrap();
        let map = iso.extent_map().unwrap();
        let (start, _, _) = map.iter().find(|(_, _, p)| p.contains("VTS_01_1.VOB")).unwrap();
        let offset = (*start as usize) * 2048;
        let scrambled_pack = make_pack(0xE0, 0, 2);
        iso_bytes[offset..offset + 2048].copy_from_slice(&scrambled_pack);

        let bad_iso_path = dir.join("scrambled.iso");
        std::fs::write(&bad_iso_path, &iso_bytes).unwrap();
        assert_eq!(image_scrambled(&bad_iso_path), Some(true));

        // 3. Non-ISO or ISO without VOB -> None
        let dummy = dir.join("empty.iso");
        std::fs::write(&dummy, [0u8; 2048 * 20]).unwrap();
        assert_eq!(image_scrambled(&dummy), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
