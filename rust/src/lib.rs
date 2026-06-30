//! edge-revenue-mcp (Rust) — offline-first edge payments, over the Model
//! Context Protocol.
//!
//! Built to run on a Raspberry Pi at a venue with flaky or absent connectivity.
//! Sales are recorded *fully offline* into a local, append-only ledger
//! ([`store`]) — money is stored as integer cents, never a float, and every
//! sale is an immutable row. A sale is born `synced = 0` (queued in the
//! outbox); when connectivity returns, [`Store::square_sync`] pushes the queued
//! rows to Square and marks them `synced = 1` with their returned Square id.
//!
//! The outbox state machine is the real product. The Square HTTP call itself is
//! stubbed behind [`store::push_to_square`] (see the TODO there) — swapping in
//! the real Square Payments API must not change the queue semantics.
//!
//! Fail-safe by design: if `SQUARE_ACCESS_TOKEN` is unset the service does not
//! error — it reports how many transactions are queued and keeps taking sales.

pub mod error;
pub mod store;

pub use error::{Error, Result};
pub use store::{ReconcileReport, Store, Tally, Transaction};
