use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use threadwatch::{
    sanitize::Sanitizer,
    server::{self, AppState, Config, MAX_BODY},
};
use tower::ServiceExt;

const TOKEN: &str = "protocol-shared-ingestion-credential-123456789";
fn config(path: &std::path::Path) -> Config {
    Config {
        local: true,
        lan: false,
        host: "127.0.0.1".into(),
        port: 8787,
        ingest_token: TOKEN.into(),
        data_dir: path.to_owned(),
    }
}
fn batch() -> Value {
    json!({"thread":{"id":"thread", "botId":"bot", "botName":"Collector", "title":"Task", "status":"working", "updatedAt":1000}, "events":[{"id":"event", "timestamp":0, "kind":"message", "role":"assistant", "text":"Hello"}], "todos":[{"text":"Current task", "status":"in_progress"}]})
}
fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:8787")
        .body(body)
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
    ));
    request
}
fn post(uri: &str, value: &Value, token: Option<&str>) -> Request<Body> {
    let mut request = request("POST", uri, Body::from(value.to_string()));
    request
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    request
}
async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BODY * 2).await.unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}
fn environment(values: &[(&str, &str)]) -> anyhow::Result<Config> {
    let map: HashMap<&str, &str> = values.iter().copied().collect();
    Config::from_values(|key| {
        map.get(key)
            .map(|value| (*value).to_owned())
            .or_else(|| (key == "HOME").then(|| "/tmp/threadwatch-test-home".to_owned()))
    })
}

#[test]
fn configuration_fails_closed_and_keeps_safe_loopback_default() {
    assert!(environment(&[]).is_err());
    let config = environment(&[("TW_LOCAL", "1")]).unwrap();
    assert_eq!(config.host, "127.0.0.1");
    assert_eq!(config.port, 8787);
    for host in [
        "0.0.0.0",
        "::",
        "8.8.8.8",
        "192.168.1.115",
        "attacker.example",
    ] {
        assert!(environment(&[("TW_LOCAL", "1"), ("TW_HOST", host)]).is_err());
    }
    for host in ["0.0.0.0", "8.8.8.8", "127.0.0.1", "192.168.999.1", "::1"] {
        assert!(environment(&[("TW_LOCAL", "1"), ("TW_LAN", "1"), ("TW_HOST", host)]).is_err());
    }
    for host in ["10.1.2.3", "172.16.0.1", "172.31.255.254", "192.168.1.115"] {
        assert!(environment(&[("TW_LOCAL", "1"), ("TW_LAN", "1"), ("TW_HOST", host)]).is_ok());
    }
    assert!(environment(&[("TW_LAN", "1"), ("TW_HOST", "192.168.1.115")]).is_err());
    for port in ["0", "65536", "-1", "1.5", "abc", "+8787"] {
        assert!(environment(&[("TW_LOCAL", "1"), ("TW_PORT", port)]).is_err());
    }
    assert!(environment(&[("TW_LOCAL", "1"), ("TW_INGEST_TOKEN", "")]).is_err());
    assert!(environment(&[("TW_LOCAL", "1"), ("TW_INGEST_TOKEN", "weak")]).is_err());
}

