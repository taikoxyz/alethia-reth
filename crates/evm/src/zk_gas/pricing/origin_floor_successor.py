#!/usr/bin/env python3
"""Replay a finite ORIGIN normal-floor candidate without accepting it for the runtime."""
import argparse
import copy
from fractions import Fraction
import gzip
import hashlib
import importlib.util
from itertools import combinations
import json
from pathlib import Path
import re

BASE = Path(__file__).resolve().parent
PARENT_SHA256 = '80160649d7d29a85879cd9524b35c271a4b8638f64de94eef91d016e56121cf8'
PARENT_PATH = 'successors/parent-80160649.json'
PARENT_FILE_SHA256 = '8d2cb705d3eb62b9ec9cc68059692bd89100ddf3da3ed18bf2d8ce96644f43d8'
EVIDENCE_PATH = 'successors/origin-normal-floor-v1.json'
EVIDENCE_SHA256 = '12db50371714a2239b7c8397f2d5f6927d730281e6062cc302503718f29e1253'
EVIDENCE_FILE_SHA256 = '531707a0a3a1577165ca03a39b8d3159e1a40d19a56411db674d4345e9e41d37'
ELF_SHA256 = 'b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034'
OBSERVER_SHA256 = 'dfaee7ac41099df02e8a66d9b15b6c4b53d539d1a7e051a67b7180c64d6a0992'
NATIVE_SHA256 = 'b0497f453b5b6f0e0210ed90a68f2072d63f7cb3337c781466428a8b77ef525b'
SCALE = 10**18
METADATA_FIELDS = ('artifact_sha256', 'purpose', 'generator', 'parent_generator',
                   'validation', 'parent_validation', 'successor')
MAX_SOURCE_BYTES = 1024 * 1024


def canonical_json(value):
    """Encode typed JSON deterministically, including all provenance."""
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(value):
    """Authenticate canonical evidence independently of whitespace."""
    return hashlib.sha256(canonical_json(value)).hexdigest()


def content_address(value):
    """Exclude only the self-referential artifact address."""
    return digest({key: item for key, item in value.items() if key != 'artifact_sha256'})


