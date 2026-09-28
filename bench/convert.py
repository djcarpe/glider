#!/usr/bin/env python3
"""Bring a graph from another database into glider.

    uv run --with neo4j python3 bench/convert.py bolt  --uri bolt://127.0.0.1:7687 --user neo4j --password secret --out g.jsonl
    python3 bench/convert.py csv     --nodes movies.csv --nodes people.csv --rels acted_in.csv --out g.jsonl
    python3 bench/convert.py graphml graph.graphml --out g.jsonl
    python3 bench/convert.py bolt --uri bolt://127.0.0.1:7687 --out g.jsonl --db g.gldb   # and import it

Every source becomes glider's JSON Lines — one object per node, then one per
edge — which `glider <db> import` reads, and `--db` runs that import for you.

Sources:

  bolt     any database speaking the Bolt protocol: Neo4j, Memgraph, AgensGraph...
           Nodes and relationships are streamed with the Neo4j Python driver;
           ids come from the server and are kept, so edges resolve.
  csv      the neo4j-admin import header format: `:ID`, `:LABEL`, `:START_ID`,
           `:END_ID`, `:TYPE`, and typed columns such as `age:int`, `tags:string[]`.
  graphml  GraphML as written by TinkerPop/Gremlin, Gephi, NetworkX and most
           others: `labelV`/`labels` and `labelE`/`label` keys are honoured.
  cypher   not converted at all: a plain script of CREATE statements runs
           straight through `glider <db> -f script.cypher`, since glider speaks
           Cypher. Neo4j scripts that use WITH, MERGE, UNWIND or APOC will not.

Values: Bolt temporals, points and durations become ISO/WKT-style strings;
nested maps become JSON text; lists of scalars are kept. `--drop` removes
properties by regular expression (embeddings, say) before they are written.
"""

import argparse
import csv
import json
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.abspath(__file__))
GLIDER = os.environ.get("GLIDER", os.path.join(HERE, "..", "target", "release", "glider"))


# ------------------------------------------------------------------ output

class Writer:
    """Nodes first, then edges, ids remapped to dense integers as they arrive."""

    def __init__(self, out, drop=None):
        self.f = open(out, "w", encoding="utf-8")
        self.drop = re.compile(drop) if drop else None
        self.nodes = 0
        self.edges = 0
        self.ids = {}
        self.pending_edges = []

    def props(self, props):
        clean = {}
        for k, v in (props or {}).items():
            if self.drop and self.drop.search(k):
                continue
            clean[k] = value(v)
        return clean

    def node(self, source_id, labels, props):
        nid = self.ids.setdefault(source_id, len(self.ids) + 1)
        self.f.write(json.dumps({"type": "node", "id": nid, "labels": list(labels), "props": self.props(props)},
                                ensure_ascii=False) + "\n")
        self.nodes += 1

    def edge(self, source_id, etype, start, end, props):
        # Edges may arrive before their endpoints (CSV, GraphML); hold them.
        self.pending_edges.append((source_id, etype, start, end, self.props(props)))

    def close(self):
        eid = 0
        missing = 0
        for _sid, etype, start, end, props in self.pending_edges:
            a, b = self.ids.get(start), self.ids.get(end)
            if a is None or b is None:
                missing += 1
                continue
            eid += 1
            self.f.write(json.dumps({"type": "edge", "id": eid, "label": etype, "from": a, "to": b, "props": props},
                                    ensure_ascii=False) + "\n")
            self.edges += 1
        self.f.close()
        if missing:
            print(f"warning: {missing} edges referenced nodes that were not in the source; dropped", file=sys.stderr)


def value(v):
    """A property value glider can hold: null, bool, int, float, text, or a list of those."""
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, (list, tuple)):
        items = [value(x) for x in v]
        if all(x is None or isinstance(x, (bool, int, float, str)) for x in items):
            return items
        return json.dumps(items, ensure_ascii=False)
    if isinstance(v, dict):
        return json.dumps({k: value(x) for k, x in v.items()}, ensure_ascii=False)
    if isinstance(v, (bytes, bytearray)):
        return v.hex()
    iso = getattr(v, "iso_format", None)
    if callable(iso):
        return iso()
    return str(v)


