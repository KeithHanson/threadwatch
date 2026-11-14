use crate::{
    bridge::{
        Bridge, BridgeReader, Cache, Cancellation, expand_path, load_bridges, private_database,
        private_directory, protected_token,
    },
    sanitize::{Sanitizer, normalize_event, normalize_todos},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_EVENTS: usize = 1000;

#[derive(Clone)]
pub struct Config {
    pub server_url: String,
    pub token: String,
    pub bridge: Bridge,
    pub data_dir: PathBuf,
    pub collect_interval: Duration,
    pub heartbeat_interval: Duration,
    pub request_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let url =
            reqwest::Url::parse(&env::var("TW_SERVER_URL").unwrap_or_default()).map_err(|_| {
                anyhow!("TW_SERVER_URL must be a plain HTTP origin without credentials or paths")
            })?;
        ensure!(
            url.scheme() == "http"
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.host_str().is_some(),
            "TW_SERVER_URL must be a plain HTTP origin without credentials or paths"
        );
        let direct = env::var("TW_INGEST_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        let file = env::var_os("TW_INGEST_TOKEN_FILE").filter(|value| !value.is_empty());
        ensure!(
            direct.is_none() || file.is_none(),
            "Choose TW_INGEST_TOKEN or TW_INGEST_TOKEN_FILE"
        );
        let token = if let Some(file) = file {
            protected_token(&expand_path(PathBuf::from(file))?)?
        } else {
            direct.unwrap_or_default()
        };
        ensure!(
            token.len() >= 32 && !token.chars().any(char::is_whitespace),
            "Ingest token must contain at least 32 non-whitespace characters"
        );
        let bridges_file = env::var_os("TW_BRIDGES_FILE")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("TW_BRIDGES_FILE is required"))?;
        let mut bridges = load_bridges(&PathBuf::from(bridges_file))?;
        ensure!(
            bridges.len() == 1,
            "A collector requires exactly one bridge"
        );
        let data_dir = expand_path(
            env::var_os("TW_COLLECTOR_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~/.local/state/threadwatch-collector")),
        )?;
        let server_dir = expand_path(
            env::var_os("TW_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~/.local/state/threadwatch")),
        )?;
        ensure!(
            canonical_target(&data_dir)? != canonical_target(&server_dir)?,
            "Collector data directory must differ from server data directory"
        );
        Ok(Self {
            server_url: url.origin().ascii_serialization(),
            token,
            bridge: bridges.remove(0),
            data_dir,
            collect_interval: interval("TW_COLLECT_INTERVAL_MS", 2000, 2_147_483_647)?,
            heartbeat_interval: interval("TW_HEARTBEAT_INTERVAL_MS", 10_000, 30_000)?,
            request_timeout: Duration::from_secs(10),
        })
    }
}

fn interval(key: &str, default: u64, maximum: u64) -> Result<Duration> {
    let value = env::var(key)
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(default))
        .map_err(|_| anyhow!("Invalid {key}"))?;
    ensure!((100..=maximum).contains(&value), "Invalid {key}");
    Ok(Duration::from_millis(value))
}

