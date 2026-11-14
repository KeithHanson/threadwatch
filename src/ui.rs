use std::{fmt::Write, sync::Arc};

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
fn status(value: &str) -> &'static str {
    match value {
        "working" => "Working",
        "queued" => "Queued",
        "waiting" => "Waiting",
        "completed" => "Completed",
        "failed" => "Failed",
        "stopped" => "Stopped",
        _ => "Unknown",
    }
}
fn status_html(value: &str) -> String {
    let label = status(value);
    format!(
        "<span class=\"status status-{}\"><span aria-hidden=\"true\">●</span> {label}</span>",
        label.to_ascii_lowercase()
    )
}
fn time(value: &Value) -> String {
    let stamp = scalar(value);
    format!(
        "<time data-time=\"{}\" title=\"{}\">{}</time>",
        escape(&stamp),
        escape(&stamp),
        escape(&stamp)
    )
}
fn failure(code: StatusCode) -> Response {
    (code, Html("<p class=\"notice\" role=\"alert\">This view is unavailable. Retry or select another thread.</p>")).into_response()
}

async fn shell(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Response {
    let Ok(snapshot) = state.index() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let selected = params
        .thread
        .as_deref()
        .or_else(|| list(&snapshot, "threads").first().map(|v| text(v, "id")));
    let panel = selected
        .and_then(|id| state.detail(id, None, 100).ok().flatten())
        .map(|detail| render_panel(&detail, &snapshot))
        .unwrap_or_default();
    let mut html = String::from(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="htmx-config" content='{"allowEval":false,"allowScriptTags":false,"includeIndicatorStyles":false,"selfRequestsOnly":true}'><title>Threadwatch · Your agents, in focus</title><link rel="stylesheet" href="/static/style.css"><script src="/static/htmx.min.js" defer></script><script src="/static/app.js" defer></script></head><body><a class="skip-link" href="#main-content">Skip to thread</a><div class="app-shell"><header class="mobile-header"><button id="menu" aria-controls="thread-sidebar" aria-expanded="false">Threads</button><strong>threadwatch<span class="accent">.</span></strong></header><aside id="thread-sidebar" class="sidebar" aria-label="Threads"><header class="brand"><span class="brand-mark" aria-hidden="true">⌁</span><div><strong>threadwatch<span class="accent">.</span></strong><p>YOUR AGENTS, IN FOCUS</p></div></header><div class="sidebar-tools"><label class="search-field"><span class="sr-only">Search threads and bots</span><input id="thread-search" type="search" placeholder="Find a thread or bot…" autocomplete="off"></label><fieldset class="filter-tabs"><legend class="sr-only">Thread filter</legend><label><input type="radio" name="thread-filter" value="all" checked> All threads</label><label><input type="radio" name="thread-filter" value="active"> Active</label></fieldset></div><div id="index-data" hx-get="/ui/index" hx-trigger="every 2s" hx-swap="innerHTML" hx-sync="this:drop">"##,
    );
    html.push_str(&render_index(&snapshot));
    html.push_str(r##"</div><p id="connection" class="connection" role="status">Connected to server · LAN</p><footer class="sidebar-footer">Read-only observer · 2s refresh</footer></aside><main id="main-content" tabindex="-1"><div id="request-error" class="notice" role="alert" hidden></div><div id="welcome" class="welcome"><span class="eyebrow">AGENT OBSERVABILITY</span><h1>A clear view of every thread.</h1><p>Select a thread to inspect its actual tasks, messages, and tool activity. Reporting sources remain visible even before their first thread.</p></div><div id="panels">"##);
    html.push_str(&panel);
    html.push_str(r##"</div><div id="detail-stage"></div></main></div><noscript><p class="noscript">This page is a server-rendered snapshot. Enable JavaScript for live updates, transcript history, search, and pause controls.</p></noscript></body></html>"##);
    Html(html).into_response()
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    match state.index() {
        Ok(snapshot) => Html(render_index(&snapshot)).into_response(),
        Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn render_index(snapshot: &Value) -> String {
    let threads = list(snapshot, "threads");
    let mut html = format!(
        "<nav class=\"thread-list\" aria-label=\"Bot threads\"><p class=\"index-count\">{} threads · {} active</p>",
        threads.len(),
        threads
            .iter()
            .filter(|t| matches!(text(t, "status"), "working" | "waiting" | "queued"))
            .count()
    );
    let mut positions = std::collections::HashMap::new();
    let mut groups: Vec<Vec<&Value>> = Vec::new();
    for thread in threads {
        let position = *positions.entry(text(thread, "botId")).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[position].push(thread);
    }
    for group in groups {
        let _ = write!(
            html,
            "<section class=\"bot-group\"><h2>{}</h2>",
            escape(text(group[0], "botName"))
        );
        for thread in group {
            let id = text(thread, "id");
            let title = text(thread, "title");
            let title = if title.is_empty() {
                "Untitled thread"
            } else {
                title
            };
            let _ = write!(
                html,
                "<a class=\"thread-item\" data-thread=\"{}\" data-search=\"{} {}\" data-active=\"{}\" href=\"/?thread={}\" hx-get=\"/ui/thread?id={}\" hx-target=\"#detail-stage\" hx-swap=\"innerHTML\" hx-sync=\"#detail-stage:replace\"><strong>{}</strong><span class=\"thread-line\">{}<span>Updated {}</span></span></a>",
                escape(id),
                escape(title),
                escape(text(thread, "botName")),
                matches!(text(thread, "status"), "working" | "queued" | "waiting"),
                url(id),
                url(id),
                escape(title),
                status_html(text(thread, "status")),
                time(&thread["updatedAt"])
            );
        }
        html.push_str("</section>");
    }
    html.push_str("<p id=\"no-threads\" class=\"empty-small\">No matching threads. Sources report independently below.</p></nav><section class=\"sources\" aria-labelledby=\"source-heading\"><h2 id=\"source-heading\">Reporting sources</h2>");
    let sources = list(snapshot, "sources");
    if sources.is_empty() {
        html.push_str("<p class=\"muted\">No collectors have reported yet.</p>");
    }
    for source in sources {
        let state = text(source, "status");
        let _ = write!(
            html,
            "<article class=\"source\"><div><strong>{}</strong><span class=\"{}\">{}</span></div><p>Last seen {} · {} queued batches</p>",
            escape(text(source, "name")),
            if state == "ready" {
                "healthy"
            } else {
                "warning"
            },
            escape(if state == "saved" || state.is_empty() {
                "Unknown"
            } else {
                state
            }),
            if source["lastSeenAt"].is_null() {
                "unknown".into()
            } else {
                time(&source["lastSeenAt"])
            },
            source["pendingBatches"].as_u64().unwrap_or(0)
        );
        if !text(source, "error").is_empty() {
            let _ = write!(
                html,
                "<p class=\"warning\">{}</p>",
                escape(text(source, "error"))
            );
        }
        html.push_str("</article>");
    }
    html.push_str("</section>");
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
        "<section class=\"thread-panel\" id=\"panel-{k}\" data-thread=\"{}\" data-version=\"{}\" data-revision=\"{}\" hx-sync=\"this:drop\">",
        escape(id),
        escape(&scalar(&detail["thread"]["historyVersion"])),
        escape(&scalar(&detail["revision"]))
    );
    html.push_str(&render_meta(detail, snapshot, false));
    html.push_str("<div class=\"detail-content\">");
    html.push_str(&render_todos(detail, false));
    let _ = write!(
        html,
        r##"<section class="transcript-panel" aria-labelledby="transcript-{k}"><header class="transcript-toolbar"><h2 id="transcript-{k}">Transcript <span class="record-count muted"></span></h2><button class="pause" aria-pressed="false">Pause</button></header><div class="transcript-subbar"><label class="search-field"><span class="sr-only">Search loaded transcript only</span><input class="transcript-search" type="search" placeholder="Search loaded history…" autocomplete="off"></label><button class="jump">Follow latest</button><span class="freshness" role="status">Live · 2s refresh</span></div><p class="search-count muted" aria-live="polite"></p><div class="transcript-scroll" tabindex="0" aria-label="Thread transcript">"##
    );
    html.push_str(&render_pager(detail, false));
    let _ = write!(html, "<div class=\"events\" id=\"events-{k}\">");
    render_events(&mut html, list(detail, "events"), &k);
    html.push_str("</div><p class=\"empty-events muted\">Nothing recorded yet.</p></div><footer class=\"transcript-footer\">Read-only · recorded messages &amp; tool activity · search covers loaded records only</footer></section></div>");
    let _ = write!(
        html,
        "<div class=\"poller\" hx-get=\"/ui/poll?id={}\" hx-trigger=\"every 2s, refresh\" hx-target=\"#events-{k}\" hx-swap=\"beforeend\" aria-hidden=\"true\"></div></section>",
        url(id)
    );
    html
}

fn render_meta(detail: &Value, snapshot: &Value, oob: bool) -> String {
    let thread = &detail["thread"];
    let k = key(text(thread, "id"));
    let source_id = thread["source"]
        .as_str()
        .unwrap_or_else(|| text(&thread["source"], "id"));
    let source = list(snapshot, "sources")
        .iter()
        .find(|s| text(s, "id") == source_id);
    let source_status = source.map(|s| text(s, "status")).unwrap_or("unknown");
    let title = if text(thread, "title").is_empty() {
        "Untitled thread"
    } else {
        text(thread, "title")
    };
    let mut html = format!(
        "<header class=\"thread-header\" id=\"meta-{k}\"{}><div class=\"breadcrumbs\">{} / Thread</div><div class=\"title-row\"><h1>{}</h1>",
        if oob {
            " hx-swap-oob=\"outerHTML\""
        } else {
            ""
        },
        escape(text(thread, "botName")),
        escape(title)
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
    html.push_str("</div><p class=\"title-source\">");
    html.push_str(match text(thread, "titleSource") {
        "discord" => "Discord thread title",
        "session" => "Saved session title",
        "user_message" => "Label from first message · Discord title unavailable",
        _ => "Thread identifier · Discord title unavailable",
    });
    let _ = write!(
        html,
        "</p><div class=\"thread-metadata\">{}<span>Updated {}</span><span>{} · {}</span></div>",
        status_html(text(thread, "status")),
        time(&thread["updatedAt"]),
        escape(
            source
                .map(|s| text(s, "name"))
                .filter(|s| !s.is_empty())
                .unwrap_or("Unknown source")
        ),
        escape(if source_status == "saved" {
            "Unknown"
        } else {
            source_status
        })
    );
    if let Some(source) = source {
        let _ = write!(
            html,
            "<p class=\"collector-detail\">Collector last seen {} · {} queued batches</p>",
            if source["lastSeenAt"].is_null() {
                "unknown".into()
            } else {
                time(&source["lastSeenAt"])
            },
            source["pendingBatches"].as_u64().unwrap_or(0)
        );
    }
    if source_status != "ready" {
        html.push_str("<p class=\"source-notice\">Source is not reporting healthy live activity. Showing the last available records.");
        if !text(thread, "reportedStatus").is_empty() {
            let _ = write!(
                html,
                " Last reported task state: {}.",
                status(text(thread, "reportedStatus"))
            );
        }
        html.push_str("</p>");
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
        "<section class=\"todo-panel\" id=\"todos-{k}\"{} aria-labelledby=\"todo-heading-{k}\"><div class=\"section-heading\"><h2 id=\"todo-heading-{k}\">Task list</h2><span>{done} / {}</span></div>",
        if oob {
            " hx-swap-oob=\"outerHTML\""
        } else {
            ""
        },
        todos.len()
    );
    if todos.is_empty() {
        html.push_str("<p class=\"muted\">No task list recorded for this thread.</p>");
    } else {
        let _ = write!(
            html,
            "<progress value=\"{done}\" max=\"{}\" aria-label=\"Completed tasks\"></progress><ul class=\"todo-list\">",
            todos.len()
        );
        for todo in todos {
            let (class, label, mark) = match text(todo, "status") {
                "completed" => ("completed", "Completed", "✓"),
                "in_progress" => ("working", "In progress", "◉"),
                _ => ("pending", "Pending", "○"),
            };
            let _ = write!(
                html,
                "<li class=\"todo todo-{class}\"><span aria-label=\"{label}\">{mark}</span><span>{}</span><small>{label}</small></li>",
                escape(text(todo, "text"))
            );
        }
        html.push_str("</ul>");
    }
    html.push_str("</section>");
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
            "<button class=\"older\" hx-get=\"/ui/older?id={}&amp;before={}\" hx-target=\"#events-{k}\" hx-swap=\"afterbegin\" hx-disabled-elt=\"this\">Load older records</button>",
            url(id),
            url(cursor)
        );
    } else {
        html.push_str("<span class=\"muted\">Beginning of recorded activity</span>");
    }
    html.push_str("</div>");
    html
}

fn render_events(html: &mut String, events: &[Value], k: &str) {
    for event in events {
        let kind = text(event, "kind");
        let role = text(event, "role");
        let name = text(event, "name");
        let content = text(event, "text");
        let tool = role == "tool"
            || ["tool", "command", "exec", "result"]
                .iter()
                .any(|part| kind.contains(part));
        let error = event["isError"].as_bool().unwrap_or(false)
            || kind.contains("error")
            || kind.contains("failed");
        let label = if !name.is_empty() {
            format!("{name} · {}", kind.replace('_', " "))
        } else if tool || role.is_empty() {
            kind.replace('_', " ")
        } else {
            role.to_owned()
        };
        let _ = write!(
            html,
            "<article class=\"event {} {}\" id=\"event-{k}-{}\" data-event=\"{}\"><div class=\"event-icon\" aria-hidden=\"true\">{}</div><div class=\"event-content\">",
            if tool { "event-tool" } else { "event-message" },
            if error { "event-error" } else { "" },
            key(text(event, "id")),
            escape(text(event, "id")),
            if error {
                "!"
            } else if tool {
                "›_"
            } else if role == "user" {
                "U"
            } else {
                "A"
            }
        );
        if tool {
            let _ = write!(
                html,
                "<details{}><summary><span class=\"event-label\">{}{}</span>{}<span class=\"tool-preview\">{}</span></summary><pre class=\"tool-output\">{}</pre></details>",
                if error { " open" } else { "" },
                escape(&label),
                if error { " · Error" } else { "" },
                time(&event["timestamp"]),
                escape(
                    content
                        .lines()
                        .find(|s| !s.trim().is_empty())
                        .unwrap_or("No text recorded.")
                ),
                escape(if content.is_empty() {
                    "No text recorded."
                } else {
                    content
                })
            );
        } else {
            let _ = write!(
                html,
                "<div class=\"event-heading\"><span class=\"event-label\">{}{}</span>{}</div><div class=\"message-text\">{}</div>",
                escape(&label),
                if error { " · Error" } else { "" },
                time(&event["timestamp"]),
                escape(if content.is_empty() {
                    "No text recorded."
                } else {
                    content
                })
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
        // Walk backwards until the previously displayed tail is reached. This also
        // recovers arbitrarily many pages accumulated while paused, without holes.
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
            if scalar(&next["thread"]["historyVersion"]) != version
                || scalar(&next["revision"]) != revision
            {
                // Never commit an incomplete catch-up across concurrent replacement.
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
    html.push_str(&render_meta(&detail, &snapshot, true));
    html.push_str(&render_todos(&detail, true));
    if older || replace {
        html.push_str(&render_pager(&detail, true));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("tw-version", scalar(&detail["thread"]["historyVersion"])),
        ("tw-revision", revision),
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
    fn escapes_text_and_attribute_delimiters() {
        assert_eq!(escape("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
        let mut html = String::new();
        render_events(
            &mut html,
            &[
                json!({"id":"\" onfocus=\"bad", "kind":"tool_result", "name":"<script>", "text":"</pre><img src=x onerror=bad>", "timestamp":0}),
            ],
            "safe",
        );
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;/pre&gt;"));
        assert!(html.contains("<details>"));
    }

    #[test]
    fn discord_link_uses_actual_channel_and_rejects_non_numeric_ids() {
        let mut detail = json!({"thread":{"id":"external","guildId":"123","channelId":"456","title":"<unsafe>","titleSource":"session"},"todos":[]});
        let html = render_meta(&detail, &json!({"sources":[]}), false);
        assert!(html.contains("https://discord.com/channels/123/456"));
        assert!(html.contains("&lt;unsafe&gt;"));
        assert!(html.contains("Saved session title"));
        detail["thread"]["channelId"] = json!("javascript:alert(1)");
        assert!(
            !render_meta(&detail, &json!({"sources":[]}), false).contains("https://discord.com")
        );
    }

    #[test]
    fn zero_thread_sources_and_real_todo_progress_are_rendered() {
        let html = render_index(
            &json!({"threads":[],"sources":[{"name":"Empty collector","status":"offline","pendingBatches":7}]}),
        );
        assert!(html.contains("Empty collector"));
        assert!(html.contains("7 queued batches"));
        assert!(html.contains("offline"));
        let todos = render_todos(
            &json!({"thread":{"id":"a"},"todos":[{"text":"done","status":"completed"},{"text":"ongoing","status":"in_progress"}]}),
            false,
        );
        assert!(todos.contains("value=\"1\" max=\"2\""));
    }
}
