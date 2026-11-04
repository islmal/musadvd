# Burn Studio: safe Auto speed, fixed 4x/8x/12x choices, CSS warning

## Background

A raw ISO of a CSS-protected DVD was burnt and did not play. Evidence: graph
`PIONEER_BD-RW_BDR-209M_1.33_MITTWOCH-04-NOVEMBER-2026_08-37_CMC_MAG-M01-00_max.ibg` (MAX, up to
12.2x, main buffer 100 %), `write.verify` was off. The user confirmed the disc was CSS protected, so
speed was NOT the cause. Decrypting CSS is explicitly out of scope (no libdvdcss, no rewrite of it);
this plan only detects and warns. STATUS.md already lists CSS as out of scope.

## Goals

1. Studio `Auto` = at most 4x on every medium (was: DVD-5/BD maximum, DVD-9 about 4x).
2. The Studio speed dropdown always offers 4x, 8x, 12x, plus any other speeds the drive reports.
3. Warn when a disc being read is CSS protected, and ask before burning an image with scrambled
   VOB sectors. The warning never blocks: the user can continue.

The main burn dialog and the `burn` CLI keep their `auto` behaviour.

## Changes

### Speed: `src/burner/mod.rs`
- `preferred_speed`, numeric wish with an empty list: return
  `Specific((x * media.sectors_per_sec_1x()).round() as u32)` instead of `Max`. IMAPI and SET STREAMING
  round to a supported value; otherwise Auto would become Max exactly when IMAPI gives no list.
- `preferred_speed` tolerance `1.02` → `1.03`. The existing comment says 8x may be reported as 5540
  sectors/s, but that is 8.19x and fails 1.02, so a fixed `8` would silently fall to 6x and `4`
  to 2.4x. The dropdown used to hide this by sending exact values.

### Speed: `src/burner/studio.rs`
- New `const STUDIO_AUTO_SPEED: &str = "4";` with a one-line why. `resolve_speed` falls back to it
  instead of `"auto"`. Update the `WriteJob.speed` comment and the assertions in
  `auto_speed_rules_match_media_then_drive_first_wins` that expect `"auto"`.

### Speed: frontend
- New pure `frontend/src/studio/speeds.ts`: `speedChoices(reportedX: number[]): [value, label][]`.
  Fixed 12, 8, 4 merged with reported speeds, fastest first; a reported speed within 0.3x of a fixed
  one is dropped so 8.2x does not appear next to 8x. Keeps the existing value/label formatting.
- `speedOptions` in `frontend/src/studio/views.ts` uses it. With no media: auto, max, 12x, 8x, 4x.
- `studio.views.auto` in `frontend/src/locales/{de,en,ru}.json`: `Auto (max. 4x)`, `Auto (up to 4x)`,
  `Авто (до 4x)`.

### CSS: new `src/burner/css.rs` (register in `src/burner/mod.rs`)
- `pes_scrambled(sector: &[u8]) -> bool`: needs pack start `00 00 01 BA`; PES header at
  `14 + (sector[13] & 7)`; stream ids `E0..=EF`, `C0..=DF`, `BD` only (NAV/system headers never count);
  true when `(flags_byte_at_pes+6 & 0x30) != 0`. Bounds-checked, never panics on short input.
- `image_scrambled(path: &Path) -> Option<bool>`: `IsoImage::open` + `extent_map`; title VOBs only
  (`VIDEO_TS/VTS_nn_m.VOB`, m ≥ 1, case-insensitive); up to 1024 evenly spaced sectors per VOB.
  `None` = cannot tell (not ISO 9660, no title VOB, I/O error). Any scrambled sector = `Some(true)`.
  `shortcut: only plain ISO files with an ISO 9660 tree, .cue/.dvd/.mds images give None; follow the
  layout file if those matter.`

