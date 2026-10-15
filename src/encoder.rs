//! ffmpeg wrapper: source -> DVD-compliant MPEG-2 program stream (`{stem}.mpg`).
//!
//! Per title (see [`run_encode`]):
//! 1. text subtitles are extracted as `{stem}_sN.srt` (spumux needs them before the encode);
//! 2. with 2-pass encoding a video-only analysis run (`-pass 1`, output to `NUL`) writes
//!    `{stem}_2pass-0.log`;
//! 3. one ffmpeg run decodes the source once and writes MPEG-2 video (720x576/480, 4:3 or 16:9)
//!    plus one AC-3 track per selected audio track (192 kbps stereo / 448 kbps 5.1) straight
//!    into a DVD program stream (`-f dvd`, 2048-byte packs) – no elementary streams on disk;
//! 4. with text subtitles that stream is piped through one spumux per subtitle stream
//!    (see [`spumux_textsub_xml`]) into `{stem}.mpg`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::author::{xml_escape, ClipSource, TvStandard};
use crate::probe::{AudioStream, MediaInfo, SubtitleStream, VideoStream};
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

/// Size of the picture inside the DVD frame (720x576 PAL, 720x480 NTSC): wider sources get
/// letterbox bars, narrower ones pillarbox bars.
pub fn active_size(source_aspect: f64, target: Aspect, tv: TvStandard) -> (u32, u32) {
    let (fw, fh) = tv.size();
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

/// ffmpeg `-vf` chain: deinterlace, frame rate of the TV standard, scale to the active area in
/// BT.601, pad to the full DVD frame.
pub fn video_filter(v: &VideoStream, aspect: Aspect, tv: TvStandard) -> String {
    let (w, h) = active_size(v.display_aspect, aspect, tv);
    let (fw, fh) = tv.size();
    let mut f = Vec::new();
    if v.is_interlaced() {
        f.push("bwdif=mode=send_frame".to_string());
    }
    f.push(format!("fps={}", tv.frame_rate()));
    f.push(format!("scale={w}:{h}:flags=lanczos:out_color_matrix=bt601:out_range=tv"));
    if (w, h) != (fw, fh) {
        f.push(format!("pad={fw}:{fh}:(ow-iw)/2:(oh-ih)/2:black"));
    }
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
    /// Text subtitles to convert (rendered later by spumux).
    pub text_subtitles: Vec<&'a SubtitleStream>,
    pub video_kbps: u32,
    pub aspect: Aspect,
    /// Work directory; all outputs are written here.
    pub dir: PathBuf,
    /// File name stem, e.g. `title_01`.
    pub stem: String,
    /// 2-pass video encoding (analysis run + final run) for a more exact average bitrate.
    pub two_pass: bool,
    pub tv: TvStandard,
    /// Hardware decoder for the source (`-hwaccel`), see [`Toolchain::probe_hwaccel`].
    pub hwaccel: Option<&'static str>,
}

/// Font for text subtitles (copied into the work directory's `.spumux/`).
pub const SUBTITLE_FONT: &str = "arial.ttf";

impl EncodeJob<'_> {
    /// `-passlogfile` prefix; ffmpeg appends `-0.log` (and `.mbtree` for x264).
    pub fn passlog_prefix(&self) -> PathBuf {
        self.dir.join(format!("{}_2pass", self.stem))
    }

    /// 2-pass statistics files currently present in the work directory.
    pub fn passlog_files(&self) -> Vec<PathBuf> {
        let prefix = format!("{}_2pass", self.stem);
        std::fs::read_dir(&self.dir)
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

    /// Deletes the 2-pass statistics files.
    pub fn cleanup_passlogs(&self) -> std::io::Result<()> {
        for f in self.passlog_files() {
            std::fs::remove_file(f)?;
        }
        Ok(())
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
            "-dc", "9", "-qmin", "2", "-mbd", "rd", "-trellis", "1",
            "-color_primaries", primaries, "-color_trc", trc, "-colorspace", matrix,
        ]
        .map(s)
        .to_vec();
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
        a.extend([
            "-i".into(),
            self.info.path.clone().into_os_string(),
            "-map".into(),
            format!("0:{}", v.index).into(),
            "-vf".into(),
            video_filter(v, self.aspect, self.tv).into(),
        ]);
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
        format!("{}_s{}.srt", self.stem, n + 1)
    }
    /// spumux XML of text subtitle stream `n`.
    pub fn subtitle_xml(&self, n: usize) -> String {
        format!("{}_s{}.xml", self.stem, n + 1)
    }
    /// Final title VOB source for dvdauthor.
    pub fn vob(&self) -> String {
        format!("{}.mpg", self.stem)
    }

    pub fn audio_kbps_total(&self) -> u32 {
        self.audio.iter().map(|a| ac3_settings(a.channels).0).sum()
    }

    /// Extraction of the text subtitles as SRT (stream copy speed, needed by spumux before the
    /// encode starts); `None` without text subtitles.
    pub fn subtitle_args(&self) -> Option<Vec<OsString>> {
        if self.text_subtitles.is_empty() {
            return None;
        }
        let mut a: Vec<OsString> = vec!["-y".into(), "-i".into(), self.info.path.clone().into_os_string()];
        for (n, sub) in self.text_subtitles.iter().enumerate() {
            a.extend(
                ["-map", &format!("0:{}", sub.index), "-c:s", "srt", "-vn", "-an", "-f", "srt"].map(OsString::from),
            );
            a.push(self.dir.join(self.srt(n)).into_os_string());
        }
        Some(a)
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
        if !self.audio.is_empty() {
            a.extend([s("-c:a"), s("ac3"), s("-ar"), s("48000")]);
        }
        for (n, track) in self.audio.iter().enumerate() {
            let (kbps, ch) = ac3_settings(track.channels);
            a.extend([format!("-b:a:{n}").into(), format!("{kbps}k").into(), format!("-ac:a:{n}").into(), ch.to_string().into()]);
        }
        a.extend(["-sn", "-dn", "-f", "dvd", "-muxrate", &format!("{DVD_MUX_RATE_KBPS}k"), "-packetsize", "2048"].map(s));
        a.push(output.into());
        a
    }
}

