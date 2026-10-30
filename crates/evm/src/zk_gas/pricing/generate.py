#!/usr/bin/env python3
"""Generate or verify the Rust zk gas pricing table from the canonical runtime JSON."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import pathlib
import re
import sys
from collections.abc import Mapping
from typing import Any


BASE = pathlib.Path(__file__).resolve().parent
# A separate acceptance record avoids a cycle between this script's provenance
# digest in the successor and the accepted successor's own content address.
RUNTIME_SPEC_SHA256 = (BASE / "accepted-runtime-spec.sha256").read_text().strip()


def canonical_json(payload: Mapping[str, Any]) -> bytes:
    """Return the canonical byte representation used by the runtime-spec exporter."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def content_address(payload: Mapping[str, Any]) -> str:
    """Hash a runtime specification while excluding its self-referential address."""
    addressed = dict(payload)
    addressed.pop("artifact_sha256", None)
    return hashlib.sha256(canonical_json(addressed)).hexdigest()


def validate_artifact(payload: Mapping[str, Any]) -> str:
    """Validate the embedded content address against the accepted runtime specification."""
    claimed = payload.get("artifact_sha256")
    actual = content_address(payload)
    if claimed != actual:
        raise ValueError(
            f"artifact_sha256 differs: embedded={claimed!r}, recomputed={actual}"
        )
    if claimed != RUNTIME_SPEC_SHA256:
        raise ValueError(
            "artifact_sha256 is not the accepted runtime specification: "
            f"{claimed!r}"
        )
    module_spec = importlib.util.spec_from_file_location(
        "zk_gas_pricing_successor", BASE / "authorization_successor.py"
    )
    assert module_spec is not None and module_spec.loader is not None
    successor = importlib.util.module_from_spec(module_spec)
    module_spec.loader.exec_module(successor)
    successor.validate_successor(
        payload,
        json.loads((BASE / successor.PARENT_PATH).read_text()),
        json.loads((BASE / successor.EVIDENCE_PATH).read_text()),
    )
    return actual


def route_expression(key: str) -> str:
    """Translate a canonical route key into its Rust typed-route expression."""
    if key.startswith("precompile:"):
        return f"OperationRoute::Precompile({int(key.split(':')[1], 16):#x})"
    opcode = int(key.split(":")[1], 16)
    if key.endswith(":confirmed_spawn_wrapper"):
        return f"OperationRoute::ConfirmedSpawn({opcode:#x})"
    return f"OperationRoute::Opcode({opcode:#x})"


def selector_expression(selector: str) -> str:
    """Translate an exported selector path into its Rust enum variant."""
    return {
        "operation.interpreter_raw_gas": "Selector::InterpreterRawGas",
        "operation.native_gas": "Selector::NativeGas",
        "constant_one_per_event": "Selector::Event",
        "operation.model_input.work_units": "Selector::WorkUnits",
    }[selector]


def selector_code(selector: str) -> int:
    """Encode an accepted-model selector stored in a generated coefficient slot."""
    return {
        "operation.interpreter_raw_gas": 0,
        "operation.native_gas": 1,
        "constant_one_per_event": 2,
        "operation.model_input.work_units": 3,
    }[selector]


def optional(value: int | None) -> str:
    """Render a generated Rust Option literal."""
    return "None" if value is None else f"Some({value})"


def memory_expression(model: Mapping[str, Any] | None) -> str:
    """Render shared-memory coefficients in typed field order."""
    values = model or {}
    return (
        "MemoryCoefficients { "
        f"growth_event: {values.get('memory_growth_event', 0)}, "
        f"evm_gas_delta: {values.get('memory_evm_gas_delta', 0)}, "
        f"boundary_4k_event: {values.get('memory_4k_boundary_event', 0)} "
        "}"
    )


def rule_expression(rule: Mapping[str, Any] | None) -> str:
    """Render an optional generated linear candidate."""
    if rule is None:
        return "None"
    return (
        "Some(LinearRule { "
        f"selector: {selector_expression(rule['selector'])}, "
        f"coefficient: {rule['coefficient']}, "
        f"constant: {rule.get('constant', 0)}, "
        f"minimum: {rule.get('minimum', 0)} "
        "})"
    )


