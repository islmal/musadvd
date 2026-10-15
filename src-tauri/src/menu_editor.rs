//! Backend of the menu editor: layout checks against the project's titles, loading/saving
//! layout files, and image previews for the editor canvas.
//!
//! Commands: `check_layout`, `text_lines`, `load_layout`, `save_layout`, `image_preview` (frames of the
//! titles for video placeholders come from `title_frame` in `main.rs`, which needs the tools).

use std::path::Path;

use base64::Engine as _;
use convertx_rust::author::{ClipSource, TvStandard};
use convertx_rust::layout::{self, Canvas, CustomLayout, Element, LayoutTitle};
use serde::Deserialize;

/// A selected audio track or subtitle of a title (source stream index; label for the UI).
#[derive(Debug, Clone, Deserialize)]
pub struct TrackRef {
    pub source: u32,
}

/// A project title as the editor knows it (further fields of the frontend are ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorTitle {
    pub label: String,
    pub path: String,
    pub chapters: usize,
    pub duration_secs: f64,
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
            clip: ClipSource { path: t.path.clone().into(), start_secs: (t.duration_secs * 0.1).min(300.0), has_audio: true },
            audio_sources: t.audio_tracks.iter().map(|a| a.source).collect(),
            subtitle_sources: t.subtitle_tracks.iter().map(|s| s.source).collect(),
        })
        .collect()
}

/// All problems of `layout` for these titles (empty = ready to build).
#[tauri::command]
pub fn check_layout(layout: CustomLayout, titles: Vec<EditorTitle>, tv: String) -> Vec<String> {
    let tv = if tv.eq_ignore_ascii_case("NTSC") { TvStandard::Ntsc } else { TvStandard::Pal };
    match layout::validate(&layout, &layout_titles(&titles), tv) {
        Ok(()) => vec![],
        Err(e) => e.0,
    }
}

/// The lines a text element shows on the disc (word wrap and "…" exactly as the build draws
/// them); empty for other elements.
#[tauri::command]
pub fn text_lines(element: Element, canvas: Canvas, tv: String) -> Vec<String> {
    let tv = if tv.eq_ignore_ascii_case("NTSC") { TvStandard::Ntsc } else { TvStandard::Pal };
    layout::element_text_lines(&element, canvas, tv).unwrap_or_default()
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
        _ => return Err(format!("{path}: kein unterstütztes Bildformat")),
    };
    let size = std::fs::metadata(p).map_err(|e| format!("{path}: {e}"))?.len();
    if size > MAX_PREVIEW_BYTES {
        return Err(format!("{path}: Bild ist zu groß für die Vorschau"));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("{path}: {e}"))?;
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
                audio_tracks: vec![],
                subtitle_tracks: vec![],
            }]
        };
        assert!(check_layout(layout.clone(), t(5), "PAL".into()).is_empty());
        let errs = check_layout(layout, t(3), "PAL".into());
        assert!(errs[0].contains("Kapitel 5"), "{errs:?}");
        assert!(image_preview("x.txt".into()).is_err());
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
}
