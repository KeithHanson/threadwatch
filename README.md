# Threadwatch

A read-only web dashboard for inspecting bots, their working threads, actual todos, and operational transcripts. The central server and independently runnable collector share this repository.

## Interface

Select a bot's thread in the sidebar to inspect its todos and transcript. Tool calls and results are expandable. Search, pause/follow, and older-history loading make long sessions navigable. The app polls saved state every two seconds; this is not token-by-token model streaming.

The OMP bridge adapter reads the bridge database and session files without changing them or restarting the bridge. Other runners can publish through the authenticated ingestion API. A source must be connected before its bots appear: the app cannot automatically observe other machines or Discord conversations it has not been given.

## Local operation

Requires Node.js 24 or newer with `node:sqlite` support.

```sh
npm ci
npm run build
cp bridges.example.json bridges.local.json
# Edit paths and bot identity in bridges.local.json.
TW_LOCAL=1 TW_BRIDGES_FILE=./bridges.local.json npm start
```

Local mode defaults to loopback and does not require Discord login. Never publish it to the internet. Transcript data is sensitive even after redaction.

Open `http://127.0.0.1:8787` on the host. Configuration can also be loaded with `node --env-file=.env server/index.mjs` after copying `.env.example` to `.env`.

### Trusted LAN access

Explicitly opt in to unauthenticated LAN viewing with a specific private IPv4 address:

```sh
TW_LOCAL=1 TW_LAN=1 TW_HOST=192.168.10.5 TW_BRIDGES_FILE=./bridges.local.json npm start
```

Open `http://192.168.10.5:8787` from another device on that network. Replace the address with the server's LAN address. Every reachable private-network device can read all connected transcripts without signing in; HTTP is unencrypted. Use only on a trusted network, with no public tunnel or router port forwarding. Wildcard/public bind addresses are rejected. Host/Origin protections remain enabled and ingestion still requires its separate token.

| Variable | Purpose |
| --- | --- |
| `TW_LOCAL` | Set `1` for unauthenticated viewing (loopback unless `TW_LAN=1`); `0` for Discord authentication. |
| `TW_LAN` | Set `1` with `TW_LOCAL=1` and a private IPv4 `TW_HOST` to allow trusted-LAN viewing without login. |
| `TW_HOST`, `TW_PORT` | Listener address and port; defaults `127.0.0.1:8787`. |
| `TW_BRIDGES_FILE` | JSON array of named read-only bridge sources. |
| `TW_DATA_DIR` | Persistent ingestion database directory. |
| `TW_DISCORD_CLIENT_ID` | Discord application ID. |
| `TW_DISCORD_CLIENT_SECRET` | Server-only OAuth secret. |
| `TW_ALLOWED_USER_IDS` | Comma-separated operator Discord IDs. |
| `TW_INGEST_TOKEN` | Separate credential for trusted event producers. |
| `TW_INGEST_TOKEN_FILE` | Mode-600 shared ingestion token file; use instead of `TW_INGEST_TOKEN`, not alongside it. |

Without `TW_BRIDGES_FILE`, the server uses the local OMP bridge under `~/.local/state/omp-discord-bridge`. Set a source file containing `[]` for ingestion-only operation. Without `TW_DATA_DIR`, ingested state is stored under `~/.local/state/threadwatch`.

Each bridge source may also set `botTokenFile` to a protected Discord bot-token file. This enables read-only Discord thread-title lookup; the token never goes to the browser. Without it, titles come from the saved session or a label derived from its first user message. Keep token paths in ignored local configuration.

## Reporting from multiple bot hosts

Run one collector alongside each Discord bridge. Each bridge needs a unique stable `id` and a human-readable `name` in its source JSON; these are identifiers, not separate credentials. All collectors use the same ingestion token.

```text
Bridge + sessions → local collector + durable queue → HTTP → central Threadwatch
```

The collector reads local bridge/session state, sanitizes it before transmission, and sends changed records. Initial history is backfilled. Queued batches and delivery IDs survive outages and restarts; the server deduplicates acknowledged deliveries, including transcript replacements. Large text records are split into labeled continuation records rather than silently truncated.

Collector heartbeats report source health and queued batches. The dashboard marks reporting offline after 45 seconds without a heartbeat; this means the collector is unreachable, not necessarily that the model stopped. Working/queued/waiting task states become unknown while reporting is offline. A collector with no threads still appears under Reporting sources.

### Central server

