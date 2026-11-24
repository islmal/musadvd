//! Blu-ray build (BD-25 / BD-50): probe -> bitrate -> H.264 + AC-3 encodes -> native BDAV muxer.
//!
//! Output layout:
//! ```text
//! <output>/<project>/BDMV/         playlist, clip info, m2ts streams
//! <output>/<project>/CERTIFICATE/
//! <output>/<project>.musadvd-work/        title_NN.mkv, subtitle files, mux.log
//! ```
//!
//! Every title gets its own mux run (one playlist + clip each), so each keeps its own frame
//! rate. Without menus the titles play one after the other (`bd_menu::plan_without_menus`); with
//! the menu editor's layout the title playlists follow the menu clips. AVCHD ("BD5/BD9") is the
//! same BDMV structure on DVD media with AVCHD limits. The DVD path in [`crate::dvd`] is
//! untouched; [`crate::dvd::build_dvd`] dispatches here for BD and AVCHD discs.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::author::{AuthorError, ClipSource, MenuFrame, Resolution, TvStandard};
use crate::bd_menu::{self, BdNav};
use crate::hdmv;
use crate::bd_author::{BdProject, BdSubTrack, BdTitleFiles, SubKind};
use crate::calculator::{Clamp, DiscType, EncodingProfile};
use crate::config::HwAccelMode;
use crate::dvd::{
    apply_selection, available_cpus, cancellable, chapter_starts, custom_chapters, default_jobs, display_path, job, remove_old_output,
    run_limited, safe_folder_name, select_streams, unique_dir, AtPath, BuildError, BuildOptions, BuildPhase, BuildReport,
    Event, Job, JobEvent, JobProgress, TitleLoad, TitleReport, WORK_DIR,
};
use crate::encoder::{self, bd_audio_copy, plan_audio_with, Aspect, AudioMode, AudioOut, AudioTarget, BdAudioMode, BdEncodeJob, BdEncoder, BdVideoMode, EncodePhase, VideoProcessing, SILENCE_OUT};
use crate::eta::{self, Eta, FrameEta, JobPhase, Medium};
use crate::i18n::{tr, UiLang};
use crate::tf;
use crate::probe::{self, ExternalSubtitle, MediaInfo, SubtitleKind, SubtitleStream, EXTERNAL_SUB_SOURCE_BASE};
use crate::subpic;
use crate::tools::{ToolError, Toolchain};

type TitleJob = (TitleReport, Option<&'static str>, Option<String>, f64);

/// Parallel hardware encodes. Measured 2026-11-17 (RTX 4070 SUPER, 1080p25 NVENC p5 `-multipass
/// fullres`, 1000 frames per job): 1 job 206 fps, 2 jobs 212, 4 jobs 216, 8 jobs 218 in total, so
/// the encoder is saturated by one job; the second only overlaps the CPU side (decode, filters,
/// audio). shortcut: NVENC only, QSV/AMF not measured; raise if one of them scales with jobs.
const HW_MAX_JOBS: usize = 2;

/// Encoder per title: `fake_interlaced` (1080i) rates cannot be flagged by the hardware
/// encoders and always use x264.
fn title_encoders(hw: Option<BdEncoder>, rates: impl Iterator<Item = BdRate>) -> Vec<BdEncoder> {
    rates.map(|r| if r.fake_interlaced { BdEncoder::X264 } else { hw.unwrap_or(BdEncoder::X264) }).collect()
}

/// Parallel encodes of moving menus. Measured 2026-11-17 (Ryzen 7 8845HS, six 1080p slot clips
/// overlaid, x264 medium, 500 frames per job): 1 job 53 fps, 2 jobs 55, 4 jobs 53 in total, at
/// 2.0 GB of RAM per job. x264 already uses every core, so more jobs only cost memory; two keep
/// the cores busy while the other waits on the piped intro compositing.
const MOTION_MENU_MAX_JOBS: usize = 2;

/// Frame rate of one Blu-ray title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BdRate {
    /// Numerator and denominator.
    pub fps: (u32, u32),
    /// Progressive frames flagged as 1080i (x264 `fake-interlaced`): 1080p25/29.97 do not exist
    /// on a BD.
    pub fake_interlaced: bool,
}

impl BdRate {
    /// ffmpeg's `fps=` value (`25`, `24000/1001`).
    pub fn ffmpeg(self) -> String {
        match self.fps {
            (n, 1) => n.to_string(),
            (n, d) => format!("{n}/{d}"),
        }
    }

    pub fn fps_f64(self) -> f64 {
        self.fps.0 as f64 / self.fps.1 as f64
    }

    /// Keyframe interval: one second.
    pub fn keyint(self) -> u32 {
        self.fps_f64().round() as u32
    }
}

/// Every Blu-ray frame rate by its GUI/CLI name; [`BdRate::allowed`] says which frame size takes it.
const BD_RATES: [(&str, BdRate); 6] = [
    ("23.976", BdRate { fps: (24000, 1001), fake_interlaced: false }),
    ("24", BdRate { fps: (24, 1), fake_interlaced: false }),
    ("25i", BdRate { fps: (25, 1), fake_interlaced: true }),
    ("29.97i", BdRate { fps: (30000, 1001), fake_interlaced: true }),
    ("50", BdRate { fps: (50, 1), fake_interlaced: false }),
    ("59.94", BdRate { fps: (60000, 1001), fake_interlaced: false }),
];

impl BdRate {
    pub fn name(self) -> &'static str {
        BD_RATES.iter().find(|(_, r)| *r == self).map_or("?", |(n, _)| n)
    }

    /// The rate called `name`, if `res` allows it.
    pub fn from_name(name: &str, res: Resolution) -> Option<BdRate> {
        BD_RATES.iter().find(|(n, _)| *n == name).map(|(_, r)| *r).filter(|r| r.allowed(res))
    }

    /// Film rates exist at both sizes; 1080 has 25/29.97 only interlaced, 720 50/59.94 only progressive.
    pub fn allowed(self, res: Resolution) -> bool {
        film_rate(self.fps_f64()).is_some() || self.fake_interlaced == (res != Resolution::Hd720)
    }
}

/// The Blu-ray format (frame size and rate) a source already has, if it is one. `avchd` also
/// accepts 1440×1080, which AVCHD allows beside 1920×1080: it is the same 1080 clip format, the
/// 16:9 shape comes from the stream's own pixel aspect ratio and the muxer only needs the 1080 class.
pub fn bd_native_format(v: &probe::VideoStream, avchd: bool) -> Option<(Resolution, BdRate)> {
    let res = match (v.width, v.height) {
        (1920 | 1440, 1080) if v.width == 1920 || avchd => Resolution::Hd1080,
        (1280, 720) => Resolution::Hd720,
        _ => return None,
    };
    let rate = BD_RATES.iter().map(|(_, r)| *r).find(|r| (v.fps - r.fps_f64()).abs() < 0.01 && r.fake_interlaced == v.is_interlaced())?;
    rate.allowed(res).then_some((res, rate))
}

fn film_rate(fps: f64) -> Option<(u32, u32)> {
    if (fps - 24000.0 / 1001.0).abs() < 0.01 {
        Some((24000, 1001))
    } else if (fps - 24.0).abs() < 0.01 {
        Some((24, 1))
    } else {
        None
    }
}

/// Frame rate of a Blu-ray title from its source rate(s): film sources keep 23.976 / 24 (mixed
/// film rates become 23.976), everything else gets the TV rate – 1080: 25 / 29.97 flagged as
/// interlaced, 720: 50 / 59.94 progressive.
pub fn bd_frame_rate(source_fps: &[f64], tv: TvStandard, res: Resolution) -> BdRate {
    let films: Vec<Option<(u32, u32)>> = source_fps.iter().map(|&f| film_rate(f)).collect();
    if !films.is_empty() && films.iter().all(Option::is_some) {
        let first = films[0].expect("checked");
        let rate = if films.iter().all(|f| *f == Some(first)) { first } else { (24000, 1001) };
        return BdRate { fps: rate, fake_interlaced: false };
    }
    match (res, tv) {
        (Resolution::Hd720, TvStandard::Pal) => BdRate { fps: (50, 1), fake_interlaced: false },
        (Resolution::Hd720, TvStandard::Ntsc) => BdRate { fps: (60000, 1001), fake_interlaced: false },
        (_, TvStandard::Pal) => BdRate { fps: (25, 1), fake_interlaced: true },
        (_, TvStandard::Ntsc) => BdRate { fps: (30000, 1001), fake_interlaced: true },
    }
}

/// Track layout shared by all titles of the playlist.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layout {
    pub audio_langs: Vec<Option<String>>,
    pub subs: Vec<BdSubTrack>,
}

