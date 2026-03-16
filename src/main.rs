use anyhow::{anyhow, Result};
use chrono::{DateTime, Local};
use eframe::{
    egui::{
        self, vec2, Align, CentralPanel, Color32, Context, CornerRadius, FontFamily, FontId, Key,
        Layout, RichText, ScrollArea, Sense, SidePanel, Stroke, TextEdit, TextStyle,
        TopBottomPanel, Ui, Vec2, ViewportBuilder, Window,
    },
    App, CreationContext, Frame, NativeOptions,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let native_options = NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("GitHub Dashboard")
            .with_inner_size([1500.0, 960.0])
            .with_min_inner_size([1200.0, 780.0]),
        ..Default::default()
    };

    eframe::run_native(
        "GitHub Dashboard",
        native_options,
        Box::new(|cc| Ok(Box::new(GithubDesktopApp::new(cc)))),
    )
    .map_err(|err| anyhow!("failed to start desktop app: {err}"))
}

#[derive(Debug, Clone)]
struct DashboardState {
    repo_name: String,
    current_branch: String,
    default_branch: String,
    last_commit_main: String,
    git_graph: Vec<String>,
    selected_commit_index: usize,
    selected_commit_show: Vec<String>,
    working_tree: Vec<FileChange>,
    selected_file_index: usize,
    selected_file_diff: Vec<String>,
    selected_file_hunks: Vec<DiffHunk>,
    selected_hunk_index: usize,
    prs: Vec<PullRequest>,
    recent_comments: Vec<Comment>,
    workflow_runs: Vec<WorkflowRun>,
    last_updated: DateTime<Local>,
    last_local_refresh: DateTime<Local>,
    last_remote_refresh: DateTime<Local>,
    error_msg: Option<String>,
    show_commit_overlay: bool,
    show_file_overlay: bool,
    show_commit_prompt: bool,
    commit_message_input: String,
    active_panel: ActivePanel,
}

#[derive(Debug)]
struct GithubDesktopApp {
    state: DashboardState,
    fs_rx: Receiver<AppEvent>,
    _watcher: Option<RecommendedWatcher>,
    last_remote_refresh_at: Instant,
}

#[derive(Debug, Clone)]
struct FileChange {
    path: String,
    status: String,
    additions: u32,
    deletions: u32,
}

#[derive(Debug, Clone)]
struct DiffHunk {
    header: String,
    patch: String,
    cached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePanel {
    Commits,
    Files,
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
        let now = Local::now();
        Self {
            repo_name: "Local Repo".to_string(),
            current_branch: "main".to_string(),
            default_branch: "main".to_string(),
            last_commit_main: "Loading...".to_string(),
            git_graph: vec![],
            selected_commit_index: 0,
            selected_commit_show: vec![],
            working_tree: vec![],
            selected_file_index: 0,
            selected_file_diff: vec![],
            selected_file_hunks: vec![],
            selected_hunk_index: 0,
            prs: vec![],
            recent_comments: vec![],
            workflow_runs: vec![],
            last_updated: now,
            last_local_refresh: now,
            last_remote_refresh: now,
            error_msg: None,
            show_commit_overlay: false,
            show_file_overlay: false,
            show_commit_prompt: false,
            commit_message_input: String::new(),
            active_panel: ActivePanel::Commits,
        }
    }
}

impl GithubDesktopApp {
    fn new(cc: &CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let (watcher, fs_rx) = spawn_git_watcher();
        let mut app = Self {
            state: DashboardState::default(),
            fs_rx,
            _watcher: watcher,
            last_remote_refresh_at: Instant::now() - Duration::from_secs(31),
        };
        app.refresh_data(RefreshKind::Local);
        app.refresh_data(RefreshKind::Remote);
        app
    }

