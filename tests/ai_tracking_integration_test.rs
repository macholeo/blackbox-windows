use assert_cmd::Command;
use blackbox::db;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Apply the environment variables that etcetera actually consumes on the
/// current platform so spawned blackbox children use the isolated temp dirs.
#[cfg(unix)]
fn apply_isolated_env(cmd: &mut Command, config_dir: &Path, data_dir: &Path) {
    cmd.env("XDG_CONFIG_HOME", config_dir);
    cmd.env("XDG_DATA_HOME", data_dir);
}

#[cfg(windows)]
fn apply_isolated_env(cmd: &mut Command, _config_dir: &Path, data_dir: &Path) {
    // etcetera::choose_base_strategy on Windows uses the Windows strategy,
    // where config_dir == data_dir == %APPDATA%. Setting APPDATA to the temp
    // data root keeps both config and database isolated from the user profile.
    cmd.env("APPDATA", data_dir);
}

/// Helper: init config + open DB with a commit and multiple AI sessions.
/// Returns (TempDir, config_dir, data_dir).
fn setup_multi_tool_env() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");

    // Init config
    let mut init_cmd = Command::cargo_bin("blackbox").unwrap();
    apply_isolated_env(&mut init_cmd, &config_dir, &data_dir);
    init_cmd
        .args([
            "init",
            "--watch-dirs",
            "/tmp/repos",
            "--poll-interval",
            "300",
        ])
        .assert()
        .success();

    // Open DB and insert test data
    let db_dir = data_dir.join("blackbox");
    fs::create_dir_all(&db_dir).unwrap();
    let conn = db::open_db(&db_dir.join("blackbox.db")).unwrap();

    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();

    // Need at least one git_activity row so the repo appears in `today` output
    conn.execute(
        "INSERT INTO git_activity (repo_path, event_type, branch, commit_hash, author, message, timestamp)
         VALUES (?1, 'commit', 'main', 'abc123', 'tester', 'test commit', ?2)",
        rusqlite::params!["/tmp/repos/myproject", &now_str],
    )
    .unwrap();

    // Insert 3 AI sessions with different tools, same repo
    let started = now.to_rfc3339();
    db::insert_ai_session(
        &conn,
        "claude-code",
        "/tmp/repos/myproject",
        "claude-sess-001",
        &started,
    )
    .unwrap();
    db::insert_ai_session(
        &conn,
        "codex",
        "/tmp/repos/myproject",
        "codex-sess-001",
        &started,
    )
    .unwrap();
    db::insert_ai_session(
        &conn,
        "cursor",
        "/tmp/repos/myproject",
        "cursor-sess-001",
        &started,
    )
    .unwrap();

    (tmp, config_dir, data_dir)
}

#[test]
fn today_json_includes_all_tool_names() {
    let (_tmp, config_dir, data_dir) = setup_multi_tool_env();

    let mut today_cmd = Command::cargo_bin("blackbox").unwrap();
    apply_isolated_env(&mut today_cmd, &config_dir, &data_dir);
    let output = today_cmd
        .args(["today", "--format", "json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "today --format json should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout should be valid JSON: {e}\nstdout: {stdout}"));

    // Collect all tool names from ai_sessions across all repos
    let empty = vec![];
    let tools: Vec<&str> = v["repos"]
        .as_array()
        .expect("repos should be an array")
        .iter()
        .flat_map(|repo| {
            repo["ai_sessions"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .filter_map(|s| s["tool"].as_str())
        })
        .collect();

    assert!(
        tools.contains(&"claude-code"),
        "should contain claude-code session, got: {tools:?}"
    );
    assert!(
        tools.contains(&"codex"),
        "should contain codex session, got: {tools:?}"
    );
    assert!(
        tools.contains(&"cursor"),
        "should contain cursor session, got: {tools:?}"
    );
    assert_eq!(
        tools.len(),
        3,
        "should have exactly 3 AI sessions, got: {tools:?}"
    );
}

#[test]
fn today_json_ai_sessions_have_required_fields() {
    let (_tmp, config_dir, data_dir) = setup_multi_tool_env();

    let mut today_cmd = Command::cargo_bin("blackbox").unwrap();
    apply_isolated_env(&mut today_cmd, &config_dir, &data_dir);
    let output = today_cmd
        .args(["today", "--format", "json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "today --format json should exit 0, stderr: {}, stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    let empty = vec![];
    let sessions: Vec<&serde_json::Value> = v["repos"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|repo| repo["ai_sessions"].as_array().unwrap_or(&empty).iter())
        .collect();

    for session in &sessions {
        assert!(
            session["tool"].is_string(),
            "session should have tool field: {session}"
        );
        assert!(
            session["session_id"].is_string(),
            "session should have session_id: {session}"
        );
        assert!(
            session["started_at"].is_string(),
            "session should have started_at: {session}"
        );
        assert!(
            session["duration_minutes"].is_number(),
            "session should have duration_minutes: {session}"
        );
    }
}
