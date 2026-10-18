#!/usr/bin/env python3
"""Drive ctxbench over the scale-gen databases: quality once, then scale on
each size, cold (files evicted from the OS page cache) then warm, under a
memory cap. Writes bench/results/context.jsonl; context_report.py renders it.

    bench/context_bench.py [--sizes 1] [--cache-mb 256] [--seeds 50]

Each run works on a copy of ctx-<size>g.gldb (it installs the extension,
builds the text index and writes), so the generated file stays pristine.
"""
import argparse, json, os, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.environ.get("GLIDER_BENCH_DATA", os.path.join(os.path.dirname(ROOT), "glider-bench-data"))
BIN = os.path.join(ROOT, "target", "release", "ctxbench")
OUT = os.path.join(ROOT, "bench", "results", "context.jsonl")


def evict(path):
    for p in [path] + [os.path.join(path + "-data", f) for f in (os.listdir(path + "-data") if os.path.isdir(path + "-data") else [])]:
        try:
            fd = os.open(p, os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)
        except OSError as e:
            print("evict failed", p, e, file=sys.stderr)


def run(args, mem):
    cmd = ["systemd-run", "--user", "--scope", "-q", "-p", f"MemoryMax={mem}", "-p", "MemorySwapMax=0", BIN] + args
    print("+", " ".join(args), file=sys.stderr, flush=True)
    r = subprocess.run(cmd, stdout=subprocess.PIPE, text=True)
    if r.returncode != 0:
        print(f"exit {r.returncode}", file=sys.stderr)
    return [json.loads(l) for l in r.stdout.splitlines() if l.startswith("{")]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default="1")
    ap.add_argument("--cache-mb", default="256")
    ap.add_argument("--seeds", default="50")
    ap.add_argument("--quality-nodes", default="200000")
    ap.add_argument("--skip-quality", action="store_true")
    a = ap.parse_args()
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    recs = []
    if not a.skip_quality:
        recs += run(["quality", "--nodes", a.quality_nodes, "--seeds", "100"], "6G")
    for s in a.sizes.split(","):
        src = os.path.join(DATA, f"ctx-{s}g.gldb")
        if not os.path.exists(src):
            print("missing", src, "(scale-gen --size %sGiB --paged --out %s)" % (s, src), file=sys.stderr)
            continue
        db = os.path.join(DATA, f"ctx-{s}g-work.gldb")
        subprocess.run(["rm", "-rf", db, db + "-wal", db + "-data", db + "-tmp", db + ".lock"], check=True)
        subprocess.run(["cp", "-r", "--reflink=auto", src, db], check=True)
        if os.path.isdir(src + "-wal"):
            subprocess.run(["cp", "-r", "--reflink=auto", src + "-wal", db + "-wal"], check=True)
        for state in ["cold", "warm"]:
            if state == "cold":
                evict(db)
            recs += run(["scale", "--db", db, "--cache-mb", a.cache_mb, "--seeds", a.seeds, "--label", state], "2G")
    with open(OUT, "w") as f:
        for r in recs:
            f.write(json.dumps(r) + "\n")
    print("wrote", OUT, file=sys.stderr)


if __name__ == "__main__":
    main()
