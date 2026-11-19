//! In-app self update for the AppImage and portable builds. Installer and
//! system installs are handled by the frontend (it only offers a download link).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

const RELEASES_API: &str = "https://api.github.com/repos/islmal/musadvd/releases/tags/";
const STAGING: &str = ".update-staging";

fn log(app: &AppHandle, msg: impl Into<String>) {
    let _ = app.emit("update-log", msg.into());
}

// ---- pure helpers (platform independent, unit tested) ----

/// Release asset to install for `mode`; `None` for modes the app cannot update itself.
fn pick_asset_name(mode: &str, version: &str, windows: bool) -> Option<String> {
    match (mode, windows) {
        ("appimage", false) => Some("_amd64.AppImage".into()), // suffix match, see find_asset
        ("portable", true) => Some(format!("MusaDVD-v{version}-windows-x64-portable.zip")),
        ("portable", false) => Some(format!("MusaDVD-v{version}-linux-x86_64-portable.tar.gz")),
        _ => None,
    }
}

fn asset_matches(wanted: &str, name: &str) -> bool {
    if wanted.starts_with('_') {
        name.ends_with(wanted)
    } else {
        name == wanted
    }
}

/// `sha256:<hex>` as GitHub reports it.
fn parse_digest(d: &str) -> Option<String> {
    let hex = d.strip_prefix("sha256:")?.trim();
    is_sha256_hex(hex).then(|| hex.to_ascii_lowercase())
}

/// First 64-hex token of a `.sha256` sidecar ("<hex>  file" or just "<hex>").
fn parse_sha_file(text: &str) -> Option<String> {
    let tok = text.split_whitespace().next()?;
    is_sha256_hex(tok).then(|| tok.to_ascii_lowercase())
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Moves every file below `src` over the same relative path in `dst`. With `keep_old`
/// (Windows) an existing file is first renamed to `<name>.old`, because a running exe can be
/// renamed but not overwritten; on Linux rename-over is safe for running binaries.
fn swap_tree(src: &Path, dst: &Path, keep_old: bool) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            swap_tree(&from, &to, keep_old)?;
            continue;
        }
        if keep_old && to.exists() {
            let mut old = to.clone().into_os_string();
            old.push(".old");
            let old = PathBuf::from(old);
            let _ = fs::remove_file(&old);
            fs::rename(&to, &old)?;
        }
        fs::rename(&from, &to)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", to.display())))?;
    }
    Ok(())
}

/// Non-recursive: a portable folder may hold user files ending in `.old`.
fn remove_old_files(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file() && p.extension().is_some_and(|x| x == "old") {
            let _ = fs::remove_file(&p);
        }
    }
}

// ---- install mode ----

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".musadvd-write-test-{}", std::process::id()));
    let ok = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

fn install_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| format!("no parent dir for {}", exe.display()))
}

fn detect_mode() -> &'static str {
    #[cfg(target_os = "linux")]
    if std::env::var_os("APPIMAGE").is_some_and(|p| Path::new(&p).is_file()) {
        return "appimage";
    }
    let (Ok(exe), Ok(dir)) = (std::env::current_exe(), install_dir()) else { return "system" };
    if cfg!(windows) {
        // Tauri's NSIS installer writes uninstall.exe next to the app exe.
        if dir.join("uninstall.exe").exists() {
            return "installer";
        }
    } else if exe.starts_with("/usr/") {
        return "system";
    }
    if dir_writable(&dir) { "portable" } else { "system" }
}

#[tauri::command]
pub fn update_install_mode() -> String {
    detect_mode().to_string()
}

/// Removes `.old` files of a previous Windows swap and stale staging dirs.
pub fn cleanup_leftovers() {
    let Ok(dir) = install_dir() else { return };
    let _ = fs::remove_dir_all(dir.join(STAGING));
    if cfg!(windows) && detect_mode() == "portable" {
        remove_old_files(&dir);
        remove_old_files(&dir.join("bin"));
    }
}

// ---- network ----

fn ensure_crypto() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

async fn get_ok(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("GET {url}: {e}"))
}

/// Streams `url` into `dest`, hashing on the fly; returns the lowercase hex SHA-256.
async fn download(app: &AppHandle, client: &reqwest::Client, url: &str, dest: &Path) -> Result<String, String> {
    use io::Write;
    let mut resp = get_ok(client, url).await?;
    let total = resp.content_length();
    let mut file = fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut hasher = Sha256::new();
    let (mut done, mut last) = (0u64, Instant::now());
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("download {url}: {e}"))? {
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|e| format!("write {}: {e}", dest.display()))?;
        done += chunk.len() as u64;
        if last.elapsed() >= Duration::from_millis(100) {
            last = Instant::now();
            let _ = app.emit("update-progress", json!({ "downloaded": done, "total": total }));
        }
    }
    file.flush().map_err(|e| format!("write {}: {e}", dest.display()))?;
    let _ = app.emit("update-progress", json!({ "downloaded": done, "total": total }));
    Ok(format!("{:x}", hasher.finalize()))
}

