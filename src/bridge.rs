use crate::sanitize::{
    Sanitizer, derive_todos, normalize_event, operational_record, sensitive_tool_call, timestamp,
    valid_todo_status,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Statement, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bridge {
    pub id: String,
    pub name: String,
    pub database_path: PathBuf,
    pub session_dir: PathBuf,
    pub bot_token_file: Option<PathBuf>,
}

pub fn expand_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let home = || {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not configured"))
    };
    let path = if path == Path::new("~") {
        home()?
    } else if let Ok(relative) = path.strip_prefix("~/") {
        home()?.join(relative)
    } else if path.is_absolute() {
        path.to_owned()
    } else {
        env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

pub fn load_bridges(path: &Path) -> Result<Vec<Bridge>> {
    let input = fs::read(expand_path(path)?).context("Cannot read bridge configuration")?;
    let mut bridges: Vec<Bridge> =
        serde_json::from_slice(&input).map_err(|_| anyhow!("Invalid bridge configuration"))?;
    let mut ids = std::collections::HashSet::new();
    for bridge in &mut bridges {
        ensure!(
            !bridge.id.is_empty()
                && bridge.id.len() <= 100
                && bridge
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                && ids.insert(bridge.id.clone())
                && !bridge.name.is_empty(),
            "Invalid or duplicate bridge configuration"
        );
        ensure!(
            !bridge.database_path.as_os_str().is_empty()
                && !bridge.session_dir.as_os_str().is_empty(),
            "Invalid bridge paths"
        );
        bridge.database_path = expand_path(&bridge.database_path)?;
        bridge.session_dir = expand_path(&bridge.session_dir)?;
        if let Some(path) = &mut bridge.bot_token_file {
            ensure!(!path.as_os_str().is_empty(), "Invalid bridge botTokenFile");
            *path = expand_path(&*path)?;
        }
    }
    Ok(bridges)
}

pub fn protected_token(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| anyhow!("Cannot open protected token file"))?;
    let metadata = file.metadata().context("Cannot inspect token file")?;
    ensure!(
        metadata.is_file()
            && metadata.mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "Token file must be owner-only and owned by this user"
    );
    ensure!(metadata.len() <= 16 * 1024, "Token file is too large");
    let mut token = String::new();
    file.read_to_string(&mut token)
        .map_err(|_| anyhow!("Cannot read token file"))?;
    Ok(token.trim().to_owned())
}

pub fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn private_database(path: &Path) -> Result<Connection> {
    // The directory is private before SQLite creates its WAL and shared-memory files.
    let db = Connection::open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    db.busy_timeout(Duration::from_millis(100))?;
    Ok(db)
}

pub fn thread_id(kind: &str, bot: &str, thread: &str) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&[kind, bot, thread]).unwrap())
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Default)]
pub struct Cancellation {
    stopped: AtomicBool,
    mutex: Mutex<()>,
    condition: Condvar,
    notify: tokio::sync::Notify,
}

impl Cancellation {
    pub fn cancel(&self) {
        let _guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        self.stopped.store(true, Ordering::Release);
        self.condition.notify_all();
        self.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(!self.is_cancelled(), "Collector is stopping");
        Ok(())
    }
    pub fn sleep(&self, duration: Duration) {
        let guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        drop(
            self.condition
                .wait_timeout_while(guard, duration, |_| !self.is_cancelled()),
        );
    }
    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }
}

pub struct Cache {
    pub db: Connection,
    pub sanitizer: Arc<Sanitizer>,
}

