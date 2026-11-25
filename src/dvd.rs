//! End-to-end DVD build: probe -> plan -> thumbnails -> menus -> bitrate -> encode -> author.
//!
//! Output layout:
//! ```text
//! <output>/<project>/VIDEO_TS/  finished DVD-Video structure ("<project> (2)" if taken)
//! <output>/<project>.musadvd-work/  menu PNGs, spumux/dvdauthor XMLs, title MPEGs, logs;
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
use crate::encoder::{self, plan_audio_kbps, Aspect, AudioMode, AudioOut, AudioTarget, DvdVideoMode, EncodeJob, EncodePhase, MenuAudio, VideoProcessing};
use crate::menu_anim;
use crate::config::HwAccelMode;
use crate::i18n::tr;
use crate::tf;
use crate::menu::{self, page_by_style, MenuError, MenuPageFiles, RenderOptions, RenderedMenu, TextOverflow, UiLang};
use crate::probe::{self, language_matches, MediaInfo, ProbeError, SubtitleKind, TrackLabelMode};
pub use crate::probe::{ExternalSubtitle, EXTERNAL_SUB_SOURCE_BASE};
use crate::template::{Page, Template};
use crate::tools::{ProcessPriority, RawStream, ToolError, Toolchain};

/// DVD-Video limits per title.
pub const MAX_AUDIO_STREAMS: usize = 8;
pub const MAX_SUBTITLE_STREAMS: usize = 32;
/// Bitrate reserved per text subtitle stream for the size calculation.
const SUBTITLE_KBPS: u32 = 10;
/// The same on Blu-ray/AVCHD: text rendered to 1080p PGS measured ~32 kbit/s per track
/// (four 24-min anime episodes, 5.8-6.2 MB each).
const BD_SUBTITLE_KBPS: u32 = 40;
/// BDAV (192-byte TS packets, PES headers, PCR/PAT/PMT) on top of the muxed streams: measured
/// 7.42 % over the encoded files (7.7 % of the raw streams) on four AVCHD episodes at ~5.6 Mbit/s.
pub const BD_STREAM_OVERHEAD_RATIO: f64 = 0.08;
/// Size reserve for hardware video encoders (NVENC/QSV/AMF), whose average bitrate is less exact
/// than x264's 2-pass: the measured NVENC run came in 2.9 % under, so the reserve is the 0.5 % buffer.
pub const HW_ENCODER_RESERVE: f64 = 0.005;
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
            BuildError::Cancelled => write!(f, "Conversion cancelled"),
            BuildError::DoesNotFit { disc, plan } => write!(
                f,
                "content does not fit on {disc:?}: even at the minimum of {} kbps it needs {:.0} MB of {} MB \
                 (would need {:.0} kbps). Use {}, fewer audio tracks or shorter inputs.",
                plan.video_kbps,
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
pub const WORK_DIR: &str = ".musadvd-work";

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

/// Temporary authoring directory `<out>/<project name>.musadvd-work`. It is created before
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

/// Windows errors that clear up by themselves: access denied (virus scanner, indexer), sharing and
/// lock violation, directory not empty (a handle of an already deleted child is still closing).
fn transient_delete_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33 | 145))
}

/// Deletes a directory tree, retrying `retries` times every `delay` on [`transient_delete_error`]s.
/// A missing directory counts as deleted.
async fn remove_dir_retry(dir: &Path, retries: u32, delay: std::time::Duration) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt < retries && transient_delete_error(&e) => {
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Deletes the previous DVD output or a work directory. Retries briefly: right after a build,
/// virus scanners or the Windows Explorer may still hold the freshly written VOBs open
/// (os error 32 / 5).
pub(crate) async fn remove_old_output(dir: &Path) -> Result<(), BuildError> {
    match remove_dir_retry(dir, 10, std::time::Duration::from_millis(500)).await {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(32) => Err(BuildError::Template(format!(
            "{}: the old DVD is still open in another program (e.g. VLC or Explorer).              Close it or choose a different --output.",
            dir.display()
        ))),
        Err(e) => Err(BuildError::File { path: dir.to_path_buf(), error: e }),
    }
}

/// Keep the work directory after a successful build: `--keep-work` / `keep_work`, or the
/// environment variable `MUSADVD_KEEP_WORK=1` (also for the GUI, which has no such switch).
pub(crate) fn keep_work(opts: &BuildOptions) -> bool {
    opts.keep_work || std::env::var("MUSADVD_KEEP_WORK").is_ok_and(|v| !matches!(v.trim(), "" | "0"))
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

/// DVD aspect ratio of the titles (the UI's "aspect ratio").
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
    /// Name of chapters without a title; `<c>` = chapter number. `None` = the localized "Chapter N".
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
    /// Start behaviour and what happens after a title (the UI's "menu options").
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
    /// AC-3 bit rate of re-encoded tracks in kbit/s (0 = automatic); snapped per medium at plan time.
    pub audio_bitrate_kbps: u32,
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
    /// Blu-ray / AVCHD: re-encode the video, copy compliant streams, or copy all of them.
    pub bd_video: crate::encoder::BdVideoMode,
    /// DVD-Video: re-encode the video, copy compliant MPEG-2 streams, or copy all of them.
    pub dvd_video: crate::encoder::DvdVideoMode,
    /// Blu-ray / AVCHD: encode every audio track, copy compliant ones, or copy all of them.
    pub bd_audio: crate::encoder::BdAudioMode,
    /// Lets another thread cancel the build.
    pub cancel: CancelToken,
    /// Menus from the menu editor instead of the ConvertX template (`template` is unused then).
    pub custom_layout: Option<crate::layout::CustomLayout>,
}

impl BuildOptions {
    /// The menu-editor layout with the global menu sound file (Settings / `--menu-audio`) filled into
    /// every silent menu when motion menus are on, so custom menus honour it like templates do.
    pub(crate) fn custom_layout_with_menu_audio(&self) -> Option<std::borrow::Cow<'_, crate::layout::CustomLayout>> {
        let layout = self.custom_layout.as_ref()?;
        Some(match &self.motion_audio {
            MenuAudio::File(path) if self.motion_menu => {
                let mut l = layout.clone();
                crate::layout::apply_default_menu_audio(&mut l, path);
                std::borrow::Cow::Owned(l)
            }
            _ => std::borrow::Cow::Borrowed(layout),
        })
    }
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
    /// Blu-ray only: frame size (`1080`/`720`), frame rate name ([`crate::bd::BdRate::from_name`])
    /// and video handling (`encode`/`copy`) of this title instead of the project's.
    #[serde(default)]
    pub bd_resolution: Option<String>,
    #[serde(default)]
    pub bd_rate: Option<String>,
    #[serde(default)]
    pub bd_video: Option<String>,
    /// DVD-Video only: video handling (`encode`/`auto`/`copy`/`strict`) of this title instead of the project's.
    #[serde(default)]
    pub dvd_video: Option<String>,
    /// Blu-ray only: quality weight of the encoded video (`low` / `high`, else normal), see [`quality_weight`].
    #[serde(default)]
    pub bd_quality: Option<String>,
    /// End action of this title (overrides the disc-wide playback options).
    #[serde(default)]
    pub after_playback: crate::author::AfterPlayback,
    /// Per-title picture adjustments (override project defaults when `Some`).
    #[serde(default)]
    pub denoise: Option<f32>,
    #[serde(default)]
    pub brightness: Option<f32>,
    #[serde(default)]
    pub contrast: Option<f32>,
    #[serde(default)]
    pub saturation: Option<f32>,
    /// Styling and timing offset of this title's text subtitles.
    #[serde(default)]
    pub subtitle_style: crate::textsub::SubtitleStyle,
    /// Standalone external subtitle files attached to this title.
    #[serde(default)]
    pub external_subtitles: Vec<ExternalSubtitle>,
    /// Disc order of all subtitles, embedded and external mixed: stream indices and
    /// `EXTERNAL_SUB_SOURCE_BASE + i` (`i` into `external_subtitles`). Unlisted tracks follow.
    #[serde(default)]
    pub subtitle_order: Option<Vec<u32>>,
}

/// Disc order of a title's subtitles as positions into `natural` (embedded stream indices, then
/// `EXTERNAL_SUB_SOURCE_BASE + i`), sorted by `wanted`; unlisted tracks keep their order at the end.
pub(crate) fn subtitle_order(natural: &[u32], wanted: Option<&[u32]>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..natural.len()).collect();
    if let Some(w) = wanted {
        order.sort_by_key(|&m| w.iter().position(|&s| s == natural[m]).unwrap_or(usize::MAX));
    }
    order
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

/// At most [`MAX_TITLES`] titles. Merged inputs play as one title, so the limit is on the merge
/// groups, not on the number of files.
pub(crate) fn check_title_count(opts: &BuildOptions) -> Result<(), BuildError> {
    let titles = groups(&opts.selections, opts.inputs.len()).len();
    if titles > MAX_TITLES {
        return Err(BuildError::Author(AuthorError::TooManyTitles(titles)));
    }
    Ok(())
}

/// One merged member: label, audio track count, subtitle track count.
pub(crate) type MemberShape<'a> = (&'a str, usize, usize);

/// Merged files play as one title, so they need the same audio and subtitle track layout.
pub(crate) fn check_group_shapes(members: &[MemberShape]) -> Result<(), BuildError> {
    let Some(&(first, audio, subs)) = members.first() else { return Ok(()) };
    for &(label, a, s) in &members[1..] {
        if a != audio {
            return Err(BuildError::Template(format!(
                "{label} cannot be merged with {first}: {a} instead of {audio} audio tracks (align the tracks in the title settings)"
            )));
        }
        if s != subs {
            return Err(BuildError::Template(format!(
                "{label} cannot be merged with {first}: {s} instead of {subs} subtitle tracks (align the tracks in the title settings)"
            )));
        }
    }
    Ok(())
}