    fn refresh_data(&mut self, kind: RefreshKind) {
        let result = match kind {
            RefreshKind::Local => refresh_local_data(&mut self.state),
            RefreshKind::Remote => refresh_remote_data(&mut self.state),
        };

        match result {
            Ok(()) => {
                if let Some(message) = self.state.error_msg.as_deref() {
                    let prefix = match kind {
                        RefreshKind::Local => "Local refresh failed:",
                        RefreshKind::Remote => "Remote refresh failed:",
                    };
                    if message.starts_with(prefix) {
                        self.state.error_msg = None;
                    }
                }
                if matches!(kind, RefreshKind::Remote) {
                    self.last_remote_refresh_at = Instant::now();
                }
            }
            Err(err) => {
                let label = match kind {
                    RefreshKind::Local => "Local refresh failed",
                    RefreshKind::Remote => "Remote refresh failed",
                };
                self.state.error_msg = Some(format!("{label}: {err}"));
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        if self.state.show_commit_prompt {
            return;
        }

        if ctx.input(|i| {
            i.key_pressed(Key::Tab)
                || i.key_pressed(Key::ArrowLeft)
                || i.key_pressed(Key::ArrowRight)
        }) {
            self.state.toggle_active_panel();
        }

        if ctx.input(|i| i.key_pressed(Key::ArrowUp)) {
            self.state.move_selection_up();
        }
        if ctx.input(|i| i.key_pressed(Key::ArrowDown)) {
            self.state.move_selection_down();
        }

        if ctx.input(|i| i.key_pressed(Key::C)) {
            self.state.show_commit_prompt = true;
            self.state.commit_message_input.clear();
            self.state.error_msg = None;
        }
        if ctx.input(|i| i.key_pressed(Key::P)) {
            push_current_branch(&mut self.state);
        }
        if ctx.input(|i| i.key_pressed(Key::S)) {
            load_selected_commit_show(&mut self.state);
        }
        if ctx.input(|i| i.key_pressed(Key::D)) {
            load_selected_file_diff(&mut self.state);
        }
        if ctx.input(|i| i.key_pressed(Key::F)) {
            revert_selected_file(&mut self.state);
        }
        if ctx.input(|i| i.key_pressed(Key::H)) {
            revert_selected_hunk(&mut self.state);
        }
        if ctx.input(|i| i.key_pressed(Key::O)) {
            open_selected_commit_on_github(&self.state);
        }
        if ctx.input(|i| i.key_pressed(Key::R)) {
            self.refresh_data(RefreshKind::Local);
            self.refresh_data(RefreshKind::Remote);
        }
        if ctx.input(|i| i.key_pressed(Key::OpenBracket)) {
            self.state.select_previous_hunk();
        }
        if ctx.input(|i| i.key_pressed(Key::CloseBracket)) {
            self.state.select_next_hunk();
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.state.show_commit_overlay = false;
            self.state.show_file_overlay = false;
        }
    }

    fn poll_background_events(&mut self) {
        let mut has_fs_change = false;
        while let Ok(event) = self.fs_rx.try_recv() {
            if matches!(event, AppEvent::FsChanged) {
                has_fs_change = true;
            }
        }
        if has_fs_change {
            self.refresh_data(RefreshKind::Local);
        }
        if self.last_remote_refresh_at.elapsed() >= Duration::from_secs(30) {
            self.refresh_data(RefreshKind::Remote);
        }
    }

    fn draw_header(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(
                RichText::new("GitHub Desktop Dashboard").color(Color32::from_rgb(95, 159, 255)),
            );
            ui.separator();
            ui.label(RichText::new(self.state.repo_name.to_uppercase()).strong());
            ui.separator();
            ui.label(format!("Branch: {}", self.state.current_branch));
            ui.separator();
            ui.label(format!("Base: {}", self.state.default_branch));
            ui.separator();
            ui.label(format!(
                "Local: {}",
                self.state.last_local_refresh.format("%H:%M:%S")
            ));
            ui.separator();
            ui.label(format!(
                "GitHub: {}",
                self.state.last_remote_refresh.format("%H:%M:%S")
            ));
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if toolbar_button(ui, "Refresh").clicked() {
                self.refresh_data(RefreshKind::Local);
                self.refresh_data(RefreshKind::Remote);
            }
            if toolbar_button(ui, "Commit").clicked() {
                self.state.show_commit_prompt = true;
                self.state.commit_message_input.clear();
                self.state.error_msg = None;
            }
            if toolbar_button(ui, "Push").clicked() {
                push_current_branch(&mut self.state);
            }
            if toolbar_button(ui, "Show Commit").clicked() {
                load_selected_commit_show(&mut self.state);
            }
            if toolbar_button(ui, "Open on GitHub").clicked() {
                open_selected_commit_on_github(&self.state);
            }
            if toolbar_button(ui, "Show Diff").clicked() {
                load_selected_file_diff(&mut self.state);
            }
            if toolbar_button(ui, "Revert File").clicked() {
                revert_selected_file(&mut self.state);
            }
            if toolbar_button(ui, "Revert Hunk").clicked() {
                revert_selected_hunk(&mut self.state);
            }
        });
    }

