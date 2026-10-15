//! Integration tests against the real ConvertXToDVD 7 templates installed on this machine.
//! Skipped (with a message) when the templates are not installed.

use std::path::{Path, PathBuf};

use convertx_rust::template::{IniDocument, Template};

const TEMPLATE_DIR: &str = r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style";

fn template_dir() -> Option<PathBuf> {
    let dir = Path::new(TEMPLATE_DIR);
    if dir.is_dir() {
        Some(dir.to_path_buf())
    } else {
        eprintln!("skipping: {TEMPLATE_DIR} not found");
        None
    }
}

#[test]
fn parses_10_titles_per_page_no_root() {
    let Some(dir) = template_dir() else { return };
    let path = dir.join("10_TitlesPerPageNoRoot.ini");

    // File really carries a UTF-8 BOM; the parser must cope with it.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..3], b"\xEF\xBB\xBF");

    let t = Template::load(&path).unwrap();
    assert_eq!(t.template_version, "5.2.0.12");
    assert_eq!(t.kind, "mtOpenStyle");
    assert_eq!(t.titles_per_page, Some(10));
    assert_eq!(t.chapters_per_page, Some(6));
    assert_eq!(t.thumbnail.as_deref(), Some("10_TitlesPerPageNoRoot.jpg"));

    assert_eq!(t.pages.len(), 1);
    let page = &t.pages[0];
    assert_eq!(page.style, "MNUS_TITLE");
    assert_eq!((page.width, page.height), (720, 576));
    assert_eq!((page.intro_duration, page.main_duration, page.outro_duration), (3.0, 25.0, 2.0));
    assert_eq!(page.backgrounds, vec![r"..\Resources\Backgrounds\bg_OpenStyle.jpg".to_string()]);

    // 10 title entries + navigation buttons, Count=13 in the file.
    assert_eq!(page.buttons.len(), 13);
    let first = &page.buttons[0];
    assert_eq!(first.name, "ch#1");
    assert_eq!((first.x, first.y, first.width, first.height), (52, 108, 616, 26));
    assert_eq!(first.location, "ML_PLAY_EPISODE");
    assert_eq!(first.color_scheme, 1);
    assert_eq!(
        page.buttons.iter().filter(|b| b.location == "ML_PLAY_EPISODE").count(),
        10
    );
    let next = page.buttons.iter().find(|b| b.location == "ML_NEXT").unwrap();
    assert_eq!((next.x, next.y, next.width, next.height), (587, 475, 80, 76));

    // Every button lies inside the page and references an existing color scheme.
    for b in &page.buttons {
        assert!(b.x >= 0 && b.y >= 0, "{b:?}");
        assert!(b.x as u32 + b.width <= page.width, "{b:?}");
        assert!(b.y as u32 + b.height <= page.height, "{b:?}");
        assert!(b.color_scheme as usize <= page.color_schemes.len(), "{b:?}");
    }

    assert_eq!(page.color_schemes.len(), 1);
    let sel_pattern = page.color_schemes[0].selection[1].as_ref().unwrap();
    assert_eq!(sel_pattern.rgb, Some((0xCC, 0xFF, 0x00)));
    assert_eq!(sel_pattern.alpha, 200);

    // Comma decimals are present in the raw file (e.g. TimeStamp=2,999).
    let ini = IniDocument::load(&path).unwrap();
    assert!(ini
        .sections()
        .flat_map(|s| s.entries())
        .any(|(k, v)| k == "TimeStamp" && v == "2,999"));
}

#[test]
fn all_open_style_templates_parse() {
    let Some(dir) = template_dir() else { return };
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")) {
            continue;
        }
        let t = Template::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!t.pages.is_empty(), "{}: no pages", path.display());
        for p in &t.pages {
            assert!(p.width > 0 && p.height > 0, "{}: page {}", path.display(), p.index);
        }
        count += 1;
    }
    assert!(count > 0, "no .ini templates found in {}", dir.display());
}
