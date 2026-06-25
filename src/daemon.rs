use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::output::OutputFormat;
use crate::platform::{self, Liveness, Signal};
use crate::poller;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub enum HealthIndicator {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, serde::Serialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub last_poll_at: Option<chrono::DateTime<chrono::Utc>>,
    pub repos_watched: Option<u64>,
    pub repos_failed_last_poll: Option<u64>,
    pub failed_sample: Vec<String>,
    pub discovery_failed_last_poll: Option<u64>,
    pub discovery_failed_sample: Vec<String>,
    pub db_size_bytes: Option<u64>,
    pub events_today: Option<u64>,
    pub health: HealthIndicator,
}

pub fn pid_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("blackbox.pid")
}

pub fn is_daemon_running(data_dir: &Path) -> anyhow::Result<Option<u32>> {
    let path = pid_file_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let pid_str = std::fs::read_to_string(&path)?;
    let pid: u32 = pid_str.trim().parse()?;
    match platform::is_process_alive(pid) {
        Liveness::Running => Ok(Some(pid)),
        Liveness::NotRunning => {
            // Stale PID file -- process is dead, clean up
            std::fs::remove_file(&path)?;
            Ok(None)
        }
        Liveness::Unsupported => {
            // Cannot determine liveness (e.g., access denied to protected
            // process). Assume running to avoid deleting a valid PID file.
            Ok(Some(pid))
        }
    }
}

pub fn start_daemon(config: Config, data_dir: &Path) -> anyhow::Result<()> {
    if let Some(pid) = is_daemon_running(data_dir)? {
        anyhow::bail!("Daemon already running (PID {})", pid);
    }

    platform::start_daemon(data_dir, move || {
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Info)
            .init();
        log::info!("Daemon started (PID {})", std::process::id());
        if let Err(e) = poller::run_poll_loop(config) {
            log::error!("Poll loop error: {}", e);
        }
        Ok(())
    })
}

pub fn stop_daemon(data_dir: &Path) -> anyhow::Result<()> {
    match is_daemon_running(data_dir)? {
        Some(pid) => {
            platform::send_signal(pid, Signal::Terminate)?;
            // Remove PID file
            let path = pid_file_path(data_dir);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            println!("Daemon stopped (PID {})", pid);
        }
        None => {
            println!("Daemon not running");
        }
    }
    Ok(())
}

/// RAII guard that writes a PID file on creation and removes it on drop.
pub struct PidGuard {
    path: PathBuf,
}

impl PidGuard {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = pid_file_path(data_dir);
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self { path })
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn run_foreground(config: Config, data_dir: &Path) -> anyhow::Result<()> {
    let _pid_guard = PidGuard::new(data_dir)?;
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();
    log::info!("Running in foreground (PID {})", std::process::id());
    poller::run_poll_loop(config)
}

pub fn reload_daemon(data_dir: &Path) -> anyhow::Result<()> {
    match is_daemon_running(data_dir)? {
        Some(pid) => {
            platform::send_signal(pid, Signal::Reload)?;
            println!("Reloading config (PID {})", pid);
        }
        None => println!("Daemon not running"),
    }
    Ok(())
}