def accepted_model(
    route: Mapping[str, Any],
) -> tuple[
    str,
    list[int],
    str,
    int | None,
    int | None,
    int | None,
    int | None,
    int | None,
    int | None,
    Mapping[str, Any] | None,
]:
    """Normalize one exported accepted model into the generated Rust representation."""
    model = route["accepted_absolute_model"]
    kind = model["kind"]
    coefficients = [0] * 12
    required = "RequiredInput::None"
    exact = model.get("raw_gas")
    maximum = model.get("maximum_raw_gas")
    fallback = None
    maximum_returned = None
    boundary_chunk = None
    return_raw = None
    memory = model.get("shared_memory")

    if kind == "fixed":
        model_kind = "ModelKind::Fixed"
        coefficients[0] = model["coefficient"]
    elif kind == "fixed_with_optional_shared_memory":
        model_kind = "ModelKind::FixedWithMemory"
        coefficients[0] = model["coefficient"]
        required = {
            "static_raw_gas": "RequiredInput::StaticRawGas",
            "log": "RequiredInput::Log",
            "spawn_memory": "RequiredInput::SpawnMemory",
        }[model["model_input_kind"]]
        if "out_of_envelope" in model:
            fallback = model["out_of_envelope"]["coefficient"]
    elif kind in {"sealed_absolute_linear_floor", "max_constant_and_linear"}:
        model_kind = "ModelKind::Linear"
        coefficients[0] = model["coefficient"]
        coefficients[1] = model.get("constant", 0)
        coefficients[2] = model.get("minimum", 0)
        coefficients[3] = selector_code(model["selector"])
    elif kind == "storage":
        model_kind = "ModelKind::Storage"
        parameters = model["parameters"]
        names = [
            "sload_warm_body",
            "sload_cold_extra",
            "sstore_branch:clear",
            "sstore_branch:dirty_rewrite",
            "sstore_branch:noop",
            "sstore_branch:reset",
            "sstore_branch:restore_original",
            "sstore_branch:set",
            "sstore_cold_extra",
        ]
        coefficients[: len(names)] = [parameters[name] for name in names]
    elif kind == "declared_context":
        model_kind = "ModelKind::Context"
        classes = model["classes"]
        names = [
            "address",
            "caller",
            "callvalue:nonzero",
            "callvalue:zero",
            "calldataload:full",
            "calldataload:partial",
            "calldataload:zero",
            "calldatasize",
            "timestamp",
        ]
        coefficients[: len(names)] = [classes[name] for name in names]
    elif kind == "opcode_model":
        parameters = model["parameters"]
        common = model["common_dispatch"]
        opcode_kind = model["model_kind"]
        mapping = {
            "static_raw_gas": ("ModelKind::StaticRawGas", ["body_per_raw_gas"]),
            "raw_gas_affine": (
                "ModelKind::RawGasAffine",
                ["constant", "body_per_raw_gas"],
            ),
            "exp": (
                "ModelKind::Exp",
                ["constant", "exponent_byte", "exponent_byte_sq", "small_bucket_body", "per_event_raw_gas_floor"],
            ),
            "keccak": (
                "ModelKind::Keccak",
                ["constant", "permutation", "zero_length_event"],
            ),
            "memory_access": ("ModelKind::MemoryAccess", ["constant"]),
            "memory_copy": ("ModelKind::MemoryCopy", ["constant", "copy_words"]),
            "copy": ("ModelKind::Copy", ["constant", "copy_words"]),
            "log_piecewise": (
                "ModelKind::LogPiecewise",
                ["constant", "topic", "nonzero_data", "additional_word"],
            ),
            "return": (
                "ModelKind::Return",
                ["constant", "nonzero_data", "additional_word"],
            ),
        }
        model_kind, names = mapping[opcode_kind]
        coefficients[0] = common
        for index, name in enumerate(names, 1):
            coefficients[index] = parameters[name]
        envelope = model.get("input_envelope")
        if isinstance(envelope, Mapping) and "raw_gas" in envelope:
            exact = envelope["raw_gas"]
        if opcode_kind == "return":
            extension = model["out_of_envelope"]
            maximum_returned = extension["maximum_returned_bytes"]
            boundary_chunk = extension["boundary_chunk_bytes"]
            return_raw = extension["raw_gas_coefficient"]
    else:
        raise ValueError(f"unsupported accepted model kind: {kind}")

    return (
        model_kind,
        coefficients,
        required,
        exact,
        maximum,
        fallback,
        maximum_returned,
        boundary_chunk,
        return_raw,
        memory,
    )


