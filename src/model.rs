use clap::Parser;
use rand::Rng;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const ADJECTIVES: &[&str] = &[
    "agile", "brisk", "calm", "clever", "eager", "fierce", "focused", "nimble", "rapid", "steady",
];

const ANIMALS: &[&str] = &[
    "badger", "falcon", "fox", "lynx", "otter", "owl", "panther", "raven", "tiger", "wolf",
];

#[derive(Parser, Debug, Clone)]
#[command(name = "codex-mux", about = "Codex multi-agent terminal supervisor")]
pub struct Args {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    #[arg(long, default_value = "main")]
    pub base_branch: String,

    #[arg(long, default_value = ".worktrees")]
    pub worktrees_dir: String,

    #[arg(long, default_value = ".codex_mux/session.db")]
    pub db_path: PathBuf,

    #[arg(long, default_value = "[[NEEDS_HELP]]")]
    pub help_token: String,

    #[arg(long)]
    pub no_worktree: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub slug: String,
    pub branch: String,
    pub path: PathBuf,
    pub base_branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Starting,
    Running,
    NeedsHelp,
    Failed,
    Done,
    Blocked,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::NeedsHelp => "needs_help",
            Self::Failed => "failed",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::Starting => "S",
            Self::Running => "R",
            Self::NeedsHelp => "!",
            Self::Failed => "F",
            Self::Done => "D",
            Self::Blocked => "B",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub model: String,
    pub prompt: String,
    pub run_dir: PathBuf,
    pub status: AgentStatus,
    pub worktree: Option<WorktreeInfo>,
    pub pid: Option<u32>,
    pub return_code: Option<i32>,
    pub created_at: i64,
    pub updated_at: i64,
    pub needs_help: bool,
}

impl Agent {
    pub fn new(id: String, model: String, prompt: String, run_dir: PathBuf) -> Self {
        let now = now_ts();
        Self {
            id,
            model,
            prompt,
            run_dir,
            status: AgentStatus::Starting,
            worktree: None,
            pid: None,
            return_code: None,
            created_at: now,
            updated_at: now,
            needs_help: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub agent_id: String,
    pub message: String,
    pub acknowledged: bool,
    pub created_at: i64,
}

impl Alert {
    pub fn new(agent_id: String, message: String) -> Self {
        Self {
            agent_id,
            message,
            acknowledged: false,
            created_at: now_ts(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    Started { agent_id: String, pid: u32 },
    OutputChunk { agent_id: String, data: Vec<u8> },
    Exited { agent_id: String, code: i32 },
}

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn first_non_empty(primary: &str, secondary: &str) -> String {
    let p = primary.trim();
    if !p.is_empty() {
        return p.to_string();
    }
    let s = secondary.trim();
    if !s.is_empty() {
        return s.to_string();
    }
    "unknown error".to_string()
}

pub fn generate_slug() -> String {
    let mut rng = rand::rng();
    let adjective = ADJECTIVES[rng.random_range(0..ADJECTIVES.len())];
    let animal = ANIMALS[rng.random_range(0..ANIMALS.len())];
    let hex = (0..4)
        .map(|_| {
            let n: u8 = rng.random_range(0..16);
            char::from_digit(n as u32, 16).unwrap_or('0')
        })
        .collect::<String>();
    format!("{adjective}-{animal}-{hex}")
}

pub fn detect_help_signal(text: &str, help_token: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains(&help_token.to_lowercase())
        || lower.contains("needs help")
        || lower.contains("need help")
}

pub fn build_codex_launch_command(model: Option<&str>, prompt: Option<&str>) -> String {
    let mut argv = vec!["codex".to_string()];

    if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
        argv.push("-m".to_string());
        argv.push(model.to_string());
    }

    if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
        argv.push(prompt.to_string());
    }

    argv.into_iter()
        .map(|part| shell_escape(&part))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value.bytes().all(|byte| {
        matches!(
            byte,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'/'
                | b'.'
                | b'-'
                | b'_'
                | b':'
                | b'='
                | b','
                | b'+'
        )
    }) {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn next_agent_id() -> String {
    let now = now_ts();
    let mut rng = rand::rng();
    let suffix = (0..4)
        .map(|_| {
            let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
            let idx = rng.random_range(0..alphabet.len());
            alphabet[idx] as char
        })
        .collect::<String>();
    format!("agent-{now}-{suffix}")
}

pub fn ring_bell() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_format() {
        let slug = generate_slug();
        let ok = slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit());
        assert!(ok);
        let parts = slug.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].len(), 4);
    }

    #[test]
    fn help_signal_detection() {
        assert!(detect_help_signal(
            "[[NEEDS_HELP]] blocked",
            "[[NEEDS_HELP]]"
        ));
        assert!(detect_help_signal("agent needs help now", "[[NEEDS_HELP]]"));
        assert!(!detect_help_signal("all good", "[[NEEDS_HELP]]"));
    }

    #[test]
    fn shell_escape_handles_quotes() {
        assert_eq!(shell_escape("plain-text"), "plain-text");
        assert_eq!(shell_escape("fix auth bug"), "'fix auth bug'");
        assert_eq!(shell_escape("it's broken"), "'it'\"'\"'s broken'");
    }

    #[test]
    fn codex_launch_command_builds_model_and_prompt() {
        let command = build_codex_launch_command(Some("gpt-5"), Some("fix auth bug"));
        assert_eq!(command, "codex -m gpt-5 'fix auth bug'");
    }

    #[test]
    fn codex_launch_command_uses_default_model_when_unspecified() {
        let command = build_codex_launch_command(None, Some("hello"));
        assert_eq!(command, "codex hello");
    }

    #[test]
    fn codex_launch_command_without_prompt_just_spawns_codex() {
        let command = build_codex_launch_command(None, None);
        assert_eq!(command, "codex");
    }
}