/// (download url, expected sha256) for the wanted asset of the release.
async fn resolve_asset(client: &reqwest::Client, version: &str, wanted: &str) -> Result<(String, String, String), String> {
    let rel: Value = get_ok(client, &format!("{RELEASES_API}v{version}"))
        .await?
        .json()
        .await
        .map_err(|e| format!("release v{version} JSON: {e}"))?;
    let assets = rel["assets"].as_array().ok_or("release has no assets")?;
    let asset = assets
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| asset_matches(wanted, n)))
        .ok_or_else(|| format!("release v{version} has no asset {wanted}"))?;
    let name = asset["name"].as_str().unwrap_or(wanted).to_string();
    let url = asset["browser_download_url"].as_str().ok_or_else(|| format!("{name}: no download url"))?.to_string();
    let mut sha = asset["digest"].as_str().and_then(parse_digest);
    if sha.is_none() {
        let side = format!("{name}.sha256");
        if let Some(u) = assets.iter().find(|a| a["name"].as_str() == Some(&side)).and_then(|a| a["browser_download_url"].as_str()) {
            let text = get_ok(client, u).await?.text().await.map_err(|e| format!("{side}: {e}"))?;
            sha = parse_sha_file(&text);
        }
    }
    let sha = sha.ok_or_else(|| format!("{name}: no SHA-256 digest published, refusing to install"))?;
    Ok((name, url, sha))
}

// ---- extraction ----

#[cfg(windows)]
fn extract(archive: &Path, out: &Path) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(fs::File::open(archive).map_err(|e| format!("open {}: {e}", archive.display()))?)
        .map_err(|e| format!("read {}: {e}", archive.display()))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("{}: {e}", archive.display()))?;
        // enclosed_name rejects absolute paths and `..` (zip-slip).
        let rel = entry.enclosed_name().ok_or_else(|| format!("unsafe path in archive: {}", entry.name()))?;
        let target = out.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            continue;
        }
        if let Some(p) = target.parent() {
            fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        let mut f = fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        io::copy(&mut entry, &mut f).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn extract(archive: &Path, out: &Path) -> Result<(), String> {
    let f = fs::File::open(archive).map_err(|e| format!("open {}: {e}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    tar.set_preserve_permissions(true);
    // unpack() goes through unpack_in, which refuses paths escaping `out`.
    tar.unpack(out).map_err(|e| format!("extract {}: {e}", archive.display()))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn extract(_: &Path, _: &Path) -> Result<(), String> {
    Err("portable update is not supported on this platform".into())
}

// ---- relaunch ----

fn relaunch(app: &AppHandle, exe: &Path) -> Result<(), String> {
    log(app, format!("Restarting {}", exe.display()));
    let mut cmd = musadvd::tools::blocking_tool_command(exe);
    // The new AppImage must not inherit the old mount's identity.
    for k in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
        cmd.env_remove(k);
    }
    cmd.spawn().map_err(|e| format!("spawn {}: {e}", exe.display()))?;
    app.exit(0);
    Ok(())
}

// ---- command ----

#[tauri::command]
pub async fn self_update(app: AppHandle, version: String) -> Result<(), String> {
    let mode = detect_mode();
    let wanted = pick_asset_name(mode, &version, cfg!(windows))
        .ok_or_else(|| format!("self update not available for install mode '{mode}'"))?;
    ensure_crypto();
    let client = reqwest::Client::builder().user_agent("MusaDVD-updater").build().map_err(|e| e.to_string())?;

    log(&app, format!("Looking up release v{version}"));
    let (name, url, expected) = resolve_asset(&client, &version, &wanted).await?;

    // Download next to the final location so the swap is a same-volume rename.
    let (work_dir, tmp) = if mode == "appimage" {
        let target = appimage_path()?;
        let dir = target.parent().ok_or("APPIMAGE has no parent dir")?.to_path_buf();
        let tmp = dir.join(format!(".{}.download", target.file_name().unwrap_or_default().to_string_lossy()));
        (dir, tmp)
    } else {
        let stage = install_dir()?.join(STAGING);
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage).map_err(|e| format!("create {}: {e}", stage.display()))?;
        let tmp = stage.join(&name);
        (stage, tmp)
    };

    log(&app, format!("Downloading {name}"));
    let got = download(&app, &client, &url, &tmp).await;
    let got = match got {
        Ok(h) if h == expected => h,
        Ok(h) => {
            cleanup_failed(mode, &tmp, &work_dir);
            return Err(format!("{name}: checksum mismatch (expected {expected}, got {h})"));
        }
        Err(e) => {
            cleanup_failed(mode, &tmp, &work_dir);
            return Err(e);
        }
    };
    log(&app, format!("Checksum verified (sha256 {got})"));

    let app2 = app.clone();
    let mode_s = mode.to_string();
    let relaunch_path = tauri::async_runtime::spawn_blocking(move || -> Result<PathBuf, String> {
        let r = if mode_s == "appimage" { install_appimage(&app2, &tmp) } else { install_portable(&app2, &tmp, &work_dir) };
        if r.is_err() {
            cleanup_failed(&mode_s, &tmp, &work_dir);
        }
        r
    })
    .await
    .map_err(|e| format!("install task: {e}"))??;

    relaunch(&app, &relaunch_path)
}

fn cleanup_failed(mode: &str, tmp: &Path, work_dir: &Path) {
    if mode == "appimage" {
        let _ = fs::remove_file(tmp);
    } else {
        let _ = fs::remove_dir_all(work_dir);
    }
}

fn appimage_path() -> Result<PathBuf, String> {
    std::env::var_os("APPIMAGE").map(PathBuf::from).ok_or_else(|| "APPIMAGE is not set".to_string())
}

fn install_appimage(app: &AppHandle, tmp: &Path) -> Result<PathBuf, String> {
    let old = appimage_path()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp, fs::Permissions::from_mode(0o755)).map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    }
    // Same path on purpose: users symlink the AppImage, so renaming it would break the link.
    log(app, format!("Installing {}", old.display()));
    fs::rename(tmp, &old).map_err(|e| format!("rename to {}: {e}", old.display()))?;
    Ok(old)
}

