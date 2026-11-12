use glider::{
    budget,
    query::{self, Cursor, QueryLimits},
    Graph, OpenOptions, Value,
};
use std::path::PathBuf;
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glider-bounded-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("db")
}
fn populate(g: &mut Graph, n: u64) {
    g.autocommit = false;
    for i in 0..n {
        g.add_node(
            &["N".into()],
            vec![
                ("i".into(), Value::Int(i as i64)),
                ("text".into(), Value::Text("x".repeat(256))),
            ],
        )
        .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
}

#[test]
fn result_sort_collect_and_distinct_stop_at_work_mem() {
    let _serial = SERIAL.lock().unwrap();
    let mut g = Graph::memory();
    populate(&mut g, 3000);
    // A file-backed graph's work_mem, rather than :memory: page headroom.
    let p = path("workspace");
    let mut disk = Graph::open_opts(
        &p,
        OpenOptions {
            work_mem: 32 << 10,
            cache_size: 1 << 20,
            ..Default::default()
        },
    )
    .unwrap();
    populate(&mut disk, 3000);
    for q in [
        "MATCH (n) RETURN n",
        "MATCH (n) RETURN n.i ORDER BY n.i",
        "MATCH (n) RETURN collect(n.text)",
        "MATCH (n) RETURN DISTINCT n.i",
        "MATCH (n) RETURN n.i, count(*)",
    ] {
        assert!(query::execute(&mut disk, q).is_err(), "must reject {q}");
    }
    let r = query::execute(&mut disk, "MATCH (n) RETURN n.i ORDER BY n.i DESC LIMIT 5").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(2999));
    assert_eq!(r.rows.len(), 5);
    let caps = QueryLimits {
        max_rows: 2,
        max_bytes: 4096,
    };
    assert!(query::execute_capped(&mut disk, "MATCH (n) RETURN n.i", &[], caps).is_err());
    assert!(query::execute_capped(
        &mut disk,
        "MATCH (n) RETURN n LIMIT 5",
        &[],
        QueryLimits {
            max_rows: 10,
            max_bytes: 100
        }
    )
    .is_err());
    assert_eq!(
        query::execute(&mut disk, "MATCH (n) RETURN count(*)")
            .unwrap()
            .rows[0][0],
        Value::Int(3000)
    );
}

#[test]
fn cursor_batches_are_capped_complete_and_removed_on_drop() {
    let _serial = SERIAL.lock().unwrap();
    let p = path("cursor");
    let mut g = Graph::open_opts(
        &p,
        OpenOptions {
            work_mem: 1 << 20,
            cache_size: 1 << 20,
            ..Default::default()
        },
    )
    .unwrap();
    populate(&mut g, 3000);
    let mut cursor = Cursor::open(
        &mut g,
        "MATCH (n:N) RETURN n.i",
        &[],
        QueryLimits {
            max_rows: 17,
            max_bytes: 1000,
        },
    )
    .unwrap();
    let mut all = Vec::new();
    loop {
        let (r, done) = cursor.next_batch().unwrap();
        assert!(r.rows.len() <= 17);
        assert!(r.rows.iter().map(|r| query::row_bytes(r)).sum::<u64>() <= 1000);
        all.extend(r.rows);
        if done {
            break;
        }
    }
    assert_eq!(all.len(), 3000);
    for (i, row) in all.iter().enumerate() {
        assert_eq!(row[0], Value::Int(i as i64));
    }
    drop(cursor);
    assert_eq!(std::fs::read_dir(g.temp_dir()).unwrap().count(), 0);
    assert!(Cursor::open(
        &mut g,
        "MATCH (n) RETURN n",
        &[],
        QueryLimits {
            max_rows: 100,
            max_bytes: 128
        }
    )
    .is_err());
    assert_eq!(std::fs::read_dir(g.temp_dir()).unwrap().count(), 0);
}

