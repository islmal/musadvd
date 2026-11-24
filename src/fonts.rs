//! Installed OS font families (via `fontdb`), so menus and subtitles can use any system font the
//! user picks in the GUI, not only the hand-mapped families in `menu.rs`.

use std::path::PathBuf;

/// One installed font family with the file of its regular face (first face found otherwise).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemFont {
    pub family: String,
    pub path: PathBuf,
    /// Face index inside a collection (`.ttc`); 0 for a single-face file.
    #[serde(skip)]
    pub index: u32,
}

fn scan() -> Vec<SystemFont> {
    use fontdb::{Source, Style, Weight};
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    // Per lowercase family: (is_regular, entry); a regular upright face beats the first one found.
    let mut map: std::collections::BTreeMap<String, (bool, SystemFont)> = Default::default();
    for f in db.faces() {
        let path = match &f.source {
            Source::File(p) => p.clone(),
            Source::SharedFile(p, _) => p.clone(),
            _ => continue,
        };
        let Some((family, _)) = f.families.first() else { continue };
        let regular = f.style == Style::Normal && f.weight == Weight::NORMAL;
        let font = SystemFont { family: family.clone(), path, index: f.index };
        let e = map.entry(family.to_lowercase()).or_insert_with(|| (regular, font.clone()));
        if regular && !e.0 {
            *e = (true, font);
        }
    }
    map.into_values().map(|(_, f)| f).collect()
}

/// All installed font families, sorted case-insensitively and deduplicated, scanned once.
pub fn system_fonts() -> &'static [SystemFont] {
    static FONTS: std::sync::OnceLock<Vec<SystemFont>> = std::sync::OnceLock::new();
    FONTS.get_or_init(scan)
}

/// The installed font file for `family` (case-insensitive), if any.
pub fn find_family(family: &str) -> Option<&'static SystemFont> {
    let want = family.trim().to_lowercase();
    system_fonts().iter().find(|f| f.family.to_lowercase() == want)
}

/// Han-unification region of a subtitle track: `"ja"`, `"ko"`, `"zh-HK"`, `"zh-TW"` or `"zh-CN"`,
/// from its language tag (`jpn`, `zh-Hant-TW`, `yue`, …) and, for Chinese or untagged tracks,
/// hints in its title or file name (`繁體`, `Traditional`, `HK`, `简体`, `SC`, …). Latin hints match
/// whole words only, case-insensitively. Chinese without hints is Simplified; an untagged track
/// without hints and every other language give `None`.
pub fn cjk_region(lang: Option<&str>, title: Option<&str>) -> Option<&'static str> {
    let lang = lang.unwrap_or("").trim().to_ascii_lowercase();
    let mut tags = lang.split(['-', '_']);
    let primary = tags.next().unwrap_or("");
    let subtags: Vec<&str> = tags.collect();
    let chinese = match primary {
        "ja" | "jpn" => return Some("ja"),
        "ko" | "kor" => return Some("ko"),
        "yue" => return Some("zh-HK"),
        "zh" | "chi" | "zho" | "cmn" => true,
        "" | "und" => false,
        _ => return None,
    };
    let has = |s: &[&str]| subtags.iter().any(|t| s.contains(t));
    if has(&["hk", "mo"]) {
        return Some("zh-HK");
    }
    if has(&["tw", "hant"]) {
        return Some("zh-TW");
    }
    if has(&["cn", "sg", "hans"]) {
        return Some("zh-CN");
    }
    let title = title.unwrap_or("");
    let words: Vec<String> = title.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect();
    let hint = |cjk: &[&str], latin: &[&str]| cjk.iter().any(|h| title.contains(h)) || words.iter().any(|w| latin.contains(&w.as_str()));
    if hint(&["香港", "粵", "粤"], &["hk", "cantonese"]) {
        Some("zh-HK")
    } else if hint(&["繁體", "繁体", "正體"], &["tc", "trad", "traditional", "tw", "hant", "cht"]) {
        Some("zh-TW")
    } else if hint(&["简体", "簡體"], &["sc", "simp", "simplified", "cn", "hans", "chs"]) || chinese {
        Some("zh-CN")
    } else {
        None
    }
}