    fn draw_commit_sidebar(&mut self, ui: &mut Ui) {
        let panel_fill = Color32::from_rgb(41, 44, 52);
        let panel_rect = ui.max_rect();
        ui.painter().rect_filled(panel_rect, 0.0, panel_fill);
        ui.set_min_size(panel_rect.size());

        ui.scope(|ui| {
            ui.set_width(panel_rect.width());
            ui.set_min_height(panel_rect.height());

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(self.state.repo_name.clone())
                        .strong()
                        .color(Color32::from_rgb(222, 226, 234)),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(self.state.current_branch.clone()).color(Color32::from_gray(150)),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if sidebar_button(ui, "Push", false).clicked() {
                        push_current_branch(&mut self.state);
                    }
                });
            });

            ui.add_space(10.0);

            let change_count = self.state.working_tree.len();
            let change_summary = if change_count == 0 {
                "No changes".to_string()
            } else if change_count == 1 {
                "1 changed file".to_string()
            } else {
                format!("{change_count} changed files")
            };
            ui.label(RichText::new(change_summary).color(Color32::from_gray(165)));

            ui.add_space(10.0);
            soft_separator(ui);
            ui.add_space(8.0);

            ui.label(RichText::new("Recent Commits").color(Color32::from_gray(160)));
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 180.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ScrollArea::vertical()
                        .id_salt("sidebar_commits")
                        .auto_shrink([false; 2])
                        .show(ui, |ui| {
                            if self.state.git_graph.is_empty() {
                                ui.label(
                                    RichText::new("No git history available")
                                        .color(Color32::from_gray(120)),
                                );
                                return;
                            }
                            let mut open_commit_index: Option<usize> = None;
                            for (index, line) in self.state.git_graph.iter().enumerate() {
                                let selected = index == self.state.selected_commit_index;
                                let response = ui
                                    .horizontal(|ui| render_git_graph_line(ui, line, selected))
                                    .response
                                    .interact(Sense::click())
                                    .on_hover_text(
                                        "Click to select. Double-click to open commit details.",
                                    );
                                if response.clicked() {
                                    self.state.active_panel = ActivePanel::Commits;
                                    self.state.selected_commit_index = index;
                                }
                                if response.double_clicked() {
                                    self.state.active_panel = ActivePanel::Commits;
                                    self.state.selected_commit_index = index;
                                    open_commit_index = Some(index);
                                }
                            }
                            if let Some(index) = open_commit_index {
                                self.state.active_panel = ActivePanel::Commits;
                                self.state.selected_commit_index = index;
                                load_selected_commit_show(&mut self.state);
                            }
                        });
                },
            );

            ui.add_space(8.0);
            soft_separator(ui);
            ui.add_space(8.0);

            ui.label(RichText::new("Changed Files").color(Color32::from_gray(160)));
            ui.add_space(6.0);

            let footer_height = 124.0;
            let files_height = (ui.available_height() - footer_height).max(140.0);
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), files_height),
                Layout::top_down(Align::Min),
                |ui| {
                    ScrollArea::vertical()
                        .id_salt("sidebar_files")
                        .auto_shrink([false; 2])
                        .show(ui, |ui| {
                            if self.state.working_tree.is_empty() {
                                ui.add_space(12.0);
                                ui.centered_and_justified(|ui| {
                                    ui.label(
                                        RichText::new("No changes to commit")
                                            .color(Color32::from_gray(120)),
                                    );
                                });
                                return;
                            }

                            let max_total = self
                                .state
                                .working_tree
                                .iter()
                                .map(|change| change.additions + change.deletions)
                                .max()
                                .unwrap_or(1)
                                .max(1);

                            let mut open_file_index: Option<usize> = None;
                            for (index, change) in self.state.working_tree.iter().enumerate() {
                                let selected = index == self.state.selected_file_index;
                                let response = ui
                                    .horizontal(|ui| {
                                        render_file_change_row(ui, change, selected, max_total)
                                    })
                                    .response
                                    .interact(Sense::click())
                                    .on_hover_text(
                                        "Click to select. Double-click to open file diff.",
                                    );
                                if response.clicked() {
                                    self.state.active_panel = ActivePanel::Files;
                                    self.state.selected_file_index = index;
                                }
                                if response.double_clicked() {
                                    self.state.active_panel = ActivePanel::Files;
                                    self.state.selected_file_index = index;
                                    open_file_index = Some(index);
                                }
                            }
                            if let Some(index) = open_file_index {
                                self.state.active_panel = ActivePanel::Files;
                                self.state.selected_file_index = index;
                                load_selected_file_diff(&mut self.state);
                            }
                        });
                },
            );

            ui.add_space(8.0);
            soft_separator(ui);
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{}/{}",
                        self.state.repo_name, self.state.current_branch
                    ))
                    .color(Color32::from_gray(140)),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if sidebar_button(ui, "Commit Tracked", true).clicked() {
                        commit_all_changes(&mut self.state);
                    }
                });
            });

            ui.add_space(6.0);
            egui::Frame::new()
                .fill(Color32::from_rgb(31, 34, 40))
                .stroke(Stroke::new(1.0, Color32::from_rgb(72, 76, 86)))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    let response = ui.add(
                        TextEdit::singleline(&mut self.state.commit_message_input)
                            .hint_text("Enter commit message")
                            .desired_width(f32::INFINITY)
                            .font(TextStyle::Body),
                    );
                    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        commit_all_changes(&mut self.state);
                    }
                });
        });
    }

    fn draw_default_branch(&self, ui: &mut Ui) {
        card_frame(ui, false, "Default Branch", |ui| {
            ui.label(
                RichText::new(format!(
                    "Last commit on {}",
                    self.state.default_branch.to_uppercase()
                ))
                .color(Color32::from_rgb(255, 214, 10))
                .strong(),
            );
            ui.add_space(6.0);
            ui.label(self.state.last_commit_main.clone());
        });
    }

    fn draw_right_column(&self, ui: &mut Ui) {
        card_frame(ui, false, "Pull Requests", |ui| {
            for pr in &self.state.prs {
                let color = match pr.state.as_str() {
                    "OPEN" => Color32::from_rgb(80, 200, 120),
                    "CLOSED" => Color32::from_rgb(230, 57, 70),
                    "MERGED" => Color32::from_rgb(177, 156, 217),
                    _ => Color32::from_rgb(255, 214, 10),
                };
                ui.label(
                    RichText::new(format!(
                        "#{} {} [{}] by @{}",
                        pr.number,
                        trim_display_width(&pr.title, 36),
                        pr.head_ref_name,
                        pr.author.login
                    ))
                    .color(color),
                );
            }
        });

        ui.add_space(10.0);
        card_frame(ui, false, "CI / CD Status", |ui| {
            for run in &self.state.workflow_runs {
                let color = match run.conclusion.as_deref() {
                    Some("success") => Color32::from_rgb(80, 200, 120),
                    Some("failure") => Color32::from_rgb(230, 57, 70),
                    _ => Color32::from_rgb(255, 214, 10),
                };
                ui.label(
                    RichText::new(format!(
                        "{} - {}",
                        trim_display_width(&run.name, 32),
                        run.status
                    ))
                    .color(color),
                );
            }
        });

        ui.add_space(10.0);
        card_frame(ui, false, "Recent Comments", |ui| {
            for comment in &self.state.recent_comments {
                ui.label(format!(
                    "@{}: {}",
                    comment.author.login,
                    trim_display_width(&comment.body.replace('\n', " "), 52)
                ));
            }
        });
    }

    fn draw_footer(&mut self, ui: &mut Ui) {
        let selected_commit =
            selected_commit_sha(&self.state).unwrap_or_else(|| "none".to_string());
        let selected_file = selected_file_change(&self.state)
            .map(|change| change.path.clone())
            .unwrap_or_else(|| "clean".to_string());

        egui::Frame::NONE
            .fill(Color32::from_rgb(28, 30, 35))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("Selected commit: {selected_commit}"))
                            .color(Color32::from_gray(185)),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!("Selected file: {selected_file}"))
                            .color(Color32::from_gray(185)),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(
                            "Tab/arrows move selection. Enter commits from the left sidebar.",
                        )
                        .color(Color32::from_gray(140)),
                    );
                });
            });
    }

    fn draw_error_window(&mut self, ctx: &Context) {
        if let Some(message) = self.state.error_msg.clone() {
            Window::new("Error")
                .collapsible(false)
                .resizable(false)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.label(RichText::new(message).color(Color32::from_rgb(230, 57, 70)));
                    if ui.button("Close").clicked() {
                        self.state.error_msg = None;
                    }
                });
        }
    }

    fn draw_commit_window(&mut self, ctx: &Context) {
        if !self.state.show_commit_prompt {
            return;
        }

        let mut commit_now = false;
        let mut close = false;
        Window::new("Commit Message")
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label(format!("Branch: {}", self.state.current_branch));
                ui.add_space(6.0);
                let response = ui.add(
                    TextEdit::singleline(&mut self.state.commit_message_input)
                        .hint_text("Type commit message")
                        .desired_width(f32::INFINITY),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    commit_now = true;
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Commit All").clicked() {
                        commit_now = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if close || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.state.show_commit_prompt = false;
            self.state.commit_message_input.clear();
        }
        if commit_now {
            commit_all_changes(&mut self.state);
        }
    }

    fn draw_commit_overlay(&mut self, ctx: &Context) {
        if !self.state.show_commit_overlay {
            return;
        }
        Window::new("Commit Details")
            .default_size(Vec2::new(980.0, 760.0))
            .open(&mut self.state.show_commit_overlay)
            .show(ctx, |ui| {
                ScrollArea::vertical()
                    .id_salt("commit_overlay")
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for line in &self.state.selected_commit_show {
                            render_diff_line(ui, line);
                        }
                    });
            });
    }

    fn draw_file_overlay(&mut self, ctx: &Context) {
        if !self.state.show_file_overlay {
            return;
        }
        let title = if let Some(hunk) = self
            .state
            .selected_file_hunks
            .get(self.state.selected_hunk_index)
        {
            format!(
                "File Diff | hunk {}/{} {}",
                self.state.selected_hunk_index + 1,
                self.state.selected_file_hunks.len(),
                hunk.header
            )
        } else {
            "File Diff".to_string()
        };
        let mut open = self.state.show_file_overlay;
        let mut revert_file = false;
        let mut revert_hunk = false;
        let mut previous_hunk = false;
        let mut next_hunk = false;
        Window::new(title)
            .default_size(Vec2::new(1080.0, 780.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if toolbar_button(ui, "Revert File").clicked() {
                        revert_file = true;
                    }
                    if toolbar_button(ui, "Revert Hunk").clicked() {
                        revert_hunk = true;
                    }
                    if toolbar_button(ui, "Previous Hunk").clicked() {
                        previous_hunk = true;
                    }
                    if toolbar_button(ui, "Next Hunk").clicked() {
                        next_hunk = true;
                    }
                });
                soft_separator(ui);
                ScrollArea::vertical()
                    .id_salt("file_overlay")
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for line in &self.state.selected_file_diff {
                            render_diff_line(ui, line);
                        }
                    });
            });
        self.state.show_file_overlay = open;
        if previous_hunk {
            self.state.select_previous_hunk();
        }
        if next_hunk {
            self.state.select_next_hunk();
        }
        if revert_file {
            revert_selected_file(&mut self.state);
        }
        if revert_hunk {
            revert_selected_hunk(&mut self.state);
        }
    }
}

