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
/// DVD-Video mux overhead per second of playing time: one 2 KiB NAV pack per VOBU (~0.5 s) plus
/// the VOBU's alignment padding. Measured 39 kbit/s of NAV packs on an 83-title disc.
pub const DVD_NAV_KBPS: f64 = 40.0;
/// DVD-Video pack and PES headers and audio padding as a fraction of the audio + video stream
/// (measured 2.5-2.6 % at 1.7 Mbit/s, rounded up; 2 KiB packs carry the same headers at any bitrate).
pub const DVD_PACK_OVERHEAD_RATIO: f64 = 0.027;
/// IFO + BUP of one DVD titleset (measured ~34 KiB; reserved with headroom).
pub const DVD_IFO_KBIT_PER_TITLE: f64 = 64.0 * 8.192;
/// Blu-ray overhead (m2ts 192/184-byte packets, PES/PCR).
pub const BD_MUX_OVERHEAD_RATIO: f64 = 0.06;
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

/// Bitrate limits and quality thresholds of a disc format (kbps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscLimits {
    pub min_video_kbps: u32,
    pub max_video_kbps: u32,
    /// Maximum combined video + audio bitrate.
    pub max_total_kbps: u32,
    pub good_kbps: u32,
    pub poor_kbps: u32,
    /// Video peak rate (VBV maxrate) and buffer size of H.264 encodes (kbit/s, kbit); the DVD
    /// values are not used by the MPEG-2 encoder.
    pub vbv_maxrate_kbps: u32,
    pub vbv_bufsize_kbit: u32,
}

impl Quality {
    /// Rating with the DVD thresholds.
    pub fn of(video_kbps: u32) -> Quality {
        Quality::rate(video_kbps, QUALITY_GOOD_KBPS, QUALITY_POOR_KBPS)
    }

    /// Rating with the thresholds of `disc`.
    pub fn for_disc(video_kbps: u32, disc: DiscType) -> Quality {
        let l = disc.limits();
        Quality::rate(video_kbps, l.good_kbps, l.poor_kbps)
    }

    fn rate(video_kbps: u32, good: u32, poor: u32) -> Quality {
        if video_kbps > good {
            Quality::Good
        } else if video_kbps >= poor {
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
    /// Blu-ray, single layer (BD-25), usable target 22,900 MiB.
    Bd25,
    /// Blu-ray, dual layer (BD-50), usable target 45,800 MiB.
    Bd50,
    /// AVCHD ("BD5"): Blu-ray folder structure on a DVD-5, usable target 4300 MB.
    Avchd5,
    /// AVCHD ("BD9") on a dual-layer DVD, usable target 7950 MB.
    Avchd9,
    /// User-defined target size in MB (JSON: `{"custom": 4000}`).
    Custom(u32),
}

impl DiscType {
    pub fn target_mb(self) -> u32 {
        match self {
            DiscType::Dvd5 => 4300,
            DiscType::Dvd9 => 7950,
            DiscType::Bd25 => 22_900,
            DiscType::Bd50 => 45_800,
            DiscType::Avchd5 => 4300,
            DiscType::Avchd9 => 7950,
            DiscType::Custom(mb) => mb,
        }
    }

    /// Uses the Blu-ray build pipeline (H.264 + native BDAV muxer, BDMV folder): BD-25/50 and AVCHD.
    pub fn is_bluray(self) -> bool {
        matches!(self, DiscType::Bd25 | DiscType::Bd50 | DiscType::Avchd5 | DiscType::Avchd9)
    }

    /// BDMV structure on DVD media.
    pub fn is_avchd(self) -> bool {
        matches!(self, DiscType::Avchd5 | DiscType::Avchd9)
    }

    /// Physical medium is a DVD (DVD-Video or AVCHD).
    pub fn is_dvd_media(self) -> bool {
        !matches!(self, DiscType::Bd25 | DiscType::Bd50)
    }

    pub fn limits(self) -> DiscLimits {
        if self.is_avchd() {
            DiscLimits {
                min_video_kbps: 2000,
                max_video_kbps: 15_000,
                max_total_kbps: 18_000,
                good_kbps: 10_000,
                poor_kbps: 5000,
                vbv_maxrate_kbps: 18_000,
                vbv_bufsize_kbit: 18_000,
            }
        } else if self.is_bluray() {
            DiscLimits {
                min_video_kbps: 4000,
                max_video_kbps: 35_000,
                max_total_kbps: 48_000,
                good_kbps: 15_000,
                poor_kbps: 8000,
                vbv_maxrate_kbps: 40_000,
                vbv_bufsize_kbit: 30_000,
            }
        } else {
            DiscLimits {
                min_video_kbps: MIN_VIDEO_KBPS,
                max_video_kbps: MAX_VIDEO_KBPS,
                max_total_kbps: DVD_MAX_TOTAL_KBPS,
                good_kbps: QUALITY_GOOD_KBPS,
                poor_kbps: QUALITY_POOR_KBPS,
                vbv_maxrate_kbps: 9800,
                vbv_bufsize_kbit: 1835,
            }
        }
    }

    /// Target capacity in kbit (`MB * 8192`).
    pub fn target_kbit(self) -> f64 {
        self.target_mb() as f64 * 8192.0
    }

    /// Default mux overhead in kbit for this disc.
    pub fn default_mux_overhead_kbit(self) -> f64 {
        self.target_kbit() * if self.is_bluray() { BD_MUX_OVERHEAD_RATIO } else { DEFAULT_MUX_OVERHEAD_RATIO }
    }
}

/// Converts a byte count to kilobits using the calculator's `1 MiB = 8192 kbit` convention (`1 kbit = 128 bytes`).
pub fn bytes_to_kbit(bytes: u64) -> f64 {
    bytes as f64 / 128.0
}

/// Encoding profile ("Profil"): an upper limit for the video bitrate. `Auto` and `Sp` fill the
/// disc up to [`MAX_VIDEO_KBPS`]; `Mp`/`Lp` stop earlier (smaller output for short projects),
/// and `Lp` switches to Half D1 when the bitrate drops below [`LP_HALF_D1_BELOW_KBPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncodingProfile {
    #[default]
    Auto,
    /// Short running time, highest quality.
    Sp,
    /// Medium running time.
    Mp,
    /// Long running time.
    Lp,
}

/// LP: below this video bitrate a Full D1 project is encoded in Half D1 instead.
pub const LP_HALF_D1_BELOW_KBPS: u32 = 3000;

impl EncodingProfile {
    /// Highest video bitrate of the profile.
    pub fn max_video_kbps(self) -> u32 {
        self.max_video_kbps_for(DiscType::Dvd5)
    }

