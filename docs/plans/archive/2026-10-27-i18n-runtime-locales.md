# Plan: runtime-loaded locale files (hot-swappable UI languages)

> 2026-10-27 · approved by the user · implement exactly as written; deviations only where the compiler forces them.

## Goal & approach

Locale files `*.json` are no longer embedded (`include_str!` / `import.meta.glob`). Engine and GUI read
`locales/*.json` at runtime; dropping a new `xx.json` makes a new language appear without recompiling.
Changed texts take effect after a language switch / on the next window open. Fallback chain stays
`language -> en -> key` (English is the master).

Deliberate design to avoid signature churn (~80 call sites with `opts.ui_lang` / `tr(ui, …)` in
`dvd.rs`, `bd.rs`, `menu.rs`, `probe.rs`, `eta.rs`, `bd_template.rs` stay untouched):

- `UiLang` becomes `#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)] pub struct UiLang(pub &'static str);`
  with `UiLang::DE`, `UiLang::EN`, `UiLang::new(code)`, `UiLang::from_setting(s)`. `Default` = `DE`.
- Global cache `static CATALOGS: LazyLock<RwLock<HashMap<String, &'static Catalog>>>`
  (`type Catalog = HashMap<String, String>`; `std::sync::LazyLock`, no `once_cell` in the project).
  `tr()` MUST keep returning `&'static str` (e.g. `bd.rs` `if … { tr(..) } else { "" }`), so loaded
  catalogs are `Box::leak`ed. Mark with
  `// shortcut: each reload leaks the old catalogs (~few hundred KB per language switch), fine for a desktop app; switch to Arc<Catalog> + owned Strings if reloads become frequent.`
- Interning in `UiLang::new` / `from_setting`: leak each distinct code only once (look it up in a small
  static set first) – `from_setting` runs per job.

## Files

| File | Change |
|---|---|
| `src/i18n.rs` | Remove `include_str!`, enum, `ALL`, `UI_LANGUAGES`, old `catalog()` match. Add `locales_dirs`, `load`, `load_from`, `available_languages`, `languages_in`, `reload`, cached `catalog(code)`, new `UiLang`. Update tests. |
| `src/config.rs` | `sanitized()`: `ui_language` must be a code from `available_languages()`, else `"de"`; default stays `"de"`. Drop `UI_LANGUAGES` import; `UiLang::En` → `UiLang::EN`. New test. |
| `src/main.rs` (CLI) | `ui_lang: Lang` (ValueEnum) → `String` (default `"de"`); unknown code → error listing available codes; remove `enum Lang` and the `match`. Pass `UiLang::new(&code)`. |
| `src-tauri/src/main.rs` | Commands `get_available_languages() -> Vec<LanguageOption>` (`#[derive(Serialize)] struct LanguageOption { code: String, name: String }`) and `get_locale_data(lang: String) -> Result<HashMap<String, String>, String>` (= `i18n::load`, fresh from disk); register in `generate_handler!`. `save_settings` calls `convertx_rust::i18n::reload()` after saving. `UiLang::default()` / `from_setting` uses stay. |
| `src-tauri/tauri.conf.json` | `bundle.resources` += `"../frontend/src/locales/*.json": "locales/"` (same pattern as `bin/`; NSIS puts resources next to the exe → `<exe-dir>/locales`). |
| `src/bd_template.rs`, `tests/build_dvd.rs`, `tests/menu_intro.rs`, `tests/motion_menu.rs`, `tests/real_templates.rs`, `tests/two_pass.rs` (+ any other hit of `grep -rn "UiLang::"`) | Mechanical: `UiLang::De` → `UiLang::DE`, `UiLang::En` → `UiLang::EN`, `UiLang::Ru` → `UiLang::new("ru")`. |
| `frontend/src/api.ts` | `availableLanguages = () => invoke<{code: string; name: string}[]>("get_available_languages")`, `localeData = (lang: string) => invoke<Record<string, string>>("get_locale_data", { lang })`. |
| `frontend/src/i18n.ts` | Remove `/// <reference types="vite/client" />`, `import.meta.glob`, `availableLanguages()`, `setLanguage`. Keep `master` (en) + `cat` (current) dictionaries. `export async function loadLanguage(code)`: load `en` and `code` in parallel (`Promise.all`); on error retry with `de`, then en only; set `current` + `document.documentElement.lang`. `initI18n` awaits `loadLanguage(settings.ui_language)` (settings error → `loadLanguage(DEFAULT)`), then `applyStatic()`. `t()`, `fmtNumber`, `applyStatic`, `currentLanguage` stay synchronous. |
| `frontend/src/boot.ts` | No change expected (`initI18n(getSettings)` still awaited first) – verify only. |
| `frontend/src/settings.ts` | `openSettings(current, info, languages: {code: string; name: string}[], onSaved)`; dropdown built from `languages`. Drop the `availableLanguages` import from `./i18n`. (`burnStudio.ts` has its own unrelated `openSettings`.) |
| `frontend/src/main.ts` | Settings click: `const [info, languages] = await Promise.all([settingsInfo(), availableLanguages()])`; pass `languages`. In `onSaved`, when `langChanged`: `await loadLanguage(saved.ui_language)` (make the callback async), then the existing `applyStatic(); renderAll();`. Import `loadLanguage` instead of `setLanguage`. |
| `vite.config.ts`, `scripts/check-i18n.mjs` | No change: the script already reads the raw JSON via `fs`; verify it still passes. |

