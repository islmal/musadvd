# Testing, scripts and CI

## Commands

```powershell
cargo check --workspace
cargo test --workspace                                      # about 3 min, needs bin/ tools and ConvertX templates
npm run build                                               # check-i18n, check-order, tsc, vite
cargo check --lib --tests --target x86_64-unknown-linux-gnu # Unix burner compile check
```

Integration tests **skip gracefully** when ffmpeg/ffprobe or the ConvertX templates
(`C:\ProgramData\VSO\ConvertXToDVD\7\Templates`, read-only) are missing. Shell setup for this
repository (PATH quirk, toolchain) is in [`../CLAUDE.md`](../CLAUDE.md).

## Test suites (`tests/`)

| Area | Files |
|---|---|
| DVD end to end | `build_dvd.rs`, `ntsc.rs`, `resolution.rs`, `two_pass.rs`, `cancel.rs`, `playback_options.rs`, `dvd_audio.rs` |
| Menus | `render_menu.rs`, `real_templates.rs`, `native_menu_templates.rs` (all installed templates avoid `Unsupported`), `custom_menu.rs`, `custom_menu_intro.rs`, `motion_menu.rs`, `menu_intro.rs`, `menu_transitions.rs`, `fastplay_vm.rs`, `dvdvm_sprm.rs` |
| Subtitles | `bitmap_subs.rs`, `textsub_reference.rs` (measured geometry with tolerances), `textsub_golden.rs` (pixel goldens, `UPDATE_GOLDEN=1`) |
| Blu-ray | `bluray.rs`, `tsmuxer_native.rs` |
| Native vs original tools | `dvdauthor_native.rs`, `spumux_native.rs`, `tsmuxer_native.rs` |
| Burning and images | `disc_image.rs`, `burn_flow.rs` (fake `OpticalBurner`), `studio_cli.rs` |
| Hygiene | `process_hygiene.rs`: fails on any `Command::new` in `src/` or `src-tauri/src/` outside `tools` |

Unit tests live next to the code (`#[cfg(test)]`); `burner/spti.rs` and `mmcwrite.rs` test CDBs and
the write sequence against byte-level expectations and a scripted fake drive.

## Oracles and fixtures

The native dvdauthor, spumux and BDAV muxers are verified against **recorded** output of the
original tools, stored in `tests/fixtures/{dvdauthor,spumux,tsmuxer,textsub}` (MD5s plus small
input streams). To re-record:

| Fixture | Command |
|---|---|
| spumux | `SPUMUX_RECORD=<spumux.exe> cargo test --test spumux_native` |
| dvdauthor | `DVDAUTHOR_RECORD=<dvdauthor.exe> cargo test --test dvdauthor_native` |
| tsMuxeR | `TSMUXER_RECORD=<tsMuxeR.exe> cargo test --test tsmuxer_native` (tsMuxeR 2.7.0 from its GitHub releases) |

`tools/oracle/` rebuilds `dvdauthor.exe` and `spumux.exe` (MSYS2 UCRT64, see its README). These
tools are not part of the product.

## Scripts (`scripts/`)

| Script | Purpose |
|---|---|
| `check-i18n.mjs` | Locale key/placeholder parity against `en.json`; no umlauts in `.ts`. Part of `npm run build`. |
| `check-order.mjs` | Runs `frontend/src/order.ts` under Node as a unit test. |
| `build_all.ps1` | Frontend, debug and release builds, then shortcut update. |
| `update_shortcuts.ps1` | Refreshes the local `MusaDVD CLI` / `MusaDVD GUI` shortcuts. |

## CI

`.github/workflows/release.yml` runs on `v*` tags (or manually): fetches the ffmpeg full build,
`npm ci`, and builds the signed NSIS installer with `tauri-action`, plus the portable ZIP. The tag
must match `version` in `src-tauri/tauri.conf.json` because the updater compares it.

## Manual checks that automation cannot cover

Playback on a standalone player or VLC (menus, chapters, intros, subtitles), Burn Studio on real
drives and media, QSV/AMF encoders on Intel/AMD hardware. The outstanding list is in
[`../STATUS.md`](../STATUS.md) (Roadmap).
