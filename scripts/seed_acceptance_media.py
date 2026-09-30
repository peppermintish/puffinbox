#!/usr/bin/env python3
"""Index the original long playback, music, and book fixtures on the isolated stack."""

from __future__ import annotations

from http import cookiejar
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / ".local" / "acceptance"
ENV_FILE = LOCAL / "acceptance.env"
EVIDENCE = LOCAL / "fixture-seed.json"


def read_env() -> dict[str, str]:
    result: dict[str, str] = {}
    for line in ENV_FILE.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            result[key.strip()] = value.strip()
    return result


class Client:
    def __init__(self, base_url: str):
        self.base = base_url.rstrip("/")
        parts = urllib.parse.urlsplit(self.base)
        self.origin = f"{parts.scheme}://{parts.netloc}"
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookiejar.CookieJar()))

    def request(self, method: str, path: str, body: object | None = None):
        data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        headers = {"Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        if method not in {"GET", "HEAD"}:
            headers["Origin"] = self.origin
        request = urllib.request.Request(self.base + path, data=data, headers=headers, method=method)
        try:
            response = self.opener.open(request, timeout=20)
        except urllib.error.HTTPError as error:
            return error.code, error.headers, error.read()
        with response:
            return response.status, response.headers, response.read()

    def json(self, method: str, path: str, body: object | None = None, expected: tuple[int, ...] = (200,)):
        status, headers, raw = self.request(method, path, body)
        if status not in expected:
            raise RuntimeError(f"{method} {path} returned HTTP {status}")
        return headers, json.loads(raw) if raw else None


def main() -> int:
    if not ENV_FILE.is_file():
        raise SystemExit("Run the local acceptance preparation and subtitle fixture generator first.")
    env = read_env()
    fixture_root_override = os.environ.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT")
    fixture_root = Path(
        fixture_root_override or env.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT", str(LOCAL / "media"))
    ).expanduser()
    if not fixture_root.is_absolute():
        fixture_root = ROOT / fixture_root
    fixture_root = fixture_root.resolve()
    fixture = fixture_root / "Movies" / "Puffinbox Subtitle Selection Fixture.mkv"
    if not fixture.is_file():
        raise SystemExit("Run the local acceptance preparation and subtitle fixture generator first.")
    base_url = env.get("PUFFINBOX_ACCEPTANCE_URL", "")
    parts = urllib.parse.urlsplit(base_url)
    if parts.scheme != "http" or parts.hostname not in {"127.0.0.1", "localhost", "::1"}:
        raise SystemExit("Refusing to seed anything except a loopback HTTP acceptance server.")
    if env.get("COMPOSE_PROJECT_NAME") != "puffinbox-acceptance" or env.get("PUFFINBOX_ACCEPTANCE_ISOLATED") != "1":
        raise SystemExit("Refusing to seed: settings do not identify the dedicated acceptance stack.")
    client = Client(base_url)
    _, ready = client.json("GET", "/health/ready")
    if not isinstance(ready, dict):
        raise SystemExit("The acceptance server is not database-ready.")
    _, login = client.json("POST", "/Users/AuthenticateByName", {
        "Username": env["PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME"],
        "Pw": env["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"],
        "DeviceId": "acceptance-media-seeder",
        "DeviceName": "Local Acceptance Seeder",
        "Client": "Puffinbox Acceptance",
        "Version": "1",
    })
    if not isinstance(login, dict) or not login.get("AccessToken"):
        raise SystemExit("Administrator authentication did not return an access token.")

    libraries = [
        (
            "Puffinbox Acceptance Music",
            str(fixture_root / "Music") if fixture_root_override else env.get("PUFFINBOX_ACCEPTANCE_MUSIC_ROOT", "/media/Music"),
            "music",
        ),
        (
            "Puffinbox Acceptance Books",
            str(fixture_root / "Books") if fixture_root_override else env.get("PUFFINBOX_ACCEPTANCE_BOOKS_ROOT", "/media/Books"),
            "books",
        ),
    ]
    _, existing = client.json("GET", "/Library/VirtualFolders")
    for name, location, collection_type in libraries:
        if any(row.get("Name") == name for row in existing or []):
            continue
        if not (fixture_root / location.rsplit("/", 1)[-1]).is_dir():
            raise SystemExit(f"Expected generated fixture directory for {name} is missing.")
        client.json("POST", "/Library/VirtualFolders", {
            "Name": name,
            "Locations": [location],
            "CollectionType": collection_type,
            "LibraryOptions": {"Enabled": True},
        }, expected=(204,))
    client.json("POST", "/Library/Refresh", {}, expected=(202,))

    deadline = time.monotonic() + 120
    scan_rows: list[dict] = []
    while time.monotonic() < deadline:
        _, rows = client.json("GET", "/Library/ScanStatus")
        scan_rows = rows if isinstance(rows, list) else []
        expected_ids = {
            str(row.get("ItemId") or row.get("Id"))
            for row in client.json("GET", "/Library/VirtualFolders")[1] or []
            if row.get("Name") in {"Puffinbox Acceptance Music", "Puffinbox Acceptance Books"}
        }
        indexed_rows = {str(row.get("LibraryId")): row for row in scan_rows}
        if expected_ids and all(
            indexed_rows.get(library_id, {}).get("Status") in {"completed", "completed_with_errors"}
            for library_id in expected_ids
        ):
            break
        time.sleep(0.5)
    else:
        raise SystemExit("The fixture library scans did not finish before the 120-second deadline.")

    query = urllib.parse.urlencode({
        "Recursive": "true", "SearchTerm": "Puffinbox", "StartIndex": 0, "Limit": 500,
        "EnableTotalRecordCount": "true",
    })
    _, result = client.json("GET", "/Items?" + query)
    items = result.get("Items", []) if isinstance(result, dict) else []
    expected = {
        "Puffinbox Subtitle Selection Fixture.mkv": "Movie",
        "Puffinbox Original Acceptance Track.flac": "Audio",
        "Puffinbox Original Acceptance Book.epub": None,
    }
    found: dict[str, dict] = {}
    for item in items:
        name = str(item.get("Name", ""))
        if name in expected:
            found[name] = item
    missing = sorted(set(expected) - set(found))
    if missing:
        raise SystemExit("Scanner did not index all original fixtures: " + ", ".join(missing))
    if found["Puffinbox Subtitle Selection Fixture.mkv"].get("Type") != "Movie":
        raise SystemExit("The long multi-track fixture was not classified as a movie.")
    if found["Puffinbox Original Acceptance Track.flac"].get("Type") != "Audio":
        raise SystemExit("The FLAC fixture was not classified as audio.")
    if str(found["Puffinbox Original Acceptance Book.epub"].get("Container", "")).lower() != "epub":
        raise SystemExit("The original EPUB fixture was not classified with its expected container.")

    EVIDENCE.parent.mkdir(parents=True, exist_ok=True)
    EVIDENCE.write_text(json.dumps({
        "seededAtUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "scope": "isolated loopback acceptance stack",
        "fixture": {
            "path": str(fixture.relative_to(ROOT)) if fixture.is_relative_to(ROOT) else str(fixture),
            "durationSeconds": 180,
            "audioTracks": ["eng 440 Hz", "spa 880 Hz"],
            "subtitleTracks": ["eng numbered cues", "spa numbered cues"],
            "burnedTimestamp": True,
        },
        "indexedItems": {
            name: {"id": str(item.get("Id")), "type": item.get("Type"), "container": item.get("Container")}
            for name, item in found.items()
        },
        "scanStatus": [row for row in scan_rows if row.get("LibraryId") in expected_ids],
    }, indent=2) + "\n", encoding="utf-8")
    print("Acceptance movie/music/book libraries are indexed and the 180-second track-selection fixture is ready.")
    print(f"Local seed evidence: {EVIDENCE}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
