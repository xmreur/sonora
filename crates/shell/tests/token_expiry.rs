//! The bug this guards: an expired developer JWT sitting in
//! `web_token_cache` used to be handed out forever — it blocked the
//! one-shot sign-in page (`MusicKit setup failed: Initialized with an
//! expired token.`) and turned catalog calls into `401 Unauthorized`
//! until the user deleted the file by hand.

use shell::commands::{
    classify_auth_failure, resolve_developer_token, web_token_cache_path, AuthFailure,
};

/// base64url payloads (no padding) + a signature-length filler: only
/// `exp` matters here, we never verify anything client-side.
const HEADER: &str = "eyJhbGciOiJFUzI1NiJ9"; // {"alg":"ES256"}
const EXPIRED_PAYLOAD: &str = "eyJleHAiOiAxfQ"; // {"exp": 1}
const FRESH_PAYLOAD: &str = "eyJleHAiOiA0MTAyNDQ0ODAwfQ"; // {"exp": 4102444800} (year 2100)

fn fake_jwt(payload: &str) -> String {
    format!("{HEADER}.{payload}.{}", "S".repeat(120))
}

/// Point the config dir at a temp dir. `None` → skip the test: on Linux
/// `init_config_dir` migrates a legacy install, which pkill's its Firefox
/// profile — never acceptable as a test side effect.
fn setup() -> Option<std::path::PathBuf> {
    let legacy = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config/apple-music-linux");
    if legacy.exists() {
        eprintln!(
            "skipped: legacy config present at {} (would migrate)",
            legacy.display()
        );
        return None;
    }
    let dir = std::env::temp_dir().join(format!("sonora-token-expiry-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    shell::paths::init_config_dir(dir.clone());
    // The official key bypasses the cache entirely; keep it out of the way.
    std::env::remove_var("APPLE_MUSIC_DEVELOPER_TOKEN");
    Some(dir)
}

#[test]
fn expired_web_token_cache_is_reused_never() {
    let Some(dir) = setup() else { return };
    let cache = web_token_cache_path().expect("config dir resolves");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // Fresh file, cold state: returned as-is (no network round-trip).
    let fresh = fake_jwt(FRESH_PAYLOAD);
    std::fs::write(&cache, &fresh).expect("seed fresh cache");
    let state = shell::AppState::new();
    let got = rt
        .block_on(resolve_developer_token(&state))
        .expect("fresh cached token must resolve");
    assert_eq!(got, fresh);

    // Expired file, cold state (memory is empty again after a restart):
    // the stale JWT must never come back — either a freshly scraped
    // token replaces it, or the failure is reported. Both are correct;
    // reusing the dead token is not.
    let expired = fake_jwt(EXPIRED_PAYLOAD);
    std::fs::write(&cache, &expired).expect("seed expired cache");
    let state = shell::AppState::new();
    let got = rt.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            resolve_developer_token(&state),
        )
        .await
    });
    match got {
        Ok(Ok(t)) => {
            assert_ne!(t, expired, "expired cached token was reused");
            assert!(
                apple_music_core::token::token_fresh_for(&t, 0),
                "replacement token must be valid"
            );
            let on_disk = std::fs::read_to_string(&cache).unwrap_or_default();
            assert_ne!(
                on_disk.trim(),
                expired.trim(),
                "stale cache file not replaced"
            );
        }
        // Offline / Apple changed the layout: reporting the failure is the
        // correct outcome — it proves the dead token was dropped, not used.
        Ok(Err(_)) => {}
        Err(_) => panic!("resolve_developer_token timed out"),
    }

    // Classification side of the same problem: an expired cache means
    // "refresh and retry", a healthy one means "the MUT is the problem".
    std::fs::write(&cache, &expired).expect("reseed expired cache");
    let state = shell::AppState::new();
    assert_eq!(
        classify_auth_failure(&state),
        AuthFailure::DevTokenRefreshed,
        "expired cache must classify as auto-fixable"
    );
    assert!(
        !cache.exists(),
        "classifying an expired cache must drop it for re-fetch"
    );

    std::fs::write(&cache, &fresh).expect("reseed fresh cache");
    let state = shell::AppState::new();
    assert_eq!(
        classify_auth_failure(&state),
        AuthFailure::SessionExpired,
        "healthy dev token + 401 means Apple rejected the MUT"
    );
    assert!(cache.exists(), "a healthy token must not be dropped");

    let _ = std::fs::remove_dir_all(&dir);
}
