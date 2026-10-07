//! In-process Widevine CDM driver + PSSH init-data builder.
//!
//! The CDM binary is loaded once per process (`open_system()` order:
//! `$SONORA_WIDEVINE_CDM` → runtime download → browser fallback). Content
//! keys stay sealed inside the CDM; Rust only shuttles challenge/license
//! bytes and per-sample ciphertext/plaintext.

use std::path::Path;

pub mod discover;
pub mod fetch;

#[cfg(not(target_os = "android"))]
use std::ffi::CString;
#[cfg(not(target_os = "android"))]
use std::os::raw::{c_char, c_int};
#[cfg(not(target_os = "android"))]
use std::sync::{Arc, OnceLock};

/// Widevine system id `edef8ba9-79d6-4ace-a3c8-27dcd51d21ed`.
pub const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];

#[cfg(not(target_os = "android"))]
unsafe extern "C" {
    fn wv_open(so_path: *const c_char) -> c_int;
    fn wv_challenge(
        init_data: *const u8,
        len: u32,
        out: *mut *mut u8,
        out_len: *mut u32,
        out_type: *mut u32,
        out_sid: *mut *mut u8,
        out_sid_len: *mut u32,
    ) -> c_int;
    fn wv_close(sid: *const u8, sid_len: u32) -> c_int;
    fn wv_update(sid: *const u8, sid_len: u32, license: *const u8, len: u32) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn wv_decrypt(
        data: *const u8,
        data_size: u32,
        key_id: *const u8,
        key_id_size: u32,
        iv: *const u8,
        iv_size: u32,
        subs: *const u32,
        num_subs: u32,
        out: *mut *mut u8,
        out_len: *mut u32,
    ) -> c_int;
    fn wv_free(p: *mut u8);
}

#[cfg(not(target_os = "android"))]
static LICENSE_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

#[cfg(not(target_os = "android"))]
fn license_lock() -> Arc<tokio::sync::Mutex<()>> {
    LICENSE_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(not(target_os = "android"))]
static CALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(not(target_os = "android"))]
fn call_lock() -> std::sync::MutexGuard<'static, ()> {
    CALL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(target_os = "android")]
