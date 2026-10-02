#!/usr/bin/env python3
"""Check HTTPS proxy, cookie, and remote policy behavior in an owned Docker fixture."""

from __future__ import annotations

import argparse
import datetime
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import socket
import ssl
import subprocess
import time
import uuid


def command(arguments: list[str], *, input_text: str | None = None, timeout: int = 60) -> str:
    result = subprocess.run(arguments, input=input_text, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        # Arguments may contain synthetic credentials. Never echo them or arbitrary output.
        raise RuntimeError(f"{arguments[0]} operation failed (exit {result.returncode})")
    return result.stdout.strip()


def write_new(path: Path, value: str) -> None:
    with path.open("x", encoding="utf-8") as output:
        output.write(value)
    path.chmod(0o600)


def certificates(root: Path) -> None:
    for name in ["ca", "other-ca"]:
        command(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                 "-subj", f"/CN=Puffinbox isolated {name}", "-keyout", str(root / f"{name}.key"),
                 "-out", str(root / f"{name}.pem"), "-addext", "keyUsage=critical,keyCertSign,cRLSign"])
    command(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
             "-keyout", str(root / "localhost.key"), "-out", str(root / "localhost.csr")])
    write_new(root / "localhost.ext", "\n".join([
        "subjectAltName=DNS:localhost,IP:127.0.0.1", "basicConstraints=critical,CA:false",
        "keyUsage=critical,digitalSignature,keyEncipherment", "extendedKeyUsage=serverAuth",
        "subjectKeyIdentifier=hash", "authorityKeyIdentifier=keyid,issuer", "",
    ]))
    command(["openssl", "x509", "-req", "-in", str(root / "localhost.csr"), "-CA", str(root / "ca.pem"),
             "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-days", "1",
             "-extfile", str(root / "localhost.ext"), "-out", str(root / "localhost.pem")])
    for path in root.iterdir():
        if path.is_file():
            path.chmod(0o600)


