# Plan: GPU vendor selection (NVIDIA / Intel QSV / AMD AMF) for BD encode + decode

Date: 2026-10-28 · Status: approved

## Goal & approach

One setting `HwAccelMode` picks the GPU vendor for Blu-ray/AVCHD H.264 encoding and GPU decoding.
The existing "probe, else software" principle stays: every hardware encoder/decoder is tested first,
failure → x264 / CPU with a log warning. `fake_interlaced` titles (1080i) always use libx264.
DVD MPEG-2 stays software. Live player (`preview.rs`) is out of scope.

## Decisions (deviations from the original request)

1. Struct is `AppSettings` (not `Settings`); the frontend field is `hw_accel_mode` (snake_case like
   every other setting), not `hwAccelMode`.
2. `bd_hw_encode: bool` is removed and replaced by `hw_accel_mode`. An old `bd_hw_encode:false` is
   NOT migrated → add `// shortcut: old bd_hw_encode=false is not migrated (NVENC back on), add a
   migration if users complain`.
3. `hw_decode` stays the on/off switch. The mode only chooses which decoders are probed, in order:
   - `Nvidia` → `["d3d11va", "cuda"]`
   - `Amd` → `["d3d11va"]`
   - `Intel` → `["qsv", "d3d11va"]`
   - `None` → current list (`cuda`, `d3d11va`, `qsv` on Windows; `cuda`, `vaapi`, `qsv` elsewhere)
   Non-Windows: replace `d3d11va` with `vaapi` (keep the `cfg!(windows)` split).
