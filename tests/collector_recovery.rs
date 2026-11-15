use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;
use threadwatch::{
    bridge::{Bridge, BridgeReader, Cache, Cancellation, protected_token, thread_id},
    collector::{
        Config, DeliveryQueue, MAX_BATCH_BYTES, acquire_state_lock, capture_changes,
        event_capacity, split_event,
    },
    sanitize::{Sanitizer, derive_todos, normalize_event, operational_record},
};

const TOKEN: &str = "collector-test-shared-ingestion-secret-123456789";

struct Fixture {
    _root: TempDir,
    bridge: Bridge,
    data: PathBuf,
    transcript: PathBuf,
}

impl Fixture {
    fn new(records: &[Value]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let transcript = sessions.join("one.jsonl");
        fs::write(
            &transcript,
            records
                .iter()
                .map(|record| record.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let bridge = Bridge {
            id: "test-bot".into(),
            name: "Test Bot".into(),
            database_path: root.path().join("bridge.sqlite"),
            session_dir: sessions,
            bot_token_file: None,
        };
        let db = Connection::open(&bridge.database_path).unwrap();
        db.execute_batch("CREATE TABLE sessions (thread_id TEXT PRIMARY KEY,guild_id TEXT,parent_channel_id TEXT,omp_session_path TEXT,status TEXT,updated_at_ms INTEGER);
            CREATE TABLE turns (id TEXT,thread_id TEXT,status TEXT,created_at_ms INTEGER,started_at_ms INTEGER,finished_at_ms INTEGER);").unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?,?,?,?,?,?)",
            params![
                "123",
                "456",
                "789",
                transcript.to_str().unwrap(),
                "active",
                100
            ],
        )
        .unwrap();
        let data = root.path().join("collector");
        Self {
            _root: root,
            bridge,
            data,
            transcript,
        }
    }
    fn sanitizer(&self) -> Arc<Sanitizer> {
        Arc::new(Sanitizer::new(vec![TOKEN.into()]))
    }
    fn cache(&self) -> Cache {
        Cache::open(&self.data, self.sanitizer()).unwrap()
    }
    fn queue(&self) -> DeliveryQueue {
        DeliveryQueue::open(&self.data, &self.bridge).unwrap()
    }
    fn reader(&self) -> BridgeReader {
        BridgeReader::new(
            self.cache(),
            self.bridge.clone(),
            Arc::new(Cancellation::default()),
        )
        .unwrap()
    }
    fn config(&self) -> Config {
        Config {
            server_url: "http://127.0.0.1:1".into(),
            token: TOKEN.into(),
            bridge: self.bridge.clone(),
            data_dir: self.data.clone(),
            collect_interval: Duration::from_millis(100),
            heartbeat_interval: Duration::from_millis(100),
            request_timeout: Duration::from_millis(250),
        }
    }
    fn append(&self, text: &str) {
        OpenOptions::new()
            .append(true)
            .open(&self.transcript)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
}

fn message(text: &str) -> Value {
    json!({"type":"message","message":{"role":"assistant","content":text}})
}
fn event(id: &str, text: &str) -> Value {
    json!({"id":id,"timestamp":0,"role":"assistant","kind":"message","text":text})
}
fn cached_thread() -> Value {
    json!({"id":thread_id("bridge","test-bot","123"),"botId":"test-bot","botName":"Test Bot","title":"Task","titleSource":"session","guildId":null,"channelId":"123","status":"working","updatedAt":100,"source":"bridge:test-bot"})
}
fn queued(queue: &DeliveryQueue) -> Vec<Value> {
    queue
        .db
        .prepare("SELECT payload FROM deliveries ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|raw| serde_json::from_str(&raw.unwrap()).unwrap())
        .collect()
}
fn cached_events(cache: &Cache) -> Vec<Value> {
    cache
        .db
        .prepare("SELECT payload FROM events ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|raw| serde_json::from_str(&raw.unwrap()).unwrap())
        .collect()
}
fn drain_locally(queue: &DeliveryQueue) {
    while let Some((id, _)) = queue.peek().unwrap() {
        queue.acknowledge(&id).unwrap();
    }
}

#[test]
fn sqlite_owner_lock_interoperates_with_deployed_js_transaction() {
    let fixture = Fixture::new(&[]);
    fs::create_dir(&fixture.data).unwrap();
    let old = Connection::open(fixture.data.join("collector-lock.sqlite")).unwrap();
    old.execute_batch("CREATE TABLE owner (id INTEGER PRIMARY KEY); BEGIN IMMEDIATE;")
        .unwrap();
    assert!(acquire_state_lock(&fixture.data).is_err());
    drop(old);
    let guard = acquire_state_lock(&fixture.data).unwrap();
    let contender = Connection::open(fixture.data.join("collector-lock.sqlite")).unwrap();
    contender.busy_timeout(Duration::ZERO).unwrap();
    assert!(contender.execute_batch("BEGIN IMMEDIATE").is_err());
    drop(guard);
    contender
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
        .unwrap();
}

#[test]
fn protected_tokens_reject_public_files_and_symlinks() {
    let fixture = Fixture::new(&[]);
    let path = fixture._root.path().join("private-token");
    fs::write(&path, format!("{TOKEN}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(protected_token(&path).unwrap(), TOKEN);
    let link = fixture._root.path().join("linked-token");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(protected_token(&link).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(protected_token(&path).is_err());
}

#[test]
fn operational_redaction_omits_hidden_payloads_without_erasing_bot_prose() {
    let sanitizer = Sanitizer::new(vec![TOKEN.into()]);
    let text = sanitizer.text(&format!("Inspect bot integration <system-reminder>instructions</system-reminder> token=abc123 Bearer xyz {TOKEN} https://u:p@example.com"));
    for hidden in ["instructions", "abc123", "Bearer xyz", TOKEN, "u:p"] {
        assert!(!text.contains(hidden));
    }
    assert!(text.starts_with("Inspect bot integration "));
    let nested = sanitizer.value(&json!({"apiKey":"hidden","providerPayload":{"text":"hidden"},"nested":{"thinking":"hidden","text":"visible"}}));
    assert!(!nested.to_string().contains("hidden"));
    let records = operational_record(
        &json!({"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private thought"},{"type":"text","text":"visible"},{"type":"toolCall","name":"read","id":"x","arguments":{"path":"/tmp/AGENTS.md"}}]}}),
        &sanitizer,
        1,
        false,
    );
    assert_eq!(records.len(), 2);
    assert!(records[1]["text"].as_str().unwrap().contains("WITHHELD"));
    assert!(
        operational_record(
            &json!({"type":"message","message":{"role":"system","content":"hidden"}}),
            &sanitizer,
            2,
            false
        )
        .is_empty()
    );
    assert!(normalize_event(&json!({"kind":"thinking","text":"hidden"}), &sanitizer).is_none());
    let normalized = normalize_event(&json!({"id":"one","kind":"message","timestamp":null,"text":"visible","providerMetadata":"hidden"}),&sanitizer).unwrap();
    assert_eq!(normalized["timestamp"], 0);
    assert!(normalized.get("providerMetadata").is_none());
}

#[test]
fn successful_todo_result_preserves_completed_and_current_tasks() {
    let result = "  Build [in_progress] (Work)\n  Verify [pending] (Work)\n  Work:\n    - [x] Plan\n    - [ ] Build (current task)\n    - [ ] Verify";
    assert_eq!(
        derive_todos(&json!({"op":"done"}), result, &json!([])),
        json!([
            {"text":"Plan","status":"completed"},{"text":"Build","status":"in_progress"},{"text":"Verify","status":"pending"}
        ])
    );
    assert_eq!(
        derive_todos(&json!({"op":"drop"}), result, &json!([])),
        json!([])
    );
}

#[test]
fn partial_records_and_tool_correlation_survive_restart_without_persisting_arguments() {
    let call = |id: &str, name: &str, args: Value| json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":id,"name":name,"arguments":args}]}});
    let result = |id: &str, name: &str, text: &str, error: bool| json!({"type":"message","message":{"role":"toolResult","toolCallId":id,"toolName":name,"isError":error,"content":[{"type":"text","text":text}]}});
    let fixture = Fixture::new(&[
        call("failed", "todo", json!({"op":"drop"})),
        result("failed", "todo", "failed", true),
        result(
            "unmatched",
            "todo",
            "[{\"text\":\"Wrong\",\"status\":\"completed\"}]",
            false,
        ),
        call(
            "success",
            "todo",
            json!({"op":"done","privateData":"do not checkpoint"}),
        ),
        call("secret", "read", json!({"path":"/tmp/.env"})),
    ]);
    let mut reader = fixture.reader();
    assert_eq!(reader.refresh()["status"], "ready");
    assert_eq!(
        reader
            .cache
            .todos(cached_thread()["id"].as_str().unwrap())
            .unwrap(),
        json!([])
    );
    let checkpoint: String = reader
        .cache
        .db
        .query_row("SELECT payload FROM imports", [], |row| row.get(0))
        .unwrap();
    assert!(!checkpoint.contains("privateData"));
    assert!(!checkpoint.contains("do not checkpoint"));
    drop(reader);
    let todo_result = result("success", "todo", "- [x] Plan\n- [ ] Ship", false).to_string();
    fixture.append(&todo_result[..todo_result.len() - 1]);
    let mut reader = fixture.reader();
    assert_eq!(reader.refresh()["status"], "ready");
    assert_eq!(
        reader
            .cache
            .todos(cached_thread()["id"].as_str().unwrap())
            .unwrap(),
        json!([])
    );
    fixture.append("}\n");
    fixture.append(&(result("secret", "read", "UNKNOWN_FORMAT_SECRET", false).to_string() + "\n"));
    assert_eq!(reader.refresh()["status"], "ready");
    assert_eq!(
        reader
            .cache
            .todos(cached_thread()["id"].as_str().unwrap())
            .unwrap(),
        json!([{"text":"Plan","status":"completed"},{"text":"Ship","status":"pending"}])
    );
    let events = cached_events(&reader.cache);
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("UNKNOWN_FORMAT_SECRET")
    );
    assert!(
        events.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("WITHHELD")
    );
    fixture.append("{malformed source secret}\n");
    assert_eq!(reader.refresh()["status"], "degraded");
    assert_eq!(reader.refresh()["status"], "degraded");
    assert_eq!(
        cached_events(&reader.cache).last().unwrap()["kind"],
        "import_error"
    );
}

#[test]
fn source_append_is_incremental_but_rewrite_and_grow_replaces_history() {
    let fixture = Fixture::new(&[message("original")]);
    let mut reader = fixture.reader();
    let queue = fixture.queue();
    let stop = Cancellation::default();
    assert_eq!(reader.refresh()["status"], "ready");
    capture_changes(&reader.cache, &queue, &fixture.bridge, &stop).unwrap();
    drain_locally(&queue);
    fixture.append(&(message("appended").to_string() + "\n"));
    assert_eq!(reader.refresh()["status"], "ready");
    capture_changes(&reader.cache, &queue, &fixture.bridge, &stop).unwrap();
    let batches = queued(&queue);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["delivery"]["reset"], false);
    assert_eq!(batches[0]["events"].as_array().unwrap().len(), 1);
    assert_eq!(batches[0]["events"][0]["text"], "appended");
    drain_locally(&queue);
    fs::write(
        &fixture.transcript,
        format!(
            "{}\n{}\n{}\n",
            message("replaced first record"),
            message("replaced second record"),
            message("long extra record so the file grows")
        ),
    )
    .unwrap();
    assert_eq!(reader.refresh()["status"], "ready");
    capture_changes(&reader.cache, &queue, &fixture.bridge, &stop).unwrap();
    assert_eq!(queued(&queue)[0]["delivery"]["reset"], true);
    let events = cached_events(&reader.cache);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["text"], "replaced first record");
}

#[test]
fn oversized_unicode_and_backfill_preserve_full_text_and_wire_bounds() {
    let fixture = Fixture::new(&[]);
    let cache = fixture.cache();
    let queue = fixture.queue();
    let thread = cached_thread();
    cache.upsert(&thread, Some(&json!([]))).unwrap();
    let text = "😀漢字\"\\\n".repeat(180_000);
    let mut events = vec![event("huge", &text)];
    events.extend((0..1005).map(|index| event(&format!("small-{index}"), "message")));
    cache
        .transaction(|| cache.append(thread["id"].as_str().unwrap(), &events))
        .unwrap();
    let stop = Cancellation::default();
    capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap();
    let batches = queued(&queue);
    assert!(batches.len() >= 3);
    assert_eq!(batches[0]["delivery"]["reset"], true);
    assert!(
        batches[1..]
            .iter()
            .all(|batch| batch["delivery"]["reset"] == false)
    );
    for batch in &batches {
        assert!(batch.to_string().len() <= MAX_BATCH_BYTES);
        assert!(batch["events"].as_array().unwrap().len() <= 1000);
    }
    let continuations: Vec<Value> = batches
        .iter()
        .flat_map(|batch| batch["events"].as_array().unwrap())
        .filter(|event| event["id"].as_str().unwrap().starts_with("continuation:"))
        .cloned()
        .collect();
    assert!(continuations.len() > 1);
    let reassembled: String = continuations
        .iter()
        .map(|event| event["text"].as_str().unwrap().split_once('\n').unwrap().1)
        .collect();
    assert_eq!(reassembled, text);
    assert_eq!(
        split_event(
            &event("huge", &text),
            event_capacity(&batches[0]["thread"], &json!([]), MAX_BATCH_BYTES).unwrap()
        )
        .unwrap(),
        continuations
    );
    assert_eq!(
        capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap(),
        0
    );
}

#[test]
fn capture_failure_rolls_back_all_batches_and_checkpoint_together() {
    let fixture = Fixture::new(&[]);
    let cache = fixture.cache();
    let queue = fixture.queue();
    let thread = cached_thread();
    cache.upsert(&thread, None).unwrap();
    cache
        .append(
            thread["id"].as_str().unwrap(),
            &(0..1001)
                .map(|index| event(&index.to_string(), "message"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    queue.db.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON deliveries WHEN (SELECT COUNT(*) FROM deliveries)=1 BEGIN SELECT RAISE(ABORT,'simulated storage failure'); END").unwrap();
    let stop = Cancellation::default();
    assert!(capture_changes(&cache, &queue, &fixture.bridge, &stop).is_err());
    assert_eq!(queue.count().unwrap(), 0);
    assert_eq!(
        queue
            .db
            .query_row("SELECT COUNT(*) FROM captured_threads", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    queue.db.execute_batch("DROP TRIGGER fail_second").unwrap();
    assert_eq!(
        capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap(),
        2
    );
}

#[test]
fn metadata_corrections_deletions_and_split_shrinkage_cannot_leave_stale_remote_events() {
    let fixture = Fixture::new(&[]);
    let cache = fixture.cache();
    let queue = fixture.queue();
    let mut thread = cached_thread();
    let id = thread["id"].as_str().unwrap().to_owned();
    cache.upsert(&thread, None).unwrap();
    cache
        .append(&id, &[event("one", "original"), event("two", "to delete")])
        .unwrap();
    let stop = Cancellation::default();
    capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap();
    drain_locally(&queue);
    thread["title"] = json!("Renamed");
    cache
        .upsert(&thread, Some(&json!([{"text":"Ship","status":"pending"}])))
        .unwrap();
    capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap();
    let batches = queued(&queue);
    assert_eq!(batches[0]["events"], json!([]));
    assert_eq!(batches[0]["delivery"]["reset"], false);
    drain_locally(&queue);
    cache
        .append(&id, &[event("one", &"😀".repeat(400_000))])
        .unwrap();
    capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap();
    assert_eq!(queued(&queue)[0]["delivery"]["reset"], true);
    drain_locally(&queue);
    cache.append(&id, &[event("one", "small again")]).unwrap();
    cache
        .db
        .execute(
            "DELETE FROM events WHERE thread_id=? AND event_id='two'",
            [&id],
        )
        .unwrap();
    capture_changes(&cache, &queue, &fixture.bridge, &stop).unwrap();
    let batches = queued(&queue);
    assert_eq!(batches[0]["delivery"]["reset"], true);
    assert_eq!(batches[0]["events"].as_array().unwrap().len(), 1);
    assert_eq!(batches[0]["events"][0]["id"], "one");
}

#[test]
fn old_js_queue_identity_payload_bytes_and_uncaptured_cache_survive_cutover() {
    let fixture = Fixture::new(&[message("New source snapshot")]);
    fs::create_dir(&fixture.data).unwrap();
    let id = cached_thread()["id"].as_str().unwrap().to_owned();
    let pending_id = "11111111-2222-4333-8444-555555555555";
    let pending = format!(
        "{{\"thread\":{{\"id\":\"123\",\"botId\":\"test-bot\",\"botName\":\"Test Bot\",\"title\":\"Task\",\"guildId\":null,\"channelId\":\"123\",\"status\":\"working\",\"updatedAt\":100}},\"todos\":[],\"events\":[{{\"id\":\"old\",\"timestamp\":0,\"role\":\"assistant\",\"kind\":\"message\",\"text\":\"Already queued\"}}],\"delivery\":{{\"id\":\"{pending_id}\",\"reset\":true}}}}"
    );
    let old = Connection::open(fixture.data.join("collector-queue.sqlite")).unwrap();
    old.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE identity (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
        CREATE TABLE deliveries (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, payload TEXT NOT NULL);
        CREATE TABLE captured_threads (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, generation INTEGER NOT NULL, metadata TEXT NOT NULL);
        CREATE TABLE captured_events (thread_id TEXT NOT NULL, event_id TEXT NOT NULL, hash TEXT NOT NULL, parts INTEGER NOT NULL, PRIMARY KEY(thread_id,event_id));").unwrap();
    old.execute(
        "INSERT INTO identity VALUES(1,?)",
        [json!([
            fixture.bridge.id,
            fixture.bridge.database_path,
            fixture.bridge.session_dir
        ])
        .to_string()],
    )
    .unwrap();
    old.execute(
        "INSERT INTO deliveries(id,payload) VALUES(?,?)",
        params![pending_id, pending],
    )
    .unwrap();
    old.execute(
        "INSERT INTO captured_threads VALUES(?,1,0,'old-js-metadata-hash')",
        [&id],
    )
    .unwrap();
    old.execute(
        "INSERT INTO captured_events VALUES(?,'old','old-js-event-hash',1)",
        [&id],
    )
    .unwrap();
    drop(old);
    let old_cache = Connection::open(fixture.data.join("threadwatch.sqlite")).unwrap();
    old_cache.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, source TEXT NOT NULL, payload TEXT NOT NULL, todos TEXT NOT NULL DEFAULT '[]', revision INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE events (thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, seq INTEGER NOT NULL, event_id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(thread_id,event_id), UNIQUE(thread_id,seq));
        CREATE TABLE imports (thread_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
        CREATE TABLE meta (key TEXT PRIMARY KEY,value INTEGER NOT NULL);
        INSERT INTO meta VALUES('revision',2);
        CREATE TABLE collector_resets (thread_id TEXT PRIMARY KEY,generation INTEGER NOT NULL);").unwrap();
    old_cache
        .execute(
            "INSERT INTO threads VALUES(?,'bridge:test-bot',?,'[]',2)",
            params![id, cached_thread().to_string()],
        )
        .unwrap();
    old_cache
        .execute(
            "INSERT INTO events VALUES(?,1,'old',?)",
            params![id, event("old", "Already queued").to_string()],
        )
        .unwrap();
    old_cache
        .execute(
            "INSERT INTO events VALUES(?,2,'uncaptured',?)",
            params![
                id,
                event("uncaptured", "Committed before interruption").to_string()
            ],
        )
        .unwrap();
    old_cache.execute("INSERT INTO imports VALUES(?,?)",params![id,json!({"version":2,"offset":30,"line":2,"pending":{},"path":fixture.transcript,"identity":"1:2","size":30,"mtime":123,"ctime":123}).to_string()]).unwrap();
    drop(old_cache);
    let queue = fixture.queue();
    assert_eq!(
        queue.peek().unwrap(),
        Some((pending_id.into(), pending.clone()))
    );
    let saved: String = queue
        .db
        .query_row("SELECT hash FROM captured_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(saved, "old-js-event-hash");
    let mut reader = fixture.reader();
    let stop = Cancellation::default();
    capture_changes(&reader.cache, &queue, &fixture.bridge, &stop).unwrap();
    assert_eq!(reader.refresh()["status"], "ready");
    capture_changes(&reader.cache, &queue, &fixture.bridge, &stop).unwrap();
    let batches = queued(&queue);
    assert_eq!(batches.len(), 3);
    assert_eq!(
        batches[1]["events"][1]["text"],
        "Committed before interruption"
    );
    assert_eq!(batches[2]["events"][0]["text"], "New source snapshot");
    assert!(
        batches
            .iter()
            .all(|batch| batch["delivery"]["reset"] == true)
    );
    assert_eq!(
        queue.peek().unwrap(),
        Some((pending_id.into(), pending.clone()))
    );
    drop(queue);
    assert_eq!(
        fixture.queue().peek().unwrap(),
        Some((pending_id.into(), pending))
    );
}

#[test]
fn bridge_confines_transcripts_and_keeps_saved_titles_with_provenance() {
    let fixture = Fixture::new(&[
        json!({"type":"title","title":""}),
        json!({"type":"message","message":{"role":"user","content":"<system-reminder>hidden</system-reminder> Inspect bot integration"}}),
    ]);
    let mut reader = fixture.reader();
    assert_eq!(reader.refresh()["status"], "ready");
    let id = cached_thread()["id"].as_str().unwrap().to_owned();
    assert_eq!(
        reader.cache.thread(&id).unwrap().unwrap()["titleSource"],
        "user_message"
    );
    fixture.append(&(json!({"type":"title","title":"Saved title"}).to_string() + "\n"));
    reader.refresh();
    let mut thread = reader.cache.thread(&id).unwrap().unwrap();
    assert_eq!(thread["title"], "Saved title");
    thread["title"] = json!("Discord title");
    thread["titleSource"] = json!("discord");
    reader.cache.upsert(&thread, None).unwrap();
    fixture
        .append(&(json!({"type":"title","title":"Do not overwrite Discord"}).to_string() + "\n"));
    reader.refresh();
    assert_eq!(
        reader.cache.thread(&id).unwrap().unwrap()["title"],
        "Discord title"
    );
    let outside = fixture._root.path().join("outside.jsonl");
    fs::write(&outside, message("private-outside").to_string() + "\n").unwrap();
    assert!(
        reader
            .import_session(&mut thread, outside.to_str().unwrap())
            .is_err()
    );
    assert!(
        !serde_json::to_string(&cached_events(&reader.cache))
            .unwrap()
            .contains("private-outside")
    );
}

#[tokio::test]
async fn once_outage_retains_queue_and_closes_lock_cleanly() {
    let fixture = Fixture::new(&[message("Keep queued")]);
    assert!(
        threadwatch::collector::run(fixture.config(), true)
            .await
            .is_err()
    );
    assert_eq!(fixture.queue().count().unwrap(), 1);
    assert!(acquire_state_lock(&fixture.data).is_ok());
}

struct Proxy {
    upstream: String,
    client: reqwest::Client,
    mode: std::sync::atomic::AtomicU8,
    started: std::sync::atomic::AtomicBool,
    pulses_during_delivery: std::sync::atomic::AtomicUsize,
    bodies: std::sync::Mutex<Vec<String>>,
}

async fn proxy_request(
    axum::extract::State(proxy): axum::extract::State<Arc<Proxy>>,
    uri: axum::http::Uri,
    body: String,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    use std::sync::atomic::Ordering;
    let ingest = uri.path() == "/api/ingest";
    let mode = proxy.mode.load(Ordering::Acquire);
    if ingest {
        proxy.bodies.lock().unwrap().push(body.clone());
        if mode == 0 {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let first = !proxy.started.swap(true, Ordering::AcqRel);
        if mode == 3 && first {
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
    } else if proxy.started.load(Ordering::Acquire) {
        proxy.pulses_during_delivery.fetch_add(1, Ordering::AcqRel);
    }
    let response = proxy
        .client
        .post(format!("{}{}", proxy.upstream, uri.path()))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    // The real receiver committed the delivery, but its ACK is lost in transit.
    if ingest && mode == 1 {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response()
}

async fn network_receiver(
    fixture: &Fixture,
) -> (
    String,
    Arc<threadwatch::server::AppState>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(
        threadwatch::server::AppState::new(threadwatch::server::Config {
            local: true,
            lan: false,
            host: "127.0.0.1".into(),
            allowed_hosts: Vec::new(),
            port: address.port(),
            ingest_token: TOKEN.into(),
            data_dir: fixture._root.path().join("server"),
        })
        .unwrap(),
    );
    let router = threadwatch::server::router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{address}"), state, handle)
}

async fn network_proxy(
    upstream: String,
    mode: u8,
) -> (String, Arc<Proxy>, tokio::task::JoinHandle<()>) {
    let proxy = Arc::new(Proxy {
        upstream,
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        mode: std::sync::atomic::AtomicU8::new(mode),
        started: std::sync::atomic::AtomicBool::new(false),
        pulses_during_delivery: std::sync::atomic::AtomicUsize::new(0),
        bodies: std::sync::Mutex::new(Vec::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/api/ingest", axum::routing::post(proxy_request))
        .route("/api/heartbeat", axum::routing::post(proxy_request))
        .with_state(proxy.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), proxy, handle)
}

async fn collect_snapshot(fixture: &Fixture) {
    let directory = fixture.data.clone();
    let bridge = fixture.bridge.clone();
    tokio::task::spawn_blocking(move || {
        let cache = Cache::open(&directory, Arc::new(Sanitizer::new(vec![TOKEN.into()]))).unwrap();
        let queue = DeliveryQueue::open(&directory, &bridge).unwrap();
        let cancellation = Arc::new(Cancellation::default());
        capture_changes(&cache, &queue, &bridge, &cancellation).unwrap();
        let mut reader = BridgeReader::new(cache, bridge.clone(), cancellation.clone()).unwrap();
        assert_eq!(reader.refresh()["status"], "ready");
        capture_changes(&reader.cache, &queue, &bridge, &cancellation).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_receiver_recovers_outage_lost_ack_restart_and_incremental_append() {
    use std::sync::atomic::Ordering;
    use threadwatch::collector::{Transport, flush};
    let fixture = Fixture::new(&[message("Initial saved event")]);
    let (upstream, state, server) = network_receiver(&fixture).await;
    let (url, proxy, forwarder) = network_proxy(upstream, 0).await;
    collect_snapshot(&fixture).await;
    let mut config = fixture.config();
    config.server_url = url;
    config.request_timeout = Duration::from_secs(2);
    let transport = Transport::new(&config).unwrap();
    let mut queue = fixture.queue();
    let original = queue.peek().unwrap().unwrap();
    let cancellation = Cancellation::default();
    assert!(flush(&mut queue, &transport, &cancellation).await.is_err());
    assert_eq!(queue.peek().unwrap().unwrap(), original);
    assert!(
        state.index().unwrap()["threads"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    proxy.mode.store(1, Ordering::Release);
    assert!(flush(&mut queue, &transport, &cancellation).await.is_err());
    assert_eq!(queue.peek().unwrap().unwrap(), original);
    let remote_id = thread_id("ingest", &fixture.bridge.id, "123");
    let before = state.detail(&remote_id, None, 100).unwrap().unwrap();
    let version = before["thread"]["historyVersion"].clone();
    assert_eq!(before["events"].as_array().unwrap().len(), 1);
    drop(queue);
    fixture.append(&(message("Appended while disconnected").to_string() + "\n"));
    collect_snapshot(&fixture).await;
    let mut queue = fixture.queue();
    assert_eq!(queue.peek().unwrap().unwrap(), original);
    assert_eq!(queue.count().unwrap(), 2);
    proxy.mode.store(2, Ordering::Release);
    flush(&mut queue, &transport, &cancellation).await.unwrap();
    assert_eq!(queue.count().unwrap(), 0);
    let after = state.detail(&remote_id, None, 100).unwrap().unwrap();
    assert_eq!(after["thread"]["historyVersion"], version);
    assert_eq!(
        after["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Initial saved event", "Appended while disconnected"]
    );
    let bodies = proxy.bodies.lock().unwrap();
    assert_eq!(bodies[0], original.1);
    assert_eq!(bodies[1], original.1);
    assert_eq!(bodies[2], original.1);
    drop(bodies);
    fs::write(
        &fixture.transcript,
        message("Replacement").to_string() + "\n",
    )
    .unwrap();
    collect_snapshot(&fixture).await;
    assert_eq!(queued(&queue)[0]["delivery"]["reset"], true);
    flush(&mut queue, &transport, &cancellation).await.unwrap();
    let replaced = state.detail(&remote_id, None, 100).unwrap().unwrap();
    assert_eq!(replaced["events"].as_array().unwrap().len(), 1);
    assert_eq!(replaced["events"][0]["text"], "Replacement");
    assert_ne!(replaced["thread"]["historyVersion"], version);
    forwarder.abort();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_backfill_keeps_heartbeats_running_while_acknowledgement_waits() {
    use std::sync::atomic::Ordering;
    let fixture = Fixture::new(
        &(0..1001)
            .map(|index| message(&format!("Saved {index}")))
            .collect::<Vec<_>>(),
    );
    let (upstream, state, server) = network_receiver(&fixture).await;
    let (url, proxy, forwarder) = network_proxy(upstream, 3).await;
    let mut config = fixture.config();
    config.server_url = url;
    config.request_timeout = Duration::from_secs(2);
    threadwatch::collector::run(config, true).await.unwrap();
    assert!(proxy.pulses_during_delivery.load(Ordering::Acquire) >= 2);
    assert_eq!(fixture.queue().count().unwrap(), 0);
    let remote_id = thread_id("ingest", &fixture.bridge.id, "123");
    let mut cursor = None;
    let mut texts = std::collections::HashSet::new();
    loop {
        let detail = state
            .detail(&remote_id, cursor.as_deref(), 500)
            .unwrap()
            .unwrap();
        for event in detail["events"].as_array().unwrap() {
            texts.insert(event["text"].as_str().unwrap().to_owned());
        }
        cursor = detail["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    for index in 0..1001 {
        assert!(texts.contains(&format!("Saved {index}")));
    }
    assert!(
        state.index().unwrap()["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["id"] == "ingest:test-bot" && source["status"] == "ready")
    );
    forwarder.abort();
    server.abort();
}

#[tokio::test]
async fn transport_never_follows_redirects_and_requests_are_timeout_and_cancel_bounded() {
    use axum::{
        http::{StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use threadwatch::collector::Transport;
    let fixture = Fixture::new(&[]);
    let forwarded = Arc::new(AtomicUsize::new(0));
    let count = forwarded.clone();
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = format!("http://{}/api/heartbeat", destination.local_addr().unwrap());
    let destination_router = axum::Router::new().route(
        "/api/heartbeat",
        post(move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::AcqRel);
                axum::Json(json!({}))
            }
        }),
    );
    let destination_task = tokio::spawn(async move {
        axum::serve(destination, destination_router).await.unwrap();
    });
    let redirect = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect_url = format!("http://{}", redirect.local_addr().unwrap());
    let redirect_router = axum::Router::new().route(
        "/api/heartbeat",
        post(move || {
            let location = location.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, location)],
                )
                    .into_response()
            }
        }),
    );
    let redirect_task = tokio::spawn(async move {
        axum::serve(redirect, redirect_router).await.unwrap();
    });
    let mut config = fixture.config();
    config.server_url = redirect_url;
    let cancellation = Cancellation::default();
    let transport = Transport::new(&config).unwrap();
    assert!(
        transport
            .post("/api/heartbeat", "{}".into(), &cancellation)
            .await
            .is_err()
    );
    assert_eq!(forwarded.load(Ordering::Acquire), 0);
    assert!(
        transport
            .post(
                "http://elsewhere.invalid/api/ingest",
                "{}".into(),
                &cancellation
            )
            .await
            .is_err()
    );
    let hanging = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.server_url = format!("http://{}", hanging.local_addr().unwrap());
    config.request_timeout = Duration::from_millis(50);
    let hanging_router = axum::Router::new().route(
        "/api/heartbeat",
        post(|| async {
            std::future::pending::<()>().await;
            StatusCode::OK
        }),
    );
    let hanging_task = tokio::spawn(async move {
        axum::serve(hanging, hanging_router).await.unwrap();
    });
    let transport = Transport::new(&config).unwrap();
    assert!(
        transport
            .post("/api/heartbeat", "{}".into(), &cancellation)
            .await
            .is_err()
    );
    config.request_timeout = Duration::from_secs(10);
    let transport = Transport::new(&config).unwrap();
    let cancellation = Arc::new(Cancellation::default());
    let trigger = cancellation.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        trigger.cancel();
    });
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        transport.post("/api/heartbeat", "{}".into(), &cancellation),
    )
    .await
    .unwrap();
    assert!(response.is_err());
    cancel_task.await.unwrap();
    hanging_task.abort();
    redirect_task.abort();
    destination_task.abort();
}
