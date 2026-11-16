# Burner modules (`src/burner/`)

Disc images, drive access and the Burn Studio operations. Nothing in here knows about windows or
terminals: GUI (`src-tauri/src/burn_studio.rs`, `burn.rs`) and CLI (`src/studio_cli.rs`,
`src/main.rs`) both call `studio.rs` / `mod.rs`. Windows is the primary target; `unix.rs` is
compile-checked only (`cargo check --lib --tests --target x86_64-unknown-linux-gnu`).

## Image writers and readers

| Module | Purpose |
|---|---|
| `udf.rs` | UDF 1.02 + ISO 9660 bridge writer for DVD-Video (`plan_image`) and file trees (`plan_tree`, optional Joliet). The image is **planned first** (every file gets its sector), then streamed (`write_image`). 2048-byte sectors. |
| `udf250.rs` | Blu-ray image writer: UDF 2.50 with metadata partition, read-only, single session, no ISO bridge (`plan_bd_image`, `write_bd_image`). |
| `isoread.rs` | Read-only ISO 9660 access to an image: listing, lookup, file contents, sector-to-path map. UDF-only images (Blu-ray) are rejected. |
| `layerbreak.rs` | Layer break for DVD-9: L0 at least L1, multiple of 16 sectors, break on a title-cell start; otherwise zero padding in front of a titleset and a patched `TT_SRPT`. |
| `label.rs` | `sanitize_volume_label` (ISO d-characters, max 32); the frontend mirrors it in `frontend/src/label.ts`. |
| `sidecar.rs` | Layout files next to images: `.dvd` (layer break), `.cue`, `.mds` (Media Descriptor; round-trip tested only, not checked against Alcohol/Daemon Tools). |
| `tools.rs` | Offline image tools: read/patch the volume label in place (fixes descriptor CRC and tag checksum), layer break inspector. Not the process helper `crate::tools`. |
| `css.rs` | CSS detection on DVD-Video sectors and images, to warn before burning raw encrypted copies. Detection only; no decryption. |

## Sources, reading and verification

| Module | Purpose |
|---|---|
| `source.rs` | `SectorSource`: image file, device, zeros (Discovery test burn), 2352-byte `.bin`, or an image built on the fly. |
| `builder.rs` | "Build image" mode: detects content (`VIDEO_TS` = DVD-Video, `BDMV` = Blu-ray, else data tree) and writes or streams the image. |
| `reader.rs` | Disc to `.iso`: retries, zero-fill for unreadable sectors, optional split, MD5 of what was written. |
| `verifier.rs` | Disc vs image comparison and readability check; miscompares mapped back to files. |
| `ringbuf.rs` | Bounded ring between producer (image builder) and burner with buffer recovery (low/high-water marks); its fill level is the Studio's main buffer. |
| `graph.rs` | `Sampler` (speed, buffers, CPU), graph file name template, `.ibg` writer and tolerant parser. |

## Drive access

| Module | Purpose |
|---|---|
| `spti.rs` | MMC command layer: CDB builders and response parsers (byte-level unit tests), `ScsiTransport` with two Windows implementations (IMAPI `IDiscRecorder2Ex`, default and admin-free; SPTI pass-through), read-only `drive_info`. Writes nothing to a disc. |
| `mmcwrite.rs` | Direct write engine (`io.backend = direct`): lock tray, profile, capacity, optional format, mode page 05, set streaming, layer break, WRITE(10) loop from the ring buffer, sync cache, close, optional verify. Generic over the transport and tested with a scripted fake drive; not yet exercised on real hardware. |
| `windows.rs` | IMAPI2 burner (`IDiscMaster2`, `IDiscRecorder2`, `IDiscFormat2Data`). |
| `unix.rs` | growisofs / xorriso burner. |
| `power.rs` | Keep-awake while writing; tray and medium-removal control through storage IOCTLs. Windows only. |

## Orchestration

| Module | Purpose |
|---|---|
| `mod.rs` | Public entry points: `create_disc_image`, `burn_video_ts` (the single burn path of GUI and CLI), `OpticalBurner` trait (fake in `tests/burn_flow.rs`), `DriveInfo`, `Capacities`, `Overburn`, error types. |
| `studio.rs` | Burn Studio operations (write, read, verify, build, Discovery). Reports through `Event`s, ends in an `Outcome`. |
| `testutil.rs` | Synthetic disc content for tests. |

## Rules

- Never run a real burn or write to a drive without the user's explicit go.
- Integration tests use the fake `OpticalBurner` and the scripted fake transport; nothing in the
  automated suite touches a real drive.
