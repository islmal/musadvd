//! ffmpeg wrapper: source -> DVD-compliant MPEG-2 program stream (`{stem}.mpg`).
//!
//! Per title (see [`run_encode`]):
//! 1. text subtitles are extracted as `{stem}_sN.srt` and rendered natively ([`crate::textsub`]);
//! 2. with 2-pass encoding a video-only analysis run (`-pass 1`, output to `NUL`) writes
//!    `{stem}_2pass-0.log`;
//! 3. one ffmpeg run decodes the source once and writes MPEG-2 video (720x576/480, 4:3 or 16:9)
//!    plus one AC-3 track per selected audio track (192 kbps stereo / 448 kbps 5.1) straight
//!    into a DVD program stream (`-f dvd`, 2048-byte packs) – no elementary streams on disk;
//! 4. with subtitles that stream is piped through one native subpicture muxer per subtitle
//!    stream ([`crate::spumux`]) into `{stem}.mpg`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::bd::BdRate;
use crate::author::{ClipSource, MenuFrame, Resolution, TvStandard};
use crate::probe::{self, AudioStream, Crop, ExternalSubtitle, MediaInfo, SubtitleKind, SubtitleStream, Trim, VideoStream};
use crate::{subpic, textsub};
use crate::tools::{FfmpegProgress, ToolError, Toolchain};

/// VBV buffer of DVD-Video MPEG-2 (224 KiB).
pub const VBV_BUFFER_KBIT: u32 = 1835;
/// Peak video bitrate allowed by DVD-Video.
pub const MAX_VIDEO_PEAK_KBPS: u32 = 9800;
/// Total mux rate of DVD-Video program streams.
pub const DVD_MUX_RATE_KBPS: u32 = 10080;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Aspect {
    Standard,
    Wide,
}

impl Aspect {
    pub fn as_str(self) -> &'static str {
        match self {
            Aspect::Standard => "4:3",
            Aspect::Wide => "16:9",
        }
    }

    fn ratio(self) -> f64 {
        match self {
            Aspect::Standard => 4.0 / 3.0,
            Aspect::Wide => 16.0 / 9.0,
        }
    }

    /// 16:9 for sources wider than the midpoint between 4:3 and 16:9 (≈1.555).
    pub fn for_source(display_aspect: f64) -> Aspect {
        if display_aspect >= (4.0 / 3.0 + 16.0 / 9.0) / 2.0 {
            Aspect::Wide
        } else {
            Aspect::Standard
        }
    }
}

fn filter_of(filters: &[Option<String>], n: usize) -> Option<&str> {
    filters.get(n).and_then(|f| f.as_deref())
}

/// Size of the picture inside a coded frame of `frame` pixels shown at `target` (e.g. 720x576
/// PAL, 352x288 SIF): wider sources get letterbox bars, narrower ones pillarbox bars.
pub fn active_size(source_aspect: f64, target: Aspect, frame: (u32, u32)) -> (u32, u32) {
    let (fw, fh) = frame;
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    let t = target.ratio();
    if source_aspect > t * 1.01 {
        (fw, even(fh as f64 * t / source_aspect))
    } else if source_aspect < t / 1.01 {
        (even(fw as f64 * source_aspect / t), fh)
    } else {
        (fw, fh)
    }
}

/// Size that fills the whole coded frame at `target` aspect; the overhang of a wider/narrower
/// source is cropped centred afterwards (Pan & Scan).
pub fn cover_size(source_aspect: f64, target: Aspect, frame: (u32, u32)) -> (u32, u32) {
    let (fw, fh) = frame;
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    let t = target.ratio();
    if source_aspect > t * 1.01 {
        (even(fw as f64 * source_aspect / t), fh)
    } else if source_aspect < t / 1.01 {
        (fw, even(fh as f64 * t / source_aspect))
    } else {
        (fw, fh)
    }
}

/// `crop=w:h:x:y` for the picture section chosen by the user, if any.
fn crop_filter(v: &VideoStream) -> Option<String> {
    v.crop.map(|c| format!("crop={}:{}:{}:{}", c.w, c.h, c.x, c.y))
}

/// How the picture is fitted into the coded frame ("Video Skaliermethode").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScaleMode {
    /// Keep the aspect, add black bars.
    #[default]
    Letterbox,
    /// Keep the aspect, fill the frame and crop the overhang.
    PanScan,
    /// Distort the picture to the frame.
    Stretch,
}

/// swscale algorithm ("Skalierungsfilter").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScaleFilter {
    Bilinear,
    #[default]
    Bicubic,
    Lanczos,
}

impl ScaleFilter {
    pub fn flag(self) -> &'static str {
        match self {
            ScaleFilter::Bilinear => "bilinear",
            ScaleFilter::Bicubic => "bicubic",
            ScaleFilter::Lanczos => "lanczos",
        }
    }
}

/// "Video-Deinterlacing".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Deinterlace {
    /// `bwdif` only for sources flagged interlaced.
    #[default]
    Auto,
    /// `yadif` for every source.
    Yadif,
    Off,
}

/// Video processing options shared by the DVD and Blu-ray encodes.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoProcessing {
    #[serde(default)]
    pub scale: ScaleMode,
    #[serde(default)]
    pub filter: ScaleFilter,
    #[serde(default)]
    pub deinterlace: Deinterlace,
    /// 3D denoiser (`hqdn3d`) spatial luma strength (`0.0` / `None` = off, clamped `0.0..=12.0`).
    #[serde(default)]
    pub denoise: Option<f32>,
    /// `eq` brightness adjustment (neutral `0.0`, clamped `-1.0..=1.0`).
    #[serde(default)]
    pub brightness: Option<f32>,
    /// `eq` contrast multiplier (neutral `1.0`, clamped `0.0..=2.0`).
    #[serde(default)]
    pub contrast: Option<f32>,
    /// `eq` saturation multiplier (neutral `1.0`, clamped `0.0..=3.0`).
    #[serde(default)]
    pub saturation: Option<f32>,
}

impl VideoProcessing {
    /// Overlays per-title picture adjustments (`Some(...)`) onto these defaults.
    pub fn with_picture(
        mut self,
        denoise: Option<f32>,
        brightness: Option<f32>,
        contrast: Option<f32>,
        saturation: Option<f32>,
    ) -> Self {
        self.denoise = denoise.or(self.denoise);
        self.brightness = brightness.or(self.brightness);
        self.contrast = contrast.or(self.contrast);
        self.saturation = saturation.or(self.saturation);
        self
    }

    /// Whether any non-neutral denoise or colour filter is active.
    pub fn has_picture_filters(self) -> bool {
        !picture_filters(self).is_empty()
    }
}

/// Denoise (`hqdn3d`) and tonal/colour (`eq`) filters active on `vp`, if any.
fn picture_filters(vp: VideoProcessing) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(s) = vp.denoise.filter(|s| s.is_finite() && *s > 0.0) {
        let luma = s.clamp(0.0, 12.0);
        f.push(format!("hqdn3d={luma:.2}:{:.2}:{:.2}:{:.2}", luma * 0.75, luma * 1.5, luma * 1.125));
    }
    let eq: Vec<String> = [
        ("brightness", vp.brightness, 0.0, -1.0, 1.0),
        ("contrast", vp.contrast, 1.0, 0.0, 2.0),
        ("saturation", vp.saturation, 1.0, 0.0, 3.0),
    ]
    .into_iter()
    .filter_map(|(name, val, neutral, min, max)| {
        let v = val.filter(|v| v.is_finite() && (*v - neutral).abs() > 1e-4)?;
        Some(format!("{name}={:.2}", v.clamp(min, max)))
    })
    .collect();
    if !eq.is_empty() {
        f.push(format!("eq={}", eq.join(":")));
    }
    f
}

/// Deinterlacer chosen by `vp` for this source, if any.
fn deint_filter(v: &VideoStream, vp: VideoProcessing) -> Option<String> {
    match vp.deinterlace {
        Deinterlace::Auto if v.is_interlaced() => Some("bwdif=mode=send_frame".to_string()),
        Deinterlace::Yadif => Some("yadif=mode=send_frame".to_string()),
        _ => None,
    }
}

/// PQ/HLG to BT.709 SDR (only for HDR sources).
fn tonemap_filter(v: &VideoStream) -> Option<&'static str> {
    v.hdr.then_some("zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv")
}

/// Head shared by encode and preview: deinterlacer, tonemap (HDR only), user crop and, with
/// `picture`, the picture filters.
fn source_chain(v: &VideoStream, vp: VideoProcessing, picture: bool) -> Vec<String> {
    let mut f: Vec<String> = deint_filter(v, vp).into_iter().collect();
    f.extend(tonemap_filter(v).map(String::from));
    f.extend(crop_filter(v));
    if picture {
        f.extend(picture_filters(vp));
    }
    f
}

/// Whether the picture filters run before the scale to `w`×`h`: they cost per pixel (`hqdn3d`
/// was 4× slower at 1080p than after a DVD downscale), so they run on the smaller picture.
fn picture_before_scale(v: &VideoStream, w: u32, h: u32) -> bool {
    let (sw, sh) = v.crop.map_or((v.width, v.height), |c| (c.w, c.h));
    u64::from(w) * u64::from(h) >= u64::from(sw) * u64::from(sh)
}

/// Filter chain of the live player: 8-bit 480p square-pixel picture of the cut/cropped source.
/// Tonemapping runs after the downscale (real-time at 4K); the encode tonemaps first, the small
/// difference is acceptable for a preview.
pub fn preview_filter(v: &VideoStream, vp: VideoProcessing, height: u32) -> String {
    let w = (v.display_aspect * height as f64 / 2.0).round() as u32 * 2;
    let scale = format!("scale={w}:{height}:flags={}", vp.filter.flag());
    let mut f = if v.hdr {
        let mut f: Vec<String> = deint_filter(v, vp).into_iter().collect();
        f.extend(crop_filter(v));
        f.push(scale);
        f.extend(tonemap_filter(v).map(String::from));
        f.extend(picture_filters(vp));
        f
    } else {
        let early = picture_before_scale(v, w, height);
        let mut f = source_chain(v, vp, early);
        f.push(scale);
        if !early {
            f.extend(picture_filters(vp));
        }
        f
    };
    f.push("setsar=1".to_string());
    f.push("format=yuv420p".to_string());
    f.join(",")
}

/// Shared chain: [`source_chain`], `fps`, scale, bars/crop to the coded `frame`.
/// Returns the filters before the aspect tag (`setdar`/`setsar`) and `format`.
fn fit_chain(v: &VideoStream, aspect: Aspect, frame: (u32, u32), fps: &str, matrix: &str, vp: VideoProcessing) -> Vec<String> {
    let (fw, fh) = frame;
    let ((w, h), fit) = match vp.scale {
        ScaleMode::Letterbox => {
            let s = active_size(v.display_aspect, aspect, frame);
            (s, (s != frame).then(|| format!("pad={fw}:{fh}:(ow-iw)/2:(oh-ih)/2:black")))
        }
        ScaleMode::PanScan => {
            let s = cover_size(v.display_aspect, aspect, frame);
            (s, (s != frame).then(|| format!("crop={fw}:{fh}")))
        }
        ScaleMode::Stretch => (frame, None),
    };
    let early = picture_before_scale(v, w, h);
    let mut f = source_chain(v, vp, early);
    f.push(format!("fps={fps}"));
    f.push(format!("scale={w}:{h}:flags={}:out_color_matrix={matrix}:out_range=tv", vp.filter.flag()));
    if !early {
        f.extend(picture_filters(vp));
    }
    f.extend(fit);
    f
}

/// ffmpeg `-vf` chain: deinterlace, frame rate of the TV standard, scale into the coded frame
/// of `resolution` in BT.601 (bars, crop or stretch per `vp.scale`).
pub fn video_filter(v: &VideoStream, aspect: Aspect, tv: TvStandard, resolution: Resolution, vp: VideoProcessing) -> String {
    let mut f = fit_chain(v, aspect, resolution.frame_size(tv), tv.frame_rate(), "bt601", vp);
    f.push(format!("setdar={}", aspect.as_str().replace(':', "/")));
    f.push("format=yuv420p".to_string());
    f.join(",")
}

/// AC-3 settings for a source track: (bitrate kbps, output channels).
pub fn ac3_settings(channels: u32) -> (u32, u32) {
    if channels >= 5 {
        (448, 6)
    } else {
        (192, 2)
    }
}

/// How audio tracks are written ("Audio-Modus").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// Copy compliant AC-3/DTS tracks, re-encode everything else to AC-3.
    #[default]
    Auto,
    /// Always AC-3.
    Ac3,
    /// Uncompressed LPCM (DVD: not reliable with ffmpeg's muxer, falls back to AC-3).
    Lpcm,
}

/// Disc format the audio has to comply with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTarget {
    Dvd,
    Bluray,
    /// AVCHD on DVD media: AC-3 and LPCM only.
    Avchd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    Ac3,
    Dts,
    Lpcm,
}

/// What happens to one audio track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOut {
    /// Stream copy (`bsf`: e.g. `dca_core` to strip DTS-HD extensions).
    Copy { kind: AudioKind, bsf: Option<&'static str>, kbps: u32, ch: u32 },
    Ac3 { kbps: u32, ch: u32 },
    /// Uncompressed PCM written with ffmpeg codec `codec` at `rate` Hz (48 kHz when encoded, the
    /// source's rate for a lossless repack).
    Pcm { codec: &'static str, ch: u32, rate: u32 },
}

impl AudioOut {
    pub fn kbps(self) -> u32 {
        match self {
            AudioOut::Copy { kbps, .. } | AudioOut::Ac3 { kbps, .. } => kbps,
            // Blu-ray LPCM stores an odd channel count padded to even.
            AudioOut::Pcm { codec, ch, rate } => rate / 1000 * if codec == "pcm_s24le" { 24 } else { 16 } * (ch + ch % 2),
        }
    }

    pub fn channels(self) -> u32 {
        match self {
            AudioOut::Copy { ch, .. } | AudioOut::Ac3 { ch, .. } | AudioOut::Pcm { ch, .. } => ch,
        }
    }

    pub fn kind(self) -> AudioKind {
        match self {
            AudioOut::Copy { kind, .. } => kind,
            AudioOut::Ac3 { .. } => AudioKind::Ac3,
            AudioOut::Pcm { .. } => AudioKind::Lpcm,
        }
    }

    /// Stereo or 5.1 label for menus and reports.
    pub fn layout_label(self) -> &'static str {
        match self.channels() {
            1 => "Mono",
            2 => "Stereo",
            6 => "5.1",
            8 => "7.1",
            _ => "Multichannel",
        }
    }

    /// dvdauthor's `<audio format=…>` value.
    pub fn dvdauthor_format(self) -> &'static str {
        match self.kind() {
            AudioKind::Ac3 => "ac3",
            AudioKind::Dts => "dts",
            AudioKind::Lpcm => "pcm",
        }
    }
}

/// Stream copy of `a` for a DVD (`None` = must be re-encoded); Blu-ray: [`bd_audio_copy`].
fn copy_plan(a: &AudioStream, target: AudioTarget) -> Option<AudioOut> {
    let kbps = a.bit_rate.map(|b| b.div_ceil(1000));
    let rate_ok = a.sample_rate == Some(48_000);
    let profile = a.profile.as_deref().unwrap_or("");
    match (a.codec.as_str(), target) {
        ("ac3", _) if rate_ok && a.channels <= 6 => {
            let kbps = kbps?;
            let max = if target == AudioTarget::Dvd { 448 } else { 640 };
            (kbps <= max).then_some(AudioOut::Copy { kind: AudioKind::Ac3, bsf: None, kbps, ch: a.channels })
        }
        // DTS-HD / 96 kHz: only the 48 kHz core fits a DVD.
        ("dts", AudioTarget::Dvd) if profile.contains("HD") || profile.contains("96/24") => {
            Some(AudioOut::Copy { kind: AudioKind::Dts, bsf: Some("dca_core"), kbps: 1536, ch: a.channels.min(6) })
        }
        ("dts", AudioTarget::Dvd) if rate_ok && a.channels <= 6 => {
            let kbps = kbps.unwrap_or(1536);
            (kbps <= 1536).then_some(AudioOut::Copy { kind: AudioKind::Dts, bsf: None, kbps, ch: a.channels })
        }
        _ => None,
    }
}

/// How Blu-ray / AVCHD audio tracks are written ("copy" = stream copy or lossless LPCM repack).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BdAudioMode {
    /// Copy compliant tracks ([`bd_audio_copy`]), encode the rest.
    #[default]
    Auto,
    /// Never copy.
    Encode,
    /// Copy every track; the build fails when one does not pass [`bd_audio_copy`].
    Strict,
}

impl BdAudioMode {
    pub fn from_setting(s: &str) -> Self {
        match s {
            "encode" => BdAudioMode::Encode,
            "strict" => BdAudioMode::Strict,
            _ => BdAudioMode::Auto,
        }
    }
}

/// Copy of `a` onto a Blu-ray / AVCHD disc: AC-3, DTS (DTS-HD as its core), or a lossless LPCM
/// repack of PCM/FLAC. `Err` is the English reason it cannot be copied.
pub fn bd_audio_copy(a: &AudioStream, target: AudioTarget) -> Result<AudioOut, String> {
    let avchd = match target {
        AudioTarget::Dvd => return Err("not a Blu-ray target".into()),
        t => t == AudioTarget::Avchd,
    };
    let rate = a.sample_rate.unwrap_or(0);
    let kbps = a.bit_rate.map(|b| b.div_ceil(1000));
    let profile = a.profile.as_deref().unwrap_or("");
    let codec = a.codec.as_str();
    match codec {
        "ac3" => {
            if rate != 48_000 || a.channels > 6 {
                return Err(format!("AC-3 needs 48 kHz and at most 6 channels (is {rate} Hz, {} ch)", a.channels));
            }
            match kbps {
                None => Err("the AC-3 bit rate is unknown".into()),
                Some(k) if k > 640 => Err(format!("AC-3 at {k} kbit/s exceeds 640 kbit/s")),
                Some(k) => Ok(AudioOut::Copy { kind: AudioKind::Ac3, bsf: None, kbps: k, ch: a.channels }),
            }
        }
        "dts" if avchd => Err("DTS cannot be copied to AVCHD".into()),
        "dts" => {
            if ![48_000, 96_000, 192_000].contains(&rate) || a.channels > 8 {
                return Err(format!("DTS needs 48/96/192 kHz and at most 8 channels (is {rate} Hz, {} ch)", a.channels));
            }
            if profile.contains("HD") || profile.contains("96/24") {
                // The extension substream is not Blu-ray DTS-HD; the core alone is a valid DTS track.
                let k = kbps.map_or(1536, |k| k.min(1536));
                Ok(AudioOut::Copy { kind: AudioKind::Dts, bsf: Some("dca_core"), kbps: k, ch: a.channels.min(6) })
            } else {
                Ok(AudioOut::Copy { kind: AudioKind::Dts, bsf: None, kbps: kbps.unwrap_or(1536), ch: a.channels })
            }
        }
        c if c == "flac" || c.starts_with("pcm_") => {
            let bps = a.bits_per_raw_sample;
            let bits = match c {
                "flac" => bps.ok_or("the FLAC bit depth is unknown")?,
                "pcm_s8" | "pcm_u8" => 8,
                "pcm_s16le" | "pcm_s16be" | "pcm_u16le" | "pcm_u16be" => 16,
                "pcm_s24le" | "pcm_s24be" | "pcm_u24le" | "pcm_u24be" => 24,
                "pcm_s32le" | "pcm_s32be" => bps.ok_or("32-bit PCM is not lossless on Blu-ray")?,
                "pcm_bluray" | "pcm_dvd" => bps.unwrap_or(24),
                _ => return Err(format!("{c} cannot be copied to Blu-ray")),
            };
            if bits > 24 {
                return Err(format!("{bits}-bit audio is not lossless as Blu-ray LPCM (24 bit maximum)"));
            }
            let ch = a.channels;
            let rate_ok = if avchd { rate == 48_000 } else { [48_000, 96_000, 192_000].contains(&rate) };
            if !rate_ok || ch == 0 || ch > if avchd { 2 } else { 8 } || (rate == 192_000 && ch > 6) || (avchd && bits > 16) {
                let what = if avchd { "AVCHD LPCM needs 48 kHz, 16 bit and at most 2 channels" } else { "Blu-ray LPCM needs 48/96/192 kHz (192 kHz: at most 6 channels) and at most 8 channels" };
                return Err(format!("{what} (is {rate} Hz, {bits} bit, {ch} ch)"));
            }
            Ok(AudioOut::Pcm { codec: if bits <= 16 { "pcm_s16le" } else { "pcm_s24le" }, ch, rate })
        }
        other => Err(format!("{other} cannot be copied to Blu-ray")),
    }
}

/// Output plan of one source audio track. `filtered`: a filter (loudnorm) has to run on it or the
/// title is trimmed, which rules out a stream copy (ffmpeg's input seeking does not cut copied
/// audio at the start, it would drift against the re-encoded video).
pub fn plan_audio(a: &AudioStream, target: AudioTarget, mode: AudioMode, filtered: bool) -> AudioOut {
    plan_audio_bd(a, target, mode, BdAudioMode::Auto, filtered)
}

/// [`plan_audio`] with the Blu-ray / AVCHD copy policy `bd` (ignored on DVD).
pub fn plan_audio_bd(a: &AudioStream, target: AudioTarget, mode: AudioMode, bd: BdAudioMode, filtered: bool) -> AudioOut {
    if !filtered {
        if target == AudioTarget::Dvd {
            if mode == AudioMode::Auto {
                if let Some(copy) = copy_plan(a, target) {
                    return copy;
                }
            }
        } else if bd != BdAudioMode::Encode {
            // Auto keeps the disc format of an explicit --audio choice; Strict copies any format.
            let keeps_format = |c: &AudioOut| bd == BdAudioMode::Strict || match mode {
                AudioMode::Auto => true,
                AudioMode::Ac3 => c.kind() == AudioKind::Ac3,
                AudioMode::Lpcm => c.kind() == AudioKind::Lpcm,
            };
            if let Some(copy) = bd_audio_copy(a, target).ok().filter(keeps_format) {
                return copy;
            }
        }
    }
    match (mode, target) {
        (AudioMode::Lpcm, AudioTarget::Bluray) => AudioOut::Pcm { codec: "pcm_s24le", ch: a.channels.clamp(1, 8), rate: 48_000 },
        (AudioMode::Lpcm, AudioTarget::Avchd) => AudioOut::Pcm { codec: "pcm_s16le", ch: 2, rate: 48_000 },
        _ => {
            let (kbps, ch) = ac3_settings(a.channels);
            AudioOut::Ac3 { kbps, ch }
        }
    }
}

/// Silent filler track of a Blu-ray title that has fewer tracks than the disc.
pub const SILENCE_OUT: AudioOut = AudioOut::Ac3 { kbps: BD_SILENCE_KBPS, ch: 2 };

/// Output options of audio track `n`: codec, bitrate/channels, optional filter.
fn audio_track_args(n: usize, out: AudioOut, filter: Option<&str>) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |k: String, v: String| a.extend([OsString::from(k), OsString::from(v)]);
    match out {
        AudioOut::Copy { bsf, .. } => {
            push(format!("-c:a:{n}"), "copy".into());
            if let Some(bsf) = bsf {
                push(format!("-bsf:a:{n}"), bsf.into());
            }
        }
        AudioOut::Ac3 { kbps, ch } => {
            push(format!("-c:a:{n}"), "ac3".into());
            push(format!("-ar:a:{n}"), "48000".into());
            push(format!("-b:a:{n}"), format!("{kbps}k"));
            push(format!("-ac:a:{n}"), ch.to_string());
        }
        AudioOut::Pcm { codec, ch, rate } => {
            push(format!("-c:a:{n}"), codec.into());
            push(format!("-ar:a:{n}"), rate.to_string());
            push(format!("-ac:a:{n}"), ch.to_string());
        }
    }
    if let Some(f) = filter.filter(|_| !matches!(out, AudioOut::Copy { .. })) {
        push(format!("-filter:a:{n}"), f.to_string());
    }
    a
}

/// Resampler of re-encoded DVD audio: 48 kHz with gap filling (see `EncodeJob::encode_args`).
const DVD_AUDIO_SYNC: &str = "aresample=48000:async=1:min_hard_comp=0.01";

/// EBU R128 target: integrated loudness, true peak and loudness range.
const LOUDNORM_TARGET: &str = "I=-23:TP=-1:LRA=20";

/// Result of loudnorm's measuring run for one audio track.
#[derive(Debug, Clone, PartialEq)]
pub struct Loudness {
    input_i: f64,
    input_tp: f64,
    input_lra: f64,
    input_thresh: f64,
    target_offset: f64,
}

