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

#[cfg(test)]
mod tests {
    use super::*;

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
