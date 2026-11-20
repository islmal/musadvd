//! Chapter file parser for DVD / Blu-ray chapter import.
//!
//! Supported formats:
//! - OGG / MKVToolNix simple (`CHAPTER01=00:00:00.000` + `CHAPTER01NAME=Intro`)
//! - Matroska XML (`<ChapterAtom>` with `<ChapterTimeStart>` and `<ChapterString>`)
//! - FFMETADATA (`;FFMETADATA1` `[CHAPTER]` with `TIMEBASE`, `START`, `title=`)
//! - Plain / YouTube timestamp lists (`0:00 Intro`, `01:23:45.500 - Scene 2`, `Intro 05:12`, bare timestamps)
//! - `CellTimes.txt` (integer frame numbers per line, converted with `fps.unwrap_or(25.0)`)

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Maximum accepted chapter file size (4 MB).
pub const MAX_CHAPTER_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Minimum spacing in seconds between consecutive chapters after deduplication.
const DEDUP_EPSILON_SECS: f64 = 0.5;
/// Default frame rate used for `CellTimes.txt` when the caller omits `fps`.
const DEFAULT_CELLTIMES_FPS: f64 = 25.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedChapter {
    pub start_secs: f64,
    pub title: Option<String>,
}

/// Reads and parses a chapter file on disk with path safety checks, BOM handling, and a 4 MB guard.
pub fn read_chapters_file(path: &Path, fps: Option<f64>) -> Result<Vec<ImportedChapter>, String> {
    let meta = crate::probe::validate_input_file_path(path, "chapter file")?;
    if meta.len() > MAX_CHAPTER_FILE_BYTES {
        return Err(format!(
            "{}: chapter file exceeds 4 MB limit ({} bytes)",
            path.display(),
            meta.len()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = decode_text_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_chapters(&text, fps)
}

fn decode_text_bytes(bytes: &[u8]) -> Result<String, String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec()).map_err(|e| format!("invalid UTF-8: {e}"));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        if rest.len() % 2 != 0 {
            return Err("invalid UTF-16 LE byte length".to_string());
        }
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16(&units).map_err(|e| format!("invalid UTF-16 LE: {e}"));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        if rest.len() % 2 != 0 {
            return Err("invalid UTF-16 BE byte length".to_string());
        }
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16(&units).map_err(|e| format!("invalid UTF-16 BE: {e}"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("invalid UTF-8: {e}"))
}

/// Parses chapter text in any supported format, sorts ascending, deduplicates within 0.5 s,
/// and ensures chapter 1 starts at `0.0`.
pub fn parse_chapters(text: &str, fps: Option<f64>) -> Result<Vec<ImportedChapter>, String> {
    if let Some(f) = fps {
        if !f.is_finite() || f <= 0.0 {
            return Err("Invalid frame rate (fps) for chapter parsing".to_string());
        }
    }
    let text = text.trim_start_matches('\u{FEFF}');
    if text.trim().is_empty() {
        return Err("Chapter input is empty".to_string());
    }

    let lower = text.to_ascii_lowercase();
    let raw = if lower.contains("<chapteratom") || lower.contains("<chapters") {
        parse_matroska_xml(text)?
    } else if text.lines().any(|l| {
        let t = l.trim();
        t.eq_ignore_ascii_case("[CHAPTER]") || t.to_ascii_uppercase().starts_with(";FFMETADATA")
    }) {
        parse_ffmetadata(text)?
    } else if text.lines().any(|l| parse_ogg_line(l).is_some()) {
        parse_ogg_chapters(text)?
    } else if is_cell_times(text) {
        parse_cell_times(text, fps.unwrap_or(DEFAULT_CELLTIMES_FPS))?
    } else {
        parse_plain_timestamps(text)?
    };

    finalize_chapters(raw)
}

fn finalize_chapters(mut chapters: Vec<ImportedChapter>) -> Result<Vec<ImportedChapter>, String> {
    chapters.retain(|c| c.start_secs.is_finite() && c.start_secs >= 0.0);
    if chapters.is_empty() {
        return Err("No valid chapters found".to_string());
    }
    chapters.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));

    let mut out: Vec<ImportedChapter> = Vec::with_capacity(chapters.len() + 1);
    let mut iter = chapters.into_iter();
    if let Some(first) = iter.next() {
        if first.start_secs > 0.0 {
            out.push(ImportedChapter { start_secs: 0.0, title: None });
            merge_or_push(&mut out, first);
        } else {
            out.push(first);
        }
    }
    for ch in iter {
        merge_or_push(&mut out, ch);
    }
    Ok(out)
}

