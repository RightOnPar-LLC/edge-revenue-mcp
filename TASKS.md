# Good first issues

Scoped tasks a new collaborator can pick up. The outbox state machine in
`rust/src/store.rs` is done and tested — these build on it.

## 1. Wire the real Square Payments API in `push_to_square`
Replace the stub body of `push_to_square` (`rust/src/store.rs`) with a real
HTTPS `POST https://connect.squareup.com/v2/payments` using the bearer token,
and return the response's `payment.id`. Add an HTTP client dependency
(`reqwest` with `rustls-tls`, blocking or async).
**Acceptance:** with a real `SQUARE_ACCESS_TOKEN` (sandbox), `square_sync`
creates a payment in the Square dashboard and stores its real id; a network
error leaves the row `synced = 0` for retry.

## 2. Add idempotency keys to prevent double-charge on retry
Pass the transaction's `receipt_id` as Square's `idempotency_key` so a sync that
retries after a dropped response never charges twice.
**Acceptance:** calling `square_sync` twice for the same queued sale (simulating
a lost first response) produces exactly one Square payment; a unit test asserts
the key passed equals the `receipt_id`.

## 3. Add a CSV export of the daily tally
Add an `export_csv` tool (and a `Store` method) that writes the `daily_tally`
breakdown — and/or the raw transactions in a window — to a CSV file path.
**Acceptance:** `export_csv` over a window produces a well-formed CSV with a
header row and correct cent totals; covered by a unit test reading the file back.

## 4. Add a `void_sale` tool
Add a `void_sale` tool that voids a sale by `receipt_id`. Keep the ledger
append-only: write a *compensating* negative-amount row referencing the original,
do not mutate or delete the original.
**Acceptance:** after voiding, `daily_tally` nets to zero for that sale and
`receipt_verify` shows both the original and the void; a unit test confirms the
original row is unchanged.

## 5. Persist a Square sync failure log
When `push_to_square` (post task 1) returns an error, record the attempt
(receipt_id, timestamp, error) in a `sync_failures` table so reconcile can show
*why* a row is stuck, not just that it is.
**Acceptance:** a simulated push failure leaves the sale queued and inserts one
`sync_failures` row; `reconcile` output includes the last error for stuck sales.

## 6. Add a `daily_tally` "today" convenience window
Add an optional `day?:String` (YYYY-MM-DD, venue-local) argument to `daily_tally`
that computes the `since_ms`/`until_ms` window for that calendar day.
**Acceptance:** passing `day` returns only that day's sales; a unit test with
sales on two different days confirms the boundary is correct.
