#!/usr/bin/env python3
"""Add small original audio, book, and long-running HLS fixtures to local acceptance data."""

from __future__ import annotations

import os
import shutil
import subprocess
import zipfile
from pathlib import Path
import embedded_audio_fixtures


ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / ".local" / "acceptance"
DEFAULT_MEDIA = LOCAL / "media"


def epub_bytes(path: Path) -> None:
    container = """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""
    package = """<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book-id" xml:lang="en">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="book-id">urn:puffinbox:acceptance:book-001</dc:identifier>
    <dc:title>A Small Original Acceptance Book</dc:title>
    <dc:creator>Acceptance Fixture Author</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine><itemref idref="chapter"/></spine>
</package>"""
    chapter = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Acceptance Fixture</title></head>
<body><h1>A Small Original Acceptance Book</h1><p>This short text was created for local library acceptance.</p></body></html>"""
    nav = """<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head>
<body><nav epub:type="toc"><ol><li><a href="chapter.xhtml">Chapter</a></li></ol></nav></body></html>"""
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("mimetype", "application/epub+zip", compress_type=zipfile.ZIP_STORED)
        archive.writestr("META-INF/container.xml", container)
        archive.writestr("OEBPS/content.opf", package)
        archive.writestr("OEBPS/chapter.xhtml", chapter)
        archive.writestr("OEBPS/nav.xhtml", nav)


def paths_overlap(left: Path, right: Path) -> bool:
    return left == right or left in right.parents or right in left.parents


def resolve_from_root(path: str | Path) -> Path:
    selected = Path(path).expanduser()
    if not selected.is_absolute():
        selected = ROOT / selected
    return selected.resolve()


def main() -> int:
    state_root_override = os.environ.get("PUFFINBOX_ACCEPTANCE_STATE_ROOT")
    state_root = resolve_from_root(state_root_override or LOCAL)
    custom_state_root = state_root != LOCAL.resolve()
    if custom_state_root and paths_overlap(state_root, LOCAL.resolve()):
        raise SystemExit(f"Alternate acceptance state must be outside the active acceptance tree: {state_root}")

    env_path = state_root / "acceptance.env"
    if not env_path.is_file():
        raise SystemExit(
            f"Run scripts/prepare_acceptance.py first for the selected acceptance state; "
            f"this script only adds generated fixtures: {env_path}"
        )
    settings = dict(line.split("=", 1) for line in env_path.read_text(encoding="utf-8").splitlines()
                    if line and not line.startswith("#") and "=" in line)
    if settings.get("PUFFINBOX_ACCEPTANCE_ISOLATED") != "1":
        raise SystemExit("Refusing to add fixtures outside the isolated acceptance media folder.")
    selected_media = os.environ.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT")
    if not selected_media:
        selected_media = settings.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT")
    if not selected_media:
        selected_media = str(state_root / "media") if custom_state_root else str(DEFAULT_MEDIA)
    media = resolve_from_root(selected_media)
    if custom_state_root and paths_overlap(media, LOCAL.resolve()):
        raise SystemExit(f"Alternate acceptance fixtures must be outside the active acceptance tree: {media}")

    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        raise SystemExit("ffmpeg is required to create supplemental synthetic audio and video fixtures.")

    movies = media / "Movies"
    music = media / "Music"
    books = media / "Books"
    for folder in (movies, music, books):
        folder.mkdir(parents=True, exist_ok=True)

    shutdown_fixture = movies / "Puffinbox Active HLS Shutdown Fixture.mkv"
    if not os.path.lexists(shutdown_fixture):
        subprocess.run([
            ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
            "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
            "-f", "lavfi", "-i", "sine=frequency=331:sample_rate=48000",
            "-t", "300", "-map", "0:v:0", "-map", "1:a:0",
            "-c:v", "mpeg4", "-q:v", "5", "-c:a", "libmp3lame", "-b:a", "128k",
            "-metadata", "title=Puffinbox Active HLS Shutdown Fixture",
            "-metadata", "comment=Original synthetic test content",
            str(shutdown_fixture),
        ], check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    audio_fixture = music / "Puffinbox Original Acceptance Track.flac"
    if not os.path.lexists(audio_fixture):
        subprocess.run([
            ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
            "-f", "lavfi", "-i", "sine=frequency=523:sample_rate=44100:duration=20",
            "-c:a", "flac", "-metadata", "title=Puffinbox Original Acceptance Track",
            "-metadata", "artist=Acceptance Fixture Artist", "-metadata", "album=Local Test Record",
            "-metadata", "genre=Test",
            str(audio_fixture),
        ], check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    embedded_audio_fixtures.prepare(ffmpeg, music)

    book_fixture = books / "Puffinbox Original Acceptance Book.epub"
    if not os.path.lexists(book_fixture):
        epub_bytes(book_fixture)

    print(f"Supplemental original audio, book, and HLS fixtures are ready in {media}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
