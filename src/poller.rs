use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::ai_tracking;
use crate::config::{self, Config};
use crate::db;
use crate::enrichment;
use crate::git_ops::{self, RepoState};
use crate::repo_scanner::{self, DiscoveredRepos};
use crate::watcher::RepoWatcher;

/// Full-scan interval when watcher is active (30 min).
pub const FULL_SCAN_SECS: u64 = 30 * 60;

/// Ensure a RepoState entry exists for a repo path, resolving worktrees.
/// For worktrees, main_repo_path = resolved main repo root.
/// For regular repos, main_repo_path = repo_path.
fn ensure_state(repo_path: &Path, repo_states: &mut HashMap<PathBuf, RepoState>) {
    repo_states.entry(repo_path.to_path_buf()).or_insert_with(|| {
        let main_repo_path = if repo_scanner::is_worktree(repo_path).is_some() {
            repo_scanner::resolve_main_repo(repo_path).unwrap_or_else(|_| repo_path.to_path_buf())
        } else {
            repo_path.to_path_buf()
        };
        RepoState {
            main_repo_path,
            ..Default::default()
        }
    });
}

/// Maximum number of failed paths to persist as a sample. Bounds the
/// daemon_state row size; the failed *count* is always exact.
const FAILED_SAMPLE_LIMIT: usize = 5;

/// Outcome of a single poll cycle. Used to write health metrics so `doctor`
/// can detect silent data loss (every repo failing while daemon stays alive).
#[derive(Debug, Default, Clone)]
pub struct PollMetrics {
    pub failed_paths: Vec<PathBuf>,
}

/// Update the running set of repos in a failure state.
/// Pure function — separated from I/O so it's trivially testable and
/// reusable across the full-scan and watcher poll paths.
pub fn record_repo_outcome(
    failed: &mut std::collections::HashSet<PathBuf>,
    repo_path: &std::path::Path,
    was_error: bool,
) {
    if was_error {
        failed.insert(repo_path.to_path_buf());
    } else {
        failed.remove(repo_path);
    }
}

/// Convenience: build PollMetrics from the current failure set.
pub fn metrics_from_set(failed: &std::collections::HashSet<PathBuf>) -> PollMetrics {
    let mut paths: Vec<PathBuf> = failed.iter().cloned().collect();
    paths.sort();
    PollMetrics { failed_paths: paths }
}

/// Outcome of probing the configured watch_dirs for read access. A watch_dir
/// that's unreadable means repos under it never reach `poll_one`, so
/// per-repo failure metrics would silently report "0 failed" — exactly the
/// blind spot Codex flagged. Tracked separately and surfaced as a Required
/// failure in `doctor`.
#[derive(Debug, Default, Clone)]
pub struct DiscoveryMetrics {
    pub failures: Vec<(PathBuf, String)>,
}

/// Verify each configured watch_dir can be opened for directory read.
/// Returns the subset of paths that errored, with the OS error message.
/// Dedups the input so a config with the same dir listed twice doesn't
/// double-count failures. Does not mutate the filesystem.
pub fn probe_watch_dirs(watch_dirs: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut seen: std::collections::HashSet<&PathBuf> = std::collections::HashSet::new();
    watch_dirs
        .iter()
        .filter(|d| seen.insert(*d))
        .filter_map(|d| match std::fs::read_dir(d) {
            Ok(_) => None,
            Err(e) => Some((d.clone(), e.to_string())),
        })
        .collect()
}

