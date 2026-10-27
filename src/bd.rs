//! Blu-ray build (BD-25 / BD-50): probe -> bitrate -> H.264 + AC-3 encodes -> tsMuxeR.
//!
//! Output layout:
//! ```text
//! <output>/<project>/BDMV/         playlist, clip info, m2ts streams
//! <output>/<project>/CERTIFICATE/
//! <output>/.convertx-work/         title_NN.mkv, subtitle files, bd.meta, tsmuxer.log
//! ```
//!
//! Every title gets its own tsMuxeR run (one playlist + clip each), so each keeps its own frame
//! rate. Without menus the titles play one after the other (`bd_menu::plan_without_menus`); with
//! the menu editor's layout the title playlists follow the menu clips. AVCHD ("BD5/BD9") is the
//! same BDMV structure on DVD media with AVCHD limits. The DVD path in [`crate::dvd`] is
//! untouched; [`crate::dvd::build_dvd`] dispatches here for BD and AVCHD discs.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::author::{AuthorError, ClipSource, MenuFrame, Resolution, TvStandard, MAX_TITLES};
use crate::bd_menu::{self, BdNav};
use crate::hdmv;
use crate::bd_author::{BdProject, BdSubTrack, BdTitleFiles, SubKind, EMPTY_SRT};
use crate::calculator::{Clamp, DiscType, EncodingProfile};
use crate::dvd::{
    apply_selection, available_cpus, cancellable, chapter_starts, custom_chapters, default_jobs, display_path, job, remove_old_output,
    run_limited, safe_folder_name, select_streams, unique_dir, AtPath, BuildError, BuildOptions, BuildPhase, BuildReport,
    Event, Job, JobEvent, JobProgress, TitleLoad, TitleReport, WORK_DIR,
};
use crate::encoder::{self, plan_audio, Aspect, AudioMode, AudioOut, AudioTarget, BdEncodeJob, BdEncoder, EncodePhase, VideoProcessing, SILENCE_OUT};
use crate::eta::{self, Eta, FrameEta, JobPhase, Medium};
use crate::i18n::{tr, UiLang};
use crate::tf;
use crate::probe::{self, MediaInfo, SubtitleKind, SubtitleStream};
use crate::subpic;
use crate::tools::{ToolError, Toolchain};

/// Parallel NVENC encodes. GeForce drivers limit concurrent sessions; the CPU-side scaling is the
/// real limit anyway. shortcut: not measured yet, set after the real-GPU run.
const NVENC_MAX_JOBS: usize = 4;

/// Parallel encodes of moving menus. shortcut: not measured, raise once RAM use per menu is known.
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
    BdSubTrack { kind: if s.kind == SubtitleKind::Text { SubKind::Text } else { SubKind::Pgs }, lang: s.language.clone() }
}

fn sub_ext(kind: SubKind) -> &'static str {
    match kind {
        SubKind::Text => "srt",
        SubKind::Pgs => "sup",
    }
}

/// One input after stream selection.
struct BdTitle {
    info: MediaInfo,
    label: String,
    /// Indices into `info.audio`.
    audio: Vec<usize>,
    /// Indices into `info.subtitles` (text and PGS only).
    subs: Vec<usize>,
    chapters: Vec<f64>,
    stem: String,
    rate: BdRate,
    audio_mode: AudioMode,
    track_label_mode: probe::TrackLabelMode,
    ui_lang: UiLang,
    video: VideoProcessing,
    audio_target: AudioTarget,
    loudnorm: bool,
}

impl BdTitle {
    fn audio_streams(&self, layout: &Layout) -> Vec<Option<&probe::AudioStream>> {
        (0..layout.audio_langs.len()).map(|k| self.audio.get(k).map(|&i| &self.info.audio[i])).collect()
    }

    /// The stream playing the role of disc subtitle track `k` (same kind as the track).
    fn sub_for(&self, layout: &Layout, k: usize) -> Option<&SubtitleStream> {
        self.subs.get(k).map(|&i| &self.info.subtitles[i]).filter(|s| sub_track(s).kind == layout.subs[k].kind)
    }