impl Cache {
    pub fn open(directory: &Path, sanitizer: Arc<Sanitizer>) -> Result<Self> {
        private_directory(directory)?;
        let db = private_database(&directory.join("threadwatch.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS threads (id TEXT PRIMARY KEY, source TEXT NOT NULL, payload TEXT NOT NULL, todos TEXT NOT NULL DEFAULT '[]', revision INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS events (thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, seq INTEGER NOT NULL, event_id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(thread_id,event_id), UNIQUE(thread_id,seq));
            CREATE TABLE IF NOT EXISTS imports (thread_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS deliveries (bot_id TEXT NOT NULL, delivery_id TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY(bot_id,delivery_id));
            CREATE TABLE IF NOT EXISTS heartbeats (bot_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
            INSERT OR IGNORE INTO meta VALUES ('revision',0);
            CREATE TABLE IF NOT EXISTS collector_resets (thread_id TEXT PRIMARY KEY, generation INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS discord_titles (thread_id TEXT PRIMARY KEY, next_attempt INTEGER NOT NULL);")?;
        Ok(Self { db, sanitizer })
    }

    pub fn transaction<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let transaction = self.db.unchecked_transaction()?;
        let result = operation()?;
        transaction.commit()?;
        Ok(result)
    }

    fn bump(&self, id: &str) -> Result<()> {
        self.db
            .execute("UPDATE meta SET value=value+1 WHERE key='revision'", [])?;
        self.db.execute(
            "UPDATE threads SET revision=(SELECT value FROM meta WHERE key='revision') WHERE id=?",
            [id],
        )?;
        Ok(())
    }

    pub fn thread(&self, id: &str) -> Result<Option<Value>> {
        let raw: Option<String> = self
            .db
            .query_row("SELECT payload FROM threads WHERE id=?", [id], |row| {
                row.get(0)
            })
            .optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .transpose()
    }

    pub fn todos(&self, id: &str) -> Result<Value> {
        let raw: Option<String> = self
            .db
            .query_row("SELECT todos FROM threads WHERE id=?", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(match raw {
            Some(raw) => serde_json::from_str(&raw)?,
            None => json!([]),
        })
    }

    pub fn upsert(&self, thread: &Value, todos: Option<&Value>) -> Result<()> {
        let id = thread["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid cached thread"))?;
        let current: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT payload,todos FROM threads WHERE id=?",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let mut stored = thread.clone();
        if stored.get("historyVersion").is_none() {
            if let Some((payload, _)) = &current {
                let old: Value = serde_json::from_str(payload)?;
                if let Some(version) = old.get("historyVersion") {
                    stored["historyVersion"] = version.clone();
                }
            }
        }
        let payload = stored.to_string();
        let todos = todos.map(Value::to_string).unwrap_or_else(|| {
            current
                .as_ref()
                .map_or_else(|| "[]".into(), |(_, todos)| todos.clone())
        });
        if current
            .as_ref()
            .is_some_and(|(old, old_todos)| *old == payload && *old_todos == todos)
        {
            return Ok(());
        }
        self.db.execute("INSERT INTO threads(id,source,payload,todos) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET source=excluded.source,payload=excluded.payload,todos=excluded.todos", params![id, thread["source"].as_str().unwrap_or(""), payload, todos])?;
        self.bump(id)
    }

    pub fn reset(&self, id: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM events WHERE thread_id=?", [id])?;
        self.db
            .execute("UPDATE threads SET todos='[]' WHERE id=?", [id])?;
        self.db.execute("INSERT INTO collector_resets VALUES(?,1) ON CONFLICT(thread_id) DO UPDATE SET generation=generation+1", [id])?;
        self.bump(id)
    }

    pub fn append(&self, id: &str, events: &[Value]) -> Result<()> {
        let mut writer = self.writer(id)?;
        for event in events {
            writer.append(event)?;
        }
        writer.finish()
    }

    fn writer<'a>(&'a self, id: &'a str) -> Result<EventWriter<'a>> {
        Ok(EventWriter {
            cache: self,
            id,
            seq: self.db.query_row(
                "SELECT COALESCE(MAX(seq),0) FROM events WHERE thread_id=?",
                [id],
                |row| row.get(0),
            )?,
            changed: false,
            lookup: self
                .db
                .prepare("SELECT payload FROM events WHERE thread_id=? AND event_id=?")?,
            insert: self
                .db
                .prepare("INSERT INTO events(thread_id,seq,event_id,payload) VALUES(?,?,?,?)")?,
            update: self
                .db
                .prepare("UPDATE events SET payload=? WHERE thread_id=? AND event_id=?")?,
        })
    }
}

struct EventWriter<'a> {
    cache: &'a Cache,
    id: &'a str,
    seq: i64,
    changed: bool,
    lookup: Statement<'a>,
    insert: Statement<'a>,
    update: Statement<'a>,
}

impl EventWriter<'_> {
    fn append(&mut self, event: &Value) -> Result<()> {
        let Some(event) = normalize_event(event, &self.cache.sanitizer) else {
            return Ok(());
        };
        let id = event["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid event identity"))?;
        let payload = event.to_string();
        let old: Option<String> = self
            .lookup
            .query_row(params![self.id, id], |row| row.get(0))
            .optional()?;
        match old {
            Some(old) if old == payload => {}
            Some(_) => {
                self.update.execute(params![payload, self.id, id])?;
                self.changed = true;
            }
            None => {
                self.seq += 1;
                self.insert
                    .execute(params![self.id, self.seq, id, payload])?;
                self.changed = true;
            }
        }
        Ok(())
    }
    fn finish(self) -> Result<()> {
        if self.changed {
            self.cache.bump(self.id)?;
        }
        Ok(())
    }
}

pub struct BridgeReader {
    pub cache: Cache,
    bridge: Bridge,
    cancellation: Arc<Cancellation>,
    titles: DiscordTitles,
}

impl BridgeReader {
    pub fn new(cache: Cache, bridge: Bridge, cancellation: Arc<Cancellation>) -> Result<Self> {
        Ok(Self {
            cache,
            bridge,
            cancellation,
            titles: DiscordTitles::new()?,
        })
    }