/// Persist discovery health to daemon_state atomically. count and sample
/// are written under a single transaction so a daemon crash mid-write can't
/// leave doctor reading a count from one cycle and a sample from another.
pub fn write_discovery_metrics(
    conn: &Connection,
    metrics: &DiscoveryMetrics,
) -> anyhow::Result<()> {
    let count = metrics.failures.len();
    let sample = metrics
        .failures
        .iter()
        .take(FAILED_SAMPLE_LIMIT)
        .map(|(p, _)| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let tx = conn.unchecked_transaction()?;
    db::set_daemon_state(&tx, "last_poll_discovery_failed", &count.to_string())?;
    db::set_daemon_state(&tx, "last_poll_discovery_failed_sample", &sample)?;
    tx.commit()?;
    Ok(())
}

/// Atomic per-cycle health snapshot. Combines heartbeat + poll metrics +
/// discovery metrics under one transaction so a partial write can never leave
/// `last_poll_at` advanced past stale failure counts. Doctor reads all 7 keys
/// to grade health; observing a fresh timestamp paired with a stale "0 failed"
/// from a prior cycle was the silent-data-loss failure mode.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub last_poll_at: chrono::DateTime<chrono::Utc>,
    /// "watcher" or "polling". Derived from the LIVE state of `watcher_opt`
    /// at write time, NOT cached at daemon init — pure-polling fallback
    /// re-tries `RepoWatcher::new` every cycle and self-heals.
    pub poll_mode: String,
    pub repos_watched: usize,
    /// The poll interval the daemon is actually using this cycle. Persisted
    /// so `doctor` and `status` derive stall thresholds from the daemon's
    /// real config even when the user's config.toml fails to parse on the
    /// reader side. Codex round 5 [medium]: status had been substituting
    /// `Config::default().poll_interval_secs = 1800` on parse failure,
    /// misclassifying health for any daemon running a non-default interval.
    pub effective_poll_interval_secs: u64,
    pub poll_metrics: PollMetrics,
    pub discovery_metrics: DiscoveryMetrics,
}

