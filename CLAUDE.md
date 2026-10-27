# convertx-rust – working rules

Answer the user in German; code, comments, identifiers and STATUS.md are English. UI text and
user-facing errors are German. Architecture and open issues: [STATUS.md](STATUS.md); file tree,
dependencies and git state: `SYSTEM_STATE.txt` (generated).

## Shell setup (every session)

The user's PATH contains `C:\Program Files\dotnet"` (stray quote), which breaks cmd/npm. Prefix
PowerShell sessions with this (never change the persistent PATH):

```powershell
$clean = ($env:Path -split ';' | % { $_.Trim().Trim('"') } | ? { $_ }) -join ';'; $env:Path = "$env:USERPROFILE\.cargo\bin;$env:USERPROFILE\scoop\apps\mingw\current\bin;$clean"
```

Toolchain: rustup host `x86_64-pc-windows-gnu` (MinGW from scoop needed for `dlltool`), target
`x86_64-unknown-linux-gnu` installed only to compile-check `burner/unix.rs`; Node 24 / npm 11.

## Build & test

```powershell
cargo check --workspace
cargo test --workspace                       # ~3 min, needs bin/ tools + ConvertX templates
npm run build                                # frontend → dist/ (tsc + vite)
cargo build --release -p convertx-gui --features tauri/custom-protocol   # GUI exe with embedded frontend
cargo build --release                        # CLI
cargo check --lib --tests --target x86_64-unknown-linux-gnu             # Unix burner compile check
```

- Without `tauri/custom-protocol` the GUI exe loads `http://localhost:5173` ("page not reachable").
- `bin/ffmpeg.exe` / `ffprobe.exe` (9.0.1 gyan.dev full, ~220 MB each) are git-ignored and must be
  copied in after cloning – they are the only external tools. `dvdauthor`/`spumux` (DVD authoring,
  subpicture muxing) and `tsMuxeR` (BDAV muxing, `bd_mux`) are gone; `tools/oracle/` rebuilds
  the first two, and tsMuxeR 2.7.0 comes from its GitHub releases, only to re-record the test
  fixtures (`TSMUXER_RECORD=<exe> cargo test --test tsmuxer_native`). NVENC needs NVIDIA driver ≥ 610.
- ConvertX templates are read from `C:\ProgramData\VSO\ConvertXToDVD\7\Templates` (read-only).

## Conventions

- Comments explain *why*; match the surrounding density.
- Every external process goes through `tools::tool_command` / `Toolchain::command`
  (`tools::blocking_tool_command` in sync code); `tests/process_hygiene.rs` fails on any other
  `Command::new` in `src/` / `src-tauri/src/`. Integration tests may spawn tools directly.
- Burning goes through `burner::burn_video_ts` (GUI and CLI); test flow logic with a fake
  `OpticalBurner` like `tests/burn_flow.rs`.
- Integration tests must skip gracefully when tools/templates are missing.
- Patch scripts through Bash heredocs lose backslashes (regexes, `\n`, Windows paths) – write
  such scripts to a file first.
- Source files are LF (`core.autocrlf=true`); an editor writing CRLF is harmless.
- Never run a real burn / write to a drive without the user's explicit go (hardware: Pioneer
  BDR-209M on `F:`).
