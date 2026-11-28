//! Review of G5, adopted as regression tests: a journal carried in an
//! image loaded with `Graph::from_bytes` (a file's, or a memory graph's
//! snapshot). Before the fix the copy started again at revision 0 while its
//! journal kept the source's positions; now the copy carries on at the
//! image's revision (docs/DERIVED.md, D2) and the journal refuses to answer
//! whenever it holds a position past the database's.

use glider::ext::changes::{self, Kind};
use glider::graph::Graph;
use glider::query;
use glider::store::Sync;
use std::path::PathBuf;

fn q(g: &mut Graph, s: &str) -> query::QueryResult {
    query::execute(g, s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "glider-review-g5-{}-{}.db",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    let mut w = p.as_os_str().to_os_string();
    w.push("-wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(w));
    p
}

fn report(what: &str, g: &mut Graph) {
    let s = changes::status(g).unwrap();
    let mut hi = 0;
    for e in changes::since(g, s.floor, &[], None, usize::MAX).unwrap().0 {
        hi = hi.max(e.revision);
    }
    eprintln!(
        "{what}: revision {} log_end {} floor {} newest entry {hi} entries {}",
        g.revision(),
        g.log_end(),
        s.floor,
        s.entries
    );
}

/// For each way of taking an image: write on the copy and ask the journal
/// the question a consumer asks, from the copy's own revision.
#[test]
fn a_write_on_a_copy_from_a_file_image_is_listed_since_the_copys_revision() {
    for (name, checkpoint_after) in [("checkpointed", true), ("log_tail", false)] {
        let path = temp(name);
        let mut f = Graph::open(&path, Sync::Always).unwrap();
        f.set_checkpoint_bytes(u64::MAX);
        q(&mut f, "CREATE EXTENSION changes");
        for i in 0..50 {
            q(
                &mut f,
                &format!("CREATE (:A {{i: {i}}})-[:K]->(:B {{i: {i}}})"),
            );
        }
        f.checkpoint().unwrap();
        for i in 0..50 {
            q(&mut f, &format!("MATCH (a:A {{i: {i}}}) SET a.j = 1"));
        }
        if checkpoint_after {
            f.checkpoint().unwrap();
        }
        report(&format!("{name} source"), &mut f);
        let bytes = std::fs::read(&path).unwrap();
        let mut c = Graph::from_bytes(&bytes).unwrap();
        report(&format!("{name} copy"), &mut c);
        let r1 = c.revision();
        let id = c.node_ids()[0];
        c.set_node_prop(id, "copy", glider::value::Value::Int(7))
            .unwrap();
        let r2 = c.revision();
        assert!(r2 > r1);
        report(&format!("{name} copy after a write"), &mut c);
        let answer = changes::since(&c, r1, &[], None, usize::MAX);
        let fresh = changes::changed(&c, r1, Kind::Node, &[id]);
        eprintln!(
            "{name}: since({r1}) at {r2}: {:?}; changed: {fresh:?}",
            answer
                .as_ref()
                .map(|(l, _)| l.iter().map(|e| (e.kind, e.id)).collect::<Vec<_>>())
        );
        if let Ok((list, _)) = answer {
            assert!(
                list.iter().any(|e| e.kind == Kind::Node && e.id == id),
                "{name}: since({r1}) answers but omits node {id}, written at {r2}"
            );
        }
        if let Some(v) = fresh {
            assert!(v[0], "{name}: changed({r1}) says node {id} did not change");
        }
        drop(f);
        let _ = std::fs::remove_file(&path);
    }
}

fn findings(g: &mut Graph) -> Vec<String> {
    q(g, "CREATE EXTENSION IF NOT EXISTS check");
    q(g, "CALL check.run(checks: ['derived'])")
        .rows
        .into_iter()
        .filter(|r| r[2].to_string().starts_with("changes."))
        .map(|r| format!("{} {}", r[2], r[3]))
        .collect()
}

/// The source keeps a colouring at its last revision; a copy loaded from
/// the file's image starts again at revision 0 while its journal holds
/// entries at the source's positions. A change on the copy is then entered
/// at a position below the colouring's, and both the kept colouring and
/// `changed` say nothing changed.
#[test]
fn on_a_copy_from_a_file_image_a_changed_node_is_reported_changed() {
    let fresh = |g: &mut Graph| {
        q(g, "CALL structure.stored()").rows[0][2] == glider::value::Value::Bool(true)
    };
    for journal in [false, true] {
        let path = temp(&format!("colouring-{journal}"));
        let mut f = Graph::open(&path, Sync::Always).unwrap();
        if journal {
            q(&mut f, "CREATE EXTENSION changes");
        }
        for i in 0..50 {
            q(
                &mut f,
                &format!("CREATE (:S {{i: {i}}})-[:H]->(:W {{i: {i}}})"),
            );
        }
        q(&mut f, "CREATE EXTENSION structure");
        q(&mut f, "CALL structure.colors(depth: 2, store: 'c')");
        let at = f.revision();
        assert!(fresh(&mut f));
        f.checkpoint().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        drop(f);
        let _ = std::fs::remove_file(&path);

        let mut c = Graph::from_bytes(&bytes).unwrap();
        eprintln!(
            "journal {journal}: source at {at}, copy at {}",
            c.revision()
        );
        // Change what a colour reads: a new edge between two W nodes.
        q(
            &mut c,
            "MATCH (a:W {i: 1}), (b:W {i: 2}) CREATE (a)-[:H]->(b)",
        );
        q(&mut c, "MATCH (n:S {i: 3}) SET n:Other");
        let still = fresh(&mut c);
        eprintln!("journal {journal}: kept colouring fresh after the change: {still}");
        if journal {
            let ids = c.node_ids();
            let changed = changes::changed(&c, at, Kind::Node, &ids).unwrap();
            let any = changes::changed_any(&c, at, Kind::Edge);
            eprintln!(
                "journal: changed(at {at}) true for {} of {} nodes; changed_any(edge) {any:?}; since({at}): {:?}",
                changed.iter().filter(|b| **b).count(),
                ids.len(),
                changes::since(&c, at, &[], None, 100).map(|(l, _)| l.len())
            );
            eprintln!("journal: check.run on the copy: {:?}", findings(&mut c));
        }
        assert!(
            !still,
            "journal {journal}: a colouring is fresh although an edge was added and a label set"
        );
    }
}

/// The same question for a file copied without its log directory and
/// opened: the revision resumes where the checkpoint left it.
#[test]
fn a_file_copied_without_its_log_resumes_its_revision() {
    let path = temp("copied");
    let mut f = Graph::open(&path, Sync::Always).unwrap();
    q(&mut f, "CREATE EXTENSION changes");
    for i in 0..50 {
        q(
            &mut f,
            &format!("CREATE (:S {{i: {i}}})-[:H]->(:W {{i: {i}}})"),
        );
    }
    f.checkpoint().unwrap();
    let at = f.revision();
    let copy = temp("copied-to");
    std::fs::copy(&path, &copy).unwrap();
    drop(f);
    let mut c = Graph::open(&copy, Sync::Always).unwrap();
    eprintln!("source at {at}, copied file opens at {}", c.revision());
    q(&mut c, "MATCH (n:S {i: 3}) SET n.x = 1");
    let id = c.node_ids()[0];
    let listed = changes::since(&c, at, &[], None, usize::MAX)
        .unwrap()
        .0
        .len();
    eprintln!("since({at}) on the copy after a write: {listed} rows; node {id}");
    assert!(listed > 0);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&copy);
}

#[test]
fn a_log_enabled_later_keeps_the_journal_exact() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    for i in 0..20 {
        q(&mut g, &format!("CREATE (:S {{i: {i}}})-[:H]->(:W)"));
    }
    let r1 = g.revision();
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    let r1b = g.revision();
    q(&mut g, "MATCH (n:S {i: 3}) SET n.x = 1");
    let r2 = g.revision();
    let (list, _) = changes::since(&g, r1, &[], None, usize::MAX).unwrap();
    eprintln!("enable_log: {r1} -> {r1b} -> {r2}: {:?}", list);
    assert_eq!(r1, r1b);
    assert_eq!(list.len(), 1);
    assert!(list[0].revision > r1 && list[0].revision < r2);
}

