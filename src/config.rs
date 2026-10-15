//! Global application settings ("Allgemeine Einstellungen"), persisted as JSON.
//!
//! Default location: `%APPDATA%\convertx-rust\settings.json`. Unknown fields are ignored and
//! missing fields take their defaults, so files from older or newer versions keep loading.
//! [`AppSettings::sanitized`] clamps numbers and replaces unknown choices with the defaults.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::author::{PlaybackOptions, TvStandard};
use crate::encoder::MenuAudio;
use crate::calculator::DiscType;
use crate::dvd::{default_jobs, AspectPolicy, BuildOptions, ChapterSettings};
use crate::menu::TextOverflow;
use crate::tools::ProcessPriority;

/// Choices for the string settings (first entry = default).
pub const TEXT_OVERFLOW_ACTIONS: &[&str] = &["reduce_font_size", "truncate"];
pub const TV_STANDARDS: &[&str] = &["PAL", "NTSC"];
pub const DVD_RESOLUTIONS: &[&str] = &["Full D1", "Half D1", "SIF"];
pub const ASPECT_POLICIES: &[&str] = &["Auto", "4:3", "16:9"];
pub const ENCODING_PROFILES: &[&str] = &["Auto", "SP_Short", "MP_Medium", "LP_Long"];
pub const PROCESS_PRIORITIES: &[&str] = &["Normal", "BelowNormal", "Idle"];
pub const MENU_AUDIO_MODES: &[&str] = &["title", "silence", "file"];
/// Write speed: `auto` = DVD-5 maximum, DVD-9 4x; otherwise maximum or a speed in x (mapped to
/// the nearest speed the drive offers).
pub const BURN_SPEEDS: &[&str] = &["auto", "max", "16", "12", "8", "6", "4", "2.4"];

