//! No input overflows the stack. Every shape of nesting that statement text,
//! parameters, imports, JSON, the C ABI or stored bytes can carry, at ten
//! times the depth that used to kill the process and at a mebibyte of
//! input, runs on a 256 KiB thread in a debug build and returns a result or
//! an error. 256 KiB is less than a BEAM dirty scheduler, an iOS secondary
//! thread or wasm gives glider, and debug frames are the larger ones.
//!
//! A stack overflow is not a panic: it aborts the whole test binary. Before
//! the parser, evaluator, matcher and codecs were bounded, `the_corpus`
//! did exactly that.

use std::ffi::{CStr, CString};

use glider::query::{self, Cursor, QueryLimits};
use glider::value::{Value, MAX_DEPTH};
use glider::Graph;

const STACK: usize = 256 << 10;
const MIB: usize = 1 << 20;

/// Run `f` on a fresh thread with the small stack.
fn small<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the case panicked")
}

/// One node with `x: 1`, the graph every case starts from.
fn graph() -> Graph {
    let mut g = Graph::memory();
    query::execute(&mut g, "CREATE (:N {x: 1})").unwrap();
    g
}

/// `unit` repeated to about a mebibyte.
fn mib(unit: &str) -> usize {
    MIB / unit.len()
}

/// A list nested `n` deep around 1, built and taken apart without recursion
/// so that the test itself can hold one a million deep.
fn nest(n: usize) -> Value {
    let mut v = Value::Int(1);
    for _ in 0..n {
        v = Value::List(vec![v]);
    }
    v
}

fn unnest(mut v: Value) {
    while let Value::List(mut items) = v {
        match items.pop() {
            Some(inner) => v = inner,
            None => return,
        }
    }
}

fn nested_text(open: &str, inner: &str, close: &str, n: usize) -> String {
    let mut s = String::with_capacity((open.len() + close.len()) * n + inner.len());
    for _ in 0..n {
        s.push_str(open);
    }
    s.push_str(inner);
    for _ in 0..n {
        s.push_str(close);
    }
    s
}

fn joined(unit: &str, sep: &str, n: usize) -> String {
    vec![unit; n].join(sep)
}

type Run = Box<dyn FnOnce(&mut Graph) -> Result<String, String> + Send>;

/// A statement, run as text.
fn text(q: String) -> Run {
    Box::new(move |g| {
        query::execute(g, &q)
            .map(|r| format!("{:?}", r.rows.first()))
            .map_err(|e| e.to_string())
    })
}

/// A statement with one parameter `$p`.
fn param(q: &'static str, p: Value) -> Run {
    Box::new(move |g| {
        let params = vec![("p".to_string(), p)];
        let r = query::execute_with(g, q, &params)
            .map(|r| r.rows.len().to_string())
            .map_err(|e| e.to_string());
        for (_, v) in params {
            unnest(v);
        }
        r
    })
}

fn import(jsonl: String) -> Run {
    Box::new(move |g| {
        query::import_jsonl(g, &jsonl)
            .map(|n| format!("{n:?}"))
            .map_err(|e| e.to_string())
    })
}

fn json_params(q: &'static str, json: String) -> Run {
    Box::new(move |g| {
        let params = glider::api::params_from_json(&json)?;
        glider::api::query_json_with(g, q, &params)
    })
}

/// Through the C ABI: a query with JSON parameters (null for none).
fn ffi(q: String, params: Option<String>) -> Run {
    Box::new(move |_| unsafe {
        let db = glider::ffi::glider_open_memory();
        let q = CString::new(q).unwrap();
        let p = params.map(|p| CString::new(p).unwrap());
        let out = glider::ffi::glider_query_params_json(
            db,
            q.as_ptr(),
            p.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
        );
        let r = if out.is_null() {
            Err(CStr::from_ptr(glider::ffi::glider_last_error())
                .to_string_lossy()
                .into_owned())
        } else {
            let s = CStr::from_ptr(out).to_string_lossy().into_owned();
            glider::ffi::glider_free(out);
            Ok(s)
        };
        glider::ffi::glider_close(db);
        r
    })
}