impl Loudness {
    /// Measured integrated loudness in LUFS.
    pub fn integrated(&self) -> f64 {
        self.input_i
    }

    /// Second-run filter: applies the measured values linearly and brings the track back to 48 kHz
    /// (loudnorm itself resamples to 192 kHz).
    pub fn filter(&self) -> String {
        format!(
            "loudnorm={LOUDNORM_TARGET}:measured_I={:.2}:measured_TP={:.2}:measured_LRA={:.2}:measured_thresh={:.2}:offset={:.2}:linear=true,aresample=48000",
            self.input_i, self.input_tp, self.input_lra, self.input_thresh, self.target_offset
        )
    }
}

/// Reads the JSON block `loudnorm=print_format=json` prints into ffmpeg's log. `None` when it is
/// missing or silent (`-inf`): such a track is left alone.
pub fn parse_loudnorm_report(log: &str) -> Option<Loudness> {
    let start = log.rfind('{')?;
    let end = start + log[start..].find('}')?;
    let v: serde_json::Value = serde_json::from_str(&log[start..=end]).ok()?;
    let num = |k: &str| v.get(k)?.as_str()?.trim().parse::<f64>().ok().filter(|x| x.is_finite());
    Some(Loudness {
        input_i: num("input_i")?,
        input_tp: num("input_tp")?,
        input_lra: num("input_lra")?,
        input_thresh: num("input_thresh")?,
        target_offset: num("target_offset")?,
    })
}

/// Arguments of the measuring run for audio stream `index` of `info` (the used section only).
pub fn loudnorm_measure_args(info: &MediaInfo, index: u32) -> Vec<OsString> {
    let mut a = info.input_args();
    a.extend(["-map".into(), format!("0:{index}").into(), "-vn".into(), "-sn".into(), "-af".into()]);
    a.extend([format!("loudnorm={LOUDNORM_TARGET}:print_format=json").into(), "-f".into(), "null".into(), NULL_DEVICE.into()]);
    a
}

/// Loudness filter per audio track (`None` for silent filler tracks and tracks that cannot be
/// measured): one measuring run each, the first half of loudnorm's two-pass mode.
pub async fn measure_loudnorm(tools: &Toolchain, info: &MediaInfo, tracks: &[Option<&AudioStream>]) -> Result<Vec<Option<String>>, ToolError> {
    let mut filters = Vec::new();
    for track in tracks {
        filters.push(match track {
            Some(t) => {
                let log = tools.ffmpeg_report(&loudnorm_measure_args(info, t.index)).await?;
                parse_loudnorm_report(&log).map(|l| l.filter())
            }
            None => None,
        });
    }
    Ok(filters)
}

/// Last `crop=w:h:x:y` that ffmpeg's `cropdetect` logged (with `reset=0` the largest picture seen).
pub fn parse_cropdetect(log: &str) -> Option<(u32, u32, u32, u32)> {
    let line = log.lines().rev().find(|l| l.contains("crop="))?;
    let spec = &line[line.rfind("crop=")? + 5..];
    let mut n = spec.split_whitespace().next()?.split(':').map(|v| v.trim().parse::<u32>());
    Some((n.next()?.ok()?, n.next()?.ok()?, n.next()?.ok()?, n.next()?.ok()?))
}

/// The crop that keeps everything every rectangle (w, h, x, y) shows: their bounding box inside
/// the `frame` (width, height).
pub fn union_crop(rects: &[(u32, u32, u32, u32)], frame: (u32, u32)) -> Crop {
    let x0 = rects.iter().map(|r| r.2).min().unwrap_or(0);
    let y0 = rects.iter().map(|r| r.3).min().unwrap_or(0);
    let x1 = rects.iter().map(|r| r.2 + r.0).max().unwrap_or(frame.0).min(frame.0);
    let y1 = rects.iter().map(|r| r.3 + r.1).max().unwrap_or(frame.1).min(frame.1);
    Crop { top: y0, bottom: frame.1 - y1, left: x0, right: frame.0 - x1 }
}

/// Finds the black bars of the video of `info`: `cropdetect` over 2 s at 25, 50 and 75 % of the
/// used section, the largest picture of the three. All zero when nothing was detected.
pub async fn detect_crop(tools: &Toolchain, info: &MediaInfo) -> Result<Crop, ToolError> {
    let v = info.video.as_ref().expect("probed file has video");
    let mut rects = Vec::new();
    for frac in [0.25, 0.5, 0.75] {
        let at = info.trim_start() + (info.duration_secs * frac - 1.0).max(0.0);
        let mut a: Vec<OsString> = vec!["-ss".into(), format!("{at:.3}").into(), "-t".into(), "2".into(), "-i".into(), info.path.clone().into_os_string()];
        a.extend(["-map".into(), format!("0:{}", v.index).into(), "-an".into(), "-sn".into(), "-vf".into(), "cropdetect=limit=24:round=2:reset=0".into()]);
        a.extend(["-f".into(), "null".into(), NULL_DEVICE.into()]);
        if let Some(r) = parse_cropdetect(&tools.ffmpeg_report(&a).await?) {
            rects.push(r);
        }
    }
    Ok(if rects.is_empty() { Crop::default() } else { union_crop(&rects, (v.width, v.height)) })
}

/// Peak video rate so that video + all audio stays within the DVD mux rate.
pub fn max_video_rate(audio_kbps_total: u32) -> u32 {
    MAX_VIDEO_PEAK_KBPS.min(DVD_MUX_RATE_KBPS.saturating_sub(audio_kbps_total + 300))
}

/// Output device ffmpeg writes to when only pass-1 statistics are wanted.
pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Parses the last `average:<dB>` value emitted by FFmpeg's `psnr` filter.
/// Maps `inf` (identical/lossless) to `99.9` so the value stays finite in JSON.
pub fn parse_psnr_average(log: &str) -> Option<f64> {
    for line in log.lines().rev() {
        if !line.contains("PSNR") {
            continue;
        }
        let Some(pos) = line.rfind("average:") else {
            continue;
        };
        let Some(tok) = line[pos + "average:".len()..].split_whitespace().next() else {
            continue;
        };
        if tok.eq_ignore_ascii_case("inf") {
            return Some(99.9);
        }
        if let Some(v) = tok.parse::<f64>().ok().filter(|v| v.is_finite()) {
            return Some(v);
        }
    }
    None
}

/// Maps an average PSNR score in dB to a short quality verdict.
pub fn psnr_verdict(psnr_db: f64) -> &'static str {
    if psnr_db >= 40.0 {
        "excellent"
    } else if psnr_db >= 36.0 {
        "good"
    } else if psnr_db >= 32.0 {
        "fair"
    } else {
        "poor"
    }
}

/// Arguments for running FFmpeg's `psnr` filter between an encoded `sample` and the matching
/// `[at_secs, at_secs + duration_secs)` window of `source` filtered with `vf`.
pub fn quality_psnr_args(
    sample: &Path,
    source: &Path,
    v_index: u32,
    at_secs: f64,
    duration_secs: f64,
    vf: &str,
) -> Vec<OsString> {
    let ref_chain = if vf.trim().is_empty() {
        "setpts=PTS-STARTPTS".to_string()
    } else {
        format!("{vf},setpts=PTS-STARTPTS")
    };
    let lavfi = format!(
        "[0:v]setpts=PTS-STARTPTS,format=yuv420p[dist];[1:{v_index}]{ref_chain}[ref];[dist][ref]psnr=shortest=1"
    );
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-i".into(),
        sample.to_path_buf().into_os_string(),
        "-ss".into(),
        format!("{:.3}", at_secs.max(0.0)).into(),
        "-t".into(),
        format!("{:.3}", duration_secs.max(0.0)).into(),
        "-i".into(),
        source.to_path_buf().into_os_string(),
        "-lavfi".into(),
        lavfi.into(),
        "-f".into(),
        "null".into(),
        NULL_DEVICE.into(),
    ]
}

/// Which encoding run a progress report belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodePhase {
    /// The only run of a 1-pass encode.
    Single,
    /// 2-pass: video analysis, writes only the statistics log.
    Analysis,
    /// 2-pass: final encode using the statistics.
    Final,
}

/// Everything needed to encode one title.
#[derive(Debug, Clone)]
pub struct EncodeJob<'a> {
    pub info: &'a MediaInfo,
    pub audio: Vec<&'a AudioStream>,
    /// Subtitles in DVD stream order: text ones are rendered by spumux, bitmap ones (PGS,
    /// VobSub) are decoded by [`subpic`] into images for spumux.
    pub subtitles: Vec<&'a SubtitleStream>,
    pub video_kbps: u32,
    pub aspect: Aspect,
    /// Work directory; all outputs are written here.
    pub dir: PathBuf,
    /// File name stem, e.g. `title_01`.
    pub stem: String,
    /// 2-pass video encoding (analysis run + final run) for a more exact average bitrate.
    pub two_pass: bool,
    /// Draft quality: no trellis quantisation (about 40 % faster, slightly lower quality).
    pub fast: bool,
    pub tv: TvStandard,
    /// Coded frame size (Full D1, Half D1, SIF).
    pub resolution: Resolution,
    pub video: VideoProcessing,
    /// Hardware decoder for the source (`-hwaccel`), see [`Toolchain::probe_hwaccel`].
    pub hwaccel: Option<&'static str>,
    pub audio_mode: AudioMode,
    /// Per-track audio mode overrides; if empty or index not found, falls back to `audio_mode`.
    pub audio_modes: Vec<AudioMode>,
    /// Audio filter (loudnorm) per audio track; a filtered track is never copied. Missing
    /// entries mean no filter.
    pub audio_filters: Vec<Option<String>>,
    /// Stream copy of the source video (passed [`dvd_video_copy`]): no filter, no hwaccel, no 2-pass.
    pub copy_video: bool,
    /// Styling and sync of the text subtitles (Subtitles tab).
    pub subtitle_style: textsub::SubtitleStyle,
}

/// 2-pass statistics files (`{stem}_2pass*.log|.mbtree|.temp`) in `dir`.
fn passlog_files_in(dir: &Path, stem: &str) -> Vec<PathBuf> {
    let prefix = format!("{stem}_2pass");
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with(&prefix) && (n.ends_with(".log") || n.ends_with(".mbtree") || n.ends_with(".temp"))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cleanup_passlogs_in(dir: &Path, stem: &str) -> std::io::Result<()> {
    for f in passlog_files_in(dir, stem) {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

impl EncodeJob<'_> {
    /// Whether this job actually runs a 2-pass analysis + final encode (disabled for stream-copied video).
    pub fn uses_two_pass(&self) -> bool {
        self.two_pass && !self.copy_video
    }

    /// `-passlogfile` prefix; ffmpeg appends `-0.log` (and `.mbtree` for x264).
    pub fn passlog_prefix(&self) -> PathBuf {
        self.dir.join(format!("{}_2pass", self.stem))
    }

    /// 2-pass statistics files currently present in the work directory.
    pub fn passlog_files(&self) -> Vec<PathBuf> {
        passlog_files_in(&self.dir, &self.stem)
    }

    /// Deletes the 2-pass statistics files.
    pub fn cleanup_passlogs(&self) -> std::io::Result<()> {
        cleanup_passlogs_in(&self.dir, &self.stem)
    }

    /// MPEG-2 video settings for encoding or Pass-1 statistics collection (`analysis = true`).
    /// Keeps frame rate, GOP structure, and rate control identical across passes.
    fn video_codec_args(&self, analysis: bool) -> Vec<OsString> {
        let s = |x: &str| OsString::from(x);
        let maxrate = max_video_rate(self.audio_kbps_total()).max(self.video_kbps);
        // BT.601 colour tags: 625-line (bt470bg) for PAL, 525-line (smpte170m) for NTSC.
        let (primaries, trc, matrix) = match self.tv {
            TvStandard::Pal => ("bt470bg", "gamma28", "bt470bg"),
            TvStandard::Ntsc => ("smpte170m", "smpte170m", "smpte170m"),
        };
        let gop = self.tv.gop().to_string();
        let mbd = if analysis { "simple" } else { "rd" };
        let mut a: Vec<OsString> = [
            "-c:v", "mpeg2video", "-threads", "0", "-pix_fmt", "yuv420p", "-r", self.tv.frame_rate(), "-g", &gop, "-bf", "2",
            "-dc", "9", "-qmin", "2", "-mbd", mbd,
            // Closed GOPs: dvdauthor warns on every chapter cell otherwise; mpeg2video refuses +cgop with scene-change detection.
            "-flags", "+cgop", "-sc_threshold", "1000000000",
        ]
        .map(s)
        .to_vec();
        if analysis {
            a.extend(
                ["-trellis", "0", "-dia_size", "1", "-subq", "1", "-cmp", "sad", "-subcmp", "sad", "-mbcmp", "sad"]
                    .map(s),
            );
        } else if !self.fast {
            a.extend(["-trellis", "1", "-intra_vlc", "1"].map(s));
        }
        a.extend(["-color_primaries", primaries, "-color_trc", trc, "-colorspace", matrix].map(s));
        a.extend([
            s("-b:v"),
            format!("{}k", self.video_kbps).into(),
            s("-maxrate"),
            format!("{maxrate}k").into(),
            s("-bufsize"),
            format!("{VBV_BUFFER_KBIT}k").into(),
            s("-aspect"),
            self.aspect.as_str().into(),
        ]);
        a
    }

    /// Input and video stream selection/filter, shared by all runs.
    fn video_input_args(&self) -> Vec<OsString> {
        let v = self.info.video.as_ref().expect("probed file has video");
        let mut a: Vec<OsString> = vec!["-y".into()];
        if self.copy_video {
            a.extend(["-fflags".into(), "+genpts".into()]);
        } else if let Some(hw) = self.hwaccel {
            a.extend(["-hwaccel".into(), hw.into()]);
        }
        a.extend(self.info.input_args());
        a.extend(["-map".into(), format!("0:{}", v.index).into()]);
        if !self.copy_video {
            a.extend(["-vf".into(), video_filter(v, self.aspect, self.tv, self.resolution, self.video).into()]);
        }
        a
    }

    /// Arguments of the 2-pass analysis run: fast video-only statistics collection, no real output.
    pub fn first_pass_args(&self) -> Vec<OsString> {
        let mut a = self.video_input_args();
        a.extend(self.video_codec_args(true));
        a.extend([
            "-pass".into(),
            "1".into(),
            "-passlogfile".into(),
            self.passlog_prefix().into_os_string(),
            "-an".into(),
            "-sn".into(),
            "-f".into(),
            "rawvideo".into(),
            NULL_DEVICE.into(),
        ]);
        a
    }
    pub fn srt(&self, n: usize) -> String {
        subtitle_file(&self.stem, n, "srt")
    }
    /// Raw PGS stream of subtitle `n`.
    pub fn sup(&self, n: usize) -> String {
        subtitle_file(&self.stem, n, "sup")
    }
    /// spumux XML of subtitle stream `n`.
    pub fn subtitle_xml(&self, n: usize) -> String {
        format!("{}_s{}.xml", self.stem, n + 1)
    }
    /// Final title VOB source for dvdauthor.
    pub fn vob(&self) -> String {
        format!("{}.mpg", self.stem)
    }

    /// Output plan of every audio track.
    pub fn audio_outs(&self) -> Vec<AudioOut> {
        self.audio
            .iter()
            .enumerate()
            .map(|(n, a)| {
                let mode = self.audio_modes.get(n).copied().unwrap_or(self.audio_mode);
                plan_audio(a, AudioTarget::Dvd, mode, filter_of(&self.audio_filters, n).is_some() || self.info.trim.is_some())
            })
            .collect()
    }

    pub fn audio_kbps_total(&self) -> u32 {
        self.audio_outs().iter().map(|o| o.kbps()).sum()
    }

    /// Extraction of the text subtitles as SRT and of PGS subtitles as raw `.sup` (stream copy
    /// speed, needed before the encode starts); `None` when no subtitle needs it (VobSub is read
    /// by ffprobe, see [`subpic::vobsub_probe_args`]).
    pub fn subtitle_args(&self) -> Option<Vec<OsString>> {
        let tracks: Vec<(usize, &SubtitleStream)> = self.subtitles.iter().copied().enumerate().collect();
        extract_subtitle_args(self.info, &tracks, &self.dir, &self.stem)
    }

    /// Where the source picture lies in the DVD frame (subpictures use the full 720-wide raster,
    /// also for Half D1/SIF titles).
    fn subpicture_target(&self, canvas: (u32, u32)) -> subpic::Target {
        let v = self.info.video.as_ref().expect("probed file has video");
        let (fw, fh) = self.tv.size();
        let (aw, ah) = active_size(v.display_aspect, self.aspect, (fw, fh));
        let target = subpic::Target::new(self.tv, ((fw - aw) / 2, (fh - ah) / 2, aw, ah), canvas, (v.width, v.height));
        match v.crop {
            Some(c) => target.cropped((c.x, c.y, c.w, c.h), (v.width, v.height)),
            None => target,
        }
    }

    /// Arguments of the transcoding run (1-pass, or the final run of a 2-pass encode): video and
    /// AC-3 audio straight into a DVD program stream at `output` (a file or `pipe:1`), so no
    /// elementary streams and no separate mux pass touch the disk.
    pub fn encode_args(&self, output: impl Into<OsString>) -> Vec<OsString> {
        let s = |x: &str| OsString::from(x);
        let mut a = self.video_input_args();
        if self.copy_video {
            a.extend([s("-c:v"), s("copy")]);
        } else {
            a.extend(self.video_codec_args(false));
            if self.uses_two_pass() {
                a.extend([s("-pass"), s("2"), s("-passlogfile"), self.passlog_prefix().into_os_string()]);
            }
        }
        for track in &self.audio {
            a.extend([s("-map"), format!("0:{}", track.index).into()]);
        }
        for (n, out) in self.audio_outs().into_iter().enumerate() {
            // async fills timestamp gaps with silence, so the AC-3 PTS stay on the 2880-tick grid dvd_mux checks.
            let filter = match filter_of(&self.audio_filters, n) {
                Some(f) => format!("{f},{DVD_AUDIO_SYNC}"),
                None => DVD_AUDIO_SYNC.to_string(),
            };
            a.extend(audio_track_args(n, out, Some(&filter)));
        }
        a.extend(["-sn", "-dn", "-f", "dvd", "-muxrate", &format!("{DVD_MUX_RATE_KBPS}k"), "-packetsize", "2048"].map(s));
        a.push(output.into());
        a
    }
}

/// `{stem}_s{n+1}.{ext}`: file of subtitle track `n` of a title.
pub fn subtitle_file(stem: &str, n: usize, ext: &str) -> String {
    format!("{stem}_s{}.{ext}", n + 1)
}

/// ffmpeg arguments extracting text subtitles as SRT and PGS as raw `.sup` into `dir`
/// (`tracks`: track number and stream; files named by [`subtitle_file`]). `None` if no stream
/// needs extraction.
pub fn extract_subtitle_args(info: &MediaInfo, tracks: &[(usize, &SubtitleStream)], dir: &Path, stem: &str) -> Option<Vec<OsString>> {
    let mut a: Vec<OsString> = vec!["-y".into()];
    a.extend(info.input_args());
    let mut any = false;
    for &(n, sub) in tracks {
        let map = format!("0:{}", sub.index);
        let (codec, format) = match (sub.kind, sub.codec.as_str()) {
            (SubtitleKind::Text, _) => ("srt", "srt"),
            (_, "dvb_teletext") => ("srt", "srt"),
            (_, "hdmv_pgs_subtitle") => ("copy", "sup"),
            _ => continue,
        };
        a.extend(["-map", &map, "-c:s", codec, "-vn", "-an", "-f", format].map(OsString::from));
        a.push(dir.join(subtitle_file(stem, n, format)).into_os_string());
        any = true;
    }
    any.then_some(a)
}

/// Encodes `job` into `job.vob()` in the work directory: text subtitles are extracted first,
/// with `job.two_pass` an analysis run (statistics only) follows, then the final run writes the
/// DVD program stream – through one native subpicture muxer per subtitle stream via pipes when there are
/// subtitles. The 2-pass statistics files are removed after success (stale ones from an aborted
/// earlier attempt before starting). Returns the muxer logs.
pub async fn run_encode(
    tools: &Toolchain,
    job: &EncodeJob<'_>,
    progress: impl FnMut(EncodePhase, FfmpegProgress),
) -> Result<Vec<String>, ToolError> {
    run_encode_with_external(tools, job, &[], progress).await
}

/// [`run_encode`] with additional standalone external subtitle files (`external_subtitles`), muxed
/// immediately after `job.subtitles` in disc stream order.
pub async fn run_encode_with_external(
    tools: &Toolchain,
    job: &EncodeJob<'_>,
    external_subtitles: &[ExternalSubtitle],
    mut progress: impl FnMut(EncodePhase, FfmpegProgress),
) -> Result<Vec<String>, ToolError> {
    let duration = job.info.duration_secs;
    let io = |error| ToolError::Io { tool: "work folder".into(), error };
    let mut stages = Vec::new();
    if let Some(args) = job.subtitle_args() {
        tools.ffmpeg(&args, duration, |_| {}).await?;
    }
    for (n, sub) in job.subtitles.iter().enumerate() {
        let xml = job.subtitle_xml(n);
        let text = if sub.kind == SubtitleKind::Text || sub.codec == "dvb_teletext" {
            text_subtitle_xml(job, n, sub).await?
        } else {
            bitmap_subtitle_xml(tools, job, n, sub).await?
        };
        std::fs::write(job.dir.join(&xml), text).map_err(io)?;
        stages.push((xml, n as u32));
    }
    for (i, ext) in external_subtitles.iter().enumerate() {
        let n = job.subtitles.len() + i;
        let xml = job.subtitle_xml(n);
        let text = external_subtitle_xml(tools, job, n, ext).await?;
        std::fs::write(job.dir.join(&xml), text).map_err(io)?;
        stages.push((xml, n as u32));
    }
    let cleanup = |job: &EncodeJob| {
        job.cleanup_passlogs().map_err(|error| ToolError::Io { tool: "2-pass log cleanup".into(), error })
    };
    let final_phase = if job.uses_two_pass() {
        cleanup(job)?;
        tools.ffmpeg_with(&job.first_pass_args(), duration, |p| progress(EncodePhase::Analysis, p)).await?;
        EncodePhase::Final
    } else {
        EncodePhase::Single
    };
    let logs = if stages.is_empty() {
        let out = job.dir.join(job.vob());
        tools.ffmpeg_with(&job.encode_args(out), duration, |p| progress(final_phase, p)).await?;
        vec![]
    } else {
        let args = job.encode_args("pipe:1");
        tools
            .ffmpeg_spumux_pipeline(&args, &job.dir, &stages, &job.vob(), duration, |p| progress(final_phase, p))
            .await?
    };
    if job.uses_two_pass() {
        cleanup(job)?;
    }
    Ok(logs)
}

// ---------------------------------------------------------------------------
// Blu-ray
// ---------------------------------------------------------------------------

/// Blu-ray NVENC options shared by the encode and the availability probe.
pub fn nvenc_bd_options() -> [&'static str; 16] {
    [
        "-bluray-compat", "1", "-aud", "1", "-slices", "4", "-rc", "vbr", "-forced-idr", "1", "-strict_gop", "1",
        "-spatial-aq", "1", "-rc-lookahead", "32",
    ]
}

/// Blu-ray Intel Quick Sync private options (verified against `ffmpeg -h encoder=h264_qsv`);
/// slices and closed GOP are the generic `-slices` / `-flags +cgop` in [`BdEncodeJob::qsv_args`].
pub const QSV_BD_OPTIONS: &[&str] = &["-aud", "1", "-forced_idr", "1", "-pic_timing_sei", "1"];

/// Blu-ray AMD AMF options (verified against `ffmpeg -h encoder=h264_amf`). AMF has no slice
/// option. shortcut: slice count / BD compliance of AMF output unchecked, verify on AMD hardware.
pub const AMF_BD_OPTIONS: &[&str] =
    &["-usage", "transcoding", "-rc", "vbr_peak", "-enforce_hrd", "1", "-aud", "1", "-forced_idr", "1"];

/// Default peak video rate of the Blu-ray encodes (x264 VBV, kbit/s) and its buffer.
pub const BD_MAXRATE_KBPS: u32 = 40_000;
pub const BD_BUFSIZE_KBIT: u32 = 30_000;
/// AC-3 bitrate of the silent filler tracks.
pub const BD_SILENCE_KBPS: u32 = 192;

/// Video filter chain of a Blu-ray title: deinterlace, project frame rate, scale into the HD
/// frame in BT.709 (bars, crop or stretch per `vp.scale`), square pixels.
pub fn bd_video_filter(v: &VideoStream, rate: BdRate, resolution: Resolution, vp: VideoProcessing) -> String {
    let mut f = fit_chain(v, Aspect::Wide, resolution.frame_size(TvStandard::Pal), &rate.ffmpeg(), "bt709", vp);
    f.push("setsar=1".to_string());
    f.push("format=yuv420p".to_string());
    f.join(",")
}

/// What happens to the video stream of a DVD-Video title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DvdVideoMode {
    /// Always re-encode.
    #[default]
    Encode,
    /// Copy streams that pass [`dvd_video_copy`], encode the rest.
    Auto,
    /// Copy every title; the build fails when one does not pass [`dvd_video_copy`].
    Strict,
}

impl DvdVideoMode {
    pub fn from_setting(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => DvdVideoMode::Auto,
            "strict" => DvdVideoMode::Strict,
            _ => DvdVideoMode::Encode,
        }
    }
}

