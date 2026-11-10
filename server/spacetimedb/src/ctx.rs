//! Common interface over `HandlerContext` (HTTP routes) and `ProcedureContext` (scheduled worker):
//! both can make HTTP requests and open transactions.

use spacetimedb::http::{HandlerContext, HttpClient};
use spacetimedb::{ProcedureContext, TxContext};
use std::convert::Infallible;

pub trait Ctx {
    fn http(&self) -> &HttpClient;
    fn tx<T>(&mut self, body: impl Fn(&TxContext) -> T) -> T;
    fn now_micros(&self) -> i64;

    fn now_ms(&self) -> i64 {
        self.now_micros() / 1000
    }
}

impl Ctx for HandlerContext {
    fn http(&self) -> &HttpClient {
        &self.http
    }
    fn tx<T>(&mut self, body: impl Fn(&TxContext) -> T) -> T {
        self.with_tx(body)
    }
    fn now_micros(&self) -> i64 {
        self.timestamp.to_micros_since_unix_epoch()
    }
}

impl Ctx for ProcedureContext {
    fn http(&self) -> &HttpClient {
        &self.http
    }
    fn tx<T>(&mut self, body: impl Fn(&TxContext) -> T) -> T {
        #[cfg(test)]
        TX_COUNT.with(|c| c.set(c.get() + 1));
        // Same as `with_tx` on the host. In SpacetimeDB 2.12 `test_utils` only `try_with_tx`
        // uses the in-memory datastore; `with_tx` still calls the host ABI and panics.
        match self.try_with_tx::<T, Infallible>(|tx| Ok(body(tx))) {
            Ok(v) => v,
            Err(never) => match never {},
        }
    }
    fn now_micros(&self) -> i64 {
        self.timestamp.to_micros_since_unix_epoch()
    }
}

// Transactions opened through `Ctx::tx` on this thread (tests count them per request).
#[cfg(test)]
thread_local! {
    pub static TX_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn tx_now_micros(tx: &TxContext) -> i64 {
    tx.timestamp.to_micros_since_unix_epoch()
}
