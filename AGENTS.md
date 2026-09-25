# AGENTS.md

Guidance for AI coding agents (Claude Code, Cursor, Copilot, Codex, and others)
working in this repository.

## What this repo is

`edge-revenue-mcp` is an offline-first payments ledger exposed as a Model Context
Protocol (MCP) server over stdio, written in Rust. It is designed to run on a
small device (for example a Raspberry Pi) at a venue with unreliable network:
sales are recorded to a local append-only libsql ledger and synced to Square
through an outbox when connectivity returns. See `README.md` for the tool list
and environment variables.

Layout:

- `rust/` - the crate (`Cargo.toml`, `src/`).
- `rust/src/store.rs` - the ledger and outbox logic, including the
  `push_to_square` stub.
- `TASKS.md` - scoped good-first-issue tasks.
- `.github/workflows/ci.yml` - CI (build + test).

## Build and test

```sh
cd rust
cargo build --locked --all-targets
cargo test --locked
```

CI runs the same two commands. Note: test coverage is currently thin (see
`TASKS.md` task 0); a green `cargo test` today mostly means the crate compiles.

## Conventions

- Money is integer cents (`i64`) end to end. Never use floats for amounts.
- The ledger is append-only. Do not mutate or delete existing rows; corrections
  are new compensating rows.
- Sync must be safe to retry. A failed push leaves the row unsynced; use the
  `receipt_id` as the idempotency key when calling Square.
- With `SQUARE_ACCESS_TOKEN` unset, sync is a no-op that reports queue depth and
  never errors. Keep that behavior.
- stdout is the JSON-RPC channel. Log to stderr only (via `tracing`).
- When you fix a bug or add a feature, add a test for it.
- Keep docs accurate; do not describe something as tested or done unless it is.

## Secrets

- Never commit secrets, API tokens (including `SQUARE_ACCESS_TOKEN`), or
  credentials. Read them from environment variables only.
- Do not commit ledger database files or local machine paths.

## Contact

support@meshtool.ai
