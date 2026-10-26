//! Stream inspector based on `ffprobe -v quiet -print_format json -show_format -show_streams
//! -show_chapters`.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum ProbeError {
    Io(std::io::Error),
    Failed { path: PathBuf, status: String },
    Json(serde_json::Error),
    NoVideo(PathBuf),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Io(e) => write!(f, "cannot run ffprobe: {e}"),
            ProbeError::Failed { path, status } => write!(f, "ffprobe failed on {}: {status}", path.display()),
            ProbeError::Json(e) => write!(f, "unexpected ffprobe output: {e}"),
            ProbeError::NoVideo(p) => write!(f, "{}: no video stream", p.display()),
        }
    }
}

impl std::error::Error for ProbeError {}

// ---------------------------------------------------------------------------
// Result model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub duration_secs: f64,
    pub video: Option<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    pub chapters: Vec<Chapter>,
    /// Section of the file that is used (see [`apply_trim`]); `duration_secs` and the chapters
    /// already describe the section.
    pub trim: Option<Trim>,
}

/// Used section of a file in seconds of the source.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Trim {
    pub start: f64,
    pub end: f64,
}

/// Pixels cut from the edges of the source picture (user input, source pixels).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Crop {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

/// The remaining picture after a [`Crop`], in source pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoStream {
    /// Absolute stream index in the container (`-map 0:N`).
    pub index: u32,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Display aspect ratio (width / height as shown).
    pub display_aspect: f64,
    pub fps: f64,
    /// `progressive`, `tt`, `bb`, `tb`, `bt` or empty if unknown.
    pub field_order: String,
    /// Picture section to keep (see [`apply_crop`]); `display_aspect` already is the aspect of
    /// that section, `width`/`height` stay those of the source frame.
    pub crop: Option<CropRect>,
    /// PQ (`smpte2084`) or HLG (`arib-std-b67`) transfer: needs tonemapping to SDR.
    pub hdr: bool,
}

