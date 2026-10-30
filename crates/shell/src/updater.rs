//! GitHub-release auto-update.
//!
//! Detection is a pure function of an injected [`InstallProbe`] so the
//! install matrix can be tested without reading the process environment.
//! Applying an update (`install_and_restart`) replaces the running
//! binary and exits; there is no single-instance lock to wait on, so the
//! relaunch helper just sleeps long enough for this process to die.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Returned from [`download`] when the cancel flag trips. Callers match
/// this exact string; it is not a failure to show as an error.
pub const CANCELLED: &str = "cancelled";

pub const RELEASES_PAGE: &str = "https://github.com/xmreur/sonora/releases/latest";

const RELEASES_LATEST: &str = "https://api.github.com/repos/xmreur/sonora/releases/latest";

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("sonora/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// `vX.Y.Z` with no pre-release or build suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// One file attached to a GitHub release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// A release newer than the running build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub tag: String,
    pub notes: String,
    pub html_url: String,
    pub assets: Vec<UpdateAsset>,
    /// Filename → lowercase hex sha256, from the `SHA256SUMS` asset.
    pub checksums: Vec<(String, String)>,
}

/// How this process was installed. Drives which asset we download and
/// whether we can replace ourselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// `$APPIMAGE` is set; replace that file.
    AppImage { path: PathBuf },
    /// Loose Linux or macOS binary (the release tar.gz).
    Portable { exe: PathBuf },
    /// deb / AUR: the executable lives under `/usr`.
    PackageManaged,
    /// `/.flatpak-info` is present.
    Flatpak,
    /// `HKLM\Software\Sonora\InstallDir` matches the executable's directory.
    WindowsNsis { exe: PathBuf },
    /// Any other Windows layout (the portable zip).
    WindowsPortable { exe: PathBuf },
    /// Executable inside `Sonora.app`. `writable` is the parent directory.
    MacApp { bundle: PathBuf, writable: bool },
    /// Read-only, unrecognized, or otherwise not self-updatable.
    NotifyOnly,
}

impl InstallKind {
    /// `true` when "Update" can download and replace this install.
    pub fn can_apply(&self) -> bool {
        match self {
            Self::AppImage { .. }
            | Self::Portable { .. }
            | Self::WindowsNsis { .. }
            | Self::WindowsPortable { .. } => true,
            Self::MacApp { writable, .. } => *writable,
            Self::PackageManaged | Self::Flatpak | Self::NotifyOnly => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsKind {
    Linux,
    Macos,
    Windows,
    Other,
}

/// Inputs to [`detect_install`]. Paths are not opened.
#[derive(Clone, Debug)]
pub struct InstallProbe {
    pub os: OsKind,
    pub exe: PathBuf,
    pub appimage: Option<String>,
    pub flatpak: bool,
    pub nsis_install_dir: Option<PathBuf>,
    /// The directory we would write into (AppImage parent, exe parent,
    /// or the `.app` parent) is writable by this user.
    pub writable: bool,
}

/// Tags are strictly `vX.Y.Z` (digits only, three components).
pub fn parse_version(tag: &str) -> Option<Version> {
    let rest = tag.strip_prefix('v')?;
    parse_triplet(rest)
}

/// Running version from `CARGO_PKG_VERSION` (`X.Y.Z`) or a tag (`vX.Y.Z`).
pub fn parse_current(version: &str) -> Option<Version> {
    parse_version(version).or_else(|| parse_triplet(version))
}

fn parse_triplet(s: &str) -> Option<Version> {
    let mut parts = s.split('.');
    let major = parse_component(parts.next()?)?;
    let minor = parse_component(parts.next()?)?;
    let patch = parse_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

fn parse_component(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

pub fn cmp_version(a: &Version, b: &Version) -> Ordering {
    a.major
        .cmp(&b.major)
        .then(a.minor.cmp(&b.minor))
        .then(a.patch.cmp(&b.patch))
}

pub fn detect_install(probe: &InstallProbe) -> InstallKind {
    if probe.flatpak {
        return InstallKind::Flatpak;
    }
    if let Some(path) = probe
        .appimage
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return if probe.writable {
            InstallKind::AppImage {
                path: PathBuf::from(path),
            }
        } else {
            InstallKind::NotifyOnly
        };
    }
    if probe.os == OsKind::Windows {
        let dir = exe_dir(&probe.exe);
        if probe
            .nsis_install_dir
            .as_ref()
            .is_some_and(|installed| same_dir(installed, &dir))
        {
            return InstallKind::WindowsNsis {
                exe: probe.exe.clone(),
            };
        }
        return if probe.writable {
            InstallKind::WindowsPortable {
                exe: probe.exe.clone(),
            }
        } else {
            InstallKind::NotifyOnly
        };
    }
    if let Some(bundle) = sonora_app_bundle(&probe.exe) {
        return InstallKind::MacApp {
            bundle,
            writable: probe.writable,
        };
    }
    if under_usr(&probe.exe) {
        return InstallKind::PackageManaged;
    }
    if matches!(probe.os, OsKind::Linux | OsKind::Macos) {
        return if probe.writable {
            InstallKind::Portable {
                exe: probe.exe.clone(),
            }
        } else {
            InstallKind::NotifyOnly
        };
    }
    InstallKind::NotifyOnly
}

/// Classify the running process.
pub fn current_install() -> InstallKind {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sonora"));
    let appimage = std::env::var("APPIMAGE").ok();
    let flatpak = Path::new("/.flatpak-info").exists();
    let nsis_install_dir = nsis_install_dir();
    let os = match std::env::consts::OS {
        "linux" => OsKind::Linux,
        "macos" => OsKind::Macos,
        "windows" => OsKind::Windows,
        _ => OsKind::Other,
    };
    let write_target =
        if let Some(path) = appimage.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            parent_dir(Path::new(path))
        } else if let Some(bundle) = sonora_app_bundle(&exe) {
            parent_dir(&bundle)
        } else {
            exe_dir(&exe)
        };
    detect_install(&InstallProbe {
        os,
        exe,
        appimage,
        flatpak,
        nsis_install_dir,
        writable: dir_writable(&write_target),
    })
}

fn exe_dir(exe: &Path) -> PathBuf {
    // `Path::parent` only splits on the host's separator, so a Windows
    // path injected into a Linux test would stay one component. Split on
    // both separators when the string uses backslashes.
    let text = exe.to_string_lossy();
    if text.contains('\\') {
        let trimmed = text.trim_end_matches(['\\', '/']);
        if let Some((dir, _)) = trimmed.rsplit_once(['\\', '/']) {
            return PathBuf::from(dir);
        }
    }
    exe.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(exe)
        .to_path_buf()
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path)
        .to_path_buf()
}

fn under_usr(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text == "/usr" || text.starts_with("/usr/")
}

fn sonora_app_bundle(exe: &Path) -> Option<PathBuf> {
    let raw = exe.to_string_lossy();
    let marker = "Sonora.app";
    let idx = raw.find(marker)?;
    let end = idx + marker.len();
    if let Some(next) = raw[end..].chars().next() {
        if next != '/' && next != '\\' {
            return None;
        }
    }
    Some(PathBuf::from(&raw[..end]))
}

/// Windows registry paths and `current_exe` can disagree on slash and case.
fn same_dir(a: &Path, b: &Path) -> bool {
    fn parts(path: &Path) -> Vec<String> {
        path.to_string_lossy()
            .split(['\\', '/'])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    }
    parts(a) == parts(b)
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".sonora-update-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(windows)]
fn nsis_install_dir() -> Option<PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("Software\\Sonora")
        .ok()?;
    let dir: String = key.get_value("InstallDir").ok()?;
    if dir.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

#[cfg(not(windows))]
fn nsis_install_dir() -> Option<PathBuf> {
    None
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Parse a `GET /repos/{owner}/{repo}/releases/latest` body.
pub fn parse_github_release(body: &str) -> Result<UpdateInfo, String> {
    let release: GhRelease = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if release.tag_name.is_empty() {
        return Err("release has no tag".into());
    }
    Ok(UpdateInfo {
        tag: release.tag_name,
        notes: release.body.unwrap_or_default(),
        html_url: release.html_url,
        assets: release
            .assets
            .into_iter()
            .map(|a| UpdateAsset {
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
            })
            .collect(),
        checksums: Vec::new(),
    })
}

/// GNU `sha256sum` output: `<hex><whitespace>[*]<filename>`.
/// Blank lines and `#` comments are skipped; malformed lines are ignored.
pub fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hash, rest)) = line.split_once([' ', '\t']) else {
            continue;
        };
        let hash = hash.trim();
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let name = rest.trim().trim_start_matches('*').trim();
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), hash.to_ascii_lowercase()));
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// `Ok(None)` when `current` is the same release or newer.
pub async fn check_for_update(current: &str) -> Result<Option<UpdateInfo>, String> {
    evaluate_release(RELEASES_LATEST, current).await
}

