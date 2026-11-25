//! Live preview player: a tiny loopback HTTP server that streams ffmpeg's fragmented MP4 output
//! (480p H.264/AAC) to the `<video>` element of the "Schnitt & Bild" tab.
//!
//! `preview_stream` registers a file under a random token and returns `http://127.0.0.1:<port>/s/<token>`;
//! the frontend appends `?t=<secs>&a=<ffprobe audio index>`. A seek is a new request with a new `t`
//! (ffmpeg output timestamps start at 0). At most ONE preview ffmpeg runs: a new stream, `preview_stop`
//! and the end of a connection kill and wait the active child (kill/wait/spawn happen under one lock).

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use musadvd::encoder::{preview_filter, VideoProcessing};
use musadvd::probe::{self, Crop, VideoStream};
use musadvd::tools::{blocking_tool_command, hwaccel_input_args};
use tauri::State;

use crate::AppState;

pub struct PreviewServer {
    port: u16,
    ffmpeg: OnceLock<PathBuf>,
    tokens: Mutex<HashMap<String, Entry>>,
    active: Mutex<Option<Child>>,
}

/// A registered source: probed once per token, with the crop already applied.
#[derive(Clone)]
struct Entry {
    path: PathBuf,
    video: VideoStream,
    vp: VideoProcessing,
    hwaccel: Option<&'static str>,
}

#[derive(Debug, PartialEq)]
struct Req {
    token: String,
    t: f64,
    a: Option<u32>,
}

/// Parses `GET /s/<token>?t=<secs>&a=<idx> HTTP/1.1`; `Err` carries the HTTP status.
fn parse_request(line: &str) -> Result<Req, u16> {
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().ok_or(400u16)?, parts.next().ok_or(400u16)?);
    if method != "GET" {
        return Err(400);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let token = path.strip_prefix("/s/").filter(|t| !t.is_empty() && !t.contains('/')).ok_or(404u16)?;
    let (mut t, mut a) = (0.0, None);
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        match kv.split_once('=') {
            Some(("t", v)) => t = v.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0).ok_or(400u16)?,
            Some(("a", v)) => a = Some(v.parse::<u32>().map_err(|_| 400u16)?),
            _ => {}
        }
    }
    Ok(Req { token: token.to_string(), t, a })
}

fn new_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let half = |salt: u8| {
        let mut h = RandomState::new().build_hasher();
        h.write(&nanos.to_le_bytes());
        h.write_u32(std::process::id());
        h.write_u64(n);
        h.write_u8(salt);
        h.finish()
    };
    format!("{:016x}{:016x}", half(0), half(1))
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait(); // Windows: an unwaited child keeps its handle
}

