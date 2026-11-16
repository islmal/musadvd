use std::path::PathBuf;
use std::process::ExitCode;

mod studio_cli;

use clap::{Parser, Subcommand, ValueEnum};
use musadvd::author::{CommandContext, Domain, Resolution, TitleSpec, TvStandard};
use musadvd::calculator::{self, Clamp, DiscType, EncodingProfile};
use musadvd::dvd::{self, BuildOptions, Event};
use musadvd::encoder::MenuAudio;
use musadvd::menu::{self, MenuSetOptions, UiLang};
use musadvd::template::Template;

#[derive(Parser)]
#[command(version, about = "ConvertX clean-room tools: template parser and DVD bitrate calculator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a ConvertX template .ini and print its pages and buttons.
    Template { path: PathBuf },
    /// Calculate the video bitrate for a DVD5/DVD9/BD25/BD50 target.
    Bitrate {
        #[arg(long, value_enum, default_value = "dvd5")]
        disc: Disc,
        /// Runtime as seconds or HH:MM:SS.
        #[arg(long, value_parser = parse_duration)]
        duration: f64,
        /// Total audio bitrate in kbps (sum of all tracks).
        #[arg(long, default_value_t = 192)]
        audio: u32,
        /// Mux overhead in kbit (default: 2% of the target size).
        #[arg(long)]
        overhead: Option<f64>,
    },
    /// Render a template menu (background + highlight masks) and write the dvdauthor/spumux project.
    RenderMenu {
        /// Template .ini file.
        #[arg(long)]
        template: PathBuf,
        /// Output directory (created if missing).
        #[arg(long)]
        output: PathBuf,
        /// Title as FILE[@CHAPTERS], e.g. "movie.mpg@0,10:00,20:00". Repeatable.
        /// Without any: placeholders title_01.mpg ... for one full menu page.
        #[arg(long = "title", value_parser = parse_title)]
        titles: Vec<TitleSpec>,
        /// Text for the menu heading (replaces the template placeholder).
        #[arg(long)]
        menu_title: Option<String>,
        /// Template page index (default: first title menu page).
        #[arg(long)]
        page: Option<usize>,
        /// Draw button rectangles into menu_bg.png for layout checks.
        #[arg(long)]
        debug_frames: bool,
    },
    /// Probe, encode, render all menus and author a complete VIDEO_TS. Ctrl+C cancels.
    BuildDvd {
        /// Input videos (.mkv, .mp4, ...), one title each.
        #[arg(long, num_args = 1.., required = true)]
        input: Vec<PathBuf>,
        #[arg(long, value_enum, default_value = "dvd5")]
        disc: Disc,
        /// Template .ini (sub menus are taken from sibling templates if it has none). Required
        /// for DVDs unless --layout is given; Blu-rays have no ConvertX templates (menu-less unless
        /// --layout is given).
        #[arg(long)]
        template: Option<PathBuf>,
        /// Custom menu layout (JSON from the menu editor) instead of a template.
        #[arg(long, value_name = "JSON", conflicts_with = "template")]
        layout: Option<PathBuf>,
        /// Output directory; the DVD ends up in <output>/<project name>/VIDEO_TS.
        #[arg(long)]
        output: PathBuf,
        /// Audio languages to keep, comma separated (e.g. "de,en"); default: all.
        #[arg(long, value_delimiter = ',')]
        audio_lang: Vec<String>,
        /// Subtitle languages to keep, comma separated; default: all.
        #[arg(long, value_delimiter = ',')]
        sub_lang: Vec<String>,
        /// Do not include subtitles.
        #[arg(long)]
        no_subs: bool,
        /// Chapter interval in minutes for inputs without chapters.
        #[arg(long, default_value_t = 5.0)]
        chapter_minutes: f64,
        /// Heading of the main menu.
        #[arg(long)]
        menu_title: Option<String>,
        /// Language of fixed menu texts.
        #[arg(long, default_value = "en")]
        ui_lang: String,
        /// Keep the work directory (<output>/<project>.musadvd-work: intermediate MPEGs, menu PNGs, logs).
        #[arg(long)]
        keep_work: bool,
        /// 2-pass video encoding: an analysis run before the final encode. Takes about twice as
        /// long but hits the target size more exactly.
        #[arg(long)]
        two_pass: bool,
        /// Fast draft encoding without trellis quantisation (about 40 % faster, slightly lower
        /// quality).
        #[arg(long)]
        fast: bool,
        /// Motion menus: menu pages with video slots (e.g. Open Style chapter thumbnails) become
        /// looping MPEG-2 clips with moving previews instead of stills.
        #[arg(long)]
        motion_menu: bool,
        /// Motion menu length in seconds (default: template main_duration, usually 25).
        #[arg(long, value_name = "SEKUNDEN", requires = "motion_menu", value_parser = parse_positive_secs)]
        motion_duration: Option<f64>,
        /// Motion menus without sound (default: audio of the first preview clip).
        #[arg(long, requires = "motion_menu")]
        motion_silent: bool,
        /// Sound of the main menu (motion menus): an audio file. Default: main menu silent; video menus always play their own video's sound.
        #[arg(long, value_name = "DATEI", requires = "motion_menu", conflicts_with = "motion_silent")]
        menu_audio: Option<PathBuf>,
        /// TV standard of titles and menus.
        #[arg(long, value_enum, default_value = "pal")]
        tv: Tv,
        /// Frame size of the titles (DVD: full-d1 (default), half-d1, sif – menus stay Full D1,
        /// half-d1/sif titles are always 4:3; Blu-ray: hd1080 (default), hd720).
        #[arg(long, value_enum)]
        resolution: Option<Res>,
        /// Blu-ray: allow only one video (no playlist of several videos).
        #[arg(long)]
        single_video: bool,
        /// Upper limit of the video bitrate: auto/sp ≤ 8500, mp ≤ 6000, lp ≤ 4000 kbps (lp
        /// switches to Half D1 below 3000 kbps); on Blu-ray 35,000 / 20,000 / 12,000 kbps.
        #[arg(long, value_enum, default_value = "auto")]
        encoding_profile: Profile,
        /// Name of the DVD folder inside --output (default: menu title / first video's name).
        #[arg(long)]
        project_name: Option<String>,
        /// Titles encoded at the same time (default: half the logical CPUs).
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=64))]
        jobs: Option<u16>,
        /// Decode the sources on the GPU when a working hardware decoder exists (default: on).
        #[arg(long, hide = true)]
        hw_decode: bool,
        /// Disable GPU source decoding and force CPU decoding.
        #[arg(long)]
        no_hw_decode: bool,
        /// GPU vendor for Blu-ray H.264 encoding and the GPU decoder order (falls back to
        /// x264/CPU when the GPU does not work).
        #[arg(long, value_enum, default_value = "nvidia")]
        hw_accel: HwAccelArg,
        /// Same as --hw-accel none (wins over --hw-accel).
        #[arg(long)]
        no_hw_encode: bool,
        /// Skip the intro animations of template menus.
        #[arg(long)]
        no_menu_animations: bool,
        /// Audio: auto copies compliant AC-3/DTS tracks, ac3 always re-encodes, lpcm writes
        /// uncompressed audio (Blu-ray/AVCHD; DVD falls back to AC-3).
        #[arg(long, value_enum, default_value = "auto")]
        audio: AudioArg,
        /// Track names on menu buttons: smart (language + file title), language only, or generic (Tonspur 1).
        #[arg(long, value_enum, default_value = "smart")]
        track_labels: TrackLabelsArg,
        /// Normalise the loudness of all audio tracks (EBU R128, -23 LUFS).
        #[arg(long)]
        loudnorm: bool,
        /// Fit into the frame with black bars (default), fill it and crop the overhang, or stretch.
        #[arg(long, value_enum, default_value = "letterbox")]
        scale: ScaleArg,
        /// Scaling algorithm of the video.
        #[arg(long, value_enum, default_value = "bicubic")]
        scale_filter: ScaleFilterArg,
        /// Deinterlacing: auto (only interlaced sources, bwdif), yadif (always), off.
        #[arg(long, value_enum, default_value = "auto")]
        deinterlace: DeinterlaceArg,
        /// Inputs (1-based) that play together with their predecessor behind one menu entry,
        /// e.g. `--merge 2,3` joins inputs 1-3.
        #[arg(long, value_name = "NR", value_delimiter = ',')]
        merge: Vec<usize>,
    },
    /// List DVD writers and the disc in each drive.
    Drives,
    /// Create a burnable disc image (UDF 1.02 + ISO 9660) from a VIDEO_TS folder. Ctrl+C cancels.
    MakeIso {
        /// The VIDEO_TS folder.
        video_ts: PathBuf,
        /// Output .iso file.
        output: PathBuf,
        /// Volume label (sanitised to A-Z, 0-9, _; max. 32 characters).
        #[arg(long, default_value = "DVD_VIDEO")]
        label: String,
    },
    /// Burn a VIDEO_TS or BDMV folder to DVD / Blu-ray (disc image, write, optional read-back
    /// verification). Ctrl+C cancels.
    Burn {
        /// The VIDEO_TS or BDMV folder, or the project folder containing it.
        video_ts: PathBuf,
        /// Drive: id, drive letter ("F:") or part of the name. Default: the only DVD writer.
        #[arg(long)]
        drive: Option<String>,
        /// Write speed: auto (DVD-5 maximum, DVD-9 about 4x), max, or a speed in x (8, 2.4);
        /// mapped to the nearest speed the drive offers.
        #[arg(long, default_value = "auto", value_parser = parse_burn_speed)]
        speed: String,
        /// Volume label (sanitised to A-Z, 0-9, _; max. 32). Default: the project folder name.
        #[arg(long)]
        label: Option<String>,
        /// Skip reading the disc back and comparing it with the image.
        #[arg(long)]
        no_verify: bool,
        /// A rewritable disc with old content may be erased/overwritten.
        #[arg(long)]
        erase: bool,
        /// Leave the disc in the drive.
        #[arg(long)]
        no_eject: bool,
        /// Keep the disc image (default: <project>.iso next to the project folder); without this
        /// option it is deleted after burning.
        #[arg(long, value_name = "DATEI", num_args = 0..=1)]
        keep_iso: Option<Option<PathBuf>>,
    },
    /// Burn Studio: read a disc into an image (`.iso`, split parts, layout file, MD5).
    ReadDisc(studio_cli::ReadDisc),
    /// Burn Studio: compare a disc with an image, or check that it is readable.
    VerifyDisc(studio_cli::VerifyDisc),
    /// Burn Studio: build an image from files and folders (UDF 1.02 + ISO 9660 + Joliet, DVD-Video, Blu-ray).
    BuildIso(studio_cli::BuildIso),
    /// Burn Studio: print or change the volume label of an image.
    IsoLabel(studio_cli::IsoLabel),
    /// Burn Studio: list the possible layer breaks of a DVD-Video image or folder.
    IfoLayerbreak(studio_cli::IfoLayerbreak),
    /// Burn Studio: drive and media details (MMC).
    DriveInfo(studio_cli::DriveInfoArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum Tv {
    Pal,
    Ntsc,
}

#[derive(Clone, Copy, ValueEnum)]
enum Res {
    FullD1,
    HalfD1,
    Sif,
    Hd1080,
    Hd720,
}

impl From<Res> for Resolution {
    fn from(r: Res) -> Self {
        match r {
            Res::FullD1 => Resolution::FullD1,
            Res::HalfD1 => Resolution::HalfD1,
            Res::Sif => Resolution::Sif,
            Res::Hd1080 => Resolution::Hd1080,
            Res::Hd720 => Resolution::Hd720,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Auto,
    Sp,
    Mp,
    Lp,
}

impl From<Profile> for EncodingProfile {
    fn from(p: Profile) -> Self {
        match p {
            Profile::Auto => EncodingProfile::Auto,
            Profile::Sp => EncodingProfile::Sp,
            Profile::Mp => EncodingProfile::Mp,
            Profile::Lp => EncodingProfile::Lp,
        }
    }
}

fn parse_positive_secs(s: &str) -> Result<f64, String> {
    match s.trim().replace(',', ".").parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 && v <= 600.0 => Ok(v),
        _ => Err(format!("'{s}' is not a duration between 0 and 600 seconds")),
    }
}

fn parse_burn_speed(s: &str) -> Result<String, String> {
    let v = s.trim().to_ascii_lowercase();
    let x = v.trim_end_matches('x').replace(',', ".");
    if v == "auto" || v == "max" || x.parse::<f64>().is_ok_and(|x| x > 0.0 && x <= 24.0) {
        Ok(v)
    } else {
        Err(format!("'{s}' is not auto, max or a speed in x (e.g. 8, 2.4)"))
    }
}

fn parse_title(s: &str) -> Result<TitleSpec, String> {
    TitleSpec::parse_cli(s).map_err(|e| e.to_string())
}

#[derive(Clone, Copy, ValueEnum)]
enum HwAccelArg {
    None,
    Nvidia,
    Intel,
    Amd,
}

impl From<HwAccelArg> for musadvd::config::HwAccelMode {
    fn from(a: HwAccelArg) -> Self {
        match a {
            HwAccelArg::None => Self::None,
            HwAccelArg::Nvidia => Self::Nvidia,
            HwAccelArg::Intel => Self::Intel,
            HwAccelArg::Amd => Self::Amd,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum AudioArg {
    Auto,
    Ac3,
    Lpcm,
}

impl From<AudioArg> for musadvd::encoder::AudioMode {
    fn from(a: AudioArg) -> Self {
        match a {
            AudioArg::Auto => Self::Auto,
            AudioArg::Ac3 => Self::Ac3,
            AudioArg::Lpcm => Self::Lpcm,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum TrackLabelsArg {
    Smart,
    Language,
    Generic,
}

impl From<TrackLabelsArg> for musadvd::probe::TrackLabelMode {
    fn from(a: TrackLabelsArg) -> Self {
        match a {
            TrackLabelsArg::Smart => Self::SmartMetadata,
            TrackLabelsArg::Language => Self::LanguageOnly,
            TrackLabelsArg::Generic => Self::Generic,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ScaleArg {
    Letterbox,
    Panscan,
    Stretch,
}

#[derive(Clone, Copy, ValueEnum)]
enum ScaleFilterArg {
    Bilinear,
    Bicubic,
    Lanczos,
}

#[derive(Clone, Copy, ValueEnum)]
enum DeinterlaceArg {
    Auto,
    Yadif,
    Off,
}

/// One selection per input; `merge` lists the 1-based inputs joined with their predecessor.
fn merge_selections(inputs: usize, merge: &[usize]) -> Result<Vec<dvd::StreamSelection>, String> {
    let mut sel = vec![dvd::StreamSelection::default(); inputs];
    for &n in merge {
        if n < 2 || n > inputs {
            return Err(format!("--merge {n}: only inputs 2 to {inputs} can be merged with the previous one"));
        }
        sel[n - 1].merge_with_previous = true;
    }
    Ok(sel)
}

#[derive(Clone, Copy, ValueEnum)]
enum Disc {
    Dvd5,
    Dvd9,
    Bd25,
    Bd50,
    /// Blu-ray structure (BDMV) on a DVD-5.
    Avchd5,
    /// Blu-ray structure (BDMV) on a dual-layer DVD.
    Avchd9,
}

impl From<Disc> for DiscType {
    fn from(d: Disc) -> Self {
        match d {
            Disc::Dvd5 => DiscType::Dvd5,
            Disc::Dvd9 => DiscType::Dvd9,
            Disc::Bd25 => DiscType::Bd25,
            Disc::Bd50 => DiscType::Bd50,
            Disc::Avchd5 => DiscType::Avchd5,
            Disc::Avchd9 => DiscType::Avchd9,
        }
    }
}

fn parse_duration(s: &str) -> Result<f64, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return Err(format!("invalid duration '{s}'"));
    }
    parts.iter().try_fold(0.0, |acc, p| {
        p.trim()
            .replace(',', ".")
            .parse::<f64>()
            .map(|v| acc * 60.0 + v)
            .map_err(|_| format!("invalid duration '{s}'"))
    })
}

/// Burns `video_ts` (see `Command::Burn`); prints progress with the remaining time on stderr.
#[allow(clippy::too_many_arguments)]
fn burn(
    video_ts: PathBuf,
    drive: Option<String>,
    speed: String,
    label: Option<String>,
    verify: bool,
    erase: bool,
    eject: bool,
    keep_iso: Option<Option<PathBuf>>,
) -> ExitCode {
    use musadvd::burner::layerbreak::Capacities;
    use musadvd::burner::{
        burn_video_ts, default_iso_path, is_bdmv_folder, media_problem, preferred_speed, speed_label, speed_x, system_burner,
        BurnError, BurnJob, BurnOptions, BurnPhase, BurnUpdate, DiscContent, WriteSpeed,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    let fail = |msg: String| {
        eprintln!("error: {msg}");
        ExitCode::FAILURE
    };
    let Some(video_ts) = [video_ts.clone(), video_ts.join("VIDEO_TS"), video_ts.join("BDMV")]
        .into_iter()
        .find(|d| d.join("VIDEO_TS.IFO").is_file() || is_bdmv_folder(d))
    else {
        return fail(format!("{}: not a finished VIDEO_TS or BDMV folder (VIDEO_TS.IFO or index.bdmv missing)", video_ts.display()));
    };
    let project = video_ts.parent().unwrap_or(&video_ts);
    let label =
        label.unwrap_or_else(|| project.file_name().map_or("DVD_VIDEO".into(), |n| n.to_string_lossy().into_owned()));
    let content = match DiscContent::of_folder(&video_ts) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let bytes = content.bytes();
    let dual_layer = content.dual_layer;
    let disc_name = match (content.bluray, dual_layer) {
        (false, false) => "DVD-5",
        (false, true) => "DVD-9",
        (true, false) => "BD-25",
        (true, true) => "BD-50",
    };

    // Drive and medium.
    let burner = system_burner();
    let drives = match burner.list_drives() {
        Ok(d) => d,
        Err(e) => return fail(e.to_string()),
    };
    let list = || {
        let arg = |d: &musadvd::burner::DriveInfo| {
            let letter = d.path.trim_end_matches('\\');
            if letter.is_empty() { d.id.clone() } else { letter.to_string() }
        };
        drives.iter().map(|d| format!("  {} (--drive {})", d.name, arg(d))).collect::<Vec<_>>().join("\n")
    };
    let candidates: Vec<_> = match &drive {
        Some(want) => {
            let w = want.trim().trim_end_matches('\\').to_lowercase();
            drives
                .iter()
                .filter(|d| {
                    d.id == *want
                        || (!w.is_empty() && d.path.trim_end_matches('\\').to_lowercase() == w)
                        || (!w.is_empty() && d.name.to_lowercase().contains(&w))
                })
                .collect()
        }
        None => drives.iter().collect(),
    };
    let d = match candidates[..] {
        [d] => d,
        _ if drives.is_empty() => return fail("No DVD/Blu-ray burner found.".into()),
        [] => return fail(format!("Drive '{}' not found. Available:\n{}", drive.unwrap_or_default(), list())),
        _ => return fail(format!("Several burners – choose one with --drive:\n{}", list())),
    };
    let media = match burner.probe_media(&d.id) {
        Ok(m) => m,
        Err(e) => return fail(format!("{}: disc unreadable: {e}", d.name)),
    };
    if let Some(problem) = media_problem(&media, &content) {
        return fail(problem);
    }
    if !media.blank && !erase {
        return fail(format!("The {} contains data – pass --erase to erase/overwrite it.", media.physical));
    }
    if dual_layer && !content.bluray && !media.layer_break_supported {
        eprintln!("warning: {}: the drive decides the layer break.", media.physical);
    }
    let speed = preferred_speed(media.media_type, &media.speeds, &speed);
    let x = speed_x(speed, &media.speeds, media.media_type);
    println!(
        "{}: {} – {} ({:.2} GB) at {}{}{}",
        d.name,
        media.physical,
        disc_name,
        bytes / 1e9,
        match speed {
            WriteSpeed::Max => format!("max. {}", speed_label((x * media.media_type.sectors_per_sec_1x()).round() as u32, media.media_type)),
            WriteSpeed::Specific(s) => speed_label(s, media.media_type),
        },
        if verify { ", with verification" } else { "" },
        if media.blank { "" } else { ", old data will be erased" }
    );

    // Ctrl+C sets the cancel flag; the image writer and the burner stop as soon as they can.
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
                cancel.store(true, Ordering::SeqCst);
                eprintln!("\nCancel requested – waiting for the drive …");
            }
        });
    }

    /// One progress line per backend message, with the remaining time of image + burn + verify.
    struct Log {
        message: String,
        printed: Option<(u32, Instant)>,
    }
    impl Log {
        fn show(&mut self, u: BurnUpdate) {
            let p = u.progress;
            if p.phase == BurnPhase::Done {
                return;
            }
            if p.message != self.message {
                if !self.message.is_empty() {
                    eprintln!();
                }
                self.message = p.message.clone();
                self.printed = None;
            }
            // Reprint on a new percentage, or once a second (finalizing reports no progress).
            let pct = (p.fraction * 100.0).floor() as u32;
            if self.printed.is_some_and(|(last, at)| last == pct && at.elapsed().as_secs_f64() < 1.0) {
                return;
            }
            self.printed = Some((pct, Instant::now()));
            let left = musadvd::eta::format_remaining(u.remaining_secs);
            eprint!("\r  {}: {pct:>3}% – {left} left    ", self.message.trim_end_matches(['…', ' ']));
        }
    }
    let mut log = Log { message: String::new(), printed: None };

    let job = BurnJob {
        iso: match &keep_iso {
            Some(Some(path)) => path.clone(),
            _ => default_iso_path(&video_ts),
        },
        video_ts,
        drive_id: d.id.clone(),
        label,
        speed,
        media_speeds: media.speeds.clone(),
        options: BurnOptions { layer_break: None, verify, eject, erase: erase && !media.blank, ..Default::default() },
        keep_iso: keep_iso.is_some(),
        capacities: Capacities::DVD,
        media_type: Some(media.media_type),
    };
    let result = burn_video_ts(burner.as_ref(), &job, &cancel, |u| log.show(u));
    eprintln!();
    match result {
        Ok(outcome) => {
            let kind = match (content.bluray, media.media_type.is_bd()) {
                (true, true) => "Blu-ray",
                (true, false) => "AVCHD disc",
                _ => "DVD",
            };
            println!("The {kind} was burned{}.", if verify { " and verified" } else { "" });
            if let Some(iso) = outcome.iso {
                println!("  Disc image: {}", iso.display());
            }
            ExitCode::SUCCESS
        }
        Err(BurnError::Cancelled) => {
            eprintln!("Burn cancelled.");
            ExitCode::from(130)
        }
        Err(e) => fail(e.to_string()),
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Burn { video_ts, drive, speed, label, no_verify, erase, no_eject, keep_iso } => {
            burn(video_ts, drive, speed, label, !no_verify, erase, !no_eject, keep_iso)
        }
        Command::ReadDisc(a) => studio_cli::read_disc(a),
        Command::VerifyDisc(a) => studio_cli::verify_disc(a),
        Command::BuildIso(a) => studio_cli::build_iso(a),
        Command::IsoLabel(a) => studio_cli::iso_label(a),
        Command::IfoLayerbreak(a) => studio_cli::ifo_layerbreak(a),
        Command::DriveInfo(a) => studio_cli::drive_info(a),
        Command::Drives => {
            use musadvd::burner::{speed_label, system_burner};
            let burner = system_burner();
            match burner.list_drives() {
                Ok(drives) if drives.is_empty() => {
                    println!("No DVD burner found.");
                    ExitCode::SUCCESS
                }
                Ok(drives) => {
                    for d in drives {
                        println!("{}", d.name);
                        match burner.probe_media(&d.id) {
                            Ok(m) if !m.present => println!("  no disc inserted"),
                            Ok(m) => {
                                let speeds: Vec<String> = m.speeds.iter().map(|&s| speed_label(s, m.media_type)).collect();
                                println!(
                                    "  {} ({:?}), {}{}, frei {:.2} GB, Geschwindigkeiten: {}",
                                    m.physical,
                                    m.media_type,
                                    if m.blank { "blank" } else { "written" },
                                    if m.writable { "" } else { ", not writable" },
                                    m.free_sectors as f64 * 2048.0 / 1e9,
                                    if speeds.is_empty() { "-".into() } else { speeds.join(", ") }
                                );
                            }
                            Err(e) => println!("  Disc unreadable: {e}"),
                        }
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::MakeIso { video_ts, output, label } => {
            use musadvd::burner::{
                create_disc_image,
                layerbreak::{Capacities, PlanError},
                udf::ImageError,
            };
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::Arc;
            let cancel = Arc::new(AtomicBool::new(false));
            {
                let cancel = cancel.clone();
                std::thread::spawn(move || {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
                    if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
                        cancel.store(true, Ordering::SeqCst);
                        eprintln!("\nAbbruch angefordert (Ctrl+C) …");
                    }
                });
            }
            let mut shown = 0;
            match create_disc_image(&video_ts, &output, &label, Capacities::DVD, &cancel, |p| {
                let pct = (p * 100.0) as u32;
                if pct >= shown + 10 {
                    shown = pct;
                    eprint!("\r{pct:>3} %");
                }
            }) {
                Ok(plan) => {
                    eprintln!();
                    println!(
                        "{}: {} ({:.2} GB, Label {})",
                        output.display(),
                        if plan.layer_break.is_some() { "DVD-9" } else { "DVD-5" },
                        plan.image.size_bytes() as f64 / 1e9,
                        plan.image.label
                    );
                    if let Some(lb) = plan.layer_break {
                        println!("  Layer break at sector {}{}", lb.l0_sectors, match lb.padding {
                            Some((vts, p)) => format!(" ({p} padding sectors before VTS {vts})"),
                            None => String::new(),
                        });
                    }
                    ExitCode::SUCCESS
                }
                Err(PlanError::Image(ImageError::Cancelled)) => {
                    eprintln!();
                    eprintln!("Abgebrochen.");
                    ExitCode::from(130)
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Template { path } => match Template::load(&path) {
            Ok(t) => {
                println!(
                    "{} (v{}, {})",
                    t.name.as_deref().unwrap_or("<unnamed>"),
                    t.template_version,
                    t.kind
                );
                for p in &t.pages {
                    println!("  page {} [{}] {}x{} '{}'", p.index, p.style, p.width, p.height, p.name);
                    for bg in &p.backgrounds {
                        println!("    background: {bg}");
                    }
                    for b in &p.buttons {
                        println!(
                            "    button {:>2}: {:>4},{:<4} {:>4}x{:<4} {:<18} scheme={} '{}'",
                            b.index, b.x, b.y, b.width, b.height, b.location, b.color_scheme, b.name
                        );
                    }
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {}: {e}", path.display());
                ExitCode::FAILURE
            }
        },
        Command::Bitrate { disc, duration, audio, overhead } => {
            let disc = DiscType::from(disc);
            let result = match overhead {
                Some(o) => calculator::calculate_with_overhead(disc, duration, audio, o),
                None => calculator::calculate(disc, duration, audio),
            };
            match result {
                Ok(p) => {
                    println!("disc:      {:?} ({} MB)", p.disc, p.disc.target_mb());
                    let l = p.disc.limits();
                    println!("limits:    video {}-{} kbps, video + audio <= {} kbps", l.min_video_kbps, l.max_video_kbps, l.max_total_kbps);
                    println!("video:     {} kbps (raw {:.0})", p.video_kbps, p.raw_video_kbps);
                    println!("audio:     {} kbps", p.audio_kbps);
                    println!("estimated: {:.0} MB", p.estimated_mb);
                    match p.clamp {
                        Clamp::Min => println!("warning:   clamped to minimum, output will NOT fit on the disc"),
                        Clamp::Max => println!("note:      clamped to maximum, disc will not be full"),
                        Clamp::None => {}
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::RenderMenu { template, output, titles, menu_title, page, debug_frames } => {
            let opts = MenuSetOptions {
                template_path: &template,
                page_index: page,
                titles,
                menu_title: menu_title.as_deref(),
                output_dir: &output,
                debug_frames,
            };
            match menu::render_menu_set(&opts) {
                Ok(summary) => {
                    for w in &summary.warnings {
                        eprintln!("warning: {w}");
                    }
                    println!(
                        "template page {}: {} menu page(s)",
                        summary.page_index,
                        summary.menus.len()
                    );
                    for (i, m) in summary.menus.iter().enumerate() {
                        println!("  menu {} ({}): {} buttons", i + 1, m.vob, m.buttons.len());
                        for b in &m.buttons {
                            println!(
                                "    {:<4} {:>3},{:<3}-{:>3},{:<3} {:<16} {}",
                                b.name,
                                b.rect.x0,
                                b.rect.y0,
                                b.rect.x1,
                                b.rect.y1,
                                b.location,
                                b.action.command(CommandContext { domain: Domain::Vmgm, menu: i as u32 + 1 })
                            );
                        }
                    }
                    for f in &summary.files {
                        println!("wrote {}", f.display());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::BuildDvd {
            input,
            disc,
            template,
            output,
            audio_lang,
            sub_lang,
            no_subs,
            chapter_minutes,
            menu_title,
            ui_lang,
            keep_work,
            two_pass,
            fast,
            motion_menu,
            motion_duration,
            motion_silent,
            jobs,
            hw_decode: _hw_decode,
            no_hw_decode,
            hw_accel,
            no_hw_encode,
            no_menu_animations,
            audio,
            track_labels,
            loudnorm,
            scale,
            scale_filter,
            deinterlace,
            merge,
            menu_audio,
            tv,
            resolution,
            single_video,
            encoding_profile,
            project_name,
            layout,
        } => {
            let disc = DiscType::from(disc);
            if !disc.is_bluray() && template.is_none() && layout.is_none() {
                eprintln!("error: pass --template or --layout (only Blu-rays work without a menu)");
                return ExitCode::FAILURE;
            }
            let ui_lang = ui_lang.trim().to_ascii_lowercase();
            let languages = musadvd::i18n::available_languages();
            if !languages.iter().any(|(code, _)| *code == ui_lang) {
                let codes: Vec<&str> = languages.iter().map(|(code, _)| code.as_str()).collect();
                eprintln!("error: unknown language '{ui_lang}' (available: {})", codes.join(", "));
                return ExitCode::FAILURE;
            }
            let custom_layout = match layout.as_deref().map(musadvd::layout::CustomLayout::load) {
                Some(Err(e)) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
                Some(Ok(l)) => Some(l),
                None => None,
            };
            let selections = match merge_selections(input.len(), &merge) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let opts = BuildOptions {
                inputs: input,
                disc,
                template: template.unwrap_or_default(),
                custom_layout,
                output,
                audio_langs: audio_lang,
                subtitle_langs: sub_lang,
                no_subtitles: no_subs,
                chapters: dvd::ChapterSettings { interval_secs: chapter_minutes * 60.0, ..Default::default() },
                menu_title,
                ui_lang: UiLang::new(&ui_lang),
                keep_work,
                two_pass,
                fast_encode: fast,
                motion_menu,
                motion_duration,
                motion_audio: match (motion_silent, menu_audio) {
                    (true, _) => MenuAudio::Silence,
                    (false, Some(file)) => MenuAudio::File(file),
                    (false, None) => MenuAudio::TitleAudio,
                },
                tv_standard: match tv {
                    Tv::Pal => TvStandard::Pal,
                    Tv::Ntsc => TvStandard::Ntsc,
                },
                resolution: resolution.map_or(if disc.is_bluray() { Resolution::Hd1080 } else { Resolution::FullD1 }, Into::into),
                bd_single_video: single_video,
                encoding_profile: encoding_profile.into(),
                project_name,
                selections,
                parallel_jobs: jobs.map(usize::from),
                hw_decode: !no_hw_decode,
                hw_accel: if no_hw_encode { HwAccelArg::None } else { hw_accel }.into(),
                menu_animations: !no_menu_animations,
                audio_mode: audio.into(),
                track_label_mode: track_labels.into(),
                loudnorm,
                video: musadvd::encoder::VideoProcessing {
                    scale: match scale {
                        ScaleArg::Letterbox => musadvd::encoder::ScaleMode::Letterbox,
                        ScaleArg::Panscan => musadvd::encoder::ScaleMode::PanScan,
                        ScaleArg::Stretch => musadvd::encoder::ScaleMode::Stretch,
                    },
                    filter: match scale_filter {
                        ScaleFilterArg::Bilinear => musadvd::encoder::ScaleFilter::Bilinear,
                        ScaleFilterArg::Bicubic => musadvd::encoder::ScaleFilter::Bicubic,
                        ScaleFilterArg::Lanczos => musadvd::encoder::ScaleFilter::Lanczos,
                    },
                    deinterlace: match deinterlace {
                        DeinterlaceArg::Auto => musadvd::encoder::Deinterlace::Auto,
                        DeinterlaceArg::Yadif => musadvd::encoder::Deinterlace::Yadif,
                        DeinterlaceArg::Off => musadvd::encoder::Deinterlace::Off,
                    },
                },
                ..Default::default()
            };
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: cannot start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Ctrl+C cancels the build: tools are killed, work directory is removed.
            // Pressing it a second time exits immediately.
            let cancel = opts.cancel.clone();
            runtime.spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    eprintln!("\nAbbruch angefordert (Ctrl+C) …");
                    cancel.cancel();
                    if tokio::signal::ctrl_c().await.is_ok() {
                        eprintln!("\nImmediate abort.");
                        std::process::exit(130);
                    }
                }
            });
            let mut last_pct = u32::MAX;
            let mut eta_text = String::new();
            let mut on_event = move |ev: Event| match ev {
                Event::Eta(e) => {
                    eta_text = match e.remaining_secs {
                        Some(s) => format!(" – {} left ([{}/{}] {})", musadvd::eta::format_remaining(s), e.phase_number, e.phase_count, e.phase_label),
                        None => String::new(),
                    };
                }
                // The CLI's step messages already say what is happening.
                Event::Phase(_) => {}
                Event::Step(s) => {
                    if last_pct != u32::MAX {
                        eprintln!();
                        last_pct = u32::MAX;
                    }
                    eprintln!("» {s}");
                }
                Event::Progress { label, fraction } => {
                    let pct = (fraction * 100.0).round() as u32;
                    if pct != last_pct {
                        eprint!("\r  {label}: {pct:>3}%{eta_text}    ");
                        last_pct = pct;
                    }
                }
                Event::Warning(w) => {
                    if last_pct != u32::MAX {
                        eprintln!();
                        last_pct = u32::MAX;
                    }
                    eprintln!("warning: {w}");
                }
            };
            match runtime.block_on(dvd::build_dvd(&opts, &mut on_event)) {
                Ok(r) => {
                    eprintln!();
                    if opts.disc.is_bluray() {
                        println!("{} finished: {}", if opts.disc.is_avchd() { "AVCHD" } else { "Blu-ray" }, r.video_ts.display());
                        let menus = if r.menu_pages == 0 { "no menu (autoplay)".to_string() } else { format!("{} menu(s)", r.menu_pages) };
                        println!("  Size {:.0} MB of {} MB ({:.1}%), {menus}", r.size_mb, r.target_mb, r.size_mb / r.target_mb as f64 * 100.0);
                    } else {
                        println!("DVD finished: {}", r.video_ts.display());
                        println!(
                            "  Size {:.0} MB of {} MB ({:.1}%), {} menu page(s)",
                            r.size_mb,
                            r.target_mb,
                            r.size_mb / r.target_mb as f64 * 100.0,
                            r.menu_pages
                        );
                    }
                    if r.motion_pages > 0 {
                        println!("  of which {} motion menu(s)", r.motion_pages);
                    }
                    if r.resolution != Resolution::FullD1 {
                        let (w, h) = r.resolution.frame_size(opts.tv_standard);
                        println!("  Titles in {} ({w}×{h})", r.resolution.label());
                    }
                    for t in &r.titles {
                        println!(
                            "  {} ({:.1} min, {}, {} kbps): audio {:?}, subtitles {:?}, {} chapters",
                            t.label,
                            t.duration_secs / 60.0,
                            t.aspect.as_str(),
                            t.video_kbps,
                            t.audio,
                            t.subtitles,
                            t.chapters
                        );
                    }
                    match &r.work_dir {
                        Some(w) => println!("  Work files (menu PNGs, XMLs, logs): {}", w.display()),
                        None => println!("  Work folder was removed (keep it with --keep-work)"),
                    }
                    ExitCode::SUCCESS
                }
                Err(musadvd::dvd::BuildError::Cancelled) => {
                    eprintln!();
                    eprintln!("Abgebrochen.");
                    ExitCode::from(130)
                }
                Err(e) => {
                    eprintln!();
                    eprintln!("error: {}", e.message(opts.ui_lang));
                    ExitCode::FAILURE
                }
            }
        }
    }
}