/// `latest_url` is the GitHub `/releases/latest` document. Tests point it
/// at a local server; the shipped check uses [`RELEASES_LATEST`].
async fn evaluate_release(latest_url: &str, current: &str) -> Result<Option<UpdateInfo>, String> {
    let current = parse_current(current).ok_or_else(|| format!("bad version {current:?}"))?;
    let body = fetch_bytes(latest_url, 2 * 1024 * 1024, true).await?;
    let text = String::from_utf8(body).map_err(|_| "release metadata is not utf-8".to_string())?;
    let mut info = parse_github_release(&text)?;
    let remote = parse_version(&info.tag)
        .ok_or_else(|| format!("release tag {} is not vX.Y.Z", info.tag))?;
    if cmp_version(&remote, &current) != Ordering::Greater {
        return Ok(None);
    }
    let sums_url = info
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .map(|a| a.url.clone())
        .ok_or_else(|| "release has no SHA256SUMS".to_string())?;
    let sums = fetch_bytes(&sums_url, 256 * 1024, false).await?;
    let sums = String::from_utf8(sums).map_err(|_| "SHA256SUMS is not utf-8".to_string())?;
    info.checksums = parse_sha256sums(&sums);
    if info.checksums.is_empty() {
        return Err("SHA256SUMS is empty".into());
    }
    Ok(Some(info))
}

async fn fetch_bytes(url: &str, limit: usize, github_api: bool) -> Result<Vec<u8>, String> {
    let mut req = HTTP.get(url).timeout(Duration::from_secs(30));
    if github_api {
        req = req
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("update check failed ({})", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("update response is unexpectedly large".into());
    }
    Ok(bytes.to_vec())
}

/// Asset name this install should download for `version` (`X.Y.Z`).
pub fn asset_filename(
    kind: &InstallKind,
    version: &str,
    arch: &str,
    os: &str,
) -> Result<String, String> {
    match kind {
        InstallKind::AppImage { .. } => {
            if arch != "x86_64" {
                return Err(format!("no AppImage is published for {arch}"));
            }
            Ok(format!("Sonora-{version}-x86_64.AppImage"))
        }
        InstallKind::Portable { .. } => match os {
            "linux" => Ok(format!("sonora-{version}-{arch}-unknown-linux-gnu.tar.gz")),
            "macos" => Ok(format!("sonora-{version}-{arch}-apple-darwin.tar.gz")),
            other => Err(format!("no portable archive is published for {other}")),
        },
        InstallKind::WindowsNsis { .. } => {
            if arch != "x86_64" {
                return Err(format!("no Windows installer is published for {arch}"));
            }
            Ok(format!("Sonora-Setup-{version}.exe"))
        }
        InstallKind::WindowsPortable { .. } => {
            if arch != "x86_64" {
                return Err(format!("no Windows portable build is published for {arch}"));
            }
            Ok(format!("Sonora-{version}-windows-x86_64-portable.zip"))
        }
        InstallKind::MacApp { writable, .. } => {
            if !writable {
                return Err("the application bundle is not writable".into());
            }
            Ok(format!("Sonora-{version}-{arch}-apple-darwin-app.zip"))
        }
        InstallKind::PackageManaged | InstallKind::Flatpak | InstallKind::NotifyOnly => {
            Err("this install cannot replace itself".into())
        }
    }
}

fn version_number(tag: &str) -> Result<String, String> {
    let v = parse_version(tag).ok_or_else(|| format!("release tag {tag} is not vX.Y.Z"))?;
    Ok(format!("{}.{}.{}", v.major, v.minor, v.patch))
}

/// Download the asset for `kind`, verify it against `info.checksums`,
/// and leave the finished file in the temp update directory.
pub async fn download_for(
    info: &UpdateInfo,
    kind: &InstallKind,
    progress: &Arc<AtomicU64>,
    total: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
) -> Result<PathBuf, String> {
    let version = version_number(&info.tag)?;
    let name = asset_filename(kind, &version, std::env::consts::ARCH, std::env::consts::OS)?;
    let asset = info
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| format!("release has no {name}"))?;
    let expected = info
        .checksums
        .iter()
        .find(|(n, _)| n == &name)
        .map(|(_, hash)| hash.as_str())
        .ok_or_else(|| format!("SHA256SUMS has no entry for {name}"))?;
    let dest = work_dir()?.join(&name);
    download(&asset.url, &dest, expected, progress, total, cancel).await?;
    Ok(dest)
}

