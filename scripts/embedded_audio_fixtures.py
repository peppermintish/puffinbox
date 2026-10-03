"""Original tagged audio fixtures and public metadata acceptance checks."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import subprocess
import time
import urllib.parse


RELATIVE_ROOT = Path("Embedded Folder Artist") / "Embedded Folder Album"
TAGS = {
    "blob-a.flac": {"title": "Embedded Alpha Title", "artist": "Embedded Lead; Embedded Guest",
                    "album_artist": "Embedded Album Artist", "album": "Embedded Album", "date": "2021-04-05",
                    "track": "3/12", "disc": "2/4", "genre": "Rock; Jazz", "comment": "Synthetic embedded overview"},
    "blob-b.flac": {"title": "Embedded Beta Title", "artist": "Embedded Lead", "album": "Embedded Album",
                    "date": "2023", "track": "04/12", "disc": "1", "genre": "Jazz"},
    "blob-c.flac": {"title": "Embedded Conflicting Title", "artist": "Embedded Lead",
                    "album_artist": "Embedded Album Artist", "album": "Embedded Album", "date": "2022",
                    "track": "7/12", "disc": "3/4", "genre": "Embedded Genre"},
    "04 Plain Track.flac": {},
}


def prepare(ffmpeg: str, music_root: Path) -> None:
    folder = music_root / RELATIVE_ROOT
    folder.mkdir(parents=True, exist_ok=True)
    for index, (name, tags) in enumerate(TAGS.items()):
        destination = folder / name
        if os.path.lexists(destination):
            continue
        command = [ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
                   "-f", "lavfi", "-i", f"sine=frequency={523 + index * 47}:sample_rate=44100:duration=5",
                   "-c:a", "flac", "-map_metadata", "-1"]
        for key, value in tags.items():
            command.extend(["-metadata", key + "=" + value])
        command.append(str(destination))
        subprocess.run(command, check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                       stderr=subprocess.PIPE, timeout=30)


def observe(client, fixture_root: Path, library_id: str, timeout: int = 120) -> dict:
    folder = fixture_root / "Music" / RELATIVE_ROOT
    hashes = {name: hashlib.sha256((folder / name).read_bytes()).hexdigest() for name in TAGS}
    query = urllib.parse.urlencode({"ParentId": library_id, "IncludeItemTypes": "Audio", "Recursive": "true", "Fields": "Path"})
    deadline = time.monotonic() + timeout
    while True:
        items = client.json("GET", "/Items?" + query)[2]["Items"]
        tracks = {Path(item.get("Path", "")).name: item for item in items
                  if RELATIVE_ROOT.as_posix() + "/" in item.get("Path", "")}
        if all(name in tracks and tracks[name]["Name"] == tags.get("title", Path(name).stem)
               for name, tags in TAGS.items()):
            break
        assert time.monotonic() < deadline, "Automatic embedded audio refresh did not finish."
        time.sleep(.5)
    snapshot = {}
    for name, tags in TAGS.items():
        item = tracks[name]
        details = client.json("GET", "/Puffinbox/Metadata/Items/" + item["Id"])[2]
        rows = [row for row in details["Providers"] if row["ProviderKey"] == "embedded-audio"]
        assert len(rows) == 1, "Expected one current embedded audio provider row."
        row = rows[0]
        metadata = row["Metadata"]
        assert row["Overview"] is None and row["ContentRating"] is None
        assert metadata["artists"] == ([tags["artist"]] if "artist" in tags else [])
        assert metadata["albumArtists"] == ([tags.get("album_artist", tags.get("artist"))] if "artist" in tags else [])
        assert metadata["album"] == tags.get("album")
        for field, tag in [("IndexNumber", "track"), ("ParentIndexNumber", "disc")]:
            if tag in tags:
                assert item[field] == int(tags[tag].split("/")[0]), "Tagged audio number was lost."
        if "date" in tags:
            date = tags["date"] if "-" in tags["date"] else tags["date"] + "-01-01"
            assert item["ProductionYear"] == int(date[:4]) and item["PremiereDate"].startswith(date + "T00:00:00")
            assert item["Album"] == tags["album"]
        assert item["Genres"] == ([tags["genre"]] if "genre" in tags else [])
        # These tag names have no matching folder artists in this fixture.
        # They must not be linked to the unrelated folder-derived artist.
        if tags:
            assert item["Artists"] == [] and item["AlbumArtists"] == []
        fields = ["Id", "Name", "Album", "IndexNumber", "ParentIndexNumber", "ProductionYear", "PremiereDate", "Genres"]
        snapshot[name] = {field: item.get(field) for field in fields}
    assert hashes == {name: hashlib.sha256((folder / name).read_bytes()).hexdigest() for name in TAGS}
    return {"items": snapshot, "fixtureFiles": hashes,
            "scope": "Embedded display fields and stored credits; automatic named-artist/album creation remains incomplete."}
