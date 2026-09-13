#!/usr/bin/env python3
"""End-to-end test: glider-stream against a signature-verifying S3 server.

Runs the server in a daemon thread so there is no stray background process,
and drives the real binaries through subprocess.
"""
import os
import shutil
import subprocess
import sys
import threading
from http.server import HTTPServer

sys.path.insert(0, "/home/claude")
import fakes3

WORK = "/tmp/s3test"
GLIDER = "/home/claude/glider/target/release/glider"
STREAM = "/home/claude/glider/target/release/glider-stream"
PORT = 9455
ENDPOINT = ["-endpoint", "http://127.0.0.1:%d" % PORT]

env = dict(os.environ)
env["AWS_ACCESS_KEY_ID"] = fakes3.ACCESS_KEY
env["AWS_SECRET_ACCESS_KEY"] = fakes3.SECRET_KEY

failures = []


def run(args, label):
    print("$ " + " ".join(args))
    p = subprocess.run(args, cwd=WORK, env=env, capture_output=True, text=True, timeout=90)
    if p.stdout.strip():
        print(p.stdout.rstrip())
    if p.returncode != 0:
        print("  !! exit %d: %s" % (p.returncode, p.stderr.strip()))
        failures.append(label)
    return p


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)

    server = HTTPServer(("127.0.0.1", PORT), fakes3.Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    db = os.path.join(WORK, "app.gldb")
    run([GLIDER, db, "-c", 'CREATE (a:Job {name:"build"})-[:BLOCKS]->(b:Job {name:"test"})'], "seed")

    url = "s3://graphs/app"
    run([STREAM, "replicate", db, url] + ENDPOINT + ["-once", "-force-snapshot"], "first sync")
    run([STREAM, "generations", url] + ENDPOINT, "generations")
    run([STREAM, "snapshots", url] + ENDPOINT, "snapshots")

    run([GLIDER, db, "-c", 'CREATE (:Job {name:"deploy"})'], "second write")
    run([STREAM, "replicate", db, url] + ENDPOINT + ["-once"], "second sync")
    run([STREAM, "segments", url] + ENDPOINT, "segments")

    out = os.path.join(WORK, "from-s3.gldb")
    run([STREAM, "restore", "-o", out] + ENDPOINT + [url], "restore")

    with open(db, "rb") as a, open(out, "rb") as b:
        same = a.read() == b.read()
    print("\nrestored file identical to live database: %s" % ("YES" if same else "NO"))
    if not same:
        failures.append("byte comparison")

    # Query the restored copy to prove it is a working database, not just bytes.
    run([GLIDER, out, "-c", "MATCH (n:Job) RETURN n.name ORDER BY n.name"], "query restored")

    print("\nsignature rejections by the server: %d" % fakes3.REJECTED[0])
    if fakes3.REJECTED[0]:
        failures.append("signature rejections")

    print("\nFAILURES: %s" % (", ".join(failures) if failures else "none"))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
