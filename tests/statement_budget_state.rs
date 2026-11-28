//! The statement budget's state on a thread, from an independent review of
//! the procedure budgets: nested statements count against the outer one,
//! short walks count as much as long ones, a panic leaves nothing set on
//! the thread, and nothing outlives its statement on a pool of threads.
use glider::{
    algo::{self, Adjacency},
    graph::Tier,
    query::{self, QueryLimits},
    Dir, Graph, QueryResult, Value,
};
use std::sync::{Arc, Mutex};

fn chain(n: usize, extra: usize) -> Graph {
    let mut g = Graph::memory();
    g.autocommit = false;
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(
            g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap(),
        );
    }
    for i in 1..n {
        g.add_edge(ids[i - 1], ids[i], "NEXT", vec![]).unwrap();
        for k in 0..extra {
            g.add_edge(ids[i], ids[(i * 7 + k * 13) % n], "X", vec![])
                .unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    g
}

fn steps(n: u64) -> QueryLimits {
    QueryLimits {
        max_steps: Some(n),
        ..Default::default()
    }
}

struct Nest;
impl glider::ext::Extension for Nest {
    fn name(&self) -> &str {
        "nest"
    }
    fn version(&self) -> &str {
        "0.1"
    }
    fn summary(&self) -> &str {
        "reviewer extension"
    }
    fn procedures(&self) -> &[glider::ext::Spec] {
        &[
            glider::ext::Spec {
                name: "capped",
                args: "(rounds)",
                doc: "runs a query through execute_capped, no budget",
            },
            glider::ext::Spec {
                name: "budgeted",
                args: "(rounds)",
                doc: "runs a query through execute_capped with its own budget",
            },
            glider::ext::Spec {
                name: "walks",
                args: "()",
                doc: "a depth-1 traverse::bfs from every node",
            },
            glider::ext::Spec {
                name: "hops",
                args: "()",
                doc: "a depth-1 algo::k_hop from every node",
            },
            glider::ext::Spec {
                name: "boom",
                args: "()",
                doc: "panics after counting",
            },
            glider::ext::Spec {
                name: "tight",
                args: "()",
                doc: "runs a query through execute_capped with a budget of 10 steps",
            },
        ]
    }
    fn call(&self, g: &mut Graph, p: &str, a: &glider::ext::Args) -> glider::Result<QueryResult> {
        let heavy = "MATCH (a:N)-->(b:N) RETURN count(*)";
        match p {
            "capped" | "budgeted" => {
                let lim = if p == "capped" {
                    QueryLimits::default()
                } else {
                    steps(1_000_000_000)
                };
                let mut total = 0i64;
                for _ in 0..a.usize("rounds", 1) {
                    let r = query::execute_capped(g, heavy, &[], lim)?;
                    total += r.rows[0][0].as_i64().unwrap();
                }
                Ok(QueryResult::table(
                    vec!["n".into()],
                    vec![vec![Value::Int(total)]],
                ))
            }
            "walks" => {
                let mut visits = 0i64;
                for id in g.node_ids() {
                    glider::traverse::bfs(g, id, Dir::Out, None, Some(1), 1 << 20, &mut |_| {
                        visits += 1;
                        true
                    });
                }
                Ok(QueryResult::table(
                    vec!["n".into()],
                    vec![vec![Value::Int(visits)]],
                ))
            }
            "tight" => {
                let r = query::execute_capped(g, heavy, &[], steps(10))?;
                Ok(r)
            }
            "hops" => {
                let p = g.projection(Dir::Out, None, None, Tier::Mem);
                let mut visits = 0i64;
                for v in 0..p.len() {
                    visits += algo::k_hop(&p, v, 1).len() as i64;
                }
                Ok(QueryResult::table(
                    vec!["n".into()],
                    vec![vec![Value::Int(visits)]],
                ))
            }
            _ => {
                let _ = glider::ext::steps(10);
                panic!("boom");
            }
        }
    }
}

fn nest(n: usize) -> Graph {
    glider::ext::register(Arc::new(Nest));
    let mut g = chain(n, 2);
    query::execute(&mut g, "CREATE EXTENSION nest").unwrap();
    g
}

fn inner_needs(g: &mut Graph) -> u64 {
    let (mut lo, mut hi) = (0u64, 1u64 << 30);
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match query::execute_capped(g, "MATCH (a:N)-->(b:N) RETURN count(*)", &[], steps(mid)) {
            Ok(_) => hi = mid,
            Err(_) => lo = mid,
        }
    }
    hi
}