fn respond(mut sock: TcpStream, code: u16) {
    let text = match code {
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Bad Request",
    };
    let _ = write!(sock, "HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

/// Builds the ordered `(hwaccel, filter)` attempts for a preview request.
///
/// HDR streams with unspecified `color_primaries` or `colorspace` fail `zscale` (`code 3074`);
/// the fallback injects BT.2020/BT.2020nc `setparams` before `zscale`, drops `-hwaccel` if
/// hardware decode fails, and finally falls back to a non-tonemapped 480p scale.
fn preview_attempts(entry: &Entry) -> Vec<(Option<&'static str>, String)> {
    let primary = preview_filter(&entry.video, entry.vp, 480);
    let mut attempts = vec![(entry.hwaccel, primary.clone())];
    if entry.video.hdr {
        let bt2020_vf = primary.replacen(
            "zscale=t=linear",
            "setparams=color_primaries=bt2020:colorspace=bt2020nc,zscale=t=linear",
            1,
        );
        if bt2020_vf != primary {
            if entry.hwaccel.is_some() {
                attempts.push((entry.hwaccel, bt2020_vf.clone()));
            }
            attempts.push((None, bt2020_vf));
        } else if entry.hwaccel.is_some() {
            attempts.push((None, primary));
        }
        let sdr = VideoStream { hdr: false, ..entry.video.clone() };
        attempts.push((None, preview_filter(&sdr, entry.vp, 480)));
    } else if entry.hwaccel.is_some() {
        attempts.push((None, primary));
    }
    attempts
}

fn build_ffmpeg_cmd(ffmpeg: &Path, entry: &Entry, req: &Req, hwaccel: Option<&str>, vf: &str) -> Command {
    let mut cmd = blocking_tool_command(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]);
    if let Some(hw) = hwaccel {
        cmd.args(hwaccel_input_args(hw));
    }
    cmd.args(["-ss", &format!("{:.3}", req.t), "-i"])
        .arg(&entry.path)
        .args(["-map", "0:v:0"]);
    match req.a {
        Some(a) => cmd.args(["-map", &format!("0:{a}?")]),
        None => cmd.arg("-an"),
    };
    // WebView2 cannot decode 10-bit H.264: force 8-bit High.
    cmd.args(["-vf", vf, "-pix_fmt", "yuv420p", "-profile:v", "high"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "25"])
        .args(["-c:a", "aac", "-ac", "2", "-f", "mp4", "-movflags", "frag_keyframe+empty_moov+default_base_moof", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

/// Reads the initial bytes from `stdout` (retrying `EINTR`) so `200 OK` is only sent once ffmpeg
/// has actually initialized and produced output.
fn read_first_chunk(stdout: &mut impl Read, buf: &mut [u8]) -> usize {
    loop {
        match stdout.read(buf) {
            Ok(n) => return n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return 0,
        }
    }
}

impl PreviewServer {
    pub fn start() -> io::Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let server = Arc::new(PreviewServer {
            port: listener.local_addr()?.port(),
            ffmpeg: OnceLock::new(),
            tokens: Mutex::default(),
            active: Mutex::default(),
        });
        let s = server.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let s = s.clone();
                std::thread::spawn(move || s.handle(sock));
            }
        });
        Ok(server)
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.active.lock().unwrap_or_else(|e| e.into_inner()).take() {
            kill(&mut c);
        }
    }

    fn handle(&self, sock: TcpStream) {
        let mut reader = BufReader::new(&sock);
        let mut line = String::new();
        if reader.by_ref().take(8192).read_line(&mut line).is_err() {
            return;
        }
        // Drain the headers (capped) so the client is not reset while we answer.
        let mut seen = 0;
        loop {
            let mut h = String::new();
            match reader.by_ref().take(8192).read_line(&mut h) {
                Ok(n) if n > 0 && h.trim_end() != "" && seen < 8192 => seen += n,
                _ => break,
            }
        }
        let req = match parse_request(&line) {
            Ok(r) => r,
            Err(code) => return respond(sock, code),
        };
        let entry = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).get(&req.token).cloned();
        let (Some(entry), Some(ffmpeg)) = (entry, self.ffmpeg.get()) else { return respond(sock, 404) };

        let mut first = [0u8; 4096];
        let mut active_stream = None;
        for (hw, vf) in preview_attempts(&entry) {
            let mut cmd = build_ffmpeg_cmd(ffmpeg, &entry, &req, hw, &vf);
            let (id, mut stdout) = {
                let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(mut old) = active.take() {
                    kill(&mut old);
                }
                let Ok(mut child) = cmd.spawn() else { return respond(sock, 400) };
                let Some(out) = child.stdout.take() else { return kill(&mut child) };
                let id = child.id();
                *active = Some(child);
                (id, out)
            };
            let n = read_first_chunk(&mut stdout, &mut first);
            if n > 0 {
                active_stream = Some((id, stdout, n));
                break;
            }
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            let still_mine = active.as_ref().is_some_and(|c| c.id() == id);
            if still_mine {
                if let Some(mut c) = active.take() {
                    kill(&mut c);
                }
            } else {
                // Superseded by a newer request or `stop()`; do not retry.
                return;
            }
        }

        let Some((id, mut stdout, n)) = active_stream else {
            return respond(sock, 502);
        };
        let mut sock = sock;
        let ok = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")
            .and_then(|()| sock.write_all(&first[..n]))
            .is_ok();
        if ok {
            let _ = io::copy(&mut stdout, &mut sock);
        }
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.as_ref().is_some_and(|c| c.id() == id) {
            if let Some(mut c) = active.take() {
                kill(&mut c);
            }
        }
    }
}

