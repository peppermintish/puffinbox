"""Safety checks for supplemental acceptance fixtures and alternate state roots."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import prepare_supplemental_fixtures as supplemental_fixtures


class SupplementalFixtureTests(unittest.TestCase):
    def make_state(self, state_root: Path, media_root: Path) -> Path:
        state_root.mkdir(parents=True, exist_ok=True)
        env_file = state_root / "acceptance.env"
        env_file.write_text(
            "PUFFINBOX_ACCEPTANCE_ISOLATED=1\n"
            f"PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT={media_root}\n",
            encoding="utf-8",
        )
        return env_file

    def configure_script(self, root: Path, active_root: Path) -> None:
        supplemental_fixtures.ROOT = root
        supplemental_fixtures.LOCAL = active_root
        supplemental_fixtures.DEFAULT_MEDIA = active_root / "media"

    def test_alternate_state_and_media_are_selected_without_changing_active_fixtures(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-supplemental-alternate-") as temporary:
            root = Path(temporary)
            active_root = root / ".local" / "acceptance"
            active_media = active_root / "media"
            alternate_state = root / "alternate-state"
            alternate_media = root / "alternate-media"
            active_root.mkdir(parents=True)
            active_env = active_root / "acceptance.env"
            active_env_contents = (
                "PUFFINBOX_ACCEPTANCE_ISOLATED=1\n"
                f"PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT={active_media}\n"
            )
            active_env.write_text(active_env_contents, encoding="utf-8")
            active_targets = [
                active_media / "Movies" / "Puffinbox Active HLS Shutdown Fixture.mkv",
                active_media / "Music" / "Puffinbox Original Acceptance Track.flac",
                active_media / "Books" / "Puffinbox Original Acceptance Book.epub",
            ]
            for target in active_targets:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b"existing active fixture")

            self.make_state(alternate_state, alternate_media)
            self.configure_script(root, active_root)

            def fake_ffmpeg(command: list[str], **_kwargs: object) -> None:
                Path(command[-1]).write_bytes(b"generated alternate fixture")

            with (
                patch.dict(os.environ, {"PUFFINBOX_ACCEPTANCE_STATE_ROOT": str(alternate_state)}, clear=True),
                patch.object(supplemental_fixtures.shutil, "which", return_value="/mock/ffmpeg"),
                patch.object(supplemental_fixtures.subprocess, "run", side_effect=fake_ffmpeg) as run,
            ):
                self.assertEqual(supplemental_fixtures.main(), 0)

            expected_targets = [
                alternate_media / "Movies" / "Puffinbox Active HLS Shutdown Fixture.mkv",
                alternate_media / "Music" / "Puffinbox Original Acceptance Track.flac",
                alternate_media / "Books" / "Puffinbox Original Acceptance Book.epub",
            ]
            self.assertEqual(run.call_count, 2)
            self.assertEqual([Path(call.args[0][-1]) for call in run.call_args_list], expected_targets[:2])
            self.assertTrue(all(target.is_file() for target in expected_targets))
            self.assertTrue(all(target.read_bytes() == b"existing active fixture" for target in active_targets))
            self.assertEqual(active_env.read_text(encoding="utf-8"), active_env_contents)

    def test_existing_supplemental_targets_are_preserved(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-supplemental-existing-") as temporary:
            root = Path(temporary)
            active_root = root / ".local" / "acceptance"
            alternate_state = root / "alternate-state"
            alternate_media = root / "alternate-media"
            self.make_state(alternate_state, alternate_media)
            targets = [
                alternate_media / "Movies" / "Puffinbox Active HLS Shutdown Fixture.mkv",
                alternate_media / "Music" / "Puffinbox Original Acceptance Track.flac",
                alternate_media / "Books" / "Puffinbox Original Acceptance Book.epub",
            ]
            for target in targets:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b"pre-existing target")
            self.configure_script(root, active_root)

            with (
                patch.dict(os.environ, {"PUFFINBOX_ACCEPTANCE_STATE_ROOT": str(alternate_state)}, clear=True),
                patch.object(supplemental_fixtures.shutil, "which", return_value="/mock/ffmpeg"),
                patch.object(supplemental_fixtures.subprocess, "run") as run,
            ):
                self.assertEqual(supplemental_fixtures.main(), 0)

            run.assert_not_called()
            self.assertTrue(all(target.read_bytes() == b"pre-existing target" for target in targets))

    def test_alternate_state_inside_active_tree_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-supplemental-overlap-") as temporary:
            root = Path(temporary)
            active_root = root / ".local" / "acceptance"
            self.configure_script(root, active_root)
            alternate_state = active_root / "alternate-state"

            with (
                patch.dict(os.environ, {"PUFFINBOX_ACCEPTANCE_STATE_ROOT": str(alternate_state)}, clear=True),
                self.assertRaisesRegex(SystemExit, "outside the active acceptance tree"),
            ):
                supplemental_fixtures.main()

    def test_alternate_media_overlapping_active_tree_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-supplemental-media-overlap-") as temporary:
            root = Path(temporary)
            active_root = root / ".local" / "acceptance"
            active_fixture = active_root / "media" / "keep.bin"
            active_fixture.parent.mkdir(parents=True)
            active_fixture.write_bytes(b"keep active data")
            alternate_state = root / "alternate-state"
            self.make_state(alternate_state, active_root / "media")
            self.configure_script(root, active_root)

            with (
                patch.dict(os.environ, {"PUFFINBOX_ACCEPTANCE_STATE_ROOT": str(alternate_state)}, clear=True),
                self.assertRaisesRegex(SystemExit, "Alternate acceptance fixtures must be outside"),
            ):
                supplemental_fixtures.main()

            self.assertEqual(active_fixture.read_bytes(), b"keep active data")

    def test_explicit_fixture_root_overlapping_active_tree_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="puffinbox-supplemental-override-overlap-") as temporary:
            root = Path(temporary)
            active_root = root / ".local" / "acceptance"
            alternate_state = root / "alternate-state"
            self.make_state(alternate_state, root / "safe-media")
            self.configure_script(root, active_root)

            with (
                patch.dict(
                    os.environ,
                    {
                        "PUFFINBOX_ACCEPTANCE_STATE_ROOT": str(alternate_state),
                        "PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT": str(active_root / "media"),
                    },
                    clear=True,
                ),
                self.assertRaisesRegex(SystemExit, "Alternate acceptance fixtures must be outside"),
            ):
                supplemental_fixtures.main()


if __name__ == "__main__":
    unittest.main()
