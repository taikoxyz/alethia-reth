#!/usr/bin/env bash
set -euo pipefail

awk '
function check_package(expected) {
    if (name !~ /^alloy-/ || version == "") {
        return
    }

    alloy_count++
    expected = ""

    if (name == "alloy-evm") {
        expected = "0.37.0"
        seen_evm = 1
    } else if (name == "alloy-rlp") {
        expected = "0.3.16"
        seen_rlp = 1
    } else if (name == "alloy-rlp-derive") {
        expected = "0.3.16"
        seen_rlp_derive = 1
    } else if (version ~ /^2\./) {
        expected = "2.1.1"
    } else if (version ~ /^1\./) {
        expected = "1.6.1"
    }

    if (expected != "") {
        checked_count++
        if (version != expected) {
            failure_count++
            bad_name[failure_count] = name
            bad_version[failure_count] = version
            bad_expected[failure_count] = expected
        }
    }
}

/^\[\[package\]\]$/ {
    check_package()
    name = ""
    version = ""
    in_package = 1
    next
}

in_package && /^name = "/ {
    name = $0
    sub(/^name = "/, "", name)
    sub(/"$/, "", name)
    next
}

in_package && /^version = "/ {
    version = $0
    sub(/^version = "/, "", version)
    sub(/"$/, "", version)
    next
}

END {
    check_package()

    if (!seen_evm) {
        failure_count++
        bad_name[failure_count] = "alloy-evm"
        bad_version[failure_count] = "<missing>"
        bad_expected[failure_count] = "0.37.0"
    }
    if (!seen_rlp) {
        failure_count++
        bad_name[failure_count] = "alloy-rlp"
        bad_version[failure_count] = "<missing>"
        bad_expected[failure_count] = "0.3.16"
    }
    if (!seen_rlp_derive) {
        failure_count++
        bad_name[failure_count] = "alloy-rlp-derive"
        bad_version[failure_count] = "<missing>"
        bad_expected[failure_count] = "0.3.16"
    }

    if (failure_count != 0) {
        print "Error: unexpected Alloy package versions in Cargo.lock:" > "/dev/stderr"
        for (i = 1; i <= failure_count; i++) {
            printf "  %s %s (expected %s)\n", \
                bad_name[i], bad_version[i], bad_expected[i] > "/dev/stderr"
        }
        exit 1
    }

    printf "approved Alloy versions in Cargo.lock: %d checked across %d packages\n", \
        checked_count, alloy_count
}
' Cargo.lock