impl VideoStream {
    pub fn is_interlaced(&self) -> bool {
        matches!(self.field_order.as_str(), "tt" | "bb" | "tb" | "bt")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub channels: u32,
    pub channel_layout: String,
    /// ISO 639-2 tag as stored in the file (`ger`, `eng`, ...), if any.
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    /// Stream bitrate in bit/s as reported by ffprobe, if any.
    pub bit_rate: Option<u32>,
    pub sample_rate: Option<u32>,
    /// ffprobe `profile` (`DTS`, `DTS-HD MA`, `LC`, ...).
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SubtitleKind {
    /// SRT, ASS, WebVTT, mov_text: rendered by spumux.
    Text,
    /// DVD/PGS/DVB bitmaps.
    Bitmap,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubtitleStream {
    pub index: u32,
    pub codec: String,
    pub kind: SubtitleKind,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    /// Bitmap canvas size, if reported.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Chapter {
    pub start_secs: f64,
    pub end_secs: f64,
    pub title: Option<String>,
}

pub fn subtitle_kind(codec: &str) -> SubtitleKind {
    match codec {
        "dvd_subtitle" | "hdmv_pgs_subtitle" | "dvb_subtitle" | "xsub" | "dvb_teletext" => SubtitleKind::Bitmap,
        _ => SubtitleKind::Text,
    }
}

// ---------------------------------------------------------------------------
// ffprobe JSON
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    #[serde(default)]
    chapters: Vec<RawChapter>,
    format: Option<RawFormat>,
}

#[derive(Deserialize, Default)]
struct RawFormat {
    duration: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawTags {
    language: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    default: u8,
    #[serde(default)]
    forced: u8,
}

#[derive(Deserialize)]
struct RawStream {
    index: u32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    sample_aspect_ratio: Option<String>,
    display_aspect_ratio: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    field_order: Option<String>,
    color_transfer: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    sample_rate: Option<String>,
    profile: Option<String>,
    #[serde(default)]
    tags: RawTags,
    #[serde(default)]
    disposition: RawDisposition,
}

#[derive(Deserialize)]
struct RawChapter {
    start_time: Option<String>,
    end_time: Option<String>,
    #[serde(default)]
    tags: RawTags,
}

/// Parses `a:b` or `a/b` ratios; `None` for `0:1`, `N/A` etc.
fn ratio(s: Option<&str>) -> Option<f64> {
    let s = s?;
    let (a, b) = s.split_once(':').or_else(|| s.split_once('/'))?;
    let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a > 0.0 && b > 0.0).then_some(a / b)
}

fn secs(s: Option<&str>) -> Option<f64> {
    s?.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

fn tag(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "und")
}

/// Builds a [`MediaInfo`] from ffprobe's JSON output.
pub fn parse_ffprobe_json(path: &Path, json: &str) -> Result<MediaInfo, ProbeError> {
    let raw: RawProbe = serde_json::from_str(json).map_err(ProbeError::Json)?;
    let mut info = MediaInfo {
        path: path.to_path_buf(),
        duration_secs: raw.format.as_ref().and_then(|f| secs(f.duration.as_deref())).unwrap_or(0.0),
        video: None,
        audio: Vec::new(),
        subtitles: Vec::new(),
        chapters: Vec::new(),
        trim: None,
    };

    for s in raw.streams {
        let codec = s.codec_name.clone().unwrap_or_default();
        match s.codec_type.as_deref() {
            Some("video") if info.video.is_none() && codec != "mjpeg" && codec != "png" => {
                let (w, h) = (s.width.unwrap_or(0), s.height.unwrap_or(0));
                let sar = ratio(s.sample_aspect_ratio.as_deref()).unwrap_or(1.0);
                let dar = ratio(s.display_aspect_ratio.as_deref())
                    .unwrap_or(if h > 0 { w as f64 * sar / h as f64 } else { 16.0 / 9.0 });
                let fps = ratio(s.avg_frame_rate.as_deref())
                    .or_else(|| ratio(s.r_frame_rate.as_deref()))
                    .unwrap_or(25.0);
                if info.duration_secs <= 0.0 {
                    info.duration_secs = secs(s.duration.as_deref()).unwrap_or(0.0);
                }
                info.video = Some(VideoStream {
                    index: s.index,
                    codec,
                    width: w,
                    height: h,
                    display_aspect: dar,
                    fps,
                    field_order: s.field_order.unwrap_or_default(),
                    crop: None,
                    hdr: matches!(s.color_transfer.as_deref(), Some("smpte2084" | "arib-std-b67")),
                });
            }
            Some("audio") => info.audio.push(AudioStream {
                index: s.index,
                codec,
                channels: s.channels.unwrap_or(2),
                channel_layout: s.channel_layout.unwrap_or_default(),
                language: tag(s.tags.language),
                title: tag(s.tags.title),
                default: s.disposition.default == 1,
                bit_rate: s.bit_rate.as_deref().and_then(|b| b.trim().parse().ok()),
                sample_rate: s.sample_rate.as_deref().and_then(|b| b.trim().parse().ok()),
                profile: s.profile.filter(|p| !p.is_empty()),
            }),
            Some("subtitle") => info.subtitles.push(SubtitleStream {
                index: s.index,
                kind: subtitle_kind(&codec),
                codec,
                language: tag(s.tags.language),
                title: tag(s.tags.title),
                forced: s.disposition.forced == 1,
                width: s.width.filter(|w| *w > 0),
                height: s.height.filter(|h| *h > 0),
            }),
            _ => {}
        }
    }

    info.chapters = raw
        .chapters
        .into_iter()
        .filter_map(|c| {
            Some(Chapter {
                start_secs: secs(c.start_time.as_deref())?,
                end_secs: secs(c.end_time.as_deref()).unwrap_or(info.duration_secs),
                title: tag(c.tags.title),
            })
        })
        .collect();
    info.chapters.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));
    Ok(info)
}

