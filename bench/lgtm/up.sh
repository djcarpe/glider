#!/usr/bin/env bash
# Grafana LGTM (Loki, Grafana, Tempo, Mimir/Prometheus, Pyroscope, OTel
# collector) plus Grafana Alloy profiling glider and the SQLite worker with
# eBPF. Everything binds to localhost. Grafana: http://localhost:3000
#
# Alloy needs --privileged and the host PID namespace to load its eBPF
# profiler; it only keeps processes named glider and the bench's SQLite
# worker (see alloy.river).
set -euo pipefail
cd "$(dirname "$0")"

docker start glider-lgtm 2>/dev/null || docker run -d --name glider-lgtm --memory 4g \
  -p 127.0.0.1:3000:3000 -p 127.0.0.1:4317:4317 -p 127.0.0.1:4318:4318 -p 127.0.0.1:4040:4040 \
  -v glider-lgtm-data:/data grafana/otel-lgtm:latest

docker start glider-alloy 2>/dev/null || docker run -d --name glider-alloy --privileged --pid=host \
  --network host --memory 1g \
  -v "$PWD/alloy.river:/etc/alloy/config.alloy:ro" \
  -v /sys/kernel/tracing:/sys/kernel/tracing -v /sys/kernel/debug:/sys/kernel/debug \
  grafana/alloy:latest run --server.http.listen-addr=127.0.0.1:12345 /etc/alloy/config.alloy

until curl -sf localhost:3000/api/health >/dev/null; do sleep 1; done
curl -sf -u admin:admin -H 'Content-Type: application/json' -X POST \
  localhost:3000/api/dashboards/db -d @dashboard.json >/dev/null
echo "Grafana: http://localhost:3000/d/glider-vs-sqlite"

# The profiling build the benchmarks run: symbols, frame pointers, and
# legacy symbol mangling (which the eBPF demangler understands; stable rustc
# needs RUSTC_BOOTSTRAP for the flag).
cd ../..
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-C force-frame-pointers=yes -Zunstable-options -C symbol-mangling-version=legacy" \
  cargo build --profile profiling --bin glider
