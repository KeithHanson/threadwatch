# Threadwatch

A Rust web dashboard and collector for bot threads, actual todos, and operational transcripts. The interface is server-rendered HTML updated by htmx. No Node runtime, React, npm install, frontend build, or Discord Activity setup.

## Architecture

```text
Bridge SQLite + session JSONL → Rust collector + durable queue
                                         ↓ HTTP/shared token
                               Rust server + SQLite
                                         ↓
                              HTML + htmx → browser
```

One binary provides `serve`, `collect`, and `install-service`. Static assets, including pinned htmx 2.0.8, are embedded; the browser does not fetch CDN resources. The collector reads its bridge without changing or restarting it.

## Build

Requires a Rust toolchain supporting edition 2024, a C compiler for bundled SQLite, and Cargo.

```sh
cargo build --locked --release
./target/release/threadwatch --help
```

Distribute the resulting `threadwatch` executable to compatible hosts. Neither source files nor Rust are needed at runtime. Use a build matching the host architecture and system libraries.

## Central server

Copy `deploy/server.env.example` to a private environment file owned by the service user. Use absolute paths. Create one mode-600 shared token file with at least 32 random characters and distribute its value privately to trusted collectors.

```sh
threadwatch --env-file /absolute/path/server.env serve
```

The server receives activity through collectors; it does not also import the same bridge directly. `TW_DATA_DIR` holds `threadwatch.sqlite`. Existing stored threads, delivery receipts, and heartbeat records are reused.

Viewer access is explicitly trusted: `TW_LOCAL=1` defaults to `127.0.0.1:8787`. For the LAN, also set `TW_LAN=1` and a specific private IPv4 `TW_HOST`. Allow TCP 8787 in the host firewall only from the trusted subnet. Wildcard/public unauthenticated binds and foreign Host/Origin requests are rejected.

For this deployment, browse **http://192.168.10.5:8787**. Reachable LAN devices can read all connected transcripts without login. HTTP is unencrypted. No public tunnels or router forwarding.

Browser-facing asset, navigation, and htmx URLs are relative, so a reverse proxy may publish the dashboard beneath a stripped path prefix such as `/threadwatch/`. The proxy must preserve the trailing slash on the entry URL.

| Variable | Purpose |
| --- | --- |
| `TW_LOCAL` | Required `1` for trusted viewing. |
| `TW_LAN` | Optional `1` to allow private-network peers on a private IPv4 bind. |
| `TW_HOST`, `TW_PORT` | Defaults `127.0.0.1:8787`. |
| `TW_DATA_DIR` | Central server database directory. |
| `TW_INGEST_TOKEN_FILE` | Private shared ingestion-token file. |
| `TW_INGEST_TOKEN` | Alternative inline environment token; do not set both token options. |

## Each collector host

Configure exactly one bridge in a JSON file based on `bridges.example.json`. Give each bot a unique stable `id` and readable `name`. These distinguish sources, not security principals; collectors share one token and trust each other.

Set the local bridge database and session directory paths. Optional `botTokenFile` permits read-only Discord thread-title lookup; that credential stays on the collector host.

Copy `deploy/collector.env.example` to a mode-600 file and adjust the server URL, bridge JSON path, token-file path, and writable collector state directory.

```sh
# Backfill and deliver once; returns nonzero for unavailable sources/undelivered data.
threadwatch --env-file /absolute/path/collector.env collect --once
# Continuous operation.
threadwatch --env-file /absolute/path/collector.env collect
```

Use `TW_COLLECTOR_DATA_DIR` separately from the server's state directory. Keep it across upgrades: it contains captured history, delivery progress, and pending batches. Only one collector may own a state directory. Existing Node-era SQLite queues and delivery IDs are migrated/reused without discarding pending work.

Collection defaults to every two seconds (`TW_COLLECT_INTERVAL_MS`). Heartbeats default to ten seconds (`TW_HEARTBEAT_INTERVAL_MS`, maximum 30 seconds). Source collection does not block heartbeat or delivery during backfill. Batches are retried with stable IDs after outages; the receiver transactionally deduplicates retries, including history resets. Large text is split into labeled Unicode-safe continuation records, not silently truncated.

Do not point an existing queue at a different server or reuse a bot ID for another bridge. The collector needs read access to its own bridge files, not network-shared databases.

## Interface

The dashboard groups working and historical threads by bot. It shows actual todos, messages, expandable tool starts/calls/results, errors, titles, and source health. Search, older-history loading, and pause/follow support long transcripts. Updates poll saved records every two seconds, not token-by-token model output.

