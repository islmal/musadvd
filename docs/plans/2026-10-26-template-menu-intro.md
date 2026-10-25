# Template menu intro animations on Blu-ray/AVCHD

Status: planned 2026-10-26. Scope: ConvertX template **intro** on BD/AVCHD via `CustomLayout`.
Not in scope: outro (only the dispatcher hook), DVD templates (own renderer in `menu.rs`),
skipping the intro by key press.

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

1. Spikes: (a) tsMuxeR packet order of the IG display set vs. the EP entry of mark 1 (packet level);
   (b) register-operand `JumpObject`/`JumpTitle` encoding (fallback: compare chain in the dispatcher).
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
- `tests/menu_intro.rs` (skips without tools): Spike 1a packet test (kept as regression guard);
  seam frame N−1 vs N ≤ 2 on static areas; video slot continuity; audio at the clip end matches the
  audio at `T` (crossfade); MPLS has 2 marks.
- `tests/bluray.rs`: template build with intro → valid navigation; opt-out → identical to a build
  without animation data.
- `cargo test --workspace`, `npm run build`, Unix check. Manual (roadmap): VLC – buttons after the
  mark jump, paging skips the intro, Top Menu replays it.

## Spike 1a result

(filled in by the spike run)
