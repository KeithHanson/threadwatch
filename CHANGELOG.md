# Changelog

## Unreleased

- Added explicit trusted-LAN viewing through `TW_LAN=1`, restricted to a private IPv4 bind address and private-network peers. Viewer authentication is disabled by explicit operator choice; Host/Origin and ingest-token checks remain enabled.

## 0.1.0

- Added a Discord Activity interface organized by bot and working thread.
- Added todo and operational transcript inspection with history, search, and follow/pause controls.
- Added a read-only OMP Discord bridge adapter and authenticated ingestion for other runners.
- Added loopback local mode and Discord operator authentication for hosted mode.

Deployment policy: local verification first. Public hosting and live Discord launch are not implied by this release.
