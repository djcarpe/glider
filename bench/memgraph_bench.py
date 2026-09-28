#!/usr/bin/env python3
"""Memgraph as a third engine in the glider vs SQLite suite.

    uv run --with neo4j python3 bench/memgraph_bench.py [--sizes 500MiB,1GiB] [--only load,reads,algos,writes]

Memgraph keeps the whole graph in memory, so the sizes it can take are
bounded by RAM (a 31 GiB machine here). A size that does not fit is
recorded as such rather than left out.

Data: the same scale-gen graph, regenerated fresh for each size (the suite's
write phase has changed its own copies), exported and split into CSV files,
one per exact label set and per relationship type, and bulk-loaded with
LOAD CSV in IN_MEMORY_ANALYTICAL mode; then indexes, and back to
IN_MEMORY_TRANSACTIONAL for the benchmarks. Every node carries its glider id
as `gid` (indexed), so queries that pin a node by id pin the same node.

Queries are the suite's Cypher with glider-only syntax translated: id(x) ->
x.gid, CALL bfs / shortestpath -> Memgraph's *BFS expansions, and the
algorithms through MAGE on a projection of the KNOWS subgraph. Timing is
Memgraph's own (parsing + planning + execution, from the query summary),
so it is inside the engine like the other two.

Results are appended to the suite's results.jsonl as phase "mg-read",
"mg-algo", "mg-write" and "mg-load"; the report merges them in.
"""

import argparse
import csv
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import otel  # noqa: E402
import scale  # noqa: E402
import sqlite_compare as sc  # noqa: E402
import suite  # noqa: E402
from scale import log  # noqa: E402

from neo4j import GraphDatabase  # noqa: E402  (uv run --with neo4j)

DATA = os.path.join(suite.DATA, "memgraph")
IMAGE = "memgraph/memgraph-mage:latest"
CONTAINER = "glider-memgraph"
BOLT = "bolt://127.0.0.1:7687"

ARGS = None


def emit(rec):
    suite.emit(rec)


# ------------------------------------------------------------------ server

def start(mem_gb):
    subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)
    os.makedirs(DATA, exist_ok=True)
    cmd = ["docker", "run", "-d", "--name", CONTAINER, "--memory", f"{mem_gb}g", "--memory-swap", f"{mem_gb}g",
           "-p", "127.0.0.1:7687:7687", "-v", f"{DATA}:/import:ro", IMAGE,
           f"--memory-limit={int((mem_gb - 1) * 1024)}", "--storage-properties-on-edges=true",
           "--storage-snapshot-on-exit=false", "--storage-snapshot-interval-sec=86400",
           "--query-execution-timeout-sec=0", "--log-level=WARNING", "--telemetry-enabled=false"]
    subprocess.run(cmd, check=True, capture_output=True)
    for _ in range(120):
        try:
            d = GraphDatabase.driver(BOLT, auth=None)
            with d.session() as s:
                s.run("RETURN 1").consume()
            return d
        except Exception:  # noqa: BLE001
            time.sleep(1)
    raise SystemExit("memgraph did not start")


def stop():
    subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)


def server_s(summary):
    m = summary.metadata or {}
    t = sum(float(m.get(k, 0) or 0) for k in ("parsing_time", "planning_time", "plan_execution_time"))
    return t if t > 0 else None


def run(sess, q, params=None):
    """-> (seconds inside Memgraph, rows, error)"""
    try:
        res = sess.run(q, params or {})
        rows = [list(r.values()) for r in res]
        return server_s(res.consume()), rows, None
    except Exception as e:  # noqa: BLE001
        msg = str(e)
        if "memory" in msg.lower():
            return None, None, "out of memory: " + msg[:200]
        return None, None, msg[:300]


# ------------------------------------------------------------------ data

def csv_dir(size):
    return os.path.join(DATA, size)


