"""An OTLP/HTTP collector that throws spans away, for bench/run.py's
`telemetry` group: it reads each request and answers 200.

    python3 bench/servers/collector.py <port>
"""

import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Discard(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"  # keeps connections open

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, format, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Discard).serve_forever()
