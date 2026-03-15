use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use serde::Deserialize;
use std::{io, path::PathBuf, time::Duration};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{interval, Instant};

#[derive(Debug, Clone)]
struct DashboardState {
    repo_name: String,
    current_branch: String,
    default_branch: String,
    last_commit_main: String,
    git_graph: Vec<String>,
    selected_commit_index: usize,
    selected_commit_show: Vec<String>,
    selected_commit_show_scroll: usize,
    prs: Vec<PullRequest>,
    recent_comments: Vec<Comment>,
    workflow_runs: Vec<WorkflowRun>,
    last_updated: chrono::DateTime<chrono::Local>,
    last_local_refresh: chrono::DateTime<chrono::Local>,
    last_remote_refresh: chrono::DateTime<chrono::Local>,
    error_msg: Option<String>,
    is_loading: bool,
    show_commit_overlay: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    author: Author,
    state: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Author {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Comment {
    id: u64,
    body: String,
    author: Author,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "pull_request_url")]
    pr_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: String,
    event: String,
}

#[derive(Debug, Clone, Copy)]
enum RefreshKind {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy)]
enum AppEvent {
    FsChanged,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            repo_name: "Unknown Repo".to_string(),
            current_branch: "main".to_string(),
            default_branch: "main".to_string(),
            last_commit_main: "Loading...".to_string(),
            git_graph: vec![],
            selected_commit_index: 0,
            selected_commit_show: vec![],
            selected_commit_show_scroll: 0,
            prs: vec![],
            recent_comments: vec![],
            workflow_runs: vec![],
            last_updated: chrono::Local::now(),
            last_local_refresh: chrono::Local::now(),
            last_remote_refresh: chrono::Local::now(),
            error_msg: None,
            is_loading: false,
            show_commit_overlay: false,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install().ok();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = DashboardState::default();
    let res = run_app(&mut terminal, app).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("Error: {:?}", err);
    }

    Ok(())
}

