//! Unix sidecar process groups and profile-tree cleanup.

use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn configure_firefox_cmd(cmd: &mut Command) {
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
            Ok(())
        });
    }
}

pub fn needle_running(needle: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", needle])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn pgids_for_needle(needle: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let pids = Command::new("pgrep")
        .args(["-f", needle])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for pid in pids.split_whitespace() {
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let pgid = Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if let Ok(pgid) = pgid.parse::<u32>() {
            if pgid != 0 && !out.contains(&pgid) {
                out.push(pgid);
            }
        }
    }
    out
}

pub fn kill_profile_trees(needles: &[String]) {
    for needle in needles {
        for pgid in pgids_for_needle(needle) {
            let _ = Command::new("pkill")
                .args(["-9", "-g", pgid.to_string().as_str()])
                .status();
        }
        let _ = Command::new("pkill")
            .args(["-9", "-f", needle.as_str()])
            .status();
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
}

pub fn kill_process_group(pgid: u32) {
    let _ = Command::new("pkill")
        .args(["-9", "-g", pgid.to_string().as_str()])
        .status();
}
