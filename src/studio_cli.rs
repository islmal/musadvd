//! CLI subcommands of Burn Studio: `read-disc`, `verify-disc`, `build-iso`, `iso-label`,
//! `ifo-layerbreak`, `drive-info`. Thin wrappers over `burner::studio`; Ctrl+C cancels.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Args;
use crate::burner::builder::{self, BuildError};
use crate::burner::layerbreak::Capacities;
use crate::burner::studio::{self, Ctx, Event, Outcome};
use crate::burner::tools::{self, CandidateOptions};
use crate::config::AppSettings;

#[derive(Args)]
pub struct ReadDisc {
    /// Image file to write (`name_part1.iso`, … when the settings split it).
    pub output: PathBuf,
    /// Drive: id, drive letter ("F:") or part of the name. Default: the only drive.
    #[arg(long)]
    pub drive: Option<String>,
}

#[derive(Args)]
pub struct VerifyDisc {
    /// Image (`.iso`, `.cue`, `.dvd`, `.mds`) to compare the disc with; without it the disc is
    /// only read to check that every sector is readable.
    pub image: Option<PathBuf>,
    #[arg(long)]
    pub drive: Option<String>,
}

#[derive(Args)]
pub struct BuildIso {
    /// Files and folders to put on the image (a single DVD-Video / Blu-ray folder becomes a disc).
    #[arg(required = true)]
    pub sources: Vec<PathBuf>,
    /// Output `.iso` file.
    #[arg(long)]
    pub output: PathBuf,
    /// Volume label (default: Blu-ray disc name or the folder's own default).
    #[arg(long, default_value = "")]
    pub label: String,
    /// Treat a DVD-Video / Blu-ray folder as a disc without asking (default: the setting `prompts`).
    #[arg(long)]
    pub as_disc: bool,
    /// Treat it as plain data.
    #[arg(long, conflicts_with = "as_disc")]
    pub as_data: bool,
}

#[derive(Args)]
pub struct IsoLabel {
    pub iso: PathBuf,
    /// Write this volume label (ISO 9660, Joliet and UDF fields) instead of printing it.
    #[arg(long)]
    pub set: Option<String>,
}

#[derive(Args)]
pub struct IfoLayerbreak {
    /// A DVD-Video `.iso` or a folder containing `VIDEO_TS`.
    pub source: PathBuf,
    /// List cells that are seamlessly linked / interleaved as well (default: the settings).
    #[arg(long)]
    pub all: bool,
}

#[derive(Args)]
pub struct DriveInfoArgs {
    #[arg(long)]
    pub drive: Option<String>,
}

fn settings() -> crate::config::BurnStudioSettings {
    AppSettings::load_or_default(&AppSettings::default_path()).0.burn_studio.sanitized()
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::FAILURE
}

/// A context that prints progress lines on stderr; Ctrl+C sets the cancel flag.
fn ctx() -> Ctx {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
                cancel.store(true, Ordering::SeqCst);
                eprintln!("\nCancel requested …");
            }
        });
    }
    let last = std::sync::Mutex::new((String::new(), -1i32));
    let emit: studio::Emit = Arc::new(move |e| match e {
        Event::Log(m) => eprintln!("\n{m}"),
        Event::Graph(_) => {}
        Event::Progress(p) => {
            let pct = (p.fraction * 100.0) as i32;
            let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
            if l.0 == p.phase && l.1 == pct {
                return;
            }
            *l = (p.phase.clone(), pct);
            let left = p.eta_secs.map_or(String::new(), |s| format!(" – {} left", crate::eta::format_remaining(s)));
            eprint!("\r  {}: {pct:>3}% ({:.1}x){left}    ", p.phase, p.speed_x);
        }
    });
    Ctx { settings: settings(), cancel, emit }
}

