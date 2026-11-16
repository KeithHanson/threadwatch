use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::{fs, path::Path, time::Duration};

pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
pub const STATUSES: &[&str] = &[
    "working",
    "queued",
    "waiting",
    "completed",
    "failed",
    "unknown",
    "stopped",
];

pub fn thread_id(kind: &str, bot: &str, thread: &str) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&[kind, bot, thread]).expect("string array"))
}

/// Only canonical Discord user-avatar URLs may cross the collector boundary.
pub fn valid_avatar_url(url: &str) -> bool {
    let Some(path) = url.strip_prefix("https://cdn.discordapp.com/avatars/") else {
        return false;
    };
    let Some((user, image)) = path.split_once('/') else {
        return false;
    };
    let image = image.strip_suffix("?size=96").unwrap_or(image);
    let Some(hash) = image.strip_suffix(".png") else {
        return false;
    };
    let hash = hash.strip_prefix("a_").unwrap_or(hash);
    (1..=20).contains(&user.len())
        && user.bytes().all(|byte| byte.is_ascii_digit())
        && hash.len() == 32
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn short_title(thread: &Value) -> String {
    thread["shortTitle"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| thread["title"].as_str())
        .unwrap_or("Untitled thread")
        .trim()
        .chars()
        .take(24)
        .collect()
}

fn event_preview(event: Value) -> Value {
    let text: String = event["text"]
        .as_str()
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(240)
        .collect();
    json!({"id": event["id"], "timestamp": event["timestamp"], "kind": event["kind"],
        "role": event["role"], "name": event["name"], "text": text, "isError": event["isError"] == true})
}

/// Uses the original Node schema verbatim; receipts and import checkpoints survive migration.
pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(directory)?;
        let path = directory.join("threadwatch.sqlite");
        let db = Connection::open(&path)?;
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS threads (id TEXT PRIMARY KEY, source TEXT NOT NULL, payload TEXT NOT NULL, todos TEXT NOT NULL DEFAULT '[]', revision INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS events (thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, seq INTEGER NOT NULL, event_id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(thread_id,event_id), UNIQUE(thread_id,seq));
            CREATE TABLE IF NOT EXISTS imports (thread_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS deliveries (bot_id TEXT NOT NULL, delivery_id TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY(bot_id,delivery_id));
            CREATE TABLE IF NOT EXISTS heartbeats (bot_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
            INSERT OR IGNORE INTO meta VALUES ('revision',0);")?;
        Ok(Self { db })
    }

    pub fn revision(&self) -> Result<String> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key='revision'", [], |row| {
                row.get::<_, i64>(0)
            })?
            .to_string())
    }

    pub fn ingest(
        &mut self,
        mut thread: Value,
        events: &[Value],
        todos: Option<&Value>,
        delivery: Option<&Value>,
    ) -> Result<String> {
        let id = thread["id"]
            .as_str()
            .context("Missing thread id")?
            .to_owned();
        let bot = thread["botId"]
            .as_str()
            .context("Missing bot id")?
            .to_owned();
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(delivery) = delivery {
            let receipt = delivery["id"].as_str().context("Missing delivery id")?;
            if let Some(previous) = tx
                .query_row(
                    "SELECT thread_id FROM deliveries WHERE bot_id=? AND delivery_id=?",
                    params![bot, receipt],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                tx.commit()?;
                return Ok(previous);
            }
            if delivery["reset"] == true {
                tx.execute("DELETE FROM events WHERE thread_id=?", [&id])?;
                tx.execute("UPDATE threads SET todos='[]' WHERE id=?", [&id])?;
                bump(&tx, &id)?;
                thread["historyVersion"] = Value::String(receipt.to_owned());
            }
        }
        let current = tx
            .query_row(
                "SELECT payload,todos FROM threads WHERE id=?",
                [&id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if thread.get("historyVersion").is_none() {
            if let Some((payload, _)) = &current {
                if let Some(version) = serde_json::from_str::<Value>(payload)?.get("historyVersion")
                {
                    thread["historyVersion"] = version.clone();
                }
            }
        }
        let payload = serde_json::to_string(&thread)?;
        let todo_text = match todos {
            Some(value) => serde_json::to_string(value)?,
            None => current
                .as_ref()
                .map(|(_, todos)| todos.clone())
                .unwrap_or_else(|| "[]".into()),
        };
        // Compare JSON semantically once across the Node-to-Rust key-order transition.
        let unchanged = if let Some((old, old_todos)) = &current {
            (old == &payload || serde_json::from_str::<Value>(old)? == thread)
                && (old_todos == &todo_text
                    || serde_json::from_str::<Value>(old_todos)?
                        == serde_json::from_str::<Value>(&todo_text)?)
        } else {
            false
        };
        if !unchanged {
            tx.execute("INSERT INTO threads(id,source,payload,todos) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET source=excluded.source,payload=excluded.payload,todos=excluded.todos", params![id, thread["source"].as_str().context("Missing source")?, payload, todo_text])?;
            bump(&tx, &id)?;
        }
        let mut seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE thread_id=?",
            [&id],
            |row| row.get(0),
        )?;
        let mut changed = false;
        {
            let mut lookup =
                tx.prepare_cached("SELECT payload FROM events WHERE thread_id=? AND event_id=?")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO events(thread_id,seq,event_id,payload) VALUES(?,?,?,?)",
            )?;
            let mut update =
                tx.prepare_cached("UPDATE events SET payload=? WHERE thread_id=? AND event_id=?")?;
            for event in events {
                let event_id = event["id"].as_str().context("Missing event id")?;
                let previous = lookup
                    .query_row(params![id, event_id], |row| row.get::<_, String>(0))
                    .optional()?;
                let payload = serde_json::to_string(event)?;
                match previous {
                    None => {
                        seq += 1;
                        insert.execute(params![id, seq, event_id, payload])?;
                        changed = true;
                    }
                    Some(old)
                        if old != payload && serde_json::from_str::<Value>(&old)? != *event =>
                    {
                        update.execute(params![payload, id, event_id])?;
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        if changed {
            bump(&tx, &id)?;
        }
        if let Some(delivery) = delivery {
            tx.execute(
                "INSERT INTO deliveries VALUES(?,?,?)",
                params![bot, delivery["id"].as_str(), id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn record_heartbeat(&mut self, heartbeat: &Value, received_at: i64) -> Result<()> {
        let bot = heartbeat["botId"].as_str().context("Missing bot id")?;
        let mut payload = json!({"id": format!("ingest:{bot}"), "name": heartbeat["botName"], "status": heartbeat["status"], "lastSeenAt": received_at, "pendingBatches": heartbeat["pendingBatches"]});
        if let Some(error) = heartbeat.get("error") {
            payload["error"] = error.clone();
        }
        if let Some(avatar) = heartbeat["avatarUrl"].as_str().filter(|url| valid_avatar_url(url)) {
            payload["avatarUrl"] = json!(avatar);
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO heartbeats VALUES(?,?) ON CONFLICT(bot_id) DO UPDATE SET payload=excluded.payload", params![bot, serde_json::to_string(&payload)?])?;
        tx.execute("UPDATE meta SET value=value+1 WHERE key='revision'", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn collector_sources(&self, now: i64) -> Result<Vec<Value>> {
        let mut statement = self
            .db
            .prepare_cached("SELECT payload FROM heartbeats ORDER BY bot_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut sources = Vec::new();
        for row in rows {
            let mut source: Value = serde_json::from_str(&row?)?;
            if !source["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("ingest:"))
            {
                continue;
            }
            if source["lastSeenAt"]
                .as_i64()
                .is_some_and(|seen| now.saturating_sub(seen) >= 45_000)
            {
                source["status"] = json!("offline");
            }
            sources.push(source);
        }
        Ok(sources)
    }

    pub fn list(&self) -> Result<Vec<Value>> {
        // Counts use the covering thread/sequence index; only the final record's
        // payload is loaded, never a thread's transcript or detail page.
        let mut statement = self.db.prepare_cached(
            "SELECT t.payload, json_array_length(t.todos),
                (SELECT COUNT(*) FROM json_each(t.todos) WHERE json_extract(value,'$.status')='completed'),
                (SELECT COUNT(*) FROM events WHERE thread_id=t.id),
                (SELECT payload FROM events WHERE thread_id=t.id ORDER BY seq DESC LIMIT 1)
             FROM threads t WHERE t.source LIKE 'ingest:%'
             ORDER BY json_extract(t.payload,'$.updatedAt') DESC, t.id",
        )?;
        let mut rows = statement.query([])?;
        let mut threads = Vec::new();
        while let Some(row) = rows.next()? {
            let mut thread: Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
            thread["shortTitle"] = json!(short_title(&thread));
            thread["todoCount"] = json!(row.get::<_, i64>(1)?);
            thread["todoCompleted"] = json!(row.get::<_, i64>(2)?);
            thread["eventCount"] = json!(row.get::<_, i64>(3)?);
            thread["latestEvent"] = match row.get::<_, Option<String>>(4)? {
                Some(payload) => event_preview(serde_json::from_str(&payload)?),
                None => Value::Null,
            };
            threads.push(thread);
        }
        Ok(threads)
    }

    pub fn detail(&self, id: &str, before: Option<&str>, limit: usize) -> Result<Option<Value>> {
        if !(1..=500).contains(&limit) {
            bail!("limit must be between 1 and 500");
        }
        let boundary = decode_cursor(id, before)?;
        let snapshot = self.db.unchecked_transaction()?;
        let current = snapshot
            .query_row(
                "SELECT payload,todos,revision,(SELECT COUNT(*) FROM events WHERE thread_id=threads.id) FROM threads WHERE id=? AND source LIKE 'ingest:%'",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((payload, todos, revision, event_count)) = current else {
            return Ok(None);
        };
        let mut statement = snapshot.prepare_cached(
            "SELECT seq,payload FROM events WHERE thread_id=? AND seq<? ORDER BY seq DESC LIMIT ?",
        )?;
        let mut rows = statement.query(params![id, boundary, (limit + 1) as i64])?;
        let mut events = Vec::with_capacity(limit + 1);
        while let Some(row) = rows.next()? {
            events.push((
                row.get::<_, i64>(0)?,
                serde_json::from_str::<Value>(&row.get::<_, String>(1)?)?,
            ));
        }
        let more = events.len() > limit;
        if more {
            events.pop();
        }
        events.reverse();
        let next = if more {
            Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!([id, events[0].0]))?))
        } else {
            None
        };
        Ok(Some(
            json!({"thread": serde_json::from_str::<Value>(&payload)?, "todos": serde_json::from_str::<Value>(&todos)?, "events": events.into_iter().map(|(_, value)| value).collect::<Vec<_>>(), "nextCursor": next, "revision": revision.to_string(), "eventCount": event_count}),
        ))
    }
}

fn bump(db: &Connection, id: &str) -> Result<()> {
    db.execute("UPDATE meta SET value=value+1 WHERE key='revision'", [])?;
    db.execute(
        "UPDATE threads SET revision=(SELECT value FROM meta WHERE key='revision') WHERE id=?",
        [id],
    )?;
    Ok(())
}

pub fn decode_cursor(id: &str, before: Option<&str>) -> Result<i64> {
    let Some(before) = before else {
        return Ok(MAX_SAFE_INTEGER);
    };
    if before.len() > 4096 {
        bail!("Invalid pagination cursor");
    }
    let cursor: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(before)
            .context("Invalid pagination cursor")?,
    )
    .context("Invalid pagination cursor")?;
    let Some(parts) = cursor.as_array() else {
        bail!("Invalid pagination cursor");
    };
    if parts.len() != 2 || parts[0].as_str() != Some(id) {
        bail!("Invalid pagination cursor");
    }
    match parts[1].as_i64() {
        Some(seq) if (1..=MAX_SAFE_INTEGER).contains(&seq) => Ok(seq),
        _ => bail!("Invalid pagination cursor"),
    }
}