/// Video copy parameters computed by [`dvd_video_copy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DvdVideoCopy {
    pub kbps: u32,
    pub bytes: u64,
}

/// Whether `v` has a native DVD-Video resolution and frame rate for `tv`.
pub fn is_dvd_native_format(v: &VideoStream, tv: TvStandard) -> bool {
    if !v.fps.is_finite() {
        return false;
    }
    match tv {
        TvStandard::Pal => {
            matches!((v.width, v.height), (720, 576) | (704, 576)) && (v.fps - 25.0).abs() <= 0.05
        }
        TvStandard::Ntsc => {
            matches!((v.width, v.height), (720, 480) | (704, 480))
                && ((v.fps - 30000.0 / 1001.0).abs() <= 0.05 || (v.fps - 24000.0 / 1001.0).abs() <= 0.05)
        }
    }
}

/// Whether the video stream of `info` can be stream-copied unchanged onto a DVD-Video title
/// without transcoding: returns its bitrate and size in bytes, or an English reason why it must
/// be re-encoded.
pub fn dvd_video_copy(
    info: &MediaInfo,
    tv: TvStandard,
    resolution: Resolution,
    aspect: Aspect,
    vp: VideoProcessing,
    max_kbps: u32,
) -> Result<DvdVideoCopy, String> {
    let v = info.video.as_ref().ok_or("no video stream")?;
    if v.codec != "mpeg2video" {
        return Err(format!("{} video (needs MPEG-2)", v.codec.to_uppercase()));
    }
    if v.pix_fmt != "yuv420p" {
        return Err(format!(
            "pixel format {} (needs yuv420p)",
            if v.pix_fmt.is_empty() { "unknown" } else { &v.pix_fmt }
        ));
    }
    if v.hdr {
        return Err("HDR video (needs SDR)".into());
    }
    if !is_dvd_native_format(v, tv) {
        let std_name = match tv {
            TvStandard::Pal => "PAL",
            TvStandard::Ntsc => "NTSC",
        };
        return Err(format!("{}×{} {:.3} fps is not DVD-compliant for {std_name}", v.width, v.height, v.fps));
    }
    if resolution != Resolution::FullD1 {
        return Err(format!("{}×{} source (title requests {})", v.width, v.height, resolution.label()));
    }
    let src_aspect = Aspect::for_source(v.display_aspect);
    if aspect != src_aspect {
        return Err(format!("{} source (title requests {})", src_aspect.as_str(), aspect.as_str()));
    }
    if info.trim.is_some() {
        return Err("trim active".into());
    }
    if v.crop.is_some() {
        return Err("crop active".into());
    }
    if vp.deinterlace == Deinterlace::Yadif {
        return Err("forced deinterlacing".into());
    }
    if vp.scale != ScaleMode::Letterbox {
        return Err("scale override active".into());
    }
    if vp.has_picture_filters() {
        return Err("picture filters active".into());
    }
    let kbps = source_video_kbps(info).ok_or("unknown video bitrate")?;
    let cap = max_kbps.min(MAX_VIDEO_PEAK_KBPS);
    if kbps > cap {
        return Err(format!("{kbps} kbit/s video (DVD limit {cap})"));
    }
    let bytes = std::fs::metadata(&info.path)
        .ok()
        .map(|m| m.len())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| ((kbps as f64 * 1000.0 / 8.0) * info.duration_secs.max(0.0)).round() as u64);
    Ok(DvdVideoCopy { kbps, bytes })
}

/// What happens to the video stream of a Blu-ray / AVCHD title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BdVideoMode {
    /// Always re-encode.
    #[default]
    Encode,
    /// Copy streams that pass [`bd_video_copy`], encode the rest.
    Auto,
    /// Copy every title; the build fails when one does not pass [`bd_video_copy`].
    Strict,
}

impl BdVideoMode {
    pub fn from_setting(s: &str) -> Self {
        match s {
            "auto" => BdVideoMode::Auto,
            "strict" => BdVideoMode::Strict,
            _ => BdVideoMode::Encode,
        }
    }
}

/// Average video bitrate of the (untrimmed) source in kbit/s: the stream's own, else the file
/// size minus the audio tracks. Peaks are checked by the build's stream scan
/// ([`crate::bd_mux::scan_video`]), which the GUI's probe does not run.
pub fn source_video_kbps(info: &MediaInfo) -> Option<u32> {
    let v = info.video.as_ref()?;
    // MPEG-2 sequence headers encode VBR as 0x3FFFF * 400 = 104,857,200 bit/s.
    if let Some(b) = v.bit_rate.filter(|&b| !(v.codec == "mpeg2video" && b >= 104_857_200)) {
        return Some(b.div_ceil(1000));
    }
    let bytes = std::fs::metadata(&info.path).ok()?.len();
    if info.duration_secs <= 0.0 {
        return None;
    }
    let total = (bytes as f64 * 8.0 / 1000.0 / info.duration_secs) as u32;
    let audio: u32 = info.audio.iter().map(|a| a.bit_rate.map_or(0, |b| b / 1000)).sum();
    Some(total.saturating_sub(audio)).filter(|k| *k > 0)
}

/// Whether the video stream of `info` can go onto a Blu-ray/AVCHD title with `rate` and
/// `resolution` unchanged: its average kbit/s, or why it has to be re-encoded (English, shown
/// in logs and the GUI). `max_kbps`: the disc's video limit beside this title's audio.
pub fn bd_video_copy(info: &MediaInfo, rate: BdRate, resolution: Resolution, vp: VideoProcessing, max_kbps: u32, avchd: bool) -> Result<u32, String> {
    let kbps = bd_stream_copy(info, vp, max_kbps)?;
    let v = info.video.as_ref().ok_or("no video stream")?;
    match crate::bd::bd_native_format(v, avchd) {
        None => Err(not_bd_format(v)),
        Some((r, _)) if r != resolution => {
            let (w, h) = resolution.frame_size(TvStandard::Pal);
            Err(format!("{}×{} source (title is {w}×{h})", v.width, v.height))
        }
        Some((_, r)) if r != rate => Err(format!("{} source (title runs at {})", r.name(), rate.name())),
        Some(_) => Ok(kbps),
    }
}

/// Why a source's picture format is no Blu-ray format.
pub fn not_bd_format(v: &VideoStream) -> String {
    let scan = if v.is_interlaced() { "interlaced" } else { "progressive" };
    format!("{}×{} {:.3} fps {scan} is not a Blu-ray video format", v.width, v.height, v.fps)
}

/// Everything about a stream copy except the picture format and what needs a scan of the whole
/// stream (buffer peaks, keyframe distance: [`crate::bd_mux::scan_video`], at build time). Missing
/// Blu-ray timing (HRD, SEI) is added by the muxer. Copies stay opt-in: they are untested on
/// standalone players.
pub fn bd_stream_copy(info: &MediaInfo, vp: VideoProcessing, max_kbps: u32) -> Result<u32, String> {
    let v = info.video.as_ref().ok_or("no video stream")?;
    if v.codec != "h264" {
        return Err(format!("{} video (needs H.264)", v.codec.to_uppercase()));
    }
    let profile = v.profile.as_deref().unwrap_or("unknown");
    if !matches!(profile, "Main" | "High") {
        return Err(format!("H.264 profile {profile} (needs Main or High)"));
    }
    if v.pix_fmt != "yuv420p" {
        return Err(format!("pixel format {} (needs 8-bit 4:2:0)", if v.pix_fmt.is_empty() { "unknown" } else { &v.pix_fmt }));
    }
    match v.level {
        Some(l) if l <= 41 => {}
        Some(l) => return Err(format!("H.264 level {}.{} (max 4.1)", l / 10, l % 10)),
        None => return Err("unknown H.264 level".into()),
    }
    if v.hdr {
        return Err("HDR video (needs SDR)".into());
    }
    if v.crop.is_some() {
        return Err("crop active".into());
    }
    if info.trim.is_some() {
        return Err("trim active".into());
    }
    if vp.deinterlace == Deinterlace::Yadif {
        return Err("forced deinterlacing".into());
    }
    if vp.has_picture_filters() {
        return Err("picture filters active".into());
    }
    let kbps = source_video_kbps(info).ok_or("unknown video bitrate")?;
    if kbps > max_kbps {
        return Err(format!("{kbps} kbit/s video (disc limit {max_kbps})"));
    }
    Ok(kbps)
}

/// H.264 encoder for Blu-ray titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BdEncoder {
    X264,
    /// NVIDIA `h264_nvenc`; its "2-pass" is the single-run `-multipass fullres`.
    Nvenc,
    /// Intel Quick Sync `h264_qsv`; no 2-pass.
    Qsv,
    /// AMD `h264_amf`; no 2-pass.
    Amf,
}

impl BdEncoder {
    pub fn label(self) -> &'static str {
        match self {
            BdEncoder::X264 => "x264",
            BdEncoder::Nvenc => "NVENC (h264_nvenc)",
            BdEncoder::Qsv => "QSV (h264_qsv)",
            BdEncoder::Amf => "AMF (h264_amf)",
        }
    }

    pub fn is_hw(self) -> bool {
        self != BdEncoder::X264
    }

    /// ffmpeg codec name of a hardware encoder.
    pub fn codec(self) -> &'static str {
        match self {
            BdEncoder::X264 => "libx264",
            BdEncoder::Nvenc => "h264_nvenc",
            BdEncoder::Qsv => "h264_qsv",
            BdEncoder::Amf => "h264_amf",
        }
    }

    /// Blu-ray options shared by the encode and the availability probe.
    pub fn bd_options(self) -> Vec<&'static str> {
        match self {
            BdEncoder::X264 => vec![],
            BdEncoder::Nvenc => nvenc_bd_options().to_vec(),
            BdEncoder::Qsv => QSV_BD_OPTIONS.to_vec(),
            BdEncoder::Amf => AMF_BD_OPTIONS.to_vec(),
        }
    }
}

/// One title encoded as H.264 + AC-3 into `{stem}.mkv` for the BDAV muxer.
#[derive(Debug, Clone)]
pub struct BdEncodeJob<'a> {
    pub info: &'a MediaInfo,
    /// Audio tracks of the whole project; `None` = silent AC-3 track (this title has fewer).
    pub audio: Vec<Option<&'a AudioStream>>,
    pub video_kbps: u32,
    pub dir: PathBuf,
    pub stem: String,
    pub two_pass: bool,
    /// `-preset veryfast` instead of `slow`.
    pub fast: bool,
    pub rate: BdRate,
    pub resolution: Resolution,
    pub video: VideoProcessing,
    pub hwaccel: Option<&'static str>,
    pub encoder: BdEncoder,
    pub audio_mode: AudioMode,
    pub bd_audio: BdAudioMode,
    /// [`AudioTarget::Bluray`] or [`AudioTarget::Avchd`].
    pub audio_target: AudioTarget,
    pub audio_filters: Vec<Option<String>>,
    /// Peak rate and VBV buffer of the video (kbit/s, kbit).
    pub maxrate_kbps: u32,
    pub bufsize_kbit: u32,
    /// Stream copy of the source video (passed [`bd_video_copy`]): no filter, no encoder.
    pub copy_video: bool,
}

impl BdEncodeJob<'_> {
    pub fn mkv(&self) -> String {
        format!("{}.mkv", self.stem)
    }

    /// x264 2-pass needs an analysis run; NVENC does it inside the single run.
    pub fn x264_two_pass(&self) -> bool {
        self.two_pass && self.encoder == BdEncoder::X264 && !self.copy_video
    }

    pub fn passlog_prefix(&self) -> PathBuf {
        self.dir.join(format!("{}_2pass", self.stem))
    }

    /// Output plan of every disc audio track (silent filler where the title has none).
    pub fn audio_outs(&self) -> Vec<AudioOut> {
        self.audio
            .iter()
            .enumerate()
            .map(|(n, a)| {
                a.map_or(SILENCE_OUT, |a| plan_audio_bd(a, self.audio_target, self.audio_mode, self.bd_audio, filter_of(&self.audio_filters, n).is_some() || self.info.trim.is_some()))
            })
            .collect()
    }

    pub fn audio_kbps_total(&self) -> u32 {
        self.audio_outs().iter().map(|o| o.kbps()).sum()
    }

    fn silence_count(&self) -> usize {
        self.audio.iter().filter(|a| a.is_none()).count()
    }

    fn video_codec_args(&self) -> Vec<OsString> {
        if self.copy_video {
            return ["-c:v", "copy"].map(OsString::from).to_vec();
        }
        match self.encoder {
            BdEncoder::Nvenc => return self.nvenc_args(),
            BdEncoder::Qsv => return self.qsv_args(),
            BdEncoder::Amf => return self.amf_args(),
            BdEncoder::X264 => {}
        }
        let mut x264 = String::from("slices=4:b-pyramid=strict:nal-hrd=vbr:aud=1:open-gop=0:weightp=1");
        if self.rate.fake_interlaced {
            x264.push_str(":fake-interlaced=1:pic-struct=1");
        }
        let preset = if self.fast { "veryfast" } else { "slow" };
        let mut a: Vec<OsString> =
            ["-c:v", "libx264", "-preset", preset, "-profile:v", "high", "-level:v", "4.1", "-bluray-compat", "1", "-b:v"]
                .map(OsString::from)
                .to_vec();
        a.push(format!("{}k", self.video_kbps).into());
        a.extend(["-maxrate".into(), format!("{}k", self.maxrate_kbps).into(), "-bufsize".into(), format!("{}k", self.bufsize_kbit).into()]);
        a.extend(["-g".into(), self.rate.keyint().to_string().into(), "-bf".into(), "3".into(), "-x264-params".into(), x264.into()]);
        a.extend(["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709"].map(OsString::from));
        a
    }

    fn nvenc_args(&self) -> Vec<OsString> {
        let preset = if self.fast { "p3" } else { "p5" };
        let mut a: Vec<OsString> = ["-c:v", "h264_nvenc", "-preset", preset, "-tune", "hq", "-profile:v", "high", "-level:v", "4.1"]
            .map(OsString::from)
            .to_vec();
        a.extend(nvenc_bd_options().map(OsString::from));
        a.extend(["-b:v".into(), format!("{}k", self.video_kbps).into()]);
        a.extend(["-maxrate".into(), format!("{}k", self.maxrate_kbps).into(), "-bufsize".into(), format!("{}k", self.bufsize_kbit).into()]);
        a.extend(["-g".into(), self.rate.keyint().to_string().into(), "-bf".into(), "3".into()]);
        if self.two_pass {
            a.extend(["-multipass", "fullres"].map(OsString::from));
        }
        a.extend(["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709"].map(OsString::from));
        a
    }

    /// Rate control, GOP and colour tags shared by the QSV and AMF encodes.
    fn hw_rate_args(&self, a: &mut Vec<OsString>) {
        a.extend(["-b:v".into(), format!("{}k", self.video_kbps).into()]);
        a.extend(["-maxrate".into(), format!("{}k", self.maxrate_kbps).into(), "-bufsize".into(), format!("{}k", self.bufsize_kbit).into()]);
        a.extend(["-g".into(), self.rate.keyint().to_string().into(), "-bf".into(), "3".into()]);
        a.extend(["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709"].map(OsString::from));
    }

    fn qsv_args(&self) -> Vec<OsString> {
        let preset = if self.fast { "veryfast" } else { "slow" };
        let mut a: Vec<OsString> =
            ["-c:v", "h264_qsv", "-preset", preset, "-profile:v", "high", "-level:v", "41", "-slices", "4", "-flags", "+cgop"]
                .map(OsString::from)
                .to_vec();
        a.extend(QSV_BD_OPTIONS.iter().map(OsString::from));
        self.hw_rate_args(&mut a);
        a
    }

    fn amf_args(&self) -> Vec<OsString> {
        let quality = if self.fast { "speed" } else { "quality" };
        let mut a: Vec<OsString> =
            ["-c:v", "h264_amf", "-quality", quality, "-profile:v", "high", "-level:v", "4.1"].map(OsString::from).to_vec();
        a.extend(AMF_BD_OPTIONS.iter().map(OsString::from));
        self.hw_rate_args(&mut a);
        a
    }

    /// Inputs and the video map/filter; `with_audio` adds the silence source when needed.
    fn input_args(&self, with_audio: bool) -> Vec<OsString> {
        let v = self.info.video.as_ref().expect("probed file has video");
        let mut a: Vec<OsString> = vec!["-y".into()];
        if let Some(hw) = self.hwaccel.filter(|_| !self.copy_video) {
            a.extend(["-hwaccel".into(), hw.into()]);
        }
        a.extend(self.info.input_args());
        if with_audio && self.silence_count() > 0 {
            a.extend(["-f", "lavfi", "-t"].map(OsString::from));
            a.push(format!("{:.3}", self.info.duration_secs).into());
            a.extend(["-i", "anullsrc=r=48000:cl=stereo"].map(OsString::from));
        }
        a.extend(["-map".into(), format!("0:{}", v.index).into()]);
        if !self.copy_video {
            a.extend(["-vf".into(), bd_video_filter(v, self.rate, self.resolution, self.video).into()]);
        }
        a
    }

    /// 2-pass analysis run: video only, statistics log, no real output.
    pub fn first_pass_args(&self) -> Vec<OsString> {
        let mut a = self.input_args(false);
        a.extend(self.video_codec_args());
        a.extend(["-pass", "1", "-passlogfile"].map(OsString::from));
        a.push(self.passlog_prefix().into_os_string());
        a.extend(["-an", "-sn", "-f", "rawvideo", NULL_DEVICE].map(OsString::from));
        a
    }

    /// The transcoding run (1-pass, or the final run of a 2-pass encode) writing `output`.
    pub fn encode_args(&self, output: impl Into<OsString>) -> Vec<OsString> {
        let s = |x: &str| OsString::from(x);
        let mut a = self.input_args(true);
        a.extend(self.video_codec_args());
        if self.x264_two_pass() {
            a.extend([s("-pass"), s("2"), s("-passlogfile"), self.passlog_prefix().into_os_string()]);
        }
        for track in &self.audio {
            a.extend([s("-map"), track.map_or_else(|| "1:a:0".into(), |t| format!("0:{}", t.index)).into()]);
        }
        for (n, out) in self.audio_outs().into_iter().enumerate() {
            a.extend(audio_track_args(n, out, filter_of(&self.audio_filters, n)));
        }
        // -t cuts every audio track (also copied ones, at packet granularity) at the video length.
        a.extend([s("-t"), format!("{:.3}", self.info.duration_secs).into()]);
        a.extend(["-map_chapters", "-1", "-sn", "-dn", "-f", "matroska"].map(s));
        a.push(output.into());
        a
    }
}

/// Encodes `job` into `job.mkv()` in its directory (2-pass: analysis run first; statistics are
/// removed before and after).
pub async fn run_bd_encode(
    tools: &Toolchain,
    job: &BdEncodeJob<'_>,
    mut progress: impl FnMut(EncodePhase, FfmpegProgress),
) -> Result<(), ToolError> {
    let duration = job.info.duration_secs;
    let cleanup = || {
        cleanup_passlogs_in(&job.dir, &job.stem).map_err(|error| ToolError::Io { tool: "2-pass log cleanup".into(), error })
    };
    let final_phase = if job.x264_two_pass() {
        cleanup()?;
        tools.ffmpeg_with(&job.first_pass_args(), duration, |p| progress(EncodePhase::Analysis, p)).await?;
        EncodePhase::Final
    } else {
        EncodePhase::Single
    };
    tools.ffmpeg_with(&job.encode_args(job.dir.join(job.mkv())), duration, |p| progress(final_phase, p)).await?;
    if job.x264_two_pass() {
        cleanup()?;
    }
    Ok(())
}

/// Decodes bitmap subtitle `n` (PGS from its extracted `.sup`, VobSub via ffprobe), writes its
/// images into `<stem>_s<n>/` and returns the spumux XML.
async fn bitmap_subtitle_xml(tools: &Toolchain, job: &EncodeJob<'_>, n: usize, sub: &SubtitleStream) -> Result<String, ToolError> {
    let fail = |e: subpic::SubpicError| ToolError::Failed { tool: format!("Bitmap subtitle #{}", sub.index), status: String::new(), log: e.to_string() };
    let decoded = match sub.codec.as_str() {
        "hdmv_pgs_subtitle" => {
            let path = job.dir.join(job.sup(n));
            let data = std::fs::read(&path).map_err(|error| ToolError::Io { tool: path.display().to_string(), error })?;
            let _ = std::fs::remove_file(&path);
            tokio::task::spawn_blocking(move || subpic::decode_pgs(&data)).await.map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
        }
        "dvd_subtitle" => {
            let json = tools.ffprobe_output(&subpic::vobsub_probe_args(&job.info.path, sub.index)).await?;
            let v = job.info.video.as_ref().expect("probed file has video");
            let trim = job.info.trim;
            // Canvas: the .idx size, else the DVD raster the source belongs to.
            let fallback = (720, if v.width == 720 { v.height } else if v.height >= 576 { 576 } else { 480 });
            tokio::task::spawn_blocking(move || {
                let mut probe = subpic::parse_vobsub_probe(&json)?;
                if let Some(t) = trim {
                    probe = probe.trimmed(t.start, t.end);
                }
                let (palette, size) = subpic::parse_idx_header(&String::from_utf8_lossy(&probe.extradata));
                Ok(subpic::decode_vobsub(&probe.packets, palette.as_ref(), size.unwrap_or(fallback)))
            })
            .await
            .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
        }
        "dvb_subtitle" | "xsub" => {
            let temp_vob = job.dir.join(format!("{}_temp_sub_{}.vob", job.stem, n));
            let mut args = vec!["-y".into()];
            args.extend(job.info.input_args());
            args.extend(["-map".into(), format!("0:{}", sub.index).into(), "-c:s".into(), "dvdsub".into(), "-f".into(), "dvd".into()]);
            args.push(temp_vob.as_os_str().to_owned());
            tools.ffmpeg(&args, job.info.duration_secs, |_| {}).await?;
            let probe_args = subpic::vobsub_probe_args(&temp_vob, 0);
            let json = tools.ffprobe_output(&probe_args).await?;
            let _ = std::fs::remove_file(&temp_vob);
            let v = job.info.video.as_ref().expect("probed file has video");
            let trim = job.info.trim;
            let fallback = (720, if v.width == 720 { v.height } else if v.height >= 576 { 576 } else { 480 });
            tokio::task::spawn_blocking(move || {
                let mut probe = subpic::parse_vobsub_probe(&json)?;
                if let Some(t) = trim {
                    probe = probe.trimmed(t.start, t.end);
                }
                let (palette, size) = subpic::parse_idx_header(&String::from_utf8_lossy(&probe.extradata));
                Ok(subpic::decode_vobsub(&probe.packets, palette.as_ref(), size.unwrap_or(fallback)))
            })
            .await
            .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
        }
        other => Err(subpic::SubpicError::Format(format!("{other} is not supported"))),
    }
    .map_err(fail)?;
    let target = job.subpicture_target(decoded.canvas);
    let (dir, tv, folder) = (job.dir.clone(), job.tv, format!("{}_s{}", job.stem, n + 1));
    let (xml, _count) = tokio::task::spawn_blocking(move || subpic::render(&decoded, &target, tv, &dir, &folder))
        .await
        .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
        .map_err(fail)?;
    Ok(xml)
}

/// Arguments converting an external text subtitle file (`.ass`, `.ssa`, `.vtt`, `.sub`, or
/// non-UTF-8 `.srt`) into normalized UTF-8 `.srt` via ffmpeg.
pub fn convert_subtitle_to_srt_args(src: &Path, dst: &Path) -> Vec<OsString> {
    vec![
        "-y".into(),
        "-i".into(),
        src.as_os_str().to_owned(),
        "-c:s".into(),
        "srt".into(),
        "-f".into(),
        "srt".into(),
        dst.as_os_str().to_owned(),
    ]
}

