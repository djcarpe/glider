#!/usr/bin/env bash
# Generate the scale dataset, recording time and peak RSS per file.
set -euo pipefail
cd "$(dirname "$0")"
T=../../glider/target/release
: > gen.jsonl
for size in 1GiB 5GiB 10GiB 25GiB; do
  out=img-$size.gldb
  [ -f "$out" ] && continue
  echo "== image $size" >&2
  line=$(python3 -c '
import os,sys,time,subprocess,json
t=time.time(); p=subprocess.Popen(sys.argv[1:],stdout=subprocess.PIPE)
out=p.stdout.read().decode(); _,st,ru=os.wait4(p.pid,0)
d=json.loads(out.strip().splitlines()[-1]); d["wall_s"]=round(time.time()-t,3); d["peak_rss_kb"]=ru.ru_maxrss; d["size"]=sys.argv[-1]; print(json.dumps(d))
' $T/scale-gen --size $size --out $out)
  echo "$line" | tee -a gen.jsonl
done
for size in 1GiB 5GiB; do
  out=log-$size.gldb
  [ -f "$out" ] && continue
  ids=$(grep "\"size\": \"$size\"" gen.jsonl | python3 -c 'import sys,json; print(json.loads(sys.stdin.readline())["ids"])')
  echo "== log $size ($ids ids)" >&2
  python3 -c '
import os,sys,time,subprocess,json
t=time.time(); p=subprocess.Popen(sys.argv[1:-1],stdout=subprocess.PIPE)
out=p.stdout.read().decode(); _,st,ru=os.wait4(p.pid,0)
d=json.loads(out.strip().splitlines()[-1]); d["wall_s"]=round(time.time()-t,3); d["peak_rss_kb"]=ru.ru_maxrss; d["size"]=sys.argv[-1]; print(json.dumps(d))
' $T/scale-gen --nodes $ids --log --v2 --out $out $size | tee -a gen.jsonl
done
echo done >&2