/// Per-title picture filter overrides passed from the preview player.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureOptions {
    #[serde(default)]
    pub denoise: Option<f32>,
    #[serde(default)]
    pub brightness: Option<f32>,
    #[serde(default)]
    pub contrast: Option<f32>,
    #[serde(default)]
    pub saturation: Option<f32>,
}

/// Registers `path` (with the user's `crop` and `picture` filters) for streaming and returns the
/// base URL (the frontend adds `?t=..&a=..`). A crop or picture change needs a new token.
#[tauri::command]
pub async fn preview_stream(
    server: State<'_, Arc<PreviewServer>>,
    state: State<'_, AppState>,
    path: String,
    crop: Option<Crop>,
    picture: Option<PictureOptions>,
) -> Result<String, String> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(format!("File not found: {}", path.display()));
    }
    let tools = crate::toolchain(&state).await?;
    let mut info = probe::probe(&tools.ffprobe, &path).await.map_err(|e| e.to_string())?;
    probe::apply_crop(&mut info, crop.unwrap_or_default())?;
    let video = info.video.ok_or("The file has no video stream")?;
    let _ = server.ffmpeg.set(tools.ffmpeg.clone());
    let settings = state.settings();
    let hwaccel = if settings.hw_decode { tools.probe_hwaccel(&path, settings.hw_accel_mode).await } else { None };
    let token = new_token();
    let p = picture.unwrap_or_default();
    let vp = settings
        .video_processing()
        .with_picture(p.denoise, p.brightness, p.contrast, p.saturation);
    let entry = Entry { path, video, vp, hwaccel };
    server.tokens.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), entry);
    Ok(format!("http://127.0.0.1:{}/s/{token}", server.port))
}