pub struct LicenseSession;
#[cfg(not(target_os = "android"))]
pub struct LicenseSession(#[allow(dead_code)] tokio::sync::OwnedMutexGuard<()>);

#[cfg(target_os = "android")]
pub struct CdmSession;
#[cfg(not(target_os = "android"))]
pub struct CdmSession {
    id: Vec<u8>,
}

#[cfg(not(target_os = "android"))]
impl CdmSession {
    pub fn close(mut self) -> Result<(), String> {
        let id = std::mem::take(&mut self.id);
        close_session(&id)
    }
}

#[cfg(not(target_os = "android"))]
fn close_session(id: &[u8]) -> Result<(), String> {
    let _guard = call_lock();
    match unsafe { wv_close(id.as_ptr(), id.len() as u32) } {
        0 => Ok(()),
        40 => Err("no CDM loaded".to_string()),
        41 => Err("the CDM rejected the session close".to_string()),
        42 => Err("the CDM never confirmed the session closed".to_string()),
        n => Err(format!("closing the CDM session failed (code {n})")),
    }
}

#[cfg(not(target_os = "android"))]
impl Drop for CdmSession {
    fn drop(&mut self) {
        if self.id.is_empty() {
            return;
        }
        if let Err(e) = close_session(&self.id) {
            eprintln!("sonora widevine: {e}");
        }
    }
}

#[cfg(not(target_os = "android"))]
unsafe fn take(out: *mut u8, len: u32) -> Vec<u8> {
    if out.is_null() {
        return Vec::new();
    }
    let v = unsafe { std::slice::from_raw_parts(out, len as usize) }.to_vec();
    unsafe { wv_free(out) };
    v
}

pub struct Cdm {
    _private: (),
}

#[cfg(target_os = "android")]
impl Cdm {
    fn unsupported<T>() -> Result<T, String> {
        Err("Apple Music native playback isn't supported on Android yet".to_string())
    }
    pub async fn open(_path: impl AsRef<Path>) -> Result<Self, String> {
        Self::unsupported()
    }
    pub async fn open_system() -> Result<Self, String> {
        Self::unsupported()
    }
    pub async fn begin_license(&self) -> LicenseSession {
        LicenseSession
    }
    pub fn challenge(
        &self,
        _session: &LicenseSession,
        _pssh: &[u8],
    ) -> Result<(Vec<u8>, CdmSession), String> {
        Self::unsupported()
    }
    pub fn update(
        &self,
        _session: &LicenseSession,
        _cdm_session: &CdmSession,
        _license: &[u8],
    ) -> Result<(), String> {
        Self::unsupported()
    }
    pub fn decrypt(
        &self,
        _data: &[u8],
        _key_id: &[u8],
        _iv: &[u8],
        _subsamples: &[(u32, u32)],
    ) -> Result<Vec<u8>, String> {
        Self::unsupported()
    }
}

#[cfg(not(target_os = "android"))]
impl Cdm {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let s = path
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 CDM path: {}", path.display()))?;
        let c = CString::new(s).map_err(|e| format!("CDM path NUL: {e}"))?;
        let _guard = call_lock();
        match unsafe { wv_open(c.as_ptr()) } {
            0 => Ok(Self { _private: () }),
            1 => Err(format!(
                "couldn't load the Widevine CDM at {}",
                path.display()
            )),
            2 => Err(format!("{} isn't a Widevine CDM", path.display())),
            3 => Err("this Widevine CDM doesn't support CDM-11".to_string()),
            n => Err(format!("Widevine CDM init failed (code {n})")),
        }
    }

    /// `$SONORA_WIDEVINE_CDM` → downloaded copy (fetching if needed) →
    /// browser fallback.
    pub async fn open_system() -> Result<Self, String> {
        if let Some(path) = discover::override_cdm() {
            return Self::open(path).await;
        }
        match fetch::ensure().await {
            Ok(path) => return Self::open(path).await,
            Err(e) => eprintln!("sonora: CDM download failed ({e}) — trying browser copy"),
        }
        let path = discover::locate().ok_or_else(|| {
            "no Widevine CDM available — install Firefox/Chrome and play any DRM video once, or set $SONORA_WIDEVINE_CDM".to_string()
        })?;
        Self::open(path).await
    }

    pub async fn begin_license(&self) -> LicenseSession {
        LicenseSession(license_lock().lock_owned().await)
    }

    const LICENSE_REQUEST: u32 = 0;
    const INDIVIDUALIZATION_REQUEST: u32 = 3;

    pub fn challenge(
        &self,
        _session: &LicenseSession,
        pssh_box: &[u8],
    ) -> Result<(Vec<u8>, CdmSession), String> {
        let _guard = call_lock();
        let mut out = std::ptr::null_mut();
        let mut len = 0u32;
        let mut msg_type = 0u32;
        let mut sid = std::ptr::null_mut();
        let mut sid_len = 0u32;
        match unsafe {
            wv_challenge(
                pssh_box.as_ptr(),
                pssh_box.len() as u32,
                &mut out,
                &mut len,
                &mut msg_type,
                &mut sid,
                &mut sid_len,
            )
        } {
            0 => {
                let message = unsafe { take(out, len) };
                let session = CdmSession {
                    id: unsafe { take(sid, sid_len) },
                };
                match msg_type {
                    Self::LICENSE_REQUEST => Ok((message, session)),
                    Self::INDIVIDUALIZATION_REQUEST => Err(
                        "CDM has no device certificate — play any DRM video in the browser the CDM came from, then retry".to_string(),
                    ),
                    other => Err(format!("CDM returned message type {other}")),
                }
            }
            11 => Err("the CDM rejected the pssh box".to_string()),
            12 => Err("the CDM produced no license challenge".to_string()),
            n => Err(format!("license challenge failed (code {n})")),
        }
    }

    pub fn update(
        &self,
        _session: &LicenseSession,
        cdm_session: &CdmSession,
        license: &[u8],
    ) -> Result<(), String> {
        let _guard = call_lock();
        match unsafe {
            wv_update(
                cdm_session.id.as_ptr(),
                cdm_session.id.len() as u32,
                license.as_ptr(),
                license.len() as u32,
            )
        } {
            0 => Ok(()),
            21 => Err("the CDM rejected the license response".to_string()),
            22 => Err("the license carried no usable key".to_string()),
            n => Err(format!("loading the license failed (code {n})")),
        }
    }

    pub fn decrypt(
        &self,
        data: &[u8],
        key_id: &[u8],
        iv: &[u8],
        subsamples: &[(u32, u32)],
    ) -> Result<Vec<u8>, String> {
        let mut subs = Vec::with_capacity(subsamples.len() * 2);
        for &(clear, encrypted) in subsamples {
            subs.push(clear);
            subs.push(encrypted);
        }
        let _guard = call_lock();
        let mut out = std::ptr::null_mut();
        let mut len = 0u32;
        let rc = unsafe {
            wv_decrypt(
                data.as_ptr(),
                data.len() as u32,
                key_id.as_ptr(),
                key_id.len() as u32,
                iv.as_ptr(),
                iv.len() as u32,
                subs.as_ptr(),
                subsamples.len() as u32,
                &mut out,
                &mut len,
            )
        };
        match rc {
            0 => Ok(unsafe { take(out, len) }),
            32 => Err("no key for this track".to_string()),
            n => Err(format!("CENC decrypt failed (code {n})")),
        }
    }
}

/// Minimal `WidevineCencHeader` protobuf (algorithm=AESCTR + key_id).
fn widevine_cenc_header(key_id: &[u8]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    let mut out = Vec::with_capacity(key_id.len() + 10);
    out.extend_from_slice(&[0x08, 0x01]);
    out.push(0x12);
    varint(&mut out, key_id.len() as u64);
    out.extend_from_slice(key_id);
    out.extend_from_slice(&[0x1a, 0x00, 0x32, 0x00]);
    out
}

/// CENC PSSH box for `key_id` — the init-data the CDM expects.
pub fn build_pssh(key_id: &[u8]) -> Vec<u8> {
    let payload = widevine_cenc_header(key_id);
    let total = 4 + 4 + 4 + 16 + 4 + payload.len();
    let mut b = Vec::with_capacity(total);
    b.extend_from_slice(&(total as u32).to_be_bytes());
    b.extend_from_slice(b"pssh");
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&WIDEVINE_SYSTEM_ID);
    b.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    b.extend_from_slice(&payload);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pssh_well_formed() {
        let kid = [0xAAu8; 16];
        let b = build_pssh(&kid);
        let declared = u32::from_be_bytes(b[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared, b.len());
        assert_eq!(&b[4..8], b"pssh");
        assert_eq!(&b[12..28], &WIDEVINE_SYSTEM_ID);
        assert!(b[32..].windows(16).any(|w| w == kid));
    }
}