/// Write all 7 daemon_state keys for one cycle in a single transaction.
/// Replaces the prior split between `write_heartbeat` / `write_poll_metrics` /
/// `write_discovery_metrics` so any individual key failing rolls back the
/// whole snapshot rather than leaving a torn read for doctor.
pub fn write_health_snapshot(
    conn: &Connection,
    snap: &HealthSnapshot,
) -> anyhow::Result<()> {
    let now = snap.last_poll_at.to_rfc3339();
    let failed_count = snap.poll_metrics.failed_paths.len();
    let failed_sample = snap
        .poll_metrics
        .failed_paths
        .iter()
        .take(FAILED_SAMPLE_LIMIT)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let disc_count = snap.discovery_metrics.failures.len();
    let disc_sample = snap
        .discovery_metrics
        .failures
        .iter()
        .take(FAILED_SAMPLE_LIMIT)
        .map(|(p, _)| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let tx = conn.unchecked_transaction()?;
    db::set_daemon_state(&tx, "last_poll_at", &now)?;
    db::set_daemon_state(&tx, "last_poll_mode", &snap.poll_mode)?;
    db::set_daemon_state(&tx, "repos_watched", &snap.repos_watched.to_string())?;
    db::set_daemon_state(
        &tx,
        "effective_poll_interval_secs",
        &snap.effective_poll_interval_secs.to_string(),
    )?;
    db::set_daemon_state(&tx, "last_poll_repos_failed", &failed_count.to_string())?;
    db::set_daemon_state(&tx, "last_poll_failed_sample", &failed_sample)?;
    db::set_daemon_state(&tx, "last_poll_discovery_failed", &disc_count.to_string())?;
    db::set_daemon_state(&tx, "last_poll_discovery_failed_sample", &disc_sample)?;
    tx.commit()?;
    Ok(())
}

/// Persist poll-cycle health metrics atomically. See `write_discovery_metrics`
/// for the transaction rationale.
pub fn write_poll_metrics(conn: &Connection, metrics: &PollMetrics) -> anyhow::Result<()> {
    let count = metrics.failed_paths.len();
    let sample = metrics
        .failed_paths
        .iter()
        .take(FAILED_SAMPLE_LIMIT)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let tx = conn.unchecked_transaction()?;
    db::set_daemon_state(&tx, "last_poll_repos_failed", &count.to_string())?;
    db::set_daemon_state(&tx, "last_poll_failed_sample", &sample)?;
    tx.commit()?;
    Ok(())
}

/// Poll all repos for git activity. Replaces the failure set with this cycle's
/// outcomes — full_scan covers every watched repo so it is authoritative.
fn poll_all_repos(
    repos: &[PathBuf],
    repo_states: &mut HashMap<PathBuf, RepoState>,
    conn: &Connection,
    failed_set: &mut std::collections::HashSet<PathBuf>,
) {
    failed_set.clear();
    for repo_path in repos {
        let was_error = poll_one(repo_path, repo_states, conn);
        record_repo_outcome(failed_set, repo_path, was_error);
    }
}

/// Poll a single repo. Returns true on error (caller decides what to log/track).
/// Used by both full_scan and the watcher event path so failure tracking stays
/// consistent across entry points.
pub fn poll_one(
    repo_path: &Path,
    repo_states: &mut HashMap<PathBuf, RepoState>,
    conn: &Connection,
) -> bool {
    ensure_state(repo_path, repo_states);
    let state = repo_states.get_mut(repo_path).unwrap();
    let db_repo_path = state.main_repo_path.to_string_lossy().to_string();
    match git_ops::poll_repo(repo_path, &db_repo_path, state, conn) {
        Ok(()) => false,
        Err(e) => {
            log::warn!("Error polling {}: {}", repo_path.display(), e);
            true
        }
    }
}

/// Drop entries in `repo_states` whose key is not in `current`, skipping any
/// path under an `unreliable_root` (a watch_dir whose probe or recursive walk
/// errored this cycle). Returns evicted paths so callers can clear matching
/// entries from `failed_set`.
///
/// `unreliable_root` skip is the load-bearing piece: a transient TCC flap or
/// network-mount blip that makes one watch_dir unreadable for a single cycle
/// would otherwise evict every repo under it. The next successful discovery
/// recreates `RepoState` from scratch, and `git_ops::poll_repo` treats a
/// fresh state as a first-poll — only seeding HEAD plus today's first 50
/// commits. Activity that happened during the blackout is irrecoverable.
/// Keeping state through transient failures means the next clean cycle
/// resumes from the prior cursor and reconstructs missed events.
///
/// Without pruning altogether, `repo_states` accumulates dead entries after
/// a config reload removes a watch_dir or a tracked repo is deleted from
/// disk. The "all polls failing" Required check is gated by
/// `failed_count == repos_watched`; an inflated denominator silently masks
/// every-active-repo failure as a partial warning. So we still prune — just
/// only for paths whose discovery this cycle was reliable.
pub fn prune_repo_states(
    repo_states: &mut HashMap<PathBuf, RepoState>,
    current: &[PathBuf],
    unreliable_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let keep: std::collections::HashSet<&PathBuf> = current.iter().collect();
    let is_under_unreliable = |p: &Path| -> bool {
        unreliable_roots
            .iter()
            .any(|root| p == root.as_path() || p.starts_with(root))
    };
    let evicted: Vec<PathBuf> = repo_states
        .keys()
        .filter(|p| !keep.contains(p) && !is_under_unreliable(p))
        .cloned()
        .collect();
    for p in &evicted {
        repo_states.remove(p);
    }
    evicted
}

/// Returns true only if a worktree path is CONFIRMED gone — the worktree
/// directory or its .git pointer is reliably absent (Ok(false) from
/// try_exists). Permission/IO errors on either probe return false so a
/// transient TCC denial or network blip does not evict the cursor.
///
/// Codex round 9 [high]: the prior `is_worktree(path).is_none()` check
/// returned None for any read failure — including transient ones — and
/// dropped RepoState. The next successful poll then re-seeded as a first
/// poll (HEAD + today's first 50 commits), losing every commit during the
/// outage.
fn is_definitely_stale_worktree(path: &Path) -> bool {
    if matches!(path.try_exists(), Ok(false)) {
        return true;
    }
    let git = path.join(".git");
    matches!(git.try_exists(), Ok(false))
}

/// Remove stale worktree entries from repo_states. Only evicts on confirmed
/// deletion — transient permission / IO errors keep state across the blip.
pub fn remove_stale_worktrees(repo_states: &mut HashMap<PathBuf, RepoState>) -> Vec<PathBuf> {
    let stale: Vec<PathBuf> = repo_states
        .iter()
        .filter(|(path, state)| {
            // Only check worktrees (main_repo_path != scanned path)
            state.main_repo_path != **path && is_definitely_stale_worktree(path)
        })
        .map(|(path, _)| path.clone())
        .collect();
    for path in &stale {
        log::warn!("Stale worktree removed: {}", path.display());
        repo_states.remove(path);
    }
    stale
}

/// Build and persist a HealthSnapshot from the current loop state. Mode is
/// derived live from `watcher_opt` so a self-healed pure-polling daemon
/// (watcher recovers on retry) starts reporting "watcher" the very next
/// snapshot — without this, doctor would keep using the more lenient
/// polling-mode threshold long after recovery.
fn write_snapshot(
    conn: &Connection,
    watcher_opt: &Option<RepoWatcher>,
    repos_watched: usize,
    effective_poll_interval_secs: u64,
    failed_set: &std::collections::HashSet<PathBuf>,
    discovery: &DiscoveryMetrics,
) {
    let snap = HealthSnapshot {
        last_poll_at: chrono::Utc::now(),
        poll_mode: if watcher_opt.is_some() { "watcher" } else { "polling" }.into(),
        repos_watched,
        effective_poll_interval_secs,
        poll_metrics: metrics_from_set(failed_set),
        discovery_metrics: discovery.clone(),
    };
    if let Err(e) = write_health_snapshot(conn, &snap) {
        log::warn!("Failed to write health snapshot: {}", e);
    }
}

/// Full scan: re-discover repos, poll all, collect reviews, track sessions.
/// Returns repos plus the DiscoveryMetrics so the caller can write a single
/// atomic HealthSnapshot covering heartbeat, poll metrics, discovery metrics,
/// and live mode. The caller stamps the snapshot timestamp + mode at write
/// time so mode reflects watcher_opt as of the write, not as of full_scan
/// entry (matters in the pure-polling fallback that retries the watcher).
fn full_scan(
    config: &Config,
    repo_states: &mut HashMap<PathBuf, RepoState>,
    conn: &Connection,
    failed_set: &mut std::collections::HashSet<PathBuf>,
) -> (Vec<PathBuf>, DiscoveryMetrics) {
    let mut discovery_failures = probe_watch_dirs(&config.watch_dirs);
    for (path, err) in &discovery_failures {
        log::warn!("Cannot read watch_dir {}: {}", path.display(), err);
    }

    let DiscoveredRepos { repos, traversal_errors, .. } =
        repo_scanner::discover_repos_with_errors(&config.watch_dirs, config.worktree_dir_name.as_deref());
    for (path, err) in &traversal_errors {
        log::warn!("Discovery walk error at {}: {}", path.display(), err);
    }
    discovery_failures.extend(traversal_errors);

    // Prune BEFORE polling. poll_all_repos calls ensure_state for every entry
    // in `repos`, which would re-insert pruned-but-still-discovered entries —
    // safe — but if we pruned AFTER, evicted entries still in `repo_states`
    // from prior cycles would hang around forever and inflate the
    // `repos_watched` denominator that gates the "all polls failing" check.
    //
    // Skip eviction for repos under any watch_dir / subtree that errored
    // discovery this cycle. A transient failure must not drop activity-state
    // — see `prune_repo_states` doc.
    let unreliable_roots: Vec<PathBuf> =
        discovery_failures.iter().map(|(p, _)| p.clone()).collect();
    let evicted = prune_repo_states(repo_states, &repos, &unreliable_roots);
    for path in &evicted {
        failed_set.remove(path);
    }

    poll_all_repos(&repos, repo_states, conn, failed_set);
    enrichment::collect_reviews(&repos, conn);
    enrichment::collect_pr_snapshots(&repos, conn);
    ai_tracking::poll_all_ai_sessions(conn, &repos);
    (repos, DiscoveryMetrics { failures: discovery_failures })
}

fn maybe_send_daily_notification(config: &Config, conn: &Connection) {
    if !config.notifications_enabled {
        return;
    }
    if !crate::notifications::is_available() {
        return;
    }

    let parts: Vec<&str> = config.notification_time.split(':').collect();
    let (notify_hour, notify_min) = match parts.as_slice() {
        [h, m] => {
            let h: u32 = match h.parse() {
                Ok(v) => v,
                Err(_) => {
                    log::warn!("Invalid notification_time '{}': bad hour", config.notification_time);
                    return;
                }
            };
            let m: u32 = match m.parse() {
                Ok(v) => v,
                Err(_) => {
                    log::warn!("Invalid notification_time '{}': bad minute", config.notification_time);
                    return;
                }
            };
            (h, m)
        }
        _ => {
            log::warn!("Invalid notification_time format '{}': expected HH:MM", config.notification_time);
            return;
        }
    };

    let now_local = chrono::Local::now();
    let now_time = now_local.time();
    let notify_time = match chrono::NaiveTime::from_hms_opt(notify_hour, notify_min, 0) {
        Some(t) => t,
        None => {
            log::warn!("Invalid notification_time '{}': out of range", config.notification_time);
            return;
        }
    };

    if now_time < notify_time {
        return;
    }

    let today_date = now_local.date_naive().to_string();

    match crate::db::notification_was_sent(conn, &today_date, "daily_summary") {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            log::warn!("Failed to check notification_log: {}", e);
            return;
        }
    }

    let body = match crate::query::daily_summary_for_notification(
        conn,
        config.session_gap_minutes,
        config.first_commit_minutes,
    ) {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            log::warn!("Failed to build daily summary for notification: {}", e);
            return;
        }
    };

    if let Err(e) = crate::notifications::send_notification("Blackbox Daily Summary", &body) {
        log::warn!("OS notification failed: {}", e);
    }

    if let Err(e) = crate::db::record_notification_sent(conn, &today_date, "daily_summary") {
        log::warn!("Failed to record notification_sent: {}", e);
    }
}

