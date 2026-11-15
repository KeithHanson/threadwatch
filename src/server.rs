use crate::{
    sanitize::Sanitizer,
    store::{self, MAX_SAFE_INTEGER, STATUSES, Store},
};
use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

pub const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Config {
    pub local: bool,
    pub lan: bool,
    pub host: String,
    pub allowed_hosts: Vec<String>,
    pub port: u16,
    pub ingest_token: String,
    pub data_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_values(|key| std::env::var(key).ok())
    }

    /// Injectable environment lookup avoids process-global environment mutation in tests.
    pub fn from_values(env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let local = env("TW_LOCAL").as_deref() == Some("1");
        let lan = env("TW_LAN").as_deref() == Some("1");
        let host = env("TW_HOST")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "127.0.0.1".into());
        let port_text = env("TW_PORT").unwrap_or_else(|| "8787".into());
        if port_text.is_empty() || !port_text.bytes().all(|c| c.is_ascii_digit()) {
            bail!("Invalid TW_PORT");
        }
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .context("Invalid TW_PORT")?;
        if !local {
            bail!("TW_LOCAL=1 is required; Discord OAuth is no longer supported");
        }
        validate_bind(lan, &host)?;
        let allowed_hosts = env("TW_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|value| {
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_owned())
            })
            .collect::<Vec<_>>();
        if allowed_hosts.iter().any(|value| {
            value.len() > 253
                || !value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        }) {
            bail!("TW_ALLOWED_HOSTS must contain comma-separated hostnames");
        }
        let token = env("TW_INGEST_TOKEN");
        let token_file = env("TW_INGEST_TOKEN_FILE");
        if token.is_some() && token_file.is_some() {
            bail!("TW_INGEST_TOKEN and TW_INGEST_TOKEN_FILE are mutually exclusive");
        }
        let configured = token.is_some() || token_file.is_some();
        let ingest_token = if let Some(path) = token_file {
            let read_token = || -> Result<String> {
                let mut file = File::open(expand_path(&path, &env)?)?;
                let info = file.metadata()?;
                if !info.is_file() {
                    bail!("Not a regular file");
                }
                #[cfg(unix)]
                if info.permissions().mode() & 0o077 != 0 {
                    bail!("Not private");
                }
                if info.len() > MAX_BODY as u64 {
                    bail!("Token file too large");
                }
                let mut content = String::new();
                file.read_to_string(&mut content)?;
                Ok(content.trim().to_owned())
            };
            read_token().map_err(|_| anyhow!("TW_INGEST_TOKEN_FILE must be a readable private regular file with no group or other permissions"))?
        } else {
            token.unwrap_or_default()
        };
        if configured && ingest_token.chars().count() < 32 {
            bail!("TW_INGEST_TOKEN must be at least 32 characters");
        }
        if ingest_token
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
        {
            bail!("TW_INGEST_TOKEN must not contain whitespace or control characters");
        }
        let directory = env("TW_DATA_DIR").unwrap_or_else(|| "~/.local/state/threadwatch".into());
        Ok(Self {
            local,
            lan,
            host,
            allowed_hosts,
            port,
            ingest_token,
            data_dir: expand_path(&directory, &env)?,
        })
    }
}

fn expand_path(path: &str, env: &impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if path == "~" || path.starts_with("~/") {
        let home = env("HOME").context("HOME is required to expand paths")?;
        return Ok(PathBuf::from(home).join(path.strip_prefix("~/").unwrap_or("")));
    }
    Ok(PathBuf::from(path))
}

fn validate_bind(lan: bool, host: &str) -> Result<()> {
    if lan {
        if !host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_private()) {
            bail!("TW_LAN requires TW_LOCAL=1 and a specific private IPv4 TW_HOST");
        }
    } else if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        bail!("TW_LOCAL requires a loopback TW_HOST");
    }
    Ok(())
}

pub struct AppState {
    config: Config,
    store: Mutex<Store>,
    sanitizer: Sanitizer,
    token_digest: [u8; 32],
}

