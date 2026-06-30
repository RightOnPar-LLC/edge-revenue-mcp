# edge-revenue-mcp

Offline-first edge payments, exposed over the **Model Context Protocol**.

Built to run on a **Raspberry Pi at a venue** where the network is flaky or
absent. It records sales **fully offline** into a local, append-only ledger,
then **syncs them to Square** through an **outbox** when connectivity returns.
Taking money never depends on being online.

## The offline-first / outbox model

1. **Record offline.** `sale_record` writes one immutable row to a local
   [libsql](https://github.com/tursodatabase/libsql) database with `synced = 0`.
   No network is touched — the sale is safe the instant it is recorded.
2. **Queue.** Unsynced rows sit in the outbox. The Pi can be offline for hours
   or days; sales keep accumulating.
3. **Sync.** When connectivity returns, `square_sync` pushes every queued row to
   Square and flips it to `synced = 1`, stamping the returned `square_id`.
   - If `SQUARE_ACCESS_TOKEN` is **unset**, sync is a **fail-safe no-op**: it
     reports how many sales are queued and errors on nothing.
   - If a push fails mid-batch, that row stays `synced = 0` and is retried on the
     next pass — that is the whole point of the outbox.
4. **Reconcile.** `reconcile` flags anomalies a human should chase: sales stuck
   unsynced too long (go check the network), or synced rows missing a
   `square_id` (a bug).

Money is stored as **integer cents** (`i64`) end to end — never a float — so
totals are exact.

## Tools

| Tool | Args | Does |
|------|------|------|
| `sale_record` | `amount_cents:i64`, `currency?:String`(USD), `item:String`, `method:String`, `note?:String` | Record a sale offline; returns a `receipt_id`. Queued (`synced=0`). |
| `receipt_verify` | `receipt_id:String` | Return the transaction if found (proof of sale), else not-found. |
| `square_sync` | — | Push all queued sales to Square, mark synced. Offline (no token) ⇒ report queue depth, sync nothing, no error. |
| `sync_status` | — | Counts: total, synced, queued, `last_synced_at`. |
| `daily_tally` | `since_ms?:i64`, `until_ms?:i64` | Sum `amount_cents` grouped by currency and by method, plus count, over the window. |
| `reconcile` | `threshold_ms?:i64`(1h) | Flag stuck-unsynced sales and synced rows missing a `square_id`. |

## Environment

| Var | Default | Purpose |
|-----|---------|---------|
| `EDGE_REVENUE_DB_PATH` | `~/.edge-revenue/ledger.db` | Local ledger file. |
| `SQUARE_ACCESS_TOKEN` | _(unset)_ | Square API token. **Unset ⇒ offline mode** (queue only, never error). |
| `RUST_LOG` | _(unset)_ | `tracing` filter, e.g. `info`. Logs go to **stderr** (stdout is the JSON-RPC channel). |

## Run

```sh
cd rust
cargo build --release
# stdio MCP server; point your MCP client at the binary:
SQUARE_ACCESS_TOKEN=...   ./target/release/edge-revenue-mcp     # online: syncs to Square
./target/release/edge-revenue-mcp                               # offline: queues only
```

On a Raspberry Pi, run it as a systemd service and let `square_sync` be called
on a timer (or whenever the agent detects the network is back).

## ⚠️ Square HTTP call is stubbed — TODO

The Square network call lives in **one clearly-marked function**,
`push_to_square(...)` in [`rust/src/store.rs`](rust/src/store.rs). Today it
returns a deterministic fake id (`sq_<receipt_id>`) so the **outbox state
machine** — the real, correct, tested logic — is fully exercised.

**TODO (see `TASKS.md`):** wire the real Square Payments API
(`POST https://connect.squareup.com/v2/payments`) inside `push_to_square`, using
the transaction's `receipt_id` as the idempotency key so retries never
double-charge. Swapping it in must not change the queue semantics.

## License

MIT.
