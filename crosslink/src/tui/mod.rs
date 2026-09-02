pub mod agents_tab;
pub mod config_tab;
pub mod issues_tab;
pub mod knowledge_tab;
pub mod milestones_tab;
pub mod tabs;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs as TabsWidget, Wrap},
    Frame,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::application::{LocalStateService, QueryService, RepositoryService};
use crate::db::Database;
use crate::sync::SyncManager;

pub const HIGHLIGHT_BG: Color = Color::Indexed(236);

pub fn format_relative_time(dt: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let diff = now.signed_duration_since(*dt);

    if diff.num_seconds() < 0 {
        "just now".to_string()
    } else if diff.num_seconds() < 60 {
        format!("{}s ago", diff.num_seconds())
    } else if diff.num_minutes() < 60 {
        format!("{}m ago", diff.num_minutes())
    } else if diff.num_hours() < 24 {
        format!("{}h ago", diff.num_hours())
    } else if diff.num_days() < 30 {
        format!("{}d ago", diff.num_days())
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Closed,
    All,
}

impl StatusFilter {
    pub const fn next(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::All,
            Self::All => Self::Open,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
            Self::All => "All",
        }
    }
}

#[cfg(test)]
pub const fn make_test_key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    KeyEvent {
        code,
        modifiers: KeyModifiers::empty(),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }
}

pub fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let end = max_len.saturating_sub(3);
        let truncated: String = s.chars().take(end).collect();
        format!("{truncated}...")
    }
}

pub fn format_event_description(event: &crate::events::Event) -> String {
    use crate::events::Event;
    match event {
        Event::IssueCreated { title, .. } => {
            format!("IssueCreated: {}", truncate_str(title, 40))
        }
        Event::LockClaimed {
            issue_display_id, ..
        } => format!("LockClaimed #{issue_display_id}"),
        Event::LockReleased {
            issue_display_id, ..
        } => format!("LockReleased #{issue_display_id}"),
        Event::IssueUpdated { title, .. } => {
            let t = title.as_deref().unwrap_or("(untitled)");
            format!("IssueUpdated: {}", truncate_str(t, 40))
        }
        Event::StatusChanged { new_status, .. } => {
            format!("StatusChanged \u{2192} {new_status}")
        }
        Event::DependencyAdded { .. } => "DependencyAdded".to_string(),
        Event::DependencyRemoved { .. } => "DependencyRemoved".to_string(),
        Event::RelationAdded { .. } => "RelationAdded".to_string(),
        Event::RelationRemoved { .. } => "RelationRemoved".to_string(),
        Event::MilestoneAssigned { .. } => "MilestoneAssigned".to_string(),
        Event::LabelAdded { label, .. } => format!("LabelAdded: {label}"),
        Event::LabelRemoved { label, .. } => format!("LabelRemoved: {label}"),
        Event::ParentChanged { .. } => "ParentChanged".to_string(),
        Event::CommentAdded { .. } => "CommentAdded".to_string(),
        Event::TimeEntryAdded { .. } => "TimeEntryAdded".to_string(),
        Event::IssueDeleted { .. } => "IssueDeleted".to_string(),
        Event::MilestoneCreated { name, .. } => {
            format!("MilestoneCreated: {}", truncate_str(name, 40))
        }
        Event::MilestoneClosed { .. } => "MilestoneClosed".to_string(),
        Event::MilestoneDeleted { .. } => "MilestoneDeleted".to_string(),
        Event::ScheduleChanged { .. } => "ScheduleChanged".to_string(),
    }
}

pub enum TabAction {
    Consumed,

    NotHandled,

    Quit,

    Flash(String),
}

pub trait Tab {
    fn title(&self) -> &'static str;
    fn render(&self, frame: &mut Frame, area: Rect);
    fn handle_key(&mut self, key: KeyEvent) -> TabAction;

    fn on_enter(&mut self);

    fn on_leave(&mut self);

    fn poll_updates(&mut self) {}

    fn force_refresh(&mut self) {
        self.on_leave();
        self.on_enter();
    }
}