/// Stream `url` to `dest` via a `.part` sibling, then rename once the
/// sha256 matches. `progress` counts bytes written; `total` is the
/// content length when the server sends one.
pub async fn download(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    progress: &Arc<AtomicU64>,
    total: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    if cancel.load(AtomicOrdering::Relaxed) {
        return Err(CANCELLED.into());
    }
    progress.store(0, AtomicOrdering::Relaxed);
    total.store(0, AtomicOrdering::Relaxed);

    let resp = HTTP.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download failed ({})", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        total.store(len, AtomicOrdering::Relaxed);
    }
    let mut resp = resp;
    let mut part_name = dest.as_os_str().to_owned();
    part_name.push(".part");
    let part = PathBuf::from(part_name);
    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return Err(CANCELLED.into());
        }
        let chunk = resp.chunk().await.map_err(|e| e.to_string())?;
        let Some(chunk) = chunk else { break };
        use tokio::io::AsyncWriteExt;
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        progress.store(got, AtomicOrdering::Relaxed);
    }
    use tokio::io::AsyncWriteExt;
    file.flush().await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    drop(file);

    let digest = hex_encode(&hasher.finalize());
    if !digest.eq_ignore_ascii_case(expected_sha256.trim()) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err("download checksum did not match SHA256SUMS".into());
    }
    if cancel.load(AtomicOrdering::Relaxed) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(CANCELLED.into());
    }
    let _ = tokio::fs::remove_file(dest).await;
    tokio::fs::rename(&part, dest)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
static TEST_TMP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn work_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    let root = TEST_TMP.get().cloned().unwrap_or_else(std::env::temp_dir);
    #[cfg(not(test))]
    let root = std::env::temp_dir();
    let dir = root.join("sonora-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Replace the running install with `package` (the verified download) and
/// start it again. On success this does not return.
pub fn install_and_restart(kind: &InstallKind, package: &Path) -> Result<(), String> {
    match kind {
        InstallKind::AppImage { path } => {
            stage_appimage(package, path)?;
            relaunch(path, None)
        }
        InstallKind::Portable { exe } => {
            stage_portable(package, exe)?;
            relaunch(exe, None)
        }
        InstallKind::WindowsNsis { exe } => install_nsis(package, exe),
        InstallKind::WindowsPortable { exe } => install_windows_portable(package, exe),
        InstallKind::MacApp { bundle, writable } => {
            if !writable {
                return Err("the application bundle is not writable".into());
            }
            install_mac_app(package, bundle)
        }
        InstallKind::PackageManaged | InstallKind::Flatpak | InstallKind::NotifyOnly => {
            Err("this install cannot replace itself".into())
        }
    }
}

#[cfg(unix)]
fn stage_appimage(package: &Path, path: &Path) -> Result<(), String> {
    replace_file(package, path)?;
    make_executable(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn stage_appimage(_package: &Path, _path: &Path) -> Result<(), String> {
    Err("AppImage replacement is only applied on unix".into())
}

/// Unpack the release tar.gz and swap its `sonora` binary into `exe`.
/// Does not relaunch — [`install_and_restart`] does that after this returns.
#[cfg(unix)]
fn stage_portable(package: &Path, exe: &Path) -> Result<(), String> {
    // Pid in the directory name so a `cargo test` run cannot wipe an
    // extract that a live Sonora process is mid-way through.
    let dest = work_dir()?.join(format!("extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let result = (|| {
        extract_tar_gz(package, &dest)?;
        let bin = find_file_named(&dest, "sonora")?;
        replace_file(&bin, exe)?;
        make_executable(exe)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dest);
    result
}

#[cfg(not(unix))]
fn stage_portable(_package: &Path, _exe: &Path) -> Result<(), String> {
    Err("portable archives are only applied on unix".into())
}

#[cfg(unix)]
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    tar.set_preserve_permissions(true);
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("archive path escapes the extract directory".into());
        }
        entry.unpack_in(dest).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms).map_err(|e| e.to_string())
}

/// Copy into the destination's directory, then rename over the target so
/// a crash mid-copy never leaves a truncated binary in place.
fn replace_file(src: &Path, dest: &Path) -> Result<(), String> {
    // Same filesystem (the usual /tmp download next to a portable binary):
    // rename is atomic. /tmp and $HOME are often different mounts, and
    // rename then fails with EXDEV — copy into the destination directory
    // and rename from there.
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    replace_file_by_copy(src, dest)
}

fn replace_file_by_copy(src: &Path, dest: &Path) -> Result<(), String> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "update target has no directory".to_string())?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sonora");
    let staged = parent.join(format!(".{name}.new"));
    std::fs::copy(src, &staged).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&staged, dest) {
        let _ = std::fs::remove_file(&staged);
        return Err(e.to_string());
    }
    Ok(())
}

fn find_file_named(root: &Path, name: &str) -> Result<PathBuf, String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Ok(path);
            }
        }
    }
    Err(format!("archive has no {name}"))
}

#[cfg(unix)]
fn relaunch_script(
    exe: &Path,
    cleanup: Option<&Path>,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> String {
    let mut script = String::from("sleep 1; ");
    if let Some(path) = cleanup {
        script.push_str("rm -rf ");
        script.push_str(&shell_quote(path.as_os_str()));
        script.push_str("; ");
    }
    script.push_str("exec ");
    script.push_str(&shell_quote(exe.as_os_str()));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(&arg));
    }
    script
}

#[cfg(unix)]
fn relaunch(exe: &Path, cleanup: Option<&Path>) -> Result<(), String> {
    let script = relaunch_script(exe, cleanup, std::env::args_os().skip(1));
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .spawn()
        .map_err(|e| e.to_string())?;
    std::process::exit(0);
}

