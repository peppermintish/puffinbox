#!/usr/bin/env python3
"""Regression checks for acceptance env-file and process-env precedence."""

from __future__ import annotations

import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from acceptance import ENV_FILE, effective_settings, read_env_file, validate_alternate_target


class EffectiveSettingsTests(unittest.TestCase):
    def test_process_environment_overrides_postgres_settings(self) -> None:
        settings = effective_settings(
            {
                "POSTGRES_DB": "puffinbox_acceptance",
                "POSTGRES_USER": "acceptance-user",
                "POSTGRES_PASSWORD": "file-password",
            },
            {
                "POSTGRES_DB": "puffinbox_acceptance_clean_20260928",
                "POSTGRES_PASSWORD": "environment-password",
                "UNRELATED_SETTING": "ignored",
            },
        )

        self.assertEqual(settings["POSTGRES_DB"], "puffinbox_acceptance_clean_20260928")
        self.assertEqual(settings["POSTGRES_USER"], "acceptance-user")
        self.assertEqual(settings["POSTGRES_PASSWORD"], "environment-password")
        self.assertNotIn("UNRELATED_SETTING", settings)

    def test_file_values_remain_the_fallback(self) -> None:
        settings = effective_settings({"POSTGRES_DB": "puffinbox_acceptance"}, {})
        self.assertEqual(settings["POSTGRES_DB"], "puffinbox_acceptance")


class AlternateAcceptanceTargetTests(unittest.TestCase):
    def alternate_values(self, root: Path) -> dict[str, str]:
        return {
            "COMPOSE_PROJECT_NAME": "puffinbox-acceptance-deadbeef",
            "POSTGRES_DB": "puffinbox_acceptance_deadbeef",
            "POSTGRES_USER": "puffinbox_acceptance_deadbeef",
            "PUFFINBOX_ACCEPTANCE_FIXTURE_ROOT": str(root / "media"),
            "PUFFINBOX_ACCEPTANCE_URL": "http://127.0.0.1:18996",
        }

    def test_disjoint_alternate_state_and_results_paths_are_accepted(self) -> None:
        with TemporaryDirectory(prefix="puffinbox-acceptance-isolation-") as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir()
            env_file = state / "acceptance.env"
            results_file = state / "acceptance-results.json"
            with patch.dict(os.environ, {"DATABASE_URL": ""}):
                validate_alternate_target(env_file, results_file, self.alternate_values(root))

    def test_active_database_identity_is_rejected(self) -> None:
        with TemporaryDirectory(prefix="puffinbox-acceptance-isolation-") as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir()
            active_values = read_env_file(ENV_FILE) if ENV_FILE.is_file() else {}
            selected = self.alternate_values(root)
            selected["POSTGRES_DB"] = active_values.get("POSTGRES_DB", "puffinbox_acceptance")
            with patch.dict(os.environ, {"DATABASE_URL": ""}), self.assertRaises(SystemExit):
                validate_alternate_target(state / "acceptance.env", state / "acceptance-results.json", selected)

    def test_existing_alternate_results_file_is_preserved_and_rejected(self) -> None:
        with TemporaryDirectory(prefix="puffinbox-acceptance-isolation-") as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir()
            results = state / "acceptance-results.json"
            original = b"prior evidence"
            results.write_bytes(original)
            with patch.dict(os.environ, {"DATABASE_URL": ""}), self.assertRaises(SystemExit):
                validate_alternate_target(state / "acceptance.env", results, self.alternate_values(root))
            self.assertEqual(results.read_bytes(), original)

    def test_alternate_database_url_must_match_generated_database_identity_and_port(self) -> None:
        with TemporaryDirectory(prefix="puffinbox-acceptance-isolation-") as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir()
            settings = self.alternate_values(root)
            settings["PUFFINBOX_ACCEPTANCE_POSTGRES_PORT"] = "127.0.0.1:5432"
            wrong_port_url = (
                f"postgres://{settings['POSTGRES_USER']}:unused@127.0.0.1:5433/"
                f"{settings['POSTGRES_DB']}"
            )
            with patch.dict(os.environ, {"DATABASE_URL": wrong_port_url}), self.assertRaises(SystemExit):
                validate_alternate_target(state / "acceptance.env", state / "acceptance-results.json", settings)


if __name__ == "__main__":
    unittest.main()