impl MediaInfo {
    /// Input arguments of every ffmpeg run that reads the video: the used section as input
    /// options (timestamps then start at 0), then `-i path`.
    pub fn input_args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = Vec::new();
        if let Some(t) = self.trim {
            a.extend(["-ss".into(), format!("{:.3}", t.start).into(), "-to".into(), format!("{:.3}", t.end).into()]);
        }
        a.extend(["-i".into(), self.path.clone().into_os_string()]);
        a
    }

    /// Start of the used section in the source file (0 without trim).
    pub fn trim_start(&self) -> f64 {
        self.trim.map_or(0.0, |t| t.start)
    }
}

/// Shortest section that makes sense.
pub const MIN_TRIM_SECS: f64 = 1.0;

/// Limits `info` to `start`..`end` seconds of the source (`None` = file start / end): checks the
/// range, sets `duration_secs`, clips and shifts the chapters. A range covering the whole file
/// changes nothing.
pub fn apply_trim(info: &mut MediaInfo, start: Option<f64>, end: Option<f64>) -> Result<(), String> {
    if start.is_none() && end.is_none() {
        return Ok(());
    }
    let full = info.duration_secs;
    let (s, e) = (start.unwrap_or(0.0), end.unwrap_or(full));
    if !s.is_finite() || !e.is_finite() || s < 0.0 || e > full + 0.5 || s >= e {
        return Err(format!("Schnitt: Start ({s:.1} s) und Ende ({e:.1} s) müssen innerhalb der Laufzeit ({full:.1} s) liegen, Start vor Ende"));
    }
    let e = e.min(full);
    if e - s < MIN_TRIM_SECS {
        return Err(format!("Schnitt: der Ausschnitt ist {:.1} s lang, mindestens {MIN_TRIM_SECS:.0} s sind nötig", e - s));
    }
    if s < 0.001 && e > full - 0.001 {
        return Ok(());
    }
    info.trim = Some(Trim { start: s, end: e });
    info.duration_secs = e - s;
    info.chapters = std::mem::take(&mut info.chapters)
        .into_iter()
        .filter(|c| c.end_secs > s && c.start_secs < e)
        .map(|c| Chapter { start_secs: (c.start_secs - s).max(0.0), end_secs: c.end_secs.min(e) - s, title: c.title })
        .collect();
    Ok(())
}

/// Smallest picture that is left after a crop.
pub const MIN_CROP_SIZE: u32 = 64;

/// Makes the video of `info` show only the picture inside `crop`: edges are rounded down to even
/// values (multiples of 4 vertically for interlaced sources, whose fields must stay intact), the
/// display aspect becomes that of the section. An all-zero crop changes nothing.
pub fn apply_crop(info: &mut MediaInfo, crop: Crop) -> Result<(), String> {
    if crop == Crop::default() {
        return Ok(());
    }
    let Some(v) = info.video.as_mut() else { return Err("Zuschnitt: die Datei hat keine Videospur".into()) };
    let step_y = if v.is_interlaced() { 4 } else { 2 };
    let down = |x: u32, step: u32| x / step * step;
    let (left, right) = (down(crop.left, 2), down(crop.right, 2));
    let (top, bottom) = (down(crop.top, step_y), down(crop.bottom, step_y));
    let w = v.width.saturating_sub(left + right);
    let h = v.height.saturating_sub(top + bottom);
    if left + right >= v.width || top + bottom >= v.height || w < MIN_CROP_SIZE || h < MIN_CROP_SIZE {
        return Err(format!(
            "Zuschnitt: von {}×{} Pixeln bleiben nur {w}×{h} übrig (mindestens {MIN_CROP_SIZE}×{MIN_CROP_SIZE})",
            v.width, v.height
        ));
    }
    v.display_aspect *= (w as f64 / v.width as f64) / (h as f64 / v.height as f64);
    v.crop = Some(CropRect { x: left, y: top, w, h });
    Ok(())
}