/// `async` so killing and waiting for ffmpeg runs off the UI thread (plain commands run on it).
#[tauri::command]
pub async fn preview_stop(server: State<'_, Arc<PreviewServer>>) -> Result<(), String> {
    server.stop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_requests() {
        let r = parse_request("GET /s/abc?t=12.5&a=3 HTTP/1.1").unwrap();
        assert_eq!(r, Req { token: "abc".into(), t: 12.5, a: Some(3) });
        assert_eq!(parse_request("GET /s/abc?t=0 HTTP/1.1").unwrap().a, None);
        assert_eq!(parse_request("GET /s/abc HTTP/1.1").unwrap().t, 0.0);
        assert_eq!(parse_request("GET /s/abc?t=1&x=y HTTP/1.1").unwrap().t, 1.0);
    }

    #[test]
    fn rejects_bad_requests() {
        for bad in ["GET /s/abc?t=-1 HTTP/1.1", "GET /s/abc?t=NaN HTTP/1.1", "GET /s/abc?t=x HTTP/1.1", "GET /s/abc?a=x HTTP/1.1", "POST /s/abc HTTP/1.1", ""] {
            assert_eq!(parse_request(bad), Err(400), "{bad}");
        }
        assert_eq!(parse_request("GET /other?t=1 HTTP/1.1"), Err(404));
        assert_eq!(parse_request("GET /s/ HTTP/1.1"), Err(404));
    }

    fn get(port: u16, target: &str) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").map_or(raw.len(), |i| i + 4);
        (String::from_utf8_lossy(&raw[..split.min(raw.len())]).into_owned(), raw[split..].to_vec())
    }

    /// 10-bit PQ HEVC in, 8-bit High H.264 480p fragmented MP4 out.
    #[test]
    fn streams_hdr_source_as_8bit_h264() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bin");
        let (ffmpeg, ffprobe) = (bin.join("ffmpeg.exe"), bin.join("ffprobe.exe"));
        if !ffmpeg.is_file() || !ffprobe.is_file() {
            eprintln!("skipped: bin/ffmpeg.exe or ffprobe.exe missing");
            return;
        }
        let dir = std::env::temp_dir().join(format!("cx-preview-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hdr.mkv");
        let st = blocking_tool_command(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=25:d=2", "-vf", "format=yuv420p10le"])
            .args(["-c:v", "libx265", "-x265-params", "log-level=error:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(st.success());

        let server = PreviewServer::start().unwrap();
        let _ = server.ffmpeg.set(ffmpeg);
        let video = VideoStream {
            index: 0,
            codec: "hevc".into(),
            width: 640,
            height: 360,
            display_aspect: 16.0 / 9.0,
            fps: 25.0,
            field_order: "progressive".into(),
            crop: None,
            hdr: true,
            profile: None,
            level: None,
            pix_fmt: String::new(),
            bit_rate: None,
        };
        server.tokens.lock().unwrap().insert(
            "tok".into(),
            Entry { path: src, video, vp: VideoProcessing::default(), hwaccel: None },
        );

        let (head, body) = get(server.port, "/s/tok?t=0.5");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        for atom in [&b"ftyp"[..], b"moof"] {
            assert!(body.windows(4).any(|w| w == atom), "{}", String::from_utf8_lossy(atom));
        }
        let out = dir.join("out.mp4");
        std::fs::write(&out, &body).unwrap();
        let probe = blocking_tool_command(&ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,profile,pix_fmt,height", "-of", "default=nw=1"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).into_owned();
        let text = text.replace("\r\n", "\n");
        for want in ["codec_name=h264", "pix_fmt=yuv420p", "height=480"] {
            assert!(text.contains(want), "{want} not in {text}");
        }
        // `ultrafast` reports Constrained Baseline; what matters is that it is no `High 10`.
        assert!(!text.contains("High 10"), "{text}");

        assert!(get(server.port, "/s/unknown").0.starts_with("HTTP/1.1 404"));
        server.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokens_are_unique() {
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 32);
    }

    #[test]
    fn picture_options_serde_defaults() {
        let empty: PictureOptions = serde_json::from_str("{}").unwrap();
        assert_eq!((empty.denoise, empty.brightness, empty.contrast, empty.saturation), (None, None, None, None));
        let partial: PictureOptions = serde_json::from_str(r#"{"denoise":2.5,"saturation":1.2}"#).unwrap();
        assert_eq!((partial.denoise, partial.brightness, partial.contrast, partial.saturation), (Some(2.5), None, None, Some(1.2)));
    }

    #[test]
    fn preview_attempts_builds_hdr_and_hwaccel_fallbacks() {
        let hdr_video = VideoStream {
            index: 0,
            codec: "hevc".into(),
            width: 3840,
            height: 2160,
            display_aspect: 16.0 / 9.0,
            fps: 24.0,
            field_order: "progressive".into(),
            crop: None,
            hdr: true,
            profile: None,
            level: None,
            pix_fmt: String::new(),
            bit_rate: None,
        };
        let hdr_entry = Entry {
            path: PathBuf::from("4k.mkv"),
            video: hdr_video.clone(),
            vp: VideoProcessing::default(),
            hwaccel: Some("d3d11va"),
        };
        let attempts = preview_attempts(&hdr_entry);
        assert_eq!(attempts.len(), 4);
        assert_eq!(attempts[0].0, Some("d3d11va"));
        assert!(!attempts[0].1.contains("setparams="));
        assert_eq!(attempts[1].0, Some("d3d11va"));
        assert!(attempts[1].1.contains("setparams=color_primaries=bt2020:colorspace=bt2020nc,zscale=t=linear"));
        assert_eq!(attempts[2].0, None);
        assert!(attempts[2].1.contains("setparams=color_primaries=bt2020:colorspace=bt2020nc,zscale=t=linear"));
        assert_eq!(attempts[3].0, None);
        assert!(!attempts[3].1.contains("zscale"));

        let sdr_entry = Entry {
            video: VideoStream { hdr: false, ..hdr_video },
            ..hdr_entry
        };
        let sdr_attempts = preview_attempts(&sdr_entry);
        assert_eq!(sdr_attempts.len(), 2);
        assert_eq!(sdr_attempts[0].0, Some("d3d11va"));
        assert_eq!(sdr_attempts[1].0, None);
    }

    /// 4K 10-bit PQ HEVC with unspecified color primaries/matrix still streams as 8-bit 480p H.264.
    #[test]
    fn streams_4k_hdr_unspecified_colorspace_as_8bit_h264() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bin");
        let (ffmpeg, ffprobe) = (bin.join("ffmpeg.exe"), bin.join("ffprobe.exe"));
        if !ffmpeg.is_file() || !ffprobe.is_file() {
            eprintln!("skipped: bin/ffmpeg.exe or ffprobe.exe missing");
            return;
        }
        let dir = std::env::temp_dir().join(format!("cx-preview-4k-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("4k_hdr_unspec.mkv");
        let st = blocking_tool_command(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=3840x2160:r=24:d=1", "-vf", "format=yuv420p10le"])
            .args(["-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "log-level=error:transfer=smpte2084"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(st.success());

        let server = PreviewServer::start().unwrap();
        let _ = server.ffmpeg.set(ffmpeg);
        let video = VideoStream {
            index: 0,
            codec: "hevc".into(),
            width: 3840,
            height: 2160,
            display_aspect: 16.0 / 9.0,
            fps: 24.0,
            field_order: "progressive".into(),
            crop: None,
            hdr: true,
            profile: None,
            level: None,
            pix_fmt: String::new(),
            bit_rate: None,
        };
        server.tokens.lock().unwrap().insert(
            "tok4k".into(),
            Entry { path: src, video, vp: VideoProcessing::default(), hwaccel: None },
        );

        let (head, body) = get(server.port, "/s/tok4k?t=0.0");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        for atom in [&b"ftyp"[..], b"moof"] {
            assert!(body.windows(4).any(|w| w == atom), "{}", String::from_utf8_lossy(atom));
        }
        let out = dir.join("out_4k.mp4");
        std::fs::write(&out, &body).unwrap();
        let probe = blocking_tool_command(&ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,pix_fmt,height", "-of", "default=nw=1"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).replace("\r\n", "\n");
        for want in ["codec_name=h264", "pix_fmt=yuv420p", "height=480"] {
            assert!(text.contains(want), "{want} not in {text}");
        }

        server.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_502_when_ffmpeg_produces_no_output() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bin");
        let ffmpeg = bin.join("ffmpeg.exe");
        if !ffmpeg.is_file() {
            eprintln!("skipped: bin/ffmpeg.exe missing");
            return;
        }
        let server = PreviewServer::start().unwrap();
        let _ = server.ffmpeg.set(ffmpeg);
        let video = VideoStream {
            index: 0,
            codec: "hevc".into(),
            width: 3840,
            height: 2160,
            display_aspect: 16.0 / 9.0,
            fps: 24.0,
            field_order: "progressive".into(),
            crop: None,
            hdr: false,
            profile: None,
            level: None,
            pix_fmt: String::new(),
            bit_rate: None,
        };
        server.tokens.lock().unwrap().insert(
            "bad".into(),
            Entry {
                path: PathBuf::from("nonexistent_preview_input.mkv"),
                video,
                vp: VideoProcessing::default(),
                hwaccel: None,
            },
        );
        let (head, body) = get(server.port, "/s/bad?t=0");
        assert!(head.starts_with("HTTP/1.1 502"), "{head}");
        assert!(body.is_empty());
        server.stop();
    }
}
