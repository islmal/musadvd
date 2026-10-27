//! Every installed ConvertX template page must go through the native subpicture muxer without
//! `Unsupported`/`TooComplex`: there is no external spumux to fall back to any more.
//! Skipped (with a message) when the templates are not installed.

use std::path::{Path, PathBuf};

use convertx_rust::author::TitleSpec;
use convertx_rust::menu::{self, MenuSetOptions};
use convertx_rust::spumux;
use convertx_rust::template::Template;

const TEMPLATES: &str = r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates";

fn inis(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            inis(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ini")) {
            out.push(p);
        }
    }
}

#[test]
fn all_template_menus_mux_natively() {
    if !Path::new(TEMPLATES).is_dir() {
        eprintln!("skipping: {TEMPLATES} not found");
        return;
    }
    let mut files = Vec::new();
    inis(Path::new(TEMPLATES), &mut files);
    files.sort();
    let video = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spumux/in_menu.mpg");
    let out = std::env::temp_dir().join(format!("cx_native_menus_{}", std::process::id()));
    let (mut muxed, mut rendered_pages) = (0, 0);
    let mut problems = Vec::new();
    for ini in &files {
        let Ok(template) = Template::load(ini) else { continue };
        for page in 0..template.pages.len() {
            // 1, a full page and several pages: different button counts and group splits.
            for titles in [1usize, 10, 25] {
                let _ = std::fs::remove_dir_all(&out);
                let opts = MenuSetOptions {
                    template_path: ini,
                    page_index: Some(page),
                    titles: (1..=titles).map(TitleSpec::placeholder).collect(),
                    menu_title: Some("Test"),
                    output_dir: &out,
                    debug_frames: false,
                };
                // Pages of other kinds (chapter/audio/subtitle menus) may refuse title input.
                let Ok(summary) = menu::render_menu_set(&opts) else { continue };
                rendered_pages += 1;
                for xml_path in summary.files.iter().filter(|f| f.extension().is_some_and(|x| x == "xml") && f.file_name().is_some_and(|n| n.to_string_lossy().starts_with("spumux_"))) {
                    let text = std::fs::read_to_string(xml_path).unwrap();
                    let label = format!("{} page {page} with {titles} titles: {}", ini.display(), xml_path.file_name().unwrap().to_string_lossy());
                    let defs = match spumux::parse_xml(&text, &out) {
                        Ok(d) => d,
                        Err(e) => {
                            problems.push(format!("{label}: parse: {e}"));
                            continue;
                        }
                    };
                    let mut sink = Vec::new();
                    match spumux::mux(std::fs::File::open(&video).unwrap(), &mut sink, defs, 0) {
                        Ok(_) => muxed += 1,
                        Err(e) => problems.push(format!("{label}: {e}")),
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&out);
    eprintln!("{} templates, {rendered_pages} rendered pages, {muxed} menus muxed natively", files.len());
    assert!(muxed > 0, "no menu was muxed at all");
    assert!(problems.is_empty(), "{} problem(s):\n{}", problems.len(), problems.join("\n"));
}