impl App for GithubDesktopApp {
    fn update(&mut self, ctx: &Context, _frame: &mut Frame) {
        self.poll_background_events();
        self.handle_shortcuts(ctx);
        ctx.request_repaint_after(Duration::from_millis(100));

        SidePanel::left("commit_sidebar")
            .resizable(false)
            .exact_width(360.0)
            .show(ctx, |ui| {
                self.draw_commit_sidebar(ui);
            });

        TopBottomPanel::top("header").show(ctx, |ui| self.draw_header(ui));
        TopBottomPanel::bottom("footer")
            .resizable(false)
            .exact_height(38.0)
            .show(ctx, |ui| self.draw_footer(ui));

        SidePanel::right("sidebar")
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| self.draw_right_column(ui));

        CentralPanel::default().show(ctx, |ui| self.draw_default_branch(ui));

        self.draw_commit_window(ctx);
        self.draw_commit_overlay(ctx);
        self.draw_file_overlay(ctx);
        self.draw_error_window(ctx);
    }
}

fn card_frame(ui: &mut Ui, selected: bool, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    let stroke = if selected {
        Stroke::new(1.0, Color32::from_rgb(81, 139, 254))
    } else {
        Stroke::new(1.0, Color32::from_rgb(57, 62, 72))
    };
    egui::Frame::new()
        .fill(Color32::from_rgb(31, 34, 40))
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.label(
                    RichText::new(title)
                        .strong()
                        .color(Color32::from_rgb(190, 194, 208)),
                );
                ui.add_space(8.0);
                add_contents(ui);
            });
        });
}

