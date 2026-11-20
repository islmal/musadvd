//! Minimal H.264 bitstream tools for Blu-ray stream copies: RBSP (un)escaping, the SPS up to and
//! including its VUI (rewriting only the timing and HRD part), buffering-period and
//! picture-timing SEI, and the leaky-bucket buffer model that checks a stream and chooses the
//! SEI delays. References: ITU-T H.264 7.3.2.1.1 (SPS), 7.3.2.3 (SEI), E.1.1/E.1.2 (VUI, HRD),
//! D.1.2/D.1.3 (buffering period, picture timing), C.1 (HRD buffer model).

/// NAL unit types used here.
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
/// SEI payload types.
pub const SEI_BUFFERING_PERIOD: u32 = 0;
pub const SEI_PIC_TIMING: u32 = 1;

/// Length of every delay field we write (`*_length_minus1` + 1), as x264 does.
const DELAY_BITS: usize = 24;

/// Emulation prevention removed: the RBSP of a NAL unit payload (without its header byte).
pub fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Emulation prevention added: an RBSP made safe for Annex B.
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Big-endian bit reader over an RBSP; every read returns `None` past the end.
pub struct BitReader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        BitReader { d, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn u(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.d.get(self.pos / 8)?;
            v = (v << 1) | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    pub fn flag(&mut self) -> Option<bool> {
        self.u(1).map(|b| b == 1)
    }

    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.u(zeros)?)) as u32)
    }

    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// Big-endian bit writer.
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub fn u(&mut self, v: u32, n: usize) {
        for i in (0..n).rev() {
            self.bit(v >> i & 1 == 1);
        }
    }

    pub fn bit(&mut self, b: bool) {
        if self.bits.is_multiple_of(8) {
            self.out.push(0);
        }
        if b {
            *self.out.last_mut().expect("pushed") |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    pub fn ue(&mut self, v: u32) {
        let v1 = u64::from(v) + 1;
        let len = 64 - v1.leading_zeros() as usize;
        self.u(0, len - 1);
        for i in (0..len).rev() {
            self.bit(v1 >> i & 1 == 1);
        }
    }

    /// The first `n` bits of `src`.
    pub fn copy(&mut self, src: &[u8], n: usize) {
        let mut r = BitReader::new(src);
        for _ in 0..n {
            self.bit(r.u(1).unwrap_or(0) == 1);
        }
    }

    pub fn byte_aligned(&self) -> bool {
        self.bits.is_multiple_of(8)
    }

    /// `rbsp_trailing_bits`: a one, then zeros to the byte boundary.
    pub fn finish(mut self) -> Vec<u8> {
        self.bit(true);
        while !self.byte_aligned() {
            self.bit(false);
        }
        self.out
    }

    /// Payload alignment of an SEI message (D.1.1): a one and zeros only when not aligned.
    fn sei_payload(mut self) -> Vec<u8> {
        if !self.byte_aligned() {
            self.bit(true);
            while !self.byte_aligned() {
                self.bit(false);
            }
        }
        self.out
    }
}

/// HRD parameters with a single CPB (more are parsed only to be skipped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hrd {
    /// Bit/s and bits of the first CPB.
    pub bit_rate: u64,
    pub cpb_size: u64,
    pub cbr: bool,
    pub initial_delay_bits: usize,
    pub removal_delay_bits: usize,
    pub output_delay_bits: usize,
    pub time_offset_bits: usize,
}

impl Hrd {
    /// The HRD we write for a disc with `rate` bit/s and `buffer` bits (VBR, 24-bit delays).
    pub fn for_disc(rate: u64, buffer: u64) -> Hrd {
        // bit_rate_scale = cpb_size_scale = 0: units of 64 bit/s and 16 bits, rounded down so the
        // signalled limits never exceed the disc's.
        Hrd {
            bit_rate: rate / 64 * 64,
            cpb_size: buffer / 16 * 16,
            cbr: false,
            initial_delay_bits: DELAY_BITS,
            removal_delay_bits: DELAY_BITS,
            output_delay_bits: DELAY_BITS,
            time_offset_bits: DELAY_BITS,
        }
    }

    fn parse(r: &mut BitReader) -> Option<Hrd> {
        let cpb_cnt = r.ue()? + 1;
        let (rate_scale, size_scale) = (r.u(4)?, r.u(4)?);
        let mut first = None;
        for _ in 0..cpb_cnt {
            let (rate, size, cbr) = (u64::from(r.ue()?) + 1, u64::from(r.ue()?) + 1, r.flag()?);
            first.get_or_insert((rate << (6 + rate_scale), size << (4 + size_scale), cbr));
        }
        let (bit_rate, cpb_size, cbr) = first?;
        Some(Hrd {
            bit_rate,
            cpb_size,
            cbr,
            initial_delay_bits: r.u(5)? as usize + 1,
            removal_delay_bits: r.u(5)? as usize + 1,
            output_delay_bits: r.u(5)? as usize + 1,
            time_offset_bits: r.u(5)? as usize,
        })
    }

