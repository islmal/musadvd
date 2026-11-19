//! Installed OS font families (via `fontdb`), so menus and subtitles can use any system font the
//! user picks in the GUI, not only the hand-mapped families in `menu.rs`.

use std::path::PathBuf;

/// One installed font family with the file of its regular face (first face found otherwise).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemFont {
    pub family: String,
    pub path: PathBuf,
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
        let e = map.entry(family.to_lowercase()).or_insert_with(|| (regular, SystemFont { family: family.clone(), path: path.clone() }));
        if regular && !e.0 {
            *e = (true, SystemFont { family: family.clone(), path });
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

#[cfg(test)]
mod tests {
    use super::*;

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
