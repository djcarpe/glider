//! The running statement's own budget: how long it may run and how many
//! steps it may take (`QueryLimits::max_millis`, `max_steps`).
//!
//! A *step* is one node or one edge looked at, whether or not it turns out
//! to matter: a candidate checked by a pattern, an edge scanned, a
//! neighbour visited by an algorithm or a walk, a node read by a scan. The
//! same count bounds a query, a built-in algorithm and an extension
//! procedure, so a budget means the same thing for all of them.
//!
//! The meter is per thread, like the rest of a statement's state: it is set
//! around a statement by `query::execute_capped` and `query::Cursor::open`,
//! and anything that statement runs on the calling thread counts against
//! it. Outside a metered statement counting costs one thread-local
//! decrement and never stops anything.
//!
//! Once the budget is spent the statement is *stopped*: [`steps`] returns
//! the error and keeps returning it for the rest of the statement, so code
//! that noticed late, or code that cannot return an error where it noticed,
//! still stops at its next count, and the statement as a whole ends with
//! that error and rolls back ([`check`]).
//!
//! The clock is read once per [`CHUNK`] steps, or at every count of more
//! than that: between two readings at most `CHUNK` steps, or one count, of
//! work happens. There is no clock on wasm32, so `max_millis` is not kept
//! there; `max_steps` is kept everywhere.

use crate::graph::{Error, Result};
use crate::query::QueryLimits;
use std::cell::Cell;

/// Steps handed out between two looks at the meter (and the clock).
pub const CHUNK: u64 = 256;

/// Entries a long scan (one node's neighbour list, a sort of one) goes
/// through between two [`poll`]s: what bounds the work between two looks
/// at the meter whatever a node's degree.
pub const SCAN: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Steps,
    // never on wasm32, which has no clock
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Time,
}

#[derive(Clone, Copy, Debug)]
struct Meter {
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Option<std::time::Instant>,
    #[cfg(not(target_arch = "wasm32"))]
    millis: u64,
    /// Steps left, not counting the credit handed out.
    steps_left: u64,
    steps: u64,
    stopped: Option<Stop>,
}

impl Meter {
    fn error(&self, why: Stop) -> Error {
        match why {
            Stop::Steps => Error::Msg(format!(
                "statement stopped: it took more than its budget of {} matching steps",
                self.steps
            )),
            #[cfg(not(target_arch = "wasm32"))]
            Stop::Time => Error::Msg(format!(
                "statement stopped: it ran past its time budget of {} ms",
                self.millis
            )),
            #[cfg(target_arch = "wasm32")]
            Stop::Time => Error::Msg("statement stopped: it ran past its time budget".into()),
        }
    }
}

thread_local! {
    static METER: Cell<Option<Meter>> = const { Cell::new(None) };
    /// Steps that may be taken before the meter is looked at again:
    /// unbounded when no meter is set, else at most [`CHUNK`].
    static CREDIT: Cell<u64> = const { Cell::new(u64::MAX) };
}

/// Puts the previous meter back when the statement ends, however it ends
/// (a return, an error, a panic), with the steps the statement took counted
/// against it.
pub(crate) struct MeterGuard {
    prev: Option<Meter>,
    /// Steps the statement's own meter started with.
    start: u64,
}

