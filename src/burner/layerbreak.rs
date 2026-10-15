//! Layer break for dual-layer (DVD-9) discs.
//!
//! Rules: the first layer (L0) holds at least as many sectors as L1, its sector count is a
//! multiple of 16 (ECC block) and the break falls on the start of a title cell, so players
//! switch layers between cells. When no cell start fits, zero sectors are inserted in front of
//! the titleset containing a suitable cell (only allowed there: VTS addresses are relative to
//! the VTS IFO) and the title start sectors in VIDEO_TS.IFO/.BUP are patched.

use std::collections::HashMap;
use std::path::Path;

use super::udf::{plan_image, ImageError, ImagePlan, SECTOR};

/// Disc capacities in 2048-byte sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacities {
    /// Largest image that fits a single-layer disc.
    pub single_layer: u32,
    /// Most sectors one layer of a dual-layer disc holds.
    pub layer: u32,
}

impl Capacities {
    /// DVD±R single layer (4.7 GB) and DVD+R DL (8.5 GB, 2 × 2,086,912 sectors).
    pub const DVD: Capacities = Capacities { single_layer: 2_295_104, layer: 2_086_912 };
}

/// A possible layer break position: the first sector of a title cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStart {
    /// Titleset number.
    pub vts: u32,
    /// Absolute sector in the image.
    pub lba: u32,
}

/// Where the second layer starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerBreak {
    /// Sectors on layer 0 (= first sector of layer 1).
    pub l0_sectors: u32,
    /// Zero sectors inserted in front of this titleset's IFO to reach the break.
    pub padding: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerBreakError {
    TooLarge { sectors: u32, max: u32 },
    NoCell,
}

impl std::fmt::Display for LayerBreakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayerBreakError::TooLarge { sectors, max } => write!(
                f,
                "das Image ist zu groß für eine DVD-9 ({:.2} GB > {:.2} GB)",
                *sectors as f64 * SECTOR as f64 / 1e9,
                *max as f64 * SECTOR as f64 / 1e9
            ),
            LayerBreakError::NoCell => write!(f, "keine geeignete Zellgrenze für den Layerwechsel gefunden"),
        }
    }
}

impl std::error::Error for LayerBreakError {}

fn fits(l0: u32, total: u32, caps: Capacities) -> bool {
    l0 % 16 == 0 && l0 >= total - l0 && l0 <= caps.layer && total - l0 <= caps.layer
}

/// Chooses the layer break for an image of `total` sectors. `None` = fits a single layer.
/// Prefers a cell start without padding, nearest to the middle; otherwise the smallest
/// padding in front of a cell's titleset that aligns it.
pub fn choose_layer_break(total: u32, cells: &[CellStart], caps: Capacities) -> Result<Option<LayerBreak>, LayerBreakError> {
    if total <= caps.single_layer {
        return Ok(None);
    }
    if total > 2 * caps.layer {
        return Err(LayerBreakError::TooLarge { sectors: total, max: 2 * caps.layer });
    }
    if let Some(c) = cells.iter().filter(|c| fits(c.lba, total, caps)).min_by_key(|c| c.lba) {
        return Ok(Some(LayerBreak { l0_sectors: c.lba, padding: None }));
    }
    // Padding P in front of titleset `vts` shifts its cells (and everything after) by P.
    let mut best: Option<(u32, u32, u32)> = None; // (padding, l0, vts)
    for c in cells {
        let pad = (16 - c.lba % 16) % 16;
        let pad = if pad == 0 { 16 } else { pad }; // aligned but did not fit without padding
        for p in [pad, pad + 16] {
            let (l0, t) = (c.lba + p, total + p);
            if fits(l0, t, caps) && best.is_none_or(|b| (p, l0) < (b.0, b.1)) {
                best = Some((p, l0, c.vts));
            }
        }
    }
    best.map(|(p, l0, vts)| Some(LayerBreak { l0_sectors: l0, padding: Some((vts, p)) })).ok_or(LayerBreakError::NoCell)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes(s.try_into().unwrap()))
}