/// Choices that are stored but not implemented by the engine yet (Full D1 / Auto only).
pub const UNSUPPORTED_CHOICES: &[&str] = &["Half D1", "SIF", "SP_Short", "MP_Medium", "LP_Long"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    // Menu options
    pub auto_start_playback: bool,
    pub loop_playback: bool,
    pub play_sequentially: bool,
    pub skip_root_menu: bool,
    pub return_to_title_menu_after_title: bool,
    pub no_title_menu_for_single_title: bool,
    pub text_overflow_action: String,
    /// Sound of the motion menus: `title` (main menu silent, video menus with their video's
    /// sound), `silence`, `file` (main menu with `menu_audio_file`, video menus as `title`).
    pub menu_audio: String,
    /// Audio file for `menu_audio = "file"`.
    pub menu_audio_file: String,
    // Output & TV format
    pub tv_standard: String,
    pub dvd_resolution: String,
    pub aspect_ratio_policy: String,
    // Encoding
    pub encoding_profile: String,
    /// Size used for the "Benutzerdefiniert" disc choice.
    pub target_size_mb: u64,
    pub max_concurrent_encodes: usize,
    pub process_priority: String,
    /// Decode the sources on the GPU (CUDA/D3D11VA/QSV) when available.
    pub hw_decode: bool,
    /// Draft encoding without trellis quantisation (about 40 % faster, slightly lower quality).
    pub fast_encode: bool,
    // Automatic chapters
    pub auto_chapter_enabled: bool,
    pub auto_chapter_interval_seconds: u32,
    pub auto_chapter_min_video_length_seconds: u32,
    /// `<c>` is replaced by the chapter number.
    pub chapter_naming_template: String,
    // Audio & subtitles
    /// ISO 639 code; empty = keep the source order.
    pub default_audio_language: String,
    pub default_subtitle_language: String,
    pub force_first_subtitle: bool,
    // Burning
    /// See [`BURN_SPEEDS`].
    pub burn_speed: String,
    /// Read the disc back and compare it with the image.
    pub verify_after_burn: bool,
    /// Keep the .iso next to the DVD folder.
    pub keep_iso: bool,
    /// After a successful burn: delete the DVD folder, the image and the work directory.
    pub delete_after_burn: bool,
    /// Neither open Explorer on the finished DVD folder nor eject the disc when done.
    pub quiet_finish: bool,
    /// Erase/overwrite rewritable discs with old content without asking (also lets the
    /// auto-start countdown run for them).
    pub auto_erase_rewritable: bool,
    /// Default project folder and volume label (A-Z, 0-9, _; max. 32); empty = from the menu
    /// heading / first video.
    pub default_volume_label: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            auto_start_playback: false,
            loop_playback: false,
            play_sequentially: false,
            skip_root_menu: false,
            return_to_title_menu_after_title: true,
            no_title_menu_for_single_title: false,
            text_overflow_action: TEXT_OVERFLOW_ACTIONS[0].into(),
            menu_audio: MENU_AUDIO_MODES[0].into(),
            menu_audio_file: String::new(),
            tv_standard: TV_STANDARDS[0].into(),
            dvd_resolution: DVD_RESOLUTIONS[0].into(),
            aspect_ratio_policy: ASPECT_POLICIES[0].into(),
            encoding_profile: ENCODING_PROFILES[0].into(),
            target_size_mb: DiscType::Dvd5.target_mb() as u64,
            max_concurrent_encodes: default_jobs(),
            process_priority: PROCESS_PRIORITIES[0].into(),
            hw_decode: false,
            fast_encode: false,
            auto_chapter_enabled: true,
            auto_chapter_interval_seconds: 300,
            auto_chapter_min_video_length_seconds: 900,
            chapter_naming_template: "Kapitel #<c>".into(),
            default_audio_language: "de".into(),
            default_subtitle_language: String::new(),
            force_first_subtitle: false,
            burn_speed: BURN_SPEEDS[0].into(),
            verify_after_burn: true,
            keep_iso: false,
            delete_after_burn: false,
            quiet_finish: false,
            auto_erase_rewritable: false,
            default_volume_label: String::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io { path: PathBuf, error: std::io::Error },
    Json { path: PathBuf, error: serde_json::Error },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            ConfigError::Json { path, error } => write!(f, "{}: ungültige Einstellungen ({error})", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

fn choice(value: &str, allowed: &[&str]) -> String {
    allowed.iter().find(|a| a.eq_ignore_ascii_case(value)).unwrap_or(&allowed[0]).to_string()
}

/// Lower-case ISO 639 code (`de`, `ger`), or empty if not a plausible code.
fn language(value: &str) -> String {
    let v = value.trim().to_ascii_lowercase();
    if (2..=3).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase()) {
        v
    } else {
        String::new()
    }
}

impl AppSettings {
    /// Clamps numbers into sensible ranges and normalises the string choices.
    pub fn sanitized(mut self) -> Self {
        self.text_overflow_action = choice(&self.text_overflow_action, TEXT_OVERFLOW_ACTIONS);
        self.tv_standard = choice(&self.tv_standard, TV_STANDARDS);
        self.menu_audio = choice(&self.menu_audio, MENU_AUDIO_MODES);
        self.menu_audio_file = self.menu_audio_file.trim().to_string();
        if self.menu_audio == "file" && self.menu_audio_file.is_empty() {
            self.menu_audio = MENU_AUDIO_MODES[0].into();
        }
        self.dvd_resolution = choice(&self.dvd_resolution, DVD_RESOLUTIONS);
        self.aspect_ratio_policy = choice(&self.aspect_ratio_policy, ASPECT_POLICIES);
        self.encoding_profile = choice(&self.encoding_profile, ENCODING_PROFILES);
        self.process_priority = choice(&self.process_priority, PROCESS_PRIORITIES);
        self.target_size_mb = self.target_size_mb.clamp(100, 20_000);
        self.max_concurrent_encodes = self.max_concurrent_encodes.clamp(1, 64);
        self.auto_chapter_interval_seconds = self.auto_chapter_interval_seconds.clamp(30, 3600);
        self.auto_chapter_min_video_length_seconds = self.auto_chapter_min_video_length_seconds.min(24 * 3600);
        if !self.chapter_naming_template.contains("<c>") {
            self.chapter_naming_template = AppSettings::default().chapter_naming_template;
        }
        self.chapter_naming_template = self.chapter_naming_template.trim().chars().take(60).collect();
        self.default_audio_language = language(&self.default_audio_language);
        self.default_subtitle_language = language(&self.default_subtitle_language);
        self.burn_speed = choice(&self.burn_speed, BURN_SPEEDS);
        self.default_volume_label = if self.default_volume_label.trim().is_empty() {
            String::new()
        } else {
            crate::burner::sanitize_volume_label(self.default_volume_label.trim())
        };
        self
    }

    /// `%APPDATA%\convertx-rust\settings.json` (or `./convertx-settings.json` without APPDATA).
    pub fn default_path() -> PathBuf {
        match std::env::var_os("APPDATA") {
            Some(dir) => PathBuf::from(dir).join("convertx-rust").join("settings.json"),
            None => PathBuf::from("convertx-settings.json"),
        }
    }

    /// Loads the settings; a missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<AppSettings>(&text)
                .map(AppSettings::sanitized)
                .map_err(|error| ConfigError::Json { path: path.to_path_buf(), error }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
            Err(error) => Err(ConfigError::Io { path: path.to_path_buf(), error }),
        }
    }

    /// Like [`load`](Self::load), but a corrupt file is renamed to `*.json.bak` and the defaults
    /// are returned together with a message, so the application always starts.
    pub fn load_or_default(path: &Path) -> (Self, Option<String>) {
        match Self::load(path) {
            Ok(s) => (s, None),
            Err(e @ ConfigError::Json { .. }) => {
                let backup = path.with_extension("json.bak");
                let moved = std::fs::rename(path, &backup).is_ok();
                let note = if moved {
                    format!("{e} – gesichert als {}, Standardwerte werden verwendet", backup.display())
                } else {
                    format!("{e} – Standardwerte werden verwendet")
                };
                (AppSettings::default(), Some(note))
            }
            Err(e) => (AppSettings::default(), Some(e.to_string())),
        }
    }

    /// Writes the settings atomically (temporary file + rename).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let io = |error| ConfigError::Io { path: path.to_path_buf(), error };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let json = serde_json::to_string_pretty(&self.clone().sanitized()).expect("settings serialize");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }

    pub fn playback(&self) -> PlaybackOptions {
        PlaybackOptions {
            auto_start: self.auto_start_playback,
            play_sequentially: self.play_sequentially,
            loop_playback: self.loop_playback,
            return_to_title_menu: self.return_to_title_menu_after_title,
        }
    }

    pub fn chapters(&self) -> ChapterSettings {
        ChapterSettings {
            interval_secs: if self.auto_chapter_enabled { self.auto_chapter_interval_seconds as f64 } else { 0.0 },
            min_video_secs: self.auto_chapter_min_video_length_seconds as f64,
            label_template: Some(self.chapter_naming_template.clone()),
        }
    }

    /// Transfers the settings the engine implements onto build options.
    pub fn apply_to(&self, opts: &mut BuildOptions) {
        opts.playback = self.playback();
        opts.skip_root_menu = self.skip_root_menu;
        opts.no_title_menu_for_single_title = self.no_title_menu_for_single_title;
        opts.text_overflow = if self.text_overflow_action == "truncate" {
            TextOverflow::Truncate
        } else {
            TextOverflow::ReduceFontSize
        };
        opts.aspect_policy = match self.aspect_ratio_policy.as_str() {
            "4:3" => AspectPolicy::Standard,
            "16:9" => AspectPolicy::Wide,
            _ => AspectPolicy::Auto,
        };
        opts.parallel_jobs = Some(self.max_concurrent_encodes);
        opts.process_priority = match self.process_priority.as_str() {
            "BelowNormal" => ProcessPriority::BelowNormal,
            "Idle" => ProcessPriority::Idle,
            _ => ProcessPriority::Normal,
        };
        opts.chapters = self.chapters();
        let lang = |l: &str| (!l.is_empty()).then(|| l.to_string());
        opts.preferred_audio_lang = lang(&self.default_audio_language);
        opts.preferred_subtitle_lang = lang(&self.default_subtitle_language);
        opts.force_first_subtitle = self.force_first_subtitle;
        opts.hw_decode = self.hw_decode;
        opts.fast_encode = self.fast_encode;
        if opts.project_name.is_none() && !self.default_volume_label.is_empty() {
            opts.project_name = Some(self.default_volume_label.clone());
        }
        opts.tv_standard = if self.tv_standard == "NTSC" { TvStandard::Ntsc } else { TvStandard::Pal };
        opts.motion_audio = match self.menu_audio.as_str() {
            "silence" => MenuAudio::Silence,
            "file" if !self.menu_audio_file.is_empty() => MenuAudio::File(PathBuf::from(&self.menu_audio_file)),
            _ => MenuAudio::TitleAudio,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cx_config_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("settings.json")
    }

    #[test]
    fn defaults() {
        let s = AppSettings::default();
        assert_eq!(s.target_size_mb, 4300);
        assert_eq!(s.chapter_naming_template, "Kapitel #<c>");
        assert_eq!(s.default_audio_language, "de");
        assert!(s.return_to_title_menu_after_title);
        assert_eq!(s.max_concurrent_encodes, default_jobs(), "same default as the engine");
        assert_eq!(s.clone().sanitized(), s, "defaults are already valid");
    }

    #[test]
    fn roundtrip_and_partial_files() {
        let path = temp("roundtrip");
        let mut s = AppSettings::default();
        s.loop_playback = true;
        s.process_priority = "Idle".into();
        s.save(&path).unwrap();
        assert_eq!(AppSettings::load(&path).unwrap(), s);
        assert!(!path.with_extension("json.tmp").exists());

        // Older/newer files: missing fields get defaults, unknown fields are ignored.
        std::fs::write(&path, r#"{"loop_playback": true, "some_future_option": 42}"#).unwrap();
        let s = AppSettings::load(&path).unwrap();
        assert!(s.loop_playback);
        assert_eq!(s.target_size_mb, 4300);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_file_is_default() {
        assert_eq!(AppSettings::load(&temp("missing")).unwrap(), AppSettings::default());
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let path = temp("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let (s, note) = AppSettings::load_or_default(&path);
        assert_eq!(s, AppSettings::default());
        assert!(note.unwrap().contains("Standardwerte"));
        assert!(path.with_extension("json.bak").is_file());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn sanitizing() {
        let s = AppSettings {
            tv_standard: "secam".into(),
            aspect_ratio_policy: "16:9".into(),
            process_priority: "realtime".into(),
            max_concurrent_encodes: 0,
            target_size_mb: 5,
            auto_chapter_interval_seconds: 1,
            chapter_naming_template: "ohne Platzhalter".into(),
            default_audio_language: "Deutsch".into(),
            default_subtitle_language: " EN ".into(),
            menu_audio: "file".into(),
            menu_audio_file: "  ".into(),
            ..AppSettings::default()
        }
        .sanitized();
        assert_eq!(s.menu_audio, "title", "file mode without a file falls back");
        let old = AppSettings { menu_audio: "first_title".into(), ..AppSettings::default() }.sanitized();
        assert_eq!(old.menu_audio, "title", "value of older versions is migrated");
        assert_eq!(s.tv_standard, "PAL");
        assert_eq!(s.aspect_ratio_policy, "16:9");
        assert_eq!(s.process_priority, "Normal");
        assert_eq!(s.max_concurrent_encodes, 1);
        assert_eq!(s.target_size_mb, 100);
        assert_eq!(s.auto_chapter_interval_seconds, 30);
        assert_eq!(s.chapter_naming_template, "Kapitel #<c>");
        assert_eq!(s.default_audio_language, "");
        assert_eq!(s.default_subtitle_language, "en");
    }

    #[test]
    fn applies_to_build_options() {
        let s = AppSettings {
            play_sequentially: true,
            skip_root_menu: true,
            aspect_ratio_policy: "4:3".into(),
            max_concurrent_encodes: 3,
            process_priority: "BelowNormal".into(),
            auto_chapter_enabled: false,
            text_overflow_action: "truncate".into(),
            default_subtitle_language: "en".into(),
            ..AppSettings::default()
        };
        let mut o = BuildOptions::default();
        s.apply_to(&mut o);
        assert!(o.playback.play_sequentially && o.skip_root_menu);
        assert_eq!(o.aspect_policy, AspectPolicy::Standard);
        assert_eq!(o.parallel_jobs, Some(3));
        assert_eq!(o.process_priority, ProcessPriority::BelowNormal);
        assert_eq!(o.chapters.interval_secs, 0.0, "automatic chapters off");
        assert_eq!(o.text_overflow, TextOverflow::Truncate);
        assert_eq!(o.preferred_audio_lang.as_deref(), Some("de"));
        assert_eq!(o.preferred_subtitle_lang.as_deref(), Some("en"));
        assert_eq!(o.tv_standard, TvStandard::Pal);
        assert_eq!(o.motion_audio, MenuAudio::TitleAudio);

        let s = AppSettings {
            tv_standard: "NTSC".into(),
            menu_audio: "file".into(),
            menu_audio_file: r"C:\Musik\menu.mp3".into(),
            ..AppSettings::default()
        };
        s.apply_to(&mut o);
        assert_eq!(o.tv_standard, TvStandard::Ntsc);
        assert_eq!(o.motion_audio, MenuAudio::File(r"C:\Musik\menu.mp3".into()));
        let s = AppSettings { menu_audio: "silence".into(), ..AppSettings::default() };
        s.apply_to(&mut o);
        assert_eq!(o.motion_audio, MenuAudio::Silence);
        assert!(!UNSUPPORTED_CHOICES.contains(&"NTSC"));
    }
}

#[cfg(test)]
mod burn_settings_tests {
    use super::*;

    #[test]
    fn burn_settings_are_sanitised_and_applied() {
        let s = AppSettings {
            burn_speed: "99x".into(),
            default_volume_label: "  Urlaub am Meer 2025!  ".into(),
            hw_decode: true,
            fast_encode: true,
            ..AppSettings::default()
        }
        .sanitized();
        assert_eq!(s.burn_speed, "auto", "unknown speed falls back to auto");
        assert_eq!(s.default_volume_label, "URLAUB_AM_MEER_2025_");
        assert_eq!(AppSettings { burn_speed: "2.4".into(), ..AppSettings::default() }.sanitized().burn_speed, "2.4");
        assert_eq!(AppSettings { default_volume_label: "   ".into(), ..AppSettings::default() }.sanitized().default_volume_label, "");

        let mut opts = BuildOptions::default();
        s.apply_to(&mut opts);
        assert!(opts.hw_decode && opts.fast_encode);
        assert_eq!(opts.project_name.as_deref(), Some("URLAUB_AM_MEER_2025_"), "default project name");
        // An explicit project name wins.
        let mut opts = BuildOptions { project_name: Some("Mein Film".into()), ..BuildOptions::default() };
        s.apply_to(&mut opts);
        assert_eq!(opts.project_name.as_deref(), Some("Mein Film"));

        // Old settings files without the new fields keep loading with the defaults.
        let old: AppSettings = serde_json::from_str(r#"{"tv_standard":"NTSC"}"#).unwrap();
        assert_eq!(old.burn_speed, "auto");
        assert!(old.verify_after_burn && !old.delete_after_burn && !old.quiet_finish && !old.auto_erase_rewritable);
        assert!(!old.fast_encode, "full-quality encoding unless switched on");
    }
}
