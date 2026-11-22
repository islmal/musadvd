//! Volume label for DVD-Video discs (ISO 9660 volume identifier: d-characters, max. 32).

/// Fallback when nothing usable is left.
pub const DEFAULT_LABEL: &str = "DVD_VIDEO";
/// ISO 9660 volume identifier length.
pub const MAX_LABEL_LEN: usize = 32;

/// Sanitises a project name into a DVD volume label:
/// upper case, spaces and punctuation become `_`, everything outside `[A-Z0-9_]` is removed,
/// runs of `_` collapse to one, at most 32 characters, `DVD_VIDEO` if nothing (or only
/// underscores) is left.
pub fn sanitize_volume_label(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        let c = c.to_ascii_uppercase();
        let mapped = if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
            Some(c)
        } else if c.is_whitespace() || c.is_ascii_punctuation() {
            Some('_')
        } else {
            None // non-ASCII letters etc. are stripped
        };
        if let Some(m) = mapped {
            if !(m == '_' && out.ends_with('_')) {
                out.push(m);
            }
        }
    }
    out.truncate(MAX_LABEL_LEN);
    if out.chars().all(|c| c == '_') {
        DEFAULT_LABEL.to_string()
    } else {
        out
    }
}

/// UDF volume identifier: a 32-byte dstring holds 30 8-bit (OSTA CS0) characters.
pub const MAX_UDF_LABEL_LEN: usize = 30;

/// Cleans a free-text UDF volume label (UDF 1.02 bridge and UDF 2.50 alike): control characters
/// and characters outside Latin-1 are removed (the dstring is 8-bit), trimmed, at most 30
/// characters. Empty means "same as the ISO 9660 label".
pub fn sanitize_udf_label(input: &str) -> String {
    let out: String = input.chars().filter(|c| (*c as u32) < 256 && !c.is_control()).collect();
    out.trim().chars().take(MAX_UDF_LABEL_LEN).collect::<String>().trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udf_rules() {
        assert_eq!(sanitize_udf_label("  Urlaub 2025 – Teil 1 "), "Urlaub 2025  Teil 1", "en dash is outside Latin-1");
        assert_eq!(sanitize_udf_label("Ärger & Öl"), "Ärger & Öl");
        assert_eq!(sanitize_udf_label("Война"), "");
        assert_eq!(sanitize_udf_label("a	b"), "ab");
        assert_eq!(sanitize_udf_label(&"x".repeat(40)).len(), MAX_UDF_LABEL_LEN);
    }

    #[test]
    fn rules() {
        assert_eq!(sanitize_volume_label("Urlaub 2025"), "URLAUB_2025");
        assert_eq!(sanitize_volume_label("my  movie -- part.1"), "MY_MOVIE_PART_1");
        assert_eq!(sanitize_volume_label("Ärger & Öl"), "RGER_L", "non-ASCII letters are stripped");
        assert_eq!(sanitize_volume_label("Война и мир"), DEFAULT_LABEL, "only underscores left");
        assert_eq!(sanitize_volume_label("Война"), DEFAULT_LABEL);
        assert_eq!(sanitize_volume_label(""), DEFAULT_LABEL);
        assert_eq!(sanitize_volume_label("___a___b___"), "_A_B_");
        let long = sanitize_volume_label("abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(long, "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345");
        assert_eq!(long.len(), 32);
        // Idempotent: a sanitised label stays the same.
        for s in ["URLAUB_2025", "DVD_VIDEO", "_A_B_"] {
            assert_eq!(sanitize_volume_label(s), s);
        }
    }
}
