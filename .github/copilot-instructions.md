# Copilot instructions

This repository is `edge-revenue-mcp`: an offline-first payments ledger exposed
as a stdio MCP server in Rust (`rust/`). Sales are written to a local append-only
libsql ledger and synced to Square through an outbox. See `AGENTS.md` and
`README.md` for details.

## Build and test

```sh
cd rust
cargo build --locked --all-targets
cargo test --locked
```

## Conventions

- Amounts are integer cents (`i64`), never floats.
- The ledger is append-only; never mutate or delete existing rows.
- Square sync must be idempotent and safe to retry (use `receipt_id` as the key).
- Unset `SQUARE_ACCESS_TOKEN` means offline mode: queue only, never error.
- stdout is reserved for JSON-RPC; log to stderr.
- Add tests with every fix or feature.

## Secrets

Never commit secrets, tokens, ledger database files, or local machine paths.
Read credentials from environment variables.