async fn run_app<B: Backend>(terminal: &mut Terminal<B>, mut app: DashboardState) -> Result<()> {
    let tick_rate = Duration::from_millis(100);
    let mut remote_refresh_interval = interval(Duration::from_secs(30));
    remote_refresh_interval.tick().await;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let _watcher = spawn_git_watcher(event_tx)?;

    refresh_data(&mut app, RefreshKind::Local).await;
    refresh_data(&mut app, RefreshKind::Remote).await;

    let mut pending_local_refresh: Option<Instant> = None;

    loop {
        terminal.draw(|f| draw_ui(f, &app))?;

        tokio::select! {
            _ = remote_refresh_interval.tick() => {
                app.is_loading = true;
                refresh_data(&mut app, RefreshKind::Remote).await;
                app.is_loading = false;
            }
            Some(app_event) = event_rx.recv() => {
                match app_event {
                    AppEvent::FsChanged => {
                        pending_local_refresh = Some(Instant::now() + Duration::from_millis(250));
                    }
                }
            }
            _ = tokio::time::sleep(tick_rate) => {
                if pending_local_refresh.is_some_and(|deadline| Instant::now() >= deadline) {
                    app.is_loading = true;
                    refresh_data(&mut app, RefreshKind::Local).await;
                    app.is_loading = false;
                    pending_local_refresh = None;
                }

                if crossterm::event::poll(Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            match key.code {
                                KeyCode::Char('q') => return Ok(()),
                                KeyCode::Esc => {
                                    if app.show_commit_overlay {
                                        app.show_commit_overlay = false;
                                    } else {
                                        return Ok(());
                                    }
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if app.show_commit_overlay {
                                        app.scroll_commit_overlay_up(1);
                                    } else {
                                        app.select_previous_commit();
                                    }
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if app.show_commit_overlay {
                                        app.scroll_commit_overlay_down(1);
                                    } else {
                                        app.select_next_commit();
                                    }
                                }
                                KeyCode::PageUp => app.scroll_commit_overlay_up(15),
                                KeyCode::PageDown => app.scroll_commit_overlay_down(15),
                                KeyCode::Home => app.scroll_commit_overlay_to_top(),
                                KeyCode::End => app.scroll_commit_overlay_to_bottom(),
                                KeyCode::Char('o') => {
                                    app.is_loading = true;
                                    open_selected_commit_on_github(&app).await;
                                    app.is_loading = false;
                                }
                                KeyCode::Char('s') => {
                                    app.is_loading = true;
                                    load_selected_commit_show(&mut app).await;
                                    app.is_loading = false;
                                }
                                KeyCode::Char('r') => {
                                    app.is_loading = true;
                                    refresh_data(&mut app, RefreshKind::Local).await;
                                    refresh_data(&mut app, RefreshKind::Remote).await;
                                    app.is_loading = false;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn refresh_data(app: &mut DashboardState, kind: RefreshKind) {
    match kind {
        RefreshKind::Local => {
            if let Err(err) = refresh_local_data(app).await {
                app.error_msg = Some(format!("Local refresh failed: {err}"));
            } else if app
                .error_msg
                .as_deref()
                .is_some_and(|msg| msg.starts_with("Local refresh failed:"))
            {
                app.error_msg = None;
            }
        }
        RefreshKind::Remote => {
            if let Err(err) = refresh_remote_data(app).await {
                app.error_msg = Some(format!("Remote refresh failed: {err}"));
            } else if app
                .error_msg
                .as_deref()
                .is_some_and(|msg| msg.starts_with("Remote refresh failed:"))
            {
                app.error_msg = None;
            }
        }
    }
}

async fn refresh_local_data(app: &mut DashboardState) -> Result<()> {
    app.last_updated = chrono::Local::now();
    app.last_local_refresh = app.last_updated;

    if let Ok(output) = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .await
    {
        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout);
            app.repo_name = extract_repo_name(&url).unwrap_or_else(|| "Local Repo".to_string());
        }
    }

    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .await?;
    if output.status.success() {
        app.current_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }

    app.default_branch = detect_default_branch()
        .await
        .unwrap_or_else(|| "main".to_string());

    let output = Command::new("git")
        .args([
            "log",
            &app.default_branch,
            "-1",
            "--format=%h | %s | %an | %ar",
        ])
        .output()
        .await?;
    if output.status.success() {
        app.last_commit_main = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }

    let output = Command::new("git")
        .args([
            "log",
            "--all",
            "--graph",
            "--decorate",
            "--oneline",
            "--color=never",
            "-15",
        ])
        .output()
        .await?;
    if output.status.success() {
        app.git_graph = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.to_string())
            .collect();
        app.selected_commit_index =
            clamp_selected_commit_index(app.selected_commit_index, &app.git_graph);
    }

    Ok(())
}

async fn refresh_remote_data(app: &mut DashboardState) -> Result<()> {
    app.last_updated = chrono::Local::now();
    app.last_remote_refresh = app.last_updated;

    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--limit",
            "5",
            "--json",
            "number,title,author,state,updatedAt,headRefName",
        ])
        .output()
        .await?;
    if output.status.success() {
        let json_str = String::from_utf8_lossy(&output.stdout);
        app.prs = serde_json::from_str::<Vec<PullRequest>>(&json_str)?;
    }

    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--limit",
            "5",
            "--json",
            "name,status,conclusion,createdAt,event",
        ])
        .output()
        .await?;
    if output.status.success() {
        let json_str = String::from_utf8_lossy(&output.stdout);
        app.workflow_runs = serde_json::from_str::<Vec<WorkflowRun>>(&json_str)?;
    }

    let output = Command::new("gh")
        .args([
            "api",
            "repos/{owner}/{repo}/issues/comments",
            "-q",
            ".[:5] | map({id: .id, body: .body[:100], author: {login: .user.login}, createdAt: .created_at})",
        ])
        .output()
        .await?;
    if output.status.success() {
        let json_str = String::from_utf8_lossy(&output.stdout);
        app.recent_comments = serde_json::from_str::<Vec<Comment>>(&json_str)?;
    }

    Ok(())
}

fn spawn_git_watcher(event_tx: mpsc::UnboundedSender<AppEvent>) -> Result<RecommendedWatcher> {
    let git_dir = resolve_git_dir()?;
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                    let _ = event_tx.send(AppEvent::FsChanged);
                }
                _ => {}
            }
        }
    })?;
    watcher.watch(&git_dir, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn resolve_git_dir() -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("unable to resolve .git directory"));
    }

    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(git_dir);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