fn ffi_import(jsonl: String) -> Run {
    Box::new(move |_| unsafe {
        let db = glider::ffi::glider_open_memory();
        let j = CString::new(jsonl).unwrap();
        let rc = glider::ffi::glider_import_jsonl(db, j.as_ptr());
        glider::ffi::glider_close(db);
        if rc == 0 {
            Ok("imported".into())
        } else {
            Err("refused".into())
        }
    })
}

/// A path of `n` relationships, then a pattern that walks all of it.
fn matched_chain(n: usize) -> Run {
    Box::new(move |g| {
        let mut create = String::from("CREATE (:Start)");
        create.push_str(&"-[:R]->(:P)".repeat(n));
        query::execute(g, &create).map_err(|e| e.to_string())?;
        let mut walk = String::from("MATCH (:Start)");
        walk.push_str(&"-[:R]->()".repeat(n));
        walk.push_str(" RETURN count(*)");
        query::execute(g, &walk)
            .map(|r| format!("{:?}", r.rows))
            .map_err(|e| e.to_string())
    })
}

/// Stored bytes: a value tag-5 list nested `n` deep, decoded as a value, as
/// a property run and as a log record.
fn bytes(n: usize) -> Run {
    Box::new(move |_| {
        let mut deep = Vec::with_capacity(2 * n + 2);
        for _ in 0..n {
            deep.extend_from_slice(&[5, 1]);
        }
        deep.extend_from_slice(&[2, 2]);
        let a = glider::codec::Reader::new(&deep).value().map(|_| ());
        let b = glider::codec::Reader::new(&deep).skip_value();
        let mut run = vec![1, 0];
        run.extend_from_slice(&deep);
        let c = glider::codec::read_prop_ids(&run).map(|_| ());
        let d = glider::codec::find_prop(&run, 0).map(|_| ());
        // NodeSet { id: 1, key: "k", value: deep }
        let mut rec = vec![4, 1, 1, b'k'];
        rec.extend_from_slice(&deep);
        let e = glider::store::Op::decode_record(&rec).map(|_| ());
        let all = [a, b, c, d, e];
        if all.iter().all(|r| r.is_err()) {
            Err(format!("{:?}", all[0]))
        } else {
            Ok(format!("{all:?}"))
        }
    })
}

