#!/usr/bin/env python3
"""Render bench/results/neo4j-<dataset>.jsonl as one HTML page.

    python3 bench/neo4j_report.py [--dataset recommendations] [--out bench/neo4j.html]
"""

import argparse
import html
import json
import math
import os
import statistics

HERE = os.path.dirname(os.path.abspath(__file__))


def esc(s):
    return html.escape("" if s is None else str(s))


def fmt_ms(v):
    if v is None:
        return "—"
    if v < 1:
        return f"{v * 1000:.0f} µs"
    if v < 1000:
        return f"{v:.2f} ms" if v < 10 else f"{v:.1f} ms"
    return f"{v / 1000:.2f} s"


def ratio(n, g):
    if n is None or g is None or n <= 0 or g <= 0:
        return None
    return n / g


def ratio_text(r):
    if r is None:
        return ""
    if r >= 1:
        return f"glider {r:.1f}×"
    return f"neo4j {1 / r:.1f}×"


def bar(ms_value, scale_max, cls):
    if ms_value is None:
        return '<span class="bar none"></span>'
    w = 100 * math.log10(ms_value + 1) / math.log10(scale_max + 1) if scale_max > 0 else 0
    return f'<span class="bar {cls}" style="width:{max(w, 1.5):.1f}%"></span>'


def agreement(rec):
    m = rec.get("match")
    if m is None:
        return '<span class="agree unknown" title="not compared">·</span>'
    if isinstance(m, bool):
        return '<span class="agree ok" title="same answer">✓</span>' if m else '<span class="agree bad" title="different answers">✗</span>'
    of = rec.get("match_of", 10)
    cls = "ok" if m >= of * 0.7 else "bad"
    return f'<span class="agree {cls}" title="top-ten overlap">{m}/{of}</span>'


def table(rows, kind):
    if not rows:
        return ""
    warm = [v for r in rows for v in (r["neo4j"].get("warm_ms"), r["glider"].get("warm_ms")) if v is not None]
    scale_max = max(warm) if warm else 1
    out = ['<div class="tablewrap"><table><thead><tr><th>query</th><th>agree</th><th class="num">Neo4j warm</th>'
           '<th class="num">glider warm</th><th class="bars">warm, log scale</th><th class="num">ratio</th></tr></thead><tbody>']
    for r in rows:
        n, g = r["neo4j"], r["glider"]
        rt = ratio(n.get("warm_ms"), g.get("warm_ms"))
        # The Bolt driver reports Neo4j's time in whole milliseconds, so a
        # sub-millisecond Neo4j answer reads as 1–3 ms and the ratio is a floor.
        coarse = n.get("warm_ms") is not None and n.get("warm_ms") <= 3
        err = n.get("error") or g.get("error")
        note = r.get("note", "")
        cy = g.get("cypher") or n.get("cypher") or ""
        same = r.get("same_query", True)
        out.append("<tr>")
        out.append(f'<td class="q"><div class="name">{esc(r["name"])}</div><div class="note">{esc(note)}'
                   f'{"" if same else " · <em>different Cypher on each side</em>"}</div>'
                   f'<details><summary>cypher</summary><pre>{esc(cy)}</pre>'
                   + (f'<pre class="alt">{esc(n.get("cypher"))}</pre>' if not same and n.get("cypher") else "")
                   + '</details></td>')
        out.append(f"<td>{agreement(r)}</td>")
        out.append(f'<td class="num">{fmt_ms(n.get("warm_ms"))}<div class="cold">cold {fmt_ms(n.get("cold_ms"))}</div></td>')
        out.append(f'<td class="num">{fmt_ms(g.get("warm_ms"))}<div class="cold">cold {fmt_ms(g.get("cold_ms"))}</div></td>')
        out.append(f'<td class="bars"><div class="pair">{bar(n.get("warm_ms"), scale_max, "neo")}{bar(g.get("warm_ms"), scale_max, "gli")}</div></td>')
        cls = "win" if rt and rt >= 1.15 else ("lose" if rt and rt <= 1 / 1.15 else "even")
        out.append(f'<td class="num ratio {cls}">{"≥ " if coarse and rt and rt >= 1 else ""}{esc(ratio_text(rt))}</td>')
        out.append("</tr>")
        if err:
            out.append(f'<tr class="err"><td colspan="6">{esc(err)}</td></tr>')
    out.append("</tbody></table></div>")
    return "".join(out)


