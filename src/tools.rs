//! Locating and running the external tools natively: ffmpeg/ffprobe, the only ones left. DVD
//! authoring, subpicture muxing and the Blu-ray muxer run in-process ([`crate::author`],
//! [`crate::spumux`], [`crate::bd_mux`]).
//!
//! Lookup order for every tool:
//! 1. `bin/` next to the running executable (all-in-one distribution),
//! 2. `bin/` in the current directory (debug builds only, e.g. `cargo run` / `cargo test` from the project root),
//! 3. the system `PATH`.
//!

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::HwAccelMode;
use crate::encoder::BdEncoder;

#[derive(Debug)]
pub enum ToolError {
    NotFound(String),
    Io { tool: String, error: std::io::Error },
    Failed { tool: String, status: String, log: String },
    /// Menu button colour grouping gave up; localised where it is shown ([`crate::dvd::BuildError::message`]).
    MenuTooComplex { buttons: usize },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotFound(t) => write!(f, "{t}"),
            ToolError::Io { tool, error } => write!(f, "cannot run {tool}: {error}"),
            ToolError::Failed { tool, status, log } => write!(f, "{tool} failed ({status}):\n{log}"),
            ToolError::MenuTooComplex { buttons } => write!(f, "button group search exceeded its limit ({buttons} buttons)"),
        }
    }
}

impl std::error::Error for ToolError {}

/// CPU priority of the external tools ("Prozesspriorität").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessPriority {
    #[default]
    Normal,
    BelowNormal,
    /// Only runs when the CPU is otherwise idle.
    Idle,
}

impl ProcessPriority {
    /// Windows process creation flag (`*_PRIORITY_CLASS`), `None` for normal priority.
    pub fn creation_flag(self) -> Option<u32> {
        match self {
            ProcessPriority::Normal => None,
            ProcessPriority::BelowNormal => Some(0x0000_4000),
            ProcessPriority::Idle => Some(0x0000_0040),
        }
    }
}

/// `CREATE_NO_WINDOW`: the GUI runs without a console, so every console tool it starts would
/// otherwise open its own console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command for an external tool: no console window (Windows), `priority` as CPU priority,
/// stdin closed (callers may still redirect it), killed when dropped.
pub fn tool_command(program: impl AsRef<std::ffi::OsStr>, priority: ProcessPriority) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | priority.creation_flag().unwrap_or(0));
    #[cfg(not(windows))]
    let _ = priority;
    cmd
}

/// Blocking counterpart of [`tool_command`] for synchronous callers (burner backends, opening a
/// folder): no console window (Windows), stdin closed. Callers that wait for the process kill it
/// themselves on cancel – `std::process::Child` has no kill-on-drop.
pub fn blocking_tool_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Live telemetry of one ffmpeg run (from `-progress`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FfmpegProgress {
    /// 0..1 of the output duration.
    pub fraction: f64,
    /// Frames written so far.
    pub frame: u64,
    /// Current encoding speed in frames per second.
    pub fps: f64,
}

/// Parses one `-progress` line into `p`; returns `true` at the end of a report block.
fn parse_progress_line(line: &str, duration_secs: f64, p: &mut FfmpegProgress) -> Option<bool> {
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    match key.trim() {
        // `out_time_us` (and the misnamed `out_time_ms`) are microseconds.
        "out_time_us" => {
            if let (Ok(us), true) = (value.parse::<f64>(), duration_secs > 0.0) {
                p.fraction = (us / 1e6 / duration_secs).clamp(0.0, 1.0);
            }
        }
        "frame" => p.frame = value.parse().unwrap_or(p.frame),
        "fps" => p.fps = value.parse().unwrap_or(p.fps),
        "progress" => return Some(true),
        "out_time_ms" | "out_time" | "bitrate" | "total_size" | "dup_frames" | "drop_frames" | "speed" => {}
        k if k.starts_with("stream_") => {}
        _ => return None,
    }
    Some(false)
}

/// Reads ffmpeg's stderr (`-progress pipe:2`): progress blocks go to `progress`, everything else
/// is the error log.
async fn drive_ffmpeg_stderr(
    stderr: tokio::process::ChildStderr,
    duration_secs: f64,
    mut progress: impl FnMut(FfmpegProgress),
) -> String {
    let mut log = String::new();
    let mut p = FfmpegProgress::default();
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_progress_line(&line, duration_secs, &mut p) {
            Some(true) => progress(p),
            Some(false) => {}
            None => {
                log.push_str(&line);
                log.push('\n');
            }
        }
    }
    log
}

