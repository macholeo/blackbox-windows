# blackbox

Flight recorder for your dev day.

## What it does

Blackbox passively tracks your git activity across all your repos -- commits, branch switches, merges -- and estimates time spent per repo using a session-gap algorithm. Shows your current commit streak as ambient data in `today` output -- positive framing only, no nagging. Zero config after `blackbox init`.

### Multi-AI Tool Tracking

Blackbox detects and tracks sessions from multiple AI coding tools alongside git activity:

- **Claude Code** — session files in `~/.claude/sessions/`, turn counts from project JSONL
- **Codex CLI** — JSONL session files in `~/.codex/sessions/`, auto-detected via process + file timestamps
- **Copilot CLI** — `workspace.yaml` in `~/.copilot/session-state/`, event counts from `events.jsonl`
- **Cursor** — workspace storage parsing (`workspace.json`), process-based liveness detection
- **Windsurf** — workspace storage if available, falls back to process detection with synthetic sessions per active repo

Each tool's sessions appear in `today`, `week`, `month`, and `standup` output with a `tool` field identifying the source. Detection is passive — no configuration needed. Tools that aren't installed are silently skipped.

## Install

```
cargo install blackbox-cli
```

Note: build includes bundled SQLite, so compile may take a minute.

## Quick Start

```
blackbox setup
blackbox today
```

## Windows Quick Start

Build the release binary:

```powershell
cargo build --release --locked
```

The binary is at `target\release\blackbox.exe`.

```powershell
.\target\release\blackbox.exe init `
  --watch-dirs "C:\path\to\projects" `
  --poll-interval 30

.\target\release\blackbox.exe start
.\target\release\blackbox.exe status
.\target\release\blackbox.exe today
.\target\release\blackbox.exe standup
.\target\release\blackbox.exe stop
```

- `start` launches a normal user-level background process; no administrator permission is required.
- Configuration and database are stored in the Windows application-data directory (`%APPDATA%\blackbox`).
- Deleting the Blackbox application-data directory resets local data — back it up first if needed.
- `reload`, `install`, `uninstall`, Windows Service, and autostart are not supported on Windows.

## Commands

| Command | Description |
|---------|-------------|
| `init` | Create config interactively (`--watch-dirs`, `--poll-interval` for non-interactive) |
| `setup` | Full interactive onboarding wizard |
| `start` | Start background daemon |
| `stop` | Stop running daemon |
| `status` | Show daemon status: health, PID, uptime, last poll, repos watched, DB size, events today (`--format pretty\|json`) |
| `today` | Show today's git activity (`--json`, `--csv`, `--format pretty\|json\|csv`, `--summarize`) |
| `week` | Show this week's activity (`--json`, `--csv`, `--format pretty\|json\|csv`, `--summarize`) |
| `month` | Show this month's activity (`--json`, `--csv`, `--format pretty\|json\|csv`, `--summarize`) |
| `standup` | Slack/Teams-friendly activity summary (`--week`, `--json`, `--csv`, `--summarize`) |
| `heatmap` | GitHub-style contribution heatmap (`--weeks N`, default 52) |
| `live` | Interactive TUI dashboard |
| `doctor` | Run health checks: config, DB, daemon, gh CLI, shell hook, LLM key format, desktop notifications, AI tool detectors. Required failures exit non-zero; optional failures render as warnings. |
| `install` | Register as OS service (launchd on macOS, systemd on Linux) |
| `uninstall` | Remove OS service registration |
| `hook <shell>` | Print shell hook script for zsh, bash, or fish |
| `rhythm` | Work rhythm analysis (`--days N`, `--format pretty\|json`) |
| `reload` | Send SIGHUP to running daemon to reload config without restart |
| `focus` | Context-switch focus report (`--week` for weekly) |
| `repo <path>` | Single-repo deep dive: language breakdown, top files, time invested, branches, PRs (`--format pretty\|json`) |
| `prs` | PR cycle time metrics (`--days N`, `--repo <path>`, `--format pretty\|json`) |
| `churn` | Code churn rate analysis (`--window N`, `--repo <path>`, `--format pretty\|json\|csv`) |
| `insights` | LLM-powered behavioral analysis of activity patterns (`--window week\|month`, `--format pretty\|json`) |
| `perf-review` | LLM-powered performance review self-assessment (`--from YYYY-MM-DD`, `--to YYYY-MM-DD`; defaults to current quarter) |
| `commit-quality` | Commit message quality scores and trends (`--weeks N`, `--show-reverts`, `--format pretty\|json\|csv`) |
| `digest` | Structured weekly digest with week-over-week comparison (`--week N`, `--compare`, `--output-file`, `--notify`, `--format pretty\|json\|csv`) |
| `completions <shell>` | Generate shell completions |

