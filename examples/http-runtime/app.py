"""An HTTP server with no Unsworn SDK, run as a runtime.

Unsworn starts it (`[runtimes.app]` in unsworn.toml), gives it a Unix
socket to listen on in UNSWORN_SOCKET, and forwards requests to it once
they've passed their route's rules. Who the caller is arrives in the
Unsworn-Context header.
"""

import base64
import json
import os
import socketserver
from http.server import BaseHTTPRequestHandler


def context(headers):
    """What a handler gets as `ctx`: verified claims under "auth"."""
    raw = headers.get("unsworn-context")
    if not raw:
        return {}
    return json.loads(base64.urlsafe_b64decode(raw + "=" * (-len(raw) % 4)))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        caller = context(self.headers).get("auth", {})
        body = json.dumps({"path": self.path, "caller": caller}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def address_string(self):
        return "unsworn"  # a Unix socket's peer has no address


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


Server(os.environ["UNSWORN_SOCKET"], Handler).serve_forever()
