//! Backend of the menu editor: layout checks against the project's titles, loading/saving
//! layout files, and image previews for the editor canvas.
//!
//! Commands: `check_layout`, `text_lines`, `menu_navigation`, `load_layout`, `save_layout`, `image_preview` (frames of the
//! titles for video placeholders come from `title_frame` in `main.rs`, which needs the tools).

use std::path::Path;

use base64::Engine as _;
use musadvd::author::{ClipSource, MenuFrame, TvStandard};
use musadvd::bd_template::layout_from_template;
use musadvd::dvd::ChapterSettings;
use musadvd::encoder::Aspect;
use musadvd::layout::{self, Canvas, CustomLayout, Element, LayoutTitle};
use musadvd::menu::UiLang;
use musadvd::template::Template;
use serde::{Deserialize, Serialize};

/// A selected audio track or subtitle of a title (source stream index; label for the UI).
#[derive(Debug, Clone, Deserialize)]
pub struct TrackRef {
    pub source: u32,
    #[serde(default)]
    pub label: String,
}

/// A project title as the editor knows it (further fields of the frontend are ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorTitle {
    pub label: String,
    pub path: String,
    pub chapters: usize,
    pub duration_secs: f64,
    /// Chapter start times in seconds as the build uses them (empty = an even grid).
    #[serde(default)]
    pub chapter_starts: Vec<f64>,
    /// Source time the build takes a menu clip from; absent = 10 % into the title.
    #[serde(default)]
    pub clip_start_secs: Option<f64>,
    #[serde(default)]
    pub audio_tracks: Vec<TrackRef>,
    #[serde(default)]
    pub subtitle_tracks: Vec<TrackRef>,
}

fn layout_titles(titles: &[EditorTitle]) -> Vec<LayoutTitle> {
    titles
        .iter()
        .map(|t| LayoutTitle {
            label: t.label.clone(),
            chapters: t.chapters,
            duration_secs: t.duration_secs,
            clip: ClipSource { path: t.path.clone().into(), start_secs: t.clip_start_secs.unwrap_or_else(|| (t.duration_secs * 0.1).min(300.0)), has_audio: true },
            audio_sources: t.audio_tracks.iter().map(|a| a.source).collect(),
            subtitle_sources: t.subtitle_tracks.iter().map(|s| s.source).collect(),
            audio_labels: t.audio_tracks.iter().map(|a| a.label.clone()).collect(),
            subtitle_labels: t.subtitle_tracks.iter().map(|s| s.label.clone()).collect(),
        })
        .collect()
}

/// The frame a layout is checked / measured on: `PAL`, `NTSC`, or `BD1080` / `BD720` (Blu-ray,
/// square pixels).
enum Frame {
    Tv(TvStandard),
    Bd(MenuFrame),
}

fn frame_of(tv: &str) -> Frame {
    match tv.to_ascii_uppercase().as_str() {
        "BD720" => Frame::Bd(MenuFrame::square(1280, 720)),
        s if s.starts_with("BD") => Frame::Bd(MenuFrame::square(1920, 1080)),
        "NTSC" => Frame::Tv(TvStandard::Ntsc),
        _ => Frame::Tv(TvStandard::Pal),
    }
}

/// All problems of `layout` for these titles (empty = ready to build).
#[tauri::command]
pub fn check_layout(layout: CustomLayout, titles: Vec<EditorTitle>, tv: String) -> Vec<String> {
    let titles = layout_titles(&titles);
    let result = match frame_of(&tv) {
        Frame::Tv(tv) => layout::validate(&layout, &titles, tv),
        Frame::Bd(frame) => layout::validate_bd(&layout, &titles, frame),
    };
    let mut errs = result.err().map(|e| e.0).unwrap_or_default();
    errs.extend(check_media_streams(&layout));
    errs
}

