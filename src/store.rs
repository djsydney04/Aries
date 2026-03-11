use crate::model::{Agent, Alert, now_ts};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::fs;
use std::path::Path;

pub struct SessionStore {
    pub conn: Connection,
}

impl SessionStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create db directory: {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("failed to open sqlite db: {}", db_path.display()))?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                model TEXT NOT NULL,
                prompt TEXT NOT NULL,
                status TEXT NOT NULL,
                branch TEXT,
                worktree TEXT,
                pid INTEGER,
                return_code INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                message TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS alerts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT NOT NULL,
                message TEXT NOT NULL,
                acknowledged INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            ",
        )?;
        Ok(())
    }

    pub fn upsert_agent(&self, agent: &Agent) -> Result<()> {
        let branch = agent.worktree.as_ref().map(|w| w.branch.clone());
        let worktree = agent
            .worktree
            .as_ref()
            .map(|w| w.path.to_string_lossy().to_string());

        self.conn.execute(
            "
            INSERT INTO agents (
                id, model, prompt, status, branch, worktree, pid, return_code, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                status=excluded.status,
                branch=excluded.branch,
                worktree=excluded.worktree,
                pid=excluded.pid,
                return_code=excluded.return_code,
                updated_at=excluded.updated_at
            ",
            params![
                agent.id,
                agent.model,
                agent.prompt,
                agent.status.as_str(),
                branch,
                worktree,
                agent.pid,
                agent.return_code,
                agent.created_at,
                agent.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn add_event(&self, agent_id: &str, event_type: &str, message: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (agent_id, event_type, message, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![agent_id, event_type, message, now_ts()],
        )?;
        Ok(())
    }

    pub fn add_alert(&self, alert: &Alert) -> Result<()> {
        self.conn.execute(
            "INSERT INTO alerts (agent_id, message, acknowledged, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                alert.agent_id,
                alert.message,
                if alert.acknowledged { 1 } else { 0 },
                alert.created_at
            ],
        )?;
        Ok(())
    }

    pub fn mark_alerts_acknowledged(&self, agent_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE alerts SET acknowledged=1 WHERE agent_id=?1 AND acknowledged=0",
            params![agent_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, AgentStatus};

    #[test]
    fn session_store_upsert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let store = SessionStore::open(&db_path).expect("open store");

        let mut agent = Agent::new("a1".to_string(), "model".to_string(), "prompt".to_string());
        agent.status = AgentStatus::Running;
        store.upsert_agent(&agent).expect("upsert");

        let row = store
            .conn
            .query_row("SELECT id, status FROM agents WHERE id='a1'", [], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .expect("query row");

        assert_eq!(row.0, "a1");
        assert_eq!(row.1, "running");
    }
}
