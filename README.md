# Threadwatch

A Rust web dashboard for bot threads, actual todos, and operational transcripts. The interface is server-rendered HTML updated by htmx. No Node runtime, React, npm install, frontend build, or Discord Activity setup.

## Architecture

```text
OMP Discord bridge + integrated durable reporter
                  ↕ authenticated HTTP
               Threadwatch server + SQLite
                         ↓
                HTML + htmx → browser
```

This repository provides the central `serve` runtime and its systemd installer. Static assets, including pinned htmx 2.0.8, are embedded; the browser does not fetch CDN resources. Bridge collection, delivery, and command admission live in `rust-omp-discord-bridge`, keeping one owner for bridge files, lifecycle, credentials, reporting state, and OMP sessions.

## Build

Requires a Rust toolchain supporting edition 2024, a C compiler for bundled SQLite, and Cargo.

```sh
cargo build --locked --release
./target/release/threadwatch --help
```

Distribute the resulting `threadwatch` executable to compatible hosts. Neither source files nor Rust are needed at runtime. Use a build matching the host architecture and system libraries.

## Central server

Copy `deploy/server.env.example` to a private environment file owned by the service user. Use absolute paths. Create separate mode-600 ingestion and control token files with at least 32 random characters. Distribute the ingestion token privately to trusted bridges; keep the control token with operators.

```sh
threadwatch --env-file /absolute/path/server.env serve
```

The server receives activity through reporting bridges; it does not import bridge files directly. `TW_DATA_DIR` holds `threadwatch.sqlite`. Existing stored threads, delivery receipts, and heartbeat records are reused.

Viewer access is explicitly trusted: `TW_LOCAL=1` defaults to `127.0.0.1:8787`. For a private network, also set `TW_LAN=1` and a specific private IPv4 `TW_HOST`. Allow TCP 8787 in the host firewall only from that trusted subnet. Wildcard/public unauthenticated binds and foreign Host/Origin requests are rejected. Sending chat messages additionally requires `TW_CONTROL_TOKEN`; read access never grants control.

For this deployment, browse **http://192.168.192.183:8787**. Reachable ZeroTier peers can read all connected transcripts without login. HTTP is unencrypted; ZeroTier supplies transport encryption. No public tunnels or router forwarding.

Browser-facing asset, navigation, and htmx URLs are relative, so a reverse proxy may publish the dashboard beneath a stripped path prefix such as `/threadwatch/`. The proxy must preserve the trailing slash on the entry URL. Set `TW_ALLOWED_HOSTS` to the proxy's comma-separated public hostnames when it preserves the original `Host` header.

| Variable | Purpose |
| --- | --- |
| `TW_LOCAL` | Required `1` for trusted viewing. |
| `TW_LAN` | Optional `1` to allow private-network peers on a private IPv4 bind. |
| `TW_HOST`, `TW_PORT` | Defaults `127.0.0.1:8787`. |
| `TW_ALLOWED_HOSTS` | Optional comma-separated public reverse-proxy hostnames accepted in addition to `TW_HOST`. |
| `TW_DATA_DIR` | Central server database directory. |
| `TW_INGEST_TOKEN_FILE` | Private shared ingestion-token file. |
| `TW_INGEST_TOKEN` | Alternative inline environment token; do not set both token options. |
| `TW_CONTROL_TOKEN_FILE` | Private operator control-token file used by the chat composer. |
| `TW_CONTROL_TOKEN` | Alternative inline control token; do not set both control-token options. |

## Bridge reporters

Configure each `rust-omp-discord-bridge` instance with its optional `[threadwatch]` table. The bridge reads its own SQLite database and OMP session directory, preserves a durable delivery queue, and sends authenticated ingestion batches and heartbeats to this server.

Each bridge needs a unique stable bot ID, the shared ingestion token, and a persistent reporter data directory. Preserve that directory across bridge upgrades: it contains captured history, delivery progress, pending batches, and stable delivery IDs. Never run the old standalone collector and the integrated bridge reporter against the same state directory simultaneously.

