# Changelog

## Unreleased

- Add the read-only `threadwatch-tui` Ratatui client with Active/Past navigation, collector health, todos, loaded-history search, pause/follow controls, and paginated transcripts over the existing viewer API.
- Replace the stacked desktop task/transcript detail with accessible per-thread tabs while preserving the mobile Todos control and transcript-first layout.
- Show the configured session model as a labeled fact in the Agent map and the thread detail metadata (`default` when no session override is set); threads ingested by current bridge versions carry the model through validation, storage, and rendering.
- Disable withheld tool details by default; set `TW_WITHHELD_TOOL_DETAILS=1` to restore withholding of credential/instruction-file-targeting tool records. Legacy WITHHELD-marked records keep their honest fallback regardless.

## 0.3.0

- Render intent-first tool cards with readable commands, code, paths, proposed patches, API/device operations, safe raw disclosures, and explicit error/background-receipt states.
- Preserve sanitized structured arguments and native session/call IDs through collection and ingestion; group only exactly correlated lifecycle records, including across loaded pages, without guessing legacy history.
- Retain raw-record search/counts/cursors, disclosure expansion and focus, pause/catch-up behavior, and held-scroll anchors while grouped results arrive.
- Implement the supplied v2 map-first design: full-width Agent map, clickable bot cards, and a focused agent rail shown only beside that bot's thread detail.
- Replace Active/All with shared Active/Past filters and global/per-bot counts; move offline-source alerts to the map and automatically select the most recent matching thread when opening a bot.
- Remove the global sidebar search, global Reporting sources panel, and mobile health strip. Mobile map mode has no drawer; thread mode exposes Map and a bot-scoped Threads drawer.
- Preserve the compact transcript, collapsible recorded tasks, pending-record counts while paused, new-record follow controls, and history-replacement notices.
- Expose bounded latest-event previews, short titles, todo totals, and snapshot-consistent event counts; collect optional Discord avatars with validated CDN URLs.
- Bundle Instrument Sans and JetBrains Mono locally, preserving the single Rust binary and server-rendered htmx architecture.
- Select Active by default; Past includes Unknown without implying successful completion.
- Sort agents alphabetically and their threads by last update, most recent first; unknown update times sort last.
- Use relative browser URLs so the dashboard works beneath a stripped reverse-proxy path prefix.
- Allow explicitly configured reverse-proxy hostnames while retaining private-peer checks.
- Move bridge collection, durable queueing, heartbeats, and Discord metadata lookup into `rust-omp-discord-bridge`.
- Remove the standalone `collect` command, collector service installer, collector deployment assets, bridge adapter, and collector recovery suite from this server repository.
- Preserve existing collector state directories and delivery IDs as the integrated bridge reporter's migration contract.
- Keep mobile thread details readable by hiding transcript search/pause/follow controls and placing a single-line-height chat field beside Send.
- Compress the mobile thread header into a compact title/status row and single task-summary row, removing duplicated navigation and secondary timestamps.
- Replace the mobile Discord action with a Todos control that reveals the complete task list on demand while keeping it out of the default transcript view.
- Pair mobile browsers from an authorized desktop using a two-minute, single-use QR code and a signed 24-hour HttpOnly control session without transferring the permanent token.
- Treat successful viewer access as browser-control authorization, removing control-token prompts while retaining private-peer, Host, Origin, same-site, and bridge-ingestion protections.

## 0.2.0

- Rewrote the server, collector, and service installer in Rust.
- Replaced React/Vite with server-rendered HTML, htmx, and embedded static assets.
- Preserved the reporting API, existing SQLite history, durable queues, and shared-token LAN deployment.
- Removed the Node runtime/toolchain and unused Discord Activity integration.

## Pre-rewrite development

- Added explicit trusted-LAN viewing through `TW_LAN=1`, restricted to a private IPv4 bind address and private-network peers. Viewer authentication is disabled by explicit operator choice; Host/Origin and ingest-token checks remain enabled.
- Added an independently runnable collector with a durable outbound queue, restart-safe delivery IDs, incremental updates, and explicit transcript replacement.
- Added shared-token-file configuration, persistent collector heartbeats, queue-depth display, and stale reporting indicators.
- Added systemd service installation and separate server/collector environment examples. Collector-only hosts require Node.js, not frontend dependencies.

## 0.1.0

- Added a Discord Activity interface organized by bot and working thread.
- Added todo and operational transcript inspection with history, search, and follow/pause controls.
- Added a read-only OMP Discord bridge adapter and authenticated ingestion for other runners.
- Added loopback local mode and Discord operator authentication for hosted mode.

Deployment policy: local verification first. Public hosting and live Discord launch are not implied by this release.