def prepare(size):
    """Fresh graph -> JSONL -> CSV files per label set and relationship type."""
    out = csv_dir(size)
    if os.path.exists(os.path.join(out, "manifest.json")):
        return json.load(open(os.path.join(out, "manifest.json")))
    os.makedirs(out, exist_ok=True)
    src = os.path.join(DATA, f"src-{size}.db")
    r = scale.run([suite.GEN, "--size", size, "--paged", "--work-mb", "256", "--cache-mb", "256", "--out", src],
                  timeout=ARGS.timeout * 12)
    if r["exit"] != 0:
        raise SystemExit(f"scale-gen failed: {r['out'][-300:]}")
    exp = subprocess.Popen([suite.GLIDER, src, "export"], stdout=subprocess.PIPE, bufsize=1 << 20,
                           preexec_fn=scale.cap)
    node_files, edge_files, labels_of = {}, {}, {}
    node_keys, edge_keys = {}, {}
    # Pass 1 writes nodes; property columns per label set are learned from
    # the first rows and extended as new keys appear (written as JSON text
    # per row, so a later key never needs a rewrite).
    for line in exp.stdout:
        d = json.loads(line)
        if d["type"] == "node":
            key = ":".join(d["labels"])
            labels_of[d["id"]] = key
            f = node_files.get(key)
            if f is None:
                fh = open(os.path.join(out, f"n-{key.replace(':', '_')}.csv"), "w", newline="")
                f = node_files[key] = (fh, csv.writer(fh))
                f[1].writerow(["gid", "props"])
            f[1].writerow([d["id"], json.dumps(d["props"], ensure_ascii=False)])
            node_keys.setdefault(key, set()).update(d["props"].keys())
        else:
            dst_label = labels_of[d["to"]].split(":")[0]
            src_label = labels_of[d["from"]].split(":")[0]
            key = (d["label"], src_label, dst_label)
            f = edge_files.get(key)
            if f is None:
                fh = open(os.path.join(out, f"e-{d['label']}-{src_label}-{dst_label}.csv"), "w", newline="")
                f = edge_files[key] = (fh, csv.writer(fh))
                f[1].writerow(["src", "dst", "props"])
            f[1].writerow([d["from"], d["to"], json.dumps(d["props"], ensure_ascii=False)])
            edge_keys.setdefault(key, set()).update(d["props"].keys())
    exp.wait()
    for fh, _ in list(node_files.values()) + list(edge_files.values()):
        fh.close()
    for p in (src, src + ".lock"):
        if os.path.exists(p):
            os.remove(p)
    for d in (src + "-wal", src + "-data", src + "-tmp"):
        shutil.rmtree(d, ignore_errors=True)
    manifest = {"nodes": {k: sorted(v) for k, v in node_keys.items()},
                "edges": [[t, s, d, sorted(v)] for (t, s, d), v in edge_keys.items()],
                "node_count": len(labels_of)}
    json.dump(manifest, open(os.path.join(out, "manifest.json"), "w"))
    return manifest


def phase_load(size):
    t_prep = time.time()
    m = prepare(size)
    prep_s = time.time() - t_prep
    drv = start(ARGS.mem_gb)
    rec = {"phase": "mg-load", "size": size, "prepare_s": round(prep_s, 1)}
    with drv.session() as s:
        s.run("STORAGE MODE IN_MEMORY_ANALYTICAL").consume()
        t0 = time.time()
        labels = sorted({k.split(":")[0] for k in m["nodes"]})
        err = None
        for key, props in m["nodes"].items():
            lab = ":".join(f"`{x}`" for x in key.split(":"))
            path = f"/import/{size}/n-{key.replace(':', '_')}.csv"
            q = (f"LOAD CSV FROM '{path}' WITH HEADER AS row "
                 f"CREATE (n:{lab}) SET n = convert.str2object(row.props), n.gid = toInteger(row.gid)")
            _, _, err = run(s, q)
            if err:
                break
        nodes_s = time.time() - t0
        t1 = time.time()
        for lab in labels:
            if not err:
                _, _, err = run(s, f"CREATE INDEX ON :`{lab}`(gid)")
        for lab, key in [("Person", "email"), ("Person", "age"), ("Person", "country"), ("Order", "ref"),
                         ("Product", "sku"), ("Event", "seq")]:
            if not err:
                _, _, err = run(s, f"CREATE INDEX ON :`{lab}`({key})")
        idx_s = time.time() - t1
        t2 = time.time()
        for t, sl, dl, props in m["edges"]:
            if err:
                break
            path = f"/import/{size}/e-{t}-{sl}-{dl}.csv"
            q = (f"LOAD CSV FROM '{path}' WITH HEADER AS row "
                 f"MATCH (a:`{sl}` {{gid: toInteger(row.src)}}), (b:`{dl}` {{gid: toInteger(row.dst)}}) "
                 f"CREATE (a)-[r:`{t}`]->(b) SET r = convert.str2object(row.props)")
            _, _, err = run(s, q)
        edges_s = time.time() - t2
        if not err:
            s.run("STORAGE MODE IN_MEMORY_TRANSACTIONAL").consume()
        info = {}
        try:
            for r in s.run("SHOW STORAGE INFO"):
                info[r["storage info"]] = r["value"]
        except Exception:  # noqa: BLE001
            pass
    rec.update({"ok": err is None, "error": err, "nodes_s": round(nodes_s, 1), "index_s": round(idx_s, 1),
                "edges_s": round(edges_s, 1), "wall_s": round(nodes_s + idx_s + edges_s, 1),
                "memory": info.get("memory_res") or info.get("memory_usage"), "storage_info": info})
    emit(rec)
    return drv if err is None else None


