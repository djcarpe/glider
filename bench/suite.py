#!/usr/bin/env python3
"""glider vs SQLite: reads, graph algorithms and writes, 500 MiB to 25 GiB,
with traces, logs, metrics and flamegraphs in a Grafana LGTM stack.

    bench/lgtm/up.sh                                  # Grafana, Loki, Tempo, Mimir, Pyroscope, Alloy
    python3 bench/suite.py [--sizes 500MiB,1GiB] [--only build,reads,algos,writes]
    python3 bench/suite_report.py                     # -> suite.html

The graph and the SQLite schema are those of bench/sqlite_compare.py (a
table per label and per relationship type, foreign keys, both-way
relationship indexes, the same property indexes as glider). Every read and
algorithm is checked: the engines must return the same answer.

Timing is inside each engine's process: glider's shell `.timer`, SQLite
around execute + fetchall in a Python worker. glider runs as the
`profiling` build (release plus symbols and frame pointers), so the eBPF
profiler can name its functions; SQLite runs in the worker process, which
the profiler finds by its `--sqlite-worker` argument.

After its timed runs, each read is repeated for a short profiling window,
so even microsecond queries collect enough samples for a flamegraph. Every
benchmark is one trace (a span per engine and per run), with a log line per
run carrying the trace and span ids, and a gauge per result.

Order per size: build, reads (cold then warm), algorithms, writes. Writes
change the data, so they run last.
"""

import argparse
import json
import os
import random
import resource
import sqlite3
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import otel  # noqa: E402
import scale  # noqa: E402
import sqlite_compare as sc  # noqa: E402
from scale import Layout, evict, fsize, log  # noqa: E402

ROOT = scale.ROOT
DATA = os.path.join(scale.BENCH_DATA, "suite")
GLIDER = os.path.join(ROOT, "target", "profiling", "glider")
GEN = os.path.join(ROOT, "target", "release", "scale-gen")
SIZES = ["500MiB", "1GiB", "5GiB", "10GiB", "25GiB"]
CACHE_KIB = sc.CACHE_KIB
PROFILE_WINDOW_S = 1.5

ARGS = None


