#!/usr/bin/env python3
"""Scale benchmark for glider: open, query, browse, write, compact, verify,
replicate and export, at every size in ../glider-bench-data/scale, against the new
build and (where it can run at all) the build before snapshot images.

    python3 bench/scale.py [--sizes 1GiB,5GiB] [--only open,queries,...]

Datasets come from ../glider-bench-data/scale/gen.sh (scale-gen). Results are appended
to ../glider-bench-data/scale/results.jsonl, one JSON object per measurement, so a
partial run is still usable; bench/scale_report.py turns them into a report.

Method notes, so the numbers can be read correctly:

* Every glider process runs under an address-space cap (RLIMIT_AS,
  --mem-cap, default 16 GiB) so a configuration that cannot fit fails fast
  and is recorded as such, instead of pushing the machine into swap.
* "cold" means the database file was evicted from the page cache first
  (posix_fadvise DONTNEED; no root needed). "warm" means it was just read.
* Query latency is measured through `glider <db> serve`, one HTTP request
  per query: "first" is the first run in a fresh server (it pays for loading
  whatever parts of the image the query touches), "warm" is the median of
  the next three. RSS is sampled from /proc after each query.
"""

import argparse
import http.client
import json
import os
import resource
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# Datasets live outside the repo: Rustler (glider_ex) digests every file under
# a path-dependency crate on each compile, so multi-GB files here OOM it.
BENCH_DATA = os.environ.get("GLIDER_BENCH_DATA", os.path.join(os.path.dirname(ROOT), "glider-bench-data"))
DATA = os.path.join(BENCH_DATA, "scale")
NEW = os.path.join(ROOT, "target", "release", "glider")
OLD = os.path.join(DATA, "glider-old")
RESULTS = os.path.join(DATA, "results.jsonl")
SIZES = ["1GiB", "5GiB", "10GiB", "25GiB"]
GIB = 1 << 30

ARGS = None


def log(*a):
    print(time.strftime("[%H:%M:%S]"), *a, file=sys.stderr, flush=True)


def emit(rec):
    rec["t"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(ARGS.results, "a") as f:
        f.write(json.dumps(rec) + "\n")
    brief = {k: v for k, v in rec.items() if k not in ("t", "stdout")}
    log("  ", json.dumps(brief)[:240])


def cap():
    lim = int(ARGS.mem_cap * GIB)
    resource.setrlimit(resource.RLIMIT_AS, (lim, lim))


def run(cmd, timeout=1800, stdin=None):
    """Run a command. Returns dict(wall_s, peak_rss_mb, exit, out, timeout)."""
    t = time.time()
    p = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
        preexec_fn=cap,
    )
    if stdin is not None:
        p.stdin.write(stdin.encode())
        p.stdin.close()
    out = []
    import threading

    def drain(s, into):
        into.append(s.read())

    th = [threading.Thread(target=drain, args=(p.stdout, out)), threading.Thread(target=drain, args=(p.stderr, out))]
    for x in th:
        x.start()
    timed_out = False
    while True:
        pid, status, ru = os.wait4(p.pid, os.WNOHANG)
        if pid:
            break
        if time.time() - t > timeout:
            p.kill()
            pid, status, ru = os.wait4(p.pid, 0)
            timed_out = True
            break
        time.sleep(0.002)
    for x in th:
        x.join()
    code = os.waitstatus_to_exitcode(status)
    text = b"".join(out).decode(errors="replace")
    return {
        "wall_s": round(time.time() - t, 4),
        "peak_rss_mb": round(ru.ru_maxrss / 1024, 1),
        "exit": code,
        "timeout": timed_out,
        "out": text,
    }


def evict(path):
    """Drop a file's pages from the page cache (clean pages; no root)."""
    try:
        fd = os.open(path, os.O_RDONLY)
        os.fsync(fd)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        os.close(fd)
    except OSError as e:
        log("evict failed", path, e)


def warm(path):
    with open(path, "rb") as f:
        while f.read(8 << 20):
            pass


def fsize(path):
    return os.path.getsize(path) if os.path.exists(path) else 0


def rm_db(path):
    for p in (path, path + ".lock", path + ".compact.tmp"):
        if os.path.exists(p):
            os.remove(p)