#[test]
fn write_slices_commit_once_and_roll_back_the_entire_failed_slice() {
    let _serial = SERIAL.lock().unwrap();
    let mut g = Graph::memory();
    let statements = (0..1000)
        .map(|i| {
            (
                "CREATE (:N {i:$i})".to_string(),
                vec![("i".to_string(), Value::Int(i))],
            )
        })
        .collect::<Vec<_>>();
    let before = g.telemetry().commits;
    assert_eq!(query::execute_batch(&mut g, &statements).unwrap(), 1000);
    assert_eq!(g.telemetry().commits - before, 1);
    assert_eq!(g.node_count(), 1000);
    let invalid = vec![("CREATE (:N)".into(), vec![]), ("BEGIN".into(), vec![])];
    assert!(query::execute_batch(&mut g, &invalid).is_err());
    assert_eq!(g.node_count(), 1000);
    let mut g = Graph::memory_with_limit(1 << 20);
    populate(&mut g, 300);
    let bad = vec![
        ("CREATE (:N)".into(), vec![]),
        (
            "MATCH (n:N) CREATE (:X {text:$text})".into(),
            vec![("text".into(), Value::Text("x".repeat(4096)))],
        ),
    ];
    assert!(query::execute_batch(&mut g, &bad).is_err());
    assert_eq!(g.node_count(), 300);
    assert!(g.autocommit);
}

#[test]
fn streaming_import_remaps_keys_on_disk_and_recovers_after_bad_record() {
    let _serial = SERIAL.lock().unwrap();
    let mut g = Graph::memory();
    let input = b"{\"id\":42,\"key\":\"a\",\"labels\":[\"N\"]}\n{\"id\":99,\"key\":\"b\"}\n{\"type\":\"edge\",\"from\":\"a\",\"to\":99,\"label\":\"R\"}\n";
    assert_eq!(
        query::import_jsonl_from(&mut g, &mut input.as_slice(), 4096).unwrap(),
        (2, 1)
    );
    let mut export = Vec::new();
    query::export_jsonl_to(&g, &mut export).unwrap();
    let mut restored = Graph::memory();
    assert_eq!(
        query::import_jsonl_from(&mut restored, &mut export.as_slice(), 4096).unwrap(),
        (2, 1)
    );
    assert_eq!(restored.edge_count(), 1);
    assert!(
        query::import_jsonl_from(&mut g, &mut b"{\"key\":\"c\"}\ninvalid\n".as_slice(), 4096)
            .is_err()
    );
    assert!(g.autocommit);
    assert_eq!(g.node_count(), 2);
    assert!(query::import_jsonl_from(&mut g, &mut vec![b'x'; 4097].as_slice(), 4096).is_err());
}

#[test]
fn multiple_databases_share_one_cache_and_reclaim_idle_frames() {
    let _serial = SERIAL.lock().unwrap();
    // One process pool covers both database caches.
    let b = budget::configure(budget::MemoryBudget {
        limit: 256 << 20,
        budget: 200 << 20,
        cache: 2 << 20,
        work: 32 << 20,
        beam: 166 << 20,
    });
    let p1 = path("pool1");
    let p2 = path("pool2");
    let mut a = Graph::open_opts(
        &p1,
        OpenOptions {
            cache_size: 64 << 20,
            ..Default::default()
        },
    )
    .unwrap();
    populate(&mut a, 3000);
    let mut bdb = Graph::open_opts(
        &p2,
        OpenOptions {
            cache_size: 64 << 20,
            ..Default::default()
        },
    )
    .unwrap();
    populate(&mut bdb, 3000);
    for g in [&mut a, &mut bdb] {
        let r = query::execute(g, "MATCH (n) RETURN n.i LIMIT 10").unwrap();
        assert_eq!(r.rows.len(), 10);
        assert!(b.cache.used() <= b.cache.limit());
    }
    glider::storage::pager::resize_shared_cache(1 << 20);
    assert!(b.cache.used() <= 1 << 20);
    glider::storage::pager::resize_shared_cache(2 << 20);
}
