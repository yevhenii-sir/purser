//! Common interface over `HandlerContext` (HTTP routes) and `ProcedureContext` (scheduled worker):
//! both can make HTTP requests and open transactions.

use spacetimedb::http::{HandlerContext, HttpClient};
use spacetimedb::{ProcedureContext, ReducerContext, TxContext};

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
        self.with_tx(|tx| {
            seen(tx);
            body(tx)
        })
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
        self.with_tx(|tx| {
            seen(tx);
            body(tx)
        })
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

/// Time of a transaction (or reducer call).
pub fn tx_now_micros(tx: &ReducerContext) -> i64 {
    tx.timestamp.to_micros_since_unix_epoch() + test_elapsed()
}

#[cfg(test)]
fn test_elapsed() -> i64 {
    TEST_ELAPSED_MICROS.with(|c| c.get())
}

#[cfg(not(test))]
const fn test_elapsed() -> i64 {
    0
}

// Time of the latest transaction opened on this thread: the module's only clock inside a call.
thread_local! {
    static LAST_TX_MICROS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

// Time the fake HTTP client lets pass on this thread (tests of `Budget`).
#[cfg(test)]
thread_local! {
    pub static TEST_ELAPSED_MICROS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// A new request or worker run in tests: no time has passed in it yet.
#[cfg(test)]
pub fn start_test_call() {
    TEST_ELAPSED_MICROS.with(|c| c.set(0));
    LAST_TX_MICROS.with(|c| c.set(0));
}

fn seen(tx: &TxContext) {
    let now = tx_now_micros(tx);
    LAST_TX_MICROS.with(|c| c.set(c.get().max(now)));
}

/// Seconds a client request may spend starting store and delivery calls; the worker finishes the rest.
pub const REQUEST_BUDGET_SECS: i64 = 5;
/// Seconds one worker run may spend starting outbound calls; the next run (a minute later) goes on.
pub const WORKER_BUDGET_SECS: i64 = 40;

/// A deadline for starting outbound calls in one request or worker run. With each call allowed
/// `net::TIMEOUT_SECS`, a slow store must not hold a run for minutes: work past the deadline is
/// left to the next worker run. Time is read from the transactions the run opens anyway.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    deadline_micros: i64,
}

impl Budget {
    pub fn new(ctx: &impl Ctx, secs: i64) -> Self {
        Budget { deadline_micros: ctx.now_micros() + secs * 1_000_000 }
    }

    /// Time is left, judged by the latest transaction of this thread.
    pub fn left(&self) -> bool {
        self.allows(LAST_TX_MICROS.with(|c| c.get()))
    }

    /// Time is left at `at_micros` (a transaction's time).
    pub fn allows(&self, at_micros: i64) -> bool {
        at_micros < self.deadline_micros
    }
}
