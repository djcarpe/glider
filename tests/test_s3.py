#!/usr/bin/env python3
"""End-to-end test: glider-stream against a signature-verifying S3 server.

A paged database (what `glider` writes today) is replicated to S3 as base
snapshots plus the write-ahead log, listed, written to again, replicated
again, and restored from S3; the restored copy must answer queries exactly as
the live database does. The server runs in a daemon thread, so there is no
stray background process, and the real binaries are driven through
subprocess.

    cargo build --release && python3 tests/test_s3.py

GLIDER and GLIDER_STREAM override the binaries' paths.
"""
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
from http.server import HTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import fakes3  # noqa: E402

GLIDER = os.environ.get("GLIDER", os.path.join(REPO, "target", "release", "glider"))
STREAM = os.environ.get("GLIDER_STREAM", os.path.join(REPO, "target", "release", "glider-stream"))

env = dict(os.environ)
env["AWS_ACCESS_KEY_ID"] = fakes3.ACCESS_KEY
env["AWS_SECRET_ACCESS_KEY"] = fakes3.SECRET_KEY

failures = []


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run(args, label, work):
    print("$ " + " ".join(args))
    p = subprocess.run(args, cwd=work, env=env, capture_output=True, text=True, timeout=120)
    if p.stdout.strip():
        print(p.stdout.rstrip())
    if p.returncode != 0:
        print("  !! exit %d: %s" % (p.returncode, p.stderr.strip()))
        failures.append(label)
    return p


def main():
    work = tempfile.mkdtemp(prefix="glider-s3test-")
    port = free_port()
    endpoint = ["-endpoint", "http://127.0.0.1:%d" % port]
    server = HTTPServer(("127.0.0.1", port), fakes3.Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    db = os.path.join(work, "app.gldb")
    url = "s3://graphs/app"
    run([GLIDER, db, "-c", 'CREATE (a:Job {name:"build"})-[:BLOCKS]->(b:Job {name:"test"})'], "seed", work)

    # A base and the log so far.
    run([STREAM, "replicate", db, url] + endpoint + ["-once", "-force-snapshot"], "first sync", work)
    run([STREAM, "generations", url] + endpoint, "generations", work)
    snaps = run([STREAM, "snapshots", url] + endpoint, "snapshots", work)
    if "base at log" not in snaps.stdout:
        failures.append("no base listed")

    # More writes: only log goes up.
    run([GLIDER, db, "-c", 'CREATE (:Job {name:"deploy"})'], "second write", work)
    run([GLIDER, db, "-c", 'MATCH (j:Job {name:"test"}) SET j.done = true'], "third write", work)
    run([STREAM, "replicate", db, url] + endpoint + ["-once"], "second sync", work)
    run([STREAM, "segments", url] + endpoint, "segments", work)

    out = os.path.join(work, "from-s3.gldb")
    restored = run([STREAM, "restore", "-o", out] + endpoint + [url], "restore", work)
    if "restored generation" not in restored.stdout:
        failures.append("restore report")

    # Same answers from the live database and the restored one.
    q = "MATCH (n:Job) RETURN n.name, n.done ORDER BY n.name"
    live = run([GLIDER, db, "-c", q], "query live", work).stdout
    back = run([GLIDER, out, "-c", q], "query restored", work).stdout
    same = live.strip() != "" and live == back
    print("\nrestored database answers like the live one: %s" % ("YES" if same else "NO"))
    if not same:
        failures.append("restored answers differ")

    # Restoring over an existing file is refused.
    p = subprocess.run([STREAM, "restore", "-o", out] + endpoint + [url], cwd=work, env=env, capture_output=True, text=True)
    if p.returncode == 0:
        failures.append("restore overwrote an existing file")

    print("\nsignature rejections by the server: %d" % fakes3.REJECTED[0])
    if fakes3.REJECTED[0]:
        failures.append("signature rejections")

    shutil.rmtree(work, ignore_errors=True)
    print("\nFAILURES: %s" % (", ".join(failures) if failures else "none"))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