### CSS: `src/burner/spti.rs`
- `parse_copyright_info(b: &[u8]) -> Option<bool>`: READ DISC STRUCTURE format 0x01, byte 4 (protection
  system type) != 0. Needs at least 5 bytes.

### CSS: `src/burner/studio.rs`
- `disc_css(drive, io) -> Option<bool>`: `transport` with `backend: "auto"` (same as `open_disc`, so
  `imapi` does not disable it), `query` with `read_disc_structure(false, 0, 0, 0x01, 8)`. Any error
  (BD, drive refuses) = `None`.
- `read()`: on `Some(true)` log before reading: `Diese Disc ist CSS-geschützt. Eine Kopie als Image ist
  auf einem Rohling nicht abspielbar.`
- `write_image()`: on `image_scrambled == Some(true)` log the matching warning (also covers the CLI).

### CSS: GUI
- `src-tauri/src/burn_studio.rs`: async command `studio_image_scrambled(image) -> Option<bool>`
  (blocking work in `spawn_blocking`), register in `src-tauri/src/main.rs` beside `studio_write_image`.
- `frontend/src/studio/api.ts`: `imageScrambled(image)`.
- `viewWriteImage` in `frontend/src/studio/views.ts`: before `host.run`, await the check. On `true`
  show `modal` with Abbrechen / Trotzdem brennen. A failed check or `null` burns without a prompt.
- New locale keys in de, en, ru for the modal title, text and continue button (English is the master).

## Order (one micro-commit per green step)

1. `preferred_speed` fixes + tests.
2. `resolve_speed` + tests.
3. `speeds.ts`, `speedOptions`, locale label, `check-order.mjs` assertions.
4. `css.rs` + `parse_copyright_info` + tests.
5. `disc_css`, log warnings in `read` / `write_image`.
6. Tauri command, API, modal, locale keys.
7. Full verification, then `/status`.

## Verification

Automated, new:
- `studio_auto_is_capped_at_4x` in `studio.rs`, chaining `resolve_speed` and `preferred_speed`:
  DVD `[16,8,6,4,2.4]` → 4x; `[16,12,8]` → 8x; empty → `Specific(4x)`; BD `[12,6,4]` → 4x; wish `max` →
  `Max`; a matching rule wins.
- `speed_preferences` additions: `"8"` with `[5540]` → 5540; `"4"` with `[2770]` → 2770.
- `scripts/check-order.mjs`: `speedChoices([])` = 12,8,4; `[16, 8.2, 2.4]` = 16,12,8,4,2.4 (no 8.2);
  `[4.1]` adds nothing.
- `css.rs`: scrambled video, audio (`C0`) and private (`BD`) sectors are true; flags 0 is false; a NAV
  pack with the bits set at that offset is false; pack stuffing is honoured; short and all-zero
  sectors are false and do not panic. `image_scrambled` on an ISO built with the crate's own writer:
  clean VOB → `Some(false)`, one patched sector → `Some(true)`, no VOB → `None`.
- `parse_copyright_info`: type 0 → false, 1 → true, short buffer → `None`.

Commands (all must pass; check logs for errors):
- `cargo test --workspace`
- `cargo check -p convertx-gui`
- `cargo check --lib --tests --target x86_64-unknown-linux-gnu`
- `npm run build`

Manual, needs the user's explicit go and does not write a disc until then:
- No burn needed: choose `CHUGGINGTON_15.iso` in "Image schreiben", the modal must appear; cancel.
  A decrypted or own-authored ISO must show no modal.
- Read the protected disc: the warning appears in the log.
- With a blank disc and "Nach dem Schreiben überprüfen" on: burn on Auto; log says `bei 4x`; the `.ibg`
  shows `WRITE_SPEED_MAX` around 4. The drive may ignore the wish, only the graph proves it.

## Out of scope

CSS decryption or key handling of any kind, blocking protected burns, changing the main burn dialog or
`burn` CLI defaults, changing the user's stored `write.verify`.