impl AppState {
    pub fn new(config: Config) -> Result<Self> {
        if !config.local {
            bail!("TW_LOCAL=1 is required");
        }
        validate_bind(config.lan, &config.host)?;
        let store = Store::open(&config.data_dir)?;
        let sanitizer = Sanitizer::new(vec![config.ingest_token.clone()]);
        let token_digest = Sha256::digest(config.ingest_token.as_bytes()).into();
        Ok(Self {
            config,
            store: Mutex::new(store),
            sanitizer,
            token_digest,
        })
    }

    fn store(&self) -> Result<MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| anyhow!("Storage lock unavailable"))
    }

    pub fn index(&self) -> Result<Value> {
        self.index_at(chrono::Utc::now().timestamp_millis())
    }

    pub fn index_at(&self, now: i64) -> Result<Value> {
        let (mut threads, mut sources, base) = {
            let store = self.store()?;
            (
                store.list()?,
                store.collector_sources(now)?,
                store.revision()?,
            )
        };
        let mut known: HashSet<String> = sources
            .iter()
            .filter_map(|source| source["id"].as_str().map(str::to_owned))
            .collect();
        for thread in &threads {
            if let Some(source) = thread["source"].as_str() {
                if known.insert(source.to_owned()) {
                    sources
                        .push(json!({"id": source, "name": thread["botName"], "status": "saved"}));
                }
            }
        }
        let by_id: HashMap<&str, &Value> = sources
            .iter()
            .filter_map(|source| source["id"].as_str().map(|id| (id, source)))
            .collect();
        for thread in &mut threads {
            let source = thread["source"]
                .as_str()
                .and_then(|id| by_id.get(id))
                .copied();
            visible_thread(thread, source);
        }
        let sources = Value::Array(sources);
        let revision = freshness_revision(&base, &sources)?;
        Ok(json!({"threads": threads, "sources": sources, "revision": revision}))
    }

    pub fn detail(&self, id: &str, before: Option<&str>, limit: usize) -> Result<Option<Value>> {
        self.detail_at(id, before, limit, chrono::Utc::now().timestamp_millis())
    }

    pub fn detail_at(
        &self,
        id: &str,
        before: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<Option<Value>> {
        let (detail, sources) = {
            let store = self.store()?;
            (
                store.detail(id, before, limit)?,
                store.collector_sources(now)?,
            )
        };
        let Some(mut detail) = detail else {
            return Ok(None);
        };
        let source = sources
            .iter()
            .find(|source| source["id"] == detail["thread"]["source"]);
        visible_thread(&mut detail["thread"], source);
        detail["revision"] = Value::String(freshness_revision(
            detail["revision"].as_str().unwrap_or("0"),
            source.unwrap_or(&Value::Null),
        )?);
        Ok(Some(detail))
    }
}

fn visible_thread(thread: &mut Value, source: Option<&Value>) {
    if source.is_some_and(|s| s["status"] == "offline")
        && matches!(
            thread["status"].as_str(),
            Some("working" | "queued" | "waiting")
        )
    {
        thread["reportedStatus"] = thread["status"].clone();
        thread["status"] = json!("unknown");
    }
}

fn freshness_revision(base: &str, sources: &Value) -> Result<String> {
    let digest = Sha256::digest(serde_json::to_vec(sources)?);
    let mut suffix = String::with_capacity(16);
    use std::fmt::Write;
    for byte in &digest[..8] {
        write!(&mut suffix, "{byte:02x}")?;
    }
    Ok(format!("{base}:{suffix}"))
}

pub struct ApiError(StatusCode, String);
impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    fn internal(_: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, "Request failed".into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
impl std::fmt::Debug for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ApiError")
            .field(&self.0)
            .field(&self.1)
            .finish()
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}
impl std::error::Error for ApiError {}
type ApiResult<T> = std::result::Result<T, ApiError>;