# ------------------------------------------------------------------ layout
# Mirrors scale-gen's id layout, to pick ids that exist without asking.

M64 = (1 << 64) - 1


def mix(x):
    x = (x + 0x9E3779B97F4A7C15) & M64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & M64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & M64
    return x ^ (x >> 31)


KINDS = [("Person", 0.40), ("Company", 0.015), ("Product", 0.08), ("Order", 0.25),
         ("Document", 0.13), ("Category", 0.005), ("Event", 0.11)]


class Layout:
    def __init__(self, n, seed=42):
        self.n = max(n, 1000)
        self.seed = seed
        self.ranges = {}
        start = 1
        for i, (k, frac) in enumerate(KINDS):
            ln = self.n + 1 - start if i == len(KINDS) - 1 else max(int(self.n * frac), 20)
            self.ranges[k] = (start, start + ln)
            start += ln

    def exists(self, i):
        if i == 0 or i > self.n:
            return False
        if i == self.n or any(s == i for s, _ in self.ranges.values()):
            return True
        return mix(self.seed ^ ((i * 0xA24BAED4963EE407) & M64)) % 97 != 0

    def member(self, kind, f):
        s, e = self.ranges[kind]
        i = s + min(int((e - s) * f), e - s - 1)
        while not self.exists(i):
            i = i + 1 if i + 1 < e else s
        return i


def queries(lay):
    person_mid = lay.member("Person", 0.5)
    hub = lay.member("Person", 0.0)
    buyer = lay.member("Person", 0.0003)
    order = lay.member("Order", 0.5)
    product = lay.member("Product", 0.5)
    event = lay.member("Event", 0.5)
    doc = lay.member("Document", 0.5)
    em = lambda i: f"user{i:010d}@example.org"
    return [
        ("point: indexed text (Person.email)", f'MATCH (p:Person {{email:"{em(person_mid)}"}}) RETURN p.name, p.age, p.country'),
        ("point: indexed text (Order.ref)", f'MATCH (o:Order {{ref:"ORD-{order:010d}"}}) RETURN o.total, o.status'),
        ("point: unindexed, by id()", f"MATCH (d:Document) WHERE id(d) = {doc} RETURN d.title"),
        ("1-hop out: community (KNOWS)", f'MATCH (p:Person {{email:"{em(person_mid)}"}})-[:KNOWS]->(f) RETURN count(f), avg(f.age)'),
        ("1-hop in: power-law hub (KNOWS)", f'MATCH (h:Person {{email:"{em(hub)}"}})<-[:KNOWS]-(f) RETURN count(f)'),
        ("2-hop: friends of friends", f'MATCH (p:Person {{email:"{em(person_mid)}"}})-[:KNOWS]->()-[:KNOWS]->(x) RETURN count(x)'),
        ("2-hop bipartite: buyer -> orders -> products", f'MATCH (p:Person {{email:"{em(buyer)}"}})-[:PLACED]->(o)-[:CONTAINS]->(pr) RETURN count(pr)'),
        ("multi-edges: order lines", f'MATCH (o:Order {{ref:"ORD-{order:010d}"}})-[c:CONTAINS]->(pr) RETURN pr.sku, count(c) ORDER BY pr.sku'),
        ("dense clique: SIMILAR 2-hop", f'MATCH (p:Product {{sku:"SKU-{product:010d}"}})-[:SIMILAR]->()-[:SIMILAR]->(r) RETURN count(r)'),
        ("tree: category ancestors (var-length)", f'MATCH (p:Product {{sku:"SKU-{product:010d}"}})-[:IN_CATEGORY]->(c)<-[:PARENT_OF*1..10]-(a) RETURN count(a)'),
        ("chain: NEXT*1..200", f"MATCH (e:Event {{seq:{event}}})-[:NEXT*1..200]->(x) RETURN count(x)"),
        ("DAG: CITES*1..3 from a document", f"MATCH (d:Document)-[:CITES*1..3]->(x) WHERE id(d) = {doc} RETURN count(x)"),
        ("heterogeneous: MENTIONS by label", f"MATCH (d:Document)-[:MENTIONS]->(x) WHERE id(d) = {doc} RETURN labels(x), count(x)"),
        ("index, low cardinality (country)", 'MATCH (p:Person {country:"JP"}) RETURN count(p)'),
        ("index + property filter", "MATCH (p:Person {age:42}) WHERE p.active = true RETURN count(p)"),
        ("self-loops: RETRY", "MATCH (e:Event)-[:RETRY]->(e) RETURN count(e)"),
        ("label scan + unindexed filter", "MATCH (o:Order) WHERE o.total > 1990 RETURN count(o)"),
        ("aggregate: group by", "MATCH (o:Order) RETURN o.status, count(o), avg(o.total) ORDER BY o.status"),
        ("top-k: ORDER BY LIMIT over big rows", "MATCH (d:Document) RETURN d.title, d.score ORDER BY d.score DESC LIMIT 5"),
        ("edge scan + edge-property filter", "MATCH ()-[r:REVIEWED]->() WHERE r.rating = 5 RETURN count(r)"),
        ("count all nodes", "MATCH (n) RETURN count(n)"),
        ("STATS", "STATS"),
        ("SCHEMA", "SCHEMA"),
    ]


