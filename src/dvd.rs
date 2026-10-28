//! End-to-end DVD build: probe -> plan -> thumbnails -> menus -> bitrate -> encode -> author.
//!
//! Output layout:
//! ```text
//! <output>/<project>/VIDEO_TS/  finished DVD-Video structure ("<project> (2)" if taken)
//! <output>/<project>.convertx-work/  menu PNGs, spumux/dvdauthor XMLs, title MPEGs, logs;
//!                               removed after the build unless `keep_work`
//! ```

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use regex::Regex;
use serde::Serialize;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use crate::author::{
    self, chapter_mark, AuthorError, ChapterEntry, ClipSource, PlaybackOptions, Resolution, TvStandard, DvdLayout, MenuKind, MenuLinks, MenuSpec, TitleEntry,
    TitleSpec, TitlesetSpec, VobSpec, MAX_CHAPTERS, MAX_TITLES,
};
use crate::calculator::{self, BitratePlan, CalcError, Clamp, DiscType, EncodingProfile, DVD_MAX_TOTAL_KBPS, LP_HALF_D1_BELOW_KBPS};
use crate::encoder::{self, plan_audio, Aspect, AudioMode, AudioOut, AudioTarget, EncodeJob, EncodePhase, MenuAudio, VideoProcessing};
use crate::menu_anim;
use crate::config::HwAccelMode;
use crate::i18n::tr;
use crate::tf;
use crate::menu::{self, page_by_style, MenuError, MenuPageFiles, RenderOptions, RenderedMenu, TextOverflow, UiLang};
use crate::probe::{self, language_matches, MediaInfo, ProbeError, SubtitleKind, TrackLabelMode};
use crate::template::{Page, Template};
use crate::tools::{ProcessPriority, RawStream, ToolError, Toolchain};

/// DVD-Video limits per title.
pub const MAX_AUDIO_STREAMS: usize = 8;
pub const MAX_SUBTITLE_STREAMS: usize = 32;
/// Bitrate reserved per text subtitle stream for the size calculation.
const SUBTITLE_KBPS: u32 = 10;
use crate::eta::{self, Eta, FrameEta, JobPhase};

#[derive(Debug)]
pub enum BuildError {
    Io(std::io::Error),
    File { path: PathBuf, error: std::io::Error },
    Tool(ToolError),
    Probe(ProbeError),
    Menu(MenuError),
    Author(AuthorError),
    Calc(CalcError),
    Template(String),
    Internal(String),
    /// The build was cancelled ([`CancelToken::cancel`]).
    Cancelled,
    DoesNotFit { disc: DiscType, plan: Box<BitratePlan> },
    /// A menu's buttons cannot be split into colour groups; the text is localised in [`BuildError::message`].
    MenuTooComplex { buttons: usize },
}

impl BuildError {
    /// User-facing text in the UI language; errors without a locale key fall back to `Display`.
    pub fn message(&self, ui: UiLang) -> String {
        match self {
            BuildError::MenuTooComplex { buttons } => tf!(ui, "err_menu_too_complex", buttons = buttons),
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::MenuTooComplex { .. } => write!(f, "{}", self.message(UiLang::EN)),
            BuildError::Io(e) => write!(f, "I/O error: {e}"),
            BuildError::File { path, error } => write!(f, "{}: {error}", path.display()),
            BuildError::Tool(e) => write!(f, "{e}"),
            BuildError::Probe(e) => write!(f, "{e}"),
            BuildError::Menu(e) => write!(f, "{e}"),
            BuildError::Author(e) => write!(f, "{e}"),
            BuildError::Calc(e) => write!(f, "{e}"),
            BuildError::Template(e) => write!(f, "{e}"),
            BuildError::Internal(e) => write!(f, "{e}"),
            BuildError::Cancelled => write!(f, "Konvertierung abgebrochen"),
            BuildError::DoesNotFit { disc, plan } => write!(
                f,
                "content does not fit on {disc:?}: even at the minimum of {} kbps it needs {:.0} MB of {} MB \
                 (would need {:.0} kbps). Use {}, fewer audio tracks or shorter inputs.",
                disc.limits().min_video_kbps,
                plan.estimated_mb,
                disc.target_mb(),
                plan.raw_video_kbps,
                match disc {
                    DiscType::Dvd5 => "a DVD9",
                    DiscType::Bd25 => "a BD-50",
                    DiscType::Avchd5 => "an AVCHD DVD-9",
                    _ => "a larger disc",
                }
            ),
        }
    }
}

impl std::error::Error for BuildError {}

/// Attaches the affected path to I/O errors.
pub(crate) trait AtPath<T> {
    fn at(self, path: &Path) -> Result<T, BuildError>;
}

impl<T> AtPath<T> for std::io::Result<T> {
    fn at(self, path: &Path) -> Result<T, BuildError> {
        self.map_err(|error| BuildError::File { path: path.to_path_buf(), error })
    }
}

/// Cancels a running build from another thread or task (GUI "Abbrechen").
///
/// Cheap to clone; all clones share one flag. Cancelling aborts the running jobs (their
/// ffmpeg/spumux/dvdauthor processes are killed), waits until they are gone and removes the
/// work directory; the build then returns [`BuildError::Cancelled`].
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CancelToken").field(&self.is_cancelled()).finish()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Completes once [`cancel`](Self::cancel) was called (immediately if it already was).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before checking the flag, so a cancel in between is not missed.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn check(&self) -> Result<(), BuildError> {
        if self.is_cancelled() {
            Err(BuildError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Runs `fut`, giving up as soon as `cancel` fires. Dropping `fut` kills its child process
/// (all tools are started with `kill_on_drop`).
pub(crate) async fn cancellable<T, E: Into<BuildError>>(
    cancel: &CancelToken,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, BuildError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(BuildError::Cancelled),
        r = fut => r.map_err(Into::into),
    }
}

/// Name of the temporary work directory inside the output directory, and of the marker file in
/// it that identifies a work directory created by this program (only those are deleted).
pub const WORK_DIR: &str = ".convertx-work";

/// Turns a project name into a valid Windows folder name: invalid characters become `_`,
/// trailing dots/spaces are removed, reserved device names get a suffix, length is limited.
pub fn safe_folder_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    let mut cleaned: String = cleaned.trim().chars().take(100).collect();
    while cleaned.ends_with(['.', ' ']) {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        return "DVD".to_string();
    }
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        cleaned.push('_');
    }
    cleaned
}

/// Temporary authoring directory `<out>/<project name>.convertx-work`. It is created before
/// probing, so the name comes from `opts` (same precedence as the default project folder name).
/// Distinct names let two projects build into one output directory in parallel.
pub fn work_dir(out: &Path, opts: &BuildOptions) -> PathBuf {
    let non_blank = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    let name = non_blank(&opts.project_name)
        .or_else(|| non_blank(&opts.menu_title))
        .or_else(|| opts.selections.first().and_then(|s| non_blank(&s.label)))
        .or_else(|| opts.inputs.first().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    out.join(format!("{}{WORK_DIR}", safe_folder_name(&name)))
}

/// `<parent>/<name>`, or `<name> (2)`, `<name> (3)`, ... if that already exists: an earlier
/// DVD is never overwritten.
pub fn unique_dir(parent: &Path, name: &str) -> PathBuf {
    let first = parent.join(name);
    if !first.exists() {
        return first;
    }
    (2..1000)
        .map(|n| parent.join(format!("{name} ({n})")))
        .find(|p| !p.exists())
        .unwrap_or_else(|| parent.join(format!("{name} ({})", std::process::id())))
}

/// Deletes the previous DVD output. Retries briefly: right after a build, virus scanners or the
/// Windows Explorer may still hold the freshly written VOBs open (os error 32).
pub(crate) async fn remove_old_output(dir: &Path) -> Result<(), BuildError> {
    let mut attempt = 0;
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt < 10 && e.raw_os_error() == Some(32) => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) if e.raw_os_error() == Some(32) => {
                return Err(BuildError::Template(format!(
                    "{}: die alte DVD ist noch in einem anderen Programm geöffnet (z. B. VLC oder Explorer). \
                     Bitte schließen oder ein anderes --output wählen.",
                    dir.display()
                )))
            }
            Err(e) => return Err(BuildError::File { path: dir.to_path_buf(), error: e }),
        }
    }
}

macro_rules! from_err {
    ($($t:ty => $v:ident),*) => {$(
        impl From<$t> for BuildError {
            fn from(e: $t) -> Self { BuildError::$v(e) }
        }
    )*};
}
from_err!(std::io::Error => Io, ProbeError => Probe, MenuError => Menu, AuthorError => Author, CalcError => Calc);

impl From<ToolError> for BuildError {
    fn from(e: ToolError) -> Self {
        match e {
            ToolError::MenuTooComplex { buttons } => BuildError::MenuTooComplex { buttons },
            e => BuildError::Tool(e),
        }
    }
}

/// DVD aspect ratio of the titles ("Seitenverhältnis").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AspectPolicy {
    /// 16:9 for wide sources, 4:3 otherwise.
    #[default]
    Auto,
    /// Always 4:3 (wide sources get letterbox bars).
    Standard,
    /// Always 16:9 (narrow sources get pillarbox bars).
    Wide,
}

impl AspectPolicy {
    pub fn aspect_for(self, display_aspect: f64) -> Aspect {
        match self {
            AspectPolicy::Auto => Aspect::for_source(display_aspect),
            AspectPolicy::Standard => Aspect::Standard,
            AspectPolicy::Wide => Aspect::Wide,
        }
    }
}

/// Automatic chapters for inputs without their own chapter marks.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterSettings {
    /// Chapter every N seconds; 0 = no automatic chapters.
    pub interval_secs: f64,
    /// Shorter videos get no automatic chapters.
    pub min_video_secs: f64,
    /// Name of chapters without a title; `<c>` = chapter number. `None` = "Kapitel N".
    pub label_template: Option<String>,
}

impl Default for ChapterSettings {
    fn default() -> Self {
        ChapterSettings { interval_secs: 300.0, min_video_secs: 0.0, label_template: None }
    }
}

impl ChapterSettings {
    /// Menu label of chapter `n` (1-based) without its own title.
    pub fn label(&self, n: usize, ui: UiLang) -> String {
        match &self.label_template {
            Some(t) if t.contains("<c>") => t.replace("<c>", &n.to_string()),
            _ => format!("{} {n}", tr(ui, "chapter")),
        }
    }
}

/// Content larger than this needs a dual-layer disc (2,295,104 sectors).
const SINGLE_LAYER_BYTES: f64 = 2_295_104.0 * 2048.0;