During migration, stop the standalone `threadwatch-collector` service before enabling `[threadwatch]` in the bridge. Point the bridge at the existing collector state directory to preserve queue/checkpoint continuity. Give the bridge's startup identity read access to the ingestion token; do not copy another bot's Discord credential or rotate the shared token as part of this cutover.

Collection defaults to every two seconds and heartbeats to ten seconds. Source collection does not block heartbeat or delivery during backfill. Batches retry with stable IDs after outages; the receiver transactionally deduplicates retries, including history resets. Large text is split into labeled Unicode-safe continuation records, not silently truncated.

The bridge's existing Discord token provides optional read-only thread-title and bot-avatar lookup. No separate collector credential or bridge JSON file remains.

## Interface

The dashboard opens on a full-width Agent map, without a global sidebar. Bot cards connect to their threads with short titles, task counts, latest-record previews, and collector health. Active shows Working, Queued, and Waiting; Past shows Completed, Failed, Stopped, and Unknown. This filter is shared between the map and focused agent rail and survives in-page navigation. Offline-source alerts explain reporting-loss transitions to Unknown and link to Past; Unknown does not mean completed.

Select a bot card to open its most recently updated thread in the current filter. If that bot has no matching threads, the filter switches and its most recent thread opens instead. Selecting a thread directly opens that thread. Thread mode shows only that bot's identity, collector health, and filtered threads in the left rail. Return through Agent map to switch bots. Collectors without threads remain visible as noninteractive map cards. The old global title/bot search and Reporting sources panel have been removed; transcript search remains available.

Select a thread for its recorded task list and chronological transcript. Tasks collapse on small/short screens; messages, tool starts/calls/results, and operational notices have separate presentations. Tool errors expand by default. Search covers loaded records only; load older pages to search farther back. Pause freezes the selected detail, not collection, and shows the number of newly recorded events waiting. Scrolling away from the bottom holds your position while new activity arrives; Follow latest returns to the bottom without resuming a paused view. History replacement clears the old generation and displays a notice rather than mixing records.

The Chat panel sends an operator message through Threadwatch to the reporting bridge that owns the selected thread. The browser keeps the control token in session storage only. Commands remain pending until that bridge accepts or rejects them; accepted means durably admitted to the bridge turn queue, not completed by the model. Offline/unknown sources disable the composer. The bridge remains authoritative for session state, interruption, queue limits, and Discord delivery.

Tool cards lead with the recorded intent and show commands, code, paths, queries, proposed patches, or task details without requiring JSON inspection. Device/API writes are distinguished from file writes. Full sanitized records remain expandable; malformed, unknown, and withheld inputs retain honest fallbacks. Result receipts do not imply success, and recognized shell background-launch receipts do not imply job completion.

Calls, starts, and results combine only when the loaded records carry the same explicit session ID, tool-call ID, and tool name. Interleaved operations and reused IDs in different sessions stay separate; legacy records without both IDs are never guessed into groups. Loading older pages can complete a partial group. Search, pause counts, and pagination still count original records, while expansion, keyboard focus, and held-scroll anchors survive incoming results.

Upgrade the server before bridges to retain new optional metadata. Updated bridges attach native session/call IDs to new records when available, including after resuming an older checkpoint with a recoverable session header. Existing stored history is not rewritten; legacy inputs still get readable summaries from their sanitized text.

Updates poll saved records every two seconds, not token-by-token model output. The server marks reporting offline after 45 seconds without a heartbeat. Active task states then show Unknown, retaining the last reported state. This detects collector reporting loss, not necessarily a stopped model process. Browser connection health, collector health, task status, and local pause/follow state remain separate.

On mobile, the map has no drawer or health strip: reporting health is on the bot cards and map alert. Thread mode provides a Map link and a keyboard-accessible Threads drawer containing only the selected bot. Instrument Sans and JetBrains Mono are bundled in the binary; no font CDN is required. Bot avatars use validated Discord CDN URLs when available, otherwise an initial-letter fallback. The reporting bridge can obtain its bot avatar through its existing Discord credential.

