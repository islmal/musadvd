//! Graph telemetry of a read / write / verify: the [`Sampler`] (speed from LBA deltas, buffer
//! fill levels, CPU load), the file-name template for saved graphs, and the `.ibg` text format
//! (plain text, as written by ImgBurn's graph files) with writer and tolerant parser.

use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------------
// Sampling.

/// One graph point. `cpu` is −1 when not captured, `device` 0 when the device buffer is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Speed in x of the medium.
    pub speed: f64,
    pub lba: u64,
    /// Main (ring) buffer fill, percent.
    pub main: f64,
    /// Device buffer fill, percent.
    pub device: f64,
    pub cpu: f64,
}

/// CPU load from system-time deltas (`GetSystemTimes`); `None` where unavailable.
pub struct CpuMeter {
    last: Option<(u64, u64)>,
}

impl CpuMeter {
    pub fn new() -> Self {
        CpuMeter { last: system_times() }
    }

    /// Load since the previous call in percent, −1 when it cannot be told.
    pub fn percent(&mut self) -> f64 {
        let now = system_times();
        let load = match (self.last, now) {
            (Some((i0, t0)), Some((i1, t1))) if t1 > t0 => 100.0 * (1.0 - (i1 - i0) as f64 / (t1 - t0) as f64),
            _ => -1.0,
        };
        self.last = now;
        load.clamp(-1.0, 100.0)
    }
}

impl Default for CpuMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// (idle, kernel + user) in 100 ns units; kernel time includes idle time.
#[cfg(windows)]
fn system_times() -> Option<(u64, u64)> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    let v = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    Some((v(idle), v(kernel) + v(user)))
}

#[cfg(not(windows))]
fn system_times() -> Option<(u64, u64)> {
    None
}

/// Collects a sample every `interval` seconds from the progress it is fed.
pub struct Sampler {
    interval: f64,
    one_x: f64,
    started: Instant,
    last_t: f64,
    last_lba: u64,
    cpu: Option<CpuMeter>,
    samples: Vec<Sample>,
}

impl Sampler {
    /// `one_x`: sectors per second at 1x of the medium; `cpu`: also capture the CPU load.
    pub fn new(interval_ms: u32, one_x: f64, cpu: bool) -> Self {
        Sampler {
            interval: interval_ms.max(1) as f64 / 1000.0,
            one_x,
            started: Instant::now(),
            last_t: 0.0,
            last_lba: 0,
            cpu: cpu.then(CpuMeter::new),
            samples: Vec::new(),
        }
    }

    /// Feeds the position now; returns the new sample when the interval has passed.
    pub fn record(&mut self, lba: u64, main: u32, device: u32) -> Option<Sample> {
        let t = self.started.elapsed().as_secs_f64();
        let cpu = if t - self.last_t >= self.interval { self.cpu.as_mut().map_or(-1.0, CpuMeter::percent) } else { -1.0 };
        self.record_at(t, lba, main, device, cpu)
    }

    /// Like [`record`](Self::record) with an explicit clock (seconds since the start) and CPU load.
    pub fn record_at(&mut self, t: f64, lba: u64, main: u32, device: u32, cpu: f64) -> Option<Sample> {
        let dt = t - self.last_t;
        if dt < self.interval {
            return None;
        }
        let speed = if self.one_x > 0.0 { lba.saturating_sub(self.last_lba) as f64 / dt / self.one_x } else { 0.0 };
        let s = Sample { speed, lba, main: main as f64, device: device as f64, cpu };
        (self.last_t, self.last_lba) = (t, lba);
        self.samples.push(s);
        Some(s)
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    pub fn interval_ms(&self) -> u32 {
        (self.interval * 1000.0).round() as u32
    }

    /// Seconds since the sampler was created.
    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// `WRITE_SPEED_*` / `READ_SPEED_*` summary keys (without the prefix) from the samples.
pub fn speed_summary(samples: &[Sample], seconds: u64) -> Vec<(&'static str, String)> {
    let moving: Vec<f64> = samples.iter().map(|s| s.speed).filter(|&s| s > 0.0).collect();
    let f = |v: f64| format!("{v:.2}");
    let avg = if moving.is_empty() { 0.0 } else { moving.iter().sum::<f64>() / moving.len() as f64 };
    vec![
        ("SPEED_START", f(moving.first().copied().unwrap_or(0.0))),
        ("SPEED_END", f(moving.last().copied().unwrap_or(0.0))),
        ("SPEED_AVERAGE", f(avg)),
        ("SPEED_MAX", f(moving.iter().copied().fold(0.0, f64::max))),
        ("TIME_TAKEN", seconds.to_string()),
    ]
}

// ---------------------------------------------------------------------------------------------
// Dates and the file-name template.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

const WEEKDAYS: [&str; 7] = ["MONTAG", "DIENSTAG", "MITTWOCH", "DONNERSTAG", "FREITAG", "SAMSTAG", "SONNTAG"];
const MONTHS: [&str; 12] =
    ["JANUAR", "FEBRUAR", "MÄRZ", "APRIL", "MAI", "JUNI", "JULI", "AUGUST", "SEPTEMBER", "OKTOBER", "NOVEMBER", "DEZEMBER"];

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400) as i32 + (m <= 2) as i32;
    (y, m, d)
}