fn merge_or_push(out: &mut Vec<ImportedChapter>, ch: ImportedChapter) {
    if let Some(last) = out.last_mut() {
        if ch.start_secs - last.start_secs < DEDUP_EPSILON_SECS {
            if last.title.is_none() && ch.title.is_some() {
                last.title = ch.title;
            }
            return;
        }
    }
    out.push(ch);
}

fn clean_title(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Parses `HH:MM:SS(.fff|,fff)` or `MM:SS(.fff|,fff)` (with `allow_mm_ss = true`).
fn parse_timestamp(raw: &str, allow_mm_ss: bool) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [hh, mm, ss_frac] => {
            if hh.is_empty() || !hh.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            if mm.len() > 2 || mm.is_empty() || !mm.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let h: f64 = hh.parse().ok()?;
            let m: u32 = mm.parse().ok()?;
            let sec = parse_seconds_part(ss_frac)?;
            if m >= 60 {
                return None;
            }
            Some(h * 3600.0 + m as f64 * 60.0 + sec)
        }
        [mm, ss_frac] if allow_mm_ss => {
            if mm.is_empty() || !mm.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let m: f64 = mm.parse().ok()?;
            let sec = parse_seconds_part(ss_frac)?;
            Some(m * 60.0 + sec)
        }
        _ => None,
    }
}

fn parse_seconds_part(raw: &str) -> Option<f64> {
    let (sec_str, frac_str) = match raw.split_once(['.', ',']) {
        Some((s, f)) => (s, Some(f)),
        None => (raw, None),
    };
    if sec_str.len() != 2 || !sec_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sec: u32 = sec_str.parse().ok()?;
    if sec >= 60 {
        return None;
    }
    let frac = match frac_str {
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            format!("0.{f}").parse::<f64>().ok()?
        }
        None => 0.0,
    };
    Some(sec as f64 + frac)
}

// ---------------------------------------------------------------------------
// 1. OGG / MKVToolNix simple format
// ---------------------------------------------------------------------------

enum OggLine<'a> {
    Time(&'a str, &'a str),
    Name(&'a str, &'a str),
}

fn parse_ogg_line(line: &str) -> Option<OggLine<'_>> {
    let trimmed = line.trim();
    let (key, val) = trimmed.split_once('=')?;
    let key_trim = key.trim();
    if key_trim.len() <= 7 || !key_trim[..7].eq_ignore_ascii_case("CHAPTER") {
        return None;
    }
    let rest = &key_trim[7..];
    let digit_end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_end == 0 {
        return None;
    }
    let id = &rest[..digit_end];
    let suffix = &rest[digit_end..];
    if suffix.is_empty() {
        Some(OggLine::Time(id, val.trim()))
    } else if suffix.eq_ignore_ascii_case("NAME") {
        Some(OggLine::Name(id, val))
    } else {
        None
    }
}

fn parse_ogg_chapters(text: &str) -> Result<Vec<ImportedChapter>, String> {
    let mut times: BTreeMap<String, f64> = BTreeMap::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        match parse_ogg_line(line) {
            Some(OggLine::Time(id, ts)) => {
                if let Some(secs) = parse_timestamp(ts, true) {
                    times.insert(id.to_string(), secs);
                }
            }
            Some(OggLine::Name(id, name)) => {
                if let Some(clean) = clean_title(name) {
                    names.insert(id.to_string(), clean);
                }
            }
            None => {}
        }
    }
    if times.is_empty() {
        return Err("Invalid OGG chapter format: no valid timestamps found".to_string());
    }
    Ok(times
        .into_iter()
        .map(|(id, start_secs)| ImportedChapter { start_secs, title: names.remove(&id) })
        .collect())
}

// ---------------------------------------------------------------------------
// 2. Matroska XML
// ---------------------------------------------------------------------------

struct XmlAtom {
    start: Option<f64>,
    title: Option<String>,
}

