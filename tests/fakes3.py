#!/usr/bin/env python3
"""A minimal S3-compatible server used to test glider-stream's S3 client.

It is deliberately strict: every request's SigV4 signature is recomputed here,
independently, from Python's hashlib/hmac. A request whose signature does not
match is rejected with 403 SignatureDoesNotMatch exactly as S3 would, so the
test proves the Rust signing implementation is correct rather than proving that
two copies of the same bug agree.
"""
import datetime
import hashlib
import hmac
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qsl, quote

ACCESS_KEY = "glidertest"
SECRET_KEY = "glidertestsecret"
REGION = "us-east-1"

STORE = {}
LOCK = threading.Lock()
REJECTED = [0]


def sign_key(secret, date, region, service):
    k = hmac.new(("AWS4" + secret).encode(), date.encode(), hashlib.sha256).digest()
    k = hmac.new(k, region.encode(), hashlib.sha256).digest()
    k = hmac.new(k, service.encode(), hashlib.sha256).digest()
    return hmac.new(k, b"aws4_request", hashlib.sha256).digest()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _verify(self, body):
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("AWS4-HMAC-SHA256 "):
            return "missing Authorization"
        parts = dict(
            p.strip().split("=", 1) for p in auth[len("AWS4-HMAC-SHA256 "):].split(",")
        )
        credential = parts["Credential"]
        signed_headers = parts["SignedHeaders"]
        signature = parts["Signature"]

        akid, date, region, service, terminator = credential.split("/")
        if akid != ACCESS_KEY:
            return "unknown access key %r" % akid

        url = urlparse(self.path)
        canonical_uri = url.path
        query = parse_qsl(url.query, keep_blank_values=True)
        canonical_query = "&".join(
            "%s=%s" % (quote(k, safe="-_.~"), quote(v, safe="-_.~"))
            for k, v in sorted(query)
        )

        payload_hash = self.headers.get("x-amz-content-sha256", "")
        if payload_hash != hashlib.sha256(body).hexdigest():
            return "x-amz-content-sha256 does not match the body"

        canonical_headers = ""
        for name in signed_headers.split(";"):
            value = self.headers.get(name, "")
            canonical_headers += "%s:%s\n" % (name, value.strip())

        canonical_request = "\n".join([
            self.command,
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            payload_hash,
        ])
        scope = "/".join([date, region, service, terminator])
        string_to_sign = "\n".join([
            "AWS4-HMAC-SHA256",
            self.headers.get("x-amz-date", ""),
            scope,
            hashlib.sha256(canonical_request.encode()).hexdigest(),
        ])
        expected = hmac.new(
            sign_key(SECRET_KEY, date, region, service),
            string_to_sign.encode(),
            hashlib.sha256,
        ).hexdigest()
        if not hmac.compare_digest(expected, signature):
            REJECTED[0] += 1
            sys.stderr.write("SIGNATURE MISMATCH\ncanonical request:\n%s\n" % canonical_request)
            return "SignatureDoesNotMatch"
        return None

    def _body(self):
        length = int(self.headers.get("Content-Length", 0))
        return self.rfile.read(length) if length else b""

    def _fail(self, code, msg):
        payload = ("<Error><Code>%s</Code></Error>" % msg).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/xml")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _key(self):
        # Path style: /bucket/key...
        path = urlparse(self.path).path.lstrip("/")
        return path.split("/", 1)[1] if "/" in path else ""

    def do_PUT(self):
        body = self._body()
        err = self._verify(body)
        if err:
            return self._fail(403, err)
        with LOCK:
            STORE[self._key()] = (body, datetime.datetime.utcnow())
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_DELETE(self):
        err = self._verify(b"")
        if err:
            return self._fail(403, err)
        with LOCK:
            STORE.pop(self._key(), None)
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        err = self._verify(b"")
        if err:
            return self._fail(403, err)
        query = dict(parse_qsl(urlparse(self.path).query))

        if query.get("list-type") == "2":
            prefix = query.get("prefix", "")
            with LOCK:
                keys = sorted(k for k in STORE if k.startswith(prefix))
            xml = ['<?xml version="1.0" encoding="UTF-8"?><ListBucketResult>']
            xml.append("<IsTruncated>false</IsTruncated>")
            for k in keys:
                body, when = STORE[k]
                xml.append(
                    "<Contents><Key>%s</Key><Size>%d</Size><LastModified>%s</LastModified></Contents>"
                    % (k, len(body), when.strftime("%Y-%m-%dT%H:%M:%S.000Z"))
                )
            xml.append("</ListBucketResult>")
            payload = "".join(xml).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/xml")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        with LOCK:
            entry = STORE.get(self._key())
        if entry is None:
            return self._fail(404, "NoSuchKey")
        self.send_response(200)
        self.send_header("Content-Length", str(len(entry[0])))
        self.end_headers()
        self.wfile.write(entry[0])


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9444
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
