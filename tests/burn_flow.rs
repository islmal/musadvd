//! The burn flow shared by GUI and CLI (`burner::burn_video_ts`) against a fake burner: image
//! creation, layer break hand-over, image clean-up, cancel and error paths, remaining time.
//! Plus the CLI `burn` error paths, which stop before anything is written.
//! Builds a small real DVD once; skipped (with a message) when tools/templates are missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use convertx_rust::burner::layerbreak::{plan_disc, Capacities};
use convertx_rust::burner::udf::plan_image;
use convertx_rust::burner::udf250;
use convertx_rust::burner::{
    burn_video_ts, BurnError, BurnJob, BurnOptions, BurnPhase, BurnProgress, BurnUpdate, DriveInfo, MediaInfo,
    OpticalBurner, WriteSpeed,
};
use convertx_rust::dvd::{self, BuildOptions};
use convertx_rust::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

/// One small authored DVD shared by all tests (read-only for them).
fn fixture() -> Option<&'static Path> {
    static DVD: OnceLock<Option<PathBuf>> = OnceLock::new();
    DVD.get_or_init(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tools = match rt.block_on(Toolchain::detect()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skipping: {e}");
                return None;
            }
        };
        if !Path::new(TEMPLATE).is_file() {
            eprintln!("skipping: {TEMPLATE} not found");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("cx_burnflow_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let inputs: Vec<PathBuf> = (1..=2)
            .map(|n| {
                let clip = dir.join(format!("clip{n}.mkv"));
                let st = Command::new(&tools.ffmpeg)
                    .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=720x576:rate=25:duration=4"])
                    .args(["-f", "lavfi", "-i", &format!("sine=f={}:duration=4", 400 * n)])
                    .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
                    .arg(&clip)
                    .status()
                    .unwrap();
                assert!(st.success());
                clip
            })
            .collect();
        let opts = BuildOptions {
            inputs,
            template: TEMPLATE.into(),
            output: dir.join("out"),
            project_name: Some("Brenntest".into()),
            chapters: dvd::ChapterSettings { interval_secs: 1.0, ..Default::default() },
            ..BuildOptions::default()
        };
        let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
        Some(report.video_ts)
    })
    .as_deref()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    /// Fails in the middle of writing.
    Fail,
    /// Writes until the cancel flag is set.
    UntilCancelled,
}

/// What the burner was asked to do.
#[derive(Debug, Clone)]
struct Seen {
    speed: WriteSpeed,
    layer_break: Option<u32>,
    verify: bool,
    eject: bool,
    erase: bool,
    iso_size: u64,
}

struct FakeBurner {
    mode: Mode,
    seen: Mutex<Option<Seen>>,
}

impl FakeBurner {
    fn new(mode: Mode) -> Self {
        FakeBurner { mode, seen: Mutex::new(None) }
    }