/// Encodes `job` into `job.vob()` in the work directory: text subtitles are extracted first,
/// with `job.two_pass` an analysis run (statistics only) follows, then the final run writes the
/// DVD program stream – through one spumux per text subtitle stream via pipes when there are
/// subtitles. The 2-pass statistics files are removed after success (stale ones from an aborted
/// earlier attempt before starting). Returns the spumux logs.
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
        for n in 0..job.text_subtitles.len() {
            let xml = job.subtitle_xml(n);
            std::fs::write(job.dir.join(&xml), spumux_textsub_xml(job.tv, &job.srt(n), SUBTITLE_FONT)).map_err(io)?;
            stages.push((xml, n as u32));
        }
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

/// spumux XML rendering an SRT file as DVD subpictures. The font is looked up in
/// `$HOME/.spumux/` (see [`crate::tools::Toolchain::spumux`]).
pub fn spumux_textsub_xml(tv: TvStandard, srt_file: &str, font_file: &str) -> String {
    // SRT times are real time; render at the movie's frame rate. Height 2 lines short of the
    // frame, as recommended for spumux.
    let fps = match tv {
        TvStandard::Pal => "25",
        TvStandard::Ntsc => "29.97",
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<subpictures format="{format}">
  <stream>
    <textsub filename="{}" characterset="UTF-8" fontsize="28.0" font="{}"
      horizontal-alignment="center" vertical-alignment="bottom"
      left-margin="60" right-margin="60" top-margin="20" bottom-margin="40"
      subtitle-fps="{fps}" movie-fps="{fps}" movie-width="{w}" movie-height="{h}"/>
  </stream>
</subpictures>
"#,
        xml_escape(srt_file),
        xml_escape(font_file),
        format = tv.spumux_format(),
        w = tv.width(),
        h = tv.height() - 2,
    )
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
#[allow(clippy::too_many_arguments)]
pub fn motion_menu_args(
    tv: TvStandard,
    base: &Path,
    foreground: Option<&Path>,
    slots: &[VideoSlot],
    duration_secs: f64,
    audio: Option<&ClipSource>,
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
        let fit = if s.letterbox {
            format!(
                "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
                w = s.width,
                h = s.height
            )
        } else {
            format!("scale={}:{}", s.width, s.height)
        };
        // Square pixels first, so anamorphic sources keep their display aspect.
        f.push(format!(
            "[{}:v]fps={rate},scale=trunc(ih*dar/2)*2:ih,setsar=1,{fit},setsar=1,format=rgba,\
             trim=duration={d},setpts=PTS-STARTPTS[clip{i}]",
            i + 1
        ));
        f.push(format!("[{cur}][clip{i}]overlay=x={}:y={}:format=rgb:eof_action=repeat[m{i}]", s.x, s.y));
        cur = format!("m{i}");
    }
    if let Some(fg) = fg_input {
        f.push(format!("[{fg}:v]fps={rate},format=rgba,setsar=1[fg]"));
        f.push(format!("[{cur}][fg]overlay=0:0:format=rgb[mfg]"));
        cur = "mfg".into();
    }
    f.push(format!("[{cur}]format=yuv420p,setdar=4/3[vout]"));
    if audio.is_some() {
        let fade_out = (duration_secs - 1.0).max(0.0);
        f.push(format!(
            "[{audio_input}:a:0]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,\
             atrim=duration={d},asetpts=N/SR/TB,afade=t=in:d=0.5,afade=t=out:st={fade_out:.3}:d=1[aout]"
        ));
    } else {
        f.push(format!("[{audio_input}:a]asetpts=N/SR/TB[aout]"));
    }

    a.extend(["-filter_complex".into(), f.join(";").into()]);
    a.extend(
        [
            "-map", "[vout]", "-map", "[aout]", "-t", &d, "-target", tv.ffmpeg_target(), "-aspect", "4:3",
            "-b:v", "5000k", "-maxrate", "8000k", "-c:a", "ac3", "-b:a", "192k",
        ]
        .map(OsString::from),
    );
    a.push(output.as_os_str().to_owned());
    a
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

/// ffmpeg filter rotating clockwise by `rotation` degrees (0/90/180/270).
fn rotate_filter(rotation: u32) -> Option<&'static str> {
    match rotation % 360 {
        90 => Some("transpose=1"),
        180 => Some("hflip,vflip"),
        270 => Some("transpose=2"),
        _ => None,
    }
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
    output: &Path,
) -> Vec<OsString> {
    let d = format!("{duration_secs:.3}");
    let rate = tv.frame_rate();
    let (fw, fh) = tv.size();
    let mut a: Vec<OsString> = vec!["-y".into()];
    let still = |a: &mut Vec<OsString>, png: &Path| {
        a.extend(["-loop", "1", "-framerate", rate, "-t", &d, "-i"].map(OsString::from));
        a.push(png.as_os_str().to_owned());
    };
    let clip_input = |a: &mut Vec<OsString>, clip: &ClipSource| {
        a.extend(["-stream_loop", "-1", "-ss", &format!("{:.3}", clip.start_secs), "-t", &d, "-i"].map(OsString::from));
        a.push(clip.path.as_os_str().to_owned());
    };
    // Square pixels first, so anamorphic sources keep their display aspect.
    let square = "scale=trunc(ih*dar/2)*2:ih,setsar=1";
    let mut f: Vec<String> = Vec::new();

    // Input 0: background, then the shading over it.
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
            still(&mut a, path);
            f.push(format!(
                "[0:v]fps={rate},{square},{}{}{pre_rgba},format=rgba{post_rgba},setsar=1[{bg_label}]",
                fit_filter(Fit::Cover, fw, fh, "black"),
                blur(*b)
            ));
        }
        MenuBackground::Video { clip, blur: b } => {
            clip_input(&mut a, clip);
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
                // Fit into the unrotated box, then rotate into the layout box.
                let (bw, bh) = if rotation % 180 == 90 { (*height, *width) } else { (*width, *height) };
                let mut chain = format!("[{input}:v]fps={rate},{square},{}", fit_filter(*fit, bw, bh, "black"));
                if let Some(r) = rotate_filter(*rotation) {
                    chain.push(',');
                    chain.push_str(r);
                }
                chain.push_str(",setsar=1,format=rgba");
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
    f.push(format!("[{cur}]format=yuv420p,setdar=4/3[vout]"));

    // Last input: the menu sound or silence.
    let audio_input = media.layers.len() + 1;
    match audio {
        Some(src) => {
            clip_input(&mut a, src);
            let fade_out = (duration_secs - 1.0).max(0.0);
            f.push(format!(
                "[{audio_input}:a:0]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,\
                 atrim=duration={d},asetpts=N/SR/TB,afade=t=in:d=0.5,afade=t=out:st={fade_out:.3}:d=1[aout]"
            ));
        }
        None => {
            a.extend(["-f", "lavfi", "-t", &d, "-i", "anullsrc=r=48000:cl=stereo"].map(OsString::from));
            f.push(format!("[{audio_input}:a]asetpts=N/SR/TB[aout]"));
        }
    }

    a.extend(["-filter_complex".into(), f.join(";").into()]);
    a.extend(
        [
            "-map", "[vout]", "-map", "[aout]", "-t", &d, "-target", tv.ffmpeg_target(), "-aspect", "4:3",
            "-b:v", "5000k", "-maxrate", "8000k", "-c:a", "ac3", "-b:a", "192k",
        ]
        .map(OsString::from),
    );
    a.push(output.as_os_str().to_owned());
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

    fn video(w: u32, h: u32, dar: f64, field_order: &str) -> VideoStream {
        VideoStream {
            index: 0,
            codec: "h264".into(),
            width: w,
            height: h,
            display_aspect: dar,
            fps: 23.976,
            field_order: field_order.into(),
        }
    }

    #[test]
    fn aspect_choice_and_bars() {
        assert_eq!(Aspect::for_source(16.0 / 9.0), Aspect::Wide);
        assert_eq!(Aspect::for_source(2.39), Aspect::Wide);
        assert_eq!(Aspect::for_source(4.0 / 3.0), Aspect::Standard);
        assert_eq!(Aspect::for_source(1.5), Aspect::Standard);
        let pal = TvStandard::Pal;
        assert_eq!(active_size(16.0 / 9.0, Aspect::Wide, pal), (720, 576));
        assert_eq!(active_size(2.39, Aspect::Wide, pal), (720, 428)); // letterbox
        assert_eq!(active_size(1.5, Aspect::Standard, pal), (720, 512)); // letterbox
        assert_eq!(active_size(1.25, Aspect::Standard, pal), (676, 576)); // pillarbox
        assert_eq!(active_size(16.0 / 9.0, Aspect::Wide, TvStandard::Ntsc), (720, 480));
        assert_eq!(active_size(2.39, Aspect::Wide, TvStandard::Ntsc), (720, 358));
    }

    #[test]
    fn filters() {
        let f = video_filter(&video(1920, 1080, 16.0 / 9.0, "progressive"), Aspect::Wide, TvStandard::Pal);
        assert_eq!(f, "fps=25,scale=720:576:flags=lanczos:out_color_matrix=bt601:out_range=tv,setdar=16/9,format=yuv420p");
        let n = video_filter(&video(1920, 1080, 16.0 / 9.0, "progressive"), Aspect::Wide, TvStandard::Ntsc);
        assert!(n.starts_with("fps=30000/1001,scale=720:480:"), "{n}");
        let f = video_filter(&video(1920, 800, 2.4, "tt"), Aspect::Wide, TvStandard::Pal);
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
                AudioStream { index: 1, codec: "eac3".into(), channels: 6, channel_layout: "5.1".into(), language: Some("ger".into()), title: None, default: true },
                AudioStream { index: 2, codec: "aac".into(), channels: 2, channel_layout: "stereo".into(), language: None, title: None, default: false },
            ],
            subtitles: vec![SubtitleStream { index: 3, codec: "subrip".into(), kind: SubtitleKind::Text, language: None, title: None, forced: false, width: None, height: None }],
            chapters: vec![],
        };
        let job = EncodeJob {
            info: &info,
            audio: info.audio.iter().collect(),
            text_subtitles: info.subtitles.iter().collect(),
            video_kbps: 5000,
            aspect: Aspect::Wide,
            dir: PathBuf::from("w"),
            stem: "title_01".into(),
            two_pass: false,
            tv: TvStandard::Pal,
            hwaccel: None,
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
        assert!(joined.contains("-map 0:1 -map 0:2 -c:a ac3 -ar 48000 -b:a:0 448k -ac:a:0 6 -b:a:1 192k -ac:a:1 2"), "{joined}");
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
        let none = EncodeJob { text_subtitles: vec![], ..job };
        assert!(none.subtitle_args().is_none());
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
        let a = motion_menu_args(TvStandard::Pal, Path::new("bg.png"), Some(Path::new("fg.png")), &slots, 25.0, Some(&menu_audio), Path::new("m.mpg"));
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
        let a = motion_menu_args(TvStandard::Pal, Path::new("bg.png"), None, &[slot(0, false, true)], 8.0, None, Path::new("m.mpg"));
        let s = joined(&a);
        assert!(s.contains("-f lavfi -t 8.000 -i anullsrc=r=48000:cl=stereo"), "{s}");
        assert!(s.contains("[2:a]asetpts=N/SR/TB[aout]"), "silence is input 2 (no foreground)");
        assert!(!s.contains("[fg]"));
        // Clips with sound do not change the menu sound any more: silence stays silence.
        let silent = joined(&motion_menu_args(TvStandard::Pal, Path::new("b"), None, &[slot(0, true, true)], 8.0, None, Path::new("m")));
        assert!(silent.contains("anullsrc") && !silent.contains("aresample"));
        let ntsc = joined(&motion_menu_args(TvStandard::Ntsc, Path::new("b"), None, &[slot(0, true, true)], 8.0, None, Path::new("m")));
        assert!(ntsc.contains("-framerate 30000/1001") && ntsc.contains("-target ntsc-dvd"), "{ntsc}");
        assert!(ntsc.contains("fps=30000/1001"));
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
        let a = joined(&custom_menu_args(TvStandard::Pal, &media, 20.0, Some(&clip("music.mp3")), Path::new("m.mpg")));
        // Inputs in order: background, layers, sound.
        let order: Vec<usize> = ["bg.jpg", "under.png", "movie.mkv", "over.png", "music.mp3"]
            .iter()
            .map(|n| a.find(&format!("-i {n}")).unwrap_or_else(|| panic!("{n} missing: {a}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{a}");
        assert!(a.contains("[0:v]fps=25,scale=trunc(ih*dar/2)*2:ih,setsar=1,scale=720:576:force_original_aspect_ratio=increase,crop=720:576,gblur=sigma=8.0"), "{a}");
        // Rotated by 90°: fitted into 90x160, then transposed into the 160x90 box.
        assert!(a.contains("scale=90:160:force_original_aspect_ratio=increase,crop=90:160,transpose=1"), "{a}");
        assert!(a.contains("colorchannelmixer=aa=0.500"), "{a}");
        assert!(a.contains("[l1]overlay=x=100:y=200:format=rgb:eof_action=repeat[m1]"), "{a}");
        assert!(a.contains("[m1][l2]overlay=0:0:format=rgb[m2];[m2]format=yuv420p,setdar=4/3[vout]"), "{a}");
        assert!(a.contains("[4:a:0]aresample=48000"), "{a}");
        assert!(a.contains("-t 20.000 -target pal-dvd"), "{a}");

        // Colour background, no layers, silence, NTSC.
        let still = CustomMenuMedia { background: MenuBackground::Color([0x10, 0x20, 0xA0]), overlay: BackgroundOverlay::None, layers: vec![] };
        let a = joined(&custom_menu_args(TvStandard::Ntsc, &still, 1.0, None, Path::new("m.mpg")));
        assert!(a.contains("color=c=0x1020A0:s=720x480:r=30000/1001:d=1.000"), "{a}");
        assert!(a.contains("[bg]format=yuv420p,setdar=4/3[vout]") && a.contains("[1:a]asetpts=N/SR/TB[aout]"), "{a}");
        assert!(a.contains("-target ntsc-dvd"));

        // Shading over the background.
        let with = |overlay| {
            let m = CustomMenuMedia { overlay, ..media.clone() };
            joined(&custom_menu_args(TvStandard::Pal, &m, 20.0, None, Path::new("m.mpg")))
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
    }

    #[test]
    fn textsub_xml() {
        let x = spumux_textsub_xml(TvStandard::Pal, "t_s1.srt", "arial.ttf");
        assert!(x.contains(r#"filename="t_s1.srt""#));
        assert!(x.contains(r#"font="arial.ttf""#));
        assert!(x.contains(r#"movie-fps="25""#) && x.contains(r#"movie-height="574""#));
        let n = spumux_textsub_xml(TvStandard::Ntsc, "t_s1.srt", "arial.ttf");
        assert!(n.contains(r#"<subpictures format="NTSC">"#) && n.contains(r#"movie-height="478""#) && n.contains(r#"movie-fps="29.97""#));
    }
}