fn canonical_target(path: &Path) -> Result<PathBuf> {
    let mut parent = path;
    let mut missing = Vec::new();
    while !parent.exists() {
        missing.push(
            parent
                .file_name()
                .ok_or_else(|| anyhow!("Invalid state directory"))?
                .to_owned(),
        );
        parent = parent
            .parent()
            .ok_or_else(|| anyhow!("Invalid state directory"))?;
    }
    let mut canonical = std::fs::canonicalize(parent)?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

pub struct StateLock {
    _db: Connection,
}

pub fn acquire_state_lock(directory: &Path) -> Result<StateLock> {
    private_directory(directory)?;
    let db = private_database(&directory.join("collector-lock.sqlite"))?;
    db.busy_timeout(Duration::ZERO)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS owner (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE;")
        .map_err(|_| anyhow!("Another collector owns this state directory"))?;
    Ok(StateLock { _db: db })
}

pub struct DeliveryQueue {
    pub db: Connection,
}

impl DeliveryQueue {
    pub fn open(directory: &Path, bridge: &Bridge) -> Result<Self> {
        private_directory(directory)?;
        let db = private_database(&directory.join("collector-queue.sqlite"))?;
        // These are the deployed JS tables verbatim. Pending payload bytes, IDs,
        // ordering, and capture checkpoints are never rewritten during cutover.
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS identity (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS deliveries (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS captured_threads (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, generation INTEGER NOT NULL, metadata TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS captured_events (thread_id TEXT NOT NULL, event_id TEXT NOT NULL, hash TEXT NOT NULL, parts INTEGER NOT NULL, PRIMARY KEY(thread_id,event_id));
            CREATE TEMP TABLE capture_events (event_id TEXT PRIMARY KEY, hash TEXT NOT NULL, parts INTEGER NOT NULL, changed INTEGER NOT NULL);")?;
        let identity = json!([bridge.id, bridge.database_path, bridge.session_dir]).to_string();
        let saved: Option<String> = db
            .query_row("SELECT value FROM identity WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let saved = match saved {
            Some(saved) => saved,
            None => {
                db.execute("INSERT INTO identity VALUES(1,?)", [&identity])?;
                identity.clone()
            }
        };
        // Compare parsed values, not serializer-specific whitespace or escapes.
        ensure!(
            serde_json::from_str::<Value>(&saved)? == serde_json::from_str::<Value>(&identity)?,
            "State directory belongs to another bridge; use a distinct directory"
        );
        Ok(Self { db })
    }
    pub fn count(&self) -> Result<u64> {
        Ok(self
            .db
            .query_row("SELECT COUNT(*) FROM deliveries", [], |row| row.get(0))?)
    }
    pub fn peek(&self) -> Result<Option<(String, String)>> {
        Ok(self
            .db
            .query_row(
                "SELECT id,payload FROM deliveries ORDER BY seq LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }
    pub fn acknowledge(&self, id: &str) -> Result<()> {
        self.db.execute("DELETE FROM deliveries WHERE id=?", [id])?;
        Ok(())
    }
}

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
struct ByteCounter(usize);
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn byte_length(value: &Value) -> usize {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).unwrap();
    counter.0
}

fn json_digest(value: &impl serde::Serialize) -> Result<String> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("{:x}", writer.0.finalize()))
}

fn envelope(thread: &Value, todos: &Value, events: Vec<Value>, reset: bool, id: String) -> Value {
    json!({"thread":thread,"todos":todos,"events":events,"delivery":{"id":id,"reset":reset}})
}

pub fn event_capacity(thread: &Value, todos: &Value, max_bytes: usize) -> Result<usize> {
    let base = byte_length(&envelope(thread, todos, Vec::new(), false, "0".repeat(36)));
    let capacity = max_bytes
        .checked_sub(base)
        .ok_or_else(|| anyhow!("Thread metadata or todos exceed delivery capacity"))?;
    ensure!(
        capacity >= 256,
        "Thread metadata or todos exceed delivery capacity"
    );
    Ok(capacity)
}

fn escaped_length(text: &str) -> usize {
    text.chars()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            '\u{0}'..='\u{1f}' => 6,
            other => other.len_utf8(),
        })
        .sum()
}

fn event_with_text(event: &Value, id: &str, text: String) -> Value {
    let mut object: Map<String, Value> = event
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, _)| *key != "text" && *key != "id")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    object.insert("id".into(), json!(id));
    object.insert("text".into(), json!(text));
    Value::Object(object)
}

pub fn split_event(event: &Value, available_bytes: usize) -> Result<Vec<Value>> {
    let id = event["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Invalid event identity"))?;
    let text = event["text"]
        .as_str()
        .ok_or_else(|| anyhow!("Invalid event text"))?;
    let empty = event_with_text(event, id, String::new());
    if byte_length(&empty) + escaped_length(text) <= available_bytes {
        return Ok(vec![event.clone()]);
    }
    let key = digest(id);
    let reserve = format!(
        "[Continuation {} of {}; original event {id}]\n",
        "9".repeat(20),
        "9".repeat(20)
    );
    let template = event_with_text(
        event,
        &format!("continuation:{key}:{}", "9".repeat(20)),
        reserve,
    );
    let base = byte_length(&template);
    ensure!(
        base + 8 <= available_bytes,
        "Event metadata exceeds delivery capacity"
    );
    let budget = available_bytes - base;
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut length = 0;
    for (offset, character) in text.char_indices() {
        let bytes = match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            '\u{0}'..='\u{1f}' => 6,
            other => other.len_utf8(),
        };
        if length + bytes > budget {
            ensure!(offset > start, "No room for event text");
            ranges.push(start..offset);
            start = offset;
            length = 0;
        }
        length += bytes;
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    let total = ranges.len();
    Ok(ranges
        .into_iter()
        .enumerate()
        .map(|(index, range)| {
            event_with_text(
                event,
                &format!("continuation:{key}:{}", index + 1),
                format!(
                    "[Continuation {} of {total}; original event {id}]\n{}",
                    index + 1,
                    &text[range]
                ),
            )
        })
        .collect())
}

