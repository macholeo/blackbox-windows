use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Helper: create isolated config + data dirs (no env var mutation)
fn setup_dirs(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let config_dir = tmp.path().join("config").join("blackbox");
    let data_dir = tmp.path().join("data").join("blackbox");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    (config_dir, data_dir)
}

#[test]
fn test_pid_file_path() {
    let tmp = TempDir::new().unwrap();
    let (_config_dir, data_dir) = setup_dirs(&tmp);

    let path = blackbox::daemon::pid_file_path(&data_dir);
    assert!(path.to_string_lossy().ends_with("blackbox.pid"));
}

#[test]
fn test_is_daemon_running_no_file() {
    let tmp = TempDir::new().unwrap();
    let (_config_dir, data_dir) = setup_dirs(&tmp);

    let result = blackbox::daemon::is_daemon_running(&data_dir).unwrap();
    assert!(result.is_none());
}

#[cfg(unix)]
#[test]
fn test_is_daemon_running_stale_pid() {
    let tmp = TempDir::new().unwrap();
    let (_config_dir, data_dir) = setup_dirs(&tmp);

    // Write a PID file with a non-existent PID
    let pid_file = data_dir.join("blackbox.pid");
    std::fs::write(&pid_file, "999999").unwrap();

    let result = blackbox::daemon::is_daemon_running(&data_dir).unwrap();
    assert!(result.is_none(), "Stale PID should return None");
    assert!(!pid_file.exists(), "Stale PID file should be cleaned up");
}

#[test]
fn test_stop_when_not_running() {
    let tmp = TempDir::new().unwrap();
    let (_config_dir, data_dir) = setup_dirs(&tmp);

    let result = blackbox::daemon::stop_daemon(&data_dir);
    assert!(result.is_ok());
}

/// Codex round 5 [medium] regression: status must derive stall thresholds
/// from the daemon's persisted poll_interval, not the reader's config. If the
/// user's config.toml is malformed when status runs, the main.rs Status arm
/// falls back to Config::default() (poll_interval_secs = 1800). For a daemon
/// running poll_interval_secs = 7200 in polling mode, the stale poll at
/// `now - 25min` would land in Yellow under the default's 1800s threshold
/// while the daemon's real 7200s threshold says Green.
#[cfg(unix)]
#[test]
fn status_uses_persisted_poll_interval_over_reader_config() {
    use blackbox::daemon::HealthIndicator;
    let tmp = TempDir::new().unwrap();
    let (_config_dir, data_dir) = setup_dirs(&tmp);

    // Pretend daemon is running so compute_health doesn't short-circuit Red.
    let pid_file = data_dir.join("blackbox.pid");
    std::fs::write(&pid_file, std::process::id().to_string()).unwrap();

    // Persist a snapshot as if the daemon is running poll_interval_secs=7200
    // in polling mode. last_poll = 25 min ago — under polling threshold
    // (3*7200 = 21600s, ~6hr) but well over Config::default()'s 1800s.
    let db_path = data_dir.join("blackbox.db");
    let conn = blackbox::db::open_db(&db_path).unwrap();
    let snap = blackbox::poller::HealthSnapshot {
        last_poll_at: chrono::Utc::now() - chrono::Duration::minutes(25),
        poll_mode: "polling".into(),
        repos_watched: 5,
        effective_poll_interval_secs: 7200,
        poll_metrics: Default::default(),
        discovery_metrics: Default::default(),
    };
    blackbox::poller::write_health_snapshot(&conn, &snap).unwrap();
    drop(conn);

    // Reader-side config is the default (poll_interval_secs = 1800) — same
    // shape as what main.rs falls back to when load_config errors.
    let config = blackbox::config::Config::default();
    let status = blackbox::daemon::get_daemon_status(&data_dir, &config).unwrap();

    // 25min ago against the daemon's true 6hr threshold is Green. If status
    // ignored the persisted interval and used config.poll_interval_secs=1800,
    // 25min would breach the 3*1800=5400s threshold and land Red.
    assert_eq!(
        status.health,
        HealthIndicator::Green,
        "status must use persisted effective_poll_interval_secs=7200, not config default 1800; got {:?}",
        status.health
    );

    // Cleanup PID
    let _ = std::fs::remove_file(&pid_file);
}

