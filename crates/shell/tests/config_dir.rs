//! Regression guard for the config-dir pollution bug: `init_config_dir`
//! once ignored its argument on Linux and always resolved to the real
//! `~/.config/sonora`, so the token-expiry test's "temp dir" writes
//! landed in the developer's live config — overwriting a real
//! `web_token_cache`/MUT with synthetic tokens. Explicit dirs must be
//! used verbatim on every platform, and no migration (pkill side
//! effects!) may run for them — that is app-startup only.
//!
//! This is an integration test (own process) on purpose: `CONFIG_DIR` is
//! a `OnceLock`, and this file never shares a process with other tests.

#[test]
fn explicit_config_dir_is_used_verbatim() {
    let dir = std::env::temp_dir().join(format!("sonora-explicit-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    shell::paths::init_config_dir(dir.clone());
    assert_eq!(
        shell::paths::app_config_dir().as_deref(),
        Some(dir.as_path()),
        "an explicit config dir must be honored exactly (no canonicalization)"
    );

    // Nothing may be created or migrated inside it by init itself.
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert!(
        entries.is_empty(),
        "init must not write into an explicit dir"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn token_files_resolve_under_the_explicit_dir() {
    let dir = std::env::temp_dir().join(format!("sonora-explicit-tokens-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // First init in this process wins; if the other test ran first this
    // is a no-op and the assertion below tells us which dir is active.
    shell::paths::init_config_dir(dir.clone());
    if shell::paths::app_config_dir().as_deref() != Some(dir.as_path()) {
        // Another test initialized first — its dir is in effect for the
        // whole process. Only check containment, never the real config.
        let active = shell::paths::app_config_dir().unwrap();
        assert!(
            active.starts_with(std::env::temp_dir()),
            "active config dir must be a temp dir in tests, got {}",
            active.display()
        );
        return;
    }
    for path in [
        shell::commands::web_token_cache_path(),
        shell::commands::mut_cache_path(),
    ] {
        let path = path.expect("config dir resolves");
        assert!(
            path.starts_with(&dir),
            "{} must live under the explicit dir",
            path.display()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
