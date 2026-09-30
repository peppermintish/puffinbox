#!/usr/bin/env python3
"""Deterministic checks for acceptance-fixture overwrite protection."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import prepare_acceptance as acceptance
from prepare_acceptance import (
    SHUTDOWN_FIXTURE_NAME,
    copy_file_exclusive,
    create_directory_if_absent,
    require_fresh_fixture_root,
    require_unoccupied_targets,
)


class AcceptanceFixtureSafetyTests(unittest.TestCase):
    def test_fresh_alternate_state_generates_long_incompatible_shutdown_fixture(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-acceptance-preparation-") as temporary:
            root = Path(temporary)
            state_root = root / "alternate-state"
            active_root = root / ".local" / "acceptance"

            def fake_ffmpeg(command: list[str], **_kwargs: object) -> None:
                Path(command[-1]).write_bytes(b"generated synthetic media")

            with (
                patch.dict("os.environ", {"PUFFINBOX_ACCEPTANCE_STATE_ROOT": str(state_root)}, clear=True),
                patch.object(acceptance, "ROOT", root),
                patch.object(acceptance, "LOCAL", active_root),
                patch.object(acceptance.shutil, "which", side_effect=["/mock/ffmpeg", "/mock/ffprobe"]),
                patch.object(acceptance, "available_loopback_ports", return_value=[18111, 55433]),
                patch.object(acceptance.subprocess, "run", side_effect=fake_ffmpeg) as run,
            ):
                self.assertEqual(acceptance.main(), 0)

            fixture = state_root / "media" / "Movies" / SHUTDOWN_FIXTURE_NAME
            self.assertEqual(fixture.read_bytes(), b"generated synthetic media")
            self.assertFalse(active_root.exists())
            command = next(
                call.args[0]
                for call in run.call_args_list
                if Path(call.args[0][-1]).name == SHUTDOWN_FIXTURE_NAME
            )
            self.assertIn("testsrc2=size=640x360:rate=24", command)
            self.assertEqual(command[command.index("-t") + 1], "300")
            self.assertEqual(command[command.index("-c:v") + 1], "mpeg4")
            self.assertEqual(command[command.index("-c:a") + 1], "libmp3lame")

    def test_separate_absent_state_and_fixture_roots_allow_state_file_creation(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-fixture-roots-") as temporary:
            root = Path(temporary)
            state_root = root / "state"
            fixture_root = root / "media"

            require_fresh_fixture_root(state_root)
            require_fresh_fixture_root(fixture_root)
            create_directory_if_absent(state_root)

            subtitle = state_root / "synthetic-subtitles.srt"
            subtitle.write_text("synthetic subtitle fixture\n", encoding="utf-8")
            self.assertEqual(subtitle.read_text(encoding="utf-8"), "synthetic subtitle fixture\n")
            self.assertFalse(fixture_root.exists())

    def test_fixture_root_with_existing_content_is_rejected_and_preserved(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-fixture-root-") as temporary:
            root = Path(temporary) / "media"
            root.mkdir()
            existing = root / "personal-video.mp4"
            original = b"pre-existing user media"
            existing.write_bytes(original)

            with self.assertRaises(SystemExit):
                require_fresh_fixture_root(root)

            self.assertEqual(existing.read_bytes(), original)

    def test_existing_fixture_target_is_rejected_without_changing_its_bytes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-fixture-safety-") as temporary:
            target = Path(temporary) / "Puffinbox Synthetic Direct Play Fixture.mp4"
            original = b"pre-existing user data"
            target.write_bytes(original)

            with self.assertRaises(SystemExit):
                require_unoccupied_targets([target])

            self.assertEqual(target.read_bytes(), original)

    def test_exclusive_fixture_copy_does_not_replace_existing_destination(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-fixture-copy-") as temporary:
            root = Path(temporary)
            source = root / "source.mkv"
            destination = root / "destination.mkv"
            source.write_bytes(b"synthetic source")
            destination.write_bytes(b"pre-existing user data")

            with self.assertRaises(FileExistsError):
                copy_file_exclusive(source, destination)

            self.assertEqual(destination.read_bytes(), b"pre-existing user data")


if __name__ == "__main__":
    unittest.main()
