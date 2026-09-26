//! The v3 file format: snapshot image at the front, log after it.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use glider::codec;
use glider::graph::{Dir, Graph};
use glider::store::{self, Sync};
use glider::value::Value;
use glider::{OpenOptions, Residency};

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
        people(&mut g, 100);
        g.compact().unwrap();
    }
    let h = store::read_header(&path).unwrap();
    let clean = fs::read(&path).unwrap();
    // Flip one byte at a spread of places inside the image.
    for k in 0..40u64 {
        let at = (h.image_at + k * h.image_len / 40) as usize;
        let mut bytes = clean.clone();
        bytes[at] ^= 0x5a;
        fs::write(&path, &bytes).unwrap();
        let _ = fs::remove_file(store::lock_path(&path));
        assert!(
            Graph::open(&path, Sync::Normal).is_err(),
            "flip at {at} opened"
        );
        assert!(
            Graph::from_bytes(&bytes).is_err(),
            "flip at {at} loaded from bytes"
        );
        let v = store::verify(&path).unwrap();
        assert!(matches!(v.image, Some(Err(_))), "flip at {at} verified");
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
    glider::query::execute(&mut g, "MATCH (n:Person) RETURN count(n)").unwrap();
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
