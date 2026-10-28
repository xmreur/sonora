//! Windows executable resources: icon + version metadata.
//!
//! eframe sets the *window* icon at runtime, but Explorer, shortcuts and
//! the NSIS installer read the icon from the PE resource section — that
//! one has to be compiled into `sonora.exe`. Non-Windows targets do
//! nothing here.

#[cfg(windows)]
fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("icons/icon.ico");
    res.set("ProductName", "Sonora");
    res.set("FileDescription", "Sonora - native Apple Music client");
    res.set("LegalCopyright", "Sonora contributors");
    res.set("OriginalFilename", "sonora.exe");
    let [major, minor, patch] = parse_version(env!("CARGO_PKG_VERSION"));
    let ver = (u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16);
    res.set_version_info(winresource::VersionInfo::WIN32, ver);
    res.compile().expect("failed to embed Windows resources");
}

#[cfg(not(windows))]
fn main() {}

/// `"0.1.8"` → `[0, 1, 8]`; junk parts become 0.
#[cfg(windows)]
fn parse_version(s: &str) -> [u16; 3] {
    let mut parts = s.split('.');
    let mut out = [0u16; 3];
    for slot in &mut out {
        *slot = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    }
    out
}
