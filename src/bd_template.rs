//! ConvertX templates on Blu-ray: turns a template (title, root, chapter and settings pages) into a
//! 1920×1080 [`CustomLayout`] that the normal Blu-ray menu path (`validate_bd` → `bd_menu` → `igs`)
//! renders unchanged.
//!
//! Translation happens at the action level: `ML_*` button locations become [`Action`]s, and
//! `bd_menu::plan` turns those into HDMV commands (no DVD registers involved).

use std::collections::HashSet;
use std::path::Path;

use crate::author::{count_loc, title_page_of, MAX_CHAPTERS, TITLE_ITEMS};
use crate::dvd::find_page_source;
use crate::encoder::{Aspect, Fit};
use crate::layout::{
    Action, Background, ButtonColors, Canvas, Content, CustomLayout, CustomMenu, Element, ElementKey, Highlight, LayoutError,
    LayoutTitle, MenuRole, MenuSound, LAYOUT_VERSION,
};
use crate::menu::{button_of_graphic, localize, page_by_style, resolve_resource, UiLang};
use crate::template::{Button, DelphiColor, Graphic, Page, Template, TextStyle};

/// Longest intro played (templates use 3 s).
const MAX_INTRO_SECS: f64 = 10.0;
const CANVAS_W: f64 = 1920.0;
const CANVAS_H: f64 = 1080.0;

/// Maps template page coordinates onto the 1920×1080 canvas.
struct TemplateFrame {
    rx: f64,
    ry: f64,
    sx: f64,
    sy: f64,
}

impl TemplateFrame {
    /// A 4:3 page is pillarboxed into the 16:9 canvas, a 16:9 page fills it.
    fn new(page_w: u32, page_h: u32, aspect: Aspect) -> Self {
        let (rx, rw) = match aspect {
            Aspect::Wide => (0.0, CANVAS_W),
            Aspect::Standard => (CANVAS_W / 8.0, CANVAS_W * 0.75),
        };
        TemplateFrame { rx, ry: 0.0, sx: rw / page_w.max(1) as f64, sy: CANVAS_H / page_h.max(1) as f64 }
    }

    fn rect(&self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        (self.rx + x * self.sx, self.ry + y * self.sy, w * self.sx, h * self.sy)
    }

    fn font(&self, size: f32) -> f32 {
        size * self.sy as f32
    }
}

/// Cuts a rectangle to the canvas (the layout validation rejects elements outside it).
fn clip((x, y, w, h): (f64, f64, f64, f64)) -> Option<(f64, f64, f64, f64)> {
    let (x0, y0) = (x.max(0.0), y.max(0.0));
    let (x1, y1) = ((x + w).min(CANVAS_W), (y + h).min(CANVAS_H));
    (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then_some((x0, y0, x1 - x0, y1 - y0))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Titles,
    /// All titles, listed on the page a series template's main menu links to.
    Episodes,
    /// Titles 2.. of a single-video template (`ML_PLAY_BONUS` items).
    Bonus,
    Root,
    Chapters,
    Settings,
}

/// The menu page being translated; `title` is 1-based (0 on title pages).
#[derive(Clone, Copy)]
struct Ctx {
    kind: Kind,
    title: u32,
    page_no: u32,
    pages: u32,
}

fn menu_id(kind: Kind, title: u32, page_no: u32) -> String {
    match kind {
        Kind::Titles => format!("title-p{page_no}"),
        Kind::Bonus => format!("bonus-p{page_no}"),
        Kind::Episodes => format!("episodes-p{page_no}"),
        Kind::Root => format!("root-{title}"),
        Kind::Chapters => format!("chap-{title}-p{page_no}"),
        Kind::Settings if page_no == 1 => format!("set-{title}"),
        Kind::Settings => format!("set-{title}-p{page_no}"),
    }
}

/// What a visible template button does and shows.
#[derive(Default)]
struct Mapped {
    action: Option<Action>,
    label: Option<String>,
    /// Title whose thumbnail / clip a video graphic of this button shows.
    video: Option<u32>,
    /// Where that clip starts (absolute in the source); `None` = the title's default sample.
    start: Option<f64>,
}

/// Where a page comes from: its template and the folder its resources are relative to.
struct Source<'a> {
    tpl: &'a Template,
    dir: &'a Path,
    page: &'a Page,
}

/// Pagination and menu existence, shared by all pages.
struct Nav<'a> {
    titles: &'a [LayoutTitle],
    /// Chapter start of each title's chapters (source time of the chapter's sample frame).
    chapter_starts: &'a [Vec<f64>],
    title_page: &'a Page,
    title_pages: u32,
    /// Pages of the bonus menu (0 = none).
    bonus_pages: u32,
    /// Pages of the episode menu (0 = none).
    episodes_pages: u32,
    has_root: Vec<bool>,
    /// Pages of the chapter / settings menu of each title (0 = none).
    chapter_pages: Vec<u32>,
    settings_pages: Vec<u32>,
}