/// Runs ffprobe on `path`.
pub async fn probe(ffprobe: &Path, path: &Path) -> Result<MediaInfo, ProbeError> {
    let out = crate::tools::tool_command(ffprobe, crate::tools::ProcessPriority::Normal)
        .args(["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams", "-show_chapters"])
        .arg(path)
        .output()
        .await
        .map_err(ProbeError::Io)?;
    if !out.status.success() {
        return Err(ProbeError::Failed { path: path.to_path_buf(), status: out.status.to_string() });
    }
    let info = parse_ffprobe_json(path, &String::from_utf8_lossy(&out.stdout))?;
    if info.video.is_none() {
        return Err(ProbeError::NoVideo(path.to_path_buf()));
    }
    Ok(info)
}

// ---------------------------------------------------------------------------
// Languages
// ---------------------------------------------------------------------------

/// (ISO 639-2/B, ISO 639-2/T, ISO 639-1, German name)
const LANGUAGES: &[(&str, &str, &str, &str)] = &[
    ("ger", "deu", "de", "Deutsch"),
    ("eng", "eng", "en", "Englisch"),
    ("fre", "fra", "fr", "Französisch"),
    ("spa", "spa", "es", "Spanisch"),
    ("ita", "ita", "it", "Italienisch"),
    ("dut", "nld", "nl", "Niederländisch"),
    ("por", "por", "pt", "Portugiesisch"),
    ("pol", "pol", "pl", "Polnisch"),
    ("rus", "rus", "ru", "Russisch"),
    ("tur", "tur", "tr", "Türkisch"),
    ("cze", "ces", "cs", "Tschechisch"),
    ("slo", "slk", "sk", "Slowakisch"),
    ("hun", "hun", "hu", "Ungarisch"),
    ("swe", "swe", "sv", "Schwedisch"),
    ("dan", "dan", "da", "Dänisch"),
    ("nor", "nor", "no", "Norwegisch"),
    ("fin", "fin", "fi", "Finnisch"),
    ("gre", "ell", "el", "Griechisch"),
    ("jpn", "jpn", "ja", "Japanisch"),
    ("chi", "zho", "zh", "Chinesisch"),
    ("kor", "kor", "ko", "Koreanisch"),
    ("ara", "ara", "ar", "Arabisch"),
    ("heb", "heb", "he", "Hebräisch"),
    ("hin", "hin", "hi", "Hindi"),
    ("ukr", "ukr", "uk", "Ukrainisch"),
    ("rum", "ron", "ro", "Rumänisch"),
    ("bul", "bul", "bg", "Bulgarisch"),
    ("hrv", "hrv", "hr", "Kroatisch"),
    ("srp", "srp", "sr", "Serbisch"),
    ("tha", "tha", "th", "Thai"),
    ("vie", "vie", "vi", "Vietnamesisch"),
];

fn lookup(tag: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    let t = tag.trim().to_ascii_lowercase();
    LANGUAGES.iter().find(|(b, term, one, _)| *b == t || *term == t || *one == t)
}

/// ISO 639-1 code for the DVD (`ger` -> `de`), `None` if unknown.
pub fn iso639_1(tag: &str) -> Option<&'static str> {
    lookup(tag).map(|l| l.2)
}

/// Human readable (German) language name, falling back to the tag itself.
pub fn language_name(tag: Option<&str>) -> String {
    match tag {
        Some(t) => lookup(t).map_or_else(|| t.to_string(), |l| l.3.to_string()),
        None => "Unbekannt".to_string(),
    }
}

