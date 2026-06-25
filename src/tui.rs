use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Sparkline, Table},
    DefaultTerminal, Frame,
};

use crate::query::{AiSessionInfo, RepoSummary};
#[cfg(test)]
use crate::query::ActivityEvent;

/// A flattened event for the recent-events feed.
#[derive(Debug, Clone)]
pub struct FeedEvent {
    pub timestamp: DateTime<Utc>,
    pub repo_name: String,
    pub event_type: String,
    pub branch: Option<String>,
    pub message: Option<String>,
    pub count: usize,
}

impl FeedEvent {
    fn color(&self) -> Color {
        match self.event_type.as_str() {
            "commit" => Color::Green,
            "branch_switch" => Color::Yellow,
            "merge" => Color::Blue,
            "review" => Color::Cyan,
            "ai_session" => Color::Magenta,
            _ => Color::White,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortMode {
    Recent,
    Time,
    Commits,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Recent => SortMode::Time,
            SortMode::Time => SortMode::Commits,
            SortMode::Commits => SortMode::Recent,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::Recent => "recent",
            SortMode::Time => "time",
            SortMode::Commits => "commits",
        }
    }
}

/// App state for the live TUI dashboard.
pub struct App {
    pub running: bool,
    pub daemon_running: bool,
    pub last_refresh: Instant,
    pub error: Option<String>,
    // Data
    pub repos: Vec<RepoSummary>,
    pub feed_events: Vec<FeedEvent>,
    pub total_time_mins: i64,
    pub active_repo: Option<String>,
    pub sparkline_data: Vec<u64>,
    // Sort
    pub sort_mode: SortMode,
    // Scroll
    pub events_state: ListState,
    // DB path for refresh
    pub db_path: Option<std::path::PathBuf>,
    pub session_gap_minutes: u64,
    pub first_commit_minutes: u64,
}

impl Default for App {
    fn default() -> Self {
        Self {
            running: true,
            daemon_running: false,
            last_refresh: Instant::now(),
            error: None,
            repos: Vec::new(),
            feed_events: Vec::new(),
            total_time_mins: 0,
            active_repo: None,
            sparkline_data: vec![0; 16],
            sort_mode: SortMode::Commits,
            events_state: ListState::default(),
            db_path: None,
            session_gap_minutes: 120,
            first_commit_minutes: 30,
        }
    }
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if daemon is running via PID file, falling back to launchd.
    pub fn refresh_daemon_status(&mut self) {
        self.daemon_running = match crate::config::data_dir() {
            Ok(data_dir) => match crate::daemon::is_daemon_running(&data_dir) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    self.error = Some(format!("Daemon status error: {e}"));
                    false
                }
            },
            Err(_) => false,
        } || crate::doctor::is_launchd_running().is_some();
        self.last_refresh = Instant::now();
    }

    /// Refresh activity data from DB.
    pub fn refresh_data(&mut self) {
        let Some(ref db_path) = self.db_path else {
            return;
        };
        let Ok(conn) = crate::db::open_db(db_path) else {
            return;
        };
        let (from, to) = crate::query::today_range();
        let Ok(mut repos) = crate::query::query_activity(
            &conn,
            from,
            to,
            self.session_gap_minutes,
            self.first_commit_minutes,
        ) else {
            return;
        };

        // Active repo = repo with most recent event (always by time, regardless of sort mode)
        self.active_repo = repos
            .iter()
            .max_by_key(|r| latest_timestamp(r))
            .map(|r| r.repo_name.clone());

        // Sort repos by selected mode
        match self.sort_mode {
            SortMode::Recent => repos.sort_by(|a, b| {
                latest_timestamp(b).cmp(&latest_timestamp(a))
            }),
            SortMode::Time => repos.sort_by(|a, b| b.estimated_time.cmp(&a.estimated_time)),
            SortMode::Commits => repos.sort_by(|a, b| b.commits.cmp(&a.commits)),
        };

        // Build feed events (latest 50 across all repos)
        let mut feed: Vec<FeedEvent> = Vec::new();
        for repo in &repos {
            for ev in &repo.events {
                feed.push(FeedEvent {
                    timestamp: ev.timestamp,
                    repo_name: repo.repo_name.clone(),
                    event_type: ev.event_type.clone(),
                    branch: ev.branch.clone(),
                    message: ev.message.clone(),
                    count: 1,
                });
            }
            for rev in &repo.reviews {
                feed.push(FeedEvent {
                    timestamp: rev.reviewed_at,
                    repo_name: repo.repo_name.clone(),
                    event_type: "review".into(),
                    branch: None,
                    message: Some(format!("{} PR #{}", rev.action, rev.pr_number)),
                    count: 1,
                });
            }
            for ses in &repo.ai_sessions {
                feed.push(FeedEvent {
                    timestamp: ses.started_at,
                    repo_name: repo.repo_name.clone(),
                    event_type: "ai_session".into(),
                    branch: None,
                    message: Some(format_session_msg(ses)),
                    count: 1,
                });
            }
        }
        feed.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        feed = collapse_similar_events(feed);
        feed.truncate(50);

        // Total time — global merge avoids double-counting concurrent repo work
        let total_time = crate::query::global_estimated_time(
            &repos, self.session_gap_minutes, self.first_commit_minutes, from, to,
        );
        self.total_time_mins = total_time.num_minutes();

        // Sparkline: 8 hours in 30-min buckets = 16 buckets
        self.sparkline_data = build_sparkline(&repos, from);

        self.repos = repos;
        self.feed_events = feed;
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match (code, modifiers) {
            (KeyCode::Char('q'), _) => self.running = false,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.running = false,
            (KeyCode::Char('r'), _) => {
                self.refresh_data();
                self.refresh_daemon_status();
            }
            (KeyCode::Char('s'), _) => {
                self.sort_mode = self.sort_mode.next();
                self.refresh_data();
            }
            (KeyCode::Char('j') | KeyCode::Down, _) => self.scroll_down(),
            (KeyCode::Char('k') | KeyCode::Up, _) => self.scroll_up(),
            _ => {}
        }
    }

    fn scroll_down(&mut self) {
        if self.feed_events.is_empty() {
            return;
        }
        let max = self.feed_events.len().saturating_sub(1);
        let i = self.events_state.selected().map_or(0, |i| (i + 1).min(max));
        self.events_state.select(Some(i));
    }

    fn scroll_up(&mut self) {
        let i = self
            .events_state
            .selected()
            .map_or(0, |i| i.saturating_sub(1));
        self.events_state.select(Some(i));
    }
}