pub fn get_daemon_status(data_dir: &Path, config: &Config) -> anyhow::Result<DaemonStatus> {
    let pid = is_daemon_running(data_dir)?;
    let running = pid.is_some();

    let uptime_secs = if running {
        let pid_path = pid_file_path(data_dir);
        std::fs::metadata(&pid_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
    } else {
        None
    };

    let db_path = data_dir.join("blackbox.db");
    let db_size_bytes = std::fs::metadata(&db_path).ok().map(|m| m.len());

    struct Probed {
        last_poll_at: Option<chrono::DateTime<chrono::Utc>>,
        repos_watched: Option<u64>,
        repos_failed: Option<u64>,
        failed_sample: Vec<String>,
        discovery_failed: Option<u64>,
        discovery_failed_sample: Vec<String>,
        events_today: Option<u64>,
        poll_mode: Option<String>,
        effective_poll_interval_secs: Option<u64>,
    }
    let probed = if db_path.exists() {
        match crate::db::open_db(&db_path) {
            Ok(conn) => {
                // Atomic snapshot read — all keys in one statement so doctor
                // and status never observe a torn read where last_poll_at is
                // fresh but failure metrics are from a prior commit.
                // Codex round 8 [high].
                let snap = crate::db::get_daemon_state_all(&conn).unwrap_or_default();
                let parse_u64 = |k: &str| snap.get(k).and_then(|s| s.parse::<u64>().ok());
                let parse_lines_key = |k: &str| -> Vec<String> {
                    snap.get(k)
                        .map(|s| s.lines().filter(|l| !l.is_empty()).map(String::from).collect())
                        .unwrap_or_default()
                };
                Probed {
                    last_poll_at: snap
                        .get("last_poll_at")
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc)),
                    repos_watched: parse_u64("repos_watched"),
                    repos_failed: parse_u64("last_poll_repos_failed"),
                    failed_sample: parse_lines_key("last_poll_failed_sample"),
                    discovery_failed: parse_u64("last_poll_discovery_failed"),
                    discovery_failed_sample: parse_lines_key("last_poll_discovery_failed_sample"),
                    events_today: crate::db::count_events_today(&conn).ok(),
                    poll_mode: snap.get("last_poll_mode").cloned(),
                    effective_poll_interval_secs: parse_u64("effective_poll_interval_secs"),
                }
            }
            Err(_) => Probed {
                last_poll_at: None, repos_watched: None, repos_failed: None,
                failed_sample: Vec::new(), discovery_failed: None,
                discovery_failed_sample: Vec::new(), events_today: None,
                poll_mode: None, effective_poll_interval_secs: None,
            },
        }
    } else {
        Probed {
            last_poll_at: None, repos_watched: None, repos_failed: None,
            failed_sample: Vec::new(), discovery_failed: None,
            discovery_failed_sample: Vec::new(), events_today: None,
            poll_mode: None, effective_poll_interval_secs: None,
        }
    };

    // Mirror check_poll_health exactly so doctor and status never disagree.
    // Watcher mode → 2*FULL_SCAN_SECS only; polling mode → 3*poll_interval
    // floored at 120s; missing key → legacy max-of-both. Pull poll_interval
    // from the daemon's persisted value so a malformed reader-side config
    // (status's load_config-fail fallback to Config::default()) doesn't
    // misclassify health for daemons running a non-default interval.
    let effective_interval = probed
        .effective_poll_interval_secs
        .unwrap_or(config.poll_interval_secs);
    let max_expected_gap_secs = crate::doctor::stall_threshold_for_mode(
        probed.poll_mode.as_deref(),
        effective_interval,
    );
    let health = compute_health(
        running,
        probed.last_poll_at,
        probed.repos_watched.unwrap_or(0),
        probed.repos_failed,
        probed.discovery_failed,
        max_expected_gap_secs,
    );

    Ok(DaemonStatus {
        running,
        pid,
        uptime_secs,
        last_poll_at: probed.last_poll_at,
        repos_watched: probed.repos_watched,
        repos_failed_last_poll: probed.repos_failed,
        failed_sample: probed.failed_sample,
        discovery_failed_last_poll: probed.discovery_failed,
        discovery_failed_sample: probed.discovery_failed_sample,
        db_size_bytes,
        events_today: probed.events_today,
        health,
    })
}

fn compute_health(
    running: bool,
    last_poll_at: Option<chrono::DateTime<chrono::Utc>>,
    repos_watched: u64,
    repos_failed: Option<u64>,
    discovery_failed: Option<u64>,
    max_expected_gap_secs: u64,
) -> HealthIndicator {
    if !running {
        return HealthIndicator::Red;
    }
    // Discovery failures = a configured watch_dir is unreadable. No repos under
    // it are reaching poll_one, so per-repo failure metrics will silently say
    // "0 failed". Treat as Red so status agrees with doctor.
    if discovery_failed.unwrap_or(0) > 0 {
        return HealthIndicator::Red;
    }
    let base = match last_poll_at {
        None => HealthIndicator::Yellow,
        Some(t) => {
            let age_secs = chrono::Utc::now().signed_duration_since(t).num_seconds().max(0) as u64;
            // Match evaluate_poll_health exactly: under threshold = healthy,
            // at-or-over = stalled. Codex round 6 [medium]: a half-threshold
            // yellow window made status report "Running (stale)" at 40min
            // while doctor reported pass at the same instant. Same daemon,
            // contradicting health colors. Drop the window — freshness can
            // be a separate advisory if we want one later, but the primary
            // color must agree with doctor.
            if age_secs >= max_expected_gap_secs {
                HealthIndicator::Red
            } else {
                HealthIndicator::Green
            }
        }
    };
    // Daemon predates the failure metric — we cannot say it's healthy. Cap at
    // Yellow so users see "unknown" rather than a misleading green.
    let failed = match repos_failed {
        None => {
            return match base {
                HealthIndicator::Green => HealthIndicator::Yellow,
                other => other,
            };
        }
        Some(n) => n,
    };
    // Process alive + polling, but every repo errored — silent data loss.
    if repos_watched > 0 && failed >= repos_watched {
        return HealthIndicator::Red;
    }
    // Partial poll failures degrade Green to Yellow.
    if failed > 0 && base == HealthIndicator::Green {
        return HealthIndicator::Yellow;
    }
    // No repos discovered + no discovery errors = unconfigured. Doctor renders
    // this as an Optional warning ("No repos discovered — check watch_dirs").
    // status must mirror that, not return Green. Codex round 8 [high]: prior
    // status returned Green for an empty watch_dirs config because failed=0,
    // contradicting doctor.
    if repos_watched == 0 && base == HealthIndicator::Green {
        return HealthIndicator::Yellow;
    }
    base
}