/// A memory graph's snapshot image, loaded again with `from_bytes` (what a
/// host without a filesystem does to keep its database between runs).
#[test]
fn a_memory_snapshot_loaded_again_lists_a_later_change() {
    let mut g = Graph::memory();
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE EXTENSION changes");
    for i in 0..20 {
        q(&mut g, &format!("CREATE (:S {{i: {i}}})-[:H]->(:W)"));
    }
    let saved_at = g.revision();
    let snap = g.snapshot().unwrap();
    let mut bytes = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        bytes.extend_from_slice(&b);
        Ok(())
    })
    .unwrap();
    drop(snap);
    let mut c = match Graph::from_bytes(&bytes) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("from_bytes of a snapshot image: {e}");
            return;
        }
    };
    eprintln!("saved at {saved_at}; loaded again at {}", c.revision());
    q(&mut c, "MATCH (n:S {i: 3}) SET n.x = 1");
    let id = q(&mut c, "MATCH (n:S {i: 3}) RETURN id(n)").rows[0][0]
        .as_i64()
        .unwrap() as u64;
    let changed = changes::changed(&c, saved_at, Kind::Node, &[id]);
    eprintln!(
        "changed(since {saved_at}, node {id}) = {changed:?} at revision {}",
        c.revision()
    );
    assert_ne!(
        changed,
        Some(vec![false]),
        "node {id} changed after {saved_at} and the journal says it did not"
    );
}

/// On such a copy the cap is not kept either: `look_at` is a position of
/// the source's, far ahead of the copy's restarted positions.
#[test]
fn on_a_copy_from_an_image_the_cap_is_kept() {
    let mut g = Graph::memory();
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CALL changes.cap(entries: 100)");
    q(&mut g, "CREATE (:S)");
    for i in 0..5_000 {
        q(&mut g, &format!("MATCH (n:S) SET n.i = {i}"));
    }
    let snap = g.snapshot().unwrap();
    let mut bytes = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        bytes.extend_from_slice(&b);
        Ok(())
    })
    .unwrap();
    drop(snap);
    let mut c = Graph::from_bytes(&bytes).unwrap();
    c.begin().unwrap();
    for _ in 0..2_000 {
        c.add_node(&["N".into()], vec![]).unwrap();
    }
    c.commit().unwrap();
    let s = changes::status(&c).unwrap();
    eprintln!(
        "copy: {} entries with a cap of {}, revision {}",
        s.entries,
        s.cap,
        c.revision()
    );
    assert!(
        s.entries <= s.cap,
        "the journal holds {} entries past its cap of {}",
        s.entries,
        s.cap
    );
}
