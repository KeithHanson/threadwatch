use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use threadwatch::{
    server::{AppState, Config},
    store::{Store, thread_id},
};

fn thread(bot: &str, external: &str, status: &str) -> Value {
    json!({"id": thread_id("ingest", bot, external), "botId": bot, "botName": "Collector", "title": "Task", "status": status, "updatedAt": 1000, "source": format!("ingest:{bot}")})
}
fn event(id: &str) -> Value {
    json!({"id": id, "timestamp": 0, "kind": "message", "role": "assistant", "text": id})
}
fn config(path: &std::path::Path) -> Config {
    Config {
        local: true,
        lan: false,
        host: "127.0.0.1".into(),
        allowed_hosts: Vec::new(),
        port: 8787,
        ingest_token: "migration-test-shared-secret-long-enough".into(),
        data_dir: path.to_owned(),
    }
}

#[test]
fn migrates_original_schema_without_losing_history_receipts_or_heartbeats() {
    let directory = tempfile::tempdir().unwrap();
    let db = Connection::open(directory.path().join("threadwatch.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, source TEXT NOT NULL, payload TEXT NOT NULL, todos TEXT NOT NULL DEFAULT '[]', revision INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE events (thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, seq INTEGER NOT NULL, event_id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(thread_id,event_id), UNIQUE(thread_id,seq));
        CREATE TABLE imports (thread_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
        CREATE TABLE deliveries (bot_id TEXT NOT NULL, delivery_id TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY(bot_id,delivery_id));
        CREATE TABLE heartbeats (bot_id TEXT PRIMARY KEY, payload TEXT NOT NULL);
        CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
        INSERT INTO meta VALUES('revision',7);").unwrap();
    let mut saved = thread("bot", "thread", "working");
    saved["historyVersion"] = json!("reset-before-port");
    let id = saved["id"].as_str().unwrap();
    let todos = json!([{"text":"Retained todo", "status":"in_progress"}]);
    db.execute(
        "INSERT INTO threads VALUES(?,?,?,?,?)",
        params![id, "ingest:bot", saved.to_string(), todos.to_string(), 7],
    )
    .unwrap();
    db.execute(
        "INSERT INTO events VALUES(?,?,?,?)",
        params![id, 42, "old", event("old").to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO deliveries VALUES(?,?,?)",
        params!["bot", "reset-before-port", id],
    )
    .unwrap();
    db.execute(
        "INSERT INTO imports VALUES(?,?)",
        params!["legacy-checkpoint", "{\"offset\":123}"],
    )
    .unwrap();
    let heartbeat = json!({"id":"ingest:bot","name":"Collector","status":"ready","lastSeenAt":1000,"pendingBatches":3});
    db.execute(
        "INSERT INTO heartbeats VALUES(?,?)",
        params!["bot", heartbeat.to_string()],
    )
    .unwrap();
    let mut hidden = thread("bot", "old-direct-import", "working");
    hidden["id"] = json!(thread_id("bridge", "bot", "old-direct-import"));
    hidden["source"] = json!("bridge:bot");
    db.execute(
        "INSERT INTO threads VALUES(?,?,?,?,?)",
        params![
            hidden["id"].as_str(),
            "bridge:bot",
            hidden.to_string(),
            "[]",
            1
        ],
    )
    .unwrap();
    drop(db);
    let mut store = Store::open(directory.path()).unwrap();
    assert_eq!(store.revision().unwrap(), "7");
    assert_eq!(store.list().unwrap(), vec![saved.clone()]);
    let detail = store.detail(id, None, 100).unwrap().unwrap();
    assert_eq!(detail["thread"], saved);
    assert_eq!(detail["todos"], todos);
    assert_eq!(detail["events"], json!([event("old")]));
    assert_eq!(store.collector_sources(45_999).unwrap(), vec![heartbeat]);
    assert_eq!(
        store.collector_sources(46_000).unwrap()[0]["status"],
        "offline"
    );
    assert!(
        store
            .detail(hidden["id"].as_str().unwrap(), None, 100)
            .unwrap()
            .is_none()
    );
    store
        .ingest(
            thread("bot", "thread", "completed"),
            &[event("replacement")],
            Some(&json!([])),
            Some(&json!({"id":"reset-before-port","reset":true})),
        )
        .unwrap();
    assert_eq!(store.revision().unwrap(), "7");
    assert_eq!(store.detail(id, None, 100).unwrap().unwrap(), detail);
    let db = Connection::open(directory.path().join("threadwatch.sqlite")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT payload FROM imports WHERE thread_id='legacy-checkpoint'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "{\"offset\":123}"
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM threads WHERE source='bridge:bot'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn reset_deduplicates_across_restart_without_erasing_later_data() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    let initial = thread("bot", "thread", "working");
    let id = initial["id"].as_str().unwrap();
    store
        .ingest(
            initial.clone(),
            &[event("old")],
            Some(&json!([{"text":"Old", "status":"pending"}])),
            None,
        )
        .unwrap();
    let delivery = json!({"id":"reset-one","reset":true});
    store
        .ingest(
            initial.clone(),
            &[event("replacement")],
            None,
            Some(&delivery),
        )
        .unwrap();
    assert_eq!(
        store.detail(id, None, 100).unwrap().unwrap()["todos"],
        json!([])
    );
    let mut later = thread("bot", "thread", "completed");
    later["title"] = json!("Latest title");
    let todos = json!([{"text":"Done", "status":"completed"}]);
    store
        .ingest(
            later,
            &[event("later")],
            Some(&todos),
            Some(&json!({"id":"later-delivery","reset":false})),
        )
        .unwrap();
    let revision = store.revision().unwrap();
    drop(store);
    let mut store = Store::open(directory.path()).unwrap();
    assert_eq!(
        store
            .ingest(
                initial.clone(),
                &[event("replacement")],
                Some(&json!([])),
                Some(&delivery)
            )
            .unwrap(),
        id
    );
    assert_eq!(store.revision().unwrap(), revision);
    let detail = store.detail(id, None, 100).unwrap().unwrap();
    assert_eq!(
        detail["events"],
        json!([event("replacement"), event("later")])
    );
    assert_eq!(detail["thread"]["historyVersion"], "reset-one");
    assert_eq!(detail["thread"]["title"], "Latest title");
    assert_eq!(detail["thread"]["status"], "completed");
    assert_eq!(detail["todos"], todos);
    store
        .ingest(initial.clone(), &[event("legacy")], None, None)
        .unwrap();
    assert_eq!(
        store.detail(id, None, 100).unwrap().unwrap()["thread"]["historyVersion"],
        "reset-one"
    );
    store
        .ingest(
            initial.clone(),
            &[event("new-history")],
            None,
            Some(&json!({"id":"reset-two","reset":true})),
        )
        .unwrap();
    store
        .ingest(initial, &[event("replacement")], None, Some(&delivery))
        .unwrap();
    let detail = store
        .detail(&thread_id("ingest", "bot", "thread"), None, 100)
        .unwrap()
        .unwrap();
    assert_eq!(detail["thread"]["historyVersion"], "reset-two");
    assert_eq!(detail["events"], json!([event("new-history")]));
}

#[test]
fn delivery_scope_is_bot_wide_and_reset_rolls_back_every_table_on_error() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    let first = thread("one", "first", "working");
    let receipt = json!({"id":"same-delivery","reset":true});
    let a = store
        .ingest(
            first.clone(),
            &[event("old")],
            Some(&json!([])),
            Some(&receipt),
        )
        .unwrap();
    let b = store
        .ingest(
            thread("two", "first", "working"),
            &[event("old")],
            None,
            Some(&receipt),
        )
        .unwrap();
    assert_ne!(a, b);
    let revision = store.revision().unwrap();
    assert_eq!(
        store
            .ingest(
                thread("one", "different", "failed"),
                &[event("erase")],
                None,
                Some(&receipt)
            )
            .unwrap(),
        a
    );
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(store.list().unwrap().len(), 2);
    let db = Connection::open(directory.path().join("threadwatch.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_event BEFORE INSERT ON events WHEN NEW.event_id='replacement' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let before = store.detail(&a, None, 100).unwrap();
    let reset = json!({"id":"failing-reset", "reset":true});
    assert!(
        store
            .ingest(
                first.clone(),
                &[event("replacement")],
                Some(&json!([])),
                Some(&reset)
            )
            .is_err()
    );
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(store.detail(&a, None, 100).unwrap(), before);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM deliveries WHERE delivery_id='failing-reset'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER reject_event").unwrap();
    store
        .ingest(
            first,
            &[event("replacement")],
            Some(&json!([])),
            Some(&reset),
        )
        .unwrap();
    assert_eq!(
        store.detail(&a, None, 100).unwrap().unwrap()["events"],
        json!([event("replacement")])
    );
}

#[test]
fn pagination_is_stable_scoped_and_updates_events_in_place() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    let thread = thread("bot", "thread", "working");
    let id = thread["id"].as_str().unwrap().to_owned();
    store
        .ingest(
            thread.clone(),
            &[event("one"), event("two"), event("three")],
            None,
            None,
        )
        .unwrap();
    let newest = store.detail(&id, None, 2).unwrap().unwrap();
    assert_eq!(newest["events"], json!([event("two"), event("three")]));
    let cursor = newest["nextCursor"].as_str().unwrap();
    let older = store.detail(&id, Some(cursor), 2).unwrap().unwrap();
    assert_eq!(older["events"], json!([event("one")]));
    assert!(older["nextCursor"].is_null());
    assert!(store.detail("another-thread", Some(cursor), 2).is_err());
    for invalid in [
        "garbage".to_owned(),
        URL_SAFE_NO_PAD.encode(json!([id, 0]).to_string()),
        URL_SAFE_NO_PAD.encode(json!([id, 1, "extra"]).to_string()),
    ] {
        assert!(store.detail(&id, Some(&invalid), 2).is_err());
    }
    let mut replacement = event("two");
    replacement["text"] = json!("Updated full result");
    store
        .ingest(thread.clone(), &[replacement.clone()], None, None)
        .unwrap();
    assert_eq!(
        store.detail(&id, None, 3).unwrap().unwrap()["events"],
        json!([event("one"), replacement, event("three")])
    );
    let revision = store.revision().unwrap();
    store.ingest(thread, &[replacement], None, None).unwrap();
    assert_eq!(store.revision().unwrap(), revision);
}

#[test]
fn freshness_changes_revisions_at_boundary_without_mutating_persisted_status() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path()).unwrap();
    let statuses = [
        "working",
        "queued",
        "waiting",
        "completed",
        "failed",
        "unknown",
        "stopped",
    ];
    for status in statuses {
        store
            .ingest(thread("bot", status, status), &[], None, None)
            .unwrap();
    }
    store
        .ingest(thread("legacy", "thread", "working"), &[], None, None)
        .unwrap();
    store
        .record_heartbeat(
            &json!({"botId":"bot", "botName":"Collector", "status":"ready", "pendingBatches":0}),
            1000,
        )
        .unwrap();
    store.record_heartbeat(&json!({"botId":"empty", "botName":"Empty collector", "status":"unavailable", "pendingBatches":2}), 1000).unwrap();
    let revision = store.revision().unwrap();
    let state = AppState::new(config(directory.path())).unwrap();
    let initial = state.index_at(1000).unwrap();
    assert_eq!(initial["sources"].as_array().unwrap().len(), 3);
    assert_eq!(
        state.index_at(45_999).unwrap()["revision"],
        initial["revision"]
    );
    let offline = state.index_at(46_000).unwrap();
    assert_ne!(offline["revision"], initial["revision"]);
    assert_eq!(store.revision().unwrap(), revision);
    for status in statuses {
        let id = thread_id("ingest", "bot", status);
        let before = state.detail_at(&id, None, 100, 1000).unwrap().unwrap();
        let after = state.detail_at(&id, None, 100, 46_000).unwrap().unwrap();
        assert_ne!(before["revision"], after["revision"]);
        if matches!(status, "working" | "queued" | "waiting") {
            assert_eq!(after["thread"]["status"], "unknown");
            assert_eq!(after["thread"]["reportedStatus"], status);
        } else {
            assert_eq!(after["thread"]["status"], status);
            assert!(after["thread"].get("reportedStatus").is_none());
        }
        assert_eq!(
            store.detail(&id, None, 100).unwrap().unwrap()["thread"]["status"],
            status
        );
    }
    assert_eq!(
        state
            .detail_at(&thread_id("ingest", "legacy", "thread"), None, 100, 46_000)
            .unwrap()
            .unwrap()["thread"]["status"],
        "working"
    );
    store
        .record_heartbeat(
            &json!({"botId":"bot", "botName":"Collector", "status":"degraded", "pendingBatches":7}),
            46_000,
        )
        .unwrap();
    let recovered = state
        .detail_at(&thread_id("ingest", "bot", "working"), None, 100, 46_000)
        .unwrap()
        .unwrap();
    assert_eq!(recovered["thread"]["status"], "working");
    assert!(recovered["thread"].get("reportedStatus").is_none());
}