/// Every shape, at ten times the depth that crashed (the table in the brief:
/// parentheses 120, lists 150, unary minus 400, EXPLAIN 300, sums 1,000,
/// subscripts 1,000, comma patterns 200) and at a mebibyte.
fn corpus() -> Vec<(&'static str, Run)> {
    let parens = |n| {
        format!(
            "MATCH (n) WHERE {} RETURN n.x",
            nested_text("(", "n.x = 1", ")", n)
        )
    };
    let lists = |n| format!("MATCH (n) RETURN {}", nested_text("[", "1", "]", n));
    let minus = |n| format!("MATCH (n) RETURN {}1", "- ".repeat(n));
    let not = |n| format!("MATCH (n) WHERE {}true RETURN n.x", "NOT ".repeat(n));
    let bang = |n| format!("MATCH (n) WHERE {}true RETURN n.x", "! ".repeat(n));
    let subs = |n| format!("MATCH (n) RETURN [1]{}", "[0]".repeat(n));
    let slices = |n| format!("MATCH (n) RETURN [1]{}", "[0..]".repeat(n));
    let explain = |n| format!("{}MATCH (n) RETURN n", "EXPLAIN ".repeat(n));
    let calls = |n| format!("MATCH (n) RETURN {}", nested_text("abs(", "1", ")", n));
    let ext_calls = |n| {
        format!(
            "MATCH (n) RETURN {}",
            nested_text("context.card(", "n", ")", n)
        )
    };
    let ins = |n| format!("MATCH (n) RETURN {}", nested_text("1 IN [", "1", "]", n));
    let and = |n| {
        format!(
            "MATCH (n) WHERE {} RETURN n.x",
            joined("n.x = 1", " AND ", n)
        )
    };
    let or = |n| {
        format!(
            "MATCH (n) WHERE {} RETURN n.x",
            joined("n.x = 2", " OR ", n)
        )
    };
    let plus = |n| format!("MATCH (n) RETURN {}", joined("1", " + ", n));
    let times = |n| format!("MATCH (n) RETURN {}", joined("1", " * ", n));
    let cmp = |n| format!("MATCH (n) RETURN {}", joined("1", " = ", n));
    let chain = |n| {
        let mut s = String::from("MATCH (a)");
        s.push_str(&"-[]-(a)".repeat(n));
        s.push_str(" RETURN count(*)");
        s
    };
    let commas = |n: usize| {
        let parts: Vec<String> = (0..n).map(|i| format!("(a{i})")).collect();
        format!("MATCH {} RETURN count(*)", parts.join(", "))
    };
    let same_commas = |n| format!("MATCH {} RETURN count(*)", joined("(a)", ", ", n));
    let sets = |n: usize| {
        let items: Vec<String> = (0..n).map(|i| format!("n.k{i} = {i}")).collect();
        format!("MATCH (n) SET {}", items.join(", "))
    };
    let same_sets = |n| format!("MATCH (n) SET {}", joined("n.k = 1", ", ", n));
    let call_args = |n| format!("CALL pagerank(top: {})", nested_text("[", "1", "]", n));
    let json_line = |n| {
        format!(
            "{{\"type\":\"node\",\"props\":{{\"p\":{}}}}}",
            nested_text("[", "1", "]", n)
        )
    };
    let json_objects = |n| {
        format!(
            "{{\"type\":\"node\",\"props\":{{\"p\":{}}}}}",
            nested_text("{\"a\":", "1", "}", n)
        )
    };
    let json_param = |n| format!("{{\"p\":{}}}", nested_text("[", "1", "]", n));

    vec![
        ("parentheses x10", text(parens(1_200))),
        ("parentheses 1 MiB", text(parens(mib("()")))),
        ("list literals x10", text(lists(1_500))),
        ("list literals 1 MiB", text(lists(mib("[]")))),
        (
            "list parameter x10",
            param("MATCH (n) RETURN $p", nest(1_500)),
        ),
        (
            "list parameter 1 MiB deep",
            param("MATCH (n) RETURN $p", nest(MIB)),
        ),
        (
            "stored list x10",
            param("MATCH (n) SET n.p = $p", nest(1_500)),
        ),
        (
            "created list x10",
            param("CREATE (:M {p: $p})", nest(1_500)),
        ),
        ("unary minus x10", text(minus(4_000))),
        ("unary minus 1 MiB", text(minus(mib("- ")))),
        ("NOT x10", text(not(4_000))),
        ("NOT 1 MiB", text(not(mib("NOT ")))),
        ("! 1 MiB", text(bang(mib("! ")))),
        ("subscripts x10", text(subs(10_000))),
        ("subscripts 1 MiB", text(subs(mib("[0]")))),
        ("slices 1 MiB", text(slices(mib("[0..]")))),
        ("EXPLAIN x10", text(explain(3_000))),
        ("EXPLAIN 1 MiB", text(explain(mib("EXPLAIN ")))),
        ("nested calls x10", text(calls(1_200))),
        ("nested calls 1 MiB", text(calls(mib("abs()")))),
        (
            "nested extension calls 1 MiB",
            text(ext_calls(mib("context.card()"))),
        ),
        ("nested IN 1 MiB", text(ins(mib("1 IN []")))),
        ("AND x10", text(and(10_000))),
        ("AND 1 MiB", text(and(mib("n.x = 1 AND ")))),
        ("OR 1 MiB", text(or(mib("n.x = 2 OR ")))),
        ("+ x10", text(plus(10_000))),
        ("+ 1 MiB", text(plus(mib("1 + ")))),
        ("* 1 MiB", text(times(mib("1 * ")))),
        ("comparisons 1 MiB", text(cmp(mib("1 = ")))),
        ("pattern chain x10", text(chain(10_000))),
        ("matched pattern chain", matched_chain(2_000)),
        ("pattern chain 1 MiB", text(chain(mib("-[]-(a)")))),
        ("comma patterns x10", text(commas(2_000))),
        ("comma patterns 1 MiB", text(same_commas(mib("(a), ")))),
        ("SET items x10", text(sets(10_000))),
        ("SET items 1 MiB", text(same_sets(mib("n.k = 1, ")))),
        ("CALL argument x10", text(call_args(1_500))),
        ("CALL argument 1 MiB", text(call_args(mib("[]")))),
        ("JSONL list x10", import(json_line(1_500))),
        ("JSONL list 1 MiB", import(json_line(mib("[]")))),
        ("JSONL objects 1 MiB", import(json_objects(mib("{\"a\":}")))),
        (
            "JSON parameters x10",
            json_params("MATCH (n) RETURN $p", json_param(1_500)),
        ),
        (
            "JSON parameters 1 MiB",
            json_params("MATCH (n) RETURN $p", json_param(mib("[]"))),
        ),
        ("C ABI parentheses 1 MiB", ffi(parens(mib("()")), None)),
        (
            "C ABI parameters 1 MiB",
            ffi("RETURN 1".into(), Some(json_param(mib("[]")))),
        ),
        ("C ABI import 1 MiB", ffi_import(json_line(mib("[]")))),
        ("stored bytes x10", bytes(1_500)),
        ("stored bytes 1 MiB", bytes(MIB)),
    ]
}

