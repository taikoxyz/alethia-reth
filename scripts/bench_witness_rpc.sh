#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/bench_witness_rpc.sh --rpc URL [options]

Benchmark raw `taiko_executionWitnessRange` against raw `debug_executionWitness`
for one contiguous historical window, and compare normalized witness payloads
block-by-block.

Options:
  --rpc URL               RPC endpoint to query (required)
  --start N               Absolute start block number
  --offset N              Start at head - N when --start is omitted (default: 10000)
  --window N              Number of blocks to benchmark (default: 192)
  --chunk-size N          Range chunk size for taiko_executionWitnessRange (default: 32)
  --parallel N            Concurrent range requests (default: 6)
  --timeout-seconds N     Per-request curl timeout (default: 600)
  --keep-tmp              Keep response and normalized JSON artifacts
  --help                  Show this message
EOF
}

require_command() {
    local command_name=$1
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        echo "missing required command: ${command_name}" >&2
        exit 1
    fi
}

now_ms() {
    date +%s%3N
}

to_hex_block() {
    printf '0x%x' "$1"
}

rpc_call() {
    local payload=$1
    curl \
        --silent \
        --show-error \
        --fail-with-body \
        --max-time "${TIMEOUT_SECONDS}" \
        --header 'content-type: application/json' \
        --data "${payload}" \
        "${RPC_URL}"
}