impl Nav<'_> {
    fn title_page_id(&self, title: u32) -> String {
        menu_id(Kind::Titles, 0, title_page_of(self.title_page, title as usize - 1))
    }

    /// `None` hides the button (no such item, no such menu, no neighbour page).
    fn map_action(&self, ctx: Ctx, page: &Page, b: &Button, warnings: &mut Vec<String>) -> Option<Mapped> {
        let loc = b.location.to_ascii_uppercase();
        let (t, n) = (ctx.title, ctx.page_no);
        let lt = t.checked_sub(1).and_then(|i| self.titles.get(i as usize));
        let item = |per: usize| (n as usize - 1) * per + b.param.max(0) as usize;
        let go = |action: Action| Some(Mapped { action: Some(action), ..Mapped::default() });
        let menu = |id: String| go(Action::Menu { menu: id });
        let home = || menu(self.title_page_id(t.max(1)));
        match (ctx.kind, loc.as_str()) {
            (Kind::Titles | Kind::Episodes | Kind::Bonus, l) if TITLE_ITEMS.contains(&l) && (ctx.kind != Kind::Bonus || l == "ML_PLAY_BONUS") => {
                // The bonus menu lists the titles after the first one.
                let (i, skip) = match ctx.kind {
                    Kind::Bonus => (item(count_loc(page, &["ML_PLAY_BONUS"])), 1),
                    _ => (item(count_loc(page, TITLE_ITEMS)), 0),
                };
                let i = i + skip;
                let label = self.titles.get(i)?.label.clone();
                let title = i as u32 + 1;
                let action = if self.has_root[i] {
                    Action::Menu { menu: menu_id(Kind::Root, title, 1) }
                } else {
                    Action::PlayTitle { title }
                };
                Some(Mapped { action: Some(action), label: Some(label), video: Some(title), start: None })
            }
            (Kind::Chapters, "ML_PLAY_CHAPTER") => {
                let i = item(count_loc(page, &["ML_PLAY_CHAPTER"]));
                if i >= lt?.chapters.min(MAX_CHAPTERS) {
                    return None;
                }
                Some(Mapped {
                    action: Some(Action::PlayChapter { title: t, chapter: i as u32 + 1 }),
                    label: Some(format!("Kapitel {}", i + 1)),
                    video: Some(t),
                    start: self.chapter_starts.get(t as usize - 1).and_then(|c| c.get(i)).copied(),
                })
            }
            (Kind::Settings, "ML_SET_AUD") => {
                let i = item(count_loc(page, &["ML_SET_AUD"]));
                let source = *lt?.audio_sources.get(i)?;
                Some(Mapped { action: Some(Action::SetAudio { source }), label: Some(format!("Tonspur {}", i + 1)), ..Mapped::default() })
            }
            (Kind::Settings, "ML_SET_STT") => {
                let i = item(count_loc(page, &["ML_SET_STT"]));
                let source = *lt?.subtitle_sources.get(i)?;
                Some(Mapped {
                    action: Some(Action::SetSubtitle { source: Some(source) }),
                    label: Some(format!("Untertitel {}", i + 1)),
                    ..Mapped::default()
                })
            }
            (Kind::Settings, "ML_RESET_STT") if !lt?.subtitle_sources.is_empty() => go(Action::SetSubtitle { source: None }),
            // Item buttons on a page used for another purpose.
            (_, l) if TITLE_ITEMS.contains(&l) || ["ML_PLAY_CHAPTER", "ML_SET_AUD", "ML_SET_STT", "ML_RESET_STT"].contains(&l) => None,
            (Kind::Titles | Kind::Episodes | Kind::Bonus, "ML_PLAY_ALL") => go(Action::PlayAll),
            (_, "ML_PLAY_ALL" | "ML_RESUME") => {
                if loc == "ML_RESUME" {
                    warnings.push("ML_RESUME: Blu-ray kennt kein Fortsetzen, der Titel startet von vorn".into());
                }
                go(Action::PlayTitle { title: t.max(1) })
            }
            (k, "ML_PREV") if n > 1 => menu(menu_id(k, t, n - 1)),
            (k, "ML_NEXT") if n < ctx.pages => menu(menu_id(k, t, n + 1)),
            (_, "ML_PREV" | "ML_NEXT") => None,
            (Kind::Titles, "ML_GO_TITLE_MENU" | "ML_BACK") => None,
            (_, "ML_GO_TITLE_MENU") | (Kind::Root | Kind::Bonus | Kind::Episodes, "ML_BACK") => home(),
            (_, "ML_BACK") => match self.has_root[t as usize - 1] {
                true => menu(menu_id(Kind::Root, t, 1)),
                false => home(),
            },
            // A main menu without item buttons links straight to the first title's menus.
            (Kind::Titles, "ML_GO_CHAPTER_MENU") if self.chapter_pages[0] > 0 => menu(menu_id(Kind::Chapters, 1, 1)),
            (Kind::Titles, "ML_GO_SETTINGS_MENU") if self.settings_pages[0] > 0 => menu(menu_id(Kind::Settings, 1, 1)),
            (Kind::Titles | Kind::Episodes | Kind::Bonus, "ML_GO_CHAPTER_MENU" | "ML_GO_SETTINGS_MENU") => None,
            (Kind::Titles, "ML_GO_SERIES_EPISODES_MENU") if self.episodes_pages > 0 => menu(menu_id(Kind::Episodes, 0, 1)),
            (_, "ML_GO_SERIES_EPISODES_MENU") => None,
            (Kind::Titles | Kind::Root, "ML_GO_BONUS_MENU") if self.bonus_pages > 0 => menu(menu_id(Kind::Bonus, 0, 1)),
            (_, "ML_GO_BONUS_MENU") => None,
            (_, "ML_GO_CHAPTER_MENU") => (self.chapter_pages[t as usize - 1] > 0).then(|| menu_id(Kind::Chapters, t, 1)).and_then(|id| menu(id)),
            (_, "ML_GO_SETTINGS_MENU") => (self.settings_pages[t as usize - 1] > 0).then(|| menu_id(Kind::Settings, t, 1)).and_then(|id| menu(id)),
            (_, other) => {
                warnings.push(format!("Button „{}“: Aktion {other} wird auf Blu-ray nicht unterstützt, er bleibt ohne Funktion", b.name));
                Some(Mapped::default())
            }
        }
    }
}

fn hex(rgb: (u8, u8, u8)) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb.0, rgb.1, rgb.2)
}

/// Template text style → text content on the canvas.
fn text_content(style: Option<&TextStyle>, g_height: u32, text: &str, frame: &TemplateFrame) -> Content {
    let size = style.map_or((g_height as f32 * 0.8).max(8.0), |s| s.size_px);
    let align = match style.map_or("", |s| s.align.as_str()) {
        a if a.contains("LEFT") => "left",
        a if a.contains("RIGHT") => "right",
        _ => "center",
    };
    Content::Text {
        text: text.to_string(),
        font: style.map_or_else(|| "Arial".into(), |s| s.font_name.clone()),
        size: frame.font(size).clamp(4.0, 300.0),
        color: style.and_then(|s| s.text.as_ref()?.rgb).map_or_else(|| "#FFFFFF".into(), hex),
        align: align.into(),
        bold: style.is_some_and(|s| s.bold),
        italic: style.is_some_and(|s| s.italic),
        shadow: style.is_some_and(|s| s.shadow.is_some()),
        wrap: false,
    }
}

