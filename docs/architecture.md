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

1. Snaps collect in a tray for the current item (up to 4 photos). The PWA decodes each photo (honouring EXIF
   orientation) and re-encodes it as a JPEG of at most 1600 px **on the phone**. **Save item** `POST`s `api/capture`
   (multipart: one `photo` field per photo, `qty`, `mode`; for `mode=store` also `location`, `location_name`).
   `mode=sort` is the *Don't know* tab: no box yet.
2. Caddy checks the user's InvenTree session against `inventree-server:8000/auth/` and only then proxies to the
   service. The service itself talks to InvenTree with its own API token.
3. The service writes `<id>.jpg` + `<id>.json` to the spool and returns the job. Nothing else happens on the
   request path, so upload latency does not depend on InvenTree or the model.

## Pipeline

A single worker loops over the spool and does **one unit of work** per step, always in this priority order:

| From | To | Work |
|---|---|---|
| `queued` | `stored` | Create part `qc-<id>` in category *Inbox*, upload the first photo as its image, add stock of `qty` at the box, and attach any further photos (the count is saved after each, so a retry never attaches twice). Idempotent on retry (found by IPN). |
| `stored` | `named` / `research` | Downscale every photo (≤1024 px, or ≤768 px when there are more than two), send them all in one request saying they show the same item from different sides, and ask the model (`format: json`, `temperature 0.2`, `num_predict 400`). Write name, description and notes, and move to the mapped category unless confidence is `low`. |
| `research` | `named` | Extract part-number-like tokens from the markings, query SearXNG (≤3 queries, ≤10 de-duplicated results), and re-ask the model with photo + results. Sources go into the notes. |
| `stored` | `unnamed` | After 3 failed identifications. The part stays in *Inbox*. |
| `named` | (duplicate checked) | Search InvenTree for the AI name and part numbers. An identical name, or a `high`-confidence model match, means it is already there: the photo becomes an attachment of that part, the qty is added to its stock in the box, and the new part is deleted. `merged_into` is saved before the cleanup, so retries never add stock twice. `DEDUP=off` skips this. |
| *Don't know* captures | (suggested) | Rank the boxes by what they hold (same category ×3, similar part names ×2, box name matches ×4), let the model pick up to 3 from the top 12 with a reason, and top up from the ranking. Boxes that already hold a merged part come first. |

*Don't know* captures are stocked at the `Inbox` location (hidden from the box picker). Once the user picks a box and
confirms, `POST api/jobs/{id}/file` moves the stock there, adding to that part's existing stock item in the box if it
has one. The worker never overwrites a filing done while it was busy with the same job.

Because *store* always wins, a burst of captures is in InvenTree within seconds while identification
(about 10 s per photo on a CPU with a 4B model) works through the backlog. The worker wakes as soon as a capture
arrives, and every 30 s otherwise.

Photos are deleted from the spool once the duplicate check is done. A 200 px thumbnail and the JSON file stay as the
activity log: `GET api/recent` returns the last 100 Store captures, every *Don't know* item still waiting, and the last
100 filed ones. Older finished entries are pruned when the worker is idle.

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
