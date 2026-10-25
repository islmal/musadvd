//! Renders the real "Open Style" template and checks images and generated XML.
//! Skipped (with a message) when ConvertXToDVD's templates are not installed.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use convertx_rust::author::{ButtonAction, TitleSpec};
use convertx_rust::menu::{self, MenuSetOptions};

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn template() -> Option<&'static Path> {
    let p = Path::new(TEMPLATE);
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("skipping: {TEMPLATE} not found");
        None
    }
}

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cx_render_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn titles(n: usize) -> Vec<TitleSpec> {
    (1..=n).map(TitleSpec::placeholder).collect()
}

/// Reads an indexed PNG: (width, height, pixels, palette entries).
fn read_indexed(path: &Path) -> (u32, u32, Vec<u8>, usize) {
    let mut reader = png::Decoder::new(BufReader::new(File::open(path).unwrap())).read_info().unwrap();
    let info = reader.info();
    assert_eq!(info.color_type, png::ColorType::Indexed, "{}", path.display());
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    let palette_len = info.palette.as_ref().unwrap().len() / 3;
    let (w, h) = (info.width, info.height);
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut buf).unwrap();
    (w, h, buf, palette_len)
}

#[test]
fn single_page_menu() {
    let Some(tpl) = template() else { return };
    let out = out_dir("single");
    let summary = menu::render_menu_set(&MenuSetOptions {
        template_path: tpl,
        page_index: None,
        titles: titles(10),
        menu_title: Some("Test"),
        output_dir: &out,
        debug_frames: false,
    })
    .unwrap();
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);

    // One page: 10 titles + play all; PREV/NEXT hidden.
    assert_eq!(summary.menus.len(), 1);
    let buttons = &summary.menus[0].buttons;
    assert_eq!(buttons.len(), 11);
    assert_eq!(buttons[0].action, ButtonAction::PlayTitle(1));
    assert_eq!(buttons[9].action, ButtonAction::PlayTitle(10));
    assert!(buttons.iter().any(|b| b.action == ButtonAction::PlayAll));
    assert!(!buttons.iter().any(|b| matches!(b.action, ButtonAction::Menu(_))));
    for b in buttons {
        assert_eq!(b.rect.y0 % 2, 0, "{b:?}");
        assert_eq!(b.rect.y1 % 2, 0, "{b:?}");
    }

    for f in ["menu_bg.png", "menu_highlight.png", "menu_select.png", "spumux_menu.xml", "dvdauthor_menu.xml", "build_dvd.cmd"] {
        assert!(out.join(f).is_file(), "{f} missing");
    }

    let bg = image::open(out.join("menu_bg.png")).unwrap();
    assert_eq!((bg.width(), bg.height()), (720, 576));

    let (w, h, px, palette_len) = read_indexed(&out.join("menu_highlight.png"));
    assert_eq!((w, h), (720, 576));
    assert!(palette_len <= 4, "subpicture allows 4 colours, got {palette_len}");
    // Every button lights up something; nothing outside the button rectangles is set.
    for b in buttons {
        let lit = (b.rect.y0..b.rect.y1)
            .flat_map(|y| (b.rect.x0..b.rect.x1).map(move |x| (x, y)))
            .filter(|&(x, y)| px[(y * w + x) as usize] != 0)
            .count();
        assert!(lit > 50, "button {} has only {lit} highlight pixels", b.name);
    }
    for y in 0..h {
        for x in 0..w {
            if px[(y * w + x) as usize] != 0 {
                assert!(buttons.iter().any(|b| b.rect.contains(x, y)), "stray pixel at {x},{y}");
            }
        }
    }
    let (_, _, sel, _) = read_indexed(&out.join("menu_select.png"));
    assert_eq!(sel, px, "select mask shape must match highlight mask");

    let xml = std::fs::read_to_string(out.join("dvdauthor_menu.xml")).unwrap();
    assert!(xml.contains(r#"<vob file="menu.mpg" pause="inf"/>"#));
    assert!(xml.contains("g1 = 0; jump title 10;"));
    assert_eq!(xml.matches("<vob file=\"title_").count(), 10);

    let spu = std::fs::read_to_string(out.join("spumux_menu.xml")).unwrap();
    assert_eq!(spu.matches("<button ").count(), 11);
    assert!(spu.contains(r#"highlight="menu_highlight.png" select="menu_select.png""#));

    std::fs::remove_dir_all(&out).ok();
}

#[test]
fn paginated_menu() {
    let Some(tpl) = template() else { return };
    let out = out_dir("pages");
    let summary = menu::render_menu_set(&MenuSetOptions {
        template_path: tpl,
        page_index: None,
        titles: titles(25),
        menu_title: None,
        output_dir: &out,
        debug_frames: false,
    })
    .unwrap();

    // 25 titles at 10 per page -> 3 menus.
    assert_eq!(summary.menus.len(), 3);
    let actions = |m: usize| summary.menus[m].buttons.iter().map(|b| b.action.clone()).collect::<Vec<_>>();
    assert!(actions(0).contains(&ButtonAction::Menu(2)));
    assert!(!actions(0).contains(&ButtonAction::Menu(0)));
    assert!(actions(1).contains(&ButtonAction::Menu(1)) && actions(1).contains(&ButtonAction::Menu(3)));
    assert!(actions(1).contains(&ButtonAction::PlayTitle(11)));
    // Last page: titles 21..25, play all, prev.
    assert_eq!(summary.menus[2].buttons.len(), 5 + 2);
    assert!(actions(2).contains(&ButtonAction::PlayTitle(25)));

    for f in ["menu_bg_2.png", "menu_highlight_3.png", "spumux_menu_3.xml"] {
        assert!(out.join(f).is_file(), "{f} missing");
    }
    let xml = std::fs::read_to_string(out.join("dvdauthor_menu.xml")).unwrap();
    // 3 menu pages + one command-only dispatcher per title + 25 title PGCs.
    assert_eq!(xml.matches("<pgc").count(), 3 + 25 + 25);
    // Title 13 is listed on page 2: after playback it returns there.
    assert!(xml.contains("<pre>if (g1 eq 1) jump title 14; jump menu 2;</pre>"));
    assert!(xml.contains(r#"<vob file="menu_3.mpg" pause="inf"/>"#));

    std::fs::remove_dir_all(&out).ok();
}

/// Metallic root menu: icon buttons have their caption below the button rectangle; the
/// highlight must still come from the icon (an empty mask lights up nothing on real players).
#[test]
fn icon_buttons_with_outside_captions_light_up() {
    let tpl = Path::new(r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\Open_Style.ini");
    if !tpl.is_file() {
        eprintln!("skipping: {} not found", tpl.display());
        return;
    }
    let out = out_dir("icons");
    let summary = menu::render_menu_set(&MenuSetOptions {
        template_path: tpl,
        page_index: Some(1),
        titles: titles(3),
        menu_title: None,
        output_dir: &out,
        debug_frames: false,
    })
    .unwrap();
    let buttons = &summary.menus[0].buttons;
    assert!(!buttons.is_empty());
    let (w, _, px, _) = read_indexed(&out.join("menu_highlight.png"));
    for b in buttons {
        let lit = (b.rect.y0..b.rect.y1)
            .flat_map(|y| (b.rect.x0..b.rect.x1).map(move |x| (x, y)))
            .filter(|&(x, y)| px[(y * w + x) as usize] != 0)
            .count();
        assert!(lit > 100, "button {} has only {lit} highlight pixels", b.name);
    }
    std::fs::remove_dir_all(&out).ok();
}
