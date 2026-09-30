#!/usr/bin/env python3
"""Run a bounded scanner benchmark against a disposable local PostgreSQL container."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import secrets
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
POSTGRES_IMAGE = "postgres:18.6"
MAX_CATALOG_ENTRIES = 100_000
DEFAULT_FILES = 2_048
DEFAULT_FILES_PER_DIRECTORY = 128


class BenchmarkError(RuntimeError):
    pass


class HttpClient:
    def __init__(self, base_url: str, token: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.token = token

    def request(self, method: str, path: str, payload: Any | None = None) -> tuple[int, Any]:
        body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        if method.upper() not in {"GET", "HEAD", "OPTIONS"}:
            headers["Origin"] = self.base_url
        if self.token:
            headers["X-Emby-Token"] = self.token
        request = urllib.request.Request(self.base_url + path, data=body, headers=headers, method=method.upper())
        try:
            response = urllib.request.urlopen(request, timeout=10)
        except urllib.error.HTTPError as error:
            content = error.read()
            if content:
                try:
                    return error.code, json.loads(content)
                except ValueError:
                    return error.code, content.decode("utf-8", errors="replace")
            return error.code, None
        with response:
            content = response.read()
        if not content:
            return response.status, None
        try:
            return response.status, json.loads(content)
        except ValueError as error:
            raise BenchmarkError(f"{method} {path} returned invalid JSON") from error

    def json(self, method: str, path: str, payload: Any | None = None, expected: tuple[int, ...] = (200,)) -> Any:
        status, result = self.request(method, path, payload)
        if status not in expected:
            detail = f": {result}" if result else ""
            raise BenchmarkError(f"{method} {path} returned HTTP {status}{detail}")
        return result


def run_command(command: list[str], *, cwd: Path = ROOT, timeout: int = 120) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=timeout, check=False)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise BenchmarkError(f"command failed ({result.returncode}): {command[0]} {detail[:500]}")
    return result.stdout.strip()


def docker_available() -> None:
    try:
        run_command(["docker", "info"], timeout=20)
    except (OSError, subprocess.SubprocessError, BenchmarkError) as error:
        raise BenchmarkError("Docker Engine must be running and accessible for the disposable PostgreSQL container") from error


def build_server() -> Path:
    run_command(["cargo", "build", "--locked", "--release", "--bin", "puffinbox-server"], timeout=1_800)
    binary = ROOT / "target" / "release" / "puffinbox-server"
    if not binary.is_file():
        raise BenchmarkError(f"release server binary was not created at {binary}")
    return binary


def generate_tree(media_root: Path, files: int, files_per_directory: int) -> int:
    media_root.mkdir(parents=True, exist_ok=False)
    directory_count = math.ceil(files / files_per_directory)
    for index in range(files):
        shard = index // files_per_directory
        directory = media_root / f"shard-{shard:06d}"
        if index % files_per_directory == 0:
            directory.mkdir()
        (directory / f"clip-{index:08d}.mkv").touch()
    return directory_count


def catalogue_shape(files: int, files_per_directory: int) -> tuple[int, int]:
    """Return generated shard directories and rows, including the indexed root."""
    directory_count = math.ceil(files / files_per_directory)
    return directory_count, files + directory_count + 1


def start_database(container_name: str, run_id: str, username: str, password: str, database: str) -> str:
    run_command([
        "docker", "run", "--detach", "--rm",
        "--name", container_name,
        "--label", f"puffinbox.scanner-scale-run={run_id}",
        "--publish", "127.0.0.1::5432",
        "--env", f"POSTGRES_USER={username}",
        "--env", f"POSTGRES_PASSWORD={password}",
        "--env", f"POSTGRES_DB={database}",
        POSTGRES_IMAGE,
    ], timeout=60)

    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        ready = subprocess.run(
            ["docker", "exec", container_name, "pg_isready", "-U", username, "-d", database],
            capture_output=True, text=True, timeout=10, check=False,
        )
        if ready.returncode == 0:
            break
        time.sleep(0.5)
    else:
        raise BenchmarkError("the disposable PostgreSQL container did not become ready within 90 seconds")

    port_line = run_command(["docker", "port", container_name, "5432/tcp"])
    host, separator, port = port_line.rpartition(":")
    if not separator or host != "127.0.0.1" or not port.isdecimal():
        raise BenchmarkError("Docker did not publish the disposable database on a loopback-only port")
    wait_for_postgres_loopback(int(port), username, database)
    return port


def allocate_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def postgres_startup_message(username: str, database: str) -> bytes:
    """Build a PostgreSQL v3 startup packet; no password or SQL is sent."""
    if any("\0" in value for value in (username, database)):
        raise ValueError("PostgreSQL startup parameters cannot contain NUL bytes")
    parameters = (
        b"user\0" + username.encode("utf-8") + b"\0"
        + b"database\0" + database.encode("utf-8") + b"\0\0"
    )
    body = struct.pack("!I", 196_608) + parameters
    return struct.pack("!I", len(body) + 4) + body


def _receive_exact(connection: socket.socket, size: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < size:
        chunk = connection.recv(size - len(chunks))
        if not chunk:
            raise ConnectionError("PostgreSQL closed the startup connection")
        chunks.extend(chunk)
    return bytes(chunks)


def probe_postgres_loopback(port: int, username: str, database: str, *, timeout_seconds: float = 2.0) -> bool:
    """Check that the published port answers with a PostgreSQL auth request."""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=timeout_seconds) as connection:
            connection.settimeout(timeout_seconds)
            connection.sendall(postgres_startup_message(username, database))
            message_type = _receive_exact(connection, 1)
            length = struct.unpack("!I", _receive_exact(connection, 4))[0]
            if length < 8 or length > 1024 * 1024:
                return False
            payload = _receive_exact(connection, length - 4)
            if message_type != b"R" or len(payload) < 4:
                return False
            # AuthenticationOk and the supported password-auth challenges
            # are only sent after PostgreSQL has accepted the startup packet.
            return struct.unpack("!I", payload[:4])[0] in {0, 3, 5, 10}
    except (OSError, ConnectionError, struct.error, ValueError):
        return False


def wait_for_postgres_loopback(
    port: int,
    username: str,
    database: str,
    *,
    timeout_seconds: int = 60,
    poll_interval_seconds: float = 0.25,
) -> None:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        if probe_postgres_loopback(port, username, database):
            return
        time.sleep(poll_interval_seconds)
    raise BenchmarkError(
        "PostgreSQL was ready inside its container but did not accept a startup packet "
        f"through its loopback-published port within {timeout_seconds} seconds"
    )


def process_peak_rss_mib(process: subprocess.Popen[bytes]) -> float | None:
    status_path = Path("/proc") / str(process.pid) / "status"
    try:
        for line in status_path.read_text(encoding="ascii").splitlines():
            if line.startswith("VmHWM:"):
                return round(int(line.split()[1]) / 1024, 2)
    except (OSError, ValueError, IndexError):
        return None
    return None


def build_input_sha256() -> str:
    """Fingerprint the Rust crate inputs as they exist in the working tree."""
    paths = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
    for optional in (ROOT / "build.rs", ROOT / "rust-toolchain.toml", ROOT / ".cargo"):
        if optional.is_file():
            paths.append(optional)
        elif optional.is_dir():
            paths.extend(path for path in optional.rglob("*") if path.is_file())
    for folder in (ROOT / "src", ROOT / "migrations", ROOT / "examples" / "plugins"):
        if folder.exists():
            paths.extend(path for path in folder.rglob("*") if path.is_file())

    digest = hashlib.sha256()
    for path in sorted(set(paths), key=lambda item: item.relative_to(ROOT).as_posix()):
        digest.update(path.relative_to(ROOT).as_posix().encode("utf-8"))
        digest.update(b"\0")
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        digest.update(b"\0")
    return digest.hexdigest()


def binary_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_catalog_rows(
    container: str,
    username: str,
    password: str,
    database: str,
    library_id: str,
    *,
    roots_only: bool = False,
) -> int:
    root_filter = " AND parent_id IS NULL" if roots_only else ""
    query = f"SELECT count(*) FROM items WHERE library_id='{library_id}'::uuid{root_filter}"
    result = run_command([
        "docker", "exec", "-e", f"PGPASSWORD={password}", container,
        "psql", "-U", username, "-d", database, "-A", "-t", "-c", query,
    ], timeout=30)
    try:
        return int(result)
    except ValueError as error:
        raise BenchmarkError("could not read the disposable library item count") from error


def wait_ready(client: HttpClient, timeout_seconds: int = 90) -> None:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        try:
            status, _ = client.request("GET", "/health/ready")
            if status == 200:
                return
        except (OSError, urllib.error.URLError, TimeoutError):
            pass
        time.sleep(0.5)
    raise BenchmarkError("the scanner server did not become ready against its disposable database")


def authenticate(base_url: str, username: str, password: str) -> HttpClient:
    anonymous = HttpClient(base_url)
    result = anonymous.json("POST", "/Users/AuthenticateByName", {
        "Username": username,
        "Pw": password,
        "Client": "PuffinBox scanner scale harness",
        "DeviceName": "Disposable local benchmark",
        "DeviceId": f"scanner-scale-{uuid.uuid4().hex}",
        "Version": "0.1.0",
    })
    token = result.get("AccessToken") if isinstance(result, dict) else None
    user = result.get("User") if isinstance(result, dict) else None
    if not isinstance(token, str) or not isinstance(user, dict) or not user.get("IsAdministrator"):
        raise BenchmarkError("disposable benchmark account could not authenticate as administrator")
    return HttpClient(base_url, token)


def run_scan(client: HttpClient, library_id: str, timeout_seconds: int) -> tuple[dict[str, Any], float]:
    started_at = time.monotonic()
    status, result = client.request("POST", "/Library/Refresh", {})
    if status != 202 or not isinstance(result, dict) or int(result.get("Started", 0)) != 1:
        raise BenchmarkError("the disposable library scan did not start")
    deadline = started_at + timeout_seconds
    while time.monotonic() < deadline:
        statuses = client.json("GET", "/Library/ScanStatus")
        row = next((item for item in statuses if str(item.get("LibraryId")) == library_id), None)
        if row and row.get("Status") != "running":
            elapsed = time.monotonic() - started_at
            if row.get("Status") != "completed" or int(row.get("Errors", 0)) != 0:
                raise BenchmarkError(f"scan ended as {row.get('Status')} with {row.get('Errors')} errors")
            return row, elapsed
        time.sleep(0.2)
    raise BenchmarkError(f"scan did not complete within {timeout_seconds} seconds")


def stop_server(process: subprocess.Popen[bytes] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def cleanup_database(container_name: str, run_id: str) -> None:
    inspect = subprocess.run(
        ["docker", "inspect", "--format", '{{ index .Config.Labels "puffinbox.scanner-scale-run" }}', container_name],
        capture_output=True, text=True, timeout=15, check=False,
    )
    if inspect.returncode != 0:
        return
    if inspect.stdout.strip() != run_id:
        raise BenchmarkError(f"refusing to stop {container_name}: its disposable-run label does not match")
    stopped = subprocess.run(
        ["docker", "stop", "--time", "10", container_name],
        capture_output=True, text=True, timeout=20, check=False,
    )
    if stopped.returncode != 0:
        remaining = subprocess.run(
            ["docker", "inspect", "--format", '{{ index .Config.Labels "puffinbox.scanner-scale-run" }}', container_name],
            capture_output=True, text=True, timeout=15, check=False,
        )
        if remaining.returncode == 0 and remaining.stdout.strip() == run_id:
            removed = subprocess.run(
                ["docker", "rm", "--force", "--volumes", container_name],
                capture_output=True, text=True, timeout=20, check=False,
            )
            if removed.returncode != 0:
                raise BenchmarkError("could not remove the labeled disposable PostgreSQL container")


def run_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    if platform.system() != "Linux":
        raise BenchmarkError("run this harness in WSL or Linux; the scanner uses Linux descriptor-relative filesystem access")
    directory_count, entries = catalogue_shape(args.files, args.files_per_directory)
    if entries > MAX_CATALOG_ENTRIES:
        raise BenchmarkError(f"catalogue rows including the library root exceed the hard cap of {MAX_CATALOG_ENTRIES:,}")

    docker_available()
    build_input_hash = build_input_sha256()
    server_binary = build_server()
    if build_input_sha256() != build_input_hash:
        raise BenchmarkError("Rust build inputs changed while Cargo was building; retry after the source tree is stable")
    source_commit = run_command(["git", "rev-parse", "HEAD"])
    source_status = run_command(["git", "status", "--porcelain", "--untracked-files=all"])
    run_id = uuid.uuid4().hex
    container_name = f"puffinbox-scanner-scale-{run_id[:16]}"
    pg_user = "puffinbox_scale"
    pg_database = "puffinbox_scale"
    pg_password = secrets.token_hex(24)
    admin_name = "scanner-scale-admin"
    admin_password = secrets.token_urlsafe(32)
    server_process: subprocess.Popen[bytes] | None = None
    log_file = None
    report: dict[str, Any] | None = None

    try:
        with tempfile.TemporaryDirectory(prefix="puffinbox-scanner-scale-", dir="/tmp") as temporary:
            try:
                run_root = Path(temporary)
                media_root = run_root / "media"
                data_root = run_root / "server-data"
                generated_directories = generate_tree(media_root, args.files, args.files_per_directory)
                if generated_directories != directory_count:
                    raise BenchmarkError("generated shard-directory count did not match its requested shape")

                mapped_db_port = start_database(container_name, run_id, pg_user, pg_password, pg_database)
                server_port = allocate_loopback_port()
                base_url = f"http://127.0.0.1:{server_port}"
                server_env = {
                    key: value for key, value in os.environ.items()
                    if not key.startswith("PUFFINBOX_") and key not in {"DATABASE_URL", "RUST_LOG"}
                }
                server_env.update({
                    "DATABASE_URL": f"postgres://{pg_user}:{pg_password}@127.0.0.1:{mapped_db_port}/{pg_database}",
                    "PUFFINBOX_BIND": f"127.0.0.1:{server_port}",
                    "PUFFINBOX_DATA_DIR": str(data_root),
                    "PUFFINBOX_MAX_SCAN_WORKERS": "1",
                    "PUFFINBOX_BOOTSTRAP_ADMIN_USERNAME": admin_name,
                    "PUFFINBOX_BOOTSTRAP_ADMIN_PASSWORD": admin_password,
                    "RUST_LOG": "warn",
                })
                log_file = (run_root / "server.log").open("wb")
                server_process = subprocess.Popen(
                    [str(server_binary)], cwd=ROOT, env=server_env, stdout=log_file, stderr=subprocess.STDOUT,
                )
                anonymous = HttpClient(base_url)
                try:
                    wait_ready(anonymous)
                except BenchmarkError:
                    if server_process.poll() is not None:
                        log_file.flush()
                        tail = (run_root / "server.log").read_text(encoding="utf-8", errors="replace").splitlines()[-40:]
                        raise BenchmarkError("server exited before readiness:\n" + "\n".join(tail))
                    raise

                admin = authenticate(base_url, admin_name, admin_password)
                library_name = f"Scanner scale {run_id[:12]}"
                status, _ = admin.request("POST", "/Library/VirtualFolders", {
                    "Name": library_name,
                    "Locations": [str(media_root)],
                    "CollectionType": "movies",
                    "RefreshLibrary": False,
                })
                if status != 204:
                    raise BenchmarkError(f"could not create the temporary scanner library (HTTP {status})")
                libraries = admin.json("GET", "/Library/VirtualFolders")
                library = next((item for item in libraries if item.get("Name") == library_name), None)
                library_id = str(library.get("ItemId") or library.get("Id") or "") if library else ""
                if not library_id:
                    raise BenchmarkError("the temporary scanner library has no identifier")

                initial_status, initial_elapsed = run_scan(admin, library_id, args.scan_timeout)
                initial_rows = read_catalog_rows(container_name, pg_user, pg_password, pg_database, library_id)
                initial_root_rows = read_catalog_rows(
                    container_name, pg_user, pg_password, pg_database, library_id, roots_only=True,
                )
                expected_directories = directory_count + 1
                if (initial_rows != entries or int(initial_status.get("ItemsIndexed", -1)) != entries
                        or int(initial_status.get("FilesSeen", -1)) != args.files
                        or int(initial_status.get("DirectoriesSeen", -1)) != expected_directories
                        or initial_root_rows != 1):
                    raise BenchmarkError(
                        f"initial scan indexed {initial_status.get('ItemsIndexed')} items / {initial_rows} rows "
                        f"({initial_root_rows} root rows); expected {entries} rows including one configured library root"
                    )

                for shard in media_root.iterdir():
                    if shard.is_symlink() or not shard.is_dir():
                        raise BenchmarkError("the generated media root changed unexpectedly before stale cleanup")
                    shutil.rmtree(shard)
                cleanup_status, cleanup_elapsed = run_scan(admin, library_id, args.scan_timeout)
                remaining_rows = read_catalog_rows(container_name, pg_user, pg_password, pg_database, library_id)
                remaining_root_rows = read_catalog_rows(
                    container_name, pg_user, pg_password, pg_database, library_id, roots_only=True,
                )
                deleted_rows = initial_rows - remaining_rows
                expected_stale_rows = entries - 1
                if (remaining_rows != 1 or remaining_root_rows != 1 or deleted_rows != expected_stale_rows
                        or int(cleanup_status.get("ItemsIndexed", -1)) != 1):
                    raise BenchmarkError(
                        f"stale cleanup removed {deleted_rows} rows and left {remaining_rows} "
                        f"({remaining_root_rows} root rows); expected {expected_stale_rows} removed and one root row"
                    )

                peak_rss = process_peak_rss_mib(server_process)
                if build_input_sha256() != build_input_hash:
                    raise BenchmarkError("Rust build inputs changed during the benchmark; results were discarded")
                report = {
                    "benchmark": "puffinbox_library_scanner",
                    "source_commit": source_commit,
                    "source_working_tree_dirty": bool(source_status),
                    "build_input_sha256": build_input_hash,
                    "server_binary_sha256": binary_sha256(server_binary),
                    "postgres_image": POSTGRES_IMAGE,
                    "database_container_is_disposable": True,
                    "media_root_is_disposable_temp": True,
                    "generated_files": args.files,
                    "files_per_directory": args.files_per_directory,
                    "generated_directories": directory_count,
                    "expected_catalogue_entries": entries,
                    "expected_stale_entries": entries - 1,
                    "initial_scan": {
                        "status": initial_status.get("Status"),
                        "files_seen": int(initial_status.get("FilesSeen", 0)),
                        "directories_seen": int(initial_status.get("DirectoriesSeen", 0)),
                        "items_indexed": int(initial_status.get("ItemsIndexed", 0)),
                        "catalogue_rows": initial_rows,
                        "library_root_rows": initial_root_rows,
                        "elapsed_seconds": round(initial_elapsed, 3),
                        "items_per_second": round(entries / initial_elapsed, 1) if initial_elapsed else None,
                    },
                    "stale_cleanup_scan": {
                        "status": cleanup_status.get("Status"),
                        "items_indexed": int(cleanup_status.get("ItemsIndexed", 0)),
                        "rows_deleted": deleted_rows,
                        "rows_remaining": remaining_rows,
                        "library_root_rows_remaining": remaining_root_rows,
                        "elapsed_seconds": round(cleanup_elapsed, 3),
                        "rows_deleted_per_second": round(deleted_rows / cleanup_elapsed, 1) if cleanup_elapsed else None,
                    },
                    "server_process_peak_rss_mib": peak_rss,
                    "rss_measurement": "Linux /proc VmHWM; cumulative process high-water mark, null when unavailable",
                    "scan_timing_note": "Client wall time from refresh request through first completed status poll; polling interval is at most 200 ms plus request latency.",
                    "scope_note": "A bounded synthetic run on this machine; it does not establish petabyte or billion-file capacity.",
                }
            finally:
                stop_server(server_process)
                if log_file is not None:
                    log_file.close()
    finally:
        cleanup_database(container_name, run_id)

    assert report is not None
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--files", type=int, default=DEFAULT_FILES, help=f"synthetic media files to create (default: {DEFAULT_FILES}; hard maximum: {MAX_CATALOG_ENTRIES:,} total catalogue rows including the library root)")
    parser.add_argument("--files-per-directory", type=int, default=DEFAULT_FILES_PER_DIRECTORY, help=f"files per generated shard directory (default: {DEFAULT_FILES_PER_DIRECTORY})")
    parser.add_argument("--scan-timeout", type=int, default=300, help="per-scan completion timeout in seconds (default: 300; maximum: 3600)")
    args = parser.parse_args()
    if not 1 <= args.files <= MAX_CATALOG_ENTRIES:
        parser.error(f"--files must be between 1 and {MAX_CATALOG_ENTRIES:,}")
    if not 1 <= args.files_per_directory <= MAX_CATALOG_ENTRIES:
        parser.error(f"--files-per-directory must be between 1 and {MAX_CATALOG_ENTRIES:,}")
    if not 1 <= args.scan_timeout <= 3_600:
        parser.error("--scan-timeout must be between 1 and 3600")
    if catalogue_shape(args.files, args.files_per_directory)[1] > MAX_CATALOG_ENTRIES:
        parser.error(f"catalogue rows including generated files, shard directories, and the library root may not exceed {MAX_CATALOG_ENTRIES:,}")
    return args


def main() -> int:
    args = parse_args()
    try:
        report = run_benchmark(args)
    except (BenchmarkError, OSError, subprocess.SubprocessError) as error:
        print(f"scanner scale benchmark failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