struct BatchWriter<'a, F: FnMut(Value) -> Result<()>> {
    thread: &'a Value,
    todos: &'a Value,
    batch: Vec<Value>,
    base: usize,
    size: usize,
    first: bool,
    reset: bool,
    write: F,
}

impl<'a, F: FnMut(Value) -> Result<()>> BatchWriter<'a, F> {
    fn new(thread: &'a Value, todos: &'a Value, reset: bool, write: F) -> Result<Self> {
        let capacity = event_capacity(thread, todos, MAX_BATCH_BYTES)?;
        let base = MAX_BATCH_BYTES - capacity;
        Ok(Self {
            thread,
            todos,
            batch: Vec::new(),
            base,
            size: base,
            first: true,
            reset,
            write,
        })
    }
    fn push(&mut self, event: Value) -> Result<()> {
        let bytes = byte_length(&event);
        ensure!(
            self.base + bytes <= MAX_BATCH_BYTES,
            "Unsplit event exceeds delivery capacity"
        );
        if !self.batch.is_empty()
            && (self.batch.len() == MAX_BATCH_EVENTS || self.size + bytes + 1 > MAX_BATCH_BYTES)
        {
            self.flush()?;
        }
        self.size += bytes + usize::from(!self.batch.is_empty());
        self.batch.push(event);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        let batch = envelope(
            self.thread,
            self.todos,
            std::mem::take(&mut self.batch),
            self.first && self.reset,
            uuid::Uuid::new_v4().to_string(),
        );
        (self.write)(batch)?;
        self.first = false;
        self.size = self.base;
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        if !self.batch.is_empty() || self.first {
            self.flush()?;
        }
        Ok(())
    }
}

fn field(value: &str, max: usize, sanitizer: &Sanitizer) -> Result<String> {
    let text = sanitizer.text(value);
    ensure!(
        !text.is_empty() && text.encode_utf16().count() <= max,
        "Invalid collector metadata field"
    );
    Ok(text)
}

fn thread_payload(thread: &Value, bridge: &Bridge, sanitizer: &Sanitizer) -> Result<Value> {
    let identity: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD.decode(
            thread["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Invalid cached thread identity"))?,
        )?,
    )?;
    ensure!(
        identity[0] == "bridge" && identity[1] == bridge.id,
        "Unexpected cached bridge identity"
    );
    let id = identity[2]
        .as_str()
        .ok_or_else(|| anyhow!("Invalid cached thread identity"))?;
    let mut payload = json!({"id":field(id,200,sanitizer)?,"botId":bridge.id,"botName":field(&bridge.name,200,sanitizer)?,
        "title":field(thread["title"].as_str().unwrap_or(""),1000,sanitizer)?,"status":thread["status"],
        "updatedAt":thread["updatedAt"].as_u64().filter(|time|*time <= 9_007_199_254_740_991).unwrap_or(0)});
    for key in ["guildId", "channelId"] {
        payload[key] = if thread[key].is_null() {
            Value::Null
        } else {
            json!(field(
                thread[key]
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid collector metadata field"))?,
                100,
                sanitizer
            )?)
        };
    }
    if let Some(source) = thread["titleSource"].as_str() {
        payload["titleSource"] = json!(sanitizer.text(source));
    }
    Ok(payload)
}