## Shell Hooks

Shell hooks improve time-per-repo accuracy by tracking directory presence between commits.

```bash
# zsh - add to ~/.zshrc
eval "$(blackbox hook zsh)"

# bash - add to ~/.bashrc
eval "$(blackbox hook bash)"

# fish - add to ~/.config/fish/config.fish
blackbox hook fish | source
```

## Config Reference

Config lives at `~/.config/blackbox/config.toml`:

| Field | Default | Description |
|-------|---------|-------------|
| `watch_dirs` | (required) | List of directories to scan for git repos |
| `poll_interval_secs` | `300` | Seconds between daemon polls |
| `session_gap_minutes` | `120` | Minutes of inactivity before new session |
| `first_commit_minutes` | `30` | Time credit for first commit in a session |
| `streak_exclude_weekends` | `false` | Skip weekends when computing commit streak |
| `show_hints` | `true` | Show contextual next-step hints after pretty output |
| `notifications_enabled` | `false` | Enable opt-in OS desktop notifications |
| `notification_time` | `"17:00"` | Local time (HH:MM, 24h) to fire daily summary notification |

## Output Formats

**Pretty** (default in terminal):
```
Today - 3 repos, 12 commits, 4h 30m estimated  12-day streak

  myproject        8 commits   3h 15m
  other-repo       3 commits   1h 00m
  dotfiles         1 commit      15m
```

**JSON:** `blackbox today --json` (or `--format json`) -- structured output with repo details, commit counts, time estimates, and PR info (when gh CLI available).

**CSV:** `blackbox today --csv` (or `--format csv`) -- flat rows suitable for spreadsheets/pipelines.

**TTY auto-detection:** When stdout is piped or redirected (not a terminal), output defaults to JSON automatically. ANSI color codes are stripped in non-TTY mode. Explicit flags (`--json`, `--csv`, `--format`) always take precedence over auto-detection.

**Next-step hints:** After `today`, `week`, and `month` pretty output, blackbox shows dim/italic hint lines on stderr suggesting logical next commands — e.g. `blackbox start` if the daemon isn't running, `blackbox today --summarize` if LLM is configured, or `blackbox live` for the TUI dashboard. Hints are suppressed in JSON/CSV output, non-TTY sessions, when `--summarize` is used, and for the `standup` command. Disable with `show_hints = false` in config.

**Standup:** `blackbox standup` -- copy-paste-ready summary for Slack/Teams. Use `--week` for weekly and `--summarize` for LLM-generated summaries.

**Heatmap:** `blackbox heatmap` -- GitHub-style contribution grid in your terminal. Shows commit frequency across all tracked repos as color-intensity blocks. Use `--weeks N` (1-260) to control the range. Displays total commits, active days, and longest streak.

The `--summarize` flag is available on `today`, `week`, `month`, and `standup` to generate a natural-language summary of your activity using an LLM. Set `llm_api_key` in `~/.config/blackbox/config.toml`, or export `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` as environment variables. Without a key, AI features (`insights`, `perf-review`, `--summarize`) are unavailable but core tracking (`today`, `heatmap`, `prs`, etc.) works fine.

**Repo deep dive:** `blackbox repo <path>` -- onefetch-inspired single-repo analysis showing language breakdown (via git2 tree walk), most-changed files, all-time estimated time, branch activity, and PR history. Works on any git repo; repos not yet tracked by blackbox show git-derived data with an "untracked" indicator.

## Live Config Reload

Edit `~/.config/blackbox/config.toml` while the daemon is running, then:

```
blackbox reload
```

The daemon picks up the new config (watch dirs, poll interval, etc.) without restarting. If the config file is missing or invalid, the daemon logs a warning and keeps the previous config.

## Context-Switch Tracking