impl Drop for MeterGuard {
    fn drop(&mut self) {
        let credit = CREDIT.with(|c| c.get());
        let inner = METER.with(|m| m.get());
        let prev = self.prev.map(|mut p| {
            if let Some(i) = inner {
                // what the inner statement used is the outer one's too
                let left = i.steps_left.saturating_add(credit.min(CHUNK));
                let used = self.start.saturating_sub(left);
                p.steps_left = p.steps_left.saturating_sub(used);
                if i.stopped == Some(Stop::Steps) && p.steps_left == 0 && p.stopped.is_none() {
                    p.stopped = Some(Stop::Steps);
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            if p.stopped.is_none() && p.deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                p.stopped = Some(Stop::Time);
            }
            p
        });
        METER.with(|m| m.set(prev));
        CREDIT.with(|c| c.set(if prev.is_some() { 0 } else { u64::MAX }));
    }
}

/// Starts metering the statement about to run on this thread; the guard
/// puts the previous meter back. A statement started inside another (a
/// procedure running a query) is bounded by both budgets, the tighter of
/// each, and what it uses is counted against the outer statement too.
pub(crate) fn meter(limits: &QueryLimits) -> MeterGuard {
    let own = if limits.max_millis.is_none() && limits.max_steps.is_none() {
        None
    } else {
        Some(Meter {
            #[cfg(not(target_arch = "wasm32"))]
            deadline: limits
                .max_millis
                .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms)),
            #[cfg(not(target_arch = "wasm32"))]
            millis: limits.max_millis.unwrap_or(0),
            steps_left: limits.max_steps.unwrap_or(u64::MAX),
            steps: limits.max_steps.unwrap_or(0),
            stopped: None,
        })
    };
    // The outer meter gets back the credit it had handed out.
    let credit = CREDIT.with(|c| c.get());
    let prev = METER.with(|m| m.get()).map(|mut p| {
        p.steps_left = p.steps_left.saturating_add(credit.min(CHUNK));
        p
    });
    let next = match (prev, own) {
        (None, own) => own,
        (Some(p), None) => Some(p),
        (Some(p), Some(mut o)) => {
            if p.steps_left < o.steps_left {
                o.steps_left = p.steps_left;
                o.steps = p.steps;
            }
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(pd) = p.deadline {
                if !matches!(o.deadline, Some(od) if od <= pd) {
                    o.deadline = Some(pd);
                    o.millis = p.millis;
                }
            }
            o.stopped = p.stopped;
            Some(o)
        }
    };
    METER.with(|m| m.set(next));
    CREDIT.with(|c| c.set(if next.is_some() { 0 } else { u64::MAX }));
    MeterGuard {
        prev,
        start: next.map_or(0, |n| n.steps_left),
    }
}

/// Counts `n` steps against the running statement: an error once its
/// budget is spent, and from then on for the rest of the statement. Outside
/// a metered statement it never fails.
///
/// This is how an extension procedure or function, or any long loop run by
/// a statement, keeps the statement's budget: count each node and each edge
/// it looks at, and stop with the error when one comes back.
#[inline]
pub fn steps(n: u64) -> Result<()> {
    let fast = CREDIT.with(|c| {
        let k = c.get();
        if k >= n && k > 0 {
            c.set(k - n);
            true
        } else {
            false
        }
    });
    if fast {
        Ok(())
    } else {
        refill(n)
    }
}

/// Looks at the meter and the clock without counting anything: true once
/// the statement must stop. For work that is not steps but can be long (a
/// scan that only counts), every few thousand entries.
pub fn poll() -> bool {
    let k = CREDIT.with(|c| c.get());
    if k == u64::MAX {
        return false;
    }
    refill(0).is_err()
}

/// Sorts `list`, looking at the meter every [`SCAN`] entries when it is
/// long (a hub's neighbours): runs of `SCAN` sorted, then merged pairwise.
/// Once the statement is stopped the list is left part-sorted; nothing
/// uses it then. Short lists are sorted as `sort_unstable` would.
pub fn sort<T: Ord + Copy>(list: &mut Vec<T>) {
    if list.len() <= SCAN {
        list.sort_unstable();
        return;
    }
    for (i, run) in list.chunks_mut(SCAN).enumerate() {
        if i > 0 && poll() {
            return;
        }
        run.sort_unstable();
    }
    let mut width = SCAN;
    let mut buf: Vec<T> = Vec::with_capacity(list.len());
    while width < list.len() {
        buf.clear();
        let mut lo = 0;
        while lo < list.len() {
            let mid = (lo + width).min(list.len());
            let hi = (lo + 2 * width).min(list.len());
            let (mut i, mut j) = (lo, mid);
            while i < mid || j < hi {
                if buf.len() % SCAN == SCAN - 1 && poll() {
                    return;
                }
                if j >= hi || (i < mid && list[i] <= list[j]) {
                    buf.push(list[i]);
                    i += 1;
                } else {
                    buf.push(list[j]);
                    j += 1;
                }
            }
            lo = hi;
        }
        std::mem::swap(list, &mut buf);
        width *= 2;
    }
}