    pub fn refresh(&mut self) -> Value {
        match self.refresh_inner() {
            Ok(degraded) => {
                if degraded {
                    json!({"status":"degraded", "error":"One or more saved transcripts are unavailable or malformed."})
                } else {
                    json!({"status":"ready"})
                }
            }
            Err(_) => {
                json!({"status":"unavailable", "error":"Bridge database is unavailable or incompatible."})
            }
        }
    }

    fn refresh_inner(&mut self) -> Result<bool> {
        self.cancellation.check()?;
        let db = Connection::open_with_flags(
            &self.bridge.database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        db.busy_timeout(Duration::from_millis(100))?;
        db.execute_batch("PRAGMA query_only=ON")?;
        let mut sessions = db.prepare("SELECT thread_id,guild_id,omp_session_path,status,updated_at_ms FROM sessions ORDER BY thread_id")?;
        let mut turns = db.prepare("SELECT status FROM turns WHERE thread_id=? ORDER BY created_at_ms DESC,id DESC LIMIT 1")?;
        let mut rows = sessions.query([])?;
        let mut degraded = false;
        while let Some(row) = rows.next()? {
            self.cancellation.check()?;
            let source_id: String = row.get(0)?;
            let guild: Option<String> = row.get(1)?;
            let path: Option<String> = row.get(2)?;
            let session_status: String = row.get(3)?;
            let updated: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
            let turn: Option<String> =
                turns.query_row([&source_id], |row| row.get(0)).optional()?;
            let status = if session_status == "stopped" {
                "stopped"
            } else {
                match turn.as_deref() {
                    Some("queued") => "queued",
                    Some("running") => "working",
                    Some("done") => "completed",
                    Some("failed") => "failed",
                    Some("aborted") => "stopped",
                    Some("uncertain") => "unknown",
                    _ if session_status == "active" => "waiting",
                    _ => "unknown",
                }
            };
            let id = thread_id("bridge", &self.bridge.id, &source_id);
            let old = self.cache.thread(&id)?.unwrap_or(Value::Null);
            let old_title = old["title"]
                .as_str()
                .filter(|title| !title.trim().is_empty());
            let mut thread = json!({"id":id,"botId":self.bridge.id,"botName":self.cache.sanitizer.text(&self.bridge.name),
                "title":old_title.map(str::to_owned).unwrap_or_else(||format!("Thread {source_id}")),
                "titleSource":if old_title.is_some() { old["titleSource"].as_str().unwrap_or("session") } else { "fallback" },
                "guildId":guild, "channelId":source_id, "status":status,
                "updatedAt":(updated.max(0) as u64).max(old["updatedAt"].as_u64().unwrap_or(0)), "source":format!("bridge:{}",self.bridge.id)});
            self.cache
                .transaction(|| self.cache.upsert(&thread, None))?;
            if let Some(path) = path.filter(|path| !path.is_empty()) {
                if self.import_session(&mut thread, &path).is_err() {
                    degraded = true;
                }
            }
            self.cancellation.check()?;
            // Channel metadata only: no Discord messages, OAuth, or bridge writes.
            self.titles
                .refresh(&self.cache, &self.bridge, &thread, &self.cancellation)?;
        }
        Ok(degraded)
    }

    pub fn import_session(&self, thread: &mut Value, configured: &str) -> Result<()> {
        let root = fs::canonicalize(&self.bridge.session_dir)?;
        let configured = Path::new(configured);
        let path = fs::canonicalize(if configured.is_absolute() || configured.starts_with("~") {
            expand_path(configured)?
        } else {
            root.join(configured)
        })?;
        ensure!(
            path != root && path.starts_with(&root),
            "Session path outside source"
        );
        let mut file = File::open(&path)?;
        let stat = file.metadata()?;
        ensure!(stat.is_file(), "Not a session file");
        let identity = format!("{}:{}", stat.dev(), stat.ino());
        let modified = format!("{}:{}", stat.mtime(), stat.mtime_nsec());
        let changed = format!("{}:{}", stat.ctime(), stat.ctime_nsec());
        let id = thread["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid thread"))?
            .to_owned();
        let raw: Option<String> = self
            .cache
            .db
            .query_row(
                "SELECT payload FROM imports WHERE thread_id=?",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        let previous: Value = raw
            .map(|raw| serde_json::from_str(&raw))
            .transpose()?
            .unwrap_or(Value::Null);
        let compatible = previous["version"] == 3
            && previous["path"].as_str() == path.to_str()
            && previous["identity"] == identity;
        if compatible
            && previous["size"] == stat.len()
            && previous["mtime"] == modified
            && previous["ctime"] == changed
        {
            ensure!(previous["malformed"] != true, "Malformed transcript");
            return Ok(());
        }
        let offset = previous["offset"].as_u64().unwrap_or(0);
        let mut hash = Sha256::new();
        let mut append = compatible
            && stat.len() > previous["size"].as_u64().unwrap_or(0)
            && offset <= stat.len();
        if append {
            let mut remaining = offset;
            let mut buffer = [0u8; 64 * 1024];
            while remaining > 0 {
                self.cancellation.check()?;
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = file.read(&mut buffer[..limit])?;
                ensure!(count != 0, "Transcript changed during import");
                hash.update(&buffer[..count]);
                remaining -= count as u64;
            }
            // Verifying the already parsed prefix detects rewrite-and-grow edits,
            // not just truncations; only JSON parsing is incremental.
            append = previous["prefixHash"].as_str()
                == Some(format!("{:x}", hash.clone().finalize()).as_str());
        }
        let mut state = if append {
            previous
        } else {
            hash = Sha256::new();
            json!({"version":3,"offset":0,"line":0,"pending":{},"malformed":false})
        };
        let mut todos = if append {
            self.cache.todos(&id)?
        } else {
            json!([])
        };
        file.seek(SeekFrom::Start(state["offset"].as_u64().unwrap_or(0)))?;
        let available = stat.len() - state["offset"].as_u64().unwrap_or(0);
        let mut reader = BufReader::with_capacity(64 * 1024, file.take(available));
        self.cache.transaction(|| {
            if !append { self.cache.reset(&id)?; }
            let mut writer = self.cache.writer(&id)?;
            let mut bytes = Vec::new();
            loop {
                self.cancellation.check()?;
                bytes.clear();
                // Check cancellation between buffers even for very large tool results.
                loop {
                    self.cancellation.check()?;
                    let buffer = reader.fill_buf()?;
                    if buffer.is_empty() { break; }
                    let end = buffer.iter().position(|byte|*byte == b'\n').map(|index|index+1);
                    let count = end.unwrap_or(buffer.len());
                    bytes.extend_from_slice(&buffer[..count]);
                    reader.consume(count);
                    if end.is_some() { break; }
                }
                if bytes.last() != Some(&b'\n') { break; }
                hash.update(&bytes);
                let line = state["line"].as_u64().unwrap_or(0)+1;
                state["line"] = json!(line);
                state["offset"] = json!(state["offset"].as_u64().unwrap_or(0)+bytes.len() as u64);
                if bytes.iter().all(u8::is_ascii_whitespace) { continue; }
                let record: Value = match serde_json::from_slice::<Value>(&bytes) {
                    Ok(record) if record.is_object() => record,
                    _ => {
                        state["malformed"] = json!(true);
                        writer.append(&json!({"id":format!("{line}:malformed"),"timestamp":0,"kind":"import_error","text":format!("Malformed saved record at line {line}; its contents were not imported."),"isError":true}))?;
                        continue;
                    }
                };
                let saved = record.get("timestamp").or_else(||record["message"].get("timestamp")).unwrap_or(&Value::Null);
                let time = timestamp(saved);
                if time > thread["updatedAt"].as_u64().unwrap_or(0) { thread["updatedAt"] = json!(time); }
                if record["type"] == "title" && thread["titleSource"] != "discord" {
                    if let Some(title) = record["title"].as_str() {
                        let title = self.cache.sanitizer.text(title);
                        if !title.trim().is_empty() { thread["title"] = json!(title.trim()); thread["titleSource"] = json!("session"); }
                    }
                }
                if thread["titleSource"] == "fallback" && record["type"] == "message" && record["message"]["role"] == "user" {
                    let text = content_text(&record["message"]["content"], " ");
                    let title = self.cache.sanitizer.text(&text).split_whitespace().collect::<Vec<_>>().join(" ");
                    if !title.is_empty() {
                        thread["title"] = json!(if title.chars().count() > 120 { title.chars().take(117).collect::<String>()+"..." } else { title });
                        thread["titleSource"] = json!("user_message");
                    }
                }
                let message = &record["message"];
                if record["type"] == "message" && message["role"] == "assistant" {
                    for part in message["content"].as_array().into_iter().flatten() {
                        let sensitive = sensitive_tool_call(part);
                        if part["type"] == "toolCall" && (part["name"] == "todo" || sensitive) {
                            if let Some(call_id) = part["id"].as_str() {
                                let op = if part["name"] == "todo" { part["arguments"]["op"].as_str().map(|op| self.cache.sanitizer.text(op)) } else { None };
                                state["pending"][call_id] = json!({"op":op,"withheld":sensitive});
                            }
                        }
                    }
                }
                if record["type"] == "custom" && record["customType"] == "tool_execution_start" && sensitive_tool_call(&record["data"]) {
                    if let Some(call_id) = record["data"]["toolCallId"].as_str() { state["pending"][call_id] = json!({"withheld":true}); }
                }
                let mut withheld = false;
                if record["type"] == "message" && message["role"] == "toolResult" {
                    if let Some(call_id) = message["toolCallId"].as_str() {
                        if let Some(pending) = state["pending"].as_object_mut().and_then(|pending|pending.remove(call_id)) {
                            withheld = pending["withheld"] == true;
                            if !withheld && message["isError"] != true && message["toolName"] == "todo" {
                                let text = self.cache.sanitizer.text(&content_text(&message["content"],"\n"));
                                todos = derive_todos(&pending,&text,&todos);
                            }
                        }
                    }
                }
                if record["type"] == "custom" && record["customType"] == "user_todo_edit" && record["data"]["phases"].is_array() {
                    todos = Value::Array(record["data"]["phases"].as_array().unwrap().iter().flat_map(|phase|phase["tasks"].as_array().into_iter().flatten()).filter_map(|task| {
                        let content = task["content"].as_str()?;
                        let status = task["status"].as_str().filter(|status|valid_todo_status(status))?;
                        Some(json!({"text":self.cache.sanitizer.text(content),"status":status}))
                    }).collect());
                    writer.append(&json!({"id":format!("{line}:todos"),"timestamp":record["timestamp"],"kind":"todo_edit","text":todos.to_string()}))?;
                }
                for event in operational_record(&record,&self.cache.sanitizer,line,withheld) { writer.append(&event)?; }
            }
            self.cancellation.check()?;
            let final_stat = fs::metadata(&path)?;
            ensure!(final_stat.dev() == stat.dev() && final_stat.ino() == stat.ino() && final_stat.len() >= stat.len(),"Transcript replaced during import");
            // A same-size concurrent rewrite cannot be committed as a coherent snapshot.
            ensure!(final_stat.len() != stat.len() || (final_stat.mtime() == stat.mtime() && final_stat.mtime_nsec() == stat.mtime_nsec() && final_stat.ctime() == stat.ctime() && final_stat.ctime_nsec() == stat.ctime_nsec()),"Transcript changed during import");
            writer.finish()?;
            self.cache.upsert(thread,Some(&todos))?;
            state["path"] = json!(path);
            state["identity"] = json!(identity);
            state["size"] = json!(stat.len());
            state["mtime"] = json!(modified);
            state["ctime"] = json!(changed);
            state["prefixHash"] = json!(format!("{:x}",hash.finalize()));
            self.cache.db.execute("INSERT INTO imports VALUES(?,?) ON CONFLICT(thread_id) DO UPDATE SET payload=excluded.payload",params![id,state.to_string()])?;
            Ok(())
        })?;
        ensure!(state["malformed"] != true, "Malformed transcript");
        Ok(())
    }
}

fn content_text(content: &Value, separator: &str) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join(separator)
}

struct DiscordTitles {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl DiscordTitles {
    fn new() -> Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    fn refresh(
        &self,
        cache: &Cache,
        bridge: &Bridge,
        thread: &Value,
        cancellation: &Cancellation,
    ) -> Result<()> {
        let Some(path) = &bridge.bot_token_file else {
            return Ok(());
        };
        let Some(channel) = thread["channelId"]
            .as_str()
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            return Ok(());
        };
        let id = thread["id"]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid thread"))?;
        let now = now_millis();
        let next: u64 = cache
            .db
            .query_row(
                "SELECT next_attempt FROM discord_titles WHERE thread_id=?",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let paused: u64 = cache
            .db
            .query_row(
                "SELECT value FROM meta WHERE key='discordPausedUntil'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if now < next || now < paused {
            return Ok(());
        }
        cache.db.execute("INSERT INTO discord_titles VALUES(?,?) ON CONFLICT(thread_id) DO UPDATE SET next_attempt=excluded.next_attempt",params![id,now+60_000])?;
        let Ok(token) = protected_token(path) else {
            return Ok(());
        };
        let token = match token.split_once(char::is_whitespace) {
            Some((prefix, value)) if prefix.eq_ignore_ascii_case("Bot") => value.trim_start(),
            _ => token.as_str(),
        };
        if !(30..=4096).contains(&token.len())
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Ok(());
        }
        let response: Result<(reqwest::StatusCode, Option<String>, Value)> = self.runtime.block_on(async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => bail!("Collector is stopping"),
                result = async {
                    let response = self.client.get(format!("https://discord.com/api/v10/channels/{channel}")).header(reqwest::header::AUTHORIZATION,format!("Bot {token}")).send().await?;
                    let status = response.status();
                    let retry = response.headers().get(reqwest::header::RETRY_AFTER).and_then(|value|value.to_str().ok()).map(str::to_owned);
                    let body = response.json::<Value>().await.unwrap_or(Value::Null);
                    Ok((status,retry,body))
                } => result,
            }
        });
        cancellation.check()?;
        let Ok((status, retry, body)) = response else {
            return Ok(());
        };
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let seconds = body["retry_after"]
                .as_f64()
                .or_else(|| {
                    body["retry_after"]
                        .as_str()
                        .and_then(|value| value.parse().ok())
                })
                .or_else(|| retry.and_then(|value| value.parse().ok()))
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .unwrap_or(1.0);
            let until = now_millis()
                .saturating_add((seconds * 1000.0).ceil() as u64)
                .min(i64::MAX as u64);
            cache.transaction(|| {
                cache.db.execute("INSERT INTO meta VALUES('discordPausedUntil',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[until])?;
                cache.db.execute("UPDATE discord_titles SET next_attempt=? WHERE thread_id=?",params![until,id])?;
                Ok(())
            })?;
            return Ok(());
        }
        if !status.is_success()
            || body["id"] != channel
            || (!thread["guildId"].is_null() && body["guild_id"] != thread["guildId"])
        {
            return Ok(());
        }
        let Some(name) = body["name"].as_str() else {
            return Ok(());
        };
        let title = cache.sanitizer.text(&name.replace(token, "[REDACTED]"));
        if title.trim().is_empty() {
            return Ok(());
        }
        if let Some(mut current) = cache.thread(id)? {
            current["title"] = json!(title.trim());
            current["titleSource"] = json!("discord");
            cache.transaction(|| {
                cache.upsert(&current, None)?;
                cache.db.execute(
                    "UPDATE discord_titles SET next_attempt=? WHERE thread_id=?",
                    params![now_millis() + 300_000, id],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }
}