/// Every case completes or refuses on the small stack; a refusal changes
/// nothing, and the handle answers the next statement.
#[test]
fn the_corpus() {
    for (name, run) in corpus() {
        let report = small(move || {
            let mut g = graph();
            let before = query::export_jsonl(&g);
            let r = run(&mut g);
            if r.is_err() {
                assert_eq!(
                    query::export_jsonl(&g),
                    before,
                    "{name}: a refusal changed the graph"
                );
            }
            let next = query::execute(&mut g, "MATCH (n:N) RETURN n.x").map_err(|e| e.to_string());
            assert_eq!(
                next.map(|r| r.rows),
                Ok(vec![vec![Value::Int(1)]]),
                "{name}: the handle answers afterwards"
            );
            assert!(g.autocommit, "{name}: autocommit is back on");
            r.map(|s| s.chars().take(80).collect::<String>())
        });
        println!("{name}: {report:?}");
    }
}

/// Structures nested by nature are refused past the limit, with an error
/// that names it and the construct, and accepted up to it.
#[test]
fn nesting_is_limited_and_says_so() {
    let expr = format!("expression nested deeper than {MAX_DEPTH} levels");
    let list = format!("list nested deeper than {MAX_DEPTH} levels");
    let explain = format!("EXPLAIN nested deeper than {MAX_DEPTH} levels");
    let shapes: Vec<(&str, Box<dyn Fn(usize) -> String + Send>, &str)> = vec![
        (
            "parentheses",
            Box::new(|n| {
                format!(
                    "MATCH (n) WHERE {} RETURN n.x",
                    nested_text("(", "n.x = 1", ")", n)
                )
            }),
            "[[1]]",
        ),
        (
            "lists",
            Box::new(|n| format!("MATCH (n) RETURN {}", nested_text("[", "1", "]", n))),
            "",
        ),
        (
            "unary minus",
            Box::new(|n| format!("MATCH (n) RETURN {}1", "- ".repeat(n))),
            "",
        ),
        (
            "NOT",
            Box::new(|n| format!("MATCH (n) WHERE {}true RETURN n.x", "NOT ".repeat(n))),
            "",
        ),
        (
            // `[1]` is one level, each subscript another
            "subscripts",
            Box::new(|n| format!("MATCH (n) RETURN [1]{}", "[0]".repeat(n - 1))),
            "",
        ),
        (
            "calls",
            Box::new(|n| format!("MATCH (n) RETURN {}", nested_text("abs(", "1", ")", n))),
            "",
        ),
        (
            "EXPLAIN",
            Box::new(|n| format!("{}MATCH (n) RETURN n", "EXPLAIN ".repeat(n))),
            "",
        ),
        (
            "CALL argument",
            Box::new(|n| format!("CALL degree(top: {})", nested_text("[", "1", "]", n))),
            "",
        ),
    ];
    for (name, q, _) in shapes {
        let (at, past) = (q(MAX_DEPTH), q(MAX_DEPTH + 1));
        let want = if name == "EXPLAIN" {
            explain.clone()
        } else {
            expr.clone()
        };
        small(move || {
            let mut g = graph();
            if let Err(e) = query::execute(&mut g, &at) {
                panic!("{name} at the limit: {e}");
            }
            let e = query::execute(&mut g, &past).err().expect("refused");
            assert_eq!(e.to_string(), want, "{name}");
        });
    }

    // A parameter is a value: the same limit, named as a list.
    small(move || {
        let mut g = graph();
        let ok = [("p".to_string(), nest(MAX_DEPTH))];
        let r = query::execute_with(&mut g, "MATCH (n) RETURN $p", &ok).unwrap();
        assert_eq!(r.rows[0][0], nest(MAX_DEPTH));
        let deep = [("p".to_string(), nest(MAX_DEPTH + 1))];
        let e = query::execute_with(&mut g, "MATCH (n) RETURN $p", &deep).err();
        assert_eq!(e.map(|e| e.to_string()), Some(list.clone()));
        // Inside a list literal, a parameter at the limit is an expression past it.
        let e = query::execute_with(&mut g, "MATCH (n) RETURN [$p]", &ok).err();
        assert_eq!(e.map(|e| e.to_string()), Some(expr.clone()));
    });
}

