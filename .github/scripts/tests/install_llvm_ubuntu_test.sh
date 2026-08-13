#!/usr/bin/env bash

set -euo pipefail

readonly llvm_key_url="https://apt.llvm.org/llvm-snapshot.gpg.key"
readonly llvm_repo_url="https://apt.llvm.org/trixie/"
readonly llvm_script_url="https://apt.llvm.org/llvm.sh"

fake_wget() {
    local destination=""
    local method="GET"
    local next_is_destination=false
    local url=""

    for argument in "$@"; do
        if [[ "$next_is_destination" == true ]]; then
            destination="$argument"
            next_is_destination=false
            continue
        fi

        case "$argument" in
            -O)
                next_is_destination=true
                ;;
            -O*)
                destination="${argument#-O}"
                ;;
            --method=HEAD)
                method="HEAD"
                ;;
            https://*)
                url="$argument"
                ;;
        esac
    done

    if [[ "$method" == "HEAD" && \
          ( "$url" == "$llvm_key_url" || "$url" == "$llvm_repo_url" ) ]]; then
        return 1
    fi

    case "$url" in
        "$llvm_script_url")
            cp /work/install_llvm_ubuntu_test.sh "$destination"
            ;;
        "$llvm_key_url")
            if [[ -n "$destination" && "$destination" != "-" ]]; then
                printf 'fake-llvm-signing-key\n' > "$destination"
            else
                printf 'fake-llvm-signing-key\n'
            fi
            ;;
    esac
}

check_url() {
    wget -q --method=HEAD --timeout=15 --tries=3 "$1"
}

fake_installer() {
    local version="$1"
    local key_path="/etc/apt/trusted.gpg.d/apt.llvm.org.asc"
    local bin
    local BASE_URL="https://apt.llvm.org"
    local CODENAME="trixie"

    if ! check_url "${BASE_URL}/${CODENAME}/"; then
        return 2
    fi

    if [[ ! -f "$key_path" ]]; then
        wget -q --method=HEAD --timeout=15 --tries=3 "$llvm_key_url"
        wget -qO- --tries=3 "$llvm_key_url" > "$key_path"
    fi

    for bin in clang llvm-config lld ld.lld FileCheck; do
        ln -fs /work/install_llvm_ubuntu_test.sh "/tmp/fake-bin/$bin-$version"
    done
}

dispatch_fake_command() {
    local command_name
    command_name="$(basename "$0")"

    case "$command_name" in
        apt-get|sleep)
            return 0
            ;;
        sha256sum)
            while IFS= read -r _; do :; done
            return 0
            ;;
        wget)
            fake_wget "$@"
            return
            ;;
        clang|clang-22|llvm-config|llvm-config-22|lld|lld-22|ld.lld|ld.lld-22|FileCheck|FileCheck-22)
            printf '22.0.0\n'
            return 0
            ;;
    esac

    if [[ "${1:-}" == "22" ]]; then
        fake_installer "$@"
        return
    fi

    return 127
}

run_in_container() {
    local script_dir
    local test_script

    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    test_script="$script_dir/install_llvm_ubuntu_test.sh"

    docker run --rm \
        --volume "$script_dir/../install_llvm_ubuntu.sh:/work/install_llvm_ubuntu.sh:ro" \
        --volume "$test_script:/work/install_llvm_ubuntu_test.sh:ro" \
        debian:trixie \
        bash /work/install_llvm_ubuntu_test.sh --inside
}

run_test() {
    local fake_command
    local output

    mkdir -p /tmp/fake-bin
    for fake_command in apt-get sha256sum sleep wget; do
        ln -fs /work/install_llvm_ubuntu_test.sh "/tmp/fake-bin/$fake_command"
    done

    if ! PATH="/tmp/fake-bin:$PATH" /work/install_llvm_ubuntu.sh; then
        echo "Expected the installer wrapper to avoid the failing upstream HEAD probes." >&2
        return 1
    fi

    output="$(/usr/bin/llvm-config --version)"
    if [[ "$output" != "22.0.0" ]]; then
        echo "Expected the installed llvm-config shim to report 22.0.0, got: $output" >&2
        return 1
    fi

    output="$(</etc/apt/trusted.gpg.d/apt.llvm.org.asc)"
    if [[ "$output" != "fake-llvm-signing-key" ]]; then
        echo "Expected the signing key to be installed by a bounded GET, got: $output" >&2
        return 1
    fi
}

case "$(basename "$0")" in
    apt-get|sha256sum|sleep|wget|clang|clang-22|llvm-config|llvm-config-22|lld|lld-22|ld.lld|ld.lld-22|FileCheck|FileCheck-22)
        dispatch_fake_command "$@"
        ;;
    *)
        if [[ "${1:-}" == "--inside" ]]; then
            run_test
        elif [[ "${1:-}" == "22" ]]; then
            fake_installer "$@"
        else
            run_in_container
        fi
        ;;
esac
