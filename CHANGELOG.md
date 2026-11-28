# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **Don't know** tab: snap an item without choosing a box. It is stocked in an `Inbox` location, identified, and
  then gets up to 3 box suggestions, based on what the other boxes already hold (same category, similar names,
  matching box name) and chosen by the model with a reason. Pick a suggestion or another box, confirm, and the
  stock moves there (`POST api/jobs/{id}/file`). Waiting items stay in a *To store* list, filed ones move to *Stored*.
- Duplicate check: a capture of a part already in InvenTree is merged into it (photo added as an attachment,
  qty added to its stock in the box) and the new part is removed. `DEDUP=off` disables it.
- Several photos per item (up to 4): snaps collect in a tray and **Save item** sends them together. The model
  sees all of them in one request (told they are the same item from different sides; 768 px each when more
  than two). The first photo is the part image, the rest are attachments, also on a duplicate merge.
- The capture logs persist across reloads, with server-side thumbnails (`GET api/thumb/{id}`): the last 100
  Store captures and the last 100 filed items. Older finished entries are pruned from the spool.

## [0.1.0] - 2026-11-16

First public release, extracted from a working home deployment.

### Added
- Capture PWA (Leptos/wasm): sticky box picker with create-on-the-spot (nested `A/B` paths), shutter, quantity,
  on-device downscaling, live progress, and one-tap retry of failed uploads.
- Service (axum): durable spool, and a pipeline worker that puts storing ahead of naming.
- Identification with any Ollama vision model; JSON-constrained output mapped to InvenTree categories.
- Web research of part-number-like markings through self-hosted SearXNG, with sources recorded in part notes.
- `RESEARCH_POLICY` = `markings` | `low` | `off`.
- Compose stack (InvenTree, SearXNG, Caddy with `forward_auth`, optional Ollama).
- `tools/yaml-import`: idempotent bulk importer for YAML catalogues, with token or password auth.
- Multi-arch container image published to GHCR.
