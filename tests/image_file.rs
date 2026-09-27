//! The legacy v3 file format (snapshot image at the front, log after it),
//! read by `glider::legacy` — kept for migrating old files.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use glider::codec;
use glider::legacy::graph::{Dir, Graph, OpenOptions};
use glider::legacy::image::Residency;
use glider::store::{self, Sync};
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-img-{}-{}.gldb", name, std::process::id()));
    let _ = fs::remove_file(&p);
    let _ = fs::remove_file(store::lock_path(&p));
    let _ = fs::remove_file(store::compact_tmp_path(&p));
    p
}

fn no_auto(sync: Sync) -> OpenOptions {
    OpenOptions {
        sync,
        auto_compact: None,
        ..OpenOptions::default()
    }
}

fn people(g: &mut Graph, n: usize) {
    for i in 0..n {
        g.add_node(
            &["Person".into()],
            vec![
                ("name".into(), Value::Text(format!("p{i}"))),
                ("age".into(), Value::Int(i as i64 % 90)),
            ],
        )
        .unwrap();
    }
    for i in 1..n as u64 {
        g.add_edge(
            i,
            i + 1,
            "KNOWS",
            vec![("w".into(), Value::Float(i as f64))],
        )
        .unwrap();
    }
    g.commit().unwrap();
}

#[test]
fn compaction_writes_an_image_and_reopen_reads_it_plus_the_tail() {
    let path = temp("reopen");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Always)).unwrap();
        g.create_index("Person", "name").unwrap();
        people(&mut g, 200);
        g.compact().unwrap();
        let h = store::read_header(&path).unwrap();
        assert_eq!(h.version, 3);
        assert!(h.image_len > 0);
        assert_eq!(h.header_len, 64 + h.image_len);
        // Right after compaction the log is empty: the committed end is the
        // log start, and nothing below it is ever scanned as a record.
        assert_eq!(store::scan_committed_end(&path, 0).unwrap(), h.header_len);
        assert_eq!(g.file_len(), h.header_len);

        // A tail after the image.
        g.set_node_prop(5, "age", Value::Int(-1)).unwrap();
        g.delete_node(7).unwrap();
        let n = g
            .add_node(
                &["Person".into()],
                vec![("name".into(), Value::from("new"))],
            )
            .unwrap();
        g.add_edge(n, 1, "KNOWS", vec![]).unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
    let s = g.stats();
    assert!(s.image_bytes > 0);
    assert!(s.tail_bytes > 0);
    assert_eq!(g.node_count(), 200);
    assert_eq!(g.edge_count(), 199 - 2 + 1);
    assert_eq!(g.node_prop(5, "age"), Some(Value::Int(-1)));
    assert!(g.node(7).is_none());
    assert_eq!(
        g.indexed_lookup("Person", "name", &Value::from("new")),
        Some(vec![201])
    );
    assert_eq!(
        g.indexed_lookup("Person", "name", &Value::from("p6")),
        Some(vec![])
    );
    assert_eq!(
        g.indexed_lookup("Person", "name", &Value::from("p9")),
        Some(vec![10])
    );
    let out: Vec<u64> = g
        .neighbors(1, Dir::Both, None)
        .iter()
        .map(|a| a.other)
        .collect();
    assert_eq!(out, vec![2, 201]);
    assert_eq!(g.edge_prop(3, "w"), Some(Value::Float(3.0)));
    let _ = fs::remove_file(&path);
}

#[test]
fn a_torn_tail_after_an_image_never_cuts_into_the_image() {
    let path = temp("torn");
    let (log_start, gen_before) = {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Always)).unwrap();
        people(&mut g, 50);
        g.compact().unwrap();
        g.add_node(&["Late".into()], vec![]).unwrap();
        g.commit().unwrap();
        let h = store::read_header(&path).unwrap();
        (h.header_len, h.generation)
    };
    let committed = fs::metadata(&path).unwrap().len();
    assert!(committed > log_start);
    // Garbage where the next record would be, like a crash mid-append.
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[9u8; 37])
        .unwrap();

    let g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
    assert_eq!(g.node_count(), 51);
    drop(g);
    assert_eq!(fs::metadata(&path).unwrap().len(), committed);
    let h = store::read_header(&path).unwrap();
    assert_eq!(h.header_len, log_start, "image untouched");
    assert_ne!(h.generation, gen_before, "truncation starts a new lineage");
    let v = store::verify(&path).unwrap();
    assert!(v.bad_offset.is_none());
    assert!(matches!(v.image, Some(Ok(_))));
    let _ = fs::remove_file(&path);
}