/// Box a text graphic is drawn in (page coordinates), minus the style's side margins.
fn text_rect(g: &Graphic, style: Option<&TextStyle>) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = (g.x as f64, g.y as f64, g.width as f64, g.height as f64);
    match style {
        Some(s) => (x + s.left_margin as f64, y, (w - (s.left_margin + s.right_margin) as f64).max(1.0), h),
        None => (x, y, w, h),
    }
}

fn element(id: String, content: Content, (x, y, w, h): (f64, f64, f64, f64), opacity: f32) -> Element {
    Element { id, content, x, y, w, h, rotation: 0, opacity, action: None, highlight: None, group: None, intro: Vec::new() }
}

/// Intro keys of a graphic whose final box is `rect` (page coordinates): the owning block's box
/// moves and fades per key, the graphic keeps its place inside the block (a zoom about the block's
/// own corner). Empty when nothing moves or the box cannot be mapped.
fn intro_keys(g: &Graphic, rect: (f64, f64, f64, f64), frame: &TemplateFrame) -> Vec<ElementKey> {
    let (bx, by, bw, bh) = (g.block_rect.0 as f64, g.block_rect.1 as f64, g.block_rect.2 as f64, g.block_rect.3 as f64);
    if g.intro.is_empty() || bw < 1.0 || bh < 1.0 {
        return Vec::new();
    }
    let final_opacity = g.alpha as f32 / 255.0;
    let keys: Vec<ElementKey> = g
        .intro
        .iter()
        .map(|k| {
            let (sx, sy) = (k.w as f64 / bw, k.h as f64 / bh);
            let (x, y, w, h) = frame.rect(k.x as f64 + (rect.0 - bx) * sx, k.y as f64 + (rect.1 - by) * sy, rect.2 * sx, rect.3 * sy);
            let opacity = k.alpha.map_or(final_opacity, |a| a as f32 / 255.0 * g.gfx_alpha as f32 / 255.0);
            ElementKey { t: k.t, x, y, w, h, opacity }
        })
        .collect();
    let (x, y, w, h) = frame.rect(rect.0, rect.1, rect.2, rect.3);
    let moves = keys.iter().any(|k| (k.x - x).abs() > 0.5 || (k.y - y).abs() > 0.5 || (k.w - w).abs() > 0.5 || (k.h - h).abs() > 0.5 || (k.opacity - final_opacity).abs() > 0.004);
    if moves { keys } else { Vec::new() }
}

/// Whether `clip` left the box alone. Not `==`: it recomputes the size as `(x + w) - x`, which is
/// not bit-identical to `w`, and such graphics would silently lose their intro keys.
fn same_box(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6 && (a.2 - b.2).abs() < 1e-6 && (a.3 - b.3).abs() < 1e-6
}

fn fit_of(padding: &str) -> Fit {
    if padding.eq_ignore_ascii_case("letterbox") {
        Fit::Contain
    } else {
        Fit::Stretch
    }
}

/// Selected / activated colour of a button: the scheme's pattern colour (what the DVD path
/// paints into the glyphs), else its first emphasis colour.
fn scheme_colors(page: &Page, b: &Button) -> Option<ButtonColors> {
    let s = page.color_scheme_of(b)?;
    let pick = |q: &[Option<DelphiColor>; 4]| q[1].as_ref().and_then(|c| c.rgb).or_else(|| q[2].as_ref().and_then(|c| c.rgb)).map(hex);
    Some(ButtonColors { color: pick(&s.selection)?, select_color: pick(&s.action)? })
}