fn render_git_graph_line(ui: &mut Ui, line: &str, selected: bool) {
    let sha_re = Regex::new(r"\b[0-9a-f]{7,40}\b").ok();
    let mut rest = line;

    if selected {
        let rect = ui.available_rect_before_wrap();
        ui.painter()
            .rect_filled(rect, 6.0, Color32::from_rgb(55, 60, 70));
    }

    ui.horizontal_wrapped(|ui| {
        if let Some(re) = sha_re.as_ref() {
            if let Some(matched) = re.find(rest) {
                let prefix = &rest[..matched.start()];
                let sha = &rest[matched.start()..matched.end()];
                graph_label(ui, prefix, Color32::from_rgb(138, 201, 38), selected);
                graph_label(ui, sha, Color32::from_rgb(255, 214, 10), selected);
                rest = &rest[matched.end()..];
            }
        }

        if let Some(start) = rest.find('(') {
            let before_refs = &rest[..start];
            graph_label(ui, before_refs, Color32::from_rgb(138, 201, 38), selected);
            if let Some(end_rel) = rest[start..].find(')') {
                let refs = &rest[start..start + end_rel + 1];
                let ref_color = if refs.contains("HEAD") {
                    Color32::from_rgb(114, 239, 221)
                } else {
                    Color32::from_rgb(181, 131, 141)
                };
                graph_label(ui, refs, ref_color, selected);
                graph_label(
                    ui,
                    &rest[start + end_rel + 1..],
                    Color32::from_rgb(241, 250, 238),
                    selected,
                );
            } else {
                graph_label(
                    ui,
                    &rest[start..],
                    Color32::from_rgb(241, 250, 238),
                    selected,
                );
            }
        } else if !rest.is_empty() {
            graph_label(ui, rest, Color32::from_rgb(241, 250, 238), selected);
        }
    });
}

fn graph_label(ui: &mut Ui, text: &str, color: Color32, selected: bool) {
    let rich = RichText::new(text.to_string())
        .family(FontFamily::Monospace)
        .color(if selected {
            Color32::from_rgb(233, 236, 241)
        } else {
            color
        });
    ui.label(rich);
}

fn render_file_change_row(ui: &mut Ui, change: &FileChange, selected: bool, max_total: u32) {
    let status_color = match change.status.as_str() {
        "M" | "MM" | "AM" | " T" => Color32::from_rgb(255, 214, 10),
        "A" | "??" => Color32::from_rgb(80, 200, 120),
        "D" | "AD" => Color32::from_rgb(230, 57, 70),
        "R" => Color32::from_rgb(114, 239, 221),
        _ => Color32::LIGHT_GRAY,
    };
    let total = change.additions + change.deletions;
    let bar_total = 18usize;
    let filled = if total == 0 {
        0
    } else {
        ((total as f32 / max_total as f32) * bar_total as f32).ceil() as usize
    };
    let add_blocks = if total == 0 {
        0
    } else {
        ((change.additions as f32 / total as f32) * filled as f32).round() as usize
    }
    .min(filled);
    let del_blocks = filled.saturating_sub(add_blocks);
    let empty_blocks = bar_total.saturating_sub(filled);

    if selected {
        let rect = ui.available_rect_before_wrap();
        ui.painter()
            .rect_filled(rect, 6.0, Color32::from_rgb(55, 60, 70));
        ui.visuals_mut().override_text_color = Some(Color32::from_rgb(233, 236, 241));
    }
    ui.label(
        RichText::new(format!("{:>2}", change.status))
            .color(status_color)
            .strong(),
    );
    ui.label(RichText::new(trim_display_width(&change.path, 28)).family(FontFamily::Proportional));
    ui.label(RichText::new("█".repeat(add_blocks)).color(Color32::from_rgb(80, 200, 120)));
    ui.label(RichText::new("█".repeat(del_blocks)).color(Color32::from_rgb(230, 57, 70)));
    ui.label(RichText::new("░".repeat(empty_blocks)).color(Color32::from_gray(80)));
    ui.label(
        RichText::new(format!("+{} -{}", change.additions, change.deletions))
            .family(FontFamily::Monospace)
            .color(Color32::GRAY),
    );
    if selected {
        ui.visuals_mut().override_text_color = None;
    }
}

fn render_diff_line(ui: &mut Ui, line: &str) {
    let color = if line.starts_with('+') && !line.starts_with("+++") {
        Color32::from_rgb(80, 200, 120)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Color32::from_rgb(230, 57, 70)
    } else if line.starts_with("@@") {
        Color32::from_rgb(255, 214, 10)
    } else if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("=====")
    {
        Color32::from_rgb(114, 239, 221)
    } else {
        Color32::from_rgb(241, 250, 238)
    };
    ui.monospace(RichText::new(line.to_string()).color(color));
}

