use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;

use crate::ai_tracking::is_ephemeral_path;
use crate::db;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFile {
    pub pid: u64,
    pub session_id: String,
    pub cwd: String,
    pub started_at: u64, // Unix timestamp in milliseconds
}

/// Encode a path the way Claude Code does: replace all non-alphanumeric chars with `-`.
/// e.g. /Users/brent.guistwite/repo → -Users-brent-guistwite-repo
pub fn encode_project_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Check if a process is still running (same pattern as daemon.rs stale PID detection).
/// Returns `None` when the platform cannot determine liveness, so callers do
/// not treat unsupported platforms as "definitely ended".
fn is_process_running(pid: u64) -> Option<bool> {
    match crate::platform::is_process_alive(pid as u32) {
        crate::platform::Liveness::Running => Some(true),
        crate::platform::Liveness::NotRunning => Some(false),
        crate::platform::Liveness::Unsupported => None,
    }
}

/// Read all session files from a sessions directory.
fn read_session_files(sessions_dir: &Path) -> Vec<SessionFile> {
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<SessionFile>(&content)
        {
            sessions.push(session);
        }
    }
    sessions
}

/// Convert millis timestamp to RFC3339 string.
fn millis_to_rfc3339(millis: u64) -> String {
    chrono::DateTime::from_timestamp_millis(millis as i64)
        .unwrap_or_default()
        .to_rfc3339()
}

/// Get file mtime as RFC3339 string.
fn mtime_rfc3339(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = modified.into();
    Some(dt.to_rfc3339())
}

/// Count turns in a JSONL conversation log file. Each line is a turn.
fn count_turns(jsonl_path: &Path) -> Option<i64> {
    let content = std::fs::read_to_string(jsonl_path).ok()?;
    Some(content.lines().filter(|l| !l.trim().is_empty()).count() as i64)
}

/// Read per-turn timestamps from a session's JSONL conversation log.
///
/// Each JSONL line is one turn; if it contains a `"timestamp"` field that parses
/// as RFC3339, include it. Malformed lines (bad JSON, missing timestamp,
/// unparseable date) are skipped silently — real logs occasionally have noise.
///
/// Scans every project dir under `projects_dir` for `<session_id>.jsonl`, since
/// we don't retain the session's original `cwd` in the DB. Returns sorted
/// timestamps (ascending). Missing file → empty Vec (graceful degradation).
pub fn read_turn_timestamps(
    projects_dir: &Path,
    session_id: &str,
) -> Vec<chrono::DateTime<chrono::Utc>> {
    let target_name = format!("{}.jsonl", session_id);
    let project_entries = match std::fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut timestamps = Vec::new();
    for entry in project_entries.flatten() {
        let jsonl = entry.path().join(&target_name);
        if !jsonl.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&jsonl) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ts_str = match value.get("timestamp").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                timestamps.push(ts.with_timezone(&chrono::Utc));
            }
        }
        break; // session_id is unique across project dirs
    }
    timestamps.sort();
    timestamps
}

/// Find the JSONL conversation log for a session in the projects directory.
fn find_session_log(projects_dir: &Path, session_cwd: &str, session_id: &str) -> Option<PathBuf> {
    let encoded = encode_project_path(session_cwd);
    let project_dir = projects_dir.join(&encoded);
    let jsonl = project_dir.join(format!("{}.jsonl", session_id));
    if jsonl.exists() {
        Some(jsonl)
    } else {
        None
    }
}

/// Map a session's cwd to a watched repo path. Returns `None` for ephemeral
/// temp dirs (test-spawned repos, OS caches). Returns the matched watched repo
/// path if cwd is inside one, or the cwd itself for real non-watched dirs.
fn map_to_repo(session_cwd: &str, watched_repos: &[PathBuf]) -> Option<String> {
    let cwd = Path::new(session_cwd);
    if is_ephemeral_path(cwd) {
        return None;
    }
    for repo in watched_repos {
        if cwd.starts_with(repo) || cwd == repo.as_path() {
            return Some(repo.to_string_lossy().to_string());
        }
    }
    Some(session_cwd.to_string())
}

/// Main entry point: poll Claude Code sessions and record to DB.
/// Reads ~/.claude/sessions/ for active sessions, detects ended sessions,
/// counts turns from conversation logs.
pub fn poll_claude_sessions(
    conn: &Connection,
    watched_repos: &[PathBuf],
) {
    poll_claude_sessions_with_paths(conn, watched_repos, None, None);
}

/// Testable version with explicit paths for claude_dir components.
pub fn poll_claude_sessions_with_paths(
    conn: &Connection,
    watched_repos: &[PathBuf],
    sessions_dir: Option<&Path>,
    projects_dir: Option<&Path>,
) {
    let home = match etcetera::home_dir() {
        Ok(h) => h,
        Err(_) => return,
    };

    let default_claude = home.join(".claude");
    let default_sessions = default_claude.join("sessions");
    let default_projects = default_claude.join("projects");
    let sessions_path = sessions_dir.unwrap_or(&default_sessions);
    let projects_path = projects_dir.unwrap_or(&default_projects);

    if !sessions_path.exists() {
        log::debug!("Claude sessions dir not found, skipping AI session tracking");
        return;
    }

    // Phase 1: Read active session files and record new ones
    let session_files = read_session_files(sessions_path);
    let mut active_pids: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    for session in &session_files {
        active_pids.insert(session.session_id.clone(), session.pid);

        let Some(repo_path) = map_to_repo(&session.cwd, watched_repos) else { continue };
        let started_at = millis_to_rfc3339(session.started_at);

        match db::insert_ai_session(conn, "claude-code", &repo_path, &session.session_id, &started_at) {
            Ok(true) => log::debug!("Recorded new AI session: {} in {}", session.session_id, repo_path),
            Ok(false) => {} // already exists
            Err(e) => log::warn!("Failed to insert AI session {}: {}", session.session_id, e),
        }
    }

    // Phase 1b: Update last_active_at from JSONL conversation log mtime
    for session in &session_files {
        if let Some(log_path) = find_session_log(projects_path, &session.cwd, &session.session_id)
            && let Some(mtime) = mtime_rfc3339(&log_path)
        {
            let _ = db::update_session_last_active(conn, &session.session_id, &mtime);
        }
    }

    // Phase 2: Check DB sessions that are still "active" (no ended_at)
    // If their PID is no longer running and not in current session files, mark ended
    let active_session_ids = match db::get_active_sessions(conn) {
        Ok(ids) => ids,
        Err(e) => {
            log::warn!("Failed to query active sessions: {}", e);
            return;
        }
    };

    for session_id in &active_session_ids {
        let still_running = active_pids
            .get(session_id)
            .and_then(|&pid| is_process_running(pid));

        // If the platform cannot determine liveness, leave the session open.
        // Do not fabricate a "not running" result.
        if still_running == Some(false) {
            let ended_at = chrono::Utc::now().to_rfc3339();

            // Try to get turn count from conversation log
            // We need the cwd for the session to find the log — query it from session files
            let turns = session_files
                .iter()
                .find(|s| s.session_id == *session_id)
                .and_then(|s| find_session_log(projects_path, &s.cwd, session_id))
                .and_then(|path| count_turns(&path));

            match db::update_session_ended(conn, session_id, &ended_at, turns) {
                Ok(true) => log::debug!("Marked AI session {} as ended (turns: {:?})", session_id, turns),
                Ok(false) => {} // already ended
                Err(e) => log::warn!("Failed to update AI session {}: {}", session_id, e),
            }
        }
    }
}
