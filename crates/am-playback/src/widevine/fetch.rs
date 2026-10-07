//! Runtime Widevine CDM download via Mozilla's GMP update service.
//!
//! Google only serves the standalone CDM package for Linux; Mozilla's GMP
//! endpoint covers Linux/macOS/Windows from Google's CDN and ships a sha512
//! so the download is verified before anything is `dlopen`ed.
//!
//! `ensure()` = reuse installed → resolve manifest → download → verify →
//! unpack CRX3 → install under `<config>/sonora/widevine/<version>/`.

use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdmRelease {
    pub version: String,
    pub url: String,
    pub sha512: String,
    pub size: u64,
}

/// Mozilla platform key for this host, or `None` where no CDM is published
/// (ARM Linux, 32-bit Linux).
pub fn gmp_platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "Linux_x86_64-gcc3",
        ("macos", "aarch64") => "Darwin_aarch64-gcc3",
        ("macos", "x86_64") => "Darwin_x86_64-gcc3-u-i386-x86_64",
        ("windows", "x86_64") => "WINNT_x86_64-msvc-x64",
        ("windows", "aarch64") => "WINNT_aarch64-msvc-aarch64",
        _ => return None,
    })
}

pub fn manifest_url(platform: &str) -> String {
    format!(
        "https://aus5.mozilla.org/update/3/GMP/140.0/20250801000000/{platform}/en-US/release/default/default/default/update.xml"
    )
}

/// Pull the `gmp-widevinecdm` entry out of a GMP manifest.
pub fn parse_manifest(xml: &str) -> Result<CdmRelease, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("parse GMP manifest: {e}"))?;
        match event {
            Event::Empty(ref e) | Event::Start(ref e) if e.name().as_ref() == b"addon" => {
                let attr = |key: &[u8]| -> Option<String> {
                    e.attributes().flatten().find_map(|a| {
                        (a.key.as_ref() == key)
                            .then(|| String::from_utf8_lossy(a.value.as_ref()).into_owned())
                    })
                };
                if attr(b"id").as_deref() != Some("gmp-widevinecdm") {
                    buf.clear();
                    continue;
                }
                let hash_fn = attr(b"hashFunction").unwrap_or_default();
                if hash_fn != "sha512" {
                    return Err(format!(
                        "unexpected hash function {hash_fn:?}, wanted sha512"
                    ));
                }
                return Ok(CdmRelease {
                    version: attr(b"version").ok_or("CDM entry has no version")?,
                    url: attr(b"URL").ok_or("CDM entry has no URL")?,
                    sha512: attr(b"hashValue")
                        .ok_or("CDM entry has no hash")?
                        .to_ascii_lowercase(),
                    size: attr(b"size")
                        .ok_or("CDM entry has no size")?
                        .parse()
                        .map_err(|e| format!("CDM entry size: {e}"))?,
                });
            }
            Event::Eof => return Err("no gmp-widevinecdm in the manifest".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

/// Library path inside the extracted CRX for this host.
pub fn archive_member() -> Option<String> {
    let (dir, name) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("linux_x64", "libwidevinecdm.so"),
        ("macos", "aarch64") => ("mac_arm64", "libwidevinecdm.dylib"),
        ("macos", "x86_64") => ("mac_x64", "libwidevinecdm.dylib"),
        ("windows", "x86_64") => ("win_x64", "widevinecdm.dll"),
        ("windows", "aarch64") => ("win_arm64", "widevinecdm.dll"),
        _ => return None,
    };
    Some(format!("_platform_specific/{dir}/{name}"))
}

pub async fn resolve() -> Result<CdmRelease, String> {
    let platform = gmp_platform().ok_or_else(|| {
        format!(
            "no Widevine CDM is published for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let xml = reqwest::Client::new()
        .get(manifest_url(platform))
        .send()
        .await
        .map_err(|e| format!("fetch GMP manifest: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read GMP manifest: {e}"))?;
    parse_manifest(&xml)
}

/// `<config>/sonora/widevine` (local app data on Windows).
pub fn install_root() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "sonora", "sonora").map(|d| {
        #[cfg(target_os = "windows")]
        let base = d.data_local_dir();
        #[cfg(not(target_os = "windows"))]
        let base = d.config_dir();
        base.join("widevine")
    })
}

/// Newest already-downloaded CDM, if any. No network.
pub fn installed() -> Option<std::path::PathBuf> {
    let name = super::discover::cdm_file_name();
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(install_root()?)
        .ok()?
        .flatten()
        .map(|e| e.path().join(name))
        .filter(|p| p.is_file())
        .collect();
    found.sort_by_key(|p| super::discover::version_key(p));
    found.pop()
}

static DOWNLOAD_LOCK: OnceLock<std::sync::Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

pub async fn ensure() -> Result<std::path::PathBuf, String> {
    if let Some(path) = installed() {
        return Ok(path);
    }
    let lock = DOWNLOAD_LOCK
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    if let Some(path) = installed() {
        return Ok(path);
    }
    install(&resolve().await?).await
}

async fn install(release: &CdmRelease) -> Result<std::path::PathBuf, String> {
    let root = install_root().ok_or("no config directory to install a CDM into")?;
    let bytes = reqwest::Client::new()
        .get(&release.url)
        .send()
        .await
        .map_err(|e| format!("download CDM: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download CDM: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("read CDM download: {e}"))?;
    if bytes.len() as u64 != release.size {
        return Err(format!(
            "CDM download is {} bytes, manifest says {}",
            bytes.len(),
            release.size
        ));
    }
    verify_sha512(&bytes, &release.sha512)?;
    let staging = root.join(format!("{}.part", release.version));
    let final_dir = root.join(&release.version);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("create {}: {e}", staging.display()))?;
    let library = extract_cdm(&bytes, &staging).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&staging, &final_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        format!("install CDM to {}: {e}", final_dir.display())
    })?;
    Ok(final_dir.join(library.file_name().ok_or("extracted CDM has no name")?))
}

fn verify_sha512(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha512};
    let mut s = String::new();
    for b in Sha512::digest(bytes) {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    if s != expected {
        return Err("CDM download failed its checksum".to_string());
    }
    Ok(())
}

/// Offset of the zip inside a CRX3 (12-byte header + protobuf block).
pub fn crx3_zip_offset(bytes: &[u8]) -> Result<usize, String> {
    if bytes.len() < 16 {
        return Err("CRX is truncated".to_string());
    }
    if &bytes[..4] != b"Cr24" {
        return Err("not a CRX archive".to_string());
    }
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if version != 3 {
        return Err(format!("unsupported CRX version {version}"));
    }
    let header_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    12usize
        .checked_add(header_len)
        .filter(|o| *o <= bytes.len())
        .ok_or("CRX header length runs past the end of the file".to_string())
}

/// Sub-package dir for this host from the component `manifest.json`.
pub fn sub_package_path(manifest_json: &str) -> Option<String> {
    let (os, arch) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", a) => ("linux", a),
        ("macos", a) => ("mac", a),
        ("windows", a) => ("win", a),
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other,
    };
    let manifest: serde_json::Value = serde_json::from_str(manifest_json).ok()?;
    manifest["platforms"]
        .as_array()?
        .iter()
        .find(|p| p["os"].as_str() == Some(os) && p["arch"].as_str() == Some(arch))
        .and_then(|p| p["sub_package_path"].as_str())
        .map(|s| s.trim_start_matches('/').to_string())
}