normalize_range_entry() {
    local response_file=$1
    local index=$2
    local output_file=$3

    jq -cS --argjson index "${index}" '
        def normalize:
            {
                state: ((.state // []) | sort),
                codes: ((.codes // []) | sort),
                keys: ((.keys // []) | sort),
                headers: (.headers // [])
            };

        .result[$index] | normalize
    ' "${response_file}" > "${output_file}"
}

normalize_debug_result() {
    local response_file=$1
    local output_file=$2

    jq -cS '
        def normalize:
            {
                state: ((.state // []) | sort),
                codes: ((.codes // []) | sort),
                keys: ((.keys // []) | sort),
                headers: (.headers // [])
            };

        .result | normalize
    ' "${response_file}" > "${output_file}"
}

hash_file() {
    local file_path=$1
    sha256sum "${file_path}" | awk '{ print $1 }'
}

print_duration_summary() {
    local label=$1
    local start_ms=$2
    local end_ms=$3
    local elapsed_ms=$((end_ms - start_ms))
    printf '%s total: %sms\n' "${label}" "${elapsed_ms}"
}

run_range_chunk() {
    local chunk_index=$1
    local chunk_start=$2
    local chunk_end=$3
    local response_file="${TMP_DIR}/range/raw/${chunk_index}.json"
    local summary_file="${TMP_DIR}/range/summary/${chunk_index}.txt"
    local expected_len=$((chunk_end - chunk_start + 1))
    local payload
    local started_ms
    local finished_ms
    local elapsed_ms
    local result_len

    payload=$(
        jq -nc \
            --argjson start_block "${chunk_start}" \
            --argjson end_block "${chunk_end}" \
            '{jsonrpc:"2.0", id:1, method:"taiko_executionWitnessRange", params:[$start_block, $end_block]}'
    )

    started_ms=$(now_ms)
    rpc_call "${payload}" > "${response_file}"
    finished_ms=$(now_ms)

    if jq -e '.error != null' "${response_file}" >/dev/null; then
        echo "range ${chunk_start}..${chunk_end} RPC error: $(jq -c '.error' "${response_file}")" >&2
        return 1
    fi

    if ! jq -e '.result | type == "array"' "${response_file}" >/dev/null; then
        echo "range ${chunk_start}..${chunk_end} missing array result" >&2
        return 1
    fi

    result_len=$(jq -r '.result | length' "${response_file}")
    if [[ "${result_len}" != "${expected_len}" ]]; then
        echo "range ${chunk_start}..${chunk_end} returned ${result_len} results, expected ${expected_len}" >&2
        return 1
    fi

    elapsed_ms=$((finished_ms - started_ms))
    printf '%s %s %s\n' "${chunk_start}" "${chunk_end}" "${elapsed_ms}" > "${summary_file}"
    printf 'range %s..%s %sms\n' "${chunk_start}" "${chunk_end}" "${elapsed_ms}"
}

RPC_URL=""
START_BLOCK=""
OFFSET=10000
WINDOW=192
CHUNK_SIZE=32
PARALLEL=6
TIMEOUT_SECONDS=600
KEEP_TMP=0

while (($# > 0)); do
    case "$1" in
        --rpc)
            RPC_URL=$2
            shift 2
            ;;
        --start)
            START_BLOCK=$2
            shift 2
            ;;
        --offset)
            OFFSET=$2
            shift 2
            ;;
        --window)
            WINDOW=$2
            shift 2
            ;;
        --chunk-size)
            CHUNK_SIZE=$2
            shift 2
            ;;
        --parallel)
            PARALLEL=$2
            shift 2
            ;;
        --timeout-seconds)
            TIMEOUT_SECONDS=$2
            shift 2
            ;;
        --keep-tmp)
            KEEP_TMP=1
            shift
            ;;
        --help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

require_command awk
require_command curl
require_command date
require_command jq
require_command mktemp
require_command sha256sum

if [[ -z "${RPC_URL}" ]]; then
    echo "--rpc is required" >&2
    usage >&2
    exit 1
fi

if ! [[ "${WINDOW}" =~ ^[0-9]+$ ]] || ((WINDOW <= 0)); then
    echo "--window must be a positive integer" >&2
    exit 1
fi

if ! [[ "${CHUNK_SIZE}" =~ ^[0-9]+$ ]] || ((CHUNK_SIZE <= 0)); then
    echo "--chunk-size must be a positive integer" >&2
    exit 1
fi

if ! [[ "${PARALLEL}" =~ ^[0-9]+$ ]] || ((PARALLEL <= 0)); then
    echo "--parallel must be a positive integer" >&2
    exit 1
fi

if ! [[ "${TIMEOUT_SECONDS}" =~ ^[0-9]+$ ]] || ((TIMEOUT_SECONDS <= 0)); then
    echo "--timeout-seconds must be a positive integer" >&2
    exit 1
fi

HEAD_HEX=$(
    rpc_call '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' |
        jq -r '.result'
)

if [[ "${HEAD_HEX}" == "null" || -z "${HEAD_HEX}" ]]; then
    echo "failed to read chain head from eth_blockNumber" >&2
    exit 1
fi

HEAD_BLOCK=$((HEAD_HEX))

if [[ -n "${START_BLOCK}" ]]; then
    if ! [[ "${START_BLOCK}" =~ ^[0-9]+$ ]]; then
        echo "--start must be a non-negative integer" >&2
        exit 1
    fi
else
    if ! [[ "${OFFSET}" =~ ^[0-9]+$ ]]; then
        echo "--offset must be a non-negative integer" >&2
        exit 1
    fi
    START_BLOCK=$((HEAD_BLOCK - OFFSET))
fi

if ((START_BLOCK < 0)); then
    echo "computed start block is negative: ${START_BLOCK}" >&2
    exit 1
fi

END_BLOCK=$((START_BLOCK + WINDOW - 1))
if ((END_BLOCK > HEAD_BLOCK)); then
    echo "end block ${END_BLOCK} exceeds current head ${HEAD_BLOCK}" >&2
    exit 1
fi

TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/bench_witness_rpc.XXXXXX")
mkdir -p \
    "${TMP_DIR}/range/raw" \
    "${TMP_DIR}/range/summary" \
    "${TMP_DIR}/range/normalized" \
    "${TMP_DIR}/debug/raw" \
    "${TMP_DIR}/debug/summary" \
    "${TMP_DIR}/debug/normalized"

cleanup() {
    local exit_code=$?
    if ((exit_code == 0)) && ((KEEP_TMP == 0)); then
        rm -rf "${TMP_DIR}"
    else
        echo "artifacts kept at ${TMP_DIR}" >&2
    fi
}
trap cleanup EXIT

printf 'resolved window: head=%s start=%s end=%s window=%s chunk=%s parallel=%s\n' \
    "${HEAD_BLOCK}" \
    "${START_BLOCK}" \
    "${END_BLOCK}" \
    "${WINDOW}" \
    "${CHUNK_SIZE}" \
    "${PARALLEL}"

declare -a CHUNK_STARTS=()
declare -a CHUNK_ENDS=()

chunk_start=${START_BLOCK}
while ((chunk_start <= END_BLOCK)); do
    chunk_end=$((chunk_start + CHUNK_SIZE - 1))
    if ((chunk_end > END_BLOCK)); then
        chunk_end=${END_BLOCK}
    fi

    CHUNK_STARTS+=("${chunk_start}")
    CHUNK_ENDS+=("${chunk_end}")
    chunk_start=$((chunk_end + 1))
done

range_started_ms=$(now_ms)
range_failures=0
active_jobs=0

for chunk_index in "${!CHUNK_STARTS[@]}"; do
    run_range_chunk "${chunk_index}" "${CHUNK_STARTS[chunk_index]}" "${CHUNK_ENDS[chunk_index]}" &
    active_jobs=$((active_jobs + 1))

    if ((active_jobs >= PARALLEL)); then
        if ! wait -n; then
            range_failures=$((range_failures + 1))
        fi
        active_jobs=$((active_jobs - 1))
    fi
done

while ((active_jobs > 0)); do
    if ! wait -n; then
        range_failures=$((range_failures + 1))
    fi
    active_jobs=$((active_jobs - 1))
done

range_finished_ms=$(now_ms)
if ((range_failures > 0)); then
    echo "range benchmark failed in ${range_failures} chunk(s)" >&2
    exit 1
fi

print_duration_summary "range" "${range_started_ms}" "${range_finished_ms}"

for chunk_index in "${!CHUNK_STARTS[@]}"; do
    response_file="${TMP_DIR}/range/raw/${chunk_index}.json"
    chunk_base=${CHUNK_STARTS[chunk_index]}
    chunk_last=${CHUNK_ENDS[chunk_index]}

    for ((block_number = chunk_base; block_number <= chunk_last; block_number++)); do
        result_index=$((block_number - chunk_base))
        normalize_range_entry \
            "${response_file}" \
            "${result_index}" \
            "${TMP_DIR}/range/normalized/${block_number}.json"
    done
done

debug_started_ms=$(now_ms)

for ((block_number = START_BLOCK; block_number <= END_BLOCK; block_number++)); do
    response_file="${TMP_DIR}/debug/raw/${block_number}.json"
    normalized_file="${TMP_DIR}/debug/normalized/${block_number}.json"
    summary_file="${TMP_DIR}/debug/summary/${block_number}.txt"
    block_hex=$(to_hex_block "${block_number}")
    payload=$(
        jq -nc \
            --arg block "${block_hex}" \
            '{jsonrpc:"2.0", id:1, method:"debug_executionWitness", params:[$block]}'
    )
    started_ms=$(now_ms)
    rpc_call "${payload}" > "${response_file}"
    finished_ms=$(now_ms)

    if jq -e '.error != null' "${response_file}" >/dev/null; then
        echo "debug ${block_number} RPC error: $(jq -c '.error' "${response_file}")" >&2
        exit 1
    fi

    if ! jq -e '.result != null' "${response_file}" >/dev/null; then
        echo "debug ${block_number} missing result" >&2
        exit 1
    fi

    normalize_debug_result "${response_file}" "${normalized_file}"

    elapsed_ms=$((finished_ms - started_ms))
    printf '%s %s\n' "${block_number}" "${elapsed_ms}" > "${summary_file}"
    printf 'debug %s %sms\n' "${block_number}" "${elapsed_ms}"
done

debug_finished_ms=$(now_ms)
print_duration_summary "debug" "${debug_started_ms}" "${debug_finished_ms}"

mismatch_count=0
for ((block_number = START_BLOCK; block_number <= END_BLOCK; block_number++)); do
    range_hash=$(hash_file "${TMP_DIR}/range/normalized/${block_number}.json")
    debug_hash=$(hash_file "${TMP_DIR}/debug/normalized/${block_number}.json")

    if [[ "${range_hash}" != "${debug_hash}" ]]; then
        mismatch_count=$((mismatch_count + 1))
        echo "mismatch ${block_number}: range=${range_hash} debug=${debug_hash}" >&2
    fi
done

range_total_ms=$((range_finished_ms - range_started_ms))
debug_total_ms=$((debug_finished_ms - debug_started_ms))
speedup=$(awk -v debug_ms="${debug_total_ms}" -v range_ms="${range_total_ms}" 'BEGIN { printf "%.2fx", debug_ms / range_ms }')

printf 'correctness mismatches: %s\n' "${mismatch_count}"
printf 'range total ms: %s\n' "${range_total_ms}"
printf 'debug total ms: %s\n' "${debug_total_ms}"
printf 'speedup: %s\n' "${speedup}"

if ((mismatch_count > 0)); then
    exit 1
fi