#[test]
fn a_compaction_temp_file_left_by_a_crash_is_removed_on_open() {
    let path = temp("stale");
    {
        let mut g = Graph::open(&path, Sync::Normal).unwrap();
        people(&mut g, 3);
    }
    let tmp = store::compact_tmp_path(&path);
    fs::write(&tmp, b"half a compaction").unwrap();
    let g = Graph::open(&path, Sync::Normal).unwrap();
    assert_eq!(g.node_count(), 3);
    assert!(!tmp.exists());
    let _ = fs::remove_file(&path);
}

// ------------------------------------------------ hand-built older formats

fn record(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    let start = out.len();
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    let crc = codec::crc32(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

fn node_add(id: u64, label: &str, name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    codec::put_varint(&mut p, id);
    codec::put_varint(&mut p, 1);
    codec::put_str(&mut p, label);
    codec::put_varint(&mut p, 1);
    codec::put_str(&mut p, "name");
    codec::put_value(&mut p, &Value::from(name));
    p
}

fn edge_add(id: u64, from: u64, to: u64) -> Vec<u8> {
    let mut p = Vec::new();
    codec::put_varint(&mut p, id);
    codec::put_varint(&mut p, from);
    codec::put_varint(&mut p, to);
    codec::put_str(&mut p, "R");
    codec::put_varint(&mut p, 0);
    p
}

fn old_log(header: &[u8]) -> Vec<u8> {
    let mut f = header.to_vec();
    record(&mut f, 0, &node_add(1, "P", "ada"));
    record(&mut f, 0, &node_add(2, "P", "bob"));
    record(&mut f, 2, &edge_add(1, 1, 2));
    record(&mut f, 255, &[]);
    f
}

#[test]
fn v1_and_v2_files_open_and_compact_to_v3() {
    let mut v1 = b"GRAPHLT\x01".to_vec();
    v1.extend_from_slice(&1u32.to_le_bytes());
    v1.extend_from_slice(&0u32.to_le_bytes());
    let mut v2 = b"GLIDER\x00\x01".to_vec();
    v2.extend_from_slice(&2u32.to_le_bytes());
    v2.extend_from_slice(&0u32.to_le_bytes());
    v2.extend_from_slice(&[0xab; 16]);

    for (name, header, version) in [("v1", v1, 1), ("v2", v2, 2)] {
        let path = temp(name);
        fs::write(&path, old_log(&header)).unwrap();
        assert_eq!(store::read_header(&path).unwrap().version, version);

        // Also readable straight from bytes, as the wasm build does.
        let g = Graph::from_bytes(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(g.node_prop(2, "name"), Some(Value::from("bob")));

        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        assert_eq!((g.node_count(), g.edge_count()), (2, 1));
        g.compact().unwrap();
        drop(g);
        let h = store::read_header(&path).unwrap();
        assert_eq!(h.version, 3, "{name}");
        assert!(h.image_len > 0);
        let g = Graph::open(&path, Sync::Normal).unwrap();
        assert_eq!(g.node_prop(1, "name"), Some(Value::from("ada")));
        assert_eq!(g.neighbors(1, Dir::Out, None)[0].other, 2);
        let _ = fs::remove_file(&path);
    }
}

#[test]
fn a_transaction_with_an_undecodable_record_is_discarded_whole() {
    let path = temp("atomic");
    {
        let _ = Graph::open(&path, Sync::Normal).unwrap();
    }
    let mut f = fs::read(&path).unwrap();
    record(&mut f, 0, &node_add(1, "P", "kept"));
    record(&mut f, 255, &[]);
    let good_end = f.len() as u64;
    // Second transaction: a fine record, then one whose CRC is valid but
    // whose payload does not decode, then a commit marker.
    record(&mut f, 0, &node_add(2, "P", "lost"));
    record(&mut f, 0, &[0x80]);
    record(&mut f, 255, &[]);
    fs::write(&path, &f).unwrap();

    let g = Graph::open(&path, Sync::Normal).unwrap();
    assert_eq!(g.node_count(), 1);
    assert!(
        g.node(2).is_none(),
        "no part of a broken transaction applies"
    );
    drop(g);
    assert_eq!(fs::metadata(&path).unwrap().len(), good_end);
    let _ = fs::remove_file(&path);
}

#[test]
fn from_bytes_reads_an_image_and_its_tail() {
    let path = temp("bytes");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        people(&mut g, 30);
        g.compact().unwrap();
        g.set_node_prop(3, "name", Value::from("changed")).unwrap();
    }
    let g = Graph::from_bytes(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(g.node_count(), 30);
    assert_eq!(g.node_prop(3, "name"), Some(Value::from("changed")));
    assert_eq!(g.node_prop(4, "name"), Some(Value::from("p3")));
    let _ = fs::remove_file(&path);
}

#[test]
fn a_corrupt_image_is_an_error_not_a_panic() {
    let path = temp("corrupt");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        g.create_index("Person", "name").unwrap();
        people(&mut g, 100);
        g.compact().unwrap();
    }
    let h = store::read_header(&path).unwrap();
    let clean = fs::read(&path).unwrap();
    let preload = OpenOptions {
        preload: true,
        ..no_auto(Sync::Normal)
    };
    // Reads that between them touch every column, every index and every
    // property chunk.
    let touch_everything = |g: &Graph| {
        for id in g.node_ids() {
            let _ = g.node_props(id);
            let _ = g.node_labels(id);
            let _ = g.neighbors(id, Dir::Both, None);
        }
        for id in g.edge_ids() {
            let _ = g.edge_props(id);
            let _ = g.edge(id).map(|e| (e.from, e.to, e.etype));
            let _ = g.edge_type_name(id);
        }
        let _ = g.edges_with_type("KNOWS");
        let _ = g.stats();
        let _ = g.sample_keys(1000);
        let _ = g.csr(Dir::Both, None, Some("w"));
        let _ = g.nodes_with_label("Person");
        let _ = g.indexed_lookup("Person", "name", &Value::from("p7"));
        let _ = g.label_count("Person");
    };
    // Flip one byte at a spread of places inside the image. Parts of an
    // image load on first use, so damage may surface at open or only when a
    // query reaches it — but always as an error, never a panic, and never
    // as results.
    let points = 200u64;
    for k in 0..points {
        let at = (h.image_at + k * h.image_len / points) as usize;
        let mut bytes = clean.clone();
        bytes[at] ^= 0x5a;
        fs::write(&path, &bytes).unwrap();
        let _ = fs::remove_file(store::lock_path(&path));

        assert!(
            Graph::open_opts(&path, preload).is_err(),
            "flip at {at} passed a preloading open"
        );
        let _ = fs::remove_file(store::lock_path(&path));
        assert!(
            Graph::from_bytes(&bytes).is_err(),
            "flip at {at} loaded from bytes"
        );
        let v = store::verify(&path).unwrap();
        assert!(matches!(v.image, Some(Err(_))), "flip at {at} verified");

        if let Ok(mut g) = Graph::open_opts(&path, no_auto(Sync::Normal)) {
            touch_everything(&g);
            assert!(g.integrity_error().is_some(), "flip at {at}: not recorded");
            assert!(g.compact().is_err(), "flip at {at}: compacted a damaged image");
            assert!(g.stats().integrity_error.is_some());
        }
        let _ = fs::remove_file(store::lock_path(&path));
    }
    // A flip in the header is caught by its own checksum.
    let mut bytes = clean.clone();
    bytes[33] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(Graph::open(&path, Sync::Normal).is_err());
    let _ = fs::remove_file(store::lock_path(&path));
    let _ = fs::remove_file(&path);
}

#[test]
fn a_corrupt_property_chunk_on_disk_reads_as_empty_and_is_counted() {
    let path = temp("coldcorrupt");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        people(&mut g, 100);
        g.compact().unwrap();
    }
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            residency: Residency::OnDisk {
                cache_bytes: 1 << 20,
            },
            ..no_auto(Sync::Normal)
        },
    )
    .unwrap();
    assert_eq!(g.node_prop(10, "name"), Some(Value::from("p9")));
    assert!(g.stats().props_on_disk);
    assert_eq!(g.stats().read_errors, 0);
    g.commit().unwrap();
    drop(g);

    // Corrupt the first property chunk. Structure still loads — property
    // runs on disk are checked when read — so open succeeds and the damage
    // shows up as counted read errors, not a crash.
    let h = store::read_header(&path).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    let needle = b"p42";
    let at = bytes[h.image_at as usize..]
        .windows(3)
        .position(|w| w == needle)
        .unwrap()
        + h.image_at as usize;
    bytes[at + 1] = b'X';
    fs::write(&path, &bytes).unwrap();
    let g = Graph::open_opts(
        &path,
        OpenOptions {
            residency: Residency::OnDisk {
                cache_bytes: 1 << 20,
            },
            ..no_auto(Sync::Normal)
        },
    )
    .unwrap();
    assert_eq!(g.node_prop(43, "name"), None);
    assert!(g.stats().read_errors > 0);
    assert!(matches!(store::verify(&path).unwrap().image, Some(Err(_))));
    drop(g);
    let _ = fs::remove_file(&path);
}

