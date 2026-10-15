//! Video bitrate calculator for DVD5 / DVD9 targets.
//!
//! `video_kbps = (target_MB * 8192 - audio_kbps * duration_s - mux_overhead_kbit) / duration_s`
//!
//! The result is clamped to [`MIN_VIDEO_KBPS`, `MAX_VIDEO_KBPS`] and additionally to
//! `DVD_MAX_TOTAL_KBPS - audio_kbps`, so video + audio never exceeds the DVD-Video mux limit.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Lowest video bitrate (as in ConvertXToDVD): long projects still fit on a DVD-5,
/// at visibly reduced quality.
pub const MIN_VIDEO_KBPS: u32 = 1000;
/// Highest video bitrate; leaves headroom below the DVD limit for audio and subtitles.
pub const MAX_VIDEO_KBPS: u32 = 8500;
/// DVD-Video maximum combined (video + audio) bitrate.
pub const DVD_MAX_TOTAL_KBPS: u32 = 9800;
/// Default mux overhead as a fraction of the target size (VOB/pack headers, NAV packs).
pub const DEFAULT_MUX_OVERHEAD_RATIO: f64 = 0.02;
/// Above this video bitrate the quality is rated good.
pub const QUALITY_GOOD_KBPS: u32 = 2500;
/// Below this video bitrate the quality is rated poor.
pub const QUALITY_POOR_KBPS: u32 = 1200;

/// Quality rating of a video bitrate (green / yellow / red in the GUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// More than 2500 kbps.
    Good,
    /// 1200 to 2500 kbps.
    Medium,
    /// Less than 1200 kbps.
    Poor,
}

