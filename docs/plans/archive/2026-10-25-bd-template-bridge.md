# Plan: ConvertX templates on Blu-ray (`bd_template.rs`)

Status: approved 2026-10-25. Scope of this plan: coordinate upscaling + navigation translation,
wired into `bd::build_bd`. Everything downstream (`validate_bd` → `bd_menu` → `igs` → `bd_patch`)
stays unchanged.

## Key decision: translate at the action level, not the register level

- ConvertX templates never use DVD registers g1/g2/g3. `author::plan_buttons` maps `ML_*` locations
  to semantic `author::ButtonAction`s; g1 (play-all) / g2 (titleset dispatch) / g3 (VMGM router)
  exist only in `layout::plan_navigation` for custom layouts.
- `bd_menu::plan` already turns `layout::Action` into HDMV commands: `PlayTitle`/`PlayChapter` →
  `JumpTitle` (+ GPR1 chapter mark), `PlayAll` → GPR2, `Menu` → `JumpObject`.
- Therefore the bridge maps **`ML_*` → `layout::Action`** and emits a `CustomLayout`. The
  register mapping is asserted in tests on `bd_menu::plan` output, not re-implemented.

## Coordinate math

Template page W×H (usually 720×576 / 720×480, non-square storage pixels) → canvas 1920×1080.
Display aspect from `encoder::Aspect`:

- `Wide` (16:9): target rect R = (0, 0, 1920, 1080)
- `Standard` (4:3): pillarbox R = (240, 0, 1440, 1080)
- `sx = R.w / W`, `sy = R.h / H`; `x' = R.x + x·sx`, `y' = R.y + y·sy`, `w' = w·sx`, `h' = h·sy`
- font size `size_px · sy`
- keep `f64`; rounding happens in `layout::frame_box` (identity since canvas == frame size). Do
  NOT apply the DVD even-y rounding of `author::Rect::from_template`.
- images use `Fit::Stretch` (anamorphic source → square pixels). The page background becomes an
  `Image` element covering R; menu background colour black (4:3 bars stay black).

Implement as a small private struct `TemplateFrame { rx, ry, sx, sy }` with `fn new(page_w,
page_h, aspect)`, `fn rect(x, y, w, h) -> (f64, f64, f64, f64)`, `fn font(size) -> f32`.

## Element mapping

| Template | `CustomLayout` element |
|---|---|
| `GT_BITMAP` | `Content::Image` (path via `menu::resolve_resource`), `Fit::Stretch`, opacity = alpha/255 |
| `GT_TEXT` | `Content::Text`: item label / page heading (`ML_PAGE_TITLE`) / localized value; font, size, colour, bold, italic, align from `TextStyle`; `shadow = style.shadow.is_some()` |
| `GT_VIDEO` on an item | thumbnail `Image` if the caller passes one, else `Content::Video{title}` |
| `GT_VIDEO` `ML_PAGE_BACKGROUND` | page background (image element in R) |
| button value `TEXT` | invisible `Text` (opacity 0) with the label → text-shaped highlight |
| button with mask image | invisible `Image{mask}` (opacity 0) → frame highlight (`bd_menu::highlight_image` with `cov = None`) |
| colour scheme | `Element.highlight = ButtonColors` from the scheme's selection/action emphasis colour; none → layout default |

Graphics that belong to hidden item buttons are dropped (use `menu::button_of_graphic`). Element
order = template graphic order, buttons last (bottom to top).

## Navigation mapping

Menu ids: `title-p{n}` (role `Title`), `root-{t}` (role `Root`, title t), chapter pages
`chap-{t}-p{n}` (extra `Root` menus of title t, after `root-{t}`), `set-{t}` (role `Settings`).
Pagination and hiding mirror `author::plan_buttons` (reuse `count_loc`, `TITLE_ITEMS`,
`title_page_of`, `page_count`).

