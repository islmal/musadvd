# Settings and internationalisation

## Settings file

`config::AppSettings` is stored as JSON in `%APPDATA%\musadvd\settings.json` (fallback
`./musadvd-settings.json` when `APPDATA` is unset). Rules:

- Unknown fields are ignored and missing fields take defaults, so files from older and newer
  versions keep loading.
- `AppSettings::sanitized()` clamps numbers and replaces unknown choices with defaults. Enums that
  grow use `#[serde(other)]` on the last variant (e.g. `HwAccelMode`).
- Burn Studio settings are the `burn_studio` section (`BurnStudioSettings`: general, read, filter,
  build, write, verify, graph, io, speed rules, per-operation overrides). The TypeScript mirror is
  `frontend/src/studio/api.ts`.
- The GUI reads and writes through the Tauri commands `get_settings` / `save_settings`; the CLI
  builds `BuildOptions` from its flags and ignores most stored settings.
- Settings that the engine does not act on stay visible in the UI but disabled, with the reason.

## Locale files

One set of files, `frontend/src/locales/<code>.json`, serves both the GUI and the engine:

- Flat dotted keys, `{name}` placeholders, `_meta.name` is the language's display name.
- Keys prefixed `engine.` are used by the Rust side (`i18n::{tr, tf!}`), including fixed template
  texts (`engine.tpl.<English text>`) and language names (`lang.<iso639-1>`).
- **English (`en.json`) is the master.** Fallback chain: language, then `en`, then the key itself.
  The default UI language is `de`. Shipped: `de`, `en`, `ru`.
- Engine catalogs are read at runtime from `<exe-dir>/locales`, `<cwd>/locales` or the repository
  path. A release exe without `locales/` next to it shows only keys.
- `save_settings` calls `i18n::reload()`; each reload leaks the old catalogs (known shortcut).

### Adding a language

1. Copy `en.json` to `<code>.json` and set `_meta.name`.
2. Translate the values; keep keys and `{placeholders}` identical.
3. Run `npm run check:i18n` (key and placeholder parity against `en.json`, and no umlauts in `.ts`).
4. No recompile is needed for the engine; the GUI loads catalogs through `get_locale_data`.

### Rules for code

- New UI text: add the key to `en.json` first, then the other locales.
- Dialogs reopened in the main window must build their text lazily (after the catalog is loaded).
- Still not localised: `BuildError`/probe error details from some modules, dvdauthor-style
  messages, and the burn dialog texts in `src-tauri/src/burn.rs` (see STATUS.md known issues).
