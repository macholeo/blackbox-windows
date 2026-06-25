//! Windows W1 implementation of the platform lifecycle layer.
//!
//! W1 is a compile substrate. Process management and detached daemon spawning
//! are intentionally unsupported and return explicit errors/capability results.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use super::{Liveness, Signal};

pub fn is_process_alive(_pid: u32) -> Liveness {
    Liveness::Unsupported
}

pub fn send_signal(_pid: u32, _signal: Signal) -> anyhow::Result<()> {
    anyhow::bail!("Process signals are not supported on Windows in W1")
}

pub fn start_daemon<F>(_data_dir: &Path, _run: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    anyhow::bail!("Daemon start is not supported on Windows in W1")
}

pub fn register_reload_flag(_flag: Arc<AtomicBool>) -> anyhow::Result<()> {
    // No-op: SIGHUP does not exist on Windows. Foreground polling can still
    // compile and run; live reload is deferred to W2.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_liveness_reports_unsupported() {
        assert_eq!(is_process_alive(12345), Liveness::Unsupported);
    }

    #[test]
    fn unsupported_liveness_is_not_not_running() {
        // Contract: callers must not collapse Unsupported into NotRunning.
        assert_ne!(Liveness::Unsupported, Liveness::NotRunning);
    }

    #[test]
    fn send_signal_is_unsupported() {
        let err = send_signal(12345, Signal::Terminate).unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }

    #[test]
    fn start_daemon_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let err = start_daemon(tmp.path(), || -> anyhow::Result<()> { Ok(()) }).unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }

    #[test]
    fn register_reload_flag_is_no_op() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(register_reload_flag(flag).is_ok());
    }
}
