//! Remaining-time estimates for the whole job: menus → video encoding → authoring (VOBs) →
//! disc image → burning.
//!
//! Encoding is estimated from ffmpeg's frame/fps telemetry, authoring and image creation from
//! the data size and measured throughputs, burning from the image size and the write speed.

use serde::Serialize;

/// Bytes per second at 1x DVD speed.
pub const DVD_1X_BYTES_PER_SEC: f64 = 1_385_000.0;
/// Bytes per second at 1x BD speed (36 Mbit/s).
pub const BD_1X_BYTES_PER_SEC: f64 = 4_495_500.0;

/// Kind of disc, for write speeds and times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Medium {
    #[default]
    Dvd,
    Bd,
}

impl Medium {
    /// Bytes per second at 1x.
    pub fn bytes_per_sec_1x(self) -> f64 {
        match self {
            Medium::Dvd => DVD_1X_BYTES_PER_SEC,
            Medium::Bd => BD_1X_BYTES_PER_SEC,
        }
    }

    /// Average read rate of the verification: about 8x on DVD (CAV up to 16x), 6x on BD.
    pub fn verify_bytes_per_sec(self) -> f64 {
        self.bytes_per_sec_1x()
            * match self {
                Medium::Dvd => 8.0,
                Medium::Bd => 6.0,
            }
    }
}
/// dvdauthor copies the title VOBs (measured ~300 MB/s on an SSD; conservative).
const AUTHORING_BYTES_PER_SEC: f64 = 200e6;
/// Image writer (sequential copy of the VIDEO_TS files).
const IMAGE_BYTES_PER_SEC: f64 = 150e6;
/// Lead-in/out, OPC and closing the disc.
const BURN_FIXED_SECS: f64 = 45.0;
/// Extra time to close a dual-layer disc.
const DUAL_LAYER_FIXED_SECS: f64 = 60.0;

/// The five steps of a complete job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobPhase {
    Menus,
    Encoding,
    Authoring,
    Image,
    Burning,
}

impl JobPhase {
    pub const COUNT: usize = 5;

    /// 1-based position.
    pub fn number(self) -> usize {
        match self {
            JobPhase::Menus => 1,
            JobPhase::Encoding => 2,
            JobPhase::Authoring => 3,
            JobPhase::Image => 4,
            JobPhase::Burning => 5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            JobPhase::Menus => "Menüs erstellen",
            JobPhase::Encoding => "Videos encodieren",
            JobPhase::Authoring => "VOBs multiplexen (Authoring)",
            JobPhase::Image => "ISO erstellen (UDF 1.02)",
            JobPhase::Burning => "Disc brennen",
        }
    }
}

/// Remaining time report for the UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Eta {
    pub phase: JobPhase,
    /// 1-based phase number and the phase count ("[2/5]").
    pub phase_number: usize,
    pub phase_count: usize,
    pub phase_label: String,
    /// e.g. "Video 2 von 4".
    pub detail: Option<String>,
    /// Remaining seconds of the conversion (this phase and the following conversion phases);
    /// `None` while there is no basis for an estimate yet.
    pub remaining_secs: Option<f64>,
    /// Estimated image + burn (+ verify) time afterwards, if known.
    pub burn_secs: Option<f64>,
}

impl Eta {
    pub fn new(phase: JobPhase, detail: Option<String>, remaining_secs: Option<f64>, burn_secs: Option<f64>) -> Self {
        Eta {
            phase,
            phase_number: phase.number(),
            phase_count: JobPhase::COUNT,
            phase_label: phase.label().to_string(),
            detail,
            remaining_secs: remaining_secs.map(|s| s.max(0.0)),
            burn_secs,
        }
    }
}

/// Encoding time estimate from frames: all titles' frames (both runs for 2-pass) against the
/// summed frame rate of the running encodes (smoothed).
///
/// ffmpeg's `fps` is an average since the start of the run, which lags behind after the slow
/// start-up; it only seeds a job's rate until frame deltas over time are available.
#[derive(Debug, Clone)]
pub struct FrameEta {
    /// Frames each job has to process in total.
    totals: Vec<f64>,
    done: Vec<f64>,
    fps: Vec<f64>,
    /// Last (time, frames) sample per job for the measured rate.
    last: Vec<Option<(f64, f64)>>,
    finished: Vec<bool>,
    smoothed_fps: Option<f64>,
}