/// Audio track k exists if any title has at least k+1 tracks (language of the first title that
/// has it); subtitle tracks likewise by position, with kind and language of the first title.
pub(crate) fn harmonise(audio: &[Vec<Option<String>>], subs: &[Vec<BdSubTrack>]) -> Layout {
    let n_audio = audio.iter().map(Vec::len).max().unwrap_or(0);
    let n_subs = subs.iter().map(Vec::len).max().unwrap_or(0);
    Layout {
        audio_langs: (0..n_audio).map(|k| audio.iter().find_map(|t| t.get(k)).cloned().flatten()).collect(),
        subs: (0..n_subs).filter_map(|k| subs.iter().find_map(|t| t.get(k)).cloned()).collect(),
    }
}

fn sub_track(s: &SubtitleStream) -> BdSubTrack {
    let is_text = s.kind == SubtitleKind::Text || s.codec == "dvb_teletext";
    BdSubTrack { kind: if is_text { SubKind::Text } else { SubKind::Pgs }, lang: s.language.clone() }
}

/// Both subtitle kinds end up as PGS `.sup` files (text cues are rendered natively).
const SUB_EXT: &str = "sup";

enum TitleSub<'a> {
    Embedded(&'a SubtitleStream),
    External(&'a ExternalSubtitle, SubKind),
}

/// One input after stream selection.
struct BdTitle {
    info: MediaInfo,
    label: String,
    /// Indices into `info.audio`.
    audio: Vec<usize>,
    /// Indices into `info.subtitles` (text and PGS only).
    subs: Vec<usize>,
    /// Standalone external subtitle files, numbered after `subs`.
    external_subtitles: Vec<(ExternalSubtitle, SubKind)>,
    /// Disc track `k` plays natural subtitle `sub_order[k]` (`subs`, then `external_subtitles`).
    sub_order: Vec<usize>,
    chapters: Vec<f64>,
    stem: String,
    rate: BdRate,
    audio_mode: AudioMode,
    ac3_kbps: u32,
    bd_audio: BdAudioMode,
    track_label_mode: probe::TrackLabelMode,
    ui_lang: UiLang,
    video: VideoProcessing,
    audio_target: AudioTarget,
    loudnorm: bool,
    /// Frame size of this title (the project's unless the title overrides it).
    resolution: Resolution,
    video_mode: BdVideoMode,
    /// Video is stream-copied at this average kbit/s ([`encoder::bd_video_copy`]).
    copy_kbps: Option<u32>,
    /// Quality weight of the encoded video ([`crate::dvd::quality_weight`]).
    weight: f64,
    /// Styling and sync of the text subtitles (rendered to PGS).
    subtitle_style: crate::textsub::SubtitleStyle,
}

impl BdTitle {
    fn audio_streams(&self, layout: &Layout) -> Vec<Option<&probe::AudioStream>> {
        (0..layout.audio_langs.len()).map(|k| self.audio.get(k).map(|&i| &self.info.audio[i])).collect()
    }

    /// The stream playing the role of disc subtitle track `k` (same kind as the track).
    fn sub_for(&self, layout: &Layout, k: usize) -> Option<TitleSub<'_>> {
        let m = *self.sub_order.get(k)?;
        if m < self.subs.len() {
            let s = &self.info.subtitles[self.subs[m]];
            (sub_track(s).kind == layout.subs[k].kind).then_some(TitleSub::Embedded(s))
        } else {
            self.external_subtitles
                .get(m - self.subs.len())
                .filter(|(_, kind)| *kind == layout.subs[k].kind)
                .map(|(ext, kind)| TitleSub::External(ext, *kind))
        }
    }

    fn sub_tracks(&self) -> Vec<BdSubTrack> {
        self.in_disc_order(
            self.subs
                .iter()
                .map(|&s| sub_track(&self.info.subtitles[s]))
                .chain(self.external_subtitles.iter().map(|(ext, kind)| BdSubTrack { kind: *kind, lang: ext.language.clone() }))
                .collect(),
        )
    }

    fn subtitle_sources(&self) -> Vec<u32> {
        self.in_disc_order(
            self.subs
                .iter()
                .map(|&s| self.info.subtitles[s].index)
                .chain((0..self.external_subtitles.len()).map(|i| EXTERNAL_SUB_SOURCE_BASE + i as u32))
                .collect(),
        )
    }

    /// Reorders a natural-order subtitle list (embedded, then external) into disc order.
    fn in_disc_order<T: Clone>(&self, natural: Vec<T>) -> Vec<T> {
        self.sub_order.iter().map(|&m| natural[m].clone()).collect()
    }

    /// Output plan of every disc audio track (silent filler where this title has none).
    fn audio_outs(&self, layout: &Layout) -> Vec<AudioOut> {
        self.audio_streams(layout)
            .into_iter()
            .map(|a| a.map_or(SILENCE_OUT, |a| plan_audio_with(a, self.audio_target, self.audio_mode, self.bd_audio, self.loudnorm || self.info.trim.is_some(), self.ac3_kbps)))
            .collect()
    }

    fn audio_kbps(&self, layout: &Layout) -> u32 {
        self.audio_outs(layout).iter().map(|o| o.kbps()).sum()
    }

    /// Menu/report label of each selected audio stream, with the channel layout of its disc slot.
    fn audio_labels(&self, layout: &Layout) -> Vec<String> {
        let all = probe::audio_labels(&self.info, self.track_label_mode, self.ui_lang);
        let outs = self.audio_outs(layout);
        self.audio
            .iter()
            .enumerate()
            .map(|(k, &i)| outs.get(k).map_or_else(|| all[i].clone(), |o| probe::with_layout(&all[i], o.layout_label(), self.track_label_mode)))
            .collect()
    }

    fn subtitle_labels(&self) -> Vec<String> {
        let exts: Vec<ExternalSubtitle> = self.external_subtitles.iter().map(|(e, _)| e.clone()).collect();
        let all = probe::all_subtitle_labels(&self.info, &exts, self.track_label_mode, self.ui_lang);
        let emb = self.subs.iter().map(|&i| all[i].clone());
        let ext_start = self.info.subtitles.len();
        let ext = (0..exts.len()).map(|i| all[ext_start + i].clone());
        self.in_disc_order(emb.chain(ext).collect())
    }
}

