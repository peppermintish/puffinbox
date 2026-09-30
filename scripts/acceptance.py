#!/usr/bin/env python3
"""Run bounded semantic checks against an explicitly isolated local server."""

from __future__ import annotations

import argparse
from http import cookiejar
from datetime import datetime, timezone
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Mapping
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ENV_FILE = ROOT / ".local" / "acceptance" / "acceptance.env"
DEFAULT_FIXTURE_ROOT = ROOT / ".local" / "acceptance" / "media"
DEFAULT_RESULTS_FILE = ROOT / ".local" / "acceptance" / "acceptance-results.json"
FIXTURE_RELATIVE = Path("Movies") / "Puffinbox Synthetic Transcode Fixture.mkv"
DIRECT_PLAY_FIXTURE_RELATIVE = Path("Movies") / "Puffinbox Synthetic Direct Play Fixture.mp4"
SHUTDOWN_FIXTURE_RELATIVE = Path("Movies") / "Puffinbox Active HLS Shutdown Fixture.mkv"
MAX_WAIT_SECONDS = 90
COMPOSE_PROJECT = "puffinbox-acceptance"
EVIDENCE: list[dict[str, str]] = []


def read_env_file(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    if not path.is_file():
        raise SystemExit(f"Missing ignored acceptance settings file: {path}. Run scripts/prepare_acceptance.py first.")
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip()
    return values


def effective_settings(file_values: dict[str, str], environ: Mapping[str, str] | None = None) -> dict[str, str]:
    """Match Compose interpolation: process environment overrides the env file."""
    process_environment = os.environ if environ is None else environ
    return {
        **file_values,
        **{key: value for key, value in process_environment.items() if key in file_values},
    }


def fixture_root_from_values(values: dict[str, str]) -> Path:
    configured = Path(values.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT", str(DEFAULT_FIXTURE_ROOT))).expanduser()
    if not configured.is_absolute():
        configured = ROOT / configured
    return configured.resolve()


def paths_overlap(left: Path, right: Path) -> bool:
    return left == right or left in right.parents or right in left.parents


def validate_alternate_target(env_file: Path, results_file: Path, values: dict[str, str]) -> None:
    active_env = ENV_FILE.resolve()
    selected_env = env_file.resolve()
    if selected_env == active_env:
        return

    active_state_root = ENV_FILE.parent.resolve()
    selected_state_root = selected_env.parent
    active_results = DEFAULT_RESULTS_FILE.resolve()
    if paths_overlap(selected_state_root, active_state_root):
        raise SystemExit("Alternate acceptance settings must live outside the active acceptance state directory.")
    if results_file.resolve() == active_results or not results_file.resolve().is_relative_to(selected_state_root):
        raise SystemExit("Alternate acceptance results must be a new file inside the selected isolated state directory.")
    if os.path.lexists(results_file):
        raise SystemExit(f"Alternate acceptance results file already exists; preserve it: {results_file}")

    active_values = read_env_file(active_env) if active_env.is_file() else {}
    active_project = active_values.get("COMPOSE_PROJECT_NAME", COMPOSE_PROJECT)
    selected_project = values.get("COMPOSE_PROJECT_NAME", "")
    if not selected_project or selected_project in {COMPOSE_PROJECT, active_project}:
        raise SystemExit("Alternate acceptance settings must name a separate Compose project.")
    active_database = active_values.get("POSTGRES_DB", "puffinbox_acceptance")
    active_user = active_values.get("POSTGRES_USER", "puffinbox_acceptance")
    if values.get("POSTGRES_DB") == active_database or values.get("POSTGRES_USER") == active_user:
        raise SystemExit("Alternate acceptance settings must use a separate PostgreSQL database and user.")
    selected_fixture_root = fixture_root_from_values(values)
    active_fixture_root = fixture_root_from_values(active_values or {
        "PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT": str(DEFAULT_FIXTURE_ROOT),
    })
    if paths_overlap(selected_fixture_root, active_fixture_root):
        raise SystemExit("Alternate acceptance settings must use a separate fixture tree.")
    selected_url = urllib.parse.urlsplit(values.get("PUFFINBOX_ACCEPTANCE_URL", ""))
    active_url = urllib.parse.urlsplit(active_values.get("PUFFINBOX_ACCEPTANCE_URL", "http://127.0.0.1:18096"))
    if selected_url.port == active_url.port and selected_url.hostname in {"127.0.0.1", "localhost", "::1"} and active_url.hostname in {"127.0.0.1", "localhost", "::1"}:
        raise SystemExit("Alternate acceptance settings must target a separate loopback server port.")

    database_url = os.environ.get("DATABASE_URL", "")
    if database_url:
        parsed = urllib.parse.urlsplit(database_url)
        database_name = urllib.parse.unquote(parsed.path.lstrip("/"))
        database_user = urllib.parse.unquote(parsed.username or "")
        if database_name == active_database or database_user == active_user:
            raise SystemExit("DATABASE_URL uses the active acceptance database identity; refusing the alternate run.")
        if database_name != values.get("POSTGRES_DB") or database_user != values.get("POSTGRES_USER"):
            raise SystemExit("DATABASE_URL must match the selected alternate database and user.")
        if parsed.hostname not in {"127.0.0.1", "localhost", "::1"}:
            raise SystemExit("Alternate DATABASE_URL must point to a loopback disposable PostgreSQL service.")
        selected_database_port = int(values.get("PUFFINBOX_ACCEPTANCE_POSTGRES_PORT", "127.0.0.1:0").rsplit(":", 1)[1])
        if parsed.port != selected_database_port:
            raise SystemExit("Alternate DATABASE_URL must use the PostgreSQL port assigned in the selected settings file.")


class HttpClient:
    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")
        self.origin = urllib.parse.urlsplit(self.base_url).scheme + "://" + urllib.parse.urlsplit(self.base_url).netloc
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookiejar.CookieJar()))

    def request(self, method: str, path: str, payload: object | None = None, headers: dict[str, str] | None = None):
        body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode("utf-8")
        request_headers = {"Accept": "application/json", **(headers or {})}
        if body is not None:
            request_headers["Content-Type"] = "application/json"
        if method.upper() not in {"GET", "HEAD", "OPTIONS"}:
            request_headers.setdefault("Origin", self.origin)
        request = urllib.request.Request(self.base_url + path, data=body, headers=request_headers, method=method.upper())
        try:
            response = self.opener.open(request, timeout=25)
        except urllib.error.HTTPError as error:
            return error.code, error.headers, error.read()
        with response:
            return response.status, response.headers, response.read()

    def json(self, method: str, path: str, payload: object | None = None, expected: tuple[int, ...] = (200,), headers: dict[str, str] | None = None):
        status, response_headers, content = self.request(method, path, payload, headers=headers)
        if status not in expected:
            message = content.decode("utf-8", errors="replace").strip()
            message = re.sub(
                r'(?i)("?(?:access_?token|password|setup_?token|api_?key|authorization)"?\s*[:=]\s*)' +
                r'("[^"\\]*"|[^,\s}]+)',
                r"\1\"[redacted]\"",
                message,
            )
            if len(message) > 500:
                message = message[:500] + "…"
            detail = f"; response={message}" if message else ""
            raise AssertionError(f"{method} {path} returned HTTP {status}; expected {expected}{detail}")
        if not content:
            return status, response_headers, None
        try:
            return status, response_headers, json.loads(content)
        except ValueError as error:
            raise AssertionError(f"{method} {path} did not return valid JSON") from error


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def report(name: str, passed: bool | None, detail: str = "") -> None:
    status = "pending" if passed is None else "pass" if passed else "fail"
    EVIDENCE.append({"name": name, "status": status, "detail": detail})
    print(f"[{status.upper()}] {name}" + (f" — {detail}" if detail else ""))


