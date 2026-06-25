//! Windows implementation of the platform lifecycle layer.
//!
//! Uses windows-sys for process management: liveness check via
//! OpenProcess+WaitForSingleObject, termination via TerminateProcess,
//! and detached daemon spawning via CommandExt::creation_flags.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use super::{Liveness, Signal};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER,
    WAIT_OBJECT_0, FALSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject,
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
};

// Standard access rights; use raw values to avoid feature-gate issues.
const PROCESS_TERMINATE: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x00100000;

pub fn is_process_alive(pid: u32) -> Liveness {
    if pid == 0 {
        return Liveness::NotRunning;
    }

    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, FALSE, pid);
        if handle.is_null() {
            let err = GetLastError();
            match err {
                // PID does not exist
                ERROR_INVALID_PARAMETER => Liveness::NotRunning,
                // PID exists but we cannot access it — do not treat as dead
                ERROR_ACCESS_DENIED => Liveness::Unsupported,
                _ => Liveness::Unsupported,
            }
        } else {
            let ret = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            match ret {
                // Process has exited
                WAIT_OBJECT_0 => Liveness::NotRunning,
                // Process still running (or unexpected — treat as running)
                _ => Liveness::Running,
            }
        }
    }
}

pub fn send_signal(pid: u32, signal: Signal) -> anyhow::Result<()> {
    match signal {
        Signal::Terminate => {
            unsafe {
                let handle = OpenProcess(PROCESS_TERMINATE, FALSE, pid);
                if handle.is_null() {
                    let err = GetLastError();
                    anyhow::bail!(
                        "Failed to open process {} for termination (error code {})",
                        pid, err
                    );
                }
                let result = TerminateProcess(handle, 1);
                let term_err = if result == 0 { Some(GetLastError()) } else { None };
                CloseHandle(handle);

                if let Some(err) = term_err {
                    anyhow::bail!(
                        "Failed to terminate process {} (error code {})",
                        pid, err
                    );
                }

                // Wait briefly for the process to actually exit
                let handle = OpenProcess(SYNCHRONIZE, FALSE, pid);
                if !handle.is_null() {
                    WaitForSingleObject(handle, 5000); // 5s grace
                    CloseHandle(handle);
                }
            }
            Ok(())
        }
        Signal::Reload => {
            anyhow::bail!("Reload signal is not supported on Windows")
        }
    }
}

pub fn start_daemon<F>(data_dir: &Path, _run: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let pid_path = data_dir.join("blackbox.pid");

    let mut child = Command::new(&exe)
        .arg("run-foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(5);

    loop {
        match child.try_wait()? {
            Some(status) => {
                anyhow::bail!(
                    "Daemon process exited early with status: {}",
                    status
                );
            }
            None => {
                if pid_path.exists() {
                    // Child is alive and PID file has been written
                    return Ok(());
                }
            }
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "Daemon process did not write PID file within {} seconds",
                timeout.as_secs()
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub fn register_reload_flag(_flag: Arc<AtomicBool>) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_reports_running() {
        let pid = std::process::id();
        assert_eq!(
            is_process_alive(pid),
            Liveness::Running,
            "current process PID {} should report Running",
            pid
        );
    }

    #[test]
    fn nonexistent_pid_reports_not_running() {
        // Use a very high PID that almost certainly doesn't exist
        assert_eq!(
            is_process_alive(99999999),
            Liveness::NotRunning,
            "nonexistent PID should report NotRunning"
        );
    }

    #[test]
    fn zero_pid_reports_not_running() {
        assert_eq!(is_process_alive(0), Liveness::NotRunning);
    }

    #[test]
    fn unsupported_liveness_is_not_not_running() {
        assert_ne!(Liveness::Unsupported, Liveness::NotRunning);
    }

    #[test]
    fn reload_signal_is_unsupported() {
        let err = send_signal(12345, Signal::Reload).unwrap_err();
        assert!(
            err.to_string().contains("not supported"),
            "expected 'not supported', got: {}",
            err
        );
    }

    #[test]
    fn terminate_nonexistent_pid_errors() {
        let err = send_signal(99999999, Signal::Terminate).unwrap_err();
        assert!(
            err.to_string().contains("Failed to open process"),
            "expected 'Failed to open process', got: {}",
            err
        );
    }

    #[test]
    fn register_reload_flag_is_no_op() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(register_reload_flag(flag).is_ok());
    }
}