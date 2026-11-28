//! Review of G5, adopted as regression tests: the writes that keep the
//! cap. Before the fix one write scanned the whole journal (133 ms and
//! 1.7 MB of log at the default cap); now each write takes at most one
//! bounded slice of the upkeep.

use std::time::Instant;

use glider::ext::changes::{self, Floor};
use glider::graph::Graph;
use glider::query;
use glider::value::Value;
use glider::Error;

fn q(g: &mut Graph, s: &str) {
    query::execute(g, s).unwrap_or_else(|e| panic!("{s}: {e}"));
}

/// Fill the journal with node entries in 1,000-node transactions until it
/// holds more than half the cap and the next batch would reach `look_at`.
fn fill_to_the_edge(g: &mut Graph) {
    loop {
        let s = changes::state_of(g).unwrap();
        let n = changes::status(g).unwrap().entries;
        let before = g.log_end();
        if n > s.cap / 2 && before + 200_000 >= s.look_at {
            return;
        }
        g.begin().unwrap();
        for i in 0..1_000 {
            g.add_node(&["N".into()], vec![("i".into(), Value::Int(i))])
                .unwrap();
        }
        g.commit().unwrap();
    }
}

/// Single writes until one of them raises the floor for the cap; returns
/// how long that one took and how long the others took at most.
fn single_writes_until_trim(g: &mut Graph) -> (u128, u128, u64) {
    let mut others = 0u128;
    loop {
        let floor = changes::oldest(g).unwrap().0;
        let log0 = g.log_end();
        let t = Instant::now();
        g.add_node(&["N".into()], vec![]).unwrap();
        let took = t.elapsed().as_micros();
        if changes::oldest(g).unwrap().0 != floor {
            assert_eq!(changes::oldest(g).unwrap().1, Floor::Cap);
            return (took, others, g.log_end() - log0);
        }
        others = others.max(took);
    }
}

#[test]
fn the_write_that_raises_the_floor_for_the_default_cap_does_one_bounded_slice() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    fill_to_the_edge(&mut g);
    let before = changes::status(&g).unwrap();
    let (took, others, log) = single_writes_until_trim(&mut g);
    let after = changes::status(&g).unwrap();
    eprintln!(
        "cap {}: entries {} -> {}; the trimming write took {} ms and logged {} bytes; \
         other single writes at most {} us",
        before.cap,
        before.entries,
        after.entries,
        took / 1000,
        log,
        others
    );
    // One slice: at most SWEEP entries forgotten and one state record.
    assert!(
        log < 4_096,
        "the write that raised the floor logged {log} bytes"
    );
    assert!(
        took < 50_000,
        "the write that raised the floor took {took} us"
    );
}

#[test]
fn a_trim_that_does_not_fit_in_memory_refuses_every_later_write() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CALL changes.cap(entries: 200000)");
    fill_to_the_edge(&mut g);
    // Headroom that an ordinary write fits in: shown by a write that does
    // no upkeep (one before look_at).
    let (used, _) = g.stats().memory.unwrap();
    let headroom: u64 = std::env::var("HEAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 * 1024);
    g.set_max_memory(used + headroom);
    let s = changes::state_of(&g).unwrap();
    assert!(g.log_end() + 100 < s.look_at);
    g.add_node(&["Fits".into()], vec![]).unwrap();
    // Walk up to look_at with single writes, each fitting.
    let mut failures = 0;
    let mut tries = 0;
    let mut first_err = String::new();
    loop {
        tries += 1;
        let (used, _) = g.stats().memory.unwrap();
        g.set_max_memory(used + headroom);
        match g.add_node(&["N".into()], vec![]) {
            Ok(_) => {
                if changes::oldest(&g).unwrap().1 == Floor::Cap {
                    eprintln!("the trim fitted after {tries} writes");
                    break;
                }
                assert_eq!(failures, 0, "a write succeeded after a failure");
            }
            Err(e) => {
                assert!(matches!(e, Error::Full(_)), "{e}");
                if failures == 0 {
                    first_err = e.to_string();
                }
                failures += 1;
                if failures == 20 {
                    break;
                }
            }
        }
    }
    // Every write from look_at on fails, with the same headroom an
    // ordinary write needs, ten times over.
    let (used, _) = g.stats().memory.unwrap();
    g.set_max_memory(used + 8 * headroom);
    let still = g.add_node(&["N".into()], vec![]);
    let set = g.set_node_prop(1, "k", Value::Int(1));
    eprintln!(
        "{failures} writes in a row failed: {first_err}; with 8x the headroom: add {:?}, set {:?}",
        still.as_ref().map(|_| ()),
        set
    );
    assert_eq!(
        failures, 0,
        "the trim made {failures} consecutive writes fail"
    );
}