impl Nav<'_> {
    fn page_elements(&self, ctx: Ctx, src: &Source, aspect: Aspect, thumbnails: &[Option<String>], warnings: &mut Vec<String>) -> Vec<Element> {
        let (tpl, page) = (src.tpl, src.page);
        let frame = TemplateFrame::new(page.width, page.height, aspect);
        let mut planned: Vec<(&Button, Mapped)> = page
            .buttons
            .iter()
            .filter(|b| b.visible)
            .filter_map(|b| self.map_action(ctx, page, b, warnings).map(|m| (b, m)))
            .collect();
        let heading = match ctx.kind {
            Kind::Bonus => "Bonus",
            Kind::Episodes => "Episoden",
            _ => &self.titles[ctx.title.max(1) as usize - 1].label,
        };
        let image_file = |value: &str, warnings: &mut Vec<String>| {
            let path = resolve_resource(src.dir, value);
            if path.is_file() {
                Some(path.to_string_lossy().into_owned())
            } else {
                warnings.push(format!("Bild nicht gefunden: {}", path.display()));
                None
            }
        };
        // Text a graphic shows: the item label, the page heading, or the (localized) template text.
        let text_of = |g: &Graphic, m: Option<&Mapped>| -> String {
            match m.and_then(|m| m.label.as_deref()) {
                Some(l) => l.to_string(),
                None if g.location.eq_ignore_ascii_case("ML_PAGE_TITLE") => heading.to_string(),
                None => localize(&g.value, UiLang::De).to_string(),
            }
        };

        let mut out: Vec<Element> = Vec::new();
        for g in &page.graphics {
            let owner = button_of_graphic(page, g);
            let mapped = owner.and_then(|o| planned.iter().find(|(b, _)| b.index == o)).map(|(_, m)| m);
            if (owner.is_some() && mapped.is_none()) || g.width == 0 || g.height == 0 {
                continue; // hidden button or empty graphic
            }
            let mut rect = (g.x as f64, g.y as f64, g.width as f64, g.height as f64);
            let content = match g.gfx_type.to_ascii_uppercase().as_str() {
                "GT_BITMAP" if !g.value.is_empty() => {
                    let Some(src) = image_file(&g.value, warnings) else { continue };
                    Content::Image { src, fit: fit_of(&g.padding) }
                }
                "GT_VIDEO" => match mapped.and_then(|m| m.video.map(|v| (v, m.start))) {
                    Some((title, start)) => match thumbnails.get(title as usize - 1).cloned().flatten().filter(|_| start.is_none()) {
                        Some(src) => Content::Image { src, fit: fit_of(&g.padding) },
                        None => Content::Video { title, start_secs: start, fit: fit_of(&g.padding) },
                    },
                    // A background video is only usable when the template names a picture.
                    None if g.location.eq_ignore_ascii_case("ML_PAGE_BACKGROUND") && image::ImageFormat::from_path(&g.value).is_ok() => {
                        let Some(src) = image_file(&g.value, warnings) else { continue };
                        Content::Image { src, fit: Fit::Stretch }
                    }
                    // The menus of a title play that title's clip in the page's video window; the main
                    // menu shows the first title.
                    None if g.location.eq_ignore_ascii_case("ML_PAGE_BACKGROUND") => {
                        Content::Video { title: ctx.title.max(1), start_secs: None, fit: fit_of(&g.padding) }
                    }
                    None => continue,
                },
                "GT_TEXT" => {
                    let text = text_of(g, mapped);
                    if text.trim().is_empty() {
                        continue;
                    }
                    let style = tpl.text_style(&g.style);
                    rect = text_rect(g, style);
                    text_content(style, g.height, &text, &frame)
                }
                _ => continue,
            };
            let full = frame.rect(rect.0, rect.1, rect.2, rect.3);
            let Some(r) = clip(full) else { continue };
            let mut e = element(format!("g{}", out.len()), content, r, g.alpha as f32 / 255.0);
            // A box cut by the canvas edge would not match the keys: such a graphic just appears.
            if same_box(r, full) {
                e.intro = intro_keys(g, rect, &frame);
            }
            out.push(e);
        }

        // Buttons last (on top), the template's default button first so it is selected initially.
        planned.sort_by_key(|(b, _)| !b.is_default);
        for (b, m) in planned {
            let Some(action) = m.action else {
                continue; // unsupported action: its graphics stay as decoration
            };
            let owned = |gfx: &str| page.graphics.iter().find(|g| button_of_graphic(page, g) == Some(b.index) && g.gfx_type.eq_ignore_ascii_case(gfx));
            // Invisible highlight carrier: the caption's glyph shape for text buttons, the overlay mask
            // (bd_menu lights its bright pixels for invisible image buttons), else a frame.
            let text = b.value.eq_ignore_ascii_case("TEXT").then(|| owned("GT_TEXT")).flatten();
            // The glyph-shaped highlight must sit exactly on the drawn caption, so the carrier takes
            // the caption's box (not the button's); other buttons keep the button's box.
            let page_rect = match text {
                Some(g) => text_rect(g, tpl.text_style(&g.style)),
                None => (b.x as f64, b.y as f64, b.width as f64, b.height as f64),
            };
            let Some(r) = clip(frame.rect(page_rect.0, page_rect.1, page_rect.2, page_rect.3)) else {
                warnings.push(format!("Button „{}“ liegt außerhalb der Seite und wurde ausgelassen", b.name));
                continue;
            };
            let content = match text {
                Some(g) => text_content(tpl.text_style(&g.style), g.height, &text_of(g, Some(&Mapped { label: m.label.clone(), ..Mapped::default() })), &frame),
                None => {
                    // Fitted like the visible icon (DVD path: the button's first graphic), else the mask is
                    // stretched into a ring wider than the round icon.
                    let fit = page
                        .graphics
                        .iter()
                        .find(|g| button_of_graphic(page, g) == Some(b.index))
                        .map_or(Fit::Stretch, |g| fit_of(&g.padding));
                    let candidates = [Some(b.value.as_str()).filter(|v| v.contains('.')), owned("GT_BITMAP").map(|g| g.value.as_str())]
                        .into_iter()
                        .flatten()
                        .chain(page.backgrounds.iter().map(String::as_str));
                    match candidates.map(|v| resolve_resource(src.dir, v)).find(|p| p.is_file()) {
                        Some(p) => Content::Image { src: p.to_string_lossy().into_owned(), fit },
                        None => {
                            warnings.push(format!("Button „{}“ hat keine Bilddatei für die Markierung und wurde ausgelassen", b.name));
                            continue;
                        }
                    }
                }
            };
            let mut e = element(format!("b{}", b.index), content, r, 0.0);
            e.action = Some(action);
            e.highlight = scheme_colors(page, b);
            out.push(e);
        }
        out
    }
}