pub fn capture_changes(
    cache: &Cache,
    queue: &DeliveryQueue,
    bridge: &Bridge,
    cancellation: &Cancellation,
) -> Result<usize> {
    let mut total = 0;
    let mut threads = cache
        .db
        .prepare("SELECT id,payload,todos,revision FROM threads ORDER BY id")?;
    let mut rows = threads.query([])?;
    while let Some(row) = rows.next()? {
        cancellation.check()?;
        let id: String = row.get(0)?;
        let revision: i64 = row.get(3)?;
        let previous: Option<(i64, i64, String)> = queue
            .db
            .query_row(
                "SELECT revision,generation,metadata FROM captured_threads WHERE id=?",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if previous
            .as_ref()
            .is_some_and(|previous| previous.0 == revision)
        {
            continue;
        }
        let thread = thread_payload(
            &serde_json::from_str::<Value>(&row.get::<_, String>(1)?)?,
            bridge,
            &cache.sanitizer,
        )?;
        let todos = normalize_todos(
            &serde_json::from_str::<Value>(&row.get::<_, String>(2)?)?,
            &cache.sanitizer,
        )?;
        ensure!(
            todos.as_array().unwrap().len() <= 2000,
            "Too many todos for ingestion"
        );
        let capacity = event_capacity(&thread, &todos, MAX_BATCH_BYTES)?;
        let metadata = digest(json!({"thread":thread,"todos":todos}).to_string());
        let generation: i64 = cache
            .db
            .query_row(
                "SELECT generation FROM collector_resets WHERE thread_id=?",
                [&id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut reset = previous
            .as_ref()
            .is_none_or(|previous| previous.1 != generation);
        queue.db.execute("DELETE FROM capture_events", [])?;
        let mut old = queue
            .db
            .prepare("SELECT hash,parts FROM captured_events WHERE thread_id=? AND event_id=?")?;
        let mut stage = queue
            .db
            .prepare("INSERT INTO capture_events VALUES(?,?,?,?)")?;
        let mut events = cache
            .db
            .prepare("SELECT payload FROM events WHERE thread_id=? ORDER BY seq")?;
        let mut event_rows = events.query([&id])?;
        let mut changed = false;
        while let Some(event_row) = event_rows.next()? {
            cancellation.check()?;
            let event: Value = serde_json::from_str(&event_row.get::<_, String>(0)?)?;
            let Some(event) = normalize_event(&event, &cache.sanitizer) else {
                continue;
            };
            let fragments = split_event(&event, capacity)?;
            let hash = json_digest(&fragments)?;
            let parts = fragments.len() as i64;
            let event_id = event["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Invalid event identity"))?;
            let before: Option<(String, i64)> = old
                .query_row(params![id, event_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            let different = before
                .as_ref()
                .is_none_or(|(old_hash, old_parts)| *old_hash != hash || *old_parts != parts);
            changed |= different;
            if before.is_some() && different {
                reset = true;
            }
            stage.execute(params![event_id, hash, parts, different])?;
        }
        drop(event_rows);
        let removed: bool = queue.db.query_row("SELECT EXISTS(SELECT 1 FROM captured_events WHERE thread_id=? AND NOT EXISTS(SELECT 1 FROM capture_events WHERE capture_events.event_id=captured_events.event_id))",[&id],|row|row.get(0))?;
        reset |= removed;
        let transaction = queue.db.unchecked_transaction()?;
        if reset
            || changed
            || previous
                .as_ref()
                .is_none_or(|previous| previous.2 != metadata)
        {
            let mut insert = queue
                .db
                .prepare("INSERT INTO deliveries(id,payload) VALUES(?,?)")?;
            let mut batches = BatchWriter::new(&thread, &todos, reset, |batch| {
                cancellation.check()?;
                insert.execute(params![
                    batch["delivery"]["id"].as_str().unwrap(),
                    batch.to_string()
                ])?;
                total += 1;
                Ok(())
            })?;
            let mut selected = queue
                .db
                .prepare("SELECT changed FROM capture_events WHERE event_id=?")?;
            let mut event_rows = events.query([&id])?;
            while let Some(event_row) = event_rows.next()? {
                cancellation.check()?;
                let event: Value = serde_json::from_str(&event_row.get::<_, String>(0)?)?;
                let Some(event) = normalize_event(&event, &cache.sanitizer) else {
                    continue;
                };
                let different: bool =
                    selected.query_row([event["id"].as_str().unwrap()], |row| row.get(0))?;
                if reset || different {
                    for fragment in split_event(&event, capacity)? {
                        batches.push(fragment)?;
                    }
                }
            }
            batches.finish()?;
        }
        queue.db.execute("DELETE FROM captured_events WHERE thread_id=? AND NOT EXISTS(SELECT 1 FROM capture_events WHERE capture_events.event_id=captured_events.event_id)",[&id])?;
        queue.db.execute("INSERT INTO captured_events(thread_id,event_id,hash,parts) SELECT ?,event_id,hash,parts FROM capture_events WHERE true ON CONFLICT(thread_id,event_id) DO UPDATE SET hash=excluded.hash,parts=excluded.parts",[&id])?;
        queue.db.execute("INSERT INTO captured_threads VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,generation=excluded.generation,metadata=excluded.metadata",params![id,revision,generation,metadata])?;
        cancellation.check()?;
        transaction.commit()?;
    }
    Ok(total)
}

pub struct Transport {
    client: reqwest::Client,
    origin: String,
    authorization: reqwest::header::HeaderValue,
}

impl Transport {
    pub fn new(config: &Config) -> Result<Self> {
        let origin = reqwest::Url::parse(&config.server_url)
            .map_err(|_| anyhow!("Invalid collector HTTP origin"))?;
        ensure!(
            origin.scheme() == "http"
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none(),
            "Invalid collector HTTP origin"
        );
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.token))
                .map_err(|_| anyhow!("Invalid shared HTTP token"))?;
        authorization.set_sensitive(true);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()
            .map_err(|_| anyhow!("Cannot initialize HTTP transport"))?;
        Ok(Self {
            client,
            origin: origin.origin().ascii_serialization(),
            authorization,
        })
    }
    pub async fn post(
        &self,
        path: &str,
        payload: String,
        cancellation: &Cancellation,
    ) -> Result<Value> {
        ensure!(
            matches!(path, "/api/ingest" | "/api/heartbeat"),
            "Invalid collector API path"
        );
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => bail!("Collector is stopping"),
            response = async {
                let response = self.client.post(format!("{}{path}",self.origin)).header(reqwest::header::AUTHORIZATION,self.authorization.clone()).header(reqwest::header::CONTENT_TYPE,"application/json").body(payload).send().await.map_err(|_|anyhow!("Reporting endpoint is unavailable"))?;
                ensure!(response.status().is_success(),"Reporting endpoint rejected request");
                let value = response.json::<Value>().await.map_err(|_|anyhow!("Reporting endpoint returned an invalid response"))?;
                if path == "/api/ingest" {
                    ensure!(value["id"].as_str().is_some_and(|id|!id.is_empty()) && value["revision"].is_string(),"Reporting endpoint returned an invalid acknowledgement");
                }
                Ok(value)
            } => response,
        }
    }
}

pub async fn flush(
    queue: &mut DeliveryQueue,
    transport: &Transport,
    cancellation: &Cancellation,
) -> Result<()> {
    loop {
        cancellation.check()?;
        let Some((id, payload)) = queue.peek()? else {
            return Ok(());
        };
        // The durable UUID remains untouched until a complete successful ACK.
        // A lost response replays the exact bytes; the receiver owns deduplication.
        transport.post("/api/ingest", payload, cancellation).await?;
        queue.acknowledge(&id)?;
    }
}

#[derive(Default)]
struct Health {
    delivery_failed: AtomicBool,
    heartbeat_failed: AtomicBool,
}

fn heartbeat_payload(
    config: &Config,
    sanitizer: &Sanitizer,
    source: &Value,
    health: &Health,
    pending: u64,
) -> Value {
    let failed = health.delivery_failed.load(Ordering::Acquire)
        || health.heartbeat_failed.load(Ordering::Acquire);
    let status = if source["status"] == "unavailable" {
        "unavailable"
    } else if source["status"] == "degraded" || failed {
        "degraded"
    } else {
        "ready"
    };
    let mut payload = json!({"botId":config.bridge.id,"botName":sanitizer.text(&config.bridge.name),"status":status,"pendingBatches":pending});
    if let Some(error) = source.get("error") {
        payload["error"] = error.clone();
    } else if failed {
        payload["error"] =
            json!("Reporting endpoint is unavailable; saved batches will be retried.");
    }
    payload
}

async fn heartbeat_loop(
    config: Arc<Config>,
    sanitizer: Arc<Sanitizer>,
    source: tokio::sync::watch::Receiver<Value>,
    health: Arc<Health>,
    cancellation: Arc<Cancellation>,
    stopped: Arc<Cancellation>,
) -> Result<()> {
    let queue = DeliveryQueue::open(&config.data_dir, &config.bridge)?;
    let transport = Transport::new(&config)?;
    let mut timer = tokio::time::interval(config.heartbeat_interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            _ = stopped.cancelled() => return Ok(()),
            _ = timer.tick() => {},
        }
        let snapshot = source.borrow().clone();
        let payload =
            heartbeat_payload(&config, &sanitizer, &snapshot, &health, queue.count()?).to_string();
        let result = tokio::select! {
            biased;
            _ = stopped.cancelled() => return Ok(()),
            result = transport.post("/api/heartbeat",payload,&cancellation) => result,
        };
        health
            .heartbeat_failed
            .store(result.is_err(), Ordering::Release);
    }
}

