# Template menu intro animations on Blu-ray/AVCHD

Status: planned 2026-10-26. Scope: ConvertX template **intro** on BD/AVCHD via `CustomLayout`.
Not in scope: outro (only the dispatcher hook), DVD templates (own renderer in `menu.rs`),
skipping the intro by key press.

## NON-NEGOTIABLE CONSTRAINTS (do not change, do not "simplify" on errors)

These were decided by the user after review. If one of them causes a bug, fix the bug inside the
constraint; never replace the mechanism. Stop and ask the user instead of deviating.

1. **No intro frames on disk.** Intro frames go Rust → ffmpeg as `-f rawvideo -pix_fmt rgba
   -s WxH -r R -i pipe:0` over **stdin**; background/slot frames come ffmpeg → Rust as `rawvideo
   rgba` over **stdout**. Forbidden: PNG/BMP/any image sequence files, temp video files for the
   intro, `eval=frame`, `geq`, per-frame filter expressions. The static element layers that exist
   today (one PNG per layer, not per frame) stay as they are.
2. **Exact audio crossfade at the loop end** (pages with intro and audio), with `D = T + L`,
   `X = min(0.5, T)`:
   ```
   [a]asplit=2[m][s];
   [s]atrim=start=T-X:end=T,asetpts=N/SR/TB[tail];
   [m]atrim=duration=D,asetpts=N/SR/TB,afade=t=in:d=0.5[main];
   [main][tail]acrossfade=d=X:c1=tri:c2=tri[aout]
   ```
   Output length stays `D`; its last `X` s fade from the clip's own end into the audio of
   `[T−X, T]`, so the restart at mark `T` continues the signal. No `afade=t=out` on these pages,
   no hard cut. Pages without intro keep today's audio chain byte for byte.
3. **IGS composition PTS = exactly T**, IG display set ≤ `IG_BUDGET` (below).
4. **One continuous encode per page** (background, video slots, audio); the intro is only overlaid.

## Findings (template data)

Every block has `blocks\ItemN\Anim\ItemK` keyframes: `Origin=MNUP_INTRO|MNUP_OUTRO`,
`TimeStamp` (s within the phase), `alpha/left/top/width/height`; `2147483647` = "inherit the block
value". `IntroAnimData/OutroAnimData` only name the kind (`matFadeIn/Out`, `matZoomIn/Out`);
start/duration are redundant with the keys. Before the first key the value holds, between keys it is
linear. Zooms are about the block centre. All templates: `intro=3 main=25 outro=2` per page.
Today only the three durations are parsed and nothing uses them.

## Architecture

- **One clip per page:** `[intro 0..T | loop T..T+L]`, one playlist, mark 0 at 0, mark 1 at `T`.
  `T` = N whole frames (N = round(intro × fps)); keyframe forced at `T`. Loop restarts with
  `PlayPLatMark(pl, 1)`, so the intro never replays.
- **Seamless background:** background, video slots and audio come from ONE continuous encode; the
  intro is an opaque frame stream overlaid on top (`eof_action=pass`). The streams below keep running,
  so from frame N on they are visible without any cut or offset.
- **Rust pre-render, no PNG sequences on disk:** (1) helper ffmpeg pass writes the N background
  frames (same background chain: blur, shading) and the N frames of every animated video slot
  (fitted to its final box) as `rawvideo rgba` to **stdout**; (2) Rust composites each frame
  (background + all elements in z-order at the interpolated box/opacity; text/images from the same
  code as the static layers) with rayon, in ordered chunks; (3) the final encode reads them as
  `-f rawvideo -pix_fmt rgba -s WxH -r R -i pipe:0` from **stdin**. No `eval=frame`, no `geq`.
- **Buttons:** IGS composition PTS = **exactly T** (buttons ready on the first loop frame; no flicker
  at every restart). Only valid if Spike 1a proves tsMuxeR keeps the IG display set behind the EP
  entry of mark 1 (see Spike 1a result below).
- **Audio loop:** pages with intro: the last `X = 0.5 s` of the audio are an `acrossfade` from the
  clip's own tail into the audio at `[T−X, T]`, so the signal at the end continues as the audio at
  mark `T`. The fade-out at the end is dropped (fade-in at 0 stays). Pages without intro keep today's
  fades. The player's restart gap itself cannot be removed (PlayPL is never seamless); the crossfade
  removes the level jump / click.
- **Opt-out:** `CustomLayout.animations` (serde default `true`) AND setting `menu_animations`
  (GUI checkbox "Menü-Animationen (Intro)", CLI `--no-menu-animations`). Off → output identical to
  today.

## HDMV state machine

Registers (existing: GPR1 MARK, GPR2 FLAG, GPR6 TMP):