pub fn run_poll_loop(mut config: Config) -> anyhow::Result<()> {
    // Register reload handler — on Unix this installs SIGHUP; on Windows it
    // is a no-op in W1 so the polling loop can still compile and run.
    let reload_requested = Arc::new(AtomicBool::new(false));
    crate::platform::register_reload_flag(Arc::clone(&reload_requested))?;

    let db_path = config::data_dir()?.join("blackbox.db");
    let conn = db::open_db(&db_path)?;
    let mut repo_states: HashMap<PathBuf, RepoState> = HashMap::new();
    let mut debounce_map: HashMap<PathBuf, Instant> = HashMap::new();
    // Track per-repo failure state across full_scan and watcher events so
    // a watcher-driven recovery clears stale failures and a watcher-driven
    // failure shows up in `doctor` immediately, not 30 minutes later.
    let mut failed_set: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Initial full scan
    let (mut repos, mut last_discovery) = full_scan(&config, &mut repo_states, &conn, &mut failed_set);

    // Try to set up filesystem watcher
    let mut watcher_opt = RepoWatcher::new(&repos, config.worktree_dir_name.as_deref()).ok();
    if watcher_opt.is_some() {
        log::info!("Watching {} repos for changes", repos.len());
    } else {
        log::warn!("File watcher unavailable, falling back to polling");
    }

    // Atomic snapshot AFTER watcher init so poll_mode reflects the live
    // watcher_opt state. Doing this after init keeps the "mode key derived
    // every snapshot from watcher_opt.is_some()" invariant tested by
    // write_health_snapshot_mode_transitions_from_polling_to_watcher.
    write_snapshot(&conn, &watcher_opt, repos.len(), config.poll_interval_secs, &failed_set, &last_discovery);
    maybe_send_daily_notification(&config, &conn);

    let mut last_full_scan = Instant::now();

    loop {
        // Check for SIGHUP reload request between poll cycles
        if reload_requested.swap(false, Ordering::Relaxed) {
            log::info!("SIGHUP received, reloading config");
            match config::reload_config() {
                Ok(new_cfg) => {
                    if new_cfg.watch_dirs != config.watch_dirs {
                        log::info!("watch_dirs: {:?} -> {:?}", config.watch_dirs, new_cfg.watch_dirs);
                    }
                    if new_cfg.poll_interval_secs != config.poll_interval_secs {
                        log::info!("poll_interval_secs: {} -> {}", config.poll_interval_secs, new_cfg.poll_interval_secs);
                    }
                    config = new_cfg;
                    log::info!("Config reloaded successfully");
                    // Re-discover repos and recreate watcher with new config
                    let (new_repos, new_disc) = full_scan(&config, &mut repo_states, &conn, &mut failed_set);
                    repos = new_repos;
                    last_discovery = new_disc;
                    watcher_opt = RepoWatcher::new(&repos, config.worktree_dir_name.as_deref()).ok();
                    // Snapshot AFTER watcher recreate so mode reflects new state.
                    write_snapshot(&conn, &watcher_opt, repos.len(), config.poll_interval_secs, &failed_set, &last_discovery);
                    maybe_send_daily_notification(&config, &conn);
                    last_full_scan = Instant::now();
                    debounce_map.clear();
                }
                Err(e) => log::warn!("Config reload failed: {e}, keeping previous config"),
            }
        }

        if let Some(ref mut watcher) = watcher_opt {
            // Hybrid mode: block until event or 1s timeout
            let events = watcher.recv_events(&mut debounce_map, Duration::from_secs(1));
            let mut metrics_dirty = false;

            for repo_path in &events.changed_repos {
                log::info!("Detected change in {}", repo_path.display());
                let was_error = poll_one(repo_path, &mut repo_states, &conn);
                record_repo_outcome(&mut failed_set, repo_path, was_error);
                metrics_dirty = true;
            }

            // Handle newly-discovered worktrees
            for wt_path in &events.new_worktrees {
                log::info!("New worktree detected: {}", wt_path.display());
                let was_error = poll_one(wt_path, &mut repo_states, &conn);
                record_repo_outcome(&mut failed_set, wt_path, was_error);
                watcher.watch_repo(wt_path);
                metrics_dirty = true;
            }

            // Clean up stale worktrees. Any removal shrinks repo_states.len()
            // → repos_watched changes → snapshot must rewrite. A previously
            // healthy stale worktree wouldn't be in failed_set, so gating
            // metrics_dirty on `failed_set.remove(path)` returning true would
            // leave the persisted denominator stale. Codex round 5 [high]:
            // live state 2/2 kept reading as 2/3 until next unrelated event,
            // hiding the all-failing condition this PR exists to surface.
            let stale = remove_stale_worktrees(&mut repo_states);
            if !stale.is_empty() {
                metrics_dirty = true;
            }
            for path in &stale {
                failed_set.remove(path);
            }

            if metrics_dirty {
                // Atomic snapshot: heartbeat + per-cycle metrics + (cached)
                // discovery state under one transaction. repos_watched uses
                // repo_states.len() — the authoritative in-memory set —
                // because new_worktrees added or stale ones removed since the
                // last full_scan are reflected there but not in `repos`.
                // last_discovery is unchanged since the last full_scan
                // (probe_watch_dirs runs only there).
                write_snapshot(&conn, &watcher_opt, repo_states.len(), config.poll_interval_secs, &failed_set, &last_discovery);
            }

            // Periodic full scan for missed events + new repos
            if last_full_scan.elapsed() >= Duration::from_secs(FULL_SCAN_SECS) {
                let (new_repos, new_disc) = full_scan(&config, &mut repo_states, &conn, &mut failed_set);
                repos = new_repos;
                last_discovery = new_disc;

                // Recreate watcher with updated repo list
                watcher_opt = RepoWatcher::new(&repos, config.worktree_dir_name.as_deref()).ok();
                if let Some(ref _w) = watcher_opt {
                    log::info!("Watching {} repos for changes", repos.len());
                }
                write_snapshot(&conn, &watcher_opt, repos.len(), config.poll_interval_secs, &failed_set, &last_discovery);
                maybe_send_daily_notification(&config, &conn);
                last_full_scan = Instant::now();
                debounce_map.clear();
            }
        } else {
            // Pure polling fallback (original behavior)
            std::thread::sleep(Duration::from_secs(config.poll_interval_secs));
            let (new_repos, new_disc) = full_scan(&config, &mut repo_states, &conn, &mut failed_set);
            repos = new_repos;
            last_discovery = new_disc;

            // Retry watcher setup on each full scan
            watcher_opt = RepoWatcher::new(&repos, config.worktree_dir_name.as_deref()).ok();
            if watcher_opt.is_some() {
                log::info!(
                    "File watcher now available, watching {} repos",
                    repos.len()
                );
                last_full_scan = Instant::now();
            }
            // Snapshot after watcher retry so mode flips to "watcher" the
            // moment the watcher self-heals — without this delay, doctor
            // would keep using the more lenient polling-mode threshold.
            write_snapshot(&conn, &watcher_opt, repos.len(), config.poll_interval_secs, &failed_set, &last_discovery);
            maybe_send_daily_notification(&config, &conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn setup_db() -> (Connection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let conn = crate::db::open_db(tmp.path()).unwrap();
        (conn, tmp)
    }

    fn test_config(enabled: bool, time: &str) -> Config {
        Config {
            notifications_enabled: enabled,
            notification_time: time.to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn notification_disabled_no_db_write() {
        let (conn, _tmp) = setup_db();
        let config = test_config(false, "00:01");
        maybe_send_daily_notification(&config, &conn);
        let today = chrono::Local::now().date_naive().to_string();
        let sent = crate::db::notification_was_sent(&conn, &today, "daily_summary").unwrap();
        assert!(!sent);
    }

    #[test]
    fn notification_future_time_no_db_write() {
        let (conn, _tmp) = setup_db();
        let config = test_config(true, "23:59");
        maybe_send_daily_notification(&config, &conn);
        let today = chrono::Local::now().date_naive().to_string();
        let sent = crate::db::notification_was_sent(&conn, &today, "daily_summary").unwrap();
        assert!(!sent);
    }

    #[test]
    fn notification_past_time_no_activity_no_db_write() {
        let (conn, _tmp) = setup_db();
        let config = test_config(true, "00:01");
        maybe_send_daily_notification(&config, &conn);
        let today = chrono::Local::now().date_naive().to_string();
        let sent = crate::db::notification_was_sent(&conn, &today, "daily_summary").unwrap();
        assert!(!sent);
    }

    fn rs() -> RepoState {
        RepoState::default()
    }

    #[test]
    fn prune_repo_states_evicts_missing_paths() {
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(PathBuf::from("/a"), rs());
        states.insert(PathBuf::from("/b"), rs());
        states.insert(PathBuf::from("/c"), rs());
        let current = [PathBuf::from("/a"), PathBuf::from("/c")];
        let evicted = prune_repo_states(&mut states, &current, &[]);
        assert_eq!(evicted, vec![PathBuf::from("/b")]);
        assert_eq!(states.len(), 2);
        assert!(states.contains_key(Path::new("/a")));
        assert!(states.contains_key(Path::new("/c")));
        assert!(!states.contains_key(Path::new("/b")));
    }

    #[test]
    fn prune_repo_states_keeps_present_paths() {
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(PathBuf::from("/a"), rs());
        let current = [PathBuf::from("/a")];
        let evicted = prune_repo_states(&mut states, &current, &[]);
        assert!(evicted.is_empty());
        assert_eq!(states.len(), 1);
    }

    #[test]
    fn prune_repo_states_evicts_all_when_current_empty() {
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(PathBuf::from("/a"), rs());
        states.insert(PathBuf::from("/b"), rs());
        let evicted = prune_repo_states(&mut states, &[], &[]);
        assert_eq!(evicted.len(), 2);
        assert!(states.is_empty());
    }

    #[test]
    fn prune_returns_evicted_paths_for_failed_set_sync() {
        // The full_scan caller uses the returned Vec to drop matching entries
        // from failed_set so a removed-from-config repo doesn't keep
        // contributing to `last_poll_repos_failed`.
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(PathBuf::from("/gone"), rs());
        states.insert(PathBuf::from("/here"), rs());
        let mut failed_set: std::collections::HashSet<PathBuf> = Default::default();
        failed_set.insert(PathBuf::from("/gone"));
        failed_set.insert(PathBuf::from("/here"));

        let evicted = prune_repo_states(&mut states, &[PathBuf::from("/here")], &[]);
        for p in &evicted {
            failed_set.remove(p);
        }

        assert_eq!(failed_set.len(), 1);
        assert!(failed_set.contains(Path::new("/here")));
        assert!(!failed_set.contains(Path::new("/gone")));
    }

    #[test]
    fn prune_skips_repos_under_unreliable_root() {
        // Codex round 5 finding: a transient TCC denial or network-mount blip
        // can make a watch_dir unreadable for one cycle. discover_repos won't
        // return its descendants. Without this guard, prune evicts them, and
        // git_ops::poll_repo treats the next successful poll as a first-poll
        // (HEAD seed + today's first 50 commits only) — losing every commit
        // that happened during the blackout.
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(PathBuf::from("/work/repo-a"), rs());
        states.insert(PathBuf::from("/work/repo-b"), rs());
        states.insert(PathBuf::from("/personal/old"), rs());

        // /work errored discovery this cycle. Keep its descendants.
        let unreliable = vec![PathBuf::from("/work")];
        // /personal probed clean but /personal/old isn't in current → real removal.
        let current: Vec<PathBuf> = vec![];

        let evicted = prune_repo_states(&mut states, &current, &unreliable);
        assert_eq!(evicted, vec![PathBuf::from("/personal/old")]);
        assert!(states.contains_key(Path::new("/work/repo-a")));
        assert!(states.contains_key(Path::new("/work/repo-b")));
        assert!(!states.contains_key(Path::new("/personal/old")));
    }

    #[test]
    fn prune_keeps_worktree_state_when_dot_git_unreadable() {
        // Codex round 11 [high]: the unreliable_root for an unreadable .git
        // pointer must be the WORKTREE ROOT, not the .git file. Pin the
        // contract: `repo_path.starts_with(unreliable_root)` must match when
        // the root is the worktree root, so prune keeps state.
        let wt = PathBuf::from("/Users/me/wt-feature");

        // Sanity: wrong root (`.git` suffix) would have evicted state.
        {
            let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
            states.insert(wt.clone(), rs());
            let wrong_root = wt.join(".git");
            let evicted = prune_repo_states(&mut states, &[], &[wrong_root]);
            assert!(
                !evicted.is_empty(),
                "wrong unreliable_root would evict state — confirms bug shape"
            );
        }

        // Post-fix: worktree-root unreliable_root keeps state.
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(wt.clone(), rs());
        let evicted = prune_repo_states(&mut states, &[], &[wt.clone()]);
        assert!(
            evicted.is_empty(),
            "normalized worktree-root unreliable_root must preserve state"
        );
        assert!(states.contains_key(&wt));
    }

    #[test]
    fn prune_unreliable_root_exact_match_kept() {
        // The watch_dir itself is also a valid repo path (some users add a
        // repo as a watch_dir directly). Exact-match against the unreliable
        // root must also be skipped.
        let mut states: HashMap<PathBuf, RepoState> = HashMap::new();
        states.insert(PathBuf::from("/work"), rs());
        let unreliable = vec![PathBuf::from("/work")];
        let evicted = prune_repo_states(&mut states, &[], &unreliable);
        assert!(evicted.is_empty());
        assert!(states.contains_key(Path::new("/work")));
    }

    #[test]
    fn is_definitely_stale_worktree_path_missing_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("never_existed");
        assert!(is_definitely_stale_worktree(&missing));
    }

    #[test]
    fn is_definitely_stale_worktree_dot_git_missing_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("wt");
        std::fs::create_dir(&dir).unwrap();
        // Path exists, .git is absent → confirmed stale.
        assert!(is_definitely_stale_worktree(&dir));
    }

    #[test]
    fn is_definitely_stale_worktree_dot_git_present_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("wt");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /some/path").unwrap();
        // Even with garbage gitdir contents, the .git file is present →
        // not definitely stale (could be transient resolve failure).
        assert!(!is_definitely_stale_worktree(&dir));
    }

    #[cfg(unix)]
    #[test]
    fn is_definitely_stale_worktree_permission_denied_returns_false() {
        // Codex round 9 [high]: a transient TCC / chmod 000 on the worktree
        // dir must NOT evict state. try_exists() on an unreadable parent dir
        // returns Err(PermissionDenied), not Ok(false). The probe must treat
        // that as "keep state" so the next clean cycle resumes from cursor.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("locked");
        std::fs::create_dir(&parent).unwrap();
        let wt = parent.join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /x").unwrap();
        // Strip parent perms so try_exists on `wt/.git` errors with EACCES.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = is_definitely_stale_worktree(&wt);

        // Restore before assertions so a panic doesn't leak.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !result,
            "permission-denied probe must keep worktree state, not evict it"
        );
    }

    #[test]
    fn notification_already_sent_idempotent() {
        let (conn, _tmp) = setup_db();
        let today = chrono::Local::now().date_naive().to_string();
        crate::db::record_notification_sent(&conn, &today, "daily_summary").unwrap();

        let config = test_config(true, "00:01");
        maybe_send_daily_notification(&config, &conn);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notification_log WHERE date = ?1",
                rusqlite::params![today],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
