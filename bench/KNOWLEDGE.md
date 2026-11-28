# A manufacturing graph that knows its own truth

`mfg-gen` writes a manufacturing graph from a seed and a size, together with
the truth about it and a fixed set of questions whose answers follow from
that truth. `mfg-bench` loads it into glider and records the baseline that
the derived-data work (`docs/DERIVED.md`) is measured against. Nothing here
may be believed because it sounds useful: each later piece has to show, on
this data, that it finds what is really there.

```sh
cargo build --release -p glider-bench
target/release/mfg-gen --nodes 100k --seed 1            # + --faults to plant faults
target/release/mfg-bench --data ../glider-bench-data/mfg/n100000-s1
glider some.gldb import ../glider-bench-data/mfg/n100000-s1/graph.jsonl
```

Data goes to `$GLIDER_BENCH_DATA/mfg/n<nodes>-s<seed>[-faults]`
(`../glider-bench-data` beside the repository by default), never into the
crate tree; `docs/BENCHMARKS.md` explains why. `--out DIR` puts it elsewhere.

## The graph

One enterprise; its catalogue; its plants down to the work instructions.

| label | what it is | key properties |
|---|---|---|
| `Enterprise` | the company | `name`, `code` |
| `Supplier` | who supplies tools and parts | `name`, `code`, `country` |
| `ProductModel` | what is built | `name`, `code`, `body`, `launched` |
| `Tool` | a catalogue tool | `name`, `code`, `kind`, `maker`, `model`, `spec` |
| `Part` | a catalogue part | `name`, `code`, `part_number`, `material`, `size` |
| `WorkInstruction` | `scope: "standard"`: a shared instruction in the catalogue; `scope: "local"`: one work element's own | `number`, `title`, `text`, `revision`, `scope` |
| `Plant`, `Shop`, `Line` | the hierarchy | `name`, `code` (`P01`, `P01-S2`, `P01-S2-L3`) |
| `Station` | a station on a line | `name`, `code` (`P01-S2-L3-ST040`), `number`, `takt_seconds`, `commissioned` |
| `Side` | `Left`/`Right`, or `Centre` | `name`, `code` |
| `WorkLocation` | a place on a side | `name`, `code` |
| `WorkElement` | a step of work | `name`, `code`, `seq`, `std_time_seconds` |
| `WorkOrder` | an order for a model on a line | `number`, `quantity`, `due`, `status` |

| edge | from → to | properties |
|---|---|---|
| `HAS_PLANT`, `HAS_SHOP`, `HAS_LINE`, `HAS_STATION`, `HAS_SIDE`, `HAS_LOCATION`, `HAS_ELEMENT` | down the hierarchy | |
| `HAS_INSTRUCTION` | WorkElement → its local WorkInstruction, one per revision | `from`, `to` (revision A ends where B starts; the current one has no `to`) |
| `USES_TOOL` | WorkElement → Tool | |
| `CONSUMES` | WorkElement → Part | `quantity` |
| `FOLLOWS` | WorkElement → standard WorkInstruction | |
| `DEPENDS_ON` | WorkElement → an earlier WorkElement of the same station | |
| `APPLIES_TO` | Part or standard WorkInstruction → ProductModel | `from`, optional `to` (effectivity) |
| `BUILDS` | Line → ProductModel | `from` |
| `FOR_MODEL`, `ON_LINE` | WorkOrder → ProductModel, Line | |
| `SUPPLIED_BY` | Tool, Part → Supplier | |
| `ISSUED_BY` | standard WorkInstruction → Enterprise | |
| `OFFERED_BY` | ProductModel → Enterprise | |

Dates are `YYYY-MM-DD` text; a date range is `from` inclusive, `to`
exclusive.

**Designs.** A handful of station designs (3 for a few hundred nodes, 24
from about a million) each fix the sides, locations, elements, their tools,
parts, standards, dependencies and instruction texts. A station draws its
design (the first design is the most common, then 1/2, 1/3, ... as often), so
most stations are a copy of another. Each keeps its own codes, numbers,
dates and times. One station in eight deviates: an extra work element (with
its own instruction, a tool, a dependency) or a work element missing one of
its design's tools. Names of sides, locations and elements are the design's;
codes and numbers are the station's.

**Sharing.** Tools, parts and standard instructions come from one catalogue,
so a tool is used by every station of every design that uses it, across
lines and plants.

**Text.** Instructions are written from sixteen topic vocabularies (torque,
sealing, inspection, welding, wiring, ...): each topic has its own verbs,
objects, terms, tools and a specification range, and every instruction
shares only a few frames and openers with other topics. Standard
instructions come in groups about one thing (one topic, one component, one
specification), worded differently. The local instructions of one design
element are the same text at every station of that design.

**Faults.** With `--faults`, one station in about twelve has a `USES_TOOL`
edge written twice, and `1 + stations/40` orphan nodes (tools, parts,
instructions with no edges) are written at the end. Without it the graph has
no node without an edge and no two edges with the same endpoints, type and
properties. Planting faults leaves every node as it was.

**Ids.** Nodes and edges carry `id` in the order written, from 1. Importing
into an empty database gives each the id written, which is the id the truth
and the questions use. Codes (`code`, `number`) identify nodes across
databases.

**Size.** `--nodes N` (`300`, `10k`, `2m`) sets everything: the number of
designs, the catalogue, the stations. The result is within a few percent of
N above a few thousand nodes (300 asked gives 286).

**Determinism.** The output is a function of `(seed, nodes, faults)`: one
splitmix64 stream per purpose, no hash map iteration, no floating point
written. A station's design and deviation are drawn from its own stream (by
its global index), so the generator holds the catalogue, the designs and one
small record per line, never the graph: memory does not grow with the
stations. `the_same_seed_and_size_give_the_same_bytes` pins a checksum of the
seed-1, 2000-node output; a deliberate change to the generator changes the
checksum and says so in its commit.

## The truth: `truth.jsonl`

One JSON object per line; `kind` says what it is. Ids are node ids, edge ids
where it says so.