4. Default = `Nvidia` (keeps today's "NVENC on by default"; probe fails on non-NVIDIA → x264 + warning).
5. Lenient deserialization: `load_or_default` resets ALL settings on a parse error, so an unknown value
   (`"hw_accel_mode":"foo"`) must become the default without failing. Use `#[serde(other)]` on the
   default variant if serde accepts it for this enum; otherwise a small `deserialize_with` helper that
   reads a string and falls back to `HwAccelMode::default()`.

## Files

- `src/config.rs`
  - `pub enum HwAccelMode { None, #[default] Nvidia, Intel, Amd }`, derives
    `Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize`, `#[serde(rename_all = "lowercase")]`.
  - Method `bd_encoder(self) -> Option<BdEncoder>` (None → None).
  - `AppSettings.hw_accel_mode` replaces `bd_hw_encode`; default in `impl Default`; `apply_to` sets
    `opts.hw_accel = self.hw_accel_mode`.
  - Update `burn_settings_are_sanitised_and_applied` (asserts on `bd_hw_encode`).
- `src/encoder.rs`
  - `BdEncoder` gets `Qsv`, `Amf`; `fn label(self) -> &'static str` ("NVENC (h264_nvenc)",
    "QSV (h264_qsv)", "AMF (h264_amf)", "x264") and `fn is_hw(self)`.
  - `qsv_bd_options()`, `amf_bd_options()` next to `nvenc_bd_options()` (shared by encode + probe).
  - `qsv_args()` / `amf_args()` next to `nvenc_args()`, dispatched in `video_codec_args`.
    Content per encoder: High@4.1, AUD, 4 slices, closed GOP / forced IDR, VBR with
    `-b:v/-maxrate/-bufsize`, `-g keyint`, `-bf 3`, BT.709 tags, fast = faster preset;
    `two_pass` → no-op for QSV/AMF unless a single-run equivalent exists (`x264_two_pass` already
    restricts the analysis run to x264).
    Starting points (VERIFY every private option name against `bin/ffmpeg.exe -h encoder=h264_qsv`
    / `-h encoder=h264_amf` before using it):
    - QSV: `-c:v h264_qsv -preset slow|veryfast -profile:v high -level 41 -aud 1 -forced_idr 1
      -pic_timing_sei 1 -flags +cgop -slices 4`
    - AMF: `-c:v h264_amf -usage transcoding -quality quality|speed -profile:v high -level 4.1
      -rc vbr_peak -enforce_hrd 1 -aud 1 -slices 4`
- `src/tools.rs`
  - `pub fn hwaccel_candidates(mode: HwAccelMode) -> &'static [&'static str]` (pure);
    `probe_hwaccel(&self, input, mode)` uses it.
  - `probe_nvenc` → `probe_hw_encoder(&self, enc: BdEncoder) -> Result<(), String>` (same 10-frame
    testsrc2 run, codec + `*_bd_options()` of `enc`).
  - Failure reason: `nvenc_failure_reason` for Nvenc (unchanged, keeps its test); others → last
    non-empty stderr line.
- `src/bd.rs`
  - Extract `fn title_encoders(hw: Option<BdEncoder>, rates: impl Iterator<Item = BdRate>) -> Vec<BdEncoder>`
    (or over `&[Title]`): `fake_interlaced` → X264, else `hw.unwrap_or(X264)`.
  - Probe `opts.hw_accel.bd_encoder()`; on `Err` warn `bd_hw_unavailable`; log `bd_encoder_hw`.
  - `NVENC_MAX_JOBS` → `HW_MAX_JOBS` (all hw encoders), condition `all(is_hw)`.
  - Pass `opts.hw_accel` into `probe_hwaccel` (encode path, line ~218).
- `src/dvd.rs`: `BuildOptions.bd_hw_encode` → `hw_accel: HwAccelMode` (docs updated); pass the mode
  through `encode_title` to `probe_hwaccel`.
- `src/main.rs`: `--hw-accel <none|nvidia|intel|amd>` (clap `ValueEnum` or value_parser, default
  nvidia); `--no-hw-encode` stays as alias → `none` (wins over `--hw-accel`).
- `frontend/src/api.ts`: `hw_accel_mode: "none" | "nvidia" | "intel" | "amd";` replaces `bd_hw_encode`.
- `frontend/src/settings.ts`: remove the `bd_hw_encode` check in the Blu-ray group; add
  `select("hw_accel_mode", t("settings.hw_accel_mode"), [["none", t("settings.hw_accel.none")], …],
  t("settings.hw_accel_mode_hint"))` in the "Leistung" group next to `hw_decode`
  (existing `select` helper; check `StrKey` accepts the literal union).
- `frontend/src/locales/{en,de,ru}.json`
  - Add: `settings.hw_accel_mode` ("Hardware acceleration"), `settings.hw_accel_mode_hint`
    (encodes Blu-ray/AVCHD on this GPU and picks the GPU decoder; falls back to x264/CPU; 1080i
    always x264), `settings.hw_accel.none` ("Off (CPU only)"), `.nvidia` ("NVIDIA (NVENC)"),
    `.intel` ("Intel (Quick Sync)"), `.amd` ("AMD (AMF)").
  - Remove: `settings.hardwareEncodingNvidiaNvenc`, `settings.kodiertBluRayVideos` (if unused elsewhere).
  - Rename: `engine.bd_nvenc_unavailable` → `engine.bd_hw_unavailable`
    ("Video encoder: x264 – {encoder} not available: {reason}"), `engine.bd_encoder_nvenc` →
    `engine.bd_encoder_hw` ("Video encoder: {encoder} for {nv} of {count} titles, otherwise x264
    (1080i frame rates)"). Same placeholders in de/ru.
- `tests/bluray.rs`, `tests/menu_intro.rs`: `bd_hw_encode: false` → `hw_accel: HwAccelMode::None`,
  `true` → `HwAccelMode::Nvidia`.

## Order

1. config (enum, field, serde tests) → 2. encoder (BdEncoder, args, options) → 3. tools (probes)
→ 4. bd + dvd (selection, decoder mode) → 5. CLI → 6. frontend (api, settings, locales) → 7. verification.

## Automated verification (mandatory)

New unit tests:
- `tools::hwaccel_candidates` for all 4 modes (Windows lists).
- `bd::title_encoders`: for every hw mode a `fake_interlaced` rate → X264, a normal rate → the mode's
  encoder; `hw = None` → only X264.
- `encoder::bd_qsv_args`, `bd_amf_args` (pattern of `bd_nvenc_args`: codec, maxrate, `-bf 3`,
  `-hwaccel` input placement).
- config: default is `Nvidia`; file without the field → default; `"foo"` → default and the other
  fields of the same file are kept; lowercase round trip (`"intel"` ↔ `Intel`); `apply_to` copies it.

New integration test (skips without `bin/ffmpeg.exe`) in `tests/bluray.rs`:
`hw_encoder_options_exist` – for h264_qsv and h264_amf every private option we pass appears in
`ffmpeg -h encoder=<name>` output. Only guard against option typos without Intel/AMD hardware.

Extend `hw_encode_uses_nvenc_or_falls_back` to run for Nvidia, Intel and Amd: expect the encoder's
label step, or x264 + the `bd_hw_unavailable` warning.

Then:
```
cargo test --workspace
cargo check -p convertx-gui
cargo check --lib --tests --target x86_64-unknown-linux-gnu
npm run build
node scripts/check-i18n.mjs
```
Manual (user): pick the dropdown in the GUI, save, reopen; one BD build with Nvidia.

## Not verifiable here / risks

- QSV and AMF on real hardware (only the fallback path runs on this NVIDIA machine).
- BD compliance (HRD/VBV, slices, GOP) of QSV/AMF output → add to STATUS known issues.
- `-hwaccel d3d11va` for NVIDIA instead of the previously first `cuda`: speed difference not measured.