def summary(reads, algos, writes):
    # Only rows Neo4j's whole-millisecond timer can resolve go into the mean;
    # a 2 ms floor against 12 µs would say 166× and mean nothing.
    def gm(rows):
        rs = [ratio(r["neo4j"].get("warm_ms"), r["glider"].get("warm_ms")) for r in rows
              if (r["neo4j"].get("warm_ms") or 0) > 3]
        rs = [x for x in rs if x]
        return math.exp(statistics.mean(math.log(x) for x in rs)) if rs else None

    def wins(rows):
        rs = [ratio(r["neo4j"].get("warm_ms"), r["glider"].get("warm_ms")) for r in rows]
        rs = [x for x in rs if x]
        return sum(1 for x in rs if x > 1), len(rs)

    agree = [r for r in reads + algos if r.get("match") is not None]
    ok = sum(1 for r in agree if r["match"] is True or (isinstance(r["match"], int) and not isinstance(r["match"], bool) and r["match"] >= 7))
    tiles = []
    for label, rows in (("reads", reads), ("algorithms", [a for a in algos if a["name"] != "gds projection"]), ("writes", writes)):
        g = gm(rows)
        w, n = wins(rows)
        if n:
            resolved = sum(1 for r in rows if (r["neo4j"].get("warm_ms") or 0) > 3)
            tiles.append(f'<div class="tile"><div class="k">{label}</div><div class="v">{ratio_text(g) if g else "—"}</div>'
                         f'<div class="s">geometric mean of warm ratios over the {resolved} Neo4j can time (≥ 4 ms) · glider faster on {w} of {n}</div></div>')
    tiles.append(f'<div class="tile"><div class="k">answers</div><div class="v">{ok} / {len(agree)}</div>'
                 f'<div class="s">queries and algorithms where both engines agreed</div></div>')
    return '<div class="tiles">' + "".join(tiles) + "</div>"