| kind | fields | meaning |
|---|---|---|
| `meta` | `format`, `seed`, `nodes_target`, `faults`, `stations`, `lines`, `designs` | first line |
| `duplicate` | `label`, `a`, `b`, `agree_on`, `differ_on` | the same real thing entered twice: equal on `agree_on`, different on `differ_on` (a name written another way, a new code), connected to the same nodes |
| `lookalike` | `label`, `a`, `b`, `differ_on`, `why` | two different things that look alike: names sharing at least half their words, different on `differ_on` (tools of one kind and maker in another specification; parts of one kind in another size; one design's instruction at two stations) |
| `topic_group` | `group`, `topic`, `component`, `members` | standard instructions about the same thing (including duplicates) |
| `topic` | `id`, `topic`, `group` (standard only) | every instruction's topic |
| `design` | `design`, `name`, `stations`, `sides`, `locations`, `elements`, `nodes`, `tools`, `parts`, `standards` | a design, how many stations use it, and the catalogue ids it uses |
| `station` | `id`, `code`, `design`, `line`, `deviations`, `order`, `tools` | a station's design; its deviations (`{"type":"extra_element","element","location","instruction","tool","depends_on"}` or `{"type":"missing_tool","element","tool"}`); its work elements in an order that respects `DEPENDS_ON`; the tools it uses |
| `tool` | `id`, `name`, `designs`, `elements`, `stations`, `lines` | who shares a tool: the designs that use it and how many elements, stations and lines do |
| `fault` | `fault: "orphan"`, `id`, `label` / `fault: "parallel_edge"`, `type`, `from`, `to`, `edges` (edge ids) | planted faults, all of them |
| `summary` | `nodes`, `edges`, `questions`, `labels`, `types` | last line, with counts per label and edge type |

## The questions: `questions.jsonl`

Up to eight of each kind (fewer when the graph is too small to have them),
chosen from the seed.

| kind | example | answer |
|---|---|---|
| `lookup` | Which tools does station P01-S1-L1-ST010 use? | its tools |
| `multihop` | Which standard work instructions apply to model Elan pickup on line P01-S1-L1? | standards followed on the line with `APPLIES_TO` that model |
| `aggregate` | How many stations, P01-S1-L1-ST020 included, are built to the same design as station P01-S1-L1-ST020? | `count`; `ids` lists the first 200 such stations, `complete` says whether that is all |
| `temporal` | Which work instruction was in effect for work element ...-WE03 on 2023-05-14? | the revision whose `from`/`to` contain `at` |
| `similarity` | Which standard work instructions are about the same thing as SWI-00012 (...)? | the other members of its topic group |

Fields: `id`, `kind`, `text`, `query` (the words a search would use),
`anchors` (ids of the things named), `at` (temporal only), `ids` and `names`
(the expected answer, ascending by id), `count`, `complete`. A retrieval is
scored by recall and precision of `ids`; a model's answer by comparing with
`names` or `count`.

## Tests

`bench/tests/mfg.rs`, all on graphs of 300 to 30,000 nodes, in about a
second:

| property | test |
|---|---|
| same seed and size, same bytes (and a pinned checksum) | `the_same_seed_and_size_give_the_same_bytes` |
| another seed, another graph of the same shape | `another_seed_gives_another_graph_of_the_same_shape` |
| one parameter sets the size | `one_parameter_sets_the_size_from_hundreds_of_nodes_up` |
| imports cleanly, counts match, ids are the ids written | `the_graph_imports_cleanly_and_the_counts_match` |
| every id in the truth and questions exists | `every_id_in_the_truth_and_the_questions_exists_in_the_graph` |
| duplicates agree, look-alikes differ | `planted_duplicates_agree_and_lookalikes_really_differ` |
| stations of a design are identical but for their deviations | `stations_of_one_design_are_identical_except_for_their_recorded_deviations` |
| the dependency order is topological | `the_dependency_order_is_a_topological_order_of_each_station` |
| every answer follows from the graph | `every_kind_of_question_is_asked_and_its_answer_follows_from_the_graph` |
| topics read alike, groups more so | `instructions_of_one_topic_read_alike_and_of_two_topics_do_not` |
| tool sharing is what the graph shows | `tool_sharing_in_the_truth_is_what_the_graph_shows` |
| faults only when asked, all listed | `faults_are_planted_only_when_asked_for_and_all_of_them_are_listed` |

## The harness: `mfg-bench`

```
mfg-bench --data DIR [--tiers memory,file] [--budgets 250,1000,4000]
          [--only stats,retrieve,pagerank,communities,check,structure,changes,vector] [--dims 64,384,768]
          [--out FILE] [--label NAME] [--detail]
```

For each tier it imports `graph.jsonl` with the streaming importer, then runs
every measurement in `MEASURES`, appending JSON lines to
`bench/results/mfg.jsonl`. Each line has `bench`, `label`, `dataset`,
`seed`, `nodes_target`, `tier`, `measure`, then:

| measure | fields |
|---|---|
| `load` | `nodes`, `edges`, `jsonl_bytes`, `import_ms`, `checkpoint_ms`, `bytes` (`file_len`), `bytes_on_disk` (file tier), `rss_peak_mb` |
| `stats` | `stats`: the `STATS` rows as an object |
| `context.index` | `ms` to build the text index, its message |
| `retrieve` | per `mode` (`seeds`: the anchors; `query`: the words), `budget` and `kind` (and `all`): `questions`, mean `recall`, mean `precision` (expected ids among the cards that are not anchors; 0 when there are none), mean `tokens`, mean `shown` cards, `ms_p50`, `ms_max`. `--detail` adds a `retrieve.question` line per question |
| `pagerank`, `communities` | `ms`, the message |
| `check.indexes` | `ms` to create `INDEX ON :WorkElement(code)` and `:WorkInstruction(text)`, so the index checks have something to read |
| `check` | per `check` (each database check alone, `all` of them, `orphans`, `duplicates` (parallel edges), `duplicates:<label>` on the properties the truth says that label's duplicates agree on, `required` for `Station -HAS_SIDE->`): `ms`, `records`, `problems`, `faults`, `rss_before_mb`, `rss_added_mb` (peak resident during the call less resident before, the peak reset first); for data checks `planted`, `found`, `hit`, `exact`, and `lookalikes_reported`. Then `all, slices of 100000`: `ms`, `calls`, `rows` |

| `structure.colors` | `what: "colours per depth"`: `depths` as `[depth, colours over every node, colours among stations]` to depth 5; `what: "design recovery"` at depths 1 to 4: the colouring kept (`kept`), read back in slices (`read_ms`, `read_slices`) and scored (`stations`, `station_colours`, `designs`, `designs_whole`, `designs_alone`, `deviated`, `deviated_apart`, `exact`); `what: "stable"`: `depth: "stable"`, its depths and message. Each with `ms`, `rss_before_mb`, `rss_added_mb` |
| `structure.classes` | `by: "neighbours"` per catalogue label, on the properties its duplicates agree on: `classes`, `planted` / `found` / `hit` / `exact` (pairs, the truth's pairs closed under "duplicate of the same original"), `lookalikes`, `lookalikes_reported`, `sizes` (`[size, classes]`, singletons included); `by: "colors"` over the kept depth-4 colouring of stations: `sizes`. Each with `ms` and memory |
| `changes.write` | per `write` (`node set`: a property set on node `1 + i mod nodes`; `edge add` between pseudo-random nodes; `edge delete` of those edges), `journal` (installed or not), `round`: `writes`, `per_commit`, `per_s`, `log_bytes_per_write` (revision moved / writes): 100,000 writes (or the node count when smaller) committed every 1,000, then 10,000 committed one by one. Rounds alternate without and with the journal, twice |
| `changes.read` | after each round with the journal: `entries`, `bytes`, `bytes_per_entry` (from `changes.status`), `since_rows`, `since_calls`, `since_all_ms` (every entry since the install, by statement, 10,000 a call), `since_tail_rows` / `since_tail_ms` (the last commit), `changed_us` (`changes::changed` for 1,000 node ids), `changed_any_us` |
| `changes.cap` | at a cap of a fifth of the writes (at least 1,000), so the journal sweeps throughout: `writes`, `per_s`, `log_bytes_per_write` for node sets in 1,000-write transactions; then 10,000 single writes: `single_per_s`, `single_slowest_us`, `single_biggest_log_bytes`; `entries` held after, `floor_rose` |
| `vector.bytes` | per `dims`: the same vectors as a list property on 10,000 nodes (`list_log_per_vector`, `list_db_growth_per_vector`, `list_per_s`), and as a space over every node (`space_bytes_per_vector`, `space_bytes_per_dim` from `vector.spaces()`, `put_log_per_vector`, `put_db_growth_per_vector`) |
| `vector.put` | `vectors`, `per_call` (1,000, with a source hash each), `ms`, `per_s`, through statements |
| `vector.search` | per `candidates` (`all`, or `WorkInstruction`), after one `counts: true` search has established the space current (`establish_ms`): `scored`, `queries`, `ms_p50`, `ms_max`, `scored_per_s` (the statement as a caller asks it, `fresh: "report"`), `counts_ms_p50` (with `counts: true`), `require_ms_p50` (with `fresh: "require"`), `scan_ms_p50` / `scan_scored_per_s` (the Rust `vector::search` scan alone), `rss_before_mb`, `rss_added_mb`. (Lines labelled `g10-vector` predate currency: their `ms_p50` counted on every search.) |
| `vector.stale` | after a property set on 100 nodes: `search_after_change_ms` (one search, which must report the space not current, from the journal), then `rows`, `calls` (slices of 1,000), `ms` for the whole list |
| `vector.drop` | `ms` |

A later slice adds a measurement by writing `fn(&mut Run) -> Result<(),
String>` and adding it to `MEASURES`: `Run` gives the loaded graph, the
tier, the questions, the budgets, `run(statement, params)` (timed) and
`emit(measure, fields)`.

## Baseline

Measured on the branch point of the derived-data work, release build, one
run each, on a shared machine (times are indicative; counts and recall are
exact and the same on both tiers).

| dataset | nodes | edges | JSONL | tier | import | `file_len` | on disk | peak RSS | `context.index` | `pagerank` | `communities` |
|---|---|---|---|---|---|---|---|---|---|---|---|
| n10000-s1 | 10,055 | 21,246 | 4.4 MB | memory | 0.3 s | 5.7 MB | – | 12 MB | 0.3 s | 11 ms | 16 ms, 1 found |
| n10000-s1 | 10,055 | 21,246 | 4.4 MB | file | 0.3 s | 6.4 MB | 9.2 MB | 25 MB | 0.4 s | 9 ms | 11 ms, 1 found |
| n100000-s1 | 103,004 | 246,260 | 49 MB | memory | 3.8 s | 65 MB | – | 76 MB | 4.2 s | 115 ms | 217 ms, 2 found |
| n100000-s1 | 103,004 | 246,260 | 49 MB | file | 3.5 s | 67 MB | 99 MB | 212 MB | 3.9 s | 123 ms | 203 ms, 2 found |
| n1000000-s1 | 1,023,660 | 2,435,930 | 494 MB | memory | 40.1 s | 647 MB | – | 658 MB | 39.2 s | 1146 ms | 9069 ms, 2 found |
| n1000000-s1 | 1,023,660 | 2,435,930 | 494 MB | file | 91.6 s | 659 MB | 701 MB | 2030 MB | 44.9 s | 4298 ms | 13490 ms, 2 found |

`context.retrieve`, recall / precision by question kind (identical on both tiers), mean tokens used and median time in memory:

| dataset | mode | budget | lookup | multihop | aggregate | temporal | similarity | all | tokens | p50 |
|---|---|---|---|---|---|---|---|---|---|---|
| n10000-s1 | seeds | 250 | 0.00 / 0.00 | 0.00 / 0.00 | 0.02 / 0.00 | 0.50 / 0.50 | 0.00 / 0.00 | 0.10 / 0.10 | 226 | 0.7 ms |
| n10000-s1 | seeds | 1000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.02 / 0.00 | 1.00 / 0.09 | 0.04 / 0.01 | 0.21 / 0.02 | 962 | 1.2 ms |
| n10000-s1 | seeds | 4000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.04 / 0.03 | 1.00 / 0.02 | 0.17 / 0.01 | 0.24 / 0.01 | 3927 | 2.3 ms |
| n10000-s1 | query | 250 | 0.00 / 0.00 | 0.00 / 0.00 | 0.02 / 0.00 | 0.00 / 0.00 | 0.20 / 0.62 | 0.04 / 0.12 | 230 | 2.3 ms |
| n10000-s1 | query | 1000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.02 / 0.00 | 1.00 / 0.10 | 0.38 / 0.14 | 0.28 / 0.05 | 958 | 4.1 ms |
| n10000-s1 | query | 4000 | 0.27 / 0.02 | 0.00 / 0.00 | 0.03 / 0.00 | 1.00 / 0.02 | 0.56 / 0.05 | 0.37 / 0.02 | 3916 | 5.3 ms |
| n100000-s1 | seeds | 250 | 0.00 / 0.00 | 0.00 / 0.00 | 0.01 / 0.00 | 0.75 / 0.69 | 0.00 / 0.00 | 0.15 / 0.14 | 225 | 0.8 ms |
| n100000-s1 | seeds | 1000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.01 / 0.00 | 1.00 / 0.10 | 0.05 / 0.03 | 0.21 / 0.03 | 961 | 1.8 ms |
| n100000-s1 | seeds | 4000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.01 / 0.01 | 1.00 / 0.02 | 0.10 / 0.01 | 0.22 / 0.01 | 3922 | 5.1 ms |
| n100000-s1 | query | 250 | 0.00 / 0.00 | 0.00 / 0.00 | 0.01 / 0.00 | 0.00 / 0.00 | 0.21 / 0.75 | 0.04 / 0.15 | 229 | 12.1 ms |
| n100000-s1 | query | 1000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.01 / 0.00 | 1.00 / 0.10 | 0.39 / 0.16 | 0.28 / 0.05 | 967 | 13.4 ms |
| n100000-s1 | query | 4000 | 0.11 / 0.02 | 0.00 / 0.00 | 0.01 / 0.00 | 1.00 / 0.02 | 0.44 / 0.03 | 0.31 / 0.01 | 3896 | 14.5 ms |
| n1000000-s1 | seeds | 250 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.62 / 0.62 | 0.00 / 0.00 | 0.12 / 0.12 | 216 | 0.4 ms |
| n1000000-s1 | seeds | 1000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 1.00 / 0.10 | 0.00 / 0.00 | 0.20 / 0.02 | 817 | 0.5 ms |
| n1000000-s1 | seeds | 4000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 1.00 / 0.03 | 0.00 / 0.00 | 0.20 / 0.01 | 2861 | 1.0 ms |
| n1000000-s1 | query | 250 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.04 / 0.12 | 0.01 / 0.03 | 222 | 41.7 ms |
| n1000000-s1 | query | 1000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.88 / 0.09 | 0.07 / 0.02 | 0.19 / 0.02 | 810 | 34.3 ms |
| n1000000-s1 | query | 4000 | 0.00 / 0.00 | 0.00 / 0.00 | 0.00 / 0.00 | 0.88 / 0.02 | 0.07 / 0.01 | 0.19 / 0.01 | 2928 | 34.7 ms |

The generator itself: 0.03 s for 10k nodes, 0.4 s for 100k, 2.6 s for 1M
(about 400,000 nodes a second with their edges and truth), with a peak
resident size of 3 MB, 5 MB and 6 MB; a 10M-node run held under 10 MB
while it wrote (that run was stopped by the disk quota of the machine it ran
on after 1.9 GB of its ~5 GB, not by the generator).

What the baseline says, to be beaten:

- From the anchors, retrieval finds a temporal answer (one hop away) and
  almost nothing else. A station's tools are four hops down, beyond the
  default depth of 3, so `lookup` recall is 0 at every budget; a line's
  standard instructions are five hops away (`multihop` 0); the stations of
  the same design are not connected to each other at all (`aggregate`
  near 0). This is the gap `structure` (templates, classes) is meant to
  close.
- From the words, text search finds look-alike instructions
  (`similarity` 0.2-0.56 at 10k-100k) but drowns at 1M (0.04-0.07), where
  every topic has tens of thousands of instructions sharing its vocabulary.
  This is the gap vectors and their comparison are meant to measure.
- Label propagation (`communities`) finds 1-2 communities: the shared
  catalogue joins everything, so it says nothing about lines or designs.
- Precision is low everywhere because the budget fills with cards near the
  anchor that are not the answer; the question set is what a compact form
  (G16) has to do better on.

## Integrity: `check.run`

Measured on the `--faults` graphs (`n<size>-s1-faults`), after
`context.index`, release build, one run each on a shared machine
(`mfg-bench --only retrieve,check --budgets 1000`; lines labelled
`g6-check` in `bench/results/mfg.jsonl`). Times in ms, memory tier / file
tier; records and findings are the same on both.

| dataset | `endpoints` | `adjacency` | `membership` | `counters` | `indexes` | `derived` | all six | records | in slices of 100k |
|---|---|---|---|---|---|---|---|---|---|
| n10000-s1-faults | 12 / 21 | 34 / 54 | 28 / 44 | 7 / 9 | 16 / 26 | 286 / 463 | 467 / 567 | 450,618 | 409 / 572, 5 calls |
| n100000-s1-faults | 168 / 176 | 461 / 495 | 447 / 349 | 93 / 64 | 263 / 207 | 3,467 / 3,649 | 5,315 / 4,876 | 4,963,690 | 4,758 / 5,015, 50 calls |
| n1000000-s1-faults | 1,958 / 3,016 | 5,721 / 9,346 | 4,150 / 6,007 | 931 / 812 | 3,039 / 2,949 | 41,461 / 43,219 | 55,827 / 61,463 | 49,330,886 | 50,671 / 69,720, 494 calls |

The database checks find no fault at any size, as they should: the planted
faults are facts about the data. They add no measurable resident memory
(`rss_added_mb` 0 at every size: a few batches of records). `derived`, the
text index compared with what a rebuild would write (21.3M postings at 1M
nodes), is three quarters of the time and costs about what
`CALL context.index()` costs (36-42 s at 1M).

The data checks, against the truth:

| dataset | `orphans` ms | planted / found | `duplicates` ms | parallel edges planted / found | `Tool` pairs | `Part` pairs | `WorkInstruction` pairs | `required` ms |
|---|---|---|---|---|---|---|---|---|
| n10000-s1-faults | 6 / 5 | 9 / 9 | 29 / 33 | 30 / 30 | 3 / 3, 0 look-alikes | 3 / 3, 0 look-alikes | 5 of 5 among 4,000 | 0 |
| n100000-s1-faults | 56 / 58 | 74 / 74 | 248 / 226 | 249 / 249 | 8 / 8, 0 look-alikes | 9 / 9, 0 look-alikes | 3 of 14 in the first 9,751 (row cap) | 2 |
| n1000000-s1-faults | 835 / 787 | 720 / 720 | 2,611 / 2,345 | 2,277 / 2,277 | 11 / 11, 0 look-alikes | 13 / 13, 0 look-alikes | 0 of 18 in the first 7,723 (row cap) | 27 |

What it says:

- `orphans` and the parallel-edge half of `duplicates` find exactly the
  planted faults at every size, nothing more.
- Node duplicates of `Tool` (on `kind, maker, model, spec`) and `Part` (on
  `part_number, material, size`) are exactly the planted pairs, and no
  look-alike (another specification, another size) is among them. The
  duplicates timings include the parallel-edge half, which always runs.
- `WorkInstruction` on `text, revision, scope` cannot be told apart this
  way: every station of a design carries the same local instruction text,
  so thousands of pairs agree, the planted standard duplicates among them,
  and a single call stops at the 10,000-row cap (continue with `after:`).
  The truth calls those pairs look-alikes; deciding between them takes more
  than equal properties, which is why `check` reports candidates and merges
  nothing (D1). This sort is also the only check that holds memory worth
  measuring: 12-13 MB at 100k and 114-123 MB at 1M, inside its external
  sort's budget of half of `algorithm_memory()` (128 MB at the default
  `work_mem`).
- Slicing costs little: 494 calls of 100,000 records at 1M nodes take
  about what one call takes in memory, and 13% more on the file tier.

## Structure: which nodes are the same shape

Measured on the graphs without faults (`n<size>-s1`), release build, one
run each on a shared machine (`mfg-bench --only structure`; lines
labelled `g7-structure`). Times in ms, memory tier / file tier; every count
is the same on both tiers.

Colours follow the hierarchy and the catalogue outwards
(`HAS_PLANT` ... `HAS_ELEMENT`, `HAS_INSTRUCTION`, `USES_TOOL`, `CONSUMES`,
`FOLLOWS`, `DEPENDS_ON`, `dir: "out"`), with no properties: a station is
seen only through what is under it and what that uses. Distinct colours by
depth, over every node / among the stations:

| dataset | nodes | stations | designs | 0 | 1 | 2 | 3 | 4 | 5 | stable |
|---|---|---|---|---|---|---|---|---|---|---|
| n10000-s1 | 10,055 | 328 | 6 | 14 / 1 | 82 / 2 | 196 / 5 | 259 / 14 | 289 / 34 | 293 / 38 | at depth 5 |
| n100000-s1 | 103,004 | 2,949 | 17 | 14 / 1 | 129 / 2 | 726 / 8 | 1,301 / 47 | 1,532 / 149 | 1,617 / 203 | at depth 9 |
| n1000000-s1 | 1,023,660 | 28,795 | 24 | 14 / 1 | 144 / 2 | 1,844 / 8 | 6,057 / 77 | 7,498 / 364 | 7,989 / 698 | at depth 9 |

**Design recovery.** A design is recovered when its stations without a
recorded deviation share one colour (*whole*) that no station of another
design has (*alone*); a deviation is set apart when the station's colour is
not its design's. Designs whole / alone, deviations apart:

| dataset | depth 1 | depth 2 | depth 3 | depth 4 |
|---|---|---|---|---|
| n10000-s1 | 6 / 0, 0 of 39 | 6 / 4, 0 of 39 | 6 / 6, 17 of 39 | **6 / 6, 39 of 39** |
| n100000-s1 | 17 / 0, 0 of 350 | 17 / 3, 0 of 350 | 17 / 12, 183 of 350 | **17 / 17, 350 of 350** |
| n1000000-s1 | 24 / 0, 0 of 3,600 | 24 / 1, 0 of 3,600 | 24 / 18, 1,782 of 3,600 | **24 / 24, 3,600 of 3,600** |

Depth 4 is the depth this needs and the one to use: station, side,
location, work element, and then what the element uses. Depth 3 sees an
extra work element (the 1,782 at 1M) but not a missing tool, which is four
hops down; below 3 the designs are not yet told apart. Depth 4 recovers
every design and sets apart every deviation at all three sizes, on both
tiers (`station_colours_recover_the_designs_and_set_apart_every_deviation`
in `bench/tests/mfg_structure.rs` asserts it, and that depths 1 to 3 do
not). Deviating stations often share a colour among themselves (the same
tool missing from the same element), which is right: they are built alike.
On the `--faults` graphs a station whose `USES_TOOL` edge was written twice
also leaves its design's colour, since edges are counted with their
multiplicity; it is structurally different, and the truth lists it as a
fault, not as a deviation.

Classes of stations by the kept depth-4 colouring, largest first (1M):
6,720, 3,297, 2,285, 1,618, 1,355, ... down to deviating stations alone:
the designs, each with its deviations split off.

**Time and memory** of a colouring (the call, not the load), memory /
file, and the resident memory it added:

| dataset | depth 4, kept | to depth 5 | stable | added (depth 4) | read back in slices of 10,000 |
|---|---|---|---|---|---|
| n10000-s1 | 22 / 32 | 28 / 27 | 33 / 37 | 0 / 0 MB | 1 slice, under 1 ms |
| n100000-s1 | 290 / 291 | 353 / 413 | 712 / 692 | 4 / 5 MB | 1 slice, under 1 ms |
| n1000000-s1 | 3,977 / 3,518 | 5,398 / 4,228 | 9,452 / 7,769 | 48 / 58 MB | 3 slices, 7 / 8 ms |

About a microsecond per node per round. The memory is the per-node state
(41 bytes a node: 42 MB at 1M) and the sort that counts colours, inside
half and a quarter of `algorithm_memory()` (128 and 64 MB at the default
`work_mem`); past them they spill (tested in `tests/structure.rs`).

**Duplicates by neighbours.** `structure.classes(by: "neighbours")` over
each catalogue label, on the properties its planted duplicates agree on
(`Tool`: `kind, maker, model, spec`; `Part`: `part_number, material,
size`; `WorkInstruction`: `text, revision, scope`), with the edges that say
what an entry hangs off: `SUPPLIED_BY` out for tools; `SUPPLIED_BY`,
`APPLIES_TO` out for parts; `APPLIES_TO`, `ISSUED_BY`, `HAS_INSTRUCTION`
both ways for instructions. Pairs planted / found, look-alikes reported:

| dataset | `Tool` | `Part` | `WorkInstruction` | `WorkInstruction` ms | added |
|---|---|---|---|---|---|
| n10000-s1 | 3 / 3, 0 of 5 | 3 / 3, 0 of 3 | 5 / 5, 0 of 63 | 10 / 10 | 0 MB |
| n100000-s1 | 8 / 8, 0 of 12 | 12 / 12, 0 of 9 | 14 / 14, 0 of 248 | 121 / 120 | 0 MB |
| n1000000-s1 | 11 / 11, 0 of 17 | 13 / 13, 0 of 8 | 18 / 18, 0 of 389 | 1,712 / 1,603 | 24 / 48 MB |

Precision and recall are 1 at every size for all three labels (the 12 at
100k include a part entered four times: one class of 4, six pairs).
`Tool` and `Part` take under a millisecond. Where `check`'s `duplicates`
finds the planted instruction pairs among thousands of look-alikes (the
same design element's instruction at every station of the design, equal on
every property), the neighbours tell them apart: a local instruction
belongs to its own work element through `HAS_INSTRUCTION`, and a planted
duplicate of a standard instruction hangs off the same models and the same
enterprise as its original. At 1M the 454,546 instructions fall into 18
classes of two and 454,510 alone. A class is a candidate, as `check`'s are:
two stations of one design are two stations, and a duplicate pair is for a
person or a rule to decide on; nothing is merged.

## Change tracking: `changes`

`mfg-bench --only changes`, release build, on a shared, busy machine
(load 5-7 on 8 cores). The first version of the journal (`before`: an
index by revision beside every entry and a state record rewritten on
every write) and this one (`after`: one blind put per changed id) were
run back to back by the same harness; lines labelled
`g5-changes-before` and `g5-changes-after` in `bench/results/mfg.jsonl`.
(The lines labelled `g5-changes` are the first version's first
measurement, committed every 10,000 writes.) Each cell: writes a second
with the journal as a share of without it (median of two rounds each),
and in brackets the log bytes a write with it as a multiple of without.

Transactions of 1,000 writes (100,000 writes, or the node count):

| dataset | tier | node set, before | after | edge add, before | after | edge delete, before | after |
|---|---|---|---|---|---|---|---|
| n10000-s1 | memory | 0.32 (7.9x) | 0.41 (2.6x) | 0.32 (11.7x) | 0.48 (3.6x) | 0.19 (26x) | 0.25 (6.1x) |
| n10000-s1 | file | 0.26 (7.9x) | 0.41 (2.6x) | 0.23 (11.7x) | 0.48 (3.6x) | 0.15 (26x) | 0.33 (6.1x) |
| n100000-s1 | memory | 0.26 (7.5x) | 0.38 (2.5x) | 0.36 (11.0x) | 0.56 (3.6x) | 0.24 (26x) | 0.44 (6.4x) |
| n100000-s1 | file | 0.25 (7.5x) | 0.44 (2.5x) | 0.40 (11.0x) | 0.69 (3.6x) | 0.25 (26x) | 0.56 (6.4x) |
| n1000000-s1 | memory | 0.22 (7.5x) | 0.44 (2.5x) | 0.42 (9.7x) | 0.65 (3.5x) | 0.31 (24x) | 0.50 (6.1x) |
| n1000000-s1 | file | 0.31 (7.5x) | 0.46 (2.5x) | 0.50 (9.7x) | 0.67 (3.5x) | 0.33 (24x) | 0.50 (6.1x) |

One write a transaction (10,000 writes):

| dataset | tier | node set, before | after | edge add, before | after | edge delete, before | after |
|---|---|---|---|---|---|---|---|
| n10000-s1 | memory | 0.28 (6.6x) | 0.32 (2.1x) | 0.38 (9.5x) | 0.50 (2.9x) | 0.28 (16x) | 0.49 (4.0x) |
| n10000-s1 | file | 0.35 (6.6x) | 0.52 (2.1x) | 0.44 (9.5x) | 0.63 (2.9x) | 0.40 (16x) | 0.59 (4.0x) |
| n100000-s1 | memory | 0.27 (6.6x) | 0.39 (2.1x) | 0.39 (9.0x) | 0.60 (2.9x) | 0.38 (16x) | 0.53 (4.2x) |
| n100000-s1 | file | 0.32 (6.6x) | 0.71 (2.1x) | 0.37 (9.0x) | 0.76 (2.9x) | 0.38 (16x) | 0.57 (4.2x) |
| n1000000-s1 | memory | 0.27 (6.6x) | 0.25 (2.1x) | 0.51 (7.8x) | 0.61 (2.9x) | 0.45 (15x) | 0.54 (4.1x) |
| n1000000-s1 | file | 0.32 (6.6x) | 0.43 (2.1x) | 0.47 (7.8x) | 0.63 (2.9x) | 0.39 (15x) | 0.56 (4.1x) |

Log bytes a write, the same at every size: node set 20 without, 151
before, 52 after (one write a transaction: 28, 186, 59); edge add 28,
305, 98 (37, 331, 107); edge delete 13, 341, 84 (22, 350, 93). Without
the journal, the 100k memory rates were 477k node sets, 119k edge adds and
177k edge deletes a second in 1,000-write transactions, and 232k, 57k and
61k one a transaction.

Reading, after one round of 3 x N writes:

| | 10k | 100k | 1M |
|---|---|---|---|
| entries | 39,971 | 300,815 | 408,198 |
| bytes an entry, before / after | 54 / 16.5 | 54 / 17.6 | 54 / 17.8 |
| `since` over every entry, 10,000 a call, before / after | 5 / 5-6 ms | 38 / 38-42 ms | 51-61 / 50-52 ms |
| `since` for the last commit, before / after | 0.01 / 1.0-1.3 ms | 0.01 / 7.8-8.3 ms | 0.01 / 10.6-10.8 ms |
| `changes::changed`, 1,000 ids | 0.25-0.37 ms | 0.29-0.31 ms | 0.29-0.51 ms |
| `changes::changed_any` when nothing changed, before / after | < 1 µs / 0.5-0.7 ms | < 1 µs / 2.8-3.1 ms | ~1 µs / 2.9 ms |

What it says:

- **Writing less per change paid off by about half.** With one blind put
  per changed id and nothing else, node writes run at 0.38-0.46 of their
  speed in 1,000-write transactions (first version 0.22-0.32), edge adds
  at 0.48-0.69 (0.23-0.50) and edge deletes at 0.25-0.56 (0.15-0.33). The
  log grows 2.5 times a node write's bytes (7.5 before), 3.5 times an edge
  add's (11) and 6 times an edge delete's (26).
- **The targets were 60% for node writes, 50% for edge writes, and about 3
  times the log bytes.** Edge adds meet them (0.48-0.69, 3.5x). Node
  writes do not (best 0.46), nor do edge delete bytes (6x), and edge delete
  speed is at 0.5 only from 100k nodes up. What stands in the way,
  measured with a hook that does nothing on a 100,000-node memory graph:
  that hook alone leaves node writes at about 0.70 of their speed (the
  engine builds the node's before and after images and wraps every hooked
  write in a unit, G4), and one entry is one insert into the extension tree,
  about as dear as the node property set it describes (about 0.9 µs
  against 1.1 µs). The read of the journal's state that keeps the cap
  costs about 0.5 µs more. An edge delete logs 13 bytes, and the smallest
  entry is 15 bytes in the log, three of them (the edge and both ends)
  with namespace and lengths about 63.
- **Why not once per transaction.** Gathering the ids a transaction
  touched and writing them at commit would save the per-write record and
  merge repeated ids, but not the tree insert per entry nor the hook's own
  cost, so it cannot bring node writes to 0.6 either. Only packing a
  transaction's ids into one value would, and then `changed` for a set of
  ids, which the vector slice depends on, becomes a scan of the journal.
  So this slice does not change the commit path.
- **Reads got slower where expected.** With no index by revision, `since`
  scans the entries of the kinds asked for whatever the revision: the
  whole journal in slices costs what it did (about 7-8 million entries a
  second), but the last commit's changes now take 8-11 ms at 300,000-400,000
  entries instead of 0.01 ms, and `changed_any` with nothing changed takes
  3 ms. `changed` for 1,000 ids is unchanged at about 0.3 ms.
- **A database without the journal pays nothing.** The engine did not
  change.

### After the review (`g5-changes-review`)

The review found that one write paid for the whole cap: at the default
cap the write that trimmed took 133 ms and logged 1.7 MB. Now the cap is
paid a slice at a time (docs/DERIVED.md, "The cap"), and the measurement
was run again by the same harness (`mfg-bench --only changes --label
g5-changes-review`, same busy machine).

Ordinary writes, with the journal far from its cap, are where they were
(share of write speed kept, log bytes as a multiple of without):

| dataset | tier | per transaction | node set | edge add | edge delete |
|---|---|---|---|---|---|
| n10000-s1 | memory | 1,000 | 0.39 (2.6x) | 0.47 (3.9x) | 0.25 (6.9x) |
| n100000-s1 | memory | 1,000 | 0.55 (2.5x) | 0.66 (3.8x) | 0.34 (7.2x) |
| n100000-s1 | file | 1,000 | 0.67 (2.5x) | 0.57 (3.8x) | 0.33 (7.2x) |
| n1000000-s1 | memory | 1,000 | 0.42 (2.5x) | 0.63 (3.8x) | 0.47 (7.1x) |
| n1000000-s1 | file | 1,000 | 0.36 (2.5x) | 0.62 (3.8x) | 0.48 (7.1x) |
| n100000-s1 | memory | 1 | 0.56 (2.1x) | 0.61 (2.9x) | 0.41 (4.4x) |
| n1000000-s1 | memory | 1 | 0.25 (6.8x) | 0.59 (2.9x) | 0.51 (4.1x) |

(The 1M single node sets ran while the journal was sweeping, so each
rewrote the state: 6.8x.)

Sweeping all the time (`changes.cap`, the cap a fifth of the writes):

| dataset | tier | cap | node sets/s, 1,000 a transaction | log bytes a write | single writes/s | slowest single write | largest single write's log |
|---|---|---|---|---|---|---|---|
| n10000-s1 | memory | 2,011 | 171k | 98 | 91k | 64 µs | 1,108 B |
| n10000-s1 | file | 2,011 | 166k | 98 | 68k | 72 µs | 1,108 B |
| n100000-s1 | memory | 20,000 | 155k | 101 | 69k | 106 µs | 1,174 B |
| n100000-s1 | file | 20,000 | 141k | 101 | 40k | 349 µs | 1,174 B |
| n1000000-s1 | memory | 20,000 | 141k | 101 | 41k | 572 µs | 1,175 B |
| n1000000-s1 | file | 20,000 | 83k | 101 | 29k | 184 µs | 1,175 B |

At the default cap of 1,000,000 (`tests/review_g5_cap.rs`), the write
that raises the floor now takes under a millisecond and logs 276 bytes
(it was 133 ms and 1.7 MB); no write logs more than one state record and
64 deletes beside its own. Reading is unchanged: `since` over the whole
journal 37-98 ms at 300,000-400,000 entries, for the last commit 9-19 ms,
`changed` for 1,000 ids 0.2-0.7 ms, `changed_any` with nothing changed
3-5 ms (it reads every entry of the kind; knowing the newest position
held would answer it at once, but keeping that exact means a write per
change, which is what this slice took out).

## Vectors: `vector`

`mfg-bench --only vector`, release build, one run each on a shared, busy
machine (load 5-7 on 8 cores); lines labelled `g10-vector` in
`bench/results/mfg.jsonl`. Seeded synthetic vectors (uniform in [-1, 1)),
one per node, cosine, with the change journal installed; the 1M memory
tier was run at 64 and 384 dimensions only (768 would hold 3.2 GB of
vectors in memory beside the graph on a machine with about 11 GB free).

**Bytes.** A vector costs 4 bytes a dimension and 48 beside (two records,
keys with their namespace), plus its hash (16 bytes in the run): 5.00
bytes a dimension at 64, 4.17 at 384, 4.08 at 768. The same vectors as a
list property log 9.2-9.9 bytes a dimension (631, 3,524 and 6,987 bytes a
vector against the space's 325, 1,605 and 3,141), the same at every size.
The memory tier's pages grow 386, 1,739 and 3,381 bytes a vector for the
space against 1,140, 4,370 and 16,384 for the list property. (Growth of a
file is not a per-vector figure: freed pages are reused, so it reads 0 at
some sizes.)

**Put**, through statements, 1,000 vectors a call: 28-40k vectors a second
at 64 dimensions, 5-8k at 384, 2.5-3.6k at 768, at every size and on both
tiers. The time goes into passing 1,000 lists of floats through a
statement; the store itself is one extension record a call.

**Search**, top 10, every vector scored, as first built (label
`g10-vector`): every search counted the space's stale, missing and
unknown vectors (one metadata read and one journal lookup a vector, plus
the label's members). `ms` is the statement, `scan` the Rust
`vector::search` alone. Superseded by "Search after currency" below.

| dataset | tier | dims | ms p50 | scan ms p50 | scored/s (statement) | scored/s (scan) | `WorkInstruction` only, ms p50 |
|---|---|---|---|---|---|---|---|
| n10000-s1 | memory | 64 | 10 | 1.8 | 1.0M | 5.6M | 11 |
| n10000-s1 | memory | 384 | 12 | 5.6 | 0.8M | 1.8M | 15 |
| n10000-s1 | memory | 768 | 16 | 9.5 | 0.6M | 1.1M | 19 |
| n100000-s1 | memory | 64 | 136 | 20 | 0.8M | 5.3M | 102 |
| n100000-s1 | memory | 384 | 179 | 83 | 0.6M | 1.2M | 139 |
| n100000-s1 | memory | 768 | 187 | 97 | 0.6M | 1.1M | 202 |
| n100000-s1 | file | 64 | 112 | 18 | 0.9M | 5.7M | 82 |
| n100000-s1 | file | 384 | 178 | 82 | 0.6M | 1.3M | 127 |
| n100000-s1 | file | 768 | 166 | 100 | 0.6M | 1.0M | 207 |
| n1000000-s1 | memory | 64 | 744 | 111 | 1.4M | 9.2M | 828 |
| n1000000-s1 | memory | 384 | 1,457 | 552 | 0.7M | 1.9M | 1,078 |
| n1000000-s1 | file | 64 | 693 | 114 | 1.5M | 9.0M | 807 |
| n1000000-s1 | file | 384 | 2,452 | 1,803 | 0.4M | 0.6M | 2,983 |
| n1000000-s1 | file | 768 | 4,216 | 3,300 | 0.2M | 0.3M | 3,614 |

(The 10k file tier is in the results file and agrees with the memory tier.)
Resident memory did not grow during any search (`rss_added_mb` 0 at every
size): the heap holds ten candidates and the scan a page at a time. At 1M
vectors of 768 dimensions on the file tier the process stayed at 1.2 GB
resident while the space held 3.2 GB.

What this says: the scan runs at about 1-2 million 384-dimension vectors a
second from memory and 0.6 million from a file larger than its cache, so
exact search answers in about 0.1 s at 100k vectors and 0.5-3.3 s at a
million. Counting staleness is the larger cost at low dimensions (0.6 s of
0.74 s at 1M x 64): it reads every vector's metadata and journal entry. A
label restriction reads candidates one by one through the label index,
which is slower per vector than the scan, so it pays only when the label
is a small part of the space. Whether an approximate index is needed
(G17) depends on the sizes that matter; at a million vectors, exact search
is seconds, not milliseconds.

**Staleness after a small change** (100 nodes' properties set): `stale`
lists exactly those 100, in 1 call at 10k, 2 at 100k and 15 at 1M (it
looks at at most 100,000 records a call): 15-20 ms at 10k, 120-210 ms at
100k, 1.2-1.7 s for the whole list at 1M. Dropping a space of 1M vectors
takes 1-2 s in memory and 34 s on the file tier at 384-768 dimensions.

**Search after currency** (label `g10-vector-current`). A search no longer
counts: it reports whether the space is current from the record a count
left (`yes` here, after one `counts: true` search) and, when something was
written since, from the journal's entries since then. Run back to back in
one process, so the columns compare under the same load: the search as a
caller asks it (`report`), the scan alone, the search with `counts: true`
(what every search paid before), and `fresh: "require"` on a current
space. `before` is the first version's statement, from its own run (above).
All in ms, p50 of 20 queries (5 at 1M), every vector a candidate.

| dataset | tier | dims | before | report | scan | report / scan | `counts: true` | `require` | after a change |
|---|---|---|---|---|---|---|---|---|---|
| n10000-s1 | memory | 64 | 10.3 | 1.0 | 0.9 | 1.11 | 5.9 | 1.0 | 1.3 |
| n10000-s1 | memory | 384 | 11.9 | 5.1 | 5.0 | 1.02 | 10.6 | 5.1 | 5.3 |
| n10000-s1 | memory | 768 | 16.0 | 9.2 | 8.9 | 1.03 | 14.8 | 9.2 | 10.2 |
| n10000-s1 | file | 64 | 10.2 | 0.9 | 0.9 | 1.00 | 5.8 | 1.0 | 1.6 |
| n10000-s1 | file | 384 | 18.7 | 5.1 | 5.0 | 1.02 | 10.6 | 5.2 | 5.6 |
| n10000-s1 | file | 768 | 25.6 | 9.4 | 9.2 | 1.02 | 15.0 | 9.5 | 9.9 |
| n100000-s1 | memory | 64 | 136.0 | 11.1 | 10.9 | 1.02 | 65.2 | 11.1 | 11.5 |
| n100000-s1 | memory | 384 | 178.5 | 50.9 | 50.6 | 1.01 | 105.6 | 50.7 | 50.9 |
| n100000-s1 | memory | 768 | 187.1 | 95.0 | 97.2 | 0.98 | 154.1 | 95.5 | 98.8 |
| n100000-s1 | file | 64 | 111.7 | 14.0 | 14.0 | 1.00 | 95.3 | 14.2 | 11.9 |
| n100000-s1 | file | 384 | 178.1 | 54.2 | 54.7 | 0.99 | 117.5 | 54.5 | 54.8 |
| n100000-s1 | file | 768 | 166.3 | 95.1 | 95.1 | 1.00 | 154.3 | 95.3 | 101.2 |
| n1000000-s1 | memory | 64 | 743.9 | 108.6 | 108.5 | 1.00 | 671.9 | 107.6 | 110.5 |
| n1000000-s1 | memory | 384 | 1456.5 | 533.9 | 529.4 | 1.01 | 1229.4 | 543.7 | 506.0 |
| n1000000-s1 | file | 64 | 692.9 | 111.5 | 111.6 | 1.00 | 684.2 | 111.3 | 112.8 |
| n1000000-s1 | file | 384 | 2452.0 | 1584.0 | 1574.7 | 1.01 | 2262.3 | 1567.0 | 1570.9 |
| n1000000-s1 | file | 768 | 4215.7 | 3159.0 | 3156.6 | 1.00 | 3850.1 | 3155.1 | 3151.7 |

Restricted to `WorkInstruction` (454,546 of 1,023,660 at 1M), report
against scan: 1.6 / 1.5 ms at 10k x 64, 41 / 41 ms at 100k x 384, 424 /
424 ms at 1M x 384 in memory, 2,822 / 2,770 at 1M x 768 from a file; the
widest gap measured was 34 / 31 ms (100k, file, 64 dims), within a tenth.
"After a change" is one search after properties were set on 100 nodes
with vectors: the record no longer ends the log, so the journal's entries
since are walked until the first changed vector, and the search reports
`no` naming it, at the scan's cost. Counting once (`establish_ms`, the
same as `counts: true`) costs what one search used to cost, and is paid
again only after the space changes. Resident memory did not grow during
any search at any size. (The `before` column comes from a run some hours
earlier under a different load; the scan itself was 0.55 s then and 0.53 s
now at 1M x 384 in memory, 1.80 s and 1.57 s from a file.)
