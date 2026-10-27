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

/// Every real template becomes a valid Blu-ray menu set that renders (PNG layers, IG object budget).
#[test]
fn all_templates_become_valid_blu_ray_menus() {
    use convertx_rust::author::{ClipSource, MenuFrame, PlaybackOptions};
    use convertx_rust::bd::BdRate;
    use convertx_rust::bd_menu;
    use convertx_rust::bd_template::layout_from_template;
    use convertx_rust::encoder::{Aspect, MenuLayer};
    use convertx_rust::layout::{validate_bd, LayoutTitle};

    let root = Path::new(r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates");
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let titles: Vec<LayoutTitle> = (1..=3)
        .map(|n| LayoutTitle {
            label: format!("Titel {n}"),
            chapters: 7,
            duration_secs: 600.0,
            clip: ClipSource { path: "a.mkv".into(), start_secs: 10.0, has_audio: true },
            audio_sources: vec![1, 2],
            subtitle_sources: vec![3, 4],
            audio_labels: vec![],
            subtitle_labels: vec![],
        })
        .collect();
    let frame = MenuFrame::square(1920, 1080);
    let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
    let out = std::env::temp_dir().join(format!("bd-real-templates-{}", std::process::id()));
    let mut count = 0;
    for style in std::fs::read_dir(root).unwrap().flatten().filter(|e| e.path().is_dir()) {
        for ini in std::fs::read_dir(style.path()).unwrap().flatten().map(|e| e.path()) {
            if !ini.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")) {
                continue;
            }
            let name = ini.display().to_string();
            let Ok(template) = Template::load(&ini) else { continue };
            if template.pages.is_empty() {
                continue; // "no menu"
            }
            // Rendering 1080p layers is slow in debug builds: the pillarbox variant only runs once.
            let aspects: &[Aspect] = if count == 0 { &[Aspect::Standard] } else { &[Aspect::Wide] };
            for &aspect in aspects {
                let (layout, _) = layout_from_template(&template, &ini, &titles, &vec![None; titles.len()], &vec![vec![5.0, 50.0]; titles.len()], aspect)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                validate_bd(&layout, &titles, frame).unwrap_or_else(|e| panic!("{name}: {e}"));
                let nav = bd_menu::plan(&layout, &titles, &bd_menu::traits(&layout, &titles, frame), PlaybackOptions::default(), &titles.iter().map(|t| vec![t.chapters]).collect::<Vec<_>>());
                let dir = out.join(format!("{count}"));
                std::fs::create_dir_all(&dir).unwrap();
                let pages = bd_menu::render_pages(&layout, &titles, frame, rate, &nav, &dir, 10.0)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(pages.len(), layout.menus.len(), "{name}");
                for layer in pages.iter().flat_map(|p| &p.media.layers) {
                    if let MenuLayer::Image(png) = layer {
                        assert!(png.is_file(), "{name}: missing {}", png.display());
                    }
                }
                count += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&out);
    assert!(count > 0, "no templates found below {}", root.display());
}

/// Intro keys of the real Open_Style page: the video stamp of block 2 zooms in from its centre.
#[test]
fn open_style_block_two_zooms_in_during_the_intro() {
    use convertx_rust::template::AnimKey;
    let Some(dir) = template_dir() else { return };
    let t = Template::load(dir.join("Open_Style.ini")).unwrap();
    let g = t.pages[0].graphics.iter().find(|g| g.block == 2).expect("block 2");
    assert_eq!(
        g.intro,
        [
            AnimKey { t: 1.0, x: 84, y: 133, w: 0, h: 0, alpha: None },
            AnimKey { t: 2.0, x: 50, y: 106, w: 68, h: 54, alpha: None },
        ]
    );
    assert_eq!(g.outro.len(), 3);
    assert_eq!(g.block_rect, (50, 106, 68, 54));
    // Every template parses with sorted keys.
    for entry in std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "ini")) {
        let t = Template::load(entry.path()).unwrap();
        for g in t.pages.iter().flat_map(|p| &p.graphics) {
            assert!(g.intro.windows(2).all(|w| w[0].t <= w[1].t), "{}", entry.path().display());
        }
    }
}
