#!/usr/bin/env python3
"""glider against Neo4j on Neo4j's own example graphs.

    uv run --with neo4j python3 bench/neo4j_bench.py [--dataset recommendations] [--warm 5] [--writes 500]

The dataset is loaded into a Neo4j 5 container from the dump the graph-examples
repository ships, then pulled out over Bolt with bench/convert.py into a glider
file, so both engines hold the same graph. Every query is the same Cypher on
both sides where glider's subset allows it; where it does not, the closest
equivalent is used and the report says so. Every answer is checked across
engines.

Timings: Neo4j's own server-side figure (result available + consumed, as the
driver reports it) and glider's shell timer; both exclude the client. Cold is
the first run, warm the median of --warm runs after it.

Results go to bench/results/neo4j-<dataset>.jsonl; bench/neo4j_report.py
renders them.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import scale  # noqa: E402  (the memory cap the shell runs under)
import suite  # noqa: E402  (the glider shell runner)

# The release binary, the same one the conversion imported with.
suite.GLIDER = os.path.join(HERE, "..", "target", "release", "glider")

from neo4j import GraphDatabase  # noqa: E402

DATA = os.path.abspath(os.path.join(HERE, "..", "..", "glider-bench-data", "neo4j"))
CONTAINER = "glider-neo4j"
BOLT = "bolt://127.0.0.1:7687"
AUTH = ("neo4j", "gliderbench")
DUMPS = {
    "recommendations": "https://raw.githubusercontent.com/neo4j-graph-examples/recommendations/main/data/recommendations-50.dump",
    "stackoverflow": "https://raw.githubusercontent.com/neo4j-graph-examples/stackoverflow/main/data/stackoverflow-50.dump",
}
ARGS = None


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# ------------------------------------------------------------------ neo4j

def neo4j_up(dataset):
    """A Neo4j 5 container holding the dataset, with Graph Data Science."""
    dump = os.path.join(DATA, f"{dataset}-50.dump")
    if not os.path.exists(dump):
        os.makedirs(DATA, exist_ok=True)
        subprocess.run(["curl", "-sSL", "-o", dump, DUMPS[dataset]], check=True)
    data = os.path.join(DATA, "data-" + dataset)
    dumps = os.path.join(DATA, "dumps-" + dataset)
    os.makedirs(dumps, exist_ok=True)
    subprocess.run(["cp", dump, os.path.join(dumps, "neo4j.dump")], check=True)
    subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)
    if not os.path.exists(os.path.join(data, "databases")):
        log(f"loading {dataset} into neo4j ...")
        subprocess.run(["docker", "run", "--rm", "-v", f"{data}:/data", "-v", f"{dumps}:/dumps:ro", "neo4j:5",
                        "neo4j-admin", "database", "load", "neo4j", "--from-path=/dumps",
                        "--overwrite-destination=true"], check=True, capture_output=True)
    subprocess.run(["docker", "run", "-d", "--name", CONTAINER, "--memory", f"{ARGS.mem_gb}g", "--memory-swap",
                    f"{ARGS.mem_gb}g", "-p", "127.0.0.1:7687:7687", "-v", f"{data}:/data",
                    "-e", f"NEO4J_AUTH={AUTH[0]}/{AUTH[1]}", "-e", 'NEO4J_PLUGINS=["graph-data-science"]',
                    "-e", "NEO4J_dbms_security_procedures_unrestricted=gds.*",
                    "-e", f"NEO4J_server_memory_heap_max__size={max(ARGS.mem_gb - 3, 1)}G",
                    "-e", "NEO4J_server_memory_pagecache_size=1G", "neo4j:5"], check=True, capture_output=True)
    for _ in range(120):
        try:
            drv = GraphDatabase.driver(BOLT, auth=AUTH)
            with drv.session() as s:
                s.run("RETURN 1").consume()
            return drv
        except Exception:  # noqa: BLE001
            time.sleep(1)
    raise SystemExit("neo4j did not start")


def neo4j_run(sess, q, params=None):
    """-> (server seconds, rows, error)"""
    try:
        res = sess.run(q, params or {})
        rows = [list(r.values()) for r in res]
        summary = res.consume()
        avail = summary.result_available_after or 0
        consumed = summary.result_consumed_after or 0
        return (avail + consumed) / 1000, rows, None
    except Exception as e:  # noqa: BLE001
        return None, None, str(e)[:300]


# ------------------------------------------------------------------ glider

def glider_db(dataset):
    """The same graph as a glider file, converted over Bolt if it is not there yet."""
    db = os.path.join(DATA, f"{dataset}.gldb")
    if not os.path.exists(db):
        log(f"converting {dataset} into glider ...")
        jsonl = os.path.join(DATA, f"{dataset}.jsonl")
        subprocess.run([sys.executable, os.path.join(HERE, "convert.py"), "bolt", "--uri", BOLT, "--user", AUTH[0],
                        "--password", AUTH[1], "--drop", "Embedding$", "--out", jsonl, "--db", db], check=True)
    return db


def glider_run(shell, q):
    """-> (engine seconds, rows, error)"""
    return shell.run(q, ARGS.timeout)


# ------------------------------------------------------------------ what to run

def lit(v):
    return json.dumps(v, ensure_ascii=False)


def recommendations_queries(sess):
    """(name, neo4j cypher, glider cypher, how to compare, note)."""
    def one(q):
        return [r[0] for r in neo4j_run(sess, q)[1]]

    titles = one("MATCH (m:Movie)<-[r:RATED]-() WITH m, count(r) AS n WHERE n > 50 RETURN m.title ORDER BY m.title LIMIT 12")
    actors = one("MATCH (p:Person)-[:ACTED_IN]->(m) WITH p, count(m) AS n WHERE n >= 8 RETURN p.name ORDER BY p.name LIMIT 12")
    directors = one("MATCH (p:Person)-[:DIRECTED]->(m) WITH p, count(m) AS n WHERE n >= 6 RETURN p.name ORDER BY p.name LIMIT 6")
    qs = []
    for t in titles[:6]:
        qs.append((f"lookup: {t[:28]}",
                   f"MATCH (m:Movie {{title: {lit(t)}}}) RETURN m.year, m.imdbRating", None, "rows",
                   "indexed point lookup on Movie(title)"))
    for t in titles[:6]:
        qs.append((f"recommend: {t[:28]}",
                   f"MATCH (m:Movie {{title: {lit(t)}}})<-[:RATED]-(u:User)-[:RATED]->(rec:Movie) RETURN DISTINCT rec.title",
                   None, "set", "Neo4j's own example: users who rated this also rated (all of them, deduplicated)"))
    for t in titles[:3]:
        qs.append((f"recommend 20: {t[:28]}",
                   f"MATCH (m:Movie {{title: {lit(t)}}})<-[:RATED]-(u:User)-[:RATED]->(rec:Movie) RETURN DISTINCT rec.title LIMIT 20",
                   None, "count", "the same, stopping at twenty"))
    for a in actors[:6]:
        qs.append((f"co-actors: {a[:26]}",
                   f"MATCH (p:Person {{name: {lit(a)}}})-[:ACTED_IN]->(m:Movie)<-[:ACTED_IN]-(q:Person) "
                   "RETURN q.name, count(m) AS n ORDER BY n DESC, q.name LIMIT 10", None, "rows",
                   "two hops with a grouped count, top ten"))
    for d in directors[:4]:
        qs.append((f"cast of a director: {d[:22]}",
                   f"MATCH (d:Person {{name: {lit(d)}}})-[:DIRECTED]->(m:Movie)<-[:ACTED_IN]-(a:Person) "
                   "RETURN DISTINCT a.name", None, "set", "everyone who acted in a film this person directed"))
    for a in actors[:3]:
        qs.append((f"actors two films away: {a[:20]}",
                   f"MATCH (p:Person {{name: {lit(a)}}})-[:ACTED_IN]->(:Movie)<-[:ACTED_IN]-(:Person)"
                   "-[:ACTED_IN]->(:Movie)<-[:ACTED_IN]-(q:Person) RETURN DISTINCT q.name", None, "set",
                   "four hops, deduplicated; thousands of rows"))
    qs.append(("most-rated movies",
               "MATCH (u:User)-[r:RATED]->(m:Movie) RETURN m.title, count(r) AS n ORDER BY n DESC, m.title LIMIT 10",
               None, "rows", "whole-graph aggregation over 100k ratings"))
    qs.append(("average rating, top ten",
               "MATCH (u:User)-[r:RATED]->(m:Movie) RETURN m.title, avg(r.rating) AS avg, count(r) AS n "
               "ORDER BY n DESC, m.title LIMIT 10", None, "rows", "two aggregates, grouped by movie"))
    qs.append(("movies per genre",
               "MATCH (g:Genre)<-[:IN_GENRE]-(m:Movie) RETURN g.name, count(m) AS n ORDER BY n DESC, g.name",
               None, "rows", "grouped count, every group"))
    qs.append(("busiest users",
               "MATCH (u:User)-[:RATED]->() RETURN u.name, count(*) AS n ORDER BY n DESC, u.name LIMIT 10",
               None, "rows", "degree by aggregation"))
    qs.append(("count everything",
               "MATCH (n) RETURN count(n)", None, "rows", "a full node count"))
    qs.append(("count ratings",
               "MATCH ()-[r:RATED]->() RETURN count(r)", None, "rows", "a typed edge count"))
    qs.append(("films from 1995",
               "MATCH (m:Movie) WHERE m.year = 1995 RETURN count(m)", None, "rows",
               "filter on an indexed property (Neo4j) / a scan (glider has no index on year here)"))
    return qs, {"actors": actors, "titles": titles}


def stackoverflow_queries(sess):
    def one(q):
        return [r[0] for r in neo4j_run(sess, q)[1]]

    tags = one("MATCH (t:Tag)<-[:TAGGED]-(q) WITH t, count(q) AS n WHERE n > 20 RETURN t.name ORDER BY t.name LIMIT 8")
    users = one("MATCH (u:User)-[:ASKED]->(q) WITH u, count(q) AS n WHERE n > 3 RETURN u.display_name ORDER BY u.display_name LIMIT 6")
    qs = []
    for t in tags[:6]:
        qs.append((f"answerers for tag: {t[:24]}",
                   f"MATCH (t:Tag {{name: {lit(t)}}})<-[:TAGGED]-(q:Question)<-[:ANSWERED]-(a:Answer)<-[:PROVIDED]-(u:User) "
                   "RETURN u.display_name, count(a) AS n ORDER BY n DESC, u.display_name LIMIT 10", None, "rows",
                   "Neo4j's example: who answers questions with this tag"))
    for u in users[:4]:
        qs.append((f"tags a user asks about: {u[:20]}",
                   f"MATCH (u:User {{display_name: {lit(u)}}})-[:ASKED]->(q:Question)-[:TAGGED]->(t:Tag) "
                   "RETURN t.name, count(q) AS n ORDER BY n DESC, t.name", None, "rows", "two hops, grouped"))
    qs.append(("questions per tag", "MATCH (t:Tag)<-[:TAGGED]-(q:Question) RETURN t.name, count(q) AS n ORDER BY n DESC, t.name LIMIT 20",
               None, "rows", "grouped count"))
    # Neo4j's "unanswered" needs a pattern predicate (NOT (q)<-[:ANSWERED]-()),
    # which glider has no way to say in one statement; its complement is the
    # same information and the same Cypher on both sides.
    qs.append(("answered questions", "MATCH (q:Question)<-[:ANSWERED]-() RETURN DISTINCT q.uuid", None, "set",
               "every question with at least one answer, deduplicated"))
    qs.append(("count everything", "MATCH (n) RETURN count(n)", None, "rows", "a full node count"))
    return qs, {"tags": tags, "users": users}


QUERIES = {"recommendations": recommendations_queries, "stackoverflow": stackoverflow_queries}

GLIDER_INDEXES = {
    "recommendations": ["INDEX ON :Movie(title)", "INDEX ON :Person(name)", "INDEX ON :User(name)"],
    "stackoverflow": ["INDEX ON :Tag(name)", "INDEX ON :User(display_name)"],
}


# ------------------------------------------------------------------ comparing

def norm(v):
    if isinstance(v, float):
        return round(v, 3)
    if isinstance(v, list):
        return tuple(norm(x) for x in v)
    return v


def same(how, a, b):
    if a is None or b is None:
        return None
    if how == "count":
        return len(a) == len(b)
    if how == "set":
        return sorted({norm(r[0]) for r in a}) == sorted({norm(r[0]) for r in b})
    return [tuple(norm(x) for x in r) for r in a] == [tuple(norm(x) for x in r) for r in b]


def measure(runner, q):
    """cold, then --warm warm runs -> (cold_s, warm_median_s, rows, error)"""
    cold, rows, err = runner(q)
    if err:
        return None, None, None, err
    warm = []
    for _ in range(ARGS.warm):
        t, rows, err = runner(q)
        if err:
            return cold, None, None, err
        warm.append(t)
    return cold, statistics.median(warm) if warm else None, rows, None


def ms(s):
    return None if s is None else round(s * 1000, 3)


# ------------------------------------------------------------------ phases

def phase_reads(dataset, sess, shell, out):
    qs, picks = QUERIES[dataset](sess)
    for name, ncy, gcy, how, note in qs:
        gcy = gcy or ncy
        n_cold, n_warm, n_rows, n_err = measure(lambda q: neo4j_run(sess, q), ncy)
        g_cold, g_warm, g_rows, g_err = measure(lambda q: glider_run(shell, q), gcy)
        rec = {"phase": "read", "dataset": dataset, "name": name, "note": note, "same_query": gcy == ncy,
               "neo4j": {"cypher": ncy, "cold_ms": ms(n_cold), "warm_ms": ms(n_warm),
                         "rows": None if n_rows is None else len(n_rows), "error": n_err},
               "glider": {"cypher": gcy, "cold_ms": ms(g_cold), "warm_ms": ms(g_warm),
                          "rows": None if g_rows is None else len(g_rows), "error": g_err},
               "match": same(how, n_rows, g_rows)}
        out(rec)
        log(f"  {name:38} neo4j {ms(n_warm)!s:>10} ms   glider {ms(g_warm)!s:>10} ms   "
            f"{'ok' if rec['match'] else ('MISMATCH' if rec['match'] is False else '?')}")
    return picks


def phase_algos(dataset, sess, shell, out, picks):
    # PageRank, top ten by score. Neo4j needs a projection first; its cost is
    # reported on its own line because a real workload pays it once.
    t0 = time.time()
    neo4j_run(sess, "CALL gds.graph.drop('g', false)")
    ps, _, perr = neo4j_run(sess, "CALL gds.graph.project('g', '*', '*')")
    project_s = time.time() - t0 if not perr else None
    out({"phase": "algo", "dataset": dataset, "name": "gds projection", "note": "Neo4j only: build the in-memory graph GDS runs on",
         "neo4j": {"cold_ms": ms(project_s), "warm_ms": None, "error": perr}, "glider": {"cold_ms": 0, "warm_ms": 0, "error": None},
         "match": None})

    ncy = ("CALL gds.pageRank.stream('g', {maxIterations: 20, dampingFactor: 0.85}) YIELD nodeId, score "
           "WITH gds.util.asNode(nodeId) AS n, score RETURN coalesce(n.title, n.name) AS name, score ORDER BY score DESC LIMIT 10")
    gcy = "CALL pagerank(iterations: 20, damping: 0.85, top: 10)"
    n_cold, n_warm, n_rows, n_err = measure(lambda q: neo4j_run(sess, q), ncy)
    g_cold, g_warm, g_rows, g_err = measure(lambda q: glider_run(shell, q), gcy)
    n_top = [r[0] for r in (n_rows or [])]
    g_top = [r[1] if len(r) > 1 else r[0] for r in (g_rows or [])]
    overlap = len(set(n_top) & set(g_top)) if n_rows and g_rows else None
    out({"phase": "algo", "dataset": dataset, "name": "pagerank top 10", "note": "20 iterations, damping 0.85; agreement is the overlap of the two top-ten lists",
         "neo4j": {"cypher": ncy, "cold_ms": ms(n_cold), "warm_ms": ms(n_warm), "rows": n_top, "error": n_err},
         "glider": {"cypher": gcy, "cold_ms": ms(g_cold), "warm_ms": ms(g_warm), "rows": g_top, "error": g_err},
         "match": overlap, "match_of": 10})
    log(f"  pagerank: neo4j {ms(n_warm)} ms  glider {ms(g_warm)} ms  overlap {overlap}/10  glider rows: {g_rows[:2] if g_rows else g_err}")

    # Weakly connected components: how many.
    ncy = "CALL gds.wcc.stats('g') YIELD componentCount RETURN componentCount"
    gcy = "CALL wcc()"
    n_cold, n_warm, n_rows, n_err = measure(lambda q: neo4j_run(sess, q), ncy)
    g_cold, g_warm, g_rows, g_err = measure(lambda q: glider_run(shell, q), gcy)
    n_count = n_rows[0][0] if n_rows else None
    g_count = len({r[-1] for r in g_rows}) if g_rows else None
    out({"phase": "algo", "dataset": dataset, "name": "connected components", "note": "number of weakly connected components",
         "neo4j": {"cypher": ncy, "cold_ms": ms(n_cold), "warm_ms": ms(n_warm), "rows": n_count, "error": n_err},
         "glider": {"cypher": gcy, "cold_ms": ms(g_cold), "warm_ms": ms(g_warm), "rows": g_count, "error": g_err},
         "match": (n_count == g_count) if n_count is not None and g_count is not None else None})
    log(f"  wcc: neo4j {ms(n_warm)} ms ({n_count})  glider {ms(g_warm)} ms ({g_count})")

    # Shortest paths between people, undirected, length only.
    people = picks.get("actors") or picks.get("users") or []
    pairs = list(zip(people[0::2], people[1::2]))[:4]
    key = "name" if dataset == "recommendations" else "display_name"
    plabel = "Person" if dataset == "recommendations" else "User"
    for a, b in pairs:
        ncy = (f"MATCH (a:{plabel} {{{key}: {lit(a)}}}), (b:{plabel} {{{key}: {lit(b)}}}) "
               "MATCH p = shortestPath((a)-[*..12]-(b)) RETURN length(p)")
        ids = {}
        for who in (a, b):
            _, rows, _ = glider_run(shell, f"MATCH (n:{plabel} {{{key}: {lit(who)}}}) RETURN id(n)")
            ids[who] = rows[0][0] if rows else None
        gcy = f"CALL shortestpath(from: {ids[a]}, to: {ids[b]}, dir: \"both\")"
        n_cold, n_warm, n_rows, n_err = measure(lambda q: neo4j_run(sess, q), ncy)
        g_cold, g_warm, g_rows, g_err = measure(lambda q: glider_run(shell, q), gcy)
        n_len = n_rows[0][0] if n_rows else None
        g_len = glider_path_length(g_rows)
        out({"phase": "algo", "dataset": dataset, "name": f"shortest path: {a[:14]} → {b[:14]}", "note": "undirected shortest path length",
             "neo4j": {"cypher": ncy, "cold_ms": ms(n_cold), "warm_ms": ms(n_warm), "rows": n_len, "error": n_err},
             "glider": {"cypher": gcy, "cold_ms": ms(g_cold), "warm_ms": ms(g_warm), "rows": g_len, "error": g_err},
             "match": (n_len == g_len) if n_len is not None and g_len is not None else None})
        log(f"  path {a[:14]}→{b[:14]}: neo4j {ms(n_warm)} ms ({n_len})  glider {ms(g_warm)} ms ({g_len})")
    neo4j_run(sess, "CALL gds.graph.drop('g', false)")


def glider_path_length(rows):
    """glider's shortestpath answer: a `length` column if it has one, else one row per hop."""
    if not rows:
        return None
    first = rows[0]
    if len(first) >= 1 and isinstance(first[-1], int) and len(rows) == 1:
        return first[-1]
    return len(rows) - 1


