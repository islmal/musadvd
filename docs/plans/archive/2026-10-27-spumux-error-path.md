# Plan: spumux error path + STATUS changelog repair (2026-10-27)

## Goal & approach
`SpumuxError::TooComplex` currently reaches the GUI as `ToolError::Failed` carrying a hard-coded German
text (`src/spumux.rs` Display), shown via `e.to_string()` in `ConversionDone.message`. Make the error
typed end to end and localise it only where it is shown:

`SpumuxError::TooComplex(n)` → `ToolError::MenuTooComplex { buttons }` → `BuildError::MenuTooComplex { buttons }`
→ `tf!(ui_lang, "err_menu_too_complex", buttons = n)` from the locale files.

Also restore the deleted "Live player hardening" changelog entry in STATUS.md.

## Files
| File | Change |
|---|---|
| `src/spumux.rs` | `TooComplex` Display → neutral English, no user advice: `button group search exceeded its limit ({n} buttons)`. Other German Display texts stay (out of scope). |
| `src/tools.rs` | New `ToolError::MenuTooComplex { buttons: usize }` + Display arm (English). In `Toolchain::spumux` add an arm `Ok(Err(crate::spumux::SpumuxError::TooComplex(n))) => return Err(ToolError::MenuTooComplex { buttons: n })` **before** the generic `Ok(Err(e))` arm. `ffmpeg_spumux_pipeline` unchanged (titles have no buttons, `TooComplex` cannot occur). |
| `src/dvd.rs` | New `BuildError::MenuTooComplex { buttons: usize }`. Remove `ToolError => Tool` from the `from_err!` invocation (line ~250) and write a manual `impl From<ToolError> for BuildError`: `MenuTooComplex { buttons }` → `BuildError::MenuTooComplex { buttons }`, everything else → `BuildError::Tool(e)`. The `?` at the menu spumux call (`tools.spumux(&work, &xml, &raw, &vob, 0).await?`) then needs no change. Add `pub fn message(&self, ui: UiLang) -> String`: `MenuTooComplex` → `tf!(ui, "err_menu_too_complex", buttons = buttons)`, otherwise `self.to_string()`. The Display arm for `MenuTooComplex` = `self.message(UiLang::EN)`. |
| `src-tauri/src/main.rs` | In the `Err(e) => ConversionDone { ok: false, message: e.to_string(), .. }` arm of the convert command: `message: e.message(opts.ui_lang)`. |
| `src/main.rs` | CLI `Err(e)` arm of `build_dvd`: `eprintln!("error: {}", e.message(opts.ui_lang))`. |
| `frontend/src/locales/en.json` (master), `de.json`, `ru.json` | Key `engine.err_menu_too_complex`, placeholder `{buttons}`, next to the other `engine.*` keys. ru.json is required: `scripts/check-i18n.mjs` fails on keys missing from any locale. Suggested texts: en "The menu with {buttons} buttons cannot be split into colour groups (search limit exceeded) – please simplify the buttons or colours"; de "Menü mit {buttons} Buttons lässt sich nicht in Farbgruppen aufteilen (Suchlimit überschritten) – bitte Buttons/Farben vereinfachen"; ru equivalent. |
| `STATUS.md` | Re-insert verbatim in the Milestone Changelog between the "i18n German/English/Russian" and "Template menu intros on BD/AVCHD" entries: `- **2026-10-26 – Live player hardening:** 8-bit proxy, HDR tonemapping (also in encodes), shared \`source_chain\`. Verified: unit + stream tests; not in the release window.` (6 of max 10 entries). |

## Order
1. `spumux.rs` neutral text.
2. `ToolError::MenuTooComplex` + mapping in `Toolchain::spumux`.
3. `BuildError::MenuTooComplex`, manual `From<ToolError>`, `message()`.
4. GUI + CLI on `message(ui_lang)`.
5. Locale keys en/de/ru.
6. STATUS.md changelog entry.

## Automated verification
- **New unit test in `src/dvd.rs`** (`#[cfg(test)]`): `BuildError::from(ToolError::MenuTooComplex { buttons: 12 })` matches `BuildError::MenuTooComplex { buttons: 12 }`; `message(UiLang::DE)` contains "12" and neither the raw key nor `engine.`; `message(UiLang::EN) != message(UiLang::DE)`; `BuildError::from(ToolError::Failed { .. })` is still `BuildError::Tool(_)`.
- `node scripts/check-i18n.mjs` (and `npm run build`): keys/placeholders consistent across all locales.
- `cargo test --workspace`: existing `TooComplex(12)` unit test in `spumux.rs` and `tests/spumux_native.rs` stay green.
- `cargo check -p convertx-gui` and `cargo check --lib --tests --target x86_64-unknown-linux-gnu`.
- `rg "Suchlimit|Farbgruppen" src/` → no hits.

## Known limits
- The route from `Toolchain::spumux` is covered by the unit test only, not by a real 12-button menu in a full build.
- STATUS.md is ~167 lines after the restore, above the global 150-line guideline; restored on explicit user request.