# ------------------------------------------------------------------ bolt

def from_bolt(args, w):
    from neo4j import GraphDatabase  # uv run --with neo4j

    auth = (args.user, args.password) if args.user else None
    driver = GraphDatabase.driver(args.uri, auth=auth)
    kw = {"database": args.database} if args.database else {}
    with driver.session(**kw) as s:
        # Whole-graph scans in batches, ordered by id so a batch never repeats.
        node_q = "MATCH (n) WHERE id(n) > $after RETURN id(n) AS id, labels(n) AS labels, properties(n) AS props ORDER BY id LIMIT $batch"
        after = -1
        while True:
            rows = list(s.run(node_q, after=after, batch=args.batch))
            for r in rows:
                w.node(r["id"], r["labels"], r["props"])
            if not rows:
                break
            after = rows[-1]["id"]
            if args.limit and w.nodes >= args.limit:
                break
            print(f"\rnodes {w.nodes}", end="", file=sys.stderr)
        print(file=sys.stderr)
        rel_q = ("MATCH (a)-[r]->(b) WHERE id(r) > $after "
                 "RETURN id(r) AS id, type(r) AS type, id(a) AS a, id(b) AS b, properties(r) AS props ORDER BY id LIMIT $batch")
        after = -1
        n = 0
        while True:
            rows = list(s.run(rel_q, after=after, batch=args.batch))
            for r in rows:
                w.edge(r["id"], r["type"], r["a"], r["b"], r["props"])
                n += 1
            if not rows:
                break
            after = rows[-1]["id"]
            print(f"\redges {n}", end="", file=sys.stderr)
        print(file=sys.stderr)
    driver.close()


# ------------------------------------------------------------------ csv

def typed(name, raw):
    """`age:int` header semantics from neo4j-admin import."""
    if ":" in name:
        base, kind = name.rsplit(":", 1)
    else:
        base, kind = name, "string"
    kind = kind.lower()
    if raw == "" or raw is None:
        return base, None
    if kind.endswith("[]"):
        items = raw.split(";")
        inner = kind[:-2]
        return base, [typed(f"x:{inner}", i)[1] for i in items]
    if kind in ("int", "long", "short", "byte"):
        return base, int(raw)
    if kind in ("float", "double"):
        return base, float(raw)
    if kind == "boolean":
        return base, raw.strip().lower() == "true"
    return base, raw


def from_csv(args, w):
    for path in args.nodes:
        with open(path, newline="", encoding="utf-8") as f:
            rd = csv.reader(f)
            header = next(rd)
            # `:ID`, `movieId:ID`, `movieId:ID(Movie)` all mark the id column.
            id_col = next((i for i, h in enumerate(header) if re.search(r"(^|:)ID(\(|$)", h)), None)
            label_col = next((i for i, h in enumerate(header) if re.search(r"(^|:)LABEL$", h)), None)
            if id_col is None:
                raise SystemExit(f"{path}: no :ID column")
            space = header[id_col].split("(")[1].rstrip(")") if "(" in header[id_col] else ""
            for row in rd:
                if not row:
                    continue
                labels = row[label_col].split(";") if label_col is not None and row[label_col] else []
                labels += args.label or []
                props = {}
                for i, h in enumerate(header):
                    if i in (id_col, label_col) or h.startswith(":"):
                        continue
                    k, v = typed(h.split("(")[0], row[i])
                    if v is not None:
                        props[k] = v
                w.node((space, row[id_col]), labels, props)
    for path in args.rels:
        with open(path, newline="", encoding="utf-8") as f:
            rd = csv.reader(f)
            header = next(rd)
            s_col = next(i for i, h in enumerate(header) if re.search(r"(^|:)START_ID(\(|$)", h))
            e_col = next(i for i, h in enumerate(header) if re.search(r"(^|:)END_ID(\(|$)", h))
            t_col = next((i for i, h in enumerate(header) if re.search(r"(^|:)TYPE$", h)), None)
            s_space = header[s_col].split("(")[1].rstrip(")") if "(" in header[s_col] else ""
            e_space = header[e_col].split("(")[1].rstrip(")") if "(" in header[e_col] else ""
            for n, row in enumerate(rd):
                if not row:
                    continue
                etype = row[t_col] if t_col is not None else (args.type or "RELATED")
                props = {}
                for i, h in enumerate(header):
                    if i in (s_col, e_col, t_col) or h.startswith(":"):
                        continue
                    k, v = typed(h, row[i])
                    if v is not None:
                        props[k] = v
                w.edge(n, etype, (s_space, row[s_col]), (e_space, row[e_col]), props)