/// Size of all files below `dir` in MB.
fn tree_mb(dir: &Path) -> f64 {
    fn bytes(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| match e.metadata() {
                        Ok(m) if m.is_dir() => bytes(&e.path()),
                        Ok(m) => m.len(),
                        Err(_) => 0,
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    bytes(dir) as f64 / 1024.0 / 1024.0
}

/// Prepares all subtitle `.sup` files (`{stem}_s{k+1}.sup`) for `t` in `work`:
/// placeholders for missing/mismatched slots, ffmpeg-extracted embedded subtitles, and
/// external `.srt`/`.ass`/`.ssa`/`.vtt`/`.sub` or `.sup` files.
async fn prepare_bd_subtitles(tools: &Toolchain, t: &BdTitle, layout: &Layout, work: &Path) -> Result<(), BuildError> {
    let duration = t.info.duration_secs;
    let (w, h) = t.resolution.frame_size(TvStandard::Pal);
    let mut extract: Vec<(usize, &SubtitleStream)> = Vec::new();
    let mut ext_text: Vec<(usize, &ExternalSubtitle)> = Vec::new();

    for k in 0..layout.subs.len() {
        match t.sub_for(layout, k) {
            Some(TitleSub::Embedded(s)) => extract.push((k, s)),
            Some(TitleSub::External(ext, SubKind::Text)) => ext_text.push((k, ext)),
            Some(TitleSub::External(ext, SubKind::Pgs)) => {
                let sup = work.join(encoder::subtitle_file(&t.stem, k, SUB_EXT));
                if let Some(tr) = t.info.trim {
                    let data = std::fs::read(&ext.path).at(&ext.path)?;
                    let fps = t.rate.fps;
                    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, subpic::SubpicError> {
                        let decoded = subpic::decode_pgs(&data)?;
                        let pics = encoder::trim_pictures(decoded.pictures, tr.start, tr.end);
                        Ok(crate::pgs::build_sup_from_pictures(&pics, decoded.canvas, fps))
                    })
                    .await
                    .map_err(|e| ToolError::Failed { tool: "External PGS".into(), status: String::new(), log: e.to_string() })?
                    .map_err(|e| ToolError::Failed { tool: "External PGS".into(), status: String::new(), log: e.to_string() })?;
                    std::fs::write(&sup, bytes).at(&sup)?;
                } else {
                    std::fs::copy(&ext.path, &sup).at(&sup)?;
                }
            }
            None => {
                let path = work.join(encoder::subtitle_file(&t.stem, k, SUB_EXT));
                std::fs::write(&path, subpic::empty_pgs(w as u16, h as u16, t.rate.fps)).at(&path)?;
            }
        }
    }
    if let Some(args) = encoder::extract_subtitle_args(&t.info, &extract, work, &t.stem) {
        tools.ffmpeg(&args, duration, |_| {}).await?;
    }
    // Text cues and nonstandard bitmap subtitles become PGS on the title's HD canvas.
    for &(k, s) in &extract {
        if sub_track(s).kind == SubKind::Text {
            let srt = work.join(encoder::subtitle_file(&t.stem, k, "srt"));
            let sup = work.join(encoder::subtitle_file(&t.stem, k, SUB_EXT));
            let text = std::fs::read(&srt).at(&srt)?;
            let (cues, _skipped) = crate::textsub::parse_srt(&String::from_utf8_lossy(&text));
            let st = &crate::textsub::SubtitleStyle { lang: crate::fonts::cjk_region(s.language.as_deref(), s.title.as_deref()).map(Into::into), ..t.subtitle_style.clone() };
            let cues = crate::textsub::apply_offset(cues, st.offset_ms);
            std::fs::write(&sup, crate::pgs::build_sup(&cues, &crate::textsub::Canvas::hd(w, h).with_style(st), t.rate.fps)).at(&sup)?;
        } else if matches!(s.codec.as_str(), "dvd_subtitle" | "dvb_subtitle" | "xsub") {
            let sup = work.join(encoder::subtitle_file(&t.stem, k, SUB_EXT));
            let json = if s.codec == "dvd_subtitle" {
                tools.ffprobe_output(&subpic::vobsub_probe_args(&t.info.path, s.index)).await?
            } else {
                let temp_vob = work.join(format!("{}_temp_sub_{}.vob", t.stem, k));
                let mut args = vec!["-y".into()];
                args.extend(t.info.input_args());
                args.extend(["-map".into(), format!("0:{}", s.index).into(), "-c:s".into(), "dvdsub".into(), "-f".into(), "dvd".into()]);
                args.push(temp_vob.as_os_str().to_owned());
                tools.ffmpeg(&args, duration, |_| {}).await?;
                let probe_args = subpic::vobsub_probe_args(&temp_vob, 0);
                let json = tools.ffprobe_output(&probe_args).await?;
                let _ = std::fs::remove_file(&temp_vob);
                json
            };
            let v = t.info.video.as_ref().expect("probed file has video");
            let fallback = (720, if v.width == 720 { v.height } else if v.height >= 576 { 576 } else { 480 });
            let trim = t.info.trim;
            let fps = t.rate.fps;
            let canvas_size = (w, h);
            let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, subpic::SubpicError> {
                let mut probe = subpic::parse_vobsub_probe(&json)?;
                if let Some(tr) = trim {
                    probe = probe.trimmed(tr.start, tr.end);
                }
                let (palette, size) = subpic::parse_idx_header(&String::from_utf8_lossy(&probe.extradata));
                let decoded = subpic::decode_vobsub(&probe.packets, palette.as_ref(), size.unwrap_or(fallback));
                Ok(crate::pgs::build_sup_from_pictures(&decoded.pictures, canvas_size, fps))
            })
            .await
            .map_err(|e| ToolError::Failed { tool: "VobSub-zu-PGS".into(), status: String::new(), log: e.to_string() })?
            .map_err(|e| ToolError::Failed { tool: "VobSub-zu-PGS".into(), status: String::new(), log: e.to_string() })?;
            std::fs::write(&sup, bytes).at(&sup)?;
        }
    }
    for (k, ext) in ext_text {
        let temp_srt = work.join(encoder::subtitle_file(&t.stem, k, "srt"));
        let sup = work.join(encoder::subtitle_file(&t.stem, k, SUB_EXT));
        let cues = encoder::load_external_text_cues(tools, &ext.path, &temp_srt, t.info.trim).await?;
        let hints = format!("{} {}", ext.title.as_deref().unwrap_or(""), ext.path.file_name().unwrap_or_default().to_string_lossy());
        let st = &crate::textsub::SubtitleStyle { lang: crate::fonts::cjk_region(ext.language.as_deref(), Some(&hints)).map(Into::into), ..t.subtitle_style.clone() };
        let cues = crate::textsub::apply_offset(cues, st.offset_ms);
        std::fs::write(&sup, crate::pgs::build_sup(&cues, &crate::textsub::Canvas::hd(w, h).with_style(st), t.rate.fps)).at(&sup)?;
    }
    // A corrupt `.sup` (copied verbatim or produced above) fails here with its track named, not
    // deep inside the muxer.
    for k in 0..layout.subs.len() {
        let sup = work.join(encoder::subtitle_file(&t.stem, k, SUB_EXT));
        let (codec, source) = match t.sub_for(layout, k) {
            Some(TitleSub::Embedded(s)) => (s.codec.as_str(), t.info.path.as_path()),
            Some(TitleSub::External(ext, _)) => (ext.path.extension().and_then(|e| e.to_str()).unwrap_or("external"), ext.path.as_path()),
            None => ("placeholder", sup.as_path()),
        };
        let data = std::fs::read(&sup).at(&sup)?;
        if let Err(e) = crate::bd_mux::validate_sup(&data) {
            return Err(BuildError::Internal(format!("subtitle track {} ({codec}, {}): {e}", k + 1, source.display())));
        }
    }
    Ok(())
}

/// Encodes one title: subtitle files (extracted or placeholders), then H.264 + AC-3 into
/// `<stem>.mkv`. Returns the report, the hardware decoder used and the real duration.
#[allow(clippy::too_many_arguments)]
async fn encode_bd_title(
    tools: &Toolchain,
    t: &BdTitle,
    layout: &Layout,
    work: &Path,
    video_kbps: u32,
    (two_pass, fast, hw_decode): (bool, bool, Option<HwAccelMode>),
    encoder: BdEncoder,
    (maxrate_kbps, bufsize_kbit): (u32, u32),
    progress: JobProgress,
) -> Result<(TitleReport, Option<&'static str>, Option<String>, f64), BuildError> {
    let resolution = t.resolution;
    let (hwaccel, hwaccel_note) = match hw_decode {
        Some(mode) => tools.probe_hwaccel_for(&t.info.path, mode, Some(encoder)).await,
        None => (None, None),
    };
    let duration = t.info.duration_secs;

    prepare_bd_subtitles(tools, t, layout, work).await?;

    let rate = t.rate;
    let audio = t.audio_streams(layout);
    let audio_filters = if t.loudnorm { encoder::measure_loudnorm(tools, &t.info, &audio).await? } else { Vec::new() };
    let job = BdEncodeJob {
        info: &t.info,
        audio,
        video_kbps,
        dir: work.to_path_buf(),
        stem: t.stem.clone(),
        two_pass,
        fast,
        rate,
        resolution,
        hwaccel,
        encoder,
        audio_mode: t.audio_mode,
        bd_audio: t.bd_audio,
        video: t.video,
        audio_target: t.audio_target,
        audio_filters,
        maxrate_kbps,
        bufsize_kbit,
        copy_video: t.copy_kbps.is_some(),
        ac3_kbps: t.ac3_kbps,
    };
    let frames_per_run = duration * rate.fps_f64();
    // Frames done count over both runs of a 2-pass encode; never let them go backwards.
    let mut frames_seen = 0.0f64;
    encoder::run_bd_encode(tools, &job, |phase, p| {
        let (fraction, base) = match phase {
            EncodePhase::Single => (0.98 * p.fraction, 0.0),
            EncodePhase::Analysis => (0.49 * p.fraction, 0.0),
            EncodePhase::Final => (0.49 + 0.49 * p.fraction, frames_per_run),
        };
        frames_seen = frames_seen.max(base + p.frame as f64);
        progress.report_frames(fraction, frames_seen, p.fps);
    })
    .await?;
    progress.report(1.0);

    let mkv = work.join(job.mkv());
    let real = probe::probe(&tools.ffprobe, &mkv).await?.duration_secs;
    if real <= 0.0 {
        return Err(BuildError::Internal(format!("{}: ffprobe reports no duration", mkv.display())));
    }
    let mut labels = t.audio_labels(layout).into_iter();
    let audio = (0..layout.audio_langs.len()).map(|k| if k < t.audio.len() { labels.next().unwrap_or_default() } else { "Silence (placeholder)".to_string() }).collect();
    let sub_labels = t.subtitle_labels();
    let subtitles = (0..layout.subs.len())
        .map(|k| match t.sub_for(layout, k) {
            Some(_) => sub_labels[k].clone(),
            None => "empty (placeholder)".to_string(),
        })
        .collect();
    let report = TitleReport {
        input: t.info.path.clone(),
        label: t.label.clone(),
        duration_secs: real,
        aspect: Aspect::Wide,
        video_kbps,
        audio,
        subtitles,
        chapters: t.chapters.len(),
    };
    Ok((report, hwaccel, hwaccel_note, real))
}

/// Menu stage result: the navigation and how many menus (= first playlists) there are.
struct MenuStage {
    nav: BdNav,
    count: usize,
    motion: usize,
    /// Size of the muxed menu clips in MB (enters the bitrate plan).
    size_mb: f64,
    warnings: Vec<String>,
}

/// Writes `index.bdmv` and `MovieObject.bdmv` into `BDMV` and `BDMV/BACKUP`.
fn write_navigation(bdmv: &Path, nav: &BdNav, avchd: bool) -> std::io::Result<()> {
    let mut index = hdmv::index_bdmv(bd_menu::FIRST_PLAY_OBJECT, bd_menu::TOP_MENU_OBJECT, &nav.title_objects);
    let mut objects = hdmv::movie_object_bdmv(&nav.objects);
    if avchd {
        index = hdmv::avchd_index(&index);
        objects[4..8].copy_from_slice(b"0100");
    }
    for dir in [bdmv.to_path_buf(), bdmv.join("BACKUP")] {
        std::fs::write(dir.join("index.bdmv"), &index)?;
        std::fs::write(dir.join("MovieObject.bdmv"), &objects)?;
    }
    Ok(())
}