fn render_status_pretty(status: &DaemonStatus) {
    use colored::Colorize;
    let (icon, label) = match status.health {
        HealthIndicator::Green => ("\u{2713}".green().bold(), "Running".green().bold()),
        HealthIndicator::Yellow => {
            // Distinguish "stale poll", "missing metric", "partial failures",
            // "no repos discovered" so users know which corrective action
            // applies.
            let text = if !status.running {
                "Stopped"
            } else if status.repos_failed_last_poll.is_none() {
                "Running (metrics unknown — restart daemon to enable)"
            } else if status.repos_failed_last_poll.unwrap_or(0) > 0 {
                "Running (poll failures)"
            } else if status.repos_watched.unwrap_or(0) == 0 {
                "Running (no repos discovered — check watch_dirs)"
            } else {
                "Running (stale)"
            };
            ("\u{26a0}".yellow().bold(), text.yellow().bold())
        }
        HealthIndicator::Red => {
            let text = if !status.running {
                "Stopped"
            } else if status.discovery_failed_last_poll.unwrap_or(0) > 0 {
                "Running (watch_dirs unreadable)"
            } else if status.repos_watched.unwrap_or(0) > 0
                && status.repos_failed_last_poll.unwrap_or(0) >= status.repos_watched.unwrap_or(0)
            {
                "Running (all polls failing)"
            } else {
                // Catches Red from stale heartbeat on a still-running process.
                "Running (stalled)"
            };
            ("\u{2717}".red().bold(), text.red().bold())
        }
    };
    println!("{} {}", icon, label);
    if let Some(pid) = status.pid {
        println!("  PID:           {}", pid);
    }
    if let Some(secs) = status.uptime_secs {
        println!("  Uptime:        {}", format_uptime(secs));
    }
    match status.last_poll_at {
        Some(t) => {
            let age = chrono::Utc::now().signed_duration_since(t);
            println!("  Last poll:     {} ago", format_duration_ago(age));
        }
        None => println!("  Last poll:     never"),
    }
    match status.repos_watched {
        Some(n) => println!("  Repos watched: {}", n),
        None => println!("  Repos watched: unknown"),
    }
    if let Some(failed) = status.repos_failed_last_poll
        && failed > 0
    {
        let total = status.repos_watched.unwrap_or(0).max(failed);
        let suffix = if status.failed_sample.is_empty() {
            String::new()
        } else {
            format!(" — {}", status.failed_sample.join(", "))
        };
        let line = format!("  Poll failures: {failed} of {total}{suffix}");
        println!("{}", line.yellow());
    }
    if let Some(disc) = status.discovery_failed_last_poll
        && disc > 0
    {
        let suffix = if status.discovery_failed_sample.is_empty() {
            String::new()
        } else {
            format!(" — {}", status.discovery_failed_sample.join(", "))
        };
        let line = format!("  Discovery failures: {disc} watch_dir(s){suffix}");
        println!("{}", line.red());
    }
    match status.db_size_bytes {
        Some(b) => println!("  DB size:       {:.1} KB", b as f64 / 1024.0),
        None => println!("  DB size:       no DB yet"),
    }
    println!(
        "  Events today:  {}",
        status.events_today.unwrap_or(0)
    );
}

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    match (h, m) {
        (0, 0) => format!("{}s", s),
        (0, _) => format!("{}m {}s", m, s),
        _ => format!("{}h {}m", h, m),
    }
}

fn format_duration_ago(d: chrono::Duration) -> String {
    let total_secs = d.num_seconds().max(0);
    let mins = total_secs / 60;
    let hours = mins / 60;
    if hours > 0 {
        format!("{}h {}m", hours, mins % 60)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", total_secs)
    }
}