impl Quality {
    pub fn of(video_kbps: u32) -> Quality {
        if video_kbps > QUALITY_GOOD_KBPS {
            Quality::Good
        } else if video_kbps >= QUALITY_POOR_KBPS {
            Quality::Medium
        } else {
            Quality::Poor
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscType {
    /// Single layer, usable target 4300 MB.
    #[default]
    Dvd5,
    /// Dual layer, usable target 7950 MB.
    Dvd9,
    /// User-defined target size in MB (JSON: `{"custom": 4000}`).
    Custom(u32),
}

impl DiscType {
    pub fn target_mb(self) -> u32 {
        match self {
            DiscType::Dvd5 => 4300,
            DiscType::Dvd9 => 7950,
            DiscType::Custom(mb) => mb,
        }
    }

    /// Target capacity in kbit (`MB * 8192`).
    pub fn target_kbit(self) -> f64 {
        self.target_mb() as f64 * 8192.0
    }

    /// Default mux overhead in kbit for this disc.
    pub fn default_mux_overhead_kbit(self) -> f64 {
        self.target_kbit() * DEFAULT_MUX_OVERHEAD_RATIO
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Clamp {
    None,
    /// Raised to the minimum: the content will NOT fit on the disc at this bitrate.
    Min,
    /// Lowered to the maximum: the disc will not be full.
    Max,
}

#[derive(Debug, Clone, Serialize)]
pub struct BitratePlan {
    pub disc: DiscType,
    pub duration_secs: f64,
    pub audio_kbps: u32,
    pub mux_overhead_kbit: f64,
    /// Unclamped result of the formula (may be negative for very long content).
    pub raw_video_kbps: f64,
    /// Final video bitrate to encode with.
    pub video_kbps: u32,
    pub clamp: Clamp,
    /// Expected output size in MB at `video_kbps`.
    pub estimated_mb: f64,
}

impl BitratePlan {
    pub fn fits(&self) -> bool {
        self.estimated_mb <= self.disc.target_mb() as f64
    }

    pub fn total_kbps(&self) -> u32 {
        self.video_kbps + self.audio_kbps
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalcError {
    InvalidDuration(f64),
    InvalidOverhead(f64),
    /// Audio leaves less than `MIN_VIDEO_KBPS` within the DVD total limit.
    AudioTooHigh(u32),
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::InvalidDuration(d) => write!(f, "duration must be > 0 s (got {d})"),
            CalcError::InvalidOverhead(o) => write!(f, "mux overhead must be >= 0 kbit (got {o})"),
            CalcError::AudioTooHigh(a) => write!(
                f,
                "audio bitrate {a} kbps leaves less than {MIN_VIDEO_KBPS} kbps video within the {DVD_MAX_TOTAL_KBPS} kbps DVD limit"
            ),
        }
    }
}

impl std::error::Error for CalcError {}

/// Computes the video bitrate with the default mux overhead for `disc`.
pub fn calculate(disc: DiscType, duration_secs: f64, audio_kbps: u32) -> Result<BitratePlan, CalcError> {
    calculate_with_overhead(disc, duration_secs, audio_kbps, disc.default_mux_overhead_kbit())
}

/// Computes the video bitrate with an explicit mux overhead (total kbit).
pub fn calculate_with_overhead(
    disc: DiscType,
    duration_secs: f64,
    audio_kbps: u32,
    mux_overhead_kbit: f64,
) -> Result<BitratePlan, CalcError> {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err(CalcError::InvalidDuration(duration_secs));
    }
    if !mux_overhead_kbit.is_finite() || mux_overhead_kbit < 0.0 {
        return Err(CalcError::InvalidOverhead(mux_overhead_kbit));
    }
    let max_video = MAX_VIDEO_KBPS.min(DVD_MAX_TOTAL_KBPS.saturating_sub(audio_kbps));
    if max_video < MIN_VIDEO_KBPS {
        return Err(CalcError::AudioTooHigh(audio_kbps));
    }

    let raw = (disc.target_kbit() - audio_kbps as f64 * duration_secs - mux_overhead_kbit) / duration_secs;

    // Round down so the estimate never overshoots the target due to rounding.
    let (video_kbps, clamp) = if raw < MIN_VIDEO_KBPS as f64 {
        (MIN_VIDEO_KBPS, Clamp::Min)
    } else if raw > max_video as f64 {
        (max_video, Clamp::Max)
    } else {
        (raw.floor() as u32, Clamp::None)
    };

    let estimated_mb =
        ((video_kbps + audio_kbps) as f64 * duration_secs + mux_overhead_kbit) / 8192.0;

    Ok(BitratePlan {
        disc,
        duration_secs,
        audio_kbps,
        mux_overhead_kbit,
        raw_video_kbps: raw,
        video_kbps,
        clamp,
        estimated_mb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dvd5_two_hours_fits() {
        let p = calculate(DiscType::Dvd5, 2.0 * 3600.0, 192).unwrap();
        assert_eq!(p.clamp, Clamp::None);
        assert!(p.fits(), "{p:?}");
        // (4300*8192*0.98 - 192*7200) / 7200 ≈ 4602
        assert_eq!(p.video_kbps, 4602);
    }

    #[test]
    fn dvd9_two_hours_fits() {
        let p = calculate(DiscType::Dvd9, 2.0 * 3600.0, 448).unwrap();
        assert_eq!(p.clamp, Clamp::None);
        assert!(p.fits());
        assert!(p.video_kbps > 8000 && p.video_kbps <= MAX_VIDEO_KBPS);
    }

    #[test]
    fn short_content_clamped_to_max() {
        let p = calculate(DiscType::Dvd9, 600.0, 448).unwrap();
        assert_eq!(p.clamp, Clamp::Max);
        assert_eq!(p.video_kbps, MAX_VIDEO_KBPS);
        assert!(p.total_kbps() <= DVD_MAX_TOTAL_KBPS);
    }

    #[test]
    fn high_audio_respects_total_limit() {
        let p = calculate(DiscType::Dvd9, 600.0, 1536).unwrap();
        assert_eq!(p.video_kbps, DVD_MAX_TOTAL_KBPS - 1536);
        assert_eq!(p.total_kbps(), DVD_MAX_TOTAL_KBPS);
    }

    #[test]
    fn long_content_clamped_to_min_and_does_not_fit() {
        let p = calculate(DiscType::Dvd5, 9.0 * 3600.0, 192).unwrap();
        assert_eq!(p.clamp, Clamp::Min);
        assert_eq!(p.video_kbps, MIN_VIDEO_KBPS);
        assert!(!p.fits());
    }

    #[test]
    fn explicit_overhead_matches_formula() {
        let p = calculate_with_overhead(DiscType::Dvd5, 6000.0, 224, 100_000.0).unwrap();
        let expected = (4300.0 * 8192.0 - 224.0 * 6000.0 - 100_000.0) / 6000.0;
        assert!((p.raw_video_kbps - expected).abs() < 1e-9);
        assert_eq!(p.video_kbps, expected.floor() as u32);
    }

    /// Regression: 46 titles with 5 h 40 min did not fit on a DVD-5 with the old 2000 kbps floor.
    #[test]
    fn long_project_fits_dvd5() {
        let p = calculate(DiscType::Dvd5, (5.0 * 60.0 + 40.0) * 60.0, 192).unwrap();
        assert_eq!(p.clamp, Clamp::None);
        assert!(p.fits(), "{p:?}");
        assert!((1000..2000).contains(&p.video_kbps), "{}", p.video_kbps);
        assert_eq!(Quality::of(p.video_kbps), Quality::Medium);
    }

    #[test]
    fn quality_thresholds() {
        assert_eq!(Quality::of(8500), Quality::Good);
        assert_eq!(Quality::of(2501), Quality::Good);
        assert_eq!(Quality::of(2500), Quality::Medium);
        assert_eq!(Quality::of(1200), Quality::Medium);
        assert_eq!(Quality::of(1199), Quality::Poor);
        assert_eq!(Quality::of(1000), Quality::Poor);
    }

    #[test]
    fn custom_target() {
        assert_eq!(DiscType::Custom(700).target_mb(), 700);
        let p = calculate(DiscType::Custom(700), 3600.0, 192).unwrap();
        assert!(p.fits() && p.estimated_mb <= 700.0);
        assert_eq!(serde_json::to_string(&DiscType::Custom(700)).unwrap(), r#"{"custom":700}"#);
        assert_eq!(serde_json::from_str::<DiscType>(r#""dvd9""#).unwrap(), DiscType::Dvd9);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(calculate(DiscType::Dvd5, 0.0, 192), Err(CalcError::InvalidDuration(_))));
        assert!(matches!(calculate(DiscType::Dvd5, f64::NAN, 192), Err(CalcError::InvalidDuration(_))));
        assert!(matches!(calculate(DiscType::Dvd5, 3600.0, 9000), Err(CalcError::AudioTooHigh(_))));
        assert!(matches!(
            calculate_with_overhead(DiscType::Dvd5, 3600.0, 192, -1.0),
            Err(CalcError::InvalidOverhead(_))
        ));
    }
}
