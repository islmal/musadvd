//! Pixel-exact goldens of the native text subtitle renderer. The font is bundled, so the pictures
//! are the same on every machine. Regenerate deliberately with `UPDATE_GOLDEN=1`.

use std::path::{Path, PathBuf};

use musadvd::author::TvStandard;
use musadvd::textsub::render_cue;

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/textsub").join(format!("{name}.png"))
}

fn check(name: &str, tv: TvStandard, lines: &[&str]) {
    let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let (img, x, y) = render_cue(tv, &lines).unwrap();
    // The position is part of the golden: it is stored in the file name's twin `.pos`.
    let pos = format!("{x} {y}");
    let (png, pos_file) = (golden(name), golden(name).with_extension("pos"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(png.parent().unwrap()).unwrap();
        img.save(&png).unwrap();
        std::fs::write(&pos_file, &pos).unwrap();
        return;
    }
    let expected = image::open(&png).unwrap_or_else(|e| panic!("{}: {e}", png.display())).to_rgba8();
    assert_eq!(std::fs::read_to_string(&pos_file).unwrap().trim(), pos, "{name}: position");
    assert_eq!(expected.dimensions(), img.dimensions(), "{name}: size");
    assert!(expected.as_raw() == img.as_raw(), "{name}: pixels differ from the golden");
}

#[test]
fn single_line_pal() {
    check("single_pal", TvStandard::Pal, &["Hallo Welt gjpq"]);
}

#[test]
fn two_lines_ntsc() {
    check("two_ntsc", TvStandard::Ntsc, &["Zeile eins", "Zeile zwei"]);
}

#[test]
fn wrapped_long_cue_with_umlauts_and_cyrillic() {
    check(
        "wrapped_pal",
        TvStandard::Pal,
        &["Ein sehr langer Satz mit Umlauten äöüß, der garantiert umgebrochen werden muss, weil er viel zu breit für den Bildschirm ist", "Привет, мир!"],
    );
}
