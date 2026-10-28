"""Focused tests for the portable zk gas pricing-table generator."""

from __future__ import annotations

import importlib.util
import copy
import pathlib
import unittest


MODULE_PATH = pathlib.Path(__file__).with_name("generate.py")
MODULE_SPEC = importlib.util.spec_from_file_location("zk_gas_pricing_generate", MODULE_PATH)
assert MODULE_SPEC is not None and MODULE_SPEC.loader is not None
GENERATOR = importlib.util.module_from_spec(MODULE_SPEC)
MODULE_SPEC.loader.exec_module(GENERATOR)


class GeneratorTests(unittest.TestCase):
    """Exercise content addressing and stale-output detection independently of Rust."""

    def test_content_address_excludes_only_the_artifact_field(self) -> None:
        payload = {"z": 1, "artifact_sha256": "ignored", "a": {"b": 2}}
        self.assertEqual(
            GENERATOR.content_address(payload),
            "13bf451a28599eb77ebb813c6b754a9cb470a7dc848ccf246254095682b78894",
        )

    def test_validate_artifact_rejects_a_mismatched_content_address(self) -> None:
        with self.assertRaisesRegex(ValueError, "artifact_sha256"):
            GENERATOR.validate_artifact({"artifact_sha256": "0" * 64, "schema_version": 1})

    def test_generated_check_ignores_formatting_but_rejects_token_drift(self) -> None:
        expected = "pub const VALUE: i128 = 123;\n"
        self.assertTrue(GENERATOR.generated_matches(expected, "pub const VALUE:i128=123;"))
        self.assertTrue(
            GENERATOR.generated_matches(
                "const VALUES: [i128; 2] = [1,2];",
                "const VALUES: [i128; 2] = [\n    1,\n    2,\n];",
            )
        )
        self.assertFalse(GENERATOR.generated_matches(expected, "pub const VALUE:i128=124;"))

    def test_repository_runtime_spec_renders_the_committed_table(self) -> None:
        runtime_spec = MODULE_PATH.with_name("runtime-spec.json")
        generated = MODULE_PATH.with_name("generated.rs")
        payload = GENERATOR.json.loads(runtime_spec.read_text())

        self.assertEqual(GENERATOR.validate_artifact(payload), GENERATOR.RUNTIME_SPEC_SHA256)
        self.assertTrue(
            GENERATOR.generated_matches(GENERATOR.render(payload), generated.read_text())
        )

    def test_exp_floor_is_an_accepted_per_event_coefficient(self) -> None:
        payload = GENERATOR.json.loads(MODULE_PATH.with_name("runtime-spec.json").read_text())
        route = copy.deepcopy(next(row for row in payload["block_local"]["operation_routes"]
                                   if row["route_key"] == "opcode:0x0a"))
        route["accepted_absolute_model"]["parameters"]["per_event_raw_gas_floor"] = 24 * 10**18
        kind, coefficients, *_ = GENERATOR.accepted_model(route)
        self.assertEqual(kind, "ModelKind::Exp")
        self.assertEqual(coefficients[5], 24 * 10**18)


if __name__ == "__main__":
    unittest.main()