fn unescape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let entity = &after[..semi];
            let replaced = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = replaced {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn tag_name(raw_tag: &str) -> &str {
    let t = raw_tag.trim().trim_start_matches('/').trim_end_matches('/');
    t.split(|c: char| c.is_whitespace()).next().unwrap_or("")
}

fn parse_matroska_xml(text: &str) -> Result<Vec<ImportedChapter>, String> {
    let mut atoms: Vec<XmlAtom> = Vec::new();
    let mut out: Vec<ImportedChapter> = Vec::new();
    let mut active_tag: Option<&str> = None;
    let mut buf = String::new();
    let mut pos = 0;

    while pos < text.len() {
        let Some(lt_rel) = text[pos..].find('<') else {
            if active_tag.is_some() {
                buf.push_str(&text[pos..]);
            }
            break;
        };
        if active_tag.is_some() {
            buf.push_str(&text[pos..pos + lt_rel]);
        }
        let tag_start = pos + lt_rel;
        if text[tag_start..].starts_with("<!--") {
            let Some(end) = text[tag_start + 4..].find("-->") else {
                break;
            };
            pos = tag_start + 4 + end + 3;
            continue;
        }
        if text[tag_start..].starts_with("<![CDATA[") {
            let Some(end) = text[tag_start + 9..].find("]]>") else {
                break;
            };
            if active_tag.is_some() {
                buf.push_str(&text[tag_start + 9..tag_start + 9 + end]);
            }
            pos = tag_start + 9 + end + 3;
            continue;
        }
        let Some(gt_rel) = text[tag_start..].find('>') else {
            return Err("Malformed Matroska XML: unclosed tag".to_string());
        };
        let raw_tag = &text[tag_start + 1..tag_start + gt_rel];
        pos = tag_start + gt_rel + 1;
        let trimmed = raw_tag.trim();
        if trimmed.starts_with('?') || trimmed.starts_with('!') {
            continue;
        }
        let is_closing = trimmed.starts_with('/');
        let is_self_closing = trimmed.ends_with('/');
        let name = tag_name(trimmed);

        if !is_closing {
            if name.eq_ignore_ascii_case("ChapterAtom") && !is_self_closing {
                atoms.push(XmlAtom { start: None, title: None });
            } else if (name.eq_ignore_ascii_case("ChapterTimeStart") || name.eq_ignore_ascii_case("ChapterString"))
                && !is_self_closing
            {
                active_tag = Some(if name.eq_ignore_ascii_case("ChapterTimeStart") {
                    "ChapterTimeStart"
                } else {
                    "ChapterString"
                });
                buf.clear();
            }
        } else if name.eq_ignore_ascii_case("ChapterTimeStart") && active_tag == Some("ChapterTimeStart") {
            if let Some(atom) = atoms.last_mut() {
                if atom.start.is_none() {
                    atom.start = parse_timestamp(&buf, true);
                }
            }
            active_tag = None;
            buf.clear();
        } else if name.eq_ignore_ascii_case("ChapterString") && active_tag == Some("ChapterString") {
            if let Some(atom) = atoms.last_mut() {
                if atom.title.is_none() {
                    atom.title = clean_title(&unescape_xml(&buf));
                }
            }
            active_tag = None;
            buf.clear();
        } else if name.eq_ignore_ascii_case("ChapterAtom") {
            if let Some(atom) = atoms.pop() {
                if let Some(start_secs) = atom.start {
                    out.push(ImportedChapter { start_secs, title: atom.title });
                }
            }
        }
    }

    if out.is_empty() {
        return Err("Invalid Matroska XML: no <ChapterAtom> with <ChapterTimeStart> found".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 3. FFMETADATA
// ---------------------------------------------------------------------------

fn unescape_ffmetadata(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push(' '),
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_timebase(val: &str) -> Option<f64> {
    let v = val.trim();
    if let Some((num, den)) = v.split_once('/') {
        let n: f64 = num.trim().parse().ok()?;
        let d: f64 = den.trim().parse().ok()?;
        (n.is_finite() && d.is_finite() && n > 0.0 && d > 0.0).then_some(n / d)
    } else {
        let n: f64 = v.parse().ok()?;
        (n.is_finite() && n > 0.0).then_some(n)
    }
}

fn parse_ffmetadata(text: &str) -> Result<Vec<ImportedChapter>, String> {
    let mut out = Vec::new();
    let mut in_chapter = false;
    let mut timebase: f64 = 1e-9; // FFmpeg default TIMEBASE=1/1000000000
    let mut start_ticks: Option<f64> = None;
    let mut title: Option<String> = None;

    let flush = |out: &mut Vec<ImportedChapter>, in_ch: bool, tb: f64, st: &mut Option<f64>, t: &mut Option<String>| {
        if in_ch {
            if let Some(ticks) = st.take() {
                let secs = ticks * tb;
                if secs.is_finite() && secs >= 0.0 {
                    out.push(ImportedChapter { start_secs: secs, title: t.take() });
                }
            }
        }
        *st = None;
        *t = None;
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            flush(&mut out, in_chapter, timebase, &mut start_ticks, &mut title);
            in_chapter = trimmed.eq_ignore_ascii_case("[CHAPTER]");
            timebase = 1e-9;
            continue;
        }
        if !in_chapter {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            match k.trim().to_ascii_uppercase().as_str() {
                "TIMEBASE" => {
                    if let Some(tb) = parse_timebase(v) {
                        timebase = tb;
                    }
                }
                "START" => {
                    start_ticks = v.trim().parse::<f64>().ok().filter(|n| n.is_finite());
                }
                "TITLE" => {
                    title = clean_title(&unescape_ffmetadata(v));
                }
                _ => {}
            }
        }
    }
    flush(&mut out, in_chapter, timebase, &mut start_ticks, &mut title);

    if out.is_empty() {
        return Err("Invalid FFMETADATA: no [CHAPTER] entries found".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 4. CellTimes.txt
// ---------------------------------------------------------------------------

fn is_cell_times(text: &str) -> bool {
    let mut count = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        if !t.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }
    count > 0
}

fn parse_cell_times(text: &str, fps: f64) -> Result<Vec<ImportedChapter>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        let frame: u64 = t.parse().map_err(|_| format!("Invalid CellTimes frame number: {t}"))?;
        out.push(ImportedChapter {
            start_secs: frame as f64 / fps,
            title: None,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 5. Plain / YouTube timestamp lists
// ---------------------------------------------------------------------------

fn strip_list_prefix(s: &str) -> &str {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")).or_else(|| s.strip_prefix("+ ")) {
        return rest.trim_start();
    }
    // Numbered prefix like "1. 0:00 Intro" or "1) 0:00 Intro"
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 {
        let after = &s[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    s
}

fn clean_title_side(s: &str, leading: bool) -> Option<String> {
    let sep = |c: char| c.is_whitespace() || matches!(c, '-' | '–' | '—' | ':' | '|');
    let trimmed = if leading { s.trim_start_matches(sep).trim_end() } else { s.trim_end_matches(sep).trim_start() };
    clean_title(trimmed)
}

fn parse_plain_line(line: &str) -> Option<ImportedChapter> {
    let s = strip_list_prefix(line).trim();
    if s.is_empty() {
        return None;
    }

    // Case A: timestamp at start (bare, or inside [...] / (...))
    if let Some(inner) = s.strip_prefix('[').or_else(|| s.strip_prefix('(')) {
        let close = if s.starts_with('[') { ']' } else { ')' };
        if let Some(end) = inner.find(close) {
            if let Some(secs) = parse_timestamp(&inner[..end], true) {
                return Some(ImportedChapter {
                    start_secs: secs,
                    title: clean_title_side(&inner[end + 1..], true),
                });
            }
        }
    }

    let first_tok_end = s
        .find(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | '—' | '|'))
        .unwrap_or(s.len());
    let first_tok = s[..first_tok_end].trim_end_matches(':');
    if let Some(secs) = parse_timestamp(first_tok, true) {
        return Some(ImportedChapter {
            start_secs: secs,
            title: clean_title_side(&s[first_tok_end..], true),
        });
    }

    // Case B: timestamp at end (e.g. "Intro 05:12", "Intro - [05:12]", "Intro (0:05:12)")
    let (body, ts_raw) = if s.ends_with(']') {
        let open = s.rfind('[')?;
        (&s[..open], &s[open + 1..s.len() - 1])
    } else if s.ends_with(')') {
        let open = s.rfind('(')?;
        (&s[..open], &s[open + 1..s.len() - 1])
    } else {
        let split_at = s
            .rfind(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | '—' | '|'))
            .map(|idx| idx + s[idx..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        (&s[..split_at], &s[split_at..])
    };

    let secs = parse_timestamp(ts_raw.trim_end_matches(':'), true)?;
    Some(ImportedChapter {
        start_secs: secs,
        title: clean_title_side(body, false),
    })
}

fn parse_plain_timestamps(text: &str) -> Result<Vec<ImportedChapter>, String> {
    let out: Vec<ImportedChapter> = text.lines().filter_map(parse_plain_line).collect();
    if out.is_empty() {
        return Err("No valid chapter timestamps found".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ogg_mkvtoolnix_format() {
        let src = "\u{FEFF}CHAPTER01=00:00:00.000\nCHAPTER01NAME=Intro\nCHAPTER02=00:05:12.500\nCHAPTER02NAME=Scene 2\nCHAPTER03=01:00:00,250\nCHAPTER03NAME=\n";
        let ch = parse_chapters(src, None).unwrap();
        assert_eq!(
            ch,
            vec![
                ImportedChapter { start_secs: 0.0, title: Some("Intro".into()) },
                ImportedChapter { start_secs: 312.5, title: Some("Scene 2".into()) },
                ImportedChapter { start_secs: 3600.25, title: None },
            ]
        );
    }

    #[test]
    fn parses_matroska_xml_format() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <Chapters>
          <EditionEntry>
            <ChapterAtom>
              <ChapterTimeStart>00:04:30.500000000</ChapterTimeStart>
              <ChapterDisplay>
                <ChapterString>Act 1 &amp; 2</ChapterString>
              </ChapterDisplay>
            </ChapterAtom>
            <ChapterAtom>
              <ChapterTimeStart>00:10:00.000000000</ChapterTimeStart>
              <ChapterDisplay>
                <ChapterString>  Finale  </ChapterString>
              </ChapterDisplay>
            </ChapterAtom>
          </EditionEntry>
        </Chapters>"#;
        let ch = parse_chapters(xml, None).unwrap();
        assert_eq!(
            ch,
            vec![
                ImportedChapter { start_secs: 0.0, title: None },
                ImportedChapter { start_secs: 270.5, title: Some("Act 1 & 2".into()) },
                ImportedChapter { start_secs: 600.0, title: Some("Finale".into()) },
            ]
        );
    }

    #[test]
    fn parses_ffmetadata_format() {
        let meta = ";FFMETADATA1\ntitle=Movie\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=300000\ntitle=Opening\n[CHAPTER]\nTIMEBASE=1/1000000000\nSTART=300000000000\nEND=600000000000\ntitle=Scene\\; Two\\=Part\n";
        let ch = parse_chapters(meta, None).unwrap();
        assert_eq!(
            ch,
            vec![
                ImportedChapter { start_secs: 0.0, title: Some("Opening".into()) },
                ImportedChapter { start_secs: 300.0, title: Some("Scene; Two=Part".into()) },
            ]
        );
    }

    #[test]
    fn parses_plain_and_youtube_timestamps() {
        let txt = "Tracklist:\n0:00 Intro\n01:23:45.500 - Scene 2\nIntro Late 05:12\n[00:10:00] Bracketed\n00:15:00\n2. 00:20:00 Numbered\n";
        let ch = parse_chapters(txt, None).unwrap();
        assert_eq!(
            ch,
            vec![
                ImportedChapter { start_secs: 0.0, title: Some("Intro".into()) },
                ImportedChapter { start_secs: 312.0, title: Some("Intro Late".into()) },
                ImportedChapter { start_secs: 600.0, title: Some("Bracketed".into()) },
                ImportedChapter { start_secs: 900.0, title: None },
                ImportedChapter { start_secs: 1200.0, title: Some("Numbered".into()) },
                ImportedChapter { start_secs: 5025.5, title: Some("Scene 2".into()) },
            ]
        );
    }

    #[test]
    fn parses_cell_times_with_default_and_custom_fps() {
        let txt = "0\n7500\n15000\n";
        let pal = parse_chapters(txt, None).unwrap();
        assert_eq!(pal.len(), 3);
        assert_eq!(pal[1].start_secs, 300.0);
        assert_eq!(pal[2].start_secs, 600.0);

        let film = parse_chapters("0\n2400\n", Some(24.0)).unwrap();
        assert_eq!(film[1].start_secs, 100.0);
    }

    #[test]
    fn deduplicates_sorts_and_prepends_zero() {
        let txt = "00:05:00.400 Second B\n00:05:00.100 Second A\n00:01:00.000 First\n";
        let ch = parse_chapters(txt, None).unwrap();
        assert_eq!(
            ch,
            vec![
                ImportedChapter { start_secs: 0.0, title: None },
                ImportedChapter { start_secs: 60.0, title: Some("First".into()) },
                ImportedChapter { start_secs: 300.1, title: Some("Second A".into()) },
            ]
        );

        // Near-zero chapter collapses into 0.0 preserving its title.
        let near_zero = parse_chapters("00:00:00.250 Prologue\n00:02:00.000 Main\n", None).unwrap();
        assert_eq!(
            near_zero,
            vec![
                ImportedChapter { start_secs: 0.0, title: Some("Prologue".into()) },
                ImportedChapter { start_secs: 120.0, title: Some("Main".into()) },
            ]
        );
    }

    #[test]
    fn rejects_invalid_inputs_and_unsafe_paths() {
        assert!(parse_chapters("", None).is_err());
        assert!(parse_chapters("just some random words", None).is_err());
        assert!(parse_chapters("0\n100\n", Some(0.0)).is_err());
        assert!(parse_chapters("0\n100\n", Some(f64::NAN)).is_err());

        assert!(read_chapters_file(Path::new(r"\\server\share\ch.txt"), None).is_err());
        assert!(read_chapters_file(Path::new(r"\\?\C:\ch.txt"), None).is_err());
        assert!(read_chapters_file(Path::new("../chapters.txt"), None).is_err());
        assert!(read_chapters_file(Path::new(r"a\..\chapters.txt"), None).is_err());
        assert!(read_chapters_file(Path::new("nonexistent_chapters_12345.txt"), None).is_err());
    }

    #[test]
    fn parses_youtube_timestamps_with_trailing_colon() {
        let txt = "0:00:00: Intro\n05:12: Scene 2\n00:10:00: Finale\n";
        let ch = parse_chapters(txt, None).unwrap();
        assert_eq!(
            ch,
            vec![
                ImportedChapter { start_secs: 0.0, title: Some("Intro".into()) },
                ImportedChapter { start_secs: 312.0, title: Some("Scene 2".into()) },
                ImportedChapter { start_secs: 600.0, title: Some("Finale".into()) },
            ]
        );
    }

    #[test]
    fn reads_real_file_with_bom_and_size_guard() {
        let dir = std::env::temp_dir().join(format!("cx_chapters_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let utf8_bom = dir.join("bom.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"0:00 Start\n5:00 Middle\n");
        std::fs::write(&utf8_bom, &bytes).unwrap();
        let ch = read_chapters_file(&utf8_bom, None).unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].title.as_deref(), Some("Start"));
        assert_eq!(ch[1].start_secs, 300.0);

        let too_big = dir.join("huge.txt");
        let huge = vec![b'0'; (MAX_CHAPTER_FILE_BYTES as usize) + 1];
        std::fs::write(&too_big, &huge).unwrap();
        assert!(read_chapters_file(&too_big, None).unwrap_err().contains("4 MB"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handles_crlf_line_endings_across_formats() {
        let ogg = "CHAPTER01=00:00:00.000\r\nCHAPTER01NAME=Intro\r\nCHAPTER02=00:02:00.000\r\nCHAPTER02NAME=Next\r\n";
        assert_eq!(parse_chapters(ogg, None).unwrap().len(), 2);

        let ff = ";FFMETADATA1\r\n[CHAPTER]\r\nTIMEBASE=1/1000\r\nSTART=0\r\ntitle=A\r\n[CHAPTER]\r\nTIMEBASE=1/1000\r\nSTART=60000\r\ntitle=B\r\n";
        assert_eq!(parse_chapters(ff, None).unwrap()[1].start_secs, 60.0);

        let plain = "0:00 Start\r\n01:30 Middle\r\n";
        assert_eq!(parse_chapters(plain, None).unwrap()[1].start_secs, 90.0);

        let cells = "0\r\n2500\r\n";
        assert_eq!(parse_chapters(cells, Some(25.0)).unwrap()[1].start_secs, 100.0);
    }
}