/// Font families that must beat the generic (Simplified-Chinese-first) CJK fallbacks for a
/// [`cjk_region`] tag, so unified Han characters take that region's glyph forms.
pub fn preferred_families(region: &str) -> &'static [&'static str] {
    match region {
        "ja" => &["Yu Gothic", "Meiryo", "MS Gothic", "Noto Sans CJK JP"],
        "zh-HK" => &["Microsoft JhengHei", "MingLiU_HKSCS", "Noto Sans CJK HK", "Noto Sans CJK TC"],
        "zh-TW" => &["Microsoft JhengHei", "PMingLiU", "Noto Sans CJK TC"],
        "zh-CN" => &["Microsoft YaHei", "SimSun", "Noto Sans CJK SC"],
        "ko" => &["Malgun Gothic", "Noto Sans CJK KR"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_region_from_language_tags() {
        let r = |l: &str| cjk_region(Some(l), None);
        for (l, want) in [
            ("jpn", "ja"),
            ("ja", "ja"),
            ("JA-jp", "ja"),
            ("kor", "ko"),
            ("ko-KR", "ko"),
            ("yue", "zh-HK"),
            ("zh-HK", "zh-HK"),
            ("zh-Hant-HK", "zh-HK"),
            ("zh_TW", "zh-TW"),
            ("zh-Hant", "zh-TW"),
            ("zh-CN", "zh-CN"),
            ("zh-Hans", "zh-CN"),
            ("chi", "zh-CN"),
            ("zho", "zh-CN"),
            ("ZH", "zh-CN"),
        ] {
            assert_eq!(r(l), Some(want), "{l}");
        }
        assert_eq!(r("eng"), None);
        assert_eq!(r("ger"), None);
        assert_eq!(cjk_region(None, None), None);
        assert_eq!(r("und"), None);
    }

    #[test]
    fn cjk_region_from_title_hints() {
        let r = |l: Option<&str>, t: &str| cjk_region(l, Some(t));
        for t in ["繁體中文", "繁体", "正體字幕", "Chinese (TC)", "Trad. Chinese", "Traditional", "zh-TW", "movie.zh-Hant.srt", "CHT"] {
            assert_eq!(r(Some("chi"), t), Some("zh-TW"), "{t}");
        }
        for t in ["香港", "粵語", "粤语", "Cantonese", "Chinese HK", "繁體 (HK)"] {
            assert_eq!(r(Some("zho"), t), Some("zh-HK"), "{t}");
        }
        for t in ["简体", "簡體中文", "SC", "Simp", "Simplified Chinese", "zh-CN"] {
            assert_eq!(r(Some("chi"), t), Some("zh-CN"), "{t}");
        }
        // Whole words only: no "TC" inside "Etcetera", no "HK" inside "Hkx".
        assert_eq!(r(Some("chi"), "Etcetera Hkx"), Some("zh-CN"));
        assert_eq!(r(Some("chi"), "etc"), Some("zh-CN"));
        // The tag wins over hints; untagged tracks use hints alone; other languages ignore them.
        assert_eq!(r(Some("zh-CN"), "Traditional"), Some("zh-CN"));
        assert_eq!(r(None, "繁體中文"), Some("zh-TW"));
        assert_eq!(r(Some("und"), "Simplified"), Some("zh-CN"));
        assert_eq!(r(None, "English SDH"), None);
        assert_eq!(r(Some("eng"), "Traditional"), None);
        assert_eq!(r(Some("jpn"), "繁體"), Some("ja"));
    }

    #[test]
    fn regional_families_put_local_forms_first() {
        assert_eq!(preferred_families("ja"), ["Yu Gothic", "Meiryo", "MS Gothic", "Noto Sans CJK JP"]);
        assert_eq!(preferred_families("zh-HK")[..2], ["Microsoft JhengHei", "MingLiU_HKSCS"]);
        assert_eq!(preferred_families("zh-TW"), ["Microsoft JhengHei", "PMingLiU", "Noto Sans CJK TC"]);
        assert_eq!(preferred_families("zh-CN"), ["Microsoft YaHei", "SimSun", "Noto Sans CJK SC"]);
        assert_eq!(preferred_families("ko"), ["Malgun Gothic", "Noto Sans CJK KR"]);
        // Only canonical tags: raw codes and other languages leave the default chain alone.
        for l in ["jpn", "chi", "eng", ""] {
            assert!(preferred_families(l).is_empty(), "{l}");
        }
        // No Simplified-Chinese face sneaks into a Japanese or Traditional list.
        for r in ["ja", "zh-TW", "zh-HK"] {
            assert!(!preferred_families(r).iter().any(|f| f.contains("YaHei") || f.contains("SimSun") || f.ends_with(" SC")), "{r}");
        }
    }

    #[test]
    fn collection_faces_resolve_to_their_own_index() {
        // Wherever a CJK collection is installed, each family maps to its own face, not face 0.
        if let (Some(a), Some(b)) = (find_family("MS Gothic"), find_family("MS PGothic")) {
            assert_eq!(a.path, b.path);
            assert_ne!(a.index, b.index);
            let data = std::fs::read(&b.path).unwrap();
            assert!(ab_glyph::FontVec::try_from_vec_and_index(data, b.index).is_ok());
        }
    }

    #[test]
    fn sorted_unique_and_findable() {
        let all = system_fonts();
        let keys: Vec<String> = all.iter().map(|f| f.family.to_lowercase()).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "sorted + unique");
        if let Some(f) = all.first() {
            assert_eq!(find_family(&f.family.to_uppercase()).map(|x| &x.path), Some(&f.path));
        }
        assert!(find_family("No Such Family 123").is_none());
    }
}