async fn delivery_loop(
    config: Arc<Config>,
    health: Arc<Health>,
    cancellation: Arc<Cancellation>,
) -> Result<()> {
    let mut queue = DeliveryQueue::open(&config.data_dir, &config.bridge)?;
    let transport = Transport::new(&config)?;
    loop {
        let result = flush(&mut queue, &transport, &cancellation).await;
        health
            .delivery_failed
            .store(result.is_err(), Ordering::Release);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            _ = tokio::time::sleep(config.collect_interval) => {},
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
    Ok(())
}

pub async fn run(config: Config, once: bool) -> Result<()> {
    let _lock = acquire_state_lock(&config.data_dir)?;
    // Fail identity checks before starting any worker or making HTTP requests.
    drop(DeliveryQueue::open(&config.data_dir, &config.bridge)?);
    let config = Arc::new(config);
    let sanitizer = Arc::new(Sanitizer::new(vec![config.token.clone()]));
    let cancellation = Arc::new(Cancellation::default());
    let heartbeat_stop = Arc::new(Cancellation::default());
    let health = Arc::new(Health::default());
    let (source_tx, source_rx) = tokio::sync::watch::channel(
        json!({"status":"degraded","error":"Initial source collection is in progress."}),
    );
    let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let worker_config = config.clone();
    let worker_sanitizer = sanitizer.clone();
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::Builder::new().name("threadwatch-collection".into()).spawn(move || {
        let result = (|| -> Result<()> {
            let cache = Cache::open(&worker_config.data_dir,worker_sanitizer)?;
            let queue = DeliveryQueue::open(&worker_config.data_dir,&worker_config.bridge)?;
            let mut reader = BridgeReader::new(cache,worker_config.bridge.clone(),worker_cancellation.clone())?;
            loop {
                if worker_cancellation.is_cancelled() { return Ok(()); }
                let result = (|| -> Result<Value> {
                    // Recover completed cache commits BEFORE version-3 reparsing
                    // may replace them. Each capture commits payloads+checkpoints
                    // atomically, after all preexisting deliveries in FIFO order.
                    capture_changes(&reader.cache,&queue,&worker_config.bridge,&worker_cancellation)?;
                    let source = reader.refresh();
                    capture_changes(&reader.cache,&queue,&worker_config.bridge,&worker_cancellation)?;
                    Ok(source)
                })();
                let source = result.unwrap_or_else(|_|json!({"status":"degraded","error":"Collector could not capture all saved updates; they will be retried."}));
                let ready = source["status"] == "ready";
                let _ = source_tx.send(source);
                if once { ensure!(ready,"Collector source is degraded or unavailable"); return Ok(()); }
                worker_cancellation.sleep(worker_config.collect_interval);
            }
        })();
        if result.is_err() && !once { let _ = source_tx.send(json!({"status":"unavailable","error":"Collector adapter worker is unavailable."})); }
        let _ = finished_tx.send(result);
    }).context("Cannot start collector worker")?;
    let mut heartbeat = tokio::spawn(heartbeat_loop(
        config.clone(),
        sanitizer.clone(),
        source_rx.clone(),
        health.clone(),
        cancellation.clone(),
        heartbeat_stop.clone(),
    ));
    let signal_cancellation = cancellation.clone();
    let signals = tokio::spawn(async move {
        let result = shutdown_signal().await;
        signal_cancellation.cancel();
        result
    });
    let mut heartbeat_consumed = false;
    let result = if once {
        let collected = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(anyhow!("Collector interrupted before completion")),
            result = &mut finished_rx => result.map_err(|_|anyhow!("Collector worker failed")).and_then(|result|result),
            result = &mut heartbeat => {
                heartbeat_consumed = true;
                cancellation.cancel();
                result.map_err(|_|anyhow!("Collector heartbeat task failed")).and_then(|result|result).and(Err(anyhow!("Collector heartbeat task stopped unexpectedly")))
            },
        };
        // Even a degraded source may have successfully captured recoverable data.
        let delivered = async {
            cancellation.check()?;
            let mut queue = DeliveryQueue::open(&config.data_dir, &config.bridge)?;
            let transport = Transport::new(&config)?;
            let result = flush(&mut queue, &transport, &cancellation).await;
            health
                .delivery_failed
                .store(result.is_err(), Ordering::Release);
            result
        }
        .await;
        heartbeat_stop.cancel();
        // Do not reuse an in-flight initial heartbeat carrying stale source state.
        let heartbeat_result = if heartbeat_consumed {
            Ok(())
        } else {
            heartbeat_consumed = true;
            (&mut heartbeat)
                .await
                .map_err(|_| anyhow!("Collector heartbeat task failed"))
                .and_then(|result| result)
        };
        let reported = async {
            cancellation.check()?;
            let queue = DeliveryQueue::open(&config.data_dir, &config.bridge)?;
            let snapshot = source_rx.borrow().clone();
            let payload =
                heartbeat_payload(&config, &sanitizer, &snapshot, &health, queue.count()?)
                    .to_string();
            Transport::new(&config)?
                .post("/api/heartbeat", payload, &cancellation)
                .await?;
            ensure!(queue.count()? == 0, "Collector has undelivered batches");
            Ok(())
        }
        .await;
        collected.and(delivered).and(heartbeat_result).and(reported)
    } else {
        let mut delivery = tokio::spawn(delivery_loop(
            config.clone(),
            health.clone(),
            cancellation.clone(),
        ));
        let mut delivery_consumed = false;
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Ok(()),
            result = &mut finished_rx => result.map_err(|_|anyhow!("Collector worker failed")).and_then(|result|result).and(Err(anyhow!("Collector worker stopped unexpectedly"))),
            result = &mut heartbeat => {
                heartbeat_consumed = true;
                result.map_err(|_|anyhow!("Collector heartbeat task failed")).and_then(|result|result).and(Err(anyhow!("Collector heartbeat task stopped unexpectedly")))
            },
            result = &mut delivery => {
                delivery_consumed = true;
                result.map_err(|_|anyhow!("Collector delivery task failed")).and_then(|result|result).and(Err(anyhow!("Collector delivery task stopped unexpectedly")))
            },
        };
        cancellation.cancel();
        if !delivery_consumed {
            let _ = delivery.await;
        }
        result
    };
    cancellation.cancel();
    heartbeat_stop.cancel();
    if !heartbeat_consumed {
        let _ = heartbeat.await;
    }
    let signal_result = if signals.is_finished() {
        signals
            .await
            .map_err(|_| anyhow!("Collector signal handler failed"))
            .and_then(|result| result)
    } else {
        signals.abort();
        Ok(())
    };
    let joined = tokio::task::spawn_blocking(move || worker.join()).await;
    ensure!(
        matches!(joined, Ok(Ok(()))),
        "Collector worker failed during shutdown"
    );
    result.and(signal_result).map_err(|_| {
        anyhow!(
            "Collector did not complete successfully; saved deliveries remain available for retry"
        )
    })
}
