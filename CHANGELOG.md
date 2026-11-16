# Changelog

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
