//! Cancelling a running build: tools are killed, the work directory is removed, nothing is
//! left behind. Skipped (with a message) when tools or templates are missing.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use convertx_rust::dvd::{self, BuildError, BuildOptions, BuildPhase, CancelToken, Event};
use convertx_rust::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

/// Running processes with this image name (Windows `tasklist`).
fn process_count(image: &str) -> usize {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains(image)).count()
}

#[test]
fn cancel_during_encoding() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = match rt.block_on(Toolchain::detect()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    if !Path::new(TEMPLATE).is_file() {
        eprintln!("skipping: {TEMPLATE} not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cx_cancel_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Long enough that encoding is still running when we cancel (2-pass, 3 titles).
    let clip = dir.join("long.mp4");
    let st = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=25:duration=90"])
        .args(["-f", "lavfi", "-i", "sine=f=440:duration=90", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(st.success());

    let ffmpeg_image = tools.ffmpeg.file_name().unwrap().to_string_lossy().into_owned();
    let before = process_count(&ffmpeg_image);
    let cancel = CancelToken::new();
    let out = dir.join("out");
    let opts = BuildOptions {
        inputs: vec![clip.clone(), clip.clone(), clip],
        template: TEMPLATE.into(),
        output: out.clone(),
        two_pass: true,
        cancel: cancel.clone(),
        ..BuildOptions::default()
    };

    // Cancel as soon as encoding has started and ffmpeg is busy.
    let mut cancelled_at: Option<Instant> = None;
    let mut encoding_running = false;
    let token = cancel.clone();
    let result = rt.block_on(dvd::build_dvd(&opts, &mut |e| match e {
        Event::Phase(BuildPhase::Encoding { index: 0, .. }) => encoding_running = true,
        Event::Progress { label, fraction } if encoding_running && label.starts_with("Encoding") && fraction > 0.01 => {
            if cancelled_at.is_none() {
                cancelled_at = Some(Instant::now());
                token.cancel();
            }
        }
        _ => {}
    }));

    let cancelled_at = cancelled_at.expect("encoding progress was reported before the end");
    assert!(matches!(result, Err(BuildError::Cancelled)), "{result:?}");
    assert!(cancelled_at.elapsed() < Duration::from_secs(10), "cancel took {:?}", cancelled_at.elapsed());

    // Nothing left behind: no work directory, no project folder, no running ffmpeg.
    assert!(!out.join(".convertx-work").exists(), "work directory removed");
    let leftovers: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(process_count(&ffmpeg_image) <= before, "ffmpeg processes were killed");
    std::fs::remove_dir_all(&dir).ok();
}