/// One step: [`steps`]`(1)`.
#[inline]
pub fn step() -> Result<()> {
    steps(1)
}

/// Counts `n` steps where the error cannot be returned (inside a callback):
/// true once the statement must stop. The stop sticks, so the next
/// [`steps`] or [`check`] returns the error.
#[inline]
pub fn spent(n: u64) -> bool {
    steps(n).is_err()
}

/// Steps counted in a local variable and passed to the meter [`CHUNK`] at a
/// time, for loops too tight to count each step through the thread-local
/// meter. A stop is noticed when a batch is passed, so at most `CHUNK`
/// steps (plus the last count) go by after the budget is spent. Call
/// [`Tally::flush`] when the work is done, so that the last fewer than
/// `CHUNK` are counted too: it has no `Drop` that would do it, because one
/// cost a tight loop more than half its speed.
#[derive(Debug, Default)]
pub struct Tally(u64);

impl Tally {
    pub fn new() -> Tally {
        Tally(0)
    }

    /// Counts `n` steps; true once the statement must stop.
    #[inline]
    pub fn spent(&mut self, n: u64) -> bool {
        self.0 += n;
        self.0 >= CHUNK && self.flush()
    }

    /// Counts `n` steps where nothing can stop on the answer (inside a
    /// callback): a stop sticks, and is noticed by the next
    /// [`Tally::spent`], the meter's next count, or the scan that called
    /// back ([`poll`]).
    #[inline]
    pub fn add(&mut self, n: u64) {
        self.spent(n);
    }

    /// Passes the steps counted so far to the meter; true once the
    /// statement must stop.
    #[cold]
    #[inline(never)]
    pub fn flush(&mut self) -> bool {
        let stop = spent(std::mem::take(&mut self.0));
        if stop {
            // so that every later count asks again, and hears it: a stop
            // noticed in a callback is seen by the loop around it
            self.0 = CHUNK;
        }
        stop
    }
}

/// The statement's error if it has been stopped, else `Ok`. Counts
/// nothing. A procedure that returned normally after something it called
/// was stopped is still stopped: the statement fails with this error.
pub fn check() -> Result<()> {
    METER.with(|cell| match cell.get() {
        Some(m) => match m.stopped {
            Some(why) => Err(m.error(why)),
            None => Ok(()),
        },
        None => Ok(()),
    })
}

/// Whether the running statement has been stopped by its budget.
pub fn stopped() -> bool {
    METER.with(|cell| cell.get().is_some_and(|m| m.stopped.is_some()))
}

#[cold]
#[inline(never)]
fn refill(n: u64) -> Result<()> {
    METER.with(|cell| {
        let Some(mut m) = cell.get() else {
            CREDIT.with(|c| c.set(u64::MAX));
            return Ok(());
        };
        let credit = CREDIT.with(|c| c.replace(0));
        m.steps_left = m.steps_left.saturating_add(credit);
        if let Some(why) = m.stopped {
            cell.set(Some(m));
            return Err(m.error(why));
        }
        if m.steps_left < n {
            m.steps_left = 0;
            m.stopped = Some(Stop::Steps);
            cell.set(Some(m));
            return Err(m.error(Stop::Steps));
        }
        m.steps_left -= n;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(d) = m.deadline {
            if std::time::Instant::now() >= d {
                m.stopped = Some(Stop::Time);
                cell.set(Some(m));
                return Err(m.error(Stop::Time));
            }
        }
        let grant = m.steps_left.min(CHUNK);
        m.steps_left -= grant;
        cell.set(Some(m));
        CREDIT.with(|c| c.set(grant));
        Ok(())
    })
}
