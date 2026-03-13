use crate::model::{
    Agent, AgentStatus, Alert, Args, SupervisorEvent, build_codex_launch_command, next_agent_id,
    now_ts, ring_bell,
};
use crate::store::SessionStore;
use crate::supervisor::AgentSupervisor;
use crate::terminal::{
    DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS, PaneTerminal, encode_key_event, encode_paste,
};
use crate::worktree::WorktreeManager;
use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use portable_pty::PtySize;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Command,
    Pane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandAction {
    SpawnPane {
        model: Option<String>,
        prompt: Option<String>,
    },
    Repo {
        path: String,
    },
    SendLine {
        line: String,
    },
    Help,
}

#[derive(Debug, Clone)]
struct PaneView {
    id: String,
    area: Rect,
}

pub struct App {
    repo_root: PathBuf,
    base_branch: String,
    worktrees_dir: String,
    allow_worktrees: bool,
    help_token: String,
    store: SessionStore,
    supervisor: AgentSupervisor,
    events_rx: Receiver<SupervisorEvent>,
    worktree_manager: Option<WorktreeManager>,

    agents: HashMap<String, Agent>,
    terminals: HashMap<String, PaneTerminal>,
    order: Vec<String>,
    selected: usize,
    alerts: VecDeque<Alert>,
    status_message: String,
    running: bool,
    needs_redraw: bool,
    last_size: Option<(u16, u16)>,

    input_mode: InputMode,
    command_return_mode: InputMode,
    input_buffer: String,
}

impl App {
    pub fn new(args: Args) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let repo_root = args.repo.canonicalize().unwrap_or(args.repo.clone());
        let allow_worktrees = !args.no_worktree;
        let (worktree_manager, status_message) = Self::build_repo_state(
            &repo_root,
            &args.base_branch,
            &args.worktrees_dir,
            allow_worktrees,
        );