| GPR | name | values |
|---|---|---|
| 3 | `ENTRY` | 0 fresh entry (intro), 1 paged (skip intro) |
| 4 | `NEXT_KIND` | 0 none, 1 title, 2 menu, 3 menu paged |
| 5 | `NEXT_INDEX` | HDMV title number / movie object id |
| 7 | `PAGE` | current menu page (hook for the outro) |

Buttons that leave the menu set MARK/FLAG as today, then `GPR4=kind`, `GPR5=index`,
`JumpObject(DISPATCH)`. "Paged" = `Action::Menu` to a menu with the same `role` and `title`
(Next/Prev). SetAudio/SetSubtitle stay as they are.

DISPATCH (new last movie object, title object numbers unchanged):
```
0  mov GPR3, 0
1  cmp GPR4 == 1
2    goto 6
3  cmp GPR4 == 3
4    mov GPR3, 1
   ; OUTRO HOOK (later): cmp GPR3 == 0 → per page: cmp GPR7 == p ; PlayPL(outro_p)
5  JumpObject(GPR5)
6  JumpTitle(GPR5)
```
Menu object of a page with intro:
```
0  mov GPR7, i
1  [SetButtonPage(1)]
2  cmp GPR3 == 1
3    goto 6
4  PlayPL(pl)               ; intro + first loop
5  goto 7                   ; (still page: holds at the end, never reached)
6  mov GPR3, 0
7  PlayPLatMark(pl, 1)
8  goto 7
```
Invariant: GPR3 is 1 only right after a paging dispatch. Top Menu sets GPR3 = 0 before its jump;
titles get 0 through the dispatcher. Pages without intro keep their current commands.

## Files

- `src/template.rs`: `AnimKey { t, x, y, w, h, alpha: Option<u8> }`; `Graphic.block_rect`,
  `Graphic.intro/outro`; MAX → block value / `None`.
- `src/layout.rs`: `CustomLayout.animations`, `CustomMenu.intro_secs: Option<f64>`,
  `Element.intro: Vec<ElementKey { t, x, y, w, h, opacity }>` (canvas coords, serde-optional).
- `src/bd_template.rs`: affine mapping block key → element rect (through `TemplateFrame`), `intro_secs`.
- new `src/menu_anim.rs`: `lerp(keys, t)`, `composite_intro(...)` → RGBA frames.
- `src/tools.rs`: `Toolchain::ffmpeg_piped` (stdin writer / stdout reader) via `Toolchain::command`.
- `src/encoder.rs`: background / video-layer chains as functions (moved code); `menu_frames_args`
  (rawvideo to stdout); `intro` input + overlay + `-force_key_frames` + audio crossfade in
  `custom_menu_graph`.
- `src/bd_menu.rs`: `MenuTraits.intro`, `BdPage.intro`, GPR constants + module doc, dispatcher,
  menu objects, IGS PTS = T.
- `src/hdmv.rs`: `jump_object` / `jump_title` with an `Op` operand.
- `src/bd_author.rs`: `menu_meta_text` with `--custom-chapters=00:00:00.000;<T>`.
- `src/bd.rs`: `build_menus`: helper pass → composite → encode (piped); opt-out.
- settings: `src/dvd.rs` (`BuildOptions.menu_animations`), `src/config.rs`, `frontend/src/settings.ts`,
  `frontend/src/api.ts`, `src/main.rs`.
- new `tests/menu_intro.rs`.

## Steps

1. Spikes 1a and 1b: **done** (see results below; `tests/menu_intro.rs`, `hdmv::jump_*_op`).
2. Parser + test. 3. Layout fields + bd_template mapping. 4. `menu_anim.rs`. 5. Piped ffmpeg +
   encoder graph. 6. bd_menu navigation + hdmv operands. 7. bd.rs muxing, meta, IGS PTS.
8. Opt-out settings/GUI/CLI. 9. Full workspace run + Unix check.

## Verification

- `template.rs` unit test (MAX inherit, gfx offset/scale, intro/outro split); `tests/real_templates.rs`:
  Open_Style block 2 → (1 s, 0×0 @ 84,133) → (2 s, 68×54 @ 50,106); all templates parse.
- `menu_anim`: hold / interpolation / zero size; composite frame 0 vs last frame.
- `bd_menu`: exact command lists of dispatcher and menu object; paging sets GPR4=3, sub-menus 2;
  Top Menu resets GPR3; **regression:** no intro / `animations=false` → menu objects, title numbers
  and playlists unchanged.
- `hdmv`: register-operand jump bytes (I-flag 0) against libbluray's layout.
- `tests/menu_intro.rs` (skips without tools): Spike 1a packet test and the IG budget test (both
  exist, keep them green); seam frame N−1 vs N ≤ 2 on static areas; video slot continuity; MPLS has
  2 marks; **audio:** decode the clip's AC-3 to PCM, the last 20 ms correlate with the source audio
  at `[T−20 ms, T]` (crossfade target reached) and no sample jump > 0.05 full scale in the last
  `X` s; **no files:** the work dir contains no intro frame files after a build (assert on the
  directory listing – guards constraint 1).