/// Prints the outcome and maps it to an exit code (130 = cancelled).
fn finish(r: Result<Outcome, String>) -> ExitCode {
    eprintln!();
    match r {
        Ok(o) if o.cancelled => {
            eprintln!("Cancelled.");
            ExitCode::from(130)
        }
        Ok(o) => {
            println!("{}", o.message);
            o.files.iter().for_each(|f| println!("  File: {f}"));
            o.layout_file.iter().for_each(|f| println!("  Layout: {f}"));
            o.md5.iter().for_each(|m| println!("  MD5: {m}"));
            o.graph.iter().for_each(|g| println!("  Graph: {g}"));
            o.details.iter().for_each(|d| println!("  {d}"));
            if o.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => fail(e),
    }
}

pub fn read_disc(a: ReadDisc) -> ExitCode {
    let drive = match studio::find_drive(a.drive.as_deref()) {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    finish(studio::read(&ctx(), &drive, &a.output))
}

pub fn verify_disc(a: VerifyDisc) -> ExitCode {
    if let Some(image) = a.image.as_ref().filter(|p| !p.is_file()) {
        return fail(format!("{}: image not found", image.display()));
    }
    let drive = match studio::find_drive(a.drive.as_deref()) {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    finish(studio::verify(&ctx(), &drive, a.image.as_deref()))
}

pub fn build_iso(a: BuildIso) -> ExitCode {
    if let Some(p) = a.sources.iter().find(|p| !p.exists()) {
        return fail(format!("{}: not found", p.display()));
    }
    let c = ctx();
    let mut cfg = c.settings.build.clone();
    if a.as_disc || a.as_data {
        cfg.prompts = if a.as_disc { "always" } else { "never" }.into();
    }
    let input = studio::build_input(&a.sources, &a.label);
    let plan = match builder::plan_build(&input, &cfg) {
        Ok(p) => p,
        Err(BuildError::NeedsAnswer(content)) => {
            return fail(format!("The folder looks like {} – build it with --as-disc as a disc or with --as-data as data.", content.name()));
        }
        Err(e) => return fail(e),
    };
    println!("{} · label {} · {} sectors ({:.2} GB)", plan.content.name(), plan.label, plan.total_sectors(), plan.total_sectors() as f64 * 2048.0 / 1e9);
    if let Some(lb) = plan.layer_break {
        println!("  Layer break at sector {}", lb.l0_sectors);
    }
    plan.warnings.iter().for_each(|w| println!("  Note: {w}"));
    finish(studio::build(&Ctx { settings: crate::config::BurnStudioSettings { build: cfg, ..c.settings.clone() }, ..c }, &plan, &a.output))
}

pub fn iso_label(a: IsoLabel) -> ExitCode {
    match a.set {
        None => match tools::read_label(&a.iso) {
            Ok(l) => {
                println!("{l}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Some(new) => match tools::patch_label(&a.iso, &new) {
            Ok((old, now)) => {
                println!("{old} → {now}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
    }
}

pub fn ifo_layerbreak(a: IfoLayerbreak) -> ExitCode {
    let g = settings().general;
    let opts = CandidateOptions { prefer_non_seamless: g.lb_check_seamless, show_interleaved: g.lb_show_interleaved || a.all };
    let found = if a.source.is_dir() {
        let cfg = crate::config::StudioBuild { prompts: "always".into(), ..Default::default() };
        let plan = match builder::plan_build(&studio::build_input(std::slice::from_ref(&a.source), ""), &cfg) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        match plan.image {
            builder::BuiltImage::Udf(p) => tools::ifo_candidates(&p, Capacities::DVD, opts).map_err(|e| e.to_string()),
            builder::BuiltImage::Bd(_) => Err("Blu-ray images have no layer break (the IFO inspector is for DVD-Video only)".into()),
        }
    } else if a.source.is_file() {
        tools::ifo_candidates_iso(&a.source, Capacities::DVD, opts).map_err(|e| e.to_string())
    } else {
        Err(format!("{}: not found", a.source.display()))
    };
    match found {
        Err(e) => fail(e),
        Ok(c) if c.is_empty() => {
            println!("No possible layer breaks found (fits on a single-layer disc, or no cells).");
            ExitCode::SUCCESS
        }
        Ok(list) => {
            println!(" VTS  TTN  PGC Cell      Start         L0         L1   L0 % seamless  Padding sectors");
            for c in list {
                println!(
                    "{:>4} {:>4} {:>4} {:>4} {:>10} {:>10} {:>10} {:>5.1}% {:<9} {}{}",
                    c.vts, c.ttn, c.pgc, c.cell, c.start_lba, c.l0_sectors, c.l1_sectors, c.split_percent,
                    if c.seamless { "ja" } else { "nein" }, c.padding_sectors, if c.interleaved { "  (verschachtelt)" } else { "" }
                );
            }
            ExitCode::SUCCESS
        }
    }
}

pub fn drive_info(a: DriveInfoArgs) -> ExitCode {
    let drive = match studio::find_drive(a.drive.as_deref()) {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    let s = settings();
    println!("{}", drive.name);
    match studio::media_of(&drive) {
        Ok(m) if !m.present => println!("  no disc inserted"),
        Ok(m) => println!("  {} ({}), {}, frei {:.2} GB", m.physical, if m.writable { "writable" } else { "not writable" }, if m.blank { "blank" } else { "written" }, m.free_sectors as f64 * 2048.0 / 1e9),
        Err(e) => println!("  Disc unreadable: {e}"),
    }
    match studio::drive_details(&drive, &s.io) {
        Ok(d) => {
            println!("  {} {} {}", d.vendor, d.model, d.firmware);
            println!("  Profil: {}{}", d.current_profile_name, if d.ready { "" } else { " (not ready)" });
            let profiles: Vec<_> = d.profiles.iter().map(|p| p.name).collect();
            println!("  Supported: {}", profiles.join(", "));
            for (label, v) in [("Book-Typ", &d.book_type), ("Media-ID", &d.media_id), ("Status", &d.disc_status)] {
                if let Some(v) = v {
                    println!("  {label}: {v}");
                }
            }
            if let Some(l) = d.layer_break {
                println!("  Layer break: sector {l}");
            }
            if let Some(c) = d.capacity_sectors {
                println!("  Capacity: {c} sectors");
            }
            if !d.write_speeds_kbps.is_empty() {
                println!("  Write speeds (kB/s): {}", d.write_speeds_kbps.iter().map(u32::to_string).collect::<Vec<_>>().join(", "));
            }
            d.notes.iter().for_each(|n| println!("  Note: {n}"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("  MMC query not possible: {e}");
            ExitCode::FAILURE
        }
    }
}
