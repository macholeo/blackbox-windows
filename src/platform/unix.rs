//! Unix implementation of the platform lifecycle layer.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use super::{Liveness, Signal};

pub fn is_process_alive(pid: u32) -> Liveness {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) {
        Ok(_) => Liveness::Running,
        Err(_) => Liveness::NotRunning,
    }
}

pub fn send_signal(pid: u32, signal: Signal) -> anyhow::Result<()> {
    let nix_signal = match signal {
        Signal::Terminate => nix::sys::signal::Signal::SIGTERM,
        Signal::Reload => nix::sys::signal::Signal::SIGHUP,
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix_signal)?;
    Ok(())
}

pub fn start_daemon<F>(data_dir: &Path, run: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    std::fs::create_dir_all(data_dir)?;
    let pid_path = data_dir.join("blackbox.pid");
    let log_path = data_dir.join("blackbox.log");

    let daemonize = daemonize::Daemonize::new()
        .pid_file(&pid_path)
        .working_directory("/")
        .stdout(File::create(&log_path)?)
        .stderr(File::create(data_dir.join("blackbox.err.log"))?);

    match daemonize.start() {
        Ok(()) => run(),
        Err(e) => anyhow::bail!("Failed to daemonize: {}", e),
    }
}

pub fn register_reload_flag(flag: Arc<AtomicBool>) -> anyhow::Result<()> {
    signal_hook::flag::register(signal_hook::consts::SIGHUP, flag)?;
    Ok(())
}
