//! A cursor spools nodes as JSON: a whole float must read back a float.
use glider::{
    query::{self, Cursor, QueryLimits},
    Graph, Value,
};

#[test]
fn whole_floats_survive_the_cursor_spool() {
    let mut g = Graph::memory();
    query::execute(&mut g, "CREATE (:Bar {length: 3.0, w: 2.5, n: 4})").unwrap();
    let mut c = Cursor::open(
        &mut g,
        "MATCH (b:Bar) RETURN b",
        &[],
        QueryLimits::default(),
    )
    .unwrap();
    let (batch, _) = c.next_batch().unwrap();
    let Value::Text(json) = &batch.rows[0][0] else {
        panic!("{:?}", batch.rows[0][0])
    };
    assert!(json.contains("\"length\":3.0"), "{json}");
    assert!(json.contains("\"n\":4"), "{json}");
    assert!(json.contains("\"w\":2.5"), "{json}");
}