        Ok(Self {
            repo_root,
            base_branch: args.base_branch,
            worktrees_dir: args.worktrees_dir,
            allow_worktrees,
            help_token: args.help_token,
            store: SessionStore::open(&args.db_path)?,
            supervisor: AgentSupervisor::new(tx),
            events_rx: rx,
            worktree_manager,
            agents: HashMap::new(),
            terminals: HashMap::new(),
            order: Vec::new(),
            selected: 0,
            alerts: VecDeque::with_capacity(200),
            status_message,
            running: true,
            needs_redraw: true,
            last_size: None,
            input_mode: InputMode::Normal,
            command_return_mode: InputMode::Normal,
            input_buffer: String::new(),
        })
    }

    pub fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let loop_result = self.main_loop(&mut terminal);

        let _ = disable_raw_mode();
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = terminal.show_cursor();
        self.supervisor.stop_all();

        loop_result
    }

    fn main_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        while self.running {
            let size = terminal.size()?;
            let current_size = (size.width, size.height);
            if self.last_size != Some(current_size) {
                self.last_size = Some(current_size);
                self.needs_redraw = true;
            }

            if self.process_events() {
                self.needs_redraw = true;
            }

            if self.needs_redraw {
                let area = Rect::new(0, 0, size.width, size.height);
                self.sync_pane_sizes(area);
                terminal.draw(|f| self.draw(f))?;
                self.needs_redraw = false;
            }

            if event::poll(Duration::from_millis(33))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    Event::Paste(text) => self.on_paste(text),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                self.needs_redraw = true;
            }
        }
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame<'_>) {
        let area = f.area();
        let (pane_area, alerts_area, command_area) = split_root(area);

        self.draw_pane_matrix(f, pane_area);
        self.draw_alerts(f, alerts_area);
        self.draw_command_center(f, command_area);
    }

    fn draw_pane_matrix(&mut self, f: &mut Frame<'_>, area: Rect) {
        if self.order.is_empty() {
            let block = Block::default()
                .title(" Codex Grid ")
                .borders(Borders::ALL)
                .border_style(border_style(false, false, false));
            let text = vec![
                Line::from("Type `codex` in the command center to open a new Codex terminal."),
                Line::from(
                    "Each pane is a real shell in its project directory with Codex started inside it.",
                ),
                Line::from("Click a pane to focus it and type directly into the terminal."),
            ];
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(terminal_style())
                .wrap(Wrap { trim: false });
            f.render_widget(paragraph, area);
            return;
        }

        for pane in self.pane_layouts(area) {
            if let Some(agent) = self.agents.get(&pane.id).cloned() {
                self.draw_single_pane(f, &pane, &agent);
            }
        }
    }

    fn draw_single_pane(&mut self, f: &mut Frame<'_>, pane: &PaneView, agent: &Agent) {
        let selected = self.current_agent_id() == Some(&pane.id);
        let interactive = selected && self.input_mode == InputMode::Pane;
        let attention = self
            .terminals
            .get(&pane.id)
            .and_then(PaneTerminal::attention)
            .map(ToString::to_string);
        let needs_input = agent.needs_help || attention.is_some();
        let failed = agent.status == AgentStatus::Failed || agent.status == AgentStatus::Blocked;
        let running = self.supervisor.is_running(&pane.id);
        let border = border_style(selected || interactive, needs_input, failed);

        let mut title = format!(" {} {} ", pane.id, short_path_label(&agent.run_dir));
        title.push_str(if running {
            if interactive { "[typing] " } else { "[live] " }
        } else {
            match agent.status {
                AgentStatus::Done => "[done] ",
                AgentStatus::Failed => "[failed] ",
                AgentStatus::Blocked => "[blocked] ",
                AgentStatus::NeedsHelp => "[input] ",
                AgentStatus::Running => "[live] ",
                AgentStatus::Starting => "[starting] ",
            }
        });

        if let Some(term) = self.terminals.get(&pane.id) {
            let title_hint = term.title().trim();
            if !title_hint.is_empty() {
                title.push_str(&format!("{} ", trim_label(title_hint, 20)));
            }
        }

        if agent.model != "default" {
            title.push_str(&format!("[{}] ", agent.model));
        }

        if needs_input {
            title.push_str("[input] ");
        }

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border);
        let inner = block.inner(pane.area);

        if let Some(term) = self.terminals.get_mut(&pane.id) {
            let paragraph = Paragraph::new(term.render_lines(terminal_style()).to_vec())
                .block(block)
                .style(terminal_style())
                .wrap(Wrap { trim: false });
            f.render_widget(paragraph, pane.area);

            if interactive && inner.width > 0 && inner.height > 0 {
                if !term.hide_cursor() {
                    let (row, col) = term.cursor_position_for_render();
                    let x = inner
                        .x
                        .saturating_add(col.min(inner.width.saturating_sub(1)));
                    let y = inner
                        .y
                        .saturating_add(row.min(inner.height.saturating_sub(1)));
                    f.set_cursor_position(Position { x, y });
                }
            }
        } else {
            let paragraph = Paragraph::new(vec![Line::from("terminal unavailable")])
                .block(block)
                .style(terminal_style())
                .wrap(Wrap { trim: false });
            f.render_widget(paragraph, pane.area);
        }
    }

    fn draw_alerts(&self, f: &mut Frame<'_>, area: Rect) {
        let unresolved = self
            .alerts
            .iter()
            .filter(|alert| !alert.acknowledged)
            .count();
        let mut lines = Vec::new();

        if unresolved == 0 {
            lines.push(Line::from(vec![
                Span::styled("alerts ", Style::default().fg(Color::DarkGray)),
                Span::raw("clear"),
                Span::raw(" | "),
                Span::styled("flow ", Style::default().fg(Color::DarkGray)),
                Span::raw("type `codex`, click a pane, then type directly into that terminal"),
            ]));
        } else {
            for alert in self
                .alerts
                .iter()
                .filter(|alert| !alert.acknowledged)
                .take(2)
            {
                lines.push(Line::from(vec![
                    Span::styled(
                        "! ",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{}: {}", alert.agent_id, alert.message)),
                ]));
            }
        }

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" Alerts ({unresolved}) "))
                    .borders(Borders::ALL)
                    .border_style(border_style(false, unresolved > 0, false)),
            )
            .style(alert_style())
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    }

    fn draw_command_center(&self, f: &mut Frame<'_>, area: Rect) {
        let target = self
            .current_agent()
            .map(|agent| format!("{} @ {}", agent.id, short_path_label(&agent.run_dir)))
            .unwrap_or_else(|| "none".to_string());
        let repo_label = trim_label(&self.repo_root.display().to_string(), 42);
        let top = Line::from(vec![
            Span::styled("repo ", Style::default().fg(Color::DarkGray)),
            Span::raw(repo_label),
            Span::raw(" | "),
            Span::styled("focus ", Style::default().fg(Color::DarkGray)),
            Span::raw(target),
            Span::raw(" | "),
            Span::styled("mode ", Style::default().fg(Color::DarkGray)),
            Span::raw(mode_label(self.input_mode)),
        ]);

        let hint = match self.input_mode {
            InputMode::Command => Line::from(vec![
                Span::styled("new ", Style::default().fg(Color::DarkGray)),
                Span::raw("`codex`"),
                Span::raw(" | "),
                Span::styled("prompt ", Style::default().fg(Color::DarkGray)),
                Span::raw("`codex fix auth bug`"),
                Span::raw(" | "),
                Span::styled("repo ", Style::default().fg(Color::DarkGray)),
                Span::raw("`repo ~/code/project`"),
            ]),
            InputMode::Pane => Line::from(vec![
                Span::styled("pane ", Style::default().fg(Color::DarkGray)),
                Span::raw("live"),
                Span::raw(" | "),
                Span::styled("switch ", Style::default().fg(Color::DarkGray)),
                Span::raw("click another pane"),
                Span::raw(" | "),
                Span::styled("commands ", Style::default().fg(Color::DarkGray)),
                Span::raw("Ctrl-G"),
            ]),
            InputMode::Normal => Line::from(vec![
                Span::styled("launch ", Style::default().fg(Color::DarkGray)),
                Span::raw("type `codex`"),
                Span::raw(" | "),
                Span::styled("focus ", Style::default().fg(Color::DarkGray)),
                Span::raw("click a pane or press Enter"),
                Span::raw(" | "),
                Span::styled("quit ", Style::default().fg(Color::DarkGray)),
                Span::raw("Ctrl-Q"),
            ]),
        };

        let input = match self.input_mode {
            InputMode::Command => Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::White)),
                Span::styled(&self.input_buffer, Style::default().fg(Color::White)),
            ]),
            InputMode::Pane => Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::White)),
                Span::styled(
                    "terminal input is live in the focused pane",
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            InputMode::Normal => Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::White)),
                Span::styled(&self.status_message, Style::default().fg(Color::DarkGray)),
            ]),
        };

        let block = Block::default()
            .title(" Command Center ")
            .borders(Borders::ALL)
            .border_style(border_style(
                self.input_mode == InputMode::Command,
                false,
                false,
            ));
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        let paragraph = Paragraph::new(vec![top, hint, input])
            .block(block)
            .style(command_center_style());
        f.render_widget(paragraph, area);

        if self.input_mode == InputMode::Command && inner.width > 0 && inner.height > 0 {
            let cursor_x = inner
                .x
                .saturating_add(2)
                .saturating_add(self.input_buffer.chars().count() as u16)
                .min(inner.x.saturating_add(inner.width.saturating_sub(1)));
            let cursor_y = inner.y.saturating_add(inner.height.saturating_sub(1));
            f.set_cursor_position(Position {
                x: cursor_x,
                y: cursor_y,
            });
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            self.running = false;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            self.begin_command("");
            return;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Command => self.handle_command_key(key),
            InputMode::Pane => self.handle_pane_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::Char('a') => self.ack_selected_alerts(),
            KeyCode::Char('x') => self.stop_selected_agent(),
            KeyCode::Enter => {
                if let Some(agent_id) = self.current_agent_id().cloned() {
                    self.focus_pane_for_live_input(&agent_id);
                    self.input_mode = InputMode::Pane;
                    self.status_message = format!("{agent_id} focused for live input");
                } else {
                    self.begin_command("");
                }
            }
            KeyCode::Char(c) if !c.is_control() => self.begin_command(&c.to_string()),
            _ => {}
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.exit_command_mode(),
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Enter => self.submit_command(),
            KeyCode::Char(c) => {
                if !c.is_control() {
                    self.input_buffer.push(c);
                }
            }
            _ => {}
        }
    }

    fn handle_pane_key(&mut self, key: KeyEvent) {
        let Some(agent_id) = self.current_agent_id().cloned() else {
            self.input_mode = InputMode::Normal;
            self.handle_normal_key(key);
            return;
        };

        let application_cursor = self
            .terminals
            .get(&agent_id)
            .map(PaneTerminal::application_cursor)
            .unwrap_or(false);

        let Some(bytes) = encode_key_event(key, application_cursor) else {
            return;
        };

        if let Some(terminal) = self.terminals.get_mut(&agent_id) {
            terminal.reset_scrollback();
        }
        self.clear_attention_for_agent(&agent_id);

        if let Err(err) = self.supervisor.send_input(&agent_id, &bytes) {
            self.status_message = err.to_string();
            self.input_mode = InputMode::Normal;
        }
    }

    fn on_paste(&mut self, text: String) {
        match self.input_mode {
            InputMode::Command => self.input_buffer.push_str(&text),
            InputMode::Pane => {
                let Some(agent_id) = self.current_agent_id().cloned() else {
                    return;
                };
                let bracketed = self
                    .terminals
                    .get(&agent_id)
                    .map(PaneTerminal::bracketed_paste)
                    .unwrap_or(false);
                let bytes = encode_paste(&text, bracketed);
                if let Some(terminal) = self.terminals.get_mut(&agent_id) {
                    terminal.reset_scrollback();
                }
                self.clear_attention_for_agent(&agent_id);
                if let Err(err) = self.supervisor.send_input(&agent_id, &bytes) {
                    self.status_message = err.to_string();
                }
            }
            InputMode::Normal => self.begin_command(&text),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let Ok((width, height)) = crossterm::terminal::size() else {
            return;
        };
        let root = Rect::new(0, 0, width, height);
        let (pane_area, _alerts_area, command_area) = split_root(root);

        if contains(command_area, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.begin_command("");
            return;
        }

        let panes = self.pane_layouts(pane_area);
        let clicked_pane = panes
            .iter()
            .enumerate()
            .find(|(_, pane)| contains(pane.area, mouse.column, mouse.row));

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((index, pane)) = clicked_pane {
                    self.selected = index;
                    self.focus_pane_for_live_input(&pane.id);
                    self.input_mode = InputMode::Pane;
                    self.status_message = format!("{} focused for live input", pane.id);
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some((index, pane)) = clicked_pane {
                    self.selected = index;
                    if let Some(term) = self.terminals.get_mut(&pane.id) {
                        term.scroll_up(3);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some((index, pane)) = clicked_pane {
                    self.selected = index;
                    if let Some(term) = self.terminals.get_mut(&pane.id) {
                        term.scroll_down(3);
                    }
                }
            }
            _ => {}
        }
    }

    fn begin_command(&mut self, seed: &str) {
        self.command_return_mode = self.input_mode;
        self.input_mode = InputMode::Command;
        self.input_buffer = seed.to_string();
    }

    fn exit_command_mode(&mut self) {
        self.input_mode = match (self.command_return_mode, self.current_agent_id().cloned()) {
            (InputMode::Pane, Some(agent_id)) => {
                self.focus_pane_for_live_input(&agent_id);
                InputMode::Pane
            }
            _ => InputMode::Normal,
        };
        self.input_buffer.clear();
    }

    fn submit_command(&mut self) {
        let command = self.input_buffer.trim().to_string();
        self.input_buffer.clear();

        if command.is_empty() {
            self.status_message = "command center input is empty".to_string();
            self.input_mode = InputMode::Normal;
            return;
        }

        match parse_command_center_action(&command, self.current_agent_id().is_some()) {
            Ok(CommandAction::SpawnPane { model, prompt }) => {
                self.spawn_pane(model, prompt);
                if let Some(agent_id) = self.current_agent_id().cloned() {
                    self.focus_pane_for_live_input(&agent_id);
                    self.input_mode = InputMode::Pane;
                } else {
                    self.input_mode = InputMode::Normal;
                }
            }
            Ok(CommandAction::Repo { path }) => {
                if let Err(err) = self.set_repo_path(&path) {
                    self.status_message = err.to_string();
                }
                self.input_mode = InputMode::Normal;
            }
            Ok(CommandAction::SendLine { line }) => {
                self.send_line_to_selected_pane(line);
                self.input_mode = if self.current_agent_id().is_some() {
                    InputMode::Pane
                } else {
                    InputMode::Normal
                };
            }
            Ok(CommandAction::Help) => {
                self.status_message =
                    "type `codex`, click a pane to focus it, then type directly into that terminal"
                        .to_string();
                self.input_mode = InputMode::Normal;
            }
            Err(err) => {
                self.status_message = err.to_string();
                self.input_mode = InputMode::Normal;
            }
        }
    }

    fn build_repo_state(
        repo_root: &Path,
        base_branch: &str,
        worktrees_dir: &str,
        allow_worktrees: bool,
    ) -> (Option<WorktreeManager>, String) {
        if !allow_worktrees {
            return (
                None,
                format!("repo ready: {} (worktrees disabled)", repo_root.display()),
            );
        }

        match WorktreeManager::new(
            repo_root.to_path_buf(),
            base_branch.to_string(),
            worktrees_dir.to_string(),
        ) {
            Ok(manager) => (
                Some(manager),
                format!("repo ready: {} (worktrees enabled)", repo_root.display()),
            ),
            Err(err) => (
                None,
                format!(
                    "repo ready: {} (worktrees disabled: {err})",
                    repo_root.display()
                ),
            ),
        }
    }

    fn set_repo_path(&mut self, raw_path: &str) -> Result<()> {
        let repo_root = Self::normalize_repo_path(raw_path)?;
        let (worktree_manager, status_message) = Self::build_repo_state(
            &repo_root,
            &self.base_branch,
            &self.worktrees_dir,
            self.allow_worktrees,
        );
        self.repo_root = repo_root;
        self.worktree_manager = worktree_manager;
        self.status_message = status_message;
        Ok(())
    }

    fn normalize_repo_path(raw_path: &str) -> Result<PathBuf> {
        let path = Self::expand_tilde_path(raw_path.trim())?;
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .context("failed to resolve current directory")?
                .join(path)
        };

        if !absolute.exists() {
            bail!("repo path does not exist: {}", absolute.display());
        }
        if !absolute.is_dir() {
            bail!("repo path is not a directory: {}", absolute.display());
        }

        Ok(absolute.canonicalize().unwrap_or(absolute))
    }

    fn expand_tilde_path(raw_path: &str) -> Result<PathBuf> {
        if raw_path == "~" {
            let home = std::env::var_os("HOME").context("could not resolve home directory")?;
            return Ok(PathBuf::from(home));
        }

        if let Some(rest) = raw_path.strip_prefix("~/") {
            let home = std::env::var_os("HOME").context("could not resolve home directory")?;
            return Ok(PathBuf::from(home).join(rest));
        }

        Ok(PathBuf::from(raw_path))
    }

    fn spawn_pane(&mut self, model: Option<String>, prompt: Option<String>) {
        let agent_id = next_agent_id();
        self.order.push(agent_id.clone());
        self.selected = self.order.len().saturating_sub(1);

        let prompt_text = prompt.clone().unwrap_or_default();
        let launch_command = build_codex_launch_command(model.as_deref(), prompt.as_deref());

        let (run_dir, worktree) = if let Some(manager) = &self.worktree_manager {
            match manager.create_worktree() {
                Ok(info) => {
                    self.status_message =
                        format!("created pane {} in {}", info.slug, info.path.display());
                    (info.path.clone(), Some(info))
                }
                Err(err) => {
                    let mut blocked_agent = Agent::new(
                        agent_id.clone(),
                        display_model(model.as_deref()),
                        prompt_text,
                        self.repo_root.clone(),
                    );
                    blocked_agent.status = AgentStatus::Blocked;
                    blocked_agent.updated_at = now_ts();
                    self.add_alert(agent_id.clone(), format!("worktree creation failed: {err}"));
                    let _ = self.store.upsert_agent(&blocked_agent);
                    self.agents.insert(agent_id, blocked_agent);
                    self.input_mode = InputMode::Normal;
                    return;
                }
            }
        } else {
            (self.repo_root.clone(), None)
        };

        let mut agent = Agent::new(
            agent_id.clone(),
            display_model(model.as_deref()),
            prompt_text,
            run_dir.clone(),
        );
        agent.worktree = worktree;
        self.terminals.insert(
            agent_id.clone(),
            PaneTerminal::new(DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLS),
        );

        match self.supervisor.start(
            &agent_id,
            &run_dir,
            Some(&launch_command),
            pty_size(DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLS),
        ) {
            Ok(()) => {
                self.status_message = format!("spawned pane {agent_id} in {}", run_dir.display());
                let _ = self.store.add_event(&agent_id, "spawn", &launch_command);
            }
            Err(err) => {
                agent.status = AgentStatus::Blocked;
                self.terminals.remove(&agent_id);
                let msg = format!("failed to start pane: {err}");
                self.add_alert(agent_id.clone(), msg);
            }
        }

        agent.updated_at = now_ts();
        let _ = self.store.upsert_agent(&agent);
        self.agents.insert(agent_id, agent);
    }

    fn focus_pane_for_live_input(&mut self, agent_id: &str) {
        if let Some(term) = self.terminals.get_mut(agent_id) {
            // Make sure the viewport is back on the active prompt before typing.
            term.reset_scrollback();
        }
    }

    fn send_line_to_selected_pane(&mut self, line: String) {
        let Some(agent_id) = self.current_agent_id().cloned() else {
            self.status_message = "no pane selected; type `codex` to open one".to_string();
            return;
        };

        if let Some(term) = self.terminals.get_mut(&agent_id) {
            term.reset_scrollback();
        }
        self.clear_attention_for_agent(&agent_id);

        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\r');
        match self.supervisor.send_input(&agent_id, &bytes) {
            Ok(()) => {
                self.status_message = format!("sent line to {agent_id}");
                let _ = self.store.add_event(&agent_id, "shell_input", &line);
            }
            Err(err) => {
                self.status_message = err.to_string();
            }
        }
    }

    fn process_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events_rx.try_recv() {
            changed = true;
            match event {
                SupervisorEvent::Started { agent_id, pid } => {
                    if let Some(agent) = self.agents.get_mut(&agent_id) {
                        agent.pid = Some(pid);
                        agent.status = AgentStatus::Running;
                        agent.updated_at = now_ts();
                        let _ = self
                            .store
                            .add_event(&agent_id, "started", &format!("pid={pid}"));
                        let _ = self.store.upsert_agent(agent);
                    }
                }
                SupervisorEvent::OutputChunk { agent_id, data } => {
                    self.handle_terminal_output(&agent_id, &data);
                }
                SupervisorEvent::Exited { agent_id, code } => {
                    let mut failed_message = None;
                    let running = false;
                    if let Some(agent) = self.agents.get_mut(&agent_id) {
                        agent.pid = None;
                        agent.return_code = Some(code);
                        agent.updated_at = now_ts();
                        if code == 0 {
                            agent.status = AgentStatus::Done;
                            agent.needs_help = false;
                            let _ = self.store.add_event(&agent_id, "exited", "exit=0");
                        } else {
                            agent.status = AgentStatus::Failed;
                            agent.needs_help = true;
                            failed_message = Some(format!("terminal exited with code {code}"));
                            let _ =
                                self.store
                                    .add_event(&agent_id, "exited", &format!("exit={code}"));
                        }
                        let _ = self.store.upsert_agent(agent);
                    }

                    if !running && self.current_agent_id() == Some(&agent_id) {
                        self.input_mode = InputMode::Normal;
                    }

                    if let Some(message) = failed_message {
                        self.add_alert(agent_id.clone(), message);
                        ring_bell();
                    }
                }
            }
        }
        changed
    }

    fn handle_terminal_output(&mut self, agent_id: &str, data: &[u8]) {
        let Some(terminal) = self.terminals.get_mut(agent_id) else {
            return;
        };

        let activity = terminal.process_output(data, &self.help_token);
        if activity.rang_bell {
            ring_bell();
        }

        let running = self.supervisor.is_running(agent_id);
        let mut clear_alerts = false;
        let mut new_alert = None;
        let mut changed = false;

        if let Some(agent) = self.agents.get_mut(agent_id) {
            if activity.attention_changed {
                changed = true;
                agent.updated_at = now_ts();
                match activity.attention {
                    Some(reason) => {
                        agent.needs_help = true;
                        if running {
                            agent.status = AgentStatus::NeedsHelp;
                        }
                        new_alert = Some(reason);
                    }
                    None => {
                        agent.needs_help = false;
                        if running {
                            agent.status = AgentStatus::Running;
                        }
                        clear_alerts = true;
                    }
                }
            }

            if changed {
                let _ = self.store.upsert_agent(agent);
            }
        }

        if clear_alerts {
            self.acknowledge_alerts_for_agent(agent_id);
        }

        if let Some(message) = new_alert {
            self.add_alert(agent_id.to_string(), message);
        }
    }

    fn sync_pane_sizes(&mut self, root: Rect) {
        let (pane_area, _, _) = split_root(root);
        for pane in self.pane_layouts(pane_area) {
            let inner = pane_inner(pane.area);
            let rows = inner.height.max(1);
            let cols = inner.width.max(1);

            if let Some(terminal) = self.terminals.get_mut(&pane.id) {
                let changed = terminal.resize(rows, cols);
                if changed && self.supervisor.is_running(&pane.id) {
                    let _ = self.supervisor.resize(&pane.id, pty_size(rows, cols));
                }
            }
        }
    }

    fn stop_selected_agent(&mut self) {
        if let Some(id) = self.current_agent_id().cloned() {
            self.supervisor.stop(&id);
            self.status_message = format!("sent stop signal to {id}");
            let _ = self.store.add_event(&id, "stop", "stop requested");
        } else {
            self.status_message = "no panes".to_string();
        }
    }

    fn ack_selected_alerts(&mut self) {
        let Some(id) = self.current_agent_id().cloned() else {
            return;
        };

        if let Some(terminal) = self.terminals.get_mut(&id) {
            let _ = terminal.clear_attention();
        }
        self.acknowledge_alerts_for_agent(&id);

        if let Some(agent) = self.agents.get_mut(&id) {
            agent.needs_help = false;
            if self.supervisor.is_running(&id) {
                agent.status = AgentStatus::Running;
            } else if agent.return_code == Some(0) {
                agent.status = AgentStatus::Done;
            }
            agent.updated_at = now_ts();
            let _ = self.store.upsert_agent(agent);
        }

        self.status_message = format!("acknowledged alerts for {id}");
    }

    fn clear_attention_for_agent(&mut self, agent_id: &str) {
        let mut changed = false;

        if let Some(terminal) = self.terminals.get_mut(agent_id) {
            changed |= terminal.clear_attention();
        }

        if let Some(agent) = self.agents.get_mut(agent_id) {
            if agent.needs_help {
                agent.needs_help = false;
                if self.supervisor.is_running(agent_id) {
                    agent.status = AgentStatus::Running;
                }
                agent.updated_at = now_ts();
                let _ = self.store.upsert_agent(agent);
                changed = true;
            }
        }

        if changed {
            self.acknowledge_alerts_for_agent(agent_id);
        }
    }

    fn acknowledge_alerts_for_agent(&mut self, agent_id: &str) {
        let mut changed = false;
        for alert in &mut self.alerts {
            if alert.agent_id == agent_id && !alert.acknowledged {
                alert.acknowledged = true;
                changed = true;
            }
        }
        if changed {
            let _ = self.store.mark_alerts_acknowledged(agent_id);
        }
    }

    fn add_alert(&mut self, agent_id: String, message: String) {
        let alert = Alert::new(agent_id, message.clone());
        if self.alerts.len() >= 200 {
            let _ = self.alerts.pop_back();
        }
        self.alerts.push_front(alert.clone());
        let _ = self.store.add_alert(&alert);
        self.status_message = format!("{}: {}", alert.agent_id, message);
    }

    fn select_next(&mut self) {
        if !self.order.is_empty() {
            self.selected = (self.selected + 1).min(self.order.len() - 1);
        }
    }

    fn select_previous(&mut self) {
        if !self.order.is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    fn current_agent_id(&self) -> Option<&String> {
        if self.order.is_empty() {
            return None;
        }
        self.order.get(self.selected.min(self.order.len() - 1))
    }

    fn current_agent(&self) -> Option<&Agent> {
        self.current_agent_id().and_then(|id| self.agents.get(id))
    }

    fn pane_layouts(&self, area: Rect) -> Vec<PaneView> {
        let count = self.order.len();
        if count == 0 {
            return Vec::new();
        }

        let columns = pane_columns(count, area.width);
        let rows = count.div_ceil(columns);
        let row_constraints = vec![Constraint::Ratio(1, rows as u32); rows];
        let row_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(area);

        let mut panes = Vec::with_capacity(count);
        let mut index = 0usize;
        for row in row_chunks.iter().take(rows) {
            let remaining = count.saturating_sub(index);
            if remaining == 0 {
                break;
            }
            let items_in_row = remaining.min(columns);
            let col_constraints = vec![Constraint::Ratio(1, items_in_row as u32); items_in_row];
            let col_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(col_constraints)
                .split(*row);

            for col in col_chunks.iter().take(items_in_row) {
                panes.push(PaneView {
                    id: self.order[index].clone(),
                    area: *col,
                });
                index += 1;
            }
        }

        panes
    }
}

