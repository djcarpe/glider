#!/usr/bin/env python3
"""The demo benchmark: the scale suite's queries against one or more served
graphs, over HTTP, with the server's own counters beside every query.

    bench.py --target 1g=http://glider-1g:7878 --gen-log 1g=/data/gen-1g.log \
             --target 5g=http://glider-5g:7878 --gen-log 5g=/data/gen-5g.log \
             --out /data/results/bench.json [--algos] [--limit 768Mi]

Every query runs once "first" (the server has answered nothing yet, so this
is the cold number) and then up to three more times; "warm" is their median.
Around the first run the server's /metrics are read, so each row carries the
page reads and cache misses that query cost and the process's resident
memory afterwards. A table goes to stdout; the full record to --out.

Only the standard library, like everything else here. `scale.py` beside this
file supplies the id layout and the query list, so the queries are exactly
the ones in bench/PAGED.md and docs/BENCHMARKS.md.
"""
import argparse
import http.client
import json
import os
import re
import statistics
import sys
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
# beside this file in the image; in the repo it is bench/scale.py
sys.path[:0] = [HERE, os.path.join(HERE, "..", "..", "bench")]
import scale  # noqa: E402


def log(msg):
    print(msg, flush=True)


# ------------------------------------------------------------------ http

def call(url, method, path, body=None, timeout=60):
    u = urllib.parse.urlsplit(url)
    conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=timeout)
    try:
        conn.request(method, path, body=body.encode() if body else None,
                     headers={"Content-Type": "text/plain"})
        r = conn.getresponse()
        return r.status, r.read()
    finally:
        conn.close()


def wait_ready(name, url, deadline_s):
    t0 = time.time()
    last = None
    while time.time() - t0 < deadline_s:
        try:
            status, body = call(url, "GET", "/health", timeout=5)
            if status == 200:
                log(f"[{name}] ready at {url}")
                return
            last = f"{status} {body[:80]!r}"
        except OSError as e:
            last = str(e)
        time.sleep(5)
    raise SystemExit(f"[{name}] not ready after {deadline_s}s: {last}")


