//! The native subpicture multiplexer (`spumux.rs`) must reproduce `spumux -m dvd` byte for byte
//! on bitmap subtitle streams. Skipped (with a message) when ffmpeg/spumux are missing.

use std::path::Path;
use std::process::{Command, Stdio};

use convertx_rust::menu::IndexedImage;
use convertx_rust::spumux;
use convertx_rust::tools::Toolchain;

/// A subtitle-like bitmap: `w`×`h`, text-ish stripes in two opaque colours on transparency.
fn image(w: u32, h: u32, seed: u32, palette: [[u8; 4]; 4]) -> IndexedImage {
    let mut img = IndexedImage::new(w, h, palette);
    for y in 0..h {
        for x in 0..w {
            let v = if (x + y * 3 + seed) % 11 < 3 && y > 1 && y < h - 1 && x > 2 && x < w - 2 {
                1 + ((x / 5 + y + seed) % 3) as u8
            } else if y > h / 2 && x > w / 3 && x < w / 3 + 80 {
                1
            } else {
                0
            };
            img.pixels[(y * w + x) as usize] = v;
        }
    }
    img
}

/// Mostly `heavy`, a little of the other two colours: separates "most frequent first" from
/// "brightest first" in spumux's pixel value assignment.
fn skewed(w: u32, h: u32, heavy: u8, palette: [[u8; 4]; 4]) -> IndexedImage {
    let mut img = IndexedImage::new(w, h, palette);
    for y in 2..h - 2 {
        for x in 4..w - 4 {
            let n = (x * 7 + y * 13) % 20;
            img.pixels[(y * w + x) as usize] = match n {
                0 => 1 + (heavy % 3),
                1 | 2 => 1 + ((heavy + 1) % 3),
                _ => heavy,
            };
        }
    }
    img
}

fn first_diff(a: &[u8], b: &[u8]) -> String {
    let n = a.len().min(b.len());
    match (0..n).find(|&i| a[i] != b[i]) {
        Some(i) => format!("first difference at byte {i} (sector {}, offset {:#x}): spumux {:02x} native {:02x}", i / 2048, i % 2048, a[i], b[i]),
        None => format!("equal prefix, lengths spumux {} native {}", a.len(), b.len()),
    }
}