fn extract_cdm(crx: &[u8], dest: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let offset = crx3_zip_offset(crx)?;
    let cursor = std::io::Cursor::new(&crx[offset..]);
    let mut zip = zip::ZipArchive::new(cursor).map_err(|e| format!("open CRX zip: {e}"))?;
    let wanted_dir = zip
        .index_for_name("manifest.json")
        .and_then(|i| {
            let mut f = zip.by_index(i).ok()?;
            let mut text = String::new();
            std::io::Read::read_to_string(&mut f, &mut text).ok()?;
            sub_package_path(&text)
        })
        .or_else(|| {
            archive_member().and_then(|m| m.rsplit_once('/').map(|(dir, _)| format!("{dir}/")))
        })
        .ok_or("no sub-package for this platform in the CRX manifest")?;
    let name = super::discover::cdm_file_name();
    let member = format!("{wanted_dir}{name}");
    let index = zip
        .index_for_name(&member)
        .ok_or_else(|| format!("{member} is not in the CRX"))?;
    let mut file = zip
        .by_index(index)
        .map_err(|e| format!("read {member} from CRX: {e}"))?;
    let out_path = dest.join(name);
    let mut out = std::fs::File::create(&out_path)
        .map_err(|e| format!("create {}: {e}", out_path.display()))?;
    std::io::copy(&mut file, &mut out).map_err(|e| format!("write {}: {e}", out_path.display()))?;
    drop(out);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"<?xml version="1.0"?>
<updates>
    <addons>
        <addon id="gmp-gmpopenh264" URL="https://example.com/openh264.zip" hashFunction="sha512" hashValue="aa" size="475261" version="2.6.0"/>
        <addon id="gmp-widevinecdm" URL="https://example.com/widevine.crx3" hashFunction="sha512" hashValue="5ADE9A40703C835026D26DC660CF5793A03E275229438F7FF7154116A33CE595D1EE99A79F6F0579231BDC9F51E72363D35C958F02ACE911A7C47FB260402560" size="20189918" version="4.10.3050.0"/>
    </addons>
</updates>"#;

    #[test]
    fn picks_cdm_entry() {
        let r = parse_manifest(MANIFEST).unwrap();
        assert_eq!(r.version, "4.10.3050.0");
        assert!(r.url.ends_with(".crx3"));
        assert_eq!(r.sha512.len(), 128);
        assert!(r.sha512.starts_with("5ade9a40"));
    }

    #[test]
    fn rejects_wrong_hash_fn() {
        let xml = MANIFEST.replace("hashFunction=\"sha512\"", "hashFunction=\"md5\"");
        assert!(parse_manifest(&xml).is_err());
    }

    #[test]
    fn crx_offsets() {
        let mut crx = b"Cr24".to_vec();
        crx.extend_from_slice(&3u32.to_le_bytes());
        crx.extend_from_slice(&5u32.to_le_bytes());
        crx.extend_from_slice(&[0xAA; 5]);
        crx.extend_from_slice(b"PK\x03\x04rest");
        assert_eq!(crx3_zip_offset(&crx).unwrap(), 17);
        assert!(crx3_zip_offset(b"PK\x03\x04").is_err());
        let mut bad = b"Cr24".to_vec();
        bad.extend_from_slice(&2u32.to_le_bytes());
        bad.extend_from_slice(&0u32.to_le_bytes());
        assert!(crx3_zip_offset(&bad).is_err());
    }

    #[test]
    fn checksum_gate() {
        use sha2::{Digest, Sha512};
        let mut s = String::new();
        for b in Sha512::digest(b"sonora") {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        assert!(verify_sha512(b"sonora", &s).is_ok());
        assert!(verify_sha512(b"sonorA", &s).is_err());
    }
}
