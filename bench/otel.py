"""Minimal OpenTelemetry export for the benchmark harness: traces, logs and
metrics as OTLP/HTTP JSON, standard library only (glider's rule for its
tooling too). Point it at an OTLP endpoint such as grafana/otel-lgtm:

    docker run -p 3000:3000 -p 4318:4318 grafana/otel-lgtm

Everything is buffered and sent by flush(); a missing collector is logged
once and otherwise ignored, so benchmarks never fail on telemetry.
"""

import json
import os
import sys
import time
import urllib.request

ENDPOINT = os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318")
SERVICE = "glider-bench"

_spans, _logs, _points = [], [], []
_warned = False


def new_trace_id():
    return os.urandom(16).hex()


def new_span_id():
    return os.urandom(8).hex()


def now_ns():
    return time.time_ns()


def _attrs(d):
    out = []
    for k, v in (d or {}).items():
        if v is None:
            continue
        if isinstance(v, bool):
            val = {"boolValue": v}
        elif isinstance(v, int):
            val = {"intValue": str(v)}
        elif isinstance(v, float):
            val = {"doubleValue": v}
        else:
            val = {"stringValue": str(v)}
        out.append({"key": k, "value": val})
    return out


def span(trace_id, name, start_ns, end_ns, parent=None, attrs=None, error=None, span_id=None):
    """Record a finished span. Returns its span id."""
    sid = span_id or new_span_id()
    s = {
        "traceId": trace_id,
        "spanId": sid,
        "name": name,
        "kind": 1,
        "startTimeUnixNano": str(start_ns),
        "endTimeUnixNano": str(max(end_ns, start_ns)),
        "attributes": _attrs(attrs),
        "status": {"code": 2, "message": error} if error else {"code": 1},
    }
    if parent:
        s["parentSpanId"] = parent
    _spans.append(s)
    return sid


def log(body, trace_id=None, span_id=None, attrs=None, severity="INFO", t_ns=None):
    rec = {
        "timeUnixNano": str(t_ns or now_ns()),
        "severityText": severity,
        "severityNumber": {"INFO": 9, "WARN": 13, "ERROR": 17}.get(severity, 9),
        "body": {"stringValue": body},
        "attributes": _attrs(attrs),
    }
    if trace_id:
        rec["traceId"] = trace_id
    if span_id:
        rec["spanId"] = span_id
    _logs.append(rec)


def gauge(name, value, attrs=None, unit="ms", t_ns=None):
    _points.append((name, unit, {"asDouble": float(value), "timeUnixNano": str(t_ns or now_ns()), "attributes": _attrs(attrs)}))


def _post(path, body):
    global _warned
    req = urllib.request.Request(ENDPOINT + path, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=10).read()
    except Exception as e:  # noqa: BLE001 - telemetry must never break a run
        if not _warned:
            print(f"otel: {ENDPOINT}{path} unavailable ({e}); continuing without telemetry", file=sys.stderr)
            _warned = True


def flush():
    resource = {"attributes": _attrs({"service.name": SERVICE, "host.name": os.uname().nodename})}
    scope = {"name": "glider.bench"}
    if _spans:
        _post("/v1/traces", {"resourceSpans": [{"resource": resource, "scopeSpans": [{"scope": scope, "spans": list(_spans)}]}]})
        _spans.clear()
    if _logs:
        _post("/v1/logs", {"resourceLogs": [{"resource": resource, "scopeLogs": [{"scope": scope, "logRecords": list(_logs)}]}]})
        _logs.clear()
    if _points:
        by_name = {}
        for name, unit, p in _points:
            by_name.setdefault((name, unit), []).append(p)
        metrics = [{"name": n, "unit": u, "gauge": {"dataPoints": ps}} for (n, u), ps in by_name.items()]
        _post("/v1/metrics", {"resourceMetrics": [{"resource": resource, "scopeMetrics": [{"scope": scope, "metrics": metrics}]}]})
        _points.clear()