impl DateTime {
    /// 0 = Monday.
    pub fn weekday(&self) -> usize {
        (days_from_civil(self.year, self.month, self.day) + 3).rem_euclid(7) as usize
    }

    pub fn from_unix(secs: i64) -> DateTime {
        let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
        let rem = secs.rem_euclid(86_400) as u32;
        DateTime { year, month, day, hour: rem / 3600, minute: rem / 60 % 60, second: rem % 60 }
    }

    /// Local time on Windows, UTC elsewhere.
    pub fn now() -> DateTime {
        #[cfg(windows)]
        {
            let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
            DateTime {
                year: t.wYear as i32,
                month: t.wMonth as u32,
                day: t.wDay as u32,
                hour: t.wHour as u32,
                minute: t.wMinute as u32,
                second: t.wSecond as u32,
            }
        }
        #[cfg(not(windows))]
        {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            DateTime::from_unix(secs)
        }
    }

    /// `13.10.2026 15:42:07` (the `.ibg` DATE line).
    pub fn ibg(&self) -> String {
        format!("{:02}.{:02}.{} {:02}:{:02}:{:02}", self.day, self.month, self.year, self.hour, self.minute, self.second)
    }
}

/// Values the file-name template can use.
#[derive(Debug, Clone)]
pub struct TemplateContext {
    pub make_model: String,
    pub firmware: String,
    pub when: DateTime,
    pub media_id: String,
    /// `MAX` or `8x`.
    pub speed: String,
    pub volume: String,
}

/// A file-name part: whitespace runs → `_`, characters Windows forbids in names → `_`.
fn name_part(s: &str) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join("_");
    joined.chars().map(|c| if r#"\/:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect()
}

/// Expands the template: `%M` make and model, `%F` firmware, `%D` weekday-day-month-year
/// (German, upper case), `%d` dd-mm-yyyy, `%B` yyyy-mm-dd, `%T` HH-MM, `%I` media ID, `%S`
/// speed, `%V` volume label, `%%` a percent sign. Unknown codes stay as written. The result has
/// no extension and no forbidden characters.
pub fn format_template(template: &str, c: &TemplateContext) -> String {
    let w = c.when;
    let mut out = String::new();
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('M') => out += &name_part(&c.make_model),
            Some('F') => out += &name_part(&c.firmware),
            Some('D') => out += &format!("{}-{:02}-{}-{}", WEEKDAYS[w.weekday()], w.day, MONTHS[(w.month as usize).clamp(1, 12) - 1], w.year),
            Some('d') => out += &format!("{:02}-{:02}-{}", w.day, w.month, w.year),
            Some('B') => out += &format!("{}-{:02}-{:02}", w.year, w.month, w.day),
            Some('T') => out += &format!("{:02}-{:02}", w.hour, w.minute),
            Some('I') => out += &name_part(&c.media_id),
            Some('S') => out += &name_part(&c.speed),
            Some('V') => out += &name_part(&c.volume),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    name_part(&out)
}

/// `<template>.ibg`.
pub fn file_name(template: &str, c: &TemplateContext) -> String {
    format!("{}.ibg", format_template(template, c))
}

/// The configured graph folder, or `%APPDATA%\musadvd\graph` when empty.
pub fn folder(configured: &str) -> PathBuf {
    if !configured.trim().is_empty() {
        return PathBuf::from(configured.trim());
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("musadvd").join("graph")
}

// ---------------------------------------------------------------------------------------------
// The .ibg file.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphKind {
    Write,
    Read,
    Verify,
}

