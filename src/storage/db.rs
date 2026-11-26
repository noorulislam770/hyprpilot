use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};

use super::config::data_dir;

#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub id: i64,
    pub title: String,
    pub provider_id: String,
    pub model: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    pub fn open_default() -> Result<Self> {
        let dir = data_dir();
        fs::create_dir_all(&dir)?;
        Self::open(&dir.join("history.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open sqlite db {}", path.display()))?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    pub fn db_path() -> PathBuf {
        data_dir().join("history.db")
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS conversations (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT NOT NULL,
                 provider_id TEXT NOT NULL,
                 model TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 role TEXT NOT NULL,
                 content TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, id);",
        )?;
        Ok(())
    }

    pub fn create_conversation(
        &self,
        title: &str,
        provider_id: &str,
        model: &str,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO conversations (title, provider_id, model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![title, provider_id, model, now, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_conversation_meta(
        &self,
        id: i64,
        title: Option<&str>,
        provider_id: &str,
        model: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        if let Some(t) = title {
            self.conn.execute(
                "UPDATE conversations SET title = ?1, provider_id = ?2, model = ?3, updated_at = ?4 WHERE id = ?5",
                params![t, provider_id, model, now, id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE conversations SET provider_id = ?1, model = ?2, updated_at = ?3 WHERE id = ?4",
                params![provider_id, model, now, id],
            )?;
        }
        Ok(())
    }

    pub fn add_message(&self, conversation_id: i64, role: &str, content: &str) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO messages (conversation_id, role, content, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![conversation_id, role, content, now],
        )?;
        let msg_id = self.conn.last_insert_rowid();
        self.conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            params![now, conversation_id],
        )?;
        Ok(msg_id)
    }

    pub fn list_conversations(&self, query: &str) -> Result<Vec<ConversationSummary>> {
        let trimmed = query.trim();
        let mut out = Vec::new();
        if trimmed.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT id, title, provider_id, model, updated_at
                 FROM conversations
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 100",
            )?;
            let rows = stmt.query_map([], map_summary)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let pattern = format!("%{trimmed}%");
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT c.id, c.title, c.provider_id, c.model, c.updated_at
                 FROM conversations c
                 LEFT JOIN messages m ON m.conversation_id = c.id
                 WHERE c.title LIKE ?1 OR m.content LIKE ?1
                 ORDER BY c.updated_at DESC, c.id DESC
                 LIMIT 100",
            )?;
            let rows = stmt.query_map(params![pattern], map_summary)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn get_messages(&self, conversation_id: i64) -> Result<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, role, content, created_at
             FROM messages
             WHERE conversation_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            Ok(ChatMessage {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_conversation(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn delete_last_message_if_role(&self, conversation_id: i64, role: &str) -> Result<bool> {
        let last: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, role FROM messages WHERE conversation_id = ?1 ORDER BY id DESC LIMIT 1",
                params![conversation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        if let Some((msg_id, msg_role)) = last {
            if msg_role == role {
                self.conn
                    .execute("DELETE FROM messages WHERE id = ?1", params![msg_id])?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn map_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationSummary> {
    Ok(ConversationSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        provider_id: row.get(2)?,
        model: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

pub fn derive_title_from_prompt(prompt: &str) -> String {
    let single_line: String = prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = single_line.chars();
    let truncated: String = chars.by_ref().take(48).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else if truncated.is_empty() {
        "New Chat".to_string()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_and_search_work() {
        let db = HistoryDb::open(Path::new(":memory:")).unwrap();
        let c1 = db
            .create_conversation("Find directory in Linux", "openai", "gpt-4o-mini")
            .unwrap();
        db.add_message(c1, "user", "how to find a directory inside linux")
            .unwrap();
        db.add_message(c1, "assistant", "Use `find / -type d -name dirname`.")
            .unwrap();

        let c2 = db
            .create_conversation("Rust async", "anthropic", "claude-sonnet-4")
            .unwrap();
        db.add_message(c2, "user", "explain tokio spawn").unwrap();

        let all = db.list_conversations("").unwrap();
        assert_eq!(all.len(), 2);

        let search_msg = db.list_conversations("find / -type d").unwrap();
        assert_eq!(search_msg.len(), 1);
        assert_eq!(search_msg[0].id, c1);

        let msgs = db.get_messages(c1).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");

        assert!(db.delete_last_message_if_role(c1, "assistant").unwrap());
        assert!(!db.delete_last_message_if_role(c1, "assistant").unwrap());
        assert_eq!(db.get_messages(c1).unwrap().len(), 1);

        db.delete_conversation(c1).unwrap();
        assert_eq!(db.list_conversations("").unwrap().len(), 1);
    }
}
