#!/usr/bin/env python3
"""Run bounded semantic checks against an explicitly isolated local server."""

from __future__ import annotations

import argparse
from http import cookiejar
from datetime import datetime, timezone
import hashlib
import json
import math
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

from acceptance_socket import SocketClient
import embedded_audio_fixtures


ROOT = Path(__file__).resolve().parents[1]
ENV_FILE = ROOT / ".local" / "acceptance" / "acceptance.env"
DEFAULT_FIXTURE_ROOT = ROOT / ".local" / "acceptance" / "media"
DEFAULT_RESULTS_FILE = ROOT / ".local" / "acceptance" / "acceptance-results.json"
FIXTURE_RELATIVE = Path("Movies") / "Puffinbox Synthetic Transcode Fixture.mkv"
DIRECT_PLAY_FIXTURE_RELATIVE = Path("Movies") / "Puffinbox Synthetic Direct Play Fixture.mp4"
SDR_DIRECT_PLAY_FIXTURE_RELATIVE = Path("Movies") / "Puffinbox Explicit SDR Direct Play Fixture.mp4"
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


def wait_for_ready(client: HttpClient, timeout_seconds: float = MAX_WAIT_SECONDS) -> None:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        try:
            status, _, _ = client.request("GET", "/health/ready")
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            status = None
        if status == 200:
            return
        if status not in (None, 502, 503, 504):
            raise AssertionError(f"database readiness endpoint returned HTTP {status}")
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    raise AssertionError("isolated server did not become database-ready before the startup deadline")


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

    resumed = client.json("POST", f"/Items/{quoted_item_id}/PlaybackInfo", {
        "DeviceProfile": device_profile, "StartTimeTicks": 12_345_678, "EnableTranscoding": False,
    })[2]["MediaSources"][0]
    require(resumed.get("SupportsDirectPlay") is True and resumed.get("DirectStreamUrl") == source.get("DirectStreamUrl"),
            "resuming a matching MP4 source lost its original-file direct playback URL")
    require(resumed.get("RunTimeTicks") == source.get("RunTimeTicks"),
            "direct playback resume changed the source timeline duration")

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
    suffixed_path = f"/Videos/{quoted_item_id}/stream.mp4?Static=true&mediaSourceId={quoted_item_id}"
    suffix_status, suffix_headers, suffix_content = client.request("GET", suffixed_path, headers={"Range": f"bytes={range_start}-{range_end}"})
    require(suffix_status == 206 and suffix_content == expected_range and suffix_headers.get("Content-Range") == f"bytes {range_start}-{range_end}/{total_length}",
            "container-suffixed MP4 stream did not preserve the requested original range")
    suffix_status, suffix_headers, suffix_content = client.request("HEAD", suffixed_path)
    require(suffix_status == 200 and not suffix_content and suffix_headers.get("Content-Length") == str(total_length),
            "container-suffixed MP4 HEAD did not return original source metadata")
    report("H.264/AAC direct-play negotiation and authenticated original MP4 full/range streaming", True,
           f"{total_length} source bytes; HTTP 200 full and HTTP 206 bytes {range_start}-{range_end}")


def verify_video_range_acceptance(client: HttpClient, sdr_path: Path, unknown_path: Path, timeout_seconds: int) -> None:
    sdr = wait_for_item(client, sdr_path, timeout_seconds)
    unknown = wait_for_item(client, unknown_path, timeout_seconds)
    require(sdr is not None and unknown is not None, "video-range acceptance fixtures are missing from the catalog")
    profile = {
        "DirectPlayProfiles": [{"Type": "Video"}],
        "CodecProfiles": [{"Type": "Video", "Conditions": [{
            "Property": "VideoRangeType", "Condition": "NotEquals", "Value": "DOVI", "IsRequired": False,
        }]}],
    }
    _, _, playback = client.json("POST", f"/Items/{sdr['Id']}/PlaybackInfo", {"DeviceProfile": profile})
    source = playback["MediaSources"][0]
    require(source.get("SupportsDirectPlay") is True, "explicit SDR did not satisfy the desktop video-range condition")
    require(source.get("DirectStreamUrl") == f"/Videos/{sdr['Id']}/stream", "SDR direct play did not select original bytes")
    video = next(stream for stream in source["MediaStreams"] if stream["Type"] == "Video")
    require(video.get("VideoRangeType") == "SDR", "explicit source range was not returned to the client")
    for value, expected in [("SDR", False), ("MisspelledRange", False), ("HDR10", True)]:
        profile["CodecProfiles"][0]["Conditions"][0]["Value"] = value
        _, _, playback = client.json("POST", f"/Items/{sdr['Id']}/PlaybackInfo", {"DeviceProfile": profile})
        require(playback["MediaSources"][0].get("SupportsDirectPlay") is expected, f"video-range condition incorrectly evaluated {value}")
    profile["CodecProfiles"][0]["Conditions"][0]["Value"] = "DOVI"
    _, _, playback = client.json("POST", f"/Items/{unknown['Id']}/PlaybackInfo", {"DeviceProfile": profile})
    require(playback["MediaSources"][0].get("SupportsDirectPlay") is False, "missing range metadata was treated as proven SDR")
    report("Explicit SDR video-range conditions and conservative unknown/mismatched profiles", True)


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


def verify_user_data_edits(client: HttpClient, item_id: str) -> dict:
    path = f"/UserItems/{urllib.parse.quote(item_id)}/UserData"
    before = client.json("GET", path)[2]
    saved = client.json("POST", path, {
        "Played": False, "PlayCount": 7, "LastPlayedDate": "2024-06-07T10:09:10+02:00", "Likes": True,
    })[2]
    require(saved.get("PlayCount") == 7 and saved.get("LastPlayedDate") == "2024-06-07T08:09:10Z"
            and saved.get("Rating") == 10 and saved.get("Likes") is True,
            "explicit count, date or like did not persist")
    require(saved.get("PlaybackPositionTicks") == before.get("PlaybackPositionTicks"),
            "a partial edit replaced the interrupted video's position")
    edited = client.json("POST", path, {"PlayCount": 3, "Likes": False, "Rating": 8.5})[2]
    require(edited.get("PlayCount") == 3 and edited.get("Rating") == 8.5 and edited.get("Likes") is True
            and edited.get("LastPlayedDate") == saved.get("LastPlayedDate"),
            "partial count edit or explicit rating precedence differs from the reference")
    unchanged = client.json("POST", path, {
        "PlayCount": None, "LastPlayedDate": None, "Rating": None, "Likes": None,
        "PlayedPercentage": 85, "ItemId": "not-a-selector", "Key": "not-a-selector",
    })[2]
    require(unchanged == edited and client.json("GET", path)[2] == edited,
            "null fields or response-only fields changed stored user data")
    report("Partial user-data counts, dates, likes, ratings and independent read", True)
    return {key: edited[key] for key in ("PlayCount", "LastPlayedDate", "Rating", "Likes")}


