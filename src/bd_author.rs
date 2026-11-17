//! Blu-ray mux jobs: turns the encoded `title_NN.mkv` files (H.264 + AC-3/DTS/LPCM) and the
//! subtitle files into [`MuxJob`]s for the native BDAV muxer ([`crate::bd_mux`]).
//!
//! Every title is muxed on its own into its own playlist + clip (so each title keeps its own
//! frame rate); the clip / playlist numbers continue after the menus ([`BdProject::mux_job`]).
//! Players address audio/subtitle tracks by position, so all titles have the same track layout –
//! the build harmonises the tracks first.

use std::path::{Path, PathBuf};

use crate::author::Resolution;
use crate::bd::BdRate;
use crate::bd_clip::Codec;
use crate::bd_mux::{MuxJob, SubSpec, VideoSpec};
use crate::encoder::AudioKind;

/// Kind of a subtitle track of the disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubKind {
    /// SRT, rendered to PGS natively (`crate::pgs`).
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
    /// Frame size of this title (titles may differ from each other and from the menus).
    pub resolution: Resolution,
    /// Stream-copied video: the disc's video buffer (bit/s, bits) the muxer's timing uses.
    pub copy_vbv: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BdProject {
    pub titles: Vec<BdTitleFiles>,
    /// Language of every audio track (same layout in every title).
    pub audio_langs: Vec<Option<String>>,
    pub sub_tracks: Vec<BdSubTrack>,
}

/// BD playlists hold at most 999 chapter marks.
const MAX_BD_CHAPTERS: usize = 999;

/// ISO 639-2/B tags (as ffprobe reports them) that differ from the /T tags the clip files use.
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

/// Three-letter tag as the clip files carry it.
fn lang_bytes(tag: Option<&str>) -> [u8; 3] {
    let code = lang_code(tag);
    [code.as_bytes()[0], code.as_bytes()[1], code.as_bytes()[2]]
}

/// Marks at whole milliseconds (the resolution `--custom-chapters` had).
pub fn chapter_secs(secs: f64) -> f64 {
    (secs.max(0.0) * 1000.0).round() / 1000.0
}

impl BdProject {
    /// Mux job of title `i`, written as playlist / clip number `number` into `bdmv`.
    pub fn mux_job(&self, i: usize, number: usize, work: &Path, bdmv: &Path) -> MuxJob {
        let t = &self.titles[i];
        let (width, _) = t.resolution.frame_size(crate::author::TvStandard::Pal);
        MuxJob {
            mkv: work.join(&t.mkv),
            video: VideoSpec { fps: t.rate.fps, width, interlaced: t.rate.fake_interlaced, copy_vbv: t.copy_vbv },
            audio: self
                .audio_langs
                .iter()
                .enumerate()
                .map(|(k, lang)| {
                    let codec = match t.audio_kinds.get(k).copied().unwrap_or(AudioKind::Ac3) {
                        AudioKind::Ac3 => Codec::Ac3,
                        AudioKind::Dts => Codec::Dts,
                        AudioKind::Lpcm => Codec::Lpcm,
                    };
                    (codec, lang_bytes(lang.as_deref()))
                })
                .collect(),
            subs: self.sub_tracks.iter().enumerate().map(|(k, s)| SubSpec { sup: work.join(&t.subs[k]), lang: lang_bytes(s.lang.as_deref()), ig: false }).collect(),
            chapters: t.chapters.iter().take(MAX_BD_CHAPTERS).map(|&c| chapter_secs(c)).collect(),
            number,
            still: false,
            bdmv: bdmv.to_path_buf(),
            duration_secs: t.duration_secs,
        }
    }
}