/// Makes the members of a merged DVD title compatible: all use the first member's aspect and,
/// if the audio formats differ at track index k, only track k across all members is re-encoded to AC-3.
/// Compliant tracks matching across all members remain stream-copied.
fn harmonize_group(members: &mut [TitlePlan]) {
    let Some(first) = members.first() else { return };
    let aspect = first.aspect;
    let track_count = first.audio.len();
    for m in members.iter_mut() {
        m.aspect = aspect;
    }
    for k in 0..track_count {
        let first_format = members[0].audio_outs().get(k).map(|o| o.dvdauthor_format());
        let same = members.iter().all(|m| m.audio_outs().get(k).map(|o| o.dvdauthor_format()) == first_format);
        if !same {
            for m in members.iter_mut() {
                if let Some(mode) = m.audio_modes.get_mut(k) {
                    *mode = AudioMode::Ac3;
                }
            }
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
    /// Blu-ray title whose video is stream-copied at this average kbit/s: a fixed size that is
    /// taken off the disc before the encoded titles share the rest.
    #[serde(default)]
    pub copy_video_kbps: Option<u32>,
    /// Exact source file size in bytes for DVD stream-copied titles (`None` = use bitrate * duration).
    #[serde(default)]
    pub copy_bytes: Option<u64>,
    /// Quality weight of an encoded title: its video bitrate relative to the disc's base bitrate
    /// ([`QUALITY_LOW`] / 1.0 / [`QUALITY_HIGH`]).
    #[serde(default = "one")]
    pub weight: f64,
}

fn one() -> f64 {
    1.0
}

/// Per-title quality weights of the Video tab.
pub const QUALITY_LOW: f64 = 0.75;
pub const QUALITY_HIGH: f64 = 1.25;

/// Weight of a Video-tab quality choice (`low` / `high`, anything else is normal).
pub fn quality_weight(name: Option<&str>) -> f64 {
    match name {
        Some("low") => QUALITY_LOW,
        Some("high") => QUALITY_HIGH,
        _ => 1.0,
    }
}

/// Lowest video bitrate of a title at `res`: smaller frames keep the same bits per pixel far
/// lower (Half D1 / SIF have a half / a quarter of Full D1's pixels, 720p under half of 1080p), so
/// long collections fit one disc. Blu-ray itself sets no minimum; these floors keep H.264 watchable.
pub fn min_video_kbps(disc: DiscType, res: Resolution) -> u32 {
    let floor = disc.limits().min_video_kbps;
    match res {
        Resolution::HalfD1 if !disc.is_bluray() => floor.min(600),
        Resolution::Sif if !disc.is_bluray() => floor.min(400),
        Resolution::Hd720 if disc.is_bluray() => floor.min(1500),
        Resolution::Hd1080 if disc.is_bluray() => floor.min(2500),
        _ => floor,
    }
}

/// Video bitrate of an encoded title: the plan's base bitrate times the title's weight, kept between
/// `min_video` ([`min_video_kbps`]) and the disc's ceiling beside the title's audio.
pub fn weighted_video_kbps(disc: DiscType, base_kbps: u32, weight: f64, audio_kbps: u32, min_video: u32) -> u32 {
    let l = disc.limits();
    let top = l.max_video_kbps.min(l.max_total_kbps.saturating_sub(audio_kbps));
    ((base_kbps as f64 * weight).round() as u32).min(top).max(min_video.min(top))
}

/// Video bitrate for `titles` on `disc`, with `menu_mb` of menus already on the disc, limited by
/// `profile`, with the disc's bitrate floor ([`bitrate_plan_at`] for a frame-size floor).
pub fn bitrate_plan(disc: DiscType, titles: &[TitleLoad], menu_mb: f64, profile: EncodingProfile) -> Result<BitratePlan, CalcError> {
    bitrate_plan_at(disc, titles, menu_mb, profile, disc.limits().min_video_kbps)
}

/// Size kept free on `disc` (MB) when a hardware H.264 encoder is selected ([`HW_ENCODER_RESERVE`]);
/// the planner treats it like menus. Blu-ray/AVCHD only: DVD-Video always encodes on the CPU.
pub fn hw_reserve_mb(disc: DiscType, hw_accel: HwAccelMode) -> f64 {
    if disc.is_bluray() && hw_accel.bd_encoder().is_some() {
        disc.target_mb() as f64 * HW_ENCODER_RESERVE
    } else {
        0.0
    }
}

/// [`bitrate_plan`] with the video bitrate floor `min_video` ([`min_video_kbps`]). Used by the
/// build itself and by the GUI's live estimate.
pub fn bitrate_plan_at(disc: DiscType, titles: &[TitleLoad], menu_mb: f64, profile: EncodingProfile, min_video: u32) -> Result<BitratePlan, CalcError> {
    let sub_kbps = if disc.is_bluray() { BD_SUBTITLE_KBPS } else { SUBTITLE_KBPS };
    let mut subs_kbit: f64 = titles.iter().map(|t| (t.subtitle_streams as u32 * sub_kbps) as f64 * t.duration_secs).sum();
    let (copied, encoded): (Vec<TitleLoad>, Vec<TitleLoad>) =
        titles.iter().partition(|t| t.copy_video_kbps.is_some() || t.copy_bytes.is_some());
    let mut copied_kbit: f64 = copied
        .iter()
        .map(|t| match t.copy_bytes {
            Some(bytes) => calculator::bytes_to_kbit(bytes),
            None => (t.copy_video_kbps.unwrap_or(0) + t.audio_kbps) as f64 * t.duration_secs,
        })
        .sum();
    // Both grow with the streams: DVD-Video by its NAV packs per second and pack/PES headers (see
    // `dvd_mux_kbit`), Blu-ray by the BDAV transport stream around every muxed stream – also the
    // copied and subtitle ones, which are re-muxed too.
    let all_secs: f64 = titles.iter().map(|t| t.duration_secs).sum();
    let (disc_share, ratio) = if disc.is_bluray() {
        subs_kbit *= 1.0 + BD_STREAM_OVERHEAD_RATIO;
        copied_kbit *= 1.0 + BD_STREAM_OVERHEAD_RATIO;
        (0.0, BD_STREAM_OVERHEAD_RATIO)
    } else {
        (calculator::DVD_NAV_KBPS * all_secs + calculator::DVD_IFO_KBIT_PER_TITLE * titles.len() as f64, calculator::DVD_PACK_OVERHEAD_RATIO)
    };
    // Everything but the encoded titles' own stream overhead (`ratio` × their audio + video).
    let overhead = disc_share + menu_mb * 8192.0 + subs_kbit + copied_kbit;
    if encoded.is_empty() && !copied.is_empty() {
        // Nothing left to encode: the copies either fit or they do not.
        let estimated_mb = overhead / 8192.0;
        let fits = estimated_mb <= disc.target_mb() as f64;
        return Ok(BitratePlan {
            disc,
            duration_secs: 0.0,
            audio_kbps: 0,
            mux_overhead_kbit: overhead,
            raw_video_kbps: 0.0,
            video_kbps: 0,
            clamp: if fits { Clamp::None } else { Clamp::Min },
            estimated_mb,
        });
    }
    // Weighted titles share the video budget in proportion to weight × duration: the calculator
    // sees the weighted duration, with the audio average scaled so the audio size stays exact.
    let weighted_secs: f64 = encoded.iter().map(|t| t.duration_secs * t.weight).sum();
    let audio_kbit: f64 = encoded.iter().map(|t| t.audio_kbps as f64 * t.duration_secs).sum();
    let audio_kbps = (audio_kbit / weighted_secs.max(1.0)).ceil() as u32;
    // The stream overhead depends on the bitrate being solved: fold it into the calculator's
    // fixed overhead at the bitrate that exactly fills the disc (`overhead + (1+ratio)·stream = target`).
    let full_stream_kbit = (disc.target_kbit() - overhead).max(0.0) / (1.0 + ratio);
    let plan = calculator::calculate_floored(disc, weighted_secs, audio_kbps, overhead + ratio * full_stream_kbit, profile.max_video_kbps_for(disc), min_video)?;
    // The calculator assumes every title gets `base * weight`, but `weighted_video_kbps` pins
    // titles at the disc's floor/ceiling. Re-solve the base against the real per-title sizes.
    let real_kbit = |base: u32| -> f64 {
        let stream: f64 = encoded.iter().map(|t| (weighted_video_kbps(disc, base, t.weight, t.audio_kbps, min_video) + t.audio_kbps) as f64 * t.duration_secs).sum();
        overhead + (1.0 + ratio) * stream
    };
    let with_real = |plan: BitratePlan| {
        let total = real_kbit(plan.video_kbps);
        let stream = (total - overhead) / (1.0 + ratio);
        BitratePlan { mux_overhead_kbit: total - stream, estimated_mb: total / 8192.0, ..plan }
    };
    // Unweighted titles are exactly what the calculator sized (its floor/ceiling clamp is the same);
    // only the size is recounted, since a clamped bitrate carries less stream overhead.
    if encoded.iter().all(|t| t.weight == 1.0) {
        return Ok(with_real(plan));
    }
    let target = disc.target_kbit();
    let (base, clamp) = if real_kbit(0) > target {
        // Even every title at its floor overflows: report that honestly (the floor base also keeps
        // `weighted_video_kbps` at the floor for each title).
        let max_w = encoded.iter().map(|t| t.weight).fold(1.0, f64::max);
        (((min_video as f64 / max_w).floor() as u32).min(plan.video_kbps), Clamp::Min)
    } else {
        // real_kbit is monotone in the base: largest base that still fits.
        let (mut lo, mut hi) = (0, plan.video_kbps);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if real_kbit(mid) <= target { lo = mid } else { hi = mid - 1 }
        }
        // Still at the calculator's base: keep its verdict (e.g. capped at the maximum).
        (lo, if lo == plan.video_kbps { plan.clamp } else { Clamp::None })
    };
    Ok(with_real(BitratePlan { video_kbps: base, clamp, ..plan }))
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

/// Estimated size of custom DVD menus from the menu editor before they are rendered:
/// still pages cost about 0.15 MB, moving pages (video or audio) cost about 5.5 MB per 25 s.
pub fn estimate_custom_layout_mb(layout: &crate::layout::CustomLayout, default_secs: f64) -> f64 {
    let menus_mb: f64 = layout
        .menus
        .iter()
        .map(|m| {
            let moving = m.background.video.is_some()
                || m.elements.iter().any(|e| matches!(e.content, crate::layout::Content::Video { .. }))
                || m.audio != crate::layout::MenuSound::None;
            if moving {
                let secs = m.duration_secs.unwrap_or(default_secs).clamp(1.0, 600.0);
                let intro = if layout.animations { m.intro_secs.unwrap_or(0.0).clamp(0.0, 60.0) } else { 0.0 };
                (secs + intro) * 5.5 / 25.0
            } else {
                0.15
            }
        })
        .sum();
    let fp_mb = match layout.fast_play.as_ref().filter(|fp| fp.enabled) {
        Some(fp) => {
            let moving = fp.splash.as_ref().is_some_and(|s| s.video.is_some()) || fp.audio != crate::layout::MenuSound::None;
            if moving { (fp.timeout_secs as f64) * 5.5 / 25.0 } else { 0.15 }
        }
        None => 0.0,
    };
    let trans_mb: f64 = layout
        .menus
        .iter()
        .flat_map(|m| &m.elements)
        .filter_map(|e| match &e.action {
            Some(crate::layout::Action::Menu { transition: Some(tr), .. }) => Some(tr.duration_secs * 5.5 / 25.0),
            _ => None,
        })
        .sum();
    menus_mb + fp_mb + trans_mb
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
    /// Standalone external subtitle files, numbered after `subtitles`.
    external_subtitles: Vec<ExternalSubtitle>,
    /// Disc track `k` plays natural subtitle `sub_order[k]` (`subtitles`, then `external_subtitles`).
    sub_order: Vec<usize>,
    /// Chapter starts in seconds, first is 0.
    chapters: Vec<f64>,
    chapter_titles: Vec<Option<String>>,
    aspect: Aspect,
    audio_mode: AudioMode,
    audio_modes: Vec<AudioMode>,
    ac3_kbps: u32,
    track_label_mode: TrackLabelMode,
    ui_lang: UiLang,
    video: VideoProcessing,
    loudnorm: bool,
    /// Styling and timing offset of the text subtitles ([`StreamSelection::subtitle_style`]).
    subtitle_style: crate::textsub::SubtitleStyle,
    video_mode: DvdVideoMode,
    /// Video is stream-copied at this average kbit/s ([`encoder::dvd_video_copy`]).
    copy_kbps: Option<u32>,
    /// Source file byte size of a stream-copied title for exact bitrate budget deduction.
    copy_bytes: Option<u64>,
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
        self.audio
            .iter()
            .enumerate()
            .map(|(idx, &i)| {
                let mode = self.audio_modes.get(idx).copied().unwrap_or(self.audio_mode);
                plan_audio_kbps(&self.info.audio[i], AudioTarget::Dvd, mode, self.loudnorm || self.info.trim.is_some(), self.ac3_kbps)
            })
            .collect()
    }

    fn audio_kbps(&self) -> u32 {
        self.audio_outs().iter().map(|o| o.kbps()).sum()
    }

    fn audio_labels(&self) -> Vec<String> {
        let all = probe::audio_labels(&self.info, self.track_label_mode, self.ui_lang);
        self.audio.iter().zip(self.audio_outs()).map(|(&i, out)| probe::with_layout(&all[i], out.layout_label(), self.track_label_mode)).collect()
    }

    fn subtitle_labels(&self) -> Vec<String> {
        let all = probe::all_subtitle_labels(&self.info, &self.external_subtitles, self.track_label_mode, self.ui_lang);
        let emb = self.subtitles.iter().map(|&i| all[i].clone());
        let ext_start = self.info.subtitles.len();
        let ext = (0..self.external_subtitles.len()).map(|i| all[ext_start + i].clone());
        self.in_disc_order(emb.chain(ext).collect())
    }

    fn subtitle_sources(&self) -> Vec<u32> {
        let emb = self.subtitles.iter().map(|&s| self.info.subtitles[s].index);
        let ext = (0..self.external_subtitles.len()).map(|i| EXTERNAL_SUB_SOURCE_BASE + i as u32);
        self.in_disc_order(emb.chain(ext).collect())
    }

    fn subtitle_langs_raw(&self) -> impl Iterator<Item = Option<&str>> {
        let emb = self.subtitles.iter().map(|&s| self.info.subtitles[s].language.as_deref());
        let ext = self.external_subtitles.iter().map(|e| e.language.as_deref());
        self.in_disc_order(emb.chain(ext).collect()).into_iter()
    }

    /// Reorders a natural-order subtitle list (embedded, then external) into disc order.
    fn in_disc_order<T: Clone>(&self, natural: Vec<T>) -> Vec<T> {
        self.sub_order.iter().map(|&m| natural[m].clone()).collect()
    }

    /// Disc subtitle stream id of natural subtitle `m` (for the encoder).
    fn sub_stream_ids(&self) -> Vec<u32> {
        let mut ids = vec![0; self.sub_order.len()];
        for (k, &m) in self.sub_order.iter().enumerate() {
            ids[m] = k as u32;
        }
        ids
    }

    fn total_subtitles(&self) -> usize {
        self.subtitles.len() + self.external_subtitles.len()
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
                Some(Err(e)) => return Err(BuildError::Internal(format!("Background job cancelled: {e}"))),
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

/// Parameters for encoding one DVD title via [`encode_title`].
struct EncodeTitleParams<'a> {
    tools: &'a Toolchain,
    title: &'a TitlePlan,
    work: &'a Path,
    video_kbps: u32,
    two_pass: bool,
    fast: bool,
    hw_decode: Option<HwAccelMode>,
    tv: TvStandard,
    resolution: Resolution,
    progress: JobProgress,
}

/// Encodes one title straight into its DVD program stream (subtitles via piped spumux);
/// reports its own progress (0..1) with frame telemetry.
async fn encode_title(p: EncodeTitleParams<'_>) -> Result<(TitleReport, Option<&'static str>), BuildError> {
    let EncodeTitleParams { tools, title: t, work, video_kbps, two_pass, fast, hw_decode, tv, resolution, progress } = p;
    let copy_video = t.copy_kbps.is_some();
    let hwaccel = match hw_decode.filter(|_| !copy_video) {
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
        audio_modes: t.audio_modes.clone(),
        ac3_kbps: t.ac3_kbps,
        video: t.video,
        audio_filters,
        copy_video,
        subtitle_style: t.subtitle_style.clone(),
    };
    let frames_per_run = t.info.duration_secs * output_fps(tv);
    // Frames done count over both runs of a 2-pass encode; never let them go backwards (the
    // closing report of a run carries no frame number).
    let mut frames_seen = 0.0f64;
    let logs = encoder::run_encode_with_external(tools, &job, &t.external_subtitles, &t.sub_stream_ids(), |phase, p| {
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
    if let Some(chosen) = &sel.audio {
        audio.sort_by_key(|&a| chosen.iter().position(|&c| c == info.audio[a].index));
    }
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
        if let Some(chosen) = &sel.subtitles {
            subtitles.sort_by_key(|&n| chosen.iter().position(|&c| c == info.subtitles[n].index));
        }
        subtitles.truncate(MAX_SUBTITLE_STREAMS);
    }
    // An explicit selection's order is the disc's track order (the GUI lets the user drag it);
    // otherwise the source order. Preferred languages are activated on playback via VM commands.
    (audio, subtitles, notes)
}

/// Languages whose text subtitles need bidi/shaping, which [`crate::textsub`] does not do.
fn is_rtl_language(lang: &str) -> bool {
    matches!(lang.to_ascii_lowercase().as_str(), "ar" | "ara" | "he" | "heb" | "fa" | "fas" | "per" | "ur" | "urd" | "yi" | "yid")
}

/// Validates and caps external subtitles for one title (respecting `opts.no_subtitles` and
/// `MAX_SUBTITLE_STREAMS`), returning the accepted external subtitles and any warnings.
pub(crate) fn select_external_subtitles(
    sel: &StreamSelection,
    opts: &BuildOptions,
    embedded_count: usize,
    label: &str,
) -> Result<(Vec<ExternalSubtitle>, Vec<String>), BuildError> {
    if opts.no_subtitles || sel.external_subtitles.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut notes = Vec::new();
    let room = MAX_SUBTITLE_STREAMS.saturating_sub(embedded_count);
    let mut out = Vec::new();
    for (i, ext) in sel.external_subtitles.iter().enumerate() {
        if out.len() >= room {
            notes.push(format!("{label}: more than {MAX_SUBTITLE_STREAMS} subtitles selected, ignoring extra external subtitle(s)"));
            break;
        }
        let probed = probe::probe_external_subtitle(&ext.path)
            .map_err(|e| BuildError::Template(format!("{label}: {e}")))?;
        if probed.subtitle_kind() == SubtitleKind::Text && !opts.disc.is_bluray() && ext.language.as_deref().is_some_and(is_rtl_language) {
            notes.push(tf!(
                opts.ui_lang,
                "note_rtl_subtitle",
                label = label,
                index = EXTERNAL_SUB_SOURCE_BASE + i as u32,
                lang = ext.language.as_deref().unwrap_or("")
            ));
        }
        out.push(ext.clone());
    }
    Ok((out, notes))
}

/// Builds a complete DVD-Video structure from `opts.inputs`.
///
/// When `opts.cancel` fires, all running tools are stopped, the work directory is removed and
/// [`BuildError::Cancelled`] is returned.
pub async fn build_dvd(opts: &BuildOptions, log: &mut (dyn FnMut(Event) + Send)) -> Result<BuildReport, BuildError> {
    let result = if opts.disc.is_bluray() { crate::bd::build_bd(opts, log).await } else { build_dvd_inner(opts, log).await };
    if matches!(result, Err(BuildError::Cancelled)) {
        log(Event::Step(tr(opts.ui_lang, "cancelled_cleanup")));
        if let Ok(out) = std::fs::canonicalize(&opts.output) {
            let work = work_dir(&display_path(out), opts);
            if work.join(WORK_DIR).is_file() {
                if let Err(e) = remove_old_output(&work).await {
                    log(Event::Warning(tf!(opts.ui_lang, "workdir_delete_failed", err = e)));
                }
            }
        }
    }
    if let (Err(e), Ok(out)) = (&result, std::fs::canonicalize(&opts.output)) {
        let work = work_dir(&display_path(out), opts);
        if !matches!(e, BuildError::Cancelled) && work.join(WORK_DIR).is_file() {
            log(Event::Step(format!("Build failed, work folder kept: {}", work.display())));
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
    check_title_count(opts)?;
    if opts.resolution.is_hd() {
        return Err(BuildError::Template(format!(
            "Resolution {} only exists for Blu-ray – for a DVD choose Full D1, Half D1 or SIF",
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
            "{} exists but was not created by this program – rename it or choose another output folder",
            work.display()
        )));
    }
    std::fs::create_dir_all(&work).at(&work)?;
    std::fs::write(work.join(WORK_DIR), "temporary files of musadvd, safe to delete\n").at(&work)?;
    let tv = opts.tv_standard;
    if tv == TvStandard::Ntsc {
        log(Event::Step(tr(opts.ui_lang, "tv_ntsc")));
    }
    if opts.audio_mode == AudioMode::Lpcm {
        warn(log, tr(opts.ui_lang, "lpcm_dvd_warn"));
    }

    // 1. Probe and select streams ------------------------------------------------------------
    log(Event::Phase(BuildPhase::Probe));
    let mut titles: Vec<TitlePlan> = Vec::new();
    for (i, input) in opts.inputs.iter().enumerate() {
        log(Event::Step(tf!(opts.ui_lang, "analyzing", path = input.display())));
        let mut info = cancellable(cancel, probe::probe(&tools.ffprobe, input)).await?;
        let sel = opts.selections.get(i).cloned().unwrap_or_default();
        let label = sel.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
            input.file_stem().map_or_else(|| format!("Title {}", i + 1), |s| s.to_string_lossy().into_owned())
        });

        let (audio, subtitles, notes) = select_streams(&info, &sel, opts, &label);
        for note in notes {
            warn(log, note);
        }
        let (external_subtitles, ext_notes) = select_external_subtitles(&sel, opts, subtitles.len(), &label)?;
        for note in ext_notes {
            warn(log, note);
        }
        apply_selection(&mut info, &sel, &label)?;
        let natural: Vec<u32> = subtitles
            .iter()
            .map(|&s| info.subtitles[s].index)
            .chain((0..external_subtitles.len()).map(|i| EXTERNAL_SUB_SOURCE_BASE + i as u32))
            .collect();
        let sub_order = subtitle_order(&natural, sel.subtitle_order.as_deref());

        let chapters = match &sel.chapters {
            Some(starts) => custom_chapters(starts, info.duration_secs),
            None => chapter_starts(&info, opts.chapters.interval_secs, opts.chapters.min_video_secs),
        };
        let aspect = opts.aspect_policy.aspect_for(info.video.as_ref().map_or(16.0 / 9.0, |v| v.display_aspect));
        let audio_modes = vec![opts.audio_mode; audio.len()];
        let video_mode = match sel.dvd_video.as_deref().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("encode") => DvdVideoMode::Encode,
            Some("auto") => DvdVideoMode::Auto,
            Some("copy" | "strict") => DvdVideoMode::Strict,
            _ => opts.dvd_video,
        };
        titles.push(TitlePlan {
            label,
            audio,
            subtitles,
            external_subtitles,
            sub_order,
            chapters: chapters.iter().map(|c| c.0).collect(),
            chapter_titles: chapters.into_iter().map(|c| c.1).collect(),
            aspect,
            audio_mode: opts.audio_mode,
            audio_modes,
            ac3_kbps: opts.audio_bitrate_kbps,
            track_label_mode: opts.track_label_mode,
            ui_lang: opts.ui_lang,
            video: opts.video.with_picture(sel.denoise, sel.brightness, sel.contrast, sel.saturation),
            loudnorm: opts.loudnorm,
            subtitle_style: sel.subtitle_style.clone(),
            video_mode,
            copy_kbps: None,
            copy_bytes: None,
            motion_secs: opts.motion_duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(menu::DEFAULT_MOTION_SECS),
            stem: format!("title_{:02}", i + 1),
            thumbnail: None,
            chapter_thumbs: Vec::new(),
            info,
        });
    }

    let groups = groups(&opts.selections, titles.len());
    for g in &groups {
        let shapes: Vec<MemberShape> = titles[g.clone()].iter().map(|t| (t.label.as_str(), t.audio.len(), t.total_subtitles())).collect();
        check_group_shapes(&shapes)?;
        let chapters: usize = titles[g.clone()].iter().map(|t| t.chapters.len()).sum();
        if chapters > MAX_CHAPTERS {
            return Err(BuildError::Template(format!(
                "{}: {chapters} chapters after merging, a DVD allows {MAX_CHAPTERS} per title",
                titles[g.start].label
            )));
        }
        harmonize_group(&mut titles[g.clone()]);
    }

    for t in titles.iter_mut().filter(|t| t.video_mode != DvdVideoMode::Encode) {
        let max = encoder::MAX_VIDEO_PEAK_KBPS.min(DVD_MAX_TOTAL_KBPS.saturating_sub(t.audio_kbps()));
        match encoder::dvd_video_copy(&t.info, tv, opts.resolution, t.aspect, t.video, max) {
            Ok(copy) => {
                t.copy_kbps = Some(copy.kbps);
                t.copy_bytes = Some(copy.bytes);
                log(Event::Step(tf!(opts.ui_lang, "bd_video_copy", label = t.label, kbps = copy.kbps)));
            }
            Err(why) if t.video_mode == DvdVideoMode::Strict => {
                return Err(BuildError::Template(format!("{}: the video cannot be copied – {why}", t.label)));
            }
            Err(why) => warn(log, tf!(opts.ui_lang, "bd_video_copy_fallback", label = t.label, reason = why)),
        }
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

    let sounded = opts.custom_layout_with_menu_audio();
    let stage: MenuStage = if let Some(layout) = sounded.as_ref() {
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
                audio_sources: t.audio.iter().map(|&a| t.info.audio[a].index).collect(),
                subtitle_sources: t.subtitle_sources(),
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
        log(Event::Step(tr(opts.ui_lang, "render_menus")));
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
                    return Err(BuildError::Template(format!("Menu audio file not found: {}", path.display())));
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
            type MenuJob<'a> = (Option<&'a Page>, Option<u32>, MenuKind<'a>, Option<&'a PathBuf>, String, &'a str);
            let jobs: [MenuJob<'_>; 3] = [
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
                rendered.push(menu::render_menu(&menu::RenderMenuParams {
                    template,
                    page,
                    kind: &kind,
                    links: MenuLinks { first, ..links },
                    entry: Some(entry),
                    prefix: &prefix,
                    out_dir: &work,
                    opts: &ropts,
                })?);
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
            let mut main_menu = menu::render_menu(&menu::RenderMenuParams {
                template: &main,
                page: title_page,
                kind: &MenuKind::Titles(&entries),
                links: MenuLinks {
                    first: 1,
                    bonus: bonus_page.map(|_| main_total + 1),
                    episodes: episodes_page.map(|_| main_total + 1),
                    title1_chapters: entry_of("ts01_chap").filter(|_| no_items),
                    title1_settings: entry_of("ts01_set").filter(|_| no_items),
                    ..MenuLinks::default()
                },
                entry: Some("title"),
                prefix: "vmgm",
                out_dir: &work,
                opts: &RenderOptions {
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
            })?;
            if let Some(page) = episodes_page {
                let episodes = menu::render_menu(&menu::RenderMenuParams {
                    template: &main,
                    page,
                    kind: &MenuKind::Titles(&entries),
                    links: MenuLinks { first: main_total + 1, main_menu: Some(1), ..MenuLinks::default() },
                    entry: None,
                    prefix: "episodes",
                    out_dir: &work,
                    opts: &RenderOptions {
                        heading: Some(&tr(ui, "episodes")),
                        ui_lang: ui,
                        motion,
                        motion_duration: opts.motion_duration,
                        text_overflow: opts.text_overflow,
                        tv,
                        intro: opts.menu_animations,
                        menu_audio: main_menu_audio.as_ref(),
                        ..RenderOptions::still(&main_dir)
                    },
                })?;
                main_menu.pages.extend(episodes.pages);
                main_menu.warnings.extend(episodes.warnings);
            }
            if let Some(page) = bonus_page {
                let bonus = menu::render_menu(&menu::RenderMenuParams {
                    template: &main,
                    page,
                    kind: &MenuKind::Bonus(&entries[1..]),
                    links: MenuLinks { first: main_total + 1, main_menu: Some(1), ..MenuLinks::default() },
                    entry: None,
                    prefix: "bonus",
                    out_dir: &work,
                    opts: &RenderOptions {
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
                })?;
                main_menu.pages.extend(bonus.pages);
                main_menu.warnings.extend(bonus.warnings);
            }
            main_menu
    } else {
            log(Event::Step(tr(opts.ui_lang, "no_title_menu")));
            RenderedMenu { pages: vec![], warnings: vec![] }
        };
        for w in &vmgm.warnings {
            warn(log, w.clone());
        }
        let sound_note = Some(match (&main_menu_audio, titles_have_sound) {
            (Some(a), _) => tf!(opts.ui_lang, "menu_sound_file", file = a.path.file_name().unwrap_or_default().to_string_lossy()),
            (None, true) => tr(opts.ui_lang, "menu_sound_titles"),
            (None, false) => tr(opts.ui_lang, "menu_sound_silent"),
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
                        &files.frame,
                        files.custom.as_ref().expect("checked"),
                        files.motion_secs.unwrap_or(1.0),
                        audio.as_ref(),
                        files.intro.as_ref().map(|i| i.secs),
                        &work.join(&raw),
                    ),
                    Some(secs) => encoder::motion_menu_args(&encoder::MotionMenuJob {
                        tv,
                        base: &files.background,
                        foreground: files.foreground.as_deref(),
                        slots: &files.video_slots,
                        duration_secs: secs,
                        audio: audio.as_ref(),
                        intro_secs: files.intro.as_ref().map(|i| i.secs),
                        output: &work.join(&raw),
                    }),
                    None => encoder::menu_still_args(tv, &files.background, &work.join(&raw)),
                };
                let secs = files.motion_secs.unwrap_or(1.0);
                match &files.intro {
                    // The intro frames go ffmpeg -> Rust (slot videos) -> ffmpeg over pipes, never to disk.
                    Some(intro) => {
                        // Stream 0 is the page background, the slot videos follow.
                        let streams: Vec<RawStream> = if let Some(media) = &files.custom {
                            let frame = files.frame;
                            let rate = menu_anim::dvd_rate(tv);
                            std::iter::once(encoder::intro_background_stream(&frame, rate, media, intro.frames))
                                .chain(intro.slots.iter().filter_map(|&l| encoder::intro_slot_stream(&frame, rate, &media.layers[l], intro.frames)))
                                .collect()
                        } else {
                            std::iter::once(encoder::dvd_background_stream(tv, files.intro_fill, intro.frames))
                                .chain(files.video_slots.iter().map(|s| encoder::dvd_slot_stream(tv, s, intro.frames)))
                                .collect()
                        };
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
                if !xml.is_empty() && files.spumux_xml.exists() {
                    let spu_log = tools.spumux(&work, &xml, &raw, &vob, 0).await?;
                    std::fs::write(work.join(format!("{xml}.log")), spu_log).at(&work)?;
                    std::fs::remove_file(work.join(&raw)).at(&work.join(&raw))?;
                    // 16:9 menus: the letterbox highlight is subpicture stream 1 (see `author::menu_pgcs`).
                    if let Some(lb) = files.letterbox_xml.as_ref().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned()) {
                        std::fs::rename(work.join(&vob), work.join(&raw)).at(&work.join(&raw))?;
                        let lb_log = tools.spumux(&work, &lb, &raw, &vob, 1).await?;
                        std::fs::write(work.join(format!("{lb}.log")), lb_log).at(&work)?;
                        std::fs::remove_file(work.join(&raw)).at(&work.join(&raw))?;
                    }
                } else {
                    let dest = work.join(&vob);
                    if dest.exists() {
                        let _ = std::fs::remove_file(&dest);
                    }
                    std::fs::rename(work.join(&raw), &dest).at(&dest)?;
                }
                Ok(file_mb(&work.join(&vob)))
            })
        })
        .collect();
    let menu_sizes = run_limited(menu_jobs, jobs, cancel, |e| {
        if let JobEvent::Finished { finished, total, .. } = e {
            log(Event::Progress {
                label: format!("Building menus ({finished}/{total})"),
                fraction: finished as f64 / total as f64,
            });
        }
    })
    .await?;
    let menu_mb: f64 = menu_sizes.iter().sum();

    // 6. Bitrate ----------------------------------------------------------------------------------
    let loads: Vec<TitleLoad> = titles
        .iter()
        .map(|t| TitleLoad {
            duration_secs: t.info.duration_secs,
            audio_kbps: t.audio_kbps(),
            subtitle_streams: t.total_subtitles(),
            copy_video_kbps: t.copy_kbps,
            copy_bytes: t.copy_bytes,
            weight: 1.0,
        })
        .collect();
    let total_secs: f64 = loads.iter().map(|l| l.duration_secs).sum();
    let avg_audio = avg_audio_kbps(&loads);
    let plan = bitrate_plan_at(opts.disc, &loads, menu_mb, opts.encoding_profile, min_video_kbps(opts.disc, opts.resolution))?;
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
    let resolution = if plan.video_kbps > 0 {
        effective_resolution(opts.resolution, opts.encoding_profile, plan.video_kbps)
    } else {
        opts.resolution
    };
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
        let wide: Vec<&str> = titles
            .iter()
            .filter(|t| t.copy_kbps.is_none() && t.aspect == Aspect::Wide)
            .map(|t| t.label.as_str())
            .collect();
        if !wide.is_empty() {
            warn(log, tf!(opts.ui_lang, "wide_letterbox_warn", res = resolution.label(), titles = wide.join(", ")));
        }
        for t in titles.iter_mut().filter(|t| t.copy_kbps.is_none()) {
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
        "Encoding {count} titles ({passes}, {encode_jobs} in parallel, {} kbps video, {} {}×{})",
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
            let video_kbps = t.copy_kbps.unwrap_or_else(|| plan.video_kbps.min(DVD_MAX_TOTAL_KBPS.saturating_sub(t.audio_kbps())));
            let (two_pass, fast, hw_decode) = (opts.two_pass, opts.fast_encode, opts.hw_decode.then_some(opts.hw_accel));
            job(move |progress| async move {
                encode_title(EncodeTitleParams {
                    tools: &tools,
                    title: &t,
                    work: &work,
                    video_kbps,
                    two_pass,
                    fast,
                    hw_decode,
                    tv,
                    resolution,
                    progress,
                })
                .await
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
    let expected_bytes = plan.estimated_mb * 1024.0 * 1024.0;
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
    } else if opts.hw_decode && titles.iter().any(|t| t.copy_kbps.is_none()) {
        log(Event::Step(tr(opts.ui_lang, "gpu_unavailable")));
    }

    // 8. dvdauthor --------------------------------------------------------------------------------
    let MenuStage { vmgm_menus, ts_menus, main_pages, title_pre, .. } = stage;
    let titlesets = groups
        .iter()
        .zip(ts_menus)
        .zip(&main_pages)
        .map(|((g, menus), &main_menu_page)| {
            // A merged entry ends with its last file, so that file's choice counts.
            let after = opts.selections.get(g.end - 1).map(|s| s.after_playback).unwrap_or_default();
            (&titles[g.clone()], menus, main_menu_page, after)
        })
        .map(|(members, menus, main_menu_page, after)| (&members[0], members, menus, main_menu_page, after))
        .map(|(t, members, menus, main_menu_page, after_playback)| TitlesetSpec {
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
                default_audio: opts.preferred_audio_lang.as_ref().and_then(|pref| {
                    t.audio.iter().position(|&a| language_matches(pref, t.info.audio[a].language.as_deref())).map(|p| p as u32)
                }),
                subtitle_langs: t
                    .subtitle_langs_raw()
                    .map(|l| l.and_then(probe::iso639_1).map(str::to_string))
                    .collect(),
                default_subtitle: opts.preferred_subtitle_lang.as_ref().and_then(|pref| {
                    t.subtitle_langs_raw().position(|l| language_matches(pref, l)).map(|p| p as u32)
                }),
                force_subtitle: opts.force_first_subtitle,
                after_playback,
            },
        })
        .collect();

    let layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: opts.custom_layout.as_ref().map_or("4:3", |c| c.menu_aspect()).into(),
        vmgm_menus,
        titlesets,
        playback: opts.playback,
        tv,
        title_pre,
        fast_play: opts.custom_layout.as_ref().and_then(|c| c.fast_play.clone()),
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
    log(Event::Step(tr(opts.ui_lang, "authoring_created")));
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
    let work_dir = if keep_work(opts) {
        log(Event::Step(format!("Work folder kept: {}", work.display())));
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
    fn title_limit_counts_merge_groups_not_files() {
        let with = |merge: &dyn Fn(usize) -> bool| BuildOptions {
            inputs: (0..151).map(|i| PathBuf::from(format!("ep{i}.mkv"))).collect(),
            selections: (0..151).map(|i| StreamSelection { merge_with_previous: merge(i), ..Default::default() }).collect(),
            ..BuildOptions::default()
        };
        // 151 episodes merged into 2 titles: fine.
        assert!(check_title_count(&with(&|i| i != 0 && i != 76)).is_ok());
        // 151 separate titles: over the limit, reported as titles.
        assert!(matches!(check_title_count(&with(&|_| false)), Err(BuildError::Author(AuthorError::TooManyTitles(151)))));
        // 100 titles (the 51 files after the 100th are merged into it): still too many.
        assert!(matches!(check_title_count(&with(&|i| i >= 100)), Err(BuildError::Author(AuthorError::TooManyTitles(100)))));
    }

    #[test]
    fn work_dir_name_precedence() {
        let out = Path::new("out");
        let name = |o: &BuildOptions| work_dir(out, o).file_name().unwrap().to_string_lossy().into_owned();
        let mut o = BuildOptions { inputs: vec!["x/clip.mkv".into()], ..BuildOptions::default() };
        assert_eq!(name(&o), "clip.musadvd-work");
        o.menu_title = Some("Menü".into());
        o.project_name = Some("  ".into());
        assert_eq!(name(&o), "Menü.musadvd-work");
        o.project_name = Some("Urlaub 2025".into());
        assert_eq!(name(&o), "Urlaub 2025.musadvd-work");
        o.project_name = Some("a/b:c".into());
        assert_eq!(name(&o), "a_b_c.musadvd-work");
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
        assert!(e.contains("b cannot be merged with a") && e.contains("audio tracks"), "{e}");
        assert!(check_group_shapes(&[("a", 1, 0), ("b", 1, 2)]).unwrap_err().to_string().contains("subtitle tracks"));
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
    fn lower_audio_rate_leaves_more_video_kbps() {
        let load = |audio_kbps| TitleLoad { duration_secs: 7200.0, audio_kbps, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let video = |a| bitrate_plan(DiscType::Dvd5, &[load(a)], 1.0, EncodingProfile::Auto).unwrap().video_kbps;
        assert!(video(192) > video(448), "{} vs {}", video(192), video(448));
    }

    #[cfg(windows)]
    #[test]
    fn work_dir_removal_waits_for_a_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("musadvd-rm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // share_mode(0): nobody may open, delete or rename it while the handle lives.
        let locked = std::fs::OpenOptions::new().write(true).create(true).truncate(true).share_mode(0).open(dir.join("a.mpg")).unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(locked);
        });
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        rt.block_on(remove_dir_retry(&dir, 10, std::time::Duration::from_millis(100))).unwrap();
        closer.join().unwrap();
        assert!(!dir.exists());
        // A missing directory is fine; without retries a lock is reported.
        rt.block_on(remove_dir_retry(&dir, 0, std::time::Duration::ZERO)).unwrap();
    }

    #[test]
    fn sif_floor_fits_a_19_hour_collection_on_dvd9() {
        assert_eq!(min_video_kbps(DiscType::Dvd9, Resolution::FullD1), 1000);
        assert_eq!(min_video_kbps(DiscType::Dvd9, Resolution::HalfD1), 600);
        assert_eq!(min_video_kbps(DiscType::Dvd9, Resolution::Sif), 400);
        assert_eq!(min_video_kbps(DiscType::Bd25, Resolution::Sif), DiscType::Bd25.limits().min_video_kbps);
        // 151 episodes of ~7.5 min (~19 h), dual stereo AC-3 and two subtitle tracks each.
        let ep = TitleLoad { duration_secs: 453.0, audio_kbps: 384, subtitle_streams: 2, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let loads = vec![ep; 151];
        let full = bitrate_plan_at(DiscType::Dvd9, &loads, 10.0, EncodingProfile::Auto, min_video_kbps(DiscType::Dvd9, Resolution::FullD1)).unwrap();
        assert_eq!(full.clamp, Clamp::Min, "Full D1 cannot hold 19 h on a DVD-9");
        let sif = bitrate_plan_at(DiscType::Dvd9, &loads, 10.0, EncodingProfile::Auto, min_video_kbps(DiscType::Dvd9, Resolution::Sif)).unwrap();
        assert_ne!(sif.clamp, Clamp::Min);
        assert!(sif.fits() && sif.estimated_mb <= DiscType::Dvd9.target_mb() as f64, "{sif:?}");
        assert!(sif.video_kbps >= 400 && sif.video_kbps < 1000, "{}", sif.video_kbps);
    }

    #[test]
    fn avchd_plan_leaves_room_for_the_measured_bdav_overhead() {
        // Four 24.6-min episodes measured on AVCHD-5 with NVENC: 2x stereo AC-3, one rendered PGS
        // track (~32 kbit/s), BDAV adding 7.7 % to the raw streams.
        let ep = TitleLoad { duration_secs: 1473.0, audio_kbps: 384, subtitle_streams: 1, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let loads = vec![ep; 4];
        let disc = DiscType::Avchd5;
        let reserve = hw_reserve_mb(disc, HwAccelMode::Nvidia);
        assert_eq!(reserve, 4300.0 * HW_ENCODER_RESERVE);
        assert_eq!(hw_reserve_mb(disc, HwAccelMode::None), 0.0);
        assert_eq!(hw_reserve_mb(DiscType::Dvd5, HwAccelMode::Nvidia), 0.0, "DVD-Video encodes on the CPU");
        let plan = bitrate_plan_at(disc, &loads, reserve, EncodingProfile::Auto, min_video_kbps(disc, Resolution::Hd1080)).unwrap();
        // Worst case: the encoder hits the planned bitrate exactly (NVENC came in 2.9 % under).
        let real_kbit: f64 = loads.iter().map(|t| ((plan.video_kbps + t.audio_kbps) as f64 + 32.0) * t.duration_secs * 1.077).sum();
        let real_mb = real_kbit / 8192.0;
        assert!(real_mb <= disc.target_mb() as f64, "{real_mb:.0} MB with {} kbit/s", plan.video_kbps);
        // And the disc is still filled well (not a blunt cut).
        assert!(real_mb > 0.97 * disc.target_mb() as f64, "{real_mb:.0} MB");
    }

    #[test]
    fn bd25_holds_a_19_hour_720p_collection() {
        assert_eq!(min_video_kbps(DiscType::Bd25, Resolution::Hd720), 1500);
        assert_eq!(min_video_kbps(DiscType::Bd50, Resolution::Hd1080), 2500);
        // AVCHD's own floor (2000) is already below the 1080p one.
        assert_eq!(min_video_kbps(DiscType::Avchd9, Resolution::Hd1080), 2000);
        assert_eq!(min_video_kbps(DiscType::Avchd9, Resolution::Hd720), 1500);
        // 151 episodes of ~7.5 min (~19 h), dual stereo AC-3 and two subtitle tracks each.
        let ep = TitleLoad { duration_secs: 453.0, audio_kbps: 384, subtitle_streams: 2, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let loads = vec![ep; 151];
        let plan = |floor| bitrate_plan_at(DiscType::Bd25, &loads, 0.0, EncodingProfile::Auto, floor).unwrap();
        assert_eq!(plan(DiscType::Bd25.limits().min_video_kbps).clamp, Clamp::Min, "the old 4000 kbit/s floor overflows");
        let hd720 = plan(min_video_kbps(DiscType::Bd25, Resolution::Hd720));
        assert_ne!(hd720.clamp, Clamp::Min);
        assert!(hd720.fits() && hd720.estimated_mb <= DiscType::Bd25.target_mb() as f64, "{hd720:?}");
        assert!(hd720.video_kbps >= 1500 && hd720.video_kbps < 4000, "{}", hd720.video_kbps);
        // 1080p needs more than a BD-25 even at its floor: the plan says so instead of overflowing silently.
        assert_eq!(plan(min_video_kbps(DiscType::Bd25, Resolution::Hd1080)).clamp, Clamp::Min);
    }

    #[test]
    fn bitrate_plan_matches_calculator() {
        let loads = [
            TitleLoad { duration_secs: 3600.0, audio_kbps: 448 + 192, subtitle_streams: 1, copy_video_kbps: None, copy_bytes: None, weight: 1.0 },
            TitleLoad { duration_secs: 1800.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 },
        ];
        assert_eq!(avg_audio_kbps(&loads), 491); // (640*3600 + 192*1800) / 5400
        let plan = bitrate_plan(DiscType::Dvd5, &loads, 1.0, EncodingProfile::Auto).unwrap();
        // DVD: NAV packs per second, IFOs per title, menus, subtitles; then the stream share.
        let fixed = calculator::DVD_NAV_KBPS * 5400.0 + 2.0 * calculator::DVD_IFO_KBIT_PER_TITLE + 8192.0 + 10.0 * 3600.0;
        let r = calculator::DVD_PACK_OVERHEAD_RATIO;
        let overhead = fixed + r * (DiscType::Dvd5.target_kbit() - fixed) / (1.0 + r);
        let direct = calculator::calculate_with_overhead(DiscType::Dvd5, 5400.0, 491, overhead).unwrap();
        assert_eq!(plan.video_kbps, direct.video_kbps);
        assert!(plan.fits());
    }

    #[test]
    fn long_low_bitrate_dvd_stays_under_its_target() {
        // The 83-cartoon DVD-9 that came out at 7980 of 7950 MB with the flat 2 % overhead:
        // 36 610 s, AC-3 192 kbit/s, 169.7 MB of menus. Measured on its VOBs: 82 kbit/s of mux
        // overhead (40 NAV + 2.5-2.6 % of the stream) and 2.8 MB of IFO/BUP.
        let per = 36_610.0 / 83.0;
        let loads: Vec<TitleLoad> = (0..83)
            .map(|_| TitleLoad { duration_secs: per, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 })
            .collect();
        let plan = bitrate_plan(DiscType::Dvd9, &loads, 169.7, EncodingProfile::Auto).unwrap();
        let real_mb = |video: u32| (video as f64 + 192.0 + 82.0) * 36_610.0 / 8192.0 + 169.7 + 2.8;
        // An encoder that hits the planned bitrate exactly now fits (the old plan, ~1513 kbit/s, did not).
        assert!(real_mb(plan.video_kbps) <= 7950.0, "{} kbit/s -> {:.0} MB", plan.video_kbps, real_mb(plan.video_kbps));
        assert!(real_mb(1513) > 7950.0);
        // The estimate the GUI shows is the model's own size and stays under the target too.
        assert!(plan.fits() && plan.estimated_mb <= 7950.0 && plan.estimated_mb >= real_mb(plan.video_kbps) - 5.0, "{}", plan.estimated_mb);
    }

    #[test]
    fn quality_weight_shifts_the_bitrate_share_and_still_fits() {
        let normal = TitleLoad { duration_secs: 7200.0, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let (low, high) = (TitleLoad { weight: QUALITY_LOW, ..normal }, TitleLoad { weight: QUALITY_HIGH, ..normal });
        let flat = bitrate_plan(DiscType::Bd25, &[normal, normal], 0.0, EncodingProfile::Auto).unwrap();
        let plan = bitrate_plan(DiscType::Bd25, &[low, high], 0.0, EncodingProfile::Auto).unwrap();
        // Equal weights on both sides average out to 1.0, so the base bitrate stays the same.
        assert!(plan.video_kbps.abs_diff(flat.video_kbps) <= 1, "{} vs {}", plan.video_kbps, flat.video_kbps);
        let (lo, hi) = (weighted_video_kbps(DiscType::Bd25, plan.video_kbps, low.weight, 448, DiscType::Bd25.limits().min_video_kbps), weighted_video_kbps(DiscType::Bd25, plan.video_kbps, high.weight, 448, DiscType::Bd25.limits().min_video_kbps));
        assert!(hi > lo && hi as f64 / lo as f64 > 1.6);
        let used_mb = (((lo + hi) as f64 + 2.0 * 448.0) * 7200.0 + plan.mux_overhead_kbit) / 8192.0;
        assert!(used_mb <= DiscType::Bd25.target_mb() as f64 + 1.0, "{used_mb} MB");
        // A heavier title takes more of the disc, so the others get less.
        let heavy = bitrate_plan(DiscType::Bd25, &[high, high], 0.0, EncodingProfile::Auto).unwrap();
        assert!(heavy.video_kbps < flat.video_kbps);
    }

    /// Sum over encoded titles of the sizes `weighted_video_kbps` really gives, plus fixed overhead.
    fn real_mb(disc: DiscType, plan: &BitratePlan, loads: &[TitleLoad], overhead_kbit: f64) -> f64 {
        let kbit: f64 = loads
            .iter()
            .map(|t| (weighted_video_kbps(disc, plan.video_kbps, t.weight, t.audio_kbps, disc.limits().min_video_kbps) + t.audio_kbps) as f64 * t.duration_secs)
            .sum();
        (kbit + overhead_kbit) / 8192.0
    }

    #[test]
    fn low_title_hitting_the_floor_still_fits_dvd5() {
        let mk = |secs: f64, weight: f64| TitleLoad { duration_secs: secs, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight };
        // Long Low title: 0.75 * base falls under the 1000 kbit/s floor, a short High title takes the rest.
        // (18 000 s: 20 000 s at the floor only fit the old flat 2 % overhead, not real NAV packs.)
        let loads = [mk(18_000.0, QUALITY_LOW), mk(8000.0, QUALITY_HIGH)];
        let plan = bitrate_plan(DiscType::Dvd5, &loads, 0.0, EncodingProfile::Auto).unwrap();
        let used = real_mb(DiscType::Dvd5, &plan, &loads, plan.mux_overhead_kbit);
        assert!(used <= DiscType::Dvd5.target_mb() as f64, "{used} MB");
        assert!((used - plan.estimated_mb).abs() < 1e-6 && plan.fits());
        assert_eq!(weighted_video_kbps(DiscType::Dvd5, plan.video_kbps, QUALITY_LOW, 192, DiscType::Dvd5.limits().min_video_kbps), DiscType::Dvd5.limits().min_video_kbps);
    }

    #[test]
    fn low_title_hitting_the_floor_still_fits_bd25() {
        let mk = |secs: f64, weight: f64| TitleLoad { duration_secs: secs, audio_kbps: 448, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight };
        let loads = [mk(26_000.0, QUALITY_LOW), mk(12_000.0, QUALITY_HIGH)];
        let plan = bitrate_plan(DiscType::Bd25, &loads, 0.0, EncodingProfile::Auto).unwrap();
        assert!(weighted_video_kbps(DiscType::Bd25, plan.video_kbps, QUALITY_LOW, 448, DiscType::Bd25.limits().min_video_kbps) == DiscType::Bd25.limits().min_video_kbps);
        let used = real_mb(DiscType::Bd25, &plan, &loads, plan.mux_overhead_kbit);
        assert!(used <= DiscType::Bd25.target_mb() as f64, "{used} MB");
        assert!((used - plan.estimated_mb).abs() < 1e-6 && plan.fits());
    }

    #[test]
    fn floors_that_overflow_are_reported_honestly() {
        let l = TitleLoad { duration_secs: 9.0 * 3600.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: QUALITY_LOW };
        let plan = bitrate_plan(DiscType::Dvd5, &[l], 0.0, EncodingProfile::Auto).unwrap();
        assert_eq!(plan.clamp, Clamp::Min);
        assert!(!plan.fits());
        let used = real_mb(DiscType::Dvd5, &plan, &[l], plan.mux_overhead_kbit);
        assert!((used - plan.estimated_mb).abs() < 1e-6);
    }

    #[test]
    fn copied_titles_take_a_fixed_share_first() {
        let enc = TitleLoad { duration_secs: 3600.0, audio_kbps: 640, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let copy = TitleLoad { copy_video_kbps: Some(20_000), ..enc };
        let alone = bitrate_plan(DiscType::Bd25, &[enc], 0.0, EncodingProfile::Auto).unwrap();
        let mixed = bitrate_plan(DiscType::Bd25, &[enc, copy], 0.0, EncodingProfile::Auto).unwrap();
        assert!(mixed.video_kbps < alone.video_kbps, "the copy's size comes off the encoded title's budget");
        assert!(mixed.fits() && mixed.estimated_mb > 20_640.0 * 3600.0 / 8192.0, "the estimate includes the copy");
        let only = bitrate_plan(DiscType::Bd25, &[copy], 0.0, EncodingProfile::Auto).unwrap();
        assert_eq!((only.video_kbps, only.clamp), (0, Clamp::None));
        let too_big = bitrate_plan(DiscType::Bd25, &[copy; 4], 0.0, EncodingProfile::Auto).unwrap();
        assert_eq!(too_big.clamp, Clamp::Min, "four 20 Mbit/s hours overflow a BD-25");
    }

    #[test]
    fn dvd_copied_titles_reserve_exact_copy_bytes() {
        let enc = TitleLoad { duration_secs: 3600.0, audio_kbps: 192, subtitle_streams: 0, copy_video_kbps: None, copy_bytes: None, weight: 1.0 };
        let copy_1500_mib = TitleLoad {
            duration_secs: 3600.0,
            audio_kbps: 192,
            subtitle_streams: 0,
            copy_video_kbps: Some(3000),
            copy_bytes: Some(1500 * 1024 * 1024),
            weight: 1.0,
        };
        let mixed = bitrate_plan(DiscType::Dvd5, &[enc, copy_1500_mib], 0.0, EncodingProfile::Auto).unwrap();
        let r = calculator::DVD_PACK_OVERHEAD_RATIO;
        let dvd_fixed = |titles: f64, secs: f64| calculator::DVD_NAV_KBPS * secs + calculator::DVD_IFO_KBIT_PER_TITLE * titles;
        let fixed = dvd_fixed(2.0, 7200.0) + calculator::bytes_to_kbit(1500 * 1024 * 1024);
        let expected_overhead = fixed + r * (DiscType::Dvd5.target_kbit() - fixed) / (1.0 + r);
        let direct = calculator::calculate_capped(
            DiscType::Dvd5,
            3600.0,
            192,
            expected_overhead,
            EncodingProfile::Auto.max_video_kbps_for(DiscType::Dvd5),
        )
        .unwrap();
        assert_eq!(mixed.video_kbps, direct.video_kbps);
        let real = (fixed + (1.0 + r) * (mixed.video_kbps + 192) as f64 * 3600.0) / 8192.0;
        assert!((mixed.estimated_mb - real).abs() < 1e-9);
        assert!(mixed.fits());

        // All-copied DVD-5: two 1500 MiB titles (3000 MiB + NAV packs and IFOs) fit cleanly.
        let fit = bitrate_plan(DiscType::Dvd5, &[copy_1500_mib, copy_1500_mib], 0.0, EncodingProfile::Auto).unwrap();
        assert_eq!((fit.video_kbps, fit.clamp), (0, Clamp::None));
        assert!(fit.fits());
        assert!((fit.estimated_mb - (3000.0 + dvd_fixed(2.0, 7200.0) / 8192.0)).abs() < 1e-6);

        // Three 1500 MiB titles (4500 MiB > 4300 MiB) overflow and clamp to Min.
        let overflow = bitrate_plan(DiscType::Dvd5, &[copy_1500_mib; 3], 0.0, EncodingProfile::Auto).unwrap();
        assert_eq!((overflow.video_kbps, overflow.clamp), (0, Clamp::Min));
        assert!(!overflow.fits());
    }

    #[test]
    fn menu_estimate() {
        assert!((estimate_menu_mb(2, false, 25.0) - 7.0 * 0.15).abs() < 1e-9);
        assert!((estimate_menu_mb(2, true, 25.0) - (7.0 * 0.15 + 4.0 * 5.5)).abs() < 1e-9);

        let still_menu = crate::layout::CustomMenu { play_mode: None, timeout_secs: None,
            id: "m1".into(),
            name: "Main".into(),
            role: crate::layout::MenuRole::Title,
            title: None,
            background: crate::layout::Background::default(),
            duration_secs: None,
            audio: crate::layout::MenuSound::None,
            intro_secs: None,
            group: None,
            elements: vec![],
        };
        let motion_menu = crate::layout::CustomMenu { play_mode: None, timeout_secs: None,
            id: "m2".into(),
            name: "Chapters".into(),
            role: crate::layout::MenuRole::Root,
            title: Some(1),
            background: crate::layout::Background {
                video: Some(crate::layout::VideoRef { title: 1, start_secs: None, path: None }),
                ..Default::default()
            },
            duration_secs: Some(25.0),
            audio: crate::layout::MenuSound::None,
            intro_secs: Some(5.0),
            group: None,
            elements: vec![],
        };
        let l = crate::layout::CustomLayout { title_screen: None, chapter_menus: None, aspect: None,
            version: 1,
            canvas: crate::layout::Canvas::default(),
            highlight: crate::layout::Highlight::default(),
            animations: true,
            fast_play: None,
            theme: None,
            settings_mode: None,
            menus: vec![still_menu, motion_menu],
        };
        // 1 still (0.15 MB) + 1 motion (25s loop + 5s intro = 30s @ 5.5MB/25s = 6.6 MB) = 6.75 MB
        let custom_mb = estimate_custom_layout_mb(&l, 25.0);
        assert!((custom_mb - (0.15 + 30.0 * 5.5 / 25.0)).abs() < 1e-9);
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
            bits_per_raw_sample: None,
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
        src.subtitles = vec![s(4, "subrip", SubtitleKind::Text), s(5, "unknown_bitmap", SubtitleKind::Bitmap)];
        let opts = BuildOptions {
            audio_langs: vec!["de".into(), "en".into()],
            preferred_audio_lang: Some("de".into()),
            ..BuildOptions::default()
        };
        let none = StreamSelection::default();

        // Language filter: source order is preserved (preferred language activates via VM command).
        let (audio, subs, notes) = select_streams(&src, &none, &opts, "Film");
        assert_eq!(audio, vec![0, 1]);
        assert_eq!(subs, vec![0]);
        assert!(notes.len() == 1 && notes[0].contains("unknown_bitmap"), "{notes:?}");

        // No language matches: the first track, with a warning.
        let jp = BuildOptions { audio_langs: vec!["ja".into()], ..BuildOptions::default() };
        let (audio, _, notes) = select_streams(&src, &none, &jp, "Film");
        assert_eq!(audio, vec![0]);
        assert!(notes.iter().any(|n| n.contains("no audio track matches")), "{notes:?}");

        // The user's choice wins over the filter.
        let chosen = StreamSelection { audio: Some(vec![3]), subtitles: Some(vec![]), ..StreamSelection::default() };
        let (audio, subs, _) = select_streams(&src, &chosen, &opts, "Film");
        assert_eq!((audio, subs), (vec![2], vec![]));

        // The chosen order is the disc's track order.
        let reordered = StreamSelection { audio: Some(vec![3, 1]), ..StreamSelection::default() };
        let (audio, _, _) = select_streams(&src, &reordered, &opts, "Film");
        assert_eq!(audio, vec![2, 0]);
    }

    #[test]
    fn harmonize_group_selective_audio_reencode() {
        use crate::probe::AudioStream;

        let mut info1 = info(60.0, vec![]);
        info1.audio = vec![
            AudioStream {
                index: 1,
                codec: "ac3".into(),
                channels: 2,
                channel_layout: "stereo".into(),
                language: Some("eng".into()),
                title: None,
                default: true,
                bit_rate: Some(192_000),
                sample_rate: Some(48_000),
                profile: None,
                bits_per_raw_sample: None,
            },
            AudioStream {
                index: 2,
                codec: "dts".into(),
                channels: 6,
                channel_layout: "5.1".into(),
                language: Some("ger".into()),
                title: None,
                default: false,
                bit_rate: Some(768_000),
                sample_rate: Some(48_000),
                profile: None,
                bits_per_raw_sample: None,
            },
        ];
        let mut info2 = info(60.0, vec![]);
        info2.audio = vec![
            AudioStream {
                index: 1,
                codec: "ac3".into(),
                channels: 2,
                channel_layout: "stereo".into(),
                language: Some("eng".into()),
                title: None,
                default: true,
                bit_rate: Some(192_000),
                sample_rate: Some(48_000),
                profile: None,
                bits_per_raw_sample: None,
            },
            AudioStream {
                index: 2,
                codec: "aac".into(),
                channels: 2,
                channel_layout: "stereo".into(),
                language: Some("ger".into()),
                title: None,
                default: false,
                bit_rate: Some(128_000),
                sample_rate: Some(48_000),
                profile: None,
                bits_per_raw_sample: None,
            },
        ];

        let make_title = |info: MediaInfo, stem: &str| TitlePlan {
            info,
            label: stem.into(),
            audio: vec![0, 1],
            subtitles: vec![],
            external_subtitles: vec![],
            sub_order: vec![],
            chapters: vec![0.0],
            chapter_titles: vec![None],
            aspect: Aspect::Wide,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![AudioMode::Auto, AudioMode::Auto],
            ac3_kbps: 0,
            track_label_mode: TrackLabelMode::SmartMetadata,
            ui_lang: UiLang::DE,
            video: VideoProcessing::default(),
            loudnorm: false,
            subtitle_style: Default::default(),
            video_mode: DvdVideoMode::Encode,
            copy_kbps: None,
            copy_bytes: None,
            motion_secs: 15.0,
            stem: stem.into(),
            thumbnail: None,
            chapter_thumbs: vec![],
        };

        let mut group = vec![make_title(info1, "part1"), make_title(info2, "part2")];
        harmonize_group(&mut group);

        // Track 0 was ac3 on both -> remains Auto / Copy
        assert_eq!(group[0].audio_modes[0], AudioMode::Auto);
        assert_eq!(group[1].audio_modes[0], AudioMode::Auto);
        assert!(matches!(group[0].audio_outs()[0], AudioOut::Copy { .. }));
        assert!(matches!(group[1].audio_outs()[0], AudioOut::Copy { .. }));

        // Track 1 differed (ac3 vs aac) -> both re-encoded to AC-3
        assert_eq!(group[0].audio_modes[1], AudioMode::Ac3);
        assert_eq!(group[1].audio_modes[1], AudioMode::Ac3);
        assert!(matches!(group[0].audio_outs()[1], AudioOut::Ac3 { .. }));
        assert!(matches!(group[1].audio_outs()[1], AudioOut::Ac3 { .. }));
    }

    #[test]
    fn external_subtitle_selection_sources_labels_and_group_shape() {
        let dir = std::env::temp_dir().join(format!("cx_dvd_ext_sub_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let srt = dir.join("film.en.srt");
        std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nHi\n").unwrap();

        let sel_json = format!(
            r#"{{"audio":[1],"subtitles":[2],"externalSubtitles":[{{"path":{},"language":"eng","title":"SDH","forced":true}}]}}"#,
            serde_json::to_string(&srt).unwrap()
        );
        let sel: StreamSelection = serde_json::from_str(&sel_json).unwrap();
        assert_eq!(sel.external_subtitles.len(), 1);
        assert_eq!(sel.external_subtitles[0].language.as_deref(), Some("eng"));
        assert!(sel.external_subtitles[0].forced);

        let opts = BuildOptions::default();
        let (ext, notes) = select_external_subtitles(&sel, &opts, 1, "Film").unwrap();
        assert_eq!(ext.len(), 1);
        assert!(notes.is_empty());

        // Invalid external subtitle path fails cleanly.
        let bad_sel = StreamSelection {
            external_subtitles: vec![ExternalSubtitle { path: dir.join("missing.srt"), language: None, title: None, forced: false }],
            ..Default::default()
        };
        assert!(select_external_subtitles(&bad_sel, &opts, 0, "Film").is_err());

        let mut src = info(60.0, vec![]);
        src.subtitles = vec![probe::SubtitleStream {
            index: 2,
            codec: "subrip".into(),
            kind: SubtitleKind::Text,
            language: Some("ger".into()),
            title: None,
            forced: false,
            width: None,
            height: None,
        }];
        let mut plan = TitlePlan {
            info: src,
            label: "Film".into(),
            audio: vec![],
            subtitles: vec![0],
            external_subtitles: ext,
            sub_order: vec![0, 1],
            chapters: vec![0.0],
            chapter_titles: vec![None],
            aspect: Aspect::Wide,
            audio_mode: AudioMode::Auto,
            audio_modes: vec![],
            ac3_kbps: 0,
            track_label_mode: TrackLabelMode::SmartMetadata,
            ui_lang: UiLang::EN,
            video: VideoProcessing::default(),
            loudnorm: false,
            subtitle_style: Default::default(),
            video_mode: DvdVideoMode::Encode,
            copy_kbps: None,
            copy_bytes: None,
            motion_secs: 15.0,
            stem: "title_01".into(),
            thumbnail: None,
            chapter_thumbs: vec![],
        };
        assert_eq!(plan.subtitle_sources(), vec![2, EXTERNAL_SUB_SOURCE_BASE]);
        assert_eq!(plan.subtitle_labels(), vec!["German", "English - SDH (forced)"]);
        assert_eq!(plan.total_subtitles(), 2);

        // Merged group shape counts embedded + external subtitles.
        assert!(check_group_shapes(&[("A", 1, plan.total_subtitles()), ("B", 1, 2)]).is_ok());
        assert!(check_group_shapes(&[("A", 1, plan.total_subtitles()), ("B", 1, 1)]).is_err());

        // A mixed order puts the external track first on the disc (labels, menu sources, IFO
        // languages and spumux stream ids all follow it).
        plan.sub_order = subtitle_order(&[2, EXTERNAL_SUB_SOURCE_BASE], Some(&[EXTERNAL_SUB_SOURCE_BASE, 2]));
        assert_eq!(plan.sub_order, vec![1, 0]);
        assert_eq!(plan.subtitle_sources(), vec![EXTERNAL_SUB_SOURCE_BASE, 2]);
        assert_eq!(plan.subtitle_labels(), vec!["English - SDH (forced)", "German"]);
        assert_eq!(plan.subtitle_langs_raw().collect::<Vec<_>>(), vec![Some("eng"), Some("ger")]);
        assert_eq!(plan.sub_stream_ids(), vec![1, 0]);
        // Unlisted tracks keep their natural order after the listed ones.
        assert_eq!(subtitle_order(&[4, 5, 6], Some(&[6])), vec![2, 0, 1]);
        assert_eq!(subtitle_order(&[4, 5], None), vec![0, 1]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