def verify_container_restart(
    item_id: str,
    base_url: str,
    admin_username: str,
    admin_password: str,
    active_play_session_id: str,
    expected_position_ticks: int,
    env_file: Path,
    project_name: str,
    expected_user_data: dict,
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
    external_network = ROOT / "external-network.yml"
    if external_network.is_file():
        command += ["-f", str(external_network)]
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
    # Prepare a previously unrequested batch after the persistence-volume checks.
    # Its first segment becomes readable while the rest is still being encoded.
    # This keeps the shutdown assertion about an active child, even when earlier
    # demand-driven batches have already finished on a fast machine.
    encoding_client = HttpClient(base_url)
    _, encoding_token = login(encoding_client, admin_username, admin_password)
    notification_socket = SocketClient(base_url, encoding_token)
    encoding_status, _, encoding_segment = encoding_client.request(
        "GET", f"/Videos/{urllib.parse.quote(item_id)}/hls/{urllib.parse.quote(active_play_session_id)}/segment000032.ts"
    )
    require(encoding_status == 200 and len(encoding_segment) > 188,
            "a fresh HLS batch did not produce its first segment before shutdown")
    process_table = subprocess.run(["docker", "top", container], check=True, capture_output=True, text=True, timeout=20).stdout
    require(any("ffmpeg" in line.casefold() for line in process_table.splitlines()), "no external FFmpeg process was active immediately before server shutdown")
    report("Server shutdown requested while an HLS FFmpeg process was active", True)
    shutdown_started = datetime.now(timezone.utc).isoformat()
    subprocess.run(command + ["stop", "--timeout", "60", "server"], check=True, capture_output=True, text=True, timeout=75, env=command_environment)
    try:
        notification_socket.expect_shutdown()
    finally:
        notification_socket.close()
    state = subprocess.run(["docker", "inspect", "--format", "{{.State.ExitCode}} {{.State.OOMKilled}}", container], check=True, capture_output=True, text=True, timeout=20).stdout.strip()
    require(state == "0 false", f"server container did not exit cleanly on graceful stop ({state})")
    log_result = subprocess.run(["docker", "logs", "--since", shutdown_started, container], check=True, capture_output=True, text=True, timeout=20)
    logs = log_result.stdout + log_result.stderr
    require("media shutdown exceeded its drain deadline" not in logs.casefold(), "server reported that the FFmpeg shutdown hook missed its drain deadline")
    require("media child required forced termination" not in logs.casefold(), "FFmpeg did not exit within the graceful termination period")
    require("all media children drained during shutdown" in logs, "server did not confirm that it reaped every media child before exit")
    require("socket shutdown exceeded" not in logs, "server could not drain its upgraded sockets before exit")
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
    restarted_user, restarted_token = login(client, admin_username, admin_password)
    _, _, item = client.json("GET", f"/Items/{urllib.parse.quote(item_id)}")
    require(str(item.get("Id")) == item_id, "catalog item did not persist across server restart")
    resume = client.json("GET", f"/UserItems/{urllib.parse.quote(item_id)}")[2]
    require(resume.get("PlaybackPositionTicks") == expected_position_ticks,
            f"shutdown did not retain the last committed playback position for play session {active_play_session_id}")
    require(resume.get("Played") is False, "shutdown incorrectly marked the interrupted item as played to completion")
    require({key: resume.get(key) for key in expected_user_data} == expected_user_data,
            "explicit count, date or personal rating changed across the container restart")
    report("Explicit user-data counts, dates and personal rating across container restart", True)
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
    with SocketClient(base_url, restarted_token) as restarted_socket:
        for method in ("POST", "DELETE"):
            committed = client.json(method, f"/UserFavoriteItems/{urllib.parse.quote(item_id)}")[2]
            event = restarted_socket.event("UserDataChanged")
            require(event.get("Data") == {"UserId": restarted_user["Id"], "UserDataList": [committed]},
                    "reconnected socket did not report committed user data after restart")
    report("Authenticated socket close and reconnected notifications across a container restart", True)
    report("Active HLS job shutdown, playback-row closure, committed resume position, and catalog persistence", True)


def verify_socket_notifications(base_url: str, admin_token: str, peer_client: HttpClient, peer_user: dict, peer_token: str, item_id: str) -> None:
    with SocketClient(base_url, admin_token) as admin_socket, SocketClient(base_url, peer_token) as peer_socket:
        for method in ("POST", "DELETE"):
            committed = peer_client.json(method, f"/UserFavoriteItems/{urllib.parse.quote(item_id)}")[2]
            event = peer_socket.event("UserDataChanged")
            require(event.get("Data") == {"UserId": peer_user["Id"], "UserDataList": [committed]},
                    "socket did not report its own committed user data")
            admin_socket.expect_quiet()
    report("Authenticated WebSocket keepalive, committed user-data notifications, and cross-account isolation", True)


def login(client: HttpClient, username: str, password: str, headers: dict[str, str] | None = None, identity: dict[str, str] | None = None) -> tuple[dict, str]:
    payload = {
        "Username": username, "Pw": password, "Client": "Puffinbox acceptance",
        "DeviceName": "Isolated local acceptance", "DeviceId": f"puffinbox-acceptance-{uuid.uuid4()}", "Version": "0.1.0",
    }
    payload.update(identity or {})
    _, _, result = client.json("POST", "/Users/AuthenticateByName", payload, headers=headers)
    require(isinstance(result, dict) and isinstance(result.get("User"), dict) and isinstance(result.get("AccessToken"), str), "authentication returned an invalid user/token payload")
    return result["User"], result["AccessToken"]


def verify_catalog_filters(client: HttpClient, library_id: str) -> None:
    scope = "?parentId=" + urllib.parse.quote(library_id) + "&includeItemTypes=Movie"
    _, legacy_headers, legacy = client.json("GET", "/Items/Filters" + scope)
    _, _, modern = client.json("GET", "/Items/Filters2" + scope)
    require(set(legacy) == {"Genres", "Tags", "OfficialRatings", "Years"}, "legacy filter DTO fields differ from the public schema")
    require(set(modern) == {"Genres", "Tags", "AudioLanguages", "SubtitleLanguages"}, "filter DTO fields differ from the public schema")
    require(all(isinstance(value, list) for value in [*legacy.values(), *modern.values()]), "filter choices are not arrays")
    require(modern["Tags"] == legacy["Tags"], "filter routes disagree on visible tags")
    require([entry.get("Name") for entry in modern["Genres"]] == legacy["Genres"], "filter routes disagree on visible genres")
    require(legacy_headers.get("Cache-Control") == "no-store", "private catalog filter choices can be cached")
    for entry in modern["Genres"]:
        genre_id = str(uuid.UUID(entry["Id"]))
        _, _, selected = client.json("GET", f"/Items?ParentId={library_id}&Recursive=true&GenreIds={genre_id}&IncludeItemTypes=Movie")
        require(selected["TotalRecordCount"] > 0, "an advertised genre ID cannot select its items")
    unknown = uuid.uuid4()
    _, _, empty = client.json("GET", f"/Items?ParentId={library_id}&Recursive=true&GenreIds={unknown}&Limit=1")
    require(empty["TotalRecordCount"] == 0 and empty["Items"] == [], "an unknown genre ID was silently ignored")
    invalid, _, _ = client.request("GET", "/Items?Years=not-a-year")
    require(invalid == 400, "an invalid production year was silently ignored")
    report("Authenticated catalog filter DTOs, private cache headers, genre selection, and invalid-filter rejection", True,
           "Known genre round trips run when the fixture catalog has genres; real NFO values and provider/policy boundaries have separate PostgreSQL coverage")


def verify_hls_timeline(client: HttpClient, item_id: str, profile: dict, fixture_root: Path, ffprobe: str) -> None:
    playback = client.json("POST", f"/Items/{urllib.parse.quote(item_id)}/PlaybackInfo", {
        "DeviceProfile": profile, "StartTimeTicks": 1_231_855_880,
        "EnableDirectPlay": False, "EnableDirectStream": False,
    })[2]
    source = (playback.get("MediaSources") or [{}])[0]
    url = source.get("TranscodingUrl")
    require(source.get("SupportsTranscoding") is True and isinstance(url, str),
            "resume did not negotiate a full-timeline HLS stream")
    query = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
    require(query.get("fullTimeline") == ["true"] and query.get("StartTimeTicks") == ["1231855880"],
            "full-timeline resume URL omitted its requested random-access point")
    status, _, master = client.request("GET", url)
    require(status == 200 and b"#EXTM3U" in master, "resume master playlist was unavailable")
    session_id = str(uuid.UUID(str(playback.get("PlaySessionId") or "")))
    session_path = f"/Videos/{urllib.parse.quote(item_id)}/hls/{session_id}"
    status, _, playlist = client.request("GET", session_path + "/playlist.m3u8")
    require(status == 200 and b"#EXT-X-PLAYLIST-TYPE:VOD" in playlist and b"#EXT-X-ENDLIST" in playlist,
            "resume playlist does not describe the complete on-demand movie")
    durations = [float(value) for value in re.findall(rb"#EXTINF:([0-9.]+),", playlist)]
    runtime = float(source.get("RunTimeTicks", 0)) / 10_000_000
    require(299.9 <= runtime <= 300.1, "the long fixture has no trustworthy 300-second runtime")
    count = math.ceil(runtime / 4)
    require(len(durations) == count and abs(sum(durations) - runtime) < 0.01,
            "the source runtime is not represented by its complete HLS playlist")
    with tempfile.TemporaryDirectory(prefix="puffinbox-hls-timeline-", dir=fixture_root) as scratch:
        for index in (30, 2, count - 1, 16, 48, 64):
            status, headers, segment = client.request("GET", session_path + f"/segment{index:06}.ts")
            require(status == 200 and len(segment) > 188 and headers.get("Content-Type") == "video/mp2t",
                    f"HLS seek segment {index} was unavailable")
            path = Path(scratch) / f"segment{index:06}.ts"
            path.write_bytes(segment)
            result = subprocess.run([ffprobe, "-v", "error", "-show_entries", "stream=codec_name,id,start_time,duration",
                                     "-of", "json", str(path)], check=True, capture_output=True, text=True, timeout=20)
            streams = json.loads(result.stdout).get("streams", [])
            video = next((stream for stream in streams if stream.get("codec_name") == "h264"), None)
            require(video is not None and abs(float(video.get("start_time", -1)) - index * 4) < 0.05,
                    f"HLS segment {index} does not carry its source-time video timestamp")
            require(abs(float(video.get("duration", -1)) - durations[index]) < 0.05,
                    f"HLS segment {index} does not have the advertised duration")
            if index == 30:
                result = subprocess.run([ffprobe, "-v", "error", "-select_streams", "v:0", "-show_frames",
                                         "-show_entries", "frame=key_frame,pts_time", "-of", "json", str(path)],
                                        check=True, capture_output=True, text=True, timeout=20)
                keys = [float(frame["pts_time"]) for frame in json.loads(result.stdout).get("frames", [])
                        if frame.get("key_frame") == 1 and "pts_time" in frame]
                require(any(0 <= point - 123.185588 < 1 / 30 + 0.001 for point in keys),
                        "HLS resume segment has no random-access frame at or immediately after the requested position")
            if index in (16, 48, 64):
                first_packets = {}
                require(len(segment) % 188 == 0, "HLS transport data is not packet aligned")
                for offset in range(0, len(segment), 188):
                    packet = segment[offset:offset + 188]
                    require(packet[0] == 0x47, "HLS transport packet lost its sync byte")
                    pid = ((packet[1] & 0x1F) << 8) | packet[2]
                    first_packets.setdefault(pid, packet)
                for stream in streams:
                    if stream.get("codec_name") not in {"h264", "aac"}:
                        continue
                    pid = int(stream["id"], 0)
                    packet = first_packets.get(pid)
                    require(packet is not None, "HLS media stream has no transport packet")
                    adaptation = (packet[3] >> 4) & 3
                    require(adaptation in (2, 3) and packet[4] > 0 and bool(packet[5] & 0x80),
                            f"HLS batch beginning at segment {index} does not signal the packet-counter reset for {stream['codec_name']}")
    status, _, _ = client.request("GET", session_path + f"/segment{count:06}.ts")
    require(status == 404, "HLS accepted a segment beyond the movie duration")
    status, _, _ = client.request("DELETE", session_path)
    require(status == 204, "full-timeline HLS session cancellation failed")
    report("Full-duration HLS resume, forward/backward segment requests, source timestamps, and end boundary", True,
           f"{count} segments cover the {runtime:.3f}-second source; the resume keyframe rounded forward within one frame, nonsequential timestamps matched, and three batch starts signaled transport-counter resets")


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


def verify_universal_audio(client: HttpClient, fixture_root: Path, music_root: str, scan_timeout: int) -> tuple[str, str]:
    fixture = fixture_root / "Music" / "Puffinbox Original Acceptance Track.flac"
    require(fixture.is_file(), "generated universal audio fixture is missing")
    name = "Puffinbox Acceptance Music"
    libraries = client.json("GET", "/Library/VirtualFolders")[2]
    library = next((entry for entry in libraries if entry.get("Name") == name), None)
    if library is None:
        client.json("POST", "/Library/VirtualFolders", {
            "Name": name, "Locations": [music_root], "CollectionType": "music",
            "LibraryOptions": {"Enabled": True},
        }, expected=(204,))
        libraries = client.json("GET", "/Library/VirtualFolders")[2]
        library = next((entry for entry in libraries if entry.get("Name") == name), None)
    require(library is not None, "audio fixture library was not created")
    library_id = str(library.get("ItemId") or library.get("Id") or "")
    scan_library(client, library_id, scan_timeout)
    query = urllib.parse.urlencode({"ParentId": library_id, "IncludeItemTypes": "Audio", "Recursive": "true"})
    tracks = client.json("GET", "/Items?" + query)[2].get("Items", [])
    track = next((entry for entry in tracks if entry.get("Name") in (fixture.name, fixture.stem)), None)
    require(track is not None, "FLAC audio fixture was not indexed")
    item_id = str(track["Id"])
    resumed = client.json("POST", f"/Items/{urllib.parse.quote(item_id)}/PlaybackInfo", {
        "DeviceProfile": {"DirectPlayProfiles": [{"Type": "Audio", "Container": "flac", "AudioCodec": "flac"}]},
        "StartTimeTicks": 50_000_000, "EnableTranscoding": False,
    })[2]["MediaSources"][0]
    require(resumed.get("SupportsDirectPlay") is True and resumed.get("DirectStreamUrl") == f"/Audio/{item_id}/stream",
            "resuming a matching FLAC source lost its original-file direct playback URL")
    audio_stream = next((entry for entry in resumed.get("MediaStreams", []) if entry.get("Type") == "Audio"), {})
    require(audio_stream.get("SampleRate") == 44_100 and audio_stream.get("BitDepth") == 16,
            "the synthetic FLAC's sample rate or valid sample depth was not reported")
    path = f"/Audio/{urllib.parse.quote(item_id)}/universal"
    options = "?Container=flac&MaxStreamingBitrate=1911466591&StartTimeTicks=0&TranscodingContainer=mp4&TranscodingProtocol=hls&AudioCodec=aac"
    source = fixture.read_bytes()
    status, headers, body = client.request("GET", path + options)
    require(status == 200 and body == source and headers.get("Content-Type") == "audio/flac",
            "universal audio did not deliver the original FLAC bytes")
    require(headers.get("Cache-Control") == "private, no-store" and headers.get("Referrer-Policy") == "no-referrer",
            "universal audio omitted private media headers")
    status, headers, body = client.request("HEAD", path + options)
    require(status == 200 and not body and headers.get("Content-Length") == str(len(source)),
            "universal audio HEAD did not return source metadata without a body")
    status, headers, body = client.request("GET", path + options, headers={"Range": "bytes=8-31"})
    require(status == 206 and body == source[8:32] and headers.get("Content-Range") == f"bytes 8-31/{len(source)}",
            "universal audio byte range differs from the source")
    for start_ticks in (50_000_000, 51_234_567):
        resumed_options = "?Container=flac&MaxAudioSampleRate=44100&MaxAudioBitDepth=16&StartTimeTicks=" + str(start_ticks)
        status, headers, body = client.request("GET", path + resumed_options)
        require(status == 200 and body == source and headers.get("Content-Type") == "audio/flac"
                and headers.get("Cache-Control") == "private, no-store",
                "a saved position changed or rejected the matching original audio timeline")
        status, headers, body = client.request("HEAD", path + resumed_options)
        require(status == 200 and not body and headers.get("Content-Length") == str(len(source)),
                "resumed original audio HEAD omitted source metadata")
        for method in ("GET", "HEAD"):
            status, headers, body = client.request(method, path + resumed_options, headers={"Range": "bytes=8-31"})
            require(status == 206 and headers.get("Content-Range") == f"bytes 8-31/{len(source)}"
                    and headers.get("Content-Length") == "24" and body == (source[8:32] if method == "GET" else b""),
                    "resumed original audio changed byte-range delivery")
        status, _, _ = HttpClient(client.base_url).request("GET", path + resumed_options)
        require(status == 401, "anonymous resumed original audio was accepted")
    limited = "?Container=flac&MaxAudioSampleRate=44100&MaxAudioBitDepth=16"
    status, headers, body = client.request("GET", path + limited)
    require(status == 200 and body == source and headers.get("Cache-Control") == "private, no-store",
            "matching audio rate/depth limits changed or rejected the original source")
    status, headers, body = client.request("HEAD", path + limited)
    require(status == 200 and not body and headers.get("Content-Length") == str(len(source)),
            "constrained universal audio HEAD omitted the original source metadata")
    status, headers, body = client.request("GET", path + "?Container=flac&MaxAudioSampleRate=48000&MaxAudioBitDepth=24",
                                         headers={"Range": "bytes=8-31"})
    require(status == 206 and body == source[8:32] and headers.get("Content-Range") == f"bytes 8-31/{len(source)}",
            "looser audio limits changed original byte-range delivery")
    for limits in ["MaxAudioSampleRate=44099", "MaxAudioBitDepth=15", "MaxAudioSampleRate=0",
                   "MaxAudioBitDepth=0", "MaxAudioSampleRate=2147483648", "MaxAudioBitDepth=16.5"]:
        status, _, _ = client.request("GET", path + "?Container=flac&" + limits)
        require(status == 400, "universal audio ignored an incompatible or invalid rate/depth limit")
    status, _, _ = client.request("GET", path + "?Container=mp3")
    require(status == 400, "universal audio ignored an incompatible container")
    status, _, _ = client.request("GET", path + "?Container=flac&MaxStreamingBitrate=1")
    require(status == 400, "universal audio ignored the source bitrate limit")
    status, _, _ = HttpClient(client.base_url).request("GET", path + options)
    require(status == 401, "anonymous universal audio was accepted")
    suffixed_path = f"/Audio/{urllib.parse.quote(item_id)}/stream.flac?Static=true&mediaSourceId={urllib.parse.quote(item_id)}"
    status, headers, body = client.request("GET", suffixed_path)
    require(status == 200 and body == source and headers.get("Content-Type") == "audio/flac",
            "container-suffixed audio stream did not return original FLAC bytes")
    status, headers, body = client.request("HEAD", suffixed_path)
    require(status == 200 and not body and headers.get("Content-Length") == str(len(source)),
            "container-suffixed audio HEAD did not return original source metadata")
    status, headers, body = client.request("GET", suffixed_path, headers={"Range": "bytes=8-31"})
    require(status == 206 and body == source[8:32] and headers.get("Content-Range") == f"bytes 8-31/{len(source)}",
            "container-suffixed audio range differs from the source")
    status, _, _ = client.request("GET", f"/Audio/{urllib.parse.quote(item_id)}/stream.mp3")
    require(status == 404, "container-suffixed audio mislabeled original FLAC bytes as MP3")
    status, _, _ = HttpClient(client.base_url).request("GET", suffixed_path)
    require(status == 401, "anonymous container-suffixed audio was accepted")
    for session_id in (str(time.time_ns() // 1_000_000), ""):
        previous = client.json("GET", f"/UserItems/{urllib.parse.quote(item_id)}")[2]
        expected_count = int(previous.get("PlayCount", 0)) + 1
        client.json("POST", "/Sessions/Playing", {
            "ItemId": item_id, "PlaySessionId": session_id, "PositionTicks": 0, "PlayMethod": "DirectPlay",
        }, expected=(204,))
        started = client.json("GET", f"/UserItems/{urllib.parse.quote(item_id)}")[2]
        require(started.get("Played") is True and started.get("PlaybackPositionTicks") == 0
                and started.get("PlayCount") == expected_count,
                "numeric or empty music session did not count its start and clear saved resume")
        for event in ("Progress", "Stopped"):
            client.json("POST", f"/Sessions/Playing/{event}", {
                "ItemId": item_id, "PlaySessionId": session_id, "PositionTicks": 50_000_000,
            }, expected=(204,))
        data = client.json("GET", f"/UserItems/{urllib.parse.quote(item_id)}")[2]
        require(data.get("Played") is True and data.get("PlaybackPositionTicks") == 0
                and data.get("PlayCount") == expected_count
                and data.get("LastPlayedDate") == started.get("LastPlayedDate"),
                "music progress or stop changed the counted play or saved a resume position")
    report("Universal and suffixed FLAC audio, byte ranges, format/bitrate/rate/depth limits, and numeric/empty playback events", True,
           "zero, five-second, and fractional requested positions retain original bytes, ranges, and HEAD metadata; each music start counts once and progress/stop keep saved resume at zero")
    return library_id, item_id


def verify_universal_audio_transcode(admin: HttpClient, library_id: str, item_id: str, fixture_root: Path, ffprobe: str) -> None:
    username, password = "audio-conversion-" + uuid.uuid4().hex, "Synthetic-" + uuid.uuid4().hex
    user_id = str(ensure_user(admin, username, password, library_id, playback=True)["Id"])
    playback = HttpClient(admin.base_url)
    login(playback, username, password)
    scoped_token = str(playback.json("POST", "/Users/Me/MediaAccessToken", {})[2]["AccessToken"])
    media = HttpClient(admin.base_url)
    path = f"/Audio/{item_id}/universal"
    direct_session = str(uuid.uuid4())
    direct_query = path + "?" + urllib.parse.urlencode({
        "Container": "flac", "StartTimeTicks": 51_234_567, "PlaySessionId": direct_session,
        "MaxAudioSampleRate": 44100, "MaxAudioBitDepth": 16, "ApiKey": scoped_token,
    })
    source = (fixture_root / "Music" / "Puffinbox Original Acceptance Track.flac").read_bytes()
    for method in ("GET", "HEAD"):
        status, headers, body = media.request(method, direct_query, headers={"Range": "bytes=8-31"})
        require(status == 206 and headers.get("Content-Type") == "audio/flac"
                and headers.get("Content-Range") == f"bytes 8-31/{len(source)}"
                and body == (source[8:32] if method == "GET" else b""),
                "a scoped resumed original-audio request changed source ranges")
    require(playback.request("DELETE", f"/Audio/{item_id}/hls/{direct_session}")[0] == 404,
            "resuming compatible original audio created an encoding session")
    for container, raw_session in [("mp4", str(uuid.uuid4())), ("ts", str(time.time_ns() // 1_000_000))]:
        options = {
            "Container": "mp3", "TranscodingContainer": container, "TranscodingProtocol": "hls", "AudioCodec": "aac",
            "AudioBitRate": 64000, "MaxStreamingBitrate": 96000, "MaxAudioChannels": 1,
            "MaxAudioSampleRate": 22050, "StartTimeTicks": 50_000_000, "PlaySessionId": raw_session,
        }
        uncredentialed = path + "?" + urllib.parse.urlencode(options)
        require(HttpClient(admin.base_url).request("GET", uncredentialed)[0] == 401, "anonymous audio conversion was accepted")
        query = uncredentialed + "&" + urllib.parse.urlencode({"ApiKey": scoped_token})
        status, headers, body = media.request("HEAD", query)
        require(status == 200 and not body and headers.get("Content-Type") == "application/vnd.apple.mpegurl",
                "converted audio HEAD omitted HLS metadata or returned a body")
        if container == "mp4":
            require(playback.request("DELETE", f"/Audio/{item_id}/hls/{raw_session}")[0] == 404,
                    "converted audio HEAD created an encoding session")
        status, headers, master = media.request("GET", query)
        require(status == 200 and headers.get("Cache-Control") == "private, no-store", "audio conversion did not return a private master playlist")
        start_hint = b"#EXT-X-START:TIME-OFFSET=5.0000000,PRECISE=YES"
        require(start_hint in master, "universal audio master omitted its original-timeline start hint")
        playlist_path = next(line for line in master.decode().splitlines() if line.startswith("/Audio/"))
        session_id = playlist_path.split("/")[4]
        playlist = b""
        for _ in range(40):
            status, _, playlist = media.request("GET", playlist_path)
            if status == 200 and b"#EXT-X-ENDLIST" in playlist:
                break
            time.sleep(.25)
        require(status == 200 and b"#EXT-X-ENDLIST" in playlist, "converted audio did not finish its bounded fixture")
        require(start_hint in playlist, "universal audio media playlist omitted its original-timeline start hint")
        require(b"ApiKey=" in playlist and scoped_token.encode() in playlist, "audio conversion dropped its child credential")
        fragment_paths = [line for line in playlist.decode().splitlines() if line.startswith("/Audio/")]
        require(bool(fragment_paths), "converted audio has no fragments")
        encoded = bytearray()
        if container == "mp4":
            match = re.search(rb'#EXT-X-MAP:URI="([^"]+)"', playlist)
            require(match is not None, "fragmented audio has no initialization resource")
            init_path = match.group(1).decode()
            status, headers, init = media.request("GET", init_path)
            require(status == 200 and headers.get("Content-Type") == "audio/mp4", "audio initialization resource is missing or mislabeled")
            encoded.extend(init)
            require(media.request("GET", init_path.replace("init.mp4", "init-other.mp4"))[0] == 404, "unknown initialization resource was served")
        for fragment_path in fragment_paths:
            status, headers, fragment = media.request("GET", fragment_path)
            require(status == 200 and headers.get("Content-Type") == ("audio/mp4" if container == "mp4" else "video/mp2t"), "converted audio fragment is missing or mislabeled")
            encoded.extend(fragment)
        with tempfile.TemporaryDirectory(prefix="puffinbox-audio-conversion-", dir=fixture_root) as directory:
            converted = Path(directory) / ("converted.m4a" if container == "mp4" else "converted.ts")
            converted.write_bytes(encoded)
            inspected = subprocess.run([ffprobe, "-v", "error", "-show_entries", "stream=codec_name,channels,sample_rate:format=duration", "-of", "json", str(converted)], capture_output=True, text=True, check=True, timeout=20)
            source = subprocess.run([ffprobe, "-v", "error", "-show_entries", "format=duration", "-of", "json", str(fixture_root / "Music/Puffinbox Original Acceptance Track.flac")], capture_output=True, text=True, check=True, timeout=20)
            info = json.loads(inspected.stdout)
            require(len(info["streams"]) == 1 and info["streams"][0]["codec_name"] == "aac"
                    and info["streams"][0]["channels"] == 1 and info["streams"][0]["sample_rate"] == "22050", "audio conversion ignored its codec, channel, or sample-rate request")
            expected_duration = float(json.loads(source.stdout)["format"]["duration"])
            require(abs(float(info["format"]["duration"]) - expected_duration) < .2, "universal audio clipped the source instead of preserving the client's seek timeline")
        playback.json("POST", "/Sessions/Playing", {"ItemId": item_id, "PlaySessionId": raw_session, "PositionTicks": 50_000_000, "PlayMethod": "Transcode"}, expected=(204,))
        admin.json("POST", f"/Users/{user_id}/Policy", {"EnableMediaPlayback": False}, expected=(204,))
        require(media.request("GET", fragment_paths[0])[0] == 403, "converted audio retained access after playback policy changed")
        admin.json("POST", f"/Users/{user_id}/Policy", {"EnableMediaPlayback": True}, expected=(204,))
        playback.json("POST", "/Sessions/Playing/Stopped", {"ItemId": item_id, "PlaySessionId": raw_session, "PositionTicks": 60_000_000}, expected=(204,))
        require(media.request("GET", fragment_paths[0])[0] == 404, "stopped converted audio session remained accessible")
        require(str(uuid.UUID(session_id)) == session_id, "converted audio exposed an invalid internal session id")
    playback.json("POST", "/Sessions/Logout", expected=(204,))
    require(media.request("GET", fragment_paths[0])[0] == 401, "converted audio retained a revoked parent credential")
    report("Universal AAC audio conversion in fragmented MP4 and TS, original timeline and start hints, constrained output, scoped child access, and stop", True,
           "both outputs independently decoded with the full source duration; HEAD created no session; UUID and opaque playback IDs stopped their encoding sessions")


def verify_item_relations(admin: HttpClient, library_id: str, item_id: str) -> None:
    username, password = "relation-reader-" + uuid.uuid4().hex, "Synthetic-" + uuid.uuid4().hex
    user_id = str(ensure_user(admin, username, password, library_id, playback=True)["Id"])
    reader = HttpClient(admin.base_url)
    login(reader, username, password)
    anonymous = HttpClient(admin.base_url)
    try:
        item = reader.json("GET", "/Items/" + item_id)[2]
        for field in ["ArtistItems", "Artists", "AlbumArtists", "Genres", "Tags"]:
            require(isinstance(item.get(field), list), "unclassified audio omitted a client-facing metadata list")
        for route in ["Similar", "Collections"]:
            path = f"/Items/{item_id}/{route}"
            require(anonymous.request("GET", path)[0] == 401, "item relation data was returned anonymously")
            _, headers, result = reader.json("GET", path + "?limit=1&fields=Path,Genres")
            require(headers.get("Cache-Control") == "private, no-store", "item relation response was not private")
            require(isinstance(result.get("Items"), list) and isinstance(result.get("TotalRecordCount"), int)
                    and result.get("StartIndex") == 0 and result["TotalRecordCount"] >= len(result["Items"])
                    and len(result["Items"]) <= 1, "item relation paging differs from its public result shape")
            require(all(entry.get("Id") != item_id and "Path" not in entry for entry in result["Items"]),
                    "item relation response exposed a path or its source item")
            status, headers, body = reader.request("HEAD", path)
            require(status == 200 and not body and headers.get("Cache-Control") == "private, no-store",
                    "item relation HEAD did not preserve private metadata without a body")
            for query in ["limit=-1", "limit=101"]:
                require(reader.request("GET", path + "?" + query)[0] == 400, "item relation limits were not bounded")
            require(reader.request("GET", path + "?userId=" + str(uuid.uuid4()))[0] == 403,
                    "item relation query could select another user")
            require(reader.request("GET", path.replace(item_id, str(uuid.uuid4())))[0] == 404,
                    "missing item relation query returned fabricated data")
        admin.json("POST", f"/Users/{user_id}/Policy", {"EnableAllFolders": False, "EnabledFolders": []}, expected=(204,))
        for route in ["Similar", "Collections"]:
            require(reader.request("GET", f"/Items/{item_id}/{route}")[0] == 404,
                    "item relation query retained revoked library access")
        report("Item Similar and Collections response shapes, bounded paging, private HEAD, metadata lists, and access revocation", True,
               "real genre/artist ranking and containing-collection fixtures have separate PostgreSQL coverage")
    finally:
        reader.json("POST", "/Sessions/Logout", expected=(204,))


def verify_catalog_ordering(admin: HttpClient, library_id: str, item_id: str) -> None:
    username, password = "ordering-reader-" + uuid.uuid4().hex, "Synthetic-" + uuid.uuid4().hex
    user_id = str(ensure_user(admin, username, password, library_id, playback=True)["Id"])
    reader = HttpClient(admin.base_url)
    login(reader, username, password)
    try:
        query = (f"?Ids={item_id}&SortBy=PremiereDate,ProductionYear,SortName"
                 "&SortOrder=Descending,Descending,Ascending")
        _, headers, result = reader.json("GET", "/Items" + query)
        require(headers.get("Cache-Control") == "private, no-store"
                and result["TotalRecordCount"] == 1 and result["Items"][0]["Id"] == item_id,
                "multi-field catalogue sorting omitted a visible requested track")
        for prefix, exclusion in [("/Items", "ExcludeItemIds"), (f"/Users/{user_id}/Items", "excludeItemIds")]:
            excluded = reader.json("GET", prefix + query + f"&{exclusion}={item_id},{item_id}")[2]
            require(excluded["Items"] == [] and excluded["TotalRecordCount"] == 0,
                    "item exclusion was not applied before catalogue counts and paging")
        for suffix in ["SortBy=SortName,Unsupported", "SortOrder=Ascending,Invalid", "ExcludeItemIds=invalid"]:
            require(reader.request("GET", "/Items?Ids=" + item_id + "&" + suffix)[0] == 400,
                    "malformed catalogue sorting or exclusion was silently accepted")
        for name in ["ArtistIds", "AlbumArtistIds"]:
            missing_artist = reader.json("GET", "/Items" + query + "&" + name + "=" + str(uuid.uuid4()))[2]
            require(missing_artist["Items"] == [] and missing_artist["TotalRecordCount"] == 0,
                    "catalogue ignored an artist identifier with no visible match")
        _, _, numbered = reader.json("GET", "/Items?Ids=" + item_id + "&SortBy=Album,ParentIndexNumber,IndexNumber,SortName")
        require(numbered["TotalRecordCount"] == 1 and numbered["Items"][0]["Id"] == item_id,
                "disc/track sorting omitted an unnumbered visible fixture")
        for prefix in ["/Items", f"/Users/{user_id}/Items"]:
            base = prefix + "?Ids=" + item_id + "&SortBy=SortName"
            for field in ["Recursive", "EnableTotalRecordCount", "IsPlayed", "IsFolder"]:
                for value in ["true", "false"]:
                    expected = reader.json("GET", base + "&" + field + "=" + value)[2]
                    for variant in [value.title(), value.upper(), " \t" + value.title() + "\t "]:
                        suffix = urllib.parse.urlencode({field: variant})
                        _, headers, observed = reader.json("GET", base + "&" + suffix)
                        require(observed == expected and headers.get("Cache-Control") == "private, no-store",
                                "catalogue boolean case or whitespace changed selection, counts, or private headers")
            expected = reader.json("GET", base)[2]
            require(reader.json("GET", base + "&Recursive=&IsPlayed=&IsFolder=")[2] == expected,
                    "empty optional catalogue booleans changed omitted-field behavior")
            for suffix in ["Recursive=1", "IsPlayed=yes", "IsFolder=null", "EnableTotalRecordCount="]:
                require(reader.request("GET", base + "&" + suffix)[0] == 400,
                        "invalid catalogue boolean was silently accepted")
        admin.json("POST", f"/Users/{user_id}/Policy", {"EnableAllFolders": False, "EnabledFolders": []}, expected=(204,))
        revoked = reader.json("GET", "/Items" + query + "&Recursive=True")[2]
        require(revoked["Items"] == [] and revoked["TotalRecordCount"] == 0,
                "multi-field ordering retained a revoked library grant")
        report("Catalogue sorting, artist filters, exclusions, boolean parsing, counts, legacy query, and access revocation", True,
               "real album/track order, metadata precedence and full/page count modes have separate PostgreSQL coverage; folder filtering remains qualified")
    finally:
        reader.json("POST", "/Sessions/Logout", expected=(204,))


def verify_instant_mix(admin: HttpClient, library_id: str, item_id: str) -> None:
    suffix = uuid.uuid4().hex
    username, password = "mix-reader-" + suffix, "Synthetic-" + uuid.uuid4().hex
    user_id = str(ensure_user(admin, username, password, library_id, playback=True)["Id"])
    reader = HttpClient(admin.base_url)
    login(reader, username, password)
    playlists = []
    try:
        for prefix in ("Items", "Songs"):
            path = f"/{prefix}/{item_id}/InstantMix"
            status, headers, result = reader.json("GET", path + "?limit=300&fields=Path,Genres")
            require(status == 200 and headers.get("Cache-Control") == "private, no-store"
                    and result["StartIndex"] == 0 and result["Items"][0]["Id"] == item_id
                    and all(entry.get("Type") == "Audio" and "Path" not in entry for entry in result["Items"])
                    and len(result["Items"]) <= 100, "Instant Mix omitted its seed or returned unbounded/private data")
            total = result["TotalRecordCount"]
            require(total >= len(result["Items"]), "Instant Mix's matching count is smaller than its queue")
            empty = reader.json("GET", path + "?limit=0")[2]
            require(empty["Items"] == [] and empty["TotalRecordCount"] == total,
                    "zero-length Instant Mix lost its matching count")
            options = reader.json("GET", path + "?enableImages=false&enableUserData=false&enableImageTypes=Chapter")[2]
            require(all("ImageTags" not in entry and "UserData" not in entry for entry in options["Items"]),
                    "Instant Mix ignored disabled images/user data")
            status, headers, body = reader.request("HEAD", path)
            require(status == 200 and not body and headers.get("Cache-Control") == "private, no-store",
                    "Instant Mix HEAD returned a body or omitted private headers")
            require(HttpClient(admin.base_url).request("GET", path)[0] == 401,
                    "anonymous Instant Mix was accepted")
            for options in ("limit=-1", "imageTypeLimit=-1", "enableImageTypes=Unknown"):
                require(reader.request("GET", path + "?" + options)[0] == 400,
                        "Instant Mix ignored invalid options")
        for ids in ([item_id], []):
            playlist_id = str(reader.json("POST", "/Playlists", {
                "Name": "Synthetic Instant Mix queue", "Ids": ids,
            })[2]["Id"])
            playlists.append(playlist_id)
            for prefix in ("Items", "Playlists"):
                result = reader.json("GET", f"/{prefix}/{playlist_id}/InstantMix?userId={user_id}")[2]
                require((bool(result["Items"]) and result["Items"][0]["Id"] == item_id) if ids else
                        result["Items"] == [] and result["TotalRecordCount"] == 0,
                        "playlist Instant Mix lost the seed or invented music for an empty queue")
        report("Instant Mix music seed, private options, bounded queues, HEAD, empty playlists, and anonymous rejection", True,
               "album/artist relationships, genre lookup, hidden/rating/library boundaries and shared revocation have PostgreSQL fixtures")
    finally:
        for playlist_id in playlists:
            reader.json("DELETE", "/Playlists/" + playlist_id, expected=(204,))
        reader.json("POST", "/Sessions/Logout", expected=(204,))


def verify_playlist_sharing(admin: HttpClient, library_id: str, item_id: str) -> None:
    suffix = uuid.uuid4().hex
    username, password = "playlist-recipient-" + suffix, "Synthetic-" + uuid.uuid4().hex
    recipient_id = str(ensure_user(admin, username, password, library_id, playback=True)["Id"])
    recipient = HttpClient(admin.base_url)
    login(recipient, username, password)
    playlist_id = str(admin.json("POST", "/Playlists", {
        "Name": "Synthetic shared music", "Ids": [item_id],
        "Users": [{"UserId": recipient_id, "CanEdit": False}],
    })[2]["Id"])
    path = "/Playlists/" + playlist_id
    permission = path + "/Users/" + recipient_id
    try:
        status, headers, shares = admin.json("GET", path + "/Users")
        require(status == 200 and headers.get("Cache-Control") == "private, no-store"
                and shares == [{"UserId": recipient_id, "CanEdit": False}],
                "shared playlist permissions were not persisted privately")
        items = recipient.json("GET", path + "/Items")[2]
        require([entry["Id"] for entry in items["Items"]] == [item_id], "recipient's shared queue changed its media selection")
        mix = recipient.json("GET", path + "/InstantMix")[2]
        require(mix["Items"][0]["Id"] == item_id, "read-only recipient could not mix a shared music queue")
        catalog = recipient.json("GET", "/Items?IncludeItemTypes=Playlist")[2]
        require(any(str(entry["Id"]).replace("-", "") == playlist_id.replace("-", "")
                    and entry.get("CanDelete") is False for entry in catalog["Items"]),
                "recipient's catalog omitted the shared playlist or granted deletion")
        require(recipient.request("POST", path, {"Name": "Denied rename"})[0] == 403,
                "read-only recipient could edit the playlist")
        require(recipient.request("POST", permission, {"CanEdit": True})[0] == 403,
                "recipient could elevate its own playlist permission")
        require(recipient.request("DELETE", path)[0] == 404, "recipient could delete the owner's playlist")
        admin.json("POST", permission, {"CanEdit": True}, expected=(204,))
        recipient.json("POST", path, {"Name": "Synthetic recipient edit"}, expected=(204,))
        require(admin.json("GET", "/Items/" + playlist_id)[2]["Name"] == "Synthetic recipient edit",
                "recipient's permitted edit did not persist")
        admin.json("DELETE", permission, expected=(204,))
        require(recipient.request("GET", path)[0] == 404 and recipient.request("GET", path + "/Items")[0] == 404,
                "revoked recipient retained playlist access on an existing session")
        require(recipient.request("GET", path + "/InstantMix")[0] == 404,
                "revoked recipient retained Instant Mix access on an existing session")
        report("Shared music playlist read/edit permissions, catalog visibility, owner-only management, and revocation", True)
    finally:
        recipient.json("POST", "/Sessions/Logout", expected=(204,))
        admin.json("DELETE", path, expected=(204,))


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
    sdr_direct_play_fixture_path = fixture_root / SDR_DIRECT_PLAY_FIXTURE_RELATIVE
    if not args.direct_play_only and not fixture_path.is_file():
        raise SystemExit(f"Missing generated synthetic fixture: {fixture_path}")
    if not direct_play_fixture_path.is_file():
        raise SystemExit(f"Missing generated H.264/AAC direct-play fixture: {direct_play_fixture_path}")
    if not sdr_direct_play_fixture_path.is_file():
        raise SystemExit(f"Missing generated explicit SDR fixture: {sdr_direct_play_fixture_path}")
    client = HttpClient(base_url)

    wait_for_ready(client)
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
        verify_video_range_acceptance(client, sdr_direct_play_fixture_path, direct_play_fixture_path, args.scan_timeout)
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
    verify_video_range_acceptance(client, sdr_direct_play_fixture_path, direct_play_fixture_path, args.scan_timeout)
    verify_scan_lifecycle(client, library_id, fixture_path.parent, args.scan_timeout)
    audio_library_id, audio_item_id = verify_universal_audio(
        client, fixture_root, values.get("PUFFINBOX_ACCEPTANCE_MUSIC_ROOT", "/media/Music"), args.scan_timeout,
    )
    embedded_audio_snapshot = embedded_audio_fixtures.observe(client, fixture_root, audio_library_id, args.scan_timeout)
    report("Automatic embedded audio metadata, bounded fields and tag-named artist roles", True, embedded_audio_snapshot["scope"])
    if args.require_transcode:
        audio_ffprobe = values.get("PUFFINBOX_ACCEPTANCE_FFPROBE_PATH") or shutil.which("ffprobe")
        require(bool(audio_ffprobe), "ffprobe is required to inspect converted audio")
        verify_universal_audio_transcode(client, audio_library_id, audio_item_id, fixture_root, audio_ffprobe)
    verify_item_relations(client, audio_library_id, audio_item_id)
    verify_instant_mix(client, audio_library_id, audio_item_id)
    verify_catalog_ordering(client, audio_library_id, audio_item_id)
    verify_playlist_sharing(client, audio_library_id, audio_item_id)
    verify_catalog_filters(client, library_id)
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
    photo_parent = urllib.parse.quote(str(photo.get("ParentId") or ""))
    require(bool(photo_parent), "photo fixture has no catalog parent")
    selected_ids = f"{photo_id},{item_id},{photo_id},{uuid.uuid4()}"
    _, _, selected = client.json("GET", f"/Users/{admin_user['Id']}/Items?Ids={selected_ids}&Limit=300")
    require([item["Id"] for item in selected["Items"]] == [photo["Id"], item_id]
            and selected["TotalRecordCount"] == 2,
            "explicit queue identifiers included unrelated items or lost requested order")
    _, _, selected_page = client.json("GET", f"/Items?ids={selected_ids}&StartIndex=1&Limit=1")
    require([item["Id"] for item in selected_page["Items"]] == [item_id]
            and selected_page["TotalRecordCount"] == 2,
            "explicit queue selection was not applied before counts and pagination")
    require(client.request("GET", f"/Items?Ids={photo_id},invalid")[0] == 400,
            "invalid queue identifiers were accepted")
    _, _, photo_page = client.json("GET", f"/Users/{admin_user['Id']}/Items?ParentId={photo_parent}&Filters=IsNotFolder&Recursive=false&SortBy=SortName&MediaTypes=Photo,Video&SortOrder=Ascending&Fields=Chapters,MediaSources,Trickplay&ExcludeLocationTypes=Virtual&EnableTotalRecordCount=false&CollapseBoxSetItems=false")
    require([item["Id"] for item in photo_page["Items"]] == [photo["Id"]]
            and all(item["IsFolder"] is False for item in photo_page["Items"])
            and photo_page.get("TotalRecordCount") == 1,
            "official photo viewer query did not select its visible nonfolder fixture")
    _, _, folder_page = client.json("GET", f"/Items?ParentId={library_id}&IsFolder=true&Recursive=true&Limit=1000")
    require(folder_page["Items"] and all(item["IsFolder"] is True for item in folder_page["Items"]),
            "folder selection returned a nonfolder or no synthetic folders")
    require(client.request("GET", "/Items?IsFolder=true&Filters=IsNotFolder")[0] == 400,
            "conflicting folder selectors were accepted")
    photo_status, photo_headers, photo_bytes = client.request("GET", f"/Items/{photo_id}/File")
    require(photo_status == 200 and photo_headers.get("Content-Type", "").startswith("image/png") and photo_bytes.startswith(b"\x89PNG\r\n\x1a\n"),
            "safe raster preview did not return the expected PNG bytes and MIME type")
    require(photo_headers.get("X-Content-Type-Options", "").lower() == "nosniff", "raster preview did not include nosniff")
    primary_status, primary_headers, primary_bytes = client.request("GET", f"/Items/{photo_id}/Images/Primary")
    require(primary_status == 200 and primary_bytes == photo_bytes
            and primary_headers.get("Content-Type", "").startswith("image/png")
            and primary_headers.get("Cache-Control", "").lower() == "private, no-store",
            "primary photo route did not serve the private original raster")
    primary_head, head_headers, head_bytes = client.request("HEAD", f"/Items/{photo_id}/Images/Primary")
    require(primary_head == 200 and not head_bytes
            and head_headers.get("Content-Length") == str(len(photo_bytes)),
            "primary photo HEAD did not preserve original metadata")
    range_status, range_headers, range_bytes = client.request("GET", f"/Items/{photo_id}/Images/Primary", headers={"Range": "bytes=1-5"})
    require(range_status == 206 and range_bytes == photo_bytes[1:6]
            and range_headers.get("Content-Range") == f"bytes 1-5/{len(photo_bytes)}",
            "primary photo range did not return the selected original bytes")
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
    _, _, hidden_selection = denied_client.json("GET", f"/Items?Ids={item_id}")
    require(hidden_selection["Items"] == [] and hidden_selection["TotalRecordCount"] == 0,
            "explicit queue identifiers bypassed the empty library policy")
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
    verify_socket_notifications(base_url, admin_token, peer_client, peer_user, peer_token, item_id)
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

    require(source.get("TranscodingSubProtocol") == "hls", "PlaybackInfo does not identify its HLS streaming protocol")
    require(source.get("TranscodingContainer") == "ts", "PlaybackInfo does not identify its MPEG-TS HLS segments")
    require(source.get("DefaultAudioStreamIndex") == audio_index, "PlaybackInfo does not preserve the selected audio track")
    require(source.get("DefaultSubtitleStreamIndex") == subtitle_index, "PlaybackInfo does not preserve the selected subtitle track")

    # Native decoders do not share the authenticated web view's cookie jar.
    query = urllib.parse.parse_qs(urllib.parse.urlsplit(transcode_url).query)
    require(len(query.get("ApiKey", [])) == 1, "negotiated HLS URL omitted its scoped media credential")
    native = HttpClient(base_url)
    native_status, native_headers, native_master = native.request("GET", transcode_url)
    require(native_status == 200 and b"#EXTM3U" in native_master,
            "negotiated HLS URL could not authenticate a decoder without cookies or headers")
    require(native_headers.get("Cache-Control") == "private, no-store", "native HLS master omitted private cache controls")
    native_variant = next((line for line in native_master.decode("utf-8").splitlines() if line.startswith("/") and "playlist.m3u8" in line), None)
    require(native_variant is not None, "native HLS master omitted its media variant")
    status, _, native_playlist = native.request("GET", native_variant)
    require(status == 200 and b"#EXTM3U" in native_playlist, "native HLS variant lost its scoped credential")
    native_segment = next((line for line in native_playlist.decode("utf-8").splitlines() if line.startswith("/") and ".ts?" in line), None)
    require(native_segment is not None, "native HLS variant omitted authenticated segments")
    status, _, native_bytes = native.request("GET", native_segment)
    require(status == 200 and len(native_bytes) > 188, "native HLS segment lost its scoped credential")

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
    verify_hls_timeline(client, shutdown_item_id, playback_payload["DeviceProfile"], fixture_root, ffprobe)
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
    explicit_user_data = verify_user_data_edits(client, shutdown_item_id)
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
            args.env_file, project_name, explicit_user_data,
        )
        after_restart = HttpClient(base_url)
        login(after_restart, values["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"], values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"])
        try:
            require(embedded_audio_fixtures.observe(after_restart, fixture_root, audio_library_id, args.scan_timeout) == embedded_audio_snapshot,
                    "Embedded metadata or original tagged fixtures changed across the container restart.")
        finally:
            after_restart.json("POST", "/Sessions/Logout", expected=(204,))
        report("Embedded audio metadata and fixture identity across container restart", True)
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