/// Mux job of one menu clip: video, its AC-3 sound and the Interactive Graphics stream (`sup`).
/// Menu `number` is its playlist / clip number. With `intro_secs` the playlist gets a second
/// mark where the loop behind the intro starts (the menu object restarts at it); `still` pages
/// wait forever at their end.
#[allow(clippy::too_many_arguments)]
pub fn menu_job(mkv: PathBuf, sup: Option<PathBuf>, number: usize, intro_secs: Option<f64>, still: bool, rate: BdRate, width: u32, bdmv: &Path, duration_secs: f64) -> MuxJob {
    // Rounded up to the millisecond: a mark before the keyframe would restart the intro.
    let chapters = intro_secs.map_or_else(Vec::new, |t| vec![0.0, (t * 1000.0 - 1e-6).ceil() / 1000.0]);
    MuxJob {
        mkv,
        video: VideoSpec { fps: rate.fps, width, interlaced: rate.fake_interlaced, copy_vbv: None },
        audio: vec![(Codec::Ac3, *b"und")],
        subs: sup.into_iter().map(|sup| SubSpec { sup, lang: *b"und", ig: true }).collect(),
        chapters,
        number,
        still,
        bdmv: bdmv.to_path_buf(),
        duration_secs,
    }
}

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
                    subs: vec!["title_01_s1.sup".into(), "title_01_s2.sup".into()],
                    rate,
                    audio_kinds: vec![AudioKind::Ac3, AudioKind::Dts],
                    resolution: Resolution::Hd1080,
                    copy_vbv: None,
                },
                BdTitleFiles {
                    mkv: "title_02.mkv".into(),
                    duration_secs: 60.0,
                    chapters: vec![0.0],
                    subs: vec!["title_02_s1.sup".into(), "title_02_s2.sup".into()],
                    rate: BdRate { fps: (25, 1), fake_interlaced: true },
                    audio_kinds: vec![AudioKind::Lpcm, AudioKind::Ac3],
                    resolution: Resolution::Hd1080,
                    copy_vbv: None,
                },
            ],
            audio_langs: vec![Some("ger".into()), None],
            sub_tracks: vec![
                BdSubTrack { kind: SubKind::Text, lang: Some("ger".into()) },
                BdSubTrack { kind: SubKind::Pgs, lang: Some("eng".into()) },
            ],
        }
    }

    #[test]
    fn title_job_shape() {
        let job = project().mux_job(0, 0, Path::new("w"), Path::new("w/BD/BDMV"));
        assert_eq!(job.mkv, Path::new("w").join("title_01.mkv"));
        assert_eq!(job.audio, vec![(Codec::Ac3, *b"deu"), (Codec::Dts, *b"und")]);
        assert_eq!(job.subs.iter().map(|s| (s.lang, s.ig)).collect::<Vec<_>>(), vec![(*b"deu", false), (*b"eng", false)]);
        assert_eq!(job.chapters, vec![0.0, 100.0]);
        assert_eq!((job.video.fps, job.video.width, job.video.interlaced), ((24000, 1001), 1920, false));
    }

    #[test]
    fn every_title_has_its_own_rate_and_codecs() {
        let job = project().mux_job(1, 3, Path::new("w"), Path::new("b"));
        assert_eq!(job.number, 3);
        assert_eq!(job.audio[0].0, Codec::Lpcm);
        assert_eq!(job.audio[1].0, Codec::Ac3);
        assert_eq!((job.video.fps, job.video.interlaced), ((25, 1), true));
        assert!(job.subs[0].sup.ends_with("title_02_s1.sup"));
    }

    #[test]
    fn single_title_720p() {
        let mut p = project();
        p.titles[0].resolution = Resolution::Hd720;
        p.titles[0].rate = BdRate { fps: (60000, 1001), fake_interlaced: false };
        let job = p.mux_job(0, 0, Path::new("w"), Path::new("b"));
        assert_eq!((job.video.fps, job.video.width), ((60000, 1001), 1280));
    }

    #[test]
    fn menu_marks() {
        let rate = BdRate { fps: (24000, 1001), fake_interlaced: false };
        let menu = |intro| menu_job("m.mkv".into(), Some("m.sup".into()), 1, intro, true, rate, 1920, Path::new("b"), 10.0);
        let m = menu(None);
        assert!(m.chapters.is_empty() && m.still && m.number == 1 && m.subs[0].ig);
        assert_eq!(menu(Some(3.003)).chapters, vec![0.0, 3.003]);
        // Never before the keyframe at the loop start.
        assert_eq!(menu(Some(3.0447083)).chapters, vec![0.0, 3.045]);
        assert!(menu_job("m.mkv".into(), None, 0, None, false, rate, 1920, Path::new("b"), 1.0).subs.is_empty());
    }

    #[test]
    fn language_codes() {
        assert_eq!(lang_code(Some("ger")), "deu");
        assert_eq!(lang_code(Some("FRE")), "fra");
        assert_eq!(lang_code(Some("eng")), "eng");
        assert_eq!(lang_code(Some("de")), "und");
        assert_eq!(lang_code(None), "und");
        assert_eq!(lang_bytes(Some("ger")), *b"deu");
    }

    #[test]
    fn chapter_marks_are_milliseconds() {
        assert_eq!(chapter_secs(252.48), 252.48);
        assert_eq!(chapter_secs(3723.0049), 3723.005);
        assert_eq!(chapter_secs(-1.0), 0.0);
    }
}
