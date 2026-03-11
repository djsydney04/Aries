use crate::model::{
    Agent, AgentStatus, Alert, Args, SupervisorEvent, WorktreeInfo, build_agent_command,
    detect_help_signal, next_agent_id, now_ts, ring_bell,
};
use crate::store::SessionStore;
use crate::supervisor::AgentSupervisor;
use crate::worktree::WorktreeManager;
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Model,
    Prompt,
}

pub struct App {
    repo_root: std::path::PathBuf,
    command_template: String,
    help_token: String,
    store: SessionStore,
    supervisor: AgentSupervisor,
    events_rx: Receiver<SupervisorEvent>,
    worktree_manager: Option<WorktreeManager>,

    agents: HashMap<String, Agent>,
    order: Vec<String>,
    selected: usize,
    alerts: VecDeque<Alert>,
    status_message: String,
    running: bool,

    input_mode: InputMode,
    input_buffer: String,
    new_model: String,
}

impl App {
    pub fn new(args: Args) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let repo_root = args.repo.canonicalize().unwrap_or(args.repo.clone());

        let worktree_manager = if args.no_worktree {
            None
        } else {
            Some(WorktreeManager::new(
                repo_root.clone(),
                args.base_branch,
                args.worktrees_dir,
            )?)
        };

        Ok(Self {
            repo_root,
            command_template: args.agent_cmd_template,
            help_token: args.help_token,
            store: SessionStore::open(&args.db_path)?,
            supervisor: AgentSupervisor::new(tx),
            events_rx: rx,
            worktree_manager,
            agents: HashMap::new(),
            order: Vec::new(),
            selected: 0,
            alerts: VecDeque::with_capacity(200),
            status_message: "Ready".to_string(),
            running: true,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            new_model: String::new(),
        })
    }

    pub fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let loop_result = self.main_loop(&mut terminal);

        let _ = disable_raw_mode();
        let _ = crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();
        self.supervisor.stop_all();

        loop_result
    }

    fn main_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        while self.running {
            self.process_events();
            terminal.draw(|f| self.draw(f))?;

            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
        }
        Ok(())
    }

    fn draw(&self, f: &mut Frame<'_>) {
        let area = f.area();
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(7),
                Constraint::Length(1),
            ])
            .split(area);

        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(20)])
            .split(sections[0]);

        self.draw_agents(f, top[0]);
        self.draw_logs(f, top[1]);
        self.draw_alerts(f, sections[1]);
        self.draw_status(f, sections[2]);
    }

    fn draw_agents(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let items = if self.order.is_empty() {
            vec![ListItem::new("No agents. Press n to spawn.")]
        } else {
            self.order
                .iter()
                .map(|id| {
                    let agent = &self.agents[id];
                    let text = format!("[{}] {} ({})", agent.status.badge(), agent.model, agent.id);
                    ListItem::new(text)
                })
                .collect::<Vec<_>>()
        };

        let list = List::new(items)
            .block(Block::default().title(" Agents ").borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");

        let mut state = ListState::default();
        if !self.order.is_empty() {
            state.select(Some(self.selected.min(self.order.len().saturating_sub(1))));
        }
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_logs(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let (title, lines) = if let Some(agent) = self.current_agent() {
            let max_lines = area.height.saturating_sub(2) as usize;
            let iter = agent
                .logs
                .iter()
                .rev()
                .take(max_lines)
                .cloned()
                .collect::<Vec<_>>();
            let mut ordered = iter;
            ordered.reverse();
            (
                format!(" Stream: {} [{}] ", agent.id, agent.status.as_str()),
                ordered,
            )
        } else {
            (
                " Stream ".to_string(),
                vec!["Select an agent to view output.".to_string()],
            )
        };

        let paragraph = Paragraph::new(lines.join("\n"))
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    }

    fn draw_alerts(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let unresolved = self.alerts.iter().filter(|a| !a.acknowledged).count();
        let max_lines = area.height.saturating_sub(2) as usize;

        let mut lines = self
            .alerts
            .iter()
            .take(max_lines)
            .map(|alert| {
                let flag = if alert.acknowledged { ' ' } else { '!' };
                Line::from(vec![
                    Span::styled(
                        format!("{flag} "),
                        if alert.acknowledged {
                            Style::default().fg(Color::Gray)
                        } else {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        },
                    ),
                    Span::raw(format!("{}: {}", alert.agent_id, alert.message)),
                ])
            })
            .collect::<Vec<_>>();

        if lines.is_empty() {
            lines.push(Line::from("No alerts"));
        }

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(" Alerts ({unresolved}) "))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true });
        f.render_widget(paragraph, area);
    }

    fn draw_status(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let right_hint = match self.input_mode {
            InputMode::Normal => "n:new j/k:select x:stop a:ack q:quit".to_string(),
            InputMode::Model => format!("Model: {}", self.input_buffer),
            InputMode::Prompt => format!("Prompt: {}", self.input_buffer),
        };
        let text = format!("{} | {}", self.status_message, right_hint);
        f.render_widget(Paragraph::new(text), area);
    }

    fn on_key(&mut self, key: KeyEvent) {
        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Model | InputMode::Prompt => self.handle_input_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.running = false,
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.order.is_empty() {
                    self.selected = (self.selected + 1).min(self.order.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.order.is_empty() {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            KeyCode::Char('n') => {
                self.input_mode = InputMode::Model;
                self.input_buffer.clear();
                self.new_model.clear();
            }
            KeyCode::Char('x') => self.stop_selected_agent(),
            KeyCode::Char('a') => self.ack_selected_alerts(),
            _ => {}
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Enter => match self.input_mode {
                InputMode::Model => {
                    let model = self.input_buffer.trim().to_string();
                    if model.is_empty() {
                        self.status_message = "model cannot be empty".to_string();
                        self.input_mode = InputMode::Normal;
                    } else {
                        self.new_model = model;
                        self.input_mode = InputMode::Prompt;
                        self.input_buffer.clear();
                    }
                }
                InputMode::Prompt => {
                    let prompt = self.input_buffer.trim().to_string();
                    if prompt.is_empty() {
                        self.status_message = "prompt cannot be empty".to_string();
                    } else {
                        self.spawn_agent(self.new_model.clone(), prompt);
                    }
                    self.input_mode = InputMode::Normal;
                    self.input_buffer.clear();
                }
                InputMode::Normal => {}
            },
            KeyCode::Char(c) => {
                if !c.is_control() {
                    self.input_buffer.push(c);
                }
            }
            _ => {}
        }
    }

    fn spawn_agent(&mut self, model: String, prompt: String) {
        let agent_id = next_agent_id();
        let mut agent = Agent::new(agent_id.clone(), model.clone(), prompt.clone());
        self.order.push(agent_id.clone());
        self.selected = self.order.len().saturating_sub(1);

        let mut run_dir = self.repo_root.clone();
        if let Some(manager) = &self.worktree_manager {
            match manager.create_worktree() {
                Ok(info) => {
                    run_dir = info.path.clone();
                    agent.worktree = Some(info.clone());
                    self.status_message = format!(
                        "created worktree {} ({})",
                        info.slug,
                        info.path.to_string_lossy()
                    );
                }
                Err(err) => {
                    agent.status = AgentStatus::Blocked;
                    agent.updated_at = now_ts();
                    let msg = format!("worktree creation failed: {err}");
                    self.add_alert(agent_id.clone(), msg.clone());
                    agent.logs.push_back(msg);
                    let _ = self.store.upsert_agent(&agent);
                    self.agents.insert(agent_id, agent);
                    return;
                }
            }
        }

        let argv = match build_agent_command(
            &self.command_template,
            &model,
            &prompt,
            &agent_id,
            &run_dir.to_string_lossy(),
        ) {
            Ok(v) => v,
            Err(err) => {
                agent.status = AgentStatus::Blocked;
                agent.updated_at = now_ts();
                let msg = format!("invalid command template: {err}");
                self.add_alert(agent_id.clone(), msg.clone());
                agent.logs.push_back(msg);
                let _ = self.store.upsert_agent(&agent);
                self.agents.insert(agent_id, agent);
                return;
            }
        };

        match self.supervisor.start(&agent_id, argv, &run_dir) {
            Ok(()) => {
                self.status_message = format!("spawned {agent_id} in {}", run_dir.display());
                let _ = self.store.add_event(&agent_id, "spawn", "agent created");
            }
            Err(err) => {
                agent.status = AgentStatus::Blocked;
                let msg = format!("spawn failed: {err}");
                self.add_alert(agent_id.clone(), msg.clone());
                agent.logs.push_back(msg);
            }
        }

        agent.updated_at = now_ts();
        let _ = self.store.upsert_agent(&agent);
        self.agents.insert(agent_id, agent);
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
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
                SupervisorEvent::Output { agent_id, line } => {
                    let mut needs_alert = false;
                    if let Some(agent) = self.agents.get_mut(&agent_id) {
                        agent.push_log(line.clone());
                        if detect_help_signal(&line, &self.help_token) && !agent.needs_help {
                            agent.needs_help = true;
                            agent.status = AgentStatus::NeedsHelp;
                            needs_alert = true;
                            let _ = self.store.add_event(&agent_id, "needs_help", &line);
                        }
                        let _ = self.store.upsert_agent(agent);
                    }
                    if needs_alert {
                        self.add_alert(agent_id, "agent requested help".to_string());
                        ring_bell();
                    }
                }
                SupervisorEvent::Exited { agent_id, code } => {
                    let mut cleanup_info: Option<WorktreeInfo> = None;
                    let mut failed = false;
                    if let Some(agent) = self.agents.get_mut(&agent_id) {
                        agent.return_code = Some(code);
                        agent.updated_at = now_ts();
                        if code == 0 {
                            agent.status = AgentStatus::Done;
                            cleanup_info = agent.worktree.clone();
                            let _ = self.store.add_event(&agent_id, "exited", "exit=0");
                        } else {
                            failed = true;
                            agent.status = AgentStatus::Failed;
                            agent.needs_help = true;
                            let _ =
                                self.store
                                    .add_event(&agent_id, "exited", &format!("exit={code}"));
                        }
                        let _ = self.store.upsert_agent(agent);
                    }

                    if failed {
                        self.add_alert(
                            agent_id.clone(),
                            format!("process exited with code {code}"),
                        );
                        ring_bell();
                    }

                    if let Some(info) = cleanup_info {
                        self.cleanup_worktree_if_safe(&agent_id, &info);
                    }
                }
            }
        }
    }

    fn cleanup_worktree_if_safe(&mut self, agent_id: &str, info: &WorktreeInfo) {
        if let Some(manager) = &self.worktree_manager {
            match manager.cleanup_if_safe(info) {
                Ok((true, message)) => {
                    self.status_message = format!("{agent_id}: {message}");
                    let _ = self.store.add_event(agent_id, "cleanup", &message);
                }
                Ok((false, reason)) => {
                    self.add_alert(agent_id.to_string(), format!("cleanup skipped: {reason}"));
                }
                Err(err) => {
                    self.add_alert(agent_id.to_string(), format!("cleanup failed: {err}"));
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
            self.status_message = "no agents".to_string();
        }
    }

    fn ack_selected_alerts(&mut self) {
        let Some(id) = self.current_agent_id().cloned() else {
            return;
        };

        let mut changed = false;
        for alert in &mut self.alerts {
            if alert.agent_id == id && !alert.acknowledged {
                alert.acknowledged = true;
                changed = true;
            }
        }
        if !changed {
            return;
        }

        if let Some(agent) = self.agents.get_mut(&id) {
            agent.needs_help = false;
            if self.supervisor.is_running(&id) {
                agent.status = AgentStatus::Running;
            }
            agent.updated_at = now_ts();
            let _ = self.store.upsert_agent(agent);
        }

        let _ = self.store.mark_alerts_acknowledged(&id);
        self.status_message = format!("acknowledged alerts for {id}");
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

    fn current_agent_id(&self) -> Option<&String> {
        if self.order.is_empty() {
            return None;
        }
        self.order.get(self.selected.min(self.order.len() - 1))
    }

    fn current_agent(&self) -> Option<&Agent> {
        self.current_agent_id().and_then(|id| self.agents.get(id))
    }
}

pub fn run_app(args: Args) -> Result<()> {
    let mut app = App::new(args)
        .context("failed to initialize app (use --no-worktree if repo is not a git repo)")?;
    app.run()
}