fn parse_command_center_action(input: &str, has_selected_agent: bool) -> Result<CommandAction> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("command center input is empty");
    }

    let normalized = trimmed.strip_prefix('/').unwrap_or(trimmed);
    if normalized == "help" || normalized == "?" {
        return Ok(CommandAction::Help);
    }

    if let Some(rest) = normalized.strip_prefix("codex") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Ok(CommandAction::SpawnPane {
                model: None,
                prompt: None,
            });
        }
        if let Some((model, prompt)) = rest.split_once("::") {
            let prompt = prompt.trim();
            if prompt.is_empty() {
                bail!("use `codex <model> :: <prompt>`");
            }
            return Ok(CommandAction::SpawnPane {
                model: non_empty(model),
                prompt: Some(prompt.to_string()),
            });
        }
        return Ok(CommandAction::SpawnPane {
            model: None,
            prompt: Some(rest.to_string()),
        });
    }

    if let Some(rest) = normalized
        .strip_prefix("new ")
        .or_else(|| normalized.strip_prefix("n "))
    {
        let Some((model, prompt)) = rest.split_once("::") else {
            bail!("use `new <model> :: <prompt>`");
        };
        let model = model.trim();
        let prompt = prompt.trim();
        if model.is_empty() || prompt.is_empty() {
            bail!("use `new <model> :: <prompt>`");
        }
        return Ok(CommandAction::SpawnPane {
            model: Some(model.to_string()),
            prompt: Some(prompt.to_string()),
        });
    }

    if let Some(rest) = normalized
        .strip_prefix("repo ")
        .or_else(|| normalized.strip_prefix("r "))
    {
        let path = rest.trim();
        if path.is_empty() {
            bail!("use `repo <path>`");
        }
        return Ok(CommandAction::Repo {
            path: path.to_string(),
        });
    }

    if has_selected_agent {
        return Ok(CommandAction::SendLine {
            line: trimmed.to_string(),
        });
    }

    bail!("type `codex` to open a pane")
}