#[test]
fn store_open_refuses_a_file_with_an_image() {
    let path = temp("storeopen");
    {
        let mut g = Graph::open(&path, Sync::Normal).unwrap();
        people(&mut g, 3);
        g.compact().unwrap();
    }
    assert!(store::Store::open(&path, Sync::Normal, |_| {}).is_err());
    let _ = fs::remove_file(&path);
}

#[test]
fn auto_compaction_folds_the_tail_into_an_image() {
    let path = temp("auto");
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            auto_compact: Some(4096),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    for i in 0..400 {
        g.add_node(&["N".into()], vec![("i".into(), Value::Int(i))])
            .unwrap();
    }
    let s = g.stats();
    assert!(s.image_bytes > 0, "an image was written");
    assert!(s.auto_compact_error.is_none());
    // Once the image is larger than the threshold, the tail may grow up to
    // the image size before the next rewrite.
    assert!(s.tail_bytes <= s.image_bytes.max(4096) + 64);
    drop(g);
    let g = Graph::open(&path, Sync::Normal).unwrap();
    assert_eq!(g.node_count(), 400);
    assert_eq!(g.node_prop(400, "i"), Some(Value::Int(399)));
    let _ = fs::remove_file(&path);
}

#[test]
fn a_read_only_session_never_triggers_auto_compaction() {
    let path = temp("readonly");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        people(&mut g, 300);
    }
    let len = fs::metadata(&path).unwrap().len();
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            auto_compact: Some(1),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(g.nodes_with_label("Person").len(), 300);
    g.commit().unwrap();
    drop(g);
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    assert_eq!(store::read_header(&path).unwrap().image_len, 0);
    let _ = fs::remove_file(&path);
}

