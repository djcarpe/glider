//! The change journal under statement budgets: its procedures stop like any
//! other and leave the journal as it was, and a journal kept at its cap,
//! which makes room a few entries at a time inside the hook of the next
//! write, carries on making room after writes stopped while doing it.
use glider::query::{self, QueryLimits};
use glider::{Graph, Value};

fn steps(n: u64) -> QueryLimits {
    QueryLimits {
        max_steps: Some(n),
        ..Default::default()
    }
}

fn q(g: &mut Graph, src: &str) -> query::QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn graph(n: usize) -> Graph {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    g.autocommit = false;
    for i in 0..n {
        g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
            .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    g
}

fn journal(g: &Graph) -> (String, Vec<(Vec<u8>, Vec<u8>)>) {
    let mut out = Vec::new();
    g.ext_scan("changes", b"", &mut |k, v| {
        out.push((k.to_vec(), v.to_vec()));
        true
    });
    (query::export_jsonl(g), out)
}

fn int(r: &query::QueryResult, col: &str) -> i64 {
    let i = r.columns.iter().position(|c| c == col).unwrap();
    r.rows[0][i].as_i64().unwrap()
}

// `changes.cap` and `changes.release`, which forget entries, stopped at
// every budget below what they need, have forgotten nothing.
#[test]
fn a_stopped_cap_or_release_forgets_nothing() {
    for release in [false, true] {
        let mut g = graph(3_000);
        q(&mut g, "MATCH (n:N) SET n.j = 1");
        let now = g.revision();
        let src = if release {
            format!("CALL changes.release(below: {now})")
        } else {
            "CALL changes.cap(entries: 64)".to_string()
        };
        let base = journal(&g);
        let mut s = 1u64;
        let mut stops = 0;
        while let Err(e) = query::execute_capped(&mut g, &src, &[], steps(s)) {
            assert!(e.to_string().contains("matching steps"), "{src}: {e}");
            assert!(journal(&g) == base, "{src} stopped at {s} left something");
            stops += 1;
            s = if s < 64 { s * 2 } else { s + s / 4 + 1 };
        }
        assert!(stops > 10, "{src}: {stops}");
    }
}

// A journal kept at a small cap, written to by statements with budgets too
// tight for the journal's work in their hook: those are stopped and leave
// nothing, and a write given room for its own work always succeeds after
// them, while the journal stays within its cap and keeps answering.
#[test]
fn a_journal_at_its_cap_keeps_making_room_after_stopped_writes() {
    let mut g = graph(200);
    q(&mut g, "CALL changes.cap(entries: 64)");
    let (mut stopped, mut through) = (0, 0);
    for i in 0..2_000u64 {
        let tight = 1 + (i * 7919) % 40;
        let src = format!("CREATE (:W {{k: {i}}})");
        let before = journal(&g);
        match query::execute_capped(&mut g, &src, &[], steps(tight)) {
            Err(e) => {
                assert!(e.to_string().contains("matching steps"), "{e}");
                assert!(journal(&g) == before, "a stopped write left something");
                stopped += 1;
            }
            Ok(_) => through += 1,
        }
        // given enough for its own work, the next write goes through
        let src = format!("CREATE (:V {{k: {i}}})");
        query::execute_capped(&mut g, &src, &[], steps(10_000))
            .unwrap_or_else(|e| panic!("write {i} failed after a stopped one: {e}"));
        let st = q(&mut g, "CALL changes.status()");
        assert!(
            int(&st, "entries") <= 2 * 64 + 16,
            "the journal grew past its cap: {:?}",
            st.rows
        );
    }
    assert!(
        stopped > 100 && through > 100,
        "stopped {stopped}, through {through}"
    );
    // and it answers for what it still holds
    let st = q(&mut g, "CALL changes.status()");
    let oldest = int(&st, "oldest");
    q(&mut g, &format!("CALL changes.since(revision: {oldest})"));
}