## Implementation order

1. **`src/i18n.rs`**
   - `fn locales_dirs() -> Vec<PathBuf>`: `<exe-dir>/locales`, `<cwd>/locales`,
     `concat!(env!("CARGO_MANIFEST_DIR"), "/frontend/src/locales")` (dedupe like `tools::bundled_bin_dirs`).
     `fn locales_dir() -> Option<PathBuf>` = first existing one.
   - `fn valid_code(code) -> bool`: non-empty, only `[a-z0-9_-]` (same as regex `^[a-z0-9_-]+$`; a char
     check is enough).
   - `pub fn load_from(dir: &Path, code: &str) -> Result<Catalog, String>`: validate code (German error
     "Ungültiger Sprachcode: …"), read `<dir>/<code>.json`, parse as flat string map (German errors with
     file path). `pub fn load(code) -> Result<Catalog, String>` = `load_from(locales_dir()?, code)`
     ("Kein locales-Verzeichnis gefunden" when none).
   - `pub fn languages_in(dir: &Path) -> Vec<(String, String)>`: every `*.json` with a valid stem, parsed;
     name = `_meta.name` or code; broken file → `eprintln!` warning and skip; sorted by code.
     `pub fn available_languages()` = `locales_dir().map(languages_in).unwrap_or_default()`.
   - `fn catalog(code: &str) -> Option<&'static Catalog>`: read lock lookup; miss → `load`, leak, insert
     (also cache failures as absent? no – just return `None`, retry next time is fine).
   - `pub fn reload()`: clear `CATALOGS` (leaked catalogs stay valid).
   - `pub fn tr(ui: UiLang, key: &'static str) -> &'static str`: `engine.{key}` in `ui`, then `en`, then
     `key`. `tr_key(ui, full_key: &str) -> Option<&'static str>` unchanged semantics. `trf`, `tf!` unchanged.
     Fix any non-literal `tr` caller the compiler reports (none expected).
   - `UiLang::from_setting(s)`: trim + lowercase; if `valid_code` and the file loads (or is in
     `available_languages()`), intern and return it; else `UiLang::DE`.
2. Adapt `config.rs`, CLI `src/main.rs`, `bd_template.rs`, tests until
   `cargo check --workspace --tests` is green.
3. Tauri commands + `reload()` in `save_settings`.
4. `tauri.conf.json` resources entry.
5. `api.ts` + `i18n.ts`.
6. `settings.ts` + `main.ts`.
7. Verification (below), then `/status` (see "Docs").

## Automated verification (mandatory)

New / updated Rust tests in `src/i18n.rs` (temp dirs via `std::env::temp_dir().join(format!("cx-i18n-{}-{name}", std::process::id()))`, no new dev-dependency; remove afterwards):

1. `hot_added_language_is_discovered`: temp dir with a small `en.json` (`_meta.name`, `engine.a`,
   `engine.b`) and a newly written `xx.json` (`_meta.name` = "Testisch", only `engine.a`).
   `languages_in` contains `("xx", "Testisch")`; lookup through the same fallback helper `tr` uses gives
   xx's `a`, en's `b`, and the key itself for a missing key. (Factor the fallback into a pure
   `fn lookup(cat: Option<&Catalog>, en: Option<&Catalog>, full: &str) -> Option<&str>` so it is testable
   without the global cache.)
2. `invalid_codes_are_rejected`: `load_from(dir, "../x")` and `load_from(dir, "a/b")` are `Err`.
3. `every_engine_key_is_translated_with_the_same_placeholders`: iterates `available_languages()`
   (asserts ≥ 3 languages, i.e. de/en/ru are found through `CARGO_MANIFEST_DIR`).
4. `setting_maps_to_language`: `"xx"` (no file) → `UiLang::DE`, `"EN"` → `UiLang::EN`.
5. Existing `keys_used_in_the_sources_exist`, `placeholders_are_filled` stay green.

`src/config.rs`: test that `ui_language = "xx"` sanitizes to `"de"` and `"ru"` stays `"ru"`.

Must be green (PATH prefix from CLAUDE.md):

- `cargo test --workspace`
- `cargo check -p convertx-gui`
- `cargo check --lib --tests --target x86_64-unknown-linux-gnu`
- `npm run build` (includes `check-i18n` + `tsc`; proves no `import.meta.glob` leftovers)

Not-embedded check: after `npm run build`, `grep -c "settings.uiLanguageHint" dist/assets/*.js` finds
nothing (the key may still appear only as a `t("…")` argument – check for a translated value such as
the English text of that key instead).

Manual acceptance (user, release GUI): copy `locales/` next to the exe, add `xx.json` with its own
`_meta.name` → appears in the dropdown without recompiling; switching to it updates GUI live and the job
log uses its engine texts.

## Docs (`/status` at the end)

- STATUS.md i18n paragraph: locale files are read at runtime (`<exe-dir>/locales`, `<cwd>/locales`, repo
  path); "new language = drop a JSON file"; remove the "embedded, NOT hot-swappable" sentence and the
  enum/`UI_LANGUAGES` recipe.
- Quirk: a release exe without the bundle needs `locales/` next to the exe (else only keys are shown);
  each reload leaks the old catalogs (shortcut).

## Out of scope

Configurable locale path (setting/env var), file watcher (auto-reload on JSON save), live switching of
already open editor/Burn Studio windows (they pick it up on reopen, as before).