# ------------------------------------------------------------------ queries

def translate(q, lay):
    """glider-only syntax -> Memgraph Cypher."""
    m = re.match(r'CALL bfs\(from: (\d+), depth: (\d+), dir: "out", type: "(\w+)"\)', q)
    if m:
        s, d, t = m.groups()
        return (f"MATCH (s:Person {{gid: {s}}}) MATCH (s)-[:{t} *BFS ..{d}]->(x) "
                f"RETURN x.gid, 0"), "bfs"
    m = re.match(r'CALL shortestpath\(from: (\d+), to: (\d+), dir: "out", type: "(\w+)"\)', q)
    if m:
        s, t_, t = m.groups()
        return (f"MATCH (a:Person {{gid: {s}}}), (b:Person {{gid: {t_}}}) "
                f"MATCH p = (a)-[:{t} *BFS]->(b) RETURN size(relationships(p))"), "hops"
    q = re.sub(r"WHERE id\((\w+)\) = (\d+)", r"WHERE \1.gid = \2", q)
    q = re.sub(r"id\((\w+)\) <> id\((\w+)\)", r"\1.gid <> \2.gid", q)
    q = re.sub(r"\bid\((\w+)\)", r"\1.gid", q)
    return alias_order(q), "rows"


def alias_order(q):
    """After an aggregation Memgraph orders by returned columns only:
    RETURN o.status, count(o) ORDER BY o.status -> RETURN o.status AS o_status ... ORDER BY o_status."""
    m = re.search(r"RETURN (.*) ORDER BY (.*?)( LIMIT \d+)?$", q)
    if not m or not re.search(r"\b(count|avg|sum|min|max|collect)\(", m.group(1)):
        return q
    items = [i.strip() for i in m.group(1).split(", ")]
    order = m.group(2)
    for i, it in enumerate(items):
        if re.fullmatch(r"\w+\.\w+", it) and re.search(rf"(?<![\w.]){re.escape(it)}(?![\w])", order):
            name = it.replace(".", "_")
            items[i] = f"{it} AS {name}"
            order = re.sub(rf"(?<![\w.]){re.escape(it)}(?![\w])", name, order)
    return q[:m.start()] + "RETURN " + ", ".join(items) + " ORDER BY " + order + (m.group(3) or "")


def parameterize(cy):
    """Literals -> $parameters, so repeated statements share one plan (as
    SQLite's prepared statements do)."""
    params = {}

    def lit(m):
        k = f"p{len(params)}"
        if m.group(1) is not None:
            params[k] = m.group(1)
        else:
            v = m.group(2)
            params[k] = float(v) if "." in v else int(v)
        return "$" + k

    return re.sub(r'"([^"]*)"|(?<=[:=] )(-?\d+(?:\.\d+)?)\b', lit, cy), params


def same(kind, g, gcount, mg):
    """Memgraph's rows against glider's recorded answer (first rows + count)."""
    if mg is None or g is None:
        return None
    if kind == "bfs":
        return len(mg) + 1 == gcount  # glider's BFS also lists the start
    if kind == "hops":
        return (mg[0][0] if mg else None) == ((gcount - 1) if gcount else None)
    norm = sc.norm_rows
    if len(mg) != gcount:
        return False
    if gcount <= len(g):
        return sorted(norm(g), key=repr) == sorted(norm(mg), key=repr)
    return True


def timed(bench, label, sess, q):
    t0 = otel.now_ns()
    s, rows, err = run(sess, q)
    bench.run("memgraph", label, t0, otel.now_ns(), s, rows=len(rows) if rows is not None else None, error=err)
    return s, rows, err


