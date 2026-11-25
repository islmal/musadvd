//! Disc image (UDF + ISO 9660) of a real authored DVD: mounted by Windows, every file must
//! match byte for byte; layer-break padding keeps the title start sectors consistent.
//! Skipped (with a message) when tools/templates are missing or mounting is not possible.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use musadvd::burner::layerbreak::{cell_starts, patch_tt_srpt, plan_disc, Capacities};
use musadvd::burner::udf::{plan_image, write_image, ImagePlan, Timestamp, SECTOR};
use musadvd::burner::{create_disc_image, sanitize_volume_label};
use musadvd::dvd::{self, BuildOptions};
use musadvd::tools::Toolchain;

const TEMPLATE: &str =
    r"C:\ProgramData\VSO\ConvertXToDVD\7\Templates\Open Style\10_TitlesPerPageNoRoot.ini";

fn build(name: &str) -> Option<(PathBuf, PathBuf)> {
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
    let dir = std::env::temp_dir().join(format!("cx_iso_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let inputs: Vec<PathBuf> = (1..=2)
        .map(|n| {
            let clip = dir.join(format!("clip{n}.mkv"));
            let st = Command::new(&tools.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=720x576:rate=25:duration=6"])
                .args(["-f", "lavfi", "-i", &format!("sine=f={}:duration=6", 400 * n)])
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
        project_name: Some("Urlaub 2025: Strand & Meer".into()),
        chapters: dvd::ChapterSettings { interval_secs: 1.0, ..Default::default() },
        ..BuildOptions::default()
    };
    let report = rt.block_on(dvd::build_dvd(&opts, &mut |_| {})).unwrap_or_else(|e| panic!("build failed: {e}"));
    Some((dir, report.video_ts))
}

struct Mounted {
    iso: PathBuf,
    root: PathBuf,
    label: String,
    fs: String,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Dismount-DiskImage -ImagePath '{}' | Out-Null", self.iso.display())])
            .status();
    }
}

fn mount(iso: &Path) -> Option<Mounted> {
    let script = format!(
        "$v = Mount-DiskImage -ImagePath '{}' -PassThru | Get-Volume; \"$($v.DriveLetter)|$($v.FileSystemLabel)|$($v.FileSystem)\"",
        iso.display()
    );
    let out = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parts: Vec<&str> = text.split('|').collect();
    if !out.status.success() || parts.len() != 3 || parts[0].is_empty() {
        eprintln!("skipping mount check: {text} {}", String::from_utf8_lossy(&out.stderr));
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Dismount-DiskImage -ImagePath '{}' | Out-Null", iso.display())])
            .status();
        return None;
    }
    Some(Mounted { iso: iso.to_path_buf(), root: PathBuf::from(format!("{}:\\", parts[0])), label: parts[1].into(), fs: parts[2].into() })
}

/// Every planned file must be readable from the mounted image with the planned content.
fn compare_files(m: &Mounted, plan: &ImagePlan) {
    let video_ts = m.root.join("VIDEO_TS");
    assert!(m.root.join("AUDIO_TS").is_dir(), "AUDIO_TS directory present");
    let mut on_disc: Vec<String> =
        std::fs::read_dir(&video_ts).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    on_disc.sort();
    let mut planned: Vec<String> = plan.files.iter().map(|f| f.name.clone()).collect();
    planned.sort();
    assert_eq!(on_disc, planned);
    for f in &plan.files {
        let got = std::fs::read(video_ts.join(&f.name)).unwrap();
        let want = f.data.clone().unwrap_or_else(|| std::fs::read(&f.source).unwrap());
        assert!(got == want, "{} differs ({} vs {} bytes)", f.name, got.len(), want.len());
    }
}

/// Raw image sectors of a file must hold the file (checks the planned LBAs, not just the FS).
fn check_lbas(iso: &Path, plan: &ImagePlan) {
    let img = std::fs::read(iso).unwrap();
    assert_eq!(img.len() as u64, plan.size_bytes());
    for f in &plan.files {
        let want = f.data.clone().unwrap_or_else(|| std::fs::read(&f.source).unwrap());
        let at = f.lba as usize * SECTOR as usize;
        assert!(img[at..at + want.len()] == want[..], "{} not at sector {}", f.name, f.lba);
    }
    // VTS start sectors in TT_SRPT are relative to VIDEO_TS.IFO.
    let vmg = plan.file("VIDEO_TS.IFO").unwrap();
    let ifo = vmg.data.clone().unwrap_or_else(|| std::fs::read(&vmg.source).unwrap());
    let be32 = |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
    let base = be32(&ifo, 0xC4) as usize * SECTOR as usize;
    let titles = u16::from_be_bytes([ifo[base], ifo[base + 1]]) as usize;
    assert!(titles >= 2);
    for i in 0..titles {
        let e = base + 8 + 12 * i;
        let vts = ifo[e + 6];
        let start = be32(&ifo, e + 8);
        let vts_ifo = plan.file(&format!("VTS_{vts:02}_0.IFO")).unwrap();
        assert_eq!(vmg.lba + start, vts_ifo.lba, "title {} → VTS {vts}", i + 1);
    }
}

#[test]
fn image_mounts_and_matches() {
    let Some((dir, video_ts)) = build("plain") else { return };
    let iso = dir.join("disc.iso");
    let label = sanitize_volume_label("Urlaub 2025: Strand & Meer");
    assert_eq!(label, "URLAUB_2025_STRAND_MEER");
    let mut last = 0.0;
    let plan = create_disc_image(&video_ts, &iso, &label, Capacities::DVD, &AtomicBool::new(false), |p| last = p).unwrap();
    assert_eq!(last, 1.0);
    assert!(plan.layer_break.is_none(), "small disc fits one layer");
    assert!(!dir.join("disc.iso.part").exists());
    check_lbas(&iso, &plan.image);

    if let Some(m) = mount(&iso) {
        assert_eq!(m.label, label);
        assert_eq!(m.fs, "UDF", "Windows reads the UDF side");
        compare_files(&m, &plan.image);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn layer_break_padding_keeps_disc_consistent() {
    let Some((dir, video_ts)) = build("dl") else { return };

    // Pretend the disc is dual layer: single layer capacity just below, each layer = whole.
    let plain = plan_image(&video_ts, "DL_TEST", &HashMap::new(), &HashMap::new()).unwrap();
    let caps = Capacities { single_layer: plain.total_sectors - 1, layer: plain.total_sectors };
    let disc = plan_disc(&video_ts, "DL_TEST", caps).unwrap();
    let lb = disc.layer_break.expect("needs two layers");
    let total = disc.image.total_sectors;
    assert_eq!(lb.l0_sectors % 16, 0);
    assert!(lb.l0_sectors >= total - lb.l0_sectors, "L0 ≥ L1");
    let cells = cell_starts(&disc.image).unwrap();
    assert!(cells.iter().any(|c| c.lba == lb.l0_sectors), "break at a cell start: {lb:?}");
    check_lbas_planned(&disc.image);

    // Forced padding in front of titleset 2 (+ patched TT_SRPT): the image stays consistent.
    let pad = 5;
    let mut overrides = HashMap::new();
    for name in ["VIDEO_TS.IFO", "VIDEO_TS.BUP"] {
        let mut data = std::fs::read(video_ts.join(name)).unwrap();
        patch_tt_srpt(&mut data, 2, pad).unwrap();
        overrides.insert(name.to_string(), data);
    }
    let padded = plan_image(&video_ts, "DL_TEST", &HashMap::from([(2, pad)]), &overrides).unwrap();
    assert_eq!(padded.total_sectors, plain.total_sectors + pad);
    assert_eq!(padded.file("VTS_02_0.IFO").unwrap().lba, plain.file("VTS_02_0.IFO").unwrap().lba + pad);
    // Cells of titleset 2 moved with it.
    let before = cell_starts(&plain).unwrap();
    let after = cell_starts(&padded).unwrap();
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(a.lba, b.lba + if b.vts >= 2 { pad } else { 0 });
    }
    let iso = dir.join("padded.iso");
    write_image(&padded, &iso, &Timestamp::now(), &AtomicBool::new(false), |_| {}).unwrap();
    check_lbas(&iso, &padded);
    if let Some(m) = mount(&iso) {
        compare_files(&m, &padded);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// TT_SRPT consistency on a plan without writing it.
fn check_lbas_planned(plan: &ImagePlan) {
    let vmg = plan.file("VIDEO_TS.IFO").unwrap();
    let ifo = vmg.data.clone().unwrap_or_else(|| std::fs::read(&vmg.source).unwrap());
    let be32 = |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
    let base = be32(&ifo, 0xC4) as usize * SECTOR as usize;
    for i in 0..u16::from_be_bytes([ifo[base], ifo[base + 1]]) as usize {
        let e = base + 8 + 12 * i;
        let vts_ifo = plan.file(&format!("VTS_{:02}_0.IFO", ifo[e + 6])).unwrap();
        assert_eq!(vmg.lba + be32(&ifo, e + 8), vts_ifo.lba);
    }
}

/// Golden hash: the VIDEO_TS image layout is burnt by users, so refactors of the UDF writer
/// must keep a synthetic disc byte-identical (value captured from the code before the refactor).
#[test]
fn golden_video_ts_image_hash() {
    use md5::{Digest, Md5};
    let dir = std::env::temp_dir().join(format!("cx_golden_{}", std::process::id()));
    let video_ts = dir.join("VIDEO_TS");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&video_ts).unwrap();
    let fill = |name: &str, len: usize, seed: u8| {
        let data: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect();
        std::fs::write(video_ts.join(name), data).unwrap();
    };
    fill("VIDEO_TS.IFO", 3000, 1);
    fill("VIDEO_TS.BUP", 3000, 1);
    fill("VIDEO_TS.VOB", 5000, 2);
    fill("VTS_01_0.IFO", 4100, 3);
    fill("VTS_01_0.BUP", 4100, 3);
    fill("VTS_01_1.VOB", 20_000, 4);
    fill("VTS_01_2.VOB", 2048, 5);
    let plan = plan_image(&video_ts, "GOLDEN", &HashMap::new(), &HashMap::new()).unwrap();
    let iso = dir.join("golden.iso");
    let ts = Timestamp { year: 2026, month: 10, day: 24, hour: 12, minute: 0, second: 0 };
    write_image(&plan, &iso, &ts, &AtomicBool::new(false), |_| {}).unwrap();
    let hash = format!("{:x}", Md5::digest(std::fs::read(&iso).unwrap()));
    std::fs::remove_dir_all(&dir).ok();
    // Re-captured after the MusaDVD rename (implementation ID `*MusaDVD`, application ID `MUSADVD`); the
    // old value `52152f47…` is the same image with the `convertx-rust` identifiers.
    assert_eq!(hash, "91039665c6b967f2dfcbebe632665330");
}

/// A data tree (deep folders, long and non-ASCII names, a duplicate) built by the Burn Studio
/// builder: Windows mounts the image and every file reads back with its content.
#[test]
fn data_tree_mounts_with_long_names() {
    use musadvd::burner::builder::{build_to_file, plan_build, BuildFileOptions, BuildInput, Source};
    use musadvd::burner::reader::Split;
    use musadvd::config::StudioBuild;

    let dir = std::env::temp_dir().join(format!("cx_data_tree_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    let files: [(&str, Vec<u8>); 5] = [
        ("Ein sehr langer Dateiname mit Umlauten äöüß.txt", b"umlaut".to_vec()),
        ("ordner/unterordner/tief/datei.bin", (0..100_000u32).map(|i| (i % 253) as u8).collect()),
        ("ordner/kopie1.dat", vec![3u8; 70_000]),
        ("kopie2.dat", vec![3u8; 70_000]),
        ("leer.txt", vec![]),
    ];
    for (rel, data) in &files {
        let p = src.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    let input = BuildInput { sources: vec![Source { path: src.clone(), dest: String::new() }], label: "Datentest".into(), ..Default::default() };
    let plan = plan_build(&input, &StudioBuild::default()).unwrap();
    let iso = dir.join("data.iso");
    let cancel = AtomicBool::new(false);
    build_to_file(&plan, &iso, &BuildFileOptions { split: Split::None, md5: false, layout_file: "none", cancel: &cancel }, |_| {}).unwrap();

    if let Some(m) = mount(&iso) {
        assert_eq!(m.label, "DATENTEST");
        assert_eq!(m.fs, "UDF");
        for (rel, data) in &files {
            let got = std::fs::read(m.root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
            assert!(&got == data, "{rel} differs");
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}