/// Structures flat by nature are bounded by size, not depth, and give the
/// answers they always did.
#[test]
fn flat_is_not_deep() {
    small(|| {
        let mut g = graph();
        let one = |g: &mut Graph, q: &str| query::execute(g, q).unwrap().rows;
        let ands = format!(
            "MATCH (n) WHERE {} RETURN n.x",
            joined("n.x = 1", " AND ", 10_000)
        );
        assert_eq!(one(&mut g, &ands), vec![vec![Value::Int(1)]]);
        let and_false = format!(
            "MATCH (n) WHERE {} AND n.x = 2 RETURN n.x",
            joined("n.x = 1", " AND ", 10_000)
        );
        assert!(one(&mut g, &and_false).is_empty());
        let ors = format!(
            "MATCH (n) WHERE {} OR n.x = 1 RETURN n.x",
            joined("n.x = 2", " OR ", 10_000)
        );
        assert_eq!(one(&mut g, &ors), vec![vec![Value::Int(1)]]);
        let sum = format!("MATCH (n) RETURN {}", joined("n.x", " + ", 10_000));
        assert_eq!(one(&mut g, &sum), vec![vec![Value::Int(10_000)]]);
        // left to right, as the nested form was: ((10000 - 1) - 1) ... and 2 * 3 / 4
        let diff = format!("MATCH (n) RETURN 10000{}", " - 1".repeat(9_999));
        assert_eq!(one(&mut g, &diff), vec![vec![Value::Int(1)]]);
        assert_eq!(
            one(
                &mut g,
                "MATCH (n) RETURN 2 * 3 / 4, 1 + 2 * 3 - 4, 'a' + 1 + 2, 1 + 2 + 'a'"
            ),
            vec![vec![
                Value::Float(1.5),
                Value::Int(3),
                Value::from("a12"),
                Value::from("3a")
            ]]
        );
        let items: Vec<String> = (0..100_000).map(|i| i.to_string()).collect();
        let list = format!("MATCH (n) RETURN size([{}])", items.join(", "));
        assert_eq!(one(&mut g, &list), vec![vec![Value::Int(100_000)]]);
        let within = format!("MATCH (n) WHERE 99999 IN [{}] RETURN n.x", items.join(", "));
        assert_eq!(one(&mut g, &within), vec![vec![Value::Int(1)]]);
        let patterns: Vec<String> = (0..300).map(|i| format!("(a{i}:N)")).collect();
        let many = format!("MATCH {} RETURN count(*)", patterns.join(", "));
        assert_eq!(one(&mut g, &many), vec![vec![Value::Int(1)]]);
    });
}

