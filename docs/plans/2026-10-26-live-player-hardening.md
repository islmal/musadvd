# Live player hardening (8-bit proxy, HDR tonemapping, shared filter chain)

Date: 2026-10-26 · Status: planned

## Goal

The live player ("Schnitt & Bild") shows the same picture as the encode (deinterlaced, cropped,
tonemapped) and always delivers 8-bit H.264 that WebView2 can decode. HDR sources are also
tonemapped to SDR in the final DVD/BD encodes (today `fit_chain` only converts the matrix, so PQ/HLG
sources come out washed out on disc).

## Architecture

- `fit_chain` is target-frame specific (720×576 / 1920×1080 with bars), so only its head is shared:
  `source_chain(v, vp)` = deinterlacer → `tonemap_filter(v)` (HDR only) → user crop.
- Encode: `source_chain` → `fps` → scale/pad (unchanged otherwise).
- Proxy: `preview_filter(v, vp, 480)` = deinterlacer + crop → `scale=<display_w>:480:flags` →
  tonemap → `setsar=1` → `format=yuv420p`. Tonemap after the downscale keeps 4K float tonemapping
  real-time; the small difference to the encode order is acceptable for a preview (comment it).
- Tonemap chain: `zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv`
  (bin/ffmpeg 9.0.1 has `zscale`, `tonemap`, `bwdif`, `libx265` – verified 2026-10-26).
- The preview server probes once per token (`preview_stream(path, crop)`), applies `apply_crop` and
  stores `(VideoStream, VideoProcessing)` with the token; seeks reuse it. Crop change in the
  frontend → new token.
- ffmpeg stays the only black box; all processes via `blocking_tool_command`. No new dependency.

## Files

| File | Change |
|---|---|
| `src/probe.rs` | `RawStream.color_transfer`; `VideoStream.hdr: bool` (`smpte2084` / `arib-std-b67`) |
| `src/encoder.rs` | `source_chain`, `tonemap_filter`, `pub fn preview_filter`; `fit_chain` uses `source_chain`; test helper `video()` gets `hdr: false` |
| `src/config.rs` | extract the `VideoProcessing` mapping (~line 761) into `AppSettings::video_processing()`; `build_options` uses it |
| `src-tauri/src/preview.rs` | `preview_stream(path, crop: Option<Crop>)`, per-token `(VideoStream, VideoProcessing)`, `preview_filter` instead of `scale=-2:480`, add `-pix_fmt yuv420p -profile:v high`; stream integration test |
| `frontend/src/api.ts` | `previewStream(path, crop)` |
| `frontend/src/edit.ts` | pass `t.crop`; drop the cached `streamUrl` when `JSON.stringify(t.crop)` changed |
| `tests/two_pass.rs` | `hdr: false` in the `VideoStream` literal |

## Steps

1. probe.rs: HDR detection + unit test (ffprobe JSON with `smpte2084`, `arib-std-b67`, `bt709` → true/true/false).
2. encoder.rs: extract `source_chain` + `tonemap_filter`; tonemap before `fps`/`scale` in the encode.
3. encoder.rs: `preview_filter` (display width from `v.display_aspect`, which already includes the crop, rounded to even).
4. config.rs: `video_processing()`.
5. preview.rs + frontend: pass crop, store per token, apply filter and force 8-bit.
6. Only if acceptance shows 4K HEVC stutter: with `hw_decode` call `Toolchain::probe_hwaccel` once per token and add `-hwaccel`. Otherwise skip.
7. Acceptance in the release window (below).

## Verification (mandatory)

- **encoder.rs unit tests**
  - Existing SDR filter asserts stay byte-identical (regression guard).
  - HDR `VideoStream` → tonemap chain present before `scale` in `video_filter`, `bd_video_filter` and `preview_filter`.
  - `preview_filter` with crop + interlaced source = `bwdif…,crop=…,scale=W:480…,…,setsar=1,format=yuv420p`.
- **Stream integration test in `src-tauri/src/preview.rs`** (skips when `bin/ffmpeg.exe` is missing)
  - Generate 2 s `testsrc2` as `libx265`, `yuv420p10le`, tagged PQ/BT.2020, into a temp dir.
  - `PreviewServer::start`, register token, `GET /s/<token>?t=0.5` over `TcpStream`.
  - Expect `200`; body contains `ftyp` and `moof`; ffprobe of the body: `h264`, profile `High` (not `High 10`), `yuv420p`, height 480.
  - Unknown token → `404`.
  - Spawn only via `blocking_tool_command` (keeps `tests/process_hygiene.rs` green).
- **Full run:** `cargo test --workspace`, `npm run build`, `cargo check --lib --tests --target x86_64-unknown-linux-gnu`.
- **Acceptance in the release window** (manual or computer use, real window, not CDP)
  - Build `cargo build --release -p convertx-gui --features tauri/custom-protocol`.
  - 4K HEVC HDR file plays with normal colours and smoothly; crop change visible in the player;
    interlaced DVD source without combing; audio track switch; set chapter, split, undo.
  - CSP `media-src http://127.0.0.1:*` must work under the release origin `http://tauri.localhost`;
    adjust `tauri.conf.json` if not.

## Out of scope

Custom URI scheme instead of the loopback port, frame stepping, faster seeking.

## Risks

- Tonemapping in `fit_chain` changes final encodes of all HDR sources (intended).
- Real-time `tonemap` at 480p and WebView2 loading the loopback stream under the release origin are
  unverified until acceptance.
