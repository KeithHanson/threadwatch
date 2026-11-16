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

Viewer access is explicitly trusted: `TW_LOCAL=1` defaults to `127.0.0.1:8787`. For a private network, also set `TW_LAN=1` and a specific private IPv4 `TW_HOST`. Allow TCP 8787 in the host firewall only from that trusted subnet. Wildcard/public unauthenticated binds and foreign Host/Origin requests are rejected.

For this deployment, browse **http://192.0.2.10:8787**. Reachable ZeroTier peers can read all connected transcripts without login. HTTP is unencrypted; ZeroTier supplies transport encryption. No public tunnels or router forwarding.

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

### Fleet collectors: Infisical on system or user managers

Keep the installed Threadwatch binary. This credential migration needs deployment
assets and per-host enrollment, not a source build.

For **user services**, use `deploy/infisical/user/threadwatch-collector.service`
and `deploy/infisical/user/collector-agent.yaml`. The YAML example assumes
`keith`, UID 1000: adapt every home/runtime path to the actual account. Preserve
the existing `TW_BRIDGES_FILE`, `TW_COLLECTOR_DATA_DIR`, source ID, checkpoints and
queue rather than adopting an example path. Keep the server at
`http://192.0.2.10:8787`.

- Install the YAML at `~/.config/threadwatch/collector-agent.yaml` and the unit
  at `~/.config/systemd/user/threadwatch-collector.service`.
- Put the dedicated **collector** identity's `collector-client-id` and
  `collector-client-secret` in `~/.config/threadwatch/credentials/`, files 0600,
  directory 0700. Do not substitute bridge, administrator or another bot's
  bootstrap. Files remain readable by the owning Unix user.
- Select the actual installed CLI and Threadwatch executable paths in the unit.
  The example uses `~/.local/bin/infisical` and `/usr/local/bin/threadwatch`.
- Use `systemd-analyze --user verify` and `systemctl --user`; preserve existing
  enabled/linger settings. User services must not contain `User=threadwatch`,
  system Docker dependencies or privileged command prefixes.

For **system collectors** outside services, adapt the existing system example's
User, working directory, CLI, bootstrap sources and host-specific YAML paths.
Use the dedicated collector identity rather than the historical services-host
`server-client-*` files. The direct Infisical endpoint is
`http://192.0.2.10:8086`, not that remote host's loopback address.

Both variants fetch Threadwatch project `<PROJECT_ID>`,
`dev`, settings `/collector` and `/file-credentials/TW_INGEST_TOKEN`. Existing
ingestion credentials are unchanged. If optional Discord lookup is configured,
its `botTokenFile` must use that bot's own Infisical-rendered token, never another
bot's token. Do not remove a shared token file until every local consumer has
migrated; collector ingestion itself does not require Discord lookup.

Back up selected units/configuration privately. First prove Agent rendering in a
separately named unit/runtime directory without launching another collector
against live state. Then stop only the affected collector, install/reload/start,
and verify actual Agent startup, 0700 runtime/0600 files, canonical token match,
fresh ready heartbeat and queue drainage. Restore previous configuration, not
stale queue/database data, if rollback is needed. Never use the generic
`install-service` command afterward to overwrite credential-aware units.

User-manager proof on 2026-11-16: a real Agent fetch and the actual Rust `dotenvy`
parser consumed the rendered configuration; token bytes, host overrides and
0700/0600 permissions matched. A missing-secret start failed. Runtime files were
removed after both oneshot proof runs; isolated units and bootstrap copies were
cleaned. No production collector or bridge was restarted by this proof.

### Services host: Infisical-managed startup

[`deploy/infisical/`](deploy/infisical/) is the authoritative, versioned deployment for the **services host**, superseding the untracked `/home/services/threadwatch-migration` assets and historical host-specific copies. It contains complete `threadwatch.service` and `threadwatch-collector.service` units, plus `server-agent.yaml` and `collector-agent.yaml`. These are not Hephaestus units: the server is **http://192.0.2.10:8787**, and Infisical is local to services at **http://127.0.0.1:8086**. Services-bot owns installation, configuration, restarts, and runtime verification; repository changes alone do not deploy anything.

#### Configuration and credentials