/// More comma patterns than are matched by recursion: the spooled ones give
/// the rows, in the order, that the recursive matcher gives.
#[test]
fn spooled_patterns_match_like_streamed_ones() {
    small(|| {
        let mut g = Graph::memory();
        query::execute(&mut g, "CREATE (:P {i: 0}), (:P {i: 1}), (:P {i: 2})").unwrap();
        query::execute(
            &mut g,
            "MATCH (a:P {i: 0}), (b:P {i: 1}) CREATE (a)-[:R]->(b)",
        )
        .unwrap();
        // Seven patterns, three of them past the streamed ones, with a
        // shared variable, a relationship and a WHERE across them.
        let q = "MATCH (a:P), (b:P), (c:P), (d:P), (a)-[:R]->(e), (f:P), (g:P) \
                 WHERE a.i + b.i + c.i + d.i + f.i + g.i < 3 AND e.i = 1 \
                 RETURN a.i, b.i, c.i, d.i, e.i, f.i, g.i";
        let rows = query::execute(&mut g, q).unwrap().rows;
        let mut want = Vec::new();
        for b in 0..3 {
            for c in 0..3 {
                for d in 0..3 {
                    for f in 0..3 {
                        for gg in 0..3 {
                            if b + c + d + f + gg < 3 {
                                want.push(
                                    [0, b, c, d, 1, f, gg]
                                        .into_iter()
                                        .map(Value::Int)
                                        .collect::<Vec<_>>(),
                                );
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(rows, want);
        let limited = query::execute(&mut g, &format!("{q} LIMIT 2"))
            .unwrap()
            .rows;
        assert_eq!(limited, want[..2].to_vec());
        let writes = query::execute(
            &mut g,
            "MATCH (a:P {i: 0}), (b:P), (c:P), (d:P), (e:P), (f:P {i: 2}) SET f.seen = true",
        )
        .unwrap();
        assert_eq!(writes.touched, 81);
    });
}

/// A value at the limit goes everywhere a value goes: stored, read, through
/// a cursor, exported, imported, written to a file and replayed on open,
/// replicated and restored. One past it gets in nowhere.
#[test]
fn values_stay_inside_the_limit_everywhere() {
    small(|| {
        let dir = std::env::temp_dir().join(format!("glider-depth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let deep = nest(MAX_DEPTH);
        let p = |v: Value| vec![("p".to_string(), v)];

        let db = dir.join("db");
        let mut g = Graph::open(&db, glider::Sync::Normal).unwrap();
        query::execute_with(&mut g, "CREATE (:N {p: $p})", &p(deep.clone())).unwrap();
        query::execute_with(&mut g, "MATCH (n:N) SET n.q = $p", &p(deep.clone())).unwrap();
        let read = query::execute(&mut g, "MATCH (n:N) RETURN n.p, n.q").unwrap();
        assert_eq!(read.rows, vec![vec![deep.clone(), deep.clone()]]);
        let mut c = Cursor::open(
            &mut g,
            "MATCH (n:N) RETURN n.p",
            &[],
            QueryLimits::default(),
        )
        .unwrap();
        assert_eq!(c.next_batch().unwrap().0.rows, vec![vec![deep.clone()]]);

        // Exported and imported.
        let dump = query::export_jsonl(&g);
        let mut copy = Graph::memory();
        query::import_jsonl(&mut copy, &dump).unwrap();
        assert_eq!(query::export_jsonl(&copy), dump);
        let mut streamed = Graph::memory();
        query::import_jsonl_from(&mut streamed, &mut dump.as_bytes(), 1 << 20).unwrap();
        assert_eq!(query::export_jsonl(&streamed), dump);

        // One level deeper is refused by every way in, and changes nothing.
        let deeper = nest(MAX_DEPTH + 1);
        let id = match &query::execute(&mut g, "MATCH (n:N) RETURN id(n)")
            .unwrap()
            .rows[0][0]
        {
            Value::Int(id) => *id as u64,
            other => panic!("{other:?}"),
        };
        let refusals = [
            query::execute_with(&mut g, "MATCH (n:N) SET n.p = $p", &p(deeper.clone())).err(),
            query::execute_with(&mut g, "CREATE (:M {p: $p})", &p(deeper.clone())).err(),
            query::execute(&mut g, "MATCH (n:N) SET n.r = [n.p]").err(),
            query::execute(&mut g, "MATCH (n:N) RETURN [n.p]").err(),
            query::execute(&mut g, "MATCH (n:N) RETURN collect(n.p)").err(),
            Cursor::open(
                &mut g,
                "MATCH (n:N) RETURN [n.p]",
                &[],
                QueryLimits::default(),
            )
            .err(),
            g.set_node_prop(id, "r", deeper.clone()).err(),
            g.add_node(&[], vec![("r".into(), deeper.clone())]).err(),
            query::import_jsonl(&mut g, &format!("{{\"props\":{{\"p\":{deeper}}}}}")).err(),
        ];
        for (i, e) in refusals.into_iter().enumerate() {
            let e = e.unwrap_or_else(|| panic!("refusal {i} was accepted"));
            assert_eq!(
                e.to_string().trim_start_matches("line 1: "),
                format!("list nested deeper than {MAX_DEPTH} levels"),
                "refusal {i}"
            );
        }
        assert_eq!(
            query::export_jsonl(&g),
            dump,
            "the refusals changed nothing"
        );
        assert_eq!(
            glider::api::params_from_json(&format!("{{\"p\":{deeper}}}")).err(),
            Some(format!("list nested deeper than {MAX_DEPTH} levels"))
        );
        // `to_value` never fails: the list past the limit becomes its text.
        let flattened = glider::value::parse_json(&deeper.to_json())
            .unwrap()
            .to_value();
        let mut want = Value::Text("[1]".into());
        for _ in 0..MAX_DEPTH {
            want = Value::List(vec![want]);
        }
        assert_eq!(flattened, want);

        // Replayed from the log on open, then from the pages after a
        // checkpoint.
        drop(c);
        drop(g);
        let mut g = Graph::open(&db, glider::Sync::Normal).unwrap();
        assert_eq!(query::export_jsonl(&g), dump);
        g.checkpoint().unwrap();
        drop(g);
        let g = Graph::open(&db, glider::Sync::Normal).unwrap();
        assert_eq!(query::export_jsonl(&g), dump);

        // Replicated and restored.
        let replica = dir.join("replica");
        let opts = glider::wal::TailOptions {
            once: true,
            quiet: true,
            ..Default::default()
        };
        drop(g);
        glider::replica::tail(&db, &replica, &opts).unwrap();
        let restored = dir.join("restored");
        glider::replica::restore(&replica, None, None, &restored).unwrap();
        let r = Graph::open(&restored, glider::Sync::Normal).unwrap();
        assert_eq!(query::export_jsonl(&r), dump);
        drop(r);
        unnest(deep);
        unnest(deeper);
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// Bytes that hold a list nested past the limit are damage, as any other
/// undecodable bytes are: every reader says so instead of following them,
/// and accepts exactly the depth the others do.
#[test]
fn deep_bytes_are_damage() {
    small(|| {
        let encode = |depth: usize| {
            let mut b = Vec::new();
            glider::codec::put_value(&mut b, &nest(depth));
            b
        };
        let ok = encode(MAX_DEPTH);
        assert_eq!(glider::codec::Reader::new(&ok).value(), Ok(nest(MAX_DEPTH)));
        assert_eq!(glider::codec::Reader::new(&ok).skip_value(), Ok(()));
        assert_eq!(glider::codec::Reader::new(&ok).skip_value_trusted(), Ok(()));
        let bad = encode(MAX_DEPTH + 1);
        let want = Err(format!("list nested deeper than {MAX_DEPTH} levels"));
        assert_eq!(glider::codec::Reader::new(&bad).value().map(|_| ()), want);
        assert_eq!(glider::codec::Reader::new(&bad).skip_value(), want);
        assert_eq!(glider::codec::Reader::new(&bad).skip_value_trusted(), want);
        // A list the record cannot hold is still caught first.
        assert!(glider::codec::Reader::new(&[5, 9]).value().is_err());
    });
}