#[test]
fn clear_after_an_image_then_compact() {
    let path = temp("clear");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        g.create_index("Person", "name").unwrap();
        people(&mut g, 20);
        g.compact().unwrap();
        g.clear().unwrap();
        assert_eq!(g.node_count(), 0);
        let a = g
            .add_node(&["Person".into()], vec![("name".into(), Value::from("z"))])
            .unwrap();
        assert_eq!(a, 1, "ids restart after a clear");
        g.compact().unwrap();
    }
    let g = Graph::open(&path, Sync::Normal).unwrap();
    assert_eq!(g.node_count(), 1);
    assert!(g.has_index("Person", "name"));
    assert_eq!(
        g.indexed_lookup("Person", "name", &Value::from("z")),
        Some(vec![1])
    );
    let _ = fs::remove_file(&path);
}

// ------------------------------------------------------- image::build

struct Tiny {
    bad: Option<&'static str>,
}

impl glider::legacy::image::ImageSource for Tiny {
    fn strings(&self) -> Vec<String> {
        ["P", "name", "R", "w"].iter().map(|s| s.to_string()).collect()
    }
    fn next_ids(&self) -> (u64, u64) {
        (100, 50)
    }
    fn indexes(&self) -> Vec<(u32, u32)> {
        vec![(0, 1)]
    }
    fn nodes(&self, f: &mut dyn FnMut(u64, &[u32], &[(u32, Value)])) -> std::io::Result<()> {
        let ids: &[u64] = if self.bad == Some("order") { &[5, 3] } else { &[3, 5, 9] };
        for id in ids {
            f(*id, &[0], &[(1, Value::Text(format!("n{id}")))]);
        }
        Ok(())
    }
    fn edges(
        &self,
        f: &mut dyn FnMut(u64, u64, u64, u32, &[(u32, Value)]),
    ) -> std::io::Result<()> {
        let to = if self.bad == Some("dangling") { 4 } else { 9 };
        let t = if self.bad == Some("string") { 17 } else { 2 };
        f(1, 3, 5, t, &[(3, Value::Float(0.5))]);
        f(7, 5, to, 2, &[]);
        f(8, 9, 9, 2, &[]);
        Ok(())
    }
}

