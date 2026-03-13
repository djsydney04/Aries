use crate::model::SupervisorEvent;
use anyhow::{Context, Result, bail};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

struct TerminalSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
}

pub struct AgentSupervisor {
    tx: Sender<SupervisorEvent>,
    sessions: Arc<Mutex<HashMap<String, TerminalSession>>>,
}

impl AgentSupervisor {
    pub fn new(tx: Sender<SupervisorEvent>) -> Self {
        Self {
            tx,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn start(
        &self,
        agent_id: &str,
        cwd: &Path,
        initial_command: Option<&str>,
        size: PtySize,
    ) -> Result<()> {
        if self.is_running(agent_id) {
            bail!("pane {agent_id} is already running");
        }

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size).context("failed to allocate PTY")?;

        let shell = shell_path();
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-i");
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "codex-mux");

        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to open PTY writer")?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn shell in {}", cwd.display()))?;
        drop(pair.slave);

        let pid = child.process_id().unwrap_or_default();
        let killer = child.clone_killer();

        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .insert(
                agent_id.to_string(),
                TerminalSession {
                    master: Arc::new(Mutex::new(pair.master)),
                    writer: Arc::new(Mutex::new(writer)),
                    killer: Arc::new(Mutex::new(killer)),
                },
            );

        self.spawn_reader(agent_id.to_string(), reader);
        self.spawn_waiter(agent_id.to_string(), child);

        let _ = self.tx.send(SupervisorEvent::Started {
            agent_id: agent_id.to_string(),
            pid,
        });

        if let Some(command) = initial_command.filter(|command| !command.trim().is_empty()) {
            let mut bytes = command.as_bytes().to_vec();
            bytes.push(b'\r');
            self.send_input(agent_id, &bytes)?;
        }

        Ok(())
    }

    pub fn send_input(&self, agent_id: &str, bytes: &[u8]) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions mutex poisoned");
        let session = sessions
            .get(agent_id)
            .with_context(|| format!("pane {agent_id} is not running"))?;
        let mut writer = session.writer.lock().expect("writer mutex poisoned");
        writer
            .write_all(bytes)
            .with_context(|| format!("failed to write to {agent_id}"))?;
        writer.flush().context("failed to flush PTY writer")?;
        Ok(())
    }

    pub fn resize(&self, agent_id: &str, size: PtySize) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions mutex poisoned");
        let session = sessions
            .get(agent_id)
            .with_context(|| format!("pane {agent_id} is not running"))?;
        session
            .master
            .lock()
            .expect("master mutex poisoned")
            .resize(size)
            .with_context(|| format!("failed to resize {agent_id}"))?;
        Ok(())
    }

    pub fn stop(&self, agent_id: &str) {
        let maybe_killer = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .get(agent_id)
            .map(|session| Arc::clone(&session.killer));

        if let Some(killer) = maybe_killer {
            let _ = killer.lock().expect("killer mutex poisoned").kill();
        }
    }

    pub fn stop_all(&self) {
        let ids = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            self.stop(&id);
        }
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .contains_key(agent_id)
    }

    fn spawn_reader(&self, agent_id: String, mut reader: Box<dyn Read + Send>) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(len) => {
                        let _ = tx.send(SupervisorEvent::OutputChunk {
                            agent_id: agent_id.clone(),
                            data: buf[..len].to_vec(),
                        });
                    }
                    Err(_) => break,
                }
            }
        });
    }

    fn spawn_waiter(
        &self,
        agent_id: String,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let tx = self.tx.clone();
        let sessions = Arc::clone(&self.sessions);
        thread::spawn(move || {
            let code = child
                .wait()
                .ok()
                .map(|status| status.exit_code() as i32)
                .unwrap_or(-1);

            sessions
                .lock()
                .expect("sessions mutex poisoned")
                .remove(&agent_id);

            let _ = tx.send(SupervisorEvent::Exited { agent_id, code });
        });
    }
}

fn shell_path() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}