    /// Output plan of every disc audio track (silent filler where this title has none).
    fn audio_outs(&self, layout: &Layout) -> Vec<AudioOut> {
        self.audio_streams(layout)
            .into_iter()
            .map(|a| a.map_or(SILENCE_OUT, |a| plan_audio(a, self.audio_target, self.audio_mode, self.loudnorm || self.info.trim.is_some())))
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
        let all = probe::subtitle_labels(&self.info, self.track_label_mode, self.ui_lang);
        self.subs.iter().map(|&i| all[i].clone()).collect()
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

/// Encodes one title: subtitle files (extracted or placeholders), then H.264 + AC-3 into
/// `<stem>.mkv`. Returns the report, the hardware decoder used and the real duration.
#[allow(clippy::too_many_arguments)]
async fn encode_bd_title(
    tools: &Toolchain,
    t: &BdTitle,
    layout: &Layout,
    work: &Path,
    video_kbps: u32,
    (two_pass, fast, hw_decode): (bool, bool, bool),
    encoder: BdEncoder,
    (maxrate_kbps, bufsize_kbit): (u32, u32),
    resolution: Resolution,
    progress: JobProgress,
) -> Result<(TitleReport, Option<&'static str>, f64), BuildError> {
    let hwaccel = if hw_decode { tools.probe_hwaccel(&t.info.path).await } else { None };
    let duration = t.info.duration_secs;

    let mut extract: Vec<(usize, &SubtitleStream)> = Vec::new();
    for (k, track) in layout.subs.iter().enumerate() {
        match t.sub_for(layout, k) {
            Some(s) => extract.push((k, s)),
            None => {
                let path = work.join(encoder::subtitle_file(&t.stem, k, sub_ext(track.kind)));
                let (w, h) = resolution.frame_size(TvStandard::Pal);
                let data = match track.kind {
                    SubKind::Text => EMPTY_SRT.as_bytes().to_vec(),
                    SubKind::Pgs => subpic::empty_pgs(w as u16, h as u16),
                };
                std::fs::write(&path, data).at(&path)?;
            }
        }
    }
    if let Some(args) = encoder::extract_subtitle_args(&t.info, &extract, work, &t.stem) {
        tools.ffmpeg(&args, duration, |_| {}).await?;
    }

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
        video: t.video,
        audio_target: t.audio_target,
        audio_filters,
        maxrate_kbps,
        bufsize_kbit,
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
        return Err(BuildError::Internal(format!("{}: ffprobe meldet keine Laufzeit", mkv.display())));
    }
    let mut labels = t.audio_labels(layout).into_iter();
    let audio = (0..layout.audio_langs.len()).map(|k| if k < t.audio.len() { labels.next().unwrap_or_default() } else { "Stille (Platzhalter)".to_string() }).collect();
    let sub_labels = t.subtitle_labels();
    let subtitles = (0..layout.subs.len())
        .map(|k| match t.sub_for(layout, k) {
            Some(_) => sub_labels[k].clone(),
            None => "leer (Platzhalter)".to_string(),
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
    Ok((report, hwaccel, real))
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

/// Writes `index.bdmv` and `MovieObject.bdmv` (over tsMuxeR's) into `BDMV` and `BDMV/BACKUP`.
fn write_navigation(bdmv: &Path, nav: &BdNav) -> std::io::Result<()> {
    let index = hdmv::index_bdmv(bd_menu::FIRST_PLAY_OBJECT, bd_menu::TOP_MENU_OBJECT, &nav.title_objects);
    let objects = hdmv::movie_object_bdmv(&nav.objects);
    for dir in [bdmv.to_path_buf(), bdmv.join("BACKUP")] {
        std::fs::write(dir.join("index.bdmv"), &index)?;
        std::fs::write(dir.join("MovieObject.bdmv"), &objects)?;
    }
    Ok(())
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
                audio_sources: t.audio.iter().map(|&a| t.info.audio[a].index as u32).collect(),
                subtitle_sources: t.subs.iter().map(|&s| t.info.subtitles[s].index as u32).collect(),
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
    let nav = bd_menu::plan(layout, &layout_titles, &traits, opts.playback, &members);
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
    for (i, page) in pages.iter().enumerate() {
        cancel.check()?;
        let sup = page.igs.as_ref().map(|_| format!("menu_{:02}.sup", i + 1));
        let meta = format!("bd_menu_{:02}.meta", i + 1);
        std::fs::write(work.join(&meta), crate::bd_author::menu_meta_text(&format!("menu_{:02}.mkv", i + 1), sup.as_deref(), i, page.intro.as_ref().map(|n| n.secs))).at(work)?;
        let out = cancellable(cancel, tools.tsmuxer(work, &meta, "BD")).await?;
        std::fs::write(work.join(format!("{meta}.log")), out).at(work)?;
        let name = format!("{i:05}");
        crate::bd_patch::patch_menu(&bdmv, &name, &name, !page.moving, page.igs.is_some()).at(&bdmv)?;
    }
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
    if opts.inputs.len() > MAX_TITLES {
        return Err(BuildError::Author(AuthorError::TooManyTitles(opts.inputs.len())));
    }
    if !opts.resolution.is_hd() {
        return Err(BuildError::Template(format!(
            "Für Blu-ray ist die Auflösung {} nicht möglich – bitte 1080p oder 720p wählen",
            opts.resolution.label()
        )));
    }
    if opts.bd_single_video && opts.inputs.len() > 1 {
        return Err(BuildError::Template(format!(
            "„Nur ein Video pro Blu-ray“ ist eingestellt, es sind aber {} Videos ausgewählt",
            opts.inputs.len()
        )));
    }

    // 0. Tools and directories --------------------------------------------------------------
    let tools = cancellable(cancel, Toolchain::detect()).await?;
    if tools.tsmuxer.is_none() {
        return Err(BuildError::Tool(ToolError::NotFound(
            "tsMuxeR nicht gefunden – für Blu-ray wird bin/tsMuxeR.exe benötigt (neben der Anwendung oder im PATH)".into(),
        )));
    }
    log(Event::Step(tf!(
        opts.ui_lang,
        "tools_bd",
        ffmpeg = tools.ffmpeg.display(),
        tsmuxer = tools.tsmuxer.as_ref().map_or_else(String::new, |p| p.display().to_string())
    )));
    for note in &tools.notes {
        warn(log, note.clone());
    }
    std::fs::create_dir_all(&opts.output).at(&opts.output)?;
    let out = display_path(std::fs::canonicalize(&opts.output).at(&opts.output)?);
    let work = out.join(WORK_DIR);
    if work.join(WORK_DIR).is_file() {
        remove_old_output(&work).await?;
    } else if work.exists() {
        return Err(BuildError::Template(format!(
            "{} existiert, stammt aber nicht von diesem Programm – bitte umbenennen oder ein anderes Ausgabeverzeichnis wählen",
            work.display()
        )));
    }
    std::fs::create_dir_all(&work).at(&work)?;
    std::fs::write(work.join(WORK_DIR), "temporary files of convertx-rust, safe to delete\n").at(&work)?;

    // 1. Probe and select streams ------------------------------------------------------------
    log(Event::Phase(BuildPhase::Probe));
    let mut titles: Vec<BdTitle> = Vec::new();
    for (i, input) in opts.inputs.iter().enumerate() {
        log(Event::Step(tf!(opts.ui_lang, "analyzing", path = input.display())));
        let mut info = cancellable(cancel, probe::probe(&tools.ffprobe, input)).await?;
        if info.video.is_none() {
            return Err(BuildError::Internal(format!("{}: enthält keine Videospur", input.display())));
        }
        let sel = opts.selections.get(i).cloned().unwrap_or_default();
        let label = sel.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
            input.file_stem().map_or_else(|| format!("Titel {}", i + 1), |s| s.to_string_lossy().into_owned())
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
            let ok = s.kind == SubtitleKind::Text || s.codec == "hdmv_pgs_subtitle";
            if !ok {
                warn(log, tf!(opts.ui_lang, "bd_sub_unsupported", label = label, index = s.index, codec = s.codec));
            }
            ok
        });
        let chapters = match &sel.chapters {
            Some(starts) => custom_chapters(starts, info.duration_secs),
            None => chapter_starts(&info, opts.chapters.interval_secs, opts.chapters.min_video_secs),
        };
        let fps = info.video.as_ref().map_or(0.0, |v| v.fps);
        titles.push(BdTitle {
            label,
            audio,
            subs,
            chapters: chapters.iter().map(|c| c.0).collect(),
            stem: format!("title_{:02}", i + 1),
            rate: bd_frame_rate(&[fps], opts.tv_standard, opts.resolution),
            audio_mode: opts.audio_mode,
            track_label_mode: opts.track_label_mode,
            ui_lang: opts.ui_lang,
            video: opts.video,
            audio_target: if opts.disc.is_avchd() { AudioTarget::Avchd } else { AudioTarget::Bluray },
            loudnorm: opts.loudnorm,
            info,
        });
    }
    let groups = crate::dvd::groups(&opts.selections, titles.len());
    for g in &groups {
        let shapes: Vec<crate::dvd::MemberShape> = titles[g.clone()].iter().map(|t| (t.label.as_str(), t.audio.len(), t.subs.len())).collect();
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
        &titles.iter().map(|t| t.subs.iter().map(|&s| sub_track(&t.info.subtitles[s])).collect()).collect::<Vec<_>>(),
    );
    for t in &titles {
        for (k, &s) in t.subs.iter().enumerate() {
            if k < layout.subs.len() && t.sub_for(&layout, k).is_none() {
                warn(log, tf!(opts.ui_lang, "bd_sub_kind_mismatch", label = t.label, index = t.info.subtitles[s].index, track = k + 1));
            }
        }
    }
    let tv = opts.tv_standard;
    let resolution = opts.resolution;
    // Menus (and their clips) use the first title's rate.
    let menu_rate = titles[0].rate;
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
            suffix = if t.rate.fake_interlaced { tr(opts.ui_lang, "bd_fake_interlaced") } else { "" }
        )));
    }

    // 3. Menus ---------------------------------------------------------------------------------
    cancel.check()?;
    log(Event::Phase(BuildPhase::Menus));
    let template_layout;
    let disc_layout = &layout;
    let menu_layout = match (&opts.custom_layout, opts.template.as_os_str().is_empty()) {
        (Some(layout), _) => {
            log(Event::Step(tf!(opts.ui_lang, "custom_layout", n = layout.menus.len())));
            Some(layout)
        }
        (None, false) => {
            let tpl = crate::template::Template::load(&opts.template)
                .map_err(|e| BuildError::Template(format!("Menü-Vorlage {}: {e}", opts.template.display())))?;
            if tpl.pages.is_empty() {
                log(Event::Step(tr(opts.ui_lang, "bd_template_no_pages").into()));
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
            let stage = build_menus(&tools, layout, &titles, &groups, &disc_layout, opts, menu_rate, &work, heavy_jobs, log).await?;
            for w in &stage.warnings {
                warn(log, w.clone());
            }
            Some(stage)
        }
        None => {
            log(Event::Eta(Eta { phase_label: tr(opts.ui_lang, "phase_bd_no_menu").into(), ..Eta::new(JobPhase::Menus, None, None, None) }));
            log(Event::Step(tr(opts.ui_lang, "bd_no_menu").into()));
            None
        }
    };
    let menu_mb = menus.as_ref().map_or(0.0, |m| m.size_mb);

    // 4. Bitrate -----------------------------------------------------------------------------
    let loads: Vec<TitleLoad> = titles
        .iter()
        .map(|t| TitleLoad { duration_secs: t.info.duration_secs, audio_kbps: t.audio_kbps(&layout), subtitle_streams: layout.subs.len() })
        .collect();
    let total_secs: f64 = loads.iter().map(|l| l.duration_secs).sum();
    let avg_audio = crate::dvd::avg_audio_kbps(&loads);
    let plan = crate::dvd::bitrate_plan(opts.disc, &loads, menu_mb, opts.encoding_profile)?;
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
    // NVENC cannot flag 1080i, so titles with fake-interlaced rates use x264.
    let nvenc_ok = if !opts.bd_hw_encode {
        false
    } else {
        match tools.probe_nvenc().await {
            Ok(()) => true,
            Err(reason) => {
                warn(log, tf!(opts.ui_lang, "bd_nvenc_unavailable", reason = reason));
                false
            }
        }
    };
    let encoders: Vec<BdEncoder> =
        titles.iter().map(|t| if nvenc_ok && !t.rate.fake_interlaced { BdEncoder::Nvenc } else { BdEncoder::X264 }).collect();
    if nvenc_ok {
        let nv = encoders.iter().filter(|e| **e == BdEncoder::Nvenc).count();
        log(Event::Step(tf!(opts.ui_lang, "bd_encoder_nvenc", nv = nv, count = count)));
    }
    let encode_jobs = if encoders.iter().all(|e| *e == BdEncoder::Nvenc) { heavy_jobs.min(NVENC_MAX_JOBS) } else { heavy_jobs }.min(count);
    log(Event::Step(tf!(opts.ui_lang, "bd_encode_titles", count = count, passes = passes, jobs = encode_jobs, kbps = plan.video_kbps)));
    log(Event::Phase(BuildPhase::Encoding { index: 0, count }));
    let limits = opts.disc.limits();
    let max_total = limits.max_total_kbps;
    let vbv = (limits.vbv_maxrate_kbps, limits.vbv_bufsize_kbit);
    let layout = Arc::new(layout);
    let titles: Vec<Arc<BdTitle>> = titles.into_iter().map(Arc::new).collect();
    let jobs: Vec<Job<(TitleReport, Option<&'static str>, f64)>> = titles
        .iter()
        .zip(&encoders)
        .map(|(t, &encoder)| {
            let (t, tools, work, layout) = (t.clone(), tools.clone(), work.clone(), layout.clone());
            let video_kbps = plan.video_kbps.min(max_total.saturating_sub(t.audio_kbps(&layout)));
            let flags = (opts.two_pass, opts.fast_encode, opts.hw_decode);
            job(move |progress| async move {
                encode_bd_title(&tools, &t, &layout, &work, video_kbps, flags, encoder, vbv, resolution, progress).await
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
            let detail = format!("Video {} von {count}", (finished_titles + 1).min(count));
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
    for (report, hw, real) in results {
        reports.push(report);
        hwaccels.push(hw);
        real_durations.push(real);
    }
    if let Some(hw) = hwaccels.iter().flatten().next() {
        log(Event::Step(tf!(opts.ui_lang, "gpu_decode", hw = hw)));
    } else if opts.hw_decode {
        log(Event::Step(tr(opts.ui_lang, "gpu_unavailable").into()));
    }

    // 6. tsMuxeR ------------------------------------------------------------------------------
    let project = BdProject {
        titles: titles
            .iter()
            .zip(&real_durations)
            .map(|(t, &duration_secs)| BdTitleFiles {
                mkv: format!("{}.mkv", t.stem),
                duration_secs,
                chapters: t.chapters.clone(),
                subs: layout.subs.iter().enumerate().map(|(k, s)| encoder::subtitle_file(&t.stem, k, sub_ext(s.kind))).collect(),
                rate: t.rate,
                audio_kinds: t.audio_outs(&layout).iter().map(|o| o.kind()).collect(),
            })
            .collect(),
        audio_langs: layout.audio_langs.clone(),
        sub_tracks: layout.subs.clone(),
        resolution,
    };
    let menu_count = menus.as_ref().map_or(0, |m| m.count);
    let mut meta = String::new();
    log(Event::Phase(BuildPhase::Authoring));
    log(Event::Eta(Eta {
        phase_label: tr(opts.ui_lang, "phase_bd_authoring").into(),
        ..Eta::new(JobPhase::Authoring, None, Some(authoring_estimate), burn_estimate)
    }));
    log(Event::Step(tr(opts.ui_lang, "bd_tsmuxer").into()));
    cancel.check()?;
    // One tsMuxeR run (playlist + clip) per title after the menus (which are already in BD/).
    let mut mux_log = String::new();
    for i in 0..titles.len() {
        cancel.check()?;
        let name = format!("bd_title_{:02}.meta", i + 1);
        let text = project.title_meta_text(i, menu_count + i);
        std::fs::write(work.join(&name), &text).at(&work)?;
        meta.push_str(&text);
        mux_log.push_str(&cancellable(cancel, tools.tsmuxer(&work, &name, "BD")).await?);
    }
    let nav = menus.as_ref().map_or_else(|| bd_menu::plan_without_menus(titles.len()), |m| m.nav.clone());
    write_navigation(&work.join("BD").join("BDMV"), &nav).at(&work)?;
    cancel.check()?;
    std::fs::write(work.join("tsmuxer.log"), &mux_log).at(&work)?;
    let bd_root = work.join("BD");
    if !bd_root.join("BDMV").join("index.bdmv").is_file() {
        let tail: Vec<&str> = mux_log.lines().rev().take(15).collect();
        return Err(BuildError::Internal(format!(
            "tsMuxeR hat keinen BDMV-Ordner erzeugt:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        )));
    }

    let project_name =
        safe_folder_name(opts.project_name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&default_project_name));
    let project_dir: PathBuf = unique_dir(&out, &project_name);
    std::fs::create_dir_all(&project_dir).at(&project_dir)?;
    for folder in ["BDMV", "CERTIFICATE"] {
        let from = bd_root.join(folder);
        if from.is_dir() {
            std::fs::rename(&from, project_dir.join(folder)).at(&from)?;
        }
    }
    // tsMuxeR 2.7 writes no CERTIFICATE folder; players and the image expect the empty skeleton.
    let backup = project_dir.join("CERTIFICATE").join("BACKUP");
    std::fs::create_dir_all(&backup).at(&backup)?;
    let bdmv = project_dir.join("BDMV");
    log(Event::Step(tf!(opts.ui_lang, "bd_folder", path = bdmv.display())));

    let work_dir = if opts.keep_work {
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
}