#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    pub inputs: Vec<PathBuf>,
    pub disc: DiscType,
    /// Template .ini; missing sub menu pages are taken from sibling templates in its folder.
    pub template: PathBuf,
    pub output: PathBuf,
    /// Audio languages to keep (`de`, `ger`, ...); empty = all tracks.
    pub audio_langs: Vec<String>,
    /// Subtitle languages to keep; empty = all.
    pub subtitle_langs: Vec<String>,
    pub no_subtitles: bool,
    /// Automatic chapters for inputs without chapters.
    pub chapters: ChapterSettings,
    /// Heading of the main menu.
    pub menu_title: Option<String>,
    pub ui_lang: UiLang,
    /// Keep the work directory (menu PNGs, XMLs, logs, title MPEGs).
    pub keep_work: bool,
    /// Decode the sources on the GPU when a working hardware decoder is found (falls back to
    /// the CPU otherwise).
    pub hw_decode: bool,
    /// GPU vendor: Blu-ray H.264 hardware encoder when it works (1080i rates and failures use
    /// x264) and, with `hw_decode`, the order of the GPU decoders tried.
    pub hw_accel: HwAccelMode,
    /// Play the intro animations of a ConvertX template's menu pages (off = the
    /// finished pages straight away, as without animation data).
    pub menu_animations: bool,
    /// 2-pass video encoding (ConvertX's "2-pass encoding" checkbox): slower, more exact size.
    pub two_pass: bool,
    /// Draft encoding without trellis quantisation: about 40 % faster, slightly lower quality.
    pub fast_encode: bool,
    /// Motion menus: pages with video slots become looping clips instead of stills.
    pub motion_menu: bool,
    /// Motion menu length; `None` = template `main_duration` (25 s).
    pub motion_duration: Option<f64>,
    /// Motion menu sound.
    pub motion_audio: MenuAudio,
    /// Per-input stream choices (index = input index); missing entries use the language filters.
    pub selections: Vec<StreamSelection>,
    /// Concurrent title encodes; `None` = half the logical CPUs ([`default_jobs`]).
    pub parallel_jobs: Option<usize>,
    /// Start behaviour and what happens after a title ("Menü Optionen").
    pub playback: PlaybackOptions,
    /// No per-title root menu: the title's entry menu is its chapter menu.
    pub skip_root_menu: bool,
    /// A disc with a single video gets no main (title) menu.
    pub no_title_menu_for_single_title: bool,
    pub text_overflow: TextOverflow,
    pub aspect_policy: AspectPolicy,
    pub process_priority: ProcessPriority,
    /// How audio tracks are written (copy compliant AC-3/DTS, always AC-3, LPCM).
    pub audio_mode: AudioMode,
    /// How audio/subtitle tracks are named on menu buttons and in reports.
    pub track_label_mode: TrackLabelMode,
    /// Scaling method, filter and deinterlacing ("Videobearbeitung").
    pub video: VideoProcessing,
    /// EBU R128 loudness normalisation of every audio track (forces a re-encode).
    pub loudnorm: bool,
    /// Tracks in this language become the first (default) audio stream.
    pub preferred_audio_lang: Option<String>,
    /// Subtitles in this language become the first subtitle stream.
    pub preferred_subtitle_lang: Option<String>,
    /// Show the first subtitle stream when a title starts.
    pub force_first_subtitle: bool,
    /// PAL (720x576, 25 fps) or NTSC (720x480, 29.97 fps) for titles and menus.
    pub tv_standard: TvStandard,
    /// Frame size of the titles (menus stay Full D1); Half D1/SIF titles are always 4:3.
    pub resolution: Resolution,
    /// Upper limit of the video bitrate; LP may switch Full D1 to Half D1 (see
    /// [`effective_resolution`]).
    pub encoding_profile: EncodingProfile,
    /// Name of the output folder (`<output>/<name>/VIDEO_TS`); `None` = menu heading, which in
    /// turn defaults to the first video's name.
    pub project_name: Option<String>,
    /// Blu-ray: reject projects with more than one video (no playlist).
    pub bd_single_video: bool,
    /// Lets another thread cancel the build.
    pub cancel: CancelToken,
    /// Menus from the menu editor instead of the ConvertX template (`template` is unused then).
    pub custom_layout: Option<crate::layout::CustomLayout>,
}

/// Result of the menu stage, whichever way the menus were made.
struct MenuStage {
    /// Visible pages to encode.
    pages: Vec<(MenuSpec, MenuPageFiles)>,
    vmgm_menus: Vec<MenuSpec>,
    /// Per title: its VTSM menu PGCs.
    ts_menus: Vec<Vec<MenuSpec>>,
    /// Per title: the VMGM menu playback returns to.
    main_pages: Vec<u32>,
    title_pre: Option<String>,
    /// Log line about the menu sound (motion menus only).
    sound_note: Option<String>,
}

/// Explicit choices for one input, e.g. from the GUI's track checkboxes.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSelection {
    /// ffprobe stream indices of the audio tracks to keep (`None` = language filter).
    pub audio: Option<Vec<u32>>,
    /// ffprobe stream indices of the subtitles to keep (`None` = language filter).
    pub subtitles: Option<Vec<u32>>,
    /// Menu label instead of the file name.
    pub label: Option<String>,
    /// Chapter starts in seconds (of the trimmed title) instead of the source chapters / grid.
    pub chapters: Option<Vec<f64>>,
    /// Start / end of the used section in seconds of the source (`None` = file start / end).
    pub start_secs: Option<f64>,
    pub end_secs: Option<f64>,
    /// Picture edges to cut away (source pixels).
    pub crop: Option<probe::Crop>,
    /// Play this input together with the previous one behind a single menu entry.
    #[serde(default)]
    pub merge_with_previous: bool,
    /// Own names of audio tracks by ffprobe stream index (replace the generated label).
    #[serde(default)]
    pub audio_names: std::collections::HashMap<u32, String>,
}

/// Index ranges of the menu entries: an input plus all directly following inputs that are merged
/// with their predecessor. The flag of the first input is ignored.
pub fn groups(selections: &[StreamSelection], n: usize) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for i in 0..n {
        let merged = i > 0 && selections.get(i).is_some_and(|s| s.merge_with_previous);
        match out.last_mut() {
            Some(last) if merged => last.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// One merged member: label, audio track count, subtitle track count.
pub(crate) type MemberShape<'a> = (&'a str, usize, usize);

/// Merged files play as one title, so they need the same audio and subtitle track layout.
pub(crate) fn check_group_shapes(members: &[MemberShape]) -> Result<(), BuildError> {
    let Some(&(first, audio, subs)) = members.first() else { return Ok(()) };
    for &(label, a, s) in &members[1..] {
        if a != audio {
            return Err(BuildError::Template(format!(
                "{label} lässt sich nicht mit {first} zusammenführen: {a} statt {audio} Tonspuren (Spuren in den Titeleinstellungen angleichen)"
            )));
        }
        if s != subs {
            return Err(BuildError::Template(format!(
                "{label} lässt sich nicht mit {first} zusammenführen: {s} statt {subs} Untertitelspuren (Spuren in den Titeleinstellungen angleichen)"
            )));
        }
    }
    Ok(())
}

/// Makes the members of a merged DVD title compatible: all use the first member's aspect and,
/// if the audio formats differ at some track, all are re-encoded to AC-3.
fn harmonize_group(members: &mut [TitlePlan]) {
    let Some(first) = members.first() else { return };
    let formats = |m: &TitlePlan| m.audio_outs().iter().map(|o| o.dvdauthor_format()).collect::<Vec<_>>();
    let (aspect, first_formats) = (first.aspect, formats(first));
    let same_audio = members.iter().all(|m| formats(m) == first_formats);
    for m in members.iter_mut() {
        m.aspect = aspect;
        if !same_audio {
            m.audio_mode = AudioMode::Ac3;
        }
    }
}

/// Coarse build stages, for progress displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum BuildPhase {
    Probe,
    Thumbnails,
    Menus,
    MenuEncoding,
    /// Encoding: `index` of `count` titles are finished (titles are encoded in parallel).
    Encoding { index: usize, count: usize },
    Authoring,
}

/// Progress events for the caller's UI.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum Event {
    Phase(BuildPhase),
    Step(String),
    Progress { label: String, fraction: f64 },
    Warning(String),
    /// Remaining time of the whole job.
    Eta(Eta),
}

/// Disc load of one title for the bitrate calculation.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleLoad {
    pub duration_secs: f64,
    /// Sum of the AC-3 bitrates of the selected tracks.
    pub audio_kbps: u32,
    pub subtitle_streams: usize,
}

/// Video bitrate for `titles` on `disc`, with `menu_mb` of menus already on the disc, limited by
/// `profile`. Used by the build itself and by the GUI's live estimate.
pub fn bitrate_plan(disc: DiscType, titles: &[TitleLoad], menu_mb: f64, profile: EncodingProfile) -> Result<BitratePlan, CalcError> {
    let total_secs: f64 = titles.iter().map(|t| t.duration_secs).sum();
    let subs_kbit: f64 =
        titles.iter().map(|t| (t.subtitle_streams as u32 * SUBTITLE_KBPS) as f64 * t.duration_secs).sum();
    let overhead = disc.default_mux_overhead_kbit() + menu_mb * 8192.0 + subs_kbit;
    calculator::calculate_capped(disc, total_secs, avg_audio_kbps(titles), overhead, profile.max_video_kbps_for(disc))
}

/// Title frame size actually used: the chosen one, except that LP turns Full D1 into Half D1
/// when the video bitrate is below [`LP_HALF_D1_BELOW_KBPS`] (sharper than a starved Full D1).
pub fn effective_resolution(chosen: Resolution, profile: EncodingProfile, video_kbps: u32) -> Resolution {
    if chosen == Resolution::FullD1 && profile == EncodingProfile::Lp && video_kbps < LP_HALF_D1_BELOW_KBPS {
        Resolution::HalfD1
    } else {
        chosen
    }
}

/// Duration-weighted average audio bitrate over all titles.
pub fn avg_audio_kbps(titles: &[TitleLoad]) -> u32 {
    let total: f64 = titles.iter().map(|t| t.duration_secs).sum();
    (titles.iter().map(|t| t.audio_kbps as f64 * t.duration_secs).sum::<f64>() / total.max(1.0)).ceil() as u32
}

/// Rough size of the menus before they are rendered: still pages are tiny, motion pages cost
/// about 5.5 MB per 25 s (measured with the Open Style templates).
pub fn estimate_menu_mb(titles: usize, motion: bool, motion_secs: f64) -> f64 {
    // Main menu plus root/chapter/settings per title.
    let pages = 1 + 3 * titles;
    let still = pages as f64 * 0.15;
    if motion {
        // Root and chapter menus have video slots.
        still + (2 * titles) as f64 * 5.5 * motion_secs / 25.0
    } else {
        still
    }
}

/// User-defined chapter starts: sorted, deduplicated (>= 1 s apart), inside the runtime, first at 0.
pub fn custom_chapters(starts: &[f64], duration_secs: f64) -> Vec<(f64, Option<String>)> {
    let end = (duration_secs - 1.0).max(0.0);
    let mut v: Vec<f64> = starts.iter().copied().filter(|s| s.is_finite() && *s >= 0.0 && *s < end).collect();
    v.sort_by(f64::total_cmp);
    let mut out: Vec<(f64, Option<String>)> = vec![(0.0, None)];
    for s in v {
        if s - out.last().map_or(0.0, |l| l.0) >= 1.0 {
            out.push((s, None));
        }
    }
    out.truncate(MAX_CHAPTERS);
    out
}

#[derive(Debug, Clone)]
pub struct TitleReport {
    pub input: PathBuf,
    pub label: String,
    pub duration_secs: f64,
    pub aspect: Aspect,
    pub video_kbps: u32,
    pub audio: Vec<String>,
    pub subtitles: Vec<String>,
    pub chapters: usize,
}

