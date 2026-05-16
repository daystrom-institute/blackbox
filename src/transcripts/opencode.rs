use std::path::{Path, PathBuf};

use rusqlite::{OpenFlags, params};
use serde_json::Value;

use crate::orchestration::providers::Provider;

use super::adapters::{TranscriptReadAdapter, TranscriptScanTarget};
use super::types::{
    NormalizedToolCall, NormalizedTranscriptEvent, RawTranscriptRef, TranscriptBatch,
    TranscriptCursor, TranscriptEventKind, TranscriptLocation, TranscriptReadError, TranscriptRole,
    TranscriptStorage,
};

const REQUIRED_TABLES: &[&str] = &["session", "message", "part"];
const DEFAULT_DB_NAME: &str = "opencode.db";
const ALT_DB_NAME: &str = "opencode-local.db";

#[derive(Debug, Clone)]
pub(crate) struct OpencodeTranscriptAdapter {
    db_dir: PathBuf,
    provider: Provider,
}

impl OpencodeTranscriptAdapter {
    pub(crate) fn new(db_dir: impl Into<PathBuf>, provider: Provider) -> Self {
        Self {
            db_dir: db_dir.into(),
            provider,
        }
    }

    pub(crate) fn default_db_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".local").join("share").join("opencode"))
    }

    fn resolve_db(&self, session_id: &str) -> Result<PathBuf, TranscriptReadError> {
        for name in [DEFAULT_DB_NAME, ALT_DB_NAME] {
            let path = self.db_dir.join(name);
            if !path.exists() {
                continue;
            }
            if session_id.is_empty() || self.db_contains_session(&path, session_id)? {
                return Ok(path);
            }
        }
        let fallback = self.db_dir.join(DEFAULT_DB_NAME);
        if fallback.exists() {
            Ok(fallback)
        } else {
            Err(TranscriptReadError::Io {
                op: "resolve_db",
                path: self.db_dir.clone(),
                kind: std::io::ErrorKind::NotFound,
                message: format!("no opencode database found in {}", self.db_dir.display()),
            })
        }
    }

    fn db_contains_session(
        &self,
        db_path: &Path,
        session_id: &str,
    ) -> Result<bool, TranscriptReadError> {
        let conn = self.open_readonly(db_path)?;
        let table_set = probe_tables(&conn)?;
        if !table_set.contains("session") {
            return Ok(false);
        }
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| rusqlite_err("session_count", db_path, e))?;
        Ok(count > 0)
    }

    fn open_readonly(&self, path: &Path) -> Result<rusqlite::Connection, TranscriptReadError> {
        rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| TranscriptReadError::Io {
            op: "sqlite_open",
            path: path.to_path_buf(),
            kind: std::io::ErrorKind::Other,
            message: e.to_string(),
        })
    }

    fn probe_schema(&self, db_path: &Path) -> Result<rusqlite::Connection, TranscriptReadError> {
        let conn = self.open_readonly(db_path)?;
        let tables = probe_tables(&conn)?;
        let missing: Vec<String> = REQUIRED_TABLES
            .iter()
            .filter(|t| !tables.contains(**t))
            .map(|s| s.to_string())
            .collect();
        if !missing.is_empty() {
            return Err(TranscriptReadError::SchemaDrift {
                provider: self.provider,
                path: db_path.to_path_buf(),
                expected: "session, message, part",
                observed: tables.into_iter().collect(),
            });
        }
        Ok(conn)
    }

    fn read_messages(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
        cursor: Option<&TranscriptCursor>,
        db_path: &Path,
    ) -> Result<Vec<NormalizedTranscriptEvent>, TranscriptReadError> {
        let (after_ts, after_id) = match cursor {
            None => (0i64, ""),
            Some(TranscriptCursor::SqliteRow {
                timestamp_ms, id, ..
            }) => (*timestamp_ms, id.as_str()),
            Some(other) => {
                return Err(TranscriptReadError::UnsupportedCursor {
                    provider: self.provider,
                    cursor: other.clone(),
                });
            }
        };

        let mut stmt = conn
            .prepare(
                "SELECT id, time_created, data FROM message \
                 WHERE session_id = ?1 \
                 AND (time_created > ?2 OR (time_created = ?2 AND id > ?3)) \
                 ORDER BY time_created, id",
            )
            .map_err(|e| rusqlite_err("prepare_messages", db_path, e))?;

        let rows = stmt
            .query_map(params![session_id, after_ts, after_id], |row| {
                Ok(MessageRow {
                    id: row.get(0)?,
                    time_created: row.get(1)?,
                    data: row.get(2)?,
                })
            })
            .map_err(|e| rusqlite_err("query_messages", db_path, e))?;

        let mut events = Vec::new();
        for row in rows {
            let msg = row.map_err(|e| rusqlite_err("read_message", db_path, e))?;
            let msg_data: Value = serde_json::from_str(&msg.data).unwrap_or_default();
            let role = msg_data["role"].as_str().unwrap_or("");
            let cwd = msg_data["path"]["cwd"].as_str();
            let timestamp = format_timestamp(msg.time_created);

            let mut part_events =
                self.read_parts(conn, &msg.id, session_id, role, cwd, &timestamp, db_path)?;

            let msg_role = match role {
                "user" => TranscriptRole::User,
                "assistant" => TranscriptRole::Assistant,
                _ => TranscriptRole::Developer,
            };

            if part_events.is_empty() {
                let entity_id = format!("opencode:{session_id}:{}:0", msg.id);
                events.push(make_event(
                    self.provider,
                    session_id,
                    &timestamp,
                    cwd,
                    msg_role,
                    TranscriptEventKind::Message,
                    msg_data["summary"].as_str().unwrap_or_default(),
                    RawTranscriptRef::provider_event(
                        self.provider,
                        TranscriptStorage::Sqlite,
                        db_path,
                        &msg.id,
                        &entity_id,
                    ),
                ));
            } else {
                events.append(&mut part_events);
            }
        }
        Ok(events)
    }

    fn read_parts(
        &self,
        conn: &rusqlite::Connection,
        message_id: &str,
        session_id: &str,
        msg_role: &str,
        cwd: Option<&str>,
        timestamp: &str,
        db_path: &Path,
    ) -> Result<Vec<NormalizedTranscriptEvent>, TranscriptReadError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, data FROM part \
                 WHERE message_id = ?1 \
                 ORDER BY time_created, id",
            )
            .map_err(|e| rusqlite_err("prepare_parts", db_path, e))?;

        let rows = stmt
            .query_map(params![message_id], |row| {
                Ok(PartRow {
                    id: row.get(0)?,
                    data: row.get(1)?,
                })
            })
            .map_err(|e| rusqlite_err("query_parts", db_path, e))?;

        let mut events = Vec::new();
        for (part_idx, row) in rows.enumerate() {
            let part = row.map_err(|e| rusqlite_err("read_part", db_path, e))?;
            let part_data: Value = serde_json::from_str(&part.data).unwrap_or_default();
            let part_type = part_data["type"].as_str().unwrap_or("");

            let entity_id = format!("opencode:{session_id}:{message_id}:{part_idx}");
            let raw = RawTranscriptRef::provider_event(
                self.provider,
                TranscriptStorage::Sqlite,
                db_path,
                &part.id,
                &entity_id,
            );

            match part_type {
                "text" => {
                    let text = part_data["text"].as_str().unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    let role = if msg_role == "user" {
                        TranscriptRole::User
                    } else {
                        TranscriptRole::Assistant
                    };
                    events.push(make_event(
                        self.provider,
                        session_id,
                        timestamp,
                        cwd,
                        role,
                        TranscriptEventKind::Message,
                        text,
                        raw,
                    ));
                }
                "reasoning" => {
                    let text = part_data["text"].as_str().unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    events.push(make_event(
                        self.provider,
                        session_id,
                        timestamp,
                        cwd,
                        TranscriptRole::Thinking,
                        TranscriptEventKind::Thinking,
                        text,
                        raw,
                    ));
                }
                "tool" => {
                    let tool_name = part_data["tool"].as_str().unwrap_or("unknown");
                    let call_id = part_data["callID"].as_str().unwrap_or("");
                    let state = &part_data["state"];
                    let status = state["status"].as_str().unwrap_or("");

                    let input = state["input"].clone();
                    let input_str = serde_json::to_string(&input).unwrap_or_default();
                    let tool_use_content = format!("tool:{tool_name} {input_str}");

                    let tool_call = NormalizedToolCall {
                        kind: crate::parser::ToolCallKind::Bash,
                        name: tool_name.to_string(),
                        tool_use_id: if call_id.is_empty() {
                            None
                        } else {
                            Some(call_id.to_string())
                        },
                        input,
                    };

                    let mut tool_use = make_event(
                        self.provider,
                        session_id,
                        timestamp,
                        cwd,
                        TranscriptRole::ToolUse,
                        TranscriptEventKind::ToolUse,
                        &tool_use_content,
                        raw.clone(),
                    );
                    tool_use.tool_call = Some(tool_call);
                    events.push(tool_use);

                    if status == "completed" || status == "error" {
                        let output = state["output"].as_str().unwrap_or("");
                        let _is_error = status == "error"
                            || output.contains("error")
                            || output.contains("Error");
                        let result_content = if output.is_empty() {
                            format!("result:{call_id} (empty)")
                        } else {
                            format!("result:{call_id} {}", truncate_str(output, 2000))
                        };
                        let result_entity =
                            format!("opencode:{session_id}:{message_id}:{part_idx}-result");
                        let result_raw = RawTranscriptRef::provider_event(
                            self.provider,
                            TranscriptStorage::Sqlite,
                            db_path,
                            format!("{}-result", part.id),
                            &result_entity,
                        );
                        let mut result_event = make_event(
                            self.provider,
                            session_id,
                            timestamp,
                            cwd,
                            TranscriptRole::ToolResult,
                            TranscriptEventKind::ToolResult,
                            &result_content,
                            result_raw,
                        );
                        result_event.tool_call = Some(NormalizedToolCall {
                            kind: crate::parser::ToolCallKind::Bash,
                            name: tool_name.to_string(),
                            tool_use_id: if call_id.is_empty() {
                                None
                            } else {
                                Some(call_id.to_string())
                            },
                            input: Value::Null,
                        });
                        events.push(result_event);
                    }
                }
                _ => {}
            }
        }
        Ok(events)
    }

    fn last_cursor(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
    ) -> Result<Option<TranscriptCursor>, TranscriptReadError> {
        let result: Option<(i64, String)> = conn
            .query_row(
                "SELECT time_created, id FROM message \
                 WHERE session_id = ?1 \
                 ORDER BY time_created DESC, id DESC LIMIT 1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        Ok(result.map(|(ts, id)| TranscriptCursor::SqliteRow {
            table: "message".to_string(),
            timestamp_ms: ts,
            id,
        }))
    }
}