| `ML_*` | context | `layout::Action` |
|---|---|---|
| `ML_PLAY_EPISODE` / `_SERIES_EPISODE` / `_BONUS` (param p) | title page n, item i = (n-1)·per + p | `Menu{root-t}` if a root menu exists, else `PlayTitle{t}`; hidden if i ≥ titles |
| `ML_PLAY_ALL` | title page | `PlayAll` |
| `ML_PLAY_ALL`, `ML_RESUME` | title t's menus | `PlayTitle{t}` (RESUME: warning, BD has no resume) |
| `ML_PREV` / `ML_NEXT` | paged menu | neighbour page id; hidden on first/last page |
| `ML_GO_TITLE_MENU`, `ML_BACK` | root menu of t | `Menu{title-p<title_page_of(t)>}`; hidden on title pages |
| `ML_BACK` | chapter/settings of t | `Menu{root-t}` (fallback: title page) |
| `ML_GO_CHAPTER_MENU` / `ML_GO_SETTINGS_MENU` | title t | `Menu{chap-t-p1}` / `Menu{set-t}`; hidden if absent |
| `ML_PLAY_CHAPTER` (item i) | chapter page | `PlayChapter{t, i+1}` |
| `ML_SET_AUD` (item i) | settings | `SetAudio{source: audio_sources[i]}` |
| `ML_SET_STT` (item i) / `ML_RESET_STT` | settings | `SetSubtitle{Some(subtitle_sources[i])}` / `SetSubtitle{None}` (RESET hidden without subs) |
| anything else | – | element without action + warning |

Which pages exist: root/chapter/settings pages come from `dvd::find_page_source` (+ `page_by_style`),
with the same rules as DVD (`dvd.rs` ~1096–1113, `has_ts_menus`): chapters only for titles with
> 1 chapter, settings only with > 1 audio track or any subtitle.

## Files

- **new** `src/bd_template.rs`:
  `pub fn layout_from_template(template: &Template, template_path: &Path, titles: &[LayoutTitle],
  thumbnails: &[Option<String>], aspect: Aspect) -> Result<(CustomLayout, Vec<String>), LayoutError>`
  (+ private `TemplateFrame`, `map_action`, `page_elements`)
- `src/lib.rs`: `pub mod bd_template;`
- `src/author.rs`: `count_loc`, `TITLE_ITEMS` → `pub(crate)`
- `src/menu.rs`: `button_of_graphic` → `pub(crate)`
- `src/dvd.rs`: `find_page_source` → `pub(crate)`
- `src/bd.rs`: extract the `layout_titles` construction of `build_menus` into a helper; in the
  `None` branch of step "3. Menus" (`opts.template` non-empty) load the template, call
  `bd_template::layout_from_template`, log its warnings and continue through `build_menus`
  (replaces the warning "Blu-ray-Menüs gibt es nur mit dem Menü-Editor …"). Load errors →
  `BuildError::Template` (German message).
- `tests/real_templates.rs`: real-template BD test (below).

## Implementation order

1. `TemplateFrame` + unit tests (math).
2. `map_action` + pagination + unit tests (navigation).
3. Graphics/buttons → elements, assemble `CustomLayout` (canvas 1920×1080, version 1).
4. Wire into `bd.rs`.
5. Integration test, then full regression.

## Verification (must all pass)

- Unit tests in `bd_template.rs` (no tools needed):
  - 720×576 `Wide`: rect (360, 288, 72, 58) → (960, 540, 192, 108.75)
  - 720×576 `Standard`: same rect → x = 960, w = 144
  - 720×480: sy = 2.25, font 24 → 54
- Navigation unit tests with a small inline INI via `Template::parse`: 5 titles, 2 items per page
  → 3 title pages; PREV absent on page 1, NEXT absent on page 3; surplus items hidden; BACK /
  chapter / settings links resolve to the right ids; unknown `ML_*` → no action + warning.
- End-to-end without tools: output passes `layout::validate_bd`; `bd_menu::traits` + `bd_menu::plan`
  give `JumpTitle(t)` for titles, `JumpObject` for menu links, GPR1 for chapters, GPR2 for play-all
  (same style as the `cmds()` helper in `bd_menu.rs` tests). Also check that multiple `Root` menus
  per title (chapter pages) are planned correctly.
- `tests/real_templates.rs`: for every real template in
  `C:\ProgramData\VSO\ConvertXToDVD\7\Templates` → `layout_from_template` → `validate_bd` →
  `bd_menu::render_pages` into a temp dir; PNG layers exist, IGS object budget ok. Skips gracefully
  without templates.
- Regression: `cargo test --workspace` (baseline 370 green) and
  `cargo check --lib --tests --target x86_64-unknown-linux-gnu`.

## Out of scope (later)

Mask-shaped highlights (frame only), text outlines, intro/outro animation, GUI flag changes. The
STATUS.md out-of-scope line is updated via `/status` after implementation.