#[derive(Debug, Clone)]
pub struct BuildReport {
    /// `<output>/<project>/VIDEO_TS`.
    pub video_ts: PathBuf,
    /// `<output>/<project>`.
    pub project_dir: PathBuf,
    /// Temporary work directory; `None` when it was removed after the build (default).
    pub work_dir: Option<PathBuf>,
    /// The dvdauthor project that was authored.
    pub authoring_xml: String,
    pub plan: BitratePlan,
    pub titles: Vec<TitleReport>,
    pub menu_pages: usize,
    /// Menu pages encoded as motion loops.
    pub motion_pages: usize,
    /// Sound of every menu page (menu VOB name, source; `None` = silent).
    pub menu_sounds: Vec<(String, Option<ClipSource>)>,
    /// Frame size the titles were encoded in.
    pub resolution: Resolution,
    pub size_mb: f64,
    pub target_mb: u32,
    pub warnings: Vec<String>,
}

/// One input after stream selection.
#[derive(Clone)]
struct TitlePlan {
    info: MediaInfo,
    label: String,
    /// Indices into `info.audio`.
    audio: Vec<usize>,
    /// Indices into `info.subtitles` (text subtitles only).
    subtitles: Vec<usize>,
    /// Chapter starts in seconds, first is 0.
    chapters: Vec<f64>,
    chapter_titles: Vec<Option<String>>,
    aspect: Aspect,
    audio_mode: AudioMode,
    track_label_mode: TrackLabelMode,
    ui_lang: UiLang,
    video: VideoProcessing,
    loudnorm: bool,
    /// Length of the motion menus that play this title's clip (limits how late the clip may start).
    motion_secs: f64,
    stem: String,
    thumbnail: Option<PathBuf>,
    chapter_thumbs: Vec<Option<PathBuf>>,
}

impl TitlePlan {
    /// Motion menu sample clip of the whole title: starts at 10 % (at most 5 min in).
    fn clip(&self) -> ClipSource {
        self.clip_at((self.info.duration_secs * 0.1).clamp(0.0, 300.0))
    }

    fn clip_at(&self, start_secs: f64) -> ClipSource {
        ClipSource {
            path: self.info.path.clone(),
            // Positions inside the title; the file is read from the trimmed start on. Clamped so the clip never
            // loops: `-stream_loop` at the end of a 4K file takes minutes.
            start_secs: self.info.trim_start() + start_secs.min((self.info.duration_secs - self.motion_secs).max(0.0)),
            has_audio: !self.info.audio.is_empty(),
        }
    }

    fn audio_outs(&self) -> Vec<AudioOut> {
        self.audio.iter().map(|&i| plan_audio(&self.info.audio[i], AudioTarget::Dvd, self.audio_mode, self.loudnorm || self.info.trim.is_some())).collect()
    }

    fn audio_kbps(&self) -> u32 {
        self.audio_outs().iter().map(|o| o.kbps()).sum()
    }

    fn audio_labels(&self) -> Vec<String> {
        let all = probe::audio_labels(&self.info, self.track_label_mode, self.ui_lang);
        self.audio.iter().zip(self.audio_outs()).map(|(&i, out)| probe::with_layout(&all[i], &out.layout_label(), self.track_label_mode)).collect()
    }

    fn subtitle_labels(&self) -> Vec<String> {
        let all = probe::subtitle_labels(&self.info, self.track_label_mode, self.ui_lang);
        self.subtitles.iter().map(|&i| all[i].clone()).collect()
    }
}

/// Chapter starts: the source's chapters (deduplicated, inside the runtime) or a fixed grid
/// every `interval_secs` (none for videos shorter than `min_video_secs`).
pub fn chapter_starts(info: &MediaInfo, interval_secs: f64, min_video_secs: f64) -> Vec<(f64, Option<String>)> {
    let generic = Regex::new(r"(?i)^(chapter|kapitel|chap)?\s*\d+$|^\d{1,2}:\d{2}(:\d{2})?(\.\d+)?$").expect("regex");
    let end = (info.duration_secs - 1.0).max(0.0);
    let mut out: Vec<(f64, Option<String>)> = vec![(0.0, None)];
    if info.chapters.is_empty() {
        if interval_secs > 0.0 && info.duration_secs >= min_video_secs {
            let mut t = interval_secs;
            while t < end {
                out.push((t, None));
                t += interval_secs;
            }
        }
    } else {
        for c in &info.chapters {
            let title = c.title.clone().filter(|t| !generic.is_match(t.trim()));
            if c.start_secs < 1.0 {
                // Several marks near 0 collapse into chapter 1; keep the first real title.
                if out[0].1.is_none() {
                    out[0].1 = title;
                }
            } else if c.start_secs < end && c.start_secs - out.last().map_or(0.0, |l| l.0) >= 1.0 {
                out.push((c.start_secs, title));
            }
        }
    }
    out.truncate(MAX_CHAPTERS);
    out
}

/// Finds a page with `style` in the main template or, failing that, in a sibling template in the
/// same folder. Siblings of the same design family (sharing a background image with the main
/// template) are preferred.
pub(crate) fn find_page_source(main: &Template, main_path: &Path, style: &str) -> Option<(Template, PathBuf)> {
    let dir = main_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    if page_by_style(main, style).is_some() {
        return Some((main.clone(), dir));
    }
    let main_bgs: Vec<&String> = main.pages.iter().flat_map(|p| &p.backgrounds).collect();
    let mut candidates: Vec<(PathBuf, Template)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")) && p != main_path)
        .filter_map(|p| Template::load(&p).ok().map(|t| (p, t)))
        .filter(|(_, t)| page_by_style(t, style).is_some())
        .collect();
    // The ranking does not depend on `style`, so all sub menus come from the same sibling.
    candidates.sort_by_key(|(path, t)| {
        let same_family = t.pages.iter().flat_map(|p| &p.backgrounds).any(|b| main_bgs.contains(&b));
        let styles = ["MNUS_ROOT", "MNUS_CHAPTER", "MNUS_SETTINGS"]
            .iter()
            .filter(|s| page_by_style(t, s).is_some())
            .count();
        // Within a family prefer the richer variant (with video thumbnails).
        let thumbs = t.pages.iter().filter(|p| has_video_graphics(p)).count();
        (!same_family, std::cmp::Reverse(styles), std::cmp::Reverse(thumbs), path.clone())
    });
    candidates.into_iter().next().map(|(_, t)| (t, dir))
}

/// Removes the `\\?\` prefix `canonicalize` adds on Windows.
pub(crate) fn display_path(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(s) if !s.starts_with("UNC\\") => PathBuf::from(s),
        _ => p,
    }
}

fn has_video_graphics(page: &Page) -> bool {
    page.graphics.iter().any(|g| g.gfx_type.eq_ignore_ascii_case("GT_VIDEO"))
}

pub(crate) fn file_mb(path: &Path) -> f64 {
    std::fs::metadata(path).map_or(0.0, |m| m.len() as f64 / 1024.0 / 1024.0)
}

fn dir_mb(dir: &Path) -> f64 {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| file_mb(&e.path())).sum())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Parallel job runner
// ---------------------------------------------------------------------------

/// Logical CPUs available to this process.
pub fn available_cpus() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

/// Default number of concurrent heavy ffmpeg encodes: half the logical CPUs, because every
/// ffmpeg MPEG-2 encode is multithreaded itself.
pub fn default_jobs() -> usize {
    (available_cpus() / 2).max(1)
}

/// (job index, fraction, telemetry: frames done and current fps)
type JobMsg = (usize, f64, Option<(f64, f64)>);

/// Lets a running job report its own progress (0..1) to the caller.
#[derive(Clone)]
pub(crate) struct JobProgress {
    index: usize,
    tx: mpsc::UnboundedSender<JobMsg>,
}

impl JobProgress {
    pub(crate) fn report(&self, fraction: f64) {
        // The receiver only disappears when the whole build is aborted.
        let _ = self.tx.send((self.index, fraction, None));
    }

    /// Progress with encoder telemetry for the remaining-time estimate.
    pub(crate) fn report_frames(&self, fraction: f64, frames_done: f64, fps: f64) {
        let _ = self.tx.send((self.index, fraction, Some((frames_done, fps))));
    }
}

pub(crate) enum JobEvent {
    Progress { index: usize, fraction: f64, telemetry: Option<(f64, f64)> },
    Finished { index: usize, finished: usize, total: usize },
}

pub(crate) type JobFuture<T> = Pin<Box<dyn Future<Output = Result<T, BuildError>> + Send>>;
pub(crate) type Job<T> = Box<dyn FnOnce(JobProgress) -> JobFuture<T> + Send>;

pub(crate) fn job<T, F, Fut>(f: F) -> Job<T>
where
    F: FnOnce(JobProgress) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, BuildError>> + Send + 'static,
{
    Box::new(move |p| Box::pin(f(p)))
}

/// Runs `jobs` with at most `limit` at a time and returns their results in input order.
///
/// Progress and completion arrive in `on_event`, which runs on the caller's task only, so the
/// caller's (non-thread-safe) logger never sees concurrent calls. The first error aborts all
/// other jobs; their ffmpeg/spumux children are killed (`kill_on_drop`).
pub(crate) async fn run_limited<T: Send + 'static>(
    limit: usize,
    jobs: Vec<Job<T>>,
    cancel: &CancelToken,
    mut on_event: impl FnMut(JobEvent),
) -> Result<Vec<T>, BuildError> {
    cancel.check()?;
    let total = jobs.len();
    let semaphore = Arc::new(Semaphore::new(limit.max(1)));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut set = JoinSet::new();
    for (index, job) in jobs.into_iter().enumerate() {
        let semaphore = semaphore.clone();
        let progress = JobProgress { index, tx: tx.clone() };
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore is never closed");
            (index, job(progress).await)
        });
    }
    drop(tx);

    let mut results: Vec<Option<T>> = (0..total).map(|_| None).collect();
    let mut finished = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Abort every job and wait until all of them are really gone: their futures are
                // dropped, which kills their ffmpeg/spumux processes, before anyone cleans up.
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Err(BuildError::Cancelled);
            }
            Some((index, fraction, telemetry)) = rx.recv() => on_event(JobEvent::Progress { index, fraction, telemetry }),
            joined = set.join_next() => match joined {
                None => break,
                Some(Err(e)) => return Err(BuildError::Internal(format!("Hintergrund-Job abgebrochen: {e}"))),
                // Returning drops `set`, which aborts the remaining jobs.
                Some(Ok((_, Err(e)))) => return Err(e),
                Some(Ok((index, Ok(value)))) => {
                    results[index] = Some(value);
                    finished += 1;
                    on_event(JobEvent::Finished { index, finished, total });
                }
            },
        }
    }
    Ok(results.into_iter().map(|r| r.expect("every job delivered a result")).collect())
}

/// Output frame rate of `tv`.
fn output_fps(tv: TvStandard) -> f64 {
    match tv {
        TvStandard::Pal => 25.0,
        TvStandard::Ntsc => 30000.0 / 1001.0,
    }
}