#[test]
fn token_files_are_private_trimmed_and_mutually_exclusive_without_path_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("private-token");
    std::fs::write(&file, format!("  {TOKEN}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let path = file.to_str().unwrap();
    assert_eq!(
        environment(&[("TW_LOCAL", "1"), ("TW_INGEST_TOKEN_FILE", path)])
            .unwrap()
            .ingest_token,
        TOKEN
    );
    for token in [TOKEN, ""] {
        assert!(
            environment(&[
                ("TW_LOCAL", "1"),
                ("TW_INGEST_TOKEN_FILE", path),
                ("TW_INGEST_TOKEN", token)
            ])
            .err()
            .unwrap()
            .to_string()
            .contains("mutually exclusive")
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o640, 0o604, 0o644, 0o601] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(environment(&[("TW_LOCAL", "1"), ("TW_INGEST_TOKEN_FILE", path)]).is_err());
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    for token in ["", "  \n", "short"] {
        std::fs::write(&file, token).unwrap();
        assert!(environment(&[("TW_LOCAL", "1"), ("TW_INGEST_TOKEN_FILE", path)]).is_err());
    }
    let missing = dir.path().join("private-machine-name/missing-token");
    let error = environment(&[
        ("TW_LOCAL", "1"),
        ("TW_INGEST_TOKEN_FILE", missing.to_str().unwrap()),
    ])
    .err()
    .unwrap()
    .to_string();
    assert!(!error.contains("private-machine-name"));
    assert!(error.contains("private regular file"));
    assert!(
        environment(&[
            ("TW_LOCAL", "1"),
            ("TW_INGEST_TOKEN_FILE", dir.path().to_str().unwrap())
        ])
        .is_err()
    );
}

#[tokio::test]
async fn every_route_checks_actual_peer_host_origin_and_cross_site_requests() {
    let dir = tempfile::tempdir().unwrap();
    let app = server::router(Arc::new(AppState::new(config(dir.path())).unwrap()));
    assert_eq!(
        send(&app, request("GET", "/api/config", Body::empty())).await,
        (StatusCode::OK, json!({"local":true}))
    );
    for route in ["/api/config", "/api/threads", "/", "/static/app.js"] {
        for (header, value) in [
            ("host", "attacker.example"),
            ("origin", "https://attacker.example"),
            ("sec-fetch-site", "cross-site"),
        ] {
            let mut req = request("GET", route, Body::empty());
            req.headers_mut().insert(header, value.parse().unwrap());
            assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
        }
        let mut req = request("GET", route, Body::empty());
        req.extensions_mut()
            .insert(ConnectInfo("8.8.8.8:40000".parse::<SocketAddr>().unwrap()));
        req.headers_mut()
            .insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
    }
    let mut req = request("GET", "/api/config", Body::empty());
    req.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
    assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
    let mut req = request("GET", "/api/config", Body::empty());
    req.headers_mut()
        .insert("origin", "http://127.0.0.1:8787".parse().unwrap());
    req.extensions_mut().insert(ConnectInfo(
        "[::ffff:127.0.0.1]:40000".parse::<SocketAddr>().unwrap(),
    ));
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
    let mut cfg = config(dir.path());
    cfg.lan = true;
    cfg.host = "192.168.1.115".into();
    let app = server::router(Arc::new(AppState::new(cfg).unwrap()));
    for (peer, expected) in [
        ("192.168.1.2:40000", StatusCode::OK),
        ("127.0.0.1:40000", StatusCode::OK),
        ("10.0.0.2:40000", StatusCode::OK),
        ("8.8.8.8:40000", StatusCode::FORBIDDEN),
        ("[2001:db8::1]:40000", StatusCode::FORBIDDEN),
    ] {
        let mut req = request("GET", "/api/config", Body::empty());
        req.headers_mut()
            .insert("host", "192.168.1.115:8787".parse().unwrap());
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        assert_eq!(send(&app, req).await.0, expected);
    }
    assert_eq!(
        send(&app, request("GET", "/api/config", Body::empty()))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn browser_urls_are_relative_for_path_prefix_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let app = server::router(Arc::new(AppState::new(config(dir.path())).unwrap()));
    let (status, body) = send(&app, request("GET", "/", Body::empty())).await;
    assert_eq!(status, StatusCode::OK);
    let html = body.as_str().unwrap();
    for expected in [
        "href=\"static/style.css\"",
        "src=\"static/app.js\"",
        "hx-get=\"ui/index\"",
    ] {
        assert!(html.contains(expected));
    }
    for forbidden in ["href=\"/static/", "src=\"/static/", "hx-get=\"/ui/"] {
        assert!(!html.contains(forbidden));
    }
}

#[tokio::test]
async fn ingestion_authentication_body_limits_and_sanitization_precede_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let app = server::router(Arc::new(AppState::new(config(dir.path())).unwrap()));
    let heartbeat =
        json!({"botId":"bot", "botName":"Collector", "status":"ready", "pendingBatches":0});
    for (route, payload) in [("/api/ingest", batch()), ("/api/heartbeat", heartbeat)] {
        for token in [None, Some("wrong")] {
            assert_eq!(
                send(&app, post(route, &payload, token)).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    let mut input = batch();
    input["thread"]["title"] = json!(format!("Title {TOKEN}"));
    input["events"][0]["text"] = json!(format!("secret={TOKEN}"));
    input["events"][0]["providerPayload"] = json!(TOKEN);
    input["todos"][0]["text"] = json!(TOKEN);
    let (status, result) = send(&app, post("/api/ingest", &input, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    let id = result["id"].as_str().unwrap();
    let (status, detail) = send(
        &app,
        request("GET", &format!("/api/threads/{id}?limit=1"), Body::empty()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!detail.to_string().contains(TOKEN));
    assert!(detail["events"][0].get("providerPayload").is_none());
    let db = rusqlite::Connection::open(dir.path().join("threadwatch.sqlite")).unwrap();
    for table in ["threads", "events"] {
        let payload: String = db
            .query_row(&format!("SELECT payload FROM {table} LIMIT 1"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!payload.contains(TOKEN));
    }
    let mut bad_type = post("/api/ingest", &batch(), Some(TOKEN));
    bad_type
        .headers_mut()
        .insert("content-type", "text/plain".parse().unwrap());
    assert_eq!(
        send(&app, bad_type).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let oversized = json!({"text":"x".repeat(MAX_BODY)});
    assert_eq!(
        send(&app, post("/api/ingest", &oversized, Some(TOKEN)))
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for invalid in [Value::Null, json!([])] {
        assert_eq!(
            send(&app, post("/api/ingest", &invalid, Some(TOKEN)))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    for query in [
        "limit=0",
        "limit=501",
        "limit=1.5",
        "limit=%2B1",
        "before=garbage",
    ] {
        assert_eq!(
            send(
                &app,
                request("GET", &format!("/api/threads/{id}?{query}"), Body::empty())
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let response = app
        .oneshot(request("GET", "/api/config", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(
        !response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("discord")
    );
}

#[test]
fn validation_preserves_unknown_timestamps_and_rejects_unsafe_fields() {
    let sanitizer = Sanitizer::new(vec![TOKEN.to_owned()]);
    let mut input = batch();
    for (raw, normalized) in [
        (Value::Null, Value::Null),
        (json!(0), json!(0)),
        (json!(1.0), json!(1)),
        (json!("1970-01-01T00:00:01.000Z"), json!(1000)),
        (json!("1970-01-01"), json!(0)),
    ] {
        input["events"][0]["timestamp"] = raw;
        assert_eq!(
            server::validate_ingest(&input, &sanitizer).unwrap()["events"][0]["timestamp"],
            normalized
        );
    }
    for value in [
        json!(-1),
        json!(0.1),
        json!(9_007_199_254_740_992u64),
        json!("1969-12-31T23:59:59Z"),
        json!("nonsense"),
        json!(true),
    ] {
        input["events"][0]["timestamp"] = value;
        assert!(server::validate_ingest(&input, &sanitizer).is_err());
    }
    for (path, value) in [
        ("/thread/botId", json!(TOKEN)),
        ("/thread/id", json!("")),
        ("/thread/status", json!("active")),
        ("/thread/updatedAt", json!(-1)),
        ("/thread/updatedAt", Value::Null),
        ("/thread/titleSource", json!("private-path")),
        ("/events/0/role", json!("system")),
        ("/events/0/isError", json!("true")),
        ("/events/0/kind", json!("thinking")),
        ("/todos/0/status", json!("failed")),
    ] {
        let mut invalid = batch();
        if path == "/thread/titleSource" {
            invalid["thread"]["titleSource"] = value;
        } else if path == "/events/0/isError" {
            invalid["events"][0]["isError"] = value;
        } else {
            *invalid.pointer_mut(path).unwrap() = value;
        }
        assert!(
            server::validate_ingest(&invalid, &sanitizer).is_err(),
            "{path}"
        );
    }
    for delivery in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"id":"", "reset":false}),
        json!({"id":TOKEN, "reset":true}),
        json!({"id":"ok", "reset":"true"}),
    ] {
        let mut invalid = batch();
        invalid["delivery"] = delivery;
        assert!(server::validate_ingest(&invalid, &sanitizer).is_err());
    }
    let mut valid = batch();
    valid["thread"]["historyVersion"] = json!("untrusted");
    valid["thread"]["titleSource"] = json!("user_message");
    valid["events"][0]["kind"] = json!("tool_start");
    let normalized = server::validate_ingest(&valid, &sanitizer).unwrap();
    assert!(normalized["thread"].get("historyVersion").is_none());
    assert_eq!(normalized["thread"]["titleSource"], "user_message");
    let mut duplicate = batch();
    duplicate["events"] = json!([duplicate["events"][0], duplicate["events"][0]]);
    assert!(server::validate_ingest(&duplicate, &sanitizer).is_err());
    let mut too_many = batch();
    too_many["events"] = json!(vec![too_many["events"][0].clone(); 1001]);
    assert!(server::validate_ingest(&too_many, &sanitizer).is_err());
    let mut too_many = batch();
    too_many["todos"] = json!(vec![too_many["todos"][0].clone(); 2001]);
    assert!(server::validate_ingest(&too_many, &sanitizer).is_err());
}

#[tokio::test]
async fn heartbeat_survives_restart_and_never_persists_remote_error_details() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let app = server::router(Arc::new(AppState::new(cfg.clone()).unwrap()));
    let input = json!({"botId":"bot", "botName":format!("Collector {TOKEN}"), "status":"degraded", "pendingBatches":7, "error":format!("Cannot read /private/machine using {TOKEN}")});
    assert_eq!(
        send(&app, post("/api/heartbeat", &input, Some(TOKEN)))
            .await
            .0,
        StatusCode::OK
    );
    let (_, before) = send(&app, request("GET", "/api/threads", Body::empty())).await;
    assert_eq!(before["sources"][0]["status"], "degraded");
    assert_eq!(before["sources"][0]["pendingBatches"], 7);
    assert_eq!(before["threads"], json!([]));
    assert!(!before.to_string().contains(TOKEN));
    assert!(!before.to_string().contains("/private/machine"));
    drop(app);
    let state = AppState::new(cfg).unwrap();
    assert_eq!(state.index().unwrap(), before);
    let received = before["sources"][0]["lastSeenAt"].as_i64().unwrap();
    let expired = state.index_at(received + 45_000).unwrap();
    assert_eq!(expired["sources"][0]["status"], "offline");
    assert_eq!(expired["sources"][0]["lastSeenAt"], received);
    assert_ne!(expired["revision"], before["revision"]);
    let sanitizer = Sanitizer::new(vec![TOKEN.into()]);
    for (field, value) in [
        ("botId", json!(TOKEN)),
        ("botName", Value::Null),
        ("status", json!("working")),
        ("pendingBatches", json!(-1)),
        ("pendingBatches", json!(0.5)),
        ("pendingBatches", json!("0")),
        ("pendingBatches", json!(9_007_199_254_740_992u64)),
        ("error", json!({"private":"path"})),
    ] {
        let mut invalid = input.clone();
        invalid[field] = value;
        assert!(server::validate_heartbeat(&invalid, &sanitizer).is_err());
    }
}
