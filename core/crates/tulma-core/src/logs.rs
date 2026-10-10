use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};

use crate::redact::truncate;

pub const ARGS_LIMIT: usize = 4096;
pub const RESULT_LIMIT: usize = 2048;
const BATCH: usize = 256;
const DAY_MS: i64 = 86_400_000;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub workspace_id: Option<String>,
    pub server_id: Option<String>,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRow {
    pub id: i64,
    pub ts: i64,
    pub workspace_id: Option<String>,
    pub server_id: String,
    pub server_name: String,
    pub tool: String,
    pub exposed_tool: String,
    pub duration_ms: i64,
    pub ok: bool,
    pub error: Option<String>,
    pub args_json: String,
    pub result_preview: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StderrRow {
    pub id: i64,
    pub ts: i64,
    pub server_id: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogRecord {
    Event(EventRow),
    ToolCall(ToolCallRow),
    Stderr(StderrRow),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cursor {
    pub ts: i64,
    pub id: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogQuery {
    pub server_id: Option<String>,
    pub kind: Option<String>,
    /// `ok` | `error`, tool calls only.
    pub status: Option<String>,
    pub text: Option<String>,
    pub limit: Option<u32>,
    pub before: Option<Cursor>,
}

enum Command {
    Write(LogRecord),
    Flush(mpsc::Sender<()>),
    Retention(u32),
}

pub struct Logs {
    tx: mpsc::Sender<Command>,
    read: Mutex<Connection>,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS events(
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, kind TEXT NOT NULL, workspace_id TEXT,
  server_id TEXT, level TEXT NOT NULL, message TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
CREATE INDEX IF NOT EXISTS events_server ON events(server_id, ts);
CREATE TABLE IF NOT EXISTS tool_calls(
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, workspace_id TEXT, server_id TEXT NOT NULL,
  server_name TEXT NOT NULL, tool TEXT NOT NULL, exposed_tool TEXT NOT NULL,
  duration_ms INTEGER NOT NULL, ok INTEGER NOT NULL, error TEXT,
  args_json TEXT NOT NULL, result_preview TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS tool_calls_ts ON tool_calls(ts);
CREATE INDEX IF NOT EXISTS tool_calls_server ON tool_calls(server_id, ts);
CREATE TABLE IF NOT EXISTS stderr(
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, server_id TEXT NOT NULL, line TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS stderr_server ON stderr(server_id, ts);
";

impl Logs {
    pub fn open(path: &Path, retention_days: u32) -> Result<Self> {
        let write = Connection::open(path)?;
        write.execute_batch(SCHEMA)?;
        let read = Connection::open(path)?;
        read.busy_timeout(Duration::from_secs(2))?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("tulma-logs".into())
            .spawn(move || writer(write, rx, retention_days))?;
        Ok(Self { tx, read: Mutex::new(read) })
    }

    pub fn record(&self, record: LogRecord) {
        let _ = self.tx.send(Command::Write(record));
    }

    /// Blocks until everything queued so far is committed.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Command::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }

    pub fn set_retention(&self, days: u32) {
        let _ = self.tx.send(Command::Retention(days));
    }

    pub fn events(&self, q: &LogQuery) -> Result<Vec<EventRow>> {
        let mut filter = Filter::new(q);
        if let Some(server) = &q.server_id {
            filter.push("server_id = ?", server.clone());
        }
        if let Some(kind) = &q.kind {
            filter.push("kind = ?", kind.clone());
        }
        if let Some(text) = &q.text {
            filter.push("instr(lower(message), lower(?)) > 0", text.clone());
        }
        let sql = format!(
            "SELECT id, ts, kind, workspace_id, server_id, level, message FROM events {} ORDER BY ts DESC, id DESC LIMIT {}",
            filter.where_clause(),
            filter.limit
        );
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(filter.args), |r| {
            Ok(EventRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                workspace_id: r.get(3)?,
                server_id: r.get(4)?,
                level: r.get(5)?,
                message: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn tool_calls(&self, q: &LogQuery) -> Result<Vec<ToolCallRow>> {
        let mut filter = Filter::new(q);
        if let Some(server) = &q.server_id {
            filter.push("server_id = ?", server.clone());
        }
        match q.status.as_deref() {
            Some("ok") => filter.clauses.push("ok = 1".into()),
            Some("error") => filter.clauses.push("ok = 0".into()),
            _ => {}
        }
        if let Some(text) = &q.text {
            filter.push(
                "instr(lower(exposed_tool || ' ' || server_name || ' ' || coalesce(error, '') || ' ' || args_json), lower(?)) > 0",
                text.clone(),
            );
        }
        let sql = format!(
            "SELECT id, ts, workspace_id, server_id, server_name, tool, exposed_tool, duration_ms, ok, error, args_json, result_preview \
             FROM tool_calls {} ORDER BY ts DESC, id DESC LIMIT {}",
            filter.where_clause(),
            filter.limit
        );
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(filter.args), |r| {
            Ok(ToolCallRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                workspace_id: r.get(2)?,
                server_id: r.get(3)?,
                server_name: r.get(4)?,
                tool: r.get(5)?,
                exposed_tool: r.get(6)?,
                duration_ms: r.get(7)?,
                ok: r.get(8)?,
                error: r.get(9)?,
                args_json: r.get(10)?,
                result_preview: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn stderr(&self, server_id: &str, limit: u32) -> Result<Vec<StderrRow>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, ts, server_id, line FROM stderr WHERE server_id = ?1 ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let mut rows: Vec<StderrRow> = stmt
            .query_map(rusqlite::params![server_id, limit], |r| {
                Ok(StderrRow { id: r.get(0)?, ts: r.get(1)?, server_id: r.get(2)?, line: r.get(3)? })
            })?
            .collect::<Result<_, _>>()?;
        rows.reverse();
        Ok(rows)
    }
}

struct Filter {
    clauses: Vec<String>,
    args: Vec<SqlValue>,
    limit: u32,
}

impl Filter {
    fn new(q: &LogQuery) -> Self {
        let mut f = Filter { clauses: Vec::new(), args: Vec::new(), limit: q.limit.unwrap_or(200).clamp(1, 5000) };
        if let Some(c) = &q.before {
            f.clauses.push("(ts < ? OR (ts = ? AND id < ?))".into());
            f.args.extend([SqlValue::Integer(c.ts), SqlValue::Integer(c.ts), SqlValue::Integer(c.id)]);
        }
        f
    }

    fn push(&mut self, clause: &str, arg: String) {
        self.clauses.push(clause.into());
        self.args.push(SqlValue::Text(arg));
    }

    fn where_clause(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.clauses.join(" AND "))
        }
    }
}

fn writer(mut conn: Connection, rx: mpsc::Receiver<Command>, mut retention_days: u32) {
    let mut last_sweep = 0i64;
    loop {
        if now_ms() - last_sweep > DAY_MS {
            last_sweep = now_ms();
            if let Err(err) = sweep(&conn, last_sweep, retention_days) {
                tracing::warn!("log retention sweep failed: {err:#}");
            }
        }
        let first = match rx.recv_timeout(Duration::from_secs(3600)) {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(cmd) => batch.push(cmd),
                Err(_) => break,
            }
        }
        let mut acks = Vec::new();
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            for cmd in batch {
                match cmd {
                    Command::Write(record) => insert(&tx, record)?,
                    Command::Flush(ack) => acks.push(ack),
                    Command::Retention(days) => {
                        retention_days = days;
                        last_sweep = 0;
                    }
                }
            }
            tx.commit()
        })();
        if let Err(err) = result {
            tracing::warn!("log write failed: {err}");
        }
        for ack in acks {
            let _ = ack.send(());
        }
    }
}

fn insert(conn: &Connection, record: LogRecord) -> rusqlite::Result<()> {
    match record {
        LogRecord::Event(e) => {
            conn.prepare_cached(
                "INSERT INTO events(ts, kind, workspace_id, server_id, level, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(rusqlite::params![e.ts, e.kind, e.workspace_id, e.server_id, e.level, e.message])?;
        }
        LogRecord::ToolCall(c) => {
            conn.prepare_cached(
                "INSERT INTO tool_calls(ts, workspace_id, server_id, server_name, tool, exposed_tool, duration_ms, ok, error, args_json, result_preview) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(rusqlite::params![
                c.ts,
                c.workspace_id,
                c.server_id,
                c.server_name,
                c.tool,
                c.exposed_tool,
                c.duration_ms,
                c.ok,
                c.error,
                truncate(&c.args_json, ARGS_LIMIT),
                truncate(&c.result_preview, RESULT_LIMIT),
            ])?;
        }
        LogRecord::Stderr(l) => {
            conn.prepare_cached("INSERT INTO stderr(ts, server_id, line) VALUES (?1, ?2, ?3)")?
                .execute(rusqlite::params![l.ts, l.server_id, l.line])?;
        }
    }
    Ok(())
}

pub fn sweep(conn: &Connection, now: i64, retention_days: u32) -> Result<usize> {
    let cutoff = now - i64::from(retention_days) * DAY_MS;
    let mut removed = 0;
    for table in ["events", "tool_calls", "stderr"] {
        removed += conn.execute(&format!("DELETE FROM {table} WHERE ts < ?1"), [cutoff])?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Logs) {
        let dir = tempfile::tempdir().unwrap();
        let logs = Logs::open(&dir.path().join("logs.sqlite"), 14).unwrap();
        (dir, logs)
    }

    fn event(ts: i64, server: &str, kind: &str, message: &str) -> LogRecord {
        LogRecord::Event(EventRow {
            id: 0,
            ts,
            kind: kind.into(),
            workspace_id: Some("w".into()),
            server_id: Some(server.into()),
            level: "info".into(),
            message: message.into(),
        })
    }

    fn call(ts: i64, ok: bool, args: String) -> LogRecord {
        LogRecord::ToolCall(ToolCallRow {
            id: 0,
            ts,
            workspace_id: Some("w".into()),
            server_id: "s1".into(),
            server_name: "echo".into(),
            tool: "ping".into(),
            exposed_tool: "echo__ping".into(),
            duration_ms: 5,
            ok,
            error: (!ok).then(|| "boom".into()),
            args_json: args,
            result_preview: "r".repeat(5000),
        })
    }

    #[test]
    fn events_filter_by_server_kind_and_text_newest_first() {
        let (_d, logs) = open();
        logs.record(event(1, "a", "server.status", "started"));
        logs.record(event(2, "b", "server.status", "started"));
        logs.record(event(3, "a", "server.error", "crashed hard"));
        logs.flush();
        let all = logs.events(&LogQuery::default()).unwrap();
        assert_eq!(all.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![3, 2, 1]);
        let a = logs.events(&LogQuery { server_id: Some("a".into()), ..Default::default() }).unwrap();
        assert_eq!(a.len(), 2);
        let crashed = logs.events(&LogQuery { text: Some("crash".into()), ..Default::default() }).unwrap();
        assert_eq!(crashed.len(), 1);
        let kind = logs.events(&LogQuery { kind: Some("server.status".into()), ..Default::default() }).unwrap();
        assert_eq!(kind.len(), 2);
    }

    #[test]
    fn cursor_pages_through_rows_with_equal_timestamps() {
        let (_d, logs) = open();
        for i in 0..5 {
            logs.record(event(10, "a", "k", &format!("m{i}")));
        }
        logs.flush();
        let first = logs.events(&LogQuery { limit: Some(2), ..Default::default() }).unwrap();
        let last = first.last().unwrap();
        let next = logs
            .events(&LogQuery { limit: Some(10), before: Some(Cursor { ts: last.ts, id: last.id }), ..Default::default() })
            .unwrap();
        assert_eq!(first.len() + next.len(), 5);
        assert!(next.iter().all(|e| e.id < last.id));
    }

    #[test]
    fn tool_calls_truncate_payloads_and_filter_status() {
        let (_d, logs) = open();
        logs.record(call(1, true, "a".repeat(10_000)));
        logs.record(call(2, false, "{}".into()));
        logs.flush();
        let rows = logs.tool_calls(&LogQuery::default()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[1].args_json.len() <= ARGS_LIMIT + 3);
        assert!(rows[0].result_preview.len() <= RESULT_LIMIT + 3);
        let errors = logs.tool_calls(&LogQuery { status: Some("error".into()), ..Default::default() }).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn stderr_returns_latest_lines_in_chronological_order() {
        let (_d, logs) = open();
        for i in 0..5 {
            logs.record(LogRecord::Stderr(StderrRow { id: 0, ts: i, server_id: "s1".into(), line: format!("l{i}") }));
        }
        logs.flush();
        let lines: Vec<_> = logs.stderr("s1", 3).unwrap().into_iter().map(|r| r.line).collect();
        assert_eq!(lines, vec!["l2", "l3", "l4"]);
    }

    #[test]
    fn sweep_removes_rows_older_than_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.sqlite");
        let logs = Logs::open(&path, 14).unwrap();
        let now = now_ms();
        logs.record(event(now - 20 * DAY_MS, "a", "k", "old"));
        logs.record(event(now - DAY_MS, "a", "k", "fresh"));
        logs.record(LogRecord::Stderr(StderrRow { id: 0, ts: now - 30 * DAY_MS, server_id: "a".into(), line: "x".into() }));
        logs.flush();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(sweep(&conn, now, 14).unwrap(), 2);
        let left = logs.events(&LogQuery::default()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, "fresh");
    }
}