def phase_writes(dataset, sess, shell, out):
    """Single-statement writes, the way an application issues them: create a
    user and a rating, update it, delete it — --writes of each."""
    n = ARGS.writes
    label, rel, target, tkey = {
        "recommendations": ("User", "RATED", "Movie", "movieId"),
        "stackoverflow": ("User", "ASKED", "Question", "uuid"),
    }[dataset]
    _, targets, _ = neo4j_run(sess, f"MATCH (t:{target}) RETURN t.{tkey} LIMIT {n}")
    targets = [r[0] for r in targets]

    def timed_batch(runner, statements):
        total = 0.0
        for q in statements:
            t, _, err = runner(q)
            if err:
                return None, err
            total += t or 0
        return total, None

    def creates(i):
        return (f"CREATE (u:{label} {{name: \"bench user {i}\", benchId: {i}}})",
                f"MATCH (u:{label} {{benchId: {i}}}), (t:{target} {{{tkey}: {lit(targets[i % len(targets)])}}}) "
                f"CREATE (u)-[:{rel} {{rating: 4.0, bench: true}}]->(t)")

    steps = [
        ("create node + edge", [s for i in range(n) for s in creates(i)]),
        ("update a property", [f"MATCH (u:{label} {{benchId: {i}}}) SET u.name = \"renamed {i}\"" for i in range(n)]),
        ("delete node + edge", [f"MATCH (u:{label} {{benchId: {i}}}) DETACH DELETE u" for i in range(n)]),
    ]
    for name, stmts in steps:
        n_total, n_err = timed_batch(lambda q: neo4j_run(sess, q), stmts)
        g_total, g_err = timed_batch(lambda q: glider_run(shell, q), stmts)
        count = len(stmts)
        out({"phase": "write", "dataset": dataset, "name": f"{name} ×{count}", "note": f"{count} single statements, autocommit each",
             "neo4j": {"cold_ms": ms(n_total), "warm_ms": ms(n_total / count) if n_total is not None else None, "error": n_err},
             "glider": {"cold_ms": ms(g_total), "warm_ms": ms(g_total / count) if g_total is not None else None, "error": g_err},
             "match": None})
        log(f"  {name}: neo4j {ms(n_total)} ms total  glider {ms(g_total)} ms total")
    # Leave nothing behind either side.
    neo4j_run(sess, f"MATCH (u:{label}) WHERE u.benchId IS NOT NULL DETACH DELETE u")
    glider_run(shell, f"MATCH (u:{label}) WHERE u.benchId IS NOT NULL DETACH DELETE u")


