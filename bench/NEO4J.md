# glider against Neo4j, and getting graphs out of other databases

Two scripts. `convert.py` brings a graph from another database into glider;
`neo4j_bench.py` uses it to run Neo4j's own example graphs on both engines
and compare.

## convert.py — into glider from anywhere

Every source becomes glider's JSON Lines (`glider <db> import` reads it),
and `--db` runs that import for you:

```sh
uv run --with neo4j python3 bench/convert.py bolt \
    --uri bolt://127.0.0.1:7687 --user neo4j --password secret \
    --drop 'Embedding$' --out graph.jsonl --db graph.gldb
python3 bench/convert.py csv --nodes movies.csv --nodes people.csv --rels acted_in.csv --out g.jsonl
python3 bench/convert.py graphml air-routes.graphml --out g.jsonl
```

| source | what it reads |
|---|---|
| `bolt` | any Bolt server — Neo4j, Memgraph, AgensGraph — streamed in id order with the Neo4j Python driver; server ids are kept so edges resolve |
| `csv` | the `neo4j-admin import` header format: `:ID(Space)`, `:LABEL`, `:START_ID`, `:END_ID`, `:TYPE`, typed columns such as `year:int`, `tags:string[]` |
| `graphml` | GraphML as TinkerPop/Gremlin, Gephi and NetworkX write it; `labelV`/`labels` and `labelE`/`label` become labels and types |
| Cypher scripts | not converted: a script of plain `CREATE` statements runs through `glider <db> -f script.cypher` unchanged. Scripts using `WITH`, `MERGE`, `UNWIND` or APOC will not |

Values map to what glider holds: temporals, points and durations become
strings, nested maps become JSON text, lists of scalars are kept. `--drop`
takes a regular expression over property names, for embedding vectors and
the like. Edges whose endpoints are missing from the source are dropped with
a count on stderr.

Neo4j's `.dump` files can only be read by Neo4j, so the path for those is:
load the dump into a container, then `convert.py bolt` out of it — which is
what the benchmark does.

## neo4j_bench.py — Neo4j's example graphs on both engines

```sh
uv run --with neo4j python3 bench/neo4j_bench.py --dataset recommendations   # or stackoverflow
python3 bench/neo4j_report.py --dataset recommendations                       # -> bench/neo4j-recommendations.html
```

The script downloads the dump from `github.com/neo4j-graph-examples`, loads
it into `neo4j:5` with the Graph Data Science plugin (Docker, 6 GB), pulls
the graph out over Bolt into a paged glider file, checks the counts match,
then runs:

* **reads** — Neo4j's own example queries and their neighbours: the
  "users who rated this also rated" recommendation, co-actors, a director's
  casts, four-hop actor reach, whole-graph aggregations, point lookups.
  The same Cypher on both sides; glider's subset has no `WITH`, so the
  queries were chosen to be expressible in both and the one exception is
  marked;
* **algorithms** — PageRank (GDS `pageRank.stream` against `CALL
  pagerank`), weakly connected components, undirected shortest paths;
  Neo4j's projection step is reported on its own line;
* **writes** — hundreds of single autocommit statements: create a node and
  an edge, update a property, delete.

Every answer is compared: row sets, distinct sets, counts, path lengths,
component counts exactly; PageRank as top-ten overlap. Timings are
server-side on both sides — Neo4j's as the Bolt driver reports them (whole
milliseconds, so sub-millisecond Neo4j answers are floors), glider's from
its shell timer — and neither includes the client.

Results land in `bench/results/neo4j-<dataset>.jsonl`; the report is one
self-contained HTML page.
