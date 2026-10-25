//! tsMuxeR project of a Blu-ray: the `.meta` files that turn the encoded `title_NN.mkv` files
//! (H.264 + AC-3/DTS/LPCM) and the subtitle files into `BDMV` + `CERTIFICATE`.
//!
//! Every title is muxed by its own tsMuxeR run into its own playlist + clip (so each title keeps
//! its own frame rate); the runs share the disc through `--mplsOffset/--m2tsOffset`
//! ([`BdProject::title_meta_text`]). Players address audio/subtitle tracks by position, so all
//! titles have the same track layout – the build harmonises the tracks first.

use crate::author::Resolution;
use crate::bd::BdRate;
use crate::encoder::AudioKind;

/// Kind of a subtitle track of the disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubKind {
    /// SRT, rendered to PGS by tsMuxeR.
    Text,
    /// PGS `.sup` stream.
    Pgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BdSubTrack {
    pub kind: SubKind,
    /// ISO 639-2 tag.
    pub lang: Option<String>,
}

/// Files of one title in the work directory.
#[derive(Debug, Clone, PartialEq)]
pub struct BdTitleFiles {
    /// Encoded `title_NN.mkv`.
    pub mkv: String,
    /// Real duration of the encoded file.
    pub duration_secs: f64,
    /// Chapter starts inside the title (first is 0).
    pub chapters: Vec<f64>,
    /// One file per disc subtitle track (real or placeholder).
    pub subs: Vec<String>,
    /// Frame rate of this title.
    pub rate: BdRate,
    /// Codec of every disc audio track in this title's mkv.
    pub audio_kinds: Vec<AudioKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BdProject {
    pub titles: Vec<BdTitleFiles>,
    /// Language of every audio track (same layout in every title).
    pub audio_langs: Vec<Option<String>>,
    pub sub_tracks: Vec<BdSubTrack>,
    pub resolution: Resolution,
}

/// BD playlists hold at most 999 chapter marks.
const MAX_BD_CHAPTERS: usize = 999;

/// ISO 639-2/B tags (as ffprobe reports them) that differ from the /T tags tsMuxeR expects.
const B_TO_T: [(&str, &str); 20] = [
    ("alb", "sqi"), ("arm", "hye"), ("baq", "eus"), ("bur", "mya"), ("chi", "zho"), ("cze", "ces"), ("dut", "nld"),
    ("fre", "fra"), ("geo", "kat"), ("ger", "deu"), ("gre", "ell"), ("ice", "isl"), ("mac", "mkd"), ("mao", "mri"),
    ("may", "msa"), ("per", "fas"), ("rum", "ron"), ("slo", "slk"), ("tib", "bod"), ("wel", "cym"),
];

/// Three-letter language code for the meta file (`und` if unknown).
pub fn lang_code(tag: Option<&str>) -> String {
    let t = tag.map(str::trim).map(str::to_ascii_lowercase).unwrap_or_default();
    if t.len() != 3 || !t.chars().all(|c| c.is_ascii_lowercase()) {
        return "und".into();
    }
    B_TO_T.iter().find(|(b, _)| *b == t).map_or(t.clone(), |(_, t)| (*t).to_string())
}

/// `HH:MM:SS.mmm` as tsMuxeR's `--custom-chapters` wants it.
pub fn chapter_time(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

impl BdProject {
    /// Meta file of title `i`, written as playlist / clip number `offset` (UTF-8, no BOM, one line
    /// per track).
    pub fn title_meta_text(&self, i: usize, offset: usize) -> String {
        let t = &self.titles[i];
        let files = |pick: &dyn Fn(&BdTitleFiles) -> &str| format!("\"{}\"", pick(t));
        let (w, h) = self.resolution.frame_size(crate::author::TvStandard::Pal);
        let chapters: Vec<String> = marks(t).into_iter().map(chapter_time).collect();
        let mut out = format!(
            "MUXOPT --no-pcr-on-video-pid --new-audio-pes --blu-ray --vbr --custom-chapters={} --vbv-len=500{}
",
            chapters.join(";"),
            offset_options(offset)
        );
        let mkvs = files(&|t| &t.mkv);
        out.push_str(&format!("V_MPEG4/ISO/AVC, {mkvs}, insertSEI, contSPS, track=1
"));
        for (k, lang) in self.audio_langs.iter().enumerate() {
            let codec = match t.audio_kinds.get(k).copied().unwrap_or(AudioKind::Ac3) {
                AudioKind::Ac3 => "A_AC3",
                AudioKind::Dts => "A_DTS",
                AudioKind::Lpcm => "A_LPCM",
            };
            out.push_str(&format!("{codec}, {mkvs}, track={}, lang={}
", 2 + k, lang_code(lang.as_deref())));
        }
        let font_size = (65.0 * h as f64 / 1080.0).round() as u32;
        let fps = format!("{:.3}", t.rate.fps_f64());
        let fps = fps.trim_end_matches('0').trim_end_matches('.');
        for (k, track) in self.sub_tracks.iter().enumerate() {
            let files = files(&|t| &t.subs[k]);
            let lang = lang_code(track.lang.as_deref());
            match track.kind {
                SubKind::Text => out.push_str(&format!(
                    "S_TEXT/UTF8, {files}, font-name=\"Arial\", font-size={font_size}, font-color=0xffffffff, \
                     bottom-offset=24, font-border=5, text-align=center, video-width={w}, video-height={h}, fps={fps}, lang={lang}\n"
                )),
                SubKind::Pgs => out.push_str(&format!("S_HDMV/PGS, {files}, lang={lang}\n")),
            }
        }
        out
    }
}

fn marks(t: &BdTitleFiles) -> Vec<f64> {
    let mut marks = t.chapters.clone();
    marks.truncate(MAX_BD_CHAPTERS);
    marks
}

/// ` --mplsOffset=N --m2tsOffset=N` for the N-th playlist of a disc built in several runs.
fn offset_options(offset: usize) -> String {
    if offset == 0 { String::new() } else { format!(" --mplsOffset={offset} --m2tsOffset={offset}") }
}

/// Meta file of one menu clip: video, its AC-3 sound and the Interactive Graphics stream (an
/// `.sup` that `bd_patch` turns from PGS into IG after muxing). Menu `offset` is its playlist /
/// clip number.
pub fn menu_meta_text(mkv: &str, sup: Option<&str>, offset: usize) -> String {
    let mut out = format!("MUXOPT --no-pcr-on-video-pid --new-audio-pes --blu-ray --vbr --vbv-len=500{}
", offset_options(offset));
    out.push_str(&format!("V_MPEG4/ISO/AVC, \"{mkv}\", insertSEI, contSPS, track=1
"));
    out.push_str(&format!("A_AC3, \"{mkv}\", track=2, lang=und
"));
    if let Some(sup) = sup {
        out.push_str(&format!("S_HDMV/PGS, \"{sup}\", lang=und
"));
    }
    out
}

/// An SRT with one invisible cue: stands in for a text subtitle track a title does not have.
pub const EMPTY_SRT: &str = "1\n00:00:00,000 --> 00:00:00,040\n \n\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> BdProject {
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        BdProject {
            titles: vec![
                BdTitleFiles {
                    mkv: "title_01.mkv".into(),
                    duration_secs: 252.48,
                    chapters: vec![0.0, 100.0],
                    subs: vec!["title_01_s1.srt".into(), "title_01_s2.sup".into()],
                    rate,
                    audio_kinds: vec![AudioKind::Ac3, AudioKind::Dts],
                },
                BdTitleFiles {
                    mkv: "title_02.mkv".into(),
                    duration_secs: 60.0,
                    chapters: vec![0.0],
                    subs: vec!["title_02_s1.srt".into(), "title_02_s2.sup".into()],
                    rate: BdRate { fps: (25, 1), fake_interlaced: true },
                    audio_kinds: vec![AudioKind::Lpcm, AudioKind::Ac3],
                },
            ],
            audio_langs: vec![Some("ger".into()), None],
            sub_tracks: vec![
                BdSubTrack { kind: SubKind::Text, lang: Some("ger".into()) },
                BdSubTrack { kind: SubKind::Pgs, lang: Some("eng".into()) },
            ],
            resolution: Resolution::Hd1080,
        }
    }

    #[test]
    fn chapter_times() {
        assert_eq!(chapter_time(252.48), "00:04:12.480");
        assert_eq!(chapter_time(3723.0049), "01:02:03.005");
        assert_eq!(chapter_time(-1.0), "00:00:00.000");
    }

    #[test]
    fn meta_text_shape() {
        let meta = project().title_meta_text(0, 0);
        let lines: Vec<&str> = meta.lines().collect();
        assert_eq!(
            lines[0],
            "MUXOPT --no-pcr-on-video-pid --new-audio-pes --blu-ray --vbr --custom-chapters=00:00:00.000;00:01:40.000 --vbv-len=500"
        );
        assert_eq!(lines[1], r#"V_MPEG4/ISO/AVC, "title_01.mkv", insertSEI, contSPS, track=1"#);
        assert_eq!(lines[2], r#"A_AC3, "title_01.mkv", track=2, lang=deu"#);
        assert_eq!(lines[3], r#"A_DTS, "title_01.mkv", track=3, lang=und"#);
        assert_eq!(
            lines[4],
            r#"S_TEXT/UTF8, "title_01_s1.srt", font-name="Arial", font-size=65, font-color=0xffffffff, bottom-offset=24, font-border=5, text-align=center, video-width=1920, video-height=1080, fps=23.976, lang=deu"#
        );
        assert_eq!(lines[5], r#"S_HDMV/PGS, "title_01_s2.sup", lang=eng"#);
        assert_eq!(lines.len(), 6);
        assert!(meta.is_ascii() && !meta.contains('\r'));
    }

    #[test]
    fn every_title_has_its_own_rate_and_codecs() {
        let p = project();
        let m = p.title_meta_text(1, 1);
        assert!(m.starts_with("MUXOPT --no-pcr-on-video-pid --new-audio-pes --blu-ray --vbr --custom-chapters=00:00:00.000 --vbv-len=500 --mplsOffset=1 --m2tsOffset=1
"));
        assert!(m.contains(r#"V_MPEG4/ISO/AVC, "title_02.mkv", insertSEI"#) && !m.contains("title_01"));
        assert!(m.contains(r#"A_LPCM, "title_02.mkv", track=2"#) && m.contains(r#"A_AC3, "title_02.mkv", track=3"#));
        assert!(m.contains("fps=25,"), "{m}");
    }

    #[test]
    fn single_title_720p() {
        let mut p = project();
        p.resolution = Resolution::Hd720;
        p.titles[0].rate = BdRate { fps: (60000, 1001), fake_interlaced: false };
        let meta = p.title_meta_text(0, 0);
        assert!(meta.contains(r#"V_MPEG4/ISO/AVC, "title_01.mkv", insertSEI"#));
        assert!(meta.contains("font-size=43") && meta.contains("video-width=1280, video-height=720, fps=59.94,"));
    }

    #[test]
    fn menu_meta() {
        let menu = menu_meta_text("menu_01.mkv", Some("menu_01.sup"), 1);
        assert!(menu.lines().next().unwrap().ends_with("--mplsOffset=1 --m2tsOffset=1"));
        assert!(menu.contains(r#"S_HDMV/PGS, "menu_01.sup", lang=und"#));
        assert!(!menu_meta_text("m.mkv", None, 0).contains("PGS"));
    }

    #[test]
    fn language_codes() {
        assert_eq!(lang_code(Some("ger")), "deu");
        assert_eq!(lang_code(Some("FRE")), "fra");
        assert_eq!(lang_code(Some("eng")), "eng");
        assert_eq!(lang_code(Some("de")), "und");
        assert_eq!(lang_code(None), "und");
    }

    #[test]
    fn chapter_limit() {
        let mut p = project();
        p.titles[0].chapters = (0..2000).map(|c| c as f64).collect();
        assert_eq!(p.title_meta_text(0, 0).lines().next().unwrap().matches(';').count(), 998);
    }
}