/// Clips and shifts text subtitle cues to a trimmed title range `[start, end)`.
pub fn trim_cues(cues: Vec<textsub::Cue>, start: f64, end: f64) -> Vec<textsub::Cue> {
    cues.into_iter()
        .filter(|c| c.end > start && c.start < end)
        .map(|mut c| {
            c.start = (c.start - start).max(0.0);
            c.end = c.end.min(end) - start;
            c
        })
        .collect()
}

/// Clips and shifts decoded bitmap subtitle pictures to a trimmed title range `[start, end)`.
pub fn trim_pictures(pics: Vec<subpic::Picture>, start: f64, end: f64) -> Vec<subpic::Picture> {
    pics.into_iter()
        .filter(|p| p.end > start && p.start < end)
        .map(|mut p| {
            p.start = (p.start - start).max(0.0);
            p.end = p.end.min(end) - start;
            p
        })
        .collect()
}

async fn ffmpeg_subtitle_to_srt(tools: &Toolchain, ext_path: &Path, temp_srt: &Path) -> Result<String, ToolError> {
    let args = convert_subtitle_to_srt_args(ext_path, temp_srt);
    if tools.ffmpeg(&args, 0.0, |_| {}).await.is_err() {
        let mut cp1252: Vec<OsString> = vec!["-y".into(), "-sub_charenc".into(), "CP1252".into()];
        cp1252.extend(args.into_iter().skip(1));
        tools.ffmpeg(&cp1252, 0.0, |_| {}).await?;
    }
    let normalized = std::fs::read(temp_srt).map_err(|error| ToolError::Io { tool: temp_srt.display().to_string(), error })?;
    let _ = std::fs::remove_file(temp_srt);
    Ok(String::from_utf8_lossy(&normalized).trim_start_matches('\u{FEFF}').to_string())
}

/// Loads cues from a standalone external text subtitle file: UTF-8 `.srt` is parsed directly,
/// while `.ass`/`.ssa`/`.vtt`/`.sub` (or non-UTF-8 `.srt`) are normalized via ffmpeg first (with
/// automatic `CP1252` fallback for legacy Windows-1252 / Latin-1 subtitle files).
pub async fn load_external_text_cues(
    tools: &Toolchain,
    ext_path: &Path,
    temp_srt: &Path,
    trim: Option<Trim>,
) -> Result<Vec<textsub::Cue>, ToolError> {
    let is_srt = ext_path.extension().is_some_and(|e| e.eq_ignore_ascii_case("srt"));
    let text = if is_srt {
        let bytes = std::fs::read(ext_path).map_err(|error| ToolError::Io { tool: ext_path.display().to_string(), error })?;
        if let Ok(utf8) = std::str::from_utf8(&bytes) {
            utf8.trim_start_matches('\u{FEFF}').to_string()
        } else {
            ffmpeg_subtitle_to_srt(tools, ext_path, temp_srt).await?
        }
    } else {
        ffmpeg_subtitle_to_srt(tools, ext_path, temp_srt).await?
    };
    let (cues, _skipped) = textsub::parse_srt(&text);
    Ok(match trim {
        Some(t) => trim_cues(cues, t.start, t.end),
        None => cues,
    })
}

async fn render_text_cues_xml(
    label: String,
    dir: PathBuf,
    tv: TvStandard,
    folder: String,
    style: textsub::SubtitleStyle,
    cues: Vec<textsub::Cue>,
    forced: bool,
) -> Result<String, ToolError> {
    let fail = |e: subpic::SubpicError| ToolError::Failed { tool: label.clone(), status: String::new(), log: e.to_string() };
    let (xml, _count) = tokio::task::spawn_blocking(move || {
        let mut decoded = textsub::render_cues_styled(&cues, tv, &style);
        if forced {
            for p in &mut decoded.pictures {
                p.forced = true;
            }
        }
        let (w, h) = tv.size();
        let whole = subpic::Target { frame: (w, h), area: (0, 0, w, h), source: (0, 0, w, h) };
        subpic::render(&decoded, &whole, tv, &dir, &folder)
    })
    .await
    .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
    .map_err(fail)?;
    Ok(xml)
}

/// Renders text subtitle `n` (its extracted `.srt`) natively into `<stem>_s<n>/` and returns the
/// spumux XML, like [`bitmap_subtitle_xml`].
async fn text_subtitle_xml(job: &EncodeJob<'_>, n: usize, sub: &SubtitleStream) -> Result<String, ToolError> {
    let path = job.dir.join(job.srt(n));
    let data = std::fs::read(&path).map_err(|error| ToolError::Io { tool: path.display().to_string(), error })?;
    let (cues, _skipped) = textsub::parse_srt(String::from_utf8_lossy(&data).trim_start_matches('\u{FEFF}'));
    render_text_cues_xml(
        format!("Text subtitle #{}", sub.index),
        job.dir.clone(),
        job.tv,
        format!("{}_s{}", job.stem, n + 1),
        job.subtitle_style.clone(),
        cues,
        sub.forced,
    )
    .await
}

/// Renders an external subtitle file (`ext`) at disc subtitle track `n` into `<stem>_s<n+1>/` and
/// returns the spumux XML.
async fn external_subtitle_xml(tools: &Toolchain, job: &EncodeJob<'_>, n: usize, ext: &ExternalSubtitle) -> Result<String, ToolError> {
    let info = probe::probe_external_subtitle(&ext.path)
        .map_err(|log| ToolError::Failed { tool: format!("External subtitle {}", ext.path.display()), status: String::new(), log })?;
    let folder = format!("{}_s{}", job.stem, n + 1);
    if info.subtitle_kind() == SubtitleKind::Text {
        let temp_srt = job.dir.join(job.srt(n));
        let cues = load_external_text_cues(tools, &ext.path, &temp_srt, job.info.trim).await?;
        render_text_cues_xml(
            format!("External subtitle {}", ext.path.display()),
            job.dir.clone(),
            job.tv,
            folder,
            job.subtitle_style.clone(),
            cues,
            ext.forced,
        )
        .await
    } else {
        let fail = |e: subpic::SubpicError| ToolError::Failed {
            tool: format!("External PGS subtitle {}", ext.path.display()),
            status: String::new(),
            log: e.to_string(),
        };
        let data = std::fs::read(&ext.path).map_err(|error| ToolError::Io { tool: ext.path.display().to_string(), error })?;
        let trim = job.info.trim;
        let forced = ext.forced;
        let mut decoded = tokio::task::spawn_blocking(move || subpic::decode_pgs(&data))
            .await
            .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
            .map_err(fail)?;
        if let Some(t) = trim {
            decoded.pictures = trim_pictures(decoded.pictures, t.start, t.end);
        }
        if forced {
            for p in &mut decoded.pictures {
                p.forced = true;
            }
        }
        let target = job.subpicture_target(decoded.canvas);
        let (dir, tv) = (job.dir.clone(), job.tv);
        let (xml, _count) = tokio::task::spawn_blocking(move || subpic::render(&decoded, &target, tv, &dir, &folder))
            .await
            .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
            .map_err(fail)?;
        Ok(xml)
    }
}

/// Arguments extracting one display-aspect-correct frame at `at_secs` as a PNG thumbnail.
pub fn thumbnail_args(input: &Path, at_secs: f64, output: &Path) -> Vec<OsString> {
    vec![
        "-y".into(),
        "-ss".into(),
        format!("{at_secs:.3}").into(),
        "-i".into(),
        input.as_os_str().to_owned(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        "scale=trunc(ih*dar/2)*2:ih,setsar=1,scale=384:-2".into(),
        output.as_os_str().to_owned(),
    ]
}

/// Arguments extracting the whole picture at `at_secs` in display proportions, `width` pixels wide,
/// as JPEG (the crop inspector shows it; crop values are fractions of it).
pub fn frame_args(input: &Path, at_secs: f64, width: u32, output: &Path) -> Vec<OsString> {
    vec![
        "-y".into(),
        "-ss".into(),
        format!("{at_secs:.3}").into(),
        "-i".into(),
        input.as_os_str().to_owned(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        format!("scale=trunc(ih*dar/2)*2:ih,setsar=1,scale={width}:-2").into(),
        output.as_os_str().to_owned(),
    ]
}

/// Peaks per second of [`waveform_peaks`] (menu editor timeline).
pub const WAVEFORM_PEAKS_PER_SEC: u32 = 40;
/// Decode rate for the waveform: high enough that a peak is a real amplitude, not an alias.
const WAVEFORM_RATE: u32 = 2000;

/// Arguments decoding `secs` of the first audio stream of `input` from `start_secs` as mono
/// unsigned 8-bit PCM to stdout (input for [`waveform_peaks`]).
pub fn waveform_args(input: &Path, start_secs: f64, secs: f64) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-ss".into(), format!("{start_secs:.3}").into(), "-t".into(), format!("{secs:.3}").into(), "-i".into()];
    args.push(input.as_os_str().to_owned());
    args.extend(["-vn", "-ac", "1", "-ar", &WAVEFORM_RATE.to_string(), "-acodec", "pcm_u8", "-f", "u8", "pipe:1"].map(OsString::from));
    args
}

/// Folds `pcm_u8` samples (silence = 128) into one 0–255 peak per 1/[`WAVEFORM_PEAKS_PER_SEC`] s.
pub fn waveform_peaks(pcm: &[u8]) -> Vec<u8> {
    pcm.chunks((WAVEFORM_RATE / WAVEFORM_PEAKS_PER_SEC) as usize)
        .map(|c| c.iter().map(|&s| s.abs_diff(128)).max().unwrap_or(0).saturating_mul(2))
        .collect()
}

/// A moving preview inside a motion menu, in output (PAL/NTSC frame) coordinates.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoSlot {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Keep the clip's aspect ratio (black bars) instead of stretching it to the slot.
    pub letterbox: bool,
    pub clip: crate::author::ClipSource,
}

/// Sound of the motion menus.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MenuAudio {
    /// Main menu silent; every video's own menus (root/chapters/settings) play that video's
    /// sound (from 10 % of its runtime, chapter pages from their first chapter).
    #[default]
    TitleAudio,
    /// All menus silent.
    Silence,
    /// The main menu plays this audio file (from its start); the video menus keep their own
    /// video's sound.
    File(PathBuf),
}

/// Arguments compiling a motion menu: the still `base` (everything below the video slots),
/// the moving sample clips overlaid at their slots, the transparent `foreground` (frames,
/// captions, icons drawn above the slots) and AC-3 sound from `audio` (`None` = silence),
/// encoded as a `duration_secs` long DVD MPEG-2 program stream of the TV standard `tv`.
///
/// `intro_secs` (whole GOPs, see `menu_anim::dvd_intro_frames`): the page opens with an intro of raw
/// RGBA frames read from `pipe:0` (`Toolchain::ffmpeg_piped`) that covers the first `intro_secs`;
/// a closed GOP starts at that mark, where the author splits the VOB into the intro and the page
/// cell, and the sound crossfades there like on Blu-ray.
#[allow(clippy::too_many_arguments)]
pub fn motion_menu_args(
    tv: TvStandard,
    base: &Path,
    foreground: Option<&Path>,
    slots: &[VideoSlot],
    duration_secs: f64,
    audio: Option<&ClipSource>,
    intro_secs: Option<f64>,
    output: &Path,
) -> Vec<OsString> {
    let d = format!("{duration_secs:.3}");
    let rate = tv.frame_rate();
    let mut a: Vec<OsString> = vec!["-y".into()];
    let still = |a: &mut Vec<OsString>, png: &Path| {
        a.extend(["-loop", "1", "-framerate", rate, "-t", &d, "-i"].map(OsString::from));
        a.push(png.as_os_str().to_owned());
    };

    // Input 0: base still.
    still(&mut a, base);
    // Inputs 1..=n: sample clips, looped if the source is shorter than the menu.
    for s in slots {
        a.extend(["-stream_loop", "-1", "-ss", &format!("{:.3}", s.clip.start_secs), "-t", &d, "-i"].map(OsString::from));
        a.push(s.clip.path.as_os_str().to_owned());
    }
    let mut next_input = 1 + slots.len();
    let fg_input = foreground.map(|fg| {
        still(&mut a, fg);
        next_input += 1;
        next_input - 1
    });
    // Last input: the menu sound (same source for every page) or silence.
    let audio_input = next_input;
    match audio {
        Some(src) => {
            a.extend(["-stream_loop", "-1", "-ss", &format!("{:.3}", src.start_secs), "-t", &d, "-i"].map(OsString::from));
            a.push(src.path.as_os_str().to_owned());
        }
        None => a.extend(["-f", "lavfi", "-t", &d, "-i", "anullsrc=r=48000:cl=stereo"].map(OsString::from)),
    }

    // Filter graph.
    let mut f: Vec<String> = vec![format!("[0:v]fps={rate},format=rgba,setsar=1[base]")];
    let mut cur = "base".to_string();
    for (i, s) in slots.iter().enumerate() {
        f.push(format!("[{}:v]{},trim=duration={d},setpts=PTS-STARTPTS[clip{i}]", i + 1, dvd_slot_filters(rate, s)));
        f.push(format!("[{cur}][clip{i}]overlay=x={}:y={}:format=rgb:eof_action=repeat[m{i}]", s.x, s.y));
        cur = format!("m{i}");
    }
    if let Some(fg) = fg_input {
        f.push(format!("[{fg}:v]fps={rate},format=rgba,setsar=1[fg]"));
        f.push(format!("[{cur}][fg]overlay=0:0:format=rgb[mfg]"));
        cur = "mfg".into();
    }
    // The intro covers everything below it until its last frame, then the page underneath shows.
    if intro_secs.is_some() {
        f.push(format!("[{}:v]setsar=1[intro]", audio_input + 1));
        f.push(format!("[{cur}][intro]overlay=0:0:format=rgb:eof_action=pass[mi]"));
        cur = "mi".into();
    }
    f.push(format!("[{cur}]format=yuv420p,setdar=4/3[vout]"));
    if audio.is_some() {
        menu_sound_filters(&mut f, audio_input, &d, duration_secs, intro_secs);
    } else {
        f.push(format!("[{audio_input}:a]asetpts=N/SR/TB[aout]"));
    }
    // Last input: the intro frames (the caller writes them as `-f rawvideo` to stdin).
    if intro_secs.is_some() {
        let (w, h) = tv.size();
        a.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "-s"].map(OsString::from));
        a.extend([format!("{w}x{h}"), "-r".into(), rate.into(), "-i".into(), "pipe:0".into()].map(OsString::from));
    }

    a.extend(["-filter_complex".into(), f.join(";").into()]);
    a.extend(["-map", "[vout]", "-map", "[aout]", "-t", &d, "-target", tv.ffmpeg_target(), "-aspect", "4:3"].map(OsString::from));
    if let Some(t) = intro_secs {
        // Rounded down like on Blu-ray; a closed GOP so the loop cell never refers back into the intro
        // (the encoder only allows that without scene-change detection).
        a.extend(["-force_key_frames".into(), format!("{:.6}", (t * 1e6).floor() / 1e6).into(), "-flags".into(), "+cgop".into(), "-sc_threshold".into(), "1000000000".into()]);
    }
    a.extend(["-b:v", "5000k", "-maxrate", "8000k", "-c:a", "ac3", "-b:a", "192k"].map(OsString::from));
    a.push(output.as_os_str().to_owned());
    a
}

/// Filter chain (without labels, ending in RGBA) turning a clip into the pictures of a DVD menu slot.
/// Square pixels first, so anamorphic sources keep their display aspect.
fn dvd_slot_filters(rate: &str, s: &VideoSlot) -> String {
    let fit = if s.letterbox {
        format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
            w = s.width,
            h = s.height
        )
    } else {
        format!("scale={}:{}", s.width, s.height)
    };
    format!("fps={rate},scale=trunc(ih*dar/2)*2:ih,setsar=1,{fit},setsar=1,format=rgba")
}

/// Helper pass of the DVD intro: the first `frames` pictures of a slot, made exactly like the page
/// graph makes them (so the loop continues them without a seam), as raw RGBA on stdout.
pub fn dvd_slot_stream(tv: TvStandard, slot: &VideoSlot, frames: usize) -> crate::tools::RawStream {
    let mut args: Vec<OsString> = ["-stream_loop", "-1", "-ss"].map(OsString::from).into();
    args.extend([format!("{:.3}", slot.clip.start_secs).into(), "-i".into(), slot.clip.path.as_os_str().to_owned()]);
    args.extend(["-vf".into(), dvd_slot_filters(tv.frame_rate(), slot).into(), "-frames:v".into(), frames.to_string().into()]);
    args.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"].map(OsString::from));
    crate::tools::RawStream { args, frame_bytes: slot.width as usize * slot.height as usize * 4 }
}

/// Helper pass of the DVD intro: `frames` pictures of the page's fill colour as raw RGBA, the base
/// the intro frames are composited on.
pub fn dvd_background_stream(tv: TvStandard, rgb: [u8; 3], frames: usize) -> crate::tools::RawStream {
    let (w, h) = tv.size();
    let src = format!("color=c=0x{:02x}{:02x}{:02x}:s={w}x{h}:r={}", rgb[0], rgb[1], rgb[2], tv.frame_rate());
    let mut args: Vec<OsString> = ["-f", "lavfi", "-i"].map(OsString::from).into();
    args.extend([src.into(), "-frames:v".into(), frames.to_string().into()]);
    args.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"].map(OsString::from));
    crate::tools::RawStream { args, frame_bytes: w as usize * h as usize * 4 }
}

// ---------------------------------------------------------------------------
// Custom menu layouts (menu editor)
// ---------------------------------------------------------------------------

/// How a picture or clip fills its box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Distort to the box.
    Stretch,
    /// Whole picture visible, black bars (video) / transparent bars (images).
    #[default]
    Contain,
    /// Fill the box, crop the overhang.
    Cover,
}

/// Bottom layer of a custom menu page.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuBackground {
    Color([u8; 3]),
    /// Still picture filling the frame (cover), optionally blurred (Gaussian sigma in pixels).
    Image { path: PathBuf, blur: f32 },
    /// Moving clip filling the frame (cover), optionally blurred.
    Video { clip: ClipSource, blur: f32 },
}

/// One layer above the background, bottom to top.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuLayer {
    /// Frame-sized transparent PNG with the static elements of this layer.
    Image(PathBuf),
    /// A moving clip in a box (rotation in 90° steps, clockwise).
    Video { clip: ClipSource, x: u32, y: u32, width: u32, height: u32, rotation: u32, fit: Fit, opacity: f32 },
}

/// Shading over the background (below all elements), so texts stay legible on busy pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundOverlay {
    #[default]
    None,
    /// Vertical gradient, clear at the top, dark at the bottom (computed once, then looped).
    BottomDark,
    /// Darkened corners (`vignette`).
    Vignette,
    /// Whole picture darkened to 60 %.
    Dim,
}

/// Everything ffmpeg needs to compile a custom menu page.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomMenuMedia {
    pub background: MenuBackground,
    pub overlay: BackgroundOverlay,
    pub layers: Vec<MenuLayer>,
}

/// ffmpeg filter fitting a stream into `w`x`h` (square pixels assumed on input).
fn fit_filter(fit: Fit, w: u32, h: u32, pad_color: &str) -> String {
    match fit {
        Fit::Stretch => format!("scale={w}:{h}"),
        Fit::Contain => format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color={pad_color}"
        ),
        Fit::Cover => format!("scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"),
    }
}

/// Square pixels first (anamorphic sources keep their display aspect), then the pixel shape of
/// the 4:3 menu frame, so pictures look undistorted on the TV: the axis that ends up horizontal
/// is scaled by 1 / [`TvStandard::menu_pixel_aspect`] (`sideways`: the clip is rotated by
/// 90°/270° afterwards, so its height becomes the horizontal).
pub fn menu_pixels(tv: TvStandard, sideways: bool) -> String {
    menu_pixels_for(tv.menu_pixel_aspect(), sideways)
}

/// [`menu_pixels`] for an arbitrary pixel aspect (1.0 = square pixels, as on Blu-ray).
pub fn menu_pixels_for(pixel_aspect: f64, sideways: bool) -> String {
    let f = 1.0 / pixel_aspect;
    if sideways {
        format!("scale=trunc(ih*dar/2)*2:trunc(ih*{f:.6}/2)*2,setsar=1")
    } else {
        format!("scale=trunc(ih*dar*{f:.6}/2)*2:ih,setsar=1")
    }
}

/// ffmpeg filter rotating clockwise by `rotation` degrees (0/90/180/270).
fn rotate_filter(rotation: u32) -> Option<&'static str> {
    match rotation % 360 {
        90 => Some("transpose=1"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=2"),
        _ => None,
    }
}

/// Filter chain (without input label) turning a clip into a video layer's pictures: `width`×`height`
/// is the layer's box on the frame, rotated clockwise by `rotation`; the result is RGBA.
fn video_layer_filters(frame: &MenuFrame, rate: &str, width: u32, height: u32, rotation: u32, fit: Fit) -> String {
    // Fit into the unrotated box, then rotate into the layout box.
    let (bw, bh) = if rotation % 180 == 90 { (height, width) } else { (width, height) };
    // Stretch fills the box as it is; the other fits keep the picture's proportions.
    let pixels = if fit == Fit::Stretch { "setsar=1".to_string() } else { menu_pixels_for(frame.pixel_aspect, rotation % 180 == 90) };
    let mut chain = format!("fps={rate},{pixels},{}", fit_filter(fit, bw, bh, "black"));
    if let Some(r) = rotate_filter(rotation) {
        chain.push(',');
        chain.push_str(r);
    }
    chain.push_str(",setsar=1,format=rgba");
    chain
}

/// Helper pass of the intro: the first `frames` pictures of a video layer made exactly like the page
/// graph makes them (same chain, so the loop continues them without a seam), as raw RGBA on stdout.
/// `None` for layers that are no video.
pub fn intro_slot_stream(frame: &MenuFrame, rate: BdRate, layer: &MenuLayer, frames: usize) -> Option<crate::tools::RawStream> {
    let MenuLayer::Video { clip, width, height, rotation, fit, .. } = layer else { return None };
    let mut args: Vec<OsString> = ["-stream_loop", "-1", "-ss"].map(OsString::from).into();
    args.extend([format!("{:.3}", clip.start_secs).into(), "-i".into(), clip.path.as_os_str().to_owned()]);
    args.extend(["-vf".into(), video_layer_filters(frame, &rate.ffmpeg(), *width, *height, *rotation, *fit).into()]);
    args.extend(["-frames:v".into(), frames.to_string().into()]);
    args.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"].map(OsString::from));
    Some(crate::tools::RawStream { args, frame_bytes: *width as usize * *height as usize * 4 })
}

/// Arguments compiling a custom (menu editor) page: the background (colour, still or clip,
/// optionally Gaussian-blurred), then every layer in order – transparent PNGs with the static
/// elements and moving clips at their boxes (fitted, rotated in 90° steps, faded by their
/// opacity) – and AC-3 sound from `audio` (`None` = silence), encoded as a `duration_secs`
/// long DVD MPEG-2 program stream. The `-filter_complex` graph is built from the layer list.
pub fn custom_menu_args(
    tv: TvStandard,
    media: &CustomMenuMedia,
    duration_secs: f64,
    audio: Option<&ClipSource>,
    intro_secs: Option<f64>,
    output: &Path,
) -> Vec<OsString> {
    let d = format!("{duration_secs:.3}");
    let frame = MenuFrame::from(tv);
    let mut a = custom_menu_graph(&frame, tv.frame_rate(), media, duration_secs, audio, "4/3", intro_secs);
    a.extend(
        [
            "-map", "[vout]", "-map", "[aout]", "-t", &d, "-target", tv.ffmpeg_target(), "-aspect", "4:3",
            "-b:v", "5000k", "-maxrate", "8000k", "-c:a", "ac3", "-b:a", "192k",
        ]
        .map(OsString::from),
    );
    if let Some(t) = intro_secs {
        a.extend([
            "-force_key_frames".into(),
            format!("expr:gte(t,{:.3})", t).into(),
            "-flags".into(),
            "+cgop".into(),
            "-sc_threshold".into(),
            "1000000000".into(),
        ]);
    }
    a.push(output.as_os_str().to_owned());
    a
}