ALGOS = [
    ("algo: degree top 5", "CALL degree(top: 5)"),
    ("algo: wcc", "CALL wcc(top: 3)"),
    ("algo: pagerank 5 iterations", "CALL pagerank(iterations: 5, top: 5)"),
]


# ------------------------------------------------------------------ server

class Server:
    def __init__(self, binary, db, extra=(), port=None):
        port = port or ARGS.port
        self.addr = ("127.0.0.1", port)
        self.cmd = [binary, db, "serve", "--addr", f"127.0.0.1:{port}"] + list(extra)
        self.p = None
        self.start_s = None

    def start(self, timeout=1800):
        t = time.time()
        self.p = subprocess.Popen(self.cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, preexec_fn=cap)
        while time.time() - t < timeout:
            if self.p.poll() is not None:
                return False
            try:
                c = http.client.HTTPConnection(*self.addr, timeout=5)
                c.request("GET", "/health")
                if c.getresponse().status == 200:
                    self.start_s = time.time() - t
                    return True
            except OSError:
                time.sleep(0.005)
        return False

    def rss_mb(self):
        try:
            with open(f"/proc/{self.p.pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return round(int(line.split()[1]) / 1024, 1)
                    if line.startswith("VmHWM:"):
                        pass
        except OSError:
            return None

    def hwm_mb(self):
        try:
            with open(f"/proc/{self.p.pid}/status") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        return round(int(line.split()[1]) / 1024, 1)
        except OSError:
            return None

    def request(self, method, path, body=None, timeout=900):
        t = time.time()
        try:
            c = http.client.HTTPConnection(*self.addr, timeout=timeout)
            c.request(method, path, body=body.encode() if body else None)
            r = c.getresponse()
            data = r.read()
            return time.time() - t, r.status, data
        except (OSError, socket.timeout) as e:
            return time.time() - t, None, str(e).encode()

    def query(self, q, timeout=900):
        return self.request("POST", "/query", q, timeout)

    def stop(self):
        if self.p and self.p.poll() is None:
            self.p.send_signal(signal.SIGTERM)
            try:
                self.p.wait(10)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
        for p in (self.cmd[1] + ".lock",):
            if os.path.exists(p) and self.p.returncode not in (0, None):
                pass


# ------------------------------------------------------------------ phases

def gen_info():
    info = {}
    path = os.path.join(DATA, "gen.jsonl")
    if os.path.exists(path):
        for line in open(path):
            d = json.loads(line)
            name = d["size"]
            info[name] = d
    return info


def image_sections(path):
    """Bytes per section kind, from the image directory."""
    names = {1: "strings", 2: "node_ids", 3: "node_label_off", 4: "node_labels", 5: "node_prop_off",
             6: "out_off", 7: "out_nbr", 8: "out_edge", 9: "in_off", 10: "in_nbr", 11: "in_edge",
             12: "edge_ids", 13: "edge_from", 14: "edge_to", 15: "edge_type", 16: "edge_prop_off",
             17: "label_off", 18: "label_nodes", 19: "prop_index_v1", 20: "node_props", 21: "edge_props",
             22: "node_props_crc", 23: "edge_props_crc", 24: "type_counts", 25: "index_dir"}
    with open(path, "rb") as f:
        head = f.read(64)
        if int.from_bytes(head[8:12], "little") != 3:
            return None
        image_len = int.from_bytes(head[32:40], "little")
        ih = f.read(64)
        nsec = int.from_bytes(ih[12:16], "little")
        d = f.read(32 * nsec)
    out = {"file_header": 64, "image_header_and_directory": 64 + 32 * nsec}
    for i in range(nsec):
        e = d[32 * i:32 * i + 32]
        kind = int.from_bytes(e[0:4], "little")
        ln = int.from_bytes(e[16:24], "little")
        key = names.get(kind, "index_bodies" if kind >= 1000 else f"kind{kind}")
        out[key] = out.get(key, 0) + ln
    out["image_len"] = image_len
    return out


def phase_file(size, img):
    rec = {"phase": "file", "size": size, "file_bytes": fsize(img)}
    secs = image_sections(img)
    if secs:
        rec["sections"] = secs
        groups = {
            "properties": ["node_props", "edge_props", "node_props_crc", "edge_props_crc"],
            "adjacency": ["out_off", "out_nbr", "out_edge", "in_off", "in_nbr", "in_edge"],
            "edge_columns": ["edge_ids", "edge_from", "edge_to", "edge_type", "edge_prop_off"],
            "node_columns": ["node_ids", "node_label_off", "node_labels", "node_prop_off", "label_off", "label_nodes"],
            "indexes": ["index_bodies", "index_dir", "prop_index_v1"],
        }
        rec["groups"] = {g: sum(secs.get(k, 0) for k in ks) for g, ks in groups.items()}
    stats = run([NEW, img, "--json", "-c", "STATS"])
    try:
        rows = json.loads(stats["out"].strip().splitlines()[-1])["rows"]
        rec["stats"] = {r[0]: r[1] for r in rows}
    except Exception:
        rec["stats_error"] = stats["out"][-300:]
    emit(rec)


def phase_open(size, img, logf):
    cfgs = [("image, lazy (default)", NEW, img, [])]
    cfgs.append(("image, props on disk", NEW, img, ["--props", "disk"]))
    if size in ("1GiB", "5GiB", "10GiB"):
        cfgs.append(("image, --preload", NEW, img, ["--preload"]))
    if logf:
        cfgs.append(("log, new build (replay)", NEW, logf, ["--auto-compact", "off"]))
        cfgs.append(("log, old build (replay)", OLD, logf, []))
    for name, binary, path, extra in cfgs:
        for state in ("cold", "warm"):
            if state == "cold":
                evict(path)
            r = run([binary, path, "-c", "STATS"] + extra, timeout=ARGS.timeout)
            ok = r["exit"] == 0 and not r["timeout"]
            emit({"phase": "open", "size": size, "config": name, "cache": state,
                  "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"], "ok": ok,
                  "error": None if ok else (("timeout" if r["timeout"] else f"exit {r['exit']}") + ": " + r["out"][-200:])})
            if not ok:
                break


