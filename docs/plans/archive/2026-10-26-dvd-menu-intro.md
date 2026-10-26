# Plan: template menu intro animations on DVD

Date: 2026-10-26. Status: implemented. Deviation: dvdauthor writes the HLI into every NAV pack regardless of the SPU start, so `author::strip_intro_buttons` clears cell 1 after authoring; `-sc_threshold 1000000000` is required with `+cgop`.

## Goal

ConvertX template menu pages with `[MNUP_INTRO]` keys play their intro on DVD too (today Blu-ray/AVCHD
only). Remove "intro on DVD" from STATUS.md "Out of scope". Custom layouts (menu editor) stay unchanged.

## Facts found during analysis

- `[MNUP_INTRO]` parsing already exists: `template.rs::parse_anim` → `Graphic.intro` (`AnimKey`), plus
  `Page.intro_duration`. Nothing new to parse.
- DVD template pages are NOT converted to `CustomLayout`; they are rendered directly by
  `menu.rs::render_page` (bg / fg layers, `VideoSlot`s, highlight/select masks). Intro data on DVD
  therefore lives in `menu_anim::Sprite`s, not in `layout::Element.intro`.
- Template pages are already in TV storage pixels (720×576 page); `render_page` scales page → output
  frame with `sx = out_w/pw`, `sy = out_h/ph` (NTSC: 576 → 480). Intro keys use the same linear scale –
  no pillarbox/aspect correction as on BD (`TemplateFrame::rect`).
- DVD menu encoding: `dvd.rs` step 5 → `encoder::motion_menu_args` / `menu_still_args` → spumux
  (`author::spumux_xml`, SPU at `00:00:00.00`) → `author::write_menus` (`pause="inf"` still, or
  `<post>jump cell 1;</post>` motion loop).
- BD intro machinery to reuse: `menu_anim::{Sprite, Pixels, Intro, composite_frame, frame_time}`,
  `Toolchain::ffmpeg_piped` (stream 0 = background, then slot streams; compose closure), audio
  crossfade at the loop mark in `encoder::custom_menu_graph`.

## Decision: one VOB, two cells (not two VOBs)

One ffmpeg encode `[intro 0..T | loop T..T+L]` (intro frames piped from Rust, never on disk), one
spumux run with `<spu start="T">`, then

```xml
<vob file="m.mpg">
  <cell start="0" end="T" program="1"/>
  <cell start="T"/>            <!-- still page: pause="inf" -->
</vob>
<post>jump cell 2;</post>      <!-- omitted for still pages (cell 2 pauses forever) -->
```

- dvdauthor fills the NAV-pack HLI only from the SPU's start → cell 1 has no active buttons.
- `jump cell 2` from `post` does not re-run the PGC's `pre` (same as today's `jump cell 1`).
- Two VOBs would need two encodes, two spumux runs, an audio seam at the VOB boundary – no gain.
- Cell boundary must be a VOBU start: T is rounded UP to whole GOPs (PAL 15, NTSC 18 frames; 3 s fits
  both exactly), plus `-force_key_frames T` and closed GOPs (`-flags +cgop`) – only on intro pages.
- Still template pages with an intro become a short video: T + 1 s still, cell 2 `pause="inf"`.
- Intro plays on every entry of the page (also when paging); skipping it on paging (BD does via
  GPR3) is out of scope for this plan.

## Implementation steps (in order)

1. **`src/menu_anim.rs`**
   - Move `bd_template::intro_keys` here as `pub fn graphic_keys(g: &Graphic, rect, map: impl Fn(f64,f64,f64,f64)->(f64,f64,f64,f64)) -> Vec<ElementKey>`
     (block → graphic mapping in page coordinates, `map` converts to frame coordinates). `bd_template`
     calls it with `|x,y,w,h| frame.rect(x,y,w,h)` – behaviour unchanged (its tests must stay green).
   - Move `MAX_INTRO_SECS` + the length rule (`max(last key, page.intro_duration).clamp(0.5, 10)`) into
     `pub fn intro_length(last_key: f64, page_secs: f64) -> f64`; use it in `bd_template.rs:527`.
   - `pub fn dvd_intro_frames(secs: f64, tv: TvStandard) -> usize`: frames rounded up to whole GOPs
     (15 PAL / 18 NTSC). Unit test both standards.
   - A `BdRate` for the DVD rate (25/1, 30000/1001, no fake interlace) for `frame_time`/`Intro` – check
     whether a helper exists first; else construct inline.