Blackbox tracks branch switches and estimates their focus cost using Gloria Mark's research (23 min per context switch). Noise is filtered — detached HEAD states, same-branch re-checkouts, and rapid round-trips (A→B→A within 2 min) are suppressed.

- **Pretty/JSON/CSV output** includes per-repo branch switch counts and total focus cost
- **`blackbox focus`** shows a dedicated focus report with per-repo switch breakdown
- **`blackbox standup`** flags high switch counts (≥5) with estimated focus cost

## Daily Summary Notifications

Blackbox can send an OS desktop notification with your daily activity summary at a configurable time. Opt in by adding to `~/.config/blackbox/config.toml`:

```toml
notifications_enabled = true
notification_time = "17:00"   # 24h local time, default 5 PM
```

The notification shows commit count, repo count, and estimated time — e.g. "12 commits across 3 repos — ~4h 30m". Rate-limited to once per day per type via a `notification_log` DB table, so daemon restarts won't re-fire. If no commits today, no notification is sent.

Supported on macOS (native notification center) and Linux (libnotify/dbus, requires DISPLAY or WAYLAND_DISPLAY). Headless/SSH sessions are detected and skipped silently.

## How It Works

A background daemon polls your watched directories for git repos, recording commits, branch switches, and merges to a local SQLite database. The CLI queries this database and estimates time using a session-gap algorithm: commits within `session_gap_minutes` of each other belong to the same work session, and the first commit in each session gets a configurable time credit.

When shell hooks are installed (see above), blackbox also records directory presence — when you enter and leave a repo directory. Presence data anchors git session start times to when you actually started working, rather than relying on estimated credits. This produces more accurate time estimates, especially for repos where you spend significant time before your first commit.

When `gh` CLI is available, PR data is collected during each poll and stored locally.

## PR Cycle Time

`blackbox prs` surfaces PR lifecycle metrics from data collected at poll time (requires `gh` CLI):

- **Cycle time** — created → merged (median across PRs)
- **Time to first review** — created → first non-pending review
- **PR size** — additions + deletions
- **Iteration count** — number of changes-requested reviews

```
blackbox prs               # last 30 days, pretty output
blackbox prs --days 7      # last 7 days
blackbox prs --repo /path  # single repo
blackbox prs --format json
```

Data is persisted in the `pr_snapshots` table and updated every full scan (≈30 min). The `--limit 50` cap means only the 50 most recent open/closed PRs per repo are captured.

## Work Rhythm

`blackbox rhythm` analyzes your commit timestamps to surface work patterns — a mirror, not a score. Includes:

- **Hour-of-day histogram** — when you commit most (local time)
- **Day-of-week histogram** — weekday vs weekend distribution
- **After-hours/weekend ratio** — commits outside core hours (09:00–18:00)
- **Session length distribution** — median, p90, mean session durations
- **Commit pattern** — bursty vs steady (coefficient of variation of inter-commit gaps)

```
blackbox rhythm              # last 30 days, pretty output
blackbox rhythm --days 7     # last 7 days
blackbox rhythm --format json
```

## Commit Streak

`blackbox today` shows your current consecutive-day commit streak as ambient info on the summary line. Streak is computed from local-time day boundaries — if you haven't committed yet today, your streak from yesterday is still alive until end of day.

Set `streak_exclude_weekends = true` in config to let weekends pass without counting as gaps (weekend commits still count toward the streak).

Streak of 0 shows nothing. No negative messaging — only what you've built.

## Code Churn

`blackbox churn` measures how much of your recently written code gets reworked within a configurable time window. High churn (>25%) may indicate unclear requirements or premature implementation; low churn (<10%) suggests stable, well-directed work.

The algorithm approximates churn using per-file line stats: for each pair of commits touching the same file within the window, `min(lines_added_by_A, lines_deleted_by_B)` counts as churned lines. This is the same heuristic GitClear uses.

```
blackbox churn                    # default 14-day window, pretty output
blackbox churn --window 7         # 7-day window
blackbox churn --repo /path/to/repo
blackbox churn --format json
```

Configure the default window in `~/.config/blackbox/config.toml`:

| Field | Default | Description |
|-------|---------|-------------|
| `churn_window_days` | `14` | Days to look back for churn detection |