/// Start sectors of all title cells in `plan` (from each VTS IFO's VTS_C_ADT).
pub fn cell_starts(plan: &ImagePlan) -> Result<Vec<CellStart>, ImageError> {
    let mut out = Vec::new();
    for f in &plan.files {
        let Some(vts) = f.name.strip_prefix("VTS_").filter(|_| f.name.ends_with("_0.IFO")).and_then(|r| r[..2].parse::<u32>().ok()) else {
            continue;
        };
        let ifo = match &f.data {
            Some(d) => d.clone(),
            None => std::fs::read(&f.source).map_err(|error| ImageError::Io { path: f.source.clone(), error })?,
        };
        let bad = || ImageError::NotDvdVideo(format!("{} ist beschädigt", f.name));
        if !ifo.starts_with(b"DVDVIDEO-VTS") {
            return Err(bad());
        }
        let tt_vobs = be32(&ifo, 0xC4).ok_or_else(bad)?;
        let c_adt = be32(&ifo, 0xE0).ok_or_else(bad)?;
        if tt_vobs == 0 || c_adt == 0 {
            continue;
        }
        let base = c_adt as usize * SECTOR as usize;
        let last_byte = be32(&ifo, base + 4).ok_or_else(bad)? as usize;
        let entries = (last_byte + 1).saturating_sub(8) / 12;
        for i in 0..entries {
            let start = be32(&ifo, base + 8 + 12 * i + 4).ok_or_else(bad)?;
            out.push(CellStart { vts, lba: f.lba + tt_vobs + start });
        }
    }
    out.sort_by_key(|c| c.lba);
    out.dedup();
    Ok(out)
}

/// Adds `shift` to the start sector of every title in titleset ≥ `from_vts` (TT_SRPT).
pub fn patch_tt_srpt(vmg_ifo: &mut [u8], from_vts: u32, shift: u32) -> Result<(), ImageError> {
    let bad = || ImageError::NotDvdVideo("VIDEO_TS.IFO ist beschädigt".into());
    if !vmg_ifo.starts_with(b"DVDVIDEO-VMG") {
        return Err(bad());
    }
    let base = be32(vmg_ifo, 0xC4).ok_or_else(bad)? as usize * SECTOR as usize;
    let titles = be16(vmg_ifo, base).ok_or_else(bad)? as usize;
    for i in 0..titles {
        let e = base + 8 + 12 * i;
        let vts = *vmg_ifo.get(e + 6).ok_or_else(bad)? as u32;
        if vts >= from_vts {
            let start = be32(vmg_ifo, e + 8).ok_or_else(bad)?;
            vmg_ifo[e + 8..e + 12].copy_from_slice(&(start + shift).to_be_bytes());
        }
    }
    Ok(())
}

/// A DVD-Video image plan with its layer break (if the image needs two layers).
#[derive(Debug, Clone)]
pub struct DiscPlan {
    pub image: ImagePlan,
    pub layer_break: Option<LayerBreak>,
}

/// Plans the image of `video_ts`; for DVD-9 sized content, including the layer break
/// (inserting padding and patching VIDEO_TS.IFO/.BUP when necessary).
pub fn plan_disc(video_ts: &Path, label: &str, caps: Capacities) -> Result<DiscPlan, PlanError> {
    let plan = plan_image(video_ts, label, &HashMap::new(), &HashMap::new())?;
    let cells = cell_starts(&plan)?;
    let lb = choose_layer_break(plan.total_sectors, &cells, caps)?;
    let Some(LayerBreak { padding: Some((vts, pad)), .. }) = lb else {
        return Ok(DiscPlan { image: plan, layer_break: lb });
    };
    let mut overrides = HashMap::new();
    for name in ["VIDEO_TS.IFO", "VIDEO_TS.BUP"] {
        let f = plan.file(name).expect("checked by plan_image");
        let mut data = std::fs::read(&f.source).map_err(|error| ImageError::Io { path: f.source.clone(), error })?;
        patch_tt_srpt(&mut data, vts, pad)?;
        overrides.insert(name.to_string(), data);
    }
    let padded = plan_image(video_ts, label, &HashMap::from([(vts, pad)]), &overrides)?;
    debug_assert_eq!(padded.total_sectors, plan.total_sectors + pad);
    Ok(DiscPlan { image: padded, layer_break: lb })
}