impl GraphKind {
    fn tag(self) -> &'static str {
        match self {
            GraphKind::Write => "WRITE",
            GraphKind::Read => "READ",
            GraphKind::Verify => "VERIFY",
        }
    }
}

/// A graph file: ordered `KEY=value` configuration plus the samples.
#[derive(Debug, Clone, PartialEq)]
pub struct IbgFile {
    pub kind: GraphKind,
    /// Without `IBGD_VERSION` (always 2) and `SAMPLE_RATE` (`sample_ms`), in file order.
    pub config: Vec<(String, String)>,
    pub sample_ms: u32,
    pub samples: Vec<Sample>,
}

impl IbgFile {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.config.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// The `.ibg` text (CRLF). shortcut: ImgBurn's trailing checksum line is omitted (algorithm
/// unknown); add it if a public description appears. Read / verify sections use `READ` /
/// `VERIFY` by analogy with the write sample.
pub fn write_ibg(f: &IbgFile) -> String {
    let mut lines: Vec<String> = vec!["IBGD".into(), String::new(), "[START_CONFIGURATION]".into(), "IBGD_VERSION=2".into()];
    // A blank line whenever the key group (text before the first `_`) changes.
    let mut group = "IBGD".to_string();
    let mut add = |lines: &mut Vec<String>, key: &str, value: &str| {
        let g = key.split('_').next().unwrap_or(key);
        if g != group {
            lines.push(String::new());
            group = g.to_string();
        }
        lines.push(format!("{key}={value}"));
    };
    // DATE first, then SAMPLE_RATE, then the other groups in order.
    if let Some(date) = f.get("DATE") {
        add(&mut lines, "DATE", date);
    }
    add(&mut lines, "SAMPLE_RATE", &f.sample_ms.to_string());
    for (k, v) in f.config.iter().filter(|(k, _)| k != "DATE") {
        add(&mut lines, k, v);
    }
    let tag = f.kind.tag();
    lines.extend(["[END_CONFIGURATION]".into(), String::new(), "HRPC=True".into(), String::new(), format!("[START_{tag}_GRAPH_VALUES]")]);
    for s in &f.samples {
        lines.push(format!("{:.2},{},{:.2},{:.2},{:.2}", s.speed, s.lba, s.main, s.device, s.cpu));
    }
    lines.push(format!("[END_{tag}_GRAPH_VALUES]"));
    lines.join("\r\n") + "\r\n"
}

/// Parses a graph file; tolerant about line endings, blank lines and a trailing checksum.
pub fn parse_ibg(text: &str) -> Result<IbgFile, String> {
    let mut lines = text.lines().map(str::trim);
    if lines.next() != Some("IBGD") {
        return Err("keine Graph-Datei (IBGD fehlt)".into());
    }
    let (mut config, mut sample_ms, mut kind, mut samples) = (Vec::new(), 100u32, None, Vec::new());
    let mut section = "";
    for line in lines.filter(|l| !l.is_empty()) {
        if let Some(name) = line.strip_prefix("[START_").and_then(|l| l.strip_suffix(']')) {
            section = if name == "CONFIGURATION" {
                "config"
            } else {
                kind = Some(match name.strip_suffix("_GRAPH_VALUES") {
                    Some("WRITE") => GraphKind::Write,
                    Some("READ") => GraphKind::Read,
                    Some("VERIFY") => GraphKind::Verify,
                    _ => return Err(format!("unbekannter Abschnitt: {name}")),
                });
                "values"
            };
        } else if line.starts_with("[END_") {
            section = "";
        } else if section == "config" {
            let (k, v) = line.split_once('=').ok_or_else(|| format!("ungültige Zeile: {line}"))?;
            match k {
                "IBGD_VERSION" => {}
                "SAMPLE_RATE" => sample_ms = v.trim().parse().map_err(|_| format!("ungültige Abtastrate: {v}"))?,
                _ => config.push((k.to_string(), v.to_string())),
            }
        } else if section == "values" {
            let f: Vec<&str> = line.split(',').collect();
            let num = |i: usize| f.get(i).and_then(|s| s.trim().parse::<f64>().ok());
            let (Some(speed), Some(lba), Some(main)) = (num(0), num(1), num(2)) else {
                return Err(format!("ungültiger Messwert: {line}"));
            };
            samples.push(Sample { speed, lba: lba as u64, main, device: num(3).unwrap_or(0.0), cpu: num(4).unwrap_or(-1.0) });
        }
        // anything else (HRPC=…, the checksum line) is ignored
    }
    Ok(IbgFile { kind: kind.ok_or("keine Messwerte in der Datei")?, config, sample_ms, samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuesday() -> DateTime {
        DateTime { year: 2026, month: 10, day: 13, hour: 15, minute: 42, second: 7 }
    }

    fn ctx() -> TemplateContext {
        TemplateContext {
            make_model: "PIONEER BD-RW   BDR-209M".into(),
            firmware: "1.33".into(),
            when: tuesday(),
            media_id: "CMC MAG-M01-00".into(),
            speed: "MAX".into(),
            volume: "MEIN FILM".into(),
        }
    }

    #[test]
    fn template_matches_the_known_file_name() {
        let name = file_name("%M_%F_%D_%T_%I_%S", &ctx());
        assert_eq!(name, "PIONEER_BD-RW_BDR-209M_1.33_DIENSTAG-13-OKTOBER-2026_15-42_CMC_MAG-M01-00_MAX.ibg");
    }

    #[test]
    fn template_other_codes_and_sanitising() {
        let c = ctx();
        assert_eq!(format_template("%d|%B|%V|%%|%x|%", &c), "13-10-2026_2026-10-13_MEIN_FILM_%_%x_%");
        let mut c2 = c.clone();
        c2.media_id = "A/B:C*D".into();
        c2.speed = "8x".into();
        assert_eq!(format_template("%I %S", &c2), "A_B_C_D_8x");
        assert_eq!(format_template("", &c), "");
    }

    #[test]
    fn dates() {
        assert_eq!(tuesday().weekday(), 1);
        assert_eq!(tuesday().ibg(), "13.10.2026 15:42:07");
        let d = DateTime::from_unix(0);
        assert_eq!((d.year, d.month, d.day, d.weekday()), (1970, 1, 1, 3), "Thursday");
        let d = DateTime::from_unix(1_781_000_000);
        assert_eq!(days_from_civil(d.year, d.month, d.day) * 86_400 + (d.hour * 3600 + d.minute * 60 + d.second) as i64, 1_781_000_000);
        assert_eq!(DateTime::from_unix(951_782_400).day, 29, "2000-02-29 exists");
        assert_eq!(MONTHS[2], "MÄRZ");
        let now = DateTime::now();
        assert!((2024..2200).contains(&now.year) && (1..=12).contains(&now.month));
    }

    #[test]
    fn sampler_speed_from_lba_deltas() {
        let one_x = 676.0; // DVD
        let mut s = Sampler::new(100, one_x, false);
        assert!(s.record_at(0.05, 100, 90, 0, -1.0).is_none(), "interval not over");
        let a = s.record_at(0.1, 676, 90, 40, 12.5).unwrap();
        assert!((a.speed - 10.0).abs() < 1e-9, "676 sectors in 0.1 s at 676/s per x = 10x: {}", a.speed);
        assert_eq!((a.main, a.device, a.cpu), (90.0, 40.0, 12.5));
    }

    #[test]
    fn sampler_follows_progress() {
        let mut s = Sampler::new(100, 100.0, false);
        // 800 sectors in 0.2 s at 100 sectors/s per x → 40x
        let a = s.record_at(0.2, 800, 80, 0, -1.0).unwrap();
        assert!((a.speed - 40.0).abs() < 1e-9);
        let b = s.record_at(0.35, 800, 80, 0, -1.0).unwrap();
        assert_eq!(b.speed, 0.0, "stalled");
        assert_eq!(s.samples().len(), 2);
        assert_eq!(s.interval_ms(), 100);
        // A position that went backwards (restart) does not underflow.
        assert_eq!(s.record_at(0.5, 10, 0, 0, -1.0).unwrap().speed, 0.0);
    }

    #[test]
    fn summary() {
        let mk = |speed| Sample { speed, lba: 0, main: 0.0, device: 0.0, cpu: -1.0 };
        let s = speed_summary(&[mk(0.0), mk(7.1), mk(10.0), mk(15.47)], 361);
        let get = |k: &str| s.iter().find(|(n, _)| *n == k).unwrap().1.clone();
        assert_eq!((get("SPEED_START").as_str(), get("SPEED_END").as_str(), get("SPEED_MAX").as_str()), ("7.10", "15.47", "15.47"));
        assert_eq!(get("SPEED_AVERAGE"), "10.86");
        assert_eq!(get("TIME_TAKEN"), "361");
        assert_eq!(speed_summary(&[], 0)[0].1, "0.00");
    }

    fn file() -> IbgFile {
        let kv = |k: &str, v: &str| (k.to_string(), v.to_string());
        IbgFile {
            kind: GraphKind::Write,
            config: vec![
                kv("DATE", "13.10.2026 15:42:07"),
                kv("DEVICE", "[0:0:0] PIONEER BD-RW   BDR-209M 1.33 (F:) (USB)"),
                kv("DEVICE_FIRMWAREVERSION", "1.33"),
                kv("MEDIA_TYPE", "DVD+R"),
                kv("MEDIA_SPEEDS", "2,4x; 4x; 8x"),
                kv("DATA_IMAGEFILE", "-==/\\/[BUILD IMAGE]\\/\\==-"),
                kv("WRITE_TEST_MODE", "False"),
                kv("WRITE_SPEED_MAX", "15.47"),
            ],
            sample_ms: 100,
            samples: vec![
                Sample { speed: 7.1, lba: 32, main: 100.0, device: 55.25, cpu: 3.5 },
                Sample { speed: 15.47, lba: 1_048_576, main: 97.0, device: 0.0, cpu: -1.0 },
            ],
        }
    }

    #[test]
    fn ibg_text_layout() {
        let t = write_ibg(&file());
        let expect = "IBGD\r\n\r\n[START_CONFIGURATION]\r\nIBGD_VERSION=2\r\n\r\nDATE=13.10.2026 15:42:07\r\n\r\nSAMPLE_RATE=100\r\n\r\n\
DEVICE=[0:0:0] PIONEER BD-RW   BDR-209M 1.33 (F:) (USB)\r\nDEVICE_FIRMWAREVERSION=1.33\r\n\r\nMEDIA_TYPE=DVD+R\r\nMEDIA_SPEEDS=2,4x; 4x; 8x\r\n\r\n\
DATA_IMAGEFILE=-==/\\/[BUILD IMAGE]\\/\\==-\r\n\r\nWRITE_TEST_MODE=False\r\nWRITE_SPEED_MAX=15.47\r\n[END_CONFIGURATION]\r\n\r\nHRPC=True\r\n\r\n\
[START_WRITE_GRAPH_VALUES]\r\n7.10,32,100.00,55.25,3.50\r\n15.47,1048576,97.00,0.00,-1.00\r\n[END_WRITE_GRAPH_VALUES]\r\n";
        assert_eq!(t, expect);
    }

    #[test]
    fn ibg_round_trip_and_tolerance() {
        let f = file();
        let back = parse_ibg(&write_ibg(&f)).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.get("MEDIA_TYPE"), Some("DVD+R"));
        // LF only, blank lines anywhere, a trailing checksum line, a read graph.
        let t = "IBGD\n\n[START_CONFIGURATION]\nIBGD_VERSION=2\nSAMPLE_RATE=250\nDEVICE=x\n[END_CONFIGURATION]\n\nHRPC=True\n\n[START_READ_GRAPH_VALUES]\n4.00,10,50.00\n[END_READ_GRAPH_VALUES]\n78780F87D24BB4B4\n";
        let r = parse_ibg(t).unwrap();
        assert_eq!((r.kind, r.sample_ms, r.samples.len()), (GraphKind::Read, 250, 1));
        assert_eq!((r.samples[0].device, r.samples[0].cpu), (0.0, -1.0), "missing columns default");
        assert_eq!(r.get("DEVICE"), Some("x"));
        assert!(parse_ibg("nope").is_err());
        assert!(parse_ibg("IBGD\n[START_CONFIGURATION]\nIBGD_VERSION=2\n[END_CONFIGURATION]\n").unwrap_err().contains("Messwerte"));
        assert!(parse_ibg("IBGD\n[START_WRITE_GRAPH_VALUES]\nabc\n[END_WRITE_GRAPH_VALUES]\n").unwrap_err().contains("Messwert"));
    }

    #[test]
    fn default_folder_is_under_appdata() {
        assert!(folder("").ends_with("musadvd/graph") || folder("").ends_with(r"musadvd\graph"));
        assert_eq!(folder(" D:\\g "), PathBuf::from("D:\\g"));
    }
}