- `tests/bluray.rs`: template build with intro → valid navigation; opt-out → identical to a build
  without animation data.
- `cargo test --workspace`, `npm run build`, Unix check. Manual (roadmap): VLC – buttons after the
  mark jump, paging skips the intro, Top Menu replays it.

## Spike 1a result (2026-10-26, tsMuxeR 2.7.0, ffmpeg 9.0.1) – PASSED

`tests/menu_intro.rs::ig_display_set_starts_behind_the_entry_point_of_the_intro_mark` (kept as
regression guard). Clip: 1080 colour menu `T + 2 s`, `-force_key_frames T`, SUP with ICS PTS = T,
`--custom-chapters=0;T`; checked at 25p-as-1080i and 23.976, with a 1.5 MB (noise) and a sparse IG
display set, before and after `bd_patch::patch_menu`:

- MPLS mark 1 = clip start + T exactly; the EP entry before it is the video PES presented exactly
  at T (EP-map PTS only keep bits 9..32 – compare via the PES, not the EP PTS).
- First IG PES starts 20 packets **behind** that entry point (SPN 689 vs 669 / 683 vs 663); no IG
  packet before T at all; ICS PTS = T exactly.
- tsMuxeR schedules the display set from the keyframe on: it arrives completely **333–383 ms
  before** the frame at T is presented (video lead 500 ms from `--vbv-len=500`), even for 1.5 MB.
  So buttons are ready on the first loop frame after `PlayPLatMark(pl, 1)`.
- Not modelled: the player's own ODS decode time (expected well inside the ~330 ms margin).

Consequence: architecture stands as planned (IGS PTS = T).

## IG display-set budget (hardware buffer safety)

Risk raised in review: a 1.5 MB display set (12 Mbit) needs ~375 ms at a 32 Mbit/s decoder, more
than the 333 ms lead. The 1.5 MB set was a synthetic worst case (noise objects). Measured with the
real templates (`tests/menu_intro.rs::real_template_ig_display_sets_fit_the_budget`, all 19
templates, one full page of every kind): **max 81 KB, median 38–48 KB**.

Rule: **`IG_BUDGET = 256 KiB`** per display set (2.1 Mbit: 131 ms at the 16 Mbit/s IG transport
rate, 66 ms at a 32 Mbit/s decoder → ≥ 2.5× headroom against the 333 ms lead).

- The test asserts it for every real template page (regression guard).
- Implementation (step 7, `bd_menu::render_page`): after building the `IgMenu`, if
  `igs::build_sup(&ig).len() > IG_BUDGET`, re-render the button objects with a reduced palette
  (`ALPHA_STEPS` 16 → 4, then 2: fewer anti-aliasing levels = longer RLE runs) and crop every
  object to the bounding box of its non-transparent pixels. Still over → German warning
  ("Menü „…“: Button-Grafiken sehr groß – Buttons erscheinen auf manchen Playern verzögert") and
  keep the smallest version. Unit test with an oversized fake menu: size shrinks, warning appears.
- The animation itself is in the video, never in the IG stream; the display set is static.

## Spike 1b result (2026-10-26) – PASSED

- libbluray `hdmv_vm.c` (`_hdmv_step`): `_fetch_operands()` runs for **every** command before the
  group switch; `imm_op1 = 0` reads `dst` from the register (`_read_reg`, GPR or PSR), and
  `INSN_JUMP_OBJECT` / `INSN_JUMP_TITLE` use that `dst`. `PlayPLatMark(pl, GPR1)` uses the same
  path and already works in VLC with our BD menus.
- `src/hdmv.rs`: `jump_object_op(Op)` / `jump_title_op(Op)` added; unit test
  `register_operand_jumps` checks the bytes (`21 00 … 05` / `21 01 … 05`) and that the immediate
  form equals `jump_object` / `jump_title`.
- Use them in DISPATCH as `jump_object_op(Op::Gpr(5))` / `jump_title_op(Op::Gpr(5))`.

**Documented fallback** (only if the manual VLC/player check of the dispatcher fails; it changes
nothing outside DISPATCH): replace commands 5/6 by compare chains over all targets,
```
; menus (k = every menu object id)            ; titles (n = 1..=title count)
cmp GPR5 == k ; JumpObject(k)                 cmp GPR5 == n ; JumpTitle(n)
```
placed under the `GPR4` branch they belong to (2 commands per target, objects stay < 1000
commands for 400 menus + titles).

Remaining manual checks (roadmap, not automatable here): VLC – buttons after the mark jump, paging
skips the intro, Top Menu replays it, dispatcher jumps reach titles and menus.
