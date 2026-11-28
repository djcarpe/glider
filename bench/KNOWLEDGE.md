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
          [--only stats,retrieve,pagerank,communities,check,structure,templates] [--out FILE] [--label NAME] [--detail]
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

| `structure.templates` | over `HAS_PLANT` ... `HAS_INSTRUCTION` outwards: `what: "design recovery"` (roots of `Station`): `templates`, `designs`, `designs_eligible` (two or more stations without a deviation), `designs_recovered` (exactly one template's instances), `deviated`, `deviated_in_design` (deviated stations among a design's template's instances: must be 0), `deviation_templates`, `exact`, `instances_ms`/`instances_calls` to list every instance; `what: "all shapes"` (every root): `templates`, `by_root` as `[label, templates, instances, largest]` |
| `structure.paths` | `what: "patterns"`: the first pattern (`top`, `top_hops`, `top_paths`, `top_maximal`); `what: "chains"`: chains in the first slice |
| `structure.plan` | `what: "whole graph"`, kept as `mfg`: `plain_*`, `compact_*`, `dictionary_*` (bytes and tokens), `saving_tokens`, `uses`, `templates_chosen`, `templates_nested`, `nested_standalone_uses`, `patterns_chosen`, `candidates`, `kept_stored_bytes`; `what: "one station"` / `"one line"` / `"300 scattered nodes"` (the first station without a deviation and its line, with everything below; ids from a fixed stream): `plain_*`, `alone_*` (planned with its dictionary inline), `kept_*` and `kept_uses` (written with the kept dictionary, not repeated), `ms_alone`, `ms_kept_min`, `ms_kept_median` (of five), `kept_rss_added_mb`, `ms_whole_graph_path` and `same_text_as_whole_graph_path` (`find: "graph"`) |
| `structure.expand` | every instance of the kept dictionary, expanded 1,000 roots a call and compared with the graph: `templates`, `instances`, `exact`, `all_exact`, `ms`, `calls`, memory, `list_ms`/`list_calls` |

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

## Templates: a repeated shape said once

Measured on the graphs without faults (`n<size>-s1`), release build, one
run each on a shared machine (`mfg-bench --only templates`; lines labelled
`g8-templates`). The hierarchy is `HAS_PLANT` ... `HAS_INSTRUCTION`,
outwards; tools, parts, standards and dependencies are references and
internal edges of the shapes. Every count, byte and token below is the same
on both tiers; times are memory / file, in ms unless marked.

**Designs.** Templates of stations, against the truth:

| dataset | stations | designs | recovered | deviated | deviated in a design's template | other station templates |
|---|---|---|---|---|---|---|
| n10000-s1 | 328 | 6 | **6 of 6** | 39 | 0 | 2 |
| n100000-s1 | 2,949 | 17 | **17 of 17** | 350 | 0 | 37 |
| n1000000-s1 | 28,795 | 24 | **24 of 24** | 3,600 | 0 | 292 |

Every design is a template whose instances are exactly its stations
without a recorded deviation, and no deviating station is an instance of
its design's template. The other station templates are stations that
deviate the same way (the same tool missing from the same element), which
are alike and say so exactly.

**Smaller shapes.** With every node a possible root: 51 templates at 10k
(9 of sides, 9 of locations, 25 of work elements with their instructions,
8 of stations), 219 at 100k, 878 at 1M. The plan nests them in station
definitions (`.k=Tm`) and uses them on their own for the parts of
deviating stations they fit: 31 of the 41 chosen templates at 10k are
nested in others (107 uses on their own), 94 of 149 at 100k (764), 151 of
467 at 1M (15,800).

**The long path.** The first pattern at every size is the whole hierarchy,
`(:Enterprise) -HAS_PLANT-> (:Plant) -HAS_SHOP-> (:Shop) -HAS_LINE-> (:Line)
-HAS_STATION-> (:Station) -HAS_SIDE-> (:Side) -HAS_LOCATION->
(:WorkLocation) -HAS_ELEMENT-> (:WorkElement) -HAS_INSTRUCTION->
(:WorkInstruction)`, 8 hops, followed by 4,075 / 45,272 / 454,308 paths:
one per local instruction. Chains (`-HAS_ELEMENT-> (:WorkElement)
-HAS_INSTRUCTION->` from a location through an element with one
instruction, or a station with one side) are chosen only where writing them
as one hop pays: 1, 4 and 6 patterns.

**Bytes and tokens**, plain against planned:

| dataset | part | plain | planned, dictionary inline | with the kept dictionary | dictionary |
|---|---|---|---|---|---|
| n10000-s1 | whole graph (10,055 nodes) | 2.59 MB, 950,841 tokens | 514 KB, 218,405 (23%) | | 49.8 KB, 18,162 |
| n10000-s1 | one station (40 nodes) | 10,514 B, 3,734 | not shorter: plain | 905 B, 441 (12%) | |
| n10000-s1 | one line (345 nodes) | 90,056 B, 31,947 | 56,835 B, 20,979 (66%) | 13,896 B, 5,919 (19%) | |
| n100000-s1 | whole graph (103,004) | 28.8 MB, 10,395,457 | 4.70 MB, 2,042,178 (20%) | | 310 KB, 113,170 |
| n100000-s1 | one station (18) | 4,893 B, 1,770 | plain | 480 B, 223 (13%) | |
| n100000-s1 | one line (361) | 100,826 B, 35,821 | 56,082 B, 20,865 (58%) | 13,318 B, 5,827 (16%) | |
| n1000000-s1 | whole graph (1,023,660) | 291 MB, 103,283,695 | 42.0 MB, 18,305,719 (18%) | | 1.66 MB, 610,774 |
| n1000000-s1 | one station (8) | 1,889 B, 683 | plain | 263 B, 109 (16%) | |
| n1000000-s1 | one line (447) | 126,292 B, 45,525 | 83,710 B, 31,305 (69%) | 10,310 B, 5,034 (11%) | |

(Percentages are of the plain tokens; "one station" is the first station
without a deviation, a different design at each size.) The whole graph
written with templates takes a fifth of the tokens, measured by writing
both. A single station planned alone is not shorter: a dictionary for one
instance costs more than it saves, so nothing is chosen. With the kept
dictionary given to the model once, a station is one use line and a line a
tenth to a fifth of its plain text: that is the case `context`'s compact
form (G16) is for.

**Expansion.** Every instance of the kept dictionary (2,149 / 18,903 /
192,865 instances of 41 / 149 / 467 templates, nested ones included)
expanded and compared with the graph, node, edge and value: **all exact**,
at every size, on both tiers.

**Time and memory** of the calls (the graph already loaded), memory /
file, and the resident memory added in memory:

| dataset | station templates | all shapes | patterns | plan, whole graph | one station planned alone (ms) / with the kept dictionary (ms) | expand, every instance | list every instance (calls) |
|---|---|---|---|---|---|---|---|
| n10000-s1 | 196 / 204 | 408 / 519 | 30 / 19 | 682 / 584, +1 MB | 364 / 3 | 286 / 361, 41 calls | 581 / 570 (2) |
| n100000-s1 | 2,632 / 2,096 | 5,175 / 5,508 | 232 / 237 | 6,645 / 8,352, +12 MB | 5,042 / 2 | 4,337 / 3,899, 149 calls | 7,788 / 7,277 (2) |
| n1000000-s1 | 23.6 / 24.2 s | 50.4 / 48.5 s | 2.5 / 2.6 s | 109 / 131 s, +115 MB | 48 s / 2.5 ms | 49 / 45 s, 570 calls | 121 / 114 s (3) |

Finding templates is a whole-graph computation, about 50 µs a node at 1M
with every node a root: the forest (56 bytes a node), then a walk of every
root whose forest shape repeats, twice more for each group's members, with
their plain and compact text priced. A plan adds nesting, the chains and
writing both texts in full (291 MB and 42 MB at 1M, counted, not kept). A
plan of a few nodes without a kept dictionary costs what finding templates
costs; with one, it reads only the selection (below). Expansion reads only
the shape under each root (about 0.25 ms an instance). The whole-graph
plan times are from two runs on a shared machine (78-87 s, then 109-131 s
at 1M).
The memory is the per-node state and the sorts, inside half and a quarter
of `algorithm_memory()`, spilling past them
(`forest_state_spills_past_its_budget_with_the_same_hashes`,
`state_spills_past_the_memory_budget_and_nothing_changes`).

**A selection written with the kept dictionary** (`plan(dictionary:
"mfg", nodes: [...], text: "compact")`), the call an agent waits on: it
reads the selection, the owners above it, the dictionary's shape index and
the definitions the selection can use, never the rest of the graph. Best
and median of five calls in ms, memory / file; "same" is whether the text
equals, byte for byte, what the whole-graph path (`find: "graph"`, its time
in the last column, memory tier) writes for the same selection:

| dataset | selection | nodes | plain tokens | kept tokens | uses | ms | same | whole-graph path |
|---|---|---|---|---|---|---|---|---|
| n10000-s1 | one station | 40 | 3,734 | 441 | 1 | 2.8 / 2.8 | yes | 320 ms |
| n10000-s1 | one line | 345 | 31,947 | 5,919 | 17 | 18 / 27 | yes | 294 ms |
| n10000-s1 | 300 scattered nodes | 300 | 27,975 | 27,809 | 1 | 7.7 / 7.5 | yes | 319 ms |
| n100000-s1 | one station | 18 | 1,770 | 223 | 1 | 2.1 / 2.2 | yes | 3.5 s |
| n100000-s1 | one line | 361 | 35,821 | 5,827 | 17 | 24 / 23 | yes | 3.6 s |
| n100000-s1 | 300 scattered nodes | 300 | 30,636 | 30,636 | 0 | 15 / 13 | yes | 3.4 s |
| n1000000-s1 | one station | 8 | 683 | 109 | 1 | 2.5 / 2.1 | yes | 39.5 s |
| n1000000-s1 | one line | 447 | 45,525 | 5,034 | 13 | 39 / 71 (median 86) | yes | 37.1 s |
| n1000000-s1 | 300 scattered nodes | 300 | 29,946 | 29,946 | 0 | 15 / 17 | yes | 38.2 s |

The time follows the selection (about 0.05 ms a selected node in memory,
up to 0.2 ms on the file tier for the pages of a line not yet read), not
the graph: a station takes 2-3 ms at 10k and at 1M. No memory is added
that the resident size shows. Scattered nodes are mostly not whole
instances (a node without the rest of its shape is written plainly), so
their text is the plain text. Nothing about which node is in which
instance is kept: it is worked out from the selection each call and every
use is matched in full, so a node changed since the dictionary was made is
written as it is now. What the dictionary keeps for this is an index of
root shapes and ids, about 50 bytes a template (24 KB of the 7.0 MB that
the kept dictionary `mfg` takes at 1M, all its records counted:
`stored_bytes`; 211 KB at 10k, 1.36 MB at 100k).