#[cfg(not(unix))]
fn relaunch(exe: &Path, cleanup: Option<&Path>) -> Result<(), String> {
    let _ = (exe, cleanup);
    Err("unix relaunch is not available on this platform".into())
}

#[cfg(unix)]
fn shell_quote(s: &std::ffi::OsStr) -> String {
    let text = s.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn install_mac_app(package: &Path, bundle: &Path) -> Result<(), String> {
    let dest = work_dir()?.join("app");
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let status = std::process::Command::new("ditto")
        .args(["-x", "-k"])
        .arg(package)
        .arg(&dest)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("ditto failed to extract the app archive".into());
    }
    let extracted = dest.join("Sonora.app");
    if !extracted.is_dir() {
        return Err("archive has no Sonora.app".into());
    }
    let backup = bundle.with_file_name("Sonora.app.previous");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(bundle, &backup).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&extracted, bundle) {
        let _ = std::fs::rename(&backup, bundle);
        return Err(e.to_string());
    }
    let binary = bundle.join("Contents/MacOS/sonora");
    relaunch(&binary, Some(&backup))
}

#[cfg(not(target_os = "macos"))]
fn install_mac_app(_package: &Path, _bundle: &Path) -> Result<(), String> {
    Err("macOS app replacement is only available on macOS".into())
}

/// `%` is expanded by cmd even inside quotes; `"` would break the quoting.
#[cfg(any(windows, test))]
fn cmd_quote(path: &Path) -> String {
    let text = path
        .display()
        .to_string()
        .replace('%', "%%")
        .replace('"', "");
    format!("\"{text}\"")
}

#[cfg(any(windows, test))]
fn nsis_install_body(installer: &Path, exe: &Path) -> String {
    format!(
        "start /wait \"\" {installer} /S\r\nif errorlevel 1 exit /b 1\r\nstart \"\" {exe}\r\n",
        installer = cmd_quote(installer),
        exe = cmd_quote(exe),
    )
}

#[cfg(any(windows, test))]
fn portable_install_body(source: &Path, target: &Path, exe: &Path) -> String {
    // `if errorlevel 8` is ">= 8". robocopy uses 0–7 for success.
    format!(
        "robocopy {source} {target} /E /NFL /NDL /NJH /NJS /nc /ns /np\r\n\
         if errorlevel 8 exit /b 8\r\n\
         start \"\" {exe}\r\n",
        source = cmd_quote(source),
        target = cmd_quote(target),
        exe = cmd_quote(exe),
    )
}

#[cfg(any(windows, test))]
fn windows_wrapper_script(pid: u32, body: &str) -> String {
    format!(
        "@echo off\r\n\
         chcp 65001 >nul\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>nul | find \"{pid}\" >nul\r\n\
         if not errorlevel 1 (\r\n\
           timeout /t 1 /nobreak >nul <nul\r\n\
           goto wait\r\n\
         )\r\n\
         {body}\
         (goto) 2>nul & del \"%~f0\"\r\n"
    )
}

#[cfg(windows)]
fn install_nsis(package: &Path, exe: &Path) -> Result<(), String> {
    spawn_windows_wrapper(&nsis_install_body(package, exe))
}

#[cfg(not(windows))]
fn install_nsis(_package: &Path, _exe: &Path) -> Result<(), String> {
    Err("the Windows installer is only applied on Windows".into())
}

#[cfg(windows)]
fn install_windows_portable(package: &Path, exe: &Path) -> Result<(), String> {
    let dest = work_dir()?.join("portable");
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    extract_zip(package, &dest)?;
    let bin = find_file_named(&dest, "sonora.exe")?;
    let source = bin
        .parent()
        .ok_or_else(|| "portable archive has no directory".to_string())?;
    let target = exe
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "executable has no directory".to_string())?;
    spawn_windows_wrapper(&portable_install_body(source, target, exe))
}

#[cfg(not(windows))]
fn install_windows_portable(_package: &Path, _exe: &Path) -> Result<(), String> {
    Err("the Windows portable zip is only applied on Windows".into())
}