Both agents read Threadwatch project `<PROJECT_ID>`, Development (`dev`):

| Source | Destination |
| --- | --- |
| `/server` settings | `/run/threadwatch/server.env` |
| `/collector` settings | `/run/threadwatch-collector/collector.env` |
| `/file-credentials/TW_INGEST_TOKEN` | Each service's `/run/.../ingest-token` |

The templates always write services-host overrides, even when those keys are absent from Infisical. They exclude those keys and inline `TW_INGEST_TOKEN` from the generic settings loop so there is exactly one selected ingestion-token mode:

| Server override | Value |
| --- | --- |
| `TW_HOST` | `192.0.2.10` |
| `TW_DATA_DIR` | `/var/lib/threadwatch` |
| `TW_INGEST_TOKEN_FILE` | `/run/threadwatch/ingest-token` |

| Collector override | Value |
| --- | --- |
| `TW_SERVER_URL` | `http://192.0.2.10:8787` |
| `TW_BRIDGES_FILE` | `/home/services/.config/threadwatch/bridge.json` |
| `TW_COLLECTOR_DATA_DIR` | `/var/lib/threadwatch-collector` |
| `TW_INGEST_TOKEN_FILE` | `/run/threadwatch-collector/ingest-token` |

Other settings remain in Infisical, including the server's required `TW_LOCAL=1`, `TW_LAN=1`, and `TW_PORT=8787`. The agent templates use Sprig `replace` to escape backslashes, double quotes, dollar signs, and newlines for Rust `dotenvy`; literal tabs, carriage returns, and Unicode remain inside double quotes. Go `printf "%q"` is not a dotenv encoder: it leaves dollar substitution active and can emit unsupported escapes such as `\t` and `\u`. This compatibility follows source inspection of [Infisical CLI 0.45.8's template functions](https://github.com/Infisical/cli/blob/v0.45.8/packages/templates/templates.go) and [dotenvy 0.15.7's parser](https://docs.rs/crate/dotenvy/0.15.7/source/src/parse.rs); test nonsecret representative values before activation. Infisical's own secret-reference expansion still applies.

Both units intentionally use the existing root-owned mode-0600 `/etc/threadwatch/credentials/client-id` and `client-secret`, under a root-only mode-0700 directory. Despite their historical filenames, these bootstrap credentials serve both services with read-only access to the Threadwatch project. `LoadCredential` maps them to `client-id` and `client-secret`; each YAML reads `/run/credentials/<unit-name>/client-id` and `client-secret`. Do not copy credentials into this repository, unit environment variables, logs, or service-user-readable persistent files. No Discord-project access is needed.

The services collector's bridge configuration retains its own stable ID, bridge database, and session paths. Its optional `botTokenFile` may point to an independently provisioned services-bot token readable only by the appropriate service account. It is not rendered by these templates. **Never copy Hephaestus's Discord token** or point services at Hephaestus's token/project; omit `botTokenFile` if services has no token. Collection and delivery do not require Discord title/avatar lookup.

#### Startup and independent bootstrap

systemd runs both units as `services`, creates their mode-0700 runtime directories, and supplies the credentials. `/usr/local/bin/infisical agent --config=/etc/threadwatch/{server,collector}-agent.yaml` runs once in `ExecStartPre`: authenticate, render, revoke its access token, then exit. The next pre-start command sets mode 0600 on both outputs. `/usr/local/bin/threadwatch --env-file <runtime-env> serve` or `collect` then starts with the selected file. No Rust SDK, persistent access-token sink, background refresh agent, or shell-sourced environment is involved.

The units retain `UMask=0077`, `NoNewPrivileges=true`, `PrivateTmp=true`, 90-second startup and 30-second stop timeouts, and failure restart after five seconds. They order after network-online, Docker, and ZeroTier; ordering does **not** prove Infisical or the ZeroTier address is ready. Cold starts fail without successfully rendered configuration; there is no stale-file fallback. Already-running processes keep their loaded values during an Infisical outage. After repairing a prolonged outage, services-bot may need `systemctl reset-failed` before restarting if systemd rate-limited failures.

Infisical must boot independently of Threadwatch and of secrets stored only in Infisical itself. Services-bot must retain root-protected, independently recoverable Infisical bootstrap configuration, encryption/authentication keys, and datastore credentials, plus consistent persistent-data backups. Keep machine Universal Auth credentials recoverable as well. An Infisical database backup without its encryption key is insufficient; never put the sole recovery copy behind the system it must recover.

#### Migration and activation (services-bot only)

1. Select a reviewed repository revision for these four assets. Inventory the actually installed Threadwatch executable/release and effective units/drop-ins; do not assume local `main` or `target/release/threadwatch` matches the deployed release. This configuration migration does not require rebuilding or replacing the application binary.
2. **Before installing**, make a timestamped, root-only backup of the selected installed units, their drop-in directories (including any `infisical.conf`), both `/etc/threadwatch/*-agent.yaml` files, the selected existing environment/token configuration, and `/home/services/.config/threadwatch/bridge.json`. Record which paths were absent, the effective unit settings, and enabled/running state. Protect any credential-bearing backup like the originals; do not print its contents. Preserve bootstrap credentials without rotating them as part of this change.
3. Record the live server's thread/event/receipt counts and collector queue/heartbeat state. Preserve `/var/lib/threadwatch/threadwatch.sqlite` and `/var/lib/threadwatch-collector` in place, including queue/checkpoint data and delivery IDs. If taking a database recovery backup, use SQLite's backup API or a cleanly stopped, checkpointed database, not a raw copy of a live WAL database.
4. Confirm the service account can access its existing state and bridge paths, the installed CLI supports the templates (source-compatible with 0.45.8), and the identity can read the required project paths. Check Infisical availability without exposing credentials. Stop only the affected Threadwatch units for cutover; do not restart the Discord bridge, discard queues, change bot IDs, rotate the shared ingestion token, or start a second collector on the same state directory.
5. Install the reviewed YAML files as root-owned mode 0644 `/etc/threadwatch/server-agent.yaml` and `/etc/threadwatch/collector-agent.yaml`; their parent must allow `services` traversal/read while `credentials/` remains root-only. Install the complete units as root-owned mode 0644 `/etc/systemd/system/threadwatch.service` and `/etc/systemd/system/threadwatch-collector.service`. Retire only backed-up drop-ins superseded by these complete units; otherwise an old `ExecStart` reset or additive `ExecStartPre` can silently override the new deployment. Preserve unrelated drop-ins and review their effective interaction.
6. Run `systemd-analyze verify` on the installed units, then `systemctl daemon-reload`. Enable the intended units for boot; start the server first, verify it, then start the collector. **Do not run `threadwatch install-service`** for this deployment: the generic installer would overwrite these credential-aware units.
7. Verify the effective commands select the `/run` environment files, both runtime directories are 0700 and generated files 0600 owned by `services`, and both agent pre-start commands succeed. Compare required values privately/in memory, never by dumping rendered files or credentials. Verify `http://192.0.2.10:8787`, fresh ready heartbeats, queue drainage with stable delivery IDs, and no loss of stored threads/events/receipts. Check the existing gateway/dashboard and confirm the Discord bridge was not restarted. A successful startup is not a reboot/outage drill; report those separately.

For later settings changes, edit the appropriate Infisical folder and restart only the affected unit. Coordinate ingestion-token rotation with **every** collector; rendering a new token on one service alone breaks delivery. Other collector hosts retain their own identities, paths, and queues; these services-host units must not be copied onto them unchanged.

#### Rollback

Stop the affected Threadwatch units, restore the backed-up selected units, drop-ins, agent YAML, bridge configuration, and any previous environment/token files needed by that configuration with their original ownership/modes. Remove only newly installed configuration paths recorded as previously absent. Restore any separately changed binary only from a known compatible release; do not substitute a convenient local build. Reload systemd, restore the recorded enabled state, and start/verify the server before the collector as above.

Rollback restores **configuration files, not stale databases or queues**. Keep the authoritative server database and collector state containing all post-cutover writes, delivery receipts, IDs, and checkpoints. Do not restart the frozen pre-migration server database or repoint a queue to it. A database reverse migration requires a separate coordinated, consistent procedure. If the old configuration also depends on Infisical, restoring it cannot repair an Infisical outage: recover Infisical from its independent bootstrap/backup first.


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