/// Blu-ray menu clip: the same graph on a square-pixel HD frame, encoded as H.264 + AC-3 (the
/// settings of the title encodes, at a fixed 8 Mbit/s) for the BDAV muxer.
///
/// `intro_secs` (a whole number of frames, see `menu_anim::intro_frames`): the page opens with an
/// intro of raw RGBA frames read from `pipe:0` (`Toolchain::ffmpeg_piped`) that covers the first
/// `intro_secs` of the clip, which then goes on as the loop; a keyframe is forced where the loop
/// starts and the audio fades into the sound at that point, so the restart is level-continuous.
/// `duration_secs` is the whole clip (intro + loop).
pub fn bd_custom_menu_args(
    frame: &MenuFrame,
    rate: BdRate,
    media: &CustomMenuMedia,
    duration_secs: f64,
    audio: Option<&ClipSource>,
    intro_secs: Option<f64>,
    output: &Path,
) -> Vec<OsString> {
    let d = format!("{duration_secs:.3}");
    let mut a = custom_menu_graph(frame, &rate.ffmpeg(), media, duration_secs, audio, "16/9", intro_secs);
    let mut x264 = String::from("slices=4:b-pyramid=strict:nal-hrd=vbr:aud=1:open-gop=0:weightp=1");
    if rate.fake_interlaced {
        x264.push_str(":fake-interlaced=1:pic-struct=1");
    }
    a.extend(["-map", "[vout]", "-map", "[aout]", "-t", &d].map(OsString::from));
    if let Some(t) = intro_secs {
        // Rounded down: the keyframe lands on the first frame at or after the mark, never one later.
        a.extend(["-force_key_frames".into(), format!("{:.6}", (t * 1e6).floor() / 1e6).into()]);
    }
    a.extend(["-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level:v", "4.1", "-bluray-compat", "1"].map(OsString::from));
    a.extend(["-b:v", "8000k", "-maxrate", "15000k", "-bufsize", "15000k"].map(OsString::from));
    a.extend(["-g".into(), rate.keyint().to_string().into(), "-bf".into(), "3".into(), "-x264-params".into(), x264.into()]);
    a.extend(["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709"].map(OsString::from));
    a.extend(["-c:a", "ac3", "-ar", "48000", "-b:a", "192k", "-f", "matroska"].map(OsString::from));
    a.push(output.as_os_str().to_owned());
    a
}

/// `-loop`ed still picture input.
fn still_input(a: &mut Vec<OsString>, rate: &str, d: &str, png: &Path) {
    a.extend(["-loop", "1", "-framerate", rate, "-t", d, "-i"].map(OsString::from));
    a.push(png.as_os_str().to_owned());
}

/// Looping clip input starting at the clip's own start.
fn clip_input_args(a: &mut Vec<OsString>, d: &str, clip: &ClipSource) {
    a.extend(["-stream_loop", "-1", "-ss", &format!("{:.3}", clip.start_secs), "-t", d, "-i"].map(OsString::from));
    a.push(clip.path.as_os_str().to_owned());
}

/// The page background: colour, still or clip (optionally blurred) plus its shading. Pushes input 0
/// onto `a` and the filters onto `f`; the result is the RGBA stream `[bg]`. Used by the page graph
/// and by the intro's helper pass, so both make the same pictures.
fn background_chain(frame: &MenuFrame, rate: &str, media: &CustomMenuMedia, d: &str, a: &mut Vec<OsString>, f: &mut Vec<String>) {
    let (fw, fh) = (frame.width, frame.height);
    let square = menu_pixels_for(frame.pixel_aspect, false);
    let blur = |b: f32| if b > 0.0 { format!(",gblur=sigma={b:.1}") } else { String::new() };
    // Applied before the conversion to RGBA (vignette) resp. right after it (dim).
    let (pre_rgba, post_rgba) = match media.overlay {
        BackgroundOverlay::Vignette => (",vignette=angle=PI/4", ""),
        BackgroundOverlay::Dim => ("", ",colorchannelmixer=rr=0.6:gg=0.6:bb=0.6"),
        BackgroundOverlay::None | BackgroundOverlay::BottomDark => ("", ""),
    };
    let bg_label = if media.overlay == BackgroundOverlay::BottomDark { "bg0" } else { "bg" };
    match &media.background {
        MenuBackground::Color([r, g, b]) => {
            a.extend(["-f", "lavfi", "-i"].map(OsString::from));
            a.push(format!("color=c=0x{r:02X}{g:02X}{b:02X}:s={fw}x{fh}:r={rate}:d={d}").into());
            let pre = pre_rgba.trim_start_matches(',');
            let pre = if pre.is_empty() { String::new() } else { format!("{pre},") };
            f.push(format!("[0:v]{pre}format=rgba{post_rgba},setsar=1[{bg_label}]"));
        }
        MenuBackground::Image { path, blur: b } => {
            still_input(a, rate, d, path);
            f.push(format!(
                "[0:v]fps={rate},{square},{}{}{pre_rgba},format=rgba{post_rgba},setsar=1[{bg_label}]",
                fit_filter(Fit::Cover, fw, fh, "black"),
                blur(*b)
            ));
        }
        MenuBackground::Video { clip, blur: b } => {
            clip_input_args(a, d, clip);
            f.push(format!(
                "[0:v]fps={rate},{square},{}{}{pre_rgba},format=rgba{post_rgba},setsar=1,trim=duration={d},setpts=PTS-STARTPTS[{bg_label}]",
                fit_filter(Fit::Cover, fw, fh, "black"),
                blur(*b)
            ));
        }
    }
    if media.overlay == BackgroundOverlay::BottomDark {
        // Black with an alpha ramp from 35 % of the height down (max. 84 %), rendered once and
        // repeated for the whole loop.
        f.push(format!(
            "color=c=black:s={fw}x{fh}:r={rate}:d=0.04,format=rgba,\
             geq=r=0:g=0:b=0:a='215*clip((Y/H-0.35)/0.65\\,0\\,1)',\
             loop=loop=-1:size=1,trim=duration={d},setpts=N/FRAME_RATE/TB[shade]"
        ));
        f.push("[bg0][shade]overlay=0:0:format=rgb[bg]".to_string());
    }
}

/// Helper pass of the intro: the first `frames` pictures of the page background (the same chain as
/// the page graph: picture / clip, blur, shading) as raw RGBA on stdout, the base the intro frames
/// are composited on.
pub fn intro_background_stream(frame: &MenuFrame, rate: BdRate, media: &CustomMenuMedia, frames: usize) -> crate::tools::RawStream {
    let rate_s = rate.ffmpeg();
    // A little longer than needed; `-frames:v` ends the stream.
    let d = format!("{:.3}", frames as f64 / rate.fps_f64() + 1.0);
    let mut a: Vec<OsString> = Vec::new();
    let mut f: Vec<String> = Vec::new();
    background_chain(frame, &rate_s, media, &d, &mut a, &mut f);
    a.extend(["-filter_complex".into(), f.join(";").into()]);
    a.extend(["-map", "[bg]", "-frames:v"].map(OsString::from));
    a.extend([frames.to_string().into(), "-f".into(), "rawvideo".into(), "-pix_fmt".into(), "rgba".into(), "pipe:1".into()]);
    crate::tools::RawStream { args: a, frame_bytes: frame.width as usize * frame.height as usize * 4 }
}

/// Filters (ending in `[aout]`) turning the menu sound input into a loop that restarts cleanly.
/// Without intro the sound fades in and out. With an intro of `t` seconds the restart jumps from
/// the clip's end back to the mark `t`: the last `x` seconds fade from the clip's own end into the
/// sound at [t - x, t], so the signal continues.
fn menu_sound_filters(f: &mut Vec<String>, audio_input: usize, d: &str, duration_secs: f64, intro_secs: Option<f64>) {
    let head = format!("[{audio_input}:a:0]{DVD_AUDIO_SYNC},aformat=sample_fmts=fltp:channel_layouts=stereo");
    match intro_secs {
        Some(t) => {
            let x = t.min(0.5);
            f.push(format!("{head},asplit=2[am][as]"));
            f.push(format!("[as]atrim=start={:.6}:end={t:.6},asetpts=N/SR/TB[tail]", t - x));
            f.push(format!("[am]atrim=duration={d},asetpts=N/SR/TB,afade=t=in:d=0.5[main]"));
            f.push(format!("[main][tail]acrossfade=d={x:.6}:c1=tri:c2=tri[aout]"));
        }
        None => {
            let fade_out = (duration_secs - 1.0).max(0.0);
            f.push(format!("{head},atrim=duration={d},asetpts=N/SR/TB,afade=t=in:d=0.5,afade=t=out:st={fade_out:.3}:d=1[aout]"));
        }
    }
}

/// Inputs and `-filter_complex` of a custom menu (everything before the output options);
/// the graph ends in `[vout]` and `[aout]`.
fn custom_menu_graph(
    frame: &MenuFrame,
    rate: &str,
    media: &CustomMenuMedia,
    duration_secs: f64,
    audio: Option<&ClipSource>,
    dar: &str,
    intro_secs: Option<f64>,
) -> Vec<OsString> {
    let d = format!("{duration_secs:.3}");
    let (fw, fh) = (frame.width, frame.height);
    let mut a: Vec<OsString> = vec!["-y".into()];
    let still = |a: &mut Vec<OsString>, png: &Path| still_input(a, rate, &d, png);
    let clip_input = |a: &mut Vec<OsString>, clip: &ClipSource| clip_input_args(a, &d, clip);
    let mut f: Vec<String> = Vec::new();

    // Input 0: background, then the shading over it (ends in `[bg]`).
    background_chain(frame, rate, media, &d, &mut a, &mut f);

    // Inputs 1..=n: layers, overlaid bottom to top.
    let mut cur = "bg".to_string();
    for (i, layer) in media.layers.iter().enumerate() {
        let input = i + 1;
        match layer {
            MenuLayer::Image(png) => {
                still(&mut a, png);
                f.push(format!("[{input}:v]fps={rate},format=rgba,setsar=1[l{i}]"));
                f.push(format!("[{cur}][l{i}]overlay=0:0:format=rgb[m{i}]"));
            }
            MenuLayer::Video { clip, x, y, width, height, rotation, fit, opacity } => {
                clip_input(&mut a, clip);
                let mut chain = format!("[{input}:v]{}", video_layer_filters(frame, rate, *width, *height, *rotation, *fit));
                if *opacity < 1.0 {
                    chain.push_str(&format!(",colorchannelmixer=aa={:.3}", opacity.clamp(0.0, 1.0)));
                }
                chain.push_str(&format!(",trim=duration={d},setpts=PTS-STARTPTS[l{i}]"));
                f.push(chain);
                f.push(format!("[{cur}][l{i}]overlay=x={x}:y={y}:format=rgb:eof_action=repeat[m{i}]"));
            }
        }
        cur = format!("m{i}");
    }
    // The intro covers everything below it until its last frame, then the page underneath shows.
    let audio_input = media.layers.len() + 1;
    if intro_secs.is_some() {
        f.push(format!("[{}:v]setsar=1[intro]", audio_input + 1));
        f.push(format!("[{cur}][intro]overlay=0:0:format=rgb:eof_action=pass[mi]"));
        cur = "mi".to_string();
    }
    f.push(format!("[{cur}]format=yuv420p,setdar={dar}[vout]"));

    // Next input: the menu sound or silence.
    match audio {
        Some(src) => {
            clip_input(&mut a, src);
            menu_sound_filters(&mut f, audio_input, &d, duration_secs, intro_secs);
        }
        None => {
            a.extend(["-f", "lavfi", "-t", &d, "-i", "anullsrc=r=48000:cl=stereo"].map(OsString::from));
            f.push(format!("[{audio_input}:a]asetpts=N/SR/TB[aout]"));
        }
    }
    // Last input: the intro frames (the caller writes them as `-f rawvideo` to stdin).
    if intro_secs.is_some() {
        a.extend(["-f", "rawvideo", "-pix_fmt", "rgba", "-s"].map(OsString::from));
        a.extend([format!("{fw}x{fh}"), "-r".into(), rate.to_string(), "-i".into(), "pipe:0".into()].map(OsString::from));
    }

    a.extend(["-filter_complex".into(), f.join(";").into()]);
    a
}