    fn write(&self, w: &mut BitWriter) {
        w.ue(0); // cpb_cnt_minus1
        w.u(0, 4);
        w.u(0, 4);
        w.ue((self.bit_rate / 64).saturating_sub(1) as u32);
        w.ue((self.cpb_size / 16).saturating_sub(1) as u32);
        w.bit(self.cbr);
        w.u(self.initial_delay_bits as u32 - 1, 5);
        w.u(self.removal_delay_bits as u32 - 1, 5);
        w.u(self.output_delay_bits as u32 - 1, 5);
        w.u(self.time_offset_bits as u32, 5);
    }
}

/// `video_format`, `video_full_range_flag`, optional colour description.
type Signal = (u32, bool, Option<(u32, u32, u32)>);

/// VUI fields that survive a rewrite unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vui {
    aspect: Option<(u32, u32, u32)>,
    overscan: Option<bool>,
    signal: Option<Signal>,
    chroma_loc: Option<(u32, u32)>,
    /// `num_units_in_tick`, `time_scale`, `fixed_frame_rate_flag`.
    pub timing: Option<(u32, u32, bool)>,
    pub nal_hrd: Option<Hrd>,
    pub vcl_hrd: Option<Hrd>,
    pub pic_struct: bool,
    restriction: Option<(bool, [u32; 6])>,
}

impl Vui {
    fn parse(r: &mut BitReader) -> Option<Vui> {
        let mut v = Vui::default();
        if r.flag()? {
            let idc = r.u(8)?;
            let (w, h) = if idc == 255 { (r.u(16)?, r.u(16)?) } else { (0, 0) };
            v.aspect = Some((idc, w, h));
        }
        if r.flag()? {
            v.overscan = Some(r.flag()?);
        }
        if r.flag()? {
            let (format, full) = (r.u(3)?, r.flag()?);
            let colour = if r.flag()? { Some((r.u(8)?, r.u(8)?, r.u(8)?)) } else { None };
            v.signal = Some((format, full, colour));
        }
        if r.flag()? {
            v.chroma_loc = Some((r.ue()?, r.ue()?));
        }
        if r.flag()? {
            v.timing = Some((r.u(32)?, r.u(32)?, r.flag()?));
        }
        if r.flag()? {
            v.nal_hrd = Some(Hrd::parse(r)?);
        }
        if r.flag()? {
            v.vcl_hrd = Some(Hrd::parse(r)?);
        }
        if v.nal_hrd.is_some() || v.vcl_hrd.is_some() {
            r.flag()?; // low_delay_hrd_flag
        }
        v.pic_struct = r.flag()?;
        if r.flag()? {
            let mv = r.flag()?;
            let mut f = [0u32; 6];
            for x in &mut f {
                *x = r.ue()?;
            }
            v.restriction = Some((mv, f));
        }
        Some(v)
    }

    fn write(&self, w: &mut BitWriter) {
        w.bit(self.aspect.is_some());
        if let Some((idc, sw, sh)) = self.aspect {
            w.u(idc, 8);
            if idc == 255 {
                w.u(sw, 16);
                w.u(sh, 16);
            }
        }
        w.bit(self.overscan.is_some());
        if let Some(o) = self.overscan {
            w.bit(o);
        }
        w.bit(self.signal.is_some());
        if let Some((format, full, colour)) = self.signal {
            w.u(format, 3);
            w.bit(full);
            w.bit(colour.is_some());
            if let Some((p, t, m)) = colour {
                w.u(p, 8);
                w.u(t, 8);
                w.u(m, 8);
            }
        }
        w.bit(self.chroma_loc.is_some());
        if let Some((top, bottom)) = self.chroma_loc {
            w.ue(top);
            w.ue(bottom);
        }
        w.bit(self.timing.is_some());
        if let Some((units, scale, fixed)) = self.timing {
            w.u(units, 32);
            w.u(scale, 32);
            w.bit(fixed);
        }
        for hrd in [&self.nal_hrd, &self.vcl_hrd] {
            w.bit(hrd.is_some());
            if let Some(h) = hrd {
                h.write(w);
            }
        }
        if self.nal_hrd.is_some() || self.vcl_hrd.is_some() {
            w.bit(false); // low_delay_hrd_flag
        }
        w.bit(self.pic_struct);
        w.bit(self.restriction.is_some());
        if let Some((mv, f)) = self.restriction {
            w.bit(mv);
            for x in f {
                w.ue(x);
            }
        }
    }
}