/// Integration test using CLI binary — needs env vars for subprocess
#[cfg(unix)]
#[test]
fn test_start_stop_integration() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Create a config with empty watch_dirs and short poll interval
    let bb_config = config_dir.join("blackbox");
    std::fs::create_dir_all(&bb_config).unwrap();
    std::fs::write(
        bb_config.join("config.toml"),
        "watch_dirs = []\npoll_interval_secs = 60\n",
    )
    .unwrap();

    // Start daemon via CLI
    let bin = assert_cmd::cargo::cargo_bin("blackbox");
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir);
    let output = cmd.arg("start").output().unwrap();
    assert!(output.status.success(), "start failed: {}", String::from_utf8_lossy(&output.stderr));

    // PID file should exist
    let pid_file = data_dir.join("blackbox").join("blackbox.pid");
    // Give daemon a moment to fork and write PID
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(pid_file.exists(), "PID file should exist after start");

    // Status should say running
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir);
    let output = cmd.arg("status").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Running"),
        "status should say Running, got: {}",
        stdout
    );

    // Stop daemon
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir);
    let output = cmd.arg("stop").output().unwrap();
    assert!(output.status.success(), "stop failed: {}", String::from_utf8_lossy(&output.stderr));

    // PID file should be gone
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!pid_file.exists(), "PID file should be removed after stop");
}

/// Helper: create a git repo with an initial commit using git2
fn create_test_repo(path: &Path) -> git2::Repository {
    let repo = git2::Repository::init(path).unwrap();
    {
        let sig = git2::Signature::now("Test", "test@test.com").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            let file = path.join("README.md");
            std::fs::write(&file, "# test").unwrap();
            index.add_path(Path::new("README.md")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
            .unwrap();
    }
    repo
}

/// E2E: daemon discovers repo, detects new commit, records to DB
#[cfg(unix)]
#[test]
#[ignore] // slow -- run with --ignored
fn test_e2e_daemon_records_commit() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Create a test git repo
    let repo_dir = tmp.path().join("repos").join("myproject");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let repo = create_test_repo(&repo_dir);

    // Write config pointing at the repos dir with 10s poll interval
    let bb_config = config_dir.join("blackbox");
    std::fs::create_dir_all(&bb_config).unwrap();
    let config_content = format!(
        "watch_dirs = [\"{}\"]\npoll_interval_secs = 10\n",
        tmp.path().join("repos").to_string_lossy().replace('\\', "/")
    );
    std::fs::write(bb_config.join("config.toml"), &config_content).unwrap();

    // Start daemon
    let bin = assert_cmd::cargo::cargo_bin("blackbox");
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir);
    let output = cmd.arg("start").output().unwrap();
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for first poll cycle (seeds state)
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Make a new commit in the test repo
    let sig = git2::Signature::now("Test", "test@test.com").unwrap();
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        let file = repo_dir.join("new_file.txt");
        std::fs::write(&file, "new content").unwrap();
        index.add_path(Path::new("new_file.txt")).unwrap();
        index.write().unwrap();
        index.write_tree().unwrap()
    };
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "second commit for e2e test",
        &tree,
        &[&head_commit],
    )
    .unwrap();

    // Wait for another poll cycle to detect the new commit
    std::thread::sleep(std::time::Duration::from_secs(12));

    // Stop daemon
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("XDG_CONFIG_HOME", &config_dir)
        .env("XDG_DATA_HOME", &data_dir);
    let output = cmd.arg("stop").output().unwrap();
    assert!(
        output.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Open DB and verify commit was recorded
    let db_path = data_dir.join("blackbox").join("blackbox.db");
    assert!(db_path.exists(), "DB should exist");
    let conn = blackbox::db::open_db(&db_path).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM git_activity WHERE event_type = 'commit' AND message LIKE '%second commit%'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(
        count >= 1,
        "DB should contain the second commit, found {} matching rows",
        count
    );
}
