#!/usr/bin/env python3
"""TLS proxy used only by check_remote_access.py on an isolated Docker network."""

from __future__ import annotations

import http.client
import http.server
import ssl


class Proxy(http.server.BaseHTTPRequestHandler):
    def forward(self) -> None:
        # This synthetic selector belongs only to the loopback test fixture.
        # Never deploy it as a public proxy or accept it as a real client address.
        address = "198.51.100.27" if self.headers.get("X-Puffinbox-Test-Remote") == "yes" else "127.0.0.1"
        blocked = {
            "forwarded", "x-forwarded-for", "x-forwarded-proto", "x-forwarded-host",
            "x-real-ip", "x-puffinbox-test-remote", "connection", "transfer-encoding",
        }
        headers = {name: value for name, value in self.headers.items() if name.lower() not in blocked}
        headers.update({"X-Forwarded-For": address, "X-Forwarded-Proto": "https"})
        length = int(self.headers.get("Content-Length", "0"))
        if length < 0 or length > 64 * 1024:
            self.send_error(413)
            return
        body = self.rfile.read(length) if length else None
        connection = http.client.HTTPConnection("server", 8096, timeout=15)
        try:
            connection.request(self.command, self.path, body=body, headers=headers)
            response = connection.getresponse()
            payload = response.read(2 * 1024 * 1024 + 1)
            if len(payload) > 2 * 1024 * 1024:
                self.send_error(502)
                return
            self.send_response(response.status)
            for name, value in response.getheaders():
                if name.lower() not in {"connection", "transfer-encoding", "content-length"}:
                    self.send_header(name, value)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(payload)
        except (OSError, http.client.HTTPException):
            self.send_error(502)
        finally:
            connection.close()

    do_GET = forward
    do_HEAD = forward
    do_POST = forward
    do_OPTIONS = forward

    def log_message(self, *_args: object) -> None:
        # Do not log cookies, tokens, passwords, or query strings.
        pass


def main() -> None:
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 8443), Proxy)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_cert_chain("/fixture/localhost.pem", "/fixture/localhost.key")
    server.socket = context.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