    fn seen(&self) -> Option<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl OpticalBurner for FakeBurner {
    fn list_drives(&self) -> Result<Vec<DriveInfo>, BurnError> {
        Ok(vec![DriveInfo { id: "fake".into(), name: "Fake Writer (X:)".into(), path: r"X:\".into() }])
    }

    fn probe_media(&self, _drive_id: &str) -> Result<MediaInfo, BurnError> {
        Ok(MediaInfo::none())
    }

    fn burn_iso(
        &self,
        drive_id: &str,
        iso_path: &Path,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        assert_eq!(drive_id, "fake");
        *self.seen.lock().unwrap() = Some(Seen {
            speed,
            layer_break: options.layer_break,
            verify: options.verify,
            eject: options.eject,
            erase: options.erase,
            iso_size: std::fs::metadata(iso_path)?.len(),
        });
        let send = |phase, f| {
            let _ = tx.send(BurnProgress::new(phase, f, format!("{phase:?}")));
        };
        send(BurnPhase::Preparing, 0.0);
        send(BurnPhase::Formatting, 0.0);
        let mut step = 0;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(BurnError::Cancelled);
            }
            let f = (step as f64 / 10.0).min(1.0);
            send(BurnPhase::Writing, f);
            match self.mode {
                Mode::Fail if step == 5 => return Err(BurnError::Backend("Schreibfehler".into())),
                Mode::UntilCancelled => {
                    assert!(step < 500, "cancel never arrived");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ if f >= 1.0 => break,
                _ => {}
            }
            step += 1;
        }
        send(BurnPhase::Finalizing, 0.0);
        if options.verify {
            send(BurnPhase::Verifying, 0.0);
            send(BurnPhase::Verifying, 1.0);
        }
        send(BurnPhase::Done, 1.0);
        Ok(())
    }
}

fn job(video_ts: &Path, iso: PathBuf) -> BurnJob {
    BurnJob {
        video_ts: video_ts.to_path_buf(),
        drive_id: "fake".into(),
        iso,
        label: "Brenntest".into(),
        speed: WriteSpeed::Specific(5410),
        media_speeds: vec![10_820, 5_410, 2_705],
        options: BurnOptions { layer_break: None, verify: true, eject: true, erase: false },
        keep_iso: false,
        capacities: Capacities::DVD,
    }
}

/// A fresh directory for one test's image.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cx_burnflow_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn no_image_left(iso: &Path) {
    assert!(!iso.exists(), "{} left behind", iso.display());
    assert!(!iso.with_extension("iso.part").exists(), "partial image left behind");
}

#[test]
fn burns_and_removes_the_image() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("ok");
    let iso = dir.join("disc.iso");
    let burner = FakeBurner::new(Mode::Ok);
    let updates = Mutex::new(Vec::<BurnUpdate>::new());
    let outcome =
        burn_video_ts(&burner, &job(video_ts, iso.clone()), &AtomicBool::new(false), |u| updates.lock().unwrap().push(u))
            .unwrap();
    assert!(!outcome.dual_layer && outcome.iso.is_none());
    no_image_left(&iso);

    let seen = burner.seen().expect("burner was called");
    let planned = plan_disc(video_ts, "BRENNTEST", Capacities::DVD).unwrap();
    assert_eq!(seen.iso_size, planned.image.size_bytes(), "the burner got the complete image");
    assert_eq!(seen.speed, WriteSpeed::Specific(5410));
    assert_eq!(seen.layer_break, None, "single layer");
    assert!(seen.verify && seen.eject && !seen.erase, "options passed through");

    // Image progress first, then the burner's phases in order; remaining time sane, 0 at the end.
    let updates = updates.into_inner().unwrap();
    assert_eq!(updates[0].progress.phase, BurnPhase::Preparing);
    assert_eq!(updates[0].progress.message, "Disc-Image wird erstellt …");
    assert!(updates.iter().any(|u| u.progress.phase == BurnPhase::Preparing && u.progress.fraction >= 1.0));
    let order = |p: BurnPhase| {
        [BurnPhase::Preparing, BurnPhase::Formatting, BurnPhase::Writing, BurnPhase::Finalizing, BurnPhase::Verifying, BurnPhase::Done]
            .iter()
            .position(|&q| q == p)
            .unwrap()
    };
    let burner_updates: Vec<_> = updates.iter().skip_while(|u| u.progress.message.starts_with("Disc-Image")).collect();
    assert!(burner_updates.windows(2).all(|w| order(w[0].progress.phase) <= order(w[1].progress.phase)), "phase order");
    assert!(updates[0].remaining_secs > 0.0);
    assert!(updates.iter().all(|u| u.remaining_secs.is_finite() && u.remaining_secs >= 0.0));
    let last = updates.last().unwrap();
    assert_eq!(last.progress.phase, BurnPhase::Done);
    assert_eq!(last.remaining_secs, 0.0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn keeps_the_image_when_asked() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("keep");
    let iso = dir.join("Brenntest.iso");
    let mut j = job(video_ts, iso.clone());
    j.keep_iso = true;
    let outcome = burn_video_ts(&FakeBurner::new(Mode::Ok), &j, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(outcome.iso.as_deref(), Some(iso.as_path()));
    assert!(iso.is_file() && !iso.with_extension("iso.part").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hands_the_layer_break_to_the_burner() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("dl");
    let iso = dir.join("disc.iso");
    // Pretend the disc is dual layer: single-layer capacity just below the image size.
    let plain = plan_image(video_ts, "BRENNTEST", &Default::default(), &Default::default()).unwrap();
    let caps = Capacities { single_layer: plain.total_sectors - 1, layer: plain.total_sectors };
    let expected = plan_disc(video_ts, "BRENNTEST", caps).unwrap().layer_break.expect("needs two layers").l0_sectors;
    let mut j = job(video_ts, iso.clone());
    j.capacities = caps;
    let burner = FakeBurner::new(Mode::Ok);
    let outcome = burn_video_ts(&burner, &j, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(outcome.dual_layer);
    assert_eq!(burner.seen().unwrap().layer_break, Some(expected));
    no_image_left(&iso);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn backend_error_removes_the_image() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("fail");
    let iso = dir.join("disc.iso");
    let mut j = job(video_ts, iso.clone());
    j.keep_iso = true; // only kept after success
    let err = burn_video_ts(&FakeBurner::new(Mode::Fail), &j, &AtomicBool::new(false), |_| {}).unwrap_err();
    assert!(matches!(&err, BurnError::Backend(m) if m == "Schreibfehler"), "{err}");
    no_image_left(&iso);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cancel_while_writing() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("cancel_write");
    let iso = dir.join("disc.iso");
    let cancel = AtomicBool::new(false);
    let burner = FakeBurner::new(Mode::UntilCancelled);
    let err = burn_video_ts(&burner, &job(video_ts, iso.clone()), &cancel, |u| {
        if u.progress.phase == BurnPhase::Writing && u.progress.fraction >= 0.3 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap_err();
    assert!(matches!(err, BurnError::Cancelled), "{err}");
    assert!(burner.seen().is_some());
    no_image_left(&iso);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cancel_while_writing_the_image() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("cancel_image");
    let iso = dir.join("disc.iso");
    let cancel = AtomicBool::new(false);
    let burner = FakeBurner::new(Mode::Ok);
    let err = burn_video_ts(&burner, &job(video_ts, iso.clone()), &cancel, |u| {
        if u.progress.phase == BurnPhase::Preparing && u.progress.fraction > 0.0 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap_err();
    assert!(matches!(err, BurnError::Cancelled), "{err}");
    assert!(burner.seen().is_none(), "nothing reaches the drive");
    no_image_left(&iso);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cancelled_image_keeps_an_existing_file() {
    let Some(video_ts) = fixture() else { return };
    let dir = scratch("keep_foreign");
    let iso = dir.join("film.iso");
    std::fs::write(&iso, b"older image").unwrap();
    let cancel = AtomicBool::new(false);
    let burner = FakeBurner::new(Mode::Ok);
    let mut j = job(video_ts, iso.clone());
    j.keep_iso = true; // CLI `--keep-iso PATH` pointing at an existing file
    let err = burn_video_ts(&burner, &j, &cancel, |u| {
        if u.progress.phase == BurnPhase::Preparing && u.progress.fraction > 0.0 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap_err();
    assert!(matches!(err, BurnError::Cancelled), "{err}");
    assert_eq!(std::fs::read(&iso).unwrap(), b"older image", "a file this run never wrote stays");
    assert!(!iso.with_extension("iso.part").exists(), "partial image left behind");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- Blu-ray: a synthetic BDMV project (no external tools needed) ---------------------------------

/// `<tmp>/Film/BDMV` with a few small files and a 3 MB "m2ts", plus `CERTIFICATE/BACKUP`.
fn bd_fixture() -> &'static Path {
    static BD: OnceLock<PathBuf> = OnceLock::new();
    BD.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("cx_burnflow_bd_{}", std::process::id())).join("Film");
        let _ = std::fs::remove_dir_all(&root);
        for d in ["BDMV/PLAYLIST", "BDMV/CLIPINF", "BDMV/STREAM", "BDMV/BACKUP/PLAYLIST", "CERTIFICATE/BACKUP"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("BDMV/index.bdmv"), b"INDX0200").unwrap();
        std::fs::write(root.join("BDMV/MovieObject.bdmv"), b"MOBJ0200").unwrap();
        std::fs::write(root.join("BDMV/PLAYLIST/00000.mpls"), vec![1u8; 300]).unwrap();
        std::fs::write(root.join("BDMV/CLIPINF/00000.clpi"), vec![2u8; 700]).unwrap();
        std::fs::write(root.join("BDMV/STREAM/00000.m2ts"), (0..3_000_000u32).map(|i| (i % 253) as u8).collect::<Vec<u8>>()).unwrap();
        root.join("BDMV")
    })
}

fn bd_job(iso: PathBuf) -> BurnJob {
    BurnJob { speed: WriteSpeed::Specific(13_170), media_speeds: vec![26_341, 13_170, 8_780], ..job(bd_fixture(), iso) }
}

#[test]
fn blu_ray_image_reaches_the_burner_without_layer_break() {
    let dir = scratch("bd_ok");
    let iso = dir.join("disc.iso");
    let burner = FakeBurner::new(Mode::Ok);
    let updates = Mutex::new(Vec::<BurnUpdate>::new());
    let outcome =
        burn_video_ts(&burner, &bd_job(iso.clone()), &AtomicBool::new(false), |u| updates.lock().unwrap().push(u)).unwrap();
    assert!(!outcome.dual_layer && outcome.iso.is_none());
    no_image_left(&iso);

    let seen = burner.seen().expect("burner was called");
    let planned = udf250::plan_bd_image(bd_fixture().parent().unwrap(), "BRENNTEST").unwrap();
    assert_eq!(seen.iso_size, planned.size_bytes(), "the burner got the complete UDF 2.50 image");
    assert_eq!((seen.layer_break, seen.speed), (None, WriteSpeed::Specific(13_170)));
    assert!(seen.verify && seen.eject && !seen.erase, "options passed through");
    let updates = updates.into_inner().unwrap();
    assert_eq!(updates[0].progress.message, "Disc-Image wird erstellt …");
    assert!(updates[0].remaining_secs > 0.0 && updates.iter().all(|u| u.remaining_secs.is_finite()));
    assert_eq!(updates.last().unwrap().remaining_secs, 0.0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn blu_ray_image_is_kept_on_request_and_is_udf_250() {
    let dir = scratch("bd_keep");
    let iso = dir.join("Film.iso");
    let mut j = bd_job(iso.clone());
    j.keep_iso = true;
    let outcome = burn_video_ts(&FakeBurner::new(Mode::Ok), &j, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(outcome.iso.as_deref(), Some(iso.as_path()));
    assert!(!iso.with_extension("iso.part").exists());
    let img = std::fs::read(&iso).unwrap();
    assert_eq!(&img[16 * 2048 + 1..16 * 2048 + 6], b"BEA01");
    assert_eq!(&img[17 * 2048 + 1..17 * 2048 + 6], b"NSR03");
    assert_eq!(&img[16 * 2048 + 1..16 * 2048 + 6], b"BEA01");
    assert_eq!(img.len() % 2048, 0);
    // No ISO 9660 bridge: sector 16 is not a primary volume descriptor.
    assert_ne!(&img[16 * 2048 + 1..16 * 2048 + 6], b"CD001");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn blu_ray_failure_and_cancel_leave_no_image() {
    let dir = scratch("bd_fail");
    let iso = dir.join("disc.iso");
    let mut j = bd_job(iso.clone());
    j.keep_iso = true;
    let err = burn_video_ts(&FakeBurner::new(Mode::Fail), &j, &AtomicBool::new(false), |_| {}).unwrap_err();
    assert!(matches!(&err, BurnError::Backend(m) if m == "Schreibfehler"), "{err}");
    no_image_left(&iso);

    let cancel = AtomicBool::new(false);
    let burner = FakeBurner::new(Mode::Ok);
    let err = burn_video_ts(&burner, &bd_job(iso.clone()), &cancel, |u| {
        if u.progress.phase == BurnPhase::Preparing && u.progress.fraction > 0.0 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap_err();
    assert!(matches!(err, BurnError::Cancelled), "{err}");
    assert!(burner.seen().is_none(), "nothing reaches the drive");
    no_image_left(&iso);

    // An existing file at the target survives a cancelled image (CLI `--keep-iso PATH`).
    std::fs::write(&iso, b"older image").unwrap();
    let cancel = AtomicBool::new(false);
    let mut j = bd_job(iso.clone());
    j.keep_iso = true;
    let err = burn_video_ts(&FakeBurner::new(Mode::Ok), &j, &cancel, |u| {
        if u.progress.phase == BurnPhase::Preparing && u.progress.fraction > 0.0 {
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap_err();
    assert!(matches!(err, BurnError::Cancelled), "{err}");
    assert_eq!(std::fs::read(&iso).unwrap(), b"older image");
    assert!(!iso.with_extension("iso.part").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn blu_ray_content_is_recognised() {
    use convertx_rust::burner::{is_bdmv_folder, DiscContent};
    assert!(is_bdmv_folder(bd_fixture()));
    let c = DiscContent::of_folder(bd_fixture()).unwrap();
    assert!(c.bluray && !c.dual_layer && c.sectors > 1500, "{c:?}");
    assert!(!is_bdmv_folder(&bd_fixture().join("STREAM")));
}

// ---- CLI `burn`: error paths that stop before an image is written ---------------------------

fn cli(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_convertx-rust")).arg("burn").args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn cli_rejects_bad_arguments() {
    let (code, err) = cli(&["x", "--speed", "99"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("'99' is not auto, max or a speed in x"), "{err}");

    let missing = std::env::temp_dir().join("cx_burnflow_does_not_exist");
    let (code, err) = cli(&[missing.to_str().unwrap()]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("kein fertiger VIDEO_TS- oder BDMV-Ordner"), "{err}");
}

/// Lists the real drives (read-only); a drive name that matches nothing stops before probing
/// or writing. Skipped with `CONVERTX_SKIP_DRIVE_TESTS`.
#[test]
fn cli_unknown_drive_stops_before_writing() {
    if std::env::var_os("CONVERTX_SKIP_DRIVE_TESTS").is_some() {
        eprintln!("skipping: CONVERTX_SKIP_DRIVE_TESTS");
        return;
    }
    let Some(video_ts) = fixture() else { return };
    let project = video_ts.parent().unwrap();
    let (code, err) = cli(&[project.to_str().unwrap(), "--drive", "KEIN_LAUFWERK_XYZ", "--keep-iso"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("nicht gefunden") || err.contains("Brenner gefunden"), "{err}");
    let iso_dir = project.parent().unwrap();
    let images: Vec<_> = std::fs::read_dir(iso_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.contains(".iso"))
        .collect();
    assert!(images.is_empty(), "no image written: {images:?}");
}