class Fixture:
    def __init__(self, root: Path, images: dict[str, str]):
        self.root = root
        self.images = images
        self.owner = "puffinbox-remote-" + uuid.uuid4().hex[:12]
        self.label = "org.puffinbox.remote-fixture=" + self.owner
        self.frontend = self.owner + "-front"
        self.containers: list[str] = []
        self.volumes: list[str] = []
        self.networks: list[str] = []
        self.port = 0
        self.origin = ""
        self.context = ssl.create_default_context(cafile=str(root / "ca.pem"))
        self.checks: list[str] = []

    def docker(self, *arguments: str, input_text: str | None = None) -> str:
        return command(["docker", *arguments], input_text=input_text)

    def check(self, name: str, condition: bool) -> None:
        if not condition:
            raise AssertionError(name)
        self.checks.append(name)
        print(f"PASS: {name}", flush=True)

    def create(self, role: str, image: str, arguments: list[str], process: list[str] | None = None,
               *, network: str | None = None) -> str:
        name = self.owner + "-" + role
        result = self.docker("create", "--name", name, "--label", self.label,
                             "--network", network or self.owner, "--network-alias", role, *arguments, image, *(process or []))
        self.containers.append(result)
        return result

    def volume(self, role: str) -> str:
        name = self.owner + "-" + role
        self.docker("volume", "create", "--label", self.label, name)
        self.volumes.append(name)
        return name

    def request(self, path: str, *, method: str = "GET", body: object = None,
                headers: dict[str, str] | None = None, remote: bool = False,
                expected: int = 200) -> tuple[dict[str, str], object]:
        request_headers = dict(headers or {})
        if remote:
            request_headers["X-Puffinbox-Test-Remote"] = "yes"
        payload = None
        if body is not None:
            payload = json.dumps(body).encode()
            request_headers["Content-Type"] = "application/json"
        connection = http.client.HTTPSConnection("127.0.0.1", self.port, context=self.context, timeout=15)
        request_headers["Host"] = f"localhost:{self.port}"
        try:
            connection.request(method, path, body=payload, headers=request_headers)
            response = connection.getresponse()
            content = response.read(2 * 1024 * 1024)
            if response.status != expected:
                raise AssertionError(f"{method} {path}: expected {expected}, received {response.status}")
            response_headers = {name.lower(): value for name, value in response.getheaders()}
            parsed = json.loads(content) if content and response_headers.get("content-type", "").startswith("application/json") else None
            return response_headers, parsed
        finally:
            connection.close()

    def direct_status(self, container: str, path: str, headers: dict[str, str], *, method: str = "GET") -> int:
        # Send tokens through stdin, never process arguments or container environment.
        program = (
            "import http.client,json,sys; r=json.load(sys.stdin); "
            "c=http.client.HTTPConnection('server',8096,timeout=10); "
            "c.request(r['method'],r['path'],headers=r['headers']); "
            "print(c.getresponse().status); c.close()"
        )
        result = self.docker("exec", "-i", container, "python3", "-c", program,
                             input_text=json.dumps({"method": method, "path": path, "headers": headers}))
        return int(result)

    def start(self) -> tuple[str, str]:
        self.docker("network", "create", "--internal", "--label", self.label, self.owner)
        self.networks.append(self.owner)
        # The backend network is internal. A separate frontend permits loopback publishing.
        self.docker("network", "create", "--label", self.label, self.frontend)
        self.networks.append(self.frontend)
        database_password = secrets.token_urlsafe(32)
        administrator_password = secrets.token_urlsafe(32)
        write_new(self.root / "database.env", f"POSTGRES_DB=remote_fixture\nPOSTGRES_USER=remote_fixture\nPOSTGRES_PASSWORD={database_password}\n")
        database = self.create("database", self.images["database"], [
            "--env-file", str(self.root / "database.env"),
            "--mount", f"type=volume,source={self.volume('database')},target=/var/lib/postgresql",
        ])
        self.docker("start", database)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            ready = subprocess.run(["docker", "exec", database, "pg_isready", "-h", "127.0.0.1", "-U", "remote_fixture", "-d", "remote_fixture"], capture_output=True, timeout=10)
            if ready.returncode == 0:
                break
            time.sleep(1)
        else:
            raise AssertionError("isolated database did not become ready")
        proxy = self.create("proxy", self.images["tools"], [
            "--user", f"{os.getuid()}:{os.getgid()}",
            "--publish", "127.0.0.1::8443", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges:true", "--pids-limit", "64",
            "--mount", f"type=bind,source={self.root},target=/fixture,readonly",
            "--mount", f"type=bind,source={Path(__file__).with_name('remote_access_proxy.py').resolve()},target=/proxy.py,readonly",
            "--entrypoint", "python3",
        ], ["/proxy.py"], network=self.frontend)
        self.docker("network", "connect", self.owner, proxy)
        self.docker("start", proxy)
        self.proxy = proxy
        info = json.loads(self.docker("inspect", proxy))[0]
        if not info["State"]["Running"]:
            write_new(self.root / "proxy-error.log", self.docker("logs", proxy))
            raise AssertionError("TLS fixture process exited before startup")
        bindings = info["NetworkSettings"]["Ports"].get("8443/tcp") or []
        if len(bindings) != 1:
            raise AssertionError("TLS fixture did not receive exactly one loopback port binding")
        binding = bindings[0]
        self.check("TLS fixture is published only on loopback", binding["HostIp"] == "127.0.0.1")
        self.port = int(binding["HostPort"])
        self.origin = f"https://localhost:{self.port}"
        proxy_ip = info["NetworkSettings"]["Networks"][self.owner]["IPAddress"]
        write_new(self.root / "server.env", "\n".join([
            f"DATABASE_URL=postgres://remote_fixture:{database_password}@database:5432/remote_fixture",
            "PUFFINBOX_BIND=0.0.0.0:8096", "PUFFINBOX_DATA_DIR=/data", "PUFFINBOX_WEB_ROOT=/app/web",
            f"PUFFINBOX_PUBLIC_BASE_URL={self.origin}", "PUFFINBOX_COOKIE_SECURE=true",
            f"PUFFINBOX_TRUSTED_PROXIES={proxy_ip}/32", "PUFFINBOX_LOCAL_NETWORKS=127.0.0.0/8",
            "PUFFINBOX_CORS_ORIGINS=https://allowed-client.invalid",
            "PUFFINBOX_BOOTSTRAP_ADMIN_USERNAME=remote-admin",
            f"PUFFINBOX_BOOTSTRAP_ADMIN_PASSWORD={administrator_password}", "",
        ]))
        server = self.create("server", self.images["server"], [
            "--env-file", str(self.root / "server.env"), "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges:true", "--pids-limit", "128",
            "--tmpfs", "/tmp:rw,noexec,nosuid,size=64m,mode=1777",
            "--mount", f"type=volume,source={self.volume('server')},target=/data",
        ])
        self.docker("start", server)
        server_info = json.loads(self.docker("inspect", server))[0]
        self.check("backend has no host-published port", not any((server_info["NetworkSettings"]["Ports"] or {}).values()))
        deadline = time.monotonic() + 90
        last_error = "no response"
        while time.monotonic() < deadline:
            try:
                self.request("/health/ready")
                break
            except (OSError, AssertionError) as error:
                if isinstance(error, ssl.SSLCertVerificationError):
                    raise AssertionError(f"TLS fixture certificate rejected: {error}") from error
                if not json.loads(self.docker("inspect", proxy))[0]["State"]["Running"]:
                    raise AssertionError("TLS fixture process exited before backend readiness") from error
                last_error = str(error)
                time.sleep(1)
        else:
            raise AssertionError(f"isolated HTTPS backend did not become ready: {last_error}")
        caller = self.docker("create", "--name", self.owner + "-caller", "--label", self.label,
                             "--user", f"{os.getuid()}:{os.getgid()}",
                             "--network", self.owner, "--read-only", "--cap-drop", "ALL",
                             "--security-opt", "no-new-privileges:true", "--entrypoint", "python3",
                             self.images["tools"], "-c", "import time; time.sleep(600)")
        self.containers.append(caller)
        self.docker("start", caller)
        self.proxy = proxy
        self.server = server
        return administrator_password, caller

    def verify(self, admin_password: str, caller: str) -> None:
        self.request("/System/Info/Public")
        self.check("trusted CA and localhost certificate accepted", True)
        for name, context, host in [
            ("wrong CA rejected", ssl.create_default_context(cafile=str(self.root / "other-ca.pem")), "localhost"),
            ("wrong certificate hostname rejected", self.context, "wrong-host.invalid"),
        ]:
            rejected = False
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=10) as raw:
                    with context.wrap_socket(raw, server_hostname=host):
                        pass
            except ssl.SSLCertVerificationError:
                rejected = True
            self.check(name, rejected)
        admin_response_headers, admin = self.request("/Users/AuthenticateByName", method="POST",
                                body={"Username": "remote-admin", "Pw": admin_password}, headers={"Origin": self.origin})
        admin_headers = {"X-Emby-Token": admin["AccessToken"]}
        _, info = self.request("/System/Info", headers=admin_headers)
        self.check("system information advertises configured HTTPS origin", info["LocalAddress"].rstrip("/") == self.origin)
        password = secrets.token_urlsafe(32)
        _, user = self.request("/Users", method="POST", headers=admin_headers,
                               body={"Name": "local-only", "Password": password})
        self.check("new account defaults to remote access disabled", user["Policy"]["EnableRemoteAccess"] is False)
        credentials = {"Username": "local-only", "Pw": password}
        response_headers, session = self.request("/Users/AuthenticateByName", method="POST", body=credentials,
                                                 headers={"Origin": self.origin})
        cookie = response_headers["set-cookie"]
        self.check("HTTPS session cookie has Secure, HttpOnly, and SameSite=Strict", all(value in cookie for value in ["; Secure", "; HttpOnly", "; SameSite=Strict"]))
        token_headers = {"X-Emby-Token": session["AccessToken"]}
        cookie_headers = {"Cookie": cookie.split(";", 1)[0], "Origin": self.origin}
        self.request("/Users/Me", headers=token_headers)
        self.check("local account authenticates through trusted proxy", True)
        self.request("/Users/AuthenticateByName", method="POST", body=credentials, remote=True, expected=401)
        self.check("remote password login denied while policy is disabled", True)
        self.request("/Users/Me", headers=token_headers, remote=True, expected=403)
        self.check("local token cannot bypass remote policy", True)
        self.request("/Users/Me", remote=True, expected=403, headers={**token_headers,
                     "X-Forwarded-For": "127.0.0.1", "Forwarded": "for=127.0.0.1;proto=https", "X-Real-IP": "127.0.0.1"})
        self.check("proxy strips spoofed client-address headers", True)
        direct = {**token_headers, "X-Forwarded-For": "127.0.0.1", "X-Forwarded-Proto": "https"}
        self.check("untrusted peer cannot spoof a local client", self.direct_status(caller, "/Users/Me", direct) == 403)
        headers, media = self.request("/Users/Me/MediaAccessToken", method="POST", headers=cookie_headers)
        self.check("same-origin HTTPS cookie can obtain a scoped media token", bool(media["AccessToken"]))
        self.check("token exchange is private and cannot be exposed through CORS", headers.get("cache-control") == "no-store" and not any(name in headers for name in ["access-control-allow-origin", "access-control-allow-credentials", "access-control-expose-headers"]))
        self.request("/Users/Me", headers={"X-Emby-Token": media["AccessToken"]}, expected=401)
        self.check("scoped media token cannot read general user API", True)
        for origin in [f"http://localhost:{self.port}", "https://allowed-client.invalid", "https://hostile.invalid"]:
            headers, _ = self.request("/Users/Me/MediaAccessToken", method="POST", expected=403,
                                       headers={**cookie_headers, "Origin": origin})
            self.check(f"token exchange rejects origin {origin}", headers.get("cache-control") == "no-store" and "access-control-allow-origin" not in headers)
        headers, _ = self.request("/Users/Me/MediaAccessToken", method="POST", headers={"Origin": self.origin}, expected=401)
        self.check("anonymous token exchange returns private unauthorized response", headers.get("cache-control") == "no-store")
        self.request("/Users/Me/MediaAccessToken", method="POST", headers=cookie_headers | {"Sec-Fetch-Site": "cross-site"}, expected=403)
        self.check("cross-site fetch metadata cannot override a matching origin", True)
        self.request("/Users/Me/MediaAccessToken", method="POST", headers=cookie_headers | token_headers, expected=401)
        self.check("cookie token exchange rejects an extra authorization credential", True)
        # A trusted proxy supplies the HTTPS scheme. An untrusted direct peer's header must be ignored.
        self.check("remote-enabled administrator is authenticated from the untrusted peer", self.direct_status(caller, "/Users/Me", admin_headers) == 200)
        admin_cookie = {"Cookie": admin_response_headers["set-cookie"].split(";", 1)[0],
                        "Host": f"localhost:{self.port}", "Origin": self.origin, "X-Forwarded-Proto": "https"}
        self.check("untrusted peer cannot forge HTTPS origin for a cookie mutation", self.direct_status(caller, "/Users/Me/MediaAccessToken", admin_cookie, method="POST") == 403)
        self.request("/Users/Me/Logout", method="POST", headers=cookie_headers | {"Origin": "https://hostile.invalid"}, expected=403)
        self.request("/Users/Me", headers=token_headers)
        self.check("cross-origin logout denied without revoking the valid session", True)
        self.request(f"/Users/{user['Id']}/Policy", method="POST", headers=admin_headers, body={"EnableRemoteAccess": True}, expected=204)
        _, remote_session = self.request("/Users/AuthenticateByName", method="POST", body=credentials, remote=True)
        remote_headers = {"X-Emby-Token": remote_session["AccessToken"]}
        self.request("/Users/Me", headers=remote_headers, remote=True)
        self.check("administrator can enable authenticated remote access", True)
        self.request(f"/Users/{user['Id']}/Policy", method="POST", headers=admin_headers, body={"EnableRemoteAccess": False}, expected=204)
        self.request("/Users/Me", headers=remote_headers, remote=True, expected=403)
        self.check("policy revocation applies to an existing remote session", True)
        headers, _ = self.request("/Users/Me/Logout", method="POST", headers=cookie_headers, expected=204)
        self.check("same-origin logout expires the secure cookie", all(value in headers["set-cookie"] for value in ["Max-Age=0", "; Secure", "; HttpOnly"]))
        self.request("/Users/Me", headers=token_headers, expected=401)
        self.request("/Users/Me/MediaAccessToken", method="POST", headers=cookie_headers, expected=401)
        self.check("logout revokes the parent session and cookie token exchange", True)

    def cleanup(self) -> None:
        # Only remove concrete resources this invocation created, after verifying their label.
        for container in reversed(self.containers):
            labels = json.loads(self.docker("inspect", container))[0]["Config"]["Labels"] or {}
            if labels.get("org.puffinbox.remote-fixture") != self.owner:
                raise AssertionError("container ownership changed; refusing cleanup")
            if container == getattr(self, "server", None):
                self.docker("stop", "--time", "60", container)
            self.docker("rm", "--force", container)
        for volume in self.volumes:
            labels = json.loads(self.docker("volume", "inspect", volume))[0]["Labels"] or {}
            if labels.get("org.puffinbox.remote-fixture") != self.owner:
                raise AssertionError("volume ownership changed; refusing cleanup")
            self.docker("volume", "rm", volume)
        for network in reversed(self.networks):
            labels = json.loads(self.docker("network", "inspect", network))[0]["Labels"] or {}
            if labels.get("org.puffinbox.remote-fixture") != self.owner:
                raise AssertionError("network ownership changed; refusing cleanup")
            self.docker("network", "rm", network)

    def save_diagnostics(self) -> None:
        states = []
        for container in self.containers:
            info = json.loads(self.docker("inspect", container))[0]
            role = info["Name"].removeprefix("/" + self.owner + "-")
            states.append({"role": role, "state": info["State"], "networks": list(info["NetworkSettings"]["Networks"])})
            logs = subprocess.run(["docker", "logs", container], capture_output=True, text=True, timeout=10)
            write_new(self.root / f"{role}-container.log", (logs.stdout + logs.stderr)[-1024 * 1024:])
        write_new(self.root / "container-states.json", json.dumps(states, indent=2) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True, help="already-built Puffinbox core image")
    parser.add_argument("--tools-image", required=True, help="external test image containing python3")
    parser.add_argument("--database-image", default="postgres:18.6-alpine")
    parser.add_argument("--output-dir", type=Path, required=True, help="fresh directory under ignored .local storage")
    args = parser.parse_args()
    repository = Path(__file__).resolve().parents[1]
    root = args.output_dir.resolve()
    if not root.is_relative_to(repository / ".local") or root == repository / ".local":
        parser.error("output directory must be a new child of repository .local")
    root.mkdir(parents=True, exist_ok=False)
    os.chmod(root, 0o700)
    images = {role: json.loads(command(["docker", "image", "inspect", reference]))[0]["Id"]
              for role, reference in {"server": args.image, "tools": args.tools_image, "database": args.database_image}.items()}
    certificates(root)
    fixture = Fixture(root, images)
    ledger = {
        "startedAtUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "gitHead": command(["git", "-C", str(repository), "rev-parse", "HEAD"]),
        "workingTreeDirty": bool(command(["git", "-C", str(repository), "status", "--porcelain"])),
        "images": images, "fixture": fixture.owner,
        "scope": "Loopback TLS proxy and synthetic remote addresses on an isolated Docker network; not external-network acceptance.",
        "harnessSha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "proxySha256": hashlib.sha256(Path(__file__).with_name("remote_access_proxy.py").read_bytes()).hexdigest(),
    }
    success = False
    try:
        password, caller = fixture.start()
        fixture.verify(password, caller)
        success = True
    except (AssertionError, RuntimeError, OSError, subprocess.SubprocessError) as error:
        ledger["failure"] = str(error)
        print(f"FAIL: {error}", flush=True)
    finally:
        if not success:
            try:
                fixture.save_diagnostics()
            except (AssertionError, RuntimeError, OSError, subprocess.SubprocessError) as error:
                ledger["diagnosticFailure"] = str(error)
        try:
            fixture.cleanup()
            ledger["ownedResourcesRemoved"] = True
        except (AssertionError, RuntimeError, OSError, subprocess.SubprocessError) as error:
            ledger["cleanupFailure"] = str(error)
            success = False
        ledger.update({"finishedAtUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                       "passed": success, "checks": fixture.checks})
        write_new(root / "results.json", json.dumps(ledger, indent=2) + "\n")
    print(f"HTTPS proxy checks: {len(fixture.checks)} passed; result {'PASS' if success else 'FAIL'}", flush=True)
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