pub fn daemon_status(data_dir: &Path, config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    let status = get_daemon_status(data_dir, config)?;
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&status)?),
        _ => render_status_pretty(&status),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_health_red_when_all_repos_failing() {
        // Daemon alive + recent poll, but every repo errored on the last cycle.
        // Status must not show green — that's silent data loss.
        let h = compute_health(true, Some(chrono::Utc::now()), 5, Some(5), Some(0), 3600);
        assert!(matches!(h, HealthIndicator::Red), "expected Red, got {:?}", h);
    }

    #[test]
    fn compute_health_yellow_on_partial_failures() {
        let h = compute_health(true, Some(chrono::Utc::now()), 5, Some(2), Some(0), 3600);
        assert!(matches!(h, HealthIndicator::Yellow), "expected Yellow, got {:?}", h);
    }

    #[test]
    fn compute_health_green_when_no_failures_and_recent_poll() {
        let h = compute_health(true, Some(chrono::Utc::now()), 5, Some(0), Some(0), 3600);
        assert!(matches!(h, HealthIndicator::Green), "expected Green, got {:?}", h);
    }

    #[test]
    fn compute_health_red_when_not_running_regardless_of_failures() {
        // Daemon dead trumps everything else.
        let h = compute_health(false, Some(chrono::Utc::now()), 5, Some(0), Some(0), 3600);
        assert!(matches!(h, HealthIndicator::Red));
    }

    #[test]
    fn compute_health_legacy_daemon_recent_poll_caps_at_yellow() {
        // Codex regression: missing failure metric must NOT silently render
        // as Green. Pre-fix, compute_health collapsed None → 0 and returned
        // Green for a recent poll. Now it must stay Yellow ("unknown") so
        // users in the rollout window see "needs daemon restart".
        let h = compute_health(true, Some(chrono::Utc::now()), 5, None, None, 3600);
        assert_eq!(h, HealthIndicator::Yellow,
            "missing failure metric should cap health at Yellow, got {:?}", h);
    }

    #[test]
    fn compute_health_legacy_daemon_old_poll_stays_red() {
        // If the legacy daemon's last poll is also stale, severity should not
        // be downgraded — old poll Red trumps the unknown-metric Yellow cap.
        let stale = chrono::Utc::now() - chrono::Duration::hours(2);
        let h = compute_health(true, Some(stale), 5, None, None, 3600);
        assert_eq!(h, HealthIndicator::Red);
    }

    #[test]
    fn compute_health_discovery_failure_is_red() {
        // Codex / impl-reviewer round 3: status must agree with doctor when a
        // watch_dir is unreadable. compute_health was ignoring discovery_failed
        // and would have shown Green for this case while doctor shows Red.
        let h = compute_health(true, Some(chrono::Utc::now()), 0, Some(0), Some(2), 3600);
        assert_eq!(h, HealthIndicator::Red);
    }

    #[test]
    fn compute_health_uses_max_expected_gap_secs_not_hardcoded_30min() {
        // Codex / code-reviewer round 3: idle watcher daemon with last poll 25
        // min ago and gap budget 60min must stay Green. Pre-fix, the hardcoded
        // 30min Yellow threshold would have downgraded it.
        let twenty_five_min_ago = chrono::Utc::now() - chrono::Duration::minutes(25);
        let h = compute_health(true, Some(twenty_five_min_ago), 5, Some(0), Some(0), 3600);
        assert_eq!(h, HealthIndicator::Green);
    }

    #[test]
    fn compute_health_at_exact_threshold_is_red_matching_doctor() {
        // Locks in the boundary-agreement contract: at exactly max_expected_gap
        // both compute_health and evaluate_poll_health must report Red /
        // stalled. If anyone changes one operator without the other, this
        // test breaks.
        let exactly_threshold_ago = chrono::Utc::now() - chrono::Duration::seconds(3600);
        let h = compute_health(true, Some(exactly_threshold_ago), 5, Some(0), Some(0), 3600);
        assert_eq!(h, HealthIndicator::Red,
            "exact-threshold age must be Red, agreeing with doctor's `>=` stall check");
    }

    #[test]
    fn compute_health_red_for_running_stalled_daemon_renders_running_label() {
        // Render-side regression: a running but stalled daemon goes Red. The
        // pretty-print branch used to print "Stopped" for any Red+running case
        // that didn't match all-failures or discovery — wrong label for stalled.
        // Asserting via DaemonStatus shape; render text covered separately.
        let stale = chrono::Utc::now() - chrono::Duration::hours(2);
        let h = compute_health(true, Some(stale), 5, Some(0), Some(0), 3600);
        assert_eq!(h, HealthIndicator::Red);
    }

    #[test]
    fn compute_health_watcher_mode_2hr_poll_interval_does_not_widen_threshold() {
        // User has poll_interval_secs=7200, daemon last polled 90min ago in
        // watcher mode. Caller resolves threshold via stall_threshold_for_mode
        // → 2*FULL_SCAN_SECS = 3600s. Status must be Red (stalled), not Green.
        let ninety_min_ago = chrono::Utc::now() - chrono::Duration::minutes(90);
        let threshold = crate::doctor::stall_threshold_for_mode(Some("watcher"), 7200);
        let h = compute_health(true, Some(ninety_min_ago), 5, Some(0), Some(0), threshold);
        assert_eq!(
            h,
            HealthIndicator::Red,
            "watcher-mode threshold must ignore poll_interval_secs; got {:?}",
            h
        );
    }

    #[test]
    fn compute_health_polling_mode_uses_poll_interval() {
        // Polling mode honors the user's interval. last_poll 25min ago,
        // interval=600 → threshold 1800s. age 1500s < 1800s → Green (matches
        // doctor's evaluate_poll_health, which has no yellow window).
        let twenty_five_min_ago = chrono::Utc::now() - chrono::Duration::minutes(25);
        let threshold = crate::doctor::stall_threshold_for_mode(Some("polling"), 600);
        assert_eq!(threshold, 1800);
        let h = compute_health(true, Some(twenty_five_min_ago), 5, Some(0), Some(0), threshold);
        assert_eq!(h, HealthIndicator::Green);
    }

    #[test]
    fn compute_health_zero_repos_returns_yellow_not_green() {
        // Codex round 8 [high]: status used to return Green for an empty
        // watch_dirs config because failed=0 and last poll was recent. doctor
        // renders the same input as an Optional warning ("No repos discovered").
        // Mirror that — Yellow, not Green.
        let h = compute_health(true, Some(chrono::Utc::now()), 0, Some(0), Some(0), 3600);
        assert_eq!(h, HealthIndicator::Yellow);
    }

    #[test]
    fn compute_health_below_threshold_matches_doctor_pass() {
        // Codex round 6 [medium]: 40min/60min was Yellow in status while
        // doctor reported pass for the same input. Lock in: any age strictly
        // under threshold is Green, no half-window.
        let forty_min_ago = chrono::Utc::now() - chrono::Duration::minutes(40);
        let h = compute_health(true, Some(forty_min_ago), 5, Some(0), Some(0), 3600);
        assert_eq!(
            h,
            HealthIndicator::Green,
            "40min/60min must be Green to match doctor; got {:?}",
            h
        );
    }

    #[test]
    fn pid_guard_writes_pid_file_on_creation() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = PidGuard::new(dir.path()).unwrap();
        let pid_path = pid_file_path(dir.path());
        assert!(pid_path.exists(), "PID file should exist after guard creation");
        let content = std::fs::read_to_string(&pid_path).unwrap();
        let pid: u32 = content.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn pid_guard_removes_pid_file_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = pid_file_path(dir.path());
        {
            let _guard = PidGuard::new(dir.path()).unwrap();
            assert!(pid_path.exists());
        }
        assert!(!pid_path.exists(), "PID file should be removed after guard is dropped");
    }

    #[test]
    fn pid_guard_creates_data_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("sub").join("dir");
        let _guard = PidGuard::new(&nested).unwrap();
        let pid_path = pid_file_path(&nested);
        assert!(pid_path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn nonexistent_pid_is_stale_and_cleaned_up() {
        // A PID file with a nonexistent PID should be detected as stale
        // and cleaned up automatically.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = pid_file_path(dir.path());
        std::fs::write(&pid_file, "99999999").unwrap();

        let result = is_daemon_running(dir.path()).unwrap();
        assert!(result.is_none(), "stale PID should return None, got {:?}", result);
        assert!(
            !pid_file.exists(),
            "stale PID file should be removed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn current_process_pid_reports_running() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = pid_file_path(dir.path());
        std::fs::write(&pid_file, std::process::id().to_string()).unwrap();

        let result = is_daemon_running(dir.path()).unwrap();
        assert_eq!(
            result,
            Some(std::process::id()),
            "current process PID should report running"
        );
        assert!(
            pid_file.exists(),
            "PID file must not be removed when process is alive"
        );
    }
}