impl TranscriptReadAdapter for OpencodeTranscriptAdapter {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        if session_id.is_empty() || session_id == "pending" {
            return Ok(None);
        }
        let db_path = match self.resolve_db(session_id) {
            Ok(p) => p,
            Err(TranscriptReadError::Io { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let conn = match self.probe_schema(&db_path) {
            Ok(c) => c,
            Err(TranscriptReadError::SchemaDrift { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| rusqlite_err("session_count", &db_path, e))?;
        if count == 0 {
            return Ok(None);
        }
        let directory: Option<String> = conn
            .query_row(
                "SELECT directory FROM session WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .ok();
        Ok(Some(TranscriptLocation {
            provider: self.provider,
            storage: TranscriptStorage::Sqlite,
            path: db_path,
            account: Some("opencode".to_string()),
            session_id: Some(session_id.to_string()),
            project: None,
            cwd: directory,
            is_subagent: false,
        }))
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        if target != TranscriptScanTarget::Sessions {
            return Ok(Vec::new());
        }
        let mut locations = Vec::new();
        for name in [DEFAULT_DB_NAME, ALT_DB_NAME] {
            let db_path = self.db_dir.join(name);
            if !db_path.exists() {
                continue;
            }
            let conn = match self.probe_schema(&db_path) {
                Ok(conn) => conn,
                Err(TranscriptReadError::SchemaDrift { .. }) => continue,
                Err(TranscriptReadError::Io { .. }) => continue,
                Err(err) => return Err(err),
            };
            let mut stmt = conn
                .prepare("SELECT id, directory FROM session")
                .map_err(|e| rusqlite_err("session_scan_prepare", &db_path, e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| rusqlite_err("session_scan", &db_path, e))?;
            for row in rows {
                let (session_id, directory) =
                    row.map_err(|e| rusqlite_err("session_scan_row", &db_path, e))?;
                locations.push(TranscriptLocation {
                    provider: self.provider,
                    storage: TranscriptStorage::Sqlite,
                    path: db_path.clone(),
                    account: Some("opencode".to_string()),
                    session_id: Some(session_id),
                    project: None,
                    cwd: directory,
                    is_subagent: false,
                });
            }
        }
        Ok(locations)
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        if location.provider != self.provider && !is_opencode_provider(location.provider) {
            return Err(TranscriptReadError::InvalidLocation {
                provider: self.provider,
                path: location.path.clone(),
                reason: "location belongs to a different provider",
            });
        }
        let session_id =
            location
                .session_id
                .as_deref()
                .ok_or_else(|| TranscriptReadError::InvalidLocation {
                    provider: self.provider,
                    path: location.path.clone(),
                    reason: "sqlite location requires session_id",
                })?;

        let conn = self.probe_schema(&location.path)?;
        let events = self.read_messages(&conn, session_id, cursor, &location.path)?;
        let next_cursor = self.last_cursor(&conn, session_id)?;

        Ok(TranscriptBatch {
            location: location.clone(),
            cursor: next_cursor,
            events,
            reached_end: true,
        })
    }
}

fn is_opencode_provider(p: Provider) -> bool {
    matches!(p, Provider::Glm | Provider::Deepseek | Provider::Inception)
}

struct MessageRow {
    id: String,
    time_created: i64,
    data: String,
}

struct PartRow {
    id: String,
    data: String,
}

fn probe_tables(
    conn: &rusqlite::Connection,
) -> Result<std::collections::HashSet<String>, TranscriptReadError> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|e| TranscriptReadError::Io {
            op: "sqlite_master",
            path: PathBuf::new(),
            kind: std::io::ErrorKind::Other,
            message: e.to_string(),
        })?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| TranscriptReadError::Io {
            op: "sqlite_tables",
            path: PathBuf::new(),
            kind: std::io::ErrorKind::Other,
            message: e.to_string(),
        })?;
    let mut tables = std::collections::HashSet::new();
    for name in rows.flatten() {
        tables.insert(name);
    }
    Ok(tables)
}

fn make_event(
    provider: Provider,
    session_id: &str,
    timestamp: &str,
    cwd: Option<&str>,
    role: TranscriptRole,
    kind: TranscriptEventKind,
    content: &str,
    raw: RawTranscriptRef,
) -> NormalizedTranscriptEvent {
    NormalizedTranscriptEvent {
        provider,
        role,
        kind,
        content: content.to_string(),
        session_id: session_id.to_string(),
        timestamp: Some(timestamp.to_string()),
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: cwd.map(String::from),
        tool_call: None,
        raw,
    }
}

fn format_timestamp(ts_ms: i64) -> String {
    let secs = ts_ms / 1000;
    let subsec = ts_ms % 1000;
    format!("{}.{:03}", secs, subsec.abs())
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

fn rusqlite_err(op: &'static str, path: &Path, e: rusqlite::Error) -> TranscriptReadError {
    TranscriptReadError::Io {
        op,
        path: path.to_path_buf(),
        kind: std::io::ErrorKind::Other,
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn create_fixture_db(path: &Path) -> Connection {
        let conn = Connection::open(path).expect("create fixture db");
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                slug TEXT NOT NULL,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                version TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .expect("create tables");
        conn
    }

    fn seed_session(conn: &Connection, session_id: &str, directory: &str) {
        conn.execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
             VALUES (?1, 'proj1', 'test', ?2, 'Test Session', '1', 1000, 1000)",
            params![session_id, directory],
        )
        .expect("seed session");
    }

    fn seed_message(
        conn: &Connection,
        id: &str,
        session_id: &str,
        time_created: i64,
        role: &str,
        cwd: &str,
    ) {
        let data = serde_json::json!({
            "role": role,
            "path": {"cwd": cwd},
            "summary": {"diffs": []}
        })
        .to_string();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![id, session_id, time_created, data],
        )
        .expect("seed message");
    }

    fn seed_text_part(
        conn: &Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        time_created: i64,
        text: &str,
    ) {
        let data = serde_json::json!({"type": "text", "text": text}).to_string();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![id, message_id, session_id, time_created, data],
        )
        .expect("seed text part");
    }

    fn seed_tool_part(
        conn: &Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        time_created: i64,
        tool_name: &str,
        call_id: &str,
        input: &serde_json::Value,
        output: &str,
        status: &str,
    ) {
        let data = serde_json::json!({
            "type": "tool",
            "tool": tool_name,
            "callID": call_id,
            "state": {
                "status": status,
                "input": input,
                "output": output
            }
        })
        .to_string();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![id, message_id, session_id, time_created, data],
        )
        .expect("seed tool part");
    }

    #[test]
    fn locate_finds_session_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_abc123", "/repo/test");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_abc123").unwrap().unwrap();
        assert_eq!(loc.session_id.as_deref(), Some("ses_abc123"));
        assert_eq!(loc.cwd.as_deref(), Some("/repo/test"));
        assert_eq!(loc.storage, TranscriptStorage::Sqlite);
        assert_eq!(loc.account.as_deref(), Some("opencode"));
    }

    #[test]
    fn locate_returns_none_for_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_known", "/repo");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        assert!(adapter.locate("ses_unknown").unwrap().is_none());
    }

    #[test]
    fn locate_returns_none_for_pending() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let _ = create_fixture_db(&db_path);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        assert!(adapter.locate("pending").unwrap().is_none());
        assert!(adapter.locate("").unwrap().is_none());
    }

    #[test]
    fn schema_drift_on_missing_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY);")
            .unwrap();
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = TranscriptLocation {
            provider: Provider::Glm,
            storage: TranscriptStorage::Sqlite,
            path: db_path,
            account: Some("opencode".into()),
            session_id: Some("ses_123".into()),
            project: None,
            cwd: None,
            is_subagent: false,
        };
        let err = adapter.read_since(&loc, None).unwrap_err();
        match &err {
            TranscriptReadError::SchemaDrift {
                expected, observed, ..
            } => {
                assert_eq!(*expected, "session, message, part");
                assert!(!observed.contains(&"message".to_string()));
            }
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    #[test]
    fn read_snapshot_returns_user_and_assistant_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_read1", "/repo/read");
        seed_message(&conn, "msg_1", "ses_read1", 1000, "user", "/repo/read");
        seed_text_part(
            &conn,
            "prt_1",
            "msg_1",
            "ses_read1",
            1010,
            "hello assistant",
        );
        seed_message(&conn, "msg_2", "ses_read1", 2000, "assistant", "/repo/read");
        seed_text_part(&conn, "prt_2", "msg_2", "ses_read1", 2010, "hello user");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_read1").unwrap().unwrap();
        let snapshot = adapter.load_snapshot(&loc).unwrap();

        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].role, TranscriptRole::User);
        assert_eq!(snapshot.events[0].content, "hello assistant");
        assert_eq!(snapshot.events[1].role, TranscriptRole::Assistant);
        assert_eq!(snapshot.events[1].content, "hello user");
    }

    #[test]
    fn read_since_with_cursor_skips_earlier_messages() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_cursor", "/repo");
        seed_message(&conn, "msg_1", "ses_cursor", 1000, "user", "/repo");
        seed_text_part(&conn, "prt_1", "msg_1", "ses_cursor", 1010, "old event");
        seed_message(&conn, "msg_2", "ses_cursor", 2000, "assistant", "/repo");
        seed_text_part(&conn, "prt_2", "msg_2", "ses_cursor", 2010, "new event");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_cursor").unwrap().unwrap();
        let cursor = TranscriptCursor::SqliteRow {
            table: "message".to_string(),
            timestamp_ms: 1000,
            id: "msg_1".to_string(),
        };
        let batch = adapter.read_since(&loc, Some(&cursor)).unwrap();

        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].content, "new event");
    }

    #[test]
    fn tool_parts_emit_tool_use_and_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_tool", "/repo");
        seed_message(&conn, "msg_tool", "ses_tool", 1000, "assistant", "/repo");
        seed_tool_part(
            &conn,
            "prt_tool1",
            "msg_tool",
            "ses_tool",
            1010,
            "Bash",
            "call_abc",
            &serde_json::json!({"command": "rtk true"}),
            "exit 0",
            "completed",
        );
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_tool").unwrap().unwrap();
        let snapshot = adapter.load_snapshot(&loc).unwrap();

        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].role, TranscriptRole::ToolUse);
        assert_eq!(snapshot.events[1].role, TranscriptRole::ToolResult);
        assert!(snapshot.events[0].content.starts_with("tool:Bash"));
        assert!(snapshot.events[1].content.starts_with("result:call_abc"));
    }

    #[test]
    fn reasoning_parts_map_to_thinking_role() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_think", "/repo");
        seed_message(&conn, "msg_think", "ses_think", 1000, "assistant", "/repo");
        let reasoning_data =
            serde_json::json!({"type": "reasoning", "text": "let me think about this"}).to_string();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES ('prt_r1', 'msg_think', 'ses_think', 1010, 1010, ?1)",
            params![reasoning_data],
        )
        .unwrap();
        seed_text_part(
            &conn,
            "prt_t1",
            "msg_think",
            "ses_think",
            1020,
            "the answer",
        );
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_think").unwrap().unwrap();
        let snapshot = adapter.load_snapshot(&loc).unwrap();

        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].role, TranscriptRole::Thinking);
        assert_eq!(snapshot.events[0].content, "let me think about this");
        assert_eq!(snapshot.events[1].role, TranscriptRole::Assistant);
        assert_eq!(snapshot.events[1].content, "the answer");
    }

    #[test]
    fn cursor_in_batch_points_to_last_message() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_cur", "/repo");
        seed_message(&conn, "msg_a", "ses_cur", 1000, "user", "/repo");
        seed_message(&conn, "msg_b", "ses_cur", 2000, "assistant", "/repo");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_cur").unwrap().unwrap();
        let batch = adapter.read_since(&loc, None).unwrap();

        match &batch.cursor {
            Some(TranscriptCursor::SqliteRow {
                id, timestamp_ms, ..
            }) => {
                assert_eq!(id, "msg_b");
                assert_eq!(*timestamp_ms, 2000);
            }
            other => panic!("expected SqliteRow cursor, got {other:?}"),
        }
    }

    #[test]
    fn entity_id_uses_provider_event_format() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_eid", "/repo");
        seed_message(&conn, "msg_e1", "ses_eid", 1000, "user", "/repo");
        seed_text_part(&conn, "prt_e1", "msg_e1", "ses_eid", 1010, "hi");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let loc = adapter.locate("ses_eid").unwrap().unwrap();
        let snapshot = adapter.load_snapshot(&loc).unwrap();

        let raw = &snapshot.events[0].raw;
        assert_eq!(raw.entity_id.as_deref(), Some("opencode:ses_eid:msg_e1:0"));
        assert!(raw.byte_offset.is_none());
        assert!(raw.provider_event_id.is_some());
    }

    #[test]
    fn readonly_connection_does_not_modify_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_ro", "/repo");
        drop(conn);

        let size_before = std::fs::metadata(&db_path).unwrap().len();
        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Glm);
        let _ = adapter.locate("ses_ro").unwrap();

        let size_after = std::fs::metadata(&db_path).unwrap().len();
        assert_eq!(size_before, size_after);
    }

    #[test]
    fn deepseek_provider_shares_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = create_fixture_db(&db_path);
        seed_session(&conn, "ses_ds", "/repo");
        drop(conn);

        let adapter = OpencodeTranscriptAdapter::new(dir.path(), Provider::Deepseek);
        let loc = adapter.locate("ses_ds").unwrap().unwrap();
        assert_eq!(loc.provider, Provider::Deepseek);
    }
}