pub fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait()
        });
    #[cfg(target_os = "linux")]
    let result = {
        let tools: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
            ("clip.exe", &[]),
        ];

        let mut last_result: Result<std::process::ExitStatus, std::io::Error> = Err(
            std::io::Error::new(std::io::ErrorKind::NotFound, "no clipboard tool found"),
        );

        for &(cmd, args) in tools {
            let attempt = std::process::Command::new(cmd)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    if let Some(ref mut stdin) = child.stdin {
                        stdin.write_all(text.as_bytes())?;
                    }
                    child.wait()
                });

            match &attempt {
                Ok(status) if status.success() => {
                    last_result = attempt;
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    last_result = attempt;
                }
            }
        }

        last_result
    };
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("clip.exe")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait()
        });
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: Result<std::process::ExitStatus, std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unsupported platform",
    ));

    result.is_ok_and(|s| s.success())
}

struct SyncResult {
    error: Option<String>,
}

pub struct App {
    tabs: Vec<Box<dyn Tab>>,
    active_tab: usize,
    show_help: bool,
    should_quit: bool,

    command_mode: bool,
    command_input: String,

    flash_message: Option<String>,

    tab_bar_area: Rect,

    crosslink_dir: PathBuf,

    last_sync: Instant,

    sync_rx: Option<mpsc::Receiver<SyncResult>>,

    syncing: bool,
}

impl App {
    pub fn new(
        db: &(impl LocalStateService + QueryService),
        crosslink_dir: &Path,
    ) -> anyhow::Result<Self> {
        let db_path = crosslink_dir.join("issues.db");
        let issues_tab = issues_tab::IssuesTab::new(db, &db_path)?;
        let agents_tab = agents_tab::AgentsTab::new(crosslink_dir);
        let knowledge_tab = knowledge_tab::KnowledgeTab::new(crosslink_dir);
        let milestones_tab = milestones_tab::MilestonesTab::new(db, &db_path);
        let config_tab = config_tab::ConfigTab::new(db, &db_path, crosslink_dir);
        let pipelines_tab = tabs::PlaceholderTab::new("Pipelines", 6);
        let tabs: Vec<Box<dyn Tab>> = vec![
            Box::new(issues_tab),
            Box::new(agents_tab),
            Box::new(knowledge_tab),
            Box::new(milestones_tab),
            Box::new(config_tab),
            Box::new(pipelines_tab),
        ];

        let mut app = App {
            tabs,
            active_tab: 0,
            show_help: false,
            should_quit: false,
            command_mode: false,
            command_input: String::new(),
            flash_message: None,
            tab_bar_area: Rect::default(),
            crosslink_dir: crosslink_dir.to_path_buf(),
            last_sync: Instant::now(),
            sync_rx: None,
            syncing: false,
        };
        app.tabs[0].on_enter();
        Ok(app)
    }

    fn next_tab(&mut self) {
        self.tabs[self.active_tab].on_leave();
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
        self.tabs[self.active_tab].on_enter();
    }

    fn prev_tab(&mut self) {
        self.tabs[self.active_tab].on_leave();
        if self.active_tab == 0 {
            self.active_tab = self.tabs.len() - 1;
        } else {
            self.active_tab -= 1;
        }
        self.tabs[self.active_tab].on_enter();
    }

