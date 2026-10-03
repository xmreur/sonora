//! Windows Job Objects for sidecar cleanup and profile-scoped taskkill.

use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;

/// Job handle stored as isize so [`SidecarManager`] stays `Send` across Tauri tasks.
pub struct SidecarJob(isize);

// HANDLE is not Send; the raw value is fine to move between threads.
unsafe impl Send for SidecarJob {}

impl SidecarJob {
    pub fn assign_child(child: &std::process::Child) -> Option<Self> {
        let job = unsafe { windows::Win32::System::JobObjects::CreateJobObjectW(None, None).ok()? };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok.is_err() {
            let _ = CloseHandle(job);
            return None;
        }
        let proc = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, child.id()) {
            Ok(h) => h,
            Err(_) => {
                let _ = CloseHandle(job);
                return None;
            }
        };
        if AssignProcessToJobObject(job, proc).is_err() {
            let _ = CloseHandle(proc);
            let _ = CloseHandle(job);
            return None;
        }
        let _ = CloseHandle(proc);
        Some(Self(job.0 as isize))
    }
}

impl Drop for SidecarJob {
    fn drop(&mut self) {
        let _ = CloseHandle(HANDLE(self.0 as *mut std::ffi::c_void));
    }
}

pub fn configure_firefox_cmd(cmd: &mut Command) {
    cmd.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
}

pub fn needle_running(needle: &str) -> bool {
    !pids_for_needle(needle).is_empty()
}

pub fn pgids_for_needle(_needle: &str) -> Vec<u32> {
    Vec::new()
}

pub fn kill_profile_trees(needles: &[String]) {
    for needle in needles {
        for pid in pids_for_needle(needle) {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", pid.to_string().as_str()])
                .status();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
}

pub fn kill_process_group(_pgid: u32) {}

fn pids_for_needle(needle: &str) -> Vec<u32> {
    let escaped = needle.replace('\'', "''");
    let script = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{escaped}*' }} | Select-Object -ExpandProperty ProcessId"
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .filter(|&p| p != std::process::id())
            .collect(),
        Err(_) => Vec::new(),
    }
}
