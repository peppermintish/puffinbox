#!/usr/bin/env python3
"""Create isolated synthetic media and ignored local acceptance credentials."""

from __future__ import annotations

import os
import secrets
import shutil
import socket
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / ".local" / "acceptance"
DIRECT_PLAY_FIXTURE_NAME = "Puffinbox Synthetic Direct Play Fixture.mp4"
SHUTDOWN_FIXTURE_NAME = "Puffinbox Active HLS Shutdown Fixture.mkv"


def require_fresh_fixture_root(path: Path) -> None:
    """Keep preparation inside a new or empty directory selected for acceptance fixtures."""
    if os.path.lexists(path) and (not path.is_dir() or next(path.iterdir(), None) is not None):
        raise SystemExit(f"Acceptance fixture root must be a new or empty directory; preserve its contents: {path}")


def create_directory_if_absent(path: Path) -> None:
    """Create the selected directory and parents without replacing existing paths."""
    if os.path.lexists(path):
        if not path.is_dir():
            raise SystemExit(f"Acceptance state path is not a directory; preserve it: {path}")
        return
    path.mkdir(parents=True, exist_ok=False)


def require_unoccupied_targets(paths: list[Path]) -> None:
    """Refuse to replace any file already present in the acceptance fixture tree."""
    collisions = [path for path in paths if os.path.lexists(path)]
    if collisions:
        names = ", ".join(str(path) for path in collisions)
        raise SystemExit(f"Acceptance fixture targets already exist; preserve them and choose a fresh fixture tree: {names}")


def copy_file_exclusive(source: Path, destination: Path) -> None:
    """Copy a synthetic fixture without replacing an existing destination."""
    with source.open("rb") as input_file, destination.open("xb") as output_file:
        shutil.copyfileobj(input_file, output_file)


def available_loopback_ports(count: int) -> list[int]:
    listeners: list[socket.socket] = []
    try:
        ports = []
        for _ in range(count):
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            listener.bind(("127.0.0.1", 0))
            listeners.append(listener)
            ports.append(int(listener.getsockname()[1]))
        return ports
    finally:
        for listener in listeners:
            listener.close()


def paths_overlap(left: Path, right: Path) -> bool:
    return left == right or left in right.parents or right in left.parents


