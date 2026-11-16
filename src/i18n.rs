//! Engine-side UI language: the texts the engine writes onto discs, into track labels and into the
//! job log. They live in the same locale files as the GUI strings
//! (`frontend/src/locales/<code>.json`, keys prefixed `engine.`), which are read at runtime from
//! `<exe-dir>/locales`, `<exe-dir>/../lib/MusaDVD/locales` and `<exe-dir>/../share/musadvd/locales` (Linux packages), `<cwd>/locales` or the repo path, so a new language is just a new JSON file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, RwLock};

/// Language for the template's fixed UI texts ("Chapters", "Back", ...) and generated texts: a locale
/// code (`de`, `en`, ...) whose file is loaded on demand. Missing texts fall back to English, then to the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UiLang(pub &'static str);

impl Default for UiLang {
    fn default() -> Self {
        Self::EN
    }
}

impl UiLang {
    pub const DE: UiLang = UiLang("de");
    pub const EN: UiLang = UiLang("en");

    /// `code` as a language, without checking that a locale file exists. Each distinct code is leaked once.
    pub fn new(code: &str) -> Self {
        static CODES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        let mut codes = CODES.lock().unwrap();
        if let Some(&known) = codes.iter().find(|&&c| c == code) {
            return UiLang(known);
        }
        let leaked: &'static str = Box::leak(code.to_owned().into_boxed_str());
        codes.push(leaked);
        UiLang(leaked)
    }

    /// The language of a setting value; anything without a loadable locale file is the default (`en`).
    pub fn from_setting(s: &str) -> Self {
        let code = s.trim().to_ascii_lowercase();
        if valid_code(&code) && catalog(&code).is_some() {
            Self::new(&code)
        } else {
            Self::EN
        }
    }
}

pub type Catalog = HashMap<String, String>;