/// Builds the Blu-ray menu layout of a ConvertX template. `thumbnails[i]` is an optional still
/// for title `i + 1` (video slots show the moving clip without one); `chapter_starts[i][c]` is the
/// source time of chapter `c + 1`'s sample frame of title `i + 1`. Returns the layout and
/// German warnings.
pub fn layout_from_template(
    template: &Template,
    template_path: &Path,
    titles: &[LayoutTitle],
    thumbnails: &[Option<String>],
    chapter_starts: &[Vec<f64>],
    aspect: Aspect,
) -> Result<(CustomLayout, Vec<String>), LayoutError> {
    let fail = |m: &str| LayoutError(vec![m.to_string()]);
    if titles.is_empty() {
        return Err(fail("keine Titel für das Menü"));
    }
    let title_page = page_by_style(template, "MNUS_TITLE")
        .or(template.pages.first())
        .ok_or_else(|| fail("Die Menü-Vorlage enthält keine Seiten"))?;
    let main_dir = template_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut warnings = Vec::new();

    let sources = ["MNUS_ROOT", "MNUS_CHAPTER", "MNUS_SETTINGS"].map(|style| find_page_source(template, template_path, style));
    let [root, chap, set] = [0, 1, 2].map(|i| {
        sources[i].as_ref().and_then(|(t, dir)| {
            let style = ["MNUS_ROOT", "MNUS_CHAPTER", "MNUS_SETTINGS"][i];
            page_by_style(t, style).map(|page| Source { tpl: t, dir, page })
        })
    });
    for (name, s) in [("Root", &root), ("Kapitel", &chap), ("Einstellungen", &set)] {
        if s.is_none() {
            warnings.push(format!("Kein {name}-Menü in der Vorlage gefunden, wird weggelassen"));
        }
    }

    let pages = |items: usize, per: usize| items.div_ceil(per.max(1)).max(1) as u32;
    let chapter_pages = titles
        .iter()
        .map(|t| match &chap {
            Some(c) if t.chapters > 1 => pages(t.chapters.min(MAX_CHAPTERS), count_loc(c.page, &["ML_PLAY_CHAPTER"])),
            _ => 0,
        })
        .collect();
    let settings_pages = titles
        .iter()
        .map(|t| match &set {
            Some(s) if t.audio_sources.len() > 1 || !t.subtitle_sources.is_empty() => pages(t.audio_sources.len(), count_loc(s.page, &["ML_SET_AUD"]))
                .max(pages(t.subtitle_sources.len(), count_loc(s.page, &["ML_SET_STT"]))),
            _ => 0,
        })
        .collect();
    // Single-video template: the main menu has no item buttons, the other titles sit in the bonus menu.
    let no_items = count_loc(title_page, TITLE_ITEMS) == 0;
    let episodes = page_by_style(template, "MNUS_SERIES_EPISODES").filter(|p| no_items && count_loc(p, TITLE_ITEMS) > 0);
    let bonus = page_by_style(template, "MNUS_BONUS")
        .filter(|p| episodes.is_none() && titles.len() > 1 && no_items && count_loc(p, &["ML_PLAY_BONUS"]) > 0);
    let nav = Nav {
        titles,
        chapter_starts,
        title_page,
        title_pages: match count_loc(title_page, TITLE_ITEMS) {
            0 => 1,
            per => pages(titles.len(), per),
        },
        bonus_pages: bonus.map_or(0, |p| pages(titles.len() - 1, count_loc(p, &["ML_PLAY_BONUS"]))),
        episodes_pages: episodes.map_or(0, |p| pages(titles.len(), count_loc(p, TITLE_ITEMS))),
        has_root: titles.iter().map(|_| root.is_some()).collect(),
        chapter_pages,
        settings_pages,
    };
    let title_src = Source { tpl: template, dir: &main_dir, page: title_page };

    let mut menus = Vec::new();
    let mut add = |src: &Source, kind: Kind, title: u32, page_no: u32, pages: u32, name: String, warnings: &mut Vec<String>| {
        let ctx = Ctx { kind, title, page_no, pages };
        let elements = nav.page_elements(ctx, src, aspect, thumbnails, warnings);
        let intro_end = elements.iter().flat_map(|e| e.intro.iter().map(|k| k.t)).fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
        menus.push(CustomMenu {
            id: menu_id(kind, title, page_no),
            name,
            role: if matches!(kind, Kind::Titles | Kind::Episodes | Kind::Bonus) { MenuRole::Title } else if kind == Kind::Settings { MenuRole::Settings } else { MenuRole::Root },
            title: (title > 0).then_some(title),
            background: Background::default(),
            duration_secs: None,
            audio: MenuSound::None,
            intro_secs: intro_end.map(|t| t.max(src.page.intro_duration).clamp(0.5, MAX_INTRO_SECS)),
            elements,
        });
    };
    for n in 1..=nav.title_pages {
        add(&title_src, Kind::Titles, 0, n, nav.title_pages, format!("Titelmenü {n}"), &mut warnings);
    }
    if let Some(page) = episodes {
        let src = Source { tpl: template, dir: &main_dir, page };
        for n in 1..=nav.episodes_pages {
            add(&src, Kind::Episodes, 0, n, nav.episodes_pages, format!("Episodenmenü {n}"), &mut warnings);
        }
    }
    if let Some(page) = bonus {
        let src = Source { tpl: template, dir: &main_dir, page };
        for n in 1..=nav.bonus_pages {
            add(&src, Kind::Bonus, 0, n, nav.bonus_pages, format!("Bonusmenü {n}"), &mut warnings);
        }
    }
    for t in 1..=titles.len() as u32 {
        let i = t as usize - 1;
        if let Some(src) = &root {
            add(src, Kind::Root, t, 1, 1, format!("Titel {t}"), &mut warnings);
        }
        if let (Some(src), n) = (&chap, nav.chapter_pages[i]) {
            for p in 1..=n {
                add(src, Kind::Chapters, t, p, n, format!("Kapitel {t} – Seite {p}"), &mut warnings);
            }
        }
        if let (Some(src), n) = (&set, nav.settings_pages[i]) {
            for p in 1..=n {
                add(src, Kind::Settings, t, p, n, format!("Einstellungen {t} – Seite {p}"), &mut warnings);
            }
        }
    }

    let mut seen = HashSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    let layout = CustomLayout {
        version: LAYOUT_VERSION,
        canvas: Canvas { width: CANVAS_W as u32, height: CANVAS_H as u32 },
        highlight: Highlight::default(),
        animations: true,
        menus,
    };
    Ok((layout, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::author::{ClipSource, MenuFrame, PlaybackOptions};
    use crate::bd_menu;
    use crate::hdmv;

    #[test]
    fn frame_math() {
        let f = TemplateFrame::new(720, 576, Aspect::Wide);
        assert_eq!(f.rect(360.0, 288.0, 72.0, 58.0), (960.0, 540.0, 192.0, 108.75));
        let f = TemplateFrame::new(720, 576, Aspect::Standard);
        let (x, _, w, _) = f.rect(360.0, 288.0, 72.0, 58.0);
        assert_eq!((x, w), (960.0, 144.0));
        let f = TemplateFrame::new(720, 480, Aspect::Wide);
        assert_eq!((f.sy, f.font(24.0)), (2.25, 54.0));
    }

    fn lt(label: &str, chapters: usize, audio: &[u32], subs: &[u32]) -> LayoutTitle {
        LayoutTitle {
            label: label.into(),
            chapters,
            duration_secs: 100.0,
            clip: ClipSource { path: "a.mkv".into(), start_secs: 10.0, has_audio: true },
            audio_sources: audio.to_vec(),
            subtitle_sources: subs.to_vec(),
        }
    }

    fn page(idx: usize, style: &str, buttons: &[(&str, i32, i32)]) -> String {
        let mut s = format!("[CXT\\pages\\Item{idx}]\nname=P{idx}\nstyle={style}\nWidth=720\nHeight=576\n");
        for (i, (loc, param, row)) in buttons.iter().enumerate() {
            // Items sit in a column, the rest in a row at the bottom: no overlaps.
            let (x, y) = match *loc {
                "ML_PLAY_EPISODE" | "ML_PLAY_CHAPTER" | "ML_SET_AUD" | "ML_SET_STT" | "ML_RESET_STT" => (20, 10 + row * 60),
                _ => (20 + i as i32 * 100, 500),
            };
            s += &format!(
                "[CXT\\pages\\Item{idx}\\buttons\\Item{i}]\nname=b{i}\nvalue=TEXT\nLeft={x}\nTop={y}\nWidth=90\nHeight=40\nlocation={loc}\nparam={param}\n"
            );
            let gx = x + 7;
            s += &format!(
                "[CXT\\pages\\Item{idx}\\blocks\\Item0\\Gfx\\Item{i}]\nvalue=Txt\ngfx_type=GT_TEXT\nLocation={loc}\nparam={param}\nLeft={gx}\nTop={y}\nWidth=80\nHeight=40\n"
            );
        }
        s
    }

    fn video(page: &str, block: usize, loc: &str, param: i32, (x, y, w, h): (i32, i32, i32, i32)) -> String {
        format!(
            "[CXT\\pages\\{page}\\blocks\\Item{block}\\Gfx\\Item0]\ngfx_type=GT_VIDEO\nLocation={loc}\nparam={param}\nLeft={x}\nTop={y}\nWidth={w}\nHeight={h}\n"
        )
    }

    /// 2 items per title page, 2 chapters / audio / subtitles per page.
    fn template() -> Template {
        let ini = [
            "[CXT]\nKind=mtOpenStyle\n".to_string(),
            page(0, "MNUS_TITLE", &[("ML_PLAY_EPISODE", 0, 0), ("ML_PLAY_EPISODE", 1, 1), ("ML_PREV", 0, 0), ("ML_NEXT", 0, 0), ("ML_PLAY_ALL", 0, 0), ("ML_BACK", 0, 0), ("ML_FOO", 0, 0)]),
            page(1, "MNUS_ROOT", &[("ML_PLAY_ALL", 0, 0), ("ML_RESUME", 0, 0), ("ML_GO_CHAPTER_MENU", 0, 0), ("ML_GO_SETTINGS_MENU", 0, 0), ("ML_BACK", 0, 0), ("ML_GO_TITLE_MENU", 0, 0)]),
            page(2, "MNUS_CHAPTER", &[("ML_PLAY_CHAPTER", 0, 0), ("ML_PLAY_CHAPTER", 1, 1), ("ML_PREV", 0, 0), ("ML_NEXT", 0, 0), ("ML_BACK", 0, 0)]),
            page(3, "MNUS_SETTINGS", &[("ML_SET_AUD", 0, 0), ("ML_SET_AUD", 1, 1), ("ML_SET_STT", 0, 2), ("ML_RESET_STT", 0, 3), ("ML_BACK", 0, 0)]),
        ]
        .concat()
            + &video("Item1", 1, "ML_PAGE_BACKGROUND", 0, (0, 0, 720, 576))
            + &video("Item2", 1, "ML_PLAY_CHAPTER", 0, (200, 10, 100, 50))
            + &video("Item2", 2, "ML_PLAY_CHAPTER", 1, (200, 70, 100, 50));
        Template::parse(&ini).unwrap()
    }

    fn build(titles: &[LayoutTitle]) -> (CustomLayout, Vec<String>) {
        layout_from_template(&template(), Path::new("t.ini"), titles, &vec![None; titles.len()], &vec![vec![11.0, 22.0, 33.0]; titles.len()], Aspect::Wide).unwrap()
    }

    fn five() -> Vec<LayoutTitle> {
        vec![lt("A", 1, &[1], &[]), lt("B", 3, &[1, 2], &[5]), lt("C", 1, &[1], &[]), lt("D", 1, &[1], &[]), lt("E", 1, &[1], &[])]
    }

    fn actions(l: &CustomLayout, id: &str) -> Vec<Action> {
        l.menus.iter().find(|m| m.id == id).unwrap().elements.iter().filter_map(|e| e.action.clone()).collect()
    }

    fn menu_to(id: &str) -> Action {
        Action::Menu { menu: id.into() }
    }

    #[test]
    fn title_pages_and_paging() {
        let (l, warnings) = build(&five());
        let ids: Vec<&str> = l.menus.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(&ids[..3], ["title-p1", "title-p2", "title-p3"]);
        // 2 items per page, first page has NEXT but no PREV, the last has PREV but no NEXT.
        assert_eq!(
            actions(&l, "title-p1"),
            [menu_to("root-1"), menu_to("root-2"), menu_to("title-p2"), Action::PlayAll]
        );
        assert_eq!(actions(&l, "title-p3"), [menu_to("root-5"), menu_to("title-p2"), Action::PlayAll]);
        // Unknown action: kept without function, with a warning; BACK is hidden on title pages.
        assert!(warnings.iter().any(|w| w.contains("ML_FOO")));
    }

    #[test]
    fn sub_menu_links() {
        let (l, warnings) = build(&five());
        // Title 2 has 3 chapters (2 pages), 2 audio tracks and a subtitle: all menus exist.
        let ids: Vec<&str> = l.menus.iter().map(|m| m.id.as_str()).collect();
        assert!(["root-2", "chap-2-p1", "chap-2-p2", "set-2"].iter().all(|id| ids.contains(id)), "{ids:?}");
        // Title 1: single chapter, one audio track, no subtitle → only the root menu.
        assert!(!ids.contains(&"chap-1-p1") && !ids.contains(&"set-1"));
        assert_eq!(actions(&l, "root-1"), [Action::PlayTitle { title: 1 }, Action::PlayTitle { title: 1 }, menu_to("title-p1"), menu_to("title-p1")], "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("ML_RESUME")));
        assert_eq!(
            actions(&l, "root-2"),
            [
                Action::PlayTitle { title: 2 },
                Action::PlayTitle { title: 2 },
                menu_to("chap-2-p1"),
                menu_to("set-2"),
                menu_to("title-p1"),
                menu_to("title-p1")
            ]
        );
        assert_eq!(
            actions(&l, "chap-2-p1"),
            [Action::PlayChapter { title: 2, chapter: 1 }, Action::PlayChapter { title: 2, chapter: 2 }, menu_to("chap-2-p2"), menu_to("root-2")]
        );
        // Page 2 shows chapter 3 only (item 2 hidden) and links back.
        assert_eq!(actions(&l, "chap-2-p2"), [Action::PlayChapter { title: 2, chapter: 3 }, menu_to("chap-2-p1"), menu_to("root-2")]);
        assert_eq!(
            actions(&l, "set-2"),
            [
                Action::SetAudio { source: 1 },
                Action::SetAudio { source: 2 },
                Action::SetSubtitle { source: Some(5) },
                Action::SetSubtitle { source: None },
                menu_to("root-2")
            ]
        );
    }

    #[test]
    fn single_video_template_has_a_bonus_menu_and_a_main_video() {
        let ini = [
            "[CXT]\nKind=mtOpenStyle\n".to_string(),
            page(0, "MNUS_TITLE", &[("ML_RESUME", 0, 0), ("ML_GO_BONUS_MENU", 0, 0)]),
            page(1, "MNUS_BONUS", &[("ML_PLAY_BONUS", 0, 0), ("ML_PLAY_BONUS", 1, 1), ("ML_BACK", 0, 0)]),
        ]
        .concat()
            + &video("Item0", 1, "ML_PAGE_BACKGROUND", 0, (0, 0, 424, 340));
        let tpl = Template::parse(&ini).unwrap();
        let titles = [lt("A", 1, &[1], &[]), lt("B", 1, &[1], &[]), lt("C", 1, &[1], &[])];
        let (l, _) = layout_from_template(&tpl, Path::new("t.ini"), &titles, &[None, None, None], &vec![vec![1.0]; 3], Aspect::Wide).unwrap();
        let ids: Vec<&str> = l.menus.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(&ids[..2], ["title-p1", "bonus-p1"], "one main page although there are three titles");
        assert_eq!(actions(&l, "title-p1"), [Action::PlayTitle { title: 1 }, menu_to("bonus-p1")]);
        // Titles 2 and 3 are the bonus items (the page holds two), the first title is not.
        assert_eq!(actions(&l, "bonus-p1"), [Action::PlayTitle { title: 2 }, Action::PlayTitle { title: 3 }, menu_to("title-p1")]);
        assert!(menu(&l, "title-p1").elements.iter().any(|e| matches!(e.content, Content::Video { title: 1, start_secs: None, .. })));
    }

    #[test]
    fn series_template_lists_all_titles_in_the_episode_menu() {
        let ini = [
            "[CXT]\nKind=mtOpenStyle\n".to_string(),
            page(0, "MNUS_TITLE", &[("ML_PLAY_ALL", 0, 0), ("ML_GO_SERIES_EPISODES_MENU", 0, 0), ("ML_GO_BONUS_MENU", 0, 0)]),
            page(1, "MNUS_SERIES_EPISODES", &[("ML_PLAY_EPISODE", 0, 0), ("ML_PLAY_EPISODE", 1, 1), ("ML_NEXT", 0, 0), ("ML_GO_TITLE_MENU", 0, 0)]),
            page(2, "MNUS_BONUS", &[("ML_PLAY_BONUS", 0, 0), ("ML_BACK", 0, 0)]),
        ]
        .concat();
        let tpl = Template::parse(&ini).unwrap();
        let titles = [lt("A", 1, &[1], &[]), lt("B", 1, &[1], &[]), lt("C", 1, &[1], &[])];
        let (l, _) = layout_from_template(&tpl, Path::new("t.ini"), &titles, &[None, None, None], &vec![vec![1.0]; 3], Aspect::Wide).unwrap();
        let ids: Vec<&str> = l.menus.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(&ids[..3], ["title-p1", "episodes-p1", "episodes-p2"], "no bonus menu next to an episode menu: {ids:?}");
        // The bonus button is hidden, the episode button opens the list.
        assert_eq!(actions(&l, "title-p1"), [Action::PlayAll, menu_to("episodes-p1")]);
        assert_eq!(
            actions(&l, "episodes-p1"),
            [Action::PlayTitle { title: 1 }, Action::PlayTitle { title: 2 }, menu_to("episodes-p2"), menu_to("title-p1")]
        );
        assert_eq!(actions(&l, "episodes-p2"), [Action::PlayTitle { title: 3 }, menu_to("title-p1")]);
    }

    fn menu<'a>(l: &'a CustomLayout, id: &str) -> &'a CustomMenu {
        l.menus.iter().find(|m| m.id == id).unwrap()
    }

    #[test]
    fn highlight_carrier_sits_on_the_caption() {
        let (l, _) = build(&five());
        let m = menu(&l, "title-p1");
        let boxed = |e: &Element| (e.x, e.y, e.w, e.h);
        for carrier in m.elements.iter().filter(|e| e.action.is_some()) {
            let Content::Text { text, .. } = &carrier.content else { panic!("text button expected") };
            assert_eq!(carrier.opacity, 0.0);
            let on_caption = m.elements.iter().any(|e| e.action.is_none() && e.opacity > 0.0 && matches!(&e.content, Content::Text { .. }) && boxed(e) == boxed(carrier));
            assert!(on_caption, "{text}: {:?}", boxed(carrier));
        }
    }

    #[test]
    fn video_slots_follow_the_chapters_and_the_title_backgrounds() {
        let (l, _) = build(&five());
        let starts: Vec<(u32, Option<f64>)> = menu(&l, "chap-2-p1")
            .elements
            .iter()
            .filter_map(|e| match e.content {
                Content::Video { title, start_secs, .. } => Some((title, start_secs)),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [(2, Some(11.0)), (2, Some(22.0))]);
        // Root menus play their title as background; the main menu has no background clip.
        assert!(menu(&l, "root-3").elements.iter().any(|e| matches!(e.content, Content::Video { title: 3, start_secs: None, .. })));
        assert!(menu(&l, "title-p1").elements.iter().all(|e| !matches!(e.content, Content::Video { .. })));
    }

    #[test]
    fn icon_masks_are_fitted_like_the_icon() {
        let dir = std::env::temp_dir().join(format!("bdtpl-mask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = |name: &str| image::RgbaImage::from_pixel(4, 3, image::Rgba([255, 255, 255, 255])).save(dir.join(name)).unwrap();
        (png("icon.png"), png("mask.png"));
        let ini = "[CXT]\n[CXT\\pages\\Item0]\nname=P\nstyle=MNUS_TITLE\nWidth=720\nHeight=576\n\
            [CXT\\pages\\Item0\\buttons\\Item0]\nname=b\nvalue=mask.png\nLeft=10\nTop=10\nWidth=154\nHeight=120\nlocation=ML_PLAY_ALL\n\
            [CXT\\pages\\Item0\\blocks\\Item0\\Gfx\\Item0]\nvalue=icon.png\ngfx_type=GT_BITMAP\nLocation=ML_PLAY_ALL\nLeft=10\nTop=10\nWidth=154\nHeight=120\nPaddingMethod=letterbox\n";
        let t = Template::parse(ini).unwrap();
        let (l, w) = layout_from_template(&t, &dir.join("t.ini"), &[lt("A", 1, &[1], &[])], &[None], &[vec![]], Aspect::Wide).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let button = l.menus[0].elements.iter().find(|e| e.action.is_some()).unwrap_or_else(|| panic!("no button: {w:?}"));
        assert!(matches!(&button.content, Content::Image { src, fit: Fit::Contain } if src.ends_with("mask.png")), "{:?}", button.content);
        assert_eq!(button.opacity, 0.0);
    }

    #[test]
    fn bd_navigation_commands() {
        let titles = five();
        let (l, _) = build(&titles);
        let frame = MenuFrame::square(1920, 1080);
        crate::layout::validate_bd(&l, &titles, frame).unwrap();
        let nav = bd_menu::plan(&l, &titles, &bd_menu::traits(&l, &titles, frame), PlaybackOptions::default(), &titles.iter().map(|t| vec![t.chapters]).collect::<Vec<_>>());
        let idx = |id: &str| l.menus.iter().position(|m| m.id == id).unwrap();
        // Graphic elements carry no commands: count buttons only.
        let cmds = |id: &str, n: usize| {
            let at = l.menus[idx(id)].elements.iter().enumerate().filter(|(_, e)| e.action.is_some()).nth(n).unwrap().0;
            nav.commands[idx(id)][at].clone().unwrap()
        };
        let obj = |id: &str| 2 + idx(id) as u32;
        // Menu links are JumpObject; play-all sets GPR2; a chapter sets the GPR1 mark; titles use JumpTitle.
        assert_eq!(cmds("title-p1", 0), [hdmv::jump_object(obj("root-1"))]);
        assert_eq!(cmds("title-p1", 3), [hdmv::mov(1, hdmv::Op::Imm(0)), hdmv::mov(2, hdmv::Op::Imm(1)), hdmv::jump_title(1)]);
        assert_eq!(cmds("chap-2-p1", 1), [hdmv::mov(1, hdmv::Op::Imm(1)), hdmv::mov(2, hdmv::Op::Imm(0)), hdmv::jump_title(2)]);
        assert_eq!(cmds("root-1", 0), [hdmv::mov(1, hdmv::Op::Imm(0)), hdmv::mov(2, hdmv::Op::Imm(0)), hdmv::jump_title(1)]);
        // Several Root menus per title (root + chapter pages) are fine: the root menu is the title's own.
        assert_eq!(nav.objects.len(), 2 + l.menus.len() + titles.len());
    }
    #[test]
    fn clipping_an_inside_box_keeps_it_the_same_box() {
        let full = (1234.567_f64, 800.654_f64, 321.098_f64, 123.456_f64);
        let r = clip(full).unwrap();
        assert!(same_box(r, full));
        assert!(!same_box(clip((1800.0, 0.0, 300.0, 50.0)).unwrap(), (1800.0, 0.0, 300.0, 50.0)), "a box cut by the edge differs");
    }

    #[test]
    fn block_animation_becomes_element_keys_in_canvas_coordinates() {
        let ini = r"[CXT]
[CXT\pages\Item0]
name=P
style=MNUS_TITLE
Width=720
Height=576
intro_duration=3
[CXT\pages\Item0\blocks\Item0]
Left=50
Top=106
Width=68
Height=54
Alpha=255
[CXT\pages\Item0\blocks\Item0\Gfx\Item0]
value=Hallo
gfx_type=GT_TEXT
Left=0
Top=0
Width=68
Height=54
[CXT\pages\Item0\blocks\Item0\Anim\Item0]
Origin=MNUP_INTRO
TimeStamp=1
alpha=2147483647
left=84
top=133
height=0
width=0
[CXT\pages\Item0\blocks\Item0\Anim\Item1]
Origin=MNUP_INTRO
TimeStamp=2
alpha=2147483647
left=50
top=106
height=54
width=68
[CXT\pages\Item0\blocks\Item1]
Left=0
Top=0
Width=100
Height=40
[CXT\pages\Item0\blocks\Item1\Gfx\Item0]
value=Still
gfx_type=GT_TEXT
Width=100
Height=40
";
        let tpl = Template::parse(ini).unwrap();
        let (l, _) = layout_from_template(&tpl, Path::new("t.ini"), &[lt("A", 1, &[1], &[])], &[None], &[vec![]], Aspect::Wide).unwrap();
        assert!(l.animations);
        let m = &l.menus[0];
        assert_eq!(m.intro_secs, Some(3.0), "the page's intro length wins over the last key");
        let moving: Vec<&Element> = m.elements.iter().filter(|e| !e.intro.is_empty()).collect();
        assert_eq!(moving.len(), 1, "a block without keys does not animate");
        let (sx, sy) = (1920.0 / 720.0, 1080.0 / 576.0);
        let [from, to] = moving[0].intro[..] else { panic!("two keys") };
        assert_eq!((from.t, from.x, from.y, from.w, from.h, from.opacity), (1.0, 84.0 * sx, 133.0 * sy, 0.0, 0.0, 1.0));
        assert_eq!((to.t, to.x, to.y, to.w, to.h), (2.0, moving[0].x, moving[0].y, moving[0].w, moving[0].h));
        assert!((to.x - 50.0 * sx).abs() < 1e-9 && (to.w - 68.0 * sx).abs() < 1e-9);
    }

}