def phase_queries(size, img, logf, lay):
    qs = queries(lay) + ALGOS
    cfgs = [("image, lazy (default)", NEW, img, []), ("image, props on disk", NEW, img, ["--props", "disk"])]
    if logf:
        cfgs.append(("log, old build (replay)", OLD, logf, []))
    for name, binary, path, extra in cfgs:
        if ARGS.only_config and ARGS.only_config not in name:
            continue
        evict(path)
        srv = Server(binary, path, extra)
        ok = srv.start(timeout=ARGS.timeout)
        emit({"phase": "server_start", "size": size, "config": name, "ok": ok,
              "wall_s": round(srv.start_s, 4) if ok else None, "rss_mb": srv.rss_mb() if ok else None,
              "error": None if ok else (srv.p.stderr.read().decode()[-300:] if srv.p else "")})
        if not ok:
            srv.stop()
            continue
        for label, q in qs:
            if not ARGS.algos and label.startswith("algo:"):
                continue
            rss0 = srv.rss_mb()
            t1, st, body = srv.query(q, timeout=ARGS.query_timeout)
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
            if first_ok:
                for _ in range(3 if t1 < 10 else 1):
                    t, st2, _ = srv.query(q, timeout=ARGS.query_timeout)
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
                srv = Server(binary, path, extra)
                if not srv.start(timeout=ARGS.timeout):
                    break
        # Browsing API.
        if srv.p.poll() is None:
            mid = lay.member("Person", 0.5)
            hub = lay.member("Person", 0.0)
            for label, path_ in [
                ("api: schema", "/api/schema"),
                ("api: stats", "/stats"),
                ("api: nodes page (label)", "/api/nodes?label=Person&limit=100"),
                ("api: nodes page (deep offset)", f"/api/nodes?label=Order&from={lay.member('Order', 0.9)}&limit=100"),
                ("api: nodes search (text)", "/api/nodes?q=Z%C3%BCrich&limit=50"),
                ("api: edges page (type)", "/api/edges?type=CITES&limit=100"),
                ("api: expand node", f"/api/expand?id={mid}&limit=200"),
                ("api: expand hub", f"/api/expand?id={hub}&limit=200"),
            ]:
                t1, st, body = srv.request("GET", path_, timeout=ARGS.query_timeout)
                t2, st2, _ = srv.request("GET", path_, timeout=ARGS.query_timeout)
                emit({"phase": "api", "size": size, "config": name, "query": label, "text": path_,
                      "first_s": round(t1, 5), "warm_s": round(t2, 5), "ok": st == 200,
                      "rss_after_mb": srv.rss_mb(), "result": body.decode(errors="replace")[:160]})
        emit({"phase": "server_end", "size": size, "config": name, "rss_mb": srv.rss_mb(), "hwm_mb": srv.hwm_mb()})
        srv.stop()


