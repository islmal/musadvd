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

use convertx_rust::tools::blocking_tool_command;
use tauri::State;

use crate::AppState;

pub struct PreviewServer {
    port: u16,
    ffmpeg: OnceLock<PathBuf>,
    tokens: Mutex<HashMap<String, PathBuf>>,
    active: Mutex<Option<Child>>,
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
        let path = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).get(&req.token).cloned();
        let (Some(path), Some(ffmpeg)) = (path, self.ffmpeg.get()) else { return respond(sock, 404) };

        let mut cmd = blocking_tool_command(ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-ss", &format!("{:.3}", req.t), "-i"])
            .arg(&path)
            .args(["-map", "0:v:0"]);
        match req.a {
            Some(a) => cmd.args(["-map", &format!("0:{a}?")]),
            None => cmd.arg("-an"),
        };
        cmd.args(["-vf", "scale=-2:480", "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "25"])
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

/// Registers `path` for streaming and returns the base URL (the frontend adds `?t=..&a=..`).
#[tauri::command]
pub async fn preview_stream(server: State<'_, Arc<PreviewServer>>, state: State<'_, AppState>, path: String) -> Result<String, String> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(format!("Datei nicht gefunden: {}", path.display()));
    }
    let tools = crate::toolchain(&state).await?;
    let _ = server.ffmpeg.set(tools.ffmpeg.clone());
    let token = new_token();
    server.tokens.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), path);
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

    #[test]
    fn tokens_are_unique() {
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 32);
    }
}
