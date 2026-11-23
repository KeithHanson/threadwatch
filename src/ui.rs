use std::{borrow::Cow, fmt::Write, sync::Arc};

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::Value;

use crate::server::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(shell))
        .route("/ui/index", get(index))
        .route("/ui/thread", get(thread))
        .route("/ui/poll", get(poll))
        .route("/ui/older", get(older))
        .route("/static/htmx.min.js", get(htmx))
        .route("/static/app.js", get(script))
        .route("/static/style.css", get(style))
        .route("/static/instrument-sans.woff2", get(instrument_sans))
        .route("/static/jetbrains-mono.woff2", get(jetbrains_mono))
}

async fn htmx() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../static/htmx.min.js"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../static/app.js"),
    )
}
async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/style.css"),
    )
}
async fn instrument_sans() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "font/woff2")],
        include_bytes!("../static/instrument-sans.woff2").as_slice(),
    )
}
async fn jetbrains_mono() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "font/woff2")],
        include_bytes!("../static/jetbrains-mono.woff2").as_slice(),
    )
}

#[derive(Default, Deserialize)]
struct Params {
    #[serde(default)]
    id: String,
    thread: Option<String>,
    before: Option<String>,
    after: Option<String>,
    version: Option<String>,
    revision: Option<String>,
}

fn escape(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(c),
        }
    }
    output
}
fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}
fn list<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
fn key(id: &str) -> String {
    URL_SAFE_NO_PAD.encode(id.as_bytes())
}
fn url(input: &str) -> String {
    let mut output = String::new();
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            output.push(b as char);
        } else {
            let _ = write!(output, "%{b:02X}");
        }
    }
    output
}
fn active(thread: &Value) -> bool {
    matches!(text(thread, "status"), "working" | "queued" | "waiting")
}
fn model_of(thread: &Value) -> &str {
    let value = text(thread, "model").trim();
    if value.is_empty() { "default" } else { value }
}
fn unknown_from_reporting_loss(thread: &Value) -> bool {
    text(thread, "status") == "unknown"
        && matches!(
            text(thread, "reportedStatus"),
            "working" | "waiting" | "queued"
        )
}
fn title(thread: &Value) -> &str {
    let value = text(thread, "title");
    if value.trim().is_empty() {
        "Untitled thread"
    } else {
        value
    }
}
fn source_id(thread: &Value) -> &str {
    thread["source"]
        .as_str()
        .unwrap_or_else(|| text(&thread["source"], "id"))
}
fn source_for<'a>(thread: &Value, snapshot: &'a Value) -> Option<&'a Value> {
    list(snapshot, "sources")
        .iter()
        .find(|s| text(s, "id") == source_id(thread))
}
fn status(value: &str) -> (&'static str, &'static str) {
    match value {
        "working" => ("Working", "●"),
        "queued" => ("Queued", "○"),
        "waiting" => ("Waiting", "◇"),
        "completed" => ("Completed", "✓"),
        "failed" => ("Failed", "✕"),
        "stopped" => ("Stopped", "■"),
        "ready" => ("Ready", "●"),
        "degraded" => ("Degraded", "▲"),
        "unavailable" => ("Unavailable", "✕"),
        "offline" => ("Offline", "○"),
        _ => ("Unknown", "?"),
    }
}
fn status_html(value: &str) -> String {
    let (label, glyph) = status(value);
    format!(
        "<span class=\"status status-{}\"><span aria-hidden=\"true\">{glyph}</span>{label}</span>",
        label.to_ascii_lowercase()
    )
}
fn time(value: &Value) -> String {
    let raw = scalar(value);
    let instant = value
        .as_i64()
        .or_else(|| raw.parse().ok())
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(&raw)
                .ok()
                .map(|date| date.with_timezone(&chrono::Utc))
        });
    let iso = instant.map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let stamp = escape(&raw);
    format!(
        "<time data-time=\"{stamp}\"{} title=\"{}\"><span class=\"time-label\">{stamp}</span><span class=\"sr-only time-full\">{}</span></time>",
        iso.as_ref()
            .map(|iso| format!(" datetime=\"{iso}\""))
            .unwrap_or_default(),
        iso.as_deref().unwrap_or(&stamp),
        iso.as_deref().unwrap_or("")
    )
}
fn avatar(name: &str, image: &str) -> String {
    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    let mut html = format!(
        "<span class=\"avatar\" aria-hidden=\"true\"><span>{}</span>",
        escape(&initial)
    );
    // The backend validates avatar URLs. Keep the HTML renderer safe in isolation too:
    // only Discord's HTTPS avatar paths can become image sources.
    if (image.starts_with("https://cdn.discordapp.com/avatars/")
        || image.starts_with("https://cdn.discordapp.com/embed/avatars/"))
        && !image
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '\\'))
    {
        let _ = write!(
            html,
            "<img src=\"{}\" alt=\"\" loading=\"lazy\" referrerpolicy=\"no-referrer\">",
            escape(image)
        );
    }
    html.push_str("</span>");
    html
}
fn groups(snapshot: &Value) -> Vec<Vec<&Value>> {
    let mut positions = std::collections::HashMap::new();
    let mut groups: Vec<Vec<&Value>> = Vec::new();
    for thread in list(snapshot, "threads") {
        let position = *positions.entry(text(thread, "botId")).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[position].push(thread);
    }
    groups.sort_by_cached_key(|group| {
        (
            text(group[0], "botName").to_lowercase(),
            text(group[0], "botId").to_owned(),
        )
    });
    for group in &mut groups {
        group.sort_by_key(|thread| {
            (
                std::cmp::Reverse(thread["updatedAt"].as_i64()),
                text(thread, "id"),
            )
        });
    }
    groups
}
fn link_attrs(thread: &Value) -> String {
    let id = text(thread, "id");
    format!(
        "data-thread=\"{}\" data-title=\"{}\" data-bot-id=\"{}\" data-bot-name=\"{}\" data-active=\"{}\" data-event-count=\"{}\" data-version=\"{}\" href=\"./?thread={}\" hx-get=\"ui/thread?id={}\" hx-target=\"#detail-stage\" hx-swap=\"innerHTML\" hx-sync=\"#detail-stage:replace\"",
        escape(id),
        escape(title(thread)),
        escape(text(thread, "botId")),
        escape(text(thread, "botName")),
        active(thread),
        thread["eventCount"].as_u64().unwrap_or(0),
        escape(&scalar(&thread["historyVersion"])),
        url(id),
        url(id)
    )
}
fn failure(code: StatusCode) -> Response {
    (code, Html("<p class=\"notice\" role=\"alert\">This view is unavailable. Existing records are retained; retry or select another thread.</p>")).into_response()
}

