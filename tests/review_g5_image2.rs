//! Review of G5 (adopted), second pass: images of every kind the engine writes,
//! taken at awkward moments, loaded with `Graph::from_bytes`.

use glider::ext::changes::{self, Kind, Mode};
use glider::graph::Graph;
use glider::query;
use glider::store::Sync;
use glider::value::Value;
use std::path::PathBuf;

fn q(g: &mut Graph, s: &str) -> query::QueryResult {
    query::execute(g, s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-review-g5b-{}-{}.db", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let mut w = p.as_os_str().to_os_string();
    w.push("-wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(w));
    p
}

fn mem_image(g: &Graph) -> Vec<u8> {
    let snap = g.snapshot().unwrap();
    let mut bytes = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        bytes.extend_from_slice(&b);
        Ok(())
    })
    .unwrap();
    bytes
}

/// Everything a copy holds is at or below its revision; a change on it is
/// listed for every revision it answers for; a kept colouring is stale.
fn check_copy(what: &str, bytes: &[u8], source_revs: &[u64]) {
    let mut c = Graph::from_bytes(bytes).unwrap();
    let rev = c.revision();
    assert_eq!(c.log_end(), rev, "{what}");
    let st = changes::state_of(&c).unwrap();
    let mut hi = 0;
    c.ext_scan(changes::NS, &[], &mut |k, v| {
        if k.first().is_some_and(|b| *b <= 2) {
            let r = v[1..].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
            hi = hi.max(r);
        }
        true
    });
    let kept: Vec<u64> = query::execute(&mut c, "CALL structure.stored()")
        .map(|r| r.rows.iter().filter_map(|r| r[1].as_i64()).map(|v| v as u64).collect())
        .unwrap_or_default();
    eprintln!(
        "{what}: copy at {rev}; state at {} floor {} mode {:?}; newest entry {hi}; colourings at {kept:?}",
        st.at, st.floor, st.mode
    );
    assert!(st.at <= rev && st.floor <= rev && hi <= rev, "{what}: the image holds positions past its revision");
    assert!(kept.iter().all(|k| *k <= rev), "{what}: a colouring past the copy's revision");
    let fresh_before: Vec<bool> = query::execute(&mut c, "CALL structure.stored()")
        .map(|r| r.rows.iter().map(|r| r[2] == Value::Bool(true)).collect())
        .unwrap_or_default();
    // Change a node and an edge on the copy.
    let n = c.node_ids()[0];
    c.set_node_prop(n, "copy", Value::Int(1)).unwrap();
    if let Some(e) = c.edge_ids().first().copied() {
        c.set_edge_prop(e, "copy", Value::Int(1)).unwrap();
    }
    let now = c.revision();
    let mut asked: Vec<u64> = source_revs.to_vec();
    asked.push(rev);
    let (mut answered, mut refused) = (0, 0);
    for r in asked {
        match changes::since(&c, r, &[], None, usize::MAX) {
            Ok((list, _)) => {
                answered += 1;
                assert!(r <= rev, "{what}: answered for {r}, past the copy's {rev}");
                assert!(
                    list.iter().any(|e| e.kind == Kind::Node && e.id == n),
                    "{what}: since({r}) at {now} omits node {n}"
                );
                assert_eq!(changes::changed(&c, r, Kind::Node, &[n]), Some(vec![true]), "{what}");
            }
            Err(_) => {
                refused += 1;
                assert_eq!(changes::changed(&c, r, Kind::Node, &[n]), None, "{what}");
            }
        }
    }
    let fresh_after: Vec<bool> = query::execute(&mut c, "CALL structure.stored()")
        .map(|r| r.rows.iter().map(|r| r[2] == Value::Bool(true)).collect())
        .unwrap_or_default();
    eprintln!("{what}: {answered} answered, {refused} refused; colourings fresh {fresh_before:?} -> {fresh_after:?}");
    assert!(fresh_after.iter().all(|f| !f), "{what}: a colouring fresh after a change");
}

fn build(g: &mut Graph, revs: &mut Vec<u64>, n: usize) {
    for i in 0..n {
        q(g, &format!("CREATE (:S {{i: {i}}})-[:H]->(:W {{i: {i}}})"));
        revs.push(g.revision());
    }
}

#[test]
fn memory_snapshots_at_awkward_moments_hold_nothing_past_their_revision() {
    // Right after install, in the same transaction's aftermath.
    let mut g = Graph::memory();
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    let mut revs = vec![g.revision()];
    build(&mut g, &mut revs, 5);
    q(&mut g, "CREATE EXTENSION structure");
    q(&mut g, "CALL structure.colors(depth: 2, store: 'c')");
    q(&mut g, "CREATE EXTENSION changes");
    revs.push(g.revision());
    check_copy("mem: after install", &mem_image(&g), &revs);

    // Right after a floor raise.
    build(&mut g, &mut revs, 5);
    let below = revs[revs.len() - 2];
    q(&mut g, &format!("CALL changes.release(below: {below})"));
    revs.push(g.revision());
    check_copy("mem: after release", &mem_image(&g), &revs);

    // Mid-sweep, with a small cap.
    q(&mut g, "CALL changes.cap(entries: 200)");
    let mut tries = 0;
    loop {
        q(&mut g, "CREATE (:T)-[:H]->(:T)");
        revs.push(g.revision());
        tries += 1;
        if changes::state_of(&g).unwrap().mode == Mode::Sweeping || tries > 5000 {
            break;
        }
    }
    assert_eq!(changes::state_of(&g).unwrap().mode, Mode::Sweeping);
    check_copy("mem: mid-sweep", &mem_image(&g), &revs);

    // A block the engine rolled back (spent), just ended.
    g.begin().unwrap();
    g.add_node(&["InBlock".into()], vec![]).unwrap();
    let (used, max) = g.stats().memory.unwrap();
    g.set_max_memory(used);
    let mut spent = false;
    for _ in 0..10_000 {
        if g.add_node(&["Big".into()], vec![("p".into(), Value::Text("x".repeat(4000)))]).is_err() {
            spent = true;
            break;
        }
    }
    g.set_max_memory(max);
    assert!(spent);
    assert!(g.commit().is_err());
    revs.push(g.revision());
    check_copy("mem: after a spent block", &mem_image(&g), &revs);

    // An image taken inside an open block.
    g.begin().unwrap();
    g.add_node(&["Open".into()], vec![]).unwrap();
    q(&mut g, "CALL structure.colors(depth: 1, store: 'open')");
    match g.snapshot() {
        Ok(_) => {
            let img = mem_image(&g);
            check_copy("mem: inside an open block", &img, &revs);
        }
        Err(e) => eprintln!("snapshot inside a block: {e}"),
    }
    g.rollback().unwrap();
}