def render(specification: Mapping[str, Any]) -> str:
    """Render the complete deterministic Rust table from a validated specification."""
    validate_artifact(specification)
    lines = [
        "//! Generated constants compiled from the canonical `runtime-spec.json`.",
        "//! The embedded artifact content address authenticates the canonical payload.",
        "//! Regenerate with `generate.py --spec <spec.json> --output generated.rs`.",
        "//! Add `--check` to verify the committed table without rewriting it.",
        "",
        "use super::{",
        "    model::{Composition, LinearRule, MemoryCoefficients, ModelKind, RequiredInput, RouteSpec, Selector},",
        "    types::OperationRoute,",
        "};",
        "",
        "/// SHA-256 content address of the canonical runtime specification.",
        "pub const RUNTIME_SPEC_SHA256: &str =",
        f'    "{specification["artifact_sha256"]}";',
        "",
        "/// Common denominator for all generated fixed-point coefficients.",
        f"pub const FIXED_POINT_SCALE: i128 = {specification['arithmetic']['scale']};",
        "",
        "/// Maximum number of terms admitted in one pricing scope.",
        "pub(crate) const MAXIMUM_TERMS_PER_SCOPE: usize = "
        f"{specification['arithmetic']['maximum_terms_per_scope']};",
        "",
    ]
    block = specification["block_local"]
    fixed = block["fixed_costs"]
    lines.extend(
        [
            "/// Block base, started-transaction, and native-transfer coefficients.",
            "pub(crate) const BLOCK_FIXED: [i128; 3] = ["
            + ",".join(
                str(fixed[name])
                for name in [
                    "block_base",
                    "started_non_anchor_transaction",
                    "native_value_transfer",
                ]
            )
            + "];",
            "",
        ]
    )
    lines.extend([
        "/// Normalized fixed-point PGU per raw EIP-7702 authorization validation entry.",
        f"pub(crate) const AUTHORIZATION_VALIDATION_TARIFF: i128 = {block['authorization_validation_tariff']};",
        "",
    ])
    resources = block["resource_tariffs"]
    resource_names = [
        "unique_account_count",
        "unique_storage_slot_access_count",
        "unique_storage_slot_write_count",
        "unique_code_hash_count",
        "unique_code_bytes",
    ]
    lines.extend(
        [
            "/// Unique-resource coefficients in the public selector field order.",
            "pub(crate) const RESOURCE_TARIFFS: [i128; 5] = ["
            + ",".join(str(resources[name]) for name in resource_names)
            + "];",
            "",
        ]
    )
    mpt_names = [
        "path_node_visits",
        "materialization_work_units",
        "hydration_hash_blocks",
        "dirty_encoded_nodes",
        "dirty_hash_blocks",
        "structural_branch_extension_churn_nodes",
        "dropped_trie_nodes",
    ]
    proposal = specification["proposal_once"]
    for constant, table, documentation in [
        (
            "MPT_TARIFFS",
            block["mpt_tariffs"],
            "Block-local MPT coefficients in the public selector field order.",
        ),
        (
            "PARENT_MPT_TARIFFS",
            proposal["parent_mpt_tariffs"],
            "Proposal-parent MPT coefficients in the public selector field order.",
        ),
    ]:
        lines.extend(
            [
                f"/// {documentation}",
                f"pub(crate) const {constant}: [i128; 7] = ["
                + ",".join(str(table[name]) for name in mpt_names)
                + "];",
                "",
            ]
        )
    transaction = proposal["transaction_payload_tariffs"]
    lines.extend(
        [
            "/// Transaction byte, nonempty-payload, and true-unattempted coefficients.",
            "pub(crate) const TRANSACTION_PAYLOAD_TARIFFS: [i128; 3] = ["
            + ",".join(
                str(transaction[name])
                for name in [
                    "all_canonical_signed_eip2718_encoded_bytes",
                    "nonempty_payload_transaction_count",
                    "true_unattempted_transaction_count",
                ]
            )
            + "];",
            "",
            "/// Extra-blob and extra-source coefficients.",
            "pub(crate) const BLOB_SOURCE_TARIFFS: [i128; 2] = ["
            + ",".join(
                str(proposal["blob_source_tariffs"][name])
                for name in ["extra_blob_count", "extra_source_count"]
            )
            + "];",
            "",
            "/// Proposal startup coefficient charged exactly once.",
            f"pub(crate) const PROPOSAL_STARTUP: i128 = {proposal['startup']};",
            "",
            "/// Complete authenticated operation route table.",
            "pub(crate) static ROUTES: &[RouteSpec] = &[",
        ]
    )

    routes = block["operation_routes"]
    for route in routes:
        (
            model_kind,
            coefficients,
            required,
            exact,
            maximum,
            fallback,
            maximum_returned,
            boundary_chunk,
            return_raw,
            memory,
        ) = accepted_model(route)
        incremental = None
        if "incremental_selector" in route:
            incremental = {
                "selector": route["incremental_selector"],
                "coefficient": route["incremental_coefficient"],
            }
        lines.extend(
            [
                "    RouteSpec {",
                f"        route: {route_expression(route['route_key'])},",
                "        composition: "
                + (
                    "Composition::Accepted"
                    if route["composition"] == "accepted_model"
                    else "Composition::Maximum"
                )
                + ",",
                f"        model_kind: {model_kind},",
                "        coefficients: [" + ", ".join(map(str, coefficients)) + "],",
                f"        memory: {memory_expression(memory)},",
                f"        required_input: {required},",
                f"        exact_raw_gas: {optional(exact)},",
                f"        maximum_raw_gas: {optional(maximum)},",
                f"        fallback_raw_gas_coefficient: {optional(fallback)},",
                f"        maximum_returned_bytes: {optional(maximum_returned)},",
                f"        return_boundary_chunk_bytes: {optional(boundary_chunk)},",
                f"        return_raw_gas_coefficient: {optional(return_raw)},",
                f"        incremental: {rule_expression(incremental)},",
                f"        structural: {rule_expression(route.get('structural_floor'))},",
                "        feature_error: "
                f"{rule_expression(route.get('feature_error_absolute_model'))},",
                "    },",
            ]
        )
    lines.extend(["];"])

    opcode_indexes = [65535] * 256
    spawn_indexes = [65535] * 256
    precompile_indexes = []
    for index, route in enumerate(routes):
        key = route["route_key"]
        if key.startswith("precompile:"):
            precompile_indexes.append((int(key.split(":")[1], 16), index))
            continue
        opcode = int(key.split(":")[1], 16)
        if key.endswith(":confirmed_spawn_wrapper"):
            spawn_indexes[opcode] = index
        else:
            opcode_indexes[opcode] = index
    lines.extend(
        [
            "",
            "/// Direct lookup from opcode byte to route-table index; `u16::MAX` means absent.",
            "pub(crate) const OPCODE_ROUTE_INDEX: [u16; 256] = ["
            + ",".join(map(str, opcode_indexes))
            + "];",
            "",
            "/// Direct lookup from spawn opcode byte to confirmed-wrapper route index.",
            "pub(crate) const SPAWN_ROUTE_INDEX: [u16; 256] = ["
            + ",".join(map(str, spawn_indexes))
            + "];",
            "",
            "/// Numeric precompile address to route-table index, sorted by address.",
            "pub(crate) const PRECOMPILE_ROUTE_INDEX: &[(u16, u16)] = &["
            + ",".join(
                f"({address},{index})" for address, index in sorted(precompile_indexes)
            )
            + "];",
            "",
        ]
    )
    return "\n".join(lines)


