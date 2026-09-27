#!/usr/bin/env python3
"""glider vs SQLite on graph workloads, at 500 MiB, 1, 5 and 10 GiB.

    python3 bench/sqlite_compare.py [--sizes 500MiB,1GiB] [--only build,query]
    python3 bench/sqlite_compare_report.py        # -> compare.html

Both engines hold the same graph. scale-gen writes it as a paged glider
database; `glider export` streams it as JSONL into a SQLite file with the
schema a SQL user would write for it: one table per label, one table per
relationship type with FOREIGN KEYs to its end tables, (src, dst) and
(dst, src) indexes on every relationship table, and the same property
indexes glider keeps (Person.email/age/country, Order.ref, Product.sku,
Event.seq). The one polymorphic relationship, MENTIONS (document -> person,
company or product), carries a kind column instead of a foreign key.

Every query is written once in glider's Cypher and once in SQL, and both
answers are compared row for row. Timings are cold (the files evicted from
the OS page cache, first run in a fresh process) and warm (median of the next
runs). Both are timed inside the process that runs the query: glider by its
shell's `.timer` (statements piped to `glider <db> --json`), SQLite around
execute + fetchall in Python's sqlite3. Both get a 1 GiB page cache.

Datasets and results live in ../glider-bench-data/compare (GLIDER_BENCH_DATA),
outside the repo: see bench/scale.py.
"""

import argparse
import json
import math
import os
import resource
import select
import sqlite3
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scale  # noqa: E402
from scale import Layout, evict, fsize, log  # noqa: E402

ROOT = scale.ROOT
DATA = os.path.join(scale.BENCH_DATA, "compare")
GLIDER = scale.NEW
GEN = os.path.join(ROOT, "target", "release", "scale-gen")
SIZES = ["500MiB", "1GiB", "5GiB", "10GiB"]
CACHE_KIB = 1 << 20  # 1 GiB, glider's default page cache

ARGS = None


