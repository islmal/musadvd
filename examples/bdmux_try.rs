use convertx_rust::bd_clip::Codec;
use convertx_rust::bd_mux::*;
use convertx_rust::tools::Toolchain;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a[1] == "sup" {
        let text = std::fs::read_to_string(&a[2]).unwrap();
        let (cues, _) = convertx_rust::textsub::parse_srt(&text);
        std::fs::write(&a[3], convertx_rust::pgs::build_sup(&cues, &convertx_rust::textsub::Canvas::hd(1920, 1080), (24000, 1001))).unwrap();
        return;
    }
    let (mkv, out, num) = (&a[1], &a[2], a[3].parse::<usize>().unwrap());
    let fps: (u32, u32) = (a[4].parse().unwrap(), a[5].parse().unwrap());
    let interlaced = a[6] == "i";
    let codecs: Vec<Codec> = a[7].split(',').filter(|s| !s.is_empty()).map(|c| match c { "ac3" => Codec::Ac3, "dts" => Codec::Dts, _ => Codec::Lpcm }).collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = rt.block_on(Toolchain::detect()).unwrap();
    let bdmv = std::path::PathBuf::from(out).join("BDMV");
    create_skeleton(&bdmv).unwrap();
    let job = MuxJob {
        mkv: mkv.into(),
        video: VideoSpec { fps, width: 1920, interlaced },
        audio: codecs.into_iter().enumerate().map(|(i, c)| (c, [b"deu", b"eng", b"fra"][i % 3].to_owned().to_owned())).collect(),
        subs: a.get(8).map(|f| vec![SubSpec { sup: f.into(), lang: *b"deu", ig: a.get(9).is_some() }]).unwrap_or_default(),
        chapters: vec![0.0, 1.0],
        number: num,
        still: false,
        bdmv,
        duration_secs: 2.0,
    };
    println!("{}", rt.block_on(mux_title(&tools, &job)).unwrap());
}