# ------------------------------------------------------------------ graphml

def from_graphml(args, w):
    ns = {"g": "http://graphml.graphdrawing.org/xmlns"}
    tree = ET.parse(args.path)
    root = tree.getroot()
    keys = {}
    for k in root.findall("g:key", ns):
        keys[k.get("id")] = (k.get("attr.name") or k.get("id"), (k.get("attr.type") or "string").lower(), k.get("for"))

    def data_of(el):
        out = {}
        for d in el.findall("g:data", ns):
            name, kind, _ = keys.get(d.get("key"), (d.get("key"), "string", None))
            raw = d.text or ""
            if kind in ("int", "long"):
                out[name] = int(raw) if raw else None
            elif kind in ("float", "double"):
                out[name] = float(raw) if raw else None
            elif kind == "boolean":
                out[name] = raw.strip().lower() == "true"
            else:
                out[name] = raw
        return out

    label_keys = ("labelV", "labels", "label", "type")
    for graph in root.findall("g:graph", ns):
        for node in graph.findall("g:node", ns):
            data = data_of(node)
            labels = []
            for lk in label_keys:
                if lk in data and isinstance(data[lk], str) and data[lk]:
                    labels = [l for l in re.split(r"[:;,]", data.pop(lk)) if l]
                    break
            labels += args.label or []
            w.node(node.get("id"), labels, data)
        for i, edge in enumerate(graph.findall("g:edge", ns)):
            data = data_of(edge)
            etype = None
            for lk in ("labelE", "label", "type"):
                if lk in data:
                    etype = data.pop(lk)
                    break
            w.edge(edge.get("id") or i, etype or args.type or "RELATED", edge.get("source"), edge.get("target"), data)


# ------------------------------------------------------------------ main

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="source", required=True)

    b = sub.add_parser("bolt", help="a Neo4j, Memgraph or other Bolt server")
    b.add_argument("--uri", default="bolt://127.0.0.1:7687")
    b.add_argument("--user")
    b.add_argument("--password")
    b.add_argument("--database", help="Neo4j 4+: the database name (community edition: neo4j)")
    b.add_argument("--batch", type=int, default=20_000)
    b.add_argument("--limit", type=int, default=0, help="stop after this many nodes (a sample)")

    c = sub.add_parser("csv", help="neo4j-admin import CSV files")
    c.add_argument("--nodes", action="append", default=[], help="node file (repeatable)")
    c.add_argument("--rels", action="append", default=[], help="relationship file (repeatable)")
    c.add_argument("--label", action="append", help="label to add to every node from these files")
    c.add_argument("--type", help="relationship type when the file has no :TYPE column")

    g = sub.add_parser("graphml", help="a GraphML file")
    g.add_argument("path")
    g.add_argument("--label", action="append")
    g.add_argument("--type")

    for p in (b, c, g):
        p.add_argument("--out", required=True, help="JSON Lines to write")
        p.add_argument("--drop", help="regex of property names to leave out (e.g. 'Embedding$')")
        p.add_argument("--db", help="also create this glider database and import into it")

    args = ap.parse_args()
    w = Writer(args.out, args.drop)
    {"bolt": from_bolt, "csv": from_csv, "graphml": from_graphml}[args.source](args, w)
    w.close()
    print(f"wrote {w.nodes} nodes and {w.edges} edges to {args.out}", file=sys.stderr)

    if args.db:
        if os.path.exists(args.db):
            raise SystemExit(f"{args.db} exists; refusing to import over it")
        r = subprocess.run([GLIDER, args.db, "import", args.out], capture_output=True, text=True)
        sys.stderr.write(r.stdout + r.stderr)
        if r.returncode != 0:
            raise SystemExit(r.returncode)
        print(f"imported into {args.db}", file=sys.stderr)


if __name__ == "__main__":
    main()