impl FrameEta {
    /// `frames_per_job`: output frames of each title; `passes`: 2 for 2-pass encoding.
    pub fn new(frames_per_job: &[f64], passes: u32) -> Self {
        let n = frames_per_job.len();
        FrameEta {
            totals: frames_per_job.iter().map(|f| f * passes as f64).collect(),
            done: vec![0.0; n],
            fps: vec![0.0; n],
            last: vec![None; n],
            finished: vec![false; n],
            smoothed_fps: None,
        }
    }

    /// Telemetry of job `index` at `now_secs` (any monotonic clock): frames done in total (all
    /// runs so far) and ffmpeg's fps.
    pub fn update(&mut self, index: usize, now_secs: f64, frames_done: f64, fps: f64) {
        let frames_done = frames_done.min(self.totals[index]);
        self.fps[index] = match self.last[index] {
            // Measured rate over at least half a second.
            Some((t, f)) if now_secs - t >= 0.5 => {
                self.last[index] = Some((now_secs, frames_done));
                ((frames_done - f) / (now_secs - t)).max(0.0)
            }
            Some(_) => self.fps[index],
            None => {
                self.last[index] = Some((now_secs, frames_done));
                fps.max(0.0)
            }
        };
        self.done[index] = frames_done;
        let total_fps: f64 = (0..self.fps.len()).filter(|&i| !self.finished[i]).map(|i| self.fps[i]).sum();
        if total_fps > 0.0 {
            // Exponential smoothing: fps jumps around between GOPs and at scene changes.
            self.smoothed_fps = Some(match self.smoothed_fps {
                Some(s) => 0.85 * s + 0.15 * total_fps,
                None => total_fps,
            });
        }
    }

    pub fn finish(&mut self, index: usize) {
        self.finished[index] = true;
        self.done[index] = self.totals[index];
        self.fps[index] = 0.0;
    }

    pub fn remaining_frames(&self) -> f64 {
        self.totals.iter().zip(&self.done).map(|(t, d)| (t - d).max(0.0)).sum()
    }

    /// Remaining seconds; `None` until the first fps report.
    pub fn remaining_secs(&self) -> Option<f64> {
        let fps = self.smoothed_fps?;
        (fps > 0.0).then(|| self.remaining_frames() / fps)
    }
}

/// dvdauthor run for `bytes` of title/menu VOBs.
pub fn authoring_secs(bytes: f64) -> f64 {
    2.0 + bytes / AUTHORING_BYTES_PER_SEC
}

/// Writing the disc image of `bytes`.
pub fn image_secs(bytes: f64) -> f64 {
    1.0 + bytes / IMAGE_BYTES_PER_SEC
}

/// Average speed of a burn at nominal `speed_x`: drives above 8x write CAV/zoned, starting
/// slower at the inner tracks, so a 16x burn averages roughly 11x.
pub fn effective_speed_x(speed_x: f64) -> f64 {
    if speed_x <= 8.0 {
        speed_x
    } else {
        8.0 + (speed_x - 8.0) * 0.4
    }
}

/// Burning `bytes` at `speed_x` (e.g. DVD 4x ≈ 5.54 MB/s), plus reading it back for
/// verification. DVD drives read at about 8x on average (CAV up to 16x), BD drives at about 6x.
pub fn burn_secs(medium: Medium, bytes: f64, speed_x: f64, verify: bool, dual_layer: bool) -> f64 {
    let one_x = medium.bytes_per_sec_1x();
    let write = bytes / (effective_speed_x(speed_x.max(1.0)) * one_x);
    let fixed = BURN_FIXED_SECS + if dual_layer { DUAL_LAYER_FIXED_SECS } else { 0.0 };
    let read = if verify { bytes / medium.verify_bytes_per_sec() } else { 0.0 };
    write + fixed + read
}

/// Default speed in x for the job estimate before a drive is known: DVD-5 at max (16x),
/// DVD-9 at 4x; BD-25 at 6x, BD-50 at 4x.
pub fn default_speed_x(medium: Medium, dual_layer: bool) -> f64 {
    match (medium, dual_layer) {
        (_, true) => 4.0,
        (Medium::Dvd, false) => 16.0,
        (Medium::Bd, false) => 6.0,
    }
}

/// Time from a finished VIDEO_TS / BDMV of `bytes` to a burnt (and verified) disc.
pub fn image_and_burn_secs(medium: Medium, bytes: f64, speed_x: f64, verify: bool, dual_layer: bool) -> f64 {
    image_secs(bytes) + burn_secs(medium, bytes, speed_x, verify, dual_layer)
}

/// Remaining time by extrapolating the elapsed time of a phase over its fraction done.
pub fn extrapolate(elapsed_secs: f64, fraction: f64) -> Option<f64> {
    (fraction > 0.02 && elapsed_secs > 1.0).then(|| elapsed_secs * (1.0 - fraction) / fraction)
}