def render(recs, dataset):
    meta = next((r for r in recs if r["phase"] == "meta"), {})
    reads = [r for r in recs if r["phase"] == "read"]
    algos = [r for r in recs if r["phase"] == "algo"]
    writes = [r for r in recs if r["phase"] == "write"]
    nn = meta.get("neo4j", {})
    gg = meta.get("glider", {})
    title = {"recommendations": "Recommendations Graph Bench", "stackoverflow": "StackOverflow Graph Bench"}.get(dataset, "Graph Bench")
    return f"""<title>{esc(title)}</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Sans:wght@400;500;600&family=IBM+Plex+Mono:wght@400;500&display=swap">
<style>
:root {{
  --bg: #f3f5f7; --paper: #ffffff; --ink: #1b2430; --muted: #5b6673; --line: #d9dee4;
  --gli: #2f6f9f; --neo: #4e8a4a; --good: #2f7d4f; --bad: #b8532b; --even: #7a8390;
  color-scheme: light;
}}
@media (prefers-color-scheme: dark) {{ :root:not([data-theme="light"]) {{
  --bg: #101418; --paper: #171c22; --ink: #e6eaee; --muted: #9aa5b1; --line: #2a323b;
  --gli: #6fb0e0; --neo: #7cc276; --good: #6fc48f; --bad: #e08a63; --even: #8f99a5; color-scheme: dark; }} }}
:root[data-theme="dark"] {{
  --bg: #101418; --paper: #171c22; --ink: #e6eaee; --muted: #9aa5b1; --line: #2a323b;
  --gli: #6fb0e0; --neo: #7cc276; --good: #6fc48f; --bad: #e08a63; --even: #8f99a5; color-scheme: dark; }}
body {{ background: var(--bg); color: var(--ink); font: 15px/1.5 "IBM Plex Sans", system-ui, sans-serif; padding-block: 32px; padding-inline: 16px; }}
main {{ max-width: 1080px; margin: 0 auto; }}
h1 {{ font-size: 28px; font-weight: 600; margin: 0 0 4px; text-wrap: balance; }}
h2 {{ font-size: 18px; font-weight: 600; margin: 40px 0 12px; }}
.lede {{ color: var(--muted); max-width: 68ch; margin: 0 0 20px; }}
.facts {{ display: flex; flex-wrap: wrap; gap: 8px 24px; font-family: "IBM Plex Mono", ui-monospace, monospace; font-size: 13px; color: var(--muted); margin-bottom: 24px; }}
.facts b {{ color: var(--ink); font-weight: 500; }}
.tiles {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 12px; }}
.tile {{ background: var(--paper); border: 1px solid var(--line); padding: 14px 16px; }}
.tile .k {{ font-size: 12px; letter-spacing: .06em; text-transform: uppercase; color: var(--muted); }}
.tile .v {{ font-size: 26px; font-weight: 600; font-variant-numeric: tabular-nums; margin: 2px 0; }}
.tile .s {{ font-size: 12px; color: var(--muted); }}
.legend {{ display: flex; gap: 18px; font-size: 13px; color: var(--muted); margin: 8px 0 12px; }}
.legend i {{ display: inline-block; width: 22px; height: 10px; vertical-align: middle; margin-right: 6px; }}
.legend .n {{ background: var(--neo); }} .legend .g {{ background: var(--gli); }}
.tablewrap {{ overflow-x: auto; background: var(--paper); border: 1px solid var(--line); }}
table {{ border-collapse: collapse; width: 100%; min-width: 760px; }}
th, td {{ padding: 8px 10px; border-top: 1px solid var(--line); vertical-align: top; text-align: left; }}
th {{ font-size: 12px; letter-spacing: .05em; text-transform: uppercase; color: var(--muted); font-weight: 500; border-top: 0; }}
td.num, th.num {{ text-align: right; font-family: "IBM Plex Mono", ui-monospace, monospace; font-variant-numeric: tabular-nums; white-space: nowrap; }}
td .cold {{ font-size: 11px; color: var(--muted); }}
td.q {{ min-width: 260px; }}
td.q .name {{ font-weight: 500; }}
td.q .note {{ font-size: 12px; color: var(--muted); }}
details summary {{ font-size: 12px; color: var(--muted); cursor: pointer; margin-top: 2px; }}
pre {{ font: 12px/1.45 "IBM Plex Mono", ui-monospace, monospace; white-space: pre-wrap; background: var(--bg); padding: 8px; margin: 6px 0 0; }}
pre.alt::before {{ content: "neo4j: "; color: var(--muted); }}
td.bars {{ width: 26%; min-width: 180px; }}
.pair {{ display: flex; flex-direction: column; gap: 3px; }}
.bar {{ display: block; height: 9px; }}
.bar.neo {{ background: var(--neo); }} .bar.gli {{ background: var(--gli); }} .bar.none {{ width: 0; }}
td.ratio.win {{ color: var(--gli); font-weight: 500; }} td.ratio.lose {{ color: var(--neo); font-weight: 500; }} td.ratio.even {{ color: var(--even); }}
.agree {{ font-family: "IBM Plex Mono", ui-monospace, monospace; }}
.agree.ok {{ color: var(--good); }} .agree.bad {{ color: var(--bad); font-weight: 600; }} .agree.unknown {{ color: var(--muted); }}
tr.err td {{ color: var(--bad); font-size: 12px; font-family: "IBM Plex Mono", ui-monospace, monospace; border-top: 0; padding-top: 0; }}
.method {{ max-width: 72ch; color: var(--ink); }}
.method li {{ margin-bottom: 6px; }}
</style>
<main>
<h1>{esc(title)}</h1>
<p class="lede">glider against Neo4j {esc(nn.get("version", meta.get("neo4j_version", "5")))} on Neo4j's own <b>{esc(dataset)}</b> example graph, the same Cypher on both sides wherever glider's subset allows it, every answer checked across engines.</p>
<div class="facts">
  <span>nodes <b>{nn.get("nodes", "?"):,}</b></span><span>edges <b>{nn.get("edges", "?"):,}</b></span>
  <span>glider file <b>{gg.get("file_bytes", 0) / 1e6:.0f} MB</b></span>
  <span>neo4j <b>{esc(meta.get("neo4j_version", ""))}</b> + GDS, heap 3 GB, page cache 1 GB</span>
  <span>glider <b>{esc(meta.get("glider_version", "").replace("glider ", ""))}</b>, cache 1 GB</span>
  <span>warm = median of {meta.get("warm", 5)} runs after a cold run</span>
</div>
{summary(reads, algos, writes)}

<h2>Reads</h2>
<div class="legend"><span><i class="n"></i>Neo4j, server-side time</span><span><i class="g"></i>glider, engine time</span></div>
{table(reads, "read")}

<h2>Algorithms</h2>
<p class="lede">Neo4j runs algorithms through Graph Data Science on a projected copy of the graph; the projection is its own line. glider runs them on the database directly.</p>
{table(algos, "algo")}

<h2>Writes</h2>
<p class="lede">{meta.get("writes", 0)} single statements each, autocommit, the way an application issues them. The warm column is the per-statement mean; cold is the total.</p>
{table(writes, "write")}

<h2>How this was run</h2>
<div class="method"><ul>
<li><b>Data.</b> Neo4j's <code>{esc(dataset)}-50.dump</code> from github.com/neo4j-graph-examples, loaded with <code>neo4j-admin database load</code> into the official <code>neo4j:5</code> image with the Graph Data Science plugin. The same graph was pulled out over Bolt by <code>bench/convert.py</code> (embedding vectors dropped) and imported into a paged glider file. Node and edge counts were checked equal before anything ran.</li>
<li><b>Indexes.</b> The dump carries Neo4j's indexes on Movie(title), Person(name), User(name) and more; glider got <code>INDEX ON</code> the three properties the queries look up by.</li>
<li><b>Timing.</b> Neo4j: <code>result_available_after + result_consumed_after</code> as the driver reports them, server time only, in whole milliseconds; a sub-millisecond Neo4j answer therefore reads as 1–3 ms, and ratios on those rows are marked ≥ as floors. glider: the shell's timer around the statement, engine time only, microsecond resolution. Neither includes the client, so the comparison is engine against engine.</li>
<li><b>Agreement.</b> Row sets (ordered where the query orders), distinct sets, counts, path lengths and component counts are compared exactly; PageRank is compared as the overlap of the two top-ten lists, since the two implementations normalise differently.</li>
<li><b>Where the Cypher differs.</b> glider has no <code>WITH</code>, <code>UNWIND</code>, <code>MERGE</code> or pattern predicates, so the read queries were chosen to be expressible in both dialects; the one exception is marked in its row. Algorithms are procedure calls on both sides and read differently by nature.</li>
<li><b>Machine.</b> One workstation; Neo4j in Docker limited to 6 GB, glider a native process. Neither had the box to itself.</li>
</ul></div>
</main>
"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", default="recommendations")
    ap.add_argument("--results", default=None)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    results = args.results or os.path.join(HERE, "results", f"neo4j-{args.dataset}.jsonl")
    out = args.out or os.path.join(HERE, f"neo4j-{args.dataset}.html")
    recs = [json.loads(l) for l in open(results) if l.strip()]
    open(out, "w").write(render(recs, args.dataset))
    print(out)


if __name__ == "__main__":
    main()