// A procedure that runs a query through execute_capped, with default
// limits or with a budget of its own, is bounded by the outer statement's
// budget, and the inner steps are counted against it.
#[test]
fn a_statement_run_inside_a_procedure_counts_against_the_outer_budget() {
    let mut g = nest(20_000);
    let needed = inner_needs(&mut g);
    for src in [
        "CALL nest.capped(rounds: 5)",
        "CALL nest.budgeted(rounds: 5)",
    ] {
        let e = query::execute_capped(&mut g, src, &[], steps(1_000))
            .err()
            .unwrap_or_else(|| {
                panic!("{src}: 1,000 steps did not stop {} inner steps", 5 * needed)
            });
        assert!(
            e.to_string().contains("budget of 1000 matching steps"),
            "{src}: {e}"
        );
        // the inner steps add up in the outer meter: four rounds' worth of
        // budget is not enough for five, six rounds' is
        let e = query::execute_capped(&mut g, src, &[], steps(4 * needed));
        assert!(e.is_err(), "{src} ran five rounds on four rounds' budget");
        query::execute_capped(&mut g, src, &[], steps(6 * needed)).unwrap();
    }
    // an inner budget only tightens
    let e = query::execute_capped(&mut g, "CALL nest.tight()", &[], steps(1 << 40))
        .err()
        .expect("the inner budget held");
    assert!(e.to_string().contains("budget of 10 matching steps"), "{e}");
}

// Every step of a short walk is counted: a procedure made of 50,000 walks
// of a few steps each stops on a budget of 1,000.
#[test]
fn many_short_walks_count_as_much_as_one_long_one() {
    let mut g = nest(50_000);
    for src in ["CALL nest.hops()", "CALL nest.walks()"] {
        let r = query::execute_capped(&mut g, src, &[], steps(1_000));
        match &r {
            Ok(r) => eprintln!(
                "{src} under max_steps 1,000 finished: {:?} visits",
                r.rows[0][0]
            ),
            Err(e) => eprintln!("{src}: {e}"),
        }
        assert!(
            r.is_err(),
            "{src} ran to the end under a budget of 1,000 steps"
        );
    }
}

fn reference(g: &mut Graph) -> (String, f64, usize, usize) {
    let pr = query::execute(g, "CALL pagerank(top: 5)").unwrap();
    let csr = g.csr(Dir::Out, None, None);
    let p = g.projection(Dir::Out, None, None, Tier::Ooc);
    let score = algo::pagerank(&p, 0.85, 20, 1e-6).scores.get(3);
    let mut walked = 0;
    let first = g.node_ids()[0];
    glider::traverse::bfs(g, first, Dir::Out, None, None, 1 << 22, &mut |_| {
        walked += 1;
        true
    });
    (format!("{:?}", pr.rows), score, csr.adj.len(), walked)
}