The server marks reporting offline after 45 seconds without a heartbeat. Active task states then show Unknown, retaining the last reported state. This detects collector reporting loss, not necessarily a stopped model process. Queued batches and sources with no threads remain visible.

## Automatic startup

Install the binary at a stable executable path first. The native installer writes systemd units, validates them, enables boot startup, and restarts the selected service as a non-root user:

```sh
sudo /usr/local/bin/threadwatch --env-file /absolute/path/server.env install-service server --user SERVICE_USER
sudo /usr/local/bin/threadwatch --env-file /absolute/path/collector.env install-service collector --user SERVICE_USER
```

Replace `SERVICE_USER` with the account owning the private environment file and runtime data. Install only the relevant role on each host. Stop manually launched copies before installing. The unit uses that exact executable and environment file, with the environment file's directory as its working directory.

Inspect `systemctl status threadwatch.service` or `systemctl status threadwatch-collector.service`; use `journalctl -u` for their logs. Replace an installed executable by staging a new file and atomically renaming it, then restart services. Do not overwrite a running executable in place.

For upgrades from Node, stop the old collector/server before starting Rust on the same databases. Back up SQLite with its backup API, not a raw copy of a live WAL database. Reuse the existing state and token files; remove the obsolete server-side `TW_BRIDGES_FILE` setting. The Discord bridge itself does not need restarting.


## Connecting other runners

Set `TW_INGEST_TOKEN` to a random secret of at least 32 characters. A trusted runner sends `POST /api/ingest` with `Authorization: Bearer <token>` and `Content-Type: application/json`:

```json
{
  "thread": {
    "id": "runner-thread-id",
    "botId": "worker-one",
    "botName": "Worker One",
    "title": "Repair package installation",
    "guildId": null,
    "channelId": null,
    "status": "working",
    "updatedAt": 1794614400000
  },
  "events": [
    {
      "id": "stable-event-id",
      "timestamp": 1794614400000,
      "kind": "message",
      "role": "assistant",
      "text": "Checking the dependency conflict."
    }
  ],
  "todos": [
    {"text": "Identify dependency conflict", "status": "in_progress"}
  ]
}
```

Use current timestamps, stable bot/thread/event IDs, and chronological events. Reusing an event ID updates that event rather than duplicating it. Omit `todos` to preserve the previous list; send `[]` to clear it. Body limit: 1 MiB, at most 1,000 events and 2,000 todos per request.

Thread states: `working`, `queued`, `waiting`, `completed`, `failed`, `unknown`, `stopped`. Todo states: `pending`, `in_progress`, `completed`. Event kinds: `message`, `tool_call`, `tool_start`, `tool_result`, `status`, `todo_edit`, `session_exit`, `import_error`. Never send hidden reasoning or credentials.

The ingest credential grants write access for trusted producers; it is not a viewer token or an identity boundary between mutually untrusted bots. The service returns its encoded thread ID and revision. Read endpoints are `GET /api/threads` and `GET /api/threads/:id?limit=100&before=<cursor>`; pages are chronological, with an opaque thread-bound cursor for earlier history.

Durable producers may include `delivery: {"id": "stable-batch-id", "reset": false}`. Retries must preserve the exact batch and ID. A first replacement batch uses `reset: true`; subsequent chunks use `false`. A reset changes the returned thread's `historyVersion`, allowing viewers to discard replaced history. `POST /api/heartbeat` uses the same bearer token and accepts `botId`, `botName`, `status` (`ready`, `degraded`, or `unavailable`), `pendingBatches`, and an optional sanitized error summary.


## Transcript boundaries

Only operational messages and tool activity belong in this app. Hidden thinking, system messages, provider payloads, and credential metadata must not be forwarded. Secret-pattern redaction reduces accidental leakage, but cannot recognize every arbitrary secret embedded in free text. Use trusted operators and protect the source files, database, transport, and host.

The adapter can only show records the runner has persisted. Missing records, disconnected sources, and unavailable history must not be mistaken for complete live execution traces. Unknown provider/custom records and nontext attachments are not rendered. Thread status comes from persisted bridge state, not a bot heartbeat.

## Development and third-party assets

```sh
cargo test --locked
cargo build --locked --release
```

Rust tests cover protocol, persistence, collector delivery, and escaping. Verify interface changes in a real browser.

`static/htmx.min.js` is htmx 2.0.8 under the bundled `static/htmx.LICENSE` (0BSD), from https://unpkg.com/htmx.org@2.0.8/dist/htmx.min.js. SHA-256: `22283ef68cb7545914f0a88a1bdedc7256a703d1d580c1d255217d0a50d31313`.

Do not commit token files, local bridge configuration, transcript databases, or runtime state.
