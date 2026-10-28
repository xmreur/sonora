//! Runs `tests/auth_page_behavior.js` under node against the real auth
//! page: guards the sign-in recovery paths — the expired-developer-token
//! refresh, and the forced one-shot retry (`/dev-token?force=1`) that
//! self-heals a MusicKit setup failure caused by a stale or poisoned
//! token cache (`MusicKit setup failed: …` used to be a dead end).
//! Skipped when node is not installed.

use std::process::Command;

#[test]
fn auth_page_recovers_from_an_expired_token() {
    let node = ["node", "nodejs"]
        .iter()
        .find(|bin| Command::new(bin).arg("--version").output().is_ok());
    let Some(node) = node else {
        eprintln!("skipped: node is not installed — run the suite manually instead");
        return;
    };

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let out = Command::new(node)
        .arg(dir.join("auth_page_behavior.js"))
        .arg(dir.join("../src/auth_page.html"))
        .output()
        .expect("run node");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "auth page behavior failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("all 6 scenarios OK"), "stdout: {stdout}");
}
