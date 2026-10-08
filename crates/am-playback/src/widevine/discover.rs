//! Locating a system Widevine CDM borrowed from an installed browser.
//!
//! The CDM is Google's proprietary binary and is never shipped with this app.
//! Two layouts exist in the wild:
//! * Firefox family — GMP plugin at `<profile>/gmp-widevinecdm/<version>/`
//! * Chromium family — `<root>/WidevineCdm/<version>/_platform_specific/<plat>/`
//!
//! `$SONORA_WIDEVINE_CDM` (file or directory) overrides everything;
//! `$KOPUZ_WIDEVINE_CDM` is honored as a migration fallback.

use std::path::{Path, PathBuf};

pub(crate) fn cdm_file_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "widevinecdm.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "libwidevinecdm.dylib"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "libwidevinecdm.so"
    }
}

const MAX_DEPTH: usize = 4;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let home = home();

    #[cfg(target_os = "linux")]
    if let Some(h) = &home {
        for dir in [
            ".mozilla/firefox",
            ".floorp",
            ".librewolf",
            ".waterfox",
            ".zen",
        ] {
            roots.push(h.join(dir));
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(h) = &home {
        for dir in [
            "Library/Application Support/Firefox/Profiles",
            "Library/Application Support/LibreWolf/Profiles",
            "Library/Application Support/Waterfox/Profiles",
            "Library/Application Support/zen/Profiles",
        ] {
            roots.push(h.join(dir));
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = env_dir("APPDATA") {
        for dir in [
            "Mozilla/Firefox/Profiles",
            "librewolf/Profiles",
            "zen/Profiles",
        ] {
            roots.push(appdata.join(dir));
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(h) = &home {
            for dir in [
                ".config/google-chrome/WidevineCdm",
                ".config/chromium/WidevineCdm",
                ".config/BraveSoftware/Brave-Browser/WidevineCdm",
                ".config/vivaldi/WidevineCdm",
                ".config/microsoft-edge/WidevineCdm",
                ".config/opera/WidevineCdm",
            ] {
                roots.push(h.join(dir));
            }
        }
        for dir in [
            "/opt/google/chrome/WidevineCdm",
            "/opt/brave.com/brave/WidevineCdm",
            "/opt/vivaldi/WidevineCdm",
            "/usr/lib/chromium/WidevineCdm",
            "/usr/lib/chromium-browser/WidevineCdm",
        ] {
            roots.push(PathBuf::from(dir));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = &home {
            for dir in [
                "Library/Application Support/Google/Chrome/WidevineCdm",
                "Library/Application Support/BraveSoftware/Brave-Browser/WidevineCdm",
                "Library/Application Support/Chromium/WidevineCdm",
            ] {
                roots.push(h.join(dir));
            }
        }
        for dir in [
            "/Applications/Google Chrome.app/Contents/Frameworks",
            "/Applications/Brave Browser.app/Contents/Frameworks",
        ] {
            roots.push(PathBuf::from(dir));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = env_dir("LOCALAPPDATA") {
            for dir in [
                "Google/Chrome/User Data/WidevineCdm",
                "BraveSoftware/Brave-Browser/User Data/WidevineCdm",
                "Chromium/User Data/WidevineCdm",
                "Microsoft/Edge/User Data/WidevineCdm",
            ] {
                roots.push(local.join(dir));
            }
        }
        for key in ["PROGRAMFILES", "PROGRAMFILES(X86)"] {
            if let Some(pf) = env_dir(key) {
                roots.push(pf.join("Google/Chrome/Application"));
                roots.push(pf.join("Microsoft/Edge/Application"));
            }
        }
    }

    let _ = &home;
    roots
}

fn search_under(dir: &Path, name: &str, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => search_under(&path, name, depth + 1, found),
            _ if entry.file_name() == name => found.push(path),
            _ => {}
        }
    }
}

/// Numeric version key from a version-looking path component, so
/// `4.10.3050.0` sorts above `4.10.999.0` (plain string sort would not).
pub(crate) fn version_key(path: &Path) -> Vec<u64> {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .find(|s| {
            s.split('.').count() >= 3
                && s.split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|s| s.split('.').filter_map(|p| p.parse().ok()).collect())
        .unwrap_or_default()
}

fn override_from(key: &str) -> Option<PathBuf> {
    let override_path = env_dir(key)?;
    if override_path.is_file() {
        return Some(override_path);
    }
    let mut found = Vec::new();
    search_under(&override_path, cdm_file_name(), 0, &mut found);
    found.sort_by_key(|p| version_key(p));
    found.pop()
}

/// Explicit CDM path: `$SONORA_WIDEVINE_CDM`, else `$KOPUZ_WIDEVINE_CDM`.
pub fn override_cdm() -> Option<PathBuf> {
    override_from("SONORA_WIDEVINE_CDM").or_else(|| override_from("KOPUZ_WIDEVINE_CDM"))
}

pub fn locate() -> Option<PathBuf> {
    if let Some(path) = override_cdm() {
        return Some(path);
    }
    let name = cdm_file_name();
    for root in search_roots() {
        if !root.is_dir() {
            continue;
        }
        let mut found = Vec::new();
        search_under(&root, name, 0, &mut found);
        found.sort_by_key(|p| version_key(p));
        if let Some(best) = found.pop() {
            return Some(best);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_matches_platform() {
        let name = cdm_file_name();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "widevinecdm.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libwidevinecdm.dylib");
        } else {
            assert_eq!(name, "libwidevinecdm.so");
        }
    }

    #[test]
    fn version_orders_numerically() {
        let older = Path::new("/x/gmp-widevinecdm/4.10.999.0/libwidevinecdm.so");
        let newer = Path::new("/x/gmp-widevinecdm/4.10.3050.0/libwidevinecdm.so");
        assert!(version_key(newer) > version_key(older));
    }

    #[test]
    fn finds_both_layouts() {
        let tmp = std::env::temp_dir().join(format!("sonora-cdm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let name = cdm_file_name();
        let ff = tmp.join("profile/gmp-widevinecdm/4.10.3050.0");
        let cr = tmp.join("WidevineCdm/4.10.2891.0/_platform_specific/linux_x64");
        std::fs::create_dir_all(&ff).unwrap();
        std::fs::create_dir_all(&cr).unwrap();
        std::fs::write(ff.join(name), b"x").unwrap();
        std::fs::write(cr.join(name), b"x").unwrap();
        let mut found = Vec::new();
        search_under(&tmp, name, 0, &mut found);
        assert_eq!(found.len(), 2);
        found.sort_by_key(|p| version_key(p));
        assert_eq!(found.last().unwrap(), &ff.join(name));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