/// Encodes one title straight into its DVD program stream (subtitles via piped spumux);
/// reports its own progress (0..1) with frame telemetry.
#[allow(clippy::too_many_arguments)]
async fn encode_title(
    tools: &Toolchain,
    t: &TitlePlan,
    work: &Path,
    video_kbps: u32,
    two_pass: bool,
    fast: bool,
    hw_decode: Option<HwAccelMode>,
    tv: TvStandard,
    resolution: Resolution,
    progress: JobProgress,
) -> Result<(TitleReport, Option<&'static str>), BuildError> {
    let hwaccel = match hw_decode {
        Some(mode) => tools.probe_hwaccel(&t.info.path, mode).await,
        None => None,
    };
    let audio: Vec<_> = t.audio.iter().map(|&a| &t.info.audio[a]).collect();
    let audio_filters = if t.loudnorm {
        let tracks: Vec<_> = audio.iter().map(|a| Some(*a)).collect();
        encoder::measure_loudnorm(tools, &t.info, &tracks).await?
    } else {
        Vec::new()
    };
    let job = EncodeJob {
        info: &t.info,
        audio,
        subtitles: t.subtitles.iter().map(|&s| &t.info.subtitles[s]).collect(),
        video_kbps,
        aspect: t.aspect,
        dir: work.to_path_buf(),
        stem: t.stem.clone(),
        two_pass,
        fast,
        tv,
        resolution,
        hwaccel,
        audio_mode: t.audio_mode,
        video: t.video,
        audio_filters,
    };
    let frames_per_run = t.info.duration_secs * output_fps(tv);
    // Frames done count over both runs of a 2-pass encode; never let them go backwards (the
    // closing report of a run carries no frame number).
    let mut frames_seen = 0.0f64;
    let logs = encoder::run_encode(tools, &job, |phase, p| {
        let (fraction, base) = match phase {
            EncodePhase::Single => (0.98 * p.fraction, 0.0),
            EncodePhase::Analysis => (0.49 * p.fraction, 0.0),
            EncodePhase::Final => (0.49 + 0.49 * p.fraction, frames_per_run),
        };
        frames_seen = frames_seen.max(base + p.frame as f64);
        progress.report_frames(fraction, frames_seen, p.fps);
    })
    .await?;
    for (n, l) in logs.iter().enumerate() {
        std::fs::write(work.join(format!("{}.log", job.subtitle_xml(n))), l).at(work)?;
    }
    progress.report(1.0);

    let report = TitleReport {
        input: t.info.path.clone(),
        label: t.label.clone(),
        duration_secs: t.info.duration_secs,
        aspect: t.aspect,
        video_kbps,
        audio: t.audio_labels(),
        subtitles: t.subtitle_labels(),
        chapters: t.chapters.len(),
    };
    Ok((report, hwaccel))
}

/// Applies the user's trim and crop of one input to its probed `info` (errors carry the title).
pub(crate) fn apply_selection(info: &mut MediaInfo, sel: &StreamSelection, label: &str) -> Result<(), BuildError> {
    let fail = |e: String| BuildError::Template(format!("{label}: {e}"));
    probe::apply_trim(info, sel.start_secs, sel.end_secs).map_err(fail)?;
    if let Some(c) = sel.crop {
        probe::apply_crop(info, c).map_err(fail)?;
    }
    info.audio_names = sel.audio_names.iter().filter(|(_, n)| !n.trim().is_empty()).map(|(&i, n)| (i, n.trim().chars().take(60).collect())).collect();
    Ok(())
}

/// Audio and subtitle streams (indices into `info.audio` / `info.subtitles`) of one title, plus
/// warnings: the user's choice in `sel` wins over the language filters of `opts`.
pub(crate) fn select_streams(info: &MediaInfo, sel: &StreamSelection, opts: &BuildOptions, label: &str) -> (Vec<usize>, Vec<usize>, Vec<String>) {
    let mut notes = Vec::new();
    let mut audio: Vec<usize> = (0..info.audio.len())
        .filter(|&a| match &sel.audio {
            Some(chosen) => chosen.contains(&info.audio[a].index),
            None => {
                opts.audio_langs.is_empty()
                    || opts.audio_langs.iter().any(|l| language_matches(l, info.audio[a].language.as_deref()))
            }
        })
        .collect();
    if audio.is_empty() && !info.audio.is_empty() {
        notes.push(tf!(opts.ui_lang, "note_no_audio_match", label = label, langs = format!("{:?}", opts.audio_langs)));
        audio.push(0);
    }
    if audio.len() > MAX_AUDIO_STREAMS {
        notes.push(tf!(opts.ui_lang, "note_audio_count", label = label, n = audio.len(), max = MAX_AUDIO_STREAMS));
        audio.truncate(MAX_AUDIO_STREAMS);
    }
    if info.audio.is_empty() {
        notes.push(tf!(opts.ui_lang, "note_no_audio", label = label));
    }

    let mut subtitles = Vec::new();
    if !opts.no_subtitles {
        for (n, s) in info.subtitles.iter().enumerate() {
            let wanted = match &sel.subtitles {
                Some(chosen) => chosen.contains(&s.index),
                None => {
                    opts.subtitle_langs.is_empty()
                        || opts.subtitle_langs.iter().any(|l| language_matches(l, s.language.as_deref()))
                }
            };
            if !wanted {
                continue;
            }
            if s.kind == SubtitleKind::Bitmap && !crate::subpic::supported(&s.codec) {
                notes.push(tf!(opts.ui_lang, "note_bitmap_sub_unsupported", label = label, index = s.index, codec = s.codec));
                continue;
            }
            if s.kind == SubtitleKind::Text && !opts.disc.is_bluray() && s.language.as_deref().is_some_and(is_rtl_language) {
                notes.push(tf!(opts.ui_lang, "note_rtl_subtitle", label = label, index = s.index, lang = s.language.as_deref().unwrap_or("")));
            }
            subtitles.push(n);
        }
        subtitles.truncate(MAX_SUBTITLE_STREAMS);
    }
    // Preferred languages become stream 0 (the player's default); the rest keep their order.
    if let Some(lang) = &opts.preferred_audio_lang {
        audio.sort_by_key(|&a| !language_matches(lang, info.audio[a].language.as_deref()));
    }
    if let Some(lang) = &opts.preferred_subtitle_lang {
        subtitles.sort_by_key(|&s| !language_matches(lang, info.subtitles[s].language.as_deref()));
    }
    (audio, subtitles, notes)
}

/// Languages whose text subtitles need bidi/shaping, which [`crate::textsub`] does not do.
fn is_rtl_language(lang: &str) -> bool {
    matches!(lang.to_ascii_lowercase().as_str(), "ar" | "ara" | "he" | "heb" | "fa" | "fas" | "per" | "ur" | "urd" | "yi" | "yid")
}

/// Builds a complete DVD-Video structure from `opts.inputs`.
///
/// When `opts.cancel` fires, all running tools are stopped, the work directory is removed and
/// [`BuildError::Cancelled`] is returned.
pub async fn build_dvd(opts: &BuildOptions, log: &mut (dyn FnMut(Event) + Send)) -> Result<BuildReport, BuildError> {
    let result = if opts.disc.is_bluray() { crate::bd::build_bd(opts, log).await } else { build_dvd_inner(opts, log).await };
    if matches!(result, Err(BuildError::Cancelled)) {
        log(Event::Step(tr(opts.ui_lang, "cancelled_cleanup").into()));
        if let Ok(out) = std::fs::canonicalize(&opts.output) {
            let work = work_dir(&display_path(out), opts);
            if work.join(WORK_DIR).is_file() {
                if let Err(e) = remove_old_output(&work).await {
                    log(Event::Warning(tf!(opts.ui_lang, "workdir_delete_failed", err = e)));
                }
            }
        }
    }
    result
}