#[derive(Debug)]
pub enum PlanError {
    Image(ImageError),
    LayerBreak(LayerBreakError),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Image(e) => e.fmt(f),
            PlanError::LayerBreak(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<ImageError> for PlanError {
    fn from(e: ImageError) -> Self {
        PlanError::Image(e)
    }
}

impl From<LayerBreakError> for PlanError {
    fn from(e: LayerBreakError) -> Self {
        PlanError::LayerBreak(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: Capacities = Capacities { single_layer: 1000, layer: 600 };

    fn cells(lbas: &[u32]) -> Vec<CellStart> {
        lbas.iter().map(|&lba| CellStart { vts: if lba < 500 { 1 } else { 2 }, lba }).collect()
    }

    #[test]
    fn single_layer_needs_no_break() {
        assert_eq!(choose_layer_break(1000, &cells(&[0, 512]), CAPS), Ok(None));
    }

    #[test]
    fn too_large() {
        assert!(matches!(choose_layer_break(1201, &cells(&[]), CAPS), Err(LayerBreakError::TooLarge { .. })));
    }

    #[test]
    fn aligned_cell_at_or_after_middle() {
        // total 1100: L0 must be ≥ 550, ≤ 600, %16 == 0, L1 ≤ 600.
        let lb = choose_layer_break(1100, &cells(&[100, 544, 560, 592]), CAPS).unwrap().unwrap();
        assert_eq!(lb, LayerBreak { l0_sectors: 560, padding: None });
        let l0 = lb.l0_sectors;
        assert!(l0 >= 1100 - l0 && l0 % 16 == 0);
    }

    #[test]
    fn padding_aligns_a_cell() {
        // No aligned cell in range: cell 555 (vts 2) needs 5 sectors of padding → L0 560.
        let lb = choose_layer_break(1100, &cells(&[100, 555]), CAPS).unwrap().unwrap();
        assert_eq!(lb, LayerBreak { l0_sectors: 560, padding: Some((2, 5)) });
        // The smallest padding wins (590 + 2).
        let lb = choose_layer_break(1100, &cells(&[100, 555, 590]), CAPS).unwrap().unwrap();
        assert_eq!(lb, LayerBreak { l0_sectors: 592, padding: Some((2, 2)) });
        // With padding the image grows: L0 still ≥ L1.
        assert!(560 >= 1105 - 560);
    }

    #[test]
    fn no_suitable_cell() {
        assert_eq!(choose_layer_break(1100, &cells(&[10, 20, 700]), CAPS), Err(LayerBreakError::NoCell));
    }

    #[test]
    fn real_capacity_limits() {
        let c = Capacities::DVD;
        let total = 3_000_000;
        let lb = choose_layer_break(total, &[CellStart { vts: 1, lba: 1_500_000 }], c).unwrap().unwrap();
        assert_eq!(lb.l0_sectors, 1_500_000);
        assert_eq!(1_500_000 % 16, 0);
    }

    #[test]
    fn tt_srpt_patch() {
        // Minimal VMG IFO: TT_SRPT in sector 1 with two titles (vts 1 at 100, vts 2 at 900).
        let mut ifo = vec![0u8; 2 * SECTOR as usize];
        ifo[..12].copy_from_slice(b"DVDVIDEO-VMG");
        ifo[0xC4..0xC8].copy_from_slice(&1u32.to_be_bytes());
        let b = SECTOR as usize;
        ifo[b..b + 2].copy_from_slice(&2u16.to_be_bytes());
        for (i, (vts, start)) in [(1u8, 100u32), (2, 900)].iter().enumerate() {
            let e = b + 8 + 12 * i;
            ifo[e + 6] = *vts;
            ifo[e + 8..e + 12].copy_from_slice(&start.to_be_bytes());
        }
        patch_tt_srpt(&mut ifo, 2, 7).unwrap();
        assert_eq!(be32(&ifo, b + 8 + 8), Some(100));
        assert_eq!(be32(&ifo, b + 20 + 8), Some(907));
    }
}