## Daemon Status

`blackbox status` answers "is it working?" at a glance with a health indicator, process info, and activity summary:

```
✓ Running
  PID:           12345
  Uptime:        2h 15m
  Last poll:     3 minutes ago
  Repos watched: 8
  DB size:       156.3 KB
  Events today:  42
```

Health indicator: **Green** (✓) = daemon running, polled within 5 min; **Yellow** (⚠) = running but stale (5–30 min since last poll); **Red** (✗) = stopped or poll older than 30 min.

Works without a running daemon — shows last-known state from the database. Works without a config file or database (fresh install shows `Stopped` with empty fields).

```
blackbox status                # pretty output (default)
blackbox status --format json  # machine-readable JSON
```

## LLM Insights

`blackbox insights` uses an LLM to analyze your activity patterns and surface behavioral insights — commit cadence by day/hour, bug-fix ratios, commit message length trends, and PR merge times.

```
blackbox insights                 # this week, streamed LLM analysis
blackbox insights --window month  # this month
blackbox insights --format json   # raw data only, no LLM call
```

Requires `llm_api_key` in config (Anthropic or OpenAI). The `--format json` path skips the LLM entirely and emits the aggregated stats as JSON.

Insights include:
- Commit distribution by day-of-week and hour-of-day
- Bug-fix commit ratio (messages matching fix/bug/hotfix/patch/revert)
- Commit message length trend across the week
- PR merge times (when `gh` CLI is available)
- Per-repo breakdown: commits, estimated time, branches touched

Configure defaults in `~/.config/blackbox/config.toml`:

| Field | Default | Description |
|-------|---------|-------------|
| `insights_max_tokens` | `1024` | Max tokens for LLM response |
| `insights_window` | `"week"` | Default time window (`week` or `month`) |

## Performance Review Self-Assessment

`blackbox perf-review` generates an LLM-powered self-assessment by aggregating a quarter (or custom date range) of git activity, PR contributions, code review history, and AI tool usage. Commit messages are analyzed for recurring themes, and the structured context is fed to an LLM which produces a markdown-formatted self-assessment suitable for performance review season.

```
blackbox perf-review                                    # current quarter
blackbox perf-review --from 2025-01-01 --to 2025-03-31 # custom range
```

Output includes sections for Summary, Key Contributions, Technical Themes, Collaboration & Code Review, and Time Investment. Requires `llm_api_key` in config (Anthropic or OpenAI). Gracefully handles missing PR data (when `gh` CLI is unavailable) and sparse activity windows.

## Commit Message Quality

`blackbox commit-quality` scores every commit message 0–100 and tracks trends over time. Helps identify vague messages ("wip", "fix", "update") and correlate low-quality commits with reverts.

Scoring factors:
- **Subject length** — 10–72 chars optimal (+30 proportional)
- **Conventional commit prefix** (feat/fix/chore/docs/etc.) — +20
- **Body present** (blank line after subject) — +10
- **Penalties** — too short (-20), too long (-10), all-lowercase no punctuation (-5)
- **Merge/revert commits** — fixed score of 50, exempt from vague detection

```
blackbox commit-quality                    # last 8 weeks, pretty output
blackbox commit-quality --weeks 4          # last 4 weeks
blackbox commit-quality --show-reverts     # include revert correlation
blackbox commit-quality --format json      # machine-readable JSON
```

Scoring happens automatically during daemon polling — no extra setup needed. Existing commits are backfilled on first poll (up to 200).

## Weekly Digest

`blackbox digest` produces a structured weekly rollup aggregating git activity, PR contributions, review history, and AI sessions — useful for 1:1s, sprint retros, or personal tracking.

```
blackbox digest                       # current week
blackbox digest --week -1             # last week
blackbox digest --compare             # week-over-week comparison (default: on)
blackbox digest --output-file weekly.md
blackbox digest --format json
blackbox digest --notify              # send OS notification with summary
blackbox digest --summarize           # LLM-generated narrative summary
```

Includes commit counts, time estimates, repo breakdown, PR merge/open/review stats, and week-over-week deltas (e.g. "+3 commits, -1h estimated time"). The `--notify` flag sends the summary as an OS desktop notification. The `--output-file` flag writes to disk instead of stdout.

## License

MIT