async fn build_dvd_inner(opts: &BuildOptions, log: &mut (dyn FnMut(Event) + Send)) -> Result<BuildReport, BuildError> {
    let cancel = &opts.cancel;
    let mut warnings: Vec<String> = Vec::new();
    let mut warn = |log: &mut (dyn FnMut(Event) + Send), w: String| {
        log(Event::Warning(w.clone()));
        warnings.push(w);
    };
    if opts.inputs.is_empty() {
        return Err(BuildError::Author(AuthorError::NoTitles));
    }
    if opts.inputs.len() > MAX_TITLES {
        return Err(BuildError::Author(AuthorError::TooManyTitles(opts.inputs.len())));
    }
    if opts.resolution.is_hd() {
        return Err(BuildError::Template(format!(
            "Die Auflösung {} gibt es nur für Blu-ray – für eine DVD bitte Full D1, Half D1 oder SIF wählen",
            opts.resolution.label()
        )));
    }

    // 0. Tools and directories --------------------------------------------------------------
    let tools = cancellable(cancel, Toolchain::detect()).await?;
    log(Event::Step(tf!(opts.ui_lang, "tools_dvd", ffmpeg = tools.ffmpeg.display())));
    for note in &tools.notes {
        warn(log, note.clone());
    }
    std::fs::create_dir_all(&opts.output).at(&opts.output)?;
    let out = display_path(std::fs::canonicalize(&opts.output).at(&opts.output)?);
    // Temporary work directory; a leftover from an aborted run (recognised by its marker) is
    // removed. The authoring writes into it with plain ASCII names, the finished VIDEO_TS is then
    // moved into the project folder.
    let work = work_dir(&out, opts);
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
    let tv = opts.tv_standard;
    if tv == TvStandard::Ntsc {
        log(Event::Step(tr(opts.ui_lang, "tv_ntsc").into()));
    }
    if opts.audio_mode == AudioMode::Lpcm {
        warn(log, tr(opts.ui_lang, "lpcm_dvd_warn").into());
    }

    // 1. Probe and select streams ------------------------------------------------------------
    log(Event::Phase(BuildPhase::Probe));
    let mut titles: Vec<TitlePlan> = Vec::new();
    for (i, input) in opts.inputs.iter().enumerate() {
        log(Event::Step(tf!(opts.ui_lang, "analyzing", path = input.display())));
        let mut info = cancellable(cancel, probe::probe(&tools.ffprobe, input)).await?;
        let sel = opts.selections.get(i).cloned().unwrap_or_default();
        let label = sel.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
            input.file_stem().map_or_else(|| format!("Titel {}", i + 1), |s| s.to_string_lossy().into_owned())
        });

        let (audio, subtitles, notes) = select_streams(&info, &sel, opts, &label);
        for note in notes {
            warn(log, note);
        }
        apply_selection(&mut info, &sel, &label)?;

        let chapters = match &sel.chapters {
            Some(starts) => custom_chapters(starts, info.duration_secs),
            None => chapter_starts(&info, opts.chapters.interval_secs, opts.chapters.min_video_secs),
        };
        let aspect = opts.aspect_policy.aspect_for(info.video.as_ref().map_or(16.0 / 9.0, |v| v.display_aspect));
        titles.push(TitlePlan {
            label,
            audio,
            subtitles,
            chapters: chapters.iter().map(|c| c.0).collect(),
            chapter_titles: chapters.into_iter().map(|c| c.1).collect(),
            aspect,
            audio_mode: opts.audio_mode,
            track_label_mode: opts.track_label_mode,
            ui_lang: opts.ui_lang,
            video: opts.video,
            loudnorm: opts.loudnorm,
            motion_secs: opts.motion_duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(menu::DEFAULT_MOTION_SECS),
            stem: format!("title_{:02}", i + 1),
            thumbnail: None,
            chapter_thumbs: Vec::new(),
            info,
        });
    }

    let groups = groups(&opts.selections, titles.len());
    for g in &groups {
        let shapes: Vec<MemberShape> = titles[g.clone()].iter().map(|t| (t.label.as_str(), t.audio.len(), t.subtitles.len())).collect();
        check_group_shapes(&shapes)?;
        let chapters: usize = titles[g.clone()].iter().map(|t| t.chapters.len()).sum();
        if chapters > MAX_CHAPTERS {
            return Err(BuildError::Template(format!(
                "{}: {chapters} Kapitel nach dem Zusammenführen, eine DVD erlaubt {MAX_CHAPTERS} pro Titel",
                titles[g.start].label
            )));
        }
        harmonize_group(&mut titles[g.clone()]);
    }

    let tools = Arc::new(tools.with_priority(opts.process_priority));
    let cpu_jobs = available_cpus();
    let heavy_jobs = opts.parallel_jobs.unwrap_or_else(default_jobs).max(1);

    // Project folder name default (ConvertX: the menu heading, else the first video's name).
    let default_project_name = opts
        .menu_title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| titles[0].label.clone());

    let stage: MenuStage = if let Some(layout) = &opts.custom_layout {
        // 2-4. Custom layout (menu editor) ------------------------------------------------------------
        cancel.check()?;
        log(Event::Phase(BuildPhase::Menus));
        log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Menus, None, None, None)));
        log(Event::Step(tf!(opts.ui_lang, "custom_layout", n = layout.menus.len())));
        let layout_titles: Vec<crate::layout::LayoutTitle> = groups
            .iter()
            .map(|g| {
                let (members, t) = (&titles[g.clone()], &titles[g.start]);
                crate::layout::LayoutTitle {
                label: t.label.clone(),
                chapters: members.iter().map(|m| m.chapters.len()).sum(),
                duration_secs: members.iter().map(|m| m.info.duration_secs).sum(),
                clip: t.clip(),
                audio_sources: t.audio.iter().map(|&a| t.info.audio[a].index as u32).collect(),
                subtitle_sources: t.subtitles.iter().map(|&s| t.info.subtitles[s].index as u32).collect(),
                audio_labels: t.audio_labels(),
                subtitle_labels: t.subtitle_labels(),
                }
            })
            .collect();
        let secs = opts.motion_duration.unwrap_or(crate::layout::DEFAULT_MENU_SECS);
        let custom = crate::layout::build_custom_menus(layout, &layout_titles, tv, &work, secs)
            .map_err(|e| BuildError::Template(e.to_string()))?;
        for w in &custom.warnings {
            warn(log, w.clone());
        }
        MenuStage {
            pages: custom.pages,
            vmgm_menus: custom.vmgm_menus,
            ts_menus: custom.titleset_menus,
            main_pages: custom.main_menu_pages,
            title_pre: Some(custom.title_pre),
            sound_note: None,
        }
    } else {
        // 2. Templates -----------------------------------------------------------------------------
        let main = Template::load(&opts.template).map_err(|e| BuildError::Template(format!("{}: {e}", opts.template.display())))?;
        let main_dir = opts.template.parent().unwrap_or(Path::new(".")).to_path_buf();
        let title_page = page_by_style(&main, "MNUS_TITLE")
            .or(main.pages.first())
            .ok_or_else(|| BuildError::Template("template has no pages".into()))?;
        let root_src = find_page_source(&main, &opts.template, "MNUS_ROOT");
        let chap_src = find_page_source(&main, &opts.template, "MNUS_CHAPTER");
        let set_src = find_page_source(&main, &opts.template, "MNUS_SETTINGS");
        for (name, src) in [(tr(opts.ui_lang, "root"), &root_src), (tr(opts.ui_lang, "chapter"), &chap_src), (tr(opts.ui_lang, "settings"), &set_src)] {
            match src {
                Some((t, _)) => log(Event::Step(tf!(opts.ui_lang, "menu_from_template", name = name, template = t.name.as_deref().unwrap_or("?")))),
                None => warn(log, tf!(opts.ui_lang, "menu_missing_warn", name = name)),
            }
        }
        let root_page = root_src
            .as_ref()
            .filter(|_| !opts.skip_root_menu)
            .and_then(|(t, _)| page_by_style(t, "MNUS_ROOT"));
        let chap_page = chap_src.as_ref().and_then(|(t, _)| page_by_style(t, "MNUS_CHAPTER"));
        let set_page = set_src.as_ref().and_then(|(t, _)| page_by_style(t, "MNUS_SETTINGS"));

        // 3. Thumbnails (parallel) -----------------------------------------------------------------
        log(Event::Phase(BuildPhase::Thumbnails));
        log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Menus, None, None, None)));
        // Motion menus show moving clips in the video slots, still thumbnails are not needed.
        let want_title_thumbs =
            !opts.motion_menu && (has_video_graphics(title_page) || root_page.is_some_and(has_video_graphics));
        let want_chapter_thumbs = !opts.motion_menu && chap_page.is_some_and(has_video_graphics);
        // (title index, chapter index or None for the title thumbnail, time, output)
        let mut thumbs: Vec<(usize, Option<usize>, f64, PathBuf)> = Vec::new();
        for (i, t) in titles.iter().enumerate() {
            if want_title_thumbs {
                let at = (t.info.duration_secs * 0.1).clamp(0.0, 300.0);
                thumbs.push((i, None, at, work.join(format!("{}_thumb.png", t.stem))));
            }
            if want_chapter_thumbs {
                for (n, &start) in t.chapters.iter().enumerate() {
                    let at = (start + 3.0).min((t.info.duration_secs - 0.5).max(0.0));
                    thumbs.push((i, Some(n), at, work.join(format!("{}_ch{:02}.png", t.stem, n + 1))));
                }
            }
        }
        if !thumbs.is_empty() {
            log(Event::Step(tf!(opts.ui_lang, "thumbnails", n = thumbs.len(), jobs = cpu_jobs)));
            let jobs: Vec<Job<()>> = thumbs
                .iter()
                .map(|(i, _, at, path)| {
                    let (tools, input, at, path) =
                        (tools.clone(), titles[*i].info.path.clone(), *at + titles[*i].info.trim_start(), path.clone());
                    job(move |_| async move {
                        tools.ffmpeg(&encoder::thumbnail_args(&input, at, &path), 0.0, |_| {}).await?;
                        Ok(())
                    })
                })
                .collect();
            let total = jobs.len();
            run_limited(cpu_jobs, jobs, cancel, |e| {
                if let JobEvent::Finished { finished, .. } = e {
                    log(Event::Progress { label: "Vorschaubilder".into(), fraction: finished as f64 / total as f64 });
                }
            })
            .await?;
            for (i, chapter, _, path) in thumbs {
                match chapter {
                    None => titles[i].thumbnail = Some(path),
                    Some(n) => {
                        let t = &mut titles[i];
                        t.chapter_thumbs.resize(t.chapters.len(), None);
                        t.chapter_thumbs[n] = Some(path);
                    }
                }
            }
        }

        // 4. Menus (rendered in parallel) --------------------------------------------------------------
        cancel.check()?;
        log(Event::Phase(BuildPhase::Menus));
        log(Event::Step(tr(opts.ui_lang, "render_menus").into()));
        let ui = opts.ui_lang;
        let chapter_word = tr(ui, "chapter");
        // A single video can do without the main (title) menu.
        let has_main_menu = !(opts.no_title_menu_for_single_title && groups.len() == 1);
        let settings_word = tr(ui, "settings");
        // ConvertX default: the project is named after the first video.
        let main_heading = opts
            .menu_title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| titles[0].label.clone());
        let motion = opts.motion_menu;
        // Menu sound: the main menu is silent unless a menu audio file is set; a video's own menus
        // (root/chapters/settings) always play that video's sound.
        let main_menu_audio: Option<ClipSource> = match &opts.motion_audio {
            MenuAudio::File(path) if motion => {
                if !path.is_file() {
                    return Err(BuildError::Template(format!("Menü-Audiodatei nicht gefunden: {}", path.display())));
                }
                Some(ClipSource { path: path.clone(), start_secs: 0.0, has_audio: true })
            }
            _ => None,
        };
        let titles_have_sound = opts.motion_audio != MenuAudio::Silence;
        // VMGM page that lists each title (back buttons and the end of playback return there).
        let main_pages: Vec<u32> = (0..groups.len()).map(|i| author::title_page_of(title_page, i)).collect();

        let render_titleset = |(i, g): (usize, &std::ops::Range<usize>)| -> Result<Vec<RenderedMenu>, MenuError> {
            let (members, t) = (&titles[g.clone()], &titles[g.start]);
            let title_clip = t.clip();
            let title_audio = (titles_have_sound && !t.info.audio.is_empty()).then(|| t.clip());
            // Chapters run across the merged files; every file starts a new chapter.
            let chapters: Vec<ChapterEntry> = members
                .iter()
                .flat_map(|m| m.chapters.iter().enumerate().map(move |(n, &start)| (m, n, start)))
                .enumerate()
                .map(|(k, (m, n, start))| ChapterEntry {
                    label: m.chapter_titles[n].clone().unwrap_or_else(|| opts.chapters.label(k + 1, ui)),
                    thumbnail: m.chapter_thumbs.get(n).cloned().flatten(),
                    clip: motion.then(|| m.clip_at(start)),
                })
                .collect();
            let audio = t.audio_labels();
            let subs = t.subtitle_labels();
            let chap_kind = MenuKind::Chapters(&chapters);
            let set_kind = MenuKind::Settings { audio: &audio, subtitles: &subs };
            let need_settings = audio.len() > 1 || !subs.is_empty();
            let need_chapters = chapters.len() > 1;

            // Menu (PGC) numbers inside the VTSM.
            let mut next = 1;
            let mut alloc = |pages: u32| {
                let first = next;
                next += pages;
                first
            };
            let root_no = root_page.map(|_| alloc(1));
            let chap_no = chap_page.filter(|_| need_chapters).map(|p| alloc(author::page_count(p, &chap_kind)));
            let set_no = set_page.filter(|_| need_settings).map(|p| alloc(author::page_count(p, &set_kind)));
            let links = MenuLinks { first: 0, root: root_no, chapters: chap_no, settings: set_no, main_menu: has_main_menu.then(|| main_pages[i]), bonus: None, episodes: None, title1_chapters: None, title1_settings: None };

            let mut rendered = Vec::new();
            let jobs: [(Option<&Page>, Option<u32>, MenuKind, Option<&PathBuf>, String, &str); 3] = [
                (root_page, root_no, MenuKind::Root, root_src.as_ref().map(|s| &s.1), t.label.clone(), "root"),
                (chap_page, chap_no, chap_kind, chap_src.as_ref().map(|s| &s.1), format!("{} – {chapter_word}", t.label), "chap"),
                (set_page, set_no, set_kind, set_src.as_ref().map(|s| &s.1), format!("{} – {settings_word}", t.label), "set"),
            ];
            let mut entry_used = false;
            for (page, first, kind, dir, heading, suffix) in jobs {
                let (Some(page), Some(first), Some(dir)) = (page, first, dir) else { continue };
                let template = [&root_src, &chap_src, &set_src]
                    .into_iter()
                    .flatten()
                    .find(|(t, _)| t.pages.iter().any(|p| std::ptr::eq(p, page)))
                    .map(|(t, _)| t)
                    .expect("page belongs to a loaded template");
                let entry = match suffix {
                    _ if !entry_used => "root",
                    "chap" => "ptt",
                    _ => "audio",
                };
                entry_used = true;
                let ropts = RenderOptions {
                    heading: Some(&heading),
                    background_video: t.thumbnail.as_deref(),
                    ui_lang: ui,
                    motion,
                    background_clip: motion.then_some(&title_clip),
                    motion_duration: opts.motion_duration,
                    menu_audio: title_audio.as_ref(),
                    audio_from_first_chapter: suffix == "chap",
                    text_overflow: opts.text_overflow,
                    tv,
                    intro: opts.menu_animations,
                    ..RenderOptions::still(dir)
                };
                let prefix = format!("ts{:02}_{suffix}", i + 1);
                rendered.push(menu::render_menu(template, page, &kind, MenuLinks { first, ..links }, Some(entry), &prefix, &work, &ropts)?);
            }
            Ok(rendered)
        };
        let ts_menus: Vec<Vec<RenderedMenu>> = groups.par_iter().enumerate().map(render_titleset).collect::<Result<_, _>>()?;
        for w in ts_menus.iter().flatten().flat_map(|m| &m.warnings) {
            warn(log, w.clone());
        }
        let has_ts_menus: Vec<bool> = ts_menus.iter().map(|m| !m.is_empty()).collect();

        let entries: Vec<TitleEntry> = groups
            .iter()
            .enumerate()
            .map(|(i, g)| (i, &titles[g.start]))
            .map(|(i, t)| TitleEntry {
                label: t.label.clone(),
                thumbnail: t.thumbnail.clone(),
                clip: motion.then(|| t.clip()),
                titleset_menu: has_ts_menus[i].then_some(i as u32 + 1),
            })
            .collect();
        let vmgm = if has_main_menu {
            // Templates whose main menu has no item buttons: it plays title 1 and shows it in its video
            // window. Series templates list all titles in an episode menu; "one main video" templates
            // put the titles after the first into a bonus menu.
            let no_items = author::count_loc(title_page, author::TITLE_ITEMS) == 0;
            let episodes_page = page_by_style(&main, "MNUS_SERIES_EPISODES").filter(|p| no_items && author::count_loc(p, author::TITLE_ITEMS) > 0);
            let bonus_page = page_by_style(&main, "MNUS_BONUS")
                .filter(|p| episodes_page.is_none() && entries.len() > 1 && no_items && author::count_loc(p, &["ML_PLAY_BONUS"]) > 0);
            let main_total = author::page_count(title_page, &MenuKind::Titles(&entries));
            // Title 1's chapter / settings menus, reachable straight from an item-less main menu.
            let entry_of = |prefix: &str| -> Option<&'static str> {
                let spec = ts_menus.first()?.iter().flat_map(|m| &m.pages).map(|(s, _)| s).find(|s| s.vob.starts_with(prefix) && s.entry.is_some())?;
                ["root", "ptt", "audio"].into_iter().find(|e| spec.entry.as_deref() == Some(e))
            };
            let mut main_menu = menu::render_menu(
                &main,
                title_page,
                &MenuKind::Titles(&entries),
                MenuLinks {
                    first: 1,
                    bonus: bonus_page.map(|_| main_total + 1),
                    episodes: episodes_page.map(|_| main_total + 1),
                    title1_chapters: entry_of("ts01_chap").filter(|_| no_items),
                    title1_settings: entry_of("ts01_set").filter(|_| no_items),
                    ..MenuLinks::default()
                },
                Some("title"),
                "vmgm",
                &work,
                &RenderOptions {
                    heading: Some(&main_heading),
                    background_video: titles[0].thumbnail.as_deref(),
                    ui_lang: ui,
                    motion,
                    background_clip: entries[0].clip.as_ref().filter(|_| motion),
                    motion_duration: opts.motion_duration,
                    text_overflow: opts.text_overflow,
                    tv,
                    intro: opts.menu_animations,
                    menu_audio: main_menu_audio.as_ref(),
                    ..RenderOptions::still(&main_dir)
                },
            )?;
            if let Some(page) = episodes_page {
                let episodes = menu::render_menu(
                    &main,
                    page,
                    &MenuKind::Titles(&entries),
                    MenuLinks { first: main_total + 1, main_menu: Some(1), ..MenuLinks::default() },
                    None,
                    "episodes",
                    &work,
                    &RenderOptions {
                        heading: Some(tr(ui, "episodes")),
                        ui_lang: ui,
                        motion,
                        motion_duration: opts.motion_duration,
                        text_overflow: opts.text_overflow,
                        tv,
                        intro: opts.menu_animations,
                        menu_audio: main_menu_audio.as_ref(),
                        ..RenderOptions::still(&main_dir)
                    },
                )?;
                main_menu.pages.extend(episodes.pages);
                main_menu.warnings.extend(episodes.warnings);
            }
            if let Some(page) = bonus_page {
                let bonus = menu::render_menu(
                    &main,
                    page,
                    &MenuKind::Bonus(&entries[1..]),
                    MenuLinks { first: main_total + 1, main_menu: Some(1), ..MenuLinks::default() },
                    None,
                    "bonus",
                    &work,
                    &RenderOptions {
                        heading: Some("Bonus"),
                        ui_lang: ui,
                        motion,
                        motion_duration: opts.motion_duration,
                        text_overflow: opts.text_overflow,
                        tv,
                        intro: opts.menu_animations,
                        menu_audio: main_menu_audio.as_ref(),
                        ..RenderOptions::still(&main_dir)
                    },
                )?;
                main_menu.pages.extend(bonus.pages);
                main_menu.warnings.extend(bonus.warnings);
            }
            main_menu
    } else {
            log(Event::Step(tr(opts.ui_lang, "no_title_menu").into()));
            RenderedMenu { pages: vec![], warnings: vec![] }
        };
        for w in &vmgm.warnings {
            warn(log, w.clone());
        }
        let sound_note = Some(match (&main_menu_audio, titles_have_sound) {
            (Some(a), _) => tf!(opts.ui_lang, "menu_sound_file", file = a.path.file_name().unwrap_or_default().to_string_lossy()),
            (None, true) => tr(opts.ui_lang, "menu_sound_titles").into(),
            (None, false) => tr(opts.ui_lang, "menu_sound_silent").into(),
        });
        MenuStage {
            pages: vmgm.pages.iter().chain(ts_menus.iter().flatten().flat_map(|m| m.pages.iter())).cloned().collect(),
            vmgm_menus: vmgm.pages.into_iter().map(|(spec, _)| spec).collect(),
            ts_menus: ts_menus
                .into_iter()
                .map(|menus| menus.into_iter().flat_map(|m| m.pages.into_iter().map(|(spec, _)| spec)).collect())
                .collect(),
            main_pages,
            title_pre: None,
            sound_note,
        }
    };

    // 5. Menu MPEGs (still or motion loop, then spumux for the highlights; parallel) -----------
    cancel.check()?;
    log(Event::Phase(BuildPhase::MenuEncoding));
    let all_pages = &stage.pages;
    let menu_pages = all_pages.len();
    let motion_pages = all_pages.iter().filter(|(_, f)| f.is_motion()).count();
    let menu_jobs = if motion_pages > 0 { heavy_jobs } else { cpu_jobs };
    log(Event::Step(if motion_pages > 0 {
        tf!(opts.ui_lang, "menu_pages_motion", pages = menu_pages, motion = motion_pages, jobs = menu_jobs)
    } else {
        tf!(opts.ui_lang, "menu_pages", pages = menu_pages, jobs = menu_jobs)
    }));
    if let Some(note) = stage.sound_note.as_ref().filter(|_| motion_pages > 0) {
        log(Event::Step(note.clone()));
    }
    let menu_sounds: Vec<(String, Option<ClipSource>)> =
        all_pages.iter().map(|(spec, files)| (spec.vob.clone(), files.audio.clone())).collect();
    let jobs: Vec<Job<f64>> = all_pages
        .iter()
        .map(|(spec, files)| {
            let audio = files.audio.clone();
            let (tools, work, vob, files) = (tools.clone(), work.clone(), spec.vob.clone(), files.clone());
            job(move |_| async move {
                let raw = format!("{}_raw.mpg", vob.trim_end_matches(".mpg"));
                let args = match files.motion_secs.filter(|_| files.is_motion()) {
                    _ if files.custom.is_some() => encoder::custom_menu_args(
                        tv,
                        files.custom.as_ref().expect("checked"),
                        files.motion_secs.unwrap_or(1.0),
                        audio.as_ref(),
                        &work.join(&raw),
                    ),
                    Some(secs) => encoder::motion_menu_args(
                        tv,
                        &files.background,
                        files.foreground.as_deref(),
                        &files.video_slots,
                        secs,
                        audio.as_ref(),
                        files.intro.as_ref().map(|i| i.secs),
                        &work.join(&raw),
                    ),
                    None => encoder::menu_still_args(tv, &files.background, &work.join(&raw)),
                };
                let secs = files.motion_secs.unwrap_or(1.0);
                match &files.intro {
                    // The intro frames go ffmpeg -> Rust (slot videos) -> ffmpeg over pipes, never to disk.
                    Some(intro) => {
                        // Stream 0 is the page background, the slot videos follow.
                        let streams: Vec<RawStream> = std::iter::once(encoder::dvd_background_stream(tv, files.intro_fill, intro.frames))
                            .chain(files.video_slots.iter().map(|s| encoder::dvd_slot_stream(tv, s, intro.frames)))
                            .collect();
                        let (sprites, frames, end) = (intro.sprites.clone(), intro.frames, intro.end);
                        let (w, h) = tv.size();
                        let compose = Arc::new(move |k: usize, streams: &[&[u8]]| {
                            let (background, slots) = streams.split_first().expect("background stream");
                            menu_anim::composite_frame(w, h, background, &sprites, slots, menu_anim::frame_time(k, frames, end))
                        });
                        tools.ffmpeg_piped(&args, &streams, frames, secs, compose, |_| {}).await?;
                    }
                    None => tools.ffmpeg(&args, secs, |_| {}).await?,
                }
                let xml = files.spumux_xml.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                let spu_log = tools.spumux(&work, &xml, &raw, &vob, 0).await?;
                std::fs::write(work.join(format!("{xml}.log")), spu_log).at(&work)?;
                std::fs::remove_file(work.join(&raw)).at(&work.join(&raw))?;
                Ok(file_mb(&work.join(&vob)))
            })
        })
        .collect();
    let menu_sizes = run_limited(menu_jobs, jobs, cancel, |e| {
        if let JobEvent::Finished { finished, total, .. } = e {
            log(Event::Progress {
                label: format!("Menüs kodieren ({finished}/{total})"),
                fraction: finished as f64 / total as f64,
            });
        }
    })
    .await?;
    let menu_mb: f64 = menu_sizes.iter().sum();

    // 6. Bitrate ----------------------------------------------------------------------------------
    let loads: Vec<TitleLoad> = titles
        .iter()
        .map(|t| TitleLoad { duration_secs: t.info.duration_secs, audio_kbps: t.audio_kbps(), subtitle_streams: t.subtitles.len() })
        .collect();
    let total_secs: f64 = loads.iter().map(|l| l.duration_secs).sum();
    let avg_audio = avg_audio_kbps(&loads);
    let plan = bitrate_plan(opts.disc, &loads, menu_mb, opts.encoding_profile)?;
    if plan.clamp == Clamp::Min {
        return Err(BuildError::DoesNotFit { disc: opts.disc, plan: Box::new(plan) });
    }
    let profile = match opts.encoding_profile {
        EncodingProfile::Auto => String::new(),
        p => tf!(opts.ui_lang, "bitrate_profile", profile = p.label(), kbps = p.max_video_kbps()),
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
    let resolution = effective_resolution(opts.resolution, opts.encoding_profile, plan.video_kbps);
    if resolution != opts.resolution {
        log(Event::Step(tf!(
            opts.ui_lang,
            "lp_half_d1",
            below = LP_HALF_D1_BELOW_KBPS,
            res = resolution.label(),
            w = resolution.frame_size(tv).0,
            h = resolution.frame_size(tv).1
        )));
    }
    if !resolution.allows_wide() {
        let wide: Vec<&str> =
            titles.iter().filter(|t| t.aspect == Aspect::Wide).map(|t| t.label.as_str()).collect();
        if !wide.is_empty() {
            warn(log, tf!(opts.ui_lang, "wide_letterbox_warn", res = resolution.label(), titles = wide.join(", ")));
        }
        for t in titles.iter_mut() {
            t.aspect = Aspect::Standard;
        }
    }

    // 7. Encode titles (independent titlesets in parallel) ---------------------------------------
    let count = titles.len();
    let passes = match (opts.two_pass, opts.fast_encode) {
        (true, true) => tr(opts.ui_lang, "passes_2_fast"),
        (true, false) => tr(opts.ui_lang, "passes_2"),
        (false, true) => tr(opts.ui_lang, "passes_1_fast"),
        (false, false) => tr(opts.ui_lang, "passes_1"),
    };
    let encode_jobs = heavy_jobs.min(count);
    log(Event::Step(format!(
        "Kodiere {count} Titel ({passes}, {encode_jobs} parallel, {} kbps Video, {} {}×{})",
        plan.video_kbps,
        resolution.label(),
        resolution.frame_size(tv).0,
        resolution.frame_size(tv).1
    )));
    log(Event::Phase(BuildPhase::Encoding { index: 0, count }));
    let jobs: Vec<Job<(TitleReport, Option<&'static str>)>> = titles
        .iter()
        .map(|t| {
            let (t, tools, work) = (t.clone(), tools.clone(), work.clone());
            let video_kbps = plan.video_kbps.min(DVD_MAX_TOTAL_KBPS - t.audio_kbps());
            let (two_pass, fast, hw_decode) = (opts.two_pass, opts.fast_encode, opts.hw_decode.then_some(opts.hw_accel));
            job(move |progress| async move {
                encode_title(&tools, &t, &work, video_kbps, two_pass, fast, hw_decode, tv, resolution, progress).await
            })
        })
        .collect();

    // Overall progress: per-title fractions weighted by duration, throttled for smooth UIs.
    let weights: Vec<f64> = titles.iter().map(|t| t.info.duration_secs.max(1.0) / total_secs.max(1.0)).collect();
    let mut fractions = vec![0.0f64; count];
    let mut finished_titles = 0usize;
    let mut last_emit = std::time::Instant::now();
    let mut last_overall = -1.0f64;
    let labels: Vec<String> = titles.iter().map(|t| t.label.clone()).collect();
    // Remaining time: frames of all titles against the encoders' fps, then authoring; the
    // image + burn estimate assumes the default speed (DVD-5 max, DVD-9 4x) with verification.
    let frames: Vec<f64> = titles.iter().map(|t| t.info.duration_secs * output_fps(tv)).collect();
    let mut frame_eta = FrameEta::new(&frames, if opts.two_pass { 2 } else { 1 });
    let expected_bytes =
        (plan.video_kbps + avg_audio) as f64 * 1000.0 / 8.0 * total_secs + menu_mb * 1024.0 * 1024.0;
    let dual_layer = expected_bytes > SINGLE_LAYER_BYTES;
    let burn_estimate = Some(eta::image_and_burn_secs(eta::Medium::Dvd, expected_bytes, eta::default_speed_x(eta::Medium::Dvd, dual_layer), true, dual_layer));
    let authoring_estimate = eta::authoring_secs(expected_bytes);
    let mut last_eta = std::time::Instant::now() - std::time::Duration::from_secs(10);
    let encode_start = std::time::Instant::now();
    log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Encoding, Some(tf!(opts.ui_lang, "eta_video_n", n = 1, count = count)), None, burn_estimate)));
    let reports = run_limited(encode_jobs, jobs, cancel, |e| {
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
            let detail = tf!(opts.ui_lang, "eta_video_n", n = (finished_titles + 1).min(count), count = count);
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
    let (reports, hwaccels): (Vec<TitleReport>, Vec<Option<&'static str>>) = reports.into_iter().unzip();
    if let Some(hw) = hwaccels.iter().flatten().next() {
        log(Event::Step(tf!(opts.ui_lang, "gpu_decode", hw = hw)));
    } else if opts.hw_decode {
        log(Event::Step(tr(opts.ui_lang, "gpu_unavailable").into()));
    }

    // 8. dvdauthor --------------------------------------------------------------------------------
    let MenuStage { vmgm_menus, ts_menus, main_pages, title_pre, .. } = stage;
    let titlesets = groups
        .iter()
        .zip(ts_menus)
        .zip(&main_pages)
        .map(|((g, menus), &main_menu_page)| (&titles[g.clone()], menus, main_menu_page))
        .map(|(members, menus, main_menu_page)| (&members[0], members, menus, main_menu_page))
        .map(|(t, members, menus, main_menu_page)| TitlesetSpec {
            main_menu_page,
            menus,
            title: TitleSpec {
                vobs: members
                    .iter()
                    .map(|m| VobSpec { file: format!("{}.mpg", m.stem), chapters: m.chapters.iter().map(|&c| chapter_mark(c)).collect() })
                    .collect(),
                label: t.label.clone(),
                aspect: t.aspect.as_str().into(),
                audio_formats: t.audio_outs().iter().map(|o| o.dvdauthor_format().to_string()).collect(),
                audio_langs: t
                    .audio
                    .iter()
                    .map(|&a| t.info.audio[a].language.as_deref().and_then(probe::iso639_1).map(str::to_string))
                    .collect(),
                subtitle_langs: t
                    .subtitles
                    .iter()
                    .map(|&s| t.info.subtitles[s].language.as_deref().and_then(probe::iso639_1).map(str::to_string))
                    .collect(),
                force_subtitle: opts.force_first_subtitle,
            },
        })
        .collect();

    let layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: "4:3".into(),
        vmgm_menus,
        titlesets,
        playback: opts.playback,
        tv,
        title_pre,
    };
    // The authoring XML is only kept for the report (and the work directory): the native authoring
    // takes the layout directly.
    let authoring_xml = author::dvdauthor_xml(&layout)?;
    std::fs::write(work.join("dvdauthor.xml"), &authoring_xml)?;
    log(Event::Phase(BuildPhase::Authoring));
    let vob_bytes: f64 = std::fs::read_dir(&work)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("mpg")))
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len() as f64)
                .sum()
        })
        .unwrap_or(expected_bytes);
    let burn_estimate = Some(eta::image_and_burn_secs(eta::Medium::Dvd, vob_bytes, eta::default_speed_x(eta::Medium::Dvd, dual_layer), true, dual_layer));
    log(Event::Eta(Eta::localized(opts.ui_lang, JobPhase::Authoring, None, Some(eta::authoring_secs(vob_bytes)), burn_estimate)));
    log(Event::Step(tr(opts.ui_lang, "authoring_created").into()));
    cancel.check()?;
    let authoring_log = {
        let (work, dest) = (work.clone(), work.join(&layout.dest));
        cancellable(cancel, async move {
            tokio::task::spawn_blocking(move || author::author_dvd(&layout, &work, &dest))
                .await
                .map_err(|e| AuthorError::Native(e.to_string()))?
        })
        .await?
    };
    cancel.check()?;
    std::fs::write(work.join("authoring.log"), authoring_log.0.join("
"))?;
    for line in authoring_log.0.iter().filter(|l| l.contains("WARN")) {
        warn(log, format!("dvdauthor: {}", line.trim()));
    }

    // Project folder named after the project / first video, never overwriting an older one.
    let project_name = safe_folder_name(opts.project_name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&default_project_name));
    let project_dir = unique_dir(&out, &project_name);
    std::fs::create_dir_all(&project_dir).at(&project_dir)?;
    for folder in ["VIDEO_TS", "AUDIO_TS"] {
        let from = work.join("DVD").join(folder);
        if from.is_dir() {
            std::fs::rename(&from, project_dir.join(folder)).at(&from)?;
        }
    }
    let video_ts = project_dir.join("VIDEO_TS");
    log(Event::Step(tf!(opts.ui_lang, "dvd_folder", path = video_ts.display())));

    // The work directory (intermediate MPEGs, menu PNGs, logs) is only kept on request.
    let work_dir = if opts.keep_work {
        Some(work)
    } else {
        if let Err(e) = remove_old_output(&work).await {
            warn(log, tf!(opts.ui_lang, "workdir_delete_failed", err = e));
        }
        None
    };

    let size_mb = dir_mb(&video_ts);
    if size_mb > opts.disc.target_mb() as f64 {
        warn(log, tf!(opts.ui_lang, "size_over_dvd", mb = format!("{size_mb:.0}"), target = opts.disc.target_mb()));
    }
    Ok(BuildReport {
        video_ts,
        project_dir,
        work_dir,
        authoring_xml,
        plan,
        titles: reports,
        menu_pages,
        motion_pages,
        menu_sounds,
        resolution,
        size_mb,
        target_mb: opts.disc.target_mb(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn menu_too_complex_is_typed_and_localised() {
        let e = BuildError::from(ToolError::MenuTooComplex { buttons: 12 });
        assert!(matches!(e, BuildError::MenuTooComplex { buttons: 12 }));
        let de = e.message(UiLang::DE);
        assert!(de.contains("12") && !de.contains("engine.") && !de.contains("err_menu_too_complex"), "{de}");
        assert_ne!(e.message(UiLang::EN), de);
        let f = ToolError::Failed { tool: "x".into(), status: String::new(), log: String::new() };
        assert!(matches!(BuildError::from(f), BuildError::Tool(_)));
    }

    use super::*;
    use crate::probe::Chapter;

    fn info(duration: f64, chapters: Vec<(f64, Option<&str>)>) -> MediaInfo {
        MediaInfo {
            path: "x.mkv".into(),
            duration_secs: duration,
            video: None,
            audio: vec![],
            subtitles: vec![],
            chapters: chapters
                .into_iter()
                .map(|(s, t)| Chapter { start_secs: s, end_secs: s + 1.0, title: t.map(str::to_string) })
                .collect(),
            trim: None, audio_names: Default::default(),
        }
    }

    #[test]
    fn work_dir_name_precedence() {
        let out = Path::new("out");
        let name = |o: &BuildOptions| work_dir(out, o).file_name().unwrap().to_string_lossy().into_owned();
        let mut o = BuildOptions { inputs: vec!["x/clip.mkv".into()], ..BuildOptions::default() };
        assert_eq!(name(&o), "clip.convertx-work");
        o.menu_title = Some("Menü".into());
        o.project_name = Some("  ".into());
        assert_eq!(name(&o), "Menü.convertx-work");
        o.project_name = Some("Urlaub 2025".into());
        assert_eq!(name(&o), "Urlaub 2025.convertx-work");
        o.project_name = Some("a/b:c".into());
        assert_eq!(name(&o), "a_b_c.convertx-work");
    }

    #[test]
    fn merge_groups() {
        let sel = |flags: &[bool]| -> Vec<StreamSelection> {
            flags.iter().map(|&m| StreamSelection { merge_with_previous: m, ..Default::default() }).collect()
        };
        assert_eq!(groups(&[], 3), vec![0..1, 1..2, 2..3], "missing selections never merge");
        assert_eq!(groups(&sel(&[true, false, false]), 3), vec![0..1, 1..2, 2..3], "flag on the first input is ignored");
        assert_eq!(groups(&sel(&[false, true, false]), 3), vec![0..2, 2..3]);
        assert_eq!(groups(&sel(&[false, false, true]), 3), vec![0..1, 1..3]);
        assert_eq!(groups(&sel(&[false, true, true]), 3), vec![0..3]);
        assert!(groups(&sel(&[]), 0).is_empty());
    }

    #[test]
    fn merged_members_need_equal_tracks() {
        assert!(check_group_shapes(&[("a", 2, 1), ("b", 2, 1)]).is_ok());
        let e = check_group_shapes(&[("a", 2, 1), ("b", 1, 1)]).unwrap_err().to_string();
        assert!(e.contains("b lässt sich nicht mit a zusammenführen") && e.contains("Tonspuren"), "{e}");
        assert!(check_group_shapes(&[("a", 1, 0), ("b", 1, 2)]).unwrap_err().to_string().contains("Untertitel"));
    }

    #[test]
    fn chapters_from_grid() {
        let c = chapter_starts(&info(1000.0, vec![]), 300.0, 0.0);
        assert_eq!(c.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0.0, 300.0, 600.0, 900.0]);
        // A mark one second before the end is useless.
        let c = chapter_starts(&info(600.5, vec![]), 300.0, 0.0);
        assert_eq!(c.len(), 2);
        assert_eq!(chapter_starts(&info(100.0, vec![]), 0.0, 0.0).len(), 1);
    }

    #[test]
    fn chapters_from_source() {
        let c = chapter_starts(
            &info(
                1000.0,
                vec![(0.0, Some("Intro")), (0.4, None), (200.0, Some("Chapter 02")), (200.5, None), (500.0, Some("Finale")), (999.5, None)],
            ),
            300.0,
            0.0,
        );
        assert_eq!(c.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0.0, 200.0, 500.0]);
        assert_eq!(c[0].1.as_deref(), Some("Intro"));
        assert_eq!(c[1].1, None, "generic titles are dropped");
        assert_eq!(c[2].1.as_deref(), Some("Finale"));
    }

    #[test]
    fn custom_chapter_list() {
        let c = custom_chapters(&[600.0, 0.0, 300.0, 300.4, -5.0, 1000.0], 900.0);
        assert_eq!(c.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0.0, 300.0, 600.0]);
        assert_eq!(custom_chapters(&[], 100.0).len(), 1);
    }

    #[test]
    fn bitrate_plan_matches_calculator() {
        let loads = [
            TitleLoad { duration_secs: 3600.0, audio_kbps: 448 + 192, subtitle_streams: 1 },
            TitleLoad { duration_secs: 1800.0, audio_kbps: 192, subtitle_streams: 0 },
        ];
        assert_eq!(avg_audio_kbps(&loads), 491); // (640*3600 + 192*1800) / 5400
        let plan = bitrate_plan(DiscType::Dvd5, &loads, 1.0, EncodingProfile::Auto).unwrap();
        let overhead = DiscType::Dvd5.default_mux_overhead_kbit() + 8192.0 + 10.0 * 3600.0;
        let direct = calculator::calculate_with_overhead(DiscType::Dvd5, 5400.0, 491, overhead).unwrap();
        assert_eq!(plan.video_kbps, direct.video_kbps);
        assert!(plan.fits());
    }

    #[test]
    fn menu_estimate() {
        assert!((estimate_menu_mb(2, false, 25.0) - 7.0 * 0.15).abs() < 1e-9);
        assert!((estimate_menu_mb(2, true, 25.0) - (7.0 * 0.15 + 4.0 * 5.5)).abs() < 1e-9);
    }

    #[test]
    fn events_serialize_for_the_gui() {
        let j = |e: Event| serde_json::to_string(&e).unwrap();
        assert_eq!(j(Event::Step("x".into())), r#"{"type":"step","data":"x"}"#);
        assert_eq!(
            j(Event::Progress { label: "l".into(), fraction: 0.5 }),
            r#"{"type":"progress","data":{"label":"l","fraction":0.5}}"#
        );
        assert_eq!(
            j(Event::Phase(BuildPhase::Encoding { index: 1, count: 2 })),
            r#"{"type":"phase","data":{"phase":"encoding","index":1,"count":2}}"#
        );
        assert_eq!(j(Event::Phase(BuildPhase::MenuEncoding)), r#"{"type":"phase","data":{"phase":"menuEncoding"}}"#);
        let sel: StreamSelection = serde_json::from_str(r#"{"audio":[1,3],"subtitles":null,"label":"Film","chapters":[0,60]}"#).unwrap();
        assert_eq!(sel.audio, Some(vec![1, 3]));
        assert_eq!(sel.chapters, Some(vec![0.0, 60.0]));
    }

    #[test]
    fn chapter_settings() {
        // Shorter than the minimum length: no grid, only the start.
        assert_eq!(chapter_starts(&info(600.0, vec![]), 120.0, 900.0).len(), 1);
        assert_eq!(chapter_starts(&info(1000.0, vec![]), 300.0, 900.0).len(), 4);
        // Source chapters are kept regardless of the minimum length.
        assert_eq!(chapter_starts(&info(600.0, vec![(0.0, None), (100.0, None)]), 300.0, 900.0).len(), 2);

        let named = ChapterSettings { label_template: Some("Kapitel #<c>".into()), ..ChapterSettings::default() };
        assert_eq!(named.label(3, UiLang::DE), "Kapitel #3");
        assert_eq!(ChapterSettings::default().label(2, UiLang::EN), "Chapter 2");
    }

    #[test]
    fn aspect_policy() {
        assert_eq!(AspectPolicy::Auto.aspect_for(2.35), Aspect::Wide);
        assert_eq!(AspectPolicy::Auto.aspect_for(1.33), Aspect::Standard);
        assert_eq!(AspectPolicy::Standard.aspect_for(2.35), Aspect::Standard);
        assert_eq!(AspectPolicy::Wide.aspect_for(1.33), Aspect::Wide);
    }

    #[test]
    fn lp_switches_to_half_d1_at_low_bitrates() {
        use EncodingProfile::*;
        assert_eq!(effective_resolution(Resolution::FullD1, Lp, 2999), Resolution::HalfD1);
        assert_eq!(effective_resolution(Resolution::FullD1, Lp, 3000), Resolution::FullD1);
        for p in [Auto, Sp, Mp] {
            assert_eq!(effective_resolution(Resolution::FullD1, p, 1500), Resolution::FullD1, "{p:?}");
        }
        // An explicit choice is kept.
        assert_eq!(effective_resolution(Resolution::Sif, Lp, 1500), Resolution::Sif);
        assert_eq!(effective_resolution(Resolution::HalfD1, Auto, 8000), Resolution::HalfD1);
    }

    #[test]
    fn folder_names() {
        assert_eq!(safe_folder_name("Urlaub 2025"), "Urlaub 2025");
        assert_eq!(safe_folder_name("Война и мир"), "Война и мир", "Unicode is fine");
        assert_eq!(safe_folder_name(r#"a/b\c:d*e?f"g<h>i|j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(safe_folder_name("Film... "), "Film");
        assert_eq!(safe_folder_name("CON"), "CON_");
        assert_eq!(safe_folder_name("com1.txt"), "com1.txt_");
        assert_eq!(safe_folder_name("Comet"), "Comet");
        assert_eq!(safe_folder_name("  "), "DVD");
        assert_eq!(safe_folder_name(&"x".repeat(300)).len(), 100);
    }

    #[test]
    fn unique_project_dirs() {
        let base = std::env::temp_dir().join(format!("cx_unique_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(unique_dir(&base, "Film"), base.join("Film"));
        std::fs::create_dir(base.join("Film")).unwrap();
        assert_eq!(unique_dir(&base, "Film"), base.join("Film (2)"));
        std::fs::create_dir(base.join("Film (2)")).unwrap();
        assert_eq!(unique_dir(&base, "Film"), base.join("Film (3)"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cancel_token() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let t = CancelToken::new();
        let clone = t.clone();
        assert!(!t.is_cancelled() && t.check().is_ok());
        let waiter = rt.spawn(async move { clone.cancelled().await });
        std::thread::sleep(std::time::Duration::from_millis(50));
        t.cancel();
        rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await }).unwrap().unwrap();
        assert!(t.is_cancelled());
        assert!(matches!(t.check(), Err(BuildError::Cancelled)));
        // Already cancelled: completes immediately.
        rt.block_on(async { tokio::time::timeout(std::time::Duration::from_millis(100), t.cancelled()).await }).unwrap();
    }

    /// Cancelling the job runner aborts every job and only returns once all of them are dropped
    /// (dropping a job is what kills its child process).
    #[test]
    fn run_limited_cancel_drains_jobs() {
        use std::sync::atomic::AtomicUsize;
        struct DropCount(Arc<AtomicUsize>);
        impl Drop for DropCount {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let cancel = CancelToken::new();
        let jobs: Vec<Job<()>> = (0..6)
            .map(|_| {
                let guard = DropCount(dropped.clone());
                job(move |_| async move {
                    let _guard = guard;
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    Ok(())
                })
            })
            .collect();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            c.cancel();
        });
        let start = std::time::Instant::now();
        let r = rt.block_on(run_limited(2, jobs, &cancel, |_| {}));
        assert!(matches!(r, Err(BuildError::Cancelled)));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(dropped.load(Ordering::SeqCst), 6, "running and queued jobs are all dropped");
    }

    #[test]
    fn chapter_limit() {
        let c = chapter_starts(&info(100_000.0, vec![]), 60.0, 0.0);
        assert_eq!(c.len(), MAX_CHAPTERS);
    }

    #[test]
    fn stream_selection() {
        use crate::probe::{AudioStream, SubtitleStream};
        let a = |index: u32, lang: &str| AudioStream {
            index,
            codec: "ac3".into(),
            channels: 2,
            channel_layout: "stereo".into(),
            language: Some(lang.into()),
            title: None,
            default: false,
            bit_rate: None,
            sample_rate: None,
            profile: None,
        };
        let s = |index: u32, codec: &str, kind: SubtitleKind| SubtitleStream {
            index,
            codec: codec.into(),
            kind,
            language: Some("ger".into()),
            title: None,
            forced: false,
            width: None,
            height: None,
        };
        let mut src = info(60.0, vec![]);
        src.audio = vec![a(1, "eng"), a(2, "ger"), a(3, "fra")];
        src.subtitles = vec![s(4, "subrip", SubtitleKind::Text), s(5, "dvb_subtitle", SubtitleKind::Bitmap)];
        let opts = BuildOptions {
            audio_langs: vec!["de".into(), "en".into()],
            preferred_audio_lang: Some("de".into()),
            ..BuildOptions::default()
        };
        let none = StreamSelection::default();

        // Language filter, preferred language first; the unsupported bitmap is skipped.
        let (audio, subs, notes) = select_streams(&src, &none, &opts, "Film");
        assert_eq!(audio, vec![1, 0]);
        assert_eq!(subs, vec![0]);
        assert!(notes.len() == 1 && notes[0].contains("dvb_subtitle"), "{notes:?}");

        // No language matches: the first track, with a warning.
        let jp = BuildOptions { audio_langs: vec!["ja".into()], ..BuildOptions::default() };
        let (audio, _, notes) = select_streams(&src, &none, &jp, "Film");
        assert_eq!(audio, vec![0]);
        assert!(notes.iter().any(|n| n.contains("keine Tonspur passt")), "{notes:?}");

        // The user's choice wins over the filter.
        let chosen = StreamSelection { audio: Some(vec![3]), subtitles: Some(vec![]), ..StreamSelection::default() };
        let (audio, subs, _) = select_streams(&src, &chosen, &opts, "Film");
        assert_eq!((audio, subs), (vec![2], vec![]));
    }
}