/// A raw RGBA stream a helper ffmpeg writes for [`Toolchain::ffmpeg_piped`].
#[derive(Debug, Clone)]
pub struct RawStream {
    /// Complete ffmpeg arguments, ending in `-f rawvideo -pix_fmt rgba pipe:1`.
    pub args: Vec<OsString>,
    /// Bytes of one frame (`width * height * 4`).
    pub frame_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct Toolchain {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Broken candidates that were skipped during detection.
    pub notes: Vec<String>,
    /// Priority of every ffmpeg process started by this toolchain.
    pub priority: ProcessPriority,
}

/// Splits a `PATH` value. Unlike `std::env::split_paths` a stray `"` in one entry (e.g.
/// `C:\Program Files\dotnet"`) does not swallow all following entries.
pub fn path_entries(path: &std::ffi::OsStr) -> Vec<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    path.to_string_lossy()
        .split(sep)
        .map(|e| e.trim().trim_matches('"'))
        .filter(|e| !e.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Searches `PATH` for an executable (`name.exe` on Windows).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let exe = exe_name(name);
    path_entries(&std::env::var_os("PATH")?)
        .into_iter()
        .map(|dir| dir.join(&exe))
        .find(|p| p.is_file())
}

/// The application's `bin/` directories: next to the executable, and (in debug builds only) in the current directory.
pub fn bundled_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(dir.join("bin"));
    }
    #[cfg(debug_assertions)]
    if let Ok(cwd) = std::env::current_dir() {
        let bin = cwd.join("bin");
        if !dirs.contains(&bin) {
            dirs.push(bin);
        }
    }
    dirs
}