fn run_case(name: &str, tools: &Toolchain, entries: &[(&str, Option<&str>, bool)], secs: u32) {
    let dir = std::env::temp_dir().join(format!("cx_spumux_native_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("s")).unwrap();
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=s=720x576:r=25", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"])
        .args(["-t", &secs.to_string(), "-target", "pal-dvd", "-b:v", "4000k", "-c:a", "ac3", "-b:a", "192k", "in.mpg"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(st.success());

    let pal = [[0, 0, 0, 0], [255, 255, 255, 255], [10, 10, 10, 255], [250, 200, 20, 255]];
    let pal_half = [[0, 0, 0, 0], [240, 240, 240, 255], [60, 60, 200, 128], [0, 0, 0, 255]];
    let mut xml = String::from("<subpictures format=\"PAL\"><stream>\n");
    for (i, (start, end, force)) in entries.iter().enumerate() {
        let img = image(300 + 37 * i as u32, 40 + 6 * i as u32, i as u32, if i % 2 == 0 { pal } else { pal_half });
        let name = format!("s/{i:05}.png");
        img.write_png(&dir.join(&name)).unwrap();
        xml.push_str(&format!(
            "<spu start=\"{start}\"{} image=\"{name}\" xoffset=\"{}\" yoffset=\"{}\"{}/>\n",
            end.map_or(String::new(), |e| format!(" end=\"{e}\"")),
            60 + 2 * i,
            400 - 2 * i,
            if *force { " force=\"yes\"" } else { "" }
        ));
    }
    xml.push_str("</stream></subpictures>\n");
    std::fs::write(dir.join("sub.xml"), &xml).unwrap();

    let out = Command::new(&tools.spumux)
        .args(["-s", "1", "-m", "dvd", "sub.xml"])
        .current_dir(&dir)
        .stdin(std::fs::File::open(dir.join("in.mpg")).unwrap())
        .stdout(std::fs::File::create(dir.join("ref.mpg")).unwrap())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let defs = spumux::parse_xml(&xml, &dir).unwrap();
    let mut native = Vec::new();
    let stats = spumux::mux(std::fs::File::open(dir.join("in.mpg")).unwrap(), &mut native, defs, 1).unwrap();
    let reference = std::fs::read(dir.join("ref.mpg")).unwrap();
    assert!(native == reference, "{name}: {} ({stats:?})\n{}", first_diff(&reference, &native), String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

fn tools() -> Option<Toolchain> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(Toolchain::detect()) {
        Ok(t) if Path::new(&t.spumux).is_file() => Some(t),
        other => {
            eprintln!("skipping: {:?}", other.err());
            None
        }
    }
}

#[test]
fn single_subtitle_with_end() {
    let Some(t) = tools() else { return };
    run_case("single", &t, &[("00:00:01.00", Some("00:00:02.50"), false)], 4);
}

/// Runs a set of images (`start`, `end`, x, y) through both muxers and compares.
fn run_set(name: &str, tools: &Toolchain, items: &[(IndexedImage, &str, Option<&str>, u32, u32)], secs: u32) {
    let dir = std::env::temp_dir().join(format!("cx_spumux_native_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=s=720x576:r=25", "-t", &secs.to_string(), "-target", "pal-dvd", "-an", "in.mpg"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(st.success());
    let mut xml = String::from("<subpictures format=\"PAL\"><stream>\n");
    for (i, (img, start, end, x, y)) in items.iter().enumerate() {
        img.write_png(&dir.join(format!("{i}.png"))).unwrap();
        xml.push_str(&format!(
            "<spu start=\"{start}\"{} image=\"{i}.png\" xoffset=\"{x}\" yoffset=\"{y}\"/>\n",
            end.map_or(String::new(), |e| format!(" end=\"{e}\""))
        ));
    }
    xml.push_str("</stream></subpictures>\n");
    std::fs::write(dir.join("sub.xml"), &xml).unwrap();
    let out = Command::new(&tools.spumux)
        .args(["-s", "0", "-m", "dvd", "sub.xml"])
        .current_dir(&dir)
        .stdin(std::fs::File::open(dir.join("in.mpg")).unwrap())
        .stdout(std::fs::File::create(dir.join("ref.mpg")).unwrap())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut native = Vec::new();
    spumux::mux(std::fs::File::open(dir.join("in.mpg")).unwrap(), &mut native, spumux::parse_xml(&xml, &dir).unwrap(), 0).unwrap();
    let reference = std::fs::read(dir.join("ref.mpg")).unwrap();
    assert!(native == reference, "{name}: {}\n{}", first_diff(&reference, &native), String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_image(name: &str, tools: &Toolchain, img: &IndexedImage) {
    run_set(name, tools, &[(img.clone(), "00:00:01.00", Some("00:00:02.00"), 100, 300)], 3);
}

/// Pseudo-random pixels (no long runs) in a transparent frame of odd width.
fn noisy(w: u32, h: u32, border: u32, palette: [[u8; 4]; 4]) -> IndexedImage {
    let mut img = IndexedImage::new(w, h, palette);
    let mut s = 12345u32;
    for y in border..h - border {
        for x in border..w - border {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            img.pixels[(y * w + x) as usize] = ((s >> 24) % 4) as u8;
        }
    }
    img
}

/// A button description: name, rectangle, explicit up/down/left/right targets.
type Btn = (&'static str, [u32; 4], [Option<&'static str>; 4]);

/// Menu mask in the style of `menu.rs`: 720×576, `inside` pixel values in every button rectangle
/// (a stripe pattern of 0/1/2/3), everything else background.
fn menu_mask(palette: [[u8; 4]; 4], buttons: &[Btn], values: impl Fn(usize, u32, u32) -> u8) -> IndexedImage {
    let mut img = IndexedImage::new(720, 576, palette);
    for (i, (_, r, _)) in buttons.iter().enumerate() {
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                img.pixels[(y * 720 + x) as usize] = values(i, x - r[0], y - r[1]);
            }
        }
    }
    img
}

/// Runs a menu (highlight + select masks, buttons) through both muxers and compares.
fn run_menu(name: &str, tools: &Toolchain, hl: &IndexedImage, sel: &IndexedImage, buttons: &[Btn], force: bool) {
    let dir = std::env::temp_dir().join(format!("cx_spumux_native_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A still menu like the real pipeline's: one second of video with a NAV pack per VOBU.
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=gray:s=720x576:r=25", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"])
        .args(["-t", "1", "-target", "pal-dvd", "-c:a", "ac3", "-b:a", "192k", "in.mpg"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(st.success());
    hl.write_png(&dir.join("hl.png")).unwrap();
    sel.write_png(&dir.join("sel.png")).unwrap();
    let mut xml = format!(
        "<subpictures format=\"PAL\">\n  <stream>\n    <spu start=\"00:00:00.000\"{} highlight=\"hl.png\" select=\"sel.png\">\n",
        if force { " force=\"yes\"" } else { "" }
    );
    for (n, r, nav) in buttons {
        xml.push_str(&format!("      <button name=\"{n}\" x0=\"{}\" y0=\"{}\" x1=\"{}\" y1=\"{}\"", r[0], r[1], r[2], r[3]));
        for (attr, t) in ["up", "down", "left", "right"].iter().zip(nav) {
            if let Some(t) = t {
                xml.push_str(&format!(" {attr}=\"{t}\""));
            }
        }
        xml.push_str("/>\n");
    }
    xml.push_str("    </spu>\n  </stream>\n</subpictures>\n");
    std::fs::write(dir.join("m.xml"), &xml).unwrap();
    let out = Command::new(&tools.spumux)
        .args(["-s", "0", "-m", "dvd", "m.xml"])
        .current_dir(&dir)
        .stdin(std::fs::File::open(dir.join("in.mpg")).unwrap())
        .stdout(std::fs::File::create(dir.join("ref.mpg")).unwrap())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut native = Vec::new();
    let stats = spumux::mux(std::fs::File::open(dir.join("in.mpg")).unwrap(), &mut native, spumux::parse_xml(&xml, &dir).unwrap(), 0).unwrap();
    let reference = std::fs::read(dir.join("ref.mpg")).unwrap();
    assert!(native == reference, "{name}: {} ({stats:?})\n{}", first_diff(&reference, &native), String::from_utf8_lossy(&out.stderr));
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    eprintln!("{name}: {}", log.lines().find(|l| l.contains("Pickbuttongroups")).unwrap_or("-"));
    let _ = std::fs::remove_dir_all(&dir);
}

const SELECTION: [[u8; 4]; 4] = [[0, 0, 0, 0], [255, 255, 0, 200], [255, 255, 0, 150], [255, 255, 0, 80]];
const ACTION: [[u8; 4]; 4] = [[0, 0, 0, 0], [255, 128, 0, 200], [255, 128, 0, 150], [255, 128, 0, 80]];

/// Text-like highlight pixels inside a rectangle: pattern with an emphasis outline.
fn caption(_button: usize, x: u32, y: u32) -> u8 {
    match (x / 3 + y / 2) % 5 {
        0 | 1 => 1,
        2 => 2,
        _ => 0,
    }
}

#[test]
fn menu_default_navigation() {
    let Some(t) = tools() else { return };
    // 2×3 grid, all navigation left to spumux's geometry search.
    let b: Vec<Btn> = (0..6)
        .map(|i| {
            let (c, r) = (i % 2, i / 2);
            let name: &'static str = ["b1", "b2", "b3", "b4", "b5", "b6"][i as usize];
            (name, [80 + c * 300, 100 + r * 120, 340 + c * 300, 180 + r * 120], [None; 4])
        })
        .collect();
    let (hl, sel) = (menu_mask(SELECTION, &b, caption), menu_mask(ACTION, &b, caption));
    run_menu("menu_nav_default", &t, &hl, &sel, &b, true);
}

#[test]
fn menu_explicit_navigation_like_the_authoring_step() {
    let Some(t) = tools() else { return };
    let b: Vec<Btn> = vec![
        ("b1", [100, 100, 300, 160], [None, Some("b2"), None, Some("b3")]),
        ("b2", [100, 300, 300, 360], [Some("b1"), None, None, Some("b4")]),
        ("b3", [400, 100, 600, 160], [None, Some("b4"), Some("b1"), None]),
        ("b4", [400, 300, 600, 360], [Some("b3"), None, Some("b2"), None]),
    ];
    let (hl, sel) = (menu_mask(SELECTION, &b, caption), menu_mask(ACTION, &b, caption));
    run_menu("menu_nav_explicit", &t, &hl, &sel, &b, true);
}

#[test]
fn menu_opaque_palettes_and_many_buttons() {
    let Some(t) = tools() else { return };
    // No transparent palette entry: spumux has to invent a transparent slot.
    let opaque_a = [[0, 0, 0, 255], [255, 255, 255, 255], [200, 0, 0, 255], [0, 0, 200, 255]];
    let opaque_b = [[0, 0, 0, 255], [0, 255, 0, 255], [200, 200, 0, 255], [0, 200, 200, 255]];
    let b: Vec<Btn> = (0..10)
        .map(|i| {
            let name: &'static str = ["b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10"][i as usize];
            (name, [60, 40 + i * 50, 660, 80 + i * 50], [None; 4])
        })
        .collect();
    let (hl, sel) = (menu_mask(opaque_a, &b, caption), menu_mask(opaque_b, &b, caption));
    run_menu("menu_opaque", &t, &hl, &sel, &b, false);
}

#[test]
fn menu_buttons_that_need_several_groups() {
    let Some(t) = tools() else { return };
    // Highlight and select use different value layouts per button, so one group would need more
    // than four (highlight, select) combinations.
    let b: Vec<Btn> = vec![
        ("b1", [100, 100, 300, 160], [None; 4]),
        ("b2", [100, 300, 300, 360], [None; 4]),
        ("b3", [400, 100, 600, 160], [None; 4]),
    ];
    // Button 1 pairs (0,0) (1,1) (2,2); button 2 pairs (0,0) (1,2) (2,1); button 3 (0,0) (3,3).
    let hl = menu_mask(SELECTION, &b, |i, x, y| if i == 2 { 3 * (caption(0, x, y) & 1) } else { caption(0, x, y) });
    let sel = menu_mask(ACTION, &b, |i, x, y| {
        let v = caption(0, x, y);
        match i {
            1 => [0, 2, 1][v as usize],
            2 => 3 * (v & 1),
            _ => v,
        }
    });
    run_menu("menu_groups", &t, &hl, &sel, &b, true);
}

#[test]
fn pixel_value_assignment() {
    let Some(t) = tools() else { return };
    let (dark, white, gray) = ([10, 10, 10, 255], [255, 255, 255, 255], [128, 128, 128, 255]);
    // The most frequent colour is the darkest / the brightest / the middle one.
    run_image("dark_heavy", &t, &skewed(200, 40, 1, [[0, 0, 0, 0], dark, white, gray]));
    run_image("white_heavy", &t, &skewed(200, 40, 2, [[0, 0, 0, 0], dark, white, gray]));
    run_image("gray_heavy", &t, &skewed(200, 40, 3, [[0, 0, 0, 0], dark, white, gray]));
}

#[test]
fn crop_odd_sizes_and_multi_sector_spus() {
    let Some(t) = tools() else { return };
    let pal = [[0, 0, 0, 0], [255, 255, 255, 255], [10, 10, 10, 255], [250, 200, 20, 128]];
    // Transparent frame (odd offsets), odd size, first raster pixel transparent.
    run_image("crop", &t, &noisy(301, 57, 5, pal));
    // Several sectors per SPU, back to back: PES header 12 -> 4 -> 9 transitions.
    run_set(
        "multi",
        &t,
        &[
            (noisy(200, 100, 3, pal), "00:00:00.80", Some("00:00:01.80"), 10, 100),
            (noisy(180, 90, 2, pal), "00:00:02.00", Some("00:00:03.00"), 40, 200),
            (noisy(120, 30, 1, pal), "00:00:03.50", None, 40, 200),
        ],
        6,
    );
}

#[test]
fn single_colour_and_opaque_first_pixel() {
    let Some(t) = tools() else { return };
    let mut solid = IndexedImage::new(64, 20, [[0, 0, 0, 0], [255, 0, 0, 255], [0, 0, 0, 0], [0, 0, 0, 0]]);
    solid.pixels.fill(1);
    run_image("solid", &t, &solid);
    let mut corner = noisy(100, 20, 0, [[200, 200, 0, 255], [0, 0, 0, 0], [30, 30, 30, 255], [250, 250, 250, 255]]);
    corner.pixels[0] = 0;
    run_image("opaque_first", &t, &corner);
}

#[test]
fn several_subtitles_overlap_and_open_end() {
    let Some(t) = tools() else { return };
    run_case(
        "several",
        &t,
        &[
            ("00:00:00.50", Some("00:00:01.20"), false),
            ("00:00:01.20", Some("00:00:02.00"), true),
            ("00:00:02.50", Some("00:00:04.50"), false),
            // Overlaps the next one: cut off before it.
            ("00:00:03.80", Some("00:00:05.50"), false),
            ("00:00:04.00", None, false),
        ],
        7,
    );
}