def glider_record(size, phase, name):
    for line in open(ARGS.results):
        d = json.loads(line)
        if d.get("phase") == phase and d.get("size") == size and d.get("name") == name:
            return d
    return None


def phase_reads(size, drv):
    lay = suite.layout(size)
    for name, shape, cy, sql, params, *cmp in sc.queries(lay):
        if ARGS.q and not any(w in name for w in ARGS.q.split(",")):
            continue
        q, kind = translate(cy, lay)
        if cmp and cmp[0] == "bfs":
            kind = "bfs"
        b = suite.Bench(size, "read", name)
        with drv.session() as s:
            first, rows, err = timed(b, "first", s, q)
            warm = []
            if not err and (first or 0) < ARGS.slow:
                for i in range(ARGS.warm):
                    t, rows, err = timed(b, f"warm {i + 1}", s, q)
                    if err:
                        break
                    warm.append(t)
                t0 = otel.now_ns()
                end = time.time() + suite.PROFILE_WINDOW_S
                while time.time() < end:
                    run(s, q)
                b.window("memgraph", t0, otel.now_ns())
        g = glider_record(size, "read", name)
        match = same(kind, g["glider"]["rows"], g["glider"]["row_count"], rows) if g and not err else None
        tel = b.finish({"match": match})
        emit({"phase": "mg-read", "size": size, "name": name, "cypher": q, "match": match, "telemetry": tel,
              "memgraph": {"cold_ms": suite.ms(first), "warm_ms": suite.ms(statistics.median(warm) if warm else None),
                           "row_count": len(rows) if rows is not None else None, "rows": (rows or [])[:8], "error": err}})


def mg_algorithms(lay):
    person, far = lay.member("Person", 0.5), lay.member("Person", 0.9)
    return [
        ("degree centrality: top 10 KNOWS in-degree", "degree",
         "MATCH (p:Person)<-[:KNOWS]-() RETURN p.gid AS gid, count(*) AS d ORDER BY d DESC, gid LIMIT 10"),
        ("PageRank: 5 iterations over KNOWS", "pagerank",
         "MATCH p = (:Person)-[:KNOWS]->(:Person) WITH project(p) AS g "
         "CALL pagerank.get(g, 5, 0.85, 0.0) YIELD node, rank RETURN node.gid, rank ORDER BY rank DESC, node.gid LIMIT 10"),
        ("weakly connected components over KNOWS", "wcc",
         "MATCH p = (:Person)-[:KNOWS]->(:Person) WITH project(p) AS g "
         "CALL weakly_connected_components.get(g) YIELD node, component_id "
         "RETURN count(DISTINCT component_id), count(node)"),
        ("BFS: 3 hops over KNOWS", "bfs3",
         f"MATCH (s:Person {{gid: {person}}}) MATCH (s)-[:KNOWS *BFS ..3]->(x) RETURN x.gid, 0"),
        ("shortest path over KNOWS", "path",
         f"MATCH (a:Person {{gid: {person}}}), (b:Person {{gid: {far}}}) MATCH p = (a)-[:KNOWS *BFS]->(b) "
         "RETURN size(relationships(p))"),
    ]


def phase_algos(size, drv, node_count):
    lay = suite.layout(size)
    for name, kind, q in mg_algorithms(lay):
        if ARGS.q and not any(w in name for w in ARGS.q.split(",")):
            continue
        b = suite.Bench(size, "algorithm", name)
        with drv.session() as s:
            first, rows, err = timed(b, "run 1", s, q)
            warm = []
            if not err and (first or 0) < ARGS.slow:
                for i in range(ARGS.warm):
                    t, rows, err = timed(b, f"warm {i + 1}", s, q)
                    if err:
                        break
                    warm.append(t)
        g = glider_record(size, "algo", name)
        ga = g["glider"]["answer"] if g else None
        answer, match = None, None
        if rows is not None and not err:
            if kind == "degree":
                answer = [list(r) for r in rows]
                match = answer == ga
            elif kind == "pagerank":
                # MAGE ranks the KNOWS subgraph only, so scores differ from
                # glider's all-node formula; the ranking is what is compared.
                answer = [[r[0], r[1]] for r in rows]
                match = [r[0] for r in answer] == [r[0] for r in ga] if ga else None
            elif kind == "wcc":
                k, in_proj = rows[0]
                answer = f"{k + (node_count - in_proj)} connected components"
                match = answer == ga
            elif kind == "bfs3":
                answer = len(rows) + 1
                match = answer == (len(ga) if ga else None)
            elif kind == "path":
                answer = rows[0][0] if rows else None
                match = answer == ga
        tel = b.finish({"match": match})
        emit({"phase": "mg-algo", "size": size, "name": name, "kind": kind, "cypher": q, "match": match,
              "telemetry": tel, "memgraph": {"cold_ms": suite.ms(first),
                                             "warm_ms": suite.ms(statistics.median(warm) if warm else None),
                                             "answer": answer, "error": err}})