/// "~14m 20s", "~45s", "~1h 05m".
pub fn format_remaining(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("~{}h {:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("~{}m {:02}s", s / 60, s % 60)
    } else {
        format!("~{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_format() {
        assert_eq!(format_remaining(860.0), "~14m 20s");
        assert_eq!(format_remaining(45.4), "~45s");
        assert_eq!(format_remaining(3900.0), "~1h 05m");
    }

    #[test]
    fn write_speeds() {
        // 4x ≈ 5.54 MB/s, 8x ≈ 11.08 MB/s, 16x ≈ 22.16 MB/s nominal.
        assert!((4.0 * DVD_1X_BYTES_PER_SEC / 1e6 - 5.54).abs() < 0.01);
        assert!((8.0 * DVD_1X_BYTES_PER_SEC / 1e6 - 11.08).abs() < 0.01);
        assert!((16.0 * DVD_1X_BYTES_PER_SEC / 1e6 - 22.16).abs() < 0.01);
        assert_eq!(effective_speed_x(4.0), 4.0);
        assert!((effective_speed_x(16.0) - 11.2).abs() < 1e-9);
    }

    #[test]
    fn burn_estimates() {
        // Full DVD-5 (4.37 GB) at 8x without verification: ~395 s writing + 45 s fixed.
        let dvd5 = 4.37e9;
        let t = burn_secs(Medium::Dvd, dvd5, 8.0, false, false);
        assert!((t - (dvd5 / 11.08e6 + 45.0)).abs() < 1.0, "{t}");
        // DVD-9 (7.95 GB) at 4x: ~24 min writing, plus DL closing.
        let t = burn_secs(Medium::Dvd, 7.95e9, 4.0, false, true);
        assert!((1530.0..1560.0).contains(&t), "{t}");
        // Verification adds a full read.
        assert!(burn_secs(Medium::Dvd, dvd5, 8.0, true, false) > burn_secs(Medium::Dvd, dvd5, 8.0, false, false) + 300.0);
        assert_eq!(default_speed_x(Medium::Dvd, false), 16.0);
        assert_eq!(default_speed_x(Medium::Dvd, true), 4.0);
        // A full BD-25 (23.3 GB) at 6x ≈ 27 MB/s: ~14 min writing + 45 s fixed.
        let bd = burn_secs(Medium::Bd, 23.3e9, 6.0, false, false);
        assert!((bd - (23.3e9 / (6.0 * 4_495_500.0) + 45.0)).abs() < 1.0, "{bd}");
        assert_eq!(default_speed_x(Medium::Bd, false), 6.0);
    }

    #[test]
    fn frame_eta() {
        // Two titles of 1000 frames, 2-pass → 4000 frames.
        let mut e = FrameEta::new(&[1000.0, 1000.0], 2);
        assert_eq!(e.remaining_secs(), None, "no telemetry yet");
        // First reports: ffmpeg's fps seeds the rate.
        e.update(0, 0.0, 500.0, 100.0);
        e.update(1, 0.0, 500.0, 100.0);
        // 3000 frames left at 100+100 fps (smoothed from 100 → 115).
        let r = e.remaining_secs().unwrap();
        assert!((3000.0 / 115.0 - r).abs() < 0.01, "{r}");
        // Later the measured rate wins over ffmpeg's lagging average: 400 frames in 1 s.
        e.update(0, 1.0, 900.0, 120.0);
        assert_eq!(e.fps[0], 400.0);
        // Reports closer than 0.5 s keep the last rate.
        e.update(0, 1.2, 950.0, 120.0);
        assert_eq!(e.fps[0], 400.0);
        e.finish(0);
        assert_eq!(e.remaining_frames(), 1500.0);
        // Overshooting reports are clamped.
        e.update(1, 2.0, 9999.0, 100.0);
        assert_eq!(e.remaining_frames(), 0.0);
    }

    #[test]
    fn eta_serialises_for_the_ui() {
        let eta = Eta::new(JobPhase::Encoding, Some("Video 2 von 4".into()), Some(860.0), None);
        let j = serde_json::to_string(&eta).unwrap();
        assert!(j.contains(r#""phase":"encoding""#) && j.contains(r#""phaseNumber":2"#) && j.contains(r#""phaseCount":5"#), "{j}");
        assert_eq!(extrapolate(10.0, 0.5), Some(10.0));
        assert_eq!(extrapolate(10.0, 0.0), None);
    }
}
