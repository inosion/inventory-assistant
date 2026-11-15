# Architecture

## Components

| Component | Tech | Role |
|---|---|---|
| Capture PWA (`crates/web`) | Rust, Leptos (CSR), wasm | Box picker, shutter, qty, live progress list |
| Service (`crates/server`) | Rust, axum, tokio, reqwest | HTTP API, durable spool, pipeline worker |
| InvenTree | Django | Source of truth: parts, stock, locations, images |
| Ollama | any vision model | Reads markings; names, describes and categorises the part |
| SearXNG | metasearch | Web research on part numbers, with no API keys |
| Caddy | reverse proxy | TLS, routing, `forward_auth` to InvenTree for the capture API |

## Request path

1. The PWA decodes the photo (honouring EXIF orientation), re-encodes it as a JPEG of at most 1600 px **on the phone**,
   and `POST`s `api/capture` (multipart: `photo`, `location`, `location_name`, `qty`).
2. Caddy checks the user's InvenTree session against `inventree-server:8000/auth/` and only then proxies to the
   service. The service itself talks to InvenTree with its own API token.
3. The service writes `<id>.jpg` + `<id>.json` to the spool and returns the job. Nothing else happens on the
   request path, so upload latency does not depend on InvenTree or the model.

## Pipeline

A single worker loops over the spool and does **one unit of work** per step, always in this priority order:

| From | To | Work |
|---|---|---|
| `queued` | `stored` | Create part `qc-<id>` in category *Inbox*, upload the photo as its image, add stock of `qty` at the box. Idempotent on retry (found by IPN). |
| `stored` | `named` / `research` | Downscale to ≤1024 px, ask the model (`format: json`, `temperature 0.2`, `num_predict 400`). Write name, description and notes, and move to the mapped category unless confidence is `low`. |
| `research` | `named` | Extract part-number-like tokens from the markings, query SearXNG (≤3 queries, ≤10 de-duplicated results), and re-ask the model with photo + results. Sources go into the notes. |
| `stored` | `unnamed` | After 3 failed identifications. The part stays in *Inbox*. |

Because *store* always wins, a burst of captures is in InvenTree within seconds while identification
(about 10 s per photo on a CPU with a 4B model) works through the backlog. The worker wakes as soon as a capture
arrives, and every 30 s otherwise.

Photos are deleted from the spool once a job is final. The JSON files stay as a small activity log
(`GET api/recent` returns the newest 80).

## Why research by default?

Small vision models are often *confidently* wrong. In testing, a 4B model called an audio board a "Bluetooth module"
with high confidence. Printed part numbers are much more reliable evidence than the model's overall impression, so
by default (`RESEARCH_POLICY=markings`) every item with a searchable marking gets a second, grounded look.
`low` limits this to low-confidence items, and `off` disables it.

## Category mapping

The model chooses from a fixed slug list (`microcontroller`, `power`, `sensor`, `cable`, …). Each slug maps to an
InvenTree category path (`crates/server/src/inventree.rs::category_for`), and the path is created on first use.
`unknown`, unmapped slugs and low-confidence guesses keep the part in *Inbox*.

## Failure handling

| Failure | Behaviour |
|---|---|
| InvenTree down | Job stays `queued`. The worker backs off for 15 s and retries. Uploads keep being accepted. |
| Model error or bad JSON | Retried 3× (5 s apart), then `unnamed`. |
| SearXNG error | Retried 3×, then the first guess is kept. |
| Service restart | The spool is on disk, so work resumes where it stopped. Writes are atomic (temp file + rename). |
| Phone upload fails | The PWA keeps the downscaled photo in memory, and tapping the item retries it. |
