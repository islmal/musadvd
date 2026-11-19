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

## Blu-ray hardware test (PS4)

VLC/libbluray accepts HDMV and playlist errors that real players reject, so Blu-ray navigation
changes need a standalone player. The reference player is a **PS4 on firmware 10.50**.

1. `scripts/make_bd_stress_test.ps1` writes `bd_stress_test/` (git-ignored): three clips
   (re-encode + PGS + chapters, copy + timing repair, 720p59.94) and a 1920x1080
   `stress_layout.json` (motion main menu with intro, still chapter and audio/subtitle menus).
2. Build from the repository root:

   ```powershell
   .\target\release\musadvd.exe build-dvd --disc bd25 --tv ntsc --bd-video auto --no-hw-encode --project-name BD_STRESS --layout ".\bd_stress_test\stress_layout.json" --output ".\bd_stress_test\out" --input ".\bd_stress_test\01_reencode_pgs_chapters.mkv" ".\bd_stress_test\02_copy_timing_repair.mkv" ".\bd_stress_test\03_secondary_720p5994.mkv"
   .\target\release\musadvd.exe build-iso ".\bd_stress_test\out\BD_STRESS" --as-disc --label BD_STRESS --output ".\bd_stress_test\BD_STRESS.iso"
   ```

   `build-iso` fails with "access denied" while the old ISO is mounted; eject it first.
3. Capture the PS4 kernel log during the test (klog server on the console, port 3232). Use an
   absolute path: .NET resolves relative paths against the process directory (`system32` in an
   admin shell), and never append to a file Windows PowerShell wrote (UTF-16) with UTF-8:

   ```powershell
   $f = "C:\convertx-rust\bd_stress_test\ps4_klog_$(Get-Date -Format HHmmss).txt"; $c = [Net.Sockets.TcpClient]::new('<ps4-ip>', 3232); $r = [IO.StreamReader]::new($c.GetStream()); $w = [IO.StreamWriter]::new($f, $false, [Text.UTF8Encoding]::new($false)); $w.AutoFlush = $true; while ($null -ne ($l = $r.ReadLine())) { $w.WriteLine("$(Get-Date -Format HH:mm:ss.fff) $l") }
   ```

   The log shows the player app (`BdmvPlayerCore.elf`, `NPXS20113`) starting and being killed,
   not HDMV commands.
4. Checklist: still menus stay up (> 30 s, focus kept), every title end returns to the right
   menu (also with the Options overlay open), Play All, the Top Menu key mid-title, settings-menu
   audio/subtitle choices take effect when the title starts.
5. VLC comparison without burning: `vlc -vvv --file-logging --logfile <log> "bluray:///C:/convertx-rust/bd_stress_test/out/BD_STRESS"`
   (libbluray's own debug output does not reach VLC's log).

## Manual checks that automation cannot cover

Playback on a standalone player or VLC (menus, chapters, intros, subtitles), Burn Studio on real
drives and media, QSV/AMF encoders on Intel/AMD hardware. The outstanding list is in
[`../STATUS.md`](../STATUS.md) (Roadmap).