/// Matches a user filter (`de`, `ger`, `deu`) against a stream language.
pub fn language_matches(filter: &str, lang: Option<&str>) -> bool {
    let Some(lang) = lang else { return false };
    match (lookup(filter), lookup(lang)) {
        (Some(a), Some(b)) => a.2 == b.2,
        _ => filter.eq_ignore_ascii_case(lang),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "streams": [
        {"index": 0, "codec_name": "h264", "codec_type": "video", "width": 1920, "height": 1080,
         "sample_aspect_ratio": "1:1", "display_aspect_ratio": "16:9", "r_frame_rate": "24000/1001",
         "avg_frame_rate": "24000/1001", "field_order": "progressive",
         "disposition": {"default": 1, "forced": 0}},
        {"index": 1, "codec_name": "eac3", "codec_type": "audio", "channels": 6, "channel_layout": "5.1(side)",
         "tags": {"language": "ger", "title": "Deutsch DD+"}, "disposition": {"default": 1, "forced": 0}},
        {"index": 2, "codec_name": "aac", "codec_type": "audio", "channels": 2, "channel_layout": "stereo",
         "tags": {"language": "eng"}},
        {"index": 3, "codec_name": "subrip", "codec_type": "subtitle", "tags": {"language": "ger"},
         "disposition": {"default": 0, "forced": 1}},
        {"index": 4, "codec_name": "hdmv_pgs_subtitle", "codec_type": "subtitle", "width": 1920, "height": 1080,
         "tags": {"language": "und"}},
        {"index": 5, "codec_name": "mjpeg", "codec_type": "video", "width": 600, "height": 900}
      ],
      "chapters": [
        {"id": 1, "start_time": "300.000000", "end_time": "600.000000", "tags": {"title": "Zwei"}},
        {"id": 0, "start_time": "0.000000", "end_time": "300.000000", "tags": {"title": "Eins"}}
      ],
      "format": {"duration": "5400.123000"}
    }"#;

    #[test]
    fn detects_hdr_transfer() {
        let hdr = |t: &str| {
            let json = format!(r#"{{"streams": [{{"index": 0, "codec_name": "hevc", "codec_type": "video", "width": 3840, "height": 2160, "color_transfer": "{t}"}}]}}"#);
            parse_ffprobe_json(Path::new("x.mkv"), &json).unwrap().video.unwrap().hdr
        };
        assert_eq!((hdr("smpte2084"), hdr("arib-std-b67"), hdr("bt709")), (true, true, false));
    }

    #[test]
    fn parses_ffprobe_json() {
        let i = parse_ffprobe_json(Path::new("x.mkv"), SAMPLE).unwrap();
        assert!((i.duration_secs - 5400.123).abs() < 1e-9);
        let v = i.video.unwrap();
        assert_eq!((v.index, v.width, v.height), (0, 1920, 1080));
        assert!((v.display_aspect - 16.0 / 9.0).abs() < 1e-9);
        assert!((v.fps - 23.976).abs() < 0.001);
        assert!(!v.is_interlaced());

        assert_eq!(i.audio.len(), 2);
        assert_eq!((i.audio[0].index, i.audio[0].channels), (1, 6));
        assert_eq!(i.audio[0].language.as_deref(), Some("ger"));
        assert!(i.audio[0].default);
        assert_eq!(i.audio[1].codec, "aac");

        assert_eq!(i.subtitles.len(), 2);
        assert_eq!(i.subtitles[0].kind, SubtitleKind::Text);
        assert!(i.subtitles[0].forced);
        assert_eq!(i.subtitles[1].kind, SubtitleKind::Bitmap);
        assert_eq!(i.subtitles[1].language, None, "'und' means unknown");
        assert_eq!(i.subtitles[1].width, Some(1920));

        assert_eq!(i.chapters.len(), 2);
        assert_eq!(i.chapters[0].title.as_deref(), Some("Eins"), "sorted by start");
        assert_eq!(i.chapters[1].start_secs, 300.0);
    }

    fn sample() -> MediaInfo {
        parse_ffprobe_json(Path::new("x.mkv"), SAMPLE).unwrap()
    }

    #[test]
    fn trim_limits_the_runtime_and_shifts_chapters() {
        let mut i = sample();
        apply_trim(&mut i, Some(100.0), Some(400.0)).unwrap();
        assert_eq!((i.duration_secs, i.trim), (300.0, Some(Trim { start: 100.0, end: 400.0 })));
        // "Eins" (0-300) is clipped to the section, "Zwei" (300-600) starts 200 s in.
        assert_eq!(i.chapters.len(), 2);
        assert_eq!((i.chapters[0].start_secs, i.chapters[0].end_secs), (0.0, 200.0));
        assert_eq!((i.chapters[1].start_secs, i.chapters[1].end_secs), (200.0, 300.0));
        let args: Vec<String> = i.input_args().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["-ss", "100.000", "-to", "400.000", "-i", "x.mkv"]);
        assert_eq!(i.trim_start(), 100.0);

        // Chapters outside the section are dropped.
        let mut i = sample();
        apply_trim(&mut i, Some(350.0), None).unwrap();
        assert_eq!(i.chapters.len(), 1);
        assert!((i.duration_secs - (5400.123 - 350.0)).abs() < 1e-6);

        // The whole file is no trim; nothing given is no trim.
        let mut i = sample();
        apply_trim(&mut i, Some(0.0), Some(5400.123)).unwrap();
        apply_trim(&mut i, None, None).unwrap();
        assert!(i.trim.is_none());
        assert_eq!(i.input_args().len(), 2);
    }

    #[test]
    fn trim_is_validated() {
        let bad = |s, e| {
            let mut i = sample();
            apply_trim(&mut i, s, e).unwrap_err()
        };
        assert!(bad(Some(-1.0), Some(10.0)).contains("Schnitt"));
        assert!(bad(Some(10.0), Some(10.0)).contains("Schnitt"));
        assert!(bad(Some(20.0), Some(10.0)).contains("Schnitt"));
        assert!(bad(Some(0.0), Some(6000.0)).contains("Laufzeit"));
        assert!(bad(Some(10.0), Some(10.5)).contains("mindestens 1 s"));
        assert!(bad(Some(f64::NAN), None).contains("Schnitt"));
    }

    #[test]
    fn crop_changes_the_aspect_and_rounds_to_even() {
        // 1920x1080 with 140 px black bars top and bottom (+1 odd pixel at the left).
        let mut i = sample();
        apply_crop(&mut i, Crop { top: 140, bottom: 140, left: 1, right: 0 }).unwrap();
        let v = i.video.unwrap();
        assert_eq!(v.crop, Some(CropRect { x: 0, y: 140, w: 1920, h: 800 }));
        assert!((v.display_aspect - 1920.0 / 800.0).abs() < 1e-9, "{}", v.display_aspect);
        assert_eq!((v.width, v.height), (1920, 1080), "the source frame size stays");

        // Interlaced: vertical edges are multiples of 4.
        let mut i = sample();
        i.video.as_mut().unwrap().field_order = "tt".into();
        apply_crop(&mut i, Crop { top: 6, bottom: 10, left: 0, right: 0 }).unwrap();
        assert_eq!(i.video.unwrap().crop, Some(CropRect { x: 0, y: 4, w: 1920, h: 1080 - 4 - 8 }));

        // Too little left / nothing to do.
        let mut i = sample();
        assert!(apply_crop(&mut i, Crop { top: 500, bottom: 560, left: 0, right: 0 }).unwrap_err().contains("Zuschnitt"));
        assert!(apply_crop(&mut i, Crop { top: 0, bottom: 0, left: 1900, right: 0 }).is_err());
        apply_crop(&mut i, Crop::default()).unwrap();
        assert!(i.video.unwrap().crop.is_none());
    }

    #[test]
    fn duration_falls_back_to_stream() {
        let json = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mpeg4","width":640,
            "height":480,"sample_aspect_ratio":"0:1","duration":"12.5"}],"format":{}}"#;
        let i = parse_ffprobe_json(Path::new("a.avi"), json).unwrap();
        assert_eq!(i.duration_secs, 12.5);
        assert!((i.video.unwrap().display_aspect - 4.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn languages() {
        assert_eq!(iso639_1("ger"), Some("de"));
        assert_eq!(iso639_1("deu"), Some("de"));
        assert_eq!(iso639_1("ENG"), Some("en"));
        assert_eq!(iso639_1("xyz"), None);
        assert_eq!(language_name(Some("fre")), "Französisch");
        assert_eq!(language_name(None), "Unbekannt");
        assert!(language_matches("de", Some("ger")));
        assert!(!language_matches("en", Some("ger")));
        assert!(!language_matches("de", None));
    }
}