    /// Highest video bitrate of the profile on `disc` (BD: 35,000 / 20,000 / 12,000; AVCHD:
    /// 15,000 / 10,000 / 6000).
    pub fn max_video_kbps_for(self, disc: DiscType) -> u32 {
        let (mp, lp) = if disc.is_avchd() {
            (10_000, 6000)
        } else if disc.is_bluray() {
            (20_000, 12_000)
        } else {
            (6000, 4000)
        };
        match self {
            EncodingProfile::Auto | EncodingProfile::Sp => disc.limits().max_video_kbps,
            EncodingProfile::Mp => mp,
            EncodingProfile::Lp => lp,
        }
    }

    /// Short name for messages ("Auto", "SP", "MP", "LP").
    pub fn label(self) -> &'static str {
        match self {
            EncodingProfile::Auto => "Auto",
            EncodingProfile::Sp => "SP",
            EncodingProfile::Mp => "MP",
            EncodingProfile::Lp => "LP",
        }
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
    /// Audio leaves less than the minimum video bitrate within the disc's total limit.
    AudioTooHigh { audio_kbps: u32, limits: DiscLimits },
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::InvalidDuration(d) => write!(f, "duration must be > 0 s (got {d})"),
            CalcError::InvalidOverhead(o) => write!(f, "mux overhead must be >= 0 kbit (got {o})"),
            CalcError::AudioTooHigh { audio_kbps, limits } => write!(
                f,
                "audio bitrate {audio_kbps} kbps leaves less than {} kbps video within the {} kbps disc limit",
                limits.min_video_kbps, limits.max_total_kbps
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
    calculate_capped(disc, duration_secs, audio_kbps, mux_overhead_kbit, disc.limits().max_video_kbps)
}

/// Like [`calculate_with_overhead`] with a lower ceiling for the video bitrate (encoding
/// profile); `max_video_kbps` is limited to [`MIN_VIDEO_KBPS`]..=[`MAX_VIDEO_KBPS`].
pub fn calculate_capped(
    disc: DiscType,
    duration_secs: f64,
    audio_kbps: u32,
    mux_overhead_kbit: f64,
    max_video_kbps: u32,
) -> Result<BitratePlan, CalcError> {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err(CalcError::InvalidDuration(duration_secs));
    }
    if !mux_overhead_kbit.is_finite() || mux_overhead_kbit < 0.0 {
        return Err(CalcError::InvalidOverhead(mux_overhead_kbit));
    }
    let limits = disc.limits();
    let min_video = limits.min_video_kbps;
    let cap = max_video_kbps.clamp(min_video, limits.max_video_kbps);
    let max_video = cap.min(limits.max_total_kbps.saturating_sub(audio_kbps));
    if max_video < min_video {
        return Err(CalcError::AudioTooHigh { audio_kbps, limits });
    }

