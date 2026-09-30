#!/usr/bin/env python3
"""Verify the local operator FFmpeg runtime without network or credentials."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
from pathlib import Path



ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], *, timeout: int = 45) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, check=False, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        detail = (result.stderr or result.stdout)[-2500:]
        raise RuntimeError(f"Local operator runtime check failed ({result.returncode}): {detail}")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="puffinbox:operator-ffmpeg")
    parser.add_argument("--uid", default="10001:10001", help="run operator tools as the service's non-root account")
    args = parser.parse_args()

    base = ["docker", "run", "--rm", "--network", "none", "--memory", "256m", "--cpus", "1", "--pids-limit", "64", "--read-only", "--tmpfs", "/tmp:rw,noexec,nosuid,size=64m,mode=1777", "--user", args.uid]
    ffmpeg = run(base + ["--entrypoint", "/operator-tools/ffmpeg", args.image, "-hide_banner", "-version"])
    ffprobe = run(base + ["--entrypoint", "/operator-tools/ffprobe", args.image, "-hide_banner", "-version"])
    print("FFmpeg: " + ffmpeg.stdout.splitlines()[0])
    print("FFprobe: " + ffprobe.stdout.splitlines()[0])
    protocol_check = run(base + ["--entrypoint", "/operator-tools/ffmpeg", args.image, "-hide_banner", "-protocols"])
    protocols = {line.strip() for line in protocol_check.stdout.splitlines() if line.strip()}
    if "fd" not in protocols:
        raise RuntimeError("The assembled operator FFmpeg runtime does not expose the fd protocol required by confined media readers")
    print("The external FFmpeg runtime exposes the fd protocol required by confined media readers.")

    local_dir = ROOT / ".local"
    local_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="puffinbox-runtime-probe-", dir=local_dir) as temporary:
        work = Path(temporary)
        work.chmod(0o777)
        output = work / "probe.ts"
        encode_command = base + ["--volume", f"{work}:/probe:rw", "--entrypoint", "/operator-tools/ffmpeg", args.image]
        run(encode_command + [
            "-hide_banner", "-loglevel", "error", "-nostdin",
            "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
            "-f", "lavfi", "-i", "sine=frequency=997:sample_rate=48000",
            "-t", "2", "-map", "0:v:0", "-map", "1:a:0", "-threads", "1",
            "-c:v", "libx264", "-profile:v", "baseline", "-level:v", "3.1", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-f", "mpegts", "/probe/probe.ts",
        ], timeout=60)
        if not output.is_file() or output.stat().st_size <= 188:
            raise RuntimeError("FFmpeg produced no usable synthetic transport stream")

        probe_command = base + ["--volume", f"{work}:/probe:ro", "--entrypoint", "/operator-tools/ffprobe", args.image]
        inspected = run(probe_command + ["-v", "error", "-show_entries", "stream=codec_type,codec_name,profile,level", "-of", "json", "/probe/probe.ts"])
        streams = json.loads(inspected.stdout).get("streams", [])
        codecs = {(stream.get("codec_type"), stream.get("codec_name")) for stream in streams}
        if ("video", "h264") not in codecs or ("audio", "aac") not in codecs:
            raise RuntimeError(f"FFprobe did not find the expected H.264/AAC output streams: {sorted(codecs)}")
        video = next(stream for stream in streams if stream.get("codec_type") == "video")
        if video.get("level") != 31:
            raise RuntimeError(f"FFmpeg emitted a video level outside the requested 3.1 probe: {video.get('level')!r}")

    print("Non-root, network-isolated H.264 baseline level 3.1/AAC encode and FFprobe inspection passed.")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, OSError, subprocess.SubprocessError, ValueError) as error:
        print(str(error), file=__import__("sys").stderr)
        raise SystemExit(1)
