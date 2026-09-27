#!/usr/bin/env python3
"""Scale benchmark for the paged engine: build, open, verify, every query
shape, the browsing API, algorithms (in memory and out of core), writes in
each sync mode, recovery after a crash, checkpoint, replication and
export/import — at each size, with the default 1 GiB page cache and with a
64 MiB one, to show that memory stays bounded while the data grows.

    python3 bench/paged_scale.py [--sizes 1GiB,5GiB] [--only build,open,...]

Datasets are built by scale-gen (--paged, a bulk load) into
../glider-bench-data/scale/pg-<size>.db, from the same generator and seed as the
snapshot-image and log files that bench/scale.py measured, so the numbers
line up with that run (../glider-bench-data/scale/results.jsonl). Results are appended
to ../glider-bench-data/scale/paged-results.jsonl; bench/paged_report.py renders them.

The query list, id layout and server harness are shared with scale.py.
"""

import argparse
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scale  # noqa: E402
from scale import GIB, Layout, Server, evict, fsize, log, queries  # noqa: E402

ROOT = scale.ROOT
DATA = scale.DATA
NEW = scale.NEW
GEN = os.path.join(ROOT, "target", "release", "scale-gen")
LOADGEN = os.path.join(ROOT, "target", "release", "loadgen")
RESULTS = os.path.join(DATA, "paged-results.jsonl")

CACHES = [("cache 1 GiB (default)", []), ("cache 64 MiB", ["--cache-size", "64M", "--work-mem", "64M"])]

ALGOS = [
    ("algo: degree top 5", "CALL degree(top: 5)"),
    ("algo: wcc", "CALL wcc(top: 3)"),
    ("algo: pagerank 5 iterations", "CALL pagerank(iterations: 5, top: 5)"),
    ("algo: kcore", "CALL kcore(top: 3)"),
    ("algo: scc", "CALL scc(top: 3)"),
    ("algo: labelprop 3 iterations", "CALL labelprop(iterations: 3, top: 3)"),
    ("algo: triangles", "CALL triangles(top: 3)"),
]

ARGS = None


def emit(rec):
    scale.emit(rec)


def run(cmd, timeout=None, stdin=None):
    return scale.run(cmd, timeout=timeout or ARGS.timeout, stdin=stdin)


def db_path(size):
    return os.path.join(DATA, f"pg-{size}.db")


def rm_db(path):
    for p in (path, path + ".lock"):
        if os.path.exists(p):
            os.remove(p)
    for d in (path + "-wal", path + "-data", path + "-tmp"):
        shutil.rmtree(d, ignore_errors=True)


def dir_bytes(path):
    total = fsize(path)
    for d in (path + "-data", path + "-wal"):
        if os.path.isdir(d):
            for f in os.listdir(d):
                total += fsize(os.path.join(d, f))
    return total


def evict_db(path):
    evict(path)
    d = path + "-data"
    if os.path.isdir(d):
        for f in os.listdir(d):
            evict(os.path.join(d, f))


def stats(path, extra=()):
    r = run([NEW, path, "--json", "-c", "STATS"] + list(extra))
    try:
        rows = json.loads(r["out"].strip().splitlines()[-1])["rows"]
        return {x[0]: x[1] for x in rows}
    except Exception:
        return {"error": r["out"][-300:]}


# ------------------------------------------------------------------ phases