def emit(rec):
    rec["t"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(ARGS.results, "a") as f:
        f.write(json.dumps(rec) + "\n")
    log("  ", json.dumps({k: v for k, v in rec.items() if k not in ("rows", "sql", "cypher")})[:220])


def gpath(size):
    return os.path.join(DATA, f"glider-{size}.db")


def spath(size):
    return os.path.join(DATA, f"sqlite-{size}.db")


# ------------------------------------------------------------------ build

def phase_build(size):
    g = gpath(size)
    if not os.path.exists(g) or ARGS.rebuild:
        t = time.time()
        r = scale.run([GEN, "--size", size, "--paged", "--work-mb", "256", "--cache-mb", "256", "--out", g],
                      timeout=ARGS.timeout * 12)
        gen = [ln for ln in r["out"].splitlines() if ln.startswith("{")]
        emit({"phase": "build", "engine": "glider", "size": size, "wall_s": round(time.time() - t, 2),
              "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0, "file_bytes": sc.tree_bytes(g),
              "gen": json.loads(gen[-1]) if gen else None})
    s = spath(size)
    old = os.path.join(sc.DATA, f"sqlite-{size}.db")
    if not os.path.exists(s) and os.path.exists(old) and not ARGS.rebuild:
        # Same generator, seed and loader as bench/sqlite_compare.py: reuse
        # its file (a rename on the same filesystem) and its build record.
        os.rename(old, s)
        for line in [] if has_record(size, "sqlite") else open(os.path.join(sc.DATA, "results.jsonl")):
            d = json.loads(line)
            if d.get("phase") == "build" and d.get("engine") == "sqlite" and d.get("size") == size:
                emit(dict(d, reused_from="bench/sqlite_compare.py"))
    if os.path.exists(s) and not ARGS.rebuild and not has_record(size, "sqlite"):
        for line in open(os.path.join(sc.DATA, "results.jsonl")):
            d = json.loads(line)
            if d.get("phase") == "build" and d.get("engine") == "sqlite" and d.get("size") == size:
                emit(dict(d, reused_from="bench/sqlite_compare.py"))
    if not os.path.exists(s) or ARGS.rebuild:
        os.environ["SQLITE_TMPDIR"] = DATA
        t = time.time()
        exp = subprocess.Popen([GLIDER, g, "export"], stdout=subprocess.PIPE, preexec_fn=scale.cap, bufsize=1 << 20)
        info = sc.load_sqlite(exp.stdout, s)
        exp.wait()
        if exp.returncode != 0 or not info["nodes"]:
            os.remove(s)
            raise SystemExit(f"glider export failed at {size}")
        emit({"phase": "build", "engine": "sqlite", "size": size, "wall_s": round(time.time() - t, 2), "ok": True,
              "file_bytes": fsize(s), **info})


def has_record(size, engine):
    if not os.path.exists(ARGS.results):
        return False
    for line in open(ARGS.results):
        d = json.loads(line)
        if d.get("phase") == "build" and d.get("engine") == engine and d.get("size") == size:
            return True
    return False


def layout(size):
    ids = None
    for line in open(ARGS.results):
        d = json.loads(line)
        if d.get("phase") == "build" and d.get("engine") == "glider" and d.get("size") == size and d.get("gen"):
            ids = d["gen"]["ids"]
    return Layout(ids)


# ------------------------------------------------------------------ engines

class Glider(sc.Shell):
    """The glider shell, profiling build."""

    def __init__(self, db, sync="normal"):
        self.db, self.sync = db, sync
        self.p = subprocess.Popen([GLIDER, db, "--json", "--cache-size", "1G", "--sync", sync],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  text=True, bufsize=1, preexec_fn=scale.cap)
        self.fd = self.p.stdout.fileno()
        self.buf = b""
        self.send(".timer")
        self.readline(60)

    def run(self, q, timeout):
        """-> (seconds, rows, error). A statement that times out leaves the
        shell busy, so the shell is replaced: the next statement must not
        read the late answer to this one."""
        try:
            return self.query(q, timeout)
        except (TimeoutError, EOFError) as e:
            self.p.kill()
            self.p.wait()
            self.__init__(self.db, self.sync)
            return None, None, "timeout" if isinstance(e, TimeoutError) else "glider exited"


class Sqlite:
    """A persistent SQLite worker process (see worker())."""

    def __init__(self, db, sync="NORMAL"):
        self.p = subprocess.Popen([sys.executable, os.path.abspath(__file__), "--sqlite-worker", db, "--sync", sync],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
                                  preexec_fn=scale.cap)

    def call(self, **req):
        self.p.stdin.write(json.dumps(req) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            return {"error": "sqlite worker exited"}
        return json.loads(line)

    def run(self, sql, params, timeout):
        r = self.call(op="query", sql=sql, params=list(params), timeout=timeout)
        return (r.get("s"), r.get("rows"), r.get("error"))

    def hwm_mb(self):
        return self.call(op="rss").get("mb")

    def close(self):
        try:
            self.call(op="quit")
            self.p.wait(30)
        except Exception:  # noqa: BLE001
            self.p.kill()


def evict_all(size):
    sc.evict_tree(gpath(size))
    sc.evict_tree(spath(size))


# ------------------------------------------------------------------ telemetry

class Bench:
    """One benchmark at one size: a trace, with a span per engine and per run."""

    def __init__(self, size, kind, name):
        self.size, self.kind, self.name = size, kind, name
        self.trace = otel.new_trace_id()
        self.root = otel.new_span_id()
        self.start = otel.now_ns()
        self.engines = {}

    def engine(self, engine):
        e = self.engines.setdefault(engine, {"span": otel.new_span_id(), "start": otel.now_ns(), "runs": []})
        return e

    def run(self, engine, label, t0_ns, t1_ns, engine_s, rows=None, error=None):
        e = self.engine(engine)
        sid = otel.span(self.trace, f"{engine} {label}", t0_ns, t1_ns, parent=e["span"],
                        attrs={"engine": engine, "size": self.size, "benchmark": self.name, "run": label,
                               "engine_ms": None if engine_s is None else engine_s * 1000, "rows": rows}, error=error)
        otel.log(f"{self.kind} {self.name!r} {engine} {label}: "
                 + (f"{engine_s * 1000:.3f} ms" if engine_s is not None else f"error {error}"),
                 trace_id=self.trace, span_id=sid,
                 attrs={"engine": engine, "size": self.size, "benchmark": self.name, "kind": self.kind, "run": label},
                 severity="ERROR" if error else "INFO", t_ns=t1_ns)
        if engine_s is not None:
            otel.gauge("bench_duration", engine_s * 1000,
                       {"engine": engine, "size": self.size, "benchmark": self.name, "kind": self.kind, "run": label}, t_ns=t1_ns)
        e["runs"].append((t0_ns, t1_ns))

    def window(self, engine, t0_ns, t1_ns):
        self.engine(engine)["window"] = (t0_ns // 1_000_000, t1_ns // 1_000_000)

    def finish(self, attrs=None):
        end = otel.now_ns()
        for engine, e in self.engines.items():
            runs = e["runs"] or [(e["start"], end)]
            otel.span(self.trace, engine, runs[0][0], runs[-1][1], parent=self.root, span_id=e["span"],
                      attrs={"engine": engine, "size": self.size, "benchmark": self.name})
        otel.span(self.trace, f"{self.kind}: {self.name} @ {self.size}", self.start, end, span_id=self.root,
                  attrs=dict({"size": self.size, "benchmark": self.name, "kind": self.kind}, **(attrs or {})))
        otel.flush()
        out = {"trace_id": self.trace, "start_ms": self.start // 1_000_000, "end_ms": end // 1_000_000}
        for engine, e in self.engines.items():
            if "window" in e:
                out[f"{engine}_window"] = e["window"]
            elif e["runs"]:
                out[f"{engine}_window"] = (e["runs"][0][0] // 1_000_000, e["runs"][-1][1] // 1_000_000)
        return out


def timed(engine_obj, bench, engine, label, fn):
    t0 = otel.now_ns()
    try:
        s, rows, err = fn()
    except (TimeoutError, EOFError) as e:
        s, rows, err = None, None, "timeout" if isinstance(e, TimeoutError) else "engine exited"
    t1 = otel.now_ns()
    bench.run(engine, label, t0, t1, s, rows=len(rows) if rows is not None else None, error=err)
    return s, rows, err


def repeat(bench, engine, fn, first):
    """Warm runs, then a profiling window. -> (warm seconds list, rows, err)."""
    times, rows, err = [], None, None
    if first is not None and first > ARGS.slow:
        return times, rows, err
    for i in range(ARGS.warm):
        s, rows, err = timed(None, bench, engine, f"warm {i + 1}", fn)
        if err:
            return times, rows, err
        times.append(s)
    # Keep running the query, untimed, long enough for the profiler.
    t0 = otel.now_ns()
    deadline = time.time() + PROFILE_WINDOW_S
    n = 0
    while time.time() < deadline and n < 100_000:
        fn()
        n += 1
    bench.window(engine, t0, otel.now_ns())
    return times, rows, err


def ms(x):
    return None if x is None else round(x * 1000, 3)


def med(xs):
    return statistics.median(xs) if xs else None


# ------------------------------------------------------------------ reads

def phase_reads(size):
    lay = layout(size)
    qs = sc.queries(lay)
    if ARGS.q:
        qs = [q for q in qs if any(w in q[0] for w in ARGS.q.split(","))]
    evict_all(size)
    results = {}
    g = Glider(gpath(size))
    benches = {name: Bench(size, "read", name) for name, *_ in qs}
    for name, shape, cy, sql, params, *cmp in qs:
        b = benches[name]
        s, rows, err = timed(g, b, "glider", "cold", lambda: g.run(cy, ARGS.timeout))
        warm, wrows, werr = repeat(b, "glider", lambda: g.run(cy, ARGS.timeout), s) if not err else ([], None, None)
        results[name] = {"glider": (s, warm, wrows if wrows is not None else rows, err or werr)}
    g_rss = g.hwm_mb()
    g.close()

    q = Sqlite(spath(size))
    for name, shape, cy, sql, params, *cmp in qs:
        b = benches[name]
        s, rows, err = timed(q, b, "sqlite", "cold", lambda: q.run(sql, params, ARGS.timeout))
        warm, wrows, werr = repeat(b, "sqlite", lambda: q.run(sql, params, ARGS.timeout), s) if not err else ([], None, None)
        results[name]["sqlite"] = (s, warm, wrows if wrows is not None else rows, err or werr)
        results[name]["plan"] = q.call(op="plan", sql=sql, params=list(params)).get("plan")
    s_rss = q.hwm_mb()
    q.close()

    for name, shape, cy, sql, params, *cmp in qs:
        gs, gw, gr, ge = results[name]["glider"]
        ss, sw, sr, se = results[name]["sqlite"]
        match = None
        if gr is not None and sr is not None and not ge and not se:
            match = sc.same_answer(cmp[0] if cmp else "rows", cy, sql, gr, sr)
        tel = benches[name].finish({"match": match})
        emit({"phase": "read", "size": size, "name": name, "shape": shape, "cypher": cy, "sql": sql,
              "params": list(params), "match": match, "telemetry": tel,
              "glider": {"cold_ms": ms(gs), "warm_ms": ms(med(gw)), "row_count": len(gr or []), "rows": (gr or [])[:8], "error": ge},
              "sqlite": {"cold_ms": ms(ss), "warm_ms": ms(med(sw)), "row_count": len(sr or []), "rows": (sr or [])[:8], "error": se,
                         "plan": results[name]["plan"]}})
    emit({"phase": "memory", "size": size, "glider_peak_rss_mb": g_rss, "sqlite_peak_rss_mb": s_rss})


# ------------------------------------------------------------------ algorithms

def algorithms(lay):
    person = lay.member("Person", 0.5)
    far = lay.member("Person", 0.9)
    return [
        ("degree centrality: top 10 KNOWS in-degree", "degree",
         'CALL degree(type: "KNOWS", dir: "in", top: 10)'),
        ("PageRank: 5 iterations over KNOWS", "pagerank",
         'CALL pagerank(type: "KNOWS", iterations: 5, tolerance: 0, top: 10)'),
        ("weakly connected components over KNOWS", "wcc",
         'CALL wcc(type: "KNOWS", top: 0)'),
        ("BFS: 3 hops over KNOWS", "bfs3",
         f'CALL bfs(from: {person}, depth: 3, dir: "out", type: "KNOWS")'),
        ("shortest path over KNOWS", "path",
         f'CALL shortestpath(from: {person}, to: {far}, dir: "out", type: "KNOWS")'),
    ], {"person": person, "far": far}


def algo_answer(kind, rows, message):
    """Normalise each engine's answer to something comparable."""
    if rows is None:
        return None
    if kind == "degree":
        return [[r[0], r[-1]] for r in rows]
    if kind == "pagerank":
        return [[r[0], round(r[-1], 9)] for r in rows]
    if kind == "wcc":
        return message
    if kind == "bfs3":
        return sorted([r[0], r[2]] for r in rows)
    if kind == "path":
        return len(rows) - 1 if rows else None
    return rows


def same_algo(kind, g, s):
    if kind == "pagerank":
        # Same ranking; scores equal to 1e-6 relative (summation order differs).
        return [r[0] for r in g] == [r[0] for r in s] and all(
            abs(a[1] - b[1]) <= 1e-12 + 1e-6 * abs(b[1]) for a, b in zip(g, s))
    return g == s


def phase_algos(size):
    lay = layout(size)
    algos, where = algorithms(lay)
    if ARGS.q:
        algos = [a for a in algos if any(w in a[0] for w in ARGS.q.split(","))]
    evict_all(size)
    out = {}
    g = Glider(gpath(size))
    benches = {name: Bench(size, "algorithm", name) for name, *_ in algos}
    for name, kind, cy in algos:
        b = benches[name]
        s, rows, err = timed(g, b, "glider", "run 1", lambda: g.run(cy, ARGS.algo_timeout))
        # WCC's answer is its message: "N connected components".
        msg = getattr(g, "last_message", None)
        warm = []
        if not err and s is not None and s < ARGS.slow:
            warm, _, _ = repeat(b, "glider", lambda: g.run(cy, ARGS.algo_timeout), s)
        out[name] = {"glider": (s, warm, rows, err), "message": msg}
    g.close()
    q = Sqlite(spath(size))
    for name, kind, cy in algos:
        b = benches[name]
        t0 = otel.now_ns()
        r = q.call(op="algo", kind=kind, where=where, timeout=ARGS.algo_timeout)
        b.run("sqlite", "run 1", t0, otel.now_ns(), r.get("s"), rows=len(r.get("rows") or []), error=r.get("error"))
        warm = []
        if not r.get("error") and r.get("s") is not None and r["s"] < ARGS.slow:
            fn = lambda: (lambda x: (x.get("s"), x.get("rows"), x.get("error")))(q.call(op="algo", kind=kind, where=where, timeout=ARGS.algo_timeout))  # noqa: E731
            warm, _, _ = repeat(b, "sqlite", fn, r["s"])
        out[name]["sqlite"] = (r.get("s"), warm, r.get("rows"), r.get("error"), r.get("answer"), r.get("sql"))
    q.close()
    for name, kind, cy in algos:
        gs, gw, gr, ge = out[name]["glider"]
        ss, sw, sr, se, sans, ssql = out[name]["sqlite"]
        gans = out[name]["message"] if kind == "wcc" else algo_answer(kind, gr, None)
        match = None if (ge or se or gans is None or sans is None) else same_algo(kind, gans, sans)
        tel = benches[name].finish({"match": match})
        emit({"phase": "algo", "size": size, "name": name, "kind": kind, "cypher": cy, "sql": ssql,
              "match": match, "telemetry": tel,
              "glider": {"cold_ms": ms(gs), "warm_ms": ms(med(gw)), "answer": gans, "error": ge},
              "sqlite": {"cold_ms": ms(ss), "warm_ms": ms(med(sw)), "answer": sans, "error": se}})


# ------------------------------------------------------------------ writes

def writes(lay, size):
    rnd = random.Random(2026)
    lo, hi = lay.ranges["Person"]
    def person():
        while True:
            i = rnd.randrange(lo, hi)
            if lay.exists(i):
                return f"user{i:010d}@example.org"
    tag = size.lower()
    return [
        ("insert a node, autocommit (x1000)", "normal", False,
         [(f'CREATE (:Person {{email: "w1-{tag}-{i}@x.org", name: "Bench {i}", age: {i % 90}, country: "NZ"}})',
           "INSERT INTO person (labels, email, name, age, country) VALUES ('Person', ?, ?, ?, 'NZ')",
           [f"w1-{tag}-{i}@x.org", f"Bench {i}", i % 90]) for i in range(1000)]),
        ("insert a node, fsync every commit (x200)", "always", False,
         [(f'CREATE (:Person {{email: "w2-{tag}-{i}@x.org", name: "Durable {i}", age: 30, country: "NZ"}})',
           "INSERT INTO person (labels, email, name, age, country) VALUES ('Person', ?, ?, 30, 'NZ')",
           [f"w2-{tag}-{i}@x.org", f"Durable {i}"]) for i in range(200)]),
        ("bulk insert 50,000 nodes, one transaction", "normal", True,
         [(f'CREATE (:Person {{email: "w3-{tag}-{i}@x.org", name: "Bulk {i}", age: {i % 90}, country: "AU"}})',
           "INSERT INTO person (labels, email, name, age, country) VALUES ('Person', ?, ?, ?, 'AU')",
           [f"w3-{tag}-{i}@x.org", f"Bulk {i}", i % 90]) for i in range(50_000)]),
        ("insert 5,000 edges between indexed nodes, one transaction", "normal", True,
         [(lambda a, b: (f'MATCH (a:Person {{email: "{a}"}}), (b:Person {{email: "{b}"}}) '
                         f'CREATE (a)-[:KNOWS {{since: 2026, weight: 0.5}}]->(b)',
                         "INSERT INTO knows (src, dst, since, weight) SELECT a.id, b.id, 2026, 0.5 "
                         "FROM person a, person b WHERE a.email = ? AND b.email = ?", [a, b]))(person(), person())
          for _ in range(5000)]),
        ("update 10,000 properties by index, one transaction", "normal", True,
         [(lambda e, v: (f'MATCH (p:Person {{email: "{e}"}}) SET p.age = {v}',
                         "UPDATE person SET age = ? WHERE email = ?", [v, e]))(person(), rnd.randrange(18, 90))
          for _ in range(10_000)]),
        ("delete 500 nodes with their edges, one transaction", "normal", True,
         [(lambda e: (f'MATCH (p:Person {{email: "{e}"}}) DETACH DELETE p', "DELETE_PERSON", [e]))(person())
          for _ in range(500)]),
    ]


def phase_writes(size):
    lay = layout(size)
    ws = writes(lay, size)
    if ARGS.q:
        ws = [w for w in ws if any(x in w[0] for x in ARGS.q.split(","))]
    for name, sync, txn, stmts in ws:
        b = Bench(size, "write", name)
        # glider
        g = Glider(gpath(size), sync=sync)
        t0 = otel.now_ns()
        total, err = 0.0, None
        if txn:
            s, _, err = g.run("BEGIN", ARGS.timeout)
            total += s or 0
        for cy, *_ in stmts:
            if err:
                break
            s, _, err = g.run(cy, ARGS.timeout)
            total += s or 0
        if txn and not err:
            s, _, err = g.run("COMMIT", ARGS.timeout)
            total += s or 0
        b.run("glider", "run", t0, otel.now_ns(), None if err else total, error=err)
        g_counts = count_graph(g)
        g.close()
        # SQLite
        q = Sqlite(spath(size), sync="FULL" if sync == "always" else "NORMAL")
        t0 = otel.now_ns()
        r = q.call(op="writes", stmts=[[sql, p] for _, sql, p in stmts], txn=txn, timeout=ARGS.timeout)
        b.run("sqlite", "run", t0, otel.now_ns(), r.get("s"), error=r.get("error"))
        s_counts = q.call(op="counts").get("counts")
        q.close()
        tel = b.finish()
        n = len(stmts)
        emit({"phase": "write", "size": size, "name": name, "ops": n, "sync": sync, "transaction": txn,
              "cypher": stmts[0][0], "sql": stmts[0][1], "telemetry": tel,
              "match": g_counts == s_counts if g_counts and s_counts else None, "counts": {"glider": g_counts, "sqlite": s_counts},
              "glider": {"ms": ms(None if err else total), "ops_per_s": round(n / total) if total and not err else None, "error": err},
              "sqlite": {"ms": ms(r.get("s")), "ops_per_s": round(n / r["s"]) if r.get("s") else None, "error": r.get("error")}})


def count_graph(g):
    out = {}
    for key, cy in [("person", "MATCH (p:Person) RETURN count(p)"), ("knows", "MATCH ()-[r:KNOWS]->() RETURN count(r)")]:
        s, rows, err = g.run(cy, ARGS.timeout)
        out[key] = rows[0][0] if rows else 0
    return out


# ------------------------------------------------------------------ the SQLite worker

PERSON_EDGES = [("knows", "src"), ("knows", "dst"), ("follows", "src"), ("follows", "dst"), ("works_at", "src"),
                ("reviewed", "src"), ("placed", "src"), ("authored", "src"), ("triggered_by", "dst")]


def worker(db_path, sync):
    db = sqlite3.connect(db_path, isolation_level=None)
    db.execute(f"PRAGMA cache_size=-{CACHE_KIB}")
    db.execute("PRAGMA foreign_keys=ON")
    db.execute("PRAGMA journal_mode=WAL")
    db.execute(f"PRAGMA synchronous={sync}")
    db.execute(f"PRAGMA temp_store=FILE")

    def guard(timeout):
        deadline = time.time() + timeout
        db.set_progress_handler(lambda: 1 if time.time() > deadline else 0, 100_000)
        return deadline

    def reply(d):
        sys.stdout.write(json.dumps(d) + "\n")
        sys.stdout.flush()

    for line in sys.stdin:
        req = json.loads(line)
        op = req["op"]
        try:
            if op == "quit":
                reply({})
                return
            if op == "rss":
                reply({"mb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)})
            elif op == "plan":
                reply({"plan": [r[3] for r in db.execute("EXPLAIN QUERY PLAN " + req["sql"], req["params"])]})
            elif op == "query":
                deadline = guard(req["timeout"])
                t = time.perf_counter()
                try:
                    rows = [list(r) for r in db.execute(req["sql"], req["params"]).fetchall()]
                    reply({"s": time.perf_counter() - t, "rows": rows})
                except sqlite3.Error as e:
                    reply({"error": ("timeout" if time.time() > deadline else f"{type(e).__name__}: {e}")})
            elif op == "algo":
                deadline = guard(req["timeout"])
                t = time.perf_counter()
                try:
                    rows, answer, sql = run_algo(db, req["kind"], req["where"])
                    reply({"s": time.perf_counter() - t, "rows": rows, "answer": answer, "sql": sql})
                except sqlite3.Error as e:
                    reply({"error": ("timeout" if time.time() > deadline else f"{type(e).__name__}: {e}"), "sql": ALGO_SQL.get(req["kind"])})
            elif op == "writes":
                deadline = guard(req["timeout"])
                t = time.perf_counter()
                try:
                    if req["txn"]:
                        db.execute("BEGIN")
                    for sql, params in req["stmts"]:
                        if sql == "DELETE_PERSON":
                            delete_person(db, params[0])
                        else:
                            db.execute(sql, params)
                    if req["txn"]:
                        db.execute("COMMIT")
                    reply({"s": time.perf_counter() - t})
                except sqlite3.Error as e:
                    if db.in_transaction:
                        db.execute("ROLLBACK")
                    reply({"error": ("timeout" if time.time() > deadline else f"{type(e).__name__}: {e}")})
            elif op == "counts":
                reply({"counts": {"person": db.execute("SELECT count(*) FROM person").fetchone()[0],
                                  "knows": db.execute("SELECT count(*) FROM knows").fetchone()[0]}})
        except Exception as e:  # noqa: BLE001
            reply({"error": f"{type(e).__name__}: {e}"})


def delete_person(db, email):
    """DETACH DELETE, relationally: every relationship row that references the
    person, then the person."""
    row = db.execute("SELECT id FROM person WHERE email = ?", [email]).fetchone()
    if not row:
        return
    pid = row[0]
    for table, col in PERSON_EDGES:
        db.execute(f"DELETE FROM {table} WHERE {col} = ?", [pid])
    db.execute("DELETE FROM mentions WHERE dst_kind = 'person' AND dst = ?", [pid])
    db.execute("DELETE FROM person WHERE id = ?", [pid])


ALGO_SQL = {
    "degree": "SELECT dst, count(*) AS d FROM knows GROUP BY dst ORDER BY d DESC, dst LIMIT 10",
    "pagerank": """-- per iteration, with N = all nodes, d = 0.85, dangling mass shared evenly:
-- non-Person nodes have no KNOWS edges, so they share one score (ro) and are not stored.
CREATE TEMP TABLE nx (id INTEGER PRIMARY KEY, r REAL);
INSERT INTO nx
SELECT p.id, :base + :d * coalesce(s.x, 0)
FROM pr p LEFT JOIN (
  SELECT k.dst AS id, sum(pr.r / deg.n) AS x
  FROM knows k JOIN pr ON pr.id = k.src JOIN deg ON deg.id = k.src
  GROUP BY k.dst) s ON s.id = p.id;
-- then: pr := nx, ro := :base, repeat""",
    "wcc": """-- minimum-label propagation until nothing changes; isolated nodes are their own components
CREATE TEMP TABLE nx (id INTEGER PRIMARY KEY, c INTEGER);
INSERT INTO nx SELECT id, min(c) FROM (
  SELECT id, c FROM comp
  UNION ALL SELECT k.dst, c.c FROM knows k JOIN comp c ON c.id = k.src
  UNION ALL SELECT k.src, c.c FROM knows k JOIN comp c ON c.id = k.dst) GROUP BY id;
-- stop when no row changed; components = distinct labels + nodes outside Person""",
    "bfs3": """WITH RECURSIVE r(id, d) AS (SELECT :start, 0 UNION
  SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id WHERE r.d < 3)
SELECT id, min(d) FROM r GROUP BY id""",
    "path": """WITH RECURSIVE r(id, d) AS (SELECT :start, 0 UNION
  SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id WHERE r.d < 12 AND r.id <> :target)
SELECT min(d) FROM r WHERE id = :target""",
}


def total_nodes(db):
    return sum(db.execute(f"SELECT count(*) FROM {t}").fetchone()[0] for t in sc.NODE_TABLES)


def run_algo(db, kind, where):
    sql = ALGO_SQL[kind]
    if kind == "degree":
        rows = [list(r) for r in db.execute(sql)]
        return rows, rows, sql
    if kind == "bfs3":
        rows = [list(r) for r in db.execute(sql, {"start": where["person"]})]
        return rows, sorted(rows), sql
    if kind == "path":
        rows = [list(r) for r in db.execute(sql, {"start": where["person"], "target": where["far"]})]
        return rows, rows[0][0] if rows else None, sql
    if kind == "pagerank":
        n = total_nodes(db)
        persons = db.execute("SELECT count(*) FROM person").fetchone()[0]
        others = n - persons
        d, base0 = 0.85, 1.0 / n
        for t in ("pr", "deg", "nx"):
            db.execute(f"DROP TABLE IF EXISTS temp.{t}")
        db.execute("CREATE TEMP TABLE deg (id INTEGER PRIMARY KEY, n INTEGER)")
        db.execute("INSERT INTO deg SELECT src, count(*) FROM knows GROUP BY src")
        db.execute("CREATE TEMP TABLE pr (id INTEGER PRIMARY KEY, r REAL)")
        db.execute("INSERT INTO pr SELECT id, ? FROM person", [base0])
        ro = base0
        for _ in range(5):
            dangling = db.execute("SELECT coalesce(sum(r), 0) FROM pr WHERE id NOT IN (SELECT id FROM deg)").fetchone()[0]
            dangling += others * ro
            base = (1 - d) / n + d * dangling / n
            db.execute("CREATE TEMP TABLE nx (id INTEGER PRIMARY KEY, r REAL)")
            db.execute("""INSERT INTO nx SELECT p.id, :base + :d * coalesce(s.x, 0) FROM pr p LEFT JOIN (
                            SELECT k.dst AS id, sum(pr.r / deg.n) AS x FROM knows k
                            JOIN pr ON pr.id = k.src JOIN deg ON deg.id = k.src GROUP BY k.dst) s ON s.id = p.id""",
                       {"base": base, "d": d})
            db.execute("DROP TABLE temp.pr")
            db.execute("ALTER TABLE temp.nx RENAME TO pr")
            ro = base
        rows = [list(r) for r in db.execute("SELECT id, r FROM pr ORDER BY r DESC, id LIMIT 10")]
        for t in ("pr", "deg"):
            db.execute(f"DROP TABLE IF EXISTS temp.{t}")
        return rows, [[i, round(r, 9)] for i, r in rows], sql
    if kind == "wcc":
        n = total_nodes(db)
        persons = db.execute("SELECT count(*) FROM person").fetchone()[0]
        for t in ("comp", "nx"):
            db.execute(f"DROP TABLE IF EXISTS temp.{t}")
        db.execute("CREATE TEMP TABLE comp (id INTEGER PRIMARY KEY, c INTEGER)")
        db.execute("INSERT INTO comp SELECT id, id FROM person")
        for _ in range(200):
            db.execute("CREATE TEMP TABLE nx (id INTEGER PRIMARY KEY, c INTEGER)")
            db.execute("""INSERT INTO nx SELECT id, min(c) FROM (
                            SELECT id, c FROM comp
                            UNION ALL SELECT k.dst, c.c FROM knows k JOIN comp c ON c.id = k.src
                            UNION ALL SELECT k.src, c.c FROM knows k JOIN comp c ON c.id = k.dst) GROUP BY id""")
            changed = db.execute("SELECT count(*) FROM nx JOIN comp USING (id) WHERE nx.c <> comp.c").fetchone()[0]
            db.execute("DROP TABLE temp.comp")
            db.execute("ALTER TABLE temp.nx RENAME TO comp")
            if changed == 0:
                break
        k = db.execute("SELECT count(DISTINCT c) FROM comp").fetchone()[0] + (n - persons)
        db.execute("DROP TABLE IF EXISTS temp.comp")
        msg = f"{k} connected components"
        return [[msg]], msg, sql
    raise ValueError(kind)


# ------------------------------------------------------------------ main

def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default=",".join(SIZES))
    ap.add_argument("--only", default="build,reads,algos,writes")
    ap.add_argument("--q", default="")
    ap.add_argument("--rebuild", action="store_true")
    ap.add_argument("--warm", type=int, default=5)
    ap.add_argument("--slow", type=float, default=20.0)
    ap.add_argument("--timeout", type=float, default=900)
    ap.add_argument("--algo-timeout", type=float, default=1800)
    ap.add_argument("--mem-cap", type=float, default=12.0)
    ap.add_argument("--results", default=os.path.join(DATA, "results.jsonl"))
    ap.add_argument("--sqlite-worker")
    ap.add_argument("--sync", default="NORMAL")
    a = ap.parse_args()
    if a.sqlite_worker:
        return worker(a.sqlite_worker, a.sync)
    ARGS = sc.ARGS = scale.ARGS = a
    os.makedirs(DATA, exist_ok=True)
    os.environ["SQLITE_TMPDIR"] = DATA  # never /tmp: it is RAM here
    for size in a.sizes.split(","):
        for ph in a.only.split(","):
            log(f"== {ph} {size}")
            {"build": phase_build, "reads": phase_reads, "algos": phase_algos, "writes": phase_writes}[ph](size)


if __name__ == "__main__":
    main()