fn split_root(area: Rect) -> (Rect, Rect, Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(5),
        ])
        .split(area);
    (sections[0], sections[1], sections[2])
}

fn pane_columns(count: usize, width: u16) -> usize {
    if count <= 1 || width < 120 {
        1
    } else if count <= 4 || width < 180 {
        2
    } else {
        3
    }
}

fn pane_inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn terminal_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Black)
}

fn alert_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Black)
}

fn command_center_style() -> Style {
    Style::default().fg(Color::White).bg(Color::Black)
}

fn border_style(selected: bool, needs_input: bool, failed: bool) -> Style {
    let color = if failed {
        Color::DarkGray
    } else if needs_input {
        Color::White
    } else if selected {
        Color::White
    } else {
        Color::DarkGray
    };
    let modifier = if selected || needs_input {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    Style::default().fg(color).add_modifier(modifier)
}

fn short_path_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn trim_label(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_string();
    }

    let keep = max_chars.saturating_sub(1);
    chars.into_iter().take(keep).collect::<String>() + "…"
}

fn display_model(model: Option<&str>) -> String {
    model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or("default")
        .to_string()
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn mode_label(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Normal => "normal",
        InputMode::Command => "command",
        InputMode::Pane => "pane",
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

pub fn run_app(args: Args) -> Result<()> {
    let mut app = App::new(args).context("failed to initialize app")?;
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn test_args(repo: &Path, db_path: PathBuf) -> Args {
        Args {
            repo: repo.to_path_buf(),
            base_branch: "main".to_string(),
            worktrees_dir: ".worktrees".to_string(),
            db_path,
            help_token: "[[NEEDS_HELP]]".to_string(),
            no_worktree: false,
        }
    }

    #[test]
    fn app_initializes_without_git_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");

        let app = App::new(test_args(dir.path(), db_path)).expect("app should initialize");

        assert_eq!(
            app.repo_root,
            dir.path().canonicalize().expect("canonical repo")
        );
        assert!(app.worktree_manager.is_none());
        assert!(app.status_message.contains("worktrees disabled"));
    }

    #[test]
    fn repo_path_switch_enables_worktrees_for_git_repo() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let db_path = scratch.path().join("session.db");
        let repo = scratch.path().join("repo");
        fs::create_dir(&repo).expect("create repo dir");
        init_git_repo(&repo);

        let mut app =
            App::new(test_args(scratch.path(), db_path)).expect("app should initialize first");
        assert!(app.worktree_manager.is_none());

        app.set_repo_path(repo.to_string_lossy().as_ref())
            .expect("repo path should update");

        assert_eq!(app.repo_root, repo.canonicalize().expect("canonical repo"));
        assert!(app.worktree_manager.is_some());
        assert!(app.status_message.contains("worktrees enabled"));
    }

    #[test]
    fn command_parser_supports_codex_spawn_repo_and_send_line() {
        assert_eq!(
            parse_command_center_action("codex", false).expect("spawn"),
            CommandAction::SpawnPane {
                model: None,
                prompt: None,
            }
        );
        assert_eq!(
            parse_command_center_action("codex fix auth bug", false).expect("spawn prompt"),
            CommandAction::SpawnPane {
                model: None,
                prompt: Some("fix auth bug".to_string()),
            }
        );
        assert_eq!(
            parse_command_center_action("codex gpt-5 :: fix auth bug", false)
                .expect("spawn explicit model"),
            CommandAction::SpawnPane {
                model: Some("gpt-5".to_string()),
                prompt: Some("fix auth bug".to_string()),
            }
        );
        assert_eq!(
            parse_command_center_action("repo ~/code/project", false).expect("repo command"),
            CommandAction::Repo {
                path: "~/code/project".to_string(),
            }
        );
        assert_eq!(
            parse_command_center_action("ls -la", true).expect("send line"),
            CommandAction::SendLine {
                line: "ls -la".to_string(),
            }
        );
        assert!(parse_command_center_action("ls -la", false).is_err());
    }

    #[test]
    fn pane_column_count_scales_with_width() {
        assert_eq!(pane_columns(1, 100), 1);
        assert_eq!(pane_columns(2, 140), 2);
        assert_eq!(pane_columns(5, 200), 3);
    }

    fn init_git_repo(repo: &Path) {
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo)
                .status()
                .expect("git init")
                .success()
        );
        fs::write(repo.join("README.md"), "init\n").expect("write readme");
        assert!(
            Command::new("git")
                .args(["add", "README.md"])
                .current_dir(repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Codex",
                    "-c",
                    "user.email=codex@example.com",
                    "commit",
                    "-qm",
                    "init",
                ])
                .current_dir(repo)
                .status()
                .expect("git commit")
                .success()
        );
    }
}