#[cfg(windows)]
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Wait until this process exits, then run `body` and delete the script.
/// `CREATE_NO_WINDOW` keeps the helper off the taskbar. The process exits
/// after the helper is spawned so the wait loop can observe it dying.
#[cfg(windows)]
fn spawn_windows_wrapper(body: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    let dir = work_dir()?;
    let script = dir.join("sonora-update.cmd");
    let text = windows_wrapper_script(std::process::id(), body);
    let mut file = std::fs::File::create(&script).map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    drop(file);
    // 0x08000000 = CREATE_NO_WINDOW
    std::process::Command::new("cmd.exe")
        .arg("/D")
        .arg("/C")
        .arg(&script)
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|e| e.to_string())?;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;

    use super::*;

    fn v(tag: &str) -> Version {
        parse_version(tag).unwrap_or_else(|| panic!("parse {tag}"))
    }

    #[test]
    fn version_compare_requires_v_prefix_and_orders_numerically() {
        assert!(parse_version("1.2.3").is_none(), "v prefix is required");
        assert!(parse_version("v1.2").is_none(), "three components");
        assert!(parse_version("v1.2.3-beta").is_none(), "no pre-release");
        assert!(parse_version("v1.2.3+1").is_none(), "no build metadata");
        assert_eq!(parse_version("v1.2.3"), Some(v("v1.2.3")));

        assert_eq!(cmp_version(&v("v1.2.3"), &v("v1.2.3")), Ordering::Equal);
        assert_eq!(cmp_version(&v("v1.2.4"), &v("v1.2.3")), Ordering::Greater);
        assert_eq!(cmp_version(&v("v1.3.0"), &v("v1.2.9")), Ordering::Greater);
        assert_eq!(cmp_version(&v("v2.0.0"), &v("v1.9.9")), Ordering::Greater);
        // A lower tag is a downgrade and must not be offered.
        assert_eq!(cmp_version(&v("v1.2.2"), &v("v1.2.3")), Ordering::Less);
        assert_eq!(cmp_version(&v("v0.9.0"), &v("v1.0.0")), Ordering::Less);

        assert_eq!(
            parse_current("0.1.9").unwrap(),
            parse_version("v0.1.9").unwrap()
        );
        assert!(parse_current("0.1").is_none());
    }

    fn probe(os: OsKind, exe: &str) -> InstallProbe {
        InstallProbe {
            os,
            exe: PathBuf::from(exe),
            appimage: None,
            flatpak: false,
            nsis_install_dir: None,
            writable: true,
        }
    }

    #[test]
    fn detect_install_matches_the_install_matrix() {
        let mut appimage = probe(OsKind::Linux, "/tmp/squashfs-root/usr/bin/sonora");
        appimage.appimage = Some("/home/me/Sonora-1.2.3-x86_64.AppImage".into());
        assert_eq!(
            detect_install(&appimage),
            InstallKind::AppImage {
                path: PathBuf::from("/home/me/Sonora-1.2.3-x86_64.AppImage"),
            }
        );
        appimage.writable = false;
        assert_eq!(detect_install(&appimage), InstallKind::NotifyOnly);

        assert_eq!(
            detect_install(&probe(OsKind::Linux, "/usr/bin/sonora")),
            InstallKind::PackageManaged
        );
        assert_eq!(
            detect_install(&probe(OsKind::Linux, "/usr/local/bin/sonora")),
            InstallKind::PackageManaged,
            "AUR and deb both land under /usr"
        );

        let mut flatpak = probe(OsKind::Linux, "/usr/bin/sonora");
        flatpak.flatpak = true;
        assert_eq!(detect_install(&flatpak), InstallKind::Flatpak);

        let mut home = probe(OsKind::Linux, "/home/me/sonora");
        assert_eq!(
            detect_install(&home),
            InstallKind::Portable {
                exe: PathBuf::from("/home/me/sonora"),
            }
        );
        home.writable = false;
        assert_eq!(detect_install(&home), InstallKind::NotifyOnly);

        let mac_bin = probe(
            OsKind::Macos,
            "/Users/me/sonora-1.2.3-aarch64-apple-darwin/sonora",
        );
        assert!(matches!(
            detect_install(&mac_bin),
            InstallKind::Portable { .. }
        ));

        let app = probe(
            OsKind::Macos,
            "/Applications/Sonora.app/Contents/MacOS/sonora",
        );
        assert_eq!(
            detect_install(&app),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Sonora.app"),
                writable: true,
            }
        );
        let mut readonly_app = app;
        readonly_app.writable = false;
        assert_eq!(
            detect_install(&readonly_app),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Sonora.app"),
                writable: false,
            }
        );
        assert!(!detect_install(&readonly_app).can_apply());

        let mut nsis = probe(OsKind::Windows, r"C:\Program Files\Sonora\sonora.exe");
        nsis.nsis_install_dir = Some(PathBuf::from(r"C:\Program Files\Sonora"));
        nsis.writable = false;
        assert_eq!(
            detect_install(&nsis),
            InstallKind::WindowsNsis {
                exe: PathBuf::from(r"C:\Program Files\Sonora\sonora.exe"),
            }
        );
        nsis.nsis_install_dir = Some(PathBuf::from(r"c:/program files/sonora/"));
        assert!(
            matches!(detect_install(&nsis), InstallKind::WindowsNsis { .. }),
            "registry path match is case- and slash-insensitive"
        );

        let mut portable = probe(OsKind::Windows, r"D:\apps\sonora\sonora.exe");
        portable.nsis_install_dir = Some(PathBuf::from(r"C:\Program Files\Sonora"));
        assert!(matches!(
            detect_install(&portable),
            InstallKind::WindowsPortable { .. }
        ));
        portable.writable = false;
        assert_eq!(detect_install(&portable), InstallKind::NotifyOnly);

        assert_eq!(
            detect_install(&probe(OsKind::Other, "/opt/sonora")),
            InstallKind::NotifyOnly
        );
    }

    #[test]
    fn parses_a_github_release_fixture() {
        let body = r#"{
            "tag_name": "v1.2.3",
            "body": "Fixes seek.\n",
            "html_url": "https://github.com/xmreur/sonora/releases/tag/v1.2.3",
            "assets": [
                {
                    "name": "Sonora-1.2.3-x86_64.AppImage",
                    "browser_download_url": "https://example.com/appimage",
                    "size": 42
                },
                {
                    "name": "SHA256SUMS",
                    "browser_download_url": "https://example.com/sums",
                    "size": 8
                }
            ]
        }"#;
        let info = parse_github_release(body).unwrap();
        assert_eq!(info.tag, "v1.2.3");
        assert_eq!(info.notes, "Fixes seek.\n");
        assert_eq!(
            info.html_url,
            "https://github.com/xmreur/sonora/releases/tag/v1.2.3"
        );
        assert_eq!(info.assets.len(), 2);
        assert_eq!(info.assets[0].name, "Sonora-1.2.3-x86_64.AppImage");
        assert_eq!(info.assets[0].url, "https://example.com/appimage");
        assert_eq!(info.assets[0].size, 42);
        assert!(info.checksums.is_empty());

        let empty_notes = r#"{"tag_name":"v1.2.4","body":null,"html_url":"","assets":[]}"#;
        let info = parse_github_release(empty_notes).unwrap();
        assert_eq!(info.notes, "");
        assert!(info.assets.is_empty());
    }

    #[test]
    fn parses_sha256sums() {
        let text = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  Sonora-1.2.3-x86_64.AppImage
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *sonora-1.2.3-x86_64-unknown-linux-gnu.tar.gz

# comment
not-a-hash  skip-me
cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\tSonora-Setup-1.2.3.exe
";
        let sums = parse_sha256sums(text);
        assert_eq!(
            sums,
            vec![
                ("Sonora-1.2.3-x86_64.AppImage".into(), "a".repeat(64),),
                (
                    "sonora-1.2.3-x86_64-unknown-linux-gnu.tar.gz".into(),
                    "b".repeat(64),
                ),
                ("Sonora-Setup-1.2.3.exe".into(), "c".repeat(64)),
            ]
        );
    }

    #[test]
    fn sha256_hex_matches_the_empty_and_abc_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn asset_names_follow_the_release_archives() {
        let version = "1.2.3";
        assert_eq!(
            asset_filename(
                &InstallKind::AppImage {
                    path: PathBuf::from("/tmp/s.AppImage")
                },
                version,
                "x86_64",
                "linux"
            )
            .unwrap(),
            "Sonora-1.2.3-x86_64.AppImage"
        );
        assert_eq!(
            asset_filename(
                &InstallKind::Portable {
                    exe: PathBuf::from("/tmp/sonora")
                },
                version,
                "x86_64",
                "linux"
            )
            .unwrap(),
            "sonora-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_filename(
                &InstallKind::WindowsNsis {
                    exe: PathBuf::from("sonora.exe")
                },
                version,
                "x86_64",
                "windows"
            )
            .unwrap(),
            "Sonora-Setup-1.2.3.exe"
        );
        assert_eq!(
            asset_filename(
                &InstallKind::WindowsPortable {
                    exe: PathBuf::from("sonora.exe")
                },
                version,
                "x86_64",
                "windows"
            )
            .unwrap(),
            "Sonora-1.2.3-windows-x86_64-portable.zip"
        );
        assert_eq!(
            asset_filename(
                &InstallKind::MacApp {
                    bundle: PathBuf::from("/Applications/Sonora.app"),
                    writable: true,
                },
                version,
                "aarch64",
                "macos"
            )
            .unwrap(),
            "Sonora-1.2.3-aarch64-apple-darwin-app.zip"
        );
        assert!(asset_filename(&InstallKind::PackageManaged, version, "x86_64", "linux").is_err());
        assert!(asset_filename(&InstallKind::Flatpak, version, "x86_64", "linux").is_err());
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fut)
    }

    /// Local stand-in for a GitHub release: one request per connection,
    /// `Connection: close`, and `/redir/<name>` → `/files/<name>` so the
    /// client has to follow the same hop release assets use.
    fn bind_local() -> (std::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        (listener, base)
    }

    fn serve(routes: HashMap<String, Vec<u8>>) -> String {
        let (listener, base) = bind_local();
        serve_on(listener, routes);
        base
    }

    fn serve_on(listener: std::net::TcpListener, routes: HashMap<String, Vec<u8>>) {
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                            if buf.len() > 8192 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let req = String::from_utf8_lossy(&buf);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                if let Some(name) = path.strip_prefix("/redir/") {
                    let loc = format!("http://{addr}/files/{name}");
                    let resp = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    continue;
                }
                if let Some(body) = routes.get(path) {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        });
    }

    fn temp_dir(label: &str) -> PathBuf {
        // `/tmp` is a small tmpfs in this environment and hits a quota
        // mid-extract. The workspace `target/` directory is the real disk.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/upd-sim");
        std::fs::create_dir_all(&root).unwrap();
        let _ = TEST_TMP.set(root.clone());
        let dir = root.join(format!(
            "sonora-upd-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sim_release_check_uses_the_github_document_and_skips_older_tags() {
        let sums = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  Sonora-0.2.0-x86_64.AppImage\n".to_vec();
        let base_routes = |base: &str| {
            let mut routes = HashMap::new();
            let sums_url = format!("{base}/redir/SHA256SUMS");
            let release = |tag: &str, with_sums: bool| {
                let assets = if with_sums {
                    serde_json::json!([{
                        "name": "SHA256SUMS",
                        "browser_download_url": sums_url,
                        "size": sums.len(),
                    }])
                } else {
                    serde_json::json!([])
                };
                serde_json::to_vec(&serde_json::json!({
                    "tag_name": tag,
                    "body": "Fixes seek.\n",
                    "html_url": format!("{base}/notes"),
                    "assets": assets,
                }))
                .unwrap()
            };
            routes.insert("/same".into(), release("v0.1.9", true));
            routes.insert("/older".into(), release("v0.1.8", true));
            routes.insert("/newer".into(), release("v0.2.0", true));
            routes.insert("/nosums".into(), release("v0.2.0", false));
            routes.insert("/files/SHA256SUMS".into(), sums.clone());
            routes.insert("/files/BADSUMS".into(), b"nope\n".to_vec());
            routes.insert(
                "/badsums".into(),
                serde_json::to_vec(&serde_json::json!({
                    "tag_name": "v0.2.0",
                    "body": "",
                    "html_url": "",
                    "assets": [{
                        "name": "SHA256SUMS",
                        "browser_download_url": format!("{base}/files/BADSUMS"),
                        "size": 5,
                    }],
                }))
                .unwrap(),
            );
            routes
        };
        let (listener, base) = bind_local();
        serve_on(listener, base_routes(&base));

        block_on(async {
            let same = evaluate_release(&format!("{base}/same"), "0.1.9")
                .await
                .expect("same tag");
            assert!(same.is_none(), "the running version is not an update");
            let older = evaluate_release(&format!("{base}/older"), "0.1.9")
                .await
                .expect("older tag");
            assert!(older.is_none(), "a downgrade must not be offered");

            let newer = evaluate_release(&format!("{base}/newer"), "0.1.9")
                .await
                .expect("newer tag")
                .expect("update");
            assert_eq!(newer.tag, "v0.2.0");
            assert_eq!(newer.notes, "Fixes seek.\n");
            assert_eq!(
                newer.checksums,
                vec![("Sonora-0.2.0-x86_64.AppImage".into(), "a".repeat(64),)]
            );

            let missing = evaluate_release(&format!("{base}/nosums"), "0.1.9")
                .await
                .expect_err("missing sums");
            assert!(missing.contains("SHA256SUMS"), "{missing}");
            let garbage = evaluate_release(&format!("{base}/badsums"), "0.1.9")
                .await
                .expect_err("garbage sums");
            assert!(garbage.contains("empty"), "{garbage}");
        });
    }

    #[test]
    fn sim_checksum_mismatch_and_cancel_leave_no_partial_file() {
        let body = b"not-the-bytes-we-hashed".to_vec();
        let mut routes = HashMap::new();
        routes.insert("/files/payload".into(), body.clone());
        let base = serve(routes);
        let dir = temp_dir("mismatch");
        let dest = dir.join("payload");
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let err = block_on(download(
            &format!("{base}/redir/payload"),
            &dest,
            &"ab".repeat(32),
            &progress,
            &total,
            &cancel,
        ))
        .expect_err("mismatch");
        assert!(err.contains("checksum"), "{err}");
        assert!(!dest.exists());
        assert!(!PathBuf::from(format!("{}.part", dest.display())).exists());

        // Pause after the first bytes so cancel is observed before rename.
        let first = vec![7u8; 32];
        let rest = vec![8u8; 64];
        let go = Arc::new(AtomicBool::new(false));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let go_srv = Arc::clone(&go);
        let first_srv = first.clone();
        let rest_srv = rest.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.set_nodelay(true);
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            // Chunked, not Content-Length: the first chunk has to reach
            // `Response::chunk` before the rest of the body exists, or the
            // client blocks and cancel can never be observed.
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            write!(stream, "{:X}\r\n", first_srv.len()).unwrap();
            stream.write_all(&first_srv).unwrap();
            stream.write_all(b"\r\n").unwrap();
            stream.flush().unwrap();
            while !go_srv.load(AtomicOrdering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = write!(stream, "{:X}\r\n", rest_srv.len());
            let _ = stream.write_all(&rest_srv);
            let _ = stream.write_all(b"\r\n0\r\n\r\n");
        });
        let dest = dir.join("slow");
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let url = format!("http://{addr}/slow");
        let err = block_on(async {
            let progress_task = Arc::clone(&progress);
            let total_task = Arc::clone(&total);
            let cancel_task = Arc::clone(&cancel);
            let dest_task = dest.clone();
            let task = tokio::spawn(async move {
                download(
                    &url,
                    &dest_task,
                    &sha256_hex(&first),
                    &progress_task,
                    &total_task,
                    &cancel_task,
                )
                .await
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while progress.load(AtomicOrdering::Relaxed) == 0 {
                if task.is_finished() || std::time::Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(
                progress.load(AtomicOrdering::Relaxed) > 0,
                "download made no progress before cancel"
            );
            cancel.store(true, AtomicOrdering::Relaxed);
            go.store(true, AtomicOrdering::Relaxed);
            task.await.expect("task")
        })
        .expect_err("cancel");
        assert_eq!(err, CANCELLED);
        assert!(!dest.exists());
        assert!(!PathBuf::from(format!("{}.part", dest.display())).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sim_copy_fallback_replaces_an_existing_binary() {
        let dir = temp_dir("copy");
        let src = dir.join("src");
        let dest_dir = dir.join("dest");
        std::fs::create_dir(&dest_dir).unwrap();
        std::fs::write(&src, b"new-bytes").unwrap();
        let dest = dest_dir.join("sonora");
        std::fs::write(&dest, b"old-bytes").unwrap();
        super::replace_file_by_copy(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new-bytes");
        assert!(
            src.exists(),
            "the download stays until a same-filesystem rename"
        );
        assert!(!dest_dir.join(".sonora.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sim_foreign_installs_do_not_exit_or_touch_the_package() {
        let dir = temp_dir("foreign");
        let pkg = dir.join("package");
        std::fs::write(&pkg, b"leave-me").unwrap();
        let kinds = [
            InstallKind::PackageManaged,
            InstallKind::Flatpak,
            InstallKind::NotifyOnly,
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Sonora.app"),
                writable: false,
            },
        ];
        for kind in kinds {
            let err = install_and_restart(&kind, &pkg).expect_err("no self update");
            assert!(!err.is_empty(), "{kind:?}");
            assert_eq!(std::fs::read(&pkg).unwrap(), b"leave-me");
        }
        // On Windows these arms spawn the helper and exit the process.
        #[cfg(not(windows))]
        {
            for kind in [
                InstallKind::WindowsNsis {
                    exe: PathBuf::from("sonora.exe"),
                },
                InstallKind::WindowsPortable {
                    exe: PathBuf::from("sonora.exe"),
                },
            ] {
                assert!(install_and_restart(&kind, &pkg).is_err());
                assert_eq!(std::fs::read(&pkg).unwrap(), b"leave-me");
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let err = install_and_restart(
                &InstallKind::MacApp {
                    bundle: PathBuf::from("/Applications/Sonora.app"),
                    writable: true,
                },
                &pkg,
            )
            .expect_err("ditto is macOS-only");
            assert!(err.contains("macOS"), "{err}");
            assert_eq!(std::fs::read(&pkg).unwrap(), b"leave-me");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sim_windows_wrapper_waits_then_installs_silently() {
        let installer = Path::new(r"C:\Users\A B\%TEMP%\Sonora-Setup-1.2.3.exe");
        let exe = Path::new(r"C:\Program Files\Sonora\sonora.exe");
        let script = windows_wrapper_script(4242, &nsis_install_body(installer, exe));
        assert!(script.contains("tasklist /FI \"PID eq 4242\""));
        assert!(script.contains("if not errorlevel 1"));
        assert!(script.contains("timeout /t 1 /nobreak >nul <nul"));
        assert!(
            script.contains(
                "start /wait \"\" \"C:\\Users\\A B\\%%TEMP%%\\Sonora-Setup-1.2.3.exe\" /S"
            ),
            "{script}"
        );
        assert!(script.contains("if errorlevel 1 exit /b 1"));
        assert!(script.contains("(goto) 2>nul & del \"%~f0\""));

        let body = portable_install_body(
            Path::new(r"D:\stage\inner"),
            Path::new(r"D:\apps\Sonora"),
            Path::new(r"D:\apps\Sonora\sonora.exe"),
        );
        assert!(body.contains("robocopy \"D:\\stage\\inner\" \"D:\\apps\\Sonora\""));
        assert!(body.contains("if errorlevel 8 exit /b 8"));
        assert!(body.contains("start \"\" \"D:\\apps\\Sonora\\sonora.exe\""));
        assert!(!body.contains("/S"));
    }

    #[cfg(unix)]
    fn shell_payload(marker: &Path) -> String {
        let quoted = marker.display().to_string().replace('\'', "'\\''");
        format!("#!/bin/sh\nprintf 'updated\\n' > '{quoted}'\n")
    }

    #[cfg(unix)]
    fn gnu_sha256_line(dir: &Path, name: &str) -> String {
        if let Ok(out) = std::process::Command::new("sha256sum")
            .arg("--")
            .arg(name)
            .current_dir(dir)
            .output()
        {
            if out.status.success() {
                return String::from_utf8(out.stdout).unwrap();
            }
        }
        let bytes = std::fs::read(dir.join(name)).unwrap();
        format!("{}  {name}\n", sha256_hex(&bytes))
    }

    /// The release workflow's tar.gz, then the real download + stage + the
    /// relaunch shell script (run as a child, so this process stays up).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sim_portable_archive_replaces_the_binary_and_relaunches() {
        let dir = temp_dir("portable");
        let version = "9.9.9";
        let installed = dir.join("sonora");
        std::fs::write(&installed, b"old-binary\n").unwrap();
        let marker = dir.join("marker");
        let cleanup = dir.join("Sonora.app.previous");
        std::fs::create_dir(&cleanup).unwrap();
        std::fs::write(cleanup.join("stale"), b"x").unwrap();

        let kind = InstallKind::Portable {
            exe: installed.clone(),
        };
        let archive_name =
            asset_filename(&kind, version, std::env::consts::ARCH, std::env::consts::OS).unwrap();
        let stage_name = archive_name.trim_end_matches(".tar.gz");
        let stage = dir.join(stage_name);
        std::fs::create_dir_all(&stage).unwrap();
        let payload = shell_payload(&marker);
        let bin = stage.join("sonora");
        std::fs::write(&bin, &payload).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::write(
            stage.join("sonora.desktop"),
            b"[Desktop Entry]\nName=Decoy\n",
        )
        .unwrap();
        let packed = std::process::Command::new("tar")
            .args(["-czf", &archive_name, stage_name])
            .current_dir(&dir)
            .status()
            .expect("tar");
        assert!(packed.success(), "tar -czf failed");
        let line = gnu_sha256_line(&dir, &archive_name);
        let parsed = parse_sha256sums(&line);
        assert_eq!(parsed.len(), 1, "{line}");
        assert_eq!(parsed[0].0, archive_name);
        let file_bytes = std::fs::read(dir.join(&archive_name)).unwrap();
        assert_eq!(
            parsed[0].1,
            sha256_hex(&file_bytes),
            "sha256sum output must match the hasher the updater uses"
        );

        let mut routes = HashMap::new();
        routes.insert(format!("/files/{archive_name}"), file_bytes);
        let base = serve(routes);
        let info = UpdateInfo {
            tag: format!("v{version}"),
            notes: String::new(),
            html_url: String::new(),
            assets: vec![UpdateAsset {
                name: archive_name.clone(),
                url: format!("{base}/redir/{archive_name}"),
                size: 0,
            }],
            checksums: vec![(archive_name.clone(), parsed[0].1.to_ascii_uppercase())],
        };
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let downloaded =
            block_on(download_for(&info, &kind, &progress, &total, &cancel)).expect("download");
        assert_eq!(
            progress.load(AtomicOrdering::Relaxed),
            total.load(AtomicOrdering::Relaxed)
        );
        assert!(total.load(AtomicOrdering::Relaxed) > 0);
        assert!(!PathBuf::from(format!("{}.part", downloaded.display())).exists());

        super::stage_portable(&downloaded, &installed).expect("stage");
        let installed_bytes = std::fs::read(&installed).unwrap();
        assert!(
            installed_bytes.starts_with(b"#!/bin/sh\n"),
            "installed the decoy desktop file or left the old binary: {installed_bytes:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
            assert!(
                mode & 0o111 != 0,
                "staged binary was not chmod +x: {mode:o}"
            );
        }

        let script = super::relaunch_script(&installed, Some(&cleanup), std::iter::empty());
        assert!(script.starts_with("sleep 1; "));
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .status()
            .unwrap();
        assert!(status.success(), "relaunch script failed: {script}");
        assert!(!cleanup.exists(), "previous bundle was not removed");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "updated\n");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&downloaded);
    }

    #[cfg(unix)]
    #[test]
    fn sim_appimage_replace_is_executable_and_relaunch_quotes_args() {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("appimage");
        let marker = dir.join("marker");
        let appimage = dir.join("Sonora.AppImage");
        std::fs::write(&appimage, b"old-appimage").unwrap();
        let payload = shell_payload(&marker);
        let expected = sha256_hex(payload.as_bytes()).to_ascii_uppercase();
        let mut routes = HashMap::new();
        routes.insert("/files/Sonora.AppImage".into(), payload.into_bytes());
        let base = serve(routes);
        let downloaded = dir.join("download");
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        block_on(download(
            &format!("{base}/redir/Sonora.AppImage"),
            &downloaded,
            &expected,
            &progress,
            &total,
            &cancel,
        ))
        .expect("appimage download");
        super::stage_appimage(&downloaded, &appimage).expect("stage appimage");
        let got = std::fs::read(&appimage).unwrap();
        assert!(got.starts_with(b"#!/bin/sh\n"), "{got:?}");
        let mode = std::fs::metadata(&appimage).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "AppImage was not marked executable: {mode:o}"
        );

        let quoted = super::relaunch_script(
            Path::new("/tmp/My Dir/sonora"),
            Some(Path::new("/tmp/it's old")),
            [OsString::from("a'b"), OsString::from("x y")],
        );
        assert!(quoted.starts_with("sleep 1; "));
        assert!(quoted.contains("rm -rf '/tmp/it'\\''s old'"));
        assert!(quoted.contains("exec '/tmp/My Dir/sonora' 'a'\\''b' 'x y'"));
        let syntax = std::process::Command::new("sh")
            .args(["-n", "-c", &quoted])
            .status()
            .unwrap();
        assert!(syntax.success(), "relaunch script does not parse: {quoted}");

        let script = super::relaunch_script(&appimage, None, std::iter::empty());
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "updated\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sim_archive_path_escape_does_not_replace_the_binary() {
        let dir = temp_dir("escape");
        let installed = dir.join("sonora");
        std::fs::write(&installed, b"old-binary").unwrap();
        // Extract dir is `<temp>/sonora-update/extract-<pid>`, so `../`
        // would land directly in `sonora-update/` if the check were missing.
        let leaked = super::work_dir()
            .unwrap()
            .join(format!("escape-{}", std::process::id()));
        let _ = std::fs::remove_file(&leaked);

        // `Header::set_path` refuses `..`, which is what a hostile archive
        // would ignore. Write the name into the header bytes directly.
        let mut header = tar::Header::new_gnu();
        header.set_path("safe").unwrap();
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        let name = format!("../escape-{}", std::process::id());
        let bytes = header.as_mut_bytes();
        bytes[..100].fill(0);
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        bytes[148..156].fill(b' ');
        let sum: u32 = bytes.iter().map(|b| u32::from(*b)).sum();
        let field = format!("{sum:06o}\0 ");
        bytes[148..156].copy_from_slice(field.as_bytes());
        let archive = dir.join("bad.tar.gz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            builder.append(&header, &b"nope"[..]).unwrap();
            let enc = builder.into_inner().unwrap();
            enc.finish().unwrap();
        }
        let err = super::stage_portable(&archive, &installed).expect_err("escape");
        assert!(
            err.contains("escapes") || err.contains("no sonora"),
            "{err}"
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"old-binary");
        assert!(
            !leaked.exists(),
            "archive entry was written outside the extract directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