def emit(rec):
    rec["t"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(ARGS.results, "a") as f:
        f.write(json.dumps(rec) + "\n")
    log("  ", json.dumps(rec)[:240])


def gpath(size):
    return os.path.join(DATA, f"glider-{size}.db")


def spath(size):
    return os.path.join(DATA, f"sqlite-{size}.db")


def tree_files(path):
    out = [path] if os.path.exists(path) else []
    for d in (path + "-data", path + "-wal"):
        if os.path.isdir(d):
            out += [os.path.join(d, f) for f in os.listdir(d)]
    for suf in ("-wal", "-journal"):
        if os.path.isfile(path + suf):
            out.append(path + suf)
    return out


def tree_bytes(path):
    return sum(fsize(p) for p in tree_files(path))


def evict_tree(path):
    for p in tree_files(path):
        evict(p)


# ------------------------------------------------------------------ schema

NODE_TABLES = {
    # table: (primary label, [(column, sql type, property)])
    "person": ("Person", [("name", "TEXT"), ("email", "TEXT"), ("age", "INTEGER"), ("country", "TEXT"),
                          ("city", "TEXT"), ("joined", "TEXT"), ("score", "REAL"), ("active", "INTEGER"),
                          ("tags", "TEXT"), ("bio", "TEXT"), ("nickname", "TEXT"), ("balance", "INTEGER")]),
    "company": ("Company", [("name", "TEXT"), ("industry", "TEXT"), ("founded", "INTEGER"), ("revenue", "REAL"),
                            ("hq", "TEXT")]),
    "product": ("Product", [("sku", "TEXT"), ("name", "TEXT"), ("price", "REAL"), ("stock", "INTEGER"),
                            ("attrs", "TEXT"), ("rating", "REAL")]),
    "orders": ("Order", [("ref", "TEXT"), ("total", "REAL"), ("status", "TEXT"), ("placed", "TEXT"),
                         ("items", "INTEGER"), ("gift", "INTEGER")]),
    "document": ("Document", [("title", "TEXT"), ("body", "TEXT"), ("lang", "TEXT"), ("score", "REAL"),
                              ("words", "INTEGER"), ("embedding", "TEXT")]),
    "category": ("Category", [("name", "TEXT"), ("depth", "INTEGER")]),
    "event": ("Event", [("seq", "INTEGER"), ("kind", "TEXT"), ("ts", "INTEGER"), ("payload", "TEXT")]),
}
LABEL_TABLE = {lab: t for t, (lab, _) in NODE_TABLES.items()}

EDGE_TABLES = {
    # TYPE: (table, src table, dst table or None, [(column, sql type)])
    "KNOWS": ("knows", "person", "person", [("since", "INTEGER"), ("weight", "REAL")]),
    "FOLLOWS": ("follows", "person", "person", []),
    "WORKS_AT": ("works_at", "person", "company", [("role", "TEXT"), ("since", "INTEGER")]),
    "REVIEWED": ("reviewed", "person", "product", [("rating", "INTEGER"), ("text", "TEXT")]),
    "SUBSIDIARY_OF": ("subsidiary_of", "company", "company", [("stake", "REAL")]),
    "IN_CATEGORY": ("in_category", "product", "category", []),
    "PARENT_OF": ("parent_of", "category", "category", []),
    "SIMILAR": ("similar", "product", "product", [("score", "REAL")]),
    "PLACED": ("placed", "person", "orders", [("channel", "TEXT")]),
    "CONTAINS": ("contains", "orders", "product", [("qty", "INTEGER"), ("price", "REAL")]),
    "AUTHORED": ("authored", "person", "document", []),
    "CITES": ("cites", "document", "document", []),
    "MENTIONS": ("mentions", "document", None, [("dst_kind", "TEXT"), ("offset", "INTEGER")]),
    "NEXT": ("next", "event", "event", [("gap", "INTEGER")]),
    "RETRY": ("retry", "event", "event", [("attempt", "INTEGER")]),
    "TRIGGERED_BY": ("triggered_by", "event", "person", []),
}

PROP_INDEXES = [("person", "email"), ("person", "age"), ("person", "country"), ("orders", "ref"),
                ("product", "sku"), ("event", "seq")]


def ddl():
    out = []
    for t, (_, cols) in NODE_TABLES.items():
        c = ", ".join(f'"{n}" {ty}' for n, ty in cols)
        out.append(f"CREATE TABLE {t} (id INTEGER PRIMARY KEY, labels TEXT NOT NULL, {c})")
    for _, (t, src, dst, cols) in EDGE_TABLES.items():
        dref = f" REFERENCES {dst}(id)" if dst else ""
        c = "".join(f', "{n}" {ty}' for n, ty in cols)
        out.append(f"CREATE TABLE {t} (id INTEGER PRIMARY KEY, src INTEGER NOT NULL REFERENCES {src}(id), "
                   f"dst INTEGER NOT NULL{dref}{c})")
    return out


def index_ddl():
    out = [f"CREATE INDEX {t}_{c} ON {t}({c})" for t, c in PROP_INDEXES]
    for _, (t, _, _, _) in EDGE_TABLES.items():
        out.append(f"CREATE INDEX {t}_src ON {t}(src, dst)")
        out.append(f"CREATE INDEX {t}_dst ON {t}(dst, src)")
    return out


def sqlval(v):
    if isinstance(v, bool):
        return int(v)
    if isinstance(v, (list, dict)):
        return json.dumps(v, ensure_ascii=False)
    return v


def load_sqlite(src_stream, path):
    """Stream glider's JSONL export into a fresh SQLite file."""
    db = sqlite3.connect(path, isolation_level=None)
    db.executescript("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA foreign_keys=OFF;"
                     f"PRAGMA cache_size=-{CACHE_KIB}; PRAGMA temp_store=FILE;")
    for s in ddl():
        db.execute(s)
    ins, batch = {}, {}
    for t, (_, cols) in NODE_TABLES.items():
        names = ", ".join(['id', 'labels'] + [f'"{n}"' for n, _ in cols])
        ins[t] = f"INSERT INTO {t} ({names}) VALUES ({', '.join('?' * (len(cols) + 2))})"
    for _, (t, _, _, cols) in EDGE_TABLES.items():
        names = ", ".join(['id', 'src', 'dst'] + [f'"{n}"' for n, _ in cols])
        ins[t] = f"INSERT INTO {t} ({names}) VALUES ({', '.join('?' * (len(cols) + 3))})"
    kind = {}  # node id -> table, only for MENTIONS' dst_kind (documents mention few kinds)
    nodes = edges = 0

    def flush(t):
        db.executemany(ins[t], batch.pop(t))

    db.execute("BEGIN")
    for line in src_stream:
        d = json.loads(line)
        if d["type"] == "node":
            t = LABEL_TABLE[d["labels"][0]]
            p = d["props"]
            row = [d["id"], ",".join(d["labels"])] + [sqlval(p.get(n)) for n, _ in NODE_TABLES[t][1]]
            if t in ("person", "company", "product"):
                kind[d["id"]] = t
            nodes += 1
        else:
            t, _, _, cols = EDGE_TABLES[d["label"]]
            p = d["props"]
            if d["label"] == "MENTIONS":
                p = dict(p, dst_kind=kind.get(d["to"]))
            row = [d["id"], d["from"], d["to"]] + [sqlval(p.get(n)) for n, _ in cols]
            edges += 1
        b = batch.setdefault(t, [])
        b.append(row)
        if len(b) >= 20000:
            flush(t)
        if (nodes + edges) % 5_000_000 == 0:
            log(f"    loaded {nodes:,} nodes, {edges:,} edges")
    for t in list(batch):
        flush(t)
    db.execute("COMMIT")
    kind.clear()
    t_idx = time.time()
    for s in index_ddl():
        db.execute(s)
    idx_s = time.time() - t_idx
    t_fk = time.time()
    bad = db.execute("PRAGMA foreign_key_check").fetchmany(5)
    fk_s = time.time() - t_fk
    db.execute("ANALYZE")
    db.close()
    return {"nodes": nodes, "edges": edges, "index_s": round(idx_s, 2), "fk_check_s": round(fk_s, 2),
            "fk_violations": len(bad)}


# ------------------------------------------------------------------ queries
# (name, shape, cypher, sql, params[, compare]). The SQL is what you would write by hand
# against the schema above; answers must match glider's.

def queries(lay):
    person_mid = lay.member("Person", 0.5)
    hub = lay.member("Person", 0.0)
    buyer = lay.member("Person", 0.0003)
    order = lay.member("Order", 0.5)
    product = lay.member("Product", 0.5)
    event = lay.member("Event", 0.5)
    doc = lay.member("Document", 0.5)
    far = lay.member("Person", 0.9)
    email = f"user{person_mid:010d}@example.org"
    hub_email = f"user{hub:010d}@example.org"
    buyer_email = f"user{buyer:010d}@example.org"
    ref = f"ORD-{order:010d}"
    sku = f"SKU-{product:010d}"
    return [
        ("point: indexed text (Person.email)", "point lookup",
         f'MATCH (p:Person {{email:"{email}"}}) RETURN p.name, p.age, p.country',
         "SELECT name, age, country FROM person WHERE email = ?", (email,)),
        ("point: indexed text (Order.ref)", "point lookup",
         f'MATCH (o:Order {{ref:"{ref}"}}) RETURN o.total, o.status',
         "SELECT total, status FROM orders WHERE ref = ?", (ref,)),
        ("point: by id", "point lookup",
         f"MATCH (d:Document) WHERE id(d) = {doc} RETURN d.title",
         "SELECT title FROM document WHERE id = ?", (doc,)),
        ("1-hop out: community (KNOWS)", "1 hop",
         f'MATCH (p:Person {{email:"{email}"}})-[:KNOWS]->(f) RETURN count(f), avg(f.age)',
         "SELECT count(*), avg(f.age) FROM person p JOIN knows k ON k.src = p.id "
         "JOIN person f ON f.id = k.dst WHERE p.email = ?", (email,)),
        ("1-hop in: power-law hub (KNOWS)", "1 hop",
         f'MATCH (h:Person {{email:"{hub_email}"}})<-[:KNOWS]-(f) RETURN count(f)',
         "SELECT count(*) FROM person h JOIN knows k ON k.dst = h.id WHERE h.email = ?", (hub_email,)),
        ("1-hop in: hub followers' average age", "1 hop",
         f'MATCH (h:Person {{email:"{hub_email}"}})<-[:KNOWS]-(f) RETURN avg(f.age)',
         "SELECT avg(f.age) FROM person h JOIN knows k ON k.dst = h.id "
         "JOIN person f ON f.id = k.src WHERE h.email = ?", (hub_email,)),
        ("2-hop: friends of friends", "2 hops",
         f'MATCH (p:Person {{email:"{email}"}})-[:KNOWS]->()-[:KNOWS]->(x) RETURN count(x)',
         "SELECT count(*) FROM person p JOIN knows a ON a.src = p.id JOIN knows b ON b.src = a.dst "
         "WHERE p.email = ? AND b.id <> a.id", (email,)),
        ("2-hop: distinct friends of friends", "2 hops",
         f'MATCH (p:Person {{email:"{email}"}})-[:KNOWS]->()-[:KNOWS]->(x) RETURN DISTINCT id(x)',
         "SELECT DISTINCT b.dst FROM person p JOIN knows a ON a.src = p.id "
         "JOIN knows b ON b.src = a.dst WHERE p.email = ? AND b.id <> a.id", (email,)),
        ("3-hop: KNOWS out to depth 3", "3 hops",
         f'MATCH (p:Person {{email:"{email}"}})-[:KNOWS]->()-[:KNOWS]->()-[:KNOWS]->(x) RETURN count(x)',
         "SELECT count(*) FROM person p JOIN knows a ON a.src = p.id JOIN knows b ON b.src = a.dst "
         "JOIN knows c ON c.src = b.dst WHERE p.email = ? AND b.id <> a.id AND c.id <> a.id AND c.id <> b.id",
         (email,)),
        ("2-hop bipartite: buyer -> orders -> products", "2 hops",
         f'MATCH (p:Person {{email:"{buyer_email}"}})-[:PLACED]->(o)-[:CONTAINS]->(pr) RETURN count(pr)',
         "SELECT count(*) FROM person p JOIN placed pl ON pl.src = p.id JOIN contains c ON c.src = pl.dst "
         "WHERE p.email = ?", (buyer_email,)),
        ("recommendation: co-purchased products", "3 hops",
         f'MATCH (p:Product {{sku:"{sku}"}})<-[:CONTAINS]-(o)-[:CONTAINS]->(other) WHERE id(other) <> id(p) '
         "RETURN DISTINCT id(other)",
         "SELECT DISTINCT c2.dst FROM product p JOIN contains c1 ON c1.dst = p.id "
         "JOIN contains c2 ON c2.src = c1.src WHERE p.sku = ? AND c2.dst <> p.id", (sku,)),
        ("1-hop fetch: friends' names, sorted page", "1 hop",
         f'MATCH (p:Person {{email:"{email}"}})-[:KNOWS]->(f) RETURN f.name, f.city ORDER BY f.name, f.city LIMIT 20',
         "SELECT f.name, f.city FROM person p JOIN knows k ON k.src = p.id JOIN person f ON f.id = k.dst "
         "WHERE p.email = ? ORDER BY f.name, f.city LIMIT 20", (email,)),
        ("BFS: everyone within 2 KNOWS hops, with depth", "traversal",
         f'CALL bfs(from: {person_mid}, depth: 2, dir: "out", type: "KNOWS")',
         "WITH RECURSIVE r(id, d) AS (SELECT ?, 0 UNION SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id"
         " WHERE r.d < 2) SELECT id, min(d) FROM r GROUP BY id", (person_mid,), "bfs"),
        ("BFS: everyone within 3 KNOWS hops, with depth", "traversal",
         f'CALL bfs(from: {person_mid}, depth: 3, dir: "out", type: "KNOWS")',
         "WITH RECURSIVE r(id, d) AS (SELECT ?, 0 UNION SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id"
         " WHERE r.d < 3) SELECT id, min(d) FROM r GROUP BY id", (person_mid,), "bfs"),
        ("shortest path: person to person (KNOWS, <= 12 hops)", "traversal",
         f'CALL shortestpath(from: {person_mid}, to: {far}, dir: "out", type: "KNOWS")',
         "WITH RECURSIVE r(id, d) AS (SELECT ?, 0 UNION SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id"
         " WHERE r.d < 12 AND r.id <> ?) SELECT min(d) FROM r WHERE id = ?", (person_mid, far, far), "hops"),
        ("multi-edges: order lines", "1 hop",
         f'MATCH (o:Order {{ref:"{ref}"}})-[c:CONTAINS]->(pr) RETURN pr.sku, count(c) ORDER BY pr.sku',
         "SELECT pr.sku, count(*) FROM orders o JOIN contains c ON c.src = o.id JOIN product pr ON pr.id = c.dst "
         "WHERE o.ref = ? GROUP BY pr.sku ORDER BY pr.sku", (ref,)),
        ("dense clique: SIMILAR 2-hop", "2 hops",
         f'MATCH (p:Product {{sku:"{sku}"}})-[:SIMILAR]->()-[:SIMILAR]->(r) RETURN count(r)',
         "SELECT count(*) FROM product p JOIN similar a ON a.src = p.id JOIN similar b ON b.src = a.dst "
         "WHERE p.sku = ? AND b.id <> a.id", (sku,)),
        ("tree: category ancestors (var-length)", "variable length",
         f'MATCH (p:Product {{sku:"{sku}"}})-[:IN_CATEGORY]->(c)<-[:PARENT_OF*1..10]-(a) RETURN count(a)',
         "WITH RECURSIVE anc(id, depth) AS ("
         " SELECT po.src, 1 FROM product p JOIN in_category ic ON ic.src = p.id"
         " JOIN parent_of po ON po.dst = ic.dst WHERE p.sku = ?"
         " UNION ALL SELECT po.src, anc.depth + 1 FROM anc JOIN parent_of po ON po.dst = anc.id"
         " WHERE anc.depth < 10) SELECT count(*) FROM anc", (sku,)),
        ("chain: NEXT*1..200", "variable length",
         f"MATCH (e:Event {{seq:{event}}})-[:NEXT*1..200]->(x) RETURN count(x)",
         "WITH RECURSIVE walk(id, depth) AS ("
         " SELECT n.dst, 1 FROM event e JOIN next n ON n.src = e.id WHERE e.seq = ?"
         " UNION ALL SELECT n.dst, walk.depth + 1 FROM walk JOIN next n ON n.src = walk.id"
         " WHERE walk.depth < 200) SELECT count(*) FROM walk", (event,)),
        ("DAG: CITES*1..3 from a document", "variable length",
         f"MATCH (d:Document)-[:CITES*1..3]->(x) WHERE id(d) = {doc} RETURN count(x)",
         "WITH RECURSIVE walk(id, depth) AS ("
         " SELECT dst, 1 FROM cites WHERE src = ?"
         " UNION ALL SELECT c.dst, walk.depth + 1 FROM walk JOIN cites c ON c.src = walk.id"
         " WHERE walk.depth < 3) SELECT count(*) FROM walk", (doc,)),
        ("heterogeneous: MENTIONS by label", "1 hop",
         f"MATCH (d:Document)-[:MENTIONS]->(x) WHERE id(d) = {doc} RETURN labels(x), count(x)",
         "SELECT coalesce(p.labels, c.labels, pr.labels) AS l, count(*) FROM mentions m"
         " LEFT JOIN person p ON m.dst_kind = 'person' AND p.id = m.dst"
         " LEFT JOIN company c ON m.dst_kind = 'company' AND c.id = m.dst"
         " LEFT JOIN product pr ON m.dst_kind = 'product' AND pr.id = m.dst"
         " WHERE m.src = ? GROUP BY l", (doc,)),
        ("index, low cardinality (country)", "index scan",
         'MATCH (p:Person {country:"JP"}) RETURN count(p)',
         "SELECT count(*) FROM person WHERE country = 'JP'", ()),
        ("index + property filter", "index scan",
         "MATCH (p:Person {age:42}) WHERE p.active = true RETURN count(p)",
         "SELECT count(*) FROM person WHERE age = 42 AND active = 1", ()),
        ("self-loops: RETRY", "edge scan",
         "MATCH (e:Event)-[:RETRY]->(e) RETURN count(e)",
         "SELECT count(*) FROM retry WHERE src = dst", ()),
        ("label scan + unindexed filter", "label scan",
         "MATCH (o:Order) WHERE o.total > 1990 RETURN count(o)",
         "SELECT count(*) FROM orders WHERE total > 1990", ()),
        ("aggregate: group by", "label scan",
         "MATCH (o:Order) RETURN o.status, count(o), avg(o.total) ORDER BY o.status",
         "SELECT status, count(*), avg(total) FROM orders GROUP BY status ORDER BY status", ()),
        ("top-k: ORDER BY LIMIT over big rows", "label scan",
         "MATCH (d:Document) RETURN d.title, d.score ORDER BY d.score DESC LIMIT 5",
         "SELECT title, score FROM document ORDER BY score DESC LIMIT 5", ()),
        ("edge scan + edge-property filter", "edge scan",
         "MATCH ()-[r:REVIEWED]->() WHERE r.rating = 5 RETURN count(r)",
         "SELECT count(*) FROM reviewed WHERE rating = 5", ()),
        ("degree: top 5 KNOWS in-degree", "global",
         "MATCH (p:Person)<-[:KNOWS]-() RETURN id(p), count(*) AS d ORDER BY d DESC, id(p) LIMIT 5",
         "SELECT dst, count(*) AS d FROM knows GROUP BY dst ORDER BY d DESC, dst LIMIT 5", ()),
        ("count all nodes", "global",
         "MATCH (n) RETURN count(n)",
         "SELECT " + " + ".join(f"(SELECT count(*) FROM {t})" for t in NODE_TABLES), ()),
    ]


# ------------------------------------------------------------------ answers

def norm_val(v):
    if isinstance(v, bool):
        return int(v)
    if isinstance(v, float):
        if v == int(v) and abs(v) < 1e15:
            return int(v)
        return float(f"{v:.9g}")
    if isinstance(v, list):
        return ",".join(str(x) for x in v)
    return v


def norm_rows(rows):
    return [[norm_val(v) for v in r] for r in rows]


def same_answer(cmp, cypher, sql, g, s):
    """Do glider's rows g and SQLite's rows s give the same answer?"""
    if cmp == "bfs":  # glider: id, node, depth, parent -> (id, depth) sets
        return sorted([r[0], r[2]] for r in g) == sorted(norm_rows(s))
    if cmp == "hops":  # glider: one row per step, start included
        return (len(g) - 1 if g else None) == (s[0][0] if s else None)
    g, s = norm_rows(g), norm_rows(s)
    if "ORDER BY" not in sql:
        g, s = sorted(g, key=repr), sorted(s, key=repr)
    return g == s


# ------------------------------------------------------------------ phases

def phase_build(size):
    g = gpath(size)
    if not os.path.exists(g) or ARGS.rebuild:
        for p in tree_files(g) + [g + ".lock"]:
            if os.path.isfile(p):
                os.remove(p)
        t = time.time()
        r = scale.run([GEN, "--size", size, "--paged", "--work-mb", "256", "--cache-mb", "256", "--out", g],
                      timeout=ARGS.timeout * 8)
        rec = {"phase": "build", "engine": "glider", "size": size, "wall_s": round(time.time() - t, 2),
               "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0, "file_bytes": tree_bytes(g)}
        gen = [ln for ln in r["out"].splitlines() if ln.startswith("{")]
        if gen:
            rec["gen"] = json.loads(gen[-1])
        else:
            rec["error"] = r["out"][-300:]
        emit(rec)
        if r["exit"] != 0:
            return
    s = spath(size)
    if not os.path.exists(s) or ARGS.rebuild:
        for p in (s, s + "-journal", s + "-wal"):
            if os.path.exists(p):
                os.remove(p)
        os.environ["SQLITE_TMPDIR"] = DATA  # never /tmp: it is RAM here
        t = time.time()
        exp = subprocess.Popen([GLIDER, g, "export"], stdout=subprocess.PIPE, preexec_fn=scale.cap, bufsize=1 << 20)
        before = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        info = load_sqlite(exp.stdout, s)
        exp.wait()
        if exp.returncode != 0 or not info["nodes"]:
            os.remove(s)
            raise SystemExit(f"glider export failed at {size} (exit {exp.returncode}); SQLite file removed")
        emit({"phase": "build", "engine": "sqlite", "size": size, "wall_s": round(time.time() - t, 2),
              "ok": exp.returncode == 0, "file_bytes": fsize(s), "loader_peak_rss_mb":
              round(max(before, resource.getrusage(resource.RUSAGE_SELF).ru_maxrss) / 1024, 1), **info})


def layout(size):
    for line in open(ARGS.results):
        d = json.loads(line)
        if d.get("phase") == "build" and d.get("engine") == "glider" and d.get("size") == size and "gen" in d:
            ids = d["gen"]["ids"]
    return Layout(ids)


def phase_query(size):
    g, s = gpath(size), spath(size)
    qs = queries(layout(size))
    if ARGS.q:
        qs = [q for q in qs if any(w in q[0] for w in ARGS.q.split(","))]

    # glider: a fresh shell on an evicted file; first run is cold.
    evict_tree(g)
    gl, g_rss = glider_session(g, qs)

    # SQLite: a fresh worker process on an evicted file, so its RSS is its own.
    evict_tree(s)
    p = subprocess.run([sys.executable, __file__, "--sqlite-worker", s, "--warm", str(ARGS.warm),
                        "--slow", str(ARGS.slow), "--timeout", str(ARGS.timeout), "--q", ARGS.q or "",
                        "--ids", str(layout(size).n)],
                       capture_output=True, text=True, preexec_fn=scale.cap)
    try:
        sq = json.loads(p.stdout.strip().splitlines()[-1])
    except Exception:
        emit({"phase": "query", "engine": "sqlite", "size": size, "error": (p.stderr or p.stdout)[-500:]})
        return

    for name, shape, cy, sql, params, *cmp in qs:
        gt, gr, ge = gl[name]
        st, sr, se = sq["q"][name]
        match = None
        if gr is not None and sr is not None and not ge and not se:
            match = same_answer(cmp[0] if cmp else "rows", cy, sql, gr, sr)
        emit({"phase": "query", "size": size, "name": name, "shape": shape, "cypher": cy, "sql": sql,
              "params": list(params),
              "glider": {"cold_ms": ms(gt[0]) if gt else None, "warm_ms": ms(med(gt[1:])), "runs": len(gt),
                         "rows": (gr or [])[:10], "row_count": len(gr or []), "error": ge},
              "sqlite": {"cold_ms": ms(st[0]) if st else None, "warm_ms": ms(med(st[1:])), "runs": len(st),
                         "rows": (sr or [])[:10], "row_count": len(sr or []), "error": se,
                         "plan": sq["plan"].get(name)},
              "match": match})
    if not ARGS.q:  # a partial rerun would understate the peak
        emit({"phase": "memory", "size": size, "glider_peak_rss_mb": g_rss, "sqlite_peak_rss_mb": sq["peak_rss_mb"]})


class Shell:
    """`glider <db> --json` with .timer on: one statement in, one JSON line
    (plus an elapsed line on success) out."""

    def __init__(self, db):
        self.p = subprocess.Popen([GLIDER, db, "--json", "--cache-size", "1G"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
                                  preexec_fn=scale.cap)
        self.fd = self.p.stdout.fileno()
        self.buf = b""
        self.send(".timer")
        self.readline(60)

    def send(self, line):
        self.p.stdin.write(line + "\n")
        self.p.stdin.flush()

    def readline(self, timeout):
        # Raw reads: select() cannot see what a buffered reader already holds.
        deadline = time.time() + timeout
        while b"\n" not in self.buf:
            r, _, _ = select.select([self.fd], [], [], max(0.0, deadline - time.time()))
            if not r:
                raise TimeoutError
            chunk = os.read(self.fd, 1 << 20)
            if not chunk:
                raise EOFError
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode()

    def query(self, q, timeout):
        """-> (engine seconds, rows, error)"""
        self.send(q + ";")
        d = json.loads(self.readline(timeout))
        if "error" in d:
            return None, None, str(d["error"])[:300]
        e = json.loads(self.readline(timeout))
        return e["elapsed_ms"] / 1000, d.get("rows", []), None

    def hwm_mb(self):
        try:
            with open(f"/proc/{self.p.pid}/status") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        return round(int(line.split()[1]) / 1024, 1)
        except OSError:
            return None

    def close(self):
        if self.p.poll() is None:
            try:
                self.p.stdin.close()
                self.p.wait(30)
            except (OSError, subprocess.TimeoutExpired):
                self.p.kill()
                self.p.wait()


def glider_session(db, qs):
    out, hwm = {}, 0
    sh = Shell(db)
    for name, _, cy, *_ in qs:
        times, rows, err = [], None, None
        for i in range(1 + ARGS.warm):
            try:
                dt, rows, err = sh.query(cy, ARGS.timeout)
            except (TimeoutError, EOFError) as e:
                err = "timeout" if isinstance(e, TimeoutError) else "glider exited"
                hwm = max(hwm, sh.hwm_mb() or 0)
                sh.p.kill()
                sh.p.wait()
                sh = Shell(db)
                break
            if err:
                break
            times.append(dt)
            if i == 0 and dt > ARGS.slow:
                break
        out[name] = (times, rows, err)
    hwm = max(hwm, sh.hwm_mb() or 0)
    sh.close()
    return out, hwm


def med(xs):
    return statistics.median(xs) if xs else None


def ms(x):
    return None if x is None else round(x * 1000, 3)


def sqlite_worker(a):
    db = sqlite3.connect(f"file:{a.sqlite_worker}?mode=ro", uri=True, isolation_level=None)
    db.execute(f"PRAGMA cache_size=-{CACHE_KIB}")
    db.execute("PRAGMA foreign_keys=ON")
    qs = queries(Layout(a.ids))
    if a.q:
        qs = [q for q in qs if any(w in q[0] for w in a.q.split(","))]
    out, plans = {}, {}
    for name, _, _, sql, params, *_ in qs:
        plans[name] = [r[3] for r in db.execute("EXPLAIN QUERY PLAN " + sql, params)]
        times, rows, err = [], None, None
        for i in range(1 + a.warm):
            deadline = time.time() + a.timeout
            db.set_progress_handler(lambda: 1 if time.time() > deadline else 0, 100000)
            t = time.time()
            try:
                rows = [list(r) for r in db.execute(sql, params).fetchall()]
            except sqlite3.Error as e:
                err = f"{type(e).__name__}: {e}" + (" (timeout)" if time.time() > deadline else "")
                break
            dt = time.time() - t
            times.append(dt)
            if i == 0 and dt > a.slow:
                break
        out[name] = (times, rows, err)
    print(json.dumps({"q": out, "plan": plans,
                      "peak_rss_mb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)}))


def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default=",".join(SIZES))
    ap.add_argument("--only", default="build,query")
    ap.add_argument("--q", default="", help="only queries whose name contains one of these (comma-separated)")
    ap.add_argument("--rebuild", action="store_true")
    ap.add_argument("--warm", type=int, default=5)
    ap.add_argument("--slow", type=float, default=20.0, help="skip warm runs when the cold run takes longer (s)")
    ap.add_argument("--timeout", type=float, default=600)
    ap.add_argument("--mem-cap", type=float, default=12.0, help="address-space cap per process, GiB")
    ap.add_argument("--results", default=os.path.join(DATA, "results.jsonl"))
    ap.add_argument("--sqlite-worker")
    ap.add_argument("--ids", type=int)
    a = ap.parse_args()
    if a.sqlite_worker:
        return sqlite_worker(a)
    ARGS = scale.ARGS = a
    os.makedirs(DATA, exist_ok=True)
    for size in a.sizes.split(","):
        for ph in a.only.split(","):
            log(f"== {ph} {size}")
            {"build": phase_build, "query": phase_query}[ph](size)


if __name__ == "__main__":
    main()
