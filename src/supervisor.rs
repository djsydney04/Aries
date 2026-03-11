use crate::model::SupervisorEvent;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

pub struct AgentSupervisor {
    tx: Sender<SupervisorEvent>,
    children: Arc<Mutex<HashMap<String, Arc<Mutex<Child>>>>>,
}

impl AgentSupervisor {
    pub fn new(tx: Sender<SupervisorEvent>) -> Self {
        Self {
            tx,
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn start(&self, agent_id: &str, argv: Vec<String>, cwd: &Path) -> Result<()> {
        if argv.is_empty() {
            bail!("empty command");
        }

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn: {}", argv.join(" ")))?;
        let pid = child.id();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let child_ref = Arc::new(Mutex::new(child));
        self.children
            .lock()
            .expect("children mutex poisoned")
            .insert(agent_id.to_string(), Arc::clone(&child_ref));

        if let Some(out) = stdout {
            Self::spawn_reader(agent_id.to_string(), out, self.tx.clone());
        }
        if let Some(err) = stderr {
            Self::spawn_reader(agent_id.to_string(), err, self.tx.clone());
        }

        let tx = self.tx.clone();
        let map = Arc::clone(&self.children);
        let wait_agent_id = agent_id.to_string();
        thread::spawn(move || {
            let code = {
                let mut child = child_ref.lock().expect("child mutex poisoned");
                child.wait().ok().and_then(|s| s.code()).unwrap_or(-1)
            };

            let _ = tx.send(SupervisorEvent::Exited {
                agent_id: wait_agent_id.clone(),
                code,
            });

            map.lock()
                .expect("children mutex poisoned")
                .remove(&wait_agent_id);
        });

        let _ = self.tx.send(SupervisorEvent::Started {
            agent_id: agent_id.to_string(),
            pid,
        });

        Ok(())
    }

    pub fn stop(&self, agent_id: &str) {
        let maybe_child = self
            .children
            .lock()
            .expect("children mutex poisoned")
            .get(agent_id)
            .cloned();

        if let Some(child) = maybe_child {
            let _ = child.lock().expect("child mutex poisoned").kill();
        }
    }

    pub fn stop_all(&self) {
        let ids = self
            .children
            .lock()
            .expect("children mutex poisoned")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            self.stop(&id);
        }
    }

    pub fn is_running(&self, agent_id: &str) -> bool {
        self.children
            .lock()
            .expect("children mutex poisoned")
            .contains_key(agent_id)
    }

    fn spawn_reader<R: std::io::Read + Send + 'static>(
        agent_id: String,
        reader: R,
        tx: Sender<SupervisorEvent>,
    ) {
        thread::spawn(move || {
            let mut buf = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match buf.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                        let _ = tx.send(SupervisorEvent::Output {
                            agent_id: agent_id.clone(),
                            line: trimmed,
                        });
                    }
                    Err(_) => break,
                }
            }
        });
    }
}