// After statements that stop on their budget, error, or panic, on the same
// threads and with two databases shared by a pool: direct API calls and
// unbudgeted statements give the same answers as on a fresh thread.
#[test]
fn no_meter_state_outlives_its_statement_across_a_pool() {
    glider::ext::register(Arc::new(Nest));
    let mut a = chain(30_000, 2);
    let mut b = chain(20_000, 3);
    query::execute(&mut a, "CREATE EXTENSION nest").unwrap();
    query::execute(&mut b, "CREATE EXTENSION nest").unwrap();
    let ra = reference(&mut a);
    let rb = reference(&mut b);
    let dbs = [Arc::new(Mutex::new(a)), Arc::new(Mutex::new(b))];
    let refs = [ra, rb];
    std::thread::scope(|s| {
        for t in 0..4 {
            let dbs = &dbs;
            let refs = &refs;
            s.spawn(move || {
                for round in 0..12 {
                    let i = (t + round) % 2;
                    let mut g = dbs[i].lock().unwrap();
                    match round % 4 {
                        0 => {
                            let e =
                                query::execute_capped(&mut g, "CALL pagerank()", &[], steps(5_000));
                            assert!(e.is_err());
                        }
                        1 => {
                            let e = query::execute_capped(
                                &mut g,
                                "CALL betweenness(samples: 5)",
                                &[],
                                QueryLimits {
                                    max_millis: Some(1),
                                    ..Default::default()
                                },
                            );
                            assert!(e.is_err());
                        }
                        2 => {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                query::execute_capped(&mut g, "CALL nest.boom()", &[], steps(100))
                            }));
                            assert!(r.is_err());
                        }
                        _ => {
                            let _ = query::execute_capped(&mut g, "CALL nope()", &[], steps(10));
                        }
                    }
                    assert!(!glider::meter::stopped());
                    assert!(glider::meter::check().is_ok());
                    for _ in 0..100_000 {
                        glider::meter::step().unwrap();
                    }
                    let got = reference(&mut g);
                    assert_eq!(got, refs[i], "thread {t} round {round}");
                    // an unbudgeted then a budgeted statement start clean
                    let r = query::execute(&mut g, "CALL components(top: 1)").unwrap();
                    assert_eq!(r.rows.len(), 1);
                    query::execute_capped(&mut g, "CALL degree(top: 1)", &[], steps(1 << 40))
                        .unwrap();
                }
            });
        }
    });
}

// A procedure that panics leaves the thread's algorithm memory budget and
// spill directory as they were: the next, unrelated call on the thread, on
// another database and outside any statement, runs as on a fresh thread.
#[test]
fn a_panicking_extension_leaves_no_memory_budget_on_the_thread() {
    glider::ext::register(Arc::new(Nest));
    let mut root = std::env::temp_dir();
    root.push(format!("glider-panic-budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("a.glider");
    let opts = glider::OpenOptions {
        work_mem: 64 << 10,
        ..Default::default()
    };
    let mut a = Graph::open_opts(&path, opts).unwrap();
    query::execute(&mut a, "CREATE EXTENSION nest").unwrap();
    let clean_dir = glider::ooc::temp_dir();
    let r = std::thread::spawn(move || {
        assert_eq!(glider::ooc::budget_left(), None, "clean thread");
        let before_dir = glider::ooc::temp_dir();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            query::execute(&mut a, "CALL nest.boom()")
        }));
        assert!(r.is_err(), "it panicked");
        drop(a);
        assert_eq!(
            glider::ooc::budget_left(),
            None,
            "the budget outlived its statement"
        );
        assert_eq!(
            glider::ooc::temp_dir(),
            before_dir,
            "so did the spill directory"
        );
        assert!(!glider::meter::stopped());
        // an unrelated memory database, a direct API call, no statement
        let mut b = chain(400_000, 1);
        let p = b.projection(Dir::Out, None, None, Tier::Auto);
        assert!(!p.is_paged(), "b's projection was paged by a's budget");
        let score = algo::pagerank(&p, 0.85, 5, 0.0).scores.get(0);
        drop(p);
        query::execute(&mut b, "CALL pagerank(top: 1)").unwrap();
        score
    })
    .join();
    assert!(r.is_ok());
    assert_eq!(glider::ooc::temp_dir(), clean_dir);
    let _ = std::fs::remove_dir_all(&root);
}