## Automatic startup

Install the binary at a stable executable path first. The native installer writes, validates, enables, and restarts the server service as a non-root user:

```sh
sudo /usr/local/bin/threadwatch --env-file /absolute/path/server.env install-service --user SERVICE_USER
```

Replace `SERVICE_USER` with the account owning the private environment file and runtime data. Stop manually launched copies before installing. The unit uses that exact executable and environment file, with the environment file's directory as its working directory.

Inspect `systemctl status threadwatch.service`; use `journalctl -u threadwatch.service` for logs. Replace an installed executable by staging a new file and atomically renaming it, then restart the service. Do not overwrite a running executable in place.

For upgrades from older Threadwatch releases, stop the old server before starting Rust on the same database. Back up SQLite with its backup API, not a raw copy of a live WAL database. Reuse the existing state and token files.

### Services host: Infisical-managed startup

[`deploy/infisical/`](deploy/infisical/) contains the authoritative services-host server Agent configuration and complete `threadwatch.service` unit. They render the server environment and ingestion token into `/run/threadwatch` before startup. Infisical must remain independently recoverable; repository changes alone do not deploy or restart the service.

Standalone collector Agent/unit assets were removed. Collector identities and services should be retired only after their corresponding bridges run the integrated reporter with the existing queue state and a fresh ready heartbeat.

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

Tool events may additionally carry `toolCallId`, `sessionId`, an `arguments` object, and `intent`. IDs are bounded to 200 UTF-16 units and intent to 4,000; structured arguments are bounded to 64 KiB, depth 30, and 4,096 nodes. These fields are sanitized before storage, and withheld arguments remain withheld rather than being restored through structured metadata. Keep the original sanitized `text` for inspection and older consumers.

The ingest credential grants write access for trusted producers; it is not a viewer token or an identity boundary between mutually untrusted bots. The service returns its encoded thread ID and revision. Read endpoints are `GET /api/threads` and `GET /api/threads/:id?limit=100&before=<cursor>`; pages are chronological, with an opaque thread-bound cursor for earlier history. The index includes `shortTitle`, `todoCount`, `todoCompleted`, `eventCount`, and a bounded `latestEvent` preview without loading full transcripts. Detail includes `eventCount` from the same stored snapshot as its records, for accurate paused-view baselines. Producers may supply a short title and a validated Discord `avatarUrl`; missing short titles fall back to the first 24 Unicode characters of the full title.

Durable producers may include `delivery: {"id": "stable-batch-id", "reset": false}`. Retries must preserve the exact batch and ID. A first replacement batch uses `reset: true`; subsequent chunks use `false`. A reset changes the returned thread's `historyVersion`, allowing viewers to discard replaced history. `POST /api/heartbeat` uses the same bearer token and accepts `botId`, `botName`, `status` (`ready`, `degraded`, or `unavailable`), `pendingBatches`, an optional sanitized error summary, and optional `avatarUrl`.

Avatar URLs must use `https://cdn.discordapp.com/avatars/<numeric-user-id>/<32-hex-hash>.png`, optionally with an `a_` hash prefix and `?size=96`. Other origins, credentials, paths, and query strings are rejected. The collector caches its optional Discord identity lookup for an hour, shares Discord rate-limit handling with title lookups, and retains only safe identity metadata. Avatar availability does not gate reporting.


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

The redesign bundles Latin variable WOFF2 subsets of Instrument Sans (400–600) and JetBrains Mono (400–500), distributed under the SIL Open Font License. Their licenses are in `static/instrument-sans.LICENSE` and `static/jetbrains-mono.LICENSE`. Font files originate from Google Fonts; browsers load them locally from the embedded assets. Other character ranges use system fallbacks.

Do not commit token files, local bridge configuration, transcript databases, or runtime state.
