//! Backend of the menu editor: layout checks against the project's titles, loading/saving
//! layout files, and image previews for the editor canvas.
//!
//! Commands: `check_layout`, `text_lines`, `load_layout`, `save_layout`, `image_preview` (frames of the
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
    match result {
        Ok(()) => vec![],
        Err(e) => e.0,
    }
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
        let template = Template::load(&path).map_err(|e| format!("Vorlage konnte nicht gelesen werden: {e}"))?;
        if template.pages.is_empty() {
            return Err("Die Vorlage enthält keine Menüseiten.".to_string());
        }
        let layout_titles = layout_titles(&titles);
        // shortcut: chapter sample frames are spread evenly; the editor has no chapter times. Pass them in if the stills matter.
        let chapter_starts: Vec<Vec<f64>> =
            titles.iter().map(|t| (0..t.chapters.max(1)).map(|c| t.duration_secs * c as f64 / t.chapters.max(1) as f64).collect()).collect();
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
                audio_tracks: vec![],
                subtitle_tracks: vec![],
            }]
        };
        assert!(check_layout(layout.clone(), t(5), "PAL".into()).is_empty());
        let errs = check_layout(layout, t(3), "PAL".into());
        assert!(errs[0].contains("Kapitel 5"), "{errs:?}");
        assert!(image_preview("x.txt".into()).is_err());
        // Blu-ray: the 720×576 default canvas is not 16:9; chapters are checked the same way.
        let errs = check_layout(layout_with_canvas(1920, 1080), t(3), "BD1080".into());
        assert!(errs.iter().all(|e| !e.contains("16:9")) && errs[0].contains("Kapitel 5"), "{errs:?}");
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
}
