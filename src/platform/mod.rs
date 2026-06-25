//! OS-dependent lifecycle primitives for the daemon and process detection.
//!
//! This module concentrates the Unix/Windows split. Business logic in
//! `daemon`, `poller`, and `claude_tracking` calls into the platform layer
//! instead of using `nix`, `daemonize`, or `signal-hook` directly.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Result of probing whether a process is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process responded to the liveness probe.
    Running,
    /// The process is definitely not alive (PID absent / signal failed).
    NotRunning,
    /// The current platform cannot determine liveness in this implementation.
    Unsupported,
}

/// Lifecycle signal that can be sent to a daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Terminate,
    Reload,
}

/// Check whether a process with the given PID is currently alive.
pub fn is_process_alive(pid: u32) -> Liveness {
    imp::is_process_alive(pid)
}

/// Send a lifecycle signal to a process.
pub fn send_signal(pid: u32, signal: Signal) -> anyhow::Result<()> {
    imp::send_signal(pid, signal)
}

/// Detach and run the daemon body.
///
/// On Unix this daemonizes the current process and then calls `run` in the
/// detached child. On Windows this is unsupported in W1 and returns an error
/// without invoking `run`.
pub fn start_daemon<F>(data_dir: &Path, run: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    imp::start_daemon(data_dir, run)
}

/// Register an atomic flag that is set when a reload signal arrives.
///
/// On Unix this installs a SIGHUP handler. On Windows no signal is registered
/// and the flag is never set by the runtime, but the call succeeds so the
/// foreground polling loop can compile and run unchanged.
pub fn register_reload_flag(flag: Arc<AtomicBool>) -> anyhow::Result<()> {
    imp::register_reload_flag(flag)
}

#[cfg(unix)]
mod imp {
    pub use super::unix::*;
}

#[cfg(windows)]
mod imp {
    pub use super::windows::*;
}

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;
