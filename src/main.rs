use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use serde::Deserialize;
use std::{io, time::Duration};
use tokio::process::Command;
use tokio::time::interval;

#[derive(Debug, Clone)]
struct DashboardState {
    repo_name: String,
    current_branch: String,
    default_branch: String,
    last_commit_main: String,
    git_graph: Vec<String>,
    prs: Vec<PullRequest>,
    recent_comments: Vec<Comment>,
    workflow_runs: Vec<WorkflowRun>,
    last_updated: chrono::DateTime<chrono::Local>,
    error_msg: Option<String>,
    is_loading: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    author: Author,
    state: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Author {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Comment {
    body: String,
    author: Author,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowRun {
    name: String,
    status: String,
    conclusion: Option<String>,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            repo_name: "Unknown Repo".to_string(),
            current_branch: "main".to_string(),
            default_branch: "main".to_string(),
            last_commit_main: "Loading...".to_string(),
            git_graph: vec![],
            prs: vec![],
            recent_comments: vec![],
            workflow_runs: vec![],
            last_updated: chrono::Local::now(),
            error_msg: None,
            is_loading: false,
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
    let mut refresh_interval = interval(Duration::from_secs(30));
    refresh_interval.tick().await;

    refresh_data(&mut app).await?;

    loop {
        terminal.draw(|f| draw_ui(f, &app))?;

        tokio::select! {
            _ = refresh_interval.tick() => {
                app.is_loading = true;
                refresh_data(&mut app).await?;
                app.is_loading = false;
            }
            _ = tokio::time::sleep(tick_rate) => {
                if crossterm::event::poll(Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                                KeyCode::Char('r') => {
                                    app.is_loading = true;
                                    refresh_data(&mut app).await?;
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

async fn refresh_data(app: &mut DashboardState) -> Result<()> {
    app.last_updated = chrono::Local::now();
    app.error_msg = None;

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

    if let Ok(output) = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .await
    {
        if output.status.success() {
            app.current_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }

    app.default_branch = detect_default_branch()
        .await
        .unwrap_or_else(|| "main".to_string());

    if let Ok(output) = Command::new("git")
        .args([
            "log",
            &app.default_branch,
            "-1",
            "--format=%h | %s | %an | %ar",
        ])
        .output()
        .await
    {
        if output.status.success() {
            app.last_commit_main = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }

    if let Ok(output) = Command::new("git")
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
        .await
    {
        if output.status.success() {
            app.git_graph = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|l| l.to_string())
                .collect();
        }
    }

    if let Ok(output) = Command::new("gh")
        .args([
            "pr",
            "list",
            "--limit",
            "5",
            "--json",
            "number,title,author,state,headRefName",
        ])
        .output()
        .await
    {
        if output.status.success() {
            let json_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(prs) = serde_json::from_str::<Vec<PullRequest>>(&json_str) {
                app.prs = prs;
            }
        }
    }

    if let Ok(output) = Command::new("gh")
        .args([
            "run",
            "list",
            "--limit",
            "5",
            "--json",
            "name,status,conclusion",
        ])
        .output()
        .await
    {
        if output.status.success() {
            let json_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(runs) = serde_json::from_str::<Vec<WorkflowRun>>(&json_str) {
                app.workflow_runs = runs;
            }
        }
    }

    if let Ok(output) = Command::new("gh")
        .args([
            "api",
            "repos/{owner}/{repo}/issues/comments",
            "-q",
            ".[:5] | map({body: .body[:100], author: {login: .user.login}})",
        ])
        .output()
        .await
    {
        if output.status.success() {
            let json_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(comments) = serde_json::from_str::<Vec<Comment>>(&json_str) {
                app.recent_comments = comments;
            }
        }
    }

    Ok(())
}

async fn detect_default_branch() -> Option<String> {
    if let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .await
    {
        if output.status.success() {
            let reference = String::from_utf8_lossy(&output.stdout);
            if let Some(branch) = reference.trim().rsplit('/').next() {
                return Some(branch.to_string());
            }
        }
    }

    for candidate in ["main", "master"] {
        if let Ok(output) = Command::new("git")
            .args(["rev-parse", "--verify", candidate])
            .output()
            .await
        {
            if output.status.success() {
                return Some(candidate.to_string());
            }
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

fn draw_ui(f: &mut Frame, app: &DashboardState) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(f.size());

    let matrix_green = Style::default().fg(Color::Green);
    let bright_green = Style::default().fg(Color::LightGreen);
    let dark_green = Style::default().fg(Color::DarkGray);
    let cyan = Style::default().fg(Color::Cyan);
    let border_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);

    let header_text = format!(
        "{} | Branch: {} | Base: {} | Last Update: {}",
        app.repo_name.to_uppercase(),
        app.current_branch,
        app.default_branch,
        app.last_updated.format("%H:%M:%S")
    );

    let header = Paragraph::new(Line::from(vec![
        Span::styled(header_text, bright_green.add_modifier(Modifier::BOLD)),
        if app.is_loading {
            Span::styled("  REFRESHING...", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("", Style::default())
        },
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(" SYSTEM STATUS ", cyan)),
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
                .map(|line| {
                    let spans: Vec<Span> = line
                        .chars()
                        .map(|c| match c {
                            '|' | '/' | '\\' | '*' => Span::styled(c.to_string(), bright_green),
                            _ => Span::styled(c.to_string(), matrix_green),
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
                .border_style(border_style)
                .title(Span::styled(" GIT GRAPH --all ", cyan)),
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
                .border_style(border_style)
                .title(Span::styled(" DEFAULT BRANCH ", cyan)),
        )
        .style(bright_green)
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
                "#{} {} by @{} [{}]",
                pr.number, pr.title, pr.author.login, pr.head_ref_name
            );
            ListItem::new(Line::from(vec![
                Span::styled("> ", bright_green),
                Span::styled(content, Style::default().fg(status_color)),
            ]))
        })
        .collect();

    let pr_list = List::new(pr_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(" PULL REQUESTS ", cyan)),
    );
    f.render_widget(pr_list, right_chunks[0]);

    let status_items: Vec<ListItem> = app
        .workflow_runs
        .iter()
        .map(|run| {
            let symbol = match run.conclusion.as_deref() {
                Some("success") => "OK",
                Some("failure") => "FAIL",
                _ => "RUN",
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
            .border_style(border_style)
            .title(Span::styled(" CI/CD STATUS ", cyan)),
    );
    f.render_widget(status_list, right_chunks[1]);

    let comment_items: Vec<ListItem> = app
        .recent_comments
        .iter()
        .map(|c| {
            let content = format!(
                "@{}: {}",
                c.author.login,
                c.body.chars().take(50).collect::<String>()
            );
            ListItem::new(Line::from(vec![
                Span::styled("> ", cyan),
                Span::styled(content, matrix_green),
            ]))
        })
        .collect();

    let comment_list = List::new(comment_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(" RECENT COMMENTS ", cyan)),
    );
    f.render_widget(comment_list, right_chunks[2]);

    let controls = "Controls: [Q]uit | [R]efresh | Auto-refresh: 30s";
    let footer = Paragraph::new(controls)
        .style(dark_green)
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::NONE));
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
