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
    CALL_DEADLINE.with(|c| c.set(None));
}

fn seen(tx: &TxContext) {
    let now = tx_now_micros(tx);
    LAST_TX_MICROS.with(|c| c.set(c.get().max(now)));
}

/// Seconds a client request may spend on store and delivery calls, counted from its start; the
/// worker finishes the rest. Every call it makes is cut at this deadline ([`Budget::bound_calls`]):
/// a waiting call holds a module instance, and requests share the host's pool of them.
pub const REQUEST_BUDGET_SECS: i64 = 3;
/// Seconds a store notification may spend on store and delivery calls (an App Store one may look
/// in production, then sandbox, then the subscription status). Past it the store gets a 503 and
/// sends the notification again.
pub const NOTIFICATION_BUDGET_SECS: i64 = 5;
/// Seconds one worker run may spend starting outbound calls; the next run (a minute later) goes on.
pub const WORKER_BUDGET_SECS: i64 = 40;

/// Time of the latest transaction of this thread (what a [`Budget`] is judged by).
pub fn last_tx_micros() -> i64 {
    LAST_TX_MICROS.with(|c| c.get())
}

/// A deadline for starting outbound calls in one request or worker run. With each call allowed
/// `net::TIMEOUT_SECS`, a slow store must not hold a run for minutes: work past the deadline is
/// left to the next worker run. Time is read from the transactions the run opens anyway.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    start_micros: i64,
    deadline_micros: i64,
}

impl Budget {
    pub fn new(ctx: &impl Ctx, secs: i64) -> Self {
        let start_micros = ctx.now_micros();
        Budget { start_micros, deadline_micros: start_micros + secs * 1_000_000 }
    }

    /// Cuts every outbound call on this thread (`net::send`) at this deadline until the guard is
    /// dropped. The outermost bound wins: a store look-up inside a worker run keeps the run's.
    #[must_use = "the bound ends when the guard is dropped"]
    pub fn bound_calls(&self) -> CallBound {
        let armed = CALL_DEADLINE.with(|c| {
            if c.get().is_some() {
                return false;
            }
            c.set(Some((self.start_micros, self.deadline_micros)));
            true
        });
        CallBound { armed }
    }

    /// Time is left, judged by the latest transaction of this thread.
    pub fn left(&self) -> bool {
        self.allows(last_tx_micros())
    }

    /// Time is left at `at_micros` (a transaction's time).
    pub fn allows(&self, at_micros: i64) -> bool {
        at_micros < self.deadline_micros
    }
}

// (start, deadline) of the outermost `Budget::bound_calls` guard alive on this thread.
thread_local! {
    static CALL_DEADLINE: std::cell::Cell<Option<(i64, i64)>> = const { std::cell::Cell::new(None) };
}

/// Ends a [`Budget::bound_calls`] bound when dropped.
pub struct CallBound {
    armed: bool,
}

impl Drop for CallBound {
    fn drop(&mut self) {
        if self.armed {
            CALL_DEADLINE.with(|c| c.set(None));
        }
    }
}

/// Time limit of the next outbound call: what is left of the bound request or run (time judged by
/// the latest transaction, at least the bound's start), between 1 s and `max_micros`.
pub fn call_timeout_micros(max_micros: i64) -> i64 {
    match CALL_DEADLINE.with(|c| c.get()) {
        None => max_micros,
        Some((start, deadline)) => purser_core::time::call_timeout_micros(deadline - last_tx_micros().max(start), max_micros),
    }
}
