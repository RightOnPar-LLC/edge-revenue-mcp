# edge-revenue-mcp (Rust crate)

The Rust crate behind `edge-revenue-mcp` — an offline-first edge-payments MCP
server. A local append-only sales ledger ([`src/store.rs`](src/store.rs)) plus a
Square sync outbox, served over stdio MCP ([`src/main.rs`](src/main.rs)).

No external database, no server — one local libsql file. Money is integer cents.

## Layout

| File | What |
|------|------|
| `src/main.rs` | rmcp stdio server; the six MCP tools. |
| `src/store.rs` | The ledger + outbox state machine, and `push_to_square` (stubbed). Unit tests land here (see TASKS.md task 0). |
| `src/error.rs` | Crate error type. |
| `src/lib.rs` | Library root; re-exports `Store`, `Transaction`, `Tally`, `ReconcileReport`. |

## Tools

| Tool | Args | Does |
|------|------|------|
| `sale_record` | `amount_cents`, `currency?`, `item`, `method`, `note?` | Record a sale offline; returns a `receipt_id` (queued). |
| `receipt_verify` | `receipt_id` | Return the transaction if found, else not-found. |
| `square_sync` | — | Push queued sales to Square; offline (no token) ⇒ report-only no-op. |
| `sync_status` | — | total / synced / queued / `last_synced_at`. |
| `daily_tally` | `since_ms?`, `until_ms?` | Sum by currency and by method, plus count. |
| `reconcile` | `threshold_ms?` | Flag stuck-unsynced and synced-without-`square_id`. |

## Env

| Var | Default | Purpose |
|-----|---------|---------|
| `EDGE_REVENUE_DB_PATH` | `~/.edge-revenue/ledger.db` | Ledger file. |
| `SQUARE_ACCESS_TOKEN` | _(unset)_ | Unset ⇒ offline mode (queue only, no error). |
| `RUST_LOG` | _(unset)_ | tracing filter; logs to stderr. |

## Run

```sh
cargo run                      # offline mode (no SQUARE_ACCESS_TOKEN)
cargo build --release          # -> target/release/edge-revenue-mcp
```

## Test

```sh
cargo test
```

Unit tests (in `src/store.rs`) cover the outbox state machine end to end:
offline record → queued, `square_sync` marking rows synced with a token (via the
`push_to_square` stub), `sync_status` counts, `daily_tally` sums by currency and
method, `receipt_verify` hit/miss, and `reconcile` flagging stuck rows. Each test
uses its own `tempfile::tempdir()` database.

## Note: Square is stubbed

`push_to_square` in `src/store.rs` returns a fake `sq_<receipt_id>` today. See
the TODO there and `../TASKS.md` to wire the real Square Payments API. The outbox
logic around it is real and tested.