fn install_portable(app: &AppHandle, archive: &Path, stage: &Path) -> Result<PathBuf, String> {
    let dir = install_dir()?;
    let out = stage.join("new");
    log(app, "Extracting update");
    extract(archive, &out)?;
    let _ = fs::remove_file(archive);
    // The Linux tarball nests everything under MusaDVD/.
    let nested = out.join("MusaDVD");
    let root = if nested.is_dir() { nested } else { out };
    log(app, format!("Replacing files in {}", dir.display()));
    swap_tree(&root, &dir, cfg!(windows)).map_err(|e| format!("replace files in {}: {e}", dir.display()))?;
    let _ = fs::remove_dir_all(stage);
    Ok(dir.join(if cfg!(windows) { "musadvd-gui.exe" } else { "musadvd-gui" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_selection() {
        assert_eq!(pick_asset_name("portable", "0.2.0", true).unwrap(), "MusaDVD-v0.2.0-windows-x64-portable.zip");
        assert_eq!(pick_asset_name("portable", "0.2.0", false).unwrap(), "MusaDVD-v0.2.0-linux-x86_64-portable.tar.gz");
        assert!(pick_asset_name("installer", "0.2.0", true).is_none());
        let w = pick_asset_name("appimage", "0.2.0", false).unwrap();
        assert!(asset_matches(&w, "MusaDVD_0.2.0_amd64.AppImage"));
        assert!(!asset_matches(&w, "MusaDVD_0.2.0_amd64.AppImage.sha256"));
    }

    #[test]
    fn digests() {
        let h = "A".repeat(64);
        assert_eq!(parse_digest(&format!("sha256:{h}")).unwrap(), "a".repeat(64));
        assert!(parse_digest("sha1:abc").is_none());
        assert!(parse_digest("sha256:zz").is_none());
        assert_eq!(parse_sha_file(&format!("{h}  file.zip\n")).unwrap(), "a".repeat(64));
        assert!(parse_sha_file("nothex file").is_none());
    }

    #[test]
    fn swap_keeps_old_copy() {
        let base = std::env::temp_dir().join(format!("musadvd-swap-{}", std::process::id()));
        let (src, dst) = (base.join("src"), base.join("dst"));
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.exe"), "new").unwrap();
        fs::write(src.join("bin/b.exe"), "newb").unwrap();
        fs::write(dst.join("a.exe"), "old").unwrap();
        fs::create_dir_all(dst.join("deep")).unwrap();
        fs::write(dst.join("deep/user.old"), "keep").unwrap();
        swap_tree(&src, &dst, true).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a.exe")).unwrap(), "new");
        assert_eq!(fs::read_to_string(dst.join("a.exe.old")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dst.join("bin/b.exe")).unwrap(), "newb");
        remove_old_files(&dst);
        assert!(!dst.join("a.exe.old").exists());
        assert!(dst.join("deep/user.old").exists());
        let _ = fs::remove_dir_all(&base);
    }
}