/// A parsed sequence parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub frame_mbs_only: bool,
    pub vui: Option<Vui>,
    /// NAL header byte and RBSP, and the bit position of `vui_parameters_present_flag` in it.
    header: u8,
    rbsp: Vec<u8>,
    vui_at: usize,
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

impl Sps {
    /// Parses an SPS NAL unit (header byte included, emulation prevention still in).
    pub fn parse(nal: &[u8]) -> Option<Sps> {
        let (&header, payload) = nal.split_first()?;
        if header & 31 != NAL_SPS {
            return None;
        }
        let rbsp = unescape(payload);
        let mut r = BitReader::new(&rbsp);
        let profile = r.u(8)?;
        r.u(16)?; // constraint flags, level_idc
        let id = r.ue()?;
        if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
            let chroma = r.ue()?;
            if chroma == 3 {
                r.flag()?;
            }
            r.ue()?;
            r.ue()?;
            r.flag()?;
            if r.flag()? {
                for i in 0..if chroma == 3 { 12 } else { 8 } {
                    if r.flag()? {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        r.ue()?; // log2_max_frame_num_minus4
        match r.ue()? {
            0 => {
                r.ue()?;
            }
            1 => {
                r.flag()?;
                r.se()?;
                r.se()?;
                for _ in 0..r.ue()? {
                    r.se()?;
                }
            }
            _ => {}
        }
        r.ue()?; // max_num_ref_frames
        r.flag()?;
        r.ue()?;
        r.ue()?;
        let frame_mbs_only = r.flag()?;
        if !frame_mbs_only {
            r.flag()?;
        }
        r.flag()?;
        if r.flag()? {
            for _ in 0..4 {
                r.ue()?;
            }
        }
        let vui_at = r.pos();
        let vui = if r.flag()? { Some(Vui::parse(&mut r)?) } else { None };
        Some(Sps { id, frame_mbs_only, vui, header, rbsp, vui_at })
    }

    /// The source's NAL HRD, if it signals one.
    pub fn nal_hrd(&self) -> Option<&Hrd> {
        self.vui.as_ref()?.nal_hrd.as_ref()
    }

    /// This SPS as a NAL unit with Blu-ray timing (`fps`, fixed rate, two ticks per frame) and
    /// `hrd` as its only HRD; every other field is kept bit for bit.
    pub fn with_timing(&self, fps: (u32, u32), hrd: &Hrd) -> Vec<u8> {
        let mut vui = self.vui.clone().unwrap_or_default();
        vui.timing = Some((fps.1, 2 * fps.0, true));
        vui.nal_hrd = Some(hrd.clone());
        vui.vcl_hrd = None;
        // Our picture-timing SEI carries no pic_struct (progressive frames only).
        vui.pic_struct = false;
        let mut w = BitWriter::default();
        w.copy(&self.rbsp, self.vui_at);
        w.bit(true);
        vui.write(&mut w);
        let mut nal = vec![self.header];
        nal.extend(escape(&w.finish()));
        nal
    }
}

/// Payload types of the messages in an SEI NAL unit (header byte included).
pub fn sei_types(nal: &[u8]) -> Vec<u32> {
    sei_messages(nal).into_iter().map(|(t, _)| t).collect()
}

fn sei_messages(nal: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let rbsp = unescape(nal.get(1..).unwrap_or_default());
    let mut out = Vec::new();
    let mut i = 0;
    // A message needs at least type, size and the trailing byte after it.
    while i + 1 < rbsp.len() && !(rbsp[i] == 0x80 && i + 1 == rbsp.len()) {
        let read = |i: &mut usize| {
            let mut v = 0u32;
            while *i < rbsp.len() && rbsp[*i] == 0xFF {
                v += 255;
                *i += 1;
            }
            let last = rbsp.get(*i).copied().map(u32::from);
            *i += 1;
            last.map(|l| v + l)
        };
        let (Some(t), Some(size)) = (read(&mut i), read(&mut i)) else { break };
        let end = (i + size as usize).min(rbsp.len());
        out.push((t, rbsp[i..end].to_vec()));
        i = end;
    }
    out
}

fn sei_nal(messages: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut rbsp = Vec::new();
    for (t, payload) in messages {
        for mut v in [*t as usize, payload.len()] {
            while v >= 255 {
                rbsp.push(0xFF);
                v -= 255;
            }
            rbsp.push(v as u8);
        }
        rbsp.extend(payload);
    }
    rbsp.push(0x80);
    let mut nal = vec![NAL_SEI];
    nal.extend(escape(&rbsp));
    nal
}

/// `nal` without its buffering-period and picture-timing messages (`None` = nothing left).
pub fn strip_timing_sei(nal: &[u8]) -> Option<Vec<u8>> {
    let kept: Vec<(u32, Vec<u8>)> =
        sei_messages(nal).into_iter().filter(|(t, _)| *t != SEI_BUFFERING_PERIOD && *t != SEI_PIC_TIMING).collect();
    (!kept.is_empty()).then(|| {
        let mut out = sei_nal(&kept);
        out[0] = nal[0];
        out
    })
}

/// SEI NAL unit with an optional buffering period (`(sps_id, initial_cpb_removal_delay)` in
/// 90 kHz units) and a picture timing (`cpb_removal_delay`, `dpb_output_delay` in ticks).
pub fn timing_sei(buffering: Option<(u32, u32)>, removal: u32, output: u32) -> Vec<u8> {
    let mut messages = Vec::new();
    if let Some((sps_id, initial)) = buffering {
        let mut w = BitWriter::default();
        w.ue(sps_id);
        w.u(initial, DELAY_BITS);
        w.u(0, DELAY_BITS); // initial_cpb_removal_delay_offset
        messages.push((SEI_BUFFERING_PERIOD, w.sei_payload()));
    }
    let mut w = BitWriter::default();
    w.u(removal, DELAY_BITS);
    w.u(output, DELAY_BITS);
    messages.push((SEI_PIC_TIMING, w.sei_payload()));
    sei_nal(&messages)
}

/// The coded picture buffer as a leaky bucket (C.1, VBR): it fills at `rate` up to `size` and
/// starts full; every picture is removed whole at its decode time.
#[derive(Debug, Clone)]
pub struct Vbv {
    rate: f64,
    size: f64,
    fill: f64,
}

impl Vbv {
    pub fn new(rate: u64, size: u64) -> Self {
        Vbv { rate: rate as f64, size: size as f64, fill: size as f64 }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Bits waiting in the buffer now.
    pub fn fill(&self) -> f64 {
        self.fill
    }

    /// Removes a picture of `bits`, then refills for `secs` (one frame). Returns the fullness
    /// before the removal, or `Err(missing bits)` when the picture is not in the buffer yet.
    pub fn picture(&mut self, bits: f64, secs: f64) -> Result<f64, f64> {
        let before = self.fill;
        if bits > before {
            return Err(bits - before);
        }
        self.fill = (before - bits + self.rate * secs).min(self.size);
        Ok(before)
    }

    /// `initial_cpb_removal_delay` (90 kHz) of a picture removed with `fullness` bits waiting.
    pub fn initial_delay(&self, fullness: f64) -> u32 {
        (fullness / self.rate * 90_000.0).floor() as u32
    }
}

/// NAL units of an Annex B access unit: `(start including start code, end, type)`.
pub fn nal_units(au: &[u8]) -> Vec<(usize, usize, u8)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let begin = if i > 0 && au[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(begin, body)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(au.len(), |&(b, _)| b);
        if body < end {
            out.push((begin, end, au[body] & 31));
        }
    }
    out
}

/// The NAL unit body (header byte onwards) of a unit from [`nal_units`].
pub fn nal_body(au: &[u8], (begin, end, _): (usize, usize, u8)) -> &[u8] {
    let s = &au[begin..end];
    let skip = if s.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
    // Trailing zero bytes belong to the next start code (`trailing_zero_8bits`).
    let mut e = s.len();
    while e > skip + 1 && s[e - 1] == 0 {
        e -= 1;
    }
    &s[skip..e]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// x264's 1080p High@4.1 SPS without VUI timing/HRD (from `-preset veryfast`), and the same
    /// stream's SPS with `--nal-hrd vbr --vbv-maxrate 40000 --vbv-bufsize 30000 --fps 24000/1001`.
    fn sps_without_hrd() -> Vec<u8> {
        // profile 100, level 41, 1920x1088 cropped to 1080, VUI with colour description only.
        let mut w = BitWriter::default();
        w.u(100, 8);
        w.u(0, 8);
        w.u(41, 8);
        w.ue(0); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(0);
        w.ue(0);
        w.bit(false);
        w.bit(false); // no scaling matrix
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // poc type 0
        w.ue(2);
        w.ue(4); // refs
        w.bit(false);
        w.ue(119); // 1920 / 16 - 1
        w.ue(67); // 1088 / 16 - 1
        w.bit(true); // frame_mbs_only
        w.bit(true); // direct_8x8
        w.bit(true); // cropping
        for c in [0, 0, 0, 4] {
            w.ue(c);
        }
        w.bit(true); // VUI
        let vui = Vui { signal: Some((5, false, Some((1, 1, 1)))), restriction: Some((true, [0, 0, 11, 11, 2, 4])), ..Vui::default() };
        vui.write(&mut w);
        let mut nal = vec![0x67];
        nal.extend(escape(&w.finish()));
        nal
    }

    #[test]
    fn escape_round_trip() {
        let raw = [0u8, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 5];
        let esc = escape(&raw);
        assert!(!esc.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2), "{esc:?}");
        assert_eq!(unescape(&esc), raw);
    }

    #[test]
    fn exp_golomb_round_trip() {
        let mut w = BitWriter::default();
        for v in [0, 1, 2, 7, 255, 625_000] {
            w.ue(v);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for v in [0, 1, 2, 7, 255, 625_000] {
            assert_eq!(r.ue(), Some(v));
        }
    }

    #[test]
    fn sps_rewrite_adds_timing_and_hrd_and_keeps_the_rest() {
        let nal = sps_without_hrd();
        let sps = Sps::parse(&nal).expect("parses");
        assert!(sps.frame_mbs_only && sps.nal_hrd().is_none());
        let hrd = Hrd::for_disc(40_000_000, 30_000_000);
        let out = Sps::parse(&sps.with_timing((24000, 1001), &hrd)).expect("rewritten SPS parses");
        assert_eq!(out.nal_hrd(), Some(&hrd));
        let vui = out.vui.as_ref().unwrap();
        assert_eq!(vui.timing, Some((1001, 48000, true)));
        let old = sps.vui.as_ref().unwrap();
        assert_eq!((&vui.signal, &vui.restriction), (&old.signal, &old.restriction));
        assert_eq!((out.id, out.vui_at), (sps.id, sps.vui_at), "fields before the VUI are untouched");
        assert_eq!(out.rbsp[..sps.vui_at / 8], sps.rbsp[..sps.vui_at / 8]);
        // Rewriting a rewritten SPS changes nothing.
        assert_eq!(out.with_timing((24000, 1001), &hrd), sps.with_timing((24000, 1001), &hrd));
    }

    #[test]
    fn timing_sei_messages_parse_back() {
        let sei = timing_sei(Some((0, 67_500)), 48, 4);
        assert_eq!(sei_types(&sei), vec![SEI_BUFFERING_PERIOD, SEI_PIC_TIMING]);
        let msgs = sei_messages(&sei);
        let mut r = BitReader::new(&msgs[0].1);
        assert_eq!((r.ue(), r.u(24), r.u(24)), (Some(0), Some(67_500), Some(0)));
        let mut r = BitReader::new(&msgs[1].1);
        assert_eq!((r.u(24), r.u(24)), (Some(48), Some(4)));
        assert!(timing_sei(None, 2, 0).len() < sei.len());
    }

    #[test]
    fn stripping_keeps_other_messages() {
        let mixed = sei_nal(&[(SEI_BUFFERING_PERIOD, vec![0x80]), (5, b"x264 banner".to_vec()), (SEI_PIC_TIMING, vec![1, 2])]);
        let kept = strip_timing_sei(&mixed).expect("user data stays");
        assert_eq!(sei_types(&kept), vec![5]);
        assert_eq!(strip_timing_sei(&timing_sei(Some((0, 1)), 0, 0)), None);
    }

    #[test]
    fn buffer_model() {
        // 8 Mbit/s into 4 Mbit: 25 fps pictures of 320 kbit just fit, 3 Mbit ones drain it.
        let mut vbv = Vbv::new(8_000_000, 4_000_000);
        for _ in 0..100 {
            assert!(vbv.picture(320_000.0, 0.04).is_ok());
        }
        assert_eq!(vbv.initial_delay(4_000_000.0), 45_000);
        let mut vbv = Vbv::new(8_000_000, 4_000_000);
        assert!(vbv.picture(3_000_000.0, 0.04).is_ok());
        assert!(vbv.picture(3_000_000.0, 0.04).is_err(), "the second big picture is not in yet");
    }

    #[test]
    fn nal_unit_split() {
        let au = [0, 0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x65, 9, 0];
        let units = nal_units(&au);
        assert_eq!(units.iter().map(|u| u.2).collect::<Vec<_>>(), vec![NAL_AUD, NAL_SPS, NAL_IDR]);
        assert_eq!(nal_body(&au, units[1]), &[0x67, 1, 2]);
        assert_eq!(nal_body(&au, units[2]), &[0x65, 9]);
    }
}
