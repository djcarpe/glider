#!/usr/bin/env bash
# Full benchmark matrix for glider. Emits one JSON object per measurement to
# results.jsonl, so a partial run is still a usable result set.
#
#   ./run.sh [scale ...]          default: 25k 100k 400k 1m
#
# Axes, chosen against what this engine actually does:
#
#   size        the real complexity curve — ingest, query, algorithms vs N
#   concurrency glider serialises every request behind one Mutex<Graph>, so
#               this quantifies that rather than asserting it
#   durability  --sync always|normal|off, a documented knob, on spinning disk
#   memory      the graph is memory-resident, so the ceiling is RAM, not IO;
#               we record peak RSS and log-replay time at every size
#
# Deliberately NOT swept: core count. Nothing in query.rs, graph.rs or algo.rs
# spawns a thread, so that axis is flat by construction and would be a
# misleading chart rather than a finding.

set -uo pipefail
cd "$(dirname "$0")"

GLIDER=../target/release/glider
GEN=target/release/ldbc-gen
LOADGEN=target/release/loadgen
DATA=data
OUT=results.jsonl
PORT=7878
ADDR=127.0.0.1:$PORT

SCALES=("$@")
[ ${#SCALES[@]} -eq 0 ] && SCALES=(25k 100k 400k 1m)

mkdir -p "$DATA"
: > "$OUT"

log()  { echo "[$(date +%H:%M:%S)] $*" >&2; }
emit() { echo "$1" >> "$OUT"; }

# JSON-escape a string for embedding in emitted records.
jesc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

# Run a command, print elapsed seconds and peak RSS in KB as "secs rss".
timed() { /usr/bin/time -f "%e %M" "$@" 2>&1 >/dev/null | tail -1; }

# Median of stdin numbers.
median() { sort -n | awk '{a[NR]=$1} END{ if(NR==0){print 0} else if(NR%2){print a[(NR+1)/2]} else {printf "%.4f", (a[NR/2]+a[NR/2+1])/2} }'; }

server_pid=""
start_server() {
  local db=$1
  "$GLIDER" "$db" serve --addr "$ADDR" >"$DATA/serve.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 60); do
    (echo > /dev/tcp/127.0.0.1/$PORT) >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  log "server failed to start for $db"; return 1
}
stop_server() {
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null
  wait "$server_pid" 2>/dev/null
  server_pid=""
}
trap 'stop_server' EXIT

total_ram_kb=$(awk '/MemTotal/{print $2}' /proc/meminfo)
log "host: $(nproc) cores, $((total_ram_kb/1024/1024)) GB RAM"
emit "{\"record\":\"host\",\"cores\":$(nproc),\"ram_kb\":$total_ram_kb,\"kernel\":\"$(jesc "$(uname -r)")\",\"rustc\":\"$(jesc "$(~/.cargo/bin/rustc --version 2>/dev/null || rustc --version)")\"}"

# ===================================================================== sizes
for scale in "${SCALES[@]}"; do
  log "===== scale $scale ====="
  jsonl="$DATA/$scale.jsonl"
  db="$DATA/$scale.gldb"

  # ---- generate
  if [ ! -f "$jsonl" ]; then
    log "generating $scale"
    genout=$("$GEN" --scale "$scale" --out "$jsonl" 2>&1)
    log "$genout"
  fi
  jsonl_bytes=$(stat -c %s "$jsonl")

  # ---- ingest, default sync
  rm -f "$db" "$db".*
  log "importing $scale"
  read -r imp_s imp_rss <<<"$(timed "$GLIDER" "$db" import "$jsonl")"
  db_bytes=$(stat -c %s "$db" 2>/dev/null || echo 0)

  counts=$("$GLIDER" "$db" -c "STATS" --json 2>/dev/null)
  nodes=$(echo "$counts" | grep -o '"nodes",[0-9]*' | grep -o '[0-9]*$' || echo 0)
  [ -z "$nodes" ] && nodes=0
  edges=$(echo "$counts" | grep -o '"edges",[0-9]*' | grep -o '[0-9]*$' || echo 0)
  [ -z "$edges" ] && edges=0

  # ---- reopen: replays the whole log from disk. The one genuinely IO-bound
  #      path in the engine, and the thing that grows with file size.
  read -r reopen_s reopen_rss <<<"$(timed "$GLIDER" "$db" -c "RETURN 1")"

  emit "{\"record\":\"size\",\"scale\":\"$scale\",\"nodes\":$nodes,\"edges\":$edges,\
\"jsonl_bytes\":$jsonl_bytes,\"db_bytes\":$db_bytes,\
\"import_s\":$imp_s,\"import_peak_rss_kb\":$imp_rss,\
\"reopen_s\":$reopen_s,\"reopen_peak_rss_kb\":$reopen_rss}"
  log "  $nodes nodes, $edges edges, import ${imp_s}s, reopen ${reopen_s}s, RSS $((reopen_rss/1024)) MB"

  # ---- index build
  read -r idx_s idx_rss <<<"$(timed "$GLIDER" "$db" -c 'INDEX ON :Person(email)')"
  emit "{\"record\":\"index\",\"scale\":\"$scale\",\"build_s\":$idx_s,\"peak_rss_kb\":$idx_rss}"

  # ---- pick real probe values out of the generated data
  probe_email=$(grep -m1 '"id":12345,' "$jsonl" | grep -o '"email":"[^"]*"' | cut -d'"' -f4)
  [ -z "$probe_email" ] && probe_email=$(head -1 "$jsonl" | grep -o '"email":"[^"]*"' | cut -d'"' -f4)
  probe_id=12345
  [ "$nodes" -lt 20000 ] && probe_id=100

  # ---- query latency, single client, through the HTTP server
  start_server "$db" || continue
  rss_loaded=$(awk '/VmRSS/{print $2}' /proc/$server_pid/status 2>/dev/null || echo 0)
  emit "{\"record\":\"resident\",\"scale\":\"$scale\",\"server_rss_kb\":$rss_loaded,\"nodes\":$nodes,\"edges\":$edges}"

  run_q() { # name query duration
    local name=$1 q=$2 dur=${3:-8}
    local j
    j=$("$LOADGEN" --addr "$ADDR" --clients 1 --duration "$dur" --warmup 2 \
          --query "$q" --label "$name" --json 2>/dev/null)
    if [ -n "$j" ]; then
      emit "{\"record\":\"query\",\"scale\":\"$scale\",\"query\":\"$(jesc "$name")\",\"detail\":${j}}"
      log "  q $name: $(echo "$j" | grep -o '\"p50_ms\":[0-9.]*')"
    else
      log "  q $name: FAILED"
      emit "{\"record\":\"query\",\"scale\":\"$scale\",\"query\":\"$(jesc "$name")\",\"failed\":true}"
    fi
  }

  run_q "point-lookup-indexed" "MATCH (p:Person {email:\"$probe_email\"}) RETURN p.name, p.age"
  run_q "label-scan-count"     "MATCH (p:Person) RETURN count(p)"
  run_q "filter-two-props"     "MATCH (p:Person) WHERE p.age > 40 AND p.city = \"London\" RETURN count(p)"
  run_q "hop1"                 "MATCH (p:Person)-[:KNOWS]->(f) WHERE id(p) = $probe_id RETURN count(f)"
  run_q "hop2"                 "MATCH (p:Person)-[:KNOWS*1..2]->(f) WHERE id(p) = $probe_id RETURN count(f)"
  run_q "hop3"                 "MATCH (p:Person)-[:KNOWS*1..3]->(f) WHERE id(p) = $probe_id RETURN count(f)" 12
  run_q "pattern-3label"       "MATCH (p:Person)-[:AUTHORED]->(o:Post)-[:HAS_TAG]->(t:Tag) WHERE t.name=\"rust\" RETURN count(o)" 12
  run_q "groupby-city"         "MATCH (c:City)<-[:LIVES_IN]-(p:Person) RETURN c.name, count(p) AS n ORDER BY n DESC LIMIT 10" 12
  stop_server

  # ---- whole-graph algorithms, via the CLI, median of 3
  for spec in \
    "pagerank:CALL pagerank(iterations: 20, top: 5)" \
    "components:CALL components(top: 5)" \
    "kcore:CALL kcore(top: 5)" \
    "triangles:CALL triangles(top: 5)" \
    "communities:CALL communities(top: 5)"
  do
    name=${spec%%:*}; q=${spec#*:}
    log "  algo $name"
    times=$(for _ in 1 2 3; do timed "$GLIDER" "$db" -c "$q" | awk '{print $1}'; done)
    med=$(echo "$times" | median)
    rssv=$(timed "$GLIDER" "$db" -c "$q" | awk '{print $2}')
    emit "{\"record\":\"algo\",\"scale\":\"$scale\",\"algo\":\"$name\",\"median_s\":$med,\"peak_rss_kb\":$rssv,\"nodes\":$nodes,\"edges\":$edges}"
    log "    ${med}s"
  done

  # ---- stop climbing if we are close to the RAM ceiling; the graph is
  #      memory-resident, so the next size up would swap or OOM rather than
  #      merely run slower, and that is not a measurement worth taking.
  used_pct=$(( reopen_rss * 100 / total_ram_kb ))
  log "  peak RSS is ${used_pct}% of RAM"
  if [ "$used_pct" -gt 55 ]; then
    log "  stopping size sweep: next scale would not fit in RAM"
    emit "{\"record\":\"ceiling\",\"scale\":\"$scale\",\"rss_pct_of_ram\":$used_pct}"
    LAST_SCALE=$scale
    break
  fi
  LAST_SCALE=$scale
done

# ============================================================== concurrency
# Run at the largest size that completed, so the serialisation cost is measured
# on a graph where queries do real work.
CSCALE=${LAST_SCALE:-${SCALES[0]}}
cdb="$DATA/$CSCALE.gldb"
if [ -f "$cdb" ]; then
  log "===== concurrency sweep at $CSCALE ====="
  probe_email=$(grep -m1 '"id":12345,' "$DATA/$CSCALE.jsonl" | grep -o '"email":"[^"]*"' | cut -d'"' -f4)
  [ -z "$probe_email" ] && probe_email=$(head -1 "$DATA/$CSCALE.jsonl" | grep -o '"email":"[^"]*"' | cut -d'"' -f4)

  cat > "$DATA/mixed.q" <<EOF
MATCH (p:Person {email:"$probe_email"}) RETURN p.name, p.age
MATCH (p:Person)-[:KNOWS]->(f) WHERE id(p) = 12345 RETURN count(f)
MATCH (c:City)<-[:LIVES_IN]-(p:Person) RETURN c.name, count(p) AS n ORDER BY n DESC LIMIT 10
EOF

  start_server "$cdb"
  for c in 1 2 4 8 16 32; do
    # Cheap indexed lookup: isolates lock contention from query cost.
    j=$("$LOADGEN" --addr "$ADDR" --clients "$c" --duration 12 --warmup 3 \
        --query "MATCH (p:Person {email:\"$probe_email\"}) RETURN p.name" \
        --label "point@c$c" --json 2>/dev/null)
    [ -n "$j" ] && emit "{\"record\":\"concurrency\",\"scale\":\"$CSCALE\",\"workload\":\"point-lookup\",\"detail\":${j}}"
    log "  point c=$c: $(echo "$j" | grep -o '\"qps\":[0-9.]*')"

    j=$("$LOADGEN" --addr "$ADDR" --clients "$c" --duration 12 --warmup 3 \
        --query-file "$DATA/mixed.q" --label "mixed@c$c" --json 2>/dev/null)
    [ -n "$j" ] && emit "{\"record\":\"concurrency\",\"scale\":\"$CSCALE\",\"workload\":\"mixed\",\"detail\":${j}}"
    log "  mixed c=$c: $(echo "$j" | grep -o '\"qps\":[0-9.]*')"
  done
  stop_server
fi

# ============================================================== durability
# Ingest under each --sync mode. On spinning disk the spread between
# always/normal/off is the whole point.
DSCALE=${SCALES[0]}
djsonl="$DATA/$DSCALE.jsonl"
if [ -f "$djsonl" ]; then
  log "===== durability sweep at $DSCALE ====="
  for mode in always normal off; do
    ddb="$DATA/sync-$mode.gldb"
    rm -f "$ddb" "$ddb".*
    read -r s rss <<<"$(timed "$GLIDER" "$ddb" --sync "$mode" import "$djsonl")"
    emit "{\"record\":\"durability\",\"scale\":\"$DSCALE\",\"sync\":\"$mode\",\"import_s\":$s,\"peak_rss_kb\":$rss}"
    log "  --sync $mode: ${s}s"
    rm -f "$ddb" "$ddb".*
  done
fi

log "done. $(wc -l < "$OUT") records in $OUT"
