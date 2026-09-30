#!/usr/bin/env python3
"""Check trusted, untrusted, and wrong-host HTTPS certificates with the Rust TLS client."""

from __future__ import annotations

import argparse
import http.server
import os
from pathlib import Path
import ssl
import subprocess
import tempfile
import threading


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"Puffinbox TLS fixture\n")

    def log_message(self, *_args: object) -> None:
        pass


def run(command: list[str]) -> None:
    subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=30)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", required=True, type=Path)
    parser.add_argument("--image", help="run the probe with Docker host networking in this image")
    args = parser.parse_args()
    probe = args.probe.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="puffinbox-tls-") as directory:
        root = Path(directory)
        ca, ca_key = root / "ca.pem", root / "ca.key"
        run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Puffinbox test CA", "-keyout", str(ca_key), "-out", str(ca)])
        run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Untrusted test CA", "-keyout", str(root / "other.key"), "-out", str(root / "other.pem")])
        for name, alternative_name in [("trusted", "IP:127.0.0.1"), ("wrong-host", "DNS:wrong-host.invalid")]:
            key, request, certificate = root / f"{name}.key", root / f"{name}.csr", root / f"{name}.pem"
            run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={name}", "-keyout", str(key), "-out", str(request)])
            extensions = root / f"{name}.ext"
            extensions.write_text(f"subjectAltName={alternative_name}\n", encoding="ascii")
            run(["openssl", "x509", "-req", "-in", str(request), "-CA", str(ca), "-CAkey", str(ca_key), "-CAcreateserial", "-days", "1", "-extfile", str(extensions), "-out", str(certificate)])
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(certificate, key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                for trust in ["ca.pem", "other.pem"]:
                    url = f"https://127.0.0.1:{server.server_port}/"
                    expected_success = name == "trusted" and trust == "ca.pem"
                    environment = os.environ.copy()
                    environment.update(SSL_CERT_FILE=str(root / trust), SSL_CERT_DIR=str(root / "empty"), PUFFINBOX_TLS_TEST_URL=url)
                    (root / "empty").mkdir(exist_ok=True)
                    command = [str(probe)]
                    if args.image:
                        command = ["docker", "run", "--rm", "--network", "host", "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "10001:10001", "--mount", f"type=bind,source={probe},target=/tls-probe,readonly", "--mount", f"type=bind,source={root},target=/trust,readonly", "-e", f"SSL_CERT_FILE=/trust/{trust}", "-e", "SSL_CERT_DIR=/trust/empty", "-e", "PUFFINBOX_TLS_TEST_URL", "--entrypoint", "/tls-probe", args.image]
                        root.chmod(0o755)
                    result = subprocess.run(command, env=environment, capture_output=True, timeout=20)
                    if (result.returncode == 0) != expected_success:
                        raise SystemExit(f"TLS verification failed for {name} with {trust}: exit {result.returncode}")
                    print(f"PASS: {name} with {trust}: {'accepted' if expected_success else 'rejected'}")
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