/// True/false whether `path` has a stream of `kind` ("audio"/"video"); `None` when ffprobe is
/// missing or fails (then the check is skipped).
fn has_stream(ffprobe: &Path, path: &Path, kind: &str) -> Option<bool> {
    let out = musadvd::tools::blocking_tool_command(ffprobe)
        .args(["-v", "error", "-select_streams", &kind[..1], "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then(|| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Menu sound files need an audio stream and background/element video files a video stream;
/// otherwise the build would only fail at encode time.
fn check_media_streams(layout: &CustomLayout) -> Vec<String> {
    let Some(ffprobe) = musadvd::tools::find_tool("ffprobe") else { return vec![] };
    let mut errs = Vec::new();
    for m in &layout.menus {
        let mut check = |p: &Path, kind: &str, what: &str| {
            if p.is_file() && has_stream(&ffprobe, p, kind) == Some(false) {
                errs.push(format!("Menu '{}': {what} '{}' has no {kind} stream", m.name, p.display()));
            }
        };
        if let layout::MenuSound::File { path } = &m.audio {
            check(Path::new(path), "audio", "sound file");
        }
        if let Some(p) = m.background.video.as_ref().and_then(|v| v.path.as_ref()) {
            check(p, "video", "background video");
        }
        for e in &m.elements {
            if let layout::Content::Video { path: Some(p), .. } = &e.content {
                check(p, "video", "video file");
            }
        }
    }
    errs
}

/// The lines a text element shows on the disc (word wrap and "…" exactly as the build draws
/// them); empty for other elements.
#[tauri::command]
pub fn text_lines(element: Element, canvas: Canvas, tv: String) -> Vec<String> {
    let lines = match frame_of(&tv) {
        Frame::Tv(tv) => layout::element_text_lines(&element, canvas, tv),
        Frame::Bd(frame) => layout::element_text_lines(&element, canvas, frame),
    };
    lines.unwrap_or_default()
}

/// Directional remote navigation links for one menu button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonNav {
    pub id: String,
    pub up: Option<String>,
    pub down: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// Computes author-consistent remote navigation links for every button element in `menu`.
#[tauri::command]
pub fn menu_navigation(menu: musadvd::layout::CustomMenu, canvas: Canvas, tv: String) -> Vec<ButtonNav> {
    let (frame, bluray) = match frame_of(&tv) {
        Frame::Tv(tv) => (MenuFrame::from(tv), false),
        Frame::Bd(frame) => (frame, true),
    };
    musadvd::layout::menu_button_navigation(&menu, canvas, frame, bluray)
        .into_iter()
        .map(|(id, nav)| ButtonNav { id, up: nav.up, down: nav.down, left: nav.left, right: nav.right })
        .collect()
}

/// A ConvertX template turned into an editable layout, with the converter's (German) notes.
#[derive(Debug, Serialize)]
pub struct ImportedTemplate {
    pub layout: CustomLayout,
    pub warnings: Vec<String>,
}

/// Converts a ConvertX template (`.ini`) into an editor layout for these titles, the same way a
/// Blu-ray build does (`bd_template::layout_from_template`). Template images are read from disk,
/// so it runs off the async runtime.
#[tauri::command]
pub async fn import_convertx_template(path: String, titles: Vec<EditorTitle>, lang: String) -> Result<ImportedTemplate, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let template = Template::load(&path).map_err(|e| format!("Template could not be read: {e}"))?;
        if template.pages.is_empty() {
            return Err("The template contains no menu pages.".to_string());
        }
        let layout_titles = layout_titles(&titles);
        let chapter_starts: Vec<Vec<f64>> = titles
            .iter()
            .map(|t| {
                if t.chapter_starts.is_empty() {
                    (0..t.chapters.max(1)).map(|c| t.duration_secs * c as f64 / t.chapters.max(1) as f64).collect()
                } else {
                    t.chapter_starts.clone()
                }
            })
            .collect();
        let (layout, warnings) = layout_from_template(
            &template,
            Path::new(&path),
            &layout_titles,
            &vec![None; titles.len()],
            &chapter_starts,
            Aspect::Wide,
            &ChapterSettings::default(),
            UiLang::from_setting(&lang),
        )
        .map_err(|e| e.to_string())?;
        Ok(ImportedTemplate { layout, warnings })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn load_layout(path: String) -> Result<CustomLayout, String> {
    CustomLayout::load(Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_layout(path: String, layout: CustomLayout) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&layout).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("{path}: {e}"))
}

/// Largest picture the editor previews (data URLs travel through IPC).
const MAX_PREVIEW_BYTES: u64 = 25 * 1024 * 1024;

/// A picture file as data URL for the canvas (images only).
#[tauri::command]
pub fn image_preview(path: String) -> Result<String, String> {
    let p = Path::new(&path);
    let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return Err(format!("{path}: unsupported image format")),
    };
    let size = std::fs::metadata(p).map_err(|e| format!("{path}: {e}"))?.len();
    if size > MAX_PREVIEW_BYTES {
        return Err(format!("{path}: image is too large for the preview"));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("{path}: {e}"))?;
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_with_canvas(w: u32, h: u32) -> CustomLayout {
        CustomLayout::from_json(&format!(
            r#"{{"canvas": {{"width": {w}, "height": {h}}}, "menus": [{{"id": "m", "name": "Haupt", "role": "title", "elements": [
                {{"type": "text", "text": "K5", "x": 10, "y": 10, "w": 100, "h": 40,
                 "action": {{"type": "playChapter", "title": 1, "chapter": 5}}}}]}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn checks_against_project_titles() {
        let layout = CustomLayout::from_json(
            r#"{"menus": [{"id": "m", "name": "Haupt", "role": "title", "elements": [
                {"type": "text", "text": "K5", "x": 10, "y": 10, "w": 100, "h": 40,
                 "action": {"type": "playChapter", "title": 1, "chapter": 5}}]}]}"#,
        )
        .unwrap();
        let t = |chapters| {
            vec![EditorTitle {
                label: "A".into(),
                path: "a.mkv".into(),
                chapters,
                duration_secs: 60.0,
                chapter_starts: vec![],
                clip_start_secs: None,
                audio_tracks: vec![],
                subtitle_tracks: vec![],
            }]
        };
        assert!(check_layout(layout.clone(), t(5), "PAL".into()).is_empty());
        let errs = check_layout(layout, t(3), "PAL".into());
        assert!(errs[0].contains("Chapter 5"), "{errs:?}");
        assert!(image_preview("x.txt".into()).is_err());
        // Blu-ray: the 720×576 default canvas is not 16:9; chapters are checked the same way.
        let errs = check_layout(layout_with_canvas(1920, 1080), t(3), "BD1080".into());
        assert!(errs.iter().all(|e| !e.contains("16:9")) && errs[0].contains("Chapter 5"), "{errs:?}");
        let errs = check_layout(layout_with_canvas(720, 576), t(5), "BD720".into());
        assert!(errs.iter().any(|e| e.contains("16:9")), "{errs:?}");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("cx_layout_io_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("menue-layout.json").to_string_lossy().into_owned();
        let layout = CustomLayout::from_json(r#"{"menus": [{"id": "m", "name": "Haupt", "role": "title"}]}"#).unwrap();
        save_layout(path.clone(), layout.clone()).unwrap();
        assert_eq!(load_layout(path.clone()).unwrap(), layout);
        // Writing into a missing folder reports the path instead of failing silently.
        let bad = dir.join("missing").join("x.json").to_string_lossy().into_owned();
        assert!(save_layout(bad.clone(), layout).unwrap_err().contains(&bad));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn nav(id: &str, up: &str, down: &str, left: &str, right: &str) -> ButtonNav {
        ButtonNav {
            id: id.into(),
            up: Some(up.into()),
            down: Some(down.into()),
            left: Some(left.into()),
            right: Some(right.into()),
        }
    }

    #[test]
    fn menu_navigation_returns_exact_links_on_pal_and_bd1080() {
        let pal_layout = CustomLayout::from_json(
            r#"{"canvas": {"width": 720, "height": 576}, "menus": [{"id": "main", "name": "Main", "role": "title", "elements": [
                {"id": "deco", "type": "text", "text": "Header", "x": 100, "y": 20, "w": 460, "h": 40},
                {"id": "play", "type": "text", "text": "Play", "x": 100, "y": 80, "w": 460, "h": 60, "action": {"type": "playTitle", "title": 1}},
                {"id": "chap", "type": "text", "text": "Chapters", "x": 100, "y": 240, "w": 180, "h": 60, "action": {"type": "menu", "menu": "chapters"}},
                {"id": "audio", "type": "text", "text": "Audio", "x": 360, "y": 240, "w": 200, "h": 60, "action": {"type": "menu", "menu": "audio"}}
            ]}]}"#,
        )
        .unwrap();
        let pal_nav = menu_navigation(pal_layout.menus[0].clone(), pal_layout.canvas, "PAL".into());
        assert_eq!(
            pal_nav,
            vec![
                nav("play", "play", "audio", "chap", "audio"),
                nav("chap", "play", "chap", "chap", "audio"),
                nav("audio", "play", "audio", "chap", "audio"),
            ]
        );

        let bd_layout = CustomLayout::from_json(
            r#"{"canvas": {"width": 1920, "height": 1080}, "menus": [{"id": "main", "name": "Main", "role": "title", "elements": [
                {"id": "deco", "type": "text", "text": "Header", "x": 200, "y": 40, "w": 800, "h": 60},
                {"id": "play", "type": "text", "text": "Play", "x": 200, "y": 160, "w": 800, "h": 100, "action": {"type": "playTitle", "title": 1}},
                {"id": "chap", "type": "text", "text": "Chapters", "x": 200, "y": 460, "w": 320, "h": 100, "action": {"type": "menu", "menu": "chapters"}},
                {"id": "audio", "type": "text", "text": "Audio", "x": 640, "y": 460, "w": 360, "h": 100, "action": {"type": "menu", "menu": "audio"}},
                {"id": "subs", "type": "text", "text": "Subtitles", "x": 200, "y": 720, "w": 320, "h": 100, "action": {"type": "menu", "menu": "subs"}}
            ]}]}"#,
        )
        .unwrap();
        let bd_nav = menu_navigation(bd_layout.menus[0].clone(), bd_layout.canvas, "BD1080".into());
        assert_eq!(
            bd_nav,
            vec![
                nav("play", "play", "audio", "chap", "audio"),
                nav("chap", "play", "subs", "chap", "audio"),
                nav("audio", "play", "subs", "chap", "audio"),
                nav("subs", "chap", "subs", "subs", "audio"),
            ]
        );
    }

    #[test]
    fn rotated_text_buttons_pass_check_and_compute_navigation() {
        let pal_layout = CustomLayout::from_json(
            r##"{"canvas": {"width": 720, "height": 576}, "menus": [{"id": "main", "name": "Main", "role": "title", "elements": [
                {"id": "play", "type": "text", "text": "Play", "x": 160, "y": 120, "w": 200, "h": 50, "rotation": 45, "action": {"type": "playTitle", "title": 1}},
                {"id": "next", "type": "rect", "color": "#336699", "x": 160, "y": 320, "w": 200, "h": 50, "rotation": 30, "action": {"type": "playTitle", "title": 1}}
            ]}]}"##,
        )
        .unwrap();
        let t = vec![EditorTitle {
            label: "A".into(),
            path: "a.mkv".into(),
            chapters: 3,
            duration_secs: 60.0,
            chapter_starts: vec![],
            clip_start_secs: None,
            audio_tracks: vec![],
            subtitle_tracks: vec![],
        }];
        assert!(check_layout(pal_layout.clone(), t, "PAL".into()).is_empty());
        let navs = menu_navigation(pal_layout.menus[0].clone(), pal_layout.canvas, "PAL".into());
        assert_eq!(navs.len(), 2);
        assert_eq!(navs[0].down.as_deref(), Some("next"));
        assert_eq!(navs[1].up.as_deref(), Some("play"));
    }
}