def replay_measurements(rows):
    """Use every signed same-target count contrast, with no scaffold or startup credit."""
    if not isinstance(rows, list) or len(rows) != 3:
        raise ValueError('exactly three retained ORIGIN target points required')
    counts, inputs = set(), set()
    for row in rows:
        for field in ('raw_gas', 'actual_pgu', 'repetitions', 'input_bytes',
                      'native_origin_count', 'native_exceptional_instructions', 'native_user_gas'):
            if type(row[field]) is not int or not 0 <= row[field] < 2**64:
                raise ValueError('unsigned u64 measurement required')
        count = row['repetitions']
        if (count not in (0, 1, 8) or count in counts or row['lane'] != 'target' or
            row['sample_id'] != f'ordinary-placeholder-origin-n{count}-target' or
            row['native_origin_count'] != count or row['native_user_success'] is not True or
            row['native_exceptional_instructions'] != 0 or
            row['native_user_gas'] != 21049 + 63 * count or row['input_bytes'] != 333450):
            raise ValueError('wrong finite count, lane, ownership or successful execution')
        if row['actual_pgu'] != row['raw_gas'] * 10 // 191:
            raise ValueError('endpoint PGU differs from raw gas normalization')
        if (row['elf_sha256'] != ELF_SHA256 or row['sdk'] != '6.3.0' or
            row['observer_sha256'] != OBSERVER_SHA256 or
            row['native_observer_sha256'] != NATIVE_SHA256):
            raise ValueError('mixed program, SDK or observer identity')
        for field in ('input_sha256', 'actual_report_sha256', 'native_report_sha256',
                      'request_sha256', 'runtime_sha256', 'program_manifest_sha256'):
            if not isinstance(row[field], str) or re.fullmatch('[0-9a-f]{64}', row[field]) is None:
                raise ValueError('invalid retained artifact identity')
        if not isinstance(row['public_output'], str) or re.fullmatch('0x[0-9a-f]{64}', row['public_output']) is None:
            raise ValueError('invalid retained public output')
        if row['input_sha256'] in inputs:
            raise ValueError('duplicate input identity')
        counts.add(count)
        inputs.add(row['input_sha256'])
    if counts != {0, 1, 8}:
        raise ValueError('missing retained ORIGIN count')
    pairs, maximum = [], Fraction(0)
    for low, high in combinations(sorted(rows, key=lambda row: row['repetitions']), 2):
        events = high['native_origin_count'] - low['native_origin_count']
        increment = high['actual_pgu'] - low['actual_pgu']
        slope = Fraction(increment, events)
        maximum = max(maximum, slope)
        pairs.append({'low_sample_id': low['sample_id'], 'high_sample_id': high['sample_id'],
            'extra_events': events, 'actual_increment_pgu': increment,
            'pgu_per_event': {'numerator': slope.numerator, 'denominator': slope.denominator}})
    required = maximum * Fraction(5, 4)
    constant = -(-required.numerator // (64 * required.denominator)) * 64
    return {'constant_pgu': constant, 'pairs': pairs,
            'max_positive_slope': {'numerator': maximum.numerator, 'denominator': maximum.denominator},
            'unrounded_required_pgu': {'numerator': required.numerator, 'denominator': required.denominator}}


def load_inputs():
    """Read byte-pinned parent and evidence, never the moving accepted artifact."""
    values = []
    for path, expected in ((PARENT_PATH, PARENT_FILE_SHA256), (EVIDENCE_PATH, EVIDENCE_FILE_SHA256)):
        raw = (BASE / path).read_bytes()
        if hashlib.sha256(raw).hexdigest() != expected:
            raise ValueError('immutable input bytes changed: ' + path)
        values.append(json.loads(raw))
    return tuple(values)


def derive(parent, evidence):
    """Reject rewritten facts and stale cached derivations before transforming a price."""
    if parent.get('artifact_sha256') != PARENT_SHA256 or content_address(parent) != PARENT_SHA256:
        raise ValueError('wrong immutable ORIGIN parent')
    if digest(evidence) != EVIDENCE_SHA256 or evidence['parent_artifact_sha256'] != PARENT_SHA256:
        raise ValueError('wrong retained ORIGIN evidence')
    result = replay_measurements(evidence['measurements'])
    if (canonical_json(result['pairs']) != canonical_json(evidence['matched_pairs']) or
        result['constant_pgu'] != evidence['candidate_constant_pgu']):
        raise ValueError('cached ORIGIN pairs or candidate differ from raw facts')
    return result


def transform(parent, evidence):
    """Change only the successful ORIGIN fixed coefficient and explicit candidate provenance."""
    derived = derive(parent, evidence)
    result = copy.deepcopy(parent)
    route = next(r for r in result['block_local']['operation_routes'] if r['route_key'] == 'opcode:0x32')
    route['accepted_absolute_model']['coefficient'] = derived['constant_pgu'] * SCALE
    result['purpose'] = 'block_local_empirical_origin_normal_floor_offline_candidate'
    result['parent_generator'], result['parent_validation'] = parent['generator'], parent['validation']
    result['generator'] = {'path': 'crates/evm/src/zk_gas/pricing/origin_floor_successor.py',
        'file_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'table_generator_path': 'crates/evm/src/zk_gas/pricing/generate.py',
        'table_generator_sha256': hashlib.sha256((BASE / 'generate.py').read_bytes()).hexdigest()}
    result['validation'] = {'retained_measurement_rows': 3, 'matched_increment_pairs': 3,
        'new_sp1_executions': 0, 'unopened_holdouts_used': 0,
        'fresh_repricing_performed': False, 'historical_repricing_performed': False,
        'universal_upper_bound_claimed': False, 'production_sdk_parity_established': False}
    result['successor'] = {'parent_artifact_sha256': PARENT_SHA256, 'parent_snapshot': PARENT_PATH,
        'parent_file_sha256': PARENT_FILE_SHA256, 'evidence_path': EVIDENCE_PATH,
        'evidence_sha256': EVIDENCE_SHA256, 'evidence_file_sha256': EVIDENCE_FILE_SHA256,
        'policy': copy.deepcopy(evidence['policy']), 'constant_pgu': derived['constant_pgu'],
        'max_positive_slope': derived['max_positive_slope'],
        'unrounded_required_pgu': derived['unrounded_required_pgu'],
        'source_bindings': copy.deepcopy(evidence['source_bindings']),
        'scope': 'ORIGIN normal fixed coefficient only; feature-error and all inherited numeric pricing, selectors, ownership, resources and Unzen unchanged',
        'empirical_limits': copy.deepcopy(evidence['limitations']),
        'publication_status_at_model_generation': 'offline_candidate_pending_reserved_and_requotes'}
    result['artifact_sha256'] = content_address(result)
    return result


def validate_successor(payload, parent, evidence):
    """Reject readdressed changes to any price, type, provenance or preserved route."""
    if canonical_json(payload) != canonical_json(transform(parent, evidence)):
        raise ValueError('ORIGIN candidate differs from deterministic raw replay')


def load_table_generator():
    """Load an isolated copy of the authoritative table renderer, leaving accepted801 intact."""
    spec = importlib.util.spec_from_file_location('isolated_origin_table_generator', BASE / 'generate.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def render_candidate(candidate, parent, evidence):
    """Adapt only validation in a private module; reuse the unchanged canonical renderer.

    The accepted801 exporter binds its own source hash into provenance. Editing that file
    merely to add a candidate switch would invalidate its replay. This fixed adapter has no
    generic validation bypass: only the exact ORIGIN transform can reach the same renderer.
    """
    validate_successor(candidate, parent, evidence)
    generator = load_table_generator()
    def validate_origin(payload):
        """Require the single byte-pinned candidate, including its recomputed content address."""
        validate_successor(payload, parent, evidence)
        return payload['artifact_sha256']
    generator.validate_artifact = validate_origin
    return generator.render(candidate)


def export_candidate(candidate, parent, evidence, output):
    """Write a separate fresh directory, never the currently accepted runtime assets."""
    output = Path(output)
    if output.resolve() == BASE or (output.exists() and any(output.iterdir())):
        raise ValueError('candidate requires a new, empty output directory')
    generated = render_candidate(candidate, parent, evidence)
    output.mkdir(parents=True, exist_ok=True)
    (output / 'runtime-spec.json').write_bytes(canonical_json(candidate) + b'\n')
    (output / 'candidate-runtime-spec.sha256').write_text(candidate['artifact_sha256'] + '\n')
    (output / 'generated.rs').write_text(generated)


def check_candidate(candidate, parent, evidence, output):
    """Check only the explicit candidate directory without granting runtime acceptance."""
    output = Path(output)
    if (canonical_json(json.loads((output / 'runtime-spec.json').read_bytes())) != canonical_json(candidate) or
        (output / 'candidate-runtime-spec.sha256').read_text().strip() != candidate['artifact_sha256'] or
        not load_table_generator().generated_matches(render_candidate(candidate, parent, evidence),
                                                     (output / 'generated.rs').read_text())):
        raise ValueError('candidate directory differs from sealed replay')


def read_bound(root, relative, expected):
    """Read one bounded artifact under a supplied source root and verify exact bytes."""
    root, relative = Path(root).resolve(), Path(relative)
    path = (root / relative).resolve()
    if relative.is_absolute() or root not in path.parents or path.stat().st_size > MAX_SOURCE_BYTES:
        raise ValueError('source escapes root or exceeds the one-MiB artifact bound')
    with path.open('rb') as source:
        raw = source.read(MAX_SOURCE_BYTES + 1)
    if len(raw) > MAX_SOURCE_BYTES or hashlib.sha256(raw).hexdigest() != expected:
        raise ValueError('source bytes differ: ' + str(relative))
    return raw


def verify_sources(evidence, actual_root, prepared_root):
    """Reconcile raw reports, native counts, program identity and canonical input with retained facts."""
    replay_measurements(evidence['measurements'])
    for row in evidence['measurements']:
        folder = Path(row['source_relative_path'])
        actual = json.loads(read_bound(actual_root, folder / 'sp1.json', row['actual_report_sha256']))
        native = json.loads(read_bound(actual_root, folder / 'native.json', row['native_report_sha256']))
        manifest = json.loads(read_bound(actual_root, folder / 'program-manifest.json', row['program_manifest_sha256']))
        request = json.loads(read_bound(prepared_root, row['request_relative_path'], row['request_sha256']))
        compressed = Path(actual_root) / folder / 'input.bin.gz'
        # Reports and input use the same pre-validated relative directory. Limit decoded bytes too.
        if Path(actual_root).resolve() not in compressed.resolve().parents or compressed.stat().st_size > MAX_SOURCE_BYTES:
            raise ValueError('input escapes source root or exceeds compressed bound')
        with gzip.open(compressed, 'rb') as source:
            canonical = source.read(MAX_SOURCE_BYTES + 1)
        code = bytes.fromhex(request['program']['code_hex'][2:])
        counts = {f'opcode:0x{op:02x}': amount for op, amount in
                  ((0x00, 1), (0x01, row['repetitions']), (0x14, 2+2*row['repetitions']),
                   (0x15, 1+row['repetitions']), (0x32, row['repetitions']), (0x33, row['repetitions']),
                   (0x56, row['repetitions']), (0x57, 2+2*row['repetitions']),
                   (0x5b, 2+row['repetitions']), (0x61, 2+3*row['repetitions']),
                   (0x62, 3+2*row['repetitions']), (0x80, 1+row['repetitions'])) if amount}
        if (len(canonical) != row['input_bytes'] or hashlib.sha256(canonical).hexdigest() != row['input_sha256'] or
            actual['guest_input_sha256'] != row['input_sha256'] or native['guest_input_sha256'] != row['input_sha256'] or
            actual['guest_input_bincode_length'] != len(canonical) or native['guest_input_bincode_length'] != len(canonical) or
            actual['public_values'] != row['public_output'] or native['public_output'] != row['public_output'] or
            '0x' + bytes(native['public_values']).hex() != row['public_output'] or
            actual['execution_report']['gas'] != row['raw_gas'] or actual['prover_gas'] != row['actual_pgu'] or
            actual['exit_code'] != 0 or actual['execution_report']['exit_code'] != 0 or
            actual['elf_sha256'] != ELF_SHA256 or manifest['elf_sha256'] != ELF_SHA256 or
            actual['executable_sha256'] != OBSERVER_SHA256 or actual['execution_profile'] != 'evm-gas-only' or
            any(actual[key] != '6.3.0' for key in ('sp1_sdk_version', 'sp1_core_executor_version', 'sp1_core_executor_runner_version')) or
            actual['program_manifest_sha256'] != row['program_manifest_sha256'] or
            native['construction_request_sha256'] != row['request_sha256'] or
            native['observed_route_counts'] != counts or native['attempted_route_counts'] != counts or
            native['terminal_counts'] != {'stop': 1} or native['block_count'] != 1 or native['transaction_count'] != 2 or
            native['observations_by_block'][0]['exceptional_instructions'] != 0 or
            native['user_transaction_gas_used_total'] != row['native_user_gas'] or
            native['receipts_by_block'][0][1]['success'] is not True or
            len(code) != 1024 or hashlib.sha256(code).hexdigest() != row['runtime_sha256'] or
            request['program']['input_hex'] != '0x'):
            raise ValueError('source report or normative count differs from retained facts: ' + row['sample_id'])


def main():
    """Export/check a separate offline candidate; source verification is an optional stricter audit."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--actual-root', type=Path)
    parser.add_argument('--prepared-root', type=Path)
    args = parser.parse_args()
    if (args.actual_root is None) != (args.prepared_root is None):
        parser.error('--actual-root and --prepared-root must be supplied together')
    parent, evidence = load_inputs()
    candidate = transform(parent, evidence)
    if args.actual_root is not None:
        verify_sources(evidence, args.actual_root, args.prepared_root)
    if args.check:
        check_candidate(candidate, parent, evidence, args.output_dir)
    else:
        export_candidate(candidate, parent, evidence, args.output_dir)
    print(candidate['artifact_sha256'])


if __name__ == '__main__':
    main()