def main() -> int:
    state_root_override = os.environ.get("PUFFINBOX_ACCEPTANCE_STATE_ROOT")
    state_root = Path(state_root_override).expanduser() if state_root_override else LOCAL
    if not state_root.is_absolute():
        state_root = ROOT / state_root
    state_root = state_root.resolve()
    env_file = state_root / "acceptance.env"
    if os.path.lexists(env_file):
        raise SystemExit(f"{env_file} already exists; preserve its credentials and media. Choose a new acceptance state root.")
    custom_state_root = state_root != LOCAL.resolve()
    if custom_state_root:
        if paths_overlap(state_root, LOCAL.resolve()):
            raise SystemExit(f"Alternate acceptance state must be outside the active acceptance tree: {state_root}")
        require_fresh_fixture_root(state_root)
    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    if not ffmpeg or not ffprobe:
        raise SystemExit("ffmpeg and ffprobe must be installed locally to generate synthetic acceptance fixtures.")

    fixture_root_override = os.environ.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT")
    media = Path(fixture_root_override).expanduser() if fixture_root_override else state_root / "media"
    if not media.is_absolute():
        media = ROOT / media
    media = media.resolve()
    if custom_state_root and paths_overlap(media, LOCAL.resolve()):
        raise SystemExit(f"Alternate acceptance fixtures must be outside the active acceptance tree: {media}")
    require_fresh_fixture_root(media)
    create_directory_if_absent(state_root)
    movies = media / "Movies"
    photos = media / "Photos"
    trust_root = media / "TrustRoot"
    movies.mkdir(parents=True, exist_ok=True)
    photos.mkdir(parents=True, exist_ok=True)
    trust_root.mkdir(parents=True, exist_ok=True)
    incompatible = movies / "Puffinbox Synthetic Transcode Fixture.mkv"
    direct_play = movies / DIRECT_PLAY_FIXTURE_NAME
    shutdown_fixture = movies / SHUTDOWN_FIXTURE_NAME
    photo = photos / "Puffinbox Synthetic Photo.png"
    trust_fixture = trust_root / "Puffinbox Scanner Trust Fixture.mkv"
    subtitle_file = state_root / "synthetic-subtitles.srt"
    require_unoccupied_targets([incompatible, direct_play, shutdown_fixture, photo, trust_fixture, subtitle_file])
    subtitle_file.write_text(
        "1\n00:00:01,000 --> 00:00:03,000\nSynthetic subtitle acceptance cue.\n\n"
        "2\n00:00:05,000 --> 00:00:07,000\nSecond synthetic cue.\n",
        encoding="utf-8",
    )
    subprocess.run(
        [
            ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
            "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
            "-f", "lavfi", "-i", "sine=frequency=997:sample_rate=48000",
            "-f", "srt", "-i", str(subtitle_file), "-t", "12",
            "-map", "0:v:0", "-map", "1:a:0", "-map", "2:s:0",
            "-c:v", "mpeg4", "-q:v", "4", "-c:a", "libmp3lame", "-b:a", "160k",
            "-c:s", "srt",
            "-metadata", "title=Puffinbox Synthetic Transcode Fixture",
            "-metadata", "comment=Generated synthetic test content",
            "-metadata:s:s:0", "language=eng", "-metadata:s:s:0", "title=Synthetic subtitles",
            str(incompatible),
        ],
        check=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    copy_file_exclusive(incompatible, trust_fixture)
    subprocess.run(
        [
            ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
            "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
            "-f", "lavfi", "-i", "sine=frequency=331:sample_rate=48000",
            "-t", "300", "-map", "0:v:0", "-map", "1:a:0",
            "-c:v", "mpeg4", "-q:v", "5", "-c:a", "libmp3lame", "-b:a", "128k",
            "-metadata", "title=Puffinbox Active HLS Shutdown Fixture",
            "-metadata", "comment=Generated synthetic test content",
            str(shutdown_fixture),
        ],
        check=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        subprocess.run(
            [
                ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
                "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
                "-t", "2", "-map", "0:v:0", "-map", "1:a:0",
                "-c:v", "libx264", "-preset", "ultrafast", "-profile:v", "baseline",
                "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "64k",
                "-movflags", "+faststart",
                "-metadata", "title=Puffinbox Synthetic Direct Play Fixture",
                "-metadata", "comment=Generated synthetic test content",
                str(direct_play),
            ],
            check=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as error:
        diagnostic = (error.stderr or b"").decode("utf-8", errors="replace").strip()
        suffix = f": {diagnostic[-1000:]}" if diagnostic else ""
        raise SystemExit(f"FFmpeg could not generate the H.264/AAC direct-play fixture; libx264 and AAC encoding are required{suffix}") from error
    subprocess.run(
        [
            ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
            "-f", "lavfi", "-i", "color=c=steelblue:s=640x360:d=0.1",
            "-frames:v", "1", str(photo),
        ],
        check=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )

    server_port, postgres_port = available_loopback_ports(2) if custom_state_root else (18096, 55432)
    values = {
        "POSTGRES_DB": f"puffinbox_acceptance_{secrets.token_hex(4)}" if custom_state_root else "puffinbox_acceptance",
        "POSTGRES_USER": f"puffinbox_acceptance_{secrets.token_hex(4)}" if custom_state_root else "puffinbox_acceptance",
        "POSTGRES_PASSWORD": secrets.token_urlsafe(32),
        "COMPOSE_PROJECT_NAME": f"puffinbox-acceptance-{secrets.token_hex(4)}" if custom_state_root else "puffinbox-acceptance",
        "PUFFINBOX_PORT": f"127.0.0.1:{server_port}",
        "PUFFINBOX_SERVER_NAME": "Puffinbox Acceptance",
        "MEDIA_ROOT": str(media) if custom_state_root else "./.local/acceptance/media",
        "PUFFINBOX_COOKIE_SECURE": "false",
        "PUFFINBOX_TRUSTED_PROXIES": "",
        "PUFFINBOX_LOCAL_NETWORKS": "",
        "PUFFINBOX_CORS_ORIGINS": "",
        "PUFFINBOX_BOOTSTRAP_ADMIN_USERNAME": "acceptance-admin",
        "PUFFINBOX_BOOTSTRAP_ADMIN_PASSWORD": secrets.token_urlsafe(32),
        "OPERATOR_FFMPEG_IMAGE": "puffinbox:acceptance-tools",
        "PUFFINBOX_ACCEPTANCE_ISOLATED": "1",
        "PUFFINBOX_ACCEPTANCE_URL": f"http://127.0.0.1:{server_port}",
        "PUFFINBOX_ACCEPTANCE_MEDIA_ROOT": "/media/Movies",
        "PUFFINBOX_ACCEPTANCE_PHOTO_ROOT": "/media/Photos",
        "PUFFINBOX_ACCEPTANCE_TRUST_ROOT": "/media/TrustRoot",
        "PUFFINBOX_ACCEPTANCE_FFMPEG": "1",
        "PUFFINBOX_ACCEPTANCE_ADMIN_USERNAME": "acceptance-admin",
        "PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD": "",
        "PUFFINBOX_ACCEPTANCE_VIEWER_USERNAME": "acceptance-viewer",
        "PUFFINBOX_ACCEPTANCE_VIEWER_PASSWORD": secrets.token_urlsafe(32),
        "PUFFINBOX_ACCEPTANCE_PEER_USERNAME": "acceptance-peer",
        "PUFFINBOX_ACCEPTANCE_PEER_PASSWORD": secrets.token_urlsafe(32),
        "PUFFINBOX_ACCEPTANCE_DENIED_USERNAME": "acceptance-denied",
        "PUFFINBOX_ACCEPTANCE_DENIED_PASSWORD": secrets.token_urlsafe(32),
        "PUFFINBOX_ACCEPTANCE_UNRATED_USERNAME": "acceptance-unrated",
        "PUFFINBOX_ACCEPTANCE_UNRATED_PASSWORD": secrets.token_urlsafe(32),
        "PUFFINBOX_ACCEPTANCE_FFPROBE_PATH": ffprobe,
    }
    if custom_state_root:
        values["PUFFINBOX_ACCEPTANCE_POSTGRES_PORT"] = f"127.0.0.1:{postgres_port}"
    if fixture_root_override or custom_state_root:
        values.update({
            "PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT": str(media),
            "MEDIA_ROOT": str(media),
            "PUFFINBOX_ACCEPTANCE_MEDIA_ROOT": "/media/Movies",
            "PUFFINBOX_ACCEPTANCE_PHOTO_ROOT": "/media/Photos",
            "PUFFINBOX_ACCEPTANCE_TRUST_ROOT": "/media/TrustRoot",
            "PUFFINBOX_ACCEPTANCE_MUSIC_ROOT": "/media/Music",
            "PUFFINBOX_ACCEPTANCE_BOOKS_ROOT": "/media/Books",
        })
    values["PUFFINBOX_ACCEPTANCE_ADMIN_PASSWORD"] = values["PUFFINBOX_BOOTSTRAP_ADMIN_PASSWORD"]
    env_file.parent.mkdir(parents=True, exist_ok=True)
    env_file.write_text("# Generated locally. Ignored by Git; do not share or commit.\n" + "".join(f"{name}={value}\n" for name, value in values.items()), encoding="utf-8")
    try:
        env_file.chmod(0o600)
    except OSError:
        pass
    print(f"Synthetic acceptance media: {media}")
    print(f"Local credentials and isolated stack settings saved to: {env_file}")
    print("The administrator password is seeded on first database start; keep this file for later runs.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