async fn detect_default_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .await
        .ok()?;
    if output.status.success() {
        let reference = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Some(branch) = reference.rsplit('/').next() {
            return Some(branch.to_string());
        }
    }

    for candidate in ["main", "master"] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", candidate])
            .output()
            .await
            .ok()?;
        if output.status.success() {
            return Some(candidate.to_string());
        }
    }

    None
}

fn extract_repo_name(url: &str) -> Option<String> {
    let url = url.trim();
    let re = regex::Regex::new(r"github\.com[:/]([^/]+/[^/]+)\.git$").ok()?;
    re.captures(url)
        .or_else(|| {
            regex::Regex::new(r"github\.com[:/]([^/]+/[^/]+)$")
                .ok()?
                .captures(url)
        })
        .map(|cap| cap[1].to_string())
}

impl DashboardState {
    fn select_next_commit(&mut self) {
        if self.git_graph.is_empty() {
            self.selected_commit_index = 0;
            return;
        }
        self.selected_commit_index =
            (self.selected_commit_index + 1).min(self.git_graph.len().saturating_sub(1));
    }

    fn select_previous_commit(&mut self) {
        self.selected_commit_index = self.selected_commit_index.saturating_sub(1);
    }

    fn scroll_commit_overlay_up(&mut self, amount: usize) {
        if !self.show_commit_overlay {
            return;
        }
        self.selected_commit_show_scroll = self.selected_commit_show_scroll.saturating_sub(amount);
    }

    fn scroll_commit_overlay_down(&mut self, amount: usize) {
        if !self.show_commit_overlay {
            return;
        }
        self.selected_commit_show_scroll = self
            .selected_commit_show_scroll
            .saturating_add(amount)
            .min(self.selected_commit_show.len().saturating_sub(1));
    }

    fn scroll_commit_overlay_to_top(&mut self) {
        if self.show_commit_overlay {
            self.selected_commit_show_scroll = 0;
        }
    }

    fn scroll_commit_overlay_to_bottom(&mut self) {
        if self.show_commit_overlay && !self.selected_commit_show.is_empty() {
            self.selected_commit_show_scroll = self.selected_commit_show.len() - 1;
        }
    }
}

fn clamp_selected_commit_index(current: usize, graph: &[String]) -> usize {
    if graph.is_empty() {
        0
    } else {
        current.min(graph.len() - 1)
    }
}

fn selected_commit_sha(app: &DashboardState) -> Option<String> {
    let line = app.git_graph.get(app.selected_commit_index)?;
    let re = regex::Regex::new(r"\b[0-9a-f]{7,40}\b").ok()?;
    re.find(line).map(|m| m.as_str().to_string())
}

async fn open_selected_commit_on_github(app: &DashboardState) {
    let Some(sha) = selected_commit_sha(app) else {
        return;
    };

    let url = format!("https://github.com/{}/commit/{}", app.repo_name, sha);
    let _ = open_url(&url).await;
}

async fn load_selected_commit_show(app: &mut DashboardState) {
    let Some(sha) = selected_commit_sha(app) else {
        app.error_msg = Some("No commit selected to show".to_string());
        return;
    };

    match Command::new("git")
        .args([
            "show",
            "--stat",
            "--patch",
            "--color=never",
            "--format=fuller",
            &sha,
        ])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            app.selected_commit_show = text
                .lines()
                .take(120)
                .map(|line| line.to_string())
                .collect();
            app.selected_commit_show_scroll = 0;
            app.show_commit_overlay = true;
            app.error_msg = None;
        }
        Ok(_) => {
            app.error_msg = Some(format!("git show failed for {sha}"));
        }
        Err(err) => {
            app.error_msg = Some(format!("git show failed: {err}"));
        }
    }
}

async fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    command.output().await?;
    Ok(())
}