fn latest_timestamp(repo: &RepoSummary) -> DateTime<Utc> {
    let git_latest = repo.events.last().map(|e| e.timestamp);
    let review_latest = repo.reviews.last().map(|r| r.reviewed_at);
    let session_latest = repo.ai_sessions.last().map(|s| s.started_at);
    [git_latest, review_latest, session_latest]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn format_session_msg(ses: &AiSessionInfo) -> String {
    if ses.ended_at.is_some() {
        let mins = ses.duration.num_minutes();
        format!("session ({mins}m)")
    } else {
        "session (active)".into()
    }
}

/// Collapse consecutive events with same repo+type+message within 5 min.
/// Different event types or targets stay separate. Preserves sorted order (newest first).
fn collapse_similar_events(mut events: Vec<FeedEvent>) -> Vec<FeedEvent> {
    if events.len() < 2 {
        return events;
    }
    let collapse_window = chrono::Duration::minutes(5);
    let mut collapsed: Vec<FeedEvent> = Vec::with_capacity(events.len());
    collapsed.push(events.remove(0));

    for ev in events {
        let last = collapsed.last_mut().unwrap();
        let same_key = last.repo_name == ev.repo_name
            && last.event_type == ev.event_type
            && last.message == ev.message;
        // Events sorted newest-first, so last.timestamp >= ev.timestamp
        let within_window = last.timestamp - ev.timestamp <= collapse_window;

        if same_key && within_window {
            last.count += 1;
            // Keep the newest timestamp (already there since sorted desc)
        } else {
            collapsed.push(ev);
        }
    }
    collapsed
}

/// Build sparkline data: 16 buckets of 30min over the past 8 hours.
fn build_sparkline(repos: &[RepoSummary], range_start: DateTime<Utc>) -> Vec<u64> {
    let now = Utc::now();
    // Use 8 hours ago or range_start, whichever is later
    let eight_hours_ago = now - chrono::Duration::hours(8);
    let start = if eight_hours_ago > range_start {
        eight_hours_ago
    } else {
        range_start
    };
    let bucket_secs = 1800i64; // 30 minutes
    let mut buckets = vec![0u64; 16];

    for repo in repos {
        for ev in &repo.events {
            let offset = (ev.timestamp - start).num_seconds();
            if offset >= 0 {
                let idx = (offset / bucket_secs) as usize;
                if idx < 16 {
                    buckets[idx] += 1;
                }
            }
        }
    }
    buckets
}

/// Entry point: initialize terminal, run event loop, restore on exit.
pub fn run_live() -> anyhow::Result<()> {
    let mut app = App::new();

    // Validate config + DB access up front; store error in app state if unavailable
    match crate::config::load_config() {
        Ok(config) => {
            app.session_gap_minutes = config.session_gap_minutes;
            app.first_commit_minutes = config.first_commit_minutes;
            match crate::config::data_dir() {
                Ok(d) => {
                    let db_path = d.join("blackbox.db");
                    if let Err(e) = crate::db::open_db(&db_path) {
                        app.error = Some(format!("DB error: {e}"));
                    } else {
                        app.db_path = Some(db_path);
                    }
                }
                Err(e) => {
                    app.error = Some(format!("Data dir error: {e}"));
                }
            }
        }
        Err(e) => {
            app.error = Some(format!("Config error: {e}"));
        }
    }

    app.refresh_daemon_status();
    app.refresh_data();

    let mut terminal = ratatui::init();
    let result = run_event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

/// Main event loop: poll input -> tick refresh -> render.
fn run_event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    let tick_rate = Duration::from_secs(1);

    while app.running {
        terminal.draw(|frame| render(frame, app))?;

        let timeout = tick_rate.saturating_sub(app.last_refresh.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == event::KeyEventKind::Press
        {
            app.handle_key(key.code, key.modifiers);
        }

        // Tick: refresh data every second
        if app.last_refresh.elapsed() >= tick_rate {
            app.refresh_daemon_status();
            app.refresh_data();
        }
    }
    Ok(())
}

/// Render the three-row layout: header, body, footer.
fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header_area, app);
    render_body(frame, body_area, app);
    render_footer(frame, footer_area, app.sort_mode);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let now = Local::now().format("%Y-%m-%d %I:%M:%S %p");
    let daemon_status = if app.daemon_running {
        Span::styled(" daemon: running ", Style::default().fg(Color::Green))
    } else {
        Span::styled(" daemon: stopped ", Style::default().fg(Color::Red))
    };

    let active = match &app.active_repo {
        Some(name) => Span::styled(
            format!(" active: {name} "),
            Style::default().fg(Color::Cyan),
        ),
        None => Span::styled(" no activity ", Style::default().fg(Color::DarkGray)),
    };

    let hours = app.total_time_mins / 60;
    let mins = app.total_time_mins % 60;
    let time_span = Span::styled(
        format!(" ~{hours}h {mins}m "),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let line = Line::from(vec![
        Span::styled(
            " blackbox live ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {now} ")),
        daemon_status,
        active,
        time_span,
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &mut App) {
    if let Some(ref err) = app.error {
        let content = vec![
            Line::raw(""),
            Line::styled(
                format!("  Error: {err}"),
                Style::default().fg(Color::Red),
            ),
            Line::raw(""),
            Line::raw("  Run 'blackbox setup' to configure, or check your data directory."),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Activity ");
        frame.render_widget(Paragraph::new(content).block(block), area);
        return;
    }

    // Split body: top for panels, bottom for sparkline
    let [panels_area, sparkline_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(4),
    ])
    .areas(area);

    // Split panels: left 40%, right 60%
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .areas(panels_area);

    render_repo_table(frame, left_area, app);
    render_events_feed(frame, right_area, app);
    render_sparkline(frame, sparkline_area, app);
}

fn render_repo_table(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Repos (today) ");

    if app.repos.is_empty() {
        let msg = vec![
            Line::raw(""),
            Line::styled(
                "  No activity recorded today",
                Style::default().fg(Color::DarkGray),
            ),
            Line::raw(""),
            Line::styled(
                "  Check daemon: blackbox status",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    }

    let header = Row::new(vec!["Repo", "Commits", "Time"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(0);

    let rows: Vec<Row> = app
        .repos
        .iter()
        .map(|r| {
            let mins = r.estimated_time.num_minutes();
            let h = mins / 60;
            let m = mins % 60;
            let time_str = if h > 0 {
                format!("~{h}h {m}m")
            } else {
                format!("~{m}m")
            };
            Row::new(vec![
                r.repo_name.clone(),
                r.commits.to_string(),
                time_str,
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(12),
        Constraint::Length(8),
        Constraint::Length(8),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));

    frame.render_widget(table, area);
}

fn render_events_feed(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Recent Events ");

    if app.feed_events.is_empty() {
        let msg = vec![
            Line::raw(""),
            Line::styled(
                "  No events yet",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    }

    let items: Vec<ListItem> = app
        .feed_events
        .iter()
        .map(|ev| {
            let local_ts = ev.timestamp.with_timezone(&Local);
            let ts = local_ts.format("%I:%M %p");
            let branch_str = ev
                .branch
                .as_deref()
                .map(|b| format!(" [{b}]"))
                .unwrap_or_default();
            let msg = ev
                .message
                .as_deref()
                .map(|m| {
                    let truncated: String = m.chars().take(40).collect();
                    let ellipsis = if m.chars().count() > 40 { "…" } else { "" };
                    format!(" {truncated}{ellipsis}")
                })
                .unwrap_or_default();

            let count_str = if ev.count > 1 {
                format!(" (x{})", ev.count)
            } else {
                String::new()
            };

            let line = Line::from(vec![
                Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:<14}", ev.repo_name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<14}", ev.event_type),
                    Style::default().fg(ev.color()),
                ),
                Span::styled(branch_str, Style::default().fg(Color::Yellow)),
                Span::raw(msg),
                Span::styled(count_str, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, area, &mut app.events_state);
}

fn render_sparkline(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Activity (8h) ");

    let sparkline = Sparkline::default()
        .block(block)
        .data(&app.sparkline_data)
        .style(Style::default().fg(Color::Green));

    frame.render_widget(sparkline, area);
}

fn render_footer(frame: &mut Frame, area: Rect, sort_mode: SortMode) {
    let line = Line::from(vec![
        Span::styled(" q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  "),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::raw(" refresh  "),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::raw(format!(" sort: {}  ", sort_mode.label())),
        Span::styled("j/k", Style::default().fg(Color::Yellow)),
        Span::raw(" scroll  "),
        Span::styled("\u{2191}/\u{2193}", Style::default().fg(Color::Yellow)),
        Span::raw(" scroll"),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_new_defaults() {
        let app = App::new();
        assert!(app.running);
        assert!(!app.daemon_running);
        assert!(app.error.is_none());
        assert!(app.repos.is_empty());
        assert!(app.feed_events.is_empty());
        assert_eq!(app.total_time_mins, 0);
        assert!(app.active_repo.is_none());
        assert_eq!(app.sparkline_data.len(), 16);
    }

    #[test]
    fn test_quit_on_q() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.running);
    }

    #[test]
    fn test_quit_on_ctrl_c() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.running);
    }

    #[test]
    fn test_other_keys_dont_quit() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.running);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.running);
    }

    #[test]
    fn test_scroll_down_empty() {
        let mut app = App::new();
        app.scroll_down();
        assert!(app.events_state.selected().is_none());
    }

    #[test]
    fn test_scroll_down_with_events() {
        let mut app = App::new();
        app.feed_events = vec![
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "a".into(),
                event_type: "commit".into(),
                branch: None,
                message: None,
                count: 1,
            },
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "b".into(),
                event_type: "commit".into(),
                branch: None,
                message: None,
                count: 1,
            },
        ];
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(0));
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(1));
        // Should not go past end
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(1));
    }

    #[test]
    fn test_scroll_up() {
        let mut app = App::new();
        app.feed_events = vec![
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "a".into(),
                event_type: "commit".into(),
                branch: None,
                message: None,
                count: 1,
            },
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "b".into(),
                event_type: "commit".into(),
                branch: None,
                message: None,
                count: 1,
            },
        ];
        app.events_state.select(Some(1));
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(0));
        // Should not go below 0
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(0));
    }

    #[test]
    fn test_arrow_keys_scroll() {
        let mut app = App::new();
        app.feed_events = vec![FeedEvent {
            timestamp: Utc::now(),
            repo_name: "a".into(),
            event_type: "commit".into(),
            branch: None,
            message: None,
            count: 1,
        }];
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(0));
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.events_state.selected(), Some(0));
    }

    #[test]
    fn test_feed_event_colors() {
        assert_eq!(
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "".into(),
                event_type: "commit".into(),
                branch: None,
                message: None,
                count: 1,
            }
            .color(),
            Color::Green
        );
        assert_eq!(
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "".into(),
                event_type: "branch_switch".into(),
                branch: None,
                message: None,
                count: 1,
            }
            .color(),
            Color::Yellow
        );
        assert_eq!(
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "".into(),
                event_type: "merge".into(),
                branch: None,
                message: None,
                count: 1,
            }
            .color(),
            Color::Blue
        );
        assert_eq!(
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "".into(),
                event_type: "review".into(),
                branch: None,
                message: None,
                count: 1,
            }
            .color(),
            Color::Cyan
        );
        assert_eq!(
            FeedEvent {
                timestamp: Utc::now(),
                repo_name: "".into(),
                event_type: "ai_session".into(),
                branch: None,
                message: None,
                count: 1,
            }
            .color(),
            Color::Magenta
        );
    }

    #[test]
    fn test_app_with_error() {
        let mut app = App::new();
        app.error = Some("test error".into());
        assert!(app.error.is_some());
        assert!(app.running);
    }

    #[test]
    fn test_build_sparkline_empty() {
        let data = build_sparkline(&[], Utc::now() - chrono::Duration::hours(8));
        assert_eq!(data.len(), 16);
        assert!(data.iter().all(|&v| v == 0));
    }

    #[test]
    fn test_render_does_not_panic() {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn test_render_with_error_does_not_panic() {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.error = Some("Config error: file not found".into());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn test_render_with_data_does_not_panic() {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.repos = vec![RepoSummary {
            repo_path: "/tmp/test".into(),
            repo_name: "test".into(),
            commits: 5,
            branches: vec!["main".into()],
            estimated_time: chrono::Duration::minutes(90),
            events: vec![ActivityEvent {
                event_type: "commit".into(),
                branch: Some("main".into()),
                commit_hash: Some("abc123".into()),
                message: Some("test commit".into()),
                timestamp: Utc::now(),
            }],
            pr_info: None,
            reviews: vec![],
            ai_sessions: vec![],
            presence_intervals: vec![],
            branch_switches: 0,
        }];
        app.feed_events = vec![FeedEvent {
            timestamp: Utc::now(),
            repo_name: "test".into(),
            event_type: "commit".into(),
            branch: Some("main".into()),
            message: Some("test commit".into()),
            count: 1,
        }];
        app.total_time_mins = 90;
        app.active_repo = Some("test".into());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn test_render_daemon_running() {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.daemon_running = true;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn test_latest_timestamp() {
        let repo = RepoSummary {
            repo_path: "/tmp/test".into(),
            repo_name: "test".into(),
            commits: 0,
            branches: vec![],
            estimated_time: chrono::Duration::zero(),
            events: vec![],
            pr_info: None,
            reviews: vec![],
            ai_sessions: vec![],
            presence_intervals: vec![],
            branch_switches: 0,
        };
        // Empty repo should return MIN_UTC
        assert_eq!(latest_timestamp(&repo), DateTime::<Utc>::MIN_UTC);
    }

    #[test]
    fn test_format_session_msg_active() {
        let ses = AiSessionInfo {
            tool: "claude-code".into(),
            session_id: "test".into(),
            started_at: Utc::now(),
            ended_at: None,
            last_active_at: None,
            duration: chrono::Duration::minutes(30),
            turns: None,
            segments: Vec::new(),
        };
        assert_eq!(format_session_msg(&ses), "session (active)");
    }

    #[test]
    fn test_format_session_msg_ended() {
        let ses = AiSessionInfo {
            tool: "claude-code".into(),
            session_id: "test".into(),
            started_at: Utc::now() - chrono::Duration::minutes(45),
            ended_at: Some(Utc::now()),
            last_active_at: None,
            duration: chrono::Duration::minutes(45),
            turns: Some(10),
            segments: Vec::new(),
        };
        assert_eq!(format_session_msg(&ses), "session (45m)");
    }
}