/// Candidate locale directories in lookup order (like [`crate::tools::bundled_bin_dirs`]).
fn locales_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(dir.join("locales"));
        // Linux packages (.deb, AppImage): Tauri puts resources in `<prefix>/lib/<productName>`,
        // the executable in `<prefix>/bin`.
        dirs.push(dir.join("../lib/MusaDVD/locales"));
        // Arch package (`packaging/aur/PKGBUILD`): `/usr/bin` + `/usr/share/musadvd/locales`.
        dirs.push(dir.join("../share/musadvd/locales"));
    }
    let candidates = [std::env::current_dir().ok().map(|d| d.join("locales")), Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/frontend/src/locales")))];
    for dir in candidates.into_iter().flatten() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

fn locales_dir() -> Option<PathBuf> {
    locales_dirs().into_iter().find(|d| d.is_dir())
}

fn valid_code(code: &str) -> bool {
    !code.is_empty() && code.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

/// Reads `<dir>/<code>.json` as a flat map of strings.
pub fn load_from(dir: &Path, code: &str) -> Result<Catalog, String> {
    if !valid_code(code) {
        return Err(format!("Invalid language code: {code:?}"));
    }
    let path = dir.join(format!("{code}.json"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{} unreadable: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not a valid language file: {e}", path.display()))
}

/// [`load_from`] in the first existing locales directory, fresh from disk.
pub fn load(code: &str) -> Result<Catalog, String> {
    load_from(&locales_dir().ok_or("No locales folder found")?, code)
}

/// `(code, display name)` of every locale file in `dir` (`_meta.name`, else the code), sorted by code.
pub fn languages_in(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(code) = path.file_stem().and_then(|s| s.to_str()).filter(|c| valid_code(c)) else { continue };
        match load_from(dir, code) {
            Ok(cat) => out.push((code.to_string(), cat.get("_meta.name").cloned().unwrap_or_else(|| code.to_string()))),
            Err(e) => eprintln!("Language file skipped: {e}"),
        }
    }
    out.sort();
    out
}

pub fn available_languages() -> Vec<(String, String)> {
    locales_dir().map(|d| languages_in(&d)).unwrap_or_default()
}

static CATALOGS: LazyLock<RwLock<HashMap<String, &'static Catalog>>> = LazyLock::new(Default::default);

/// Cached catalog of `code`, loaded on first use. `tr` hands out `&'static str`, hence the leak.
fn catalog(code: &str) -> Option<&'static Catalog> {
    if let Some(&cat) = CATALOGS.read().unwrap().get(code) {
        return Some(cat);
    }
    let cat: &'static Catalog = Box::leak(Box::new(load(code).ok()?));
    CATALOGS.write().unwrap().insert(code.to_string(), cat);
    Some(cat)
}

/// Drops the cache so edited or added locale files are picked up on the next lookup.
// shortcut: each reload leaks the old catalogs (~few hundred KB per language switch), fine for a desktop app; switch to Arc<Catalog> + owned Strings if reloads become frequent.
pub fn reload() {
    CATALOGS.write().unwrap().clear();
}

/// `full` in the language's catalog, then in English.
fn lookup<'a>(cat: Option<&'a Catalog>, en: Option<&'a Catalog>, full: &str) -> Option<&'a str> {
    cat.and_then(|c| c.get(full)).or_else(|| en.and_then(|c| c.get(full))).map(String::as_str)
}

/// Text of the engine key `key` (`engine.<key>` in the locale file) in `ui`; falls back to English
/// (the master file), then to the key itself.
pub fn tr(ui: UiLang, key: &'static str) -> &'static str {
    tr_key(ui, &format!("engine.{key}")).unwrap_or(key)
}

/// Text of an arbitrary locale key (current language, then English), e.g. `lang.de`.
pub fn tr_key(ui: UiLang, full_key: &str) -> Option<&'static str> {
    lookup(catalog(ui.0), catalog("en"), full_key)
}

/// [`tr`] with `{name}` placeholders replaced.
pub fn trf(ui: UiLang, key: &'static str, args: &[(&str, String)]) -> String {
    args.iter().fold(tr(ui, key).to_string(), |s, (k, v)| s.replace(&format!("{{{k}}}"), v))
}

/// `tf!(ui, "key", name = value, ...)`: [`trf`] with `Display` values.
#[macro_export]
macro_rules! tf {
    ($ui:expr, $key:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::i18n::trf($ui, $key, &[$((stringify!($k), $v.to_string())),*])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s.split('{').skip(1).filter_map(|p| p.split('}').next()).collect();
        v.sort_unstable();
        v
    }

    fn engine_keys() -> Vec<&'static str> {
        catalog("en").unwrap().keys().filter_map(|k| k.strip_prefix("engine.")).collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cx-i18n-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_engine_key_is_translated_with_the_same_placeholders() {
        let keys = engine_keys();
        assert!(keys.len() > 50);
        let langs = available_languages();
        assert!(langs.len() >= 3, "found only {langs:?}");
        for (code, _) in &langs {
            let ui = UiLang::new(code);
            for key in &keys {
                let full = format!("engine.{key}");
                let text = tr_key(ui, &full).unwrap();
                assert!(catalog(code).unwrap().contains_key(&full), "{code} lacks {key}");
                assert_eq!(placeholders(text), placeholders(tr_key(UiLang::EN, &full).unwrap()), "placeholders of {key} in {code}");
            }
        }
    }

    /// Every `tr(ui, "key")` / `tf!(ui, "key", ..)` in the sources names an existing key.
    #[test]
    fn keys_used_in_the_sources_exist() {
        let known = engine_keys();
        let mut used = 0;
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap().chain(std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src-tauri/src")).unwrap()) {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("i18n.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for marker in ["tr(", "tf!(", "tr(&"] {
                for (at, _) in text.match_indices(marker) {
                    if at > 0 && text[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                        continue; // e.g. `str(`, `entry(`
                    }
                    let rest = &text[at + marker.len()..];
                    let Some(comma) = rest.find(',') else { continue };
                    let after = rest[comma + 1..].trim_start();
                    if let Some(lit) = after.strip_prefix('"') {
                        let key = lit.split('"').next().unwrap();
                        assert!(known.contains(&key), "{}: unknown engine key {key:?}", path.display());
                        used += 1;
                    }
                }
            }
        }
        assert!(used > 50, "found only {used} key uses");
    }

    #[test]
    fn setting_maps_to_language() {
        assert_eq!(UiLang::from_setting("EN"), UiLang::EN);
        assert_eq!(UiLang::from_setting("xx"), UiLang::EN);
    }

    #[test]
    fn hot_added_language_is_discovered() {
        let dir = temp_dir("hot");
        std::fs::write(dir.join("en.json"), r#"{"_meta.name":"English","engine.a":"A","engine.b":"B"}"#).unwrap();
        assert_eq!(languages_in(&dir), [("en".to_string(), "English".to_string())]);
        std::fs::write(dir.join("xx.json"), r#"{"_meta.name":"Testisch","engine.a":"AX"}"#).unwrap();
        assert!(languages_in(&dir).contains(&("xx".to_string(), "Testisch".to_string())));
        let (en, xx) = (load_from(&dir, "en").unwrap(), load_from(&dir, "xx").unwrap());
        assert_eq!(lookup(Some(&xx), Some(&en), "engine.a"), Some("AX"));
        assert_eq!(lookup(Some(&xx), Some(&en), "engine.b"), Some("B"));
        assert_eq!(lookup(Some(&xx), Some(&en), "engine.c"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_codes_are_rejected() {
        let dir = temp_dir("invalid");
        assert!(load_from(&dir, "../x").is_err());
        assert!(load_from(&dir, "a/b").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn placeholders_are_filled() {
        assert_eq!(tf!(UiLang::EN, "title_finished", done = 1, count = 2, title = "A"), "[1/2] A finished");
        assert_eq!(tf!(UiLang::DE, "title_finished", done = 1, count = 2, title = "A"), "[1/2] A fertig");
    }
}
