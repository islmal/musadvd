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
use crate::probe::{AudioStream, Crop, MediaInfo, SubtitleKind, SubtitleStream, VideoStream};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deinterlace {
    /// `bwdif` only for sources flagged interlaced.
    #[default]
    Auto,
    /// `yadif` for every source.
    Yadif,
    Off,
}

/// Video processing options shared by the DVD and Blu-ray encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoProcessing {
    pub scale: ScaleMode,
    pub filter: ScaleFilter,
    pub deinterlace: Deinterlace,
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

/// Head shared by encode and preview: deinterlacer, tonemap (HDR only), user crop.
fn source_chain(v: &VideoStream, vp: VideoProcessing) -> Vec<String> {
    let mut f: Vec<String> = deint_filter(v, vp).into_iter().collect();
    f.extend(tonemap_filter(v).map(String::from));
    f.extend(crop_filter(v));
    f
}

/// Filter chain of the live player: 8-bit 480p square-pixel picture of the cut/cropped source.
/// Tonemapping runs after the downscale (real-time at 4K); the encode tonemaps first, the small
/// difference is acceptable for a preview.
pub fn preview_filter(v: &VideoStream, vp: VideoProcessing, height: u32) -> String {
    let mut f: Vec<String> = deint_filter(v, vp).into_iter().collect();
    f.extend(crop_filter(v));
    let w = (v.display_aspect * height as f64 / 2.0).round() as u32 * 2;
    f.push(format!("scale={w}:{height}:flags={}", vp.filter.flag()));
    f.extend(tonemap_filter(v).map(String::from));
    f.push("setsar=1".to_string());
    f.push("format=yuv420p".to_string());
    f.join(",")
}