/// AVCHD on DVD media, as tsMuxeR writes it: `BDMV` stays at the root with its long names, but
/// playlists and clip infos carry the AVCHD version "0100" (their structure is the BD one), and
/// the Blu-ray-only folders (BD-J, META) are left out; `AUXDATA` stays. The index and movie
/// objects get their AVCHD form in [`write_navigation`].
fn avchd_versions(bdmv: &Path) -> std::io::Result<()> {
    for dir in [bdmv.to_path_buf(), bdmv.join("BACKUP")] {
        for (sub, ext) in [("PLAYLIST", "mpls"), ("CLIPINF", "clpi")] {
            let Ok(entries) = std::fs::read_dir(dir.join(sub)) else { continue };
            for path in entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext))) {
                let mut data = std::fs::read(&path)?;
                if data.get(4..8) == Some(b"0200") {
                    data[4..8].copy_from_slice(b"0100");
                    std::fs::write(&path, &data)?;
                }
            }
        }
    }
    for d in ["BDJO", "JAR", "META", "BACKUP/BDJO", "BACKUP/JAR"] {
        let p = bdmv.join(d);
        if p.is_dir() && std::fs::read_dir(&p)?.next().is_none() {
            std::fs::remove_dir(&p)?;
        }
    }
    std::fs::create_dir_all(bdmv.join("AUXDATA"))
}

/// Source time of each chapter's sample frame (3 s in, like the DVD chapter thumbnails), per
/// menu entry; merged files contribute their chapters one after the other.
fn chapter_samples(titles: &[BdTitle], groups: &[Range<usize>]) -> Vec<Vec<f64>> {
    groups
        .iter()
        .map(|g| {
            titles[g.clone()]
                .iter()
                .flat_map(|t| t.chapters.iter().map(|&c| t.info.trim_start() + (c + 3.0).min((t.info.duration_secs - 0.5).max(0.0))))
                .collect()
        })
        .collect()
}

/// What the menu layout may refer to: one entry per group of merged titles (label, clip and
/// tracks of the first file; chapters and duration summed up).
fn layout_titles(titles: &[BdTitle], groups: &[Range<usize>], disc: &Layout) -> Vec<crate::layout::LayoutTitle> {
    groups
        .iter()
        .map(|g| {
            let (members, t) = (&titles[g.clone()], &titles[g.start]);
            crate::layout::LayoutTitle {
                label: t.label.clone(),
                chapters: members.iter().map(|m| m.chapters.len()).sum(),
                duration_secs: members.iter().map(|m| m.info.duration_secs).sum(),
                clip: ClipSource {
                    path: t.info.path.clone(),
                    start_secs: t.info.trim_start() + (t.info.duration_secs * 0.1).clamp(0.0, 300.0).min((t.info.duration_secs - 1.0).max(0.0)),
                    has_audio: !t.info.audio.is_empty(),
                },
                audio_sources: t.audio.iter().map(|&a| t.info.audio[a].index).collect(),
                subtitle_sources: t.subtitle_sources(),
                audio_labels: t.audio_labels(disc),
                subtitle_labels: t.subtitle_labels(),
            }
        })
        .collect()
}

/// Renders, encodes and muxes the menus of a custom layout into `<work>/BD/BDMV` as playlists /
/// clips `0..count`; the titles follow with higher numbers (see [`bd_menu`]).
#[allow(clippy::too_many_arguments)]
async fn build_menus(
    tools: &Arc<Toolchain>,
    layout: &crate::layout::CustomLayout,
    titles: &[BdTitle],
    groups: &[Range<usize>],
    disc: &Layout,
    opts: &BuildOptions,
    rate: BdRate,
    work: &Path,
    jobs: usize,
    log: &mut (dyn FnMut(Event) + Send),
) -> Result<MenuStage, BuildError> {
    let cancel = &opts.cancel;
    let template = |e: crate::layout::LayoutError| BuildError::Template(e.to_string());
    // The setting switches the layout's intros off; without intro data nothing else changes.
    let silent;
    let layout = if layout.animations && !opts.menu_animations {
        silent = crate::layout::CustomLayout { animations: false, ..layout.clone() };
        &silent
    } else {
        layout
    };
    let (fw, fh) = opts.resolution.frame_size(TvStandard::Pal);
    let frame = MenuFrame::square(fw, fh);
    let layout_titles = layout_titles(titles, groups, disc);
    crate::layout::validate_bd(layout, &layout_titles, frame).map_err(template)?;
    let traits = bd_menu::traits(layout, &layout_titles, frame);
    let members: Vec<Vec<usize>> = groups.iter().map(|g| titles[g.clone()].iter().map(|t| t.chapters.len()).collect()).collect();
    // The entry's end action is the one of its last file (where it applies; GUI and DVD agree).
    let after: Vec<crate::author::AfterPlayback> = groups.iter().map(|g| opts.selections.get(g.end - 1).map(|s| s.after_playback).unwrap_or_default()).collect();
    let nav = bd_menu::plan_with_after(layout, &layout_titles, &traits, opts.playback, &members, &after);
    let secs = opts.motion_duration.unwrap_or(crate::layout::DEFAULT_MENU_SECS);
    let pages = bd_menu::render_pages(layout, &layout_titles, frame, rate, &nav, work, secs).map_err(template)?;
    let warnings: Vec<String> = pages.iter().flat_map(|p| p.warnings.clone()).collect();
    let count = pages.len();
    let motion = pages.iter().filter(|p| p.moving).count();
    let intros = pages.iter().filter(|p| p.intro.is_some()).count();

    cancel.check()?;
    log(Event::Phase(BuildPhase::MenuEncoding));
    log(Event::Step(tf!(opts.ui_lang, "bd_menus_encoding", count = count, motion = motion, intros = intros, jobs = jobs)));
    let mut encode: Vec<Job<()>> = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        if let Some(ig) = &page.igs {
            let sup = work.join(format!("menu_{:02}.sup", i + 1));
            std::fs::write(&sup, crate::igs::build_sup(ig)).at(&sup)?;
        }
        let intro = page.intro.as_ref();
        let args = encoder::bd_custom_menu_args(
            &frame,
            rate,
            &page.media,
            page.secs,
            page.audio.as_ref(),
            intro.map(|i| i.secs),
            &work.join(format!("menu_{:02}.mkv", i + 1)),
        );
        let (tools, secs) = (tools.clone(), page.secs);
        match intro {
            // The intro frames go ffmpeg -> Rust (slot videos) -> ffmpeg over pipes, never to disk.
            Some(intro) => {
                // Stream 0 is the page background, the slot videos follow.
                let streams: Vec<crate::tools::RawStream> = std::iter::once(encoder::intro_background_stream(&frame, rate, &page.media, intro.frames))
                    .chain(intro.slots.iter().filter_map(|&l| encoder::intro_slot_stream(&frame, rate, &page.media.layers[l], intro.frames)))
                    .collect();
                let (sprites, frames, end, size) = (intro.sprites.clone(), intro.frames, intro.end, (fw, fh));
                let compose = std::sync::Arc::new(move |k: usize, streams: &[&[u8]]| {
                    let (background, slots) = streams.split_first().expect("background stream");
                    crate::menu_anim::composite_frame(size.0, size.1, background, &sprites, slots, crate::menu_anim::frame_time(k, frames, end))
                });
                encode.push(job(move |_| async move {
                    tools.ffmpeg_piped(&args, &streams, frames, secs, compose, |_| {}).await?;
                    Ok(())
                }));
            }
            None => encode.push(job(move |_| async move {
                tools.ffmpeg(&args, secs, |_| {}).await?;
                Ok(())
            })),
        }
    }
    // Every moving menu decodes several 1080p clips at once (~1 GB each): template menus have many pages.
    let menu_jobs = if motion > 0 { jobs.min(MOTION_MENU_MAX_JOBS) } else { jobs };
    run_limited(menu_jobs.min(count).max(1), encode, cancel, |e| {
        if let JobEvent::Finished { finished, total, .. } = e {
            log(Event::Progress { label: tf!(opts.ui_lang, "bd_menu_progress", done = finished, total = total), fraction: finished as f64 / total as f64 });
        }
    })
    .await?;

    // Mux every menu into the shared BD folder (clip / playlist number = menu number).
    let bdmv = work.join("BD").join("BDMV");
    crate::bd_mux::create_skeleton(&bdmv).at(&bdmv)?;
    let width = fw;
    let mut mux_log = String::new();
    for (i, page) in pages.iter().enumerate() {
        cancel.check()?;
        let sup = page.igs.as_ref().map(|_| work.join(format!("menu_{:02}.sup", i + 1)));
        let job = crate::bd_author::menu_job(work.join(format!("menu_{:02}.mkv", i + 1)), sup, i, page.intro.as_ref().map(|n| n.secs), page.still, rate, width, &bdmv, page.secs);
        mux_log.push_str(&cancellable(cancel, crate::bd_mux::mux_title(tools, &job)).await?);
    }
    std::fs::write(work.join("mux_menus.log"), mux_log).at(work)?;
    let size_mb = (0..count).map(|i| crate::dvd::file_mb(&bdmv.join(format!("STREAM/{i:05}.m2ts")))).sum();
    Ok(MenuStage { nav, count, motion, size_mb, warnings })
}