/// All candidate locations of a tool, in lookup order: bundled `bin/` directories, then `PATH`.
pub fn tool_candidates(name: &str) -> Vec<PathBuf> {
    let exe = exe_name(name);
    let mut out: Vec<PathBuf> = bundled_bin_dirs().into_iter().map(|dir| dir.join(&exe)).filter(|p| p.is_file()).collect();
    if let Some(p) = find_on_path(name) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Finds a tool in the bundled `bin/` directories, falling back to `PATH` (existence only).
pub fn find_tool(name: &str) -> Option<PathBuf> {
    tool_candidates(name).into_iter().next()
}

/// Checks that a candidate actually starts. ffmpeg/ffprobe must also answer `-version`
/// successfully; this rejects e.g. scoop shims copied without their `.shim` file.
async fn tool_works(path: &Path, version_check: bool) -> bool {
    let mut cmd = tool_command(path, ProcessPriority::Normal);
    if version_check {
        cmd.arg("-version");
    }
    let run = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
    match tokio::time::timeout(std::time::Duration::from_secs(20), run).await {
        Ok(Ok(status)) => !version_check || status.success(),
        _ => false,
    }
}

/// First working candidate of `name`; skipped broken candidates are reported in `notes`.
async fn locate(name: &str, version_check: bool, hint: &str, notes: &mut Vec<String>) -> Result<PathBuf, ToolError> {
    for candidate in tool_candidates(name) {
        if tool_works(&candidate, version_check).await {
            return Ok(candidate);
        }
        notes.push(format!("{} ignored: does not start correctly", candidate.display()));
    }
    Err(ToolError::NotFound(format!("{name} not found in bin/ or on PATH ({hint})")))
}

impl Toolchain {
    pub async fn detect() -> Result<Self, ToolError> {
        let mut notes = Vec::new();
        let ff_hint = "scoop install ffmpeg, or copy the real ffmpeg.exe/ffprobe.exe into bin/";
        let ffmpeg = locate("ffmpeg", true, ff_hint, &mut notes).await?;
        let ffprobe = locate("ffprobe", true, ff_hint, &mut notes).await?;
        Ok(Toolchain {
            ffmpeg,
            ffprobe,
            notes,
            priority: ProcessPriority::Normal,
        })
    }

    /// Same tools, started with another CPU priority.
    pub fn with_priority(mut self, priority: ProcessPriority) -> Self {
        self.priority = priority;
        self
    }

    /// New command for `program` with this toolchain's priority (no console window).
    fn command(&self, program: &Path) -> Command {
        tool_command(program, self.priority)
    }

    /// Muxes the subpicture stream described by `xml` (menu masks or bitmap subtitles, see
    /// [`crate::spumux`]) from `input` into `output`, both file names in `dir`; returns a log line.
    pub async fn spumux(&self, dir: &Path, xml: &str, input: &str, output: &str, stream: u32) -> Result<String, ToolError> {
        let tool = format!("spumux ({xml})");
        let failed = |log: String| ToolError::Failed { tool: tool.clone(), status: String::new(), log };
        let open = |p: PathBuf, write: bool| {
            let f = if write { std::fs::File::create(&p) } else { std::fs::File::open(&p) };
            f.map_err(|error| ToolError::Io { tool: tool.clone(), error })
        };
        let text = std::fs::read_to_string(dir.join(xml)).map_err(|error| ToolError::Io { tool: tool.clone(), error })?;
        if !crate::spumux::handles_xml(&text) && !crate::spumux::is_menu_xml(&text) {
            return Err(failed("unsupported subpicture XML".into()));
        }
        let defs = crate::spumux::parse_xml(&text, dir).map_err(|e| failed(e.to_string()))?;
        let (input, output) = (open(dir.join(input), false)?, open(dir.join(output), true)?);
        let id = stream as u8;
        match tokio::task::spawn_blocking(move || crate::spumux::mux(input, output, defs, id)).await {
            Ok(Ok(s)) => Ok(format!("INFO: {} subtitles added, {} subtitles skipped, offset: {:.2}
", s.added, s.skipped, s.offset as f64 / 90000.0)),
            Ok(Err(crate::spumux::SpumuxError::TooComplex(n))) => Err(ToolError::MenuTooComplex { buttons: n }),
            Ok(Err(e)) => Err(failed(e.to_string())),
            Err(e) => Err(failed(e.to_string())),
        }
    }

    /// ffprobe with `args`; returns its stdout (e.g. JSON).
    pub async fn ffprobe_output(&self, args: &[OsString]) -> Result<String, ToolError> {
        let io = |error| ToolError::Io { tool: "ffprobe".into(), error };
        let out = self.command(&self.ffprobe).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).output().await.map_err(io)?;
        if !out.status.success() {
            let log = String::from_utf8_lossy(&out.stderr).into_owned();
            return Err(ToolError::Failed { tool: "ffprobe".into(), status: out.status.to_string(), log: tail(&log, 30) });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// ffmpeg with `args` (ending in `pipe:1`); returns its stdout.
    pub async fn ffmpeg_output(&self, args: &[OsString]) -> Result<Vec<u8>, ToolError> {
        let io = |error| ToolError::Io { tool: "ffmpeg".into(), error };
        let mut cmd = self.command(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().await.map_err(io)?;
        if !out.status.success() {
            let log = String::from_utf8_lossy(&out.stderr).into_owned();
            return Err(ToolError::Failed { tool: "ffmpeg".into(), status: out.status.to_string(), log: tail(&log, 30) });
        }
        Ok(out.stdout)
    }

    /// Runs ffmpeg with `args`, reporting progress (0..1) for an output of `duration_secs`.
    pub async fn ffmpeg(
        &self,
        args: &[OsString],
        duration_secs: f64,
        mut progress: impl FnMut(f64),
    ) -> Result<(), ToolError> {
        self.ffmpeg_with(args, duration_secs, |p| progress(p.fraction)).await
    }

    fn ffmpeg_command(&self, args: &[OsString]) -> Command {
        let mut cmd = self.command(&self.ffmpeg);
        // Progress goes to stderr, so stdout stays free for piped output.
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-progress", "pipe:2", "-nostats"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }

    /// Runs ffmpeg with `args` at `-loglevel info` and returns everything it wrote to stderr (the
    /// normal runs use `-loglevel error`, which hides reports such as loudnorm's measurement).
    pub async fn ffmpeg_report(&self, args: &[OsString]) -> Result<String, ToolError> {
        let mut cmd = self.command(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", "info"]).args(args).stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd.output().await.map_err(|error| ToolError::Io { tool: "ffmpeg".into(), error })?;
        let log = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() {
            Ok(log)
        } else {
            Err(ToolError::Failed { tool: "ffmpeg".into(), status: out.status.to_string(), log: tail(&log, 30) })
        }
    }

    /// Runs ffmpeg with `args`, reporting progress with frame/fps telemetry.
    pub async fn ffmpeg_with(
        &self,
        args: &[OsString],
        duration_secs: f64,
        mut progress: impl FnMut(FfmpegProgress),
    ) -> Result<(), ToolError> {
        let mut child =
            self.ffmpeg_command(args).spawn().map_err(|error| ToolError::Io { tool: "ffmpeg".into(), error })?;
        let stderr = child.stderr.take().expect("piped stderr");
        let log = drive_ffmpeg_stderr(stderr, duration_secs, &mut progress).await;
        let status = child.wait().await.map_err(|error| ToolError::Io { tool: "ffmpeg".into(), error })?;
        if status.success() {
            progress(FfmpegProgress { fraction: 1.0, ..Default::default() });
            Ok(())
        } else {
            Err(ToolError::Failed { tool: "ffmpeg".into(), status: status.to_string(), log: tail(&log, 30) })
        }
    }

    /// ffmpeg encoding a raw RGBA video it reads from `pipe:0` (`args` declare it with
    /// `-f rawvideo -pix_fmt rgba -s WxH -r R -i pipe:0`), with the frames made on the fly: frame `i`
    /// is `compose(i, current frames of the streams)`. Each of `streams` is a helper ffmpeg (complete
    /// `args` ending in `pipe:1`) writing raw frames of `frame_bytes` bytes to its stdout. Frames are
    /// made 8 at a time on the rayon pool and written in order; nothing touches the disk.
    pub async fn ffmpeg_piped(
        &self,
        args: &[OsString],
        streams: &[RawStream],
        frames: usize,
        duration_secs: f64,
        compose: Arc<dyn Fn(usize, &[&[u8]]) -> Vec<u8> + Send + Sync>,
        mut progress: impl FnMut(FfmpegProgress),
    ) -> Result<(), ToolError> {
        const CHUNK: usize = 8;
        let io = |tool: &str| {
            let tool = tool.to_string();
            move |error| ToolError::Io { tool: tool.clone(), error }
        };
        let mut main = self.ffmpeg_command(args);
        main.stdin(Stdio::piped());
        let mut main_child = main.spawn().map_err(io("ffmpeg"))?;
        // Taken out of the Option at the end: only dropping the handle closes the pipe (EOF for ffmpeg).
        let mut stdin = main_child.stdin.take();
        let stderr = main_child.stderr.take().expect("piped stderr");

        let mut readers = Vec::new();
        for s in streams {
            let mut cmd = self.command(&self.ffmpeg);
            cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"]).args(&s.args).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(io("ffmpeg"))?;
            let out = child.stdout.take().expect("piped stdout");
            let mut err = child.stderr.take().expect("piped stderr");
            let log = tokio::spawn(async move {
                let mut text = String::new();
                let _ = err.read_to_string(&mut text).await;
                text
            });
            readers.push((child, out, log, s.frame_bytes));
        }

        let feed = async {
            let mut next = 0;
            while next < frames {
                let n = CHUNK.min(frames - next);
                let mut raw: Vec<Vec<Vec<u8>>> = Vec::with_capacity(readers.len());
                for (_, out, _, bytes) in readers.iter_mut() {
                    let mut got = Vec::with_capacity(n);
                    for _ in 0..n {
                        let mut buf = vec![0u8; *bytes];
                        out.read_exact(&mut buf).await.map_err(|error| ToolError::Io { tool: "ffmpeg (Intro-Videos)".into(), error })?;
                        got.push(buf);
                    }
                    raw.push(got);
                }
                let compose = compose.clone();
                let made: Vec<Vec<u8>> = tokio::task::spawn_blocking(move || {
                    use rayon::prelude::*;
                    (0..n)
                        .into_par_iter()
                        .map(|i| {
                            let slots: Vec<&[u8]> = raw.iter().map(|s| s[i].as_slice()).collect();
                            compose(next + i, &slots)
                        })
                        .collect()
                })
                .await
                .map_err(|e| ToolError::Failed { tool: "intro compositing".into(), status: "panic".into(), log: e.to_string() })?;
                for frame in made {
                    stdin.as_mut().expect("open until the end").write_all(&frame).await.map_err(|error| ToolError::Io { tool: "ffmpeg".into(), error })?;
                }
                next += n;
            }
            drop(stdin.take());
            Ok(())
        };
        let (fed, log) = tokio::join!(feed, drive_ffmpeg_stderr(stderr, duration_secs, &mut progress));
        let status = main_child.wait().await.map_err(io("ffmpeg"))?;
        // A helper that failed explains the short read better than the encoder it starved.
        let mut helper_error = None;
        for (mut child, _, logs, _) in readers {
            let _ = child.kill().await;
            let text = logs.await.unwrap_or_default();
            if helper_error.is_none() && !text.trim().is_empty() {
                helper_error = Some(ToolError::Failed { tool: "ffmpeg (Intro-Videos)".into(), status: "error".into(), log: tail(&text, 30) });
            }
        }
        if let (Err(_), Some(e)) = (&fed, helper_error) {
            return Err(e);
        }
        if !status.success() {
            return Err(ToolError::Failed { tool: "ffmpeg".into(), status: status.to_string(), log: tail(&log, 30) });
        }
        fed?;
        progress(FfmpegProgress { fraction: 1.0, ..Default::default() });
        Ok(())
    }

    /// ffmpeg writing an MPEG program stream to its stdout (`args` must end with `pipe:1`),
    /// piped through one native subpicture muxer ([`crate::spumux`]) per `(xml, stream)` into
    /// `dir/output` – no intermediate files. Without stages ffmpeg's stdout goes straight into
    /// the file. Returns the stage logs.
    pub async fn ffmpeg_spumux_pipeline(
        &self,
        args: &[OsString],
        dir: &Path,
        stages: &[(String, u32)],
        output: &str,
        duration_secs: f64,
        mut progress: impl FnMut(FfmpegProgress),
    ) -> Result<Vec<String>, ToolError> {
        let io = |tool: &str| {
            let tool = tool.to_string();
            move |error| ToolError::Io { tool: tool.clone(), error }
        };
        let out_file = std::fs::File::create(dir.join(output)).map_err(io(output))?;
        let mut ffmpeg = self.ffmpeg_command(args);
        let mut out_file = Some(out_file);
        if stages.is_empty() {
            ffmpeg.stdout(out_file.take().unwrap());
        } else {
            ffmpeg.stdout(Stdio::piped());
        }
        let mut ff_child = ffmpeg.spawn().map_err(io("ffmpeg"))?;
        let mut upstream = ff_child.stdout.take().map(|s| owned_file(s)).transpose().map_err(io("ffmpeg"))?;
        let mut spumux = Vec::new();
        for (n, (xml, stream)) in stages.iter().enumerate() {
            let last = n + 1 == stages.len();
            let input = upstream.take().expect("piped stdout");
            let tool = format!("spumux ({xml})");
            let fail = |log: String| ToolError::Failed { tool: tool.clone(), status: String::new(), log };
            let text = std::fs::read_to_string(dir.join(xml)).map_err(|e| fail(e.to_string()))?;
            if !crate::spumux::handles_xml(&text) {
                return Err(fail("unsupported subtitle XML".into()));
            }
            let defs = crate::spumux::parse_xml(&text, dir).map_err(|e| fail(e.to_string()))?;
            let sink: Box<dyn std::io::Write + Send> = if last {
                Box::new(out_file.take().unwrap())
            } else {
                let (reader, writer) = std::io::pipe().map_err(io("spumux"))?;
                upstream = Some(owned_file(reader).map_err(io("spumux"))?);
                Box::new(writer)
            };
            let id = *stream as u8;
            spumux.push((tool, tokio::task::spawn_blocking(move || crate::spumux::mux(input, sink, defs, id))));
        }
        let stderr = ff_child.stderr.take().expect("piped stderr");
        let ff_log = drive_ffmpeg_stderr(stderr, duration_secs, &mut progress).await;
        let ff_status = ff_child.wait().await.map_err(io("ffmpeg"))?;
        let mut logs = Vec::new();
        let mut spumux_error = None;
        for (tool, task) in spumux {
            let (ok, log) = match task.await {
                Ok(Ok(s)) => (true, format!("INFO: {} subtitles added, {} subtitles skipped, offset: {:.2}
", s.added, s.skipped, s.offset as f64 / 90000.0)),
                Ok(Err(e)) => (false, e.to_string()),
                Err(e) => (false, e.to_string()),
            };
            if !ok && spumux_error.is_none() {
                spumux_error = Some(ToolError::Failed { tool, status: String::new(), log: tail(&log, 30) });
            }
            logs.push(log);
        }
        // A failing spumux makes ffmpeg fail with a broken pipe: report the root cause.
        if let Some(e) = spumux_error {
            return Err(e);
        }
        if !ff_status.success() {
            return Err(ToolError::Failed { tool: "ffmpeg".into(), status: ff_status.to_string(), log: tail(&ff_log, 30) });
        }
        progress(FfmpegProgress { fraction: 1.0, ..Default::default() });
        Ok(logs)
    }

    /// ffmpeg writing to its stdout (`args` must end with `pipe:1`), consumed by `sink` on a
    /// blocking thread (e.g. the native BDAV muxer); returns what `sink` returns.
    pub async fn ffmpeg_sink<T: Send + 'static>(
        &self,
        args: &[OsString],
        duration_secs: f64,
        mut progress: impl FnMut(f64),
        sink: impl FnOnce(std::fs::File) -> std::io::Result<T> + Send + 'static,
    ) -> Result<T, ToolError> {
        let io = |error| ToolError::Io { tool: "ffmpeg".into(), error };
        let mut cmd = self.ffmpeg_command(args);
        cmd.stdout(Stdio::piped());
        let mut child = cmd.spawn().map_err(io)?;
        let stdout = owned_file(child.stdout.take().expect("piped stdout")).map_err(io)?;
        let task = tokio::task::spawn_blocking(move || sink(stdout));
        let stderr = child.stderr.take().expect("piped stderr");
        let log = drive_ffmpeg_stderr(stderr, duration_secs, &mut |p: FfmpegProgress| progress(p.fraction)).await;
        let status = child.wait().await.map_err(io)?;
        // A failing sink makes ffmpeg fail with a broken pipe: report the root cause.
        let sunk = task.await.map_err(|e| ToolError::Failed { tool: "BDAV-Muxer".into(), status: String::new(), log: e.to_string() })?;
        let value = sunk.map_err(|e| ToolError::Failed { tool: "BDAV-Muxer".into(), status: String::new(), log: e.to_string() })?;
        if !status.success() {
            return Err(ToolError::Failed { tool: "ffmpeg".into(), status: status.to_string(), log: tail(&log, 30) });
        }
        progress(1.0);
        Ok(value)
    }

    /// Hardware decoder that works for the video of `input` (tried for 2 s of decoding), or
    /// `None` to decode on the CPU.
    pub async fn probe_hwaccel(&self, input: &Path, mode: HwAccelMode) -> Option<&'static str> {
        for &hw in hwaccel_candidates(mode) {
            let mut cmd = self.command(&self.ffmpeg);
            cmd.args(["-hide_banner", "-nostdin", "-loglevel", "warning", "-hwaccel", hw, "-t", "2", "-i"])
                .arg(input)
                .args(["-map", "0:v:0", "-f", "null", "-"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let run = cmd.output();
            let Ok(Ok(out)) = tokio::time::timeout(std::time::Duration::from_secs(30), run).await else { continue };
            let log = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
            // ffmpeg silently falls back to software decoding when the hwaccel fails to set up.
            if out.status.success() && !log.contains("failed") && !log.contains("error") {
                return Some(hw);
            }
        }
        None
    }

    /// Checks that a hardware encoder works with the Blu-ray options (10 test frames); `Err` is a
    /// short reason for the log.
    pub async fn probe_hw_encoder(&self, enc: BdEncoder) -> Result<(), String> {
        let mut cmd = self.command(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=24000/1001"])
            .args(["-frames:v", "10", "-pix_fmt", "yuv420p", "-c:v", enc.codec(), "-profile:v", "high", "-level:v", "4.1"])
            .args(enc.bd_options())
            .args(["-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let Ok(Ok(out)) = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await else {
            return Err(format!("{} test could not run", enc.label()));
        };
        if out.status.success() {
            return Ok(());
        }
        let log = String::from_utf8_lossy(&out.stderr);
        Err(if enc == BdEncoder::Nvenc {
            nvenc_failure_reason(&log)
        } else {
            log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim().into()
        })
    }
}

/// A pipe end as a blocking `File` (child stdout or `io::pipe` reader).
fn owned_file(pipe: impl IntoOwnedFile) -> std::io::Result<std::fs::File> {
    pipe.into_owned_file()
}

trait IntoOwnedFile {
    fn into_owned_file(self) -> std::io::Result<std::fs::File>;
}

impl IntoOwnedFile for tokio::process::ChildStdout {
    fn into_owned_file(self) -> std::io::Result<std::fs::File> {
        #[cfg(windows)]
        return Ok(self.into_owned_handle()?.into());
        #[cfg(unix)]
        return Ok(self.into_owned_fd()?.into());
    }
}

impl IntoOwnedFile for std::io::PipeReader {
    fn into_owned_file(self) -> std::io::Result<std::fs::File> {
        #[cfg(windows)]
        return Ok(std::os::windows::io::OwnedHandle::from(self).into());
        #[cfg(unix)]
        return Ok(std::os::fd::OwnedFd::from(self).into());
    }
}

/// Hardware decoders tried for a GPU vendor, in order, falling back to any other available
/// platform decoder so auto-detection still finds a working GPU decoder on mixed/other hardware.
pub fn hwaccel_candidates(mode: HwAccelMode) -> &'static [&'static str] {
    if cfg!(windows) {
        match mode {
            HwAccelMode::Nvidia => &["d3d11va", "cuda", "qsv"],
            HwAccelMode::Amd => &["d3d11va", "qsv", "cuda"],
            HwAccelMode::Intel => &["qsv", "d3d11va", "cuda"],
            HwAccelMode::None => &["d3d11va", "cuda", "qsv"],
        }
    } else {
        match mode {
            HwAccelMode::Nvidia => &["cuda", "vaapi", "qsv"],
            HwAccelMode::Amd => &["vaapi", "qsv", "cuda"],
            HwAccelMode::Intel => &["qsv", "vaapi", "cuda"],
            HwAccelMode::None => &["vaapi", "qsv", "cuda"],
        }
    }
}

/// Maps an ffmpeg NVENC failure log to a short German reason.
pub fn nvenc_failure_reason(log: &str) -> String {
    let l = log.to_ascii_lowercase();
    if l.contains("driver does not support") || l.contains("minimum required nvidia driver") {
        "NVIDIA driver too old (≥ 610 required)".into()
    } else if l.contains("no capable devices") || l.contains("cannot load") {
        "no NVIDIA GPU with NVENC".into()
    } else {
        log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim().into()
    }
}

fn tail(s: &str, lines: usize) -> String {
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn stray_quote_in_path() {
        let p = path_entries(std::ffi::OsStr::new(r#"C:\a;C:\Program Files\dotnet";C:\b; ;"C:\c d""#));
        assert_eq!(p, vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\Program Files\dotnet"), PathBuf::from(r"C:\b"), PathBuf::from(r"C:\c d")]);
    }

    #[test]
    fn hwaccel_lists() {
        if cfg!(windows) {
            assert_eq!(hwaccel_candidates(HwAccelMode::Nvidia), ["d3d11va", "cuda", "qsv"]);
            assert_eq!(hwaccel_candidates(HwAccelMode::Amd), ["d3d11va", "qsv", "cuda"]);
            assert_eq!(hwaccel_candidates(HwAccelMode::Intel), ["qsv", "d3d11va", "cuda"]);
            assert_eq!(hwaccel_candidates(HwAccelMode::None), ["d3d11va", "cuda", "qsv"]);
        } else {
            assert_eq!(hwaccel_candidates(HwAccelMode::Nvidia), ["cuda", "vaapi", "qsv"]);
            assert_eq!(hwaccel_candidates(HwAccelMode::Amd), ["vaapi", "qsv", "cuda"]);
            assert_eq!(hwaccel_candidates(HwAccelMode::Intel), ["qsv", "vaapi", "cuda"]);
            assert_eq!(hwaccel_candidates(HwAccelMode::None), ["vaapi", "qsv", "cuda"]);
        }
    }

    #[test]
    fn nvenc_reasons() {
        assert!(nvenc_failure_reason("Driver does not support the required nvenc API version. Required: 13.1 Found: 13.0").contains("driver"));
        assert!(nvenc_failure_reason("OpenEncodeSessionEx failed: no capable devices found").contains("GPU"));
        assert_eq!(nvenc_failure_reason("a\nsomething odd\n\n"), "something odd");
    }

    #[test]
    fn progress_lines() {
        let mut p = FfmpegProgress::default();
        let block = ["frame=250", "fps=125.5", "stream_0_0_q=3.0", "out_time_us=5000000", "speed=5x", "progress=continue"];
        let ends: Vec<Option<bool>> = block.iter().map(|l| parse_progress_line(l, 20.0, &mut p)).collect();
        assert_eq!(ends.last(), Some(&Some(true)));
        assert_eq!(p, FfmpegProgress { fraction: 0.25, frame: 250, fps: 125.5 });
        // Error messages are not progress.
        assert_eq!(parse_progress_line("[mpeg2video @ 0x1] buffer underflow", 20.0, &mut p), None);
        assert_eq!(parse_progress_line("Error opening input: No such file", 20.0, &mut p), None);
    }

    #[test]
    fn bundled_bin_comes_first() {
        let dirs = bundled_bin_dirs();
        let exe_bin = std::env::current_exe().unwrap().parent().unwrap().join("bin");
        assert_eq!(dirs.first(), Some(&exe_bin), "{dirs:?}");
        if cfg!(debug_assertions) {
            let cwd_bin = std::env::current_dir().unwrap().join("bin");
            assert!(dirs.contains(&cwd_bin), "{dirs:?}");
            // `cargo test` runs in the project root, where bin/ holds ffmpeg.
            if cwd_bin.join(exe_name("ffmpeg")).is_file() {
                assert_eq!(find_tool("ffmpeg"), Some(cwd_bin.join(exe_name("ffmpeg"))));
            }
        } else {
            assert_eq!(dirs, vec![exe_bin]);
        }
        assert_eq!(find_tool("definitely-not-a-tool-xyz"), None);
    }

    #[test]
    fn broken_candidate_is_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = std::env::temp_dir().join(format!("cx_tools_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Not an executable at all: must not count as a working tool.
        let fake = dir.join(exe_name("fake-tool"));
        std::fs::write(&fake, b"not a program").unwrap();
        assert!(!rt.block_on(tool_works(&fake, true)));
        assert!(!rt.block_on(tool_works(&dir.join("missing.exe"), false)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn priority_flags() {
        assert_eq!(ProcessPriority::Normal.creation_flag(), None);
        assert_eq!(ProcessPriority::BelowNormal.creation_flag(), Some(0x4000));
        assert_eq!(ProcessPriority::Idle.creation_flag(), Some(0x40));
    }

    #[test]
    fn tail_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }
}