# ------------------------------------------------------------------ main

def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", default="recommendations", choices=sorted(DUMPS))
    ap.add_argument("--only", default="reads,algos,writes")
    ap.add_argument("--warm", type=int, default=5)
    ap.add_argument("--writes", type=int, default=300)
    ap.add_argument("--timeout", type=float, default=300)
    ap.add_argument("--mem-gb", type=int, default=6)
    ap.add_argument("--mem-cap", type=float, default=8.0, help="GiB address-space cap on the glider shell")
    ap.add_argument("--results", default=None)
    ARGS = ap.parse_args()
    scale.ARGS = ARGS
    results = ARGS.results or os.path.join(HERE, "results", f"neo4j-{ARGS.dataset}.jsonl")
    os.makedirs(os.path.dirname(results), exist_ok=True)
    rf = open(results, "w")

    def out(rec):
        rf.write(json.dumps(rec, ensure_ascii=False) + "\n")
        rf.flush()

    drv = neo4j_up(ARGS.dataset)
    db = glider_db(ARGS.dataset)
    shell = suite.Glider(db)
    for idx in GLIDER_INDEXES[ARGS.dataset]:
        glider_run(shell, idx)
    with drv.session() as sess:
        _, nn, _ = neo4j_run(sess, "MATCH (n) RETURN count(n)")
        _, ne, _ = neo4j_run(sess, "MATCH ()-[r]->() RETURN count(r)")
        _, gn, _ = glider_run(shell, "MATCH (n) RETURN count(n)")
        _, ge, _ = glider_run(shell, "MATCH ()-[r]->() RETURN count(r)")
        out({"phase": "meta", "dataset": ARGS.dataset, "neo4j": {"nodes": nn[0][0], "edges": ne[0][0]},
             "glider": {"nodes": gn[0][0], "edges": ge[0][0], "file_bytes": os.path.getsize(db)},
             "warm": ARGS.warm, "writes": ARGS.writes, "neo4j_version": neo4j_run(sess, "CALL dbms.components() YIELD versions RETURN versions[0]")[1][0][0],
             "glider_version": subprocess.run([suite.GLIDER, "--version"], capture_output=True, text=True).stdout.strip()})
        log(f"neo4j: {nn[0][0]} nodes / {ne[0][0]} edges   glider: {gn[0][0]} / {ge[0][0]}")
        picks = {}
        if "reads" in ARGS.only:
            log("reads"); picks = phase_reads(ARGS.dataset, sess, shell, out)
        if "algos" in ARGS.only:
            log("algorithms"); phase_algos(ARGS.dataset, sess, shell, out, picks or QUERIES[ARGS.dataset](sess)[1])
        if "writes" in ARGS.only:
            log("writes"); phase_writes(ARGS.dataset, sess, shell, out)
    rf.close()
    drv.close()
    log(f"results: {results}")


if __name__ == "__main__":
    main()
