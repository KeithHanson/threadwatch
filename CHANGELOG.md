# Changelog

## Unreleased

- Show Active before All threads and select Active by default.
- Sort agents alphabetically and their threads by Discord creation time, newest first; unknown creation times sort last.

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