def phase_load(size, img, lay):
    """Concurrent read load through the HTTP server."""
    loadgen = os.path.join(ROOT, "target", "release", "loadgen")
    mid = lay.member("Person", 0.5)
    qfile = os.path.join(DATA, f"load-{size}.txt")
    with open(qfile, "w") as f:
        for k in range(200):
            i = lay.member("Person", (k * 0.618) % 1.0)
            f.write(f'MATCH (p:Person {{email:"user{i:010d}@example.org"}})-[:KNOWS]->(f) RETURN count(f)\n')
    srv = Server(NEW, img)
    if not srv.start():
        return
    for clients in (1, 4, 16):
        r = run([loadgen, "--addr", f"127.0.0.1:{ARGS.port}", "--clients", str(clients), "--duration", "10",
                 "--warmup", "2", "--query-file", qfile, "--json"], timeout=120)
        try:
            j = json.loads(r["out"].strip().splitlines()[-1])
        except Exception:
            j = {"raw": r["out"][-300:]}
        emit({"phase": "load", "size": size, "clients": clients, "result": j, "rss_mb": srv.rss_mb()})
    srv.stop()
    os.remove(qfile)


def phase_verify(size, img):
    for state in ("cold", "warm"):
        if state == "cold":
            evict(img)
        r = run([NEW, img, "verify"], timeout=ARGS.timeout)
        emit({"phase": "verify", "size": size, "cache": state, "wall_s": r["wall_s"],
              "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0 and "image         ok" in r["out"],
              "throughput_mb_s": round(fsize(img) / (1 << 20) / max(r["wall_s"], 1e-9), 1)})