def generated_matches(expected: str, actual: str) -> bool:
    """Compare generated Rust tokens while leaving presentation to rustfmt."""
    def normalize(source: str) -> str:
        compact = re.sub(r"\s+", "", source)
        return re.sub(r",(?=[}\])])", "", compact)

    return normalize(expected) == normalize(actual)


def parse_args(arguments: list[str] | None = None) -> argparse.Namespace:
    """Parse portable input and output paths with repository-local defaults."""
    parser = argparse.ArgumentParser(description=__doc__)
    base = pathlib.Path(__file__).resolve().parent
    parser.add_argument("--spec", default=base / "runtime-spec.json", type=pathlib.Path)
    parser.add_argument("--output", default=base / "generated.rs", type=pathlib.Path)
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify output matches without rewriting it",
    )
    return parser.parse_args(arguments)


def main(arguments: list[str] | None = None) -> int:
    """Generate a table or fail if the committed table is stale."""
    options = parse_args(arguments)
    specification = json.loads(options.spec.read_text())
    if not isinstance(specification, Mapping):
        raise ValueError("runtime specification must be a JSON object")
    generated = render(specification)
    if options.check:
        if not options.output.is_file():
            print(f"generated output is missing: {options.output}", file=sys.stderr)
            return 1
        if not generated_matches(generated, options.output.read_text()):
            print(
                "generated zk gas pricing table is stale; rerun without --check and format Rust",
                file=sys.stderr,
            )
            return 1
        print(
            f"verified {len(specification['block_local']['operation_routes'])} routes "
            f"from {specification['artifact_sha256']}"
        )
        return 0
    options.output.parent.mkdir(parents=True, exist_ok=True)
    options.output.write_text(generated)
    print(
        f"generated {len(specification['block_local']['operation_routes'])} routes "
        f"from {specification['artifact_sha256']} into {options.output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
