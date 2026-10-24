//! CLI subcommands of Burn Studio: argument errors and the paths that need no drive
//! (`build-iso`, `iso-label`, `ifo-layerbreak`, missing images).

use std::path::{Path, PathBuf};
use std::process::Command;

fn cli(args: &[&str]) -> (i32, String, String) {
    // An empty APPDATA: the default settings, never the developer's own settings file.
    let appdata = std::env::temp_dir().join(format!("cx_studio_cli_appdata_{}", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_convertx-rust")).args(args).env("APPDATA", appdata).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cx_studio_cli_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A folder with two files in two directories.
fn data_folder(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("Unter")).unwrap();
    std::fs::write(src.join("a.txt"), "hallo").unwrap();
    std::fs::write(src.join("Unter").join("b.bin"), vec![7u8; 5000]).unwrap();
    src
}

#[test]
fn argument_errors() {
    // Missing required arguments are clap errors (exit 2).
    for args in [&["read-disc"][..], &["build-iso"], &["build-iso", "x"], &["iso-label"], &["ifo-layerbreak"]] {
        let (code, _, err) = cli(args);
        assert_eq!(code, 2, "{args:?}: {err}");
    }
    let (code, _, err) = cli(&["build-iso", "x", "--output", "y.iso", "--as-disc", "--as-data"]);
    assert_eq!(code, 2, "{err}");
}

#[test]
fn missing_inputs_fail_before_any_drive_is_touched() {
    let dir = temp("missing");
    let none = dir.join("gibt_es_nicht");
    let (code, _, err) = cli(&["build-iso", s(&none), "--output", s(&dir.join("x.iso"))]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("nicht gefunden"), "{err}");
    let (code, _, err) = cli(&["verify-disc", s(&none)]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("Image nicht gefunden"), "{err}");
    let (code, _, err) = cli(&["iso-label", s(&none)]);
    assert_eq!(code, 1, "{err}");
    let (code, _, err) = cli(&["ifo-layerbreak", s(&none)]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("nicht gefunden"), "{err}");
}

#[test]
fn build_label_and_inspect_an_image() {
    let dir = temp("build");
    let src = data_folder(&dir);
    let iso = dir.join("daten.iso");
    let (code, out, err) = cli(&["build-iso", s(&src), "--output", s(&iso), "--label", "mein_test"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Daten") && out.contains("MEIN_TEST"), "{out}");
    assert!(iso.is_file() && std::fs::metadata(&iso).unwrap().len() >= 600 * 2048, "min_sectors");

    let (code, out, err) = cli(&["iso-label", s(&iso)]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "MEIN_TEST");
    let (code, out, err) = cli(&["iso-label", s(&iso), "--set", "NEUES_LABEL"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("MEIN_TEST") && out.contains("NEUES_LABEL"), "{out}");
    let (_, out, _) = cli(&["iso-label", s(&iso)]);
    assert_eq!(out.trim(), "NEUES_LABEL");

    // A data image has no VIDEO_TS: the inspector says so instead of crashing.
    let (code, _, err) = cli(&["ifo-layerbreak", s(&iso)]);
    assert_eq!(code, 1, "{err}");
}

#[test]
fn dvd_folder_needs_an_answer_and_the_flags_give_it() {
    let dir = temp("dvd");
    let video_ts = dir.join("src").join("VIDEO_TS");
    std::fs::create_dir_all(&video_ts).unwrap();
    std::fs::write(video_ts.join("VIDEO_TS.IFO"), vec![1u8; 3000]).unwrap();
    std::fs::write(video_ts.join("VIDEO_TS.BUP"), vec![1u8; 3000]).unwrap();
    std::fs::write(video_ts.join("VIDEO_TS.VOB"), vec![2u8; 5000]).unwrap();
    let src = dir.join("src");
    let iso = dir.join("dvd.iso");
    // The default setting asks; a command line cannot, so it names the flags.
    let (code, _, err) = cli(&["build-iso", s(&src), "--output", s(&iso)]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("--as-disc") && err.contains("--as-data"), "{err}");
    assert!(!iso.exists());
    let (code, out, err) = cli(&["build-iso", s(&src), "--output", s(&iso), "--as-disc"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("DVD-Video"), "{out}");
    let data = dir.join("data.iso");
    let (code, out, err) = cli(&["build-iso", s(&src), "--output", s(&data), "--as-data"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Daten"), "{out}");
}
