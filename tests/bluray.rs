//! Blu-ray output end to end: two clips (16:9 25 fps with 5.1 + stereo audio, an SRT and chapters;
//! 4:3 24 fps with one stereo track) become two chained playlists, each with its own frame rate, on
//! a BD-25 (H.264 + AC-3 through the native BDAV muxer),
//! 720p NTSC, the `--single-video` check, and the UDF 2.50 image of the result.
//! Skipped (with a message) when ffmpeg is missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

use musadvd::author::{Resolution, TvStandard};
use musadvd::burner::udf250;
use musadvd::calculator::DiscType;
use musadvd::config::HwAccelMode;
use musadvd::encoder::BdEncoder;
use musadvd::dvd::{self, BuildOptions, BuildReport, StreamSelection};
use musadvd::tools::Toolchain;

fn tools_pair() -> (tokio::runtime::Runtime, Toolchain) {
    tools().expect("tools were available a moment ago")
}

fn tools() -> Option<(tokio::runtime::Runtime, Toolchain)> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    match rt.block_on(Toolchain::detect()) {
        Ok(t) => Some((rt, t)),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

/// `field value` lines of the H.264 headers of the first pictures of `file` (ffmpeg's
/// `trace_headers`, whitespace collapsed; SEI payload types as `payload_type N`).
fn headers(tools: &Toolchain, file: &Path) -> String {
    let out = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-loglevel", "trace", "-i"])
        .arg(file)
        .args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v", "trace_headers", "-frames:v", "3", "-f", "null", "-"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.starts_with("[trace_headers"))
        .filter_map(|l| {
            // "[trace_headers @ 0x..] 167   nal_hrd_parameters_present_flag   0 = 0"
            let mut parts = l.split_once(']')?.1.split_whitespace().skip(1);
            let name = parts.next()?;
            let value = l.rsplit('=').next()?.trim();
            let name = if name.starts_with("last_payload_type_byte") { "payload_type" } else { name };
            Some(format!("{name} {value}\n"))
        })
        .collect()
}

fn ffmpeg(tools: &Toolchain, args: &[&str]) {
    let st = Command::new(&tools.ffmpeg).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

/// `key=value` lines of the top-level streams selected by `select` (ffprobe also lists the
/// streams of the transport stream's programs, which would count everything twice).
fn probe(tools: &Toolchain, file: &Path, select: &str, entries: &str) -> String {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", select, "-show_entries", entries, "-of", "json"])
        .arg(file)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut text = String::new();
    for stream in json["streams"].as_array().into_iter().flatten() {
        for (k, v) in stream.as_object().unwrap() {
            text.push_str(&format!("{k}={}
", v.as_str().map_or_else(|| v.to_string(), str::to_string)));
        }
    }
    text
}

/// Clips in `dir`: `a.mkv` (12 s, 1280×720 25 fps, 5.1 + stereo AC-3, SRT) and `b.mkv` (6 s, 4:3, 24 fps).
fn make_clips(tools: &Toolchain, dir: &Path) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let srt = dir.join("a.srt");
    std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:03,000\nHallo Welt\n\n2\n00:00:04,000 --> 00:00:06,000\nZweite Zeile\n\n").unwrap();
    let (a, b) = (dir.join("a.mkv"), dir.join("b.mkv"));
    ffmpeg(
        tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=12", "-f", "lavfi", "-i", "sine=f=440:d=12,aformat=channel_layouts=5.1",
            "-f", "lavfi", "-i", "sine=f=660:d=12", "-i", srt.to_str().unwrap(), "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3",
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3", "-c:s", "srt", "-metadata:s:a:0", "language=ger",
            "-metadata:s:a:1", "language=eng", "-metadata:s:s:0", "language=ger", a.to_str().unwrap(),
        ],
    );
    ffmpeg(
        tools,
        &[
            "-f", "lavfi", "-i", "testsrc=s=640x480:r=24:d=6", "-f", "lavfi", "-i", "sine=f=330:d=6", "-c:v", "libx264", "-preset", "ultrafast",
            "-c:a", "ac3", "-metadata:s:a:0", "language=ger", b.to_str().unwrap(),
        ],
    );
    (a, b)
}

fn build(rt: &tokio::runtime::Runtime, opts: &BuildOptions) -> Result<BuildReport, dvd::BuildError> {
    rt.block_on(dvd::build_dvd(opts, &mut |_| {}))
}

/// The two-clip Blu-ray, built once for the tests that inspect it.
fn fixture() -> Option<&'static (PathBuf, BuildReport)> {
    static BD: OnceLock<Option<(PathBuf, BuildReport)>> = OnceLock::new();
    BD.get_or_init(|| {
        let (rt, tools) = tools()?;
        let dir = std::env::temp_dir().join(format!("cx_bluray_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = make_clips(&tools, &dir.join("clips"));
        let opts = BuildOptions {
            inputs: vec![a, b],
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd1080,
            fast_encode: true,
            project_name: Some("BDTEST".into()),
            selections: vec![
                StreamSelection { audio: Some(vec![1, 2]), subtitles: Some(vec![3]), label: None, chapters: Some(vec![0.0, 6.0]), ..StreamSelection::default() },
                StreamSelection::default(),
            ],
            ..BuildOptions::default()
        };
        let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
        Some((dir, report))
    })
    .as_ref()
}

#[test]
fn two_clips_make_two_playlists_with_their_own_rates() {
    let Some((_, tools)) = tools() else { return };
    let Some((_, report)) = fixture() else { return };
    let bd = report.project_dir.as_path();
    assert_eq!(report.video_ts, bd.join("BDMV"));
    for f in [
        "BDMV/index.bdmv",
        "BDMV/MovieObject.bdmv",
        "BDMV/PLAYLIST/00000.mpls",
        "BDMV/PLAYLIST/00001.mpls",
        "BDMV/CLIPINF/00000.clpi",
        "BDMV/CLIPINF/00001.clpi",
        "BDMV/STREAM/00000.m2ts",
        "BDMV/STREAM/00001.m2ts",
    ] {
        assert!(bd.join(f).is_file(), "{f} missing");
    }
    assert!(bd.join("CERTIFICATE").is_dir());
    assert_eq!((report.menu_pages, report.resolution), (0, Resolution::Hd1080));
    assert_eq!(report.titles.len(), 2);
    assert!(report.work_dir.is_none(), "work directory removed");

    // Menu-less navigation: First Play + Top Menu + 2 title objects, title 1 chains into title 2.
    let index = std::fs::read(bd.join("BDMV/index.bdmv")).unwrap();
    assert_eq!(index, musadvd::hdmv::index_bdmv(0, 1, &[2, 3]));
    let mobj = std::fs::read(bd.join("BDMV/MovieObject.bdmv")).unwrap();
    assert_eq!(mobj, musadvd::hdmv::movie_object_bdmv(&musadvd::bd_menu::plan_without_menus(2).objects));
    // Blu-ray keeps the 0200 versions and its BD-J folders (the AVCHD changes do not touch it).
    for (f, magic) in [("BDMV/PLAYLIST/00000.mpls", b"MPLS0200"), ("BDMV/CLIPINF/00000.clpi", b"HDMV0200")] {
        assert_eq!(&std::fs::read(bd.join(f)).unwrap()[..8], magic, "{f}");
    }
    assert!(bd.join("BDMV/BDJO").is_dir() && bd.join("CERTIFICATE/BACKUP").is_dir());

    // Clip A keeps its 25 fps (1080i-flagged), clip B its 24 fps: H.264 High@4.1 1080 in both.
    let (ma, mb) = (bd.join("BDMV/STREAM/00000.m2ts"), bd.join("BDMV/STREAM/00001.m2ts"));
    for (m2ts, rate) in [(&ma, "r_frame_rate=25/1"), (&mb, "r_frame_rate=24/1")] {
        let v = probe(&tools, m2ts, "v:0", "stream=codec_name,profile,level,width,height,r_frame_rate");
        for want in ["codec_name=h264", "profile=High", "level=41", "width=1920", "height=1080", rate] {
            assert!(v.contains(want), "{want} in {v}");
        }
        // Same track layout in both playlists: two AC-3 tracks.
        let audio = probe(&tools, m2ts, "a", "stream=codec_name,channels");
        assert_eq!(audio.matches("codec_name=ac3").count(), 2, "{audio}");
    }
    let audio = probe(&tools, &ma, "a", "stream=codec_name,channels");
    assert!(audio.contains("channels=6") && audio.contains("channels=2"), "{audio}");
    assert_eq!(probe(&tools, &ma, "s", "stream=codec_name").matches("hdmv_pgs_subtitle").count(), 1);
    assert_eq!(probe(&tools, &mb, "s", "stream=codec_name").matches("hdmv_pgs_subtitle").count(), 1, "placeholder subtitle track");
    let duration = |m2ts: &Path| -> f64 {
        String::from_utf8_lossy(
            &Command::new(&tools.ffprobe).args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(m2ts).output().unwrap().stdout,
        )
        .trim()
        .parse()
        .unwrap()
    };
    assert!((duration(&ma) - 12.0).abs() < 0.5 && (duration(&mb) - 6.0).abs() < 0.5, "{} / {}", duration(&ma), duration(&mb));

    // B has one audio track only: its second one is silent, A's is audible.
    let max_volume = |m2ts: &Path, from: f64| -> f64 {
        let out = Command::new(&tools.ffmpeg)
            .args(["-hide_banner", "-nostats", "-ss", &from.to_string(), "-i"])
            .arg(m2ts)
            .args(["-map", "0:a:1", "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stderr).into_owned();
        let at = log.find("max_volume:").unwrap_or_else(|| panic!("no volumedetect in {log}"));
        log[at + 11..].split_whitespace().next().unwrap().parse().unwrap()
    };
    assert!(max_volume(&ma, 1.0) > -40.0, "A's stereo track is audible");
    assert!(max_volume(&mb, 1.0) < -80.0, "B's filler track is silent");

    // Chapter marks per playlist: A has two (0 and 6 s), B one.
    let marks = |n: u32| {
        let mpls = std::fs::read(bd.join(format!("BDMV/PLAYLIST/{n:05}.mpls"))).unwrap();
        let at = u32::from_be_bytes(mpls[0x0c..0x10].try_into().unwrap()) as usize;
        let count = u16::from_be_bytes(mpls[at + 4..at + 6].try_into().unwrap()) as usize;
        (0..count)
            .map(|i| u32::from_be_bytes(mpls[at + 6 + i * 14 + 4..at + 6 + i * 14 + 8].try_into().unwrap()) as f64 / 45_000.0)
            .collect::<Vec<_>>()
    };
    let a = marks(0);
    assert_eq!(a.len(), 2, "{a:?}");
    assert!((a[1] - a[0] - 6.0).abs() < 0.2, "{a:?}");
    assert_eq!(marks(1).len(), 1);
}

#[test]
fn hd720_ntsc_single_clip() {
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_720_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (a, _) = make_clips(&tools, &dir.join("clips"));
    let opts = BuildOptions {
        inputs: vec![a],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd720,
        tv_standard: TvStandard::Ntsc,
        fast_encode: true,
        selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), label: None, chapters: None, ..StreamSelection::default() }],
        ..BuildOptions::default()
    };
    let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
    let m2ts = report.project_dir.join("BDMV/STREAM/00000.m2ts");
    let v = probe(&tools, &m2ts, "v:0", "stream=width,height,r_frame_rate,profile,level");
    for want in ["width=1280", "height=720", "r_frame_rate=60000/1001", "profile=High", "level=41"] {
        assert!(v.contains(want), "{want} in {v}");
    }
    assert!(probe(&tools, &m2ts, "s", "stream=codec_name").trim().is_empty(), "no subtitles selected");
    assert_eq!(probe(&tools, &m2ts, "a", "stream=codec_name").matches("ac3").count(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// Hardware encoder selected (NVIDIA, Intel, AMD): with a working GPU/driver the encode uses it,
/// otherwise it falls back to x264 with a warning; either way the result is the same High@4.1 stream.
#[test]
fn hw_encode_uses_hw_encoder_or_falls_back() {
    let Some((rt, tools)) = tools() else { return };
    for mode in [HwAccelMode::Nvidia, HwAccelMode::Intel, HwAccelMode::Amd] {
        let enc = mode.bd_encoder().unwrap();
        let dir = std::env::temp_dir().join(format!("cx_bluray_hw_{mode:?}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, _) = make_clips(&tools, &dir.join("clips"));
        let opts = BuildOptions {
            inputs: vec![a],
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            hw_accel: mode,
            selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), label: None, chapters: None, ..StreamSelection::default() }],
            ..BuildOptions::default()
        };
        let (mut steps, mut warnings) = (Vec::new(), Vec::new());
        let report = rt
            .block_on(dvd::build_dvd(&opts, &mut |e| match e {
                dvd::Event::Step(s) => steps.push(s),
                dvd::Event::Warning(w) => warnings.push(w),
                _ => {}
            }))
            .unwrap_or_else(|e| panic!("{mode:?}: build failed: {e}"));
        match rt.block_on(tools.probe_hw_encoder(enc)) {
            Ok(()) => assert!(steps.iter().any(|s| s.contains(enc.label())), "{mode:?}: {steps:?}"),
            // The reason text can differ between two probe runs (pointer addresses in ffmpeg logs).
            Err(reason) => assert!(warnings.iter().any(|w| w.contains("x264") && w.contains(enc.label())), "{mode:?} {reason}: {warnings:?}"),
        }
        let m2ts = report.project_dir.join("BDMV/STREAM/00000.m2ts");
        let v = probe(&tools, &m2ts, "v:0", "stream=codec_name,width,height,profile,level");
        for want in ["codec_name=h264", "width=1280", "profile=High", "level=41"] {
            assert!(v.contains(want), "{mode:?}: {want} in {v}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Guards against option typos for encoders we cannot run here (no Intel/AMD GPU): every private
/// option we pass must be listed by `ffmpeg -h encoder=<name>`.
#[test]
fn hw_encoder_options_exist() {
    let Some((_rt, tools)) = tools() else { return };
    for enc in [BdEncoder::Qsv, BdEncoder::Amf] {
        let out = std::process::Command::new(&tools.ffmpeg).args(["-hide_banner", "-h"]).arg(format!("encoder={}", enc.codec())).output().unwrap();
        let help = String::from_utf8_lossy(&out.stdout);
        for opt in enc.bd_options().into_iter().filter(|o| o.starts_with('-')) {
            assert!(help.lines().any(|l| l.split_whitespace().next() == Some(opt)), "{opt} missing for {}: {help}", enc.codec());
        }
    }
}

/// A 6 s 1280×720 clip with one 5.1 track at 48 kHz in `codec` (`ac3` copies, `dca` is experimental).
fn make_audio_clip(tools: &Toolchain, dir: &Path, name: &str, codec: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let clip = dir.join(name);
    ffmpeg(
        tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=25:d=6", "-f", "lavfi", "-i",
            "sine=f=440:d=6:sample_rate=48000,aformat=channel_layouts=5.1", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", codec,
            "-strict", "-2", "-b:a", if codec == "dca" { "768k" } else { "384k" }, "-metadata:s:a:0", "language=ger", clip.to_str().unwrap(),
        ],
    );
    clip
}

fn audio_options(inputs: Vec<PathBuf>, out: PathBuf, disc: DiscType, mode: musadvd::encoder::AudioMode) -> BuildOptions {
    BuildOptions {
        inputs,
        disc,
        output: out,
        resolution: Resolution::Hd1080,
        fast_encode: true,
        audio_mode: mode,
        selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), label: None, chapters: None, ..StreamSelection::default() }],
        ..BuildOptions::default()
    }
}

use musadvd::encoder::BdAudioMode;

/// Compliant AC-3 is copied (Auto), DTS is copied on a Blu-ray but re-encoded for AVCHD.
#[test]
fn audio_is_copied_when_compliant() {
    use musadvd::encoder::AudioMode;
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_copy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ac3 = make_audio_clip(&tools, &dir.join("clips"), "ac3.mkv", "ac3");
    let dts = make_audio_clip(&tools, &dir.join("clips"), "dts.mkv", "dca");
    let bitrate = |m2ts: &Path| probe(&tools, m2ts, "a", "stream=codec_name,bit_rate,channels");

    // AC-3 384 kbps 5.1 -> copied on BD (the track keeps its bitrate).
    let r = build(&rt, &audio_options(vec![ac3.clone()], dir.join("o1"), DiscType::Bd25, AudioMode::Auto)).unwrap();
    let a = bitrate(&r.project_dir.join("BDMV/STREAM/00000.m2ts"));
    assert!(a.contains("codec_name=ac3") && a.contains("channels=6"), "{a}");
    assert!(a.contains("bit_rate=384000"), "copied, not re-encoded at 448k: {a}");
    // "Always AC-3" re-encodes at the default 448 kbps.
    let r = build(&rt, &BuildOptions { bd_audio: BdAudioMode::Encode, ..audio_options(vec![ac3], dir.join("o2"), DiscType::Bd25, AudioMode::Ac3) }).unwrap();
    assert!(bitrate(&r.project_dir.join("BDMV/STREAM/00000.m2ts")).contains("bit_rate=448000"));

    // DTS: copied on a Blu-ray, AC-3 on AVCHD.
    let r = build(&rt, &audio_options(vec![dts.clone()], dir.join("o3"), DiscType::Bd25, AudioMode::Auto)).unwrap();
    assert!(bitrate(&r.project_dir.join("BDMV/STREAM/00000.m2ts")).contains("codec_name=dts"));
    let r = build(&rt, &audio_options(vec![dts], dir.join("o4"), DiscType::Avchd5, AudioMode::Auto)).unwrap();
    assert!(bitrate(&r.project_dir.join("BDMV/STREAM/00000.m2ts")).contains("codec_name=ac3"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Audio copy policy: PCM is repacked losslessly (Auto), Encode re-encodes AC-3, Strict refuses
/// what cannot be copied before any work.
#[test]
fn bd_audio_modes() {
    use musadvd::encoder::AudioMode;
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_bdaudio_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pcm = make_audio_clip(&tools, &dir.join("clips"), "pcm.mkv", "pcm_s24le");
    let ac3 = make_audio_clip(&tools, &dir.join("clips"), "ac3.mkv", "ac3");
    let eac3 = make_audio_clip(&tools, &dir.join("clips"), "eac3.mkv", "eac3");
    let m2ts = |r: &BuildReport| r.project_dir.join("BDMV/STREAM/00000.m2ts");

    // Auto: PCM 5.1 is a lossless repack, not AC-3.
    let r = build(&rt, &audio_options(vec![pcm], dir.join("o1"), DiscType::Bd25, AudioMode::Auto)).unwrap();
    let a = probe(&tools, &m2ts(&r), "a", "stream=codec_name,channels,sample_rate");
    assert!(a.contains("codec_name=pcm_bluray") && a.contains("channels=6") && a.contains("sample_rate=48000"), "{a}");
    // Encode: the 384 kbit/s AC-3 is re-encoded at the default rate.
    let r = build(&rt, &BuildOptions { bd_audio: BdAudioMode::Encode, ..audio_options(vec![ac3], dir.join("o2"), DiscType::Bd25, AudioMode::Auto) }).unwrap();
    let a = probe(&tools, &m2ts(&r), "a", "stream=codec_name,bit_rate");
    assert!(a.contains("codec_name=ac3") && !a.contains("bit_rate=384000"), "{a}");
    // Strict: E-AC-3 cannot be copied and the build stops.
    let err = build(&rt, &BuildOptions { bd_audio: BdAudioMode::Strict, ..audio_options(vec![eac3], dir.join("o3"), DiscType::Bd25, AudioMode::Auto) }).unwrap_err();
    assert!(err.to_string().contains("cannot be copied"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// LPCM: written as `pcm_s24le` into the Matroska intermediate, ffmpeg makes BD LPCM of it.
#[test]
fn lpcm_ends_up_as_bluray_pcm() {
    use musadvd::encoder::AudioMode;
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_lpcm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clip = make_audio_clip(&tools, &dir.join("clips"), "ac3.mkv", "ac3");
    let r = build(&rt, &audio_options(vec![clip.clone()], dir.join("o1"), DiscType::Bd25, AudioMode::Lpcm)).unwrap();
    let a = probe(&tools, &r.project_dir.join("BDMV/STREAM/00000.m2ts"), "a", "stream=codec_name,channels,sample_rate");
    assert!(a.contains("codec_name=pcm_bluray") && a.contains("channels=6") && a.contains("sample_rate=48000"), "{a}");
    // AVCHD: stereo 16 bit.
    let r = build(&rt, &audio_options(vec![clip], dir.join("o2"), DiscType::Avchd5, AudioMode::Lpcm)).unwrap();
    let a = probe(&tools, &r.project_dir.join("BDMV/STREAM/00000.m2ts"), "a", "stream=codec_name,channels");
    assert!(a.contains("codec_name=pcm_bluray") && a.contains("channels=2"), "{a}");
    std::fs::remove_dir_all(&dir).ok();
}

/// AVCHD ("BD5"): the BDMV structure on a DVD-5 – small, within AVCHD limits, UDF 2.50 image that
/// fits a single-layer DVD.
#[test]
fn avchd5_builds_a_dvd_sized_bdmv() {
    use musadvd::burner::{DiscContent, DVD5_SECTORS};
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_avchd_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (a, _) = make_clips(&tools, &dir.join("clips"));
    let opts = BuildOptions {
        inputs: vec![a],
        disc: DiscType::Avchd5,
        output: dir.join("out"),
        resolution: Resolution::Hd1080,
        fast_encode: true,
        project_name: Some("AVCHD".into()),
        selections: vec![StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), label: None, chapters: None, ..StreamSelection::default() }],
        ..BuildOptions::default()
    };
    let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
    assert_eq!(report.target_mb, 4300);
    assert!(report.size_mb < 4300.0, "{}", report.size_mb);
    // Video within AVCHD limits: at most 15 Mbit/s of video, 18 Mbit/s in total.
    assert!(report.plan.video_kbps <= 15_000 && report.plan.total_kbps() <= 18_000, "{:?}", report.plan);
    let m2ts = report.project_dir.join("BDMV/STREAM/00000.m2ts");
    let v = probe(&tools, &m2ts, "v:0", "stream=codec_name,profile,level,width,height,bit_rate");
    for want in ["codec_name=h264", "profile=High", "width=1920", "height=1080"] {
        assert!(v.contains(want), "{want} in {v}");
    }
    // The finished folder is BD-structured and fits a DVD.
    let content = DiscContent::of_folder(&report.video_ts).unwrap();
    assert!(content.bluray && content.fits_dvd && content.sectors <= DVD5_SECTORS, "{content:?}");
    let iso = dir.join("avchd.iso");
    let plan = udf250::create_bd_image(&report.project_dir, &iso, "AVCHD", &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(std::fs::metadata(&iso).unwrap().len(), plan.size_bytes());
    assert_eq!(&std::fs::read(&iso).unwrap()[17 * 2048 + 1..17 * 2048 + 6], b"NSR03");

    // AVCHD as tsMuxeR writes it: BDMV at the root with long names, version "0100" everywhere,
    // the IDEX extension in index.bdmv, AUXDATA, no BD-J/META folders and no CERTIFICATE.
    let bdmv = report.project_dir.join("BDMV");
    assert_eq!(report.video_ts, bdmv);
    let read = |p: &str| std::fs::read(bdmv.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
    for (file, magic) in [
        ("index.bdmv", b"INDX0100"),
        ("MovieObject.bdmv", b"MOBJ0100"),
        ("PLAYLIST/00000.mpls", b"MPLS0100"),
        ("CLIPINF/00000.clpi", b"HDMV0100"),
        ("BACKUP/index.bdmv", b"INDX0100"),
        ("BACKUP/MovieObject.bdmv", b"MOBJ0100"),
        ("BACKUP/PLAYLIST/00000.mpls", b"MPLS0100"),
        ("BACKUP/CLIPINF/00000.clpi", b"HDMV0100"),
    ] {
        assert_eq!(&read(file)[..8], magic, "{file}");
    }
    let index = read("index.bdmv");
    let ext = u32::from_be_bytes(index[12..16].try_into().unwrap()) as usize;
    assert!(ext > 0 && ext.is_multiple_of(4) && &index[ext + 0x18..ext + 0x1c] == b"IDEX", "extension data at {ext}");
    assert_eq!(index.len(), ext + musadvd::hdmv::AVCHD_INDEX_EXTENSION.len());
    assert!(bdmv.join("AUXDATA").is_dir() && bdmv.join("STREAM/00000.m2ts").is_file());
    assert!(!report.project_dir.join("CERTIFICATE").exists() && !report.project_dir.join("AVCHD").exists());
    for d in ["BDJO", "JAR", "META"] {
        assert!(!bdmv.join(d).exists(), "{d}");
    }
    // libbluray (a real BDMV/AVCHD reader) opens the 0100 navigation and finds the streams. ffmpeg's
    // bluray protocol skips playlists under 3 minutes, so this needs a longer clip.
    let protocols = Command::new(&tools.ffmpeg).args(["-hide_banner", "-protocols"]).output().unwrap();
    if String::from_utf8_lossy(&protocols.stdout).lines().any(|l| l.trim() == "bluray") {
        let long = dir.join("long.mkv");
        ffmpeg(&tools, &["-f", "lavfi", "-i", "color=c=0x2060a0:s=1280x720:r=25:d=200", "-f", "lavfi", "-i", "sine=f=220:d=200", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3", long.to_str().unwrap()]);
        let long_opts = BuildOptions { inputs: vec![long], output: dir.join("out_long"), resolution: Resolution::Hd720, selections: vec![], ..opts };
        let long_report = build(&rt, &long_opts).unwrap_or_else(|e| panic!("build failed: {e}"));
        assert!(long_report.size_mb <= long_opts.disc.target_mb() as f64);
        let url = format!("bluray:{}", long_report.project_dir.to_string_lossy().replace('\\', "/"));
        let out = Command::new(&tools.ffprobe).args(["-v", "error", "-playlist", "0", "-show_entries", "stream=codec_name", "-of", "csv=p=0", &url]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && text.contains("h264") && text.contains("ac3"), "libbluray cannot read the AVCHD folder: {text} {}", String::from_utf8_lossy(&out.stderr));
    } else {
        eprintln!("bluray protocol not in this ffmpeg build: libbluray check skipped");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Trim and crop on a Blu-ray: the clip is cut to the section, its audio is re-encoded in step
/// with the video, and the picture keeps the cropped aspect (pillarboxed into 16:9).
#[test]
fn trim_and_crop_on_bluray() {
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_trim_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clip = make_audio_clip(&tools, &dir.join("clips"), "ac3.mkv", "ac3");
    let mut opts = audio_options(vec![clip], dir.join("out"), DiscType::Bd25, musadvd::encoder::AudioMode::Auto);
    opts.selections[0].start_secs = Some(1.0);
    opts.selections[0].end_secs = Some(5.0);
    // 1280x720 minus 160 px left and right = 960x720 (4:3).
    opts.selections[0].crop = Some(musadvd::probe::Crop { top: 0, bottom: 0, left: 160, right: 160 });
    let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
    let m2ts = report.project_dir.join("BDMV/STREAM/00000.m2ts");
    let dur: f64 = String::from_utf8_lossy(
        &Command::new(&tools.ffprobe).args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(&m2ts).output().unwrap().stdout,
    )
    .trim()
    .parse()
    .unwrap();
    assert!((dur - 4.0).abs() < 0.3, "4 s section, got {dur}");
    assert!((report.titles[0].duration_secs - 4.0).abs() < 0.3);
    let audio = probe(&tools, &m2ts, "a", "stream=codec_name,bit_rate");
    assert!(audio.contains("codec_name=ac3"), "{audio}");
    // A pillarboxed 4:3 picture: the left and right edge of a frame are black.
    let frame = dir.join("frame.png");
    ffmpeg(&tools, &["-i", m2ts.to_str().unwrap(), "-frames:v", "1", "-vf", "crop=100:1080:0:0,format=gray", frame.to_str().unwrap()]);
    let stats = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-i", frame.to_str().unwrap(), "-vf", "signalstats,metadata=print", "-f", "null", "-"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&stats.stderr).into_owned();
    let ymax: u32 = log.split("lavfi.signalstats.YMAX=").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap();
    assert!(ymax < 40, "left edge should be a black bar, YMAX={ymax}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn wrong_options_fail_before_any_work() {
    let dir = std::env::temp_dir().join(format!("cx_bluray_opts_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let base = BuildOptions {
        inputs: vec!["a.mkv".into(), "b.mkv".into()],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd1080,
        ..BuildOptions::default()
    };
    // --single-video with two inputs.
    let err = build(&rt, &BuildOptions { bd_single_video: true, ..base.clone() }).unwrap_err().to_string();
    assert!(err.contains("Only one video per Blu-ray") && err.contains('2'), "{err}");
    // DVD resolutions on a BD and HD resolutions on a DVD.
    let err = build(&rt, &BuildOptions { resolution: Resolution::FullD1, ..base.clone() }).unwrap_err().to_string();
    assert!(err.contains("Full D1") && err.contains("1080p"), "{err}");
    let err = build(&rt, &BuildOptions { disc: DiscType::Dvd5, ..base.clone() }).unwrap_err().to_string();
    assert!(err.contains("1080p") && err.contains("only exists for Blu-ray"), "{err}");
    let err = build(&rt, &BuildOptions { disc: DiscType::Dvd9, resolution: Resolution::Hd720, ..base }).unwrap_err().to_string();
    assert!(err.contains("720p"), "{err}");
    assert!(!dir.exists(), "nothing was created");
}

/// Asserts that `dst` holds the same directories and byte-identical files as `src`.
#[cfg(windows)]
fn compare_tree(src: &Path, dst: &Path, files: &mut usize) {
    assert!(dst.is_dir(), "{} missing on the disc", dst.display());
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.path().is_dir() {
            compare_tree(&e.path(), &to, files);
        } else {
            assert_eq!(std::fs::read(e.path()).unwrap(), std::fs::read(&to).unwrap(), "{} differs", to.display());
            *files += 1;
        }
    }
}

#[cfg(windows)]
struct Mounted(PathBuf);

#[cfg(windows)]
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Dismount-DiskImage -ImagePath '{}' | Out-Null", self.0.display())])
            .status();
    }
}

#[test]
fn udf_250_image_of_the_build() {
    let Some((_, tools)) = tools() else { return };
    let Some((dir, report)) = fixture() else { return };
    let iso = dir.join("bdtest.iso");
    let plan = udf250::create_bd_image(&report.project_dir, &iso, "BDTEST", &AtomicBool::new(false), |_| {}).unwrap();
    let img = std::fs::read(&iso).unwrap();
    assert_eq!(img.len() as u64, plan.size_bytes());
    // The volume descriptor: revision 2.50, physical + metadata partition map.
    let lvd = &img[35 * 2048..36 * 2048];
    assert_eq!(u16::from_le_bytes([lvd[216 + 24], lvd[216 + 25]]), 0x0250, "domain identifier suffix");
    assert_eq!(u32::from_le_bytes(lvd[268..272].try_into().unwrap()), 2, "two partition maps");
    assert_eq!((lvd[440], lvd[446]), (1, 2), "physical map, then metadata map (type 2)");
    assert_eq!(&lvd[451..451 + 23], b"*UDF Metadata Partition");

    // ffmpeg's bluray protocol (libbluray + libudfread) is a real UDF 2.50 reader. libbluray
    // ignores playlists under 3 minutes, so a longer, trivially compressible project is used;
    // the plain folder is the control that shows the reader works at all.
    let protocols = Command::new(&tools.ffmpeg).args(["-hide_banner", "-protocols"]).output().unwrap();
    if String::from_utf8_lossy(&protocols.stdout).lines().any(|l| l.trim() == "bluray") {
        let (rt, _) = tools_pair();
        let long = dir.join("long");
        ffmpeg(&tools, &["-f", "lavfi", "-i", "color=c=0x2060a0:s=1280x720:r=25:d=200", "-f", "lavfi", "-i", "sine=f=220:d=200", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3", long.with_extension("mkv").to_str().unwrap()]);
        let opts = BuildOptions {
            inputs: vec![long.with_extension("mkv")],
            disc: DiscType::Bd25,
            output: long.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            project_name: Some("LONG".into()),
            ..BuildOptions::default()
        };
        let long_report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
        let long_iso = dir.join("long.iso");
        udf250::create_bd_image(&long_report.project_dir, &long_iso, "LONG", &AtomicBool::new(false), |_| {}).unwrap();
        let streams = |source: &Path| {
            let url = format!("bluray:{}", source.to_string_lossy().replace('\\', "/"));
            let out = Command::new(&tools.ffprobe)
                .args(["-v", "error", "-playlist", "0", "-show_entries", "stream=codec_name", "-of", "csv=p=0", &url])
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let (ok, text, err) = streams(&long_report.project_dir);
        assert!(ok && text.contains("h264"), "libbluray cannot read the folder: {text} {err}");
        let (ok, text, err) = streams(&long_iso);
        assert!(ok && text.contains("h264"), "libbluray cannot read the UDF 2.50 image: {text} {err}");
    } else {
        eprintln!("bluray protocol not in this ffmpeg build: libbluray check skipped");
    }

    #[cfg(windows)]
    {
        // Windows' own UDF driver mounts the image; every file is byte-identical.
        let script = format!(
            "$ErrorActionPreference='Stop'; $d = Mount-DiskImage -ImagePath '{}' -StorageType ISO -PassThru; $v = $d | Get-Volume; Write-Output \"$($v.DriveLetter)|$($v.FileSystem)\"",
            iso.display()
        );
        let _mounted = Mounted(iso.clone());
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().unwrap();
        if !out.status.success() {
            eprintln!("Mount-DiskImage failed, mount check skipped: {}", String::from_utf8_lossy(&out.stderr));
            return;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (letter, fs) = text.trim().split_once('|').unwrap_or_else(|| panic!("unexpected output {text}"));
        assert_eq!(fs, "UDF", "mounted as {text}");
        let root = PathBuf::from(format!("{letter}:\\"));
        let mut files = 0;
        for top in ["BDMV", "CERTIFICATE"] {
            compare_tree(&report.project_dir.join(top), &root.join(top), &mut files);
        }
        assert!(files >= 5, "{files} files compared");
    }
}

// ---------------------------------------------------------------------------
// Menus from the menu editor (HDMV / IGS)
// ---------------------------------------------------------------------------

mod menus {
    use super::*;
    use musadvd::layout::{Action, Background, Canvas, Content, CustomLayout, CustomMenu, Element, Highlight, MenuRole, MenuSound};

    fn button(y: f64, text: &str, action: Action) -> Element {
        Element { indicator_for: None,
            id: String::new(),
            content: Content::Text {
                text: text.into(),
                font: "Arial".into(),
                size: 54.0,
                color: "#ffffff".into(),
                align: "left".into(),
                bold: false,
                italic: false,
                shadow: true,
                wrap: false,
                bg_color: None,
                bg_opacity: None,
                rx: None,
                ry: None,
                padding: None,
                shadow_color: None,
                shadow_opacity: None,
                shadow_x: None,
                shadow_y: None,
                shadow_blur: None,
                outline_color: None,
                outline_width: None,
            },
            x: 160.0,
            y,
            w: 900.0,
            h: 90.0,
            rotation: 0.0,
            opacity: 1.0,
            action: Some(action),
            highlight: None,
            group: None,
            intro: Vec::new(),
            easter_egg: false,
            nav: None,
            auto_execute: false,
        }
    }

    fn menu(id: &str, role: MenuRole, title: Option<u32>, elements: Vec<Element>) -> CustomMenu {
        CustomMenu { play_mode: None, timeout_secs: None,
            id: id.into(),
            name: id.into(),
            role,
            title,
            background: Background { color: "#102040".into(), ..Background::default() },
            duration_secs: None,
            audio: MenuSound::None,
            intro_secs: None,
            group: None,
            elements,
        }
    }

    /// Title menu, root menu of title 1 and its settings menu.
    fn layout() -> CustomLayout {
        CustomLayout { title_screen: None, chapter_menus: None, aspect: None,
            version: 1,
            canvas: Canvas { width: 1920, height: 1080 },
            highlight: Highlight::default(),
            animations: true,
            fast_play: None,
            theme: None,
            settings_mode: None,
            menus: vec![
                menu("main", MenuRole::Title, None, vec![
                    button(200.0, "Film 1", Action::PlayTitle { title: 1 }),
                    button(320.0, "Film 2", Action::PlayTitle { title: 2 }),
                    button(440.0, "Alle abspielen", Action::PlayAll),
                    button(560.0, "Kapitel 2", Action::PlayChapter { title: 1, chapter: 2 }),
                ]),
                menu("root", MenuRole::Root, Some(1), vec![
                    button(200.0, "Abspielen", Action::PlayTitle { title: 1 }),
                    button(320.0, "Einstellungen", Action::menu("set")),
                ]),
                menu("set", MenuRole::Settings, Some(1), vec![
                    button(200.0, "Ton 2", Action::SetAudio { source: 2 }),
                    button(320.0, "Untertitel an", Action::SetSubtitle { source: Some(3) }),
                    button(440.0, "Untertitel aus", Action::SetSubtitle { source: None }),
                    button(560.0, "Zurück", Action::menu("root")),
                ]),
            ],
        }
    }

    /// First segment type of every PES that starts on `pid`.
    fn first_segment_types(m2ts: &Path, pid: u16) -> Vec<u8> {
        let d = std::fs::read(m2ts).unwrap();
        d.as_chunks::<192>()
            .0
            .iter()
            .filter_map(|pkt| {
                let p = &pkt[4..];
                let this = (u16::from_be_bytes([p[1], p[2]])) & 0x1FFF;
                (this == pid && p[1] & 0x40 != 0).then(|| {
                    let off = if p[3] & 0x20 != 0 { 5 + p[4] as usize } else { 4 };
                    p[off + 9 + p[off + 8] as usize]
                })
            })
            .collect()
    }

    fn options(dir: &Path, inputs: Vec<PathBuf>, layout: CustomLayout) -> BuildOptions {
        BuildOptions {
            inputs,
            disc: DiscType::Bd25,
            output: dir.join("out"),
            resolution: Resolution::Hd720,
            fast_encode: true,
            hw_accel: HwAccelMode::None,
            project_name: Some("BDMENU".into()),
            custom_layout: Some(layout),
            selections: vec![
                StreamSelection { audio: Some(vec![1, 2]), subtitles: Some(vec![3]), label: None, chapters: Some(vec![0.0, 6.0]), ..StreamSelection::default() },
                StreamSelection::default(),
            ],
            ..BuildOptions::default()
        }
    }

    #[test]
    fn layout_builds_menus_and_navigation() {
        let Some((rt, tools)) = tools() else { return };
        let dir = std::env::temp_dir().join(format!("cx_bdmenu_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = make_clips(&tools, &dir.join("clips"));
        let report = build(&rt, &options(&dir, vec![a, b], layout())).unwrap_or_else(|e| panic!("build failed: {e}"));
        let bd = report.project_dir.as_path();
        assert_eq!(report.menu_pages, 3);
        // Playlists / clips: 0..2 menus, 3 and 4 titles.
        for n in 0..5 {
            for f in [format!("PLAYLIST/{n:05}.mpls"), format!("CLIPINF/{n:05}.clpi"), format!("STREAM/{n:05}.m2ts")] {
                assert!(bd.join("BDMV").join(&f).is_file(), "{f} missing");
                if !f.starts_with("STREAM") {
                    assert!(bd.join("BDMV/BACKUP").join(&f).is_file(), "BACKUP/{f} missing");
                }
            }
        }
        assert!(!bd.join("BDMV/STREAM/00005.m2ts").exists());

        // Menus: 720p H.264 + AC-3 + an IG stream (PID 0x1400; ICS = 0x18, PDS 0x14, ODS 0x15).
        for n in 0..3 {
            let m2ts = bd.join(format!("BDMV/STREAM/{n:05}.m2ts"));
            let v = probe(&tools, &m2ts, "v:0", "stream=codec_name,width,height");
            assert!(v.contains("codec_name=h264") && v.contains("width=1280"), "{v}");
            let ids = probe(&tools, &m2ts, "", "stream=id");
            assert!(ids.contains("id=0x1400"), "menu {n} has no IG stream: {ids}");
            let segs = first_segment_types(&m2ts, 0x1400);
            assert_eq!(segs.first(), Some(&0x18), "ICS first: {segs:?}");
            assert!(segs.contains(&0x14) && segs.contains(&0x15) && segs.last() == Some(&0x80), "{segs:?}");
            assert!(first_segment_types(&m2ts, 0x1200).is_empty(), "no PG PID left");
            // CLPI and MPLS (and their BACKUP copies) say IG, not PG.
            for dir in ["BDMV", "BDMV/BACKUP"] {
                let clpi = std::fs::read(bd.join(dir).join(format!("CLIPINF/{n:05}.clpi"))).unwrap();
                assert!(clpi.windows(4).any(|w| w == [0x14, 0x00, 0x15, 0x91]) && !clpi.windows(4).any(|w| w == [0x12, 0x00, 0x15, 0x90]));
                let mpls = std::fs::read(bd.join(dir).join(format!("PLAYLIST/{n:05}.mpls"))).unwrap();
                let item = u32::from_be_bytes(mpls[8..12].try_into().unwrap()) as usize + 10;
                // still_mode 2 (infinite), still_time 0: mode 1 with time 0 ends the item on a PS4.
                assert_eq!(mpls[item + 31..item + 34], [2, 0, 0], "still mode of a still menu");
                let stn = item + 34;
                assert_eq!((mpls[stn + 6], mpls[stn + 7]), (0, 1), "no PG, one IG");
            }
        }
        // Titles keep their subtitle track as PGS; title 1 has chapters 0 / 6 s (two marks).
        let t1 = bd.join("BDMV/STREAM/00003.m2ts");
        assert_eq!(probe(&tools, &t1, "s", "stream=codec_name").matches("hdmv_pgs_subtitle").count(), 1);
        let mpls = std::fs::read(bd.join("BDMV/PLAYLIST/00003.mpls")).unwrap();
        let marks = u32::from_be_bytes(mpls[12..16].try_into().unwrap()) as usize;
        assert_eq!(u16::from_be_bytes([mpls[marks + 4], mpls[marks + 5]]), 2, "PlayListMark count of title 1");

        // index.bdmv: First Play -> object 0, Top Menu -> object 1, titles -> objects 5 and 6.
        let index = std::fs::read(bd.join("BDMV/index.bdmv")).unwrap();
        assert_eq!(&index[..8], b"INDX0200");
        let obj = |at: usize| u16::from_be_bytes([index[at + 6], index[at + 7]]);
        assert_eq!((obj(0x52), obj(0x5E)), (0, 1));
        assert_eq!(u16::from_be_bytes([index[0x6A], index[0x6B]]), 2);
        assert_eq!((obj(0x6C), obj(0x78)), (5, 6));
        // MovieObject.bdmv: 2 + 3 menus + 2 titles = 7 objects.
        let mobj = std::fs::read(bd.join("BDMV/MovieObject.bdmv")).unwrap();
        assert_eq!(u16::from_be_bytes([mobj[0x30], mobj[0x31]]), 7);
        assert_eq!(std::fs::read(bd.join("BDMV/BACKUP/MovieObject.bdmv")).unwrap(), mobj);
        assert!(bd.join("CERTIFICATE/BACKUP").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two merged inputs form one menu entry but stay two playlists chained by the navigation.
    #[test]
    fn merged_inputs_chain_their_playlists() {
        let Some((rt, tools)) = tools() else { return };
        let dir = std::env::temp_dir().join(format!("cx_bdmerge_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = make_clips(&tools, &dir.join("clips"));
        let mut l = layout();
        l.menus = vec![menu("main", MenuRole::Title, None, vec![
            button(200.0, "Film", Action::PlayTitle { title: 1 }),
            button(320.0, "Alle abspielen", Action::PlayAll),
        ])];
        let mut opts = options(&dir, vec![a, b], l);
        // One audio track and no subtitles on both, so the files may be merged.
        opts.selections = vec![
            StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), ..StreamSelection::default() },
            StreamSelection { merge_with_previous: true, ..StreamSelection::default() },
        ];
        let report = build(&rt, &opts).unwrap_or_else(|e| panic!("build failed: {e}"));
        let bd = report.project_dir.as_path();
        // Playlists: 0 menu, 1 and 2 the merged files.
        for n in 0..3 {
            assert!(bd.join(format!("BDMV/PLAYLIST/{n:05}.mpls")).is_file(), "playlist {n}");
        }
        assert!(!bd.join("BDMV/PLAYLIST/00003.mpls").exists());
        // index.bdmv: both files are titles (objects 3 and 4); the first one jumps on to title 2.
        let index = std::fs::read(bd.join("BDMV/index.bdmv")).unwrap();
        assert_eq!(u16::from_be_bytes([index[0x6A], index[0x6B]]), 2);
        let mobj = std::fs::read(bd.join("BDMV/MovieObject.bdmv")).unwrap();
        assert_eq!(u16::from_be_bytes([mobj[0x30], mobj[0x31]]), 5, "2 + 1 menu + 2 files");
        let jump: Vec<u8> = musadvd::hdmv::jump_title(2).into_iter().collect();
        assert!(mobj.windows(jump.len()).any(|w| w == jump.as_slice()), "file 1 ends with JumpTitle 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_layouts_fail_before_any_work() {
        let Some((rt, tools)) = tools() else { return };
        let dir = std::env::temp_dir().join(format!("cx_bdmenu_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, _) = make_clips(&tools, &dir.join("clips"));
        let mut l = layout();
        l.canvas = Canvas { width: 720, height: 576 };
        let err = build(&rt, &options(&dir, vec![a], l)).unwrap_err().to_string();
        assert!(err.contains("16:9"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Per-title Blu-ray video: an in-spec 1080p23.976 H.264 clip is stream-copied (auto mode),
/// a 720p25 clip (ultrafast x264: Constrained Baseline, no BD profile) forced to 720p50 by its title override is re-encoded at that rate, and strict
/// mode refuses the clip that cannot be copied.
#[test]
fn compliant_video_is_copied_and_titles_keep_their_own_format() {
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_vcopy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (a, _) = make_clips(&tools, &dir.join("clips"));
    let film = dir.join("clips/film.mkv");
    ffmpeg(
        &tools,
        &[
            "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=24000/1001:d=8", "-f", "lavfi", "-i", "sine=f=440:d=8", "-c:v", "libx264",
            "-preset", "veryfast", "-profile:v", "high", "-level:v", "4.1", "-pix_fmt", "yuv420p", "-bluray-compat", "1", "-g", "24", "-b:v", "6M",
            "-c:a", "ac3", film.to_str().unwrap(),
        ],
    );
    let one = || StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), ..StreamSelection::default() };
    let opts = BuildOptions {
        inputs: vec![film.clone(), a.clone()],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd1080,
        fast_encode: true,
        bd_video: musadvd::encoder::BdVideoMode::Auto,
        selections: vec![one(), StreamSelection { bd_resolution: Some("720".into()), bd_rate: Some("50".into()), ..one() }],
        ..BuildOptions::default()
    };
    let mut warnings = Vec::new();
    let report = rt
        .block_on(dvd::build_dvd(&opts, &mut |e| {
            if let dvd::Event::Warning(w) = e {
                warnings.push(w)
            }
        }))
        .unwrap_or_else(|e| panic!("build failed: {e}"));
    assert!(warnings.iter().any(|w| w.starts_with("a:") && w.contains("re-encoded")), "{warnings:?}");
    assert!(!warnings.iter().any(|w| w.starts_with("film:") && w.contains("video is re-encoded")), "the film title is copied: {warnings:?}");
    let stream = |n: usize| report.project_dir.join(format!("BDMV/STREAM/{n:05}.m2ts"));
    let copied = probe(&tools, &stream(0), "v:0", "stream=width,height,r_frame_rate,profile,level");
    for want in ["width=1920", "height=1080", "r_frame_rate=24000/1001", "profile=High", "level=41"] {
        assert!(copied.contains(want), "{want} in {copied}");
    }
    // The copied stream decodes cleanly after the BDAV mux.
    let decode = Command::new(&tools.ffmpeg).args(["-v", "error", "-i"]).arg(stream(0)).args(["-f", "null", "-"]).output().unwrap();
    assert!(decode.status.success() && decode.stderr.is_empty(), "{}", String::from_utf8_lossy(&decode.stderr));
    // The source had no HRD and no timing SEI (x264 without --nal-hrd); the muxer added both,
    // and ffmpeg's own header parser reads them back.
    let source = headers(&tools, &film);
    assert!(source.contains("nal_hrd_parameters_present_flag 0") && !source.contains("payload_type 1"), "{source}");
    let muxed = headers(&tools, &stream(0));
    for want in ["nal_hrd_parameters_present_flag 1", "time_scale 48000", "num_units_in_tick 1001", "bit_rate_value_minus1[0] 624999", "payload_type 0", "payload_type 1"] {
        assert!(muxed.contains(want), "{want} in {muxed}");
    }
    assert!(!muxed.to_lowercase().contains("error"), "{muxed}");
    let encoded = probe(&tools, &stream(1), "v:0", "stream=width,height,r_frame_rate");
    for want in ["width=1280", "height=720", "r_frame_rate=50/1"] {
        assert!(encoded.contains(want), "{want} in {encoded}");
    }
    assert!(report.titles[0].video_kbps > 0, "the copy reports its own bitrate");

    // Strict: the 720p25 clip has no Blu-ray format, so nothing is built.
    let strict = BuildOptions { inputs: vec![a], output: dir.join("strict"), bd_video: musadvd::encoder::BdVideoMode::Strict, selections: vec![one()], ..opts };
    let err = build(&rt, &strict).unwrap_err().to_string();
    assert!(err.contains("cannot be copied"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Copy candidates that pass the format checks but not the stream scan are re-encoded: a
/// 0.6 s bitrate burst overflows the disc's video buffer, a 10 s GOP has too few seek points.
#[test]
fn burst_and_long_gop_sources_fall_back_to_encoding() {
    let Some((rt, tools)) = tools() else { return };
    let dir = std::env::temp_dir().join(format!("cx_bluray_scan_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clip = |name: &str, filter: &str, extra: &[&str]| {
        let path = dir.join(name);
        let mut args = vec!["-f", "lavfi", "-i", filter, "-f", "lavfi", "-i", "sine=f=440:d=12", "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level:v", "4.1", "-pix_fmt", "yuv420p"];
        args.extend_from_slice(extra);
        args.extend(["-c:a", "ac3", "-shortest", path.to_str().unwrap()]);
        ffmpeg(&tools, &args);
        path
    };
    let burst = clip(
        "burst.mkv",
        "color=c=gray:s=1920x1080:r=24000/1001:d=12,noise=alls=60:allf=t+u:enable='between(t,5,5.6)'",
        &["-x264-params", "keyint=24:min-keyint=24", "-qp", "14"],
    );
    let long_gop = clip("gop.mkv", "testsrc2=s=1920x1080:r=24000/1001:d=12", &["-x264-params", "keyint=240:min-keyint=240:scenecut=0", "-b:v", "6M"]);
    let one = || StreamSelection { audio: Some(vec![1]), subtitles: Some(vec![]), ..StreamSelection::default() };
    let opts = BuildOptions {
        inputs: vec![burst, long_gop],
        disc: DiscType::Bd25,
        output: dir.join("out"),
        resolution: Resolution::Hd1080,
        fast_encode: true,
        bd_video: musadvd::encoder::BdVideoMode::Auto,
        selections: vec![one(), one()],
        ..BuildOptions::default()
    };
    let mut warnings = Vec::new();
    rt.block_on(dvd::build_dvd(&opts, &mut |e| {
        if let dvd::Event::Warning(w) = e {
            warnings.push(w)
        }
    }))
    .unwrap_or_else(|e| panic!("build failed: {e}"));
    assert!(warnings.iter().any(|w| w.starts_with("burst:") && w.contains("exceeds the disc's video buffer")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.starts_with("gop:") && w.contains("keyframes up to 240 frames apart")), "{warnings:?}");
    std::fs::remove_dir_all(&dir).ok();
}