fn apply_theme(ctx: &Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = vec2(8.0, 8.0);
    style.spacing.button_padding = vec2(12.0, 6.0);
    style.spacing.indent = 10.0;
    style.visuals.panel_fill = Color32::from_rgb(36, 39, 46);
    style.visuals.window_fill = Color32::from_rgb(31, 34, 40);
    style.visuals.extreme_bg_color = Color32::from_rgb(24, 26, 31);
    style.visuals.faint_bg_color = Color32::from_rgb(45, 48, 56);
    style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(31, 34, 40);
    style.visuals.widgets.noninteractive.bg_stroke =
        Stroke::new(1.0, Color32::from_rgb(57, 62, 72));
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(58, 63, 73);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(74, 80, 92));
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(72, 78, 90);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(93, 101, 116));
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(81, 139, 254);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(81, 139, 254));
    style.visuals.widgets.open.bg_fill = Color32::from_rgb(72, 78, 90);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(6);
    style.visuals.widgets.open.corner_radius = CornerRadius::same(6);
    style.visuals.override_text_color = Some(Color32::from_rgb(221, 225, 230));
    style.visuals.selection.bg_fill = Color32::from_rgb(81, 139, 254);
    style.visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(201, 218, 248));

    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(24.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.5, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(13.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(12.0, FontFamily::Proportional),
    );

    ctx.set_style(style);
}

fn toolbar_button(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add_sized([96.0, 28.0], egui::Button::new(label))
}

fn sidebar_button(ui: &mut Ui, label: &str, primary: bool) -> egui::Response {
    let button = egui::Button::new(label).fill(if primary {
        Color32::from_rgb(64, 120, 244)
    } else {
        Color32::from_rgb(58, 63, 73)
    });
    ui.add_sized([108.0, 30.0], button)
}

fn soft_separator(ui: &mut Ui) {
    ui.separator();
}

fn refresh_local_data(state: &mut DashboardState) -> Result<()> {
    state.last_updated = Local::now();
    state.last_local_refresh = state.last_updated;

    if let Ok(output) = git_output(&["remote", "get-url", "origin"]) {
        state.repo_name = extract_repo_name(&output).unwrap_or_else(|| "Local Repo".to_string());
    }

    state.current_branch = git_output(&["branch", "--show-current"])?
        .trim()
        .to_string();
    state.default_branch = detect_default_branch().unwrap_or_else(|| "main".to_string());
    state.last_commit_main = git_output(&[
        "log",
        &state.default_branch,
        "-1",
        "--format=%h | %s | %an | %ar",
    ])?
    .trim()
    .to_string();

    state.git_graph = git_output(&[
        "log",
        "--all",
        "--graph",
        "--decorate",
        "--oneline",
        "--color=never",
        "-24",
    ])?
    .lines()
    .map(str::to_string)
    .collect();
    state.selected_commit_index = clamp_index(state.selected_commit_index, state.git_graph.len());

    state.working_tree = load_working_tree_changes()?;
    state.selected_file_index = clamp_index(state.selected_file_index, state.working_tree.len());

    if state.show_file_overlay {
        load_selected_file_diff(state);
    }

    Ok(())
}

fn refresh_remote_data(state: &mut DashboardState) -> Result<()> {
    state.last_updated = Local::now();
    state.last_remote_refresh = state.last_updated;

    let pr_json = git_gh_output(&[
        "pr",
        "list",
        "--limit",
        "5",
        "--json",
        "number,title,author,state,headRefName",
    ])?;
    state.prs = serde_json::from_str::<Vec<PullRequest>>(&pr_json)?;

    let run_json = git_gh_output(&[
        "run",
        "list",
        "--limit",
        "5",
        "--json",
        "name,status,conclusion",
    ])?;
    state.workflow_runs = serde_json::from_str::<Vec<WorkflowRun>>(&run_json)?;

    let comments_json = git_gh_output(&[
        "api",
        "repos/{owner}/{repo}/issues/comments",
        "-q",
        ".[:5] | map({body: .body[:100], author: {login: .user.login}})",
    ])?;
    state.recent_comments = serde_json::from_str::<Vec<Comment>>(&comments_json)?;

    Ok(())
}

fn load_working_tree_changes() -> Result<Vec<FileChange>> {
    let mut changes = parse_status_entries(&git_output(&["status", "--short"])?);
    let unstaged = parse_numstat_entries(&git_output(&["diff", "--numstat"])?);
    let staged = parse_numstat_entries(&git_output(&["diff", "--cached", "--numstat"])?);

    for (path, (adds, dels)) in unstaged.into_iter().chain(staged) {
        let entry = changes.entry(path.clone()).or_insert_with(|| FileChange {
            path,
            status: "??".to_string(),
            additions: 0,
            deletions: 0,
        });
        entry.additions += adds;
        entry.deletions += dels;
    }

    let mut files: Vec<FileChange> = changes.into_values().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn parse_status_entries(output: &str) -> BTreeMap<String, FileChange> {
    let mut changes = BTreeMap::new();
    for line in output.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = line[..2].trim().to_string();
        let raw_path = line[3..].trim();
        let path = raw_path
            .split(" -> ")
            .last()
            .unwrap_or(raw_path)
            .to_string();
        changes.insert(
            path.clone(),
            FileChange {
                path,
                status: if status.is_empty() {
                    "M".to_string()
                } else {
                    status
                },
                additions: 0,
                deletions: 0,
            },
        );
    }
    changes
}

fn parse_numstat_entries(output: &str) -> BTreeMap<String, (u32, u32)> {
    let mut counts = BTreeMap::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let path = parts.last().unwrap_or(&"").to_string();
        let additions = parts[0].parse::<u32>().unwrap_or(0);
        let deletions = parts[1].parse::<u32>().unwrap_or(0);
        let entry = counts.entry(path).or_insert((0, 0));
        entry.0 += additions;
        entry.1 += deletions;
    }
    counts
}

