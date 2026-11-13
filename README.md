# Threadwatch

A read-only Discord Activity for inspecting bots, their working threads, actual todos, and operational transcripts.

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

Local mode is for the machine owner, binds to loopback, and does not require Discord login. Never publish or tunnel local mode. Transcript data is sensitive even after redaction.

Open `http://127.0.0.1:8787` on the host. Configuration can also be loaded with `node --env-file=.env server/index.mjs` after copying `.env.example` to `.env`.

| Variable | Purpose |
| --- | --- |
| `TW_LOCAL` | Set `1` for loopback-only operator access; `0` for Discord authentication. |
| `TW_HOST`, `TW_PORT` | Listener address and port; defaults `127.0.0.1:8787`. |
| `TW_BRIDGES_FILE` | JSON array of named read-only bridge sources. |
| `TW_DATA_DIR` | Persistent ingestion database directory. |
| `TW_DISCORD_CLIENT_ID` | Discord application ID. |
| `TW_DISCORD_CLIENT_SECRET` | Server-only OAuth secret. |
| `TW_ALLOWED_USER_IDS` | Comma-separated operator Discord IDs. |
| `TW_INGEST_TOKEN` | Separate credential for trusted event producers. |

Without `TW_BRIDGES_FILE`, the server uses the local OMP bridge under `~/.local/state/omp-discord-bridge`. Set a source file containing `[]` for ingestion-only operation. Without `TW_DATA_DIR`, ingested state is stored under `~/.local/state/threadwatch`.

Each bridge source may also set `botTokenFile` to a protected Discord bot-token file. This enables read-only Discord thread-title lookup; the token never goes to the browser. Without it, titles come from the saved session or a label derived from its first user message. Keep token paths in ignored local configuration.

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

Thread states: `working`, `queued`, `waiting`, `completed`, `failed`, `unknown`, `stopped`. Todo states: `pending`, `in_progress`, `completed`. Event kinds: `message`, `tool_call`, `tool_result`, `status`, `todo_edit`, `session_exit`. Never send hidden reasoning or credentials.

The ingest credential grants write access for trusted producers; it is not a viewer token or an identity boundary between mutually untrusted bots. The service returns its encoded thread ID and revision. Read endpoints are `GET /api/threads` and `GET /api/threads/:id?limit=100&before=<cursor>`; pages are chronological, with an opaque thread-bound cursor for earlier history.

## Discord Activity setup

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
