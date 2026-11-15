# Contributing

Issues and pull requests are welcome.

## Setup

- Rust stable, plus `rustup target add wasm32-unknown-unknown`
- [trunk](https://trunkrs.dev) for the web UI: `cargo install trunk --locked`
- Python 3.9+ and [ruff](https://docs.astral.sh/ruff/) for `tools/yaml-import`

## Before you open a PR

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd crates/web && trunk build)
(cd tools/yaml-import && ruff check . && ruff format --check .)
```

CI runs the same checks, plus a Docker build.

## Guidelines

- Keep the capture path fast. Nothing slow (InvenTree, the model, the web) belongs in the HTTP request. Put it in
  the pipeline.
- InvenTree is the source of truth. Never overwrite something a person may have edited, unless the code is
  replacing its own placeholder.
- Pure logic (parsing, query building, mapping) goes in plain functions with unit tests next to them.
- Write commit messages in the imperative mood ("Add …", "Fix …").