async fn shell(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Response {
    let Ok(snapshot) = state.index() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR);
    };
    // No implicit first-thread selection: the map is the default destination.
    let detail = params
        .thread
        .as_deref()
        .and_then(|id| state.detail(id, None, 100).ok().flatten());
    let selected = detail.as_ref().map(|detail| &detail["thread"]);
    let mut html = String::from(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="htmx-config" content='{"allowEval":false,"allowScriptTags":false,"includeIndicatorStyles":false,"selfRequestsOnly":true}'><title>Threadwatch</title><link rel="stylesheet" href="static/style.css"><link rel="preload" href="static/instrument-sans.woff2" as="font" type="font/woff2" crossorigin><script src="static/htmx.min.js" defer></script><script src="static/app.js" defer></script></head><body>"##,
    );
    let _ = write!(
        html,
        "<div class=\"app-shell\" data-mode=\"{}\">",
        if selected.is_some() { "thread" } else { "map" }
    );
    html.push_str(r##"<a class="skip-link" href="#thread-main">Skip to thread</a><div id="announcer" class="sr-only" aria-live="polite"></div><div id="drawer-scrim" hidden></div><aside id="thread-sidebar" class="sidebar" aria-label="Agent and its threads"><header class="rail-topbar"><a class="map-link" href="./">← Agent map</a><span id="rail-connection" class="healthy" role="status" title="Server connected">● Connected</span><button class="pair-device" type="button">Pair mobile</button><button id="close-drawer" type="button">Close</button></header>"##);
    html.push_str("<div id=\"index-data\" hx-get=\"ui/index\" hx-trigger=\"every 2s\" hx-swap=\"innerHTML\" hx-sync=\"this:drop\">");
    html.push_str(&render_index(
        &snapshot,
        selected.map(|thread| text(thread, "id")),
    ));
    html.push_str(r##"</div></aside><main id="thread-main" tabindex="-1"><header class="mobile-header"><a id="mobile-map-link" class="map-link" href="./">← Map</a><strong id="mobile-title">"##);
    html.push_str(&escape(
        selected
            .map(|thread| text(thread, "botName"))
            .unwrap_or("Threadwatch"),
    ));
    html.push_str(r##"</strong><button id="menu" type="button" aria-controls="thread-sidebar" aria-expanded="false">Threads</button><span id="mobile-connection" class="healthy" role="status" aria-label="Server Connected">● Live</span></header><header id="map-topbar" class="map-topbar"><div class="brand-row"><strong>Threadwatch</strong><span class="read-only">Fleet console</span></div><div class="connection" role="status"><span class="micro-label">Server</span><span id="connection" class="healthy">● Connected</span><span id="connection-age">refreshed 0s ago</span><button class="pair-device" type="button">Pair mobile</button></div></header><div id="server-error" class="notice" role="alert" hidden></div><div id="request-error" class="notice" role="alert" hidden><span></span><button class="retry-open" type="button" hidden>Retry opening</button></div>"##);
    let _ = write!(
        html,
        "<section id=\"agent-map\" aria-labelledby=\"map-heading\"{}><div id=\"map-data\">",
        if selected.is_none() { "" } else { " hidden" }
    );
    html.push_str(&render_map(&snapshot));
    html.push_str(r##"</div></section><section id="opening" role="status" hidden><span>Opening thread…</span><h1></h1><div class="skeleton"></div><div class="skeleton"></div></section>"##);
    let _ = write!(
        html,
        "<div id=\"panels\"{}>",
        if selected.is_none() { " hidden" } else { "" }
    );
    if let Some(detail) = detail.as_ref() {
        html.push_str(&render_panel(detail, &snapshot));
    }
    html.push_str(r##"</div><div id="detail-stage" hidden></div></main></div><dialog id="pair-dialog" aria-labelledby="pair-heading"><form method="dialog"><button class="dialog-close" type="submit">Close</button></form><h2 id="pair-heading">Pair mobile browser</h2><p>Scan once within two minutes. The paired browser stays authorized for 24 hours.</p><img id="pair-qr" alt="One-time Threadwatch pairing QR code"><p id="pair-status" role="status"></p></dialog><noscript><p class="noscript">This is a server-rendered snapshot. Enable JavaScript for filters, live updates, transcript history, search, and pause controls.</p></noscript></body></html>"##);
    Html(html).into_response()
}

async fn index(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Response {
    match state.index() {
        Ok(snapshot) => {
            let mut html = format!(
                "{}<div id=\"map-data\" hx-swap-oob=\"innerHTML\">{}</div>",
                render_index(&snapshot, Some(&params.id)),
                render_map(&snapshot)
            );
            // A paused transcript must not also freeze the collector's health.
            // Index metadata suffices; do not load a transcript to refresh it.
            if let Some(thread) = list(&snapshot, "threads")
                .iter()
                .find(|thread| text(thread, "id") == params.id)
            {
                html.push_str(&render_meta(thread, &snapshot, true));
            }
            Html(html).into_response()
        }
        Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn filter_tabs(scope: &str, active_count: usize, total: usize) -> String {
    format!(
        "<div class=\"filter-tabs\" data-scope=\"{scope}\" role=\"group\" aria-label=\"Thread filter\"><button type=\"button\" data-filter=\"active\" aria-pressed=\"true\">Active <span class=\"filter-count\">{active_count}</span></button><button type=\"button\" data-filter=\"past\" aria-pressed=\"false\">Past <span class=\"filter-count\">{}</span></button></div>",
        total - active_count
    )
}

fn render_agent_identity(html: &mut String, group: &[&Value], snapshot: &Value, map: bool) {
    let thread = group[0];
    let name = text(thread, "botName");
    let source = source_for(thread, snapshot);
    let state = source.map(|s| text(s, "status")).unwrap_or("unknown");
    let image = group
        .iter()
        .map(|t| text(t, "avatarUrl"))
        .find(|s| !s.is_empty())
        .or_else(|| source.map(|s| text(s, "avatarUrl")))
        .unwrap_or("");
    let _ = write!(
        html,
        "{}<div class=\"bot-title\"><h2>{}</h2><span>{} active · {} total{}</span></div><div class=\"bot-source\"><span class=\"mono\">{}</span>{}<span>heartbeat {}</span></div>",
        avatar(name, image),
        escape(name),
        group.iter().filter(|t| active(t)).count(),
        group.len(),
        if map {
            " <span class=\"bot-arrow\" aria-hidden=\"true\">→</span>"
        } else {
            ""
        },
        escape(source.map(|s| text(s, "name")).unwrap_or("Unknown source")),
        status_html(state),
        source
            .map(|s| time(&s["lastSeenAt"]))
            .unwrap_or_else(|| "unknown".into())
    );
    if state != "ready" {
        html.push_str("<div class=\"bot-warning-text\" role=\"status\">");
        if state == "offline" {
            let affected = group
                .iter()
                .filter(|t| unknown_from_reporting_loss(t))
                .count();
            if affected == 0 {
                html.push_str("Reporting lost. Showing the last available records.");
            } else {
                let _ = write!(
                    html,
                    "Reporting lost. {affected} threads shown as Unknown with last reported state."
                );
            }
        } else {
            let _ = write!(
                html,
                "Collector reports {}.",
                status(state).0.to_ascii_lowercase()
            );
        }
        if let Some(source) = source.filter(|s| !text(s, "error").is_empty()) {
            let _ = write!(
                html,
                "<p class=\"source-error\">{}</p>",
                escape(text(source, "error"))
            );
        }
        html.push_str("</div>");
    }
}

fn render_index(snapshot: &Value, selected_id: Option<&str>) -> String {
    let threads = list(snapshot, "threads");
    let groups = groups(snapshot);
    let selected_bot = threads
        .iter()
        .find(|t| Some(text(t, "id")) == selected_id)
        .map(|t| text(t, "botId"));
    let unknown = threads
        .iter()
        .filter(|t| unknown_from_reporting_loss(t))
        .count();
    let mut html = format!(
        "<div class=\"index-summary\" hidden data-total=\"{}\" data-active=\"{}\" data-bots=\"{}\" data-unknown=\"{unknown}\"></div>",
        threads.len(),
        threads.iter().filter(|t| active(t)).count(),
        groups.len()
    );
    for group in groups {
        let bot_id = text(group[0], "botId");
        let name = text(group[0], "botName");
        let count = group.iter().filter(|t| active(t)).count();
        let _ = write!(
            html,
            "<div class=\"agent-rail\" data-bot-id=\"{}\" data-bot-name=\"{}\" data-active-count=\"{count}\" data-total-count=\"{}\"{}><section class=\"agent-block\" aria-label=\"Agent\">",
            escape(bot_id),
            escape(name),
            group.len(),
            if selected_bot == Some(bot_id) {
                ""
            } else {
                " hidden"
            }
        );
        render_agent_identity(&mut html, &group, snapshot, false);
        let _ = write!(
            html,
            "</section><div class=\"rail-threads-heading\"><h3>Threads</h3>{}</div><nav class=\"thread-list\" aria-label=\"{} threads\">",
            filter_tabs("rail", count, group.len()),
            escape(name)
        );
        for thread in group {
            let _ = write!(
                html,
                "<a class=\"thread-link thread-item\" {}{}{}><span class=\"thread-title\">{}</span>{}{}</a>",
                link_attrs(thread),
                if Some(text(thread, "id")) == selected_id {
                    " aria-current=\"page\""
                } else {
                    ""
                },
                if active(thread) { "" } else { " hidden" },
                escape(title(thread)),
                status_html(text(thread, "status")),
                time(&thread["updatedAt"])
            );
        }
        let _ = write!(
            html,
            "<div class=\"rail-empty empty-state\"{}><p>No active threads for {}.</p><button class=\"toggle-filter\" type=\"button\">Show past threads</button></div></nav></div>",
            if count == 0 { "" } else { " hidden" },
            escape(name)
        );
    }
    html
}

fn render_map(snapshot: &Value) -> String {
    let threads = list(snapshot, "threads");
    let groups = groups(snapshot);
    let count = threads.iter().filter(|t| active(t)).count();
    let unknown = threads
        .iter()
        .filter(|t| unknown_from_reporting_loss(t))
        .count();
    let mut html = format!(
        "<header class=\"map-heading\"><div><h1 id=\"map-heading\">Agent map</h1><p id=\"map-summary\">{count} active threads across {} bots · select a bot to focus on it</p></div>{}</header>",
        groups.len(),
        filter_tabs("map", count, threads.len())
    );
    let offline: Vec<_> = list(snapshot, "sources")
        .iter()
        .filter(|s| text(s, "status") == "offline")
        .collect();
    if !offline.is_empty() {
        let heading = if offline.len() == 1 {
            format!("{} is offline", text(offline[0], "name"))
        } else {
            format!("{} collectors are offline", offline.len())
        };
        let _ = write!(
            html,
            "<div class=\"reporting-alert\" role=\"alert\"><strong>{}</strong><p>",
            escape(&heading)
        );
        if unknown > 0 {
            let _ = write!(
                html,
                "{unknown} previously active threads are now Unknown and listed under Past with their last reported state."
            );
        } else {
            html.push_str("No active threads were affected.");
        }
        html.push_str("</p>");
        if unknown > 0 {
            html.push_str("<button class=\"reporting-past\" data-filter=\"past\" type=\"button\">Show in Past</button>");
        }
        html.push_str("</div>");
    }
    let _ = write!(
        html,
        "<div id=\"map-empty\" class=\"empty-state\"{}><p>",
        if count == 0 { "" } else { " hidden" }
    );
    if unknown > 0 {
        let _ = write!(
            html,
            "No active threads. {unknown} previously active threads are Unknown because their collectors are offline; they are under Past."
        );
    } else {
        html.push_str("No thread is Working, Queued or Waiting.");
    }
    html.push_str("</p><button class=\"toggle-filter\" type=\"button\">Show past threads</button></div><div class=\"map-grid\">");
    for group in groups {
        let thread = group[0];
        let name = text(thread, "botName");
        let source = source_for(thread, snapshot);
        let state = source.map(|s| text(s, "status")).unwrap_or("unknown");
        let count = group.iter().filter(|t| active(t)).count();
        // Groups are newest-first. Prefer an active thread, then the newest saved thread.
        let target = group.iter().copied().find(|t| active(t)).unwrap_or(thread);
        let _ = write!(
            html,
            "<section class=\"map-bot\" data-bot-id=\"{}\" aria-label=\"{}\"><div class=\"bot-card{}\"><a class=\"bot-open\" data-bot-id=\"{}\" href=\"./?thread={}\" aria-label=\"Open {}: {count} active · {} total\"></a>",
            escape(text(thread, "botId")),
            escape(name),
            if state == "ready" { "" } else { " bot-warning" },
            escape(text(thread, "botId")),
            url(text(target, "id")),
            escape(name),
            group.len()
        );
        render_agent_identity(&mut html, &group, snapshot, true);
        let _ = write!(
            html,
            "</div><p class=\"map-bot-empty\"{}>No active threads</p><ul class=\"map-threads\">",
            if count == 0 { "" } else { " hidden" }
        );
        for thread in group {
            let short = if text(thread, "shortTitle").is_empty() {
                title(thread).chars().take(24).collect::<String>()
            } else {
                text(thread, "shortTitle").to_owned()
            };
            let _ = write!(
                html,
                "<li class=\"map-branch\"{}><span class=\"connector\" aria-hidden=\"true\"></span><a class=\"thread-link map-thread\" {}><span class=\"map-thread-title\"><strong title=\"{}\">{}</strong>{}</span><span class=\"map-thread-facts\">{}<span class=\"thread-model\" title=\"Session model\"><span class=\"micro-label\">Model</span><span class=\"mono\">{}</span></span>",
                if active(thread) { "" } else { " hidden" },
                link_attrs(thread),
                escape(title(thread)),
                escape(&short),
                time(&thread["updatedAt"]),
                status_html(text(thread, "status")),
                escape(model_of(thread))
            );
            if text(thread, "status") == "unknown" && !text(thread, "reportedStatus").is_empty() {
                let _ = write!(
                    html,
                    "<span class=\"warning\">last reported {}</span>",
                    status(text(thread, "reportedStatus")).0
                );
            }
            if thread["todoCount"].as_u64().unwrap_or(0) > 0 {
                let _ = write!(
                    html,
                    "<span>{}/{} todos</span>",
                    thread["todoCompleted"].as_u64().unwrap_or(0),
                    thread["todoCount"].as_u64().unwrap_or(0)
                );
            }
            html.push_str("</span><span class=\"map-preview\">");
            let event = &thread["latestEvent"];
            if event.is_object() {
                let _ = write!(
                    html,
                    "<span>{}{}:</span> {}",
                    escape(&event_label(event)),
                    if text(event, "name").is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", escape(text(event, "name")))
                    },
                    escape(text(event, "text"))
                );
            } else {
                html.push_str("Nothing recorded yet.");
            }
            html.push_str("</span></a></li>");
        }
        html.push_str("</ul></section>");
    }
    // A collector is useful evidence even before it has delivered a thread.
    for source in list(snapshot, "sources") {
        if list(snapshot, "threads")
            .iter()
            .any(|t| source_id(t) == text(source, "id"))
        {
            continue;
        }
        let name = text(source, "name");
        let state = text(source, "status");
        let _ = write!(
            html,
            "<section class=\"map-bot orphan-source\" aria-label=\"{}\"><div class=\"bot-card{}\">{}<div class=\"bot-title\"><h2>{}</h2></div><div class=\"bot-source\">{}<span>heartbeat {}</span><span>{} queued batches</span></div><p class=\"bot-warning-text\">No threads delivered.</p>",
            escape(name),
            if state == "ready" { "" } else { " bot-warning" },
            avatar(name, text(source, "avatarUrl")),
            escape(name),
            status_html(state),
            time(&source["lastSeenAt"]),
            source["pendingBatches"].as_u64().unwrap_or(0)
        );
        if !text(source, "error").is_empty() {
            let _ = write!(
                html,
                "<p class=\"source-error\">{}</p>",
                escape(text(source, "error"))
            );
        }
        html.push_str("</div></section>");
    }
    html.push_str("</div>");
    html
}

async fn thread(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Response {
    let Ok(Some(detail)) = state.detail(&params.id, None, 100) else {
        return failure(StatusCode::NOT_FOUND);
    };
    let Ok(snapshot) = state.index() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Html(render_panel(&detail, &snapshot)).into_response()
}

fn render_panel(detail: &Value, snapshot: &Value) -> String {
    let id = text(&detail["thread"], "id");
    let k = key(id);
    let mut html = format!(
        "<section class=\"thread-panel\" id=\"panel-{k}\" data-thread=\"{}\" data-bot=\"{}\" data-bot-id=\"{}\" data-version=\"{}\" data-revision=\"{}\" data-event-count=\"{}\" hx-sync=\"this:drop\">",
        escape(id),
        escape(text(&detail["thread"], "botName")),
        escape(text(&detail["thread"], "botId")),
        escape(&scalar(&detail["thread"]["historyVersion"])),
        escape(&scalar(&detail["revision"])),
        detail["eventCount"].as_u64().unwrap_or(0)
    );
    html.push_str(&render_meta(&detail["thread"], snapshot, false));
    let todo_count = list(detail, "todos").len();
    let _ = write!(
        html,
        "<div class=\"detail-tabs\" role=\"tablist\" aria-label=\"Thread detail\"><button class=\"detail-tab\" id=\"transcript-tab-{k}\" type=\"button\" role=\"tab\" aria-selected=\"false\" aria-controls=\"transcript-panel-{k}\" data-detail-tab=\"transcript\" tabindex=\"-1\">Transcript</button><button class=\"detail-tab\" id=\"tasks-tab-{k}\" type=\"button\" role=\"tab\" aria-selected=\"true\" aria-controls=\"todos-{k}\" data-detail-tab=\"tasks\">Tasks <span class=\"tab-count\">{todo_count}</span></button></div>"
    );
    html.push_str(&render_todos(detail, false));
    let _ = write!(
        html,
        r##"<section class="transcript-panel" id="transcript-panel-{k}" role="tabpanel" aria-labelledby="transcript-tab-{k}" hidden><header class="transcript-toolbar"><h2 id="transcript-{k}" class="sr-only">Transcript</h2><span class="record-count"></span><span class="freshness" role="status"></span><div class="transcript-controls"><label class="search-field"><span class="sr-only">Search loaded history</span><input class="transcript-search" type="search" placeholder="Search loaded history" autocomplete="off"></label><button class="pause" type="button" aria-pressed="false">Pause</button><button class="jump" type="button" aria-pressed="true">Follow latest ↓</button></div><p class="search-count" aria-live="polite"></p></header><div class="transcript-body"><div class="transcript-scroll" tabindex="0" aria-label="Transcript records"><div class="view-error notice" role="alert" hidden></div><div class="history-notice" role="status" hidden><span></span><button class="dismiss-history" type="button">Dismiss</button></div>"##
    );
    html.push_str(&render_pager(detail, false));
    let _ = write!(html, "<div class=\"events\" id=\"events-{k}\">");
    render_events(&mut html, list(detail, "events"), &k);
    html.push_str("</div><div class=\"empty-events empty-state\"><p>Nothing recorded yet.</p><button class=\"search-older\" type=\"button\" hidden>Load older records</button><button class=\"clear-search\" type=\"button\" hidden>Clear search</button></div></div><button class=\"new-records jump\" type=\"button\" hidden></button></div></section>");
    html.push_str(&render_chat(detail, false));
    let _ = write!(
        html,
        "<div class=\"poller\" hx-get=\"ui/poll?id={}\" hx-trigger=\"every 2s, refresh\" hx-target=\"#events-{k}\" hx-swap=\"beforeend\" aria-hidden=\"true\"></div></section>",
        url(id)
    );
    html
}

fn render_meta(thread: &Value, snapshot: &Value, oob: bool) -> String {
    let k = key(text(thread, "id"));
    let source = source_for(thread, snapshot);
    let source_status = source.map(|s| text(s, "status")).unwrap_or("unknown");
    let title_source = match text(thread, "titleSource") {
        "discord" => "Discord thread title",
        "session" => "saved session title",
        "user_message" => "label from first message",
        _ => "thread identifier (no title recorded)",
    };
    let mut html = format!(
        "<header class=\"thread-header\" id=\"meta-{k}\"{}><div class=\"breadcrumbs\"><a class=\"map-link\" href=\"./\">← Agent map</a><span class=\"bot-name\">{}</span><span>Title from {title_source}</span></div><h1 title=\"{}\">{}</h1><div class=\"thread-metadata\"><span><span class=\"micro-label\">Task</span>{}<span class=\"muted\">updated {}</span></span><span><span class=\"micro-label\">Source</span><span class=\"mono\">{}</span>{}<span class=\"muted\">heartbeat {} · {} queued batches</span></span><span><span class=\"micro-label\">Model</span><span class=\"mono\">{}</span></span>",
        if oob {
            " hx-swap-oob=\"outerHTML\""
        } else {
            ""
        },
        escape(text(thread, "botName")),
        escape(title(thread)),
        escape(title(thread)),
        status_html(text(thread, "status")),
        time(&thread["updatedAt"]),
        escape(source.map(|s| text(s, "name")).unwrap_or("Unknown source")),
        status_html(source_status),
        source
            .map(|s| time(&s["lastSeenAt"]))
            .unwrap_or_else(|| "unknown".into()),
        source
            .and_then(|s| s["pendingBatches"].as_u64())
            .unwrap_or(0),
        escape(model_of(thread))
    );
    let guild = text(thread, "guildId");
    let channel = text(thread, "channelId");
    if !guild.is_empty()
        && !channel.is_empty()
        && guild.bytes().all(|c| c.is_ascii_digit())
        && channel.bytes().all(|c| c.is_ascii_digit())
    {
        let _ = write!(
            html,
            "<a class=\"discord-link\" href=\"https://discord.com/channels/{guild}/{channel}\" target=\"_blank\" rel=\"noopener noreferrer\">Open in Discord ↗</a>"
        );
    }
    let _ = write!(
        html,
        "<button class=\"mobile-todos\" type=\"button\" aria-expanded=\"false\" aria-controls=\"todos-{k}\">Todos</button>"
    );
    html.push_str("</div>");
    if source_status != "ready" {
        let _ = write!(
            html,
            "<div class=\"source-notice\" role=\"status\"><strong>Source {}.</strong> {}",
            status(source_status).0.to_ascii_lowercase(),
            match source_status {
                "offline" =>
                    "Heartbeat stopped. Saved records remain available, but reporting is lost. This does not establish that the task process stopped.",
                "degraded" =>
                    "The collector reports impaired operation. Saved records remain available.",
                "unavailable" =>
                    "The collector reports this source unavailable. Saved records remain available.",
                _ =>
                    "No current heartbeat evidence for this source. Showing the last available records.",
            }
        );
        if text(thread, "status") == "unknown" && !text(thread, "reportedStatus").is_empty() {
            let _ = write!(
                html,
                "<p>Last reported task state: <strong>{}</strong>. Shown as Unknown, not Completed.</p>",
                status(text(thread, "reportedStatus")).0
            );
        }
        html.push_str("</div>");
    }
    html.push_str("</header>");
    html
}

fn render_todos(detail: &Value, oob: bool) -> String {
    let k = key(text(&detail["thread"], "id"));
    let todos = list(detail, "todos");
    let done = todos
        .iter()
        .filter(|t| text(t, "status") == "completed")
        .count();
    let mut html = format!(
        "<section class=\"todo-panel\" id=\"todos-{k}\"{} role=\"tabpanel\" aria-labelledby=\"tasks-tab-{k}\" tabindex=\"0\"><div class=\"section-heading\"><h2 id=\"todo-heading-{k}\" class=\"sr-only\">Recorded task list</h2>",
        if oob {
            " hx-swap-oob=\"outerHTML\""
        } else {
            ""
        }
    );
    if !todos.is_empty() {
        let _ = write!(
            html,
            "<progress value=\"{done}\" max=\"{}\" aria-label=\"Completed recorded tasks\"></progress><span>{done} of {} todos completed</span>",
            todos.len(),
            todos.len()
        );
    }
    html.push_str(
        "<span class=\"todo-hint\">Read-only · recorded todos, not an estimate of progress</span>",
    );
    if !todos.is_empty() {
        let _ = write!(
            html,
            "<button class=\"toggle-todos\" type=\"button\" aria-expanded=\"true\" aria-controls=\"todo-list-{k}\">Collapse</button>"
        );
    }
    html.push_str("</div>");
    if todos.is_empty() {
        html.push_str("<p class=\"muted no-todos\">No task list recorded for this thread.</p>");
    } else {
        let _ = write!(
            html,
            "<ol class=\"todo-list\" id=\"todo-list-{k}\" tabindex=\"0\" aria-label=\"Recorded tasks\">"
        );
        for todo in todos {
            let (class, label, mark) = match text(todo, "status") {
                "completed" => ("completed", "Completed", "✓"),
                "in_progress" => ("working", "In progress", "▸"),
                _ => ("pending", "Pending", "○"),
            };
            let _ = write!(
                html,
                "<li class=\"todo todo-{class}\"><span aria-hidden=\"true\">{mark}</span><span>{}</span><small>{label}</small></li>",
                escape(text(todo, "text"))
            );
        }
        html.push_str("</ol>");
    }
    html.push_str("</section>");
    html
}

fn render_chat_commands(detail: &Value, k: &str, oob: bool) -> String {
    let mut html = format!(
        "<div class=\"chat-commands\" id=\"chat-commands-{k}\"{} aria-live=\"polite\">",
        if oob {
            " hx-swap-oob=\"outerHTML\""
        } else {
            ""
        }
    );
    for command in list(detail, "commands") {
        let status = text(command, "status");
        let _ = write!(
            html,
            "<article class=\"chat-command chat-{status}\"><div class=\"message-text\">{}</div><div class=\"chat-command-meta\"><time data-time=\"{}\"><span class=\"time-label\">{}</span><span class=\"time-full sr-only\"></span></time><span>{}</span></div>",
            escape(text(command, "text")),
            escape(&scalar(&command["createdAt"])),
            time(&command["createdAt"]),
            match status {
                "accepted" => "Accepted by bridge",
                "rejected" => "Rejected by bridge",
                _ => "Waiting for bridge",
            }
        );
        if let Some(error) = command["error"].as_str().filter(|error| !error.is_empty()) {
            let _ = write!(html, "<p class=\"chat-error\">{}</p>", escape(error));
        }
        html.push_str("</article>");
    }
    html.push_str("</div>");
    html
}

fn render_chat(detail: &Value, oob: bool) -> String {
    let id = text(&detail["thread"], "id");
    let k = key(id);
    if oob {
        return render_chat_commands(detail, &k, true);
    }
    let mut html = format!(
        "<section class=\"chat-panel\" id=\"chat-{k}\" aria-labelledby=\"chat-heading-{k}\"><div class=\"section-heading\"><h2 id=\"chat-heading-{k}\">Chat</h2><span class=\"chat-hint\">Messages are queued through the reporting bridge.</span></div>{}",
        render_chat_commands(detail, &k, false)
    );
    let disabled =
        detail["controlEnabled"] != true || text(&detail["thread"], "status") == "unknown";
    let _ = write!(
        html,
        "<form class=\"chat-form\" data-command-url=\"api/threads/{}/commands\"><label for=\"chat-input-{k}\" class=\"sr-only\">Message the agent</label><textarea id=\"chat-input-{k}\" maxlength=\"20000\" rows=\"3\" placeholder=\"Message the agent…\"{}></textarea><div><span class=\"chat-feedback\" role=\"status\"></span><button type=\"submit\"{}>Send</button></div></form></section>",
        url(id),
        if disabled { " disabled" } else { "" },
        if disabled { " disabled" } else { "" },
    );
    html
}

fn render_pager(detail: &Value, oob: bool) -> String {
    let id = text(&detail["thread"], "id");
    let k = key(id);
    let mut html = format!(
        "<div class=\"older-row\" id=\"pager-{k}\"{}>",
        if oob {
            " hx-swap-oob=\"outerHTML\""
        } else {
            ""
        }
    );
    if let Some(cursor) = detail["nextCursor"].as_str() {
        let _ = write!(
            html,
            "<button class=\"older\" type=\"button\" hx-get=\"ui/older?id={}&amp;before={}\" hx-target=\"#events-{k}\" hx-swap=\"afterbegin\">Load older records</button><span class=\"paused-older-hint\" hidden>Resume to load older records.</span>",
            url(id),
            url(cursor)
        );
    } else {
        html.push_str("<span class=\"muted\">— Beginning of recorded activity —</span>");
    }
    html.push_str("</div>");
    html
}

fn event_label(event: &Value) -> String {
    match text(event, "kind") {
        "message" => if text(event, "role") == "user" {
            "User"
        } else {
            "Assistant"
        }
        .into(),
        "tool_call" => "Tool call".into(),
        "tool_start" | "tool_started" => "Tool started".into(),
        "tool_result" => if event["isError"].as_bool().unwrap_or(false) {
            "Tool result · error"
        } else {
            "Tool result"
        }
        .into(),
        "todo_edit" => "Todo edit".into(),
        "session_exit" => "Session exit".into(),
        "import_error" => "Import error".into(),
        "status" => "Status".into(),
        kind => kind.replace('_', " "),
    }
}
fn recorded_text<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields.iter().find_map(|field| {
        value
            .get(field)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    })
}

fn tool_preview_end(value: &str) -> usize {
    let mut lines = 1;
    for (count, (index, c)) in value.char_indices().enumerate() {
        if count == 1200 || (c == '\n' && lines == 12) {
            return index;
        }
        if c == '\n' {
            lines += 1;
        }
    }
    value.len()
}

fn render_tool_field(html: &mut String, label: &str, value: &Value, disclosure: bool) {
    if value.is_null() {
        return;
    }
    let value = match value {
        Value::String(value) => Cow::Borrowed(value.as_str()),
        _ => Cow::Owned(serde_json::to_string_pretty(value).unwrap()),
    };
    let label = escape(label);
    if disclosure {
        let _ = write!(
            html,
            "<details class=\"tool-field-details\"><summary>{label}</summary><pre class=\"tool-code\">{}</pre></details>",
            escape(&value)
        );
        return;
    }
    let end = tool_preview_end(&value);
    let _ = write!(
        html,
        "<div class=\"tool-field\"><span class=\"tool-field-label\">{label}</span><pre class=\"tool-code\">{}</pre>",
        escape(&value[..end])
    );
    if end < value.len() {
        let _ = write!(
            html,
            "<details class=\"tool-field-details tool-full-text\"><summary>Show all {label} · preview above</summary><pre class=\"tool-code\">{}</pre></details>",
            escape(&value)
        );
    }
    html.push_str("</div>");
}

fn tool_fields(name: &str, device: bool) -> &'static [(&'static str, &'static str)] {
    match name {
        "bash" | "shell" | "exec" | "command" => {
            &[("command", "Command"), ("cwd", "Working directory")]
        }
        "eval" => &[("language", "Language"), ("code", "Code")],
        "read" => &[
            ("path", "Path / range"),
            ("range", "Range"),
            ("offset", "Offset"),
            ("limit", "Limit"),
        ],
        "grep" => &[("pattern", "Pattern"), ("path", "Search in")],
        "glob" => &[("path", "Path pattern")],
        "web_search" => &[("query", "Query")],
        "edit" | "ast_edit" | "apply_patch" => &[
            ("path", "Target"),
            ("patch", "Proposed patch"),
            ("diff", "Proposed patch"),
            ("input", "Proposed patch"),
            ("old_text", "Recorded match"),
            ("new_text", "Proposed replacement"),
            ("content", "Proposed content"),
        ],
        "write" if device => &[("path", "API / device"), ("content", "Request body")],
        "write" => &[("path", "File"), ("content", "File content")],
        "task" => &[("context", "Shared context"), ("tasks", "Tasks")],
        "todo" => &[
            ("op", "Operation"),
            ("text", "Task"),
            ("tasks", "Tasks"),
            ("todos", "Todos"),
            ("items", "Items"),
        ],
        "wait" => &[
            ("ids", "Waiting for"),
            ("handles", "Waiting for"),
            ("timeout", "Timeout"),
        ],
        _ => &[],
    }
}

fn render_tool_argument(html: &mut String, name: &str, field: &str, label: &str, value: &Value) {
    if matches!(field, "tasks" | "todos" | "items")
        && let Some(items) = value.as_array().filter(|items| !items.is_empty())
    {
        let _ = write!(
            html,
            "<div class=\"tool-field\"><span class=\"tool-field-label\">{}</span><ol class=\"tool-task-list\">",
            escape(label)
        );
        for item in items {
            html.push_str("<li>");
            if let Some(fields) = item.as_object() {
                for (field, value) in fields {
                    render_tool_field(html, field, value, false);
                }
            } else {
                render_tool_field(html, "Task", item, false);
            }
            html.push_str("</li>");
        }
        html.push_str("</ol></div>");
    } else {
        render_tool_field(html, label, value, name == "write" && field == "content");
    }
}

fn tool_content_keys(name: &str) -> &'static [&'static str] {
    match name {
        "bash" | "shell" | "exec" | "command" => &["command"],
        "eval" => &["code"],
        "read" => &["path"],
        "grep" => &["pattern"],
        "glob" => &["path"],
        "web_search" => &["query"],
        "edit" | "ast_edit" | "apply_patch" => {
            &["patch", "diff", "input", "content", "old_text", "new_text"]
        }
        "write" => &["content"],
        "task" => &["context", "tasks"],
        "todo" => &["text", "tasks", "todos", "items"],
        "wait" => &["ids", "handles"],
        _ => &[],
    }
}

fn render_tool_input(html: &mut String, name: &str, arguments: Option<&Value>, device: bool) {
    let Some(arguments) = arguments else {
        html.push_str("<p class=\"tool-note\">Readable input unavailable; the recorded text is retained below.</p>");
        return;
    };
    let Some(fields) = arguments.as_object() else {
        render_tool_field(
            html,
            if matches!(name, "edit" | "apply_patch") {
                "Proposed patch"
            } else {
                "Recorded input"
            },
            arguments,
            false,
        );
        return;
    };
    let ordered = tool_fields(name, device);
    let content_keys = tool_content_keys(name);
    let is_content = |key: &str| content_keys.iter().any(|&content| content == key);
    let mut content_html = String::new();
    let mut meta_html = String::new();
    for (field, label) in ordered {
        if let Some(value) = fields.get(*field) {
            let target = if is_content(field) {
                &mut content_html
            } else {
                &mut meta_html
            };
            render_tool_argument(target, name, field, label, value);
        }
    }
    for (field, value) in fields {
        if !matches!(field.as_str(), "i" | "intent" | "title")
            && !ordered.iter().any(|(key, _)| *key == field.as_str())
        {
            let label = field.replace('_', " ");
            let target = if is_content(field.as_str()) {
                &mut content_html
            } else {
                &mut meta_html
            };
            render_tool_argument(target, name, field, &label, value);
        }
    }
    html.push_str(&content_html);
    if !meta_html.is_empty() {
        let _ = write!(html, "<div class=\"tool-meta\">{meta_html}</div>");
    }
    if matches!(name, "bash" | "shell" | "exec" | "command")
        && fields.get("cwd").is_none_or(Value::is_null)
    {
        html.push_str("<p class=\"tool-note\">Working directory not recorded.</p>");
    }
    if matches!(name, "edit" | "ast_edit" | "apply_patch") {
        html.push_str("<p class=\"tool-note\">Proposed edit; this input does not establish that a file changed.</p>");
    }
}

fn render_tool_card(html: &mut String, event: &Value, content: &str, error: bool) {
    let name = text(event, "name");
    let operation = name.rsplit('.').next().unwrap_or(name);
    let kind = text(event, "kind");
    let result = kind.contains("result") || text(event, "role") == "toolResult";
    let withheld = content.trim_start().starts_with("[WITHHELD") || event["withheld"] == true;
    let structured = event.get("arguments").filter(|value| value.is_object());
    let structured_title =
        structured.and_then(|value| recorded_text(value, &["i", "title", "intent"]));
    let legacy = if !result
        && !withheld
        && (structured.is_none()
            || (recorded_text(event, &["intent"]).is_none() && structured_title.is_none()))
    {
        serde_json::from_str::<Value>(content)
            .ok()
            .filter(Value::is_object)
    } else {
        None
    };
    let legacy_arguments = legacy
        .as_ref()
        .and_then(|value| {
            value
                .get("arguments")
                .or_else(|| value.get("args"))
                .or(Some(value))
        })
        .filter(|value| value.is_object() || value.is_string());
    let arguments = if withheld {
        None
    } else {
        structured.or(legacy_arguments)
    };
    let path = arguments.map(|value| text(value, "path")).unwrap_or("");
    let device = operation == "write" && path.starts_with("xd://");
    let device_operation = path
        .strip_prefix("xd://")
        .unwrap_or("")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let notebook_operation = device_operation.strip_prefix("mcp__open_notebook_");
    let language = arguments.map(|value| text(value, "language")).unwrap_or("");
    let category = match operation {
        "bash" | "shell" | "exec" | "command" => "Shell",
        "eval" => match language {
            "py" | "python" => "Python",
            "js" | "javascript" => "JavaScript",
            _ => "Eval",
        },
        "read" => "Read",
        "grep" | "glob" | "web_search" => "Search",
        "edit" | "ast_edit" | "apply_patch" => "Edit",
        "write" if device && notebook_operation.is_some() => "Notebook",
        "write" if device => "API / device",
        "write" => "File write",
        "task" => "Task",
        "todo" => "Todo",
        "wait" => "Wait",
        _ => "Tool",
    };
    let fallback = if withheld {
        "Tool details withheld"
    } else if result {
        "Recorded tool output"
    } else {
        match operation {
            "bash" | "shell" | "exec" | "command" => "Run a shell command",
            "eval" => "Evaluate code",
            "read" => "Read recorded target",
            "grep" | "glob" | "web_search" => "Search recorded target",
            "edit" | "ast_edit" | "apply_patch" => "Propose an edit",
            "write" if device => "Send an API / device request",
            "write" => "Write file content",
            "task" => "Delegate tasks",
            "todo" => "Manage recorded tasks",
            "wait" => "Wait for recorded work",
            _ => "Recorded tool operation",
        }
    };
    let title = if withheld {
        fallback
    } else {
        recorded_text(event, &["intent"])
            .or(structured_title)
            .or_else(|| {
                legacy
                    .as_ref()
                    .and_then(|value| recorded_text(value, &["intent"]))
            })
            .or_else(|| {
                legacy_arguments.and_then(|value| recorded_text(value, &["i", "title", "intent"]))
            })
            .unwrap_or(fallback)
    };
    let background = result
        && operation == "bash"
        && !withheld
        && (content.starts_with("Backgrounded as job ")
            || content.starts_with("Background job launched: "));
    let (state, status) = if error {
        ("error", "Tool error")
    } else if background {
        (
            "started",
            "Background job launched (completion not recorded)",
        )
    } else if result {
        ("result", "Result received")
    } else if matches!(kind, "tool_start" | "tool_started") {
        ("started", "Started (no result recorded)")
    } else {
        ("call", "Call recorded")
    };
    let _ = write!(
        html,
        "<div class=\"tool-card-heading\"><div class=\"tool-card-meta\"><span class=\"tool-type\">{category} <span class=\"tool-name\">· {}</span></span><span class=\"tool-status\" data-state=\"{state}\">{status}</span></div><h3 class=\"tool-title\">{}</h3></div><div class=\"tool-input\">",
        escape(if name.is_empty() { "unknown" } else { name }),
        escape(title)
    );
    if withheld {
        html.push_str("<p class=\"tool-note\">Sanitized record withheld the tool details.</p>");
    } else if !result {
        if device {
            let _ = write!(
                html,
                "<p class=\"tool-operation\">API operation: <code>{}</code></p><p class=\"tool-note\">A device request, not a file write.</p>",
                escape(notebook_operation.unwrap_or(device_operation))
            );
        }
        render_tool_input(html, operation, arguments, device);
    }
    html.push_str("</div>");
    let record_label = if error {
        "Error record"
    } else if result {
        "Recorded output"
    } else if matches!(kind, "tool_start" | "tool_started") {
        "Recorded start"
    } else {
        "Recorded input"
    };
    let _ = write!(
        html,
        "<details class=\"tool-record\"{}><summary>{record_label}</summary><pre class=\"tool-output\">{}</pre></details>",
        if error { " open" } else { "" },
        escape(content)
    );
}

fn render_events(html: &mut String, events: &[Value], k: &str) {
    for event in events {
        let kind = text(event, "kind");
        let role = text(event, "role");
        let content = text(event, "text");
        let tool = matches!(role, "tool" | "toolResult")
            || ["tool", "command", "exec", "result"]
                .iter()
                .any(|part| kind.contains(part));
        let message = !tool && kind == "message";
        let error = event["isError"].as_bool().unwrap_or(false)
            || kind.contains("error")
            || kind.contains("failed");
        let _ = write!(
            html,
            "<article class=\"event {}{}{}\" id=\"event-{k}-{}\" data-event=\"{}\"",
            if tool {
                "event-tool"
            } else if message {
                "event-message"
            } else {
                "event-notice"
            },
            if message && role == "user" {
                " event-user"
            } else {
                ""
            },
            if error { " event-error" } else { "" },
            key(text(event, "id")),
            escape(text(event, "id"))
        );
        if tool {
            let _ = write!(
                html,
                " data-tool-call=\"{}\" data-tool-session=\"{}\" data-tool-kind=\"{}\" data-tool-name=\"{}\"",
                escape(text(event, "toolCallId")),
                escape(text(event, "sessionId")),
                escape(kind),
                escape(text(event, "name"))
            );
        }
        let _ = write!(
            html,
            ">{}<div class=\"event-content\">",
            time(&event["timestamp"])
        );
        if tool {
            render_tool_card(html, event, content, error);
        } else {
            let content = if content.trim().is_empty() {
                "No text recorded."
            } else {
                content
            };
            let _ = write!(
                html,
                "<span class=\"event-label\">{}</span><div class=\"message-text\">{}</div>",
                escape(&event_label(event)),
                escape(content)
            );
        }
        html.push_str("</div></article>");
    }
}

async fn poll(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Response {
    update(&state, params, false)
}
async fn older(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Response {
    update(&state, params, true)
}

fn update(state: &AppState, params: Params, older: bool) -> Response {
    let Ok(Some(mut detail)) = state.detail(
        &params.id,
        if older {
            params.before.as_deref()
        } else {
            None
        },
        100,
    ) else {
        return failure(StatusCode::NOT_FOUND);
    };
    let version = scalar(&detail["thread"]["historyVersion"]);
    let replace = params.version.as_deref().unwrap_or("") != version;
    if older && replace {
        let Ok(Some(latest)) = state.detail(&params.id, None, 100) else {
            return failure(StatusCode::NOT_FOUND);
        };
        detail = latest;
    }
    let revision = scalar(&detail["revision"]);
    let mut html = String::new();
    let k = key(&params.id);
    if older || replace || params.revision.as_deref() != Some(revision.as_str()) {
        // Walk backwards to the displayed tail, recovering every page accumulated
        // while paused without merging two history generations.
        let mut pages = Vec::new();
        let mut next_page = None;
        loop {
            let page = next_page.as_ref().unwrap_or(&detail);
            let events = list(page, "events");
            let boundary = if !older && !replace {
                params
                    .after
                    .as_deref()
                    .and_then(|after| events.iter().position(|event| text(event, "id") == after))
            } else {
                None
            };
            let start = boundary.map(|n| n + 1).unwrap_or(0);
            let mut rendered = String::new();
            render_events(&mut rendered, &events[start..], &k);
            pages.push(rendered);
            if older || replace || boundary.is_some() {
                break;
            }
            let Some(cursor) = page["nextCursor"].as_str() else {
                break;
            };
            let Ok(Some(next)) = state.detail(&params.id, Some(cursor), 100) else {
                return failure(StatusCode::INTERNAL_SERVER_ERROR);
            };
            if scalar(&next["thread"]["historyVersion"])
                != scalar(&detail["thread"]["historyVersion"])
                || scalar(&next["revision"]) != revision
            {
                return StatusCode::NO_CONTENT.into_response();
            }
            next_page = Some(next);
        }
        for page in pages.iter().rev() {
            html.push_str(page);
        }
    }
    let Ok(snapshot) = state.index() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR);
    };
    html.push_str(&render_meta(&detail["thread"], &snapshot, true));
    html.push_str(&render_todos(&detail, true));
    html.push_str(&render_chat(&detail, true));
    if older || replace {
        html.push_str(&render_pager(&detail, true));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("tw-version", scalar(&detail["thread"]["historyVersion"])),
        ("tw-revision", revision),
        (
            "tw-event-count",
            detail["eventCount"].as_u64().unwrap_or(0).to_string(),
        ),
        (
            "tw-mode",
            if replace {
                "replace"
            } else if older {
                "older"
            } else {
                "poll"
            }
            .into(),
        ),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    if replace {
        headers.insert("hx-reswap", HeaderValue::from_static("innerHTML"));
    }
    (headers, Html(html)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_status_alone_does_not_imply_a_reporting_loss_transition() {
        assert!(!unknown_from_reporting_loss(&json!({"status":"unknown"})));
        assert!(!unknown_from_reporting_loss(
            &json!({"status":"unknown","reportedStatus":"completed"})
        ));
        for previous in ["working", "waiting", "queued"] {
            assert!(unknown_from_reporting_loss(
                &json!({"status":"unknown","reportedStatus":previous})
            ));
        }
    }

    #[test]
    fn event_types_escape_content_and_keep_full_searchable_output() {
        let mut html = String::new();
        render_events(
            &mut html,
            &[
                json!({"id":"\" onfocus=\"bad", "kind":"tool_result", "name":"<script>", "text":"</pre><img src=x onerror=bad>\nneedle at end", "isError":true, "timestamp":0}),
                json!({"id":"u", "kind":"message", "role":"user", "text":"hello"}),
                json!({"id":"s", "kind":"session_exit", "text":"done"}),
            ],
            "safe",
        );
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;/pre&gt;"));
        assert!(html.contains("<details class=\"tool-record\" open>"));
        assert!(html.contains("needle at end</pre>"));
        assert!(html.contains("event-message event-user"));
        assert!(html.contains("event-notice"));
        assert!(html.contains("datetime=\"1970-01-01T00:00:00.000Z\""));
    }

    #[test]
    fn tool_cards_show_intent_and_exact_command_with_safe_metadata() {
        let mut html = String::new();
        let command = "printf '<script>&\"\\n'\n  pwd";
        let raw = "{\"arguments\":{\"command\":\"retained legacy text\"}}";
        render_events(
            &mut html,
            &[json!({
                "id":"call", "kind":"tool_call", "name":"functions.bash", "intent":"Inspect <workspace>",
                "arguments":{"i":"Lower priority", "command":command, "cwd":"/tmp/<project>"},
                "toolCallId":"call\"<&", "sessionId":"session\"<&", "text":raw
            })],
            "safe",
        );
        assert!(html.contains("<h3 class=\"tool-title\">Inspect &lt;workspace&gt;</h3>"));
        assert!(!html.contains("Lower priority"));
        assert!(html.contains(&format!(
            "<pre class=\"tool-code\">{}</pre>",
            escape(command)
        )));
        assert!(html.contains(
            "Working directory</span><pre class=\"tool-code\">/tmp/&lt;project&gt;</pre>"
        ));
        assert!(html.contains("data-tool-call=\"call&quot;&lt;&amp;\""));
        assert!(html.contains("data-tool-session=\"session&quot;&lt;&amp;\""));
        assert!(html.contains("data-tool-kind=\"tool_call\" data-tool-name=\"functions.bash\""));
        assert!(
            html.contains("<span class=\"tool-status\" data-state=\"call\">Call recorded</span>")
        );
        assert!(html.contains(&format!("<pre class=\"tool-output\">{}</pre>", escape(raw))));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn tool_input_lays_meta_fields_out_horizontally_after_the_command() {
        let mut html = String::new();
        render_events(
            &mut html,
            &[json!({
                "id":"call", "kind":"tool_call", "name":"bash", "intent":"Inspect the repo",
                "arguments":{"command":"echo one\necho two\necho three", "cwd":"/tmp/<project>",
                             "async":false, "pty":false, "timeout":120},
                "text":"sanitized original"
            })],
            "safe",
        );
        let meta_open = html.find("<div class=\"tool-meta\">").expect("meta wrapper");
        let command_preview = html.find("echo one").expect("command preview");
        assert!(
            command_preview < meta_open,
            "command must render before the metadata row"
        );
        let meta = &html[meta_open..];
        assert!(meta.contains("Working directory</span><pre class=\"tool-code\">/tmp/&lt;project&gt;"));
        assert!(meta.contains("async</span><pre class=\"tool-code\">false"));
        assert!(meta.contains("pty</span><pre class=\"tool-code\">false"));
        assert!(meta.contains("timeout</span><pre class=\"tool-code\">120"));
        assert!(!meta.contains("echo one"), "command must not repeat inside the metadata row");
    }

    #[test]
    fn tool_titles_use_recorded_intent_without_guessing_from_commands() {
        for (event, expected) in [
            (
                json!({"arguments":{"i":"Argument intent","title":"Argument title","command":"echo not a title"}}),
                "Argument intent",
            ),
            (
                json!({"arguments":{"title":"Argument title","command":"echo not a title"}}),
                "Argument title",
            ),
            (
                json!({"text":"{\"intent\":\"Legacy intent\",\"arguments\":{\"i\":\"Legacy argument intent\",\"command\":\"pwd\"}}"}),
                "Legacy intent",
            ),
            (
                json!({"text":"{\"arguments\":{\"i\":\"Legacy argument intent\",\"command\":\"pwd\"}}"}),
                "Legacy argument intent",
            ),
            (
                json!({"text":"{\"i\":\"Unwrapped intent\",\"command\":\"pwd\"}"}),
                "Unwrapped intent",
            ),
            (
                json!({"arguments":{"command":"echo not a title"},"text":"not JSON"}),
                "Run a shell command",
            ),
        ] {
            let mut event = event;
            event["id"] = json!("call");
            event["kind"] = json!("tool_call");
            event["name"] = json!("bash");
            let mut html = String::new();
            render_events(&mut html, &[event], "safe");
            assert!(html.contains(&format!("<h3 class=\"tool-title\">{expected}</h3>")));
        }
    }

    #[test]
    fn malformed_and_withheld_tool_records_keep_safe_fallbacks() {
        for raw in ["{broken JSON <tag>", "{\"arguments\":17}", "[]", "null", ""] {
            let mut html = String::new();
            render_events(
                &mut html,
                &[
                    json!({"id":"bad","kind":"tool_start","name":"read","text":raw,"arguments":false}),
                ],
                "safe",
            );
            assert!(html.contains("Readable input unavailable"));
            assert!(html.contains("Started (no result recorded)"));
            assert!(html.contains(&format!("<pre class=\"tool-output\">{}</pre>", escape(raw))));
            assert!(!html.contains("<tag>"));
        }
        let raw = "[WITHHELD: tool arguments target a credential or instruction file]";
        let mut html = String::new();
        render_events(
            &mut html,
            &[json!({
                "id":"withheld","kind":"tool_call","name":"bash","text":raw,
                "intent":"Must not reveal this intent","arguments":{"command":"must-not-display","cwd":"/secret"}
            })],
            "safe",
        );
        assert!(html.contains("Tool details withheld"));
        assert!(html.contains(&format!("<pre class=\"tool-output\">{raw}</pre>")));
        assert!(!html.contains("Must not reveal"));
        assert!(!html.contains("must-not-display"));
        assert!(!html.contains("/secret"));
    }

    #[test]
    fn read_search_eval_and_patch_cards_show_only_recorded_input() {
        let patch = "*** Begin Patch\n[file.rs#ABCD]\nPUT 1.=1:\n+new <line>\n*** End Patch";
        for (name, arguments, expected) in [
            (
                "read",
                json!({"path":"src/ui.rs:502-542"}),
                "Path / range</span><pre class=\"tool-code\">src/ui.rs:502-542",
            ),
            (
                "grep",
                json!({"pattern":"tool_call|<script>","path":"src/"}),
                "Pattern</span><pre class=\"tool-code\">tool_call|&lt;script&gt;",
            ),
            (
                "eval",
                json!({"language":"py","code":"print('<value>')"}),
                "Code</span><pre class=\"tool-code\">print(&#39;&lt;value&gt;&#39;)",
            ),
            (
                "edit",
                json!({"patch":patch}),
                "Proposed patch</span><pre class=\"tool-code\">*** Begin Patch",
            ),
        ] {
            let mut html = String::new();
            render_events(
                &mut html,
                &[
                    json!({"id":"input","kind":"tool_call","name":name,"arguments":arguments,"text":"sanitized original"}),
                ],
                "safe",
            );
            assert!(html.contains(expected));
            assert!(html.contains("<pre class=\"tool-output\">sanitized original</pre>"));
            if name == "grep" {
                assert!(html.contains("Search in</span><pre class=\"tool-code\">src/"));
            }
            if name == "eval" {
                assert!(html.contains("Python <span class=\"tool-name\">"));
            }
            if name == "edit" {
                assert!(html.contains(&escape(patch)));
                assert!(html.contains("does not establish that a file changed"));
                assert!(!html.contains("Before"));
                assert!(!html.contains("-old"));
            }
        }
    }

    #[test]
    fn file_writes_and_device_requests_have_distinct_labels_and_disclosures() {
        for (path, category, body_label, operation) in [
            ("notes.txt", "File write", "File content", None),
            (
                "xd://mcp__open_notebook_create_note",
                "Notebook",
                "Request body",
                Some("create_note"),
            ),
            ("xd://debug", "API / device", "Request body", Some("debug")),
        ] {
            let mut html = String::new();
            render_events(
                &mut html,
                &[json!({
                    "id":"write","kind":"tool_call","name":"write","intent":"Record a note",
                    "arguments":{"path":path,"content":"<unsafe>\ncomplete body"},"text":"recorded write"
                })],
                "safe",
            );
            assert!(html.contains(&format!("class=\"tool-type\">{category} ")));
            assert!(html.contains(&format!("<pre class=\"tool-code\">{path}</pre>")));
            assert!(html.contains(&format!("<details class=\"tool-field-details\"><summary>{body_label}</summary><pre class=\"tool-code\">&lt;unsafe&gt;\ncomplete body</pre></details>")));
            if let Some(operation) = operation {
                assert!(html.contains(&format!("API operation: <code>{operation}</code>")));
                assert!(html.contains("not a file write"));
            } else {
                assert!(!html.contains("API operation"));
            }
        }
    }

    #[test]
    fn long_tool_input_has_a_bounded_utf8_preview_and_complete_disclosure() {
        let code = format!("{}last <line>", "界\n".repeat(20));
        let mut html = String::new();
        render_events(
            &mut html,
            &[json!({
                "id":"long","kind":"tool_call","name":"eval","arguments":{"language":"js","code":code},
                "text":"complete original text"
            })],
            "safe",
        );
        let preview = html
            .split("Code</span><pre class=\"tool-code\">")
            .nth(1)
            .unwrap()
            .split("</pre>")
            .next()
            .unwrap();
        assert_eq!(preview.lines().count(), 12);
        assert!(!preview.contains("last"));
        assert!(html.contains("Show all Code · preview above"));
        assert!(html.contains(&format!("<pre class=\"tool-code\">{}</pre>", escape(&code))));
        let unicode = "界".repeat(1201);
        assert_eq!(&unicode[..tool_preview_end(&unicode)], "界".repeat(1200));
    }

    #[test]
    fn tool_results_errors_and_background_receipts_do_not_claim_completion() {
        let mut html = String::new();
        render_events(
            &mut html,
            &[
                json!({"id":"call","kind":"tool_call","name":"bash","text":"{}","toolCallId":"same","sessionId":"session"}),
                json!({"id":"start","kind":"tool_start","name":"bash","text":"{}","toolCallId":"same","sessionId":"session"}),
                json!({"id":"result","kind":"tool_result","name":"bash","text":"Background job launched: bg_1","toolCallId":"same","sessionId":"session"}),
                json!({"id":"error","kind":"tool_result","name":"eval","text":"RuntimeError: <missing>","isError":true}),
            ],
            "safe",
        );
        assert_eq!(html.matches("<article class=\"event ").count(), 4);
        assert_eq!(html.matches("class=\"tool-record\"").count(), 4);
        assert!(html.contains("Call recorded"));
        assert!(html.contains("Started (no result recorded)"));
        assert!(html.contains("Background job launched (completion not recorded)"));
        assert!(html.contains("Tool error"));
        assert!(
            html.contains("<details class=\"tool-record\" open><summary>Error record</summary>")
        );
        assert!(html.contains("RuntimeError: &lt;missing&gt;"));
        assert!(html.contains("Background job launched: bg_1</pre>"));
        assert!(!html.contains("Succeeded"));
        assert!(!html.contains("Completed"));
        assert!(!html.contains("duration"));
    }

    #[test]
    fn tasks_todos_waits_and_unknown_tools_keep_readable_fields() {
        for (name, arguments, labels) in [
            (
                "task",
                json!({"context":"Shared goal","tasks":[{"name":"Worker","task":"Read <file>","agent":"scout"}]}),
                &[
                    "Shared context",
                    "tool-task-list",
                    "Read &lt;file&gt;",
                    "scout",
                ][..],
            ),
            (
                "todo",
                json!({"op":"update","items":[{"text":"Review work","status":"working"}]}),
                &["Operation", "Review work", "working"][..],
            ),
            (
                "wait",
                json!({"ids":["worker"],"timeout":30}),
                &["Waiting for", "worker", "Timeout"][..],
            ),
            (
                "custom",
                json!({"target_name":"<safe>","enabled":false,"options":{"depth":2}}),
                &["target name", "&lt;safe&gt;", "false", "&quot;depth&quot;"][..],
            ),
        ] {
            let mut html = String::new();
            render_events(
                &mut html,
                &[
                    json!({"id":"other","kind":"tool_call","name":name,"arguments":arguments,"text":"original"}),
                ],
                "safe",
            );
            for label in labels {
                assert!(html.contains(*label), "missing {label} in {name} card");
            }
            assert!(html.contains("<pre class=\"tool-output\">original</pre>"));
        }
    }

    #[test]
    fn discord_and_avatar_links_cannot_inject_external_schemes() {
        let mut detail = json!({"thread":{"id":"external","guildId":"123","channelId":"456","title":"<unsafe>","titleSource":"session"},"todos":[]});
        let html = render_meta(&detail["thread"], &json!({"sources":[]}), false);
        assert!(html.contains("https://discord.com/channels/123/456"));
        assert!(html.contains("&lt;unsafe&gt;"));
        detail["thread"]["channelId"] = json!("javascript:alert(1)");
        assert!(
            !render_meta(&detail["thread"], &json!({"sources":[]}), false)
                .contains("https://discord.com")
        );
        assert!(!avatar("Agent", "javascript:alert(1)").contains("<img"));
        assert!(
            !avatar(
                "Agent",
                "https://cdn.discordapp.com.evil.test/avatars/1/a.png"
            )
            .contains("<img")
        );
        assert!(
            avatar(
                "Agent",
                "https://cdn.discordapp.com/avatars/1/a.png?size=96"
            )
            .contains("<img")
        );
    }

    #[test]
    fn map_and_sidebar_share_filter_metadata_without_transcripts() {
        let snapshot = json!({"threads":[
            {"id":"a","botId":"b","botName":"Bot","title":"A title","shortTitle":"Short","status":"working","source":"s","eventCount":42,"historyVersion":3,"todoCount":5,"todoCompleted":2,"latestEvent":{"kind":"tool_result","name":"read","text":"preview"}},
            {"id":"z","botId":"b","botName":"Bot","title":"Old","status":"unknown","reportedStatus":"working","source":"s"}
        ],"sources":[{"id":"s","name":"Collector","status":"offline"},{"id":"empty","name":"Empty collector","status":"ready","pendingBatches":7}]});
        let sidebar = render_index(&snapshot, None);
        let map = render_map(&snapshot);
        for html in [&sidebar, &map] {
            assert!(html.contains("data-active=\"true\""));
            assert!(html.contains("data-active=\"false\""));
            assert!(html.contains("data-event-count=\"42\""));
            assert!(html.contains("data-version=\"3\""));
            assert!(html.contains("data-bot-id=\"b\""));
            assert!(html.contains("data-bot-name=\"Bot\""));
            assert!(!html.contains("data-search="));
        }
        assert!(!sidebar.contains("reporting-alert"));
        assert!(map.contains("reporting-past"));
        assert!(map.contains("Empty collector"));
        assert!(sidebar.contains("data-unknown=\"1\""));
        assert!(map.contains("2/5 todos"));
        assert!(map.contains("orphan-source"));
        assert!(map.contains("map-preview"));
        assert!(map.contains(">Short</strong>"));
        assert!(!map.contains("tool-output"));
    }

    #[test]
    fn rails_keep_all_metadata_but_only_the_selected_bot_is_visible() {
        let snapshot = json!({"threads":[
            {"id":"a","botId":"one","botName":"First","status":"working"},
            {"id":"b","botId":"two","botName":"Second","status":"completed"},
            {"id":"c","botId":"two","botName":"Second","status":"unknown","reportedStatus":"working"}
        ]});
        for selected in [None, Some("b")] {
            let html = render_index(&snapshot, selected);
            let rails: Vec<_> = html.split("<div class=\"agent-rail\"").skip(1).collect();
            assert_eq!(rails.len(), 2);
            for rail in rails {
                let attributes = rail.split('>').next().unwrap();
                let is_selected = selected.is_some() && attributes.contains("data-bot-id=\"two\"");
                assert_eq!(attributes.ends_with(" hidden"), !is_selected);
            }
            for id in ["a", "b", "c"] {
                assert!(html.contains(&format!("data-thread=\"{id}\"")));
            }
            assert!(html.contains("data-active-count=\"0\" data-total-count=\"2\""));
            assert!(html.contains("data-active=\"false\""));
        }
    }

    #[test]
    fn bot_targets_prefer_latest_active_then_latest_past_and_orphans_are_not_links() {
        let snapshot = json!({"threads":[
            {"id":"new-past","botId":"one","botName":"First","status":"completed","updatedAt":30,"source":"s"},
            {"id":"old-active","botId":"one","botName":"First","status":"working","updatedAt":10,"source":"s"},
            {"id":"new-active","botId":"one","botName":"First","status":"waiting","updatedAt":20,"source":"s"},
            {"id":"old-past","botId":"two","botName":"Second","status":"failed","updatedAt":10,"source":"s"},
            {"id":"latest-past","botId":"two","botName":"Second","status":"unknown","updatedAt":20,"source":"s"}
        ],"sources":[{"id":"s","status":"ready"},{"id":"empty","name":"No records","status":"offline"}]});
        let map = render_map(&snapshot);
        assert!(
            map.contains("class=\"bot-open\" data-bot-id=\"one\" href=\"./?thread=new-active\"")
        );
        assert!(
            map.contains("class=\"bot-open\" data-bot-id=\"two\" href=\"./?thread=latest-past\"")
        );
        let orphan = map.split("class=\"map-bot orphan-source\"").nth(1).unwrap();
        assert!(!orphan.contains("<a "));
        assert!(map.contains("Past <span class=\"filter-count\">3</span>"));
        for state in ["working", "queued", "waiting"] {
            assert!(active(&json!({"status":state})));
        }
        for state in ["completed", "failed", "stopped", "unknown", ""] {
            assert!(!active(&json!({"status":state})));
        }
    }

    #[test]
    fn recorded_tasks_and_generation_metadata_are_structural() {
        let detail = json!({"thread":{"id":"a","botId":"bot","botName":"Agent","historyVersion":7},"eventCount":200,"todos":[{"text":"done","status":"completed"},{"text":"ongoing","status":"in_progress"}]});
        let panel = render_panel(&detail, &json!({"sources":[]}));
        assert!(panel.contains("data-version=\"7\""));
        assert!(panel.contains("data-event-count=\"200\""));
        assert!(panel.contains("data-bot=\"Agent\" data-bot-id=\"bot\""));
        assert!(panel.contains("value=\"1\" max=\"2\""));
        assert!(panel.contains("aria-controls=\"todo-list-YQ\""));
        assert!(panel.contains("class=\"history-notice\" role=\"status\" hidden"));
        assert!(panel.contains("class=\"new-records jump\""));
        assert!(panel.contains("class=\"transcript-search\""));
        assert!(panel.contains("class=\"detail-tabs\" role=\"tablist\""));
        assert!(panel.contains("role=\"tab\" aria-selected=\"false\" aria-controls=\"transcript-panel-YQ\""));
        assert!(panel.contains("role=\"tab\" aria-selected=\"true\" aria-controls=\"todos-YQ\" data-detail-tab=\"tasks\""));
        assert!(panel.contains("role=\"tabpanel\" aria-labelledby=\"tasks-tab-YQ\""));
        assert!(panel.contains("id=\"transcript-panel-YQ\" role=\"tabpanel\" aria-labelledby=\"transcript-tab-YQ\" hidden"));
    }

    #[test]
    fn map_and_detail_show_the_session_model() {
        let snapshot = json!({"threads":[
            {"id":"a","botId":"b","botName":"Bot","title":"A title","shortTitle":"Short","status":"working","source":"s","updatedAt":1000,"model":"anthropic/claude-sonnet-4-5"},
            {"id":"b","botId":"b","botName":"Bot","title":"Other","status":"completed","source":"s","updatedAt":1000}
        ],"sources":[{"id":"s","name":"Collector","status":"ready"}]});
        let map = render_map(&snapshot);
        assert!(map.contains("<span class=\"thread-model\" title=\"Session model\"><span class=\"micro-label\">Model</span><span class=\"mono\">anthropic/claude-sonnet-4-5</span></span>"));
        assert!(map.contains("<span class=\"thread-model\" title=\"Session model\"><span class=\"micro-label\">Model</span><span class=\"mono\">default</span></span>"));
        let detail = json!({"thread":{"id":"a","botId":"b","botName":"Bot","title":"A title","status":"working","updatedAt":1000,"model":"anthropic/claude-sonnet-4-5"},"eventCount":0,"todos":[]});
        let panel = render_panel(&detail, &snapshot);
        assert!(panel.contains("<span class=\"micro-label\">Model</span><span class=\"mono\">anthropic/claude-sonnet-4-5</span>"));
        let default_detail = json!({"thread":{"id":"a","botId":"b","botName":"Bot","title":"A title","status":"working","updatedAt":1000},"eventCount":0,"todos":[]});
        let panel = render_panel(&default_detail, &snapshot);
        assert!(panel.contains(
            "<span class=\"micro-label\">Model</span><span class=\"mono\">default</span>"
        ));
    }
}