/// Shared chain: [`source_chain`], `fps`, scale, bars/crop to the coded `frame`.
/// Returns the filters before the aspect tag (`setdar`/`setsar`) and `format`.
fn fit_chain(v: &VideoStream, aspect: Aspect, frame: (u32, u32), fps: &str, matrix: &str, vp: VideoProcessing) -> Vec<String> {
    let (fw, fh) = frame;
    let mut f = source_chain(v, vp);
    f.push(format!("fps={fps}"));
    let flags = format!("flags={}:out_color_matrix={matrix}:out_range=tv", vp.filter.flag());
    match vp.scale {
        ScaleMode::Letterbox => {
            let (w, h) = active_size(v.display_aspect, aspect, frame);
            f.push(format!("scale={w}:{h}:{flags}"));
            if (w, h) != (fw, fh) {
                f.push(format!("pad={fw}:{fh}:(ow-iw)/2:(oh-ih)/2:black"));
            }
        }
        ScaleMode::PanScan => {
            let (w, h) = cover_size(v.display_aspect, aspect, frame);
            f.push(format!("scale={w}:{h}:{flags}"));
            if (w, h) != (fw, fh) {
                f.push(format!("crop={fw}:{fh}"));
            }
        }
        ScaleMode::Stretch => f.push(format!("scale={fw}:{fh}:{flags}")),
    }
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
    /// Uncompressed PCM written with ffmpeg codec `codec`.
    Pcm { codec: &'static str, ch: u32 },
}

impl AudioOut {
    pub fn kbps(self) -> u32 {
        match self {
            AudioOut::Copy { kbps, .. } | AudioOut::Ac3 { kbps, .. } => kbps,
            AudioOut::Pcm { codec, ch } => 48 * if codec == "pcm_s24le" { 24 } else { 16 } * ch,
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
            _ => "Mehrkanal",
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

/// Stream copy of `a` if it complies with `target` (`None` = must be re-encoded).
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
        // shortcut: DTS-HD MA bitrate is often reported as the core's; estimate 5 Mbit/s when absent.
        ("dts", AudioTarget::Bluray) => Some(AudioOut::Copy {
            kind: AudioKind::Dts,
            bsf: None,
            kbps: kbps.unwrap_or(if profile.contains("MA") { 5000 } else { 1536 }),
            ch: a.channels,
        }),
        _ => None,
    }
}

/// Output plan of one source audio track. `filtered`: a filter (loudnorm) has to run on it or the
/// title is trimmed, which rules out a stream copy (ffmpeg's input seeking does not cut copied
/// audio at the start, it would drift against the re-encoded video).
pub fn plan_audio(a: &AudioStream, target: AudioTarget, mode: AudioMode, filtered: bool) -> AudioOut {
    match (mode, target) {
        (AudioMode::Lpcm, AudioTarget::Bluray) => return AudioOut::Pcm { codec: "pcm_s24le", ch: a.channels.clamp(1, 8) },
        (AudioMode::Lpcm, AudioTarget::Avchd) => return AudioOut::Pcm { codec: "pcm_s16le", ch: 2 },
        _ => {}
    }
    if mode == AudioMode::Auto && !filtered {
        if let Some(copy) = copy_plan(a, target) {
            return copy;
        }
    }
    let (kbps, ch) = ac3_settings(a.channels);
    AudioOut::Ac3 { kbps, ch }
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
        AudioOut::Pcm { codec, ch } => {
            push(format!("-c:a:{n}"), codec.into());
            push(format!("-ar:a:{n}"), "48000".into());
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

    /// MPEG-2 video settings shared by all runs (codec, GOP, rate control, aspect).
    fn video_codec_args(&self) -> Vec<OsString> {
        let s = |x: &str| OsString::from(x);
        let maxrate = max_video_rate(self.audio_kbps_total()).max(self.video_kbps);
        // BT.601 colour tags: 625-line (bt470bg) for PAL, 525-line (smpte170m) for NTSC.
        let (primaries, trc, matrix) = match self.tv {
            TvStandard::Pal => ("bt470bg", "gamma28", "bt470bg"),
            TvStandard::Ntsc => ("smpte170m", "smpte170m", "smpte170m"),
        };
        let gop = self.tv.gop().to_string();
        let mut a: Vec<OsString> = [
            "-c:v", "mpeg2video", "-threads", "0", "-pix_fmt", "yuv420p", "-r", self.tv.frame_rate(), "-g", &gop, "-bf", "2",
            "-dc", "9", "-qmin", "2", "-mbd", "rd",
            // Closed GOPs: dvdauthor warns on every chapter cell otherwise; mpeg2video refuses +cgop with scene-change detection.
            "-flags", "+cgop", "-sc_threshold", "1000000000",
        ]
        .map(s)
        .to_vec();
        if !self.fast {
            a.extend([s("-trellis"), s("1")]);
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
        if let Some(hw) = self.hwaccel {
            a.extend(["-hwaccel".into(), hw.into()]);
        }
        a.extend(self.info.input_args());
        a.extend(["-map".into(), format!("0:{}", v.index).into(), "-vf".into(), video_filter(v, self.aspect, self.tv, self.resolution, self.video).into()]);
        a
    }

    /// Arguments of the 2-pass analysis run: video only, statistics log, no real output.
    pub fn first_pass_args(&self) -> Vec<OsString> {
        let mut a = self.video_input_args();
        a.extend(self.video_codec_args());
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
        a.extend(self.video_codec_args());
        if self.two_pass {
            a.extend([s("-pass"), s("2"), s("-passlogfile"), self.passlog_prefix().into_os_string()]);
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
    mut progress: impl FnMut(EncodePhase, FfmpegProgress),
) -> Result<Vec<String>, ToolError> {
    let duration = job.info.duration_secs;
    let io = |error| ToolError::Io { tool: "Arbeitsordner".into(), error };
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
    let cleanup = |job: &EncodeJob| {
        job.cleanup_passlogs().map_err(|error| ToolError::Io { tool: "2-pass log cleanup".into(), error })
    };
    let final_phase = if job.two_pass {
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
    if job.two_pass {
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
/// size minus the audio tracks. shortcut: average only, peaks above the disc's VBV limit are
/// not detected (that needs a full bitstream scan).
pub fn source_video_kbps(info: &MediaInfo) -> Option<u32> {
    let v = info.video.as_ref()?;
    if let Some(b) = v.bit_rate {
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
pub fn bd_video_copy(info: &MediaInfo, rate: BdRate, resolution: Resolution, vp: VideoProcessing, max_kbps: u32) -> Result<u32, String> {
    let kbps = bd_stream_copy(info, vp, max_kbps)?;
    let v = info.video.as_ref().ok_or("no video stream")?;
    match crate::bd::bd_native_format(v) {
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

/// Everything about a stream copy except the picture format (see [`bd_video_copy`]).
/// shortcut: HRD/VBV parameters and GOP structure of the stream are not checked; a picky player
/// may still reject an in-spec web rip, which is what the "encode" mode is for.
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
                a.map_or(SILENCE_OUT, |a| plan_audio(a, self.audio_target, self.audio_mode, filter_of(&self.audio_filters, n).is_some() || self.info.trim.is_some()))
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

/// Renders text subtitle `n` (its extracted `.srt`) natively into `<stem>_s<n>/` and returns the
/// spumux XML, like [`bitmap_subtitle_xml`].
async fn text_subtitle_xml(job: &EncodeJob<'_>, n: usize, sub: &SubtitleStream) -> Result<String, ToolError> {
    let fail = |e: subpic::SubpicError| ToolError::Failed { tool: format!("Text subtitle #{}", sub.index), status: String::new(), log: e.to_string() };
    let path = job.dir.join(job.srt(n));
    let data = std::fs::read(&path).map_err(|error| ToolError::Io { tool: path.display().to_string(), error })?;
    let (dir, tv, folder) = (job.dir.clone(), job.tv, format!("{}_s{}", job.stem, n + 1));
    let (xml, _count) = tokio::task::spawn_blocking(move || {
        let (cues, _skipped) = textsub::parse_srt(&String::from_utf8_lossy(&data));
        let decoded = textsub::render_cues(&cues, tv);
        let (w, h) = tv.size();
        let whole = subpic::Target { frame: (w, h), area: (0, 0, w, h), source: (0, 0, w, h) };
        subpic::render(&decoded, &whole, tv, &dir, &folder)
    })
    .await
    .map_err(|e| fail(subpic::SubpicError::Format(e.to_string())))?
    .map_err(fail)?;
    Ok(xml)
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
                AudioStream { index: 1, codec: "eac3".into(), channels: 6, channel_layout: "5.1".into(), language: Some("ger".into()), title: None, default: true, bit_rate: None, sample_rate: None, profile: None },
                AudioStream { index: 2, codec: "aac".into(), channels: 2, channel_layout: "stereo".into(), language: None, title: None, default: false, bit_rate: None, sample_rate: None, profile: None },
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

        // Fast mode drops exactly the trellis option, in every run.
        let strings = |a: Vec<OsString>| a.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let without_trellis = |a: Vec<String>| {
            let i = a.iter().position(|x| x == "-trellis").expect("default encode uses trellis");
            assert_eq!(a[i - 2..=i + 1], ["-sc_threshold", "1000000000", "-trellis", "1"]);
            [&a[..i], &a[i + 2..]].concat()
        };
        let fast = EncodeJob { fast: true, two_pass: true, ..job.clone() };
        let slow = EncodeJob { two_pass: true, ..job };
        for (f, s) in [
            (strings(fast.encode_args("x.mpg")), strings(slow.encode_args("x.mpg"))),
            (strings(fast.first_pass_args()), strings(slow.first_pass_args())),
        ] {
            assert!(!f.iter().any(|x| x == "-trellis"), "{f:?}");
            assert!(f.join(" ").contains("-mbd rd -flags +cgop -sc_threshold 1000000000 -color_primaries bt470bg"), "{f:?}");
            assert_eq!(f, without_trellis(s));
        }
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
                AudioStream { index: 1, codec: "eac3".into(), channels: 6, channel_layout: "5.1".into(), language: Some("ger".into()), title: None, default: true, bit_rate: None, sample_rate: None, profile: None },
                AudioStream { index: 2, codec: "aac".into(), channels: 2, channel_layout: "stereo".into(), language: None, title: None, default: false, bit_rate: None, sample_rate: None, profile: None },
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
        let ok = |i: &MediaInfo, r| bd_video_copy(i, r, Resolution::Hd1080, VideoProcessing::default(), 30_000);
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
        // 1080p25 does not exist on a BD; real 1080i25 is copied.
        let pal = BdRate { fps: (25, 1), fake_interlaced: true };
        let mut i = info.clone();
        i.video.as_mut().unwrap().fps = 25.0;
        assert!(ok(&i, pal).unwrap_err().contains("not a Blu-ray"));
        i.video.as_mut().unwrap().field_order = "tt".into();
        assert!(ok(&i, pal).is_ok());
        // A compliant film source on a 720 title, or forced to another rate, is re-encoded.
        assert!(bd_video_copy(&info, film, Resolution::Hd720, VideoProcessing::default(), 30_000).unwrap_err().contains("title is 1280×720"));
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
            audio_target: AudioTarget::Bluray,
            audio_filters: vec![],
            maxrate_kbps: BD_MAXRATE_KBPS,
            bufsize_kbit: BD_BUFSIZE_KBIT,
            copy_video: false,
        }
    }

    #[test]
    fn video_processing_modes() {
        let vp = |scale, filter, deinterlace| VideoProcessing { scale, filter, deinterlace };
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
        assert!(matches!(kind(&hd, Bluray, Auto, false), AudioOut::Copy { bsf: None, kbps: 4000, ch: 8, .. }));
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
        assert_eq!(p, AudioOut::Pcm { codec: "pcm_s24le", ch: 6 });
        assert_eq!(p.kbps(), 48 * 24 * 6);
        assert_eq!(plan_audio(&a, Avchd, AudioMode::Lpcm, false), AudioOut::Pcm { codec: "pcm_s16le", ch: 2 });
        // DVD: ffmpeg's muxer cannot write usable DVD LPCM, so LPCM mode means AC-3 there.
        assert!(matches!(plan_audio(&a, Dvd, AudioMode::Lpcm, false), AudioOut::Ac3 { .. }));
        assert_eq!(plan_audio(&a, Bluray, AudioMode::Lpcm, false).kind(), AudioKind::Lpcm);
    }

    #[test]
    fn audio_args_copy_and_pcm() {
        let copy = AudioOut::Copy { kind: AudioKind::Dts, bsf: Some("dca_core"), kbps: 1536, ch: 6 };
        let strings = |v: Vec<OsString>| v.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert_eq!(strings(audio_track_args(1, copy, Some("loudnorm"))), "-c:a:1 copy -bsf:a:1 dca_core", "no filter on copies");
        assert_eq!(
            strings(audio_track_args(0, AudioOut::Pcm { codec: "pcm_s24le", ch: 6 }, None)),
            "-c:a:0 pcm_s24le -ar:a:0 48000 -ac:a:0 6"
        );
        assert_eq!(
            strings(audio_track_args(2, AudioOut::Ac3 { kbps: 192, ch: 2 }, Some("aresample=48000"))),
            "-c:a:2 ac3 -ar:a:2 48000 -b:a:2 192k -ac:a:2 2 -filter:a:2 aresample=48000"
        );
        assert_eq!(AudioOut::Pcm { codec: "pcm_s16le", ch: 2 }.dvdauthor_format(), "pcm");
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

}