def phase_writes(size, img, lay):
    """Writes land in the log after the image. Ends with reopen + compact."""
    n = 200
    for sync in ("off", "normal", "always"):
        cn = 200 if sync == "always" else 2000
        script = "".join(f'CREATE (:Bench {{i:{k}, sync:"{sync}"}});\n' for k in range(cn))
        path = os.path.join(DATA, f"w-{size}.gql")
        open(path, "w").write(script)
        # The open (including any log tail from earlier writes) is measured
        # just before, in the same state, and subtracted.
        base = run([NEW, img, "-c", "STATS"])
        r = run([NEW, img, "--sync", sync, "--auto-compact", "off", "-f", path], timeout=ARGS.timeout)
        emit({"phase": "write", "size": size, "op": f"CREATE node, autocommit, sync {sync}", "n": cn,
              "wall_s": r["wall_s"], "per_op_ms": round(max(r["wall_s"] - base["wall_s"], 0) / cn * 1000, 4),
              "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0})
    ops = []
    for k in range(n):
        i = lay.member("Person", (k * 0.37) % 1.0)
        ops.append(("SET on image node (copy-on-write)", f'MATCH (p:Person {{email:"user{i:010d}@example.org"}}) SET p.score = {k}.5;'))
    for k in range(n):
        a = lay.member("Person", (k * 0.11) % 1.0)
        b = lay.member("Person", (k * 0.29) % 1.0)
        ops.append(("CREATE edge between image nodes", f'MATCH (a:Person {{email:"user{a:010d}@example.org"}}),(b:Person {{email:"user{b:010d}@example.org"}}) CREATE (a)-[:KNOWS {{since:2026}}]->(b);'))
    for k in range(50):
        i = lay.member("Person", 0.2 + k * 0.013)
        ops.append(("DETACH DELETE image node", f'MATCH (p:Person {{email:"user{i:010d}@example.org"}}) DETACH DELETE p;'))
    for op in dict.fromkeys(o for o, _ in ops):
        stmts = [q for o, q in ops if o == op]
        path = os.path.join(DATA, f"w-{size}.gql")
        open(path, "w").write("\n".join(stmts) + "\n")
        base = run([NEW, img, "-c", "STATS"])
        r = run([NEW, img, "--sync", "normal", "--auto-compact", "off", "-f", path], timeout=ARGS.timeout)
        emit({"phase": "write", "size": size, "op": op, "n": len(stmts), "wall_s": r["wall_s"],
              "per_op_ms": round(max(r["wall_s"] - base["wall_s"], 0) / len(stmts) * 1000, 3),
              "peak_rss_mb": r["peak_rss_mb"], "ok": r["exit"] == 0, "error": None if r["exit"] == 0 else r["out"][-200:]})
    os.remove(os.path.join(DATA, f"w-{size}.gql"))
    # Reopen with a tail, then compact, then reopen again.
    for state in ("cold", "warm"):
        if state == "cold":
            evict(img)
        r = run([NEW, img, "--json", "-c", "STATS"])
        tail = None
        try:
            rows = json.loads(r["out"].strip().splitlines()[-1])["rows"]
            tail = {x[0]: x[1] for x in rows}.get("tail_bytes")
        except Exception:
            pass
        emit({"phase": "reopen_with_tail", "size": size, "cache": state, "wall_s": r["wall_s"],
              "peak_rss_mb": r["peak_rss_mb"], "tail_bytes": tail})
    before = fsize(img)
    r = run([NEW, img, "compact"], timeout=ARGS.timeout * 2)
    emit({"phase": "compact", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "bytes_before": before, "bytes_after": fsize(img),
          "throughput_mb_s": round(fsize(img) / (1 << 20) / max(r["wall_s"], 1e-9), 1),
          "error": None if r["exit"] == 0 else r["out"][-300:]})
    evict(img)
    r = run([NEW, img, "-c", "STATS"])
    emit({"phase": "reopen_after_compact", "size": size, "cache": "cold", "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"]})


def phase_replicate(size, img):
    rep = os.path.join(DATA, f"replica-{size}")
    out = os.path.join(DATA, f"restored-{size}.gldb")
    shutil.rmtree(rep, ignore_errors=True)
    rm_db(out)
    r = run([NEW, img, "wal", "tail", "--to", rep, "--once"], timeout=ARGS.timeout)
    emit({"phase": "replicate_ship", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "throughput_mb_s": round(fsize(img) / (1 << 20) / max(r["wall_s"], 1e-9), 1),
          "error": None if r["exit"] == 0 else r["out"][-200:]})
    r = run([NEW, "wal", "restore", "--from", rep, "--to", out], timeout=ARGS.timeout)
    same = False
    if r["exit"] == 0:
        c = run(["cmp", img, out], timeout=ARGS.timeout)
        same = c["exit"] == 0
    emit({"phase": "replicate_restore", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "byte_identical": same})
    shutil.rmtree(rep, ignore_errors=True)
    rm_db(out)


def phase_export(size, img):
    out = os.path.join(DATA, f"export-{size}.jsonl")
    db2 = os.path.join(DATA, f"reimport-{size}.gldb")
    rm_db(db2)
    r = run([NEW, img, "export", out], timeout=ARGS.timeout)
    emit({"phase": "export", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
          "ok": r["exit"] == 0, "jsonl_bytes": fsize(out), "error": None if r["exit"] == 0 else r["out"][-200:]})
    if r["exit"] == 0:
        r = run([NEW, db2, "--auto-compact", "off", "import", out], timeout=ARGS.timeout)
        emit({"phase": "import", "size": size, "wall_s": r["wall_s"], "peak_rss_mb": r["peak_rss_mb"],
              "ok": r["exit"] == 0, "log_bytes": fsize(db2), "error": None if r["exit"] == 0 else r["out"][-200:]})
        a = run([NEW, img, "-c", "MATCH (n) RETURN count(n)"])
        b = run([NEW, db2, "-c", "MATCH (n) RETURN count(n)"])
        emit({"phase": "roundtrip_check", "size": size, "same_node_count": a["out"] == b["out"]})
    if os.path.exists(out):
        os.remove(out)
    rm_db(db2)


def phase_oldlog(size, logf):
    """The old build on a log it cannot fit, capped, to record how it fails."""
    for name, binary, extra in (("log, old build (replay)", OLD, []), ("log, new build (replay)", NEW, ["--auto-compact", "off"])):
        r = run([binary, logf, "-c", "STATS"] + extra, timeout=ARGS.timeout)
        ok = r["exit"] == 0 and not r["timeout"]
        emit({"phase": "open", "size": size, "config": name, "cache": "warm", "wall_s": r["wall_s"],
              "peak_rss_mb": r["peak_rss_mb"], "ok": ok,
              "error": None if ok else (("timeout" if r["timeout"] else f"exit {r['exit']}") + ": " + r["out"][-160:])})


def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default=",".join(SIZES))
    ap.add_argument("--only", default="file,open,queries,api,load,verify,writes,replicate,export")
    ap.add_argument("--only-config", default=None)
    ap.add_argument("--mem-cap", type=float, default=16.0, help="GiB address-space cap per process")
    ap.add_argument("--timeout", type=float, default=1800)
    ap.add_argument("--query-timeout", type=float, default=600)
    ap.add_argument("--algos", action="store_true", default=True)
    ap.add_argument("--results", default=RESULTS)
    ap.add_argument("--port", type=int, default=7979)
    ARGS = ap.parse_args()
    phases = ARGS.only.split(",")
    info = gen_info()
    for size in ARGS.sizes.split(","):
        img = os.path.join(DATA, f"img-{size}.gldb")
        logf = os.path.join(DATA, f"log-{size}.gldb")
        if not os.path.exists(img):
            log("missing", img)
            continue
        gi = info.get(f"img-{size}.gldb", {})
        lay = Layout(gi.get("ids", 0))
        # Old build can only hold the 1 GiB log in RAM; larger logs get a
        # capped attempt that records the failure.
        log_ok = os.path.exists(logf) and size == "1GiB"
        log(f"=== {size}: {fsize(img) / GIB:.2f} GiB image, {gi.get('ids')} ids")
        if "file" in phases:
            phase_file(size, img)
        if "open" in phases:
            phase_open(size, img, logf if log_ok else None)
            if os.path.exists(logf) and not log_ok:
                phase_oldlog(size, logf)
        if "verify" in phases:
            phase_verify(size, img)
        if "queries" in phases:
            phase_queries(size, img, logf if log_ok else None, lay)
        if "load" in phases:
            phase_load(size, img, lay)
        if "replicate" in phases and size in ("1GiB", "5GiB", "tiny"):
            phase_replicate(size, img)
        if "export" in phases and size in ("1GiB", "tiny"):
            phase_export(size, img)
        if "writes" in phases:
            phase_writes(size, img, lay)


if __name__ == "__main__":
    main()