fn load_selected_commit_show(state: &mut DashboardState) {
    let Some(sha) = selected_commit_sha(state) else {
        state.error_msg = Some("No commit selected to show".to_string());
        return;
    };

    match git_output(&[
        "show",
        "--stat",
        "--patch",
        "--color=never",
        "--format=fuller",
        &sha,
    ]) {
        Ok(text) => {
            state.selected_commit_show = text.lines().take(220).map(str::to_string).collect();
            state.show_commit_overlay = true;
            state.show_file_overlay = false;
            state.error_msg = None;
        }
        Err(err) => state.error_msg = Some(format!("git show failed: {err}")),
    }
}

fn load_selected_file_diff(state: &mut DashboardState) {
    let Some(file) = selected_file_change(state).cloned() else {
        state.error_msg = Some("No file selected".to_string());
        return;
    };

    let mut lines = vec![format!(
        "{} (+{} / -{})",
        file.path, file.additions, file.deletions
    )];
    let mut hunks = vec![];

    if file.status == "??" {
        lines.push("Untracked file. Revert file will remove it from the working tree.".to_string());
    } else {
        for (label, args, cached) in [
            (
                "STAGED",
                vec![
                    "diff",
                    "--cached",
                    "--stat",
                    "--patch",
                    "--color=never",
                    "--",
                    file.path.as_str(),
                ],
                true,
            ),
            (
                "WORKTREE",
                vec![
                    "diff",
                    "--stat",
                    "--patch",
                    "--color=never",
                    "--",
                    file.path.as_str(),
                ],
                false,
            ),
        ] {
            if let Ok(output) = git_output(&args) {
                if !output.trim().is_empty() {
                    lines.push(String::new());
                    lines.push(format!("===== {label} ====="));
                    lines.extend(output.lines().map(str::to_string));
                }
            }
            if let Ok(output) = git_output(&build_hunk_args(&file.path, cached)) {
                hunks.extend(parse_diff_hunks(&output, cached));
            }
        }
    }

    if lines.len() == 1 {
        lines.push("No diff available for this file.".to_string());
    }

    state.selected_file_diff = lines;
    state.selected_file_hunks = hunks;
    state.selected_hunk_index =
        clamp_index(state.selected_hunk_index, state.selected_file_hunks.len());
    state.show_file_overlay = true;
    state.show_commit_overlay = false;
    state.error_msg = None;
}

fn build_hunk_args(path: &str, cached: bool) -> Vec<&str> {
    if cached {
        vec!["diff", "--cached", "-U0", "--color=never", "--", path]
    } else {
        vec!["diff", "-U0", "--color=never", "--", path]
    }
}

fn parse_diff_hunks(output: &str, cached: bool) -> Vec<DiffHunk> {
    if output.trim().is_empty() {
        return vec![];
    }

    let mut header_lines: Vec<String> = vec![];
    let mut current_hunk: Vec<String> = vec![];
    let mut hunks = vec![];

    for line in output.lines() {
        if line.starts_with("@@") {
            if !current_hunk.is_empty() {
                hunks.push(DiffHunk {
                    header: current_hunk[0].clone(),
                    patch: format!("{}\n{}\n", header_lines.join("\n"), current_hunk.join("\n")),
                    cached,
                });
                current_hunk.clear();
            }
            current_hunk.push(line.to_string());
        } else if current_hunk.is_empty() {
            header_lines.push(line.to_string());
        } else {
            current_hunk.push(line.to_string());
        }
    }

    if !current_hunk.is_empty() {
        hunks.push(DiffHunk {
            header: current_hunk[0].clone(),
            patch: format!("{}\n{}\n", header_lines.join("\n"), current_hunk.join("\n")),
            cached,
        });
    }

    hunks
}

fn commit_all_changes(state: &mut DashboardState) {
    let message = state.commit_message_input.trim().to_string();
    if message.is_empty() {
        state.error_msg = Some("Commit message cannot be empty".to_string());
        return;
    }

    if let Err(err) = run_git(&["add", "-A"]) {
        state.error_msg = Some(format!("git add failed: {err}"));
        return;
    }
    if let Err(err) = run_git(&["commit", "-m", &message]) {
        state.error_msg = Some(format!("git commit failed: {err}"));
        return;
    }

    state.show_commit_prompt = false;
    state.commit_message_input.clear();
    state.error_msg = None;
    if let Err(err) = refresh_local_data(state) {
        state.error_msg = Some(format!("Local refresh failed: {err}"));
    }
}

fn push_current_branch(state: &mut DashboardState) {
    let branch = state.current_branch.trim().to_string();
    if branch.is_empty() {
        state.error_msg = Some("No current branch available to push".to_string());
        return;
    }
    match run_git(&["push", "origin", &branch]) {
        Ok(()) => {
            state.error_msg = None;
            if let Err(err) = refresh_local_data(state) {
                state.error_msg = Some(format!("Local refresh failed: {err}"));
            }
            if let Err(err) = refresh_remote_data(state) {
                state.error_msg = Some(format!("Remote refresh failed: {err}"));
            }
        }
        Err(err) => state.error_msg = Some(format!("git push failed: {err}")),
    }
}