fn string<'a>(value: &'a Value, name: &str, max: usize) -> ApiResult<&'a str> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.encode_utf16().count() <= max)
        .ok_or_else(|| ApiError::bad(format!("Invalid {name}")))
}
fn identifier<'a>(
    value: &'a Value,
    name: &str,
    max: usize,
    sanitizer: &Sanitizer,
) -> ApiResult<&'a str> {
    let value = string(value, name, max)?;
    if value.chars().any(char::is_control) || sanitizer.text(value) != value {
        return Err(ApiError::bad(
            "Identifiers must not contain sensitive content or control characters",
        ));
    }
    Ok(value)
}
fn safe_integer(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return (0..=MAX_SAFE_INTEGER).contains(&number).then_some(number);
    }
    // JavaScript JSON numbers such as 1.0 and 1e3 remain valid integers on the wire.
    let number = value.as_f64()?;
    (number.is_finite()
        && number >= 0.0
        && number <= MAX_SAFE_INTEGER as f64
        && number.fract() == 0.0)
        .then_some(number as i64)
}
fn timestamp(value: &Value) -> ApiResult<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(number) = safe_integer(value) {
        return Ok(json!(number));
    }
    if let Some(text) = value.as_str() {
        let parsed = chrono::DateTime::parse_from_rfc3339(text)
            .map(|date| date.timestamp_millis())
            .ok()
            .or_else(|| {
                chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
                    .map(|date| date.and_utc().timestamp_millis())
            });
        if let Some(number) = parsed.filter(|number| (0..=MAX_SAFE_INTEGER).contains(number)) {
            return Ok(json!(number));
        }
    }
    Err(ApiError::bad("Invalid event timestamp"))
}

/// Whitelists the wire schema; no raw provider fields or client history versions are persisted.
pub fn validate_ingest(input: &Value, sanitizer: &Sanitizer) -> ApiResult<Value> {
    let data = input
        .get("thread")
        .filter(|data| data.is_object())
        .ok_or_else(|| ApiError::bad("Expected thread"))?;
    let bot = identifier(&data["botId"], "botId", 100, sanitizer)?;
    let external = identifier(&data["id"], "thread.id", 200, sanitizer)?;
    let status = data["status"]
        .as_str()
        .filter(|status| STATUSES.contains(status))
        .ok_or_else(|| ApiError::bad("Invalid thread status"))?;
    let updated_at = safe_integer(&data["updatedAt"])
        .ok_or_else(|| ApiError::bad("updatedAt must be epoch milliseconds"))?;
    let events = input["events"]
        .as_array()
        .filter(|events| events.len() <= 1000)
        .ok_or_else(|| ApiError::bad("events must be an array of at most 1000 records"))?;
    let mut thread = json!({"id": store::thread_id("ingest", bot, external), "botId": bot, "botName": sanitizer.text(string(&data["botName"], "botName", 200)?), "title": sanitizer.text(string(&data["title"], "title", 1000)?), "guildId": null, "channelId": null, "status": status, "updatedAt": updated_at, "source": format!("ingest:{bot}")});
    for key in ["guildId", "channelId"] {
        if !data[key].is_null() {
            thread[key] = json!(sanitizer.text(string(&data[key], key, 100)?));
        }
    }
    if let Some(source) = data.get("titleSource") {
        if !matches!(
            source.as_str(),
            Some("discord" | "session" | "user_message" | "fallback")
        ) {
            return Err(ApiError::bad("Invalid titleSource"));
        }
        thread["titleSource"] = source.clone();
    }
    let mut ids = HashSet::with_capacity(events.len());
    let mut normalized = Vec::with_capacity(events.len());
    for event in events {
        if !matches!(
            event["kind"].as_str(),
            Some(
                "message"
                    | "tool_call"
                    | "tool_result"
                    | "tool_start"
                    | "import_error"
                    | "status"
                    | "todo_edit"
                    | "session_exit"
            )
        ) {
            return Err(ApiError::bad("Invalid event kind"));
        }
        let id = identifier(&event["id"], "event.id", 200, sanitizer)?;
        if !ids.insert(id) {
            return Err(ApiError::bad("Duplicate event id"));
        }
        let time = event
            .get("timestamp")
            .ok_or_else(|| ApiError::bad("Invalid event timestamp"))?;
        let mut record = json!({"id": id, "timestamp": timestamp(time)?, "kind": event["kind"], "text": sanitizer.text(string(&event["text"], "event.text", MAX_BODY)?)});
        if let Some(role) = event.get("role") {
            if !matches!(role.as_str(), Some("user" | "assistant" | "toolResult")) {
                return Err(ApiError::bad("Invalid event role"));
            }
            record["role"] = role.clone();
        }
        if let Some(name) = event.get("name") {
            record["name"] = json!(sanitizer.text(string(name, "event.name", 200)?));
        }
        if let Some(error) = event.get("isError") {
            let error = error
                .as_bool()
                .ok_or_else(|| ApiError::bad("Invalid isError"))?;
            if error {
                record["isError"] = json!(true);
            }
        }
        normalized.push(record);
    }
    let mut output = json!({"thread": thread, "events": normalized});
    if let Some(todos) = input.get("todos") {
        let todos = todos
            .as_array()
            .filter(|todos| todos.len() <= 2000)
            .ok_or_else(|| ApiError::bad("Invalid todos"))?;
        let mut normalized = Vec::with_capacity(todos.len());
        for todo in todos {
            let text = todo["text"]
                .as_str()
                .ok_or_else(|| ApiError::bad("Invalid todos"))?;
            if !matches!(
                todo["status"].as_str(),
                Some("pending" | "in_progress" | "completed")
            ) {
                return Err(ApiError::bad("Invalid todos"));
            }
            normalized.push(json!({"text": sanitizer.text(text), "status": todo["status"]}));
        }
        output["todos"] = Value::Array(normalized);
    }
    if let Some(delivery) = input.get("delivery") {
        if !delivery.is_object() || !delivery["reset"].is_boolean() {
            return Err(ApiError::bad("Invalid delivery"));
        }
        let id = identifier(&delivery["id"], "delivery.id", 200, sanitizer)?;
        output["delivery"] = json!({"id": id, "reset": delivery["reset"]});
    }
    Ok(output)
}

