#!/usr/bin/env python3
"""Render ../glider-bench-data/compare/results.jsonl (from bench/sqlite_compare.py)
as a single self-contained HTML page: one tab per dataset size with every
query's Cypher, SQL, timings, answers and SQLite plan, plus a cross-size view.

    python3 bench/sqlite_compare_report.py [--out compare.html]
"""

import argparse
import json
import os
import sqlite3
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sqlite_compare  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def collect(path):
    sizes, builds, mem, queries = [], {}, {}, {}
    for line in open(path):
        d = json.loads(line)
        size = d.get("size")
        if size and size not in sizes:
            sizes.append(size)
        if d["phase"] == "build":
            builds.setdefault(size, {})[d["engine"]] = d
        elif d["phase"] == "memory":
            mem[size] = d
        elif d["phase"] == "query":
            # later runs of the same query replace earlier ones
            q = queries.setdefault(size, {})
            q[d["name"]] = d
    order = {s: i for i, s in enumerate(sqlite_compare.SIZES)}
    sizes = [s for s in sizes if s in queries]
    sizes.sort(key=lambda s: order.get(s, 99))
    return {"sizes": sizes, "builds": builds, "memory": mem, "sqlite_version": sqlite3.sqlite_version,
            "queries": {s: list(queries[s].values()) for s in sizes}}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", default=os.path.join(sqlite_compare.DATA, "results.jsonl"))
    ap.add_argument("--out", default=os.path.join(sqlite_compare.DATA, "compare.html"))
    a = ap.parse_args()
    data = collect(a.results)
    tpl = open(os.path.join(HERE, "sqlite_compare.html")).read()
    blob = json.dumps(data, ensure_ascii=False).replace("</", "<\\/")
    open(a.out, "w").write(tpl.replace("/*__DATA__*/null", blob))
    print(f"wrote {a.out} ({len(data['sizes'])} sizes)")


if __name__ == "__main__":
    main()