/// Arguments turning a menu still into a 1-second DVD menu MPEG with silent AC-3 audio.
pub fn menu_still_args(tv: TvStandard, png: &Path, output: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> =
        vec!["-y".into(), "-loop".into(), "1".into(), "-framerate".into(), tv.frame_rate().into(), "-i".into()];
    a.push(png.as_os_str().to_owned());
    a.extend(
        [
            "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-t", "1", "-target", tv.ffmpeg_target(),
            "-aspect", "4:3", "-b:v", "6000k", "-c:a", "ac3", "-b:a", "192k",
        ]
        .map(OsString::from),
    );
    a.push(output.as_os_str().to_owned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::SubtitleKind;

    #[test]
    fn waveform_peaks_fold_amplitude() {
        // 50 samples per peak: silence, a full swing, a small swing, then a short tail.
        let mut pcm = vec![128u8; 50];
        pcm.extend([128u8; 49].iter().chain(&[0]));
        pcm.extend([138u8; 50]);
        pcm.extend([128u8, 255]);
        assert_eq!(waveform_peaks(&pcm), vec![0, 255, 20, 254]);
    }

    fn video(w: u32, h: u32, dar: f64, field_order: &str) -> VideoStream {
        VideoStream {
            index: 0,
            codec: "h264".into(),
            width: w,
            height: h,
            display_aspect: dar,
            fps: 23.976,
            field_order: field_order.into(),
            crop: None,
            hdr: false,
            profile: None,
            level: None,
            pix_fmt: String::new(),
            bit_rate: None,
        }
    }

    #[test]
    fn aspect_choice_and_bars() {
        assert_eq!(Aspect::for_source(16.0 / 9.0), Aspect::Wide);
        assert_eq!(Aspect::for_source(2.39), Aspect::Wide);
        assert_eq!(Aspect::for_source(4.0 / 3.0), Aspect::Standard);
        assert_eq!(Aspect::for_source(1.5), Aspect::Standard);
        let pal = TvStandard::Pal.size();
        assert_eq!(active_size(16.0 / 9.0, Aspect::Wide, pal), (720, 576));
        assert_eq!(active_size(2.39, Aspect::Wide, pal), (720, 428)); // letterbox
        assert_eq!(active_size(1.5, Aspect::Standard, pal), (720, 512)); // letterbox
        assert_eq!(active_size(1.25, Aspect::Standard, pal), (676, 576)); // pillarbox
        assert_eq!(active_size(16.0 / 9.0, Aspect::Wide, TvStandard::Ntsc.size()), (720, 480));
        assert_eq!(active_size(2.39, Aspect::Wide, TvStandard::Ntsc.size()), (720, 358));
        // Reduced frames: bars scale with the frame height.
        assert_eq!(active_size(16.0 / 9.0, Aspect::Standard, (352, 576)), (352, 432));
        assert_eq!(active_size(16.0 / 9.0, Aspect::Standard, (352, 288)), (352, 216));
        assert_eq!(active_size(4.0 / 3.0, Aspect::Standard, (352, 240)), (352, 240));
    }

    #[test]
    fn resolutions() {
        assert_eq!(Resolution::FullD1.frame_size(TvStandard::Pal), (720, 576));
        assert_eq!(Resolution::HalfD1.frame_size(TvStandard::Pal), (352, 576));
        assert_eq!(Resolution::HalfD1.frame_size(TvStandard::Ntsc), (352, 480));
        assert_eq!(Resolution::Sif.frame_size(TvStandard::Pal), (352, 288));
        assert_eq!(Resolution::Sif.frame_size(TvStandard::Ntsc), (352, 240));
        assert!(Resolution::FullD1.allows_wide() && !Resolution::HalfD1.allows_wide() && !Resolution::Sif.allows_wide());
        let src = video(1920, 1080, 16.0 / 9.0, "progressive");
        assert_eq!(
            video_filter(&src, Aspect::Standard, TvStandard::Pal, Resolution::HalfD1, VideoProcessing::default()),
            "fps=25,scale=352:432:flags=bicubic:out_color_matrix=bt601:out_range=tv,pad=352:576:(ow-iw)/2:(oh-ih)/2:black,setdar=4/3,format=yuv420p"
        );
        let sif = video_filter(&video(640, 480, 4.0 / 3.0, "progressive"), Aspect::Standard, TvStandard::Ntsc, Resolution::Sif, VideoProcessing::default());
        assert_eq!(sif, "fps=30000/1001,scale=352:240:flags=bicubic:out_color_matrix=bt601:out_range=tv,setdar=4/3,format=yuv420p");
    }

    #[test]
    fn filters() {
        let f = video_filter(&video(1920, 1080, 16.0 / 9.0, "progressive"), Aspect::Wide, TvStandard::Pal, Resolution::FullD1, VideoProcessing::default());
        assert_eq!(f, "fps=25,scale=720:576:flags=bicubic:out_color_matrix=bt601:out_range=tv,setdar=16/9,format=yuv420p");
        let n = video_filter(&video(1920, 1080, 16.0 / 9.0, "progressive"), Aspect::Wide, TvStandard::Ntsc, Resolution::FullD1, VideoProcessing::default());
        assert!(n.starts_with("fps=30000/1001,scale=720:480:"), "{n}");
        let f = video_filter(&video(1920, 800, 2.4, "tt"), Aspect::Wide, TvStandard::Pal, Resolution::FullD1, VideoProcessing::default());
        assert!(f.starts_with("bwdif=mode=send_frame,fps=25,scale=720:426:"), "{f}");
        assert!(f.contains("pad=720:576:(ow-iw)/2:(oh-ih)/2:black"));
    }

    #[test]
    fn audio_and_rates() {
        assert_eq!(ac3_settings(6), (448, 6));
        assert_eq!(ac3_settings(8), (448, 6));
        assert_eq!(ac3_settings(2), (192, 2));
        assert_eq!(ac3_settings(1), (192, 2));
        assert_eq!(max_video_rate(192), 9588);
        assert_eq!(max_video_rate(448 + 192), 9140);
    }

    #[test]
    fn encode_and_mux_args() {
        let info = MediaInfo {
            path: "in.mkv".into(),
            duration_secs: 60.0,
            video: Some(video(1920, 1080, 16.0 / 9.0, "progressive")),
            audio: vec![
                AudioStream { index: 1, codec: "eac3".into(), channels: 6, channel_layout: "5.1".into(), language: Some("ger".into()), title: None, default: true, bit_rate: None, sample_rate: None, profile: None, bits_per_raw_sample: None },
                AudioStream { index: 2, codec: "aac".into(), channels: 2, channel_layout: "stereo".into(), language: None, title: None, default: false, bit_rate: None, sample_rate: None, profile: None, bits_per_raw_sample: None },
            ],
            subtitles: vec![SubtitleStream { index: 3, codec: "subrip".into(), kind: SubtitleKind::Text, language: None, title: None, forced: false, width: None, height: None }],
            chapters: vec![],
            trim: None, audio_names: Default::default(),
        };
        let job = EncodeJob {
            info: &info,
            audio: info.audio.iter().collect(),
            subtitles: info.subtitles.iter().collect(),
            video_kbps: 5000,
            aspect: Aspect::Wide,
            dir: PathBuf::from("w"),
            stem: "title_01".into(),
            two_pass: false,
            fast: false,
            tv: TvStandard::Pal,
            resolution: Resolution::FullD1,
            video: Default::default(),
            hwaccel: None,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![],
            audio_filters: vec![],
            copy_video: false,
            subtitle_style: Default::default(),
        };
        let args: Vec<String> = job.encode_args("pipe:1").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let joined = args.join(" ");
        assert!(!joined.contains("-pass"), "1-pass must not use pass options: {joined}");
        assert!(!joined.contains("-hwaccel"));
        assert!(joined.starts_with("-y -i in.mkv -map 0:0 -vf fps=25"), "{joined}");
        assert!(joined.contains("-c:v mpeg2video -threads 0"));
        assert!(joined.contains("-g 12 -bf 2"));
        assert!(joined.contains("-b:v 5000k -maxrate 9140k -bufsize 1835k -aspect 16:9"));
        // Video and both AC-3 tracks go straight into one DVD program stream (no mux pass).
        assert!(joined.contains("-map 0:1 -map 0:2 -c:a:0 ac3 -ar:a:0 48000 -b:a:0 448k -ac:a:0 6 -filter:a:0 aresample=48000:async=1:min_hard_comp=0.01 -c:a:1 ac3 -ar:a:1 48000 -b:a:1 192k -ac:a:1 2 -filter:a:1 aresample=48000:async=1:min_hard_comp=0.01"), "{joined}");
        assert!(joined.contains("-flags +cgop -sc_threshold 1000000000"), "{joined}");
        let first: Vec<String> = job.first_pass_args().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(first.join(" ").contains("-flags +cgop -sc_threshold 1000000000"));
        assert!(joined.ends_with("-sn -dn -f dvd -muxrate 10080k -packetsize 2048 pipe:1"), "{joined}");
        assert!(!joined.contains(".m2v") && !joined.contains(".ac3") && !joined.contains("srt"));

        // Text subtitles are extracted up front for spumux.
        let subs: Vec<String> = job.subtitle_args().unwrap().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let p = |n: &str| PathBuf::from("w").join(n).to_string_lossy().into_owned();
        assert!(subs.join(" ").contains("-map 0:3 -c:s srt -vn -an -f srt"));
        assert_eq!(subs.last().unwrap(), &p("title_01_s1.srt"));
        assert_eq!(job.subtitle_xml(0), "title_01_s1.xml");
        assert_eq!(job.vob(), "title_01.mpg");

        // GPU decoding is an input option.
        let hw = EncodeJob { hwaccel: Some("cuda"), ..job.clone() };
        let a: Vec<String> = hw.encode_args("x.mpg").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(a.join(" ").starts_with("-y -hwaccel cuda -i in.mkv"));
        let none = EncodeJob { subtitles: vec![], ..job.clone() };
        assert!(none.subtitle_args().is_none());

        // Fast mode drops -trellis 1 and -intra_vlc 1 on final/single-pass encodes; Pass 1 always
        // uses fast statistics-only settings regardless of `fast`.
        let strings = |a: Vec<OsString>| a.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let without_quality_tuning = |a: Vec<String>| {
            let i = a.iter().position(|x| x == "-trellis").expect("default encode uses trellis");
            assert_eq!(a[i - 2..=i + 3], ["-sc_threshold", "1000000000", "-trellis", "1", "-intra_vlc", "1"]);
            [&a[..i], &a[i + 4..]].concat()
        };
        let fast = EncodeJob { fast: true, two_pass: true, ..job.clone() };
        let slow = EncodeJob { two_pass: true, ..job };

        let fast_enc = strings(fast.encode_args("x.mpg"));
        let slow_enc = strings(slow.encode_args("x.mpg"));
        assert!(!fast_enc.iter().any(|x| x == "-trellis" || x == "-intra_vlc"), "{fast_enc:?}");
        assert!(fast_enc.join(" ").contains("-mbd rd -flags +cgop -sc_threshold 1000000000 -color_primaries bt470bg"), "{fast_enc:?}");
        assert!(slow_enc.join(" ").contains("-mbd rd -flags +cgop -sc_threshold 1000000000 -trellis 1 -intra_vlc 1 -color_primaries bt470bg"), "{slow_enc:?}");
        assert_eq!(fast_enc, without_quality_tuning(slow_enc));

        let fast_p1 = strings(fast.first_pass_args());
        let slow_p1 = strings(slow.first_pass_args());
        assert_eq!(fast_p1, slow_p1);
        let p1_joined = slow_p1.join(" ");
        assert!(
            p1_joined.contains(
                "-g 12 -bf 2 -dc 9 -qmin 2 -mbd simple -flags +cgop -sc_threshold 1000000000 -trellis 0 -dia_size 1 -subq 1 -cmp sad -subcmp sad -mbcmp sad -color_primaries bt470bg"
            ),
            "{p1_joined}"
        );
        assert!(p1_joined.contains("-b:v 5000k -maxrate 9140k -bufsize 1835k"), "{p1_joined}");
        assert!(p1_joined.contains("-pass 1 -passlogfile "), "{p1_joined}");
        assert!(!p1_joined.contains("-mbd rd") && !p1_joined.contains("-trellis 1") && !p1_joined.contains("-intra_vlc"), "{p1_joined}");
    }

    fn slot(x: u32, has_audio: bool, letterbox: bool) -> VideoSlot {
        VideoSlot {
            x,
            y: 106,
            width: 174,
            height: 139,
            letterbox,
            clip: crate::author::ClipSource { path: format!("src{x}.mkv").into(), start_secs: 12.5, has_audio },
        }
    }

    fn joined(a: &[OsString]) -> String {
        a.iter().map(|x| x.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn motion_menu_graph() {
        let slots = [slot(95, true, true), slot(273, false, false)];
        let menu_audio = ClipSource { path: "title1.mkv".into(), start_secs: 30.0, has_audio: true };
        let a = motion_menu_args(TvStandard::Pal, Path::new("bg.png"), Some(Path::new("fg.png")), &slots, 25.0, Some(&menu_audio), None, Path::new("m.mpg"));
        let s = joined(&a);
        assert!(s.starts_with("-y -loop 1 -framerate 25 -t 25.000 -i bg.png"), "{s}");
        assert!(s.contains("-stream_loop -1 -ss 12.500 -t 25.000 -i src95.mkv"));
        assert!(s.contains("-loop 1 -framerate 25 -t 25.000 -i fg.png"));
        assert!(!s.contains("anullsrc"), "menu audio given");
        // The menu sound is its own input (4 = after base, 2 clips, foreground), not a slot clip.
        assert!(s.contains("-stream_loop -1 -ss 30.000 -t 25.000 -i title1.mkv"));
        // Letterboxed and stretched slots, overlaid at the template coordinates.
        assert!(s.contains("scale=174:139:force_original_aspect_ratio=decrease,pad=174:139"));
        assert!(s.contains("[2:v]fps=25,scale=trunc(ih*dar/2)*2:ih,setsar=1,scale=174:139,setsar=1"));
        assert!(s.contains("[base][clip0]overlay=x=95:y=106"));
        assert!(s.contains("[m0][clip1]overlay=x=273:y=106"));
        // Foreground (frames, captions) goes on top, then the menu sound with fades.
        assert!(s.contains("[3:v]fps=25,format=rgba,setsar=1[fg];[m1][fg]overlay=0:0"));
        assert!(s.contains("[4:a:0]aresample=48000"));
        assert!(s.contains("afade=t=out:st=24.000:d=1[aout]"));
        assert!(s.ends_with("-c:a ac3 -b:a 192k m.mpg"));
        assert!(s.contains("-t 25.000 -target pal-dvd -aspect 4:3"));
    }

    #[test]
    fn motion_menu_silence() {
        let a = motion_menu_args(TvStandard::Pal, Path::new("bg.png"), None, &[slot(0, false, true)], 8.0, None, None, Path::new("m.mpg"));
        let s = joined(&a);
        assert!(s.contains("-f lavfi -t 8.000 -i anullsrc=r=48000:cl=stereo"), "{s}");
        assert!(s.contains("[2:a]asetpts=N/SR/TB[aout]"), "silence is input 2 (no foreground)");
        assert!(!s.contains("[fg]"));
        // Clips with sound do not change the menu sound any more: silence stays silence.
        let silent = joined(&motion_menu_args(TvStandard::Pal, Path::new("b"), None, &[slot(0, true, true)], 8.0, None, None, Path::new("m")));
        assert!(silent.contains("anullsrc") && !silent.contains("aresample"));
        let ntsc = joined(&motion_menu_args(TvStandard::Ntsc, Path::new("b"), None, &[slot(0, true, true)], 8.0, None, None, Path::new("m")));
        assert!(ntsc.contains("-framerate 30000/1001") && ntsc.contains("-target ntsc-dvd"), "{ntsc}");
        assert!(ntsc.contains("fps=30000/1001"));
    }

    #[test]
    fn motion_menu_intro_reads_frames_from_the_pipe() {
        let audio = ClipSource { path: "a.mp3".into(), start_secs: 0.0, has_audio: true };
        let a = joined(&motion_menu_args(TvStandard::Pal, Path::new("bg.png"), None, &[slot(0, false, true)], 28.0, Some(&audio), Some(3.0), Path::new("m.mpg")));
        assert!(a.contains("-f rawvideo -pix_fmt rgba -s 720x576 -r 25 -i pipe:0"), "{a}");
        assert!(a.contains("[mi]format=yuv420p") && a.contains("eof_action=pass"), "{a}");
        assert!(a.contains("acrossfade=d=0.500000"), "{a}");
        assert!(a.contains("-force_key_frames 3.000000 -flags +cgop -sc_threshold 1000000000"), "{a}");
        let ntsc = joined(&motion_menu_args(TvStandard::Ntsc, Path::new("bg.png"), None, &[], 4.0, None, Some(3.0), Path::new("m.mpg")));
        assert!(ntsc.contains("-s 720x480 -r 30000/1001 -i pipe:0"), "{ntsc}");
        let plain = joined(&motion_menu_args(TvStandard::Pal, Path::new("bg.png"), None, &[], 4.0, None, None, Path::new("m.mpg")));
        assert!(!plain.contains("pipe:0") && !plain.contains("force_key_frames") && !plain.contains("cgop"), "{plain}");
        let bg = dvd_background_stream(TvStandard::Pal, [1, 2, 255], 75);
        assert_eq!(bg.frame_bytes, 720 * 576 * 4);
        assert!(joined(&bg.args).contains("color=c=0x0102ff:s=720x576:r=25"));
    }

    #[test]
    fn custom_menu_graph() {
        let clip = |p: &str| ClipSource { path: p.into(), start_secs: 12.5, has_audio: true };
        let media = CustomMenuMedia {
            background: MenuBackground::Image { path: "bg.jpg".into(), blur: 8.0 },
            overlay: BackgroundOverlay::None,
            layers: vec![
                MenuLayer::Image("under.png".into()),
                MenuLayer::Video {
                    clip: clip("movie.mkv"),
                    x: 100,
                    y: 200,
                    width: 160,
                    height: 90,
                    rotation: 90,
                    fit: Fit::Cover,
                    opacity: 0.5,
                },
                MenuLayer::Image("over.png".into()),
            ],
        };
        let a = joined(&custom_menu_args(TvStandard::Pal, &media, 20.0, Some(&clip("music.mp3")), None, Path::new("m.mpg")));
        // Inputs in order: background, layers, sound.
        let order: Vec<usize> = ["bg.jpg", "under.png", "movie.mkv", "over.png", "music.mp3"]
            .iter()
            .map(|n| a.find(&format!("-i {n}")).unwrap_or_else(|| panic!("{n} missing: {a}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{a}");
        // Square pixels, then the PAL menu pixel shape (15/16 horizontally), then the fit.
        assert!(a.contains("[0:v]fps=25,scale=trunc(ih*dar*0.937500/2)*2:ih,setsar=1,scale=720:576:force_original_aspect_ratio=increase,crop=720:576,gblur=sigma=8.0"), "{a}");
        // Rotated by 90°: the height becomes the horizontal, so it is squeezed; fitted into
        // 90x160, then transposed into the 160x90 box.
        assert!(a.contains("scale=trunc(ih*dar/2)*2:trunc(ih*0.937500/2)*2,setsar=1,scale=90:160:force_original_aspect_ratio=increase,crop=90:160,transpose=1"), "{a}");
        assert_eq!(menu_pixels(TvStandard::Ntsc, false), "scale=trunc(ih*dar*1.125000/2)*2:ih,setsar=1", "NTSC pixels are narrow");
        assert!(a.contains("colorchannelmixer=aa=0.500"), "{a}");
        assert!(a.contains("[l1]overlay=x=100:y=200:format=rgb:eof_action=repeat[m1]"), "{a}");
        assert!(a.contains("[m1][l2]overlay=0:0:format=rgb[m2];[m2]format=yuv420p,setdar=4/3[vout]"), "{a}");
        assert!(a.contains("[4:a:0]aresample=48000"), "{a}");
        assert!(a.contains("-t 20.000 -target pal-dvd"), "{a}");

        // Colour background, no layers, silence, NTSC.
        let still = CustomMenuMedia { background: MenuBackground::Color([0x10, 0x20, 0xA0]), overlay: BackgroundOverlay::None, layers: vec![] };
        let a = joined(&custom_menu_args(TvStandard::Ntsc, &still, 1.0, None, None, Path::new("m.mpg")));
        assert!(a.contains("color=c=0x1020A0:s=720x480:r=30000/1001:d=1.000"), "{a}");
        assert!(a.contains("[bg]format=yuv420p,setdar=4/3[vout]") && a.contains("[1:a]asetpts=N/SR/TB[aout]"), "{a}");
        assert!(a.contains("-target ntsc-dvd"));

        // Shading over the background.
        let with = |overlay| {
            let m = CustomMenuMedia { overlay, ..media.clone() };
            joined(&custom_menu_args(TvStandard::Pal, &m, 20.0, None, None, Path::new("m.mpg")))
        };
        let a = with(BackgroundOverlay::Vignette);
        assert!(a.contains("gblur=sigma=8.0,vignette=angle=PI/4,format=rgba,setsar=1[bg]"), "{a}");
        let a = with(BackgroundOverlay::Dim);
        assert!(a.contains("format=rgba,colorchannelmixer=rr=0.6:gg=0.6:bb=0.6,setsar=1[bg]"), "{a}");
        let a = with(BackgroundOverlay::BottomDark);
        assert!(a.contains("setsar=1[bg0]"), "{a}");
        assert!(a.contains("color=c=black:s=720x576:r=25:d=0.04,format=rgba,geq=r=0:g=0:b=0:a='215*clip((Y/H-0.35)/0.65\\,0\\,1)',loop=loop=-1:size=1,trim=duration=20.000"), "{a}");
        // The shaded background is what the first layer goes onto.
        assert!(a.contains("[bg0][shade]overlay=0:0:format=rgb[bg];") && a.contains("[bg][l0]overlay"), "{a}");

        // Custom menu with intro: forced closed GOP at intro split boundary
        let intro_args = joined(&custom_menu_args(TvStandard::Pal, &still, 23.0, None, Some(3.0), Path::new("m.mpg")));
        assert!(intro_args.contains("-force_key_frames expr:gte(t,3.000)"), "{intro_args}");
        assert!(intro_args.contains("-flags +cgop -sc_threshold 1000000000"), "{intro_args}");
    }

    fn strings(a: Vec<OsString>) -> String {
        a.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
    }

    fn bd_info() -> MediaInfo {
        MediaInfo {
            path: "in.mkv".into(),
            duration_secs: 60.0,
            video: Some(video(1920, 1080, 16.0 / 9.0, "progressive")),
            audio: vec![
                AudioStream { index: 1, codec: "eac3".into(), channels: 6, channel_layout: "5.1".into(), language: Some("ger".into()), title: None, default: true, bit_rate: None, sample_rate: None, profile: None, bits_per_raw_sample: None },
                AudioStream { index: 2, codec: "aac".into(), channels: 2, channel_layout: "stereo".into(), language: None, title: None, default: false, bit_rate: None, sample_rate: None, profile: None, bits_per_raw_sample: None },
            ],
            subtitles: vec![],
            chapters: vec![],
            trim: None, audio_names: Default::default(),
        }
    }

    #[test]
    fn bd_video_copy_rules() {
        let film = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let mut info = bd_info();
        let v = info.video.as_mut().unwrap();
        (v.profile, v.level, v.pix_fmt, v.bit_rate) = (Some("High".into()), Some(41), "yuv420p".into(), Some(15_000_000));
        let ok = |i: &MediaInfo, r| bd_video_copy(i, r, Resolution::Hd1080, VideoProcessing::default(), 30_000, false);
        assert_eq!(ok(&info, film), Ok(15_000));
        let bad = |f: &dyn Fn(&mut MediaInfo)| {
            let mut i = info.clone();
            f(&mut i);
            ok(&i, film).unwrap_err()
        };
        assert!(bad(&|i| i.video.as_mut().unwrap().codec = "hevc".into()).contains("HEVC"));
        assert!(bad(&|i| i.video.as_mut().unwrap().profile = Some("High 10".into())).contains("profile"));
        assert!(bad(&|i| i.video.as_mut().unwrap().level = Some(51)).contains("5.1"));
        assert!(bad(&|i| i.video.as_mut().unwrap().width = 1280).contains("1280"));
        assert!(bad(&|i| i.video.as_mut().unwrap().fps = 60.0).contains("fps"));
        assert!(bad(&|i| i.trim = Some(crate::probe::Trim { start: 1.0, end: 9.0 })).contains("trim"));
        assert!(bad(&|i| i.video.as_mut().unwrap().bit_rate = Some(40_000_000)).contains("limit"));
        // 1440×1080 is an AVCHD-only size.
        let mut narrow = info.clone();
        narrow.video.as_mut().unwrap().width = 1440;
        assert!(ok(&narrow, film).unwrap_err().contains("1440"));
        assert_eq!(bd_video_copy(&narrow, film, Resolution::Hd1080, VideoProcessing::default(), 30_000, true), Ok(15_000));
        // 1080p25 does not exist on a BD; real 1080i25 is copied.
        let pal = BdRate { fps: (25, 1), fake_interlaced: true };
        let mut i = info.clone();
        i.video.as_mut().unwrap().fps = 25.0;
        assert!(ok(&i, pal).unwrap_err().contains("not a Blu-ray"));
        i.video.as_mut().unwrap().field_order = "tt".into();
        assert!(ok(&i, pal).is_ok());
        // A compliant film source on a 720 title, or forced to another rate, is re-encoded.
        assert!(bd_video_copy(&info, film, Resolution::Hd720, VideoProcessing::default(), 30_000, false).unwrap_err().contains("title is 1280×720"));
        let f24 = BdRate { fps: (24, 1), fake_interlaced: false };
        assert!(ok(&info, f24).unwrap_err().contains("title runs at 24"));
    }

    #[test]
    fn bd_copy_job_skips_filter_and_encoder() {
        let info = bd_info();
        let job = BdEncodeJob { copy_video: true, two_pass: true, hwaccel: Some("cuda"), ..bd_job(&info) };
        let args = strings(job.encode_args("out.mkv"));
        assert!(args.contains("-c:v copy") && !args.contains("-vf") && !args.contains("-hwaccel"));
        assert!(!job.x264_two_pass());
    }

    fn bd_job(info: &MediaInfo) -> BdEncodeJob<'_> {
        BdEncodeJob {
            info,
            audio: vec![Some(&info.audio[0]), Some(&info.audio[1]), None],
            video_kbps: 20_000,
            dir: PathBuf::from("w"),
            stem: "title_01".into(),
            two_pass: false,
            fast: false,
            rate: BdRate { fps: (24000, 1001), fake_interlaced: false },
            encoder: BdEncoder::X264,
            resolution: Resolution::Hd1080,
            video: Default::default(),
            hwaccel: None,
            audio_mode: AudioMode::Auto,
            bd_audio: BdAudioMode::Auto,
            audio_target: AudioTarget::Bluray,
            audio_filters: vec![],
            maxrate_kbps: BD_MAXRATE_KBPS,
            bufsize_kbit: BD_BUFSIZE_KBIT,
            copy_video: false,
        }
    }

    #[test]
    fn video_processing_modes() {
        let vp = |scale, filter, deinterlace| VideoProcessing { scale, filter, deinterlace, ..Default::default() };
        let scope = video(1920, 800, 2.4, "progressive");
        let pan = vp(ScaleMode::PanScan, ScaleFilter::Bicubic, Deinterlace::Auto);
        assert_eq!(
            video_filter(&scope, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, pan),
            "fps=25,scale=972:576:flags=bicubic:out_color_matrix=bt601:out_range=tv,crop=720:576,setdar=16/9,format=yuv420p"
        );
        // NTSC frame, 4:3 source into 16:9: the height overhangs.
        let n = video_filter(&video(720, 576, 4.0 / 3.0, "progressive"), Aspect::Wide, TvStandard::Ntsc, Resolution::FullD1, pan);
        assert!(n.contains("scale=720:640:") && n.contains("crop=720:480,"), "{n}");
        let stretch = vp(ScaleMode::Stretch, ScaleFilter::Bicubic, Deinterlace::Auto);
        assert_eq!(
            video_filter(&scope, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, stretch),
            "fps=25,scale=720:576:flags=bicubic:out_color_matrix=bt601:out_range=tv,setdar=16/9,format=yuv420p"
        );
        let bd = bd_video_filter(&video(720, 576, 4.0 / 3.0, "progressive"), BdRate { fps: (25, 1), fake_interlaced: false }, Resolution::Hd1080, pan);
        assert_eq!(
            bd,
            "fps=25,scale=1920:1440:flags=bicubic:out_color_matrix=bt709:out_range=tv,crop=1920:1080,setsar=1,format=yuv420p"
        );
        let bd = bd_video_filter(&scope, BdRate { fps: (25, 1), fake_interlaced: false }, Resolution::Hd1080, stretch);
        assert!(bd.contains("scale=1920:1080:") && !bd.contains("pad="), "{bd}");
    }

    #[test]
    fn scale_filter_flags() {
        let v = video(1920, 1080, 16.0 / 9.0, "progressive");
        for (filter, flag) in [(ScaleFilter::Bilinear, "bilinear"), (ScaleFilter::Bicubic, "bicubic"), (ScaleFilter::Lanczos, "lanczos")] {
            let vp = VideoProcessing { filter, ..Default::default() };
            assert!(video_filter(&v, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, vp).contains(&format!("scale=720:576:flags={flag}:")));
            assert!(bd_video_filter(&v, BdRate { fps: (25, 1), fake_interlaced: false }, Resolution::Hd1080, vp).contains(&format!("flags={flag}:")));
        }
    }

    #[test]
    fn deinterlace_modes() {
        let chain = |v: &VideoStream, d| {
            video_filter(v, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, VideoProcessing { deinterlace: d, ..Default::default() })
        };
        let prog = video(1920, 1080, 16.0 / 9.0, "progressive");
        let inter = video(1920, 1080, 16.0 / 9.0, "tt");
        assert!(chain(&prog, Deinterlace::Auto).starts_with("fps=25,"));
        assert!(chain(&inter, Deinterlace::Auto).starts_with("bwdif=mode=send_frame,fps=25,"));
        assert!(chain(&prog, Deinterlace::Yadif).starts_with("yadif=mode=send_frame,fps=25,"));
        assert!(chain(&inter, Deinterlace::Off).starts_with("fps=25,"));
        // The user crop stays directly behind the deinterlacer (or first without one).
        let mut c = inter.clone();
        c.crop = Some(crate::probe::CropRect { x: 0, y: 100, w: 1920, h: 800 });
        assert!(chain(&c, Deinterlace::Auto).starts_with("bwdif=mode=send_frame,crop=1920:800:0:100,fps=25,"));
        assert!(chain(&c, Deinterlace::Off).starts_with("crop=1920:800:0:100,fps=25,"));
    }

    #[test]
    fn hdr_is_tonemapped_before_scaling() {
        let mut v = video(3840, 2160, 16.0 / 9.0, "progressive");
        v.hdr = true;
        let vp = VideoProcessing::default();
        let tm = tonemap_filter(&v).unwrap();
        let r = BdRate { fps: (24000, 1001), fake_interlaced: false };
        for f in [video_filter(&v, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, vp), bd_video_filter(&v, r, Resolution::Hd1080, vp)] {
            assert!(f.starts_with(&format!("{tm},fps=")), "{f}");
        }
        // The player tonemaps after its downscale.
        assert!(preview_filter(&v, vp, 480).starts_with(&format!("scale=854:480:flags=bicubic,{tm},setsar=1")));
    }

    #[test]
    fn preview_filter_chain() {
        let mut v = video(1920, 1080, 16.0 / 9.0, "tt");
        v.crop = Some(crate::probe::CropRect { x: 0, y: 100, w: 1920, h: 800 });
        v.display_aspect = 2.4;
        assert_eq!(
            preview_filter(&v, VideoProcessing::default(), 480),
            "bwdif=mode=send_frame,crop=1920:800:0:100,scale=1152:480:flags=bicubic,setsar=1,format=yuv420p"
        );
    }

    #[test]
    fn bd_filter_chain() {
        let r = BdRate { fps: (24000, 1001), fake_interlaced: false };
        assert_eq!(
            bd_video_filter(&video(1920, 1080, 16.0 / 9.0, "progressive"), r, Resolution::Hd1080, VideoProcessing::default()),
            "fps=24000/1001,scale=1920:1080:flags=bicubic:out_color_matrix=bt709:out_range=tv,setsar=1,format=yuv420p"
        );
        // 4:3 sources are pillarboxed into the 16:9 frame; interlaced ones are deinterlaced first.
        let f = bd_video_filter(&video(720, 576, 4.0 / 3.0, "tt"), BdRate { fps: (25, 1), fake_interlaced: true }, Resolution::Hd1080, VideoProcessing::default());
        assert_eq!(
            f,
            "bwdif=mode=send_frame,fps=25,scale=1440:1080:flags=bicubic:out_color_matrix=bt709:out_range=tv,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,setsar=1,format=yuv420p"
        );
        // Scope films are letterboxed, 720p uses its own frame and doubled TV rate.
        let f = bd_video_filter(&video(1920, 800, 2.4, "progressive"), BdRate { fps: (50, 1), fake_interlaced: false }, Resolution::Hd720, VideoProcessing::default());
        assert!(f.starts_with("fps=50,scale=1280:534:") && f.contains("pad=1280:720:(ow-iw)/2:(oh-ih)/2:black"), "{f}");
    }

    #[test]
    fn bd_x264_flags() {
        let info = bd_info();
        let job = bd_job(&info);
        let a = strings(job.encode_args("out.mkv"));
        assert!(
            a.contains(
                "-c:v libx264 -preset slow -profile:v high -level:v 4.1 -bluray-compat 1 -b:v 20000k -maxrate 40000k -bufsize 30000k \
                 -g 24 -bf 3 -x264-params slices=4:b-pyramid=strict:nal-hrd=vbr:aud=1:open-gop=0:weightp=1 \
                 -color_primaries bt709 -color_trc bt709 -colorspace bt709"
            ),
            "{a}"
        );
        assert!(!a.contains("fake-interlaced") && !a.contains("-pass"), "{a}");
        // 1080 at a TV rate: progressive frames flagged as 1080i, one-second GOPs.
        let tv = BdEncodeJob { rate: BdRate { fps: (30000, 1001), fake_interlaced: true }, ..job.clone() };
        let a = strings(tv.encode_args("out.mkv"));
        assert!(a.contains("-g 30 ") && a.contains("weightp=1:fake-interlaced=1:pic-struct=1"), "{a}");
        // 720p50 is real progressive: no flag.
        let hd = BdEncodeJob { rate: BdRate { fps: (50, 1), fake_interlaced: false }, resolution: Resolution::Hd720, ..job.clone() };
        let a = strings(hd.encode_args("out.mkv"));
        assert!(a.contains("-g 50 ") && !a.contains("fake-interlaced") && a.contains("scale=1280:720"), "{a}");
        // Fast mode only changes the preset.
        let fast = strings(BdEncodeJob { fast: true, ..job.clone() }.encode_args("out.mkv"));
        assert_eq!(fast, strings(job.encode_args("out.mkv")).replace("-preset slow", "-preset veryfast"));
        assert_eq!(job.mkv(), "title_01.mkv");
    }

    #[test]
    fn bd_nvenc_args() {
        let info = bd_info();
        let job = BdEncodeJob { encoder: BdEncoder::Nvenc, ..bd_job(&info) };
        let a = strings(job.encode_args("out.mkv"));
        for want in ["-c:v h264_nvenc -preset p5 -tune hq", "-bluray-compat 1", "-aud 1", "-slices 4", "-rc vbr", "-strict_gop 1", "-maxrate 40000k", "-bf 3"] {
            assert!(a.contains(want), "{want} in {a}");
        }
        assert!(!a.contains("x264") && !a.contains("-multipass") && !a.contains("-pass"), "{a}");
        assert!(strings(BdEncodeJob { fast: true, ..job.clone() }.encode_args("o")).contains("-preset p3"));
        // 2-pass is -multipass fullres inside one run: no analysis run, no passlog.
        let two = BdEncodeJob { two_pass: true, ..job };
        assert!(!two.x264_two_pass());
        let a = strings(two.encode_args("out.mkv"));
        assert!(a.contains("-multipass fullres") && !a.contains("-pass") && !a.contains("passlogfile"), "{a}");
    }

    #[test]
    fn bd_qsv_args() {
        let info = bd_info();
        let job = BdEncodeJob { encoder: BdEncoder::Qsv, two_pass: true, hwaccel: Some("qsv"), ..bd_job(&info) };
        let a = strings(job.encode_args("out.mkv"));
        assert!(a.starts_with("-y -hwaccel qsv -i in.mkv"), "{a}");
        for want in ["-c:v h264_qsv -preset slow", "-slices 4 -flags +cgop", "-aud 1", "-forced_idr 1", "-maxrate 40000k", "-bf 3"] {
            assert!(a.contains(want), "{want} in {a}");
        }
        assert!(!a.contains("x264") && !a.contains("-pass") && !job.x264_two_pass(), "{a}");
        assert!(strings(BdEncodeJob { fast: true, ..job }.encode_args("o")).contains("-preset veryfast"));
    }

    #[test]
    fn bd_amf_args() {
        let info = bd_info();
        let job = BdEncodeJob { encoder: BdEncoder::Amf, two_pass: true, hwaccel: Some("d3d11va"), ..bd_job(&info) };
        let a = strings(job.encode_args("out.mkv"));
        assert!(a.starts_with("-y -hwaccel d3d11va -i in.mkv"), "{a}");
        for want in ["-c:v h264_amf -quality quality", "-usage transcoding", "-rc vbr_peak", "-enforce_hrd 1", "-aud 1", "-maxrate 40000k", "-bf 3"] {
            assert!(a.contains(want), "{want} in {a}");
        }
        assert!(!a.contains("x264") && !a.contains("-pass") && !job.x264_two_pass(), "{a}");
        assert!(strings(BdEncodeJob { fast: true, ..job }.encode_args("o")).contains("-quality speed"));
    }

    #[test]
    fn bd_silence_fills_missing_tracks() {
        let info = bd_info();
        let job = bd_job(&info);
        let a = strings(job.encode_args("out.mkv"));
        assert!(a.starts_with("-y -i in.mkv -f lavfi -t 60.000 -i anullsrc=r=48000:cl=stereo -map 0:0 -vf "), "{a}");
        assert!(a.contains("-map 0:1 -map 0:2 -map 1:a:0 -c:a:0 ac3"), "{a}");
        assert!(a.contains("-b:a:0 448k -ac:a:0 6 -c:a:1 ac3 -ar:a:1 48000 -b:a:1 192k -ac:a:1 2 -c:a:2 ac3 -ar:a:2 48000 -b:a:2 192k -ac:a:2 2"), "{a}");
        assert!(!a.contains("apad"), "{a}");
        assert!(a.ends_with("-t 60.000 -map_chapters -1 -sn -dn -f matroska out.mkv"), "{a}");
        assert_eq!(job.audio_kbps_total(), 448 + 192 + 192);
        // No silent track: no extra input.
        let plain = strings(BdEncodeJob { audio: vec![Some(&info.audio[1])], ..job.clone() }.encode_args("out.mkv"));
        assert!(plain.starts_with("-y -i in.mkv -map 0:0") && !plain.contains("lavfi"), "{plain}");
        // Hardware decoding only changes the input side.
        let hw = strings(BdEncodeJob { hwaccel: Some("cuda"), ..job }.encode_args("out.mkv"));
        assert!(hw.starts_with("-y -hwaccel cuda -i in.mkv"), "{hw}");
    }

    #[test]
    fn bd_two_pass() {
        let info = bd_info();
        let job = BdEncodeJob { two_pass: true, ..bd_job(&info) };
        let first = strings(job.first_pass_args());
        assert!(first.contains("-pass 1 -passlogfile") && first.contains("title_01_2pass"), "{first}");
        assert!(first.ends_with(&format!("-an -sn -f rawvideo {NULL_DEVICE}")) && !first.contains("lavfi") && !first.contains("-c:a"), "{first}");
        let last = strings(job.encode_args("out.mkv"));
        assert!(last.contains("-pass 2 -passlogfile") && last.contains("-c:a:0 ac3"), "{last}");
        // Same video settings in both runs.
        let video = |s: &str| s[s.find("-c:v libx264").unwrap()..s.find("-pass").unwrap()].to_string();
        assert_eq!(video(&first), video(&last));
    }

    #[test]
    fn subtitle_extraction_by_track() {
        let sub = |index, codec: &str, kind| SubtitleStream { index, codec: codec.into(), kind, language: None, title: None, forced: false, width: None, height: None };
        let text = sub(3, "subrip", SubtitleKind::Text);
        let pgs = sub(4, "hdmv_pgs_subtitle", SubtitleKind::Bitmap);
        let vob = sub(5, "dvd_subtitle", SubtitleKind::Bitmap);
        // Track numbers pick the file names (BD tracks may skip positions a title lacks).
        let a = strings(extract_subtitle_args(&bd_info(), &[(0, &text), (1, &pgs), (2, &vob)], Path::new("w"), "title_02").unwrap());
        assert!(a.starts_with("-y -i in.mkv -map 0:3 -c:s srt -vn -an -f srt "), "{a}");
        assert!(a.contains("title_02_s1.srt") && a.contains("-map 0:4 -c:s copy -vn -an -f sup ") && a.contains("title_02_s2.sup"), "{a}");
        assert!(!a.contains("0:5"), "VobSub is not extracted: {a}");
        assert!(extract_subtitle_args(&bd_info(), &[(0, &vob)], Path::new("w"), "t").is_none());
        assert_eq!(subtitle_file("title_01", 2, "sup"), "title_01_s3.sup");
    }

    fn audio(codec: &str, channels: u32, kbps: Option<u32>, rate: Option<u32>, profile: Option<&str>) -> AudioStream {
        AudioStream {
            index: 1,
            codec: codec.into(),
            channels,
            channel_layout: String::new(),
            language: None,
            title: None,
            default: false,
            bit_rate: kbps.map(|k| k * 1000),
            sample_rate: rate,
            profile: profile.map(str::to_string),
            bits_per_raw_sample: None,
        }
    }

    #[test]
    fn audio_copy_rules() {
        use AudioMode::*;
        use AudioTarget::*;
        let ac3 = |kbps| audio("ac3", 6, Some(kbps), Some(48_000), None);
        let kind = |a: &AudioStream, t, m, f| plan_audio(a, t, m, f);
        // AC-3: DVD ≤ 448, BD/AVCHD ≤ 640 at 48 kHz.
        assert!(matches!(kind(&ac3(448), Dvd, Auto, false), AudioOut::Copy { kind: AudioKind::Ac3, kbps: 448, ch: 6, bsf: None }));
        assert!(matches!(kind(&ac3(640), Dvd, Auto, false), AudioOut::Ac3 { kbps: 448, ch: 6 }));
        assert!(matches!(kind(&ac3(640), Bluray, Auto, false), AudioOut::Copy { .. }));
        assert!(matches!(kind(&ac3(640), Avchd, Auto, false), AudioOut::Copy { .. }));
        assert!(matches!(kind(&audio("ac3", 2, Some(192), Some(44_100), None), Bluray, Auto, false), AudioOut::Ac3 { kbps: 192, ch: 2 }));
        assert!(matches!(kind(&audio("ac3", 6, None, Some(48_000), None), Dvd, Auto, false), AudioOut::Ac3 { .. }), "unknown bitrate");
        // Mode "always AC-3" and filters rule copying out.
        assert!(matches!(kind(&ac3(384), Dvd, Ac3, false), AudioOut::Ac3 { .. }));
        assert!(matches!(kind(&ac3(384), Dvd, Auto, true), AudioOut::Ac3 { .. }));
        // DTS: core copied on DVD (HD → core only), as is on BD, re-encoded on AVCHD; E-AC-3 never copied.
        let dts = audio("dts", 6, Some(1509), Some(48_000), Some("DTS"));
        assert!(matches!(kind(&dts, Dvd, Auto, false), AudioOut::Copy { kind: AudioKind::Dts, bsf: None, kbps: 1509, .. }));
        let hd = audio("dts", 8, Some(4000), Some(48_000), Some("DTS-HD MA"));
        assert!(matches!(kind(&hd, Dvd, Auto, false), AudioOut::Copy { bsf: Some("dca_core"), kbps: 1536, ch: 6, .. }));
        assert!(matches!(kind(&hd, Bluray, Auto, false), AudioOut::Copy { bsf: Some("dca_core"), kbps: 1536, ch: 6, .. }));
        assert!(matches!(kind(&dts, Avchd, Auto, false), AudioOut::Ac3 { kbps: 448, ch: 6 }));
        assert!(matches!(kind(&audio("eac3", 6, Some(640), Some(48_000), None), Bluray, Auto, false), AudioOut::Ac3 { .. }));
    }

    #[test]
    fn trimmed_titles_never_copy_audio() {
        let mut info = bd_info();
        info.audio[0] = audio("ac3", 6, Some(384), Some(48_000), None);
        let job = BdEncodeJob { audio: vec![Some(&info.audio[0])], ..bd_job(&info) };
        assert!(matches!(job.audio_outs()[0], AudioOut::Copy { .. }));
        crate::probe::apply_trim(&mut info, Some(5.0), Some(30.0)).unwrap();
        let job = BdEncodeJob { audio: vec![Some(&info.audio[0])], ..bd_job(&info) };
        assert!(matches!(job.audio_outs()[0], AudioOut::Ac3 { kbps: 448, ch: 6 }), "{:?}", job.audio_outs());
    }

    #[test]
    fn lpcm_plans() {
        use AudioTarget::*;
        let a = audio("flac", 6, None, Some(96_000), None);
        let p = plan_audio(&a, Bluray, AudioMode::Lpcm, false);
        assert_eq!(p, AudioOut::Pcm { codec: "pcm_s24le", ch: 6, rate: 48_000 });
        assert_eq!(p.kbps(), 48 * 24 * 6);
        assert_eq!(plan_audio(&a, Avchd, AudioMode::Lpcm, false), AudioOut::Pcm { codec: "pcm_s16le", ch: 2, rate: 48_000 });
        // DVD: ffmpeg's muxer cannot write usable DVD LPCM, so LPCM mode means AC-3 there.
        assert!(matches!(plan_audio(&a, Dvd, AudioMode::Lpcm, false), AudioOut::Ac3 { .. }));
        assert_eq!(plan_audio(&a, Bluray, AudioMode::Lpcm, false).kind(), AudioKind::Lpcm);
    }

    #[test]
    fn bd_audio_copy_rules() {
        use AudioTarget::*;
        let pcm = |codec: &str, ch, rate, bits| AudioStream { bits_per_raw_sample: bits, ..audio(codec, ch, None, Some(rate), None) };
        let why = |a: &AudioStream, t| bd_audio_copy(a, t).unwrap_err();
        // DTS: 48/96/192 kHz and at most 8 channels; HD is copied as its core.
        assert!(why(&audio("dts", 6, Some(1509), Some(44_100), Some("DTS")), Bluray).contains("48/96/192"));
        assert!(why(&audio("dts", 9, Some(1509), Some(48_000), Some("DTS")), Bluray).contains("at most 8"));
        assert!(matches!(bd_audio_copy(&audio("dts", 6, Some(1509), Some(96_000), Some("DTS")), Bluray), Ok(AudioOut::Copy { bsf: None, kbps: 1509, .. })));
        let ma = audio("dts", 8, Some(4000), Some(48_000), Some("DTS-HD MA"));
        assert_eq!(bd_audio_copy(&ma, Bluray), Ok(AudioOut::Copy { kind: AudioKind::Dts, bsf: Some("dca_core"), kbps: 1536, ch: 6 }));
        assert!(why(&ma, Avchd).contains("AVCHD"));
        // PCM / FLAC: lossless repack keeping the rate.
        let p = bd_audio_copy(&pcm("pcm_s24le", 6, 96_000, None), Bluray).unwrap();
        assert_eq!(p, AudioOut::Pcm { codec: "pcm_s24le", ch: 6, rate: 96_000 });
        assert_eq!(p.kbps(), 96 * 24 * 6);
        assert_eq!(bd_audio_copy(&pcm("pcm_s16be", 2, 48_000, None), Bluray), Ok(AudioOut::Pcm { codec: "pcm_s16le", ch: 2, rate: 48_000 }));
        assert!(bd_audio_copy(&pcm("flac", 2, 44_100, Some(16)), Bluray).is_err(), "44.1 kHz is no BD LPCM rate");
        assert!(bd_audio_copy(&pcm("flac", 2, 48_000, Some(20)), Bluray).is_ok());
        assert_eq!(bd_audio_copy(&pcm("pcm_s24le", 5, 48_000, None), Bluray).unwrap().kbps(), 48 * 24 * 6, "odd channel counts are padded");
        assert!(why(&pcm("pcm_f32le", 2, 48_000, None), Bluray).contains("cannot be copied"));
        assert!(why(&pcm("pcm_s32le", 2, 48_000, Some(32)), Bluray).contains("24 bit"));
        assert!(why(&pcm("flac", 2, 48_000, Some(32)), Bluray).contains("24 bit"));
        assert!(why(&pcm("pcm_s24le", 6, 48_000, None), Avchd).contains("AVCHD"));
        assert!(bd_audio_copy(&pcm("pcm_s16le", 2, 48_000, None), Avchd).is_ok());
        assert!(why(&audio("eac3", 6, Some(640), Some(48_000), None), Bluray).contains("eac3 cannot be copied to Blu-ray"));
    }

    #[test]
    fn bd_audio_mode_policy() {
        use AudioTarget::*;
        assert_eq!(BdAudioMode::from_setting("strict"), BdAudioMode::Strict);
        assert_eq!(BdAudioMode::from_setting("encode"), BdAudioMode::Encode);
        assert_eq!(BdAudioMode::from_setting(""), BdAudioMode::Auto);
        let ac3 = audio("ac3", 6, Some(384), Some(48_000), None);
        let plan = |m| plan_audio_bd(&ac3, Bluray, AudioMode::Auto, m, false);
        assert!(matches!(plan(BdAudioMode::Auto), AudioOut::Copy { .. }));
        assert!(matches!(plan(BdAudioMode::Strict), AudioOut::Copy { .. }));
        assert!(matches!(plan(BdAudioMode::Encode), AudioOut::Ac3 { kbps: 448, ch: 6 }));
        // Encode with LPCM wanted: always LPCM; Auto keeps a compliant AC-3 track.
        assert_eq!(plan_audio_bd(&ac3, Bluray, AudioMode::Lpcm, BdAudioMode::Encode, false).kind(), AudioKind::Lpcm);
        assert_eq!(plan_audio_bd(&ac3, Bluray, AudioMode::Lpcm, BdAudioMode::Auto, false).kind(), AudioKind::Lpcm, "explicit LPCM re-encodes AC-3");
        assert_eq!(plan_audio_bd(&ac3, Bluray, AudioMode::Lpcm, BdAudioMode::Strict, false).kind(), AudioKind::Ac3);
        assert!(matches!(plan_audio_bd(&ac3, Bluray, AudioMode::Ac3, BdAudioMode::Auto, false), AudioOut::Copy { .. }));
        let dts = audio("dts", 6, Some(1509), Some(48_000), Some("DTS"));
        let pcm = AudioStream { bits_per_raw_sample: Some(24), ..audio("pcm_s24le", 6, None, Some(96_000), None) };
        assert!(matches!(plan_audio_bd(&dts, Bluray, AudioMode::Ac3, BdAudioMode::Auto, false), AudioOut::Ac3 { .. }));
        assert!(matches!(plan_audio_bd(&dts, Bluray, AudioMode::Auto, BdAudioMode::Auto, false), AudioOut::Copy { .. }));
        assert!(matches!(plan_audio_bd(&pcm, Bluray, AudioMode::Ac3, BdAudioMode::Auto, false), AudioOut::Ac3 { .. }));
        assert_eq!(plan_audio_bd(&pcm, Bluray, AudioMode::Lpcm, BdAudioMode::Auto, false), AudioOut::Pcm { codec: "pcm_s24le", ch: 6, rate: 96_000 });
        assert!(matches!(plan_audio_bd(&pcm, Bluray, AudioMode::Auto, BdAudioMode::Auto, false), AudioOut::Pcm { rate: 96_000, .. }));
    }

    #[test]
    fn audio_args_copy_and_pcm() {
        let copy = AudioOut::Copy { kind: AudioKind::Dts, bsf: Some("dca_core"), kbps: 1536, ch: 6 };
        let strings = |v: Vec<OsString>| v.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert_eq!(strings(audio_track_args(1, copy, Some("loudnorm"))), "-c:a:1 copy -bsf:a:1 dca_core", "no filter on copies");
        assert_eq!(
            strings(audio_track_args(0, AudioOut::Pcm { codec: "pcm_s24le", ch: 6, rate: 48_000 }, None)),
            "-c:a:0 pcm_s24le -ar:a:0 48000 -ac:a:0 6"
        );
        assert_eq!(
            strings(audio_track_args(2, AudioOut::Ac3 { kbps: 192, ch: 2 }, Some("aresample=48000"))),
            "-c:a:2 ac3 -ar:a:2 48000 -b:a:2 192k -ac:a:2 2 -filter:a:2 aresample=48000"
        );
        assert_eq!(AudioOut::Pcm { codec: "pcm_s16le", ch: 2, rate: 48_000 }.dvdauthor_format(), "pcm");
    }

    #[test]
    fn copied_audio_enters_the_dvd_job() {
        let mut info = bd_info();
        info.audio[0] = audio("ac3", 6, Some(384), Some(48_000), None);
        let job = EncodeJob {
            info: &info,
            audio: vec![&info.audio[0]],
            subtitles: vec![],
            video_kbps: 5000,
            aspect: Aspect::Wide,
            dir: PathBuf::from("w"),
            stem: "t".into(),
            two_pass: false,
            fast: false,
            tv: TvStandard::Pal,
            resolution: Resolution::FullD1,
            video: Default::default(),
            hwaccel: None,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![],
            audio_filters: vec![],
            copy_video: false,
            subtitle_style: Default::default(),
        };
        let args: Vec<String> = job.encode_args("o.mpg").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.join(" ").contains("-c:a:0 copy -sn"), "{args:?}");
        assert_eq!(job.audio_kbps_total(), 384);
    }

    #[test]
    fn selective_audio_modes_in_dvd_job() {
        let mut info = bd_info();
        info.audio = vec![
            audio("ac3", 6, Some(384), Some(48_000), None),
            audio("aac", 2, Some(192), Some(48_000), None),
        ];
        let job = EncodeJob {
            info: &info,
            audio: vec![&info.audio[0], &info.audio[1]],
            subtitles: vec![],
            video_kbps: 5000,
            aspect: Aspect::Wide,
            dir: PathBuf::from("w"),
            stem: "t".into(),
            two_pass: false,
            fast: false,
            tv: TvStandard::Pal,
            resolution: Resolution::FullD1,
            video: Default::default(),
            hwaccel: None,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![AudioMode::Auto, AudioMode::Ac3],
            audio_filters: vec![],
            copy_video: false,
            subtitle_style: Default::default(),
        };
        let outs = job.audio_outs();
        assert_eq!(outs.len(), 2);
        assert!(matches!(outs[0], AudioOut::Copy { .. }), "track 0 should remain stream copy");
        assert!(matches!(outs[1], AudioOut::Ac3 { .. }), "track 1 should be re-encoded to AC-3");
    }

    #[test]
    fn trim_and_crop_reach_every_ffmpeg_run() {
        let mut info = bd_info();
        crate::probe::apply_trim(&mut info, Some(10.0), Some(40.0)).unwrap();
        crate::probe::apply_crop(&mut info, crate::probe::Crop { top: 140, bottom: 140, left: 0, right: 0 }).unwrap();
        let strings = |v: Vec<OsString>| v.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");

        // Blu-ray: section as input options, crop before the scale, silence/-t for the section.
        let bd = strings(bd_job(&info).encode_args("out.mkv"));
        assert!(bd.starts_with("-y -ss 10.000 -to 40.000 -i in.mkv -f lavfi -t 30.000 -i anullsrc"), "{bd}");
        // 1920x800 is 2.4:1 -> pillar/letterboxed into 16:9: the crop comes first, the active size is 1920x800.
        assert!(bd.contains("-vf crop=1920:800:0:140,fps=24000/1001,scale=1920:800:"), "{bd}");
        assert!(bd.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black"), "{bd}");
        assert!(strings(bd_job(&info).first_pass_args()).starts_with("-y -ss 10.000 -to 40.000 -i in.mkv -map 0:0"));

        // DVD: same input options; the aspect is that of the cropped picture (2.4:1 -> 16:9 letterbox).
        let job = EncodeJob {
            info: &info,
            audio: vec![],
            subtitles: info.subtitles.iter().collect(),
            video_kbps: 5000,
            aspect: Aspect::Wide,
            dir: PathBuf::from("w"),
            stem: "t".into(),
            two_pass: false,
            fast: false,
            tv: TvStandard::Pal,
            resolution: Resolution::FullD1,
            video: Default::default(),
            hwaccel: None,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![],
            audio_filters: vec![],
            copy_video: false,
            subtitle_style: Default::default(),
        };
        let dvd = strings(job.encode_args("o.mpg"));
        assert!(dvd.starts_with("-y -ss 10.000 -to 40.000 -i in.mkv -map 0:0 -vf crop=1920:800:0:140,fps=25,scale=720:"), "{dvd}");
        let (_, h) = active_size(info.video.as_ref().unwrap().display_aspect, Aspect::Wide, (720, 576));
        assert_eq!(h, 426, "2.4:1 in a 16:9 frame leaves 426 of 576 lines");

        // Subtitle extraction reads the same section.
        let mut with_sub = info.clone();
        with_sub.subtitles =
            vec![SubtitleStream { index: 3, codec: "subrip".into(), kind: SubtitleKind::Text, language: None, title: None, forced: false, width: None, height: None }];
        let sub = strings(extract_subtitle_args(&with_sub, &[(0, &with_sub.subtitles[0])], Path::new("w"), "t").unwrap());
        assert!(sub.starts_with("-y -ss 10.000 -to 40.000 -i in.mkv -map 0:3"), "{sub}");
    }

    #[test]
    fn loudnorm_report_and_filter() {
        let log = "[Parsed_loudnorm_0 @ 0000] \n{\n\t\"input_i\" : \"-27.61\",\n\t\"input_tp\" : \"-4.47\",\n\t\"input_lra\" : \"18.06\",\n\t\"input_thresh\" : \"-39.20\",\n\t\"output_i\" : \"-16.58\",\n\t\"target_offset\" : \"0.58\"\n}\n";
        let l = parse_loudnorm_report(log).unwrap();
        assert_eq!(
            l.filter(),
            "loudnorm=I=-23:TP=-1:LRA=20:measured_I=-27.61:measured_TP=-4.47:measured_LRA=18.06:measured_thresh=-39.20:offset=0.58:linear=true,aresample=48000"
        );
        // Silence measures as -inf: no filter. Garbage and missing blocks too.
        assert!(parse_loudnorm_report(&log.replace("-27.61", "-inf")).is_none());
        assert!(parse_loudnorm_report("nothing here").is_none());
        let info = bd_info();
        let args: Vec<String> = loudnorm_measure_args(&info, 1).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..2], ["-i", "in.mkv"]);
        assert!(args.join(" ").contains("-map 0:1 -vn -sn -af loudnorm=I=-23:TP=-1:LRA=20:print_format=json -f null"), "{args:?}");
    }

    #[test]
    fn filtered_tracks_are_re_encoded_with_their_filter() {
        let mut info = bd_info();
        info.audio[0] = audio("ac3", 6, Some(384), Some(48_000), None);
        let job = BdEncodeJob {
            audio: vec![Some(&info.audio[0]), None],
            audio_filters: vec![Some("loudnorm=I=-23,aresample=48000".into())],
            ..bd_job(&info)
        };
        let a = job.encode_args("o.mkv").iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(a.contains("-c:a:0 ac3 -ar:a:0 48000 -b:a:0 448k -ac:a:0 6 -filter:a:0 loudnorm=I=-23,aresample=48000"), "{a}");
        assert!(!a.contains("copy"), "{a}");
    }

    #[test]
    fn cropdetect_log_and_union() {
        let log = "[Parsed_cropdetect_0 @ 0x1] x1:0 x2:1919 y1:138 y2:941 w:1920 h:800 x:0 y:140 pts:1 t:0.04 limit:0.094118 crop=1920:800:0:140\n\
                   [Parsed_cropdetect_0 @ 0x1] x1:0 x2:1919 y1:130 y2:949 w:1920 h:816 x:0 y:132 pts:2 t:0.08 limit:0.094118 crop=1920:816:0:132\n";
        assert_eq!(parse_cropdetect(log), Some((1920, 816, 0, 132)), "the last (largest) report counts");
        assert_eq!(parse_cropdetect("no crop here"), None);
        // Union of two scenes: the larger picture wins, the frame size gives the bars.
        let c = union_crop(&[(1920, 800, 0, 140), (1920, 816, 0, 132), (1900, 1080, 10, 0)], (1920, 1080));
        assert_eq!(c, Crop { top: 0, bottom: 0, left: 0, right: 0 });
        let c = union_crop(&[(1920, 800, 0, 140), (1920, 816, 0, 132)], (1920, 1080));
        assert_eq!(c, Crop { top: 132, bottom: 132, left: 0, right: 0 });
        assert_eq!(union_crop(&[], (1920, 1080)), Crop::default());
    }
    #[test]
    fn bd_menu_with_intro_overlays_piped_frames_and_crossfades_the_loop_audio() {
        let clip = ClipSource { path: "music.mp3".into(), start_secs: 0.0, has_audio: true };
        let media = CustomMenuMedia {
            background: MenuBackground::Color([0, 0, 0]),
            overlay: BackgroundOverlay::None,
            layers: vec![MenuLayer::Image("l0.png".into())],
        };
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let frame = MenuFrame::square(1920, 1080);
        let t = 72.0 * 1001.0 / 24000.0;
        let a = joined(&bd_custom_menu_args(&frame, rate, &media, t + 20.0, Some(&clip), Some(t), Path::new("m.mkv")));
        // The intro is the last input, read from stdin as raw RGBA, and covers the page until it ends.
        assert!(a.contains("-f rawvideo -pix_fmt rgba -s 1920x1080 -r 24000/1001 -i pipe:0"), "{a}");
        assert!(a.ends_with("m.mkv") && a.rfind("pipe:0") > a.rfind("music.mp3"), "{a}");
        assert!(a.contains("[3:v]setsar=1[intro];[m0][intro]overlay=0:0:format=rgb:eof_action=pass[mi];[mi]format=yuv420p"), "{a}");
        // A keyframe where the loop starts (rounded down to the microsecond).
        assert!(a.contains("-force_key_frames 3.003000"), "{a}");
        // The exact crossfade of the plan: clip tail -> sound at [T-X, T], no fade-out.
        assert!(a.contains("asplit=2[am][as];[as]atrim=start=2.503000:end=3.003000,asetpts=N/SR/TB[tail];"), "{a}");
        assert!(a.contains("[am]atrim=duration=23.003,asetpts=N/SR/TB,afade=t=in:d=0.5[main];[main][tail]acrossfade=d=0.500000:c1=tri:c2=tri[aout]"), "{a}");
        assert!(!a.contains("afade=t=out"), "{a}");
        // Without intro the old chain and no extra input.
        let a = joined(&bd_custom_menu_args(&frame, rate, &media, 20.0, Some(&clip), None, Path::new("m.mkv")));
        assert!(!a.contains("pipe:0") && !a.contains("force_key_frames") && a.contains("afade=t=out:st=19.000:d=1[aout]"), "{a}");
    }

    #[test]
    fn intro_slot_stream_makes_the_same_pictures_as_the_page_graph() {
        let rate = BdRate { fps: (25, 1), fake_interlaced: true };
        let frame = MenuFrame::square(1920, 1080);
        let layer = MenuLayer::Video {
            clip: ClipSource { path: "movie.mkv".into(), start_secs: 12.5, has_audio: true },
            x: 10,
            y: 20,
            width: 160,
            height: 90,
            rotation: 0,
            fit: Fit::Contain,
            opacity: 0.5,
        };
        let s = intro_slot_stream(&frame, rate, &layer, 75).unwrap();
        assert_eq!(s.frame_bytes, 160 * 90 * 4);
        let a = joined(&s.args);
        assert!(a.starts_with("-stream_loop -1 -ss 12.500 -i movie.mkv -vf fps=25,"), "{a}");
        assert!(a.contains(&video_layer_filters(&frame, "25", 160, 90, 0, Fit::Contain)) && a.ends_with("-frames:v 75 -f rawvideo -pix_fmt rgba pipe:1"), "{a}");
        assert!(intro_slot_stream(&frame, rate, &MenuLayer::Image("x.png".into()), 75).is_none());
    }

    #[test]
    fn intro_background_stream_uses_the_page_background_chain() {
        let rate = BdRate { fps: (25, 1), fake_interlaced: true };
        let frame = MenuFrame::square(1920, 1080);
        let media = CustomMenuMedia {
            background: MenuBackground::Image { path: "bg.jpg".into(), blur: 8.0 },
            overlay: BackgroundOverlay::Dim,
            layers: vec![MenuLayer::Image("l0.png".into())],
        };
        let s = intro_background_stream(&frame, rate, &media, 75);
        assert_eq!(s.frame_bytes, 1920 * 1080 * 4);
        let a = joined(&s.args);
        // The very filters of the page graph, then raw frames of the first 75 pictures.
        let page = joined(&bd_custom_menu_args(&frame, rate, &media, 10.0, None, None, Path::new("m.mkv")));
        let chain = "[0:v]fps=25,scale=trunc(ih*dar*1.000000/2)*2:ih,setsar=1,scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,gblur=sigma=8.0,format=rgba,colorchannelmixer=rr=0.6:gg=0.6:bb=0.6,setsar=1[bg]";
        assert!(a.contains(chain) && page.contains(chain), "{a}");
        assert!(a.ends_with("-map [bg] -frames:v 75 -f rawvideo -pix_fmt rgba pipe:1"), "{a}");
        // A clip background loops from its own start; the shading is part of the chain.
        let clip = CustomMenuMedia {
            background: MenuBackground::Video { clip: ClipSource { path: "m.mkv".into(), start_secs: 4.0, has_audio: true }, blur: 0.0 },
            overlay: BackgroundOverlay::BottomDark,
            layers: vec![],
        };
        let a = joined(&intro_background_stream(&frame, rate, &clip, 75).args);
        assert!(a.starts_with("-stream_loop -1 -ss 4.000 -t ") && a.contains("[bg0][shade]overlay=0:0:format=rgb[bg]"), "{a}");
    }

    #[test]
    fn video_processing_and_stream_selection_serde_defaults() {
        let vp: VideoProcessing = serde_json::from_str("{}").unwrap();
        assert_eq!(vp, VideoProcessing::default());
        assert_eq!(vp.scale, ScaleMode::Letterbox);
        assert_eq!(vp.filter, ScaleFilter::Bicubic);
        assert_eq!(vp.deinterlace, Deinterlace::Auto);
        assert_eq!((vp.denoise, vp.brightness, vp.contrast, vp.saturation), (None, None, None, None));
        assert!(!vp.has_picture_filters());

        let parsed: VideoProcessing = serde_json::from_str(
            r#"{"scale":"panscan","filter":"lanczos","deinterlace":"off","denoise":4.5,"brightness":-0.1,"contrast":1.2,"saturation":1.5}"#,
        )
        .unwrap();
        assert_eq!(parsed.scale, ScaleMode::PanScan);
        assert_eq!(parsed.filter, ScaleFilter::Lanczos);
        assert_eq!(parsed.deinterlace, Deinterlace::Off);
        assert_eq!(parsed.denoise, Some(4.5));
        assert_eq!(parsed.brightness, Some(-0.1));
        assert_eq!(parsed.contrast, Some(1.2));
        assert_eq!(parsed.saturation, Some(1.5));

        let sel: crate::dvd::StreamSelection = serde_json::from_str("{}").unwrap();
        assert_eq!(sel, crate::dvd::StreamSelection::default());
        assert_eq!((sel.denoise, sel.brightness, sel.contrast, sel.saturation), (None, None, None, None));
    }

    #[test]
    fn picture_filters_in_encode_and_preview_chains() {
        let base = VideoProcessing { denoise: Some(4.0), brightness: Some(0.1), ..Default::default() };
        let overlaid = base.with_picture(None, Some(-0.2), Some(1.25), Some(0.8));
        assert_eq!(overlaid.denoise, Some(4.0));
        assert_eq!(overlaid.brightness, Some(-0.2));
        assert_eq!(overlaid.contrast, Some(1.25));
        assert_eq!(overlaid.saturation, Some(0.8));
        assert!(overlaid.has_picture_filters());

        let expected_pic = "hqdn3d=4.00:3.00:6.00:4.50,eq=brightness=-0.20:contrast=1.25:saturation=0.80";

        let mut sdr = video(1920, 1080, 16.0 / 9.0, "tt");
        sdr.crop = Some(crate::probe::CropRect { x: 0, y: 100, w: 1920, h: 800 });
        sdr.display_aspect = 2.4;

        // DVD downscale: picture filters run on the scaled picture, before the letterbox bars.
        let dvd_vf = video_filter(&sdr, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, overlaid);
        assert!(dvd_vf.starts_with("bwdif=mode=send_frame,crop=1920:800:0:100,fps=25,scale="), "{dvd_vf}");
        assert!(dvd_vf.contains(&format!(":out_range=tv,{expected_pic},pad=")), "{dvd_vf}");

        // Same size on Blu-ray (1920 wide letterboxed): no downscale, filters stay in front.
        let bd_vf = bd_video_filter(&sdr, BdRate { fps: (25, 1), fake_interlaced: false }, Resolution::Hd1080, overlaid);
        assert!(bd_vf.starts_with(&format!("bwdif=mode=send_frame,crop=1920:800:0:100,{expected_pic},fps=25,")), "{bd_vf}");

        // Upscale 720p -> 1080p: filters run before the scale, on the smaller source.
        let small = video(1280, 720, 16.0 / 9.0, "progressive");
        let up_vf = bd_video_filter(&small, BdRate { fps: (25, 1), fake_interlaced: false }, Resolution::Hd1080, overlaid);
        assert!(up_vf.starts_with(&format!("{expected_pic},fps=25,scale=1920:1080")), "{up_vf}");

        let sdr_prev = preview_filter(&sdr, overlaid, 480);
        assert_eq!(
            sdr_prev,
            format!("bwdif=mode=send_frame,crop=1920:800:0:100,scale=1152:480:flags=bicubic,{expected_pic},setsar=1,format=yuv420p")
        );

        let mut hdr = video(3840, 2160, 16.0 / 9.0, "progressive");
        hdr.hdr = true;
        let tm = tonemap_filter(&hdr).unwrap();
        let hdr_prev = preview_filter(&hdr, overlaid, 480);
        assert_eq!(
            hdr_prev,
            format!("scale=854:480:flags=bicubic,{tm},{expected_pic},setsar=1,format=yuv420p")
        );

        // Neutral and non-finite values emit no picture filters.
        let neutral = VideoProcessing {
            denoise: Some(0.0),
            brightness: Some(0.0),
            contrast: Some(1.0),
            saturation: Some(1.0),
            ..Default::default()
        };
        assert!(!neutral.has_picture_filters());
        let non_finite = VideoProcessing {
            denoise: Some(f32::NAN),
            brightness: Some(f32::INFINITY),
            contrast: Some(f32::NEG_INFINITY),
            saturation: Some(f32::NAN),
            ..Default::default()
        };
        assert!(!non_finite.has_picture_filters());
        assert_eq!(
            preview_filter(&sdr, neutral, 480),
            preview_filter(&sdr, VideoProcessing::default(), 480)
        );
        assert_eq!(
            video_filter(&sdr, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, non_finite),
            video_filter(&sdr, Aspect::Wide, TvStandard::Pal, Resolution::FullD1, VideoProcessing::default())
        );

        // Out-of-range values are clamped.
        let clamped = VideoProcessing {
            denoise: Some(25.0),
            brightness: Some(-2.5),
            contrast: Some(5.0),
            saturation: Some(5.0),
            ..Default::default()
        };
        let clamped_vf = preview_filter(&video(1280, 720, 16.0 / 9.0, "progressive"), clamped, 480);
        assert!(
            clamped_vf.contains(",hqdn3d=12.00:9.00:18.00:13.50,eq=brightness=-1.00:contrast=2.00:saturation=3.00,"),
            "{clamped_vf}"
        );
    }

    #[test]
    fn bd_stream_copy_rejects_picture_filters() {
        let mut info = bd_info();
        let v = info.video.as_mut().unwrap();
        (v.profile, v.level, v.pix_fmt, v.bit_rate) = (Some("High".into()), Some(41), "yuv420p".into(), Some(15_000_000));

        let neutral = VideoProcessing {
            denoise: Some(0.0),
            brightness: Some(0.0),
            contrast: Some(1.0),
            saturation: Some(1.0),
            ..Default::default()
        };
        assert_eq!(bd_stream_copy(&info, neutral, 30_000), Ok(15_000));

        for active in [
            VideoProcessing { denoise: Some(2.0), ..Default::default() },
            VideoProcessing { brightness: Some(0.1), ..Default::default() },
            VideoProcessing { contrast: Some(1.1), ..Default::default() },
            VideoProcessing { saturation: Some(0.9), ..Default::default() },
        ] {
            assert_eq!(bd_stream_copy(&info, active, 30_000), Err("picture filters active".into()));
        }
    }

    #[test]
    fn psnr_helpers_parse_verdict_and_args() {
        let log = "[Parsed_psnr_2 @ 0x1] PSNR y:25.10 u:29.00 v:28.50 average:35.123456 min:30.00 max:40.00\n\
                   [Parsed_psnr_2 @ 0x2] PSNR y:26.33 u:30.14 v:29.72 average:38.456789 min:31.20 max:45.10\n";
        assert_eq!(parse_psnr_average(log), Some(38.456789));
        assert_eq!(parse_psnr_average("[Parsed_psnr_2 @ 0x1] PSNR y:inf u:inf v:inf average:inf min:inf max:inf"), Some(99.9));
        assert_eq!(parse_psnr_average("PSNR y:inf average:INF"), Some(99.9));
        assert_eq!(parse_psnr_average("average:38.5 without marker"), None);
        assert_eq!(parse_psnr_average("PSNR average:NaN"), None);
        assert_eq!(parse_psnr_average("nothing relevant"), None);

        assert_eq!(psnr_verdict(40.0), "excellent");
        assert_eq!(psnr_verdict(39.99), "good");
        assert_eq!(psnr_verdict(36.0), "good");
        assert_eq!(psnr_verdict(35.99), "fair");
        assert_eq!(psnr_verdict(32.0), "fair");
        assert_eq!(psnr_verdict(31.99), "poor");

        let args = joined(&quality_psnr_args(
            Path::new("sample.mpg"),
            Path::new("source.mkv"),
            2,
            12.5,
            10.0,
            "fps=25,scale=720:576",
        ));
        assert_eq!(
            args,
            format!(
                "-hide_banner -nostdin -i sample.mpg -ss 12.500 -t 10.000 -i source.mkv \
                 -lavfi [0:v]setpts=PTS-STARTPTS,format=yuv420p[dist];[1:2]fps=25,scale=720:576,setpts=PTS-STARTPTS[ref];[dist][ref]psnr=shortest=1 \
                 -f null {NULL_DEVICE}"
            )
        );
    }

    #[test]
    fn dvd_video_copy_rules() {
        assert_eq!(DvdVideoMode::default(), DvdVideoMode::Encode);
        assert_eq!(DvdVideoMode::from_setting("encode"), DvdVideoMode::Encode);
        assert_eq!(DvdVideoMode::from_setting(" AUTO "), DvdVideoMode::Auto);
        assert_eq!(DvdVideoMode::from_setting("Strict"), DvdVideoMode::Strict);
        assert_eq!(DvdVideoMode::from_setting("unknown"), DvdVideoMode::Encode);

        let pal_info = || MediaInfo {
            path: PathBuf::from("nonexistent_dvd_copy_test.mpg"),
            duration_secs: 10.0,
            video: Some(VideoStream {
                index: 0,
                codec: "mpeg2video".into(),
                width: 720,
                height: 576,
                display_aspect: 16.0 / 9.0,
                fps: 25.0,
                field_order: "progressive".into(),
                crop: None,
                hdr: false,
                profile: Some("Main".into()),
                level: Some(8),
                pix_fmt: "yuv420p".into(),
                bit_rate: Some(6_000_000),
            }),
            audio: vec![audio("ac3", 2, Some(192), Some(48_000), None)],
            subtitles: vec![],
            chapters: vec![],
            trim: None,
            audio_names: Default::default(),
        };

        // PAL 720x576 & 704x576 @ 25 fps; fallback bytes = 6_000_000 / 8 * 10 = 7_500_000.
        let base = pal_info();
        assert_eq!(
            dvd_video_copy(&base, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 9500),
            Ok(DvdVideoCopy { kbps: 6000, bytes: 7_500_000 })
        );
        let mut pal704 = pal_info();
        pal704.video.as_mut().unwrap().width = 704;
        assert!(is_dvd_native_format(pal704.video.as_ref().unwrap(), TvStandard::Pal));
        assert!(dvd_video_copy(&pal704, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 9500).is_ok());

        // NTSC 720x480 / 704x480 @ 29.97 and 23.976 fps.
        for (w, fps) in [(720, 30000.0 / 1001.0), (704, 24000.0 / 1001.0), (720, 29.97), (720, 23.976)] {
            let mut ntsc = pal_info();
            {
                let v = ntsc.video.as_mut().unwrap();
                (v.width, v.height, v.fps) = (w, 480, fps);
                assert!(is_dvd_native_format(v, TvStandard::Ntsc), "w={w} fps={fps}");
                assert!(!is_dvd_native_format(v, TvStandard::Pal));
            }
            assert!(dvd_video_copy(&ntsc, TvStandard::Ntsc, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 9500).is_ok());
        }

        // Rejections in order.
        let mut no_vid = pal_info();
        no_vid.video = None;
        assert_eq!(dvd_video_copy(&no_vid, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 9500), Err("no video stream".into()));

        let bad = |f: &dyn Fn(&mut MediaInfo)| {
            let mut i = pal_info();
            f(&mut i);
            dvd_video_copy(&i, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 9500).unwrap_err()
        };
        assert!(bad(&|i| i.video.as_mut().unwrap().codec = "h264".into()).contains("needs MPEG-2"));
        assert!(bad(&|i| i.video.as_mut().unwrap().pix_fmt = "yuv422p".into()).contains("needs yuv420p"));
        assert!(bad(&|i| i.video.as_mut().unwrap().hdr = true).contains("HDR"));
        assert!(bad(&|i| i.video.as_mut().unwrap().fps = 29.97).contains("not DVD-compliant for PAL"));
        assert!(bad(&|i| i.video.as_mut().unwrap().width = 1280).contains("not DVD-compliant for PAL"));
        assert!(dvd_video_copy(&base, TvStandard::Pal, Resolution::HalfD1, Aspect::Wide, VideoProcessing::default(), 9500).unwrap_err().contains("Half D1"));
        assert!(dvd_video_copy(&base, TvStandard::Pal, Resolution::FullD1, Aspect::Standard, VideoProcessing::default(), 9500).unwrap_err().contains("title requests 4:3"));
        assert_eq!(bad(&|i| i.trim = Some(crate::probe::Trim { start: 1.0, end: 5.0 })), "trim active");
        assert_eq!(bad(&|i| i.video.as_mut().unwrap().crop = Some(crate::probe::CropRect { x: 0, y: 0, w: 700, h: 576 })), "crop active");
        assert_eq!(
            dvd_video_copy(&base, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing { deinterlace: Deinterlace::Yadif, ..Default::default() }, 9500).unwrap_err(),
            "forced deinterlacing"
        );
        assert_eq!(
            dvd_video_copy(&base, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing { scale: ScaleMode::PanScan, ..Default::default() }, 9500).unwrap_err(),
            "scale override active"
        );
        assert_eq!(
            dvd_video_copy(&base, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing { denoise: Some(2.0), ..Default::default() }, 9500).unwrap_err(),
            "picture filters active"
        );
        assert_eq!(bad(&|i| i.video.as_mut().unwrap().bit_rate = None), "unknown video bitrate");
        assert!(dvd_video_copy(&base, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 5000).unwrap_err().contains("DVD limit 5000"));
        assert!(bad(&|i| i.video.as_mut().unwrap().bit_rate = Some(9_900_000)).contains("DVD limit 9500"));

        // MPEG-2 VBR sentinel (>= 0x3FFFF * 400 = 104,857,200) falls back to file size minus audio.
        let tmp = std::env::temp_dir().join(format!("cx_mpeg2_vbr_{}.mpg", std::process::id()));
        // 10 s @ 4192 kbps total -> 5_240_000 bytes; minus 192 kbps audio = 4000 kbps video.
        std::fs::write(&tmp, vec![0u8; 5_240_000]).unwrap();
        let mut vbr = pal_info();
        vbr.path = tmp.clone();
        vbr.video.as_mut().unwrap().bit_rate = Some(0x3FFFF * 400);
        assert_eq!(source_video_kbps(&vbr), Some(4000));
        assert_eq!(
            dvd_video_copy(&vbr, TvStandard::Pal, Resolution::FullD1, Aspect::Wide, VideoProcessing::default(), 9500),
            Ok(DvdVideoCopy { kbps: 4000, bytes: 5_240_000 })
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn dvd_copy_job_skips_filter_hwaccel_and_two_pass() {
        let mut info = bd_info();
        let v = info.video.as_mut().unwrap();
        (v.codec, v.width, v.height, v.fps, v.pix_fmt) = ("mpeg2video".into(), 720, 576, 25.0, "yuv420p".into());
        let job = EncodeJob {
            info: &info,
            audio: vec![&info.audio[0]],
            subtitles: vec![],
            video_kbps: 4500,
            aspect: Aspect::Wide,
            dir: PathBuf::from("w"),
            stem: "title_01".into(),
            two_pass: true,
            fast: false,
            tv: TvStandard::Pal,
            resolution: Resolution::FullD1,
            video: VideoProcessing::default(),
            hwaccel: Some("cuda"),
            audio_mode: AudioMode::Auto,
            audio_modes: vec![],
            audio_filters: vec![],
            copy_video: true,
            subtitle_style: Default::default(),
        };
        assert!(!job.uses_two_pass());

        let args = strings(job.encode_args("out.mpg"));
        assert!(args.starts_with("-y -fflags +genpts -i in.mkv -map 0:0 -c:v copy"), "{args}");
        assert!(!args.contains("-vf"), "{args}");
        assert!(!args.contains("-hwaccel"), "{args}");
        assert!(!args.contains("-pass"), "{args}");
        assert!(!args.contains("-passlogfile"), "{args}");
        assert!(!args.contains("-c:v mpeg2video"), "{args}");
        assert!(args.contains("-map 0:1 -c:a:0"), "{args}");
        assert!(args.ends_with("-sn -dn -f dvd -muxrate 10080k -packetsize 2048 out.mpg"), "{args}");

        let piped = strings(job.encode_args("pipe:1"));
        assert!(piped.ends_with("-sn -dn -f dvd -muxrate 10080k -packetsize 2048 pipe:1"), "{piped}");
    }

    #[test]
    fn convert_subtitle_args_and_trim_helpers() {
        let args = strings(convert_subtitle_to_srt_args(Path::new("sub.ass"), Path::new("out.srt")));
        assert_eq!(args, "-y -i sub.ass -c:s srt -f srt out.srt");

        let cues = vec![
            textsub::Cue { start: 1.0, end: 3.0, lines: vec!["Before".into()] },
            textsub::Cue { start: 4.0, end: 8.0, lines: vec!["Overlap".into()] },
            textsub::Cue { start: 12.0, end: 15.0, lines: vec!["After".into()] },
        ];
        let trimmed = trim_cues(cues, 5.0, 10.0);
        assert_eq!(trimmed.len(), 1);
        assert_eq!((trimmed[0].start, trimmed[0].end), (0.0, 3.0));
    }

    #[test]
    fn external_subtitle_xml_renders_srt_and_sup() {
        let dir = std::env::temp_dir().join(format!("cx_ext_sub_enc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let srt = dir.join("test.en.srt");
        std::fs::write(&srt, "\u{FEFF}1\n00:00:01,000 --> 00:00:03,000\nHello world\n").unwrap();

        let sup = dir.join("test.de.sup");
        let sup_bytes = crate::pgs::build_sup(
            &[textsub::Cue { start: 1.0, end: 2.5, lines: vec!["Hallo".into()] }],
            &textsub::Canvas::hd(1920, 1080),
            (25, 1),
        );
        std::fs::write(&sup, sup_bytes).unwrap();

        let info = MediaInfo {
            path: dir.join("movie.mkv"),
            duration_secs: 20.0,
            video: Some(VideoStream {
                index: 0,
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                display_aspect: 16.0 / 9.0,
                fps: 25.0,
                field_order: "progressive".into(),
                crop: None,
                hdr: false,
                profile: None,
                level: None,
                pix_fmt: "yuv420p".into(),
                bit_rate: None,
            }),
            audio: vec![],
            subtitles: vec![],
            chapters: vec![],
            trim: None,
            audio_names: Default::default(),
        };
        let job = EncodeJob {
            info: &info,
            audio: vec![],
            subtitles: vec![],
            video_kbps: 5000,
            aspect: Aspect::Wide,
            dir: dir.clone(),
            stem: "title_01".into(),
            two_pass: false,
            fast: true,
            tv: TvStandard::Pal,
            resolution: Resolution::FullD1,
            video: VideoProcessing::default(),
            hwaccel: None,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![],
            audio_filters: vec![],
            copy_video: false,
            subtitle_style: textsub::SubtitleStyle { offset_ms: 500, ..Default::default() },
        };
        let tools = Toolchain {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            notes: vec![],
            priority: crate::tools::ProcessPriority::Normal,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ext_srt = ExternalSubtitle { path: srt, language: Some("eng".into()), title: None, forced: true };
        let xml_srt = rt.block_on(external_subtitle_xml(&tools, &job, 0, &ext_srt)).unwrap();
        assert!(xml_srt.contains("force=\"yes\""), "{xml_srt}");
        assert!(xml_srt.contains("00:00:01.50"), "offset_ms should shift 1.0s to 1.5s: {xml_srt}");

        let ext_sup = ExternalSubtitle { path: sup, language: Some("ger".into()), title: None, forced: false };
        let xml_sup = rt.block_on(external_subtitle_xml(&tools, &job, 1, &ext_sup)).unwrap();
        assert!(xml_sup.contains("title_01_s2"), "{xml_sup}");

        let _ = std::fs::remove_dir_all(&dir);
    }

}