def phase_writes(size, drv):
    lay = suite.layout(size)
    for name, sync, txn, stmts in suite.writes(lay, size):
        if ARGS.q and not any(w in name for w in ARGS.q.split(",")):
            continue
        b = suite.Bench(size, "write", name)
        # Memgraph's WAL flush interval is a startup flag
        # (--storage-wal-file-flush-every-n-tx), so the fsync workload runs
        # with its default flushing: timed, but not fsync per commit.
        note = "Memgraph default WAL flushing, not fsync per commit" if sync == "always" else None
        total, err = 0.0, None
        t0 = otel.now_ns()
        with drv.session() as s:
            if txn:
                tx = s.begin_transaction()
                for cy, *_ in stmts:
                    try:
                        r = tx.run(*parameterize(cy))
                        r.consume()
                        total += server_s(r.consume()) or 0
                    except Exception as e:  # noqa: BLE001
                        err = str(e)[:300]
                        break
                c0 = time.perf_counter()
                if err:
                    tx.rollback()
                else:
                    tx.commit()
                total += time.perf_counter() - c0
            else:
                for cy, *_ in stmts:
                    c0 = time.perf_counter()
                    t, _, err = run(s, *parameterize(cy))
                    if err:
                        break
                    total += time.perf_counter() - c0  # autocommit: include the commit
            counts = {}
            for key, cy in [("person", "MATCH (p:Person) RETURN count(p)"), ("knows", "MATCH ()-[r:KNOWS]->() RETURN count(r)")]:
                _, rows, _ = run(s, cy)
                counts[key] = rows[0][0] if rows else None
        b.run("memgraph", "run", t0, otel.now_ns(), None if err else total, error=err)
        g = glider_record(size, "write", name)
        match = (counts == g["counts"]["glider"]) if g and g.get("counts") and not err else None
        n = len(stmts)
        emit({"phase": "mg-write", "size": size, "name": name, "cypher": stmts[0][0], "match": match,
              "counts": counts, "telemetry": b.finish(),
              "memgraph": {"ms": suite.ms(None if err else total), "ops_per_s": round(n / total) if total and not err else None,
                           "error": err, "note": note}})


def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default="500MiB,1GiB,5GiB,10GiB,25GiB")
    ap.add_argument("--only", default="load,reads,algos,writes")
    ap.add_argument("--q", default="")
    ap.add_argument("--warm", type=int, default=5)
    ap.add_argument("--slow", type=float, default=20.0)
    ap.add_argument("--timeout", type=float, default=900)
    ap.add_argument("--mem-gb", type=int, default=24)
    ap.add_argument("--mem-cap", type=float, default=12.0)
    ap.add_argument("--results", default=os.path.join(suite.DATA, "results.jsonl"))
    a = ap.parse_args()
    ARGS = suite.ARGS = scale.ARGS = sc.ARGS = a
    os.makedirs(DATA, exist_ok=True)
    try:
        for size in a.sizes.split(","):
            log(f"== memgraph {size}")
            drv = phase_load(size) if "load" in a.only else GraphDatabase.driver(BOLT, auth=None)
            if drv is None:
                log(f"   memgraph could not load {size}; skipping its benchmarks")
                stop()
                continue
            node_count = json.load(open(os.path.join(csv_dir(size), "manifest.json")))["node_count"]
            if "reads" in a.only:
                phase_reads(size, drv)
            if "algos" in a.only:
                phase_algos(size, drv, node_count)
            if "writes" in a.only:
                phase_writes(size, drv)
            drv.close()
            if "load" in a.only:
                stop()
            if not a.q:
                shutil.rmtree(csv_dir(size), ignore_errors=True)
    finally:
        otel.flush()


if __name__ == "__main__":
    main()