def phase_build(size):
    out = db_path(size)
    if os.path.exists(out) and not ARGS.rebuild:
        return
    rm_db(out)
    evict_db(out)
    r = run([GEN, "--size", size, "--paged", "--work-mb", "256", "--cache-mb", "256", "--out", out], timeout=ARGS.timeout * 8)
    rec = {"phase": "build", "size": size, "method": "bulk load (scale-gen --paged)", "wall_s": r["wall_s"],
           "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0, "file_bytes": dir_bytes(out)}
    try:
        rec["gen"] = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        rec["error"] = r["out"][-300:]
    emit(rec)


def phase_insert(size):
    """The transactional path: every node and edge through add_node/add_edge."""
    out = os.path.join(DATA, f"pgi-{size}.db")
    rm_db(out)
    r = run([GEN, "--size", size, "--paged-insert", "--cache-mb", "256", "--out", out], timeout=ARGS.timeout * 8)
    emit({"phase": "build", "size": size, "method": "transactional inserts (scale-gen --paged-insert)",
          "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0,
          "file_bytes": dir_bytes(out), "error": None if r["exit"] == 0 else r["out"][-300:]})
    rm_db(out)


def phase_file(size):
    path = db_path(size)
    emit({"phase": "file", "size": size, "file_bytes": dir_bytes(path),
          "legacy_image_bytes": fsize(os.path.join(DATA, f"img-{size}.gldb")), "stats": stats(path)})


def phase_open(size):
    path = db_path(size)
    for name, extra in CACHES:
        for state in ("cold", "warm"):
            if state == "cold":
                evict_db(path)
            r = run([NEW, path, "-c", "STATS"] + extra)
            ok = r["exit"] == 0
            emit({"phase": "open", "size": size, "config": name, "cache": state, "wall_s": r["wall_s"],
                  "peak_rss_mb": r["peak_rss_mb"], "ok": ok, "error": None if ok else r["out"][-200:]})


def phase_verify(size):
    path = db_path(size)
    evict_db(path)
    r = run([NEW, path, "verify", "--cache-size", "64M"], timeout=ARGS.timeout * 4)
    emit({"phase": "verify", "size": size, "cache": "cold", "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0 and "integrity     ok" in r["out"],
          "throughput_mb_s": round(dir_bytes(path) / (1 << 20) / max(r["wall_s"], 1e-9), 1)})


def phase_queries(size, lay):
    path = db_path(size)
    qs = queries(lay) + ALGOS + [(l.replace("algo:", "algo (forced out of core):"),
                                  q.replace(")", ', tier: "ooc")') if "()" not in q else q.replace("()", '(tier: "ooc")'))
                                 for l, q in ALGOS[:3]]
    for name, extra in CACHES:
        if ARGS.only_config and ARGS.only_config not in name:
            continue
        evict_db(path)
        srv = Server(NEW, path, extra, port=ARGS.port)
        ok = srv.start(timeout=ARGS.timeout)
        emit({"phase": "server_start", "size": size, "config": name, "ok": ok,
              "wall_s": round(srv.start_s, 4) if ok else None, "rss_mb": srv.rss_mb() if ok else None})
        if not ok:
            srv.stop()
            continue
        for label, q in qs:
            algo = label.startswith("algo")
            # With a 64 MiB budget the whole-graph algorithms read the graph
            # from disk many times over; at 10 GiB and up keep the cheap ones.
            if algo and extra and size not in ("1GiB", "5GiB") and not any(
                    k in q for k in ("degree(", "wcc(", "pagerank(")):
                continue
            timeout = ARGS.query_timeout * (4 if algo else 1)
            rss0 = srv.rss_mb()
            t1, st, body = srv.query(q, timeout=timeout)
            first_ok = st == 200
            rows = None
            try:
                j = json.loads(body)
                rows = j.get("rows")
                if "error" in j:
                    first_ok = False
            except Exception:
                pass
            warm_times = []
            if first_ok and t1 < 60:
                for _ in range(3 if t1 < 10 else 1):
                    t, st2, _ = srv.query(q, timeout=timeout)
                    if st2 != 200:
                        break
                    warm_times.append(t)
            alive = srv.p.poll() is None
            emit({"phase": "query", "size": size, "config": name, "query": label, "text": q,
                  "first_s": round(t1, 5), "warm_s": round(statistics.median(warm_times), 5) if warm_times else None,
                  "ok": first_ok, "rss_before_mb": rss0, "rss_after_mb": srv.rss_mb() if alive else None,
                  "result": json.dumps(rows)[:300] if rows is not None else body.decode(errors="replace")[:300]})
            if not alive:
                log("server died; restarting")
                srv.stop()
                srv = Server(NEW, path, extra, port=ARGS.port)
                if not srv.start(timeout=ARGS.timeout):
                    break
        if srv.p.poll() is None:
            mid = lay.member("Person", 0.5)
            hub = lay.member("Person", 0.0)
            for label, p in [
                ("api: schema", "/api/schema"),
                ("api: stats", "/stats"),
                ("api: nodes page (label)", "/api/nodes?label=Person&limit=100"),
                ("api: nodes page (deep offset)", f"/api/nodes?label=Order&from={lay.member('Order', 0.9)}&limit=100"),
                ("api: nodes search (text)", "/api/nodes?q=Z%C3%BCrich&limit=50"),
                ("api: edges page (type)", "/api/edges?type=CITES&limit=100"),
                ("api: expand node", f"/api/expand?id={mid}&limit=200"),
                ("api: expand hub", f"/api/expand?id={hub}&limit=200"),
            ]:
                t1, st, body = srv.request("GET", p, timeout=ARGS.query_timeout)
                t2, st2, _ = srv.request("GET", p, timeout=ARGS.query_timeout)
                emit({"phase": "api", "size": size, "config": name, "query": label, "text": p,
                      "first_s": round(t1, 5), "warm_s": round(t2, 5), "ok": st == 200,
                      "rss_after_mb": srv.rss_mb(), "result": body.decode(errors="replace")[:160]})
        emit({"phase": "server_end", "size": size, "config": name, "rss_mb": srv.rss_mb(), "hwm_mb": srv.hwm_mb()})
        srv.stop()


def phase_load(size, lay):
    path = db_path(size)
    if not os.path.exists(LOADGEN):
        return
    qfile = os.path.join(DATA, f"load-{size}.txt")
    with open(qfile, "w") as f:
        for k in range(200):
            i = lay.member("Person", (k * 0.618) % 1.0)
            f.write(f'MATCH (p:Person {{email:"user{i:010d}@example.org"}})-[:KNOWS]->(f) RETURN count(f)\n')
    srv = Server(NEW, path, port=ARGS.port)
    if not srv.start():
        return
    for clients in (1, 4, 16):
        r = run([LOADGEN, "--addr", f"127.0.0.1:{ARGS.port}", "--clients", str(clients), "--duration", "10",
                 "--warmup", "2", "--query-file", qfile, "--json"], timeout=120)
        try:
            j = json.loads(r["out"].strip().splitlines()[-1])
        except Exception:
            j = {"raw": r["out"][-300:]}
        emit({"phase": "load", "size": size, "clients": clients, "result": j, "rss_mb": srv.rss_mb()})
    srv.stop()
    os.remove(qfile)


def phase_writes(size, lay):
    """On a copy-free basis: writes go into the database itself (and are
    counted by later phases' STATS)."""
    path = db_path(size)
    w = os.path.join(DATA, f"w-{size}.gql")
    for sync in ("off", "normal", "always"):
        cn = 200 if sync == "always" else 2000
        open(w, "w").write("".join(f'CREATE (:Bench {{i:{k}, sync:"{sync}"}});\n' for k in range(cn)))
        base = run([NEW, path, "-c", "STATS"])
        r = run([NEW, path, "--sync", sync, "-f", w])
        emit({"phase": "write", "size": size, "op": f"CREATE node, autocommit, sync {sync}", "n": cn,
              "wall_s": r["wall_s"], "per_op_ms": round(max(r["wall_s"] - base["wall_s"], 0) / cn * 1000, 4),
              "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0})
    ops = []
    for k in range(200):
        i = lay.member("Person", (k * 0.37) % 1.0)
        ops.append(("SET property", f'MATCH (p:Person {{email:"user{i:010d}@example.org"}}) SET p.score = {k}.5;'))
    for k in range(200):
        a = lay.member("Person", (k * 0.11) % 1.0)
        b = lay.member("Person", (k * 0.29) % 1.0)
        ops.append(("CREATE edge", f'MATCH (a:Person {{email:"user{a:010d}@example.org"}}),(b:Person {{email:"user{b:010d}@example.org"}}) CREATE (a)-[:KNOWS {{since:2026}}]->(b);'))
    for k in range(50):
        i = lay.member("Person", 0.2 + k * 0.013)
        ops.append(("DETACH DELETE node", f'MATCH (p:Person {{email:"user{i:010d}@example.org"}}) DETACH DELETE p;'))
    ops.append(("bulk SET over a label (spooled)", "MATCH (c:Category) SET c.touched = true;"))
    for op in dict.fromkeys(o for o, _ in ops):
        stmts = [q for o, q in ops if o == op]
        open(w, "w").write("\n".join(stmts) + "\n")
        base = run([NEW, path, "-c", "STATS"])
        r = run([NEW, path, "--sync", "normal", "-f", w])
        emit({"phase": "write", "size": size, "op": op, "n": len(stmts), "wall_s": r["wall_s"],
              "per_op_ms": round(max(r["wall_s"] - base["wall_s"], 0) / len(stmts) * 1000, 3),
              "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0, "error": None if r["exit"] == 0 else r["out"][-200:]})
    os.remove(w)


def phase_recovery(size):
    """Kill -9 a writer mid-stream; reopen must replay and agree."""
    path = db_path(size)
    w = os.path.join(DATA, f"crash-{size}.gql")
    open(w, "w").write("".join(f'CREATE (:Crash {{i:{k}}});\n' for k in range(200000)))
    before = stats(path).get("nodes")
    p = subprocess.Popen([NEW, path, "--checkpoint", "off", "-f", w], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)
    p.send_signal(signal.SIGKILL)
    p.wait()
    lock = path + ".lock"
    evict_db(path)
    log_bytes = sum(fsize(os.path.join(path + "-wal", f)) for f in os.listdir(path + "-wal"))
    r = run([NEW, path, "--force", "--json", "-c", "MATCH (c:Crash) RETURN count(c)"])
    n = None
    try:
        n = json.loads(r["out"].strip().splitlines()[-1])["rows"][0][0]
    except Exception:
        pass
    v = run([NEW, path, "verify", "--cache-size", "64M"], timeout=ARGS.timeout * 4)
    emit({"phase": "recovery", "size": size, "log_bytes": log_bytes, "wall_s": r["wall_s"],
          "peak_rss_mb": r["peak_rss_mb"], "recovered_nodes": n, "nodes_before": before,
          "ok": r["exit"] == 0 and n is not None and n > 0, "verify_ok": v["exit"] == 0 and "integrity     ok" in v["out"],
          "stale_lock_left": os.path.exists(lock)})
    run([NEW, path, "-c", "MATCH (c:Crash) DETACH DELETE c"])
    os.remove(w)


def phase_checkpoint(size):
    path = db_path(size)
    w = os.path.join(DATA, f"ck-{size}.gql")
    open(w, "w").write("".join(f'CREATE (:Ck {{i:{k}}});\n' for k in range(20000)))
    run([NEW, path, "--checkpoint", "off", "--sync", "off", "-f", w])
    os.remove(w)
    log_bytes = sum(fsize(os.path.join(path + "-wal", f)) for f in os.listdir(path + "-wal"))
    # COMPACT is a checkpoint on a paged database.
    r = run([NEW, path, "compact"])
    emit({"phase": "checkpoint", "size": size, "log_bytes": log_bytes, "wall_s": r["wall_s"],
          "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0})
    run([NEW, path, "-c", "MATCH (c:Ck) DETACH DELETE c"])


def phase_replicate(size):
    path = db_path(size)
    rep = os.path.join(DATA, f"preplica-{size}")
    out = os.path.join(DATA, f"prestored-{size}.db")
    shutil.rmtree(rep, ignore_errors=True)
    rm_db(out)
    evict_db(path)
    r = run([NEW, path, "wal", "tail", "--to", rep, "--once", "--quiet"], timeout=ARGS.timeout * 4)
    emit({"phase": "replicate_base", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "throughput_mb_s": round(dir_bytes(path) / (1 << 20) / max(r["wall_s"], 1e-9), 1),
          "error": None if r["exit"] == 0 else r["out"][-200:]})
    # Log shipping: some writes, then one more pass.
    w = os.path.join(DATA, f"rw-{size}.gql")
    open(w, "w").write("".join(f'CREATE (:Rep {{i:{k}}});\n' for k in range(5000)))
    run([NEW, path, "-f", w])
    os.remove(w)
    r = run([NEW, path, "wal", "tail", "--to", rep, "--once", "--quiet"])
    emit({"phase": "replicate_log", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0})
    r = run([NEW, "wal", "restore", "--from", rep, "--to", out], timeout=ARGS.timeout * 4)
    a = stats(path)
    b = stats(out) if r["exit"] == 0 else {}
    same = r["exit"] == 0 and a.get("nodes") == b.get("nodes") and a.get("edges") == b.get("edges")
    emit({"phase": "replicate_restore", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "same_counts": same, "error": None if r["exit"] == 0 else r["out"][-300:]})
    run([NEW, path, "-c", "MATCH (c:Rep) DETACH DELETE c"])
    shutil.rmtree(rep, ignore_errors=True)
    rm_db(out)


def phase_export(size):
    path = db_path(size)
    out = os.path.join(DATA, f"pexport-{size}.jsonl")
    db2 = os.path.join(DATA, f"preimport-{size}.db")
    rm_db(db2)
    r = run([NEW, path, "export", out])
    emit({"phase": "export", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "jsonl_bytes": fsize(out), "error": None if r["exit"] == 0 else r["out"][-200:]})
    if r["exit"] == 0:
        r = run([NEW, db2, "import", out])
        emit({"phase": "import", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
              "ok": r["exit"] == 0, "db_bytes": dir_bytes(db2), "error": None if r["exit"] == 0 else r["out"][-200:]})
        a, b = stats(path), stats(db2)
        emit({"phase": "roundtrip_check", "size": size,
              "same_counts": a.get("nodes") == b.get("nodes") and a.get("edges") == b.get("edges")})
    if os.path.exists(out):
        os.remove(out)
    rm_db(db2)


def phase_memory(size):
    """`:memory:` with a limit: fills to it and reports Full cleanly."""
    pb = os.path.join(ROOT, "target", "release", "pagebench")
    r = run([pb, "memfill", "--max-mb", "256"], timeout=600)
    try:
        j = json.loads(r["out"].strip().splitlines()[-1])
    except Exception:
        j = {"raw": r["out"][-300:]}
    emit({"phase": "memory_limit", "size": "n/a", "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "result": j})


def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default="1GiB,5GiB,10GiB,25GiB")
    ap.add_argument("--only", default="build,file,open,verify,queries,load,writes,recovery,checkpoint,replicate,export,insert,memory")
    ap.add_argument("--only-config", default=None)
    ap.add_argument("--mem-cap", type=float, default=16.0)
    ap.add_argument("--timeout", type=float, default=1800)
    ap.add_argument("--query-timeout", type=float, default=900)
    ap.add_argument("--results", default=RESULTS)
    ap.add_argument("--port", type=int, default=7981)
    ap.add_argument("--rebuild", action="store_true")
    ARGS = ap.parse_args()
    scale.ARGS = ARGS
    phases = ARGS.only.split(",")
    info = scale.gen_info()
    if "memory" in phases:
        phase_memory("n/a")
    for size in ARGS.sizes.split(","):
        gi = info.get(f"img-{size}.gldb", {})
        lay = Layout(gi.get("ids", 0))
        log(f"=== paged {size}")
        if "build" in phases:
            phase_build(size)
        if not os.path.exists(db_path(size)):
            log("missing", db_path(size))
            continue
        if "insert" in phases and size == "1GiB":
            phase_insert(size)
        if "file" in phases:
            phase_file(size)
        if "open" in phases:
            phase_open(size)
        if "verify" in phases:
            phase_verify(size)
        if "queries" in phases:
            phase_queries(size, lay)
        if "load" in phases:
            phase_load(size, lay)
        if "writes" in phases:
            phase_writes(size, lay)
        if "recovery" in phases:
            phase_recovery(size)
        if "checkpoint" in phases:
            phase_checkpoint(size)
        if "replicate" in phases:
            phase_replicate(size)
        if "export" in phases and size in ("1GiB",):
            phase_export(size)


if __name__ == "__main__":
    main()