fn draw_ui(f: &mut Frame, app: &DashboardState) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(4),
        ])
        .split(f.size());

    let matrix_green = Style::default().fg(Color::Green);
    let bright_green = Style::default().fg(Color::LightGreen);
    let cyan = Style::default().fg(Color::Cyan);
    let sky = Style::default().fg(Color::LightBlue);
    let amber = Style::default().fg(Color::Yellow);
    let rose = Style::default().fg(Color::LightRed);
    let slate = Style::default().fg(Color::DarkGray);
    let header_border_style = Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD);
    let graph_border_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let branch_border_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let pr_border_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let status_border_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let comment_border_style = Style::default()
        .fg(Color::LightRed)
        .add_modifier(Modifier::BOLD);

    let header_text = format!(
        "⚡ {} | Branch: {} | Base: {} | Local: {} | GitHub: {}",
        app.repo_name.to_uppercase(),
        app.current_branch,
        app.default_branch,
        app.last_local_refresh.format("%H:%M:%S"),
        app.last_remote_refresh.format("%H:%M:%S")
    );

    let header = Paragraph::new(Line::from(vec![
        Span::styled(header_text, bright_green.add_modifier(Modifier::BOLD)),
        if app.is_loading {
            Span::styled(" ⟳ REFRESHING...", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("", Style::default())
        },
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(header_border_style)
            .title(Span::styled(" SYSTEM STATUS ", sky)),
    )
    .alignment(Alignment::Center);
    f.render_widget(header, main_layout[0]);

    let content_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(main_layout[1]);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(4)])
        .split(content_layout[0]);

    let graph_text = if app.git_graph.is_empty() {
        Text::from("No git history available")
    } else {
        Text::from(
            app.git_graph
                .iter()
                .enumerate()
                .map(|line| {
                    let (index, line) = line;
                    let is_selected = index == app.selected_commit_index;
                    let spans: Vec<Span> = line
                        .chars()
                        .map(|c| match c {
                            '|' | '/' | '\\' | '*' => {
                                if is_selected {
                                    Span::styled(
                                        c.to_string(),
                                        Style::default()
                                            .fg(Color::Black)
                                            .bg(Color::LightGreen)
                                            .add_modifier(Modifier::BOLD),
                                    )
                                } else {
                                    Span::styled(c.to_string(), bright_green)
                                }
                            }
                            _ => {
                                if is_selected {
                                    Span::styled(
                                        c.to_string(),
                                        Style::default()
                                            .fg(Color::Black)
                                            .bg(Color::LightYellow)
                                            .add_modifier(Modifier::BOLD),
                                    )
                                } else {
                                    Span::styled(c.to_string(), matrix_green)
                                }
                            }
                        })
                        .collect();
                    Line::from(spans)
                })
                .collect::<Vec<_>>(),
        )
    };

    let graph_widget = Paragraph::new(graph_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(graph_border_style)
                .title(Span::styled(" GIT GRAPH --all ", bright_green)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(graph_widget, left_chunks[0]);

    let commit_text = format!(
        "LAST COMMIT ON {}\n{}",
        app.default_branch.to_uppercase(),
        app.last_commit_main
    );
    let commit_widget = Paragraph::new(commit_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(branch_border_style)
                .title(Span::styled(" DEFAULT BRANCH ", amber)),
        )
        .style(amber)
        .alignment(Alignment::Left);
    f.render_widget(commit_widget, left_chunks[1]);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(content_layout[1]);

    let pr_items: Vec<ListItem> = app
        .prs
        .iter()
        .map(|pr| {
            let status_color = match pr.state.as_str() {
                "OPEN" => Color::Green,
                "CLOSED" => Color::Red,
                "MERGED" => Color::Magenta,
                _ => Color::Yellow,
            };
            let content = format!(
                "#{} {} [{}] by @{}",
                pr.number, pr.title, pr.head_ref_name, pr.author.login
            );
            ListItem::new(Line::from(vec![
                Span::styled("▶ ", Style::default().fg(Color::Magenta)),
                Span::styled(content, Style::default().fg(status_color)),
            ]))
        })
        .collect();

    let pr_list = List::new(pr_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(pr_border_style)
            .title(Span::styled(
                " PULL REQUESTS ",
                Style::default().fg(Color::Magenta),
            )),
    );
    f.render_widget(pr_list, right_chunks[0]);

    let status_items: Vec<ListItem> = app
        .workflow_runs
        .iter()
        .map(|run| {
            let symbol = match run.conclusion.as_deref() {
                Some("success") => "✓",
                Some("failure") => "✗",
                _ => "○",
            };
            let color = match run.conclusion.as_deref() {
                Some("success") => Color::Green,
                Some("failure") => Color::Red,
                _ => Color::Yellow,
            };
            let content = format!("{} {} - {}", symbol, run.name, run.status);
            ListItem::new(Span::styled(content, Style::default().fg(color)))
        })
        .collect();

    let status_list = List::new(status_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(status_border_style)
            .title(Span::styled(" CI/CD STATUS ", cyan)),
    );
    f.render_widget(status_list, right_chunks[1]);

    let comment_items: Vec<ListItem> = app
        .recent_comments
        .iter()
        .map(|comment| {
            let content = format!(
                "@{}: {}",
                comment.author.login,
                comment.body.chars().take(50).collect::<String>()
            );
            ListItem::new(Line::from(vec![
                Span::styled("💬 ", rose),
                Span::styled(content, Style::default().fg(Color::LightYellow)),
            ]))
        })
        .collect();

    let comment_list = List::new(comment_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(comment_border_style)
            .title(Span::styled(" RECENT COMMENTS ", rose)),
    );
    f.render_widget(comment_list, right_chunks[2]);

    let selected_commit = selected_commit_sha(app).unwrap_or_else(|| "none".to_string());
    let controls = vec![
        Line::from(vec![
            Span::styled("KEYS ", sky.add_modifier(Modifier::BOLD)),
            Span::styled("[Q]", bright_green.add_modifier(Modifier::BOLD)),
            Span::styled(" Quit  ", slate),
            Span::styled("[Esc]", amber.add_modifier(Modifier::BOLD)),
            Span::styled(" Quit  ", slate),
            Span::styled("[↑/↓ or J/K]", amber.add_modifier(Modifier::BOLD)),
            Span::styled(" Move commit  ", slate),
            Span::styled("[R]", rose.add_modifier(Modifier::BOLD)),
            Span::styled(" Refresh  ", slate),
            Span::styled("[S]", cyan.add_modifier(Modifier::BOLD)),
            Span::styled(" Show commit  ", slate),
            Span::styled("[PgUp/PgDn]", amber.add_modifier(Modifier::BOLD)),
            Span::styled(" Scroll overlay  ", slate),
            Span::styled(
                "[O]",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Open commit on GitHub", slate),
        ]),
        Line::from(vec![
            Span::styled("UPDATES ", sky.add_modifier(Modifier::BOLD)),
            Span::styled("Local git", bright_green),
            Span::styled(" real-time  ", slate),
            Span::styled("GitHub", cyan),
            Span::styled(" every 30s", slate),
        ]),
        Line::from(vec![
            Span::styled("SELECTED ", sky.add_modifier(Modifier::BOLD)),
            Span::styled(selected_commit, Style::default().fg(Color::LightYellow)),
        ]),
        Line::from(vec![
            Span::styled("OVERLAY ", sky.add_modifier(Modifier::BOLD)),
            Span::styled("[Esc]", amber.add_modifier(Modifier::BOLD)),
            Span::styled(" Close  ", slate),
            Span::styled("[Home/End]", bright_green.add_modifier(Modifier::BOLD)),
            Span::styled(" Jump top/bottom", slate),
        ]),
    ];
    let footer = Paragraph::new(controls).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(header_border_style)
            .title(Span::styled(" CONTROLS ", sky)),
    );
    f.render_widget(footer, main_layout[2]);

    if let Some(err) = &app.error_msg {
        let error_block = Paragraph::new(err.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red))
                    .title(" ERROR "),
            )
            .style(Style::default().fg(Color::Red).bg(Color::Black));

        let area = centered_rect(60, 20, f.size());
        f.render_widget(Clear, area);
        f.render_widget(error_block, area);
    }

    if app.show_commit_overlay {
        let area = centered_rect(88, 80, f.size());
        let visible_height = area.height.saturating_sub(2) as usize;
        let max_scroll = app
            .selected_commit_show
            .len()
            .saturating_sub(visible_height.max(1));
        let scroll = app.selected_commit_show_scroll.min(max_scroll);
        let commit_lines = if app.selected_commit_show.is_empty() {
            vec![Line::from("No commit details loaded")]
        } else {
            app.selected_commit_show
                .iter()
                .skip(scroll)
                .take(visible_height.max(1))
                .map(|line| Line::from(line.clone()))
                .collect()
        };
        let commit_overlay = Paragraph::new(commit_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(status_border_style)
                    .title(Span::styled(
                        format!(
                            " GIT SHOW [Esc closes] line {}-{} / {} ",
                            scroll.saturating_add(1),
                            (scroll + visible_height).min(app.selected_commit_show.len()),
                            app.selected_commit_show.len()
                        ),
                        cyan.add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(Clear, area);
        f.render_widget(commit_overlay, area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
