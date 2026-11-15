# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [SemVer](https://semver.org/).

## [Unreleased]

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
