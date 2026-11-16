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
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use musadvd::encoder::{preview_filter, VideoProcessing};
use musadvd::probe::{self, Crop, VideoStream};
use musadvd::tools::blocking_tool_command;
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
        _ => "Bad Request",
    };
    let _ = write!(sock, "HTTP/1.1 {code} {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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

        let mut cmd = blocking_tool_command(ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-ss", &format!("{:.3}", req.t), "-i"])
            .arg(&entry.path)
            .args(["-map", "0:v:0"]);
        match req.a {
            Some(a) => cmd.args(["-map", &format!("0:{a}?")]),
            None => cmd.arg("-an"),
        };
        // WebView2 cannot decode 10-bit H.264: force 8-bit High.
        cmd.args(["-vf", &preview_filter(&entry.video, entry.vp, 480), "-pix_fmt", "yuv420p", "-profile:v", "high"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "25"])
            .args(["-c:a", "aac", "-ac", "2", "-f", "mp4", "-movflags", "frag_keyframe+empty_moov+default_base_moof", "pipe:1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

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
        let mut sock = sock;
        let ok = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")
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

/// Registers `path` (with the user's `crop`) for streaming and returns the base URL (the frontend
/// adds `?t=..&a=..`). A crop change needs a new token.
#[tauri::command]
pub async fn preview_stream(
    server: State<'_, Arc<PreviewServer>>,
    state: State<'_, AppState>,
    path: String,
    crop: Option<Crop>,
) -> Result<String, String> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(format!("Datei nicht gefunden: {}", path.display()));
    }
    let tools = crate::toolchain(&state).await?;
    let mut info = probe::probe(&tools.ffprobe, &path).await.map_err(|e| e.to_string())?;
    probe::apply_crop(&mut info, crop.unwrap_or_default())?;
    let video = info.video.ok_or("Die Datei hat keine Videospur")?;
    let _ = server.ffmpeg.set(tools.ffmpeg.clone());
    let token = new_token();
    let entry = Entry { path, video, vp: state.settings().video_processing() };
    server.tokens.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), entry);
    Ok(format!("http://127.0.0.1:{}/s/{token}", server.port))
}

#[tauri::command]
pub fn preview_stop(server: State<'_, Arc<PreviewServer>>) {
    server.stop();
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
        };
        server.tokens.lock().unwrap().insert("tok".into(), Entry { path: src, video, vp: VideoProcessing::default() });

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
}
