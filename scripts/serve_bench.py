#!/usr/bin/env python3
"""Tiny static server for the browser bench: serves dist/ at / and the repo's
testdata/ at /testdata/. No COEP (Phase 1.5 needs no cross-origin isolation), so
same-origin fetch of the big GDS just works. Streams large files.

Usage: python3 scripts/serve_bench.py [PORT]
"""
import http.server, os, sys, socketserver

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
DIST = os.path.join(ROOT, "dist")
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8090


class Handler(http.server.SimpleHTTPRequestHandler):
    def translate_path(self, path):
        # Strip query/fragment.
        path = path.split("?", 1)[0].split("#", 1)[0]
        if path.startswith("/testdata/"):
            return os.path.join(ROOT, path.lstrip("/"))
        # Everything else comes from dist/.
        rel = path.lstrip("/")
        return os.path.join(DIST, rel) if rel else os.path.join(DIST, "index.html")

    def log_message(self, *a):
        pass  # quiet


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    with Server(("127.0.0.1", PORT), Handler) as httpd:
        print(f"serving dist/ + /testdata on http://127.0.0.1:{PORT}")
        httpd.serve_forever()
