use regex::Regex;
use serde_json::{Map, Value, json};
use std::{borrow::Cow, sync::LazyLock};

struct Rules {
    hidden_open: Regex,
    hidden_close: Vec<(&'static str, Regex)>,
    replacements: Vec<(Regex, &'static str)>,
    hidden_key: Regex,
    secret_key: Regex,
    sensitive: Regex,
    summaries: Regex,
    checks: Regex,
}

static RULES: LazyLock<Rules> = LazyLock::new(|| {
    let tags = [
        "system-reminder",
        "system_instruction",
        "system-instruction",
        "system",
        "thinking",
        "analysis",
        "contextSnapshot",
        "providerPayload",
    ];
    Rules {
        hidden_open: Regex::new(&format!(r"(?is)<({})\b[^>]*>", tags.join("|"))).unwrap(),
        hidden_close: tags.into_iter().map(|tag| (tag, Regex::new(&format!(r"(?is)</{}\s*>", tag)).unwrap())).collect(),
        replacements: vec![
            (Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----").unwrap(), "[REDACTED]"),
            (Regex::new(r"(?i)\b(?:Bearer\s+[A-Za-z0-9._~+/\-]+=*|Bot\s+[A-Za-z0-9._~+/\-]{30,}=*)").unwrap(), "[REDACTED AUTH]"),
            (Regex::new(r"\b(?:sk-[A-Za-z0-9_\-]{16,}|gh[pousr]_[A-Za-z0-9_]{16,}|github_pat_[A-Za-z0-9_]{16,}|AKIA[A-Z0-9]{16}|mfa\.[A-Za-z0-9_\-]{20,}|[A-Za-z0-9_\-]{23,28}\.[A-Za-z0-9_\-]{6}\.[A-Za-z0-9_\-]{25,})\b").unwrap(), "[REDACTED]"),
            (Regex::new(r#"(?i)((?:[A-Za-z][A-Za-z0-9_\-]*[_\-])?(?:password|passwd|secret|token|authorization|api[_\-]?key|private[_\-]?key|cookie)\s*["']?\s*[:=]\s*)(?:"[^"\n]*"|'[^'\n]*'|[^\s,;}]+)"#).unwrap(), "${1}[REDACTED]"),
            (Regex::new(r"(?i)(https?://)[^\s/@:]+:[^\s/@]+@").unwrap(), "${1}[REDACTED]@"),
            (Regex::new(r"(?i)(https?://(?:canary\.|ptb\.)?discord(?:app)?\.com/api/webhooks/\d+/)[^\s/?#]+").unwrap(), "${1}[REDACTED]"),
        ],
        hidden_key: Regex::new(r"(?i)^(?:thinking|reasoning|analysis|system|systemPrompt|instructions|contextSnapshot|providerPayload|providerMetadata|credential.*)$").unwrap(),
        secret_key: Regex::new(r"(?i)(?:password|passwd|secret|token|authorization|api[_\-]?key|private[_\-]?key|cookie|credential)").unwrap(),
        sensitive: Regex::new(r#"(?i)(?:AGENTS\.md|SKILL\.md|(?:^|[/\\\s"'=])\.env(?:[.\s"'/\\]|$)|auth\.json|credentials?(?:[.\s"'/\\]|$)|id_(?:rsa|ed25519|ecdsa)|(?:private|secret)[_\-]?key|/(?:etc/shadow|\.ssh/)|system[_\-]?prompt)"#).unwrap(),
        summaries: Regex::new(r"(?m)^\s*(?:[-*•]\s*)?(.+?)\s+\[(pending|in_progress|completed)\](?:[^\S\n]+.*)?$").unwrap(),
        checks: Regex::new(r"(?m)^\s*[-*•]\s+\[([ xX✓])\]\s+(.+)$").unwrap(),
    }
});

pub struct Sanitizer {
    secrets: Vec<String>,
}

impl Sanitizer {
    pub fn new(mut secrets: Vec<String>) -> Self {
        secrets.retain(|secret| secret.len() >= 8);
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn text(&self, input: &str) -> String {
        let mut output = String::new();
        let mut position = 0;
        for captures in RULES.hidden_open.captures_iter(input) {
            let opening = captures.get(0).unwrap();
            if opening.start() < position {
                continue;
            }
            output.push_str(&input[position..opening.start()]);
            let tag = captures.get(1).unwrap().as_str();
            let closing = &RULES
                .hidden_close
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(tag))
                .unwrap()
                .1;
            position = closing
                .find(&input[opening.end()..])
                .map_or(input.len(), |found| opening.end() + found.end());
        }
        output.push_str(&input[position..]);
        for (expression, replacement) in &RULES.replacements {
            if let Cow::Owned(replaced) = expression.replace_all(&output, *replacement) {
                output = replaced;
            }
        }
        for secret in &self.secrets {
            if output.contains(secret) {
                output = output.replace(secret, "[REDACTED]");
            }
        }
        output
    }

    pub fn value(&self, input: &Value) -> Value {
        self.value_at(input, 0)
    }

    fn value_at(&self, input: &Value, depth: usize) -> Value {
        if depth > 30 {
            return json!("[OMITTED: excessive nesting]");
        }
        match input {
            Value::String(text) => json!(self.text(text)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.value_at(item, depth + 1))
                    .collect(),
            ),
            Value::Object(items) => Value::Object(
                items
                    .iter()
                    .filter_map(|(key, item)| {
                        if RULES.hidden_key.is_match(key) {
                            None
                        } else {
                            Some((
                                key.clone(),
                                if RULES.secret_key.is_match(key) {
                                    json!("[REDACTED]")
                                } else {
                                    self.value_at(item, depth + 1)
                                },
                            ))
                        }
                    })
                    .collect(),
            ),
            _ => input.clone(),
        }
    }
}

pub fn valid_todo_status(status: &str) -> bool {
    matches!(status, "pending" | "in_progress" | "completed")
}

pub fn normalize_todos(input: &Value, sanitizer: &Sanitizer) -> anyhow::Result<Value> {
    let items = input
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Todos must be an array"))?;
    let mut todos = Vec::with_capacity(items.len());
    for item in items {
        let text = item["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid todo"))?;
        let status = item["status"]
            .as_str()
            .filter(|status| valid_todo_status(status))
            .ok_or_else(|| anyhow::anyhow!("Invalid todo"))?;
        todos.push(json!({"text": sanitizer.text(text), "status": status}));
    }
    Ok(Value::Array(todos))
}

pub fn derive_todos(arguments: &Value, result: &str, previous: &Value) -> Value {
    if arguments["op"] == "drop" {
        return json!([]);
    }
    let summaries: Vec<Value> = RULES
        .summaries
        .captures_iter(result)
        .map(|captures| {
            json!({
                "text": captures[1].trim(), "status": &captures[2]
            })
        })
        .collect();
    let checks: Vec<Value> = RULES.checks.captures_iter(result).map(|captures| {
        let raw = captures[2].trim();
        let summary = summaries.iter().find(|item| {
            let text = item["text"].as_str().unwrap();
            raw == text || raw.strip_prefix(text).is_some_and(|suffix| suffix.starts_with(" ("))
        });
        json!({"text": summary.map_or(raw, |item| item["text"].as_str().unwrap()),
            "status": if &captures[1] != " " { "completed" } else { summary.and_then(|item| item["status"].as_str()).unwrap_or("pending") }})
    }).collect();
    if !checks.is_empty() {
        return Value::Array(checks);
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(result) {
        let items = parsed.as_array().or_else(|| parsed["todos"].as_array());
        if let Some(items) = items.filter(|items| {
            items.iter().all(|item| {
                item["status"].as_str().is_some_and(valid_todo_status)
                    && item
                        .get("text")
                        .or_else(|| item.get("content"))
                        .is_some_and(Value::is_string)
            })
        }) {
            return Value::Array(items.iter().map(|item| json!({"text": item.get("text").or_else(|| item.get("content")).unwrap(), "status": item["status"]})).collect());
        }
    }
    if !summaries.is_empty() {
        let mut todos: Vec<Value> = previous
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["status"] == "completed")
            .cloned()
            .collect();
        todos.extend(summaries);
        return Value::Array(todos);
    }
    previous.clone()
}

pub fn sensitive_tool_call(part: &Value) -> bool {
    let arguments = part
        .get("arguments")
        .or_else(|| part.get("args"))
        .unwrap_or(&Value::Null);
    RULES.sensitive.is_match(&arguments.to_string())
}

pub fn timestamp(value: &Value) -> u64 {
    match value {
        Value::Number(number) => number
            .as_u64()
            .filter(|n| *n <= 9_007_199_254_740_991)
            .unwrap_or(0),
        Value::String(text) => chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn normalize_event(event: &Value, sanitizer: &Sanitizer) -> Option<Value> {
    let kind = event["kind"].as_str()?;
    if !matches!(
        kind,
        "message"
            | "tool_call"
            | "tool_result"
            | "status"
            | "todo_edit"
            | "session_exit"
            | "tool_start"
            | "import_error"
    ) {
        return None;
    }
    let text = sanitizer.text(event["text"].as_str().unwrap_or(""));
    if text.is_empty() {
        return None;
    }
    let mut normalized = json!({"id": sanitizer.text(event["id"].as_str().unwrap_or("")), "timestamp": timestamp(&event["timestamp"]), "kind": kind, "text": text});
    if let Some(role @ ("user" | "assistant" | "toolResult")) = event["role"].as_str() {
        normalized["role"] = json!(role);
    }
    if let Some(name) = event["name"].as_str().filter(|name| !name.is_empty()) {
        normalized["name"] = json!(sanitizer.text(name));
    }
    if event["isError"] == true {
        normalized["isError"] = json!(true);
    }
    Some(normalized)
}

pub fn operational_record(
    record: &Value,
    sanitizer: &Sanitizer,
    line: u64,
    withheld: bool,
) -> Vec<Value> {
    let mut events = Vec::new();
    let time = record.get("timestamp").unwrap_or(&Value::Null);
    let message = &record["message"];
    if record["type"] == "message" {
        let Some(role @ ("user" | "assistant" | "toolResult")) = message["role"].as_str() else {
            return events;
        };
        if withheld && role == "toolResult" {
            let mut event = json!({"id": format!("{line}:withheld"), "timestamp": time, "role": role, "kind": "tool_result", "name": sanitizer.text(message["toolName"].as_str().unwrap_or("tool")), "text": "[WITHHELD: tool accessed a credential or instruction file]"});
            if message["isError"] == true {
                event["isError"] = json!(true);
            }
            return vec![event];
        }
        let string_content;
        let content = if let Some(text) = message["content"].as_str() {
            string_content = vec![json!({"type": "text", "text": text})];
            &string_content
        } else if let Some(content) = message["content"].as_array() {
            content
        } else {
            return events;
        };
        for (index, part) in content.iter().enumerate() {
            let mut event =
                json!({"id": format!("{line}:{index}"), "timestamp": time, "role": role});
            if part["type"] == "text" {
                let Some(text) = part["text"].as_str() else {
                    continue;
                };
                let text = sanitizer.text(text);
                if text.trim().is_empty() {
                    continue;
                }
                event["kind"] = json!(if role == "toolResult" {
                    "tool_result"
                } else {
                    "message"
                });
                event["text"] = json!(text);
                if let Some(name) = message["toolName"].as_str() {
                    event["name"] = json!(sanitizer.text(name));
                }
                if message["isError"] == true {
                    event["isError"] = json!(true);
                }
            } else if part["type"] == "toolCall" && role == "assistant" {
                let Some(name) = part["name"].as_str() else {
                    continue;
                };
                event["kind"] = json!("tool_call");
                event["name"] = json!(sanitizer.text(name));
                event["text"] = if sensitive_tool_call(part) {
                    json!("[WITHHELD: tool arguments target a credential or instruction file]")
                } else {
                    let mut arguments = Map::new();
                    if let Some(intent) = part["intent"].as_str() {
                        arguments.insert("intent".into(), json!(sanitizer.text(intent)));
                    }
                    if let Some(value) = part.get("arguments") {
                        arguments.insert("arguments".into(), sanitizer.value(value));
                    }
                    json!(serde_json::to_string_pretty(&arguments).unwrap())
                };
            } else {
                continue;
            }
            events.push(event);
        }
    } else if record["type"] == "custom"
        && record["customType"] == "session_exit"
        && record["data"].is_object()
    {
        let data = &record["data"];
        let visible: Map<String, Value> = ["kind", "reason", "recordedAt"]
            .into_iter()
            .filter_map(|key| data.get(key).map(|value| (key.into(), value.clone())))
            .collect();
        events.push(json!({"id": format!("{line}:exit"), "timestamp": time, "kind": "session_exit", "text": sanitizer.value(&Value::Object(visible)).to_string()}));
    } else if record["type"] == "custom" && record["customType"] == "tool_execution_start" {
        let data = &record["data"];
        if let Some(name) = data["toolName"].as_str() {
            let text = if sensitive_tool_call(data) {
                "[WITHHELD: tool arguments target a credential or instruction file]".to_owned()
            } else {
                let visible: Map<String, Value> = [
                    ("intent", "intent"),
                    ("arguments", "args"),
                    ("startedAt", "startedAt"),
                ]
                .into_iter()
                .filter_map(|(key, source)| {
                    data.get(source).map(|value| (key.into(), value.clone()))
                })
                .collect();
                sanitizer.value(&Value::Object(visible)).to_string()
            };
            events.push(json!({"id": format!("{line}:start"), "timestamp": time, "kind": "tool_start", "name": sanitizer.text(name), "text": text}));
        }
    }
    events
}