LINE = re.compile(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+(\S+)')


def metrics(url):
    """/metrics as {name: value}; a name with several label sets is summed.
    Empty when the server cannot be reached: a dead target is reported in
    its rows, not by ending the run."""
    try:
        status, body = call(url, "GET", "/metrics", timeout=30)
    except OSError:
        return {}
    if status != 200:
        return {}
    out = {}
    for line in body.decode(errors="replace").splitlines():
        if line.startswith("#"):
            continue
        m = LINE.match(line)
        if not m:
            continue
        try:
            v = float(m.group(3))
        except ValueError:
            continue
        out[m.group(1)] = out.get(m.group(1), 0.0) + v
    return out


UNIT = r"(_(bytes|pages|seconds|total|By|node|relationship))*"


def find(m, base):
    """The metric named `base` whatever unit suffix the renderer appended."""
    rx = re.compile("^" + re.escape(base) + UNIT + "$")
    for k, v in m.items():
        if rx.match(k):
            return v
    return None


def mb(v):
    return None if v is None else round(v / (1024 * 1024), 1)


# ----------------------------------------------------------------- query

def run(url, src, timeout):
    t = time.perf_counter()
    try:
        status, body = call(url, "POST", "/query", body=src, timeout=timeout)
    except OSError as e:
        return (time.perf_counter() - t) * 1000, None, f"{type(e).__name__}: {e}"
    dt = (time.perf_counter() - t) * 1000
    try:
        j = json.loads(body)
    except ValueError:
        return dt, None, f"HTTP {status}: {body[:120]!r}"
    if status != 200 or "error" in j:
        return dt, None, j.get("error", f"HTTP {status}")
    return dt, j, None


def stats(url):
    try:
        status, body = call(url, "GET", "/stats", timeout=60)
        j = json.loads(body)
        return {str(r[0]): r[1] for r in j.get("rows", []) if len(r) >= 2}
    except (OSError, ValueError, TypeError):
        return {}


# The whole-graph algorithms of bench/paged_scale.py.
ALGOS = [
    ("algo: degree top 5", "CALL degree(top: 5)"),
    ("algo: wcc", "CALL wcc(top: 3)"),
    ("algo: pagerank 5 iterations", "CALL pagerank(iterations: 5, top: 5)"),
    ("algo: kcore", "CALL kcore(top: 3)"),
    ("algo: scc", "CALL scc(top: 3)"),
    ("algo: labelprop 3 iterations", "CALL labelprop(iterations: 3, top: 3)"),
    ("algo: triangles", "CALL triangles(top: 3)"),
]


def api_requests(lay):
    """The browsing API requests of bench/paged_scale.py."""
    mid = lay.member("Person", 0.5)
    hub = lay.member("Person", 0.0)
    return [
        ("api: schema", "/api/schema"),
        ("api: stats", "/stats"),
        ("api: nodes page (label)", "/api/nodes?label=Person&limit=100"),
        ("api: nodes page (deep offset)", f"/api/nodes?label=Order&from={lay.member('Order', 0.9)}&limit=100"),
        ("api: nodes search (text)", "/api/nodes?q=Z%C3%BCrich&limit=50"),
        ("api: edges page (type)", "/api/edges?type=CITES&limit=100"),
        ("api: expand node", f"/api/expand?id={mid}&limit=200"),
        ("api: expand hub", f"/api/expand?id={hub}&limit=200"),
    ]


def write_groups(tag):
    """The write benchmarks of bench/suite.py, against the live graph. Each
    group is (name, in one transaction?, statements). Unlike the suite,
    edges, updates and deletes work on the nodes this run inserted, so the
    graph is left as it was found; the fsync-every-commit case needs a
    server started with --sync always and is not run here."""
    w3 = lambda i: f"w3-{tag}-{i}@x.org"
    return [
        ("insert a node, autocommit (x1000)", False,
         [f'CREATE (:Person {{email: "w1-{tag}-{i}@x.org", name: "Bench {i}", age: {i % 90}, country: "NZ"}})'
          for i in range(1000)]),
        ("bulk insert 50,000 nodes, one transaction", True,
         [f'CREATE (:Person {{email: "{w3(i)}", name: "Bulk {i}", age: {i % 90}, country: "AU"}})'
          for i in range(50_000)]),
        ("insert 5,000 edges between indexed nodes, one transaction", True,
         [f'MATCH (a:Person {{email: "{w3(i)}"}}), (b:Person {{email: "{w3((i * 7919) % 50_000)}"}}) '
          f'CREATE (a)-[:KNOWS {{since: 2026, weight: 0.5}}]->(b)' for i in range(5000)]),
        ("update 10,000 properties by index, one transaction", True,
         [f'MATCH (p:Person {{email: "{w3(i % 50_000)}"}}) SET p.age = {(i * 31) % 72 + 18}' for i in range(10_000)]),
        ("delete 500 nodes with their edges, one transaction", True,
         [f'MATCH (p:Person {{email: "{w3(i)}"}}) DETACH DELETE p' for i in range(500)]),
    ]


CLEANUP = [
    'MATCH (p:Person) WHERE p.email STARTS WITH "w1-" DETACH DELETE p',
    'MATCH (p:Person) WHERE p.email STARTS WITH "w3-" DETACH DELETE p',
]


def bench_api(name, url, lay, args):
    rows = []
    for label, path in api_requests(lay):
        times, err, status, size = [], None, None, 0
        for _ in range(4):
            t = time.perf_counter()
            try:
                status, body = call(url, "GET", path, timeout=args.timeout)
            except OSError as e:
                err = f"{type(e).__name__}: {e}"
                break
            times.append((time.perf_counter() - t) * 1000)
            size = len(body)
            if status != 200:
                err = f"HTTP {status}"
                break
        first = times[0] if times else None
        warm = statistics.median(times[1:]) if len(times) > 1 else None
        rows.append({"name": label, "path": path, "first_ms": None if first is None else round(first, 1),
                     "warm_ms": None if warm is None else round(warm, 1), "bytes": size, "error": err})
        log(f"[{name}] {label:48s} first {first or 0:9.1f} ms  warm {warm or 0:9.1f} ms  {size:7d} B"
            + (f"  ERROR {err}" if err else ""))
    return rows


def bench_writes(name, url, args):
    rows = []
    m0 = metrics(url)
    for label, txn, stmts in write_groups(name):
        t0 = time.perf_counter()
        err = None
        if txn:
            _, _, err = run(url, "BEGIN", args.timeout)
        for src in stmts:
            if err:
                break
            _, _, err = run(url, src, args.timeout)
        if txn:
            _, _, e = run(url, "COMMIT" if not err else "ROLLBACK", args.timeout)
            err = err or e
        total = time.perf_counter() - t0
        if err:
            settle(name, url, args)
        m1 = metrics(url)
        rec = {"name": label, "ops": len(stmts), "transaction": txn, "ms": round(total * 1000, 1),
               "ops_per_s": None if err else round(len(stmts) / total),
               "page_writes": (find(m1, "glider_db_io_writes") or 0) - (find(m0, "glider_db_io_writes") or 0),
               "rss_mb": mb(find(m1, "glider_process_memory")), "error": err}
        m0 = m1
        rows.append(rec)
        log(f"[{name}] {label:60s} {rec['ms']:10.1f} ms  {rec['ops_per_s'] or '-':>7} ops/s  rss {rec['rss_mb']} MB"
            + (f"  ERROR {err}" if err else ""))
    for src in CLEANUP:
        _, j, err = run(url, src, args.timeout)
        log(f"[{name}] cleanup: {src[:50]}… touched {j.get('touched') if j else '-'}" + (f" ERROR {err}" if err else ""))
    return rows


def settle(name, url, args):
    """After a statement failed, make sure the server is answering again
    before the next one is timed. A statement that timed out on our side is
    still running on the server (one mutex around the graph), and a server
    that was killed comes back after a restart; either way the next
    measurement must not start until STATS answers. Returns seconds waited."""
    t0 = time.time()
    while time.time() - t0 < args.wait:
        try:
            status, _ = call(url, "POST", "/query", body="STATS", timeout=10)
            if status == 200:
                waited = time.time() - t0
                if waited > 1:
                    log(f"[{name}] server answering again after {waited:.0f}s")
                return waited
        except OSError:
            pass
        time.sleep(5)
    raise SystemExit(f"[{name}] server did not answer within {args.wait}s")


def unreached(name, src, err, first_ms):
    return {"name": name, "cypher": src, "first_ms": None if first_ms is None else round(first_ms, 1),
            "warm_ms": None, "rows": None, "row0": None, "touched": None, "error": err, "page_reads": 0,
            "cache_misses": 0, "cache_hits": 0, "resident_pages": None, "rss_mb": None, "peak_rss_mb": None}


def ids_from_log(path):
    txt = open(path).read()
    m = re.search(r'"ids":\s*(\d+)', txt)
    if not m:
        raise SystemExit(f"{path}: no scale-gen summary with \"ids\" in it")
    return int(m.group(1))


def bench_target(name, url, ids, args):
    wait_ready(name, url, args.wait)
    st = stats(url)
    lay = scale.Layout(ids)
    qs = list(scale.queries(lay))
    if args.algos:
        qs += ALGOS
    m0 = metrics(url)
    rows = []
    peak = find(m0, "glider_process_memory_peak")
    down = 0
    for qname, src in qs:
        before = metrics(url)
        first, j, err = run(url, src, args.timeout)
        after = metrics(url)
        # settle() waited for the server, so a statement that still cannot
        # reach it means it is not coming back; stop and record the rest.
        if err and err.startswith(("ConnectionRefusedError", "ConnectionResetError", "BrokenPipeError", "RemoteDisconnected")):
            down += 1
        else:
            down = 0
        if down >= 3:
            log(f"[{name}] server unreachable; recording the remaining queries as unreached")
            for rest, rsrc in qs[len(rows):]:
                rows.append(unreached(rest, rsrc, "server unreachable", None))
            break
        warm = []
        if not err and first < 10_000:
            for _ in range(3):
                dt, _, e = run(url, src, args.timeout)
                if e:
                    err = e
                    break
                warm.append(dt)
        elif not err and first < 120_000:
            dt, _, e = run(url, src, args.timeout)
            if not e:
                warm.append(dt)
        # past two minutes (an algorithm out of core) a repeat costs as much
        # as the first run and tells nothing new
        restarted = False
        settled = 0.0
        if err:
            # Was the server killed? A restart resets its peak-memory gauge.
            settled = settle(name, url, args)
            after = metrics(url)
            hwm_now = find(after, "glider_process_memory_peak")
            restarted = hwm_now is not None and peak is not None and hwm_now < peak
            if restarted:
                log(f"[{name}] the server restarted during this statement")
        rss = find(after, "glider_process_memory")
        hwm = find(after, "glider_process_memory_peak")
        peak = max(peak or 0, hwm or 0) or None
        rec = {
            "name": qname,
            "cypher": src,
            "first_ms": round(first, 1),
            "warm_ms": round(statistics.median(warm), 1) if warm else None,
            "rows": len(j["rows"]) if j else None,
            "row0": (json.dumps(j["rows"][0])[:200] if j and j.get("rows") else None),
            "touched": j.get("touched") if j else None,
            "error": err,
            "page_reads": (find(after, "glider_db_io_reads") or 0) - (find(before, "glider_db_io_reads") or 0),
            "cache_misses": (find(after, "glider_db_cache_misses") or 0) - (find(before, "glider_db_cache_misses") or 0),
            "cache_hits": (find(after, "glider_db_cache_hits") or 0) - (find(before, "glider_db_cache_hits") or 0),
            "resident_pages": find(after, "glider_db_cache_resident"),
            "rss_mb": mb(rss),
            "peak_rss_mb": mb(hwm),
            "server_restarted": restarted,
            "settle_s": round(settled, 1),
        }
        rows.append(rec)
        w = f"{rec['warm_ms']:.1f}" if rec["warm_ms"] is not None else "-"
        log(f"[{name}] {qname:48s} first {first:9.1f} ms  warm {w:>9s} ms  "
            f"reads {int(rec['page_reads']):8d}  rss {rec['rss_mb']} MB"
            + (f"  ERROR {err}" if err else ""))
    api = bench_api(name, url, lay, args) if args.api else []
    writes = bench_writes(name, url, args) if args.writes else []
    mend = metrics(url)
    peak = max(peak or 0, find(mend, "glider_process_memory_peak") or 0) or None
    return {
        "url": url,
        "ids": ids,
        "stats": st,
        "db_bytes": find(mend, "glider_db_size"),
        "nodes": find(mend, "glider_db_nodes"),
        "edges": find(mend, "glider_db_edges"),
        "peak_rss_mb": mb(peak),
        "results": rows,
        "api": api,
        "writes": writes,
    }


# ----------------------------------------------------------------- report

def fmt_ms(v):
    if v is None:
        return "-"
    if v >= 60_000:
        return f"{v / 60_000:.1f} min"
    if v >= 1000:
        return f"{v / 1000:.2f} s"
    return f"{v:.1f} ms"


def report(out):
    names = list(out["targets"])
    log("")
    log(f"## glider on k3s: {', '.join(names)} — pod memory limit {out['limit']}")
    log("")
    log("| graph | nodes | edges | file | peak RSS |")
    log("|---|---|---|---|---|")
    for n in names:
        t = out["targets"][n]
        log(f"| {n} | {int(t['nodes'] or 0):,} | {int(t['edges'] or 0):,} | "
            f"{(t['db_bytes'] or 0) / 2**30:.2f} GiB | {t['peak_rss_mb']} MB |")
    log("")
    head = "| query (first / warm) | " + " | ".join(names) + " |"
    log(head)
    log("|---|" + "---|" * len(names))
    first = out["targets"][names[0]]["results"]
    for i, r in enumerate(first):
        cells = []
        for n in names:
            rs = out["targets"][n]["results"]
            x = rs[i] if i < len(rs) else None
            if x is None:
                cells.append("-")
            elif x["error"]:
                cells.append(f"**failed** ({x['error'][:40]})")
            else:
                cells.append(f"{fmt_ms(x['first_ms'])} / {fmt_ms(x['warm_ms'])}")
        log(f"| {r['name']} | " + " | ".join(cells) + " |")
    log("")
    if any(out["targets"][n]["api"] for n in names):
        log("| request (first / warm) | " + " | ".join(names) + " |")
        log("|---|" + "---|" * len(names))
        first = out["targets"][names[0]]["api"]
        for i, r in enumerate(first):
            cells = []
            for n in names:
                rs = out["targets"][n]["api"]
                x = rs[i] if i < len(rs) else None
                cells.append("-" if x is None else f"**failed** ({x['error']})" if x["error"]
                             else f"{fmt_ms(x['first_ms'])} / {fmt_ms(x['warm_ms'])}")
            log(f"| {r['name']} | " + " | ".join(cells) + " |")
        log("")
    if any(out["targets"][n]["writes"] for n in names):
        log("| writes (time, ops/s) | " + " | ".join(names) + " |")
        log("|---|" + "---|" * len(names))
        first = out["targets"][names[0]]["writes"]
        for i, r in enumerate(first):
            cells = []
            for n in names:
                rs = out["targets"][n]["writes"]
                x = rs[i] if i < len(rs) else None
                cells.append("-" if x is None else f"**failed** ({x['error'][:40]})" if x["error"]
                             else f"{fmt_ms(x['ms'])}, {x['ops_per_s']:,} ops/s")
            log(f"| {r['name']} | " + " | ".join(cells) + " |")
        log("")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target", action="append", default=[], help="name=url", required=True)
    ap.add_argument("--gen-log", action="append", default=[], help="name=path of the scale-gen log")
    ap.add_argument("--ids", action="append", default=[], help="name=N: the id count, instead of a gen log")
    ap.add_argument("--out", help="write the full record here (JSON)")
    ap.add_argument("--algos", action="store_true", help="also run the whole-graph algorithms")
    ap.add_argument("--api", action="store_true", help="also time the browsing API requests")
    ap.add_argument("--writes", action="store_true", help="also run the write benchmarks (last; cleans up after itself)")
    ap.add_argument("--timeout", type=float, default=1800, help="seconds per statement")
    ap.add_argument("--wait", type=float, default=7200, help="seconds to wait for a server to be ready")
    ap.add_argument("--limit", default=os.environ.get("POD_MEMORY_LIMIT", "?"), help="the pod memory limit, for the report")
    args = ap.parse_args()

    kv = lambda items: dict(x.split("=", 1) for x in items)
    targets, logs, ids = kv(args.target), kv(args.gen_log), kv(args.ids)
    out = {"started": time.strftime("%Y-%m-%dT%H:%M:%S"), "limit": args.limit, "targets": {}}
    for name, url in targets.items():
        n = int(ids[name]) if name in ids else ids_from_log(logs[name])
        out["targets"][name] = bench_target(name, url, n, args)
        if args.out:
            os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
            with open(args.out, "w") as f:
                json.dump(out, f, indent=1)
    out["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    if args.out:
        with open(args.out, "w") as f:
            json.dump(out, f, indent=1)
        log(f"wrote {args.out}")
    report(out)


if __name__ == "__main__":
    main()