    fn handle_key(&mut self, key: KeyEvent) {
        self.flash_message = None;

        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc => self.show_help = false,
                _ => {}
            }
            return;
        }

        if self.command_mode {
            self.handle_command_key(key);
            return;
        }

        match self.tabs[self.active_tab].handle_key(key) {
            TabAction::Consumed => return,
            TabAction::Quit => {
                self.should_quit = true;
                return;
            }
            TabAction::Flash(msg) => {
                self.flash_message = Some(msg);
                return;
            }
            TabAction::NotHandled => {}
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.prev_tab(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char(':') => {
                self.command_mode = true;
                self.command_input.clear();
            }
            KeyCode::Char('r') => {
                self.start_background_sync();
            }

            KeyCode::Char(c @ '1'..='6') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.tabs.len() && idx != self.active_tab {
                    self.tabs[self.active_tab].on_leave();
                    self.active_tab = idx;
                    self.tabs[self.active_tab].on_enter();
                }
            }
            _ => {}
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.command_mode = false;
                self.command_input.clear();
            }
            KeyCode::Enter => {
                let cmd = self.command_input.trim().to_string();
                self.command_mode = false;
                self.command_input.clear();
                self.execute_command(&cmd);
            }
            KeyCode::Backspace => {
                self.command_input.pop();
            }
            KeyCode::Char(c) => {
                self.command_input.push(c);
            }
            _ => {}
        }
    }

    fn execute_command(&mut self, cmd: &str) {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        match parts.first().copied() {
            Some("q" | "quit") => self.should_quit = true,
            Some("help" | "?") => self.show_help = true,
            Some("r" | "refresh") => {
                self.tabs[self.active_tab].on_leave();
                self.tabs[self.active_tab].on_enter();
                self.flash_message = Some("Refreshed".to_string());
            }
            Some("tab" | "t") => {
                if let Some(n) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
                    if n >= 1 && n <= self.tabs.len() && (n - 1) != self.active_tab {
                        self.tabs[self.active_tab].on_leave();
                        self.active_tab = n - 1;
                        self.tabs[self.active_tab].on_enter();
                    }
                } else {
                    self.flash_message = Some(format!("Usage: :tab <1-{}>", self.tabs.len()));
                }
            }
            Some(other) => {
                self.flash_message = Some(format!("Unknown command: {other}"));
            }
            None => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.flash_message = None;

                if mouse.row >= self.tab_bar_area.y
                    && mouse.row < self.tab_bar_area.y + self.tab_bar_area.height
                {
                    self.click_tab_bar(mouse.column);
                }
            }
            MouseEventKind::ScrollUp => {
                let up = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());

                let _ = self.tabs[self.active_tab].handle_key(up);
            }
            MouseEventKind::ScrollDown => {
                let down = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());

                let _ = self.tabs[self.active_tab].handle_key(down);
            }
            _ => {}
        }
    }

    fn start_background_sync(&mut self) {
        if self.syncing {
            return;
        }
        if let Err(error) = crate::reconcile::readiness::require_mutation_ready(&self.crosslink_dir)
        {
            self.flash_message = Some(format!("Sync blocked: {error}"));
            return;
        }
        self.syncing = true;
        self.flash_message = Some("Syncing...".to_string());
        let (tx, rx) = mpsc::channel();
        self.sync_rx = Some(rx);
        let crosslink_dir = self.crosslink_dir.clone();

        std::thread::spawn(move || {
            let permit =
                crate::reconcile::readiness::acquire_mutation_operation_permit(&crosslink_dir);
            let result = match SyncManager::new(&crosslink_dir) {
                Ok(sync_mgr) if permit.is_ok() => {
                    let _permit = permit.ok();
                    let _ = sync_mgr.init_cache();
                    match sync_mgr.fetch().and_then(|()| {
                        let db = Database::open(&crosslink_dir.join("issues.db"))?;
                        crate::hydration::hydrate_current_authority_under_operation(
                            &crosslink_dir,
                            &db,
                        )?;
                        Ok(())
                    }) {
                        Ok(()) => SyncResult { error: None },
                        Err(e) => SyncResult {
                            error: Some(e.to_string()),
                        },
                    }
                }
                Ok(_) => SyncResult {
                    error: permit.err().map(|error| error.to_string()),
                },
                Err(e) => SyncResult {
                    error: Some(e.to_string()),
                },
            };

            let _ = tx.send(result);
        });
    }

    fn poll_sync(&mut self) {
        let result = self.sync_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.syncing = false;
            self.sync_rx = None;
            self.last_sync = Instant::now();

            if let Some(err) = result.error {
                self.flash_message = Some(format!("Sync error: {err}"));
            } else {
                self.tabs[self.active_tab].force_refresh();
                self.flash_message = Some("Synced".to_string());
            }
        }
    }

    fn click_tab_bar(&mut self, col: u16) {
        let inner_x = self.tab_bar_area.x + 1;
        if col < inner_x {
            return;
        }
        let rel_col = col - inner_x;

        let mut offset: u16 = 0;
        for (idx, tab) in self.tabs.iter().enumerate() {
            let tab_width = tab.title().chars().count() as u16 + 2;
            let with_divider = tab_width + 1;
            if rel_col >= offset && rel_col < offset + with_divider {
                if idx != self.active_tab {
                    self.tabs[self.active_tab].on_leave();
                    self.active_tab = idx;
                    self.tabs[self.active_tab].on_enter();
                }
                return;
            }
            offset += with_divider;
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(frame.area());

        self.tab_bar_area = chunks[0];

        self.render_tab_bar(frame, chunks[0]);
        self.tabs[self.active_tab].render(frame, chunks[1]);

        if self.command_mode {
            self.render_command_bar(frame, chunks[2]);
        } else if let Some(ref msg) = self.flash_message {
            let flash = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(msg.as_str(), Style::default().fg(Color::Yellow)),
            ]))
            .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_widget(flash, chunks[2]);
        } else {
            Self::render_status_bar(frame, chunks[2]);
        }

        if self.show_help {
            Self::render_help_overlay(frame);
        }
    }

    fn render_tab_bar(&self, frame: &mut Frame, area: Rect) {
        let titles: Vec<Line> = self.tabs.iter().map(|t| Line::from(t.title())).collect();

        let tabs = TabsWidget::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(
                            "crosslink",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" tui ", Style::default().fg(Color::DarkGray)),
                    ])),
            )
            .select(self.active_tab)
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(tabs, area);
    }

    fn render_command_bar(&self, frame: &mut Frame, area: Rect) {
        let input_spans = vec![
            Span::styled(":", Style::default().fg(Color::Cyan)),
            Span::raw(&self.command_input),
            Span::styled("█", Style::default().fg(Color::White)),
        ];
        let bar = Paragraph::new(Line::from(input_spans))
            .style(Style::default().bg(Color::Black).fg(Color::White));
        frame.render_widget(bar, area);
    }

    fn render_status_bar(frame: &mut Frame, area: Rect) {
        let keys = vec![
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(":Quit  "),
            Span::styled("Tab", Style::default().fg(Color::Cyan)),
            Span::raw(":Next  "),
            Span::styled("S-Tab", Style::default().fg(Color::Cyan)),
            Span::raw(":Prev  "),
            Span::styled("1-6", Style::default().fg(Color::Cyan)),
            Span::raw(":Jump  "),
            Span::styled("?", Style::default().fg(Color::Cyan)),
            Span::raw(":Help  "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(":Sync"),
        ];

        let status = Paragraph::new(Line::from(keys))
            .style(Style::default().bg(Color::DarkGray).fg(Color::White));
        frame.render_widget(status, area);
    }

    fn render_help_overlay(frame: &mut Frame) {
        let area = centered_rect(60, 70, frame.area());

        frame.render_widget(ratatui::widgets::Clear, area);

        let help_text = vec![
            Line::from(Span::styled(
                "Keyboard Shortcuts",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Global",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  q / Ctrl-c    Quit"),
            Line::from("  Tab           Next tab"),
            Line::from("  Shift-Tab     Previous tab"),
            Line::from("  1-6           Jump to tab"),
            Line::from("  :             Command palette"),
            Line::from("  ?             Toggle this help"),
            Line::from("  Mouse         Click tabs, scroll wheel"),
            Line::from(""),
            Line::from(Span::styled(
                "Issues List",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Up/Down / j/k Navigate issues"),
            Line::from("  Enter         View issue details"),
            Line::from("  f             Cycle status filter"),
            Line::from("  s             Cycle sort order"),
            Line::from("  r             Sync & refresh"),
            Line::from("  /             Search (type to filter)"),
            Line::from("  Esc           Clear search"),
            Line::from("  t             Tree view"),
            Line::from(""),
            Line::from(Span::styled(
                "Issue Detail",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Esc           Back to list"),
            Line::from("  Up/Down / j/k Scroll"),
            Line::from("  y             Copy to clipboard"),
            Line::from(""),
            Line::from(Span::styled(
                "Agents Tab",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Up/Down / j/k Navigate agents"),
            Line::from("  Enter         View agent details"),
            Line::from("  v             Cycle view (Agents/Locks/Trust)"),
            Line::from("  r             Sync & refresh"),
            Line::from("  Esc           Back to list"),
            Line::from(""),
            Line::from(Span::styled(
                "Knowledge Tab",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Up/Down / j/k Navigate pages"),
            Line::from("  Enter         Read page"),
            Line::from("  /             Search pages"),
            Line::from("  t             Cycle tag filter"),
            Line::from("  y             Copy page to clipboard"),
            Line::from("  r             Sync & refresh"),
            Line::from("  Esc           Back to list"),
            Line::from(""),
            Line::from(Span::styled(
                "Milestones Tab",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Up/Down / j/k Navigate milestones"),
            Line::from("  Enter         View milestone details"),
            Line::from("  f             Cycle status filter"),
            Line::from("  y             Copy to clipboard"),
            Line::from("  r             Sync & refresh"),
            Line::from("  Esc           Back to list"),
            Line::from(""),
            Line::from(Span::styled(
                "Config Tab",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  Up/Down / j/k Scroll"),
            Line::from("  e             Full event log"),
            Line::from("  r             Sync & refresh"),
            Line::from("  Esc           Back to main"),
            Line::from(""),
            Line::from(Span::styled(
                "Command Palette (:)",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  :q / :quit    Quit"),
            Line::from("  :r / :refresh Refresh current tab"),
            Line::from("  :tab N        Jump to tab N"),
            Line::from("  :help         Show this help"),
            Line::from(""),
            Line::from(Span::styled(
                "Press ? or Esc to close",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let help = Paragraph::new(help_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(
                            "Help",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                    ])),
            )
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::White).bg(Color::Black));

        frame.render_widget(help, area);
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Self {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = io::stdout().execute(DisableMouseCapture);
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);

            original_hook(panic_info);
        }));
        TerminalGuard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);

        let _ = std::panic::take_hook();
    }
}

const PERIODIC_SYNC_INTERVAL: Duration = Duration::from_secs(30);

fn sync_projection_at_startup(crosslink_dir: &Path) -> anyhow::Result<bool> {
    let Ok(_permit) = crate::reconcile::readiness::acquire_mutation_operation_permit(crosslink_dir)
    else {
        return Ok(false);
    };
    let sync_mgr = SyncManager::new(crosslink_dir)?;
    sync_mgr.init_cache()?;
    sync_mgr.fetch()?;
    let writable = Database::open(&crosslink_dir.join("issues.db"))?;
    crate::hydration::hydrate_current_authority_under_operation(crosslink_dir, &writable)?;
    Ok(true)
}

pub fn run(
    db: &Database,
    crosslink_dir: &Path,
    database_unavailable: Option<&str>,
) -> anyhow::Result<()> {
    eprint!("Syncing...");
    let synchronized = match sync_projection_at_startup(crosslink_dir) {
        Ok(true) => {
            eprintln!(" done.");
            true
        }
        Ok(false) => {
            eprintln!(" unavailable while repository reconciliation is pending.");
            false
        }
        Err(error) => {
            eprintln!(" failed: {error:#}");
            return Err(error);
        }
    };

    let _guard = TerminalGuard::new();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;

    let refreshed = (synchronized && database_unavailable.is_some())
        .then(|| Database::open_read_only(&crosslink_dir.join("issues.db")))
        .transpose()?;
    let mut app = app_with_database_status(
        refreshed.as_ref().unwrap_or(db),
        crosslink_dir,
        (!synchronized).then_some(database_unavailable).flatten(),
    )?;

    loop {
        terminal.draw(|frame| app.render(frame))?;

        app.tabs[app.active_tab].poll_updates();

        app.poll_sync();

        if app.last_sync.elapsed() > PERIODIC_SYNC_INTERVAL && !app.syncing {
            app.start_background_sync();
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }
                    app.handle_key(key);
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                }
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn app_with_database_status(
    db: &Database,
    crosslink_dir: &Path,
    database_unavailable: Option<&str>,
) -> anyhow::Result<App> {
    let service = RepositoryService::projection(db);
    let mut app = App::new(&service, crosslink_dir)?;
    if let Some(reason) = database_unavailable {
        app.flash_message = Some(format!("Database unavailable: {reason}"));
    }
    Ok(app)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use tempfile::tempdir;

    fn make_key(code: KeyCode) -> KeyEvent {
        super::make_test_key(code)
    }

    fn make_key_with_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn setup_test_app() -> (App, tempfile::TempDir) {
        let dir = tempdir().unwrap();

        let crosslink_dir = dir.path().join(".crosslink");
        std::fs::create_dir_all(&crosslink_dir).unwrap();
        let db_path = crosslink_dir.join("issues.db");
        let db = Database::open(&db_path).unwrap();
        db.create_issue("Test issue 1", Some("Description"), "high")
            .unwrap();
        db.create_issue("Test issue 2", None, "medium").unwrap();
        let app = App::new(&db, &crosslink_dir).unwrap();
        (app, dir)
    }

    #[test]
    fn test_app_initial_state() {
        let (app, _dir) = setup_test_app();
        assert_eq!(app.active_tab, 0);
        assert!(!app.show_help);
        assert!(!app.should_quit);
        assert_eq!(app.tabs.len(), 6);
    }

    #[test]
    fn test_tab_navigation_forward() {
        let (mut app, _dir) = setup_test_app();
        assert_eq!(app.active_tab, 0);
        app.handle_key(make_key(KeyCode::Tab));
        assert_eq!(app.active_tab, 1);
        app.handle_key(make_key(KeyCode::Tab));
        assert_eq!(app.active_tab, 2);
    }

    #[test]
    fn test_tab_navigation_wraps() {
        let (mut app, _dir) = setup_test_app();

        for _ in 0..5 {
            app.handle_key(make_key(KeyCode::Tab));
        }
        assert_eq!(app.active_tab, 5);

        app.handle_key(make_key(KeyCode::Tab));
        assert_eq!(app.active_tab, 0);
    }

    #[test]
    fn test_tab_navigation_backward() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::BackTab));
        assert_eq!(app.active_tab, 5);
        app.handle_key(make_key(KeyCode::BackTab));
        assert_eq!(app.active_tab, 4);
    }

    #[test]
    fn test_direct_tab_selection() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char('3')));
        assert_eq!(app.active_tab, 2);
        app.handle_key(make_key(KeyCode::Char('1')));
        assert_eq!(app.active_tab, 0);
        app.handle_key(make_key(KeyCode::Char('6')));
        assert_eq!(app.active_tab, 5);
    }

    #[test]
    fn test_quit_with_q() {
        let (mut app, _dir) = setup_test_app();
        assert!(!app.should_quit);
        app.handle_key(make_key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn test_quit_with_ctrl_c() {
        let (mut app, _dir) = setup_test_app();
        assert!(!app.should_quit);
        app.handle_key(make_key_with_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn test_help_toggle() {
        let (mut app, _dir) = setup_test_app();
        assert!(!app.show_help);
        app.handle_key(make_key(KeyCode::Char('?')));
        assert!(app.show_help);

        app.handle_key(make_key(KeyCode::Tab));
        assert_eq!(app.active_tab, 0);

        app.handle_key(make_key(KeyCode::Char('?')));
        assert!(!app.show_help);
    }

    #[test]
    fn test_help_dismiss_with_esc() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char('?')));
        assert!(app.show_help);
        app.handle_key(make_key(KeyCode::Esc));
        assert!(!app.show_help);
    }

    #[test]
    fn test_render_does_not_panic() {
        let (mut app, _dir) = setup_test_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
    }

    #[test]
    fn unavailable_projection_builds_renderable_diagnostic_shell_without_mutation() {
        let dir = tempdir().unwrap();
        let crosslink = dir.path().join(".crosslink");
        std::fs::create_dir(&crosslink).unwrap();
        let source = b"truncated sqlite projection";
        std::fs::write(crosslink.join("issues.db"), source).unwrap();
        let db = Database::open_ephemeral().unwrap();
        let mut app =
            app_with_database_status(&db, &crosslink, Some("projection is corrupt")).unwrap();
        assert_eq!(
            app.flash_message.as_deref(),
            Some("Database unavailable: projection is corrupt")
        );
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert_eq!(std::fs::read(crosslink.join("issues.db")).unwrap(), source);
    }

    #[test]
    fn startup_sync_hydrates_advanced_authority_and_refreshes_readiness() {
        let (_work, _remote, crosslink, cache) = crate::reconcile::migration::tests::setup_v2_hub();
        crate::reconcile::migration::hub_v3(&crosslink, false, false, false, false).unwrap();
        let identity = crate::reconcile::readiness::DaemonIdentity {
            schema_version: crate::reconcile::readiness::READINESS_SCHEMA_VERSION,
            repository_id: crate::reconcile::readiness::repository_id(&crosslink).unwrap(),
            daemon_epoch: "tui-startup-sync".to_string(),
            pid: std::process::id(),
            process_start: crate::reconcile::readiness::current_process_start_token().unwrap(),
        };
        crate::reconcile::readiness::write_daemon_identity(&crosslink, &identity).unwrap();
        let generation = crate::reconcile::publication::generation_id_at_ref(
            &cache,
            crate::reconcile::publication::GENERATION_REF,
        )
        .unwrap()
        .unwrap();
        crate::reconcile::readiness::write_record(
            &crosslink,
            crate::reconcile::readiness::ReadinessDraft {
                daemon_epoch: &identity.daemon_epoch,
                daemon_pid: identity.pid,
                attempt_id: "tui-startup-sync",
                state: crate::reconcile::readiness::ReadinessState::ReadyCurrent,
                generation_id: Some(&generation),
                reason: None,
            },
        )
        .unwrap();
        let heartbeat_tip = crate::hub_v3::write_heartbeat_to_ref(
            &cache,
            "tui-startup-agent",
            &crate::locks::Heartbeat {
                agent_id: "tui-startup-agent".to_string(),
                last_heartbeat: chrono::Utc::now(),
                active_issue_id: None,
                machine_id: "test-machine".to_string(),
            },
        )
        .unwrap();
        let heartbeat_ref = crate::hub_v3::agent_ref_name("tui-startup-agent").unwrap();
        assert!(std::process::Command::new("git")
            .current_dir(&cache)
            .args([
                "push",
                "origin",
                &format!("{heartbeat_tip}:{heartbeat_ref}"),
            ])
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .current_dir(&cache)
            .args(["update-ref", "-d", &heartbeat_ref])
            .status()
            .unwrap()
            .success());
        assert!(crate::reconcile::readiness::projection_is_current(&crosslink).unwrap());
        assert!(sync_projection_at_startup(&crosslink).unwrap());
        assert!(crate::reconcile::readiness::projection_is_current(&crosslink).unwrap());
        let record = crate::reconcile::readiness::read_record(&crosslink)
            .unwrap()
            .unwrap();
        crate::reconcile::readiness::validate_record(&crosslink, &record).unwrap();
    }

    #[test]
    fn test_render_with_help_overlay() {
        let (mut app, _dir) = setup_test_app();
        app.show_help = true;
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
    }

    #[test]
    fn test_render_each_tab() {
        let (mut app, _dir) = setup_test_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        for i in 0..6 {
            app.tabs[app.active_tab].on_leave();
            app.active_tab = i;
            app.tabs[app.active_tab].on_enter();
            terminal.draw(|frame| app.render(frame)).unwrap();
        }
    }

    #[test]
    fn test_centered_rect() {
        let area = Rect::new(0, 0, 100, 50);
        let centered = centered_rect(60, 70, area);

        assert!(centered.x > 0);
        assert!(centered.y > 0);
        assert!(centered.width < area.width);
        assert!(centered.height < area.height);
    }

    #[test]
    fn test_command_mode_enter_exit() {
        let (mut app, _dir) = setup_test_app();
        assert!(!app.command_mode);

        app.handle_key(make_key(KeyCode::Char(':')));
        assert!(app.command_mode);
        assert!(app.command_input.is_empty());

        app.handle_key(make_key(KeyCode::Esc));
        assert!(!app.command_mode);
    }

    #[test]
    fn test_command_mode_typing() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char(':')));
        app.handle_key(make_key(KeyCode::Char('t')));
        app.handle_key(make_key(KeyCode::Char('a')));
        app.handle_key(make_key(KeyCode::Char('b')));
        assert_eq!(app.command_input, "tab");
        app.handle_key(make_key(KeyCode::Backspace));
        assert_eq!(app.command_input, "ta");
    }

    #[test]
    fn test_command_quit() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char(':')));
        app.handle_key(make_key(KeyCode::Char('q')));
        app.handle_key(make_key(KeyCode::Enter));
        assert!(app.should_quit);
    }

    #[test]
    fn test_command_tab_switch() {
        let (mut app, _dir) = setup_test_app();
        assert_eq!(app.active_tab, 0);
        app.handle_key(make_key(KeyCode::Char(':')));
        for c in "tab 3".chars() {
            app.handle_key(make_key(KeyCode::Char(c)));
        }
        app.handle_key(make_key(KeyCode::Enter));
        assert!(!app.command_mode);
        assert_eq!(app.active_tab, 2);
    }

    #[test]
    fn test_command_refresh() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char(':')));
        app.handle_key(make_key(KeyCode::Char('r')));
        app.handle_key(make_key(KeyCode::Enter));
        assert!(!app.command_mode);
        assert_eq!(app.flash_message.as_deref(), Some("Refreshed"));
    }

    #[test]
    fn test_command_unknown() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char(':')));
        for c in "foo".chars() {
            app.handle_key(make_key(KeyCode::Char(c)));
        }
        app.handle_key(make_key(KeyCode::Enter));
        assert_eq!(app.flash_message.as_deref(), Some("Unknown command: foo"));
    }

    #[test]
    fn test_command_help() {
        let (mut app, _dir) = setup_test_app();
        app.handle_key(make_key(KeyCode::Char(':')));
        for c in "help".chars() {
            app.handle_key(make_key(KeyCode::Char(c)));
        }
        app.handle_key(make_key(KeyCode::Enter));
        assert!(app.show_help);
    }

    #[test]
    fn test_flash_cleared_on_keypress() {
        let (mut app, _dir) = setup_test_app();
        app.flash_message = Some("test".to_string());
        app.handle_key(make_key(KeyCode::Char('j')));
        assert!(app.flash_message.is_none());
    }

    #[test]
    fn test_mouse_scroll_down() {
        let (mut app, _dir) = setup_test_app();

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 10,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(mouse);
    }

    #[test]
    fn test_mouse_scroll_up() {
        let (mut app, _dir) = setup_test_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 40,
            row: 10,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(mouse);
    }

    #[test]
    fn test_mouse_click_tab_bar() {
        let (mut app, _dir) = setup_test_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();

        assert_eq!(app.active_tab, 0);

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.tab_bar_area.x + 1 + 10,
            row: app.tab_bar_area.y + 1,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(mouse);
        assert_eq!(app.active_tab, 1);
    }

    #[test]
    fn test_render_command_bar() {
        let (mut app, _dir) = setup_test_app();
        app.command_mode = true;
        app.command_input = "tab 3".to_string();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
    }

    #[test]
    fn test_render_flash_message() {
        let (mut app, _dir) = setup_test_app();
        app.flash_message = Some("Copied!".to_string());
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
    }
}