#[test]
fn image_build_writes_a_database_from_a_stream() {
    let path = temp("build");
    glider::legacy::image::build(&path, &Tiny { bad: None }).unwrap();
    let mut g = Graph::open(&path, Sync::Normal).unwrap();
    assert_eq!((g.node_count(), g.edge_count()), (3, 3));
    assert_eq!(g.node_prop(5, "name"), Some(Value::from("n5")));
    assert_eq!(g.edge_prop(1, "w"), Some(Value::Float(0.5)));
    assert_eq!(g.indexed_lookup("P", "name", &Value::from("n9")), Some(vec![9]));
    let out: Vec<u64> = g.neighbors(9, Dir::Both, None).iter().map(|a| a.edge).collect();
    assert_eq!(out, vec![8, 7, 8], "self-loop appears once each way, runs in edge order");
    // Ids carry on from the source's next ids.
    assert_eq!(g.add_node(&[], vec![]).unwrap(), 100);
    assert!(matches!(store::verify(&path).unwrap().image, Some(Ok(_))));
    drop(g);
    let _ = fs::remove_file(&path);

    for bad in ["order", "dangling", "string"] {
        let path = temp(&format!("build-{bad}"));
        assert!(glider::legacy::image::build(&path, &Tiny { bad: Some(bad) }).is_err(), "{bad}");
        assert!(!path.exists(), "{bad}: a failed build leaves no file");
    }
}

#[test]
fn preload_loads_everything_and_a_graph_copies_into_a_new_file() {
    let path = temp("preload");
    {
        let mut g = Graph::open_opts(&path, no_auto(Sync::Normal)).unwrap();
        g.create_index("Person", "name").unwrap();
        people(&mut g, 500);
        g.compact().unwrap();
    }
    let g = Graph::open_opts(
        &path,
        OpenOptions {
            preload: true,
            ..no_auto(Sync::Normal)
        },
    )
    .unwrap();
    assert!(g.integrity_error().is_none());
    // A graph is an ImageSource: copy it, image and delta, to a new file.
    let copy = temp("preload-copy");
    glider::legacy::image::build(&copy, &g).unwrap();
    drop(g);
    let a = Graph::open(&path, Sync::Normal).unwrap();
    let b = Graph::open(&copy, Sync::Normal).unwrap();
    assert_eq!(a.node_ids(), b.node_ids());
    assert_eq!(a.edge_ids(), b.edge_ids());
    assert_eq!(b.indexed_lookup("Person", "name", &Value::from("p9")), Some(vec![10]));
    drop((a, b));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&copy);
}