    let raw = (disc.target_kbit() - audio_kbps as f64 * duration_secs - mux_overhead_kbit) / duration_secs;

    // Round down so the estimate never overshoots the target due to rounding.
    let (video_kbps, clamp) = if raw < min_video as f64 {
        (min_video, Clamp::Min)
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
    fn bluray_limits_and_targets() {
        assert_eq!((DiscType::Bd25.target_mb(), DiscType::Bd50.target_mb()), (22_900, 45_800));
        assert!(DiscType::Bd25.is_bluray() && !DiscType::Dvd9.is_bluray() && !DiscType::Custom(5).is_bluray());
        assert_eq!(DiscType::Dvd5.limits().max_total_kbps, DVD_MAX_TOTAL_KBPS);
        let l = DiscType::Bd50.limits();
        assert_eq!((l.min_video_kbps, l.max_video_kbps, l.max_total_kbps, l.good_kbps, l.poor_kbps), (4000, 35_000, 48_000, 15_000, 8000));
        // 6 % overhead on BD, 2 % on DVD.
        assert!((DiscType::Bd25.default_mux_overhead_kbit() - 22_900.0 * 8192.0 * 0.06).abs() < 1e-6);
        // 2 h on a BD-25 lands between the limits; 10 min hits the ceiling.
        let p = calculate(DiscType::Bd25, 7200.0, 448).unwrap();
        assert_eq!(p.clamp, Clamp::None);
        assert!(p.fits() && (20_000..35_000).contains(&p.video_kbps), "{}", p.video_kbps);
        assert_eq!(calculate(DiscType::Bd25, 600.0, 448).unwrap().video_kbps, 35_000);
        // A 12 h project is clamped to the minimum and does not fit.
        let long = calculate(DiscType::Bd25, 12.0 * 3600.0, 448).unwrap();
        assert_eq!((long.clamp, long.video_kbps, long.fits()), (Clamp::Min, 4000, false));
        assert!(matches!(calculate(DiscType::Bd25, 3600.0, 46_000), Err(CalcError::AudioTooHigh { .. })));
        // Profile ceilings and quality per disc.
        let ceil = |p: EncodingProfile| p.max_video_kbps_for(DiscType::Bd50);
        assert_eq!((ceil(EncodingProfile::Auto), ceil(EncodingProfile::Sp), ceil(EncodingProfile::Mp), ceil(EncodingProfile::Lp)), (35_000, 35_000, 20_000, 12_000));
        assert_eq!(EncodingProfile::Mp.max_video_kbps_for(DiscType::Dvd9), 6000);
        assert_eq!(Quality::for_disc(15_001, DiscType::Bd25), Quality::Good);
        assert_eq!(Quality::for_disc(8000, DiscType::Bd25), Quality::Medium);
        assert_eq!(Quality::for_disc(7999, DiscType::Bd25), Quality::Poor);
        assert_eq!(Quality::for_disc(3000, DiscType::Dvd5), Quality::of(3000));
        assert_eq!(serde_json::from_str::<DiscType>(r#""bd50""#).unwrap(), DiscType::Bd50);
    }

    #[test]
    fn avchd_limits() {
        let d = DiscType::Avchd5;
        assert_eq!((d.target_mb(), DiscType::Avchd9.target_mb()), (4300, 7950));
        assert!(d.is_bluray() && d.is_avchd() && d.is_dvd_media() && !DiscType::Bd25.is_dvd_media());
        assert!(DiscType::Dvd5.is_dvd_media() && !DiscType::Dvd5.is_bluray());
        let l = d.limits();
        assert_eq!((l.min_video_kbps, l.max_video_kbps, l.max_total_kbps, l.good_kbps, l.poor_kbps), (2000, 15_000, 18_000, 10_000, 5000));
        assert_eq!((l.vbv_maxrate_kbps, l.vbv_bufsize_kbit), (18_000, 18_000));
        assert_eq!(DiscType::Bd25.limits().vbv_maxrate_kbps, 40_000);
        let ceil = |p: EncodingProfile| p.max_video_kbps_for(d);
        assert_eq!((ceil(EncodingProfile::Auto), ceil(EncodingProfile::Sp), ceil(EncodingProfile::Mp), ceil(EncodingProfile::Lp)), (15_000, 15_000, 10_000, 6000));
        // 6 % overhead like BD; one hour fits at a high rate, 10 h is clamped to the minimum.
        assert!((d.default_mux_overhead_kbit() - 4300.0 * 8192.0 * 0.06).abs() < 1e-6);
        let p = calculate(d, 3600.0, 448).unwrap();
        assert!(p.fits() && p.video_kbps > 5000 && p.video_kbps <= 15_000, "{}", p.video_kbps);
        assert_eq!(calculate(d, 10.0 * 3600.0, 448).unwrap().clamp, Clamp::Min);
        assert_eq!(serde_json::from_str::<DiscType>(r#""avchd9""#).unwrap(), DiscType::Avchd9);
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
        assert!(matches!(calculate(DiscType::Dvd5, 3600.0, 9000), Err(CalcError::AudioTooHigh { .. })));
        assert!(matches!(
            calculate_with_overhead(DiscType::Dvd5, 3600.0, 192, -1.0),
            Err(CalcError::InvalidOverhead(_))
        ));
    }

    #[test]
    fn profiles_cap_the_bitrate() {
        assert_eq!(EncodingProfile::default(), EncodingProfile::Auto);
        assert_eq!(EncodingProfile::Sp.max_video_kbps(), MAX_VIDEO_KBPS);
        // 30 min on a DVD-5 would get the maximum; MP/LP stop at their ceiling.
        for (profile, want) in [(EncodingProfile::Auto, MAX_VIDEO_KBPS), (EncodingProfile::Mp, 6000), (EncodingProfile::Lp, 4000)] {
            let p = calculate_capped(DiscType::Dvd5, 1800.0, 192, DiscType::Dvd5.default_mux_overhead_kbit(), profile.max_video_kbps())
                .unwrap();
            assert_eq!((p.video_kbps, p.clamp), (want, Clamp::Max), "{profile:?}");
            assert!(p.estimated_mb < 4300.0);
        }
        // Long content below the ceiling is unaffected.
        let long = calculate_capped(DiscType::Dvd5, 4.0 * 3600.0, 192, 0.0, 4000).unwrap();
        assert_eq!(long.clamp, Clamp::None);
        assert_eq!(long.video_kbps, calculate_with_overhead(DiscType::Dvd5, 4.0 * 3600.0, 192, 0.0).unwrap().video_kbps);
        // The ceiling never goes below the minimum.
        assert_eq!(calculate_capped(DiscType::Dvd5, 1800.0, 192, 0.0, 10).unwrap().video_kbps, MIN_VIDEO_KBPS);
    }

    #[test]
    fn bytes_to_kbit_and_fixed_overhead_deduction() {
        assert_eq!(bytes_to_kbit(1024 * 1024), 8192.0);
        assert_eq!(bytes_to_kbit(128), 1.0);
        assert_eq!(bytes_to_kbit(0), 0.0);

        // Reserving 1000 MiB of fixed byte overhead reduces available bitrate accordingly.
        let fixed = bytes_to_kbit(1000 * 1024 * 1024);
        let base = calculate_with_overhead(DiscType::Dvd5, 3600.0, 192, 0.0).unwrap();
        let reduced = calculate_with_overhead(DiscType::Dvd5, 3600.0, 192, fixed).unwrap();
        assert_eq!(reduced.clamp, Clamp::None);
        assert!(reduced.fits());
        assert!(reduced.video_kbps < base.video_kbps);
        let expected_raw = (DiscType::Dvd5.target_kbit() - 192.0 * 3600.0 - fixed) / 3600.0;
        assert!((reduced.raw_video_kbps - expected_raw).abs() < 1e-9);
        assert_eq!(reduced.video_kbps, expected_raw.floor() as u32);

        // Overflowing fixed byte overhead clamps down to MIN_VIDEO_KBPS with Clamp::Min.
        let overflow = bytes_to_kbit(4200 * 1024 * 1024);
        let clamped = calculate_capped(DiscType::Dvd5, 3600.0, 192, overflow, MAX_VIDEO_KBPS).unwrap();
        assert_eq!(clamped.clamp, Clamp::Min);
        assert_eq!(clamped.video_kbps, MIN_VIDEO_KBPS);
        assert!(!clamped.fits());
    }
}