pub fn validate_heartbeat(input: &Value, sanitizer: &Sanitizer) -> ApiResult<Value> {
    let bot = identifier(&input["botId"], "botId", 100, sanitizer)?;
    let name = sanitizer.text(string(&input["botName"], "botName", 200)?);
    if !matches!(
        input["status"].as_str(),
        Some("ready" | "degraded" | "unavailable")
    ) {
        return Err(ApiError::bad("Invalid collector status"));
    }
    let pending = safe_integer(&input["pendingBatches"])
        .ok_or_else(|| ApiError::bad("Invalid pendingBatches"))?;
    let mut heartbeat = json!({"botId": bot, "botName": name, "status": input["status"], "pendingBatches": pending});
    if let Some(error) = input.get("error") {
        string(error, "error", 1000)?;
        heartbeat["error"] = json!("Collector reported a collection or delivery error.");
    }
    Ok(heartbeat)
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/config",
            get(|| async { Json(json!({"local": true})) }),
        )
        .route("/api/threads", get(index))
        .route("/api/threads/{id}", get(detail))
        .route("/api/ingest", post(ingest))
        .route("/api/heartbeat", post(heartbeat))
        .merge(crate::ui::router())
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

async fn index(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    Ok(Json(state.index().map_err(ApiError::internal)?))
}
#[derive(Deserialize)]
struct Pagination {
    before: Option<String>,
    limit: Option<String>,
}
async fn detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<Pagination>,
) -> ApiResult<Json<Value>> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(ApiError(StatusCode::NOT_FOUND, "Thread not found".into()));
    }
    let raw = query.limit.as_deref().unwrap_or("100");
    let limit = raw
        .parse::<usize>()
        .ok()
        .filter(|limit| (1..=500).contains(limit));
    if raw.is_empty() || !raw.bytes().all(|c| c.is_ascii_digit()) || limit.is_none() {
        return Err(ApiError::bad("limit must be between 1 and 500"));
    }
    store::decode_cursor(&id, query.before.as_deref())
        .map_err(|_| ApiError::bad("Invalid pagination cursor"))?;
    let result = state
        .detail(&id, query.before.as_deref(), limit.unwrap())
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Thread not found".into()))?;
    Ok(Json(result))
}

async fn json_body(request: Request) -> ApiResult<Value> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json".into(),
        ));
    }
    if let Some(length) = request.headers().get(header::CONTENT_LENGTH) {
        let length = length
            .to_str()
            .ok()
            .and_then(|text| text.parse::<usize>().ok());
        if !length.is_some_and(|length| length <= MAX_BODY) {
            return Err(ApiError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body too large".into(),
            ));
        }
    }
    let body = to_bytes(request.into_body(), MAX_BODY).await.map_err(|_| {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body too large".into(),
        )
    })?;
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| ApiError::bad("Invalid JSON"))?;
    if !parsed.is_object() {
        return Err(ApiError::bad("Expected a JSON object"));
    }
    Ok(parsed)
}

fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let credential = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty() && !value.bytes().any(|c| c.is_ascii_whitespace()));
    let digest = Sha256::digest(credential.unwrap_or("").as_bytes());
    let difference = digest
        .iter()
        .zip(state.token_digest.iter())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    if state.config.ingest_token.is_empty() || credential.is_none() || difference != 0 {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Ingest authorization required".into(),
        ));
    }
    Ok(())
}
async fn ingest(State(state): State<Arc<AppState>>, request: Request) -> ApiResult<Json<Value>> {
    authorize(&state, request.headers())?;
    let value = validate_ingest(&json_body(request).await?, &state.sanitizer)?;
    let Value::Object(mut value) = value else {
        unreachable!("validated object");
    };
    let thread = value.remove("thread").expect("validated thread");
    let events = value.remove("events").expect("validated events");
    let mut store = state.store().map_err(ApiError::internal)?;
    let id = store
        .ingest(
            thread,
            events.as_array().expect("validated event array"),
            value.get("todos"),
            value.get("delivery"),
        )
        .map_err(ApiError::internal)?;
    Ok(Json(
        json!({"id": id, "revision": store.revision().map_err(ApiError::internal)?}),
    ))
}
async fn heartbeat(State(state): State<Arc<AppState>>, request: Request) -> ApiResult<Json<Value>> {
    authorize(&state, request.headers())?;
    let heartbeat = validate_heartbeat(&json_body(request).await?, &state.sanitizer)?;
    let mut store = state.store().map_err(ApiError::internal)?;
    store
        .record_heartbeat(&heartbeat, chrono::Utc::now().timestamp_millis())
        .map_err(ApiError::internal)?;
    Ok(Json(
        json!({"revision": store.revision().map_err(ApiError::internal)?}),
    ))
}

fn normalized_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        other => other,
    }
}
fn check_request(config: &Config, request: &Request<Body>) -> ApiResult<()> {
    let forbidden = || {
        ApiError(
            StatusCode::FORBIDDEN,
            "Request host, origin, or peer rejected".into(),
        )
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| normalized_ip(peer.ip()))
        .ok_or_else(forbidden)?;
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .ok_or_else(forbidden)?;
    if host.is_empty()
        || host
            .bytes()
            .any(|c| c.is_ascii_whitespace() || matches!(c, b'/' | b'@' | b'\\' | b'?' | b'#'))
    {
        return Err(forbidden());
    }
    let authority = host
        .parse::<axum::http::uri::Authority>()
        .map_err(|_| forbidden())?;
    if authority.as_str() != host || (host != authority.host() && authority.port_u16().is_none()) {
        return Err(forbidden());
    }
    if config.lan {
        let private_peer = matches!(peer, IpAddr::V4(ip) if ip.is_private());
        let allowed_host = authority.host() == config.host
            || config.allowed_hosts.iter().any(|host| host == authority.host());
        if (!peer.is_loopback() && !private_peer) || !allowed_host {
            return Err(forbidden());
        }
    } else if !peer.is_loopback()
        || !matches!(authority.host(), "localhost" | "127.0.0.1" | "[::1]")
    {
        return Err(forbidden());
    }
    if request
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|site| site.as_bytes().eq_ignore_ascii_case(b"cross-site"))
    {
        return Err(forbidden());
    }
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin = origin.to_str().map_err(|_| forbidden())?;
        if origin != format!("http://{host}") && origin != format!("https://{host}") {
            return Err(forbidden());
        }
    }
    Ok(())
}
async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let mut response = match check_request(&state.config, &request) {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cache-control", "no-store"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

pub async fn run(config: Config) -> Result<()> {
    let address = if config.host == "localhost" {
        SocketAddr::from(([127, 0, 0, 1], config.port))
    } else {
        SocketAddr::new(
            config.host.parse().context("Invalid listener address")?,
            config.port,
        )
    };
    let state = Arc::new(AppState::new(config)?);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("Threadwatch could not start its HTTP listener")?;
    println!("Threadwatch listening on http://{address}");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        if let Ok(mut term) = term {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