fn revert_selected_file(state: &mut DashboardState) {
    let Some(file) = selected_file_change(state).cloned() else {
        state.error_msg = Some("No file selected".to_string());
        return;
    };

    let result = if file.status == "??" {
        run_git(&["clean", "-f", "--", &file.path])
    } else {
        run_git(&[
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            &file.path,
        ])
    };

    match result {
        Ok(()) => {
            state.show_file_overlay = false;
            state.error_msg = None;
            if let Err(err) = refresh_local_data(state) {
                state.error_msg = Some(format!("Local refresh failed: {err}"));
            }
        }
        Err(err) => state.error_msg = Some(format!("Revert failed: {err}")),
    }
}

fn revert_selected_hunk(state: &mut DashboardState) {
    let Some(hunk) = state
        .selected_file_hunks
        .get(state.selected_hunk_index)
        .cloned()
    else {
        state.error_msg = Some("No diff hunk available to revert".to_string());
        return;
    };

    match apply_reverse_patch(&hunk.patch, hunk.cached) {
        Ok(()) => {
            state.error_msg = None;
            if let Err(err) = refresh_local_data(state) {
                state.error_msg = Some(format!("Local refresh failed: {err}"));
                return;
            }
            load_selected_file_diff(state);
        }
        Err(err) => state.error_msg = Some(format!("Hunk revert failed: {err}")),
    }
}

fn apply_reverse_patch(patch: &str, cached: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("apply").arg("-R");
    if cached {
        command.arg("--cached");
    }
    command.stdin(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(patch.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_string()))
    }
}

fn open_selected_commit_on_github(state: &DashboardState) {
    let Some(sha) = selected_commit_sha(state) else {
        return;
    };
    let url = format!("https://github.com/{}/commit/{}", state.repo_name, sha);
    let _ = open_url(&url);
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status()?;

    #[cfg(target_os = "linux")]
    let status = Command::new("xdg-open").arg(url).status()?;

    #[cfg(target_os = "windows")]
    let status = Command::new("cmd")
        .args(["/C", "start", "", url])
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("failed to open URL"))
    }
}

fn selected_commit_sha(state: &DashboardState) -> Option<String> {
    let line = state.git_graph.get(state.selected_commit_index)?;
    let re = Regex::new(r"\b[0-9a-f]{7,40}\b").ok()?;
    re.find(line).map(|m| m.as_str().to_string())
}

fn selected_file_change(state: &DashboardState) -> Option<&FileChange> {
    state.working_tree.get(state.selected_file_index)
}

fn clamp_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        current.min(len - 1)
    }
}

fn trim_display_width(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    format!("{}…", text.chars().take(width - 1).collect::<String>())
}

fn detect_default_branch() -> Option<String> {
    if let Ok(reference) = git_output(&["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = reference.trim().rsplit('/').next() {
            return Some(branch.to_string());
        }
    }

    for candidate in ["main", "master"] {
        if git_output(&["rev-parse", "--verify", candidate]).is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn extract_repo_name(url: &str) -> Option<String> {
    let url = url.trim();
    let re = Regex::new(r"github\.com[:/]([^/]+/[^/]+)\.git$").ok()?;
    re.captures(url)
        .or_else(|| {
            Regex::new(r"github\.com[:/]([^/]+/[^/]+)$")
                .ok()?
                .captures(url)
        })
        .map(|cap| cap[1].to_string())
}

fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() { stderr } else { stdout };
        Err(anyhow!(message))
    }
}

fn git_gh_output(args: &[&str]) -> Result<String> {
    let output = Command::new("gh").args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() { stderr } else { stdout };
        Err(anyhow!(message))
    }
}

fn run_git(args: &[&str]) -> Result<()> {
    git_output(args).map(|_| ())
}

fn spawn_git_watcher() -> (Option<RecommendedWatcher>, Receiver<AppEvent>) {
    let (tx, rx) = mpsc::channel();
    let Some(git_dir) = resolve_git_dir() else {
        return (None, rx);
    };

    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                    let _ = tx.send(AppEvent::FsChanged);
                }
                _ => {}
            }
        }
    })
    .ok()
    .and_then(|mut watcher| {
        watcher.watch(&git_dir, RecursiveMode::Recursive).ok()?;
        Some(watcher)
    });

    (watcher, rx)
}

fn resolve_git_dir() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(git_dir);
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

impl DashboardState {
    fn toggle_active_panel(&mut self) {
        self.active_panel = match self.active_panel {
            ActivePanel::Commits => ActivePanel::Files,
            ActivePanel::Files => ActivePanel::Commits,
        };
    }

    fn move_selection_down(&mut self) {
        match self.active_panel {
            ActivePanel::Commits => {
                self.selected_commit_index =
                    clamp_index(self.selected_commit_index + 1, self.git_graph.len())
            }
            ActivePanel::Files => {
                self.selected_file_index =
                    clamp_index(self.selected_file_index + 1, self.working_tree.len())
            }
        }
    }

    fn move_selection_up(&mut self) {
        match self.active_panel {
            ActivePanel::Commits => {
                self.selected_commit_index = self.selected_commit_index.saturating_sub(1)
            }
            ActivePanel::Files => {
                self.selected_file_index = self.selected_file_index.saturating_sub(1)
            }
        }
    }

    fn select_next_hunk(&mut self) {
        self.selected_hunk_index =
            clamp_index(self.selected_hunk_index + 1, self.selected_file_hunks.len());
    }

    fn select_previous_hunk(&mut self) {
        self.selected_hunk_index = self.selected_hunk_index.saturating_sub(1);
    }
}