/// Builds a Blu-ray structure (`BDMV` + `CERTIFICATE`) from `opts.inputs` (see the module docs).
pub async fn build_bd(opts: &BuildOptions, log: &mut (dyn FnMut(Event) + Send)) -> Result<BuildReport, BuildError> {
    let cancel = &opts.cancel;
    let mut warnings: Vec<String> = Vec::new();
    let mut warn = |log: &mut (dyn FnMut(Event) + Send), w: String| {
        log(Event::Warning(w.clone()));
        warnings.push(w);
    };

    // Checks before any work ----------------------------------------------------------------
    if opts.inputs.is_empty() {
        return Err(BuildError::Author(AuthorError::NoTitles));
    }
    crate::dvd::check_title_count(opts)?;
    if !opts.resolution.is_hd() {
        return Err(BuildError::Template(format!(
            "Resolution {} is not possible for Blu-ray – please choose 1080p or 720p",
            opts.resolution.label()
        )));
    }
    if opts.bd_single_video && opts.inputs.len() > 1 {
        return Err(BuildError::Template(format!(
            "'Only one video per Blu-ray' is set, but {} videos are selected",
            opts.inputs.len()
        )));
    }

    // 0. Tools and directories --------------------------------------------------------------
    let tools = cancellable(cancel, Toolchain::detect()).await?;
    log(Event::Step(tf!(opts.ui_lang, "tools_bd", ffmpeg = tools.ffmpeg.display())));
    for note in &tools.notes {
        warn(log, note.clone());
    }
    std::fs::create_dir_all(&opts.output).at(&opts.output)?;
    let out = display_path(std::fs::canonicalize(&opts.output).at(&opts.output)?);
    let work = crate::dvd::work_dir(&out, opts);
    if work.join(WORK_DIR).is_file() {
        remove_old_output(&work).await?;
    } else if work.exists() {
        return Err(BuildError::Template(format!(
            "{} exists but was not created by this program – rename it or choose another output folder",
            work.display()
        )));
    }
    std::fs::create_dir_all(&work).at(&work)?;
    std::fs::write(work.join(WORK_DIR), "temporary files of musadvd, safe to delete\n").at(&work)?;

    // 1. Probe and select streams ------------------------------------------------------------
    log(Event::Phase(BuildPhase::Probe));
    let mut titles: Vec<BdTitle> = Vec::new();
    for (i, input) in opts.inputs.iter().enumerate() {
        log(Event::Step(tf!(opts.ui_lang, "analyzing", path = input.display())));
        let mut info = cancellable(cancel, probe::probe(&tools.ffprobe, input)).await?;
        if info.video.is_none() {
            return Err(BuildError::Internal(format!("{}: contains no video stream", input.display())));
        }
        let sel = opts.selections.get(i).cloned().unwrap_or_default();
        let label = sel.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
            input.file_stem().map_or_else(|| format!("Title {}", i + 1), |s| s.to_string_lossy().into_owned())
        });
        let (audio, mut subs, notes) = select_streams(&info, &sel, opts, &label);
        for note in notes {
            warn(log, note);
        }
        apply_selection(&mut info, &sel, &label)?;
        if info.video.as_ref().is_some_and(|v| v.crop.is_some()) && subs.iter().any(|&s| info.subtitles[s].kind == SubtitleKind::Bitmap) {
            warn(log, tf!(opts.ui_lang, "bd_pgs_crop_warn", label = label));
        }
        // Only SRT-like text and PGS can go on a BD here.
        subs.retain(|&s| {
            let s = &info.subtitles[s];
            let ok = s.kind == SubtitleKind::Text || matches!(s.codec.as_str(), "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" | "dvb_teletext");
            if !ok {
                warn(log, tf!(opts.ui_lang, "bd_sub_unsupported", label = label, index = s.index, codec = s.codec));
            }
            ok
        });
        let (ext_raw, ext_notes) = crate::dvd::select_external_subtitles(&sel, opts, subs.len(), &label)?;
        for note in ext_notes {
            warn(log, note);
        }
        let mut external_subtitles = Vec::with_capacity(ext_raw.len());
        for ext in ext_raw {
            let probed = probe::probe_external_subtitle(&ext.path)
                .map_err(|e| BuildError::Template(format!("{label}: {e}")))?;
            let kind = if probed.subtitle_kind() == SubtitleKind::Text { SubKind::Text } else { SubKind::Pgs };
            external_subtitles.push((ext, kind));
        }
        let natural: Vec<u32> = subs
            .iter()
            .map(|&s| info.subtitles[s].index)
            .chain((0..external_subtitles.len()).map(|i| EXTERNAL_SUB_SOURCE_BASE + i as u32))
            .collect();
        let sub_order = crate::dvd::subtitle_order(&natural, sel.subtitle_order.as_deref());
        let chapters = match &sel.chapters {
            Some(starts) => custom_chapters(starts, info.duration_secs),
            None => chapter_starts(&info, opts.chapters.interval_secs, opts.chapters.min_video_secs),
        };
        let fps = info.video.as_ref().map_or(0.0, |v| v.fps);
        let resolution = match sel.bd_resolution.as_deref().filter(|s| !s.is_empty()) {
            None => opts.resolution,
            Some("1080") => Resolution::Hd1080,
            Some("720") => Resolution::Hd720,
            Some(other) => return Err(BuildError::Template(format!("{label}: unknown Blu-ray resolution {other}"))),
        };
        let rate = match sel.bd_rate.as_deref().filter(|s| !s.is_empty()) {
            None => bd_frame_rate(&[fps], opts.tv_standard, resolution),
            Some(name) => BdRate::from_name(name, resolution).ok_or_else(|| {
                BuildError::Template(format!("{label}: {name} is not a Blu-ray frame rate at {}", resolution.label()))
            })?,
        };
        let video_mode = match sel.bd_video.as_deref() {
            Some("encode") => BdVideoMode::Encode,
            Some("copy") => BdVideoMode::Strict,
            _ => opts.bd_video,
        };
        titles.push(BdTitle {
            label,
            audio,
            subs,
            external_subtitles,
            sub_order,
            chapters: chapters.iter().map(|c| c.0).collect(),
            stem: format!("title_{:02}", i + 1),
            rate,
            resolution,
            video_mode,
            audio_mode: opts.audio_mode,
            ac3_kbps: opts.audio_bitrate_kbps,
            bd_audio: opts.bd_audio,
            track_label_mode: opts.track_label_mode,
            ui_lang: opts.ui_lang,
            video: opts.video.with_picture(sel.denoise, sel.brightness, sel.contrast, sel.saturation),
            audio_target: if opts.disc.is_avchd() { AudioTarget::Avchd } else { AudioTarget::Bluray },
            loudnorm: opts.loudnorm,
            copy_kbps: None,
            weight: crate::dvd::quality_weight(sel.bd_quality.as_deref()),
            subtitle_style: sel.subtitle_style.clone(),
            info,
        });
    }
    let groups = crate::dvd::groups(&opts.selections, titles.len());
    for g in &groups {
        let shapes: Vec<crate::dvd::MemberShape> = titles[g.clone()]
            .iter()
            .map(|t| (t.label.as_str(), t.audio.len(), t.subs.len() + t.external_subtitles.len()))
            .collect();
        crate::dvd::check_group_shapes(&shapes)?;
    }
    let tools = Arc::new(tools.with_priority(opts.process_priority));
    let heavy_jobs = opts.parallel_jobs.unwrap_or_else(default_jobs).max(1);
    let _ = available_cpus;
    let default_project_name = opts
        .menu_title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| titles[0].label.clone());

    // 2. Track layout and frame rate ---------------------------------------------------------
    let layout = harmonise(
        &titles.iter().map(|t| t.audio.iter().map(|&a| t.info.audio[a].language.clone()).collect()).collect::<Vec<_>>(),
        &titles.iter().map(|t| t.sub_tracks()).collect::<Vec<_>>(),
    );
    for t in &titles {
        let sources = t.subtitle_sources();
        for (k, &idx) in sources.iter().enumerate().take(layout.subs.len()) {
            if t.sub_for(&layout, k).is_none() {
                warn(log, tf!(opts.ui_lang, "bd_sub_kind_mismatch", label = t.label, index = idx, track = k + 1));
            }
        }
    }
    let tv = opts.tv_standard;
    let resolution = opts.resolution;
    let limits = opts.disc.limits();
    // Audio copy policy, checked before any bitrate planning or encoding.
    for t in titles.iter().filter(|t| t.bd_audio != BdAudioMode::Encode) {
        let filtered = t.loudnorm || t.info.trim.is_some();
        for (n, &i) in t.audio.iter().enumerate() {
            let a = &t.info.audio[i];
            if filtered && t.bd_audio == BdAudioMode::Auto {
                continue;
            }
            let verdict = if filtered { Err("loudness normalisation or trimming re-encodes the audio".to_string()) } else { bd_audio_copy(a, t.audio_target).map(|_| ()) };
            let Err(why) = verdict else { continue };
            if t.bd_audio == BdAudioMode::Strict {
                return Err(BuildError::Template(format!("{}: audio track {} ({}) cannot be copied – {why}", t.label, n + 1, a.codec)));
            }
            warn(log, tf!(opts.ui_lang, "bd_audio_copy_fallback", label = t.label, track = n + 1, codec = a.codec, reason = why));
        }
    }
    // The disc's video buffer: the copy check and the muxer's added timing use the same model.
    let copy_vbv = (u64::from(limits.vbv_maxrate_kbps) * 1000, u64::from(limits.vbv_bufsize_kbit) * 1000);
    for t in titles.iter_mut().filter(|t| t.video_mode != BdVideoMode::Encode) {
        let max = limits.max_video_kbps.min(limits.max_total_kbps.saturating_sub(t.audio_kbps(&layout)));
        let mut verdict = encoder::bd_video_copy(&t.info, t.rate, t.resolution, t.video, max, opts.disc.is_avchd());
        if verdict.is_ok() {
            log(Event::Step(tf!(opts.ui_lang, "bd_video_scan", label = t.label)));
            let v = t.info.video.as_ref().expect("probed file has video");
            let (fps, keyint) = (t.rate.fps, t.rate.keyint());
            let args = crate::bd_mux::scan_args(t.info.input_args(), v.index);
            let scan = cancellable(cancel, tools.ffmpeg_sink(&args, t.info.duration_secs, |_| {}, move |pipe| {
                crate::bd_mux::scan_video(std::io::BufReader::with_capacity(1 << 20, pipe), fps, copy_vbv)
            }))
            .await?;
            if let Some(why) = scan.and_then(|s| s.issue(keyint).map_or(Ok(()), Err)).err() {
                verdict = Err(why);
            }
        }
        match verdict {
            Ok(kbps) => {
                t.copy_kbps = Some(kbps);
                log(Event::Step(tf!(opts.ui_lang, "bd_video_copy", label = t.label, kbps = kbps)));
            }
            Err(why) if t.video_mode == BdVideoMode::Strict => {
                return Err(BuildError::Template(format!("{}: the video cannot be copied – {why}", t.label)));
            }
            Err(why) => warn(log, tf!(opts.ui_lang, "bd_video_copy_fallback", label = t.label, reason = why)),
        }
    }
    // Menus (and their clips) use the first title's rate, made valid for the menus' frame size.
    let menu_rate = if titles[0].resolution == resolution { titles[0].rate } else { bd_frame_rate(&[titles[0].rate.fps_f64()], tv, resolution) };
    let medium_name = if opts.disc.is_avchd() { "AVCHD" } else { "Blu-ray" };
    log(Event::Step(tf!(
        opts.ui_lang,
        "bd_summary",
        medium = medium_name,
        res = resolution.label(),
        w = resolution.frame_size(tv).0,
        h = resolution.frame_size(tv).1,
        n = titles.len(),
        audio = layout.audio_langs.len(),
        subs = layout.subs.len()
    )));
    for t in &titles {
        log(Event::Step(tf!(
            opts.ui_lang,
            "bd_title_fps",
            title = t.label,
            fps = format!("{:.3}", t.rate.fps_f64()),
            suffix = if t.rate.fake_interlaced { tr(opts.ui_lang, "bd_fake_interlaced") } else { String::new() }
        )));
    }

    // 3. Menus ---------------------------------------------------------------------------------
    cancel.check()?;
    log(Event::Phase(BuildPhase::Menus));
    let template_layout;
    let disc_layout = &layout;
    let sounded = opts.custom_layout_with_menu_audio();
    let menu_layout = match (sounded.as_deref(), opts.template.as_os_str().is_empty()) {
        (Some(layout), _) => {
            log(Event::Step(tf!(opts.ui_lang, "custom_layout", n = layout.menus.len())));
            Some(layout)
        }
        (None, false) => {
            let tpl = crate::template::Template::load(&opts.template)
                .map_err(|e| BuildError::Template(format!("Menu template {}: {e}", opts.template.display())))?;
            if tpl.pages.is_empty() {
                log(Event::Step(tr(opts.ui_lang, "bd_template_no_pages")));
                None
            } else {
                let aspect = if opts.aspect_policy == crate::dvd::AspectPolicy::Standard { Aspect::Standard } else { Aspect::Wide };
                let (built, notes) = crate::bd_template::layout_from_template(&tpl, &opts.template, &layout_titles(&titles, &groups, &layout), &vec![None; groups.len()], &chapter_samples(&titles, &groups), aspect, &opts.chapters, opts.ui_lang)
                    .map_err(|e| BuildError::Template(e.to_string()))?;
                for w in notes {
                    warn(log, w);
                }
                template_layout = built;
                log(Event::Step(tf!(opts.ui_lang, "bd_template_menus", n = template_layout.menus.len())));
                Some(&template_layout)
            }
        }
        (None, true) => None,
    };
    let menus = match menu_layout {
        Some(layout) => {
            log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Menus, None, None, None)));
            let stage = build_menus(&tools, layout, &titles, &groups, disc_layout, opts, menu_rate, &work, heavy_jobs, log).await?;
            for w in &stage.warnings {
                warn(log, w.clone());
            }
            Some(stage)
        }
        None => {
            log(Event::Eta(Eta { phase_label: tr(opts.ui_lang, "phase_bd_no_menu"), ..Eta::new(JobPhase::Menus, None, None, None) }));
            log(Event::Step(tr(opts.ui_lang, "bd_no_menu")));
            None
        }
    };
    let menu_mb = menus.as_ref().map_or(0.0, |m| m.size_mb);

    // 4. Bitrate -----------------------------------------------------------------------------
    let loads: Vec<TitleLoad> = titles
        .iter()
        .map(|t| TitleLoad { duration_secs: t.info.duration_secs, audio_kbps: t.audio_kbps(&layout), subtitle_streams: layout.subs.len(), copy_video_kbps: t.copy_kbps, copy_bytes: None, weight: t.weight })
        .collect();
    let total_secs: f64 = loads.iter().map(|l| l.duration_secs).sum();
    let avg_audio = crate::dvd::avg_audio_kbps(&loads);
    // One floor for the whole plan (the lowest of the titles' frame sizes), so the plan's size
    // estimate and every title's bitrate agree.
    let floor = titles.iter().map(|t| crate::dvd::min_video_kbps(opts.disc, t.resolution)).min().unwrap_or(opts.disc.limits().min_video_kbps);
    let plan = crate::dvd::bitrate_plan_at(opts.disc, &loads, menu_mb, opts.encoding_profile, floor)?;
    if plan.clamp == Clamp::Min {
        return Err(BuildError::DoesNotFit { disc: opts.disc, plan: Box::new(plan) });
    }
    let profile = match opts.encoding_profile {
        EncodingProfile::Auto => String::new(),
        p => tf!(opts.ui_lang, "bitrate_profile", profile = p.label(), kbps = p.max_video_kbps_for(opts.disc)),
    };
    log(Event::Step(tf!(
        opts.ui_lang,
        "bitrate",
        kbps = plan.video_kbps,
        min = format!("{:.0}", total_secs / 60.0),
        audio = avg_audio,
        menus = format!("{menu_mb:.1}"),
        target = opts.disc.target_mb(),
        profile = profile
    )));

    // 5. Encode titles -----------------------------------------------------------------------
    let count = titles.len();
    let passes = match (opts.two_pass, opts.fast_encode) {
        (true, true) => "2-Pass, schnell",
        (true, false) => "2-Pass",
        (false, true) => "1-Pass, schnell",
        (false, false) => "1-Pass",
    };
    let hw = match opts.hw_accel.bd_encoder() {
        None => None,
        Some(enc) => match tools.probe_hw_encoder(enc).await {
            Ok(()) => Some(enc),
            Err(reason) => {
                warn(log, tf!(opts.ui_lang, "bd_hw_unavailable", encoder = enc.label(), reason = reason));
                None
            }
        },
    };
    let encoders = title_encoders(hw, titles.iter().map(|t| t.rate));
    if let Some(enc) = hw {
        let nv = encoders.iter().filter(|e| **e == enc).count();
        log(Event::Step(tf!(opts.ui_lang, "bd_encoder_hw", encoder = enc.label(), nv = nv, count = count)));
    }
    let encode_jobs = if encoders.iter().all(|e| e.is_hw()) { heavy_jobs.min(HW_MAX_JOBS) } else { heavy_jobs }.min(count);
    log(Event::Step(tf!(opts.ui_lang, "bd_encode_titles", count = count, passes = passes, jobs = encode_jobs, kbps = plan.video_kbps)));
    log(Event::Phase(BuildPhase::Encoding { index: 0, count }));
    let limits = opts.disc.limits();
    let vbv = (limits.vbv_maxrate_kbps, limits.vbv_bufsize_kbit);
    let layout = Arc::new(layout);
    let titles: Vec<Arc<BdTitle>> = titles.into_iter().map(Arc::new).collect();
    let jobs: Vec<Job<TitleJob>> = titles
        .iter()
        .zip(&encoders)
        .map(|(t, &encoder)| {
            let (t, tools, work, layout) = (t.clone(), tools.clone(), work.clone(), layout.clone());
            let video_kbps = t.copy_kbps.unwrap_or_else(|| crate::dvd::weighted_video_kbps(opts.disc, plan.video_kbps, t.weight, t.audio_kbps(&layout), floor));
            let flags = (opts.two_pass, opts.fast_encode, opts.hw_decode.then_some(opts.hw_accel));
            job(move |progress| async move {
                encode_bd_title(&tools, &t, &layout, &work, video_kbps, flags, encoder, vbv, progress).await
            })
        })
        .collect();

    let weights: Vec<f64> = titles.iter().map(|t| t.info.duration_secs.max(1.0) / total_secs.max(1.0)).collect();
    let mut fractions = vec![0.0f64; count];
    let mut finished_titles = 0usize;
    let mut last_emit = std::time::Instant::now();
    let mut last_overall = -1.0f64;
    let labels: Vec<String> = titles.iter().map(|t| t.label.clone()).collect();
    let frames: Vec<f64> = titles.iter().map(|t| t.info.duration_secs * t.rate.fps_f64()).collect();
    let mut frame_eta = FrameEta::new(&frames, if opts.two_pass && encoders.contains(&BdEncoder::X264) { 2 } else { 1 });
    let expected_bytes = plan.estimated_mb * 1024.0 * 1024.0;
    let dual_layer = matches!(opts.disc, DiscType::Bd50 | DiscType::Avchd9);
    let medium = if opts.disc.is_avchd() { Medium::Dvd } else { Medium::Bd };
    let burn_estimate = Some(eta::image_and_burn_secs(medium, expected_bytes, eta::default_speed_x(medium, dual_layer), true, dual_layer));
    let authoring_estimate = eta::authoring_secs(expected_bytes);
    let mut last_eta = std::time::Instant::now() - std::time::Duration::from_secs(10);
    let encode_start = std::time::Instant::now();
    log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Encoding, Some(tf!(opts.ui_lang, "eta_video_n", n = 1, count = count)), None, burn_estimate)));
    let results = run_limited(encode_jobs, jobs, cancel, |e| {
        let finished_now = match e {
            JobEvent::Progress { index, fraction, telemetry } => {
                fractions[index] = fraction.clamp(0.0, 1.0);
                if let Some((done, fps)) = telemetry {
                    frame_eta.update(index, encode_start.elapsed().as_secs_f64(), done, fps);
                }
                None
            }
            JobEvent::Finished { index, finished, .. } => {
                fractions[index] = 1.0;
                finished_titles = finished;
                frame_eta.finish(index);
                Some(index)
            }
        };
        if finished_now.is_some() || last_eta.elapsed().as_millis() >= 1000 {
            last_eta = std::time::Instant::now();
            let detail = format!("Video {} of {count}",(finished_titles + 1).min(count));
            let remaining = frame_eta.remaining_secs().map(|s| s + authoring_estimate);
            log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Encoding, Some(detail), remaining, burn_estimate)));
        }
        let overall: f64 = fractions.iter().zip(&weights).map(|(f, w)| f * w).sum();
        if let Some(index) = finished_now {
            log(Event::Phase(BuildPhase::Encoding { index: finished_titles, count }));
            log(Event::Step(tf!(opts.ui_lang, "title_finished", done = finished_titles, count = count, title = labels[index])));
        }
        if finished_now.is_some() || (overall - last_overall >= 0.002 && last_emit.elapsed().as_millis() >= 150) {
            last_emit = std::time::Instant::now();
            last_overall = overall;
            log(Event::Progress {
                label: tf!(opts.ui_lang, "progress_encoding", done = finished_titles, count = count, jobs = encode_jobs),
                fraction: overall,
            });
        }
    })
    .await?;
    let mut reports = Vec::new();
    let mut real_durations = Vec::new();
    let mut hwaccels = Vec::new();
    let mut hwaccel_notes = Vec::new();
    for (report, hw, note, real) in results {
        reports.push(report);
        hwaccels.push(hw);
        hwaccel_notes.push(note);
        real_durations.push(real);
    }
    if let Some(hw) = hwaccels.iter().flatten().next() {
        log(Event::Step(tf!(opts.ui_lang, "gpu_decode", hw = hw)));
    } else if opts.hw_decode {
        if let Some(reason) = hwaccel_notes.into_iter().flatten().next() {
            log(Event::Step(format!("{} ({reason})", tr(opts.ui_lang, "gpu_unavailable"))));
        } else {
            log(Event::Step(tr(opts.ui_lang, "gpu_unavailable")));
        }
    }

    // 6. Mux ----------------------------------------------------------------------------------
    let project = BdProject {
        titles: titles
            .iter()
            .zip(&real_durations)
            .map(|(t, &duration_secs)| BdTitleFiles {
                mkv: format!("{}.mkv", t.stem),
                duration_secs,
                chapters: t.chapters.clone(),
                subs: (0..layout.subs.len()).map(|k| encoder::subtitle_file(&t.stem, k, SUB_EXT)).collect(),
                rate: t.rate,
                audio_kinds: t.audio_outs(&layout).iter().map(|o| o.kind()).collect(),
                resolution: t.resolution,
                copy_vbv: t.copy_kbps.map(|_| copy_vbv),
            })
            .collect(),
        audio_langs: layout.audio_langs.clone(),
        sub_tracks: layout.subs.clone(),
    };
    let menu_count = menus.as_ref().map_or(0, |m| m.count);
    log(Event::Phase(BuildPhase::Authoring));
    log(Event::Eta(Eta {
        phase_label: tr(opts.ui_lang, "phase_bd_authoring"),
        ..Eta::new(JobPhase::Authoring, None, Some(authoring_estimate), burn_estimate)
    }));
    log(Event::Step(tr(opts.ui_lang, "bd_mux")));
    cancel.check()?;
    // One mux run (playlist + clip) per title after the menus (which are already in BD/).
    let bd_root = work.join("BD");
    let bdmv_dir = bd_root.join("BDMV");
    crate::bd_mux::create_skeleton(&bdmv_dir).at(&bdmv_dir)?;
    let mut mux_log = String::new();
    for i in 0..titles.len() {
        cancel.check()?;
        let job = project.mux_job(i, menu_count + i, &work, &bdmv_dir);
        mux_log.push_str(&cancellable(cancel, crate::bd_mux::mux_title(&tools, &job)).await?);
    }
    let nav = menus.as_ref().map_or_else(|| bd_menu::plan_without_menus(titles.len()), |m| m.nav.clone());
    let avchd = opts.disc.is_avchd();
    write_navigation(&bdmv_dir, &nav, avchd).at(&work)?;
    if avchd {
        avchd_versions(&bdmv_dir).at(&bdmv_dir)?;
    }
    cancel.check()?;
    std::fs::write(work.join("mux.log"), &mux_log).at(&work)?;
    let meta = mux_log;

    let project_name =
        safe_folder_name(opts.project_name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&default_project_name));
    let project_dir: PathBuf = unique_dir(&out, &project_name);
    std::fs::create_dir_all(&project_dir).at(&project_dir)?;
    // Blu-ray players and the image expect the empty CERTIFICATE skeleton; AVCHD on DVD has none.
    let folders: &[&str] = if avchd { &["BDMV"] } else { &["BDMV", "CERTIFICATE"] };
    for folder in folders {
        let from = bd_root.join(folder);
        if from.is_dir() {
            std::fs::rename(&from, project_dir.join(folder)).at(&from)?;
        }
    }
    if !avchd {
        let backup = project_dir.join("CERTIFICATE").join("BACKUP");
        std::fs::create_dir_all(&backup).at(&backup)?;
    }
    let bdmv = project_dir.join("BDMV");
    log(Event::Step(tf!(opts.ui_lang, "bd_folder", path = bdmv.display())));

    let work_dir = if crate::dvd::keep_work(opts) {
        log(Event::Step(format!("Work folder kept: {}", work.display())));
        Some(work)
    } else {
        if let Err(e) = remove_old_output(&work).await {
            warn(log, tf!(opts.ui_lang, "workdir_delete_failed", err = e));
        }
        None
    };
    let size_mb = tree_mb(&project_dir);
    if size_mb > opts.disc.target_mb() as f64 {
        warn(log, tf!(opts.ui_lang, "size_over_bd", mb = format!("{size_mb:.0}"), target = opts.disc.target_mb()));
    }
    Ok(BuildReport {
        video_ts: bdmv,
        project_dir,
        work_dir,
        authoring_xml: meta,
        plan,
        titles: reports,
        menu_pages: menus.as_ref().map_or(0, |m| m.count),
        motion_pages: menus.as_ref().map_or(0, |m| m.motion),
        menu_sounds: Vec::new(),
        resolution,
        size_mb,
        target_mb: opts.disc.target_mb(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_encoders_per_mode() {
        let prog = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let i1080 = BdRate { fps: (25, 1), fake_interlaced: true };
        for mode in [HwAccelMode::Nvidia, HwAccelMode::Intel, HwAccelMode::Amd] {
            let enc = mode.bd_encoder().unwrap();
            assert_eq!(title_encoders(Some(enc), [i1080, prog].into_iter()), vec![BdEncoder::X264, enc]);
        }
        assert_eq!(HwAccelMode::None.bd_encoder(), None);
        assert_eq!(title_encoders(None, [i1080, prog].into_iter()), vec![BdEncoder::X264; 2]);
    }

    fn rate(f: &[f64], tv: TvStandard, res: Resolution) -> (u32, u32, bool) {
        let r = bd_frame_rate(f, tv, res);
        (r.fps.0, r.fps.1, r.fake_interlaced)
    }

    #[test]
    fn film_keeps_its_rate() {
        for res in [Resolution::Hd1080, Resolution::Hd720] {
            assert_eq!(rate(&[23.976], TvStandard::Pal, res), (24000, 1001, false));
            assert_eq!(rate(&[24000.0 / 1001.0, 23.98], TvStandard::Ntsc, res), (24000, 1001, false));
            assert_eq!(rate(&[24.0], TvStandard::Pal, res), (24, 1, false));
            assert_eq!(rate(&[24.0, 24.005], TvStandard::Pal, res), (24, 1, false));
            // Mixed film rates: 23.976 for all.
            assert_eq!(rate(&[24.0, 23.976], TvStandard::Pal, res), (24000, 1001, false));
        }
    }

    #[test]
    fn tv_rates() {
        // 1080: 25 / 29.97 flagged as interlaced; 720: doubled, progressive.
        assert_eq!(rate(&[25.0], TvStandard::Pal, Resolution::Hd1080), (25, 1, true));
        assert_eq!(rate(&[29.97], TvStandard::Ntsc, Resolution::Hd1080), (30000, 1001, true));
        assert_eq!(rate(&[25.0], TvStandard::Pal, Resolution::Hd720), (50, 1, false));
        assert_eq!(rate(&[29.97], TvStandard::Ntsc, Resolution::Hd720), (60000, 1001, false));
        // One non-film source (or a film/TV mix) turns the whole project into a TV rate.
        assert_eq!(rate(&[24.0, 25.0], TvStandard::Pal, Resolution::Hd1080), (25, 1, true));
        assert_eq!(rate(&[], TvStandard::Pal, Resolution::Hd1080), (25, 1, true));
        assert_eq!(rate(&[0.0], TvStandard::Ntsc, Resolution::Hd720), (60000, 1001, false));
    }

    #[test]
    fn rate_strings() {
        let r = |n, d| BdRate { fps: (n, d), fake_interlaced: false };
        assert_eq!(r(24000, 1001).ffmpeg(), "24000/1001");
        assert_eq!(r(25, 1).ffmpeg(), "25");
        let keys: Vec<u32> = [r(24000, 1001), r(24, 1), r(25, 1), r(30000, 1001), r(50, 1), r(60000, 1001)].iter().map(|r| r.keyint()).collect();
        assert_eq!(keys, vec![24, 24, 25, 30, 50, 60]);
    }

    #[test]
    fn tracks_are_harmonised_by_position() {
        let sub = |kind, lang: &str| BdSubTrack { kind, lang: Some(lang.into()) };
        let audio = vec![vec![Some("ger".to_string()), Some("eng".to_string())], vec![None], vec![]];
        let subs = vec![vec![sub(SubKind::Text, "ger")], vec![], vec![sub(SubKind::Pgs, "eng"), sub(SubKind::Pgs, "fre")]];
        let l = harmonise(&audio, &subs);
        // Track 0: first title's language (the second title has an untagged one).
        assert_eq!(l.audio_langs, vec![Some("ger".to_string()), Some("eng".to_string())]);
        assert_eq!(l.subs, vec![sub(SubKind::Text, "ger"), sub(SubKind::Pgs, "fre")]);
        assert_eq!(harmonise(&[], &[]), Layout { audio_langs: vec![], subs: vec![] });
        // Only the later title has a second track: its language is used.
        let l = harmonise(&[vec![Some("ger".into())], vec![None, Some("fre".into())]], &[vec![], vec![]]);
        assert_eq!(l.audio_langs, vec![Some("ger".to_string()), Some("fre".to_string())]);
    }

    #[test]
    fn external_subtitles_integrate_into_bd_planning_and_sup_prep() {
        let dir = std::env::temp_dir().join(format!("cx_bd_ext_sub_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let srt = dir.join("film.en.srt");
        std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nHello BD\n").unwrap();

        let sup = dir.join("film.de.sup");
        let sup_bytes = crate::pgs::build_sup(
            &[crate::textsub::Cue { start: 1.0, end: 2.0, lines: vec!["Hallo BD".into()] }],
            &crate::textsub::Canvas::hd(1920, 1080),
            (25, 1),
        );
        std::fs::write(&sup, &sup_bytes).unwrap();

        let info = MediaInfo {
            path: dir.join("movie.mkv"),
            duration_secs: 10.0,
            video: Some(probe::VideoStream {
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

        let mut title = BdTitle {
            info,
            label: "Film".into(),
            audio: vec![],
            subs: vec![],
            external_subtitles: vec![
                (ExternalSubtitle { path: srt, language: Some("eng".into()), title: None, forced: false }, SubKind::Text),
                (ExternalSubtitle { path: sup.clone(), language: Some("ger".into()), title: None, forced: true }, SubKind::Pgs),
            ],
            sub_order: vec![0, 1],
            chapters: vec![0.0],
            stem: "title_01".into(),
            rate: BdRate { fps: (25, 1), fake_interlaced: true },
            audio_mode: AudioMode::Auto,
            ac3_kbps: 0,
            bd_audio: BdAudioMode::Auto,
            track_label_mode: probe::TrackLabelMode::SmartMetadata,
            ui_lang: UiLang::EN,
            video: VideoProcessing::default(),
            audio_target: AudioTarget::Bluray,
            loudnorm: false,
            resolution: Resolution::Hd1080,
            video_mode: BdVideoMode::Encode,
            copy_kbps: None,
            weight: 1.0,
            subtitle_style: crate::textsub::SubtitleStyle { offset_ms: 500, ..Default::default() },
        };

        let tracks = title.sub_tracks();
        assert_eq!(tracks, vec![
            BdSubTrack { kind: SubKind::Text, lang: Some("eng".into()) },
            BdSubTrack { kind: SubKind::Pgs, lang: Some("ger".into()) },
        ]);
        assert_eq!(title.subtitle_sources(), vec![EXTERNAL_SUB_SOURCE_BASE, EXTERNAL_SUB_SOURCE_BASE + 1]);
        assert_eq!(title.subtitle_labels(), vec!["English", "German (forced)"]);

        let layout = harmonise(&[vec![]], &[tracks]);
        let tools = Toolchain {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            notes: vec![],
            priority: crate::tools::ProcessPriority::Normal,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(prepare_bd_subtitles(&tools, &title, &layout, &dir)).unwrap();

        let out_srt_sup = dir.join("title_01_s1.sup");
        let out_pgs_sup = dir.join("title_01_s2.sup");
        assert!(out_srt_sup.is_file());
        assert_eq!(&std::fs::read(&out_srt_sup).unwrap()[..2], b"PG");
        assert_eq!(std::fs::read(&out_pgs_sup).unwrap(), sup_bytes);

        // A user order puts the second file first: the disc tracks and their .sup files follow it.
        title.sub_order = vec![1, 0];
        let tracks = title.sub_tracks();
        assert_eq!(tracks[0], BdSubTrack { kind: SubKind::Pgs, lang: Some("ger".into()) });
        assert_eq!(title.subtitle_sources(), vec![EXTERNAL_SUB_SOURCE_BASE + 1, EXTERNAL_SUB_SOURCE_BASE]);
        assert_eq!(title.subtitle_labels(), vec!["German (forced)", "English"]);
        let layout = harmonise(&[vec![]], &[tracks]);
        std::fs::remove_file(&out_srt_sup).unwrap();
        std::fs::remove_file(&out_pgs_sup).unwrap();
        rt.block_on(prepare_bd_subtitles(&tools, &title, &layout, &dir)).unwrap();
        assert_eq!(std::fs::read(&out_srt_sup).unwrap(), sup_bytes);
        assert_eq!(&std::fs::read(&out_pgs_sup).unwrap()[..2], b"PG");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