#[test]
fn file_images_at_awkward_moments_hold_nothing_past_their_revision() {
    for checkpoint in [true, false] {
        let path = temp(&format!("awk-{checkpoint}"));
        let mut f = Graph::open(&path, Sync::Always).unwrap();
        f.set_checkpoint_bytes(u64::MAX);
        let mut revs = vec![f.revision()];
        build(&mut f, &mut revs, 5);
        q(&mut f, "CREATE EXTENSION structure");
        q(&mut f, "CALL structure.colors(depth: 2, store: 'c')");
        if checkpoint {
            f.checkpoint().unwrap();
        }
        q(&mut f, "CREATE EXTENSION changes");
        revs.push(f.revision());
        if checkpoint {
            f.checkpoint().unwrap();
        }
        let what = format!("file ckpt={checkpoint}: after install");
        let bytes = std::fs::read(&path).unwrap();
        // A file image without the journal at all (not checkpointed since
        // the install) is fine: it has no journal.
        if Graph::from_bytes(&bytes)
            .ok()
            .is_some_and(|c| changes::state_of(&c).is_some())
        {
            check_copy(&what, &bytes, &revs);
        }
        build(&mut f, &mut revs, 30);
        q(&mut f, "CALL changes.cap(entries: 100)");
        for _ in 0..200 {
            q(&mut f, "CREATE (:T)-[:H]->(:T)");
            revs.push(f.revision());
            if checkpoint && f.revision() % 3 == 0 {
                f.checkpoint().unwrap();
            }
        }
        if checkpoint {
            f.checkpoint().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        if Graph::from_bytes(&bytes)
            .ok()
            .is_some_and(|c| changes::state_of(&c).is_some())
        {
            check_copy(&format!("file ckpt={checkpoint}: after sweeps"), &bytes, &revs);
        }
        drop(f);
        let _ = std::fs::remove_file(&path);
    }
}

/// A file with a small page cache, written without checkpoints, so pages
/// are evicted to the file before any checkpoint: the image still holds
/// only its checkpoint's state.
#[test]
fn a_file_image_with_evicted_pages_holds_nothing_past_its_revision() {
    let path = temp("evict");
    let opts = glider::graph::OpenOptions {
        cache_size: 64 * 1024,
        ..Default::default()
    };
    let mut f = Graph::open_opts(&path, opts).unwrap();
    f.set_checkpoint_bytes(u64::MAX);
    let mut revs = vec![f.revision()];
    q(&mut f, "CREATE EXTENSION changes");
    q(&mut f, "CREATE EXTENSION structure");
    build(&mut f, &mut revs, 50);
    f.checkpoint().unwrap();
    for i in 0..3000 {
        q(&mut f, &format!("CREATE (:E {{i: {i}, pad: '{}'}})", "y".repeat(200)));
        revs.push(f.revision());
    }
    q(&mut f, "CALL structure.colors(depth: 2, store: 'late')");
    let bytes = std::fs::read(&path).unwrap();
    check_copy("file with evicted pages", &bytes, &revs);
    drop(f);
    let _ = std::fs::remove_file(&path);
}

/// A colouring kept last before the image is fresh on the copy, and stale
/// once the copy changes what a colour reads; on a file and in memory.
#[test]
fn a_colouring_kept_last_before_an_image_goes_stale_on_the_copy() {
    let mut g = Graph::memory();
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION structure");
    let mut revs = vec![];
    build(&mut g, &mut revs, 20);
    q(&mut g, "CALL structure.colors(depth: 2, store: 'c')");
    let fresh = |g: &mut Graph| q(g, "CALL structure.stored()").rows[0][2] == Value::Bool(true);
    assert!(fresh(&mut g));
    let mut c = Graph::from_bytes(&mem_image(&g)).unwrap();
    assert!(fresh(&mut c), "fresh on the copy before any change");
    q(&mut c, "MATCH (a:W {i: 1}), (b:W {i: 2}) CREATE (a)-[:H]->(b)");
    assert!(!fresh(&mut c), "stale after an edge on the copy");

    let path = temp("colour-last");
    let mut f = Graph::open(&path, Sync::Always).unwrap();
    q(&mut f, "CREATE EXTENSION changes");
    q(&mut f, "CREATE EXTENSION structure");
    build(&mut f, &mut revs, 20);
    q(&mut f, "CALL structure.colors(depth: 2, store: 'c')");
    f.checkpoint().unwrap();
    let mut c = Graph::from_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert!(fresh(&mut c), "file copy: fresh before any change");
    q(&mut c, "MATCH (n:S {i: 3}) SET n:Other");
    assert!(!fresh(&mut c), "file copy: stale after a label");
    drop(f);
    let _ = std::fs::remove_file(&path);
}
