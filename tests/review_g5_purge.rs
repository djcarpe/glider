//! Review of G5, second pass, adopted as regression tests: the restart
//! after positions went back. The last test failed before the fix (a `cap`
//! call ended the restart as if the journal were whole).
//! Positions are made to go back as the builder's own test does: the
//! journal of a graph further along is copied into one that is behind.

use glider::ext::changes::{self, Kind, Mode};
use glider::graph::Graph;
use glider::query;
use glider::value::Value;

fn q(g: &mut Graph, s: &str) -> query::QueryResult {
    query::execute(g, s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn ahead_and_behind() -> Graph {
    let mut a = Graph::memory();
    q(&mut a, "CREATE EXTENSION changes");
    q(&mut a, "CALL changes.cap(entries: 1000)");
    for i in 0..1500 {
        q(&mut a, &format!("CREATE (:A {{i: {i}}})"));
    }
    let mut b = Graph::memory();
    q(&mut b, "CREATE EXTENSION changes");
    for i in 0..20 {
        q(&mut b, &format!("CREATE (:B {{i: {i}}})-[:K]->(:B)"));
    }
    let mut puts = Vec::new();
    a.ext_scan(changes::NS, &[], &mut |k, v| {
        puts.push((k.to_vec(), v.to_vec()));
        true
    });
    b.ext_write(changes::NS, puts, Vec::new()).unwrap();
    assert!(changes::state_of(&b).unwrap().at > b.log_end());
    b
}

/// Through the whole restart, every question is refused; after it, every
/// change made during it is either listed or the revision refused.
fn through_the_restart(b: &mut Graph, during: &mut dyn FnMut(&mut Graph, usize)) {
    let r0 = b.revision();
    let mut boundaries = vec![r0];
    let mut changed_nodes = Vec::new();
    let mut steps = 0;
    loop {
        let id = b.node_ids()[steps % 20];
        b.set_node_prop(id, "w", Value::Int(steps as i64)).unwrap();
        changed_nodes.push((b.revision(), id));
        during(b, steps);
        steps += 1;
        boundaries.push(b.revision());
        let st = changes::state_of(b).unwrap();
        let purging = st.mode == Mode::Purging;
        for &r in &boundaries {
            let answer = changes::since(b, r, &[], None, usize::MAX);
            let ch = changes::changed(b, r, Kind::Node, &[id]);
            if purging {
                assert!(answer.is_err() && ch.is_none(), "step {steps}: answered for {r} while purging");
                continue;
            }
            if let Ok((list, _)) = &answer {
                // Every node changed after r must be listed.
                for &(at, n) in &changed_nodes {
                    if at > r {
                        assert!(
                            list.iter().any(|e| e.kind == Kind::Node && e.id == n),
                            "step {steps}: since({r}) omits node {n} changed by the commit at {at} (state {st:?})"
                        );
                    }
                }
            }
            if let Some(v) = ch {
                let should = changed_nodes.iter().any(|&(at, n)| n == id && at > r);
                assert_eq!(v[0], should, "step {steps}: changed({r}, node {id}) wrong (state {st:?})");
            }
        }
        if !purging && steps > 2 && changes::answers(b, b.revision()) && steps > 40 {
            eprintln!("restart over after {steps} writes: {:?} floor {}", st.mode, st.floor);
            return;
        }
        assert!(steps < 500, "the restart never ends");
    }
}

#[test]
fn a_restart_refuses_everything_until_it_is_over() {
    let mut b = ahead_and_behind();
    through_the_restart(&mut b, &mut |_, _| {});
}

#[test]
fn a_cap_call_during_the_restart_does_not_make_it_answer_for_what_it_missed() {
    let mut b = ahead_and_behind();
    through_the_restart(&mut b, &mut |g, s| {
        if s == 3 {
            q(g, "CALL changes.cap(entries: 100000)");
        }
    });
}

#[test]
fn a_release_during_the_restart_does_not_make_it_answer_for_what_it_missed() {
    let mut b = ahead_and_behind();
    through_the_restart(&mut b, &mut |g, s| {
        if s == 3 {
            let r = g.revision();
            q(g, &format!("CALL changes.release(below: {r})"));
        }
    });
}

#[test]
fn a_rolled_back_restart_step_is_taken_again() {
    let mut b = ahead_and_behind();
    through_the_restart(&mut b, &mut |g, s| {
        if s % 2 == 1 {
            g.begin().unwrap();
            g.add_node(&["Gone".into()], vec![]).unwrap();
            g.rollback().unwrap();
        }
    });
}

/// A journal whose state was written further along but whose floor is not
/// past this database (it never raised its floor): a `cap` call during
/// the restart ends it as if the journal were whole.
#[test]
fn a_cap_call_does_not_end_a_restart_whose_floor_is_not_past_the_database() {
    let mut a = Graph::memory();
    q(&mut a, "CREATE EXTENSION changes");
    q(&mut a, "CALL changes.cap(entries: 64)");
    q(&mut a, "CREATE (:A)");
    for i in 0..3000 {
        q(&mut a, &format!("MATCH (n:A) SET n.i = {i}"));
    }
    let sa = changes::state_of(&a).unwrap();
    let mut b = Graph::memory();
    q(&mut b, "CREATE EXTENSION changes");
    for i in 0..400 {
        q(&mut b, &format!("CREATE (:B {{i: {i}}})"));
    }
    let now = b.log_end();
    eprintln!("a: state at {} floor {}; b at {now}", sa.at, sa.floor);
    assert!(sa.floor <= now && sa.at > now);
    let mut puts = Vec::new();
    a.ext_scan(changes::NS, &[], &mut |k, v| {
        puts.push((k.to_vec(), v.to_vec()));
        true
    });
    b.ext_write(changes::NS, puts, Vec::new()).unwrap();
    let r = b.revision();
    // A change during the restart (not recorded), then a cap call.
    let x = 300;
    b.set_node_prop(x, "w", Value::Int(1)).unwrap();
    assert_eq!(changes::state_of(&b).unwrap().mode, Mode::Purging);
    q(&mut b, "CALL changes.cap(entries: 1000)");
    let st = changes::state_of(&b).unwrap();
    let ans = changes::changed(&b, r, Kind::Node, &[x]);
    eprintln!("after cap: {:?} floor {} at {}; changed({r}, node {x}) = {ans:?}", st.mode, st.floor, st.at);
    assert_ne!(ans, Some(vec![false]), "node {x} changed after {r} and the journal says it did not");
}
