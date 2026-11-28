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
          [--only stats,retrieve,pagerank,communities] [--out FILE] [--label NAME] [--detail]
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
