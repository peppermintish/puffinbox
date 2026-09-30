#!/usr/bin/env python3
"""Generate a long, original, multi-track playback acceptance fixture."""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / ".local" / "acceptance"
ENV_FILE = LOCAL / "acceptance.env"
DEFAULT_FIXTURE_ROOT = LOCAL / "media"
DURATION_SECONDS = 180


def configured_fixture_root() -> Path:
    values: dict[str, str] = {}
    if ENV_FILE.is_file():
        for line in ENV_FILE.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            values[key.strip()] = value.strip()
    root = Path(
        os.environ.get(
            "PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT",
            values.get("PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT", str(DEFAULT_FIXTURE_ROOT)),
        )
    ).expanduser()
    if not root.is_absolute():
        root = ROOT / root
    return root.resolve()


def write_cues(path: Path, language: str) -> None:
    lines: list[str] = []
    for cue_index, start in enumerate(range(8, DURATION_SECONDS - 10, 15), start=1):
        end = start + 4
        start_minutes, start_seconds = divmod(start, 60)
        end_minutes, end_seconds = divmod(end, 60)
        lines.extend([
            str(cue_index),
            f"00:{start_minutes:02}:{start_seconds:02},000 --> 00:{end_minutes:02}:{end_seconds:02},000",
            f"{language} timed acceptance cue {cue_index:02}",
            "",
        ])
    path.write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        raise SystemExit("ffmpeg must be installed locally to generate synthetic acceptance media.")
    output = configured_fixture_root() / "Movies" / "Puffinbox Subtitle Selection Fixture.mkv"
    if output.exists():
        raise SystemExit(f"Refusing to overwrite existing fixture: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    english = LOCAL / "subtitle-selection-en.srt"
    spanish = LOCAL / "subtitle-selection-es.srt"
    write_cues(english, "English")
    write_cues(spanish, "Spanish")

    # The video clock, two distinct audio tones, and numbered subtitle cues
    # make track selection and seeking observable without any copyrighted media.
    video_filter = (
        "drawtext=fontfile=/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf:"
        "text='%{pts\\:hms}':x=16:y=16:fontsize=28:fontcolor=white:"
        "box=1:boxcolor=black@0.65"
    )
    command = [
        ffmpeg, "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
        "-f", "lavfi", "-i", f"testsrc2=size=640x360:rate=24:duration={DURATION_SECONDS}",
        "-f", "lavfi", "-i", f"sine=frequency=440:sample_rate=48000:duration={DURATION_SECONDS}",
        "-f", "lavfi", "-i", f"sine=frequency=880:sample_rate=48000:duration={DURATION_SECONDS}",
        "-i", str(english), "-i", str(spanish),
        "-filter:v", video_filter, "-map", "0:v:0", "-map", "1:a:0", "-map", "2:a:0",
        "-map", "3:s:0", "-map", "4:s:0", "-t", str(DURATION_SECONDS),
        "-c:v", "mpeg4", "-q:v", "5", "-c:a", "libmp3lame", "-b:a", "96k", "-c:s", "srt",
        "-metadata", "title=Puffinbox Subtitle Selection Fixture",
        "-metadata", "comment=Original generated playback acceptance content",
        "-metadata:s:a:0", "language=eng", "-metadata:s:a:0", "title=440 Hz English track",
        "-metadata:s:a:1", "language=spa", "-metadata:s:a:1", "title=880 Hz Spanish track",
        "-metadata:s:s:0", "language=eng", "-metadata:s:s:0", "title=English numbered cues",
        "-metadata:s:s:1", "language=spa", "-metadata:s:s:1", "title=Spanish numbered cues",
        "-disposition:a:0", "default", "-disposition:a:1", "0",
        "-disposition:s:0", "default", "-disposition:s:1", "0",
        str(output),
    ]
    try:
        subprocess.run(
            command, check=True, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=900,
        )
    except subprocess.SubprocessError as error:
        diagnostic = getattr(error, "stderr", b"") or b""
        raise SystemExit(f"Could not generate fixture: {diagnostic.decode(errors='replace')[-2000:]}") from error
    print(f"Generated {output} ({DURATION_SECONDS}s, two audio tracks, two subtitle tracks).")
    print("Cues appear every 15 seconds; the video burns the current timestamp into each frame.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