2. **`src/menu.rs`**
   - `RenderOptions.intro: bool` (default false in `still()`).
   - In `render_page`, when `opts.intro`: every drawn graphic is ALSO drawn on its own transparent
     page-size scratch layer (keep the normal bg/fg drawing untouched → zero regression), cropped to its
     non-transparent bbox, scaled to the output frame → `Sprite { pixels: Image, rect, opacity, keys }`
     with keys from `menu_anim::graphic_keys` using `sx/sy`. Motion `GT_VIDEO` slots →
     `Pixels::Slot { index, width, height }` (index into `video_slots`). Keep z order.
   - `RenderedPage.intro: Option<Intro>` = sprites present AND some sprite has keys; `secs` =
     `dvd_intro_frames(intro_length(..)) / fps`, `end` = last key time.
   - `MenuPageFiles.intro: Option<Intro>`; motion length for intro pages = T + L (still: T + 1.0).
     `MenuSpec.intro_secs = Some(T)`.
3. **`src/encoder.rs`**
   - Extract the intro audio crossfade from `custom_menu_graph` into a shared helper; use it there and in
     `motion_menu_args`.
   - `motion_menu_args(…, intro_secs: Option<f64>)`: last input `-f rawvideo -pix_fmt rgba -s WxH -r <rate> -i pipe:0`,
     overlaid with `eof_action=pass` on top of everything; `-force_key_frames T` (floored to µs like
     `bd_custom_menu_args`) and `-flags +cgop`. Without intro the args are byte-identical to today.
   - `pub fn dvd_slot_stream(tv, slot: &VideoSlot, frames) -> RawStream` (same clip chain as
     `motion_menu_args`, extracted into one shared filter string fn) and
     `pub fn dvd_background_stream(tv, rgb, frames) -> RawStream` (lavfi `color`, the page fill colour).
4. **`src/author.rs`**
   - `MenuSpec.intro_secs: Option<f64>` (update all constructors / test literals).
   - `write_menus`: intro → two `<cell>`s as above + `jump cell 2` (motion) or `pause="inf"` on cell 2
     (still). Times written with 3 decimals.
   - `spumux_xml(tv, hl, sel, buttons, start_secs: f64)` – start rounded UP to the millisecond so the SPU
     never lands in cell 1. Verify spumux accepts the format (NAV test below catches it).
5. **`src/dvd.rs`**
   - Template `RenderOptions { intro: opts.menu_animations, .. }` (all four call sites ~1336–1445).
   - Menu job: `files.intro` set → `tools.ffmpeg_piped(args, [background, slots…], frames, secs, compose)`
     with `composite_frame` (as in `bd.rs:410`); else the existing path.
6. **Texts / docs**: drop "Blu-ray/AVCHD only" wording in `config.rs:76`, `dvd.rs:326`, `main.rs:148`,
   `layout.rs:71`, GUI help `frontend/src/settings.ts:265` (German; mention intro replays when paging on DVD).
   STATUS.md: remove "intro on DVD" from Out-of-scope (done by `/status` at the end).
7. **Custom layouts**: no code change; `intro_secs: None` → identical XML. Covered by existing tests.

## Verification (mandatory)

- Unit: `author.rs` – intro menu yields exactly two `<cell>` + `jump cell 2`; still-intro yields
  `pause="inf"` on cell 2 and no `post`; non-intro XML unchanged (`dvdauthor_xml_structure` green).
  `spumux_xml` start time. `dvd_intro_frames` PAL/NTSC. `motion_menu_args` with intro contains
  `pipe:0`, `force_key_frames`, `acrossfade`, `+cgop`; without intro unchanged.
  `bd_template` tests unchanged after the move.
- New in `tests/menu_intro.rs` (skip gracefully without tools/templates): build a small DVD menu with
  an intro (PAL and NTSC) through encode → spumux → dvdauthor, then scan the menu VOB's NAV packs
  (sector bytes `0x26..0x2A` = `00 00 01 BF`, substream `0x2C == 0x00`): `vobu_s_ptm` at `0x39` (u32 BE),
  `hli_ss` at `0x8D` (u16 BE, low 2 bits), DSI `vobu_c_idn` at `0x422`. First confirm these offsets on a
  known still menu (hli_ss != 0). Assert: cell 2 starts at T; every cell-1 VOBU has `hli_ss == 0`;
  every cell-2 VOBU has `hli_ss != 0`.
- Real template smoke test: one real template with intro keys builds as DVD (skip if templates missing).
- Regression: `cargo test --workspace` fully green (esp. `custom_menu`, `motion_menu`, `build_dvd`,
  `ntsc`, calculator/bitrate, BD part of `menu_intro`), `npm run build`, Unix compile check.

## Out of scope / follow-ups

- Skipping the intro on paging / re-entry (register + dispatcher).
- `estimate_menu_mb` ignores intro length (real bitrate uses measured menu size).
- Standalone-player check of the cell jump (needs hardware).