def command_first_line(command: list[str], timeout: int = 10) -> str | None:
    try:
        result = subprocess.run(command, check=False, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return next((line.strip() for line in result.stdout.splitlines() if line.strip()), None)


def evidence_metadata(project_name: str = COMPOSE_PROJECT) -> dict[str, object]:
    try:
        git_prefix = ["git", "-c", f"safe.directory={ROOT}"]
        source_commit = subprocess.run(git_prefix + ["rev-parse", "HEAD"], cwd=ROOT, check=True, capture_output=True, text=True, timeout=10).stdout.strip()
        dirty = bool(subprocess.run(git_prefix + ["status", "--porcelain", "--untracked-files=all"], cwd=ROOT,
                                    check=True, capture_output=True, text=True, timeout=10).stdout.strip())
    except (OSError, subprocess.SubprocessError):
        source_commit, dirty = None, None

    image_id = None
    container = command_first_line([
        "docker", "ps", "--quiet",
        "--filter", f"label=com.docker.compose.project={project_name}",
        "--filter", "label=com.docker.compose.service=server",
    ])
    if container:
        image_id = command_first_line(["docker", "inspect", "--format", "{{.Image}}", container])

    return {
        "apiTargetVersion": "12.0.0",
        "source": {
            "gitHead": source_commit,
            "workingTreeDirty": dirty,
            "serverImageBuiltFromCommit": os.environ.get("PUFFINBOX_ACCEPTANCE_BUILD_SOURCE"),
        },
        "testedServerImageId": image_id,
        "tools": {
            "python": sys.version.split()[0],
            "rustc": command_first_line(["rustc", "--version"]),
            "cargo": command_first_line(["cargo", "--version"]),
            "docker": command_first_line(["docker", "--version"]),
            "compose": command_first_line(["docker", "compose", "version", "--short"]),
            "ffmpeg": command_first_line(["ffmpeg", "-version"]),
            "ffprobe": command_first_line(["ffprobe", "-version"]),
        },
    }


def wait_for_item(admin: HttpClient, fixture_path: Path, timeout_seconds: int) -> dict | None:
    deadline = time.monotonic() + timeout_seconds
    filename = fixture_path.name.casefold()
    stem = fixture_path.stem.casefold()
    while time.monotonic() < deadline:
        query = urllib.parse.urlencode({"SearchTerm": fixture_path.name, "Recursive": "true", "StartIndex": 0, "Limit": 100, "EnableTotalRecordCount": "true"})
        _, _, result = admin.json("GET", "/Items?" + query)
        items = result.get("Items", []) if isinstance(result, dict) else []
        match = next((item for item in items if str(item.get("Name", "")).casefold() in {filename, stem} or str(item.get("Path", "")).replace("\\", "/").casefold().endswith("/" + filename)), None)
        if match:
            return match
        time.sleep(1)
    return None


def find_item(admin: HttpClient, path: Path) -> dict | None:
    query = urllib.parse.urlencode({"SearchTerm": path.name, "Recursive": "true", "StartIndex": 0, "Limit": 100, "EnableTotalRecordCount": "true"})
    _, _, result = admin.json("GET", "/Items?" + query)
    items = result.get("Items", []) if isinstance(result, dict) else []
    expected_path = path.as_posix().casefold()
    return next((item for item in items if str(item.get("Path", "")).replace("\\", "/").casefold() == expected_path or str(item.get("Name", "")).casefold() == path.name.casefold()), None)


def verify_direct_play_acceptance(client: HttpClient, fixture_path: Path, timeout_seconds: int) -> None:
    item = wait_for_item(client, fixture_path, timeout_seconds)
    require(item is not None, "scanner did not index the synthetic H.264/AAC MP4 direct-play fixture")
    item_id = str(item.get("Id") or "")
    require(bool(item_id), "direct-play fixture has no catalog identifier")
    quoted_item_id = urllib.parse.quote(item_id)
    device_profile = {
        "DirectPlayProfiles": [{
            "Container": "mp4",
            "Type": "Video",
            "AudioCodec": "aac",
            "VideoCodec": "h264",
        }],
    }
    _, _, playback = client.json(
        "POST",
        f"/Items/{quoted_item_id}/PlaybackInfo",
        {"DeviceProfile": device_profile},
    )
    sources = playback.get("MediaSources") or []
    require(bool(sources), "direct-play fixture PlaybackInfo returned no MediaSources")
    source = sources[0]
    require(source.get("SupportsDirectPlay") is True,
            "matching MP4/H.264/AAC DeviceProfile did not offer direct play")
    require(source.get("DirectStreamUrl") == f"/Videos/{item_id}/stream",
            "direct-play MediaSource did not return the expected original-file stream URL")
    streams = source.get("MediaStreams") or []
    video_codec = next((str(stream.get("Codec") or "").casefold() for stream in streams if stream.get("Type") == "Video"), None)
    audio_codec = next((str(stream.get("Codec") or "").casefold() for stream in streams if stream.get("Type") == "Audio"), None)
    require(video_codec == "h264" and audio_codec == "aac",
            f"PlaybackInfo codecs were video={video_codec!r}, audio={audio_codec!r}; expected H.264/AAC")

    expected_bytes = fixture_path.read_bytes()
    total_length = len(expected_bytes)
    require(total_length > 128, "synthetic direct-play MP4 is too small for a useful byte-range check")
    stream_path = f"/Videos/{quoted_item_id}/stream"
    status, headers, content = client.request("GET", stream_path)
    require(status == 200, f"authenticated full direct-play stream returned HTTP {status}, not 200")
    require(headers.get("Content-Type", "").lower().split(";", 1)[0] == "video/mp4",
            "full direct-play stream did not return video/mp4")
    require(headers.get("Content-Length") == str(total_length), "full direct-play Content-Length did not match the source file")
    require(headers.get("Accept-Ranges", "").lower() == "bytes", "full direct-play stream did not advertise byte ranges")
    require(headers.get("Content-Range") is None, "full direct-play stream unexpectedly returned Content-Range")
    require(headers.get("ETag", "").startswith('W/"'), "full direct-play stream did not return the weak ETag")
    require(headers.get("X-Content-Type-Options", "").lower() == "nosniff", "full direct-play stream omitted nosniff")
    require(headers.get("Referrer-Policy", "").lower() == "no-referrer", "full direct-play stream referrer policy was unexpected")
    require(headers.get("Cache-Control", "").lower() == "private, no-store", "full direct-play stream cache policy was unexpected")
    require(headers.get("Content-Disposition") is None, "safe inline MP4 stream was incorrectly forced to a download")
    require(content == expected_bytes and hashlib.sha256(content).digest() == hashlib.sha256(expected_bytes).digest(),
            "authenticated full direct-play response did not return the original MP4 bytes")

    range_start = 128
    range_length = min(4096, total_length - range_start)
    range_end = range_start + range_length - 1
    range_status, range_headers, range_content = client.request(
        "GET", stream_path, headers={"Range": f"bytes={range_start}-{range_end}"}
    )
    expected_range = expected_bytes[range_start:range_end + 1]
    require(range_status == 206, f"authenticated direct-play range returned HTTP {range_status}, not 206")
    require(range_headers.get("Content-Range") == f"bytes {range_start}-{range_end}/{total_length}",
            "direct-play range Content-Range did not match the requested source interval")
    require(range_headers.get("Content-Length") == str(range_length), "direct-play range Content-Length was incorrect")
    require(range_headers.get("Accept-Ranges", "").lower() == "bytes", "range response did not advertise byte ranges")
    require(range_headers.get("Content-Type", "").lower().split(";", 1)[0] == "video/mp4",
            "direct-play range did not return video/mp4")
    require(range_headers.get("X-Content-Type-Options", "").lower() == "nosniff", "direct-play range omitted nosniff")
    require(range_headers.get("Cache-Control", "").lower() == "private, no-store", "direct-play range cache policy was unexpected")
    require(range_content == expected_range and hashlib.sha256(range_content).digest() == hashlib.sha256(expected_range).digest(),
            "authenticated direct-play range did not return the original MP4 bytes")
    report("H.264/AAC direct-play negotiation and authenticated original MP4 full/range streaming", True,
           f"{total_length} source bytes; HTTP 200 full and HTTP 206 bytes {range_start}-{range_end}")


def scan_library(admin: HttpClient, library_id: str, timeout_seconds: int, expected_status: str = "completed") -> dict:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        status, _, content = admin.request("POST", "/Library/Refresh", {})
        if status == 202:
            result = json.loads(content) if content else {}
            require(sum(int(result.get(key, 0)) for key in ("Started", "AlreadyRunning")) > 0,
                    "refresh request was accepted without starting or finding a running scan")
            break
        if status != 503:
            raise AssertionError(f"POST /Library/Refresh returned HTTP {status}")
        time.sleep(1)
    else:
        raise AssertionError("scanner capacity remained unavailable for the full wait period")

    while time.monotonic() < deadline:
        _, _, statuses = admin.json("GET", "/Library/ScanStatus")
        row = next((entry for entry in statuses if str(entry.get("LibraryId")) == library_id), None)
        if row and row.get("Status") != "running":
            require(row.get("Status") == expected_status, f"library scan ended as {row.get('Status')}; expected {expected_status}")
            if expected_status == "completed":
                require(int(row.get("Errors", 0)) == 0, f"library scan reported {row.get('Errors')} errors")
            elif expected_status == "completed_with_errors":
                require(int(row.get("Errors", 0)) > 0, "root replacement scan reported no error")
            return row
        time.sleep(0.25)
    raise AssertionError(f"library scan did not finish within {timeout_seconds} seconds")


def verify_scan_lifecycle(admin: HttpClient, library_id: str, media_dir: Path, timeout_seconds: int) -> None:
    lifecycle_path = media_dir / "Puffinbox Scan Lifecycle Fixture.mkv"
    fixture_source = media_dir / FIXTURE_RELATIVE.name
    shutil.copyfile(fixture_source, lifecycle_path)
    first_scan = scan_library(admin, library_id, timeout_seconds)
    original = find_item(admin, lifecycle_path)
    require(original is not None, "scanner did not index the newly created lifecycle fixture")
    item_id = str(original.get("Id") or "")
    require(bool(item_id), "new lifecycle fixture has no identifier")
    original_modified = original.get("DateModified")
    time.sleep(1.1)
    future = time.time() + 2
    os.utime(lifecycle_path, (future, future))
    modified_scan = scan_library(admin, library_id, timeout_seconds)
    updated = admin.json("GET", f"/Items/{urllib.parse.quote(item_id)}")[2]
    require(updated.get("DateModified") != original_modified, "scanner did not update DateModified after the fixture changed")

    lifecycle_path.unlink()
    deletion_scan = scan_library(admin, library_id, timeout_seconds)
    require(find_item(admin, lifecycle_path) is None, "scanner did not remove the deleted fixture during reconciliation")
    report("Scanner create, file-modify, delete, and stale-item reconciliation", True,
           f"scan item counts: {first_scan.get('ItemsIndexed')}, {modified_scan.get('ItemsIndexed')}, {deletion_scan.get('ItemsIndexed')}")


def verify_scanner_root_identity(
    admin: HttpClient,
    timeout_seconds: int,
    fixture_root: Path,
    trust_root_api: str = "/media/TrustRoot",
) -> None:
    trust_root = fixture_root / "TrustRoot"
    original_directory = fixture_root / "TrustRoot-original"
    fixture = trust_root / "Puffinbox Scanner Trust Fixture.mkv"
    source = fixture_root / FIXTURE_RELATIVE
    trust_root.mkdir(parents=True, exist_ok=True)
    if not fixture.is_file():
        shutil.copyfile(source, fixture)

    library_name = "Puffinbox Scanner Boundary"
    _, _, libraries = admin.json("GET", "/Library/VirtualFolders")
    library = next((entry for entry in libraries if entry.get("Name") == library_name), None)
    if library is None:
        admin.json("POST", "/Library/VirtualFolders", {
            "Name": library_name,
            "Locations": [trust_root_api],
            "CollectionType": "movies",
            "LibraryOptions": {"Enabled": True},
        }, expected=(204,))
        _, _, libraries = admin.json("GET", "/Library/VirtualFolders")
        library = next((entry for entry in libraries if entry.get("Name") == library_name), None)
    require(library is not None, "scanner identity library was not created")
    library_id = str(library.get("ItemId") or library.get("Id") or "")
    require(bool(library_id), "scanner identity library has no identifier")
    initial = scan_library(admin, library_id, timeout_seconds)
    original_item = find_item(admin, fixture)
    require(original_item is not None, "scanner did not index the trust-boundary fixture")
    original_item_id = str(original_item.get("Id") or "")
    require(bool(original_item_id), "trust-boundary item has no identifier")
    original_identity = (trust_root.stat().st_dev, trust_root.stat().st_ino)

    fixture.unlink()
    empty_scan = scan_library(admin, library_id, timeout_seconds)
    require(find_item(admin, fixture) is None, "successful scan of an unchanged empty root retained stale catalog rows")
    require((trust_root.stat().st_dev, trust_root.stat().st_ino) == original_identity, "deleting root contents unexpectedly changed the root directory identity")

    shutil.copyfile(source, fixture)
    restored_scan = scan_library(admin, library_id, timeout_seconds)
    restored = find_item(admin, fixture)
    require(restored is not None and str(restored.get("Id")) == original_item_id, "scanner did not restore the fixture row before root replacement")
    require(initial.get("Status") == empty_scan.get("Status") == restored_scan.get("Status") == "completed", "same-inode scan lifecycle did not complete cleanly")

    if original_directory.exists():
        raise AssertionError("scanner trust test refuses to reuse its reserved backup directory")
    try:
        trust_root.rename(original_directory)
        trust_root.mkdir()
        replacement_identity = (trust_root.stat().st_dev, trust_root.stat().st_ino)
        require(replacement_identity != original_identity, "replacement root unexpectedly reused the original directory identity")
        replaced_scan = scan_library(admin, library_id, timeout_seconds, expected_status="completed_with_errors")
        require(replaced_scan.get("Status") == "completed_with_errors", "replacement-root scan did not preserve its error state")
        preserved = admin.json("GET", f"/Items/{urllib.parse.quote(original_item_id)}")[2]
        require(str(preserved.get("Id")) == original_item_id, "root identity mismatch reconciled away previously indexed rows")
    finally:
        if trust_root.exists():
            trust_root.rmdir()
        if original_directory.exists():
            original_directory.rename(trust_root)

    scan_library(admin, library_id, timeout_seconds)
    admin.json("DELETE", "/Library/VirtualFolders?Name=" + urllib.parse.quote(library_name), expected=(204,))
    report("Scanner root identity boundaries", True,
           f"unchanged-inode statuses: {initial.get('Status')}, {empty_scan.get('Status')}, {restored_scan.get('Status')}; replacement status: completed_with_errors with old row retained")


def verify_container_restart(
    item_id: str,
    base_url: str,
    admin_username: str,
    admin_password: str,
    active_play_session_id: str,
    expected_position_ticks: int,
    env_file: Path,
    project_name: str,
) -> None:
    file_settings = read_env_file(env_file)
    settings = effective_settings(file_settings, environ={} if project_name != COMPOSE_PROJECT else None)
    command_environment = os.environ.copy()
    if project_name != COMPOSE_PROJECT:
        for name in file_settings:
            command_environment.pop(name, None)
    command = ["docker", "compose", "--project-name", project_name, "--env-file", str(env_file), "-f", str(ROOT / "docker-compose.yml"), "-f", str(ROOT / "docker-compose.acceptance.yml")]
    if settings.get("PUFFINBOX_ACCEPTANCE_FFMPEG") == "1":
        command += ["-f", str(ROOT / "docker-compose.ffmpeg-runtime.yml")]
    container = subprocess.run(command + ["ps", "-q", "server"], check=True, capture_output=True, text=True, timeout=20, env=command_environment).stdout.strip()
    require(bool(container), "acceptance server container could not be identified before graceful stop")
    database = subprocess.run(command + ["ps", "-q", "database"], check=True, capture_output=True, text=True, timeout=20, env=command_environment).stdout.strip()
    require(bool(database), "acceptance database container could not be identified before graceful stop")
    for service_container in (container, database):
        project = subprocess.run(["docker", "inspect", "--format", "{{index .Config.Labels \"com.docker.compose.project\"}}", service_container], check=True, capture_output=True, text=True, timeout=20).stdout.strip()
        require(project == project_name, f"refusing a restart because container project label is {project!r}")
        mounts = json.loads(subprocess.run(["docker", "inspect", "--format", "{{json .Mounts}}", service_container], check=True, capture_output=True, text=True, timeout=20).stdout)
        named_volumes = [mount.get("Name") for mount in mounts if mount.get("Type") == "volume"]
        require(bool(named_volumes), "acceptance service has no named persistence volume to verify")
        for volume in named_volumes:
            volume_project = subprocess.run(["docker", "volume", "inspect", "--format", "{{index .Labels \"com.docker.compose.project\"}}", volume], check=True, capture_output=True, text=True, timeout=20).stdout.strip()
            require(volume_project == project_name, f"refusing a restart because volume {volume!r} belongs to project {project_name!r}")
    process_table = subprocess.run(["docker", "top", container], check=True, capture_output=True, text=True, timeout=20).stdout
    require(any("ffmpeg" in line.casefold() for line in process_table.splitlines()), "no external FFmpeg process was active immediately before server shutdown")
    report("Server shutdown requested while an HLS FFmpeg process was active", True)
    shutdown_started = datetime.now(timezone.utc).isoformat()
    subprocess.run(command + ["stop", "--timeout", "60", "server"], check=True, capture_output=True, text=True, timeout=75, env=command_environment)
    state = subprocess.run(["docker", "inspect", "--format", "{{.State.ExitCode}} {{.State.OOMKilled}}", container], check=True, capture_output=True, text=True, timeout=20).stdout.strip()
    require(state == "0 false", f"server container did not exit cleanly on graceful stop ({state})")
    log_result = subprocess.run(["docker", "logs", "--since", shutdown_started, container], check=True, capture_output=True, text=True, timeout=20)
    logs = log_result.stdout + log_result.stderr
    require("media shutdown exceeded its drain deadline" not in logs.casefold(), "server reported that the FFmpeg shutdown hook missed its drain deadline")
    require("media child required forced termination" not in logs.casefold(), "FFmpeg did not exit within the graceful termination period")
    require("all media children drained during shutdown" in logs, "server did not confirm that it reaped every media child before exit")
    subprocess.run(command + ["up", "-d", "--no-deps", "server"], check=True, capture_output=True, text=True, timeout=60, env=command_environment)
    client = HttpClient(base_url)
    deadline = time.monotonic() + MAX_WAIT_SECONDS
    while time.monotonic() < deadline:
        try:
            status, _, _ = client.request("GET", "/health/ready")
        except (urllib.error.URLError, OSError):
            time.sleep(1)
            continue
        if status == 200:
            break
        time.sleep(1)
    else:
        raise AssertionError("server did not become ready after a graceful restart")
    login(client, admin_username, admin_password)
    _, _, item = client.json("GET", f"/Items/{urllib.parse.quote(item_id)}")
    require(str(item.get("Id")) == item_id, "catalog item did not persist across server restart")
    resume = client.json("GET", f"/UserItems/{urllib.parse.quote(item_id)}")[2]
    require(resume.get("PlaybackPositionTicks") == expected_position_ticks,
            f"shutdown did not retain the last committed playback position for play session {active_play_session_id}")
    require(resume.get("Played") is False, "shutdown incorrectly marked the interrupted item as played to completion")
    session_uuid = str(uuid.UUID(active_play_session_id))
    database_env = os.environ.copy()
    database_env["PGPASSWORD"] = settings["POSTGRES_PASSWORD"]
    session_query = f"SELECT ended_at IS NOT NULL FROM playback_sessions WHERE id='{session_uuid}'::uuid"
    ended = subprocess.run(
        ["docker", "exec", "-e", "PGPASSWORD", database, "psql", "-U", settings["POSTGRES_USER"],
         "-d", settings["POSTGRES_DB"], "-A", "-t", "-c", session_query],
        check=True, capture_output=True, text=True, timeout=20, env=database_env,
    ).stdout.strip()
    require(ended == "t", f"active playback session {session_uuid} was not closed in PostgreSQL after shutdown")
    old_session_status, _, _ = client.request("GET", f"/Videos/{urllib.parse.quote(item_id)}/hls/{urllib.parse.quote(active_play_session_id)}/playlist.m3u8")
    require(old_session_status in (404, 410), f"pre-shutdown HLS session remained addressable after restart (HTTP {old_session_status})")
    report("Active HLS job shutdown, playback-row closure, committed resume position, and catalog persistence", True)


def login(client: HttpClient, username: str, password: str, headers: dict[str, str] | None = None, identity: dict[str, str] | None = None) -> tuple[dict, str]:
    payload = {
        "Username": username, "Pw": password, "Client": "Puffinbox acceptance",
        "DeviceName": "Isolated local acceptance", "DeviceId": f"puffinbox-acceptance-{uuid.uuid4()}", "Version": "0.1.0",
    }
    payload.update(identity or {})
    _, _, result = client.json("POST", "/Users/AuthenticateByName", payload, headers=headers)
    require(isinstance(result, dict) and isinstance(result.get("User"), dict) and isinstance(result.get("AccessToken"), str), "authentication returned an invalid user/token payload")
    return result["User"], result["AccessToken"]


def ensure_user(admin: HttpClient, username: str, password: str, library_id: str | None, *, playback: bool, blocked_categories: list[str] | None = None) -> dict:
    _, _, users = admin.json("GET", "/Users")
    existing = next((user for user in users if user.get("Name", "").casefold() == username.casefold()), None)
    if existing:
        admin.json("DELETE", f"/Users/{urllib.parse.quote(str(existing['Id']))}", expected=(204,))
    _, _, user = admin.json("POST", "/Users", {
        "Name": username,
        "Password": password,
        "IsAdministrator": False,
        "EnableRemoteAccess": False,
        "EnableMediaPlayback": playback,
        "EnableAllFolders": False,
        "EnabledFolders": [library_id] if library_id else [],
        "BlockUnratedItems": blocked_categories or [],
    }, expected=(200,))
    return user


def verify_playback_sessions(admin: HttpClient, viewer: HttpClient, item_id: str) -> None:
    session_id = str(uuid.uuid4())
    item_path = urllib.parse.quote(item_id)
    initial = admin.json("GET", f"/UserItems/{item_path}")[2]
    require(initial.get("Played") is not True, "playback test account was not reset before resumable-position checks")
    start = {"ItemId": item_id, "PlaySessionId": session_id, "PositionTicks": 0, "PlayMethod": "Transcode"}
    admin.json("POST", "/Sessions/Playing", start, expected=(204,))
    unauthorized_progress = viewer.request("POST", "/Sessions/Playing/Progress", {
        "ItemId": item_id, "PlaySessionId": session_id, "PositionTicks": 25_000_000,
    })[0]
    require(unauthorized_progress in (401, 403, 404), f"another user could update the playback session (HTTP {unauthorized_progress})")
    admin.json("POST", "/Sessions/Playing/Progress", {
        "ItemId": item_id, "PlaySessionId": session_id, "PositionTicks": 50_000_000, "PlayMethod": "Transcode",
    }, expected=(204,))
    progress = admin.json("GET", f"/UserItems/{item_path}")[2]
    require(progress.get("PlaybackPositionTicks") == 50_000_000, "playback progress did not save this account's resume position")
    admin.json("POST", "/Sessions/Playing/Stopped", {
        "ItemId": item_id, "PlaySessionId": session_id, "PositionTicks": 50_000_000, "PlayedToCompletion": False,
    }, expected=(204,))
    stopped = admin.json("GET", f"/UserItems/{item_path}")[2]
    require(stopped.get("PlaybackPositionTicks") == 50_000_000 and stopped.get("Played") is False,
            f"playback stop did not preserve the resumable position (position={stopped.get('PlaybackPositionTicks')}, played={stopped.get('Played')})")

    completed_session = str(uuid.uuid4())
    admin.json("POST", "/Sessions/Playing", {
        "ItemId": item_id, "PlaySessionId": completed_session, "PositionTicks": 50_000_000, "PlayMethod": "Transcode",
    }, expected=(204,))
    admin.json("POST", "/Sessions/Playing/Stopped", {
        "ItemId": item_id, "PlaySessionId": completed_session, "PositionTicks": 50_000_000, "PlayedToCompletion": True,
    }, expected=(204,))
    completed = admin.json("GET", f"/UserItems/{item_path}")[2]
    require(completed.get("Played") is True, "completed playback was not recorded for this account")

    replay_session = str(uuid.uuid4())
    admin.json("POST", "/Sessions/Playing", {
        "ItemId": item_id, "PlaySessionId": replay_session, "PositionTicks": 0, "PlayMethod": "Transcode",
    }, expected=(204,))
    admin.json("POST", "/Sessions/Playing/Progress", {
        "ItemId": item_id, "PlaySessionId": replay_session, "PositionTicks": 70_000_000, "PlayMethod": "Transcode",
    }, expected=(204,))
    admin.json("POST", "/Sessions/Playing/Stopped", {
        "ItemId": item_id, "PlaySessionId": replay_session, "PositionTicks": 70_000_000, "PlayedToCompletion": False,
    }, expected=(204,))
    replayed = admin.json("GET", f"/UserItems/{item_path}")[2]
    require(replayed.get("PlaybackPositionTicks") == 70_000_000 and replayed.get("Played") is True,
            "a partial replay cleared the item's previously watched state")
    report("Playback session ownership, progress, stop, resume position, and completion", True)


def run(args: argparse.Namespace) -> int:
    values = read_env_file(args.env_file)
    environment_overrides = {
        key: value for key, value in os.environ.items() if key.startswith("PUFFINBOX_ACCEPTANCE_")
    }
    if args.env_file.resolve() == ENV_FILE.resolve():
        values.update(environment_overrides)
    validate_alternate_target(args.env_file, args.results_file, values)
    if values.get("PUFFINBOX_ACCEPTANCE_ISOLATED") != "1":
        raise SystemExit("Refusing to run: the generated configuration does not identify an isolated acceptance server.")
    project_name = values.get("COMPOSE_PROJECT_NAME", "")
    if args.env_file.resolve() == ENV_FILE.resolve():
        if project_name != COMPOSE_PROJECT:
            raise SystemExit("Refusing to run: the generated settings do not name the dedicated acceptance Compose project.")
    elif not re.fullmatch(r"puffinbox-acceptance-[a-f0-9]{8}", project_name):
        raise SystemExit("Refusing to run: alternate settings must use a generated, unique acceptance Compose project name.")
    base_url = values.get("PUFFINBOX_ACCEPTANCE_URL", "")
    parsed = urllib.parse.urlsplit(base_url)
    if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost", "::1"} or parsed.username or parsed.password:
        raise SystemExit("Refusing to run: PUFFINBOX_ACCEPTANCE_URL must be a plain-HTTP loopback address with no embedded credentials.")
    for name in ("PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME", "PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD", "PUFFINBOX_ACCEPTANCE_VIEWER_USERNAME", "PUFFINBOX_ACCEPTANCE_VIEWER_PASSWORD", "PUFFINBOX_ACCEPTANCE_PEER_USERNAME", "PUFFINBOX_ACCEPTANCE_PEER_PASSWORD", "PUFFINBOX_ACCEPTANCE_DENIED_USERNAME", "PUFFINBOX_ACCEPTANCE_DENIED_PASSWORD", "PUFFINBOX_ACCEPTANCE_UNRATED_USERNAME", "PUFFINBOX_ACCEPTANCE_UNRATED_PASSWORD"):
        if not values.get(name):
            raise SystemExit(f"Missing generated local setting: {name}")

    fixture_root = fixture_root_from_values(values)
    if "PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT" in values:
        for key, relative in (
            ("PUFFINBOX_ACCEPTANCE_MEDIA_ROOT", "Movies"),
            ("PUFFINBOX_ACCEPTANCE_PHOTO_ROOT", "Photos"),
            ("PUFFINBOX_ACCEPTANCE_TRUST_ROOT", "TrustRoot"),
            ("PUFFINBOX_ACCEPTANCE_MUSIC_ROOT", "Music"),
            ("PUFFINBOX_ACCEPTANCE_BOOKS_ROOT", "Books"),
        ):
            if key not in environment_overrides:
                values[key] = str(fixture_root / relative)
    if args.host_media_paths:
        for key, relative in (
            ("PUFFINBOX_ACCEPTANCE_MEDIA_ROOT", "Movies"),
            ("PUFFINBOX_ACCEPTANCE_PHOTO_ROOT", "Photos"),
            ("PUFFINBOX_ACCEPTANCE_TRUST_ROOT", "TrustRoot"),
            ("PUFFINBOX_ACCEPTANCE_MUSIC_ROOT", "Music"),
            ("PUFFINBOX_ACCEPTANCE_BOOKS_ROOT", "Books"),
        ):
            values[key] = str(fixture_root / relative)
    media_root = values.get("PUFFINBOX_ACCEPTANCE_MEDIA_ROOT", "/media/Movies").rstrip("/")
    photo_root = values.get("PUFFINBOX_ACCEPTANCE_PHOTO_ROOT", "/media/Photos").rstrip("/")
    fixture_path = fixture_root / FIXTURE_RELATIVE
    direct_play_fixture_path = fixture_root / DIRECT_PLAY_FIXTURE_RELATIVE
    if not args.direct_play_only and not fixture_path.is_file():
        raise SystemExit(f"Missing generated synthetic fixture: {fixture_path}")
    if not direct_play_fixture_path.is_file():
        raise SystemExit(f"Missing generated H.264/AAC direct-play fixture: {direct_play_fixture_path}")
    client = HttpClient(base_url)

    readiness_deadline = time.monotonic() + MAX_WAIT_SECONDS
    readiness_status = None
    while time.monotonic() < readiness_deadline:
        try:
            readiness_status, _, _ = client.request("GET", "/health/ready")
        except urllib.error.URLError:
            readiness_status = None
        if readiness_status == 200:
            break
        if readiness_status not in (None, 502, 503, 504):
            raise AssertionError(f"database readiness endpoint returned HTTP {readiness_status}")
        time.sleep(1)
    else:
        raise AssertionError("isolated server did not become database-ready before the startup deadline")
    status, _, _ = client.request("GET", "/Users")
    require(status in (401, 403), f"unauthenticated administrator request was accepted (HTTP {status})")
    _, _, ready = client.json("GET", "/health/ready")
    require(isinstance(ready, dict), "readiness endpoint returned no JSON status")
    report("PostgreSQL-backed readiness and unauthenticated admin rejection", True)

    if args.health_soak_seconds > 0:
        deadline = time.monotonic() + args.health_soak_seconds
        while time.monotonic() < deadline:
            status, _, _ = client.request("GET", "/health/ready")
            require(status == 200, f"readiness stopped during the {args.health_soak_seconds}-second liveness check (HTTP {status})")
            time.sleep(min(1, max(0, deadline - time.monotonic())))
        report(f"Readiness remains available for {args.health_soak_seconds} seconds", True)

    admin_user, admin_token = login(client, values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"], values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"])
    require(admin_user.get("IsAdministrator") is True or admin_user.get("Policy", {}).get("IsAdministrator") is True, "bootstrap account is not an administrator")
    _, _, startup = client.json("GET", "/Startup/Configuration")
    require(startup.get("IsStartupWizardCompleted") is True, "startup wizard is not complete after admin seeding")
    report("Bootstrap administrator authentication and session", True)

    client_identity = {
        "Authorization": 'MediaBrowser Client="Jellyfin Desktop", Device="Jellyfin Desktop", DeviceId="acceptance-jmp-device", Version="12.0.0"',
    }
    desktop_client = HttpClient(base_url)
    login(desktop_client, values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"], values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"], headers=client_identity, identity={
        "Client": "Jellyfin Desktop", "DeviceName": "Jellyfin Desktop", "DeviceId": "acceptance-jmp-device", "Version": "12.0.0",
    })
    _, _, sessions = desktop_client.json("GET", "/Sessions")
    matched_session = next((entry for entry in sessions if entry.get("DeviceId") == "acceptance-jmp-device"), None)
    require(matched_session is not None and matched_session.get("Client") == "Jellyfin Desktop" and matched_session.get("DeviceName") == "Jellyfin Desktop",
            "MediaBrowser client identity was not retained in the authenticated session")
    report("MediaBrowser client metadata login and session identity", True)

    conflicting_login = HttpClient(base_url)
    conflicting_identity = {
        "Username": values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"],
        "Pw": values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"],
        "Client": "Body Client",
        "DeviceName": "Jellyfin Desktop",
        "DeviceId": "acceptance-conflicting-client",
        "Version": "12.0.0",
    }
    conflict_status, _, _ = conflicting_login.request(
        "POST", "/Users/AuthenticateByName", conflicting_identity,
        headers={"Authorization": 'MediaBrowser Client="Header Client", Device="Jellyfin Desktop", DeviceId="acceptance-conflicting-client", Version="12.0.0"'},
    )
    require(conflict_status == 400, f"conflicting MediaBrowser header/body identity was accepted (HTTP {conflict_status})")
    report("Conflicting MediaBrowser header and body identity rejection", True)

    token_client = HttpClient(base_url)
    _, _, by_header = token_client.json("GET", "/Users/Me", headers={"X-Emby-Token": admin_token})
    require(str(by_header.get("Id")) == str(admin_user.get("Id")), "X-Emby-Token did not authenticate the same user")
    _, _, by_bearer = token_client.json("GET", "/Users/Me", headers={"Authorization": f"Bearer {admin_token}"})
    require(str(by_bearer.get("Id")) == str(admin_user.get("Id")), "Bearer token did not authenticate the same user")
    anonymous = HttpClient(base_url)
    query_status, _, _ = anonymous.request("GET", "/Users/Me?api_key=not-a-token")
    require(query_status in (401, 403), f"query parameter authenticated an anonymous request (HTTP {query_status})")
    origin_status, _, _ = client.request("POST", "/Library/Refresh", {}, headers={"Origin": "http://attacker.invalid"})
    require(origin_status == 403, f"cross-origin state-changing request was accepted (HTTP {origin_status})")
    logout_client = HttpClient(base_url)
    _, logout_token = login(logout_client, values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"], values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"])
    logout_client.json("POST", "/Sessions/Logout", expected=(204,))
    revoked_status, _, _ = anonymous.request("GET", "/Users/Me", headers={"X-Emby-Token": logout_token})
    require(revoked_status in (401, 403), f"logout did not revoke the issued token (HTTP {revoked_status})")
    report("Cookie, header and bearer authentication; query rejection and logout revocation", True)

    media_folder_url = "/Library/VirtualFolders"
    _, _, current_libraries = client.json("GET", media_folder_url)
    library_name = "Puffinbox Acceptance Media"
    library = next((entry for entry in current_libraries if entry.get("Name") == library_name), None)
    if library is None:
        client.json("POST", media_folder_url, {"Name": library_name, "Locations": [media_root, photo_root], "CollectionType": "movies", "LibraryOptions": {"Enabled": True}}, expected=(204,))
        _, _, current_libraries = client.json("GET", media_folder_url)
        library = next((entry for entry in current_libraries if entry.get("Name") == library_name), None)
    require(library is not None, "acceptance library was not created or returned")
    library_id = str(library.get("ItemId") or library.get("Id") or "")
    require(bool(library_id), "acceptance library has no identifier")
    report("Isolated synthetic-media library setup", True)

    if args.direct_play_only:
        scan_library(client, library_id, args.scan_timeout)
        verify_direct_play_acceptance(client, direct_play_fixture_path, args.scan_timeout)
        return 0

    initial_scan = scan_library(client, library_id, args.scan_timeout)
    fixture = wait_for_item(client, fixture_path, args.scan_timeout)
    if fixture is None:
        if args.require_scan:
            raise AssertionError(f"synthetic fixture did not appear in the catalog within {args.scan_timeout}s")
        report("Scanner visibility and lifecycle", None, "no fixture appeared before timeout; scan behavior remains unverified")
        return 2
    item_id = str(fixture.get("Id") or "")
    require(bool(item_id), "catalog item has no identifier")
    require(initial_scan.get("FilesSeen", 0) >= 2, "initial scanner pass did not see the video and photo fixtures")
    report("Synthetic media discovered by a completed, error-free catalog scan", True)
    verify_direct_play_acceptance(client, direct_play_fixture_path, args.scan_timeout)
    verify_scan_lifecycle(client, library_id, fixture_path.parent, args.scan_timeout)
    verify_scanner_root_identity(
        client,
        args.scan_timeout,
        fixture_root,
        values.get("PUFFINBOX_ACCEPTANCE_TRUST_ROOT", "/media/TrustRoot"),
    )

    photo_path = fixture_root / "Photos" / "Puffinbox Synthetic Photo.png"
    photo = find_item(client, photo_path)
    require(photo is not None, "scanner did not index the synthetic raster photo")
    photo_id = urllib.parse.quote(str(photo.get("Id") or ""))
    photo_status, photo_headers, photo_bytes = client.request("GET", f"/Items/{photo_id}/File")
    require(photo_status == 200 and photo_headers.get("Content-Type", "").startswith("image/png") and photo_bytes.startswith(b"\x89PNG\r\n\x1a\n"),
            "safe raster preview did not return the expected PNG bytes and MIME type")
    require(photo_headers.get("X-Content-Type-Options", "").lower() == "nosniff", "raster preview did not include nosniff")
    download_status, download_headers, _ = client.request("GET", f"/Items/{photo_id}/Download")
    require(download_status == 200 and download_headers.get("Content-Disposition", "").lower().startswith("attachment"),
            "explicit photo download was not forced to an attachment")
    report("Raster photo preview safety headers and attachment download", True)

    viewer = ensure_user(client, values["PUFFINBOX_ACCEPTANCE_VIEWER_USERNAME"], values["PUFFINBOX_ACCEPTANCE_VIEWER_PASSWORD"], library_id, playback=False)
    peer = ensure_user(client, values["PUFFINBOX_ACCEPTANCE_PEER_USERNAME"], values["PUFFINBOX_ACCEPTANCE_PEER_PASSWORD"], library_id, playback=True)
    denied = ensure_user(client, values["PUFFINBOX_ACCEPTANCE_DENIED_USERNAME"], values["PUFFINBOX_ACCEPTANCE_DENIED_PASSWORD"], None, playback=True)
    unrated = ensure_user(client, values["PUFFINBOX_ACCEPTANCE_UNRATED_USERNAME"], values["PUFFINBOX_ACCEPTANCE_UNRATED_PASSWORD"], library_id, playback=True, blocked_categories=["Movie"])
    require(viewer.get("Policy", {}).get("EnableAllFolders") is False, "viewer did not retain restricted-library policy")
    require(viewer.get("Policy", {}).get("EnabledFolders") == [library_id], "viewer did not retain the selected library policy")
    require(denied.get("Policy", {}).get("EnableAllFolders") is False and denied.get("Policy", {}).get("EnabledFolders") == [], "empty library policy was broadened")
    report("Explicit library access policies, including empty access", True)

    require(unrated.get("Policy", {}).get("BlockUnratedItems") == ["Movie"], "unrated content category policy was not retained")
    viewer_client = HttpClient(base_url)
    viewer_user, _ = login(viewer_client, values["PUFFINBOX_ACCEPTANCE_VIEWER_USERNAME"], values["PUFFINBOX_ACCEPTANCE_VIEWER_PASSWORD"])
    require(not viewer_user.get("IsAdministrator"), "viewer authenticated as an administrator")
    _, _, visible = viewer_client.json("GET", "/Items?Recursive=true&StartIndex=0&Limit=100&EnableTotalRecordCount=true")
    visible_items = visible.get("Items", [])
    require(any(str(entry.get("Id")) == item_id for entry in visible_items), "viewer could not browse the explicitly allowed library")
    denied_client = HttpClient(base_url)
    login(denied_client, values["PUFFINBOX_ACCEPTANCE_DENIED_USERNAME"], values["PUFFINBOX_ACCEPTANCE_DENIED_PASSWORD"])
    _, _, hidden = denied_client.json("GET", "/Items?Recursive=true&StartIndex=0&Limit=100&EnableTotalRecordCount=true")
    require(not any(str(entry.get("Id")) == item_id for entry in hidden.get("Items", [])), "empty-library user can see a restricted item")
    unrated_client = HttpClient(base_url)
    login(unrated_client, values["PUFFINBOX_ACCEPTANCE_UNRATED_USERNAME"], values["PUFFINBOX_ACCEPTANCE_UNRATED_PASSWORD"])
    _, _, unrated_items = unrated_client.json("GET", "/Items?Recursive=true&StartIndex=0&Limit=100&EnableTotalRecordCount=true")
    require(not any(str(entry.get("Id")) == item_id for entry in unrated_items.get("Items", [])), "unrated Movie policy did not hide the unrated fixture")
    admin_only_status, _, _ = viewer_client.request("GET", "/Users")
    require(admin_only_status in (401, 403), f"viewer could access administrator user list (HTTP {admin_only_status})")
    cross_user_status, _, _ = viewer_client.request("GET", f"/Users/{urllib.parse.quote(str(admin_user.get('Id')))}")
    require(cross_user_status in (401, 403), f"viewer could inspect another account (HTTP {cross_user_status})")
    viewer_mutation_status, _, _ = viewer_client.request("POST", "/Library/Refresh", {})
    require(viewer_mutation_status in (401, 403), f"viewer could trigger administrator library scan (HTTP {viewer_mutation_status})")
    peer_client = HttpClient(base_url)
    peer_user, peer_token = login(peer_client, values["PUFFINBOX_ACCEPTANCE_PEER_USERNAME"], values["PUFFINBOX_ACCEPTANCE_PEER_PASSWORD"])
    require(peer.get("Id") == peer_user.get("Id") and peer.get("Policy", {}).get("EnableMediaPlayback") is True,
            "independent playback test account did not retain its configured policy")
    conflict_status, _, _ = anonymous.request("GET", "/Users/Me", headers={"X-Emby-Token": admin_token, "Authorization": f"Bearer {peer_token}"})
    require(conflict_status in (401, 403), f"conflicting account tokens were not rejected (HTTP {conflict_status})")
    verify_playback_sessions(peer_client, client, item_id)
    report("Authenticated browsing and cross-account requests follow user policies", True)

    range_path = f"/Videos/{urllib.parse.quote(item_id)}/stream"
    range_start, range_length = 128, 4096
    status, headers, content = client.request("GET", range_path, headers={"Range": f"bytes={range_start}-{range_start + range_length - 1}"})
    expected = fixture_path.read_bytes()[range_start:range_start + range_length]
    require(status == 206, f"byte-range media request returned HTTP {status}, not 206")
    require(headers.get("Content-Range", "").startswith(f"bytes {range_start}-{range_start + range_length - 1}/"), "byte-range response has an invalid Content-Range")
    require(len(content) == range_length and hashlib.sha256(content).digest() == hashlib.sha256(expected).digest(), "byte-range bytes/hash differ from the synthetic source")
    api_key_path = range_path + "?ApiKey=" + urllib.parse.quote(admin_token, safe="")
    api_key_status, _, api_key_bytes = anonymous.request("GET", api_key_path, headers={"Range": f"bytes={range_start}-{range_start + range_length - 1}"})
    require(api_key_status == 206 and hashlib.sha256(api_key_bytes).digest() == hashlib.sha256(expected).digest(),
            f"GET-only ApiKey media authentication returned HTTP {api_key_status}")
    duplicate_key_status, _, _ = anonymous.request("GET", api_key_path + "&ApiKey=" + urllib.parse.quote(admin_token, safe=""))
    require(duplicate_key_status in (401, 403), f"duplicate ApiKey query credentials were accepted (HTTP {duplicate_key_status})")
    mutation_key_status, _, _ = anonymous.request("POST", f"/Sessions/Playing?ApiKey={urllib.parse.quote(admin_token, safe='')}", {"ItemId": item_id})
    require(mutation_key_status in (401, 403), f"ApiKey query authenticated a state-changing request (HTTP {mutation_key_status})")
    disabled_status, _, _ = viewer_client.request("GET", range_path, headers={"Range": "bytes=0-15"})
    require(disabled_status == 403, f"playback-disabled viewer received media bytes (HTTP {disabled_status})")
    report("Range response hashes, GET-only ApiKey authentication, and playback-policy denial", True)

    playback_payload = {
        "DeviceProfile": {
            "DirectPlayProfiles": [
                {"Container": "mp4,m4v,mkv", "Type": "Video", "AudioCodec": "aac,mp3", "VideoCodec": "h264"},
                {"Container": "mp3,mp4,m4a", "Type": "Audio", "AudioCodec": "aac,mp3"},
            ],
            "TranscodingProfiles": [
                {"Container": "ts", "Type": "Video", "AudioCodec": "aac", "VideoCodec": "h264", "Protocol": "hls"},
                {"Container": "ts", "Type": "Audio", "AudioCodec": "aac", "Protocol": "hls"},
            ],
            "SubtitleProfiles": [{"Format": "vtt", "Method": "external"}],
        },
    }
    _, _, playback = client.json("POST", f"/Items/{urllib.parse.quote(item_id)}/PlaybackInfo", playback_payload)
    source = (playback.get("MediaSources") or [{}])[0]
    require(source.get("SupportsDirectPlay") is False, "MPEG-4/MP3 Matroska fixture was incorrectly accepted as direct play")
    streams = source.get("MediaStreams") or []
    subtitle_index = next((stream.get("Index") for stream in streams if stream.get("Type") == "Subtitle"), None)
    audio_index = next((stream.get("Index") for stream in streams if stream.get("Type") == "Audio"), None)
    require(isinstance(subtitle_index, int), "embedded subtitle stream was not exposed by PlaybackInfo")
    require(isinstance(audio_index, int), "audio stream was not exposed by PlaybackInfo")
    playback_payload["SubtitleStreamIndex"] = subtitle_index
    playback_payload["AudioStreamIndex"] = audio_index
    _, _, playback = client.json("POST", f"/Items/{urllib.parse.quote(item_id)}/PlaybackInfo", playback_payload)
    source = (playback.get("MediaSources") or [{}])[0]
    transcode_url = source.get("TranscodingUrl")
    transcode_enabled = source.get("SupportsTranscoding") is True and isinstance(transcode_url, str)
    if not transcode_enabled:
        detail = "operator FFmpeg/HLS or compatible profile is unavailable"
        if args.require_transcode:
            raise AssertionError(f"incompatible-media transcode was not offered: {detail}")
        report("Incompatible-media FFmpeg HLS playback", None, detail)
        return 2

    master_url = urllib.parse.urljoin(base_url + "/", transcode_url.lstrip("/"))
    status, _, master = client.request("GET", urllib.parse.urlsplit(master_url).path + ("?" + urllib.parse.urlsplit(master_url).query if urllib.parse.urlsplit(master_url).query else ""))
    require(status == 200 and b"#EXTM3U" in master, "transcoding master playlist was not returned")
    session_match = re.search(rb"/(Videos|Audio)/[^/]+/hls/([0-9a-fA-F-]{36})/playlist\.m3u8", master)
    require(session_match is not None, "master playlist does not identify its generated HLS session")
    require(b"TYPE=SUBTITLES" in master and b"subtitle.m3u8" in master, "selected subtitle is missing from the master playlist")
    kind, session_id = session_match.group(1).decode("ascii"), session_match.group(2).decode("ascii")
    playlist_path = f"/{kind}/{urllib.parse.quote(item_id)}/hls/{session_id}/playlist.m3u8"
    playlist = b""
    for _ in range(30):
        status, _, playlist = client.request("GET", playlist_path)
        if status == 200 and b"segment" in playlist:
            break
        time.sleep(1)
    require(status == 200, "generated media playlist did not become available")
    segment_match = re.search(rb"(/(?:Videos|Audio)/[^/\r\n]+/hls/[0-9a-fA-F-]{36}/segment[0-9]{6}\.ts)", playlist)
    require(segment_match is not None, "generated media playlist contains no safe segment URL")
    segment_path = segment_match.group(1).decode("ascii")
    segment_status, _, segment = client.request("GET", segment_path)
    require(segment_status == 200 and len(segment) > 188, "first HLS segment is missing or too small")
    subtitle_playlist_path = f"/{kind}/{urllib.parse.quote(item_id)}/hls/{session_id}/subtitle.m3u8"
    subtitle_status, _, subtitle_playlist = client.request("GET", subtitle_playlist_path)
    require(subtitle_status == 200 and b"subtitle.vtt" in subtitle_playlist, "generated WebVTT playlist is missing")
    subtitle_status, _, subtitle_text = client.request("GET", f"/{kind}/{urllib.parse.quote(item_id)}/hls/{session_id}/subtitle.vtt")
    require(subtitle_status == 200 and subtitle_text.startswith(b"WEBVTT") and b"Synthetic subtitle acceptance cue" in subtitle_text,
            "embedded subtitle was not extracted as the expected bounded WebVTT content")
    ffprobe = values.get("PUFFINBOX_ACCEPTANCE_FFPROBE_PATH") or shutil.which("ffprobe")
    require(bool(ffprobe), "ffprobe is required to inspect the generated HLS output")
    with tempfile.TemporaryDirectory(prefix="puffinbox-acceptance-", dir=fixture_root) as scratch_dir:
        temporary_segment = Path(scratch_dir) / "first-segment.ts"
        temporary_segment.write_bytes(segment)
        try:
            probed = subprocess.run([ffprobe, "-v", "error", "-show_entries", "stream=codec_name", "-of", "json", str(temporary_segment)], check=True, capture_output=True, text=True, timeout=20)
        except subprocess.SubprocessError as error:
            diagnostic = getattr(error, "stderr", "") or ""
            raise AssertionError(f"ffprobe failed to inspect the generated segment: {diagnostic[-2000:]}") from error
        codecs = {entry.get("codec_name") for entry in json.loads(probed.stdout).get("streams", [])}
    require("h264" in codecs and "aac" in codecs, f"transcoded HLS codecs were {sorted(str(value) for value in codecs)}")
    cancel_status, _, _ = client.request("DELETE", f"/{kind}/{urllib.parse.quote(item_id)}/hls/{session_id}")
    require(cancel_status in (200, 204), f"HLS cancel returned HTTP {cancel_status}")
    report("Incompatible Matroska transcode, embedded subtitle extraction, H.264/AAC HLS probe, cancellation", True)

    shutdown_fixture = fixture_root / SHUTDOWN_FIXTURE_RELATIVE
    shutdown_item = wait_for_item(client, shutdown_fixture, args.scan_timeout)
    if shutdown_item is None:
        if args.require_transcode:
            raise AssertionError("long synthetic HLS fixture was not indexed before shutdown validation")
        report("Active HLS FFmpeg shutdown and committed playback resume", None, "long synthetic fixture was unavailable")
        return 2
    shutdown_item_id = str(shutdown_item.get("Id") or "")
    require(bool(shutdown_item_id), "long synthetic HLS fixture has no catalog identifier")
    active_playback = client.json("POST", f"/Items/{urllib.parse.quote(shutdown_item_id)}/PlaybackInfo", {
        "DeviceProfile": playback_payload["DeviceProfile"],
    })[2]
    active_source = (active_playback.get("MediaSources") or [{}])[0]
    active_transcode_url = active_source.get("TranscodingUrl")
    require(active_source.get("SupportsTranscoding") is True and isinstance(active_transcode_url, str),
            "the long synthetic fixture did not negotiate an HLS stream for shutdown validation")
    active_master_url = urllib.parse.urljoin(base_url + "/", active_transcode_url.lstrip("/"))
    active_status, _, active_master = client.request("GET", urllib.parse.urlsplit(active_master_url).path + ("?" + urllib.parse.urlsplit(active_master_url).query if urllib.parse.urlsplit(active_master_url).query else ""))
    require(active_status == 200 and b"#EXTM3U" in active_master, "long-fixture HLS master playlist did not start")
    active_match = re.search(rb"/(Videos|Audio)/[^/]+/hls/([0-9a-fA-F-]{36})/playlist\.m3u8", active_master)
    require(active_match is not None, "long-fixture master playlist has no HLS session URL")
    active_kind, active_hls_id = active_match.group(1).decode("ascii"), active_match.group(2).decode("ascii")
    active_play_session_id = str(active_playback.get("PlaySessionId") or "")
    require(active_play_session_id == active_hls_id, "HLS session ID differs from PlaybackInfo.PlaySessionId")
    active_playlist_path = f"/{active_kind}/{urllib.parse.quote(shutdown_item_id)}/hls/{active_hls_id}/playlist.m3u8"
    active_status, _, active_playlist = client.request("GET", active_playlist_path)
    require(active_status == 200 and b"segment" in active_playlist, "long-fixture HLS playlist did not publish a segment")
    active_client = HttpClient(base_url)
    login(active_client, values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"], values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"])
    active_position_ticks = 450_000_000
    active_client.json("POST", "/Sessions/Playing", {
        "ItemId": shutdown_item_id, "PlaySessionId": active_play_session_id,
        "PositionTicks": 0, "PlayMethod": "Transcode",
    }, expected=(204,))
    active_client.json("POST", "/Sessions/Playing/Progress", {
        "ItemId": shutdown_item_id, "PlaySessionId": active_play_session_id,
        "PositionTicks": active_position_ticks, "PlayMethod": "Transcode",
    }, expected=(204,))
    persisted = active_client.json("GET", f"/UserItems/{urllib.parse.quote(shutdown_item_id)}")[2]
    require(persisted.get("PlaybackPositionTicks") == active_position_ticks,
            "the active HLS fixture did not persist its pre-shutdown resume position")
    report(
        "Active HLS session startup and playback-position persistence",
        True,
        "the 300-second fixture negotiated HLS, served a playlist and segment, and persisted its playback position before restart validation",
    )
    if args.skip_container_restart:
        report(
            "Container/runtime restart validation",
            None,
            "skipped by --skip-container-restart; container restart, FFmpeg shutdown, and post-restart resume validation remain pending",
        )
    else:
        verify_container_restart(
            shutdown_item_id, base_url, values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"],
            values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"], active_play_session_id, active_position_ticks,
            args.env_file, project_name,
        )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env-file", type=Path, help="use an isolated acceptance settings file instead of the active local settings")
    parser.add_argument("--results-file", type=Path, help="write evidence to a new file; required with --env-file")
    parser.add_argument("--direct-play-only", action="store_true", help="run only the synthetic H.264/AAC PlaybackInfo and authenticated full/range checks")
    parser.add_argument("--host-media-paths", action="store_true", help="use fixture-tree host paths for a direct-source server instead of Compose /media paths")
    parser.add_argument("--scan-timeout", type=int, default=MAX_WAIT_SECONDS)
    parser.add_argument("--health-soak-seconds", type=int, default=35, help="keep checking database readiness for this long")
    parser.add_argument("--require-scan", action="store_true", help="fail if the synthetic file is not indexed before timeout")
    parser.add_argument("--require-transcode", action="store_true", help="fail if operator FFmpeg/HLS is unavailable")
    parser.add_argument(
        "--skip-container-restart",
        action="store_true",
        help="skip only container/runtime restart validation and record it as pending",
    )
    args = parser.parse_args()
    explicit_env_file = args.env_file is not None
    args.env_file = (args.env_file or ENV_FILE).expanduser().resolve()
    if explicit_env_file and args.env_file == ENV_FILE.resolve():
        parser.error("--env-file resolves to the active acceptance settings; omit it for the active stack")
    alternate_settings = explicit_env_file
    if not alternate_settings and args.results_file is not None:
        parser.error("--results-file is only allowed with an alternate --env-file")
    if alternate_settings and args.results_file is None:
        parser.error("--results-file is required with --env-file to keep evidence outside active acceptance state")
    if args.direct_play_only and (not alternate_settings or not args.skip_container_restart):
        parser.error("--direct-play-only requires an alternate --env-file and --skip-container-restart")
    args.results_file = (args.results_file or DEFAULT_RESULTS_FILE).expanduser().resolve()
    started_at = datetime.now(timezone.utc).isoformat()
    try:
        result = run(args)
    except (AssertionError, urllib.error.URLError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        report("Acceptance checks", False, str(error))
        result = 1
    evidence_path = args.results_file
    evidence_path.parent.mkdir(parents=True, exist_ok=True)
    project_name = COMPOSE_PROJECT
    try:
        recorded_values = read_env_file(args.env_file)
        if alternate_settings:
            project_name = recorded_values.get("COMPOSE_PROJECT_NAME", "puffinbox-acceptance-invalid")
    except SystemExit:
        if alternate_settings:
            project_name = "puffinbox-acceptance-invalid"
    metadata = evidence_metadata(project_name)
    evidence_path.write_text(json.dumps({
        "startedAtUtc": started_at,
        "finishedAtUtc": datetime.now(timezone.utc).isoformat(),
        "resultCode": result,
        **metadata,
        "checks": EVIDENCE,
    }, indent=2) + "\n", encoding="utf-8")
    print(f"Evidence written to ignored local file: {evidence_path}")
    return result


if __name__ == "__main__":
    raise SystemExit(main())