1. Build the dashboard with `npm ci && npm run build`.
2. Create a protected shared token file (at least 32 random characters) outside the checkout. Share its value privately with the other trusted hosts.
3. Copy `deploy/server.env.example` to a mode-600 file owned by the service user and adjust paths/address. Set `TW_BRIDGES_FILE` to a JSON file containing `[]` (see `deploy/no-bridges.json`) so the server does not also import the same bridge directly.
4. Run `node --env-file=/absolute/path/server.env server/index.mjs`.
5. Allow TCP 8787 in the host firewall from the trusted subnet only. Do not expose this unauthenticated HTTP deployment publicly.

### Each collector host

Requires Node.js 24 or newer. Clone this repo; **no `npm install`, frontend build, or dashboard process is needed** for a collector-only host.

1. Copy `bridges.example.json` outside the checkout. Configure exactly one bridge, with a unique bot `id`/`name` and the local database/session paths. Optional `botTokenFile` enables Discord titles.
2. Copy `deploy/collector.env.example` to a mode-600 file owned by the bot's service user. Set the central server URL, source-file path, shared-token-file path, and a dedicated writable state directory.
3. Store the shared token in a file readable only by the service user. The collector must be able to read its own bridge database and session files; do not share those files over the network.
4. Exercise collection and delivery:

```sh
node --env-file=/absolute/path/collector.env collector/index.mjs --once
```

5. Run continuously without `--once`, or install the service below. Keep the collector state directory across upgrades: it contains its delivery progress and pending queue. Use exactly one collector process per bot identity/state directory.

`TW_COLLECT_INTERVAL_MS` defaults to 2,000; `TW_HEARTBEAT_INTERVAL_MS` defaults to 10,000. Set `TW_COLLECTOR_DATA_DIR` separately from the server's `TW_DATA_DIR`. Do not change destination/token identity while carrying a queue for a different server.

### Automatic startup

The installer creates a systemd system service running as the specified **non-root** user, enables it at boot, and restarts it. Run from the checkout on the intended host; stop any manually launched copy first.

```sh
sudo "$(node -p 'process.execPath')" scripts/install-service.mjs server /absolute/path/server.env SERVICE_USER
sudo "$(node -p 'process.execPath')" scripts/install-service.mjs collector /absolute/path/collector.env SERVICE_USER
```

Replace `SERVICE_USER` with the local account owning the environment file and runtime data. Install only the applicable role on each host. Environment files must be mode 600 and owned by that account. The generated unit records the actual Node executable, checkout path, and selected environment file; rerun the installer after moving the checkout or Node installation.

Inspect with `systemctl status threadwatch.service` or `systemctl status threadwatch-collector.service`; logs are available through `journalctl -u` for the corresponding unit. Other machines are not configured automatically.


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

## Optional Discord Activity setup

1. Create an application in the [Discord Developer Portal](https://discord.com/developers/applications). Enable Activities and the desired user/guild installation contexts.
2. Add the OAuth redirect URI `https://127.0.0.1`, as in Discord's Activity tutorial. The Embedded App SDK handles the embedded authorization flow.
3. Configure the server with the application's client ID, client secret, and an explicit allowlist of operator Discord user IDs. Secrets belong in server-side environment configuration, never frontend variables or commits.
4. Serve the app and API together through an approved HTTPS endpoint. Do not expose the local unauthenticated mode.
5. Add a root `/` Activity URL mapping to that HTTPS host. All app requests use same-origin paths.
6. Enable Discord Developer Mode and launch the Activity from the App Launcher using the developer/team account. Non-distributed Activities have developer/team access restrictions.
7. Confirm that an allowed operator sees real thread data and an unlisted user is denied. Verify the actual embedded interface, not just a browser build.

Discord login identifies the user on the server via Discord's API. The operator allowlist grants access to **all configured sources**. This is an operator console, not a guild-wide dashboard: it does not infer private-thread membership or offer per-channel authorization. Do not add users who should not see every connected source.

Public hosting and Developer Portal configuration are separate deployment steps; running locally does not make the app launchable in Discord.

## Transcript boundaries

Only operational messages and tool activity belong in this app. Hidden thinking, system messages, provider payloads, and credential metadata must not be forwarded. Secret-pattern redaction reduces accidental leakage, but cannot recognize every arbitrary secret embedded in free text. Use trusted operators and protect the source files, database, transport, and host.

The adapter can only show records the runner has persisted. Missing records, disconnected sources, and unavailable history must not be mistaken for complete live execution traces. Unknown provider/custom records and nontext attachments are not rendered. Thread status comes from persisted bridge state, not a bot heartbeat.

## Development

```sh
npm test
npm run build
```

No bot restart is required for the read-only adapter. Do not commit local source configuration, session files, databases, OAuth credentials, or ingest tokens.

## References

- [Building a Discord Activity](https://docs.discord.com/developers/activities/building-an-activity)
- [Activity networking and security](https://docs.discord.com/developers/activities/development-guides/networking)
