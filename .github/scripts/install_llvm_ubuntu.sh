#!/usr/bin/env bash
set -eo pipefail

version=${1:-22}
bins=(clang llvm-config lld ld.lld FileCheck)

# The official installer needs this package on Debian bookworm but not on newer distributions.
apt-get update -qq
apt-get install -y --no-install-recommends \
    lsb-release wget gnupg ca-certificates
apt-get install -y --no-install-recommends software-properties-common 2>/dev/null || true

llvm_installer=$(mktemp)
wget -qO "$llvm_installer" https://apt.llvm.org/llvm.sh
chmod +x "$llvm_installer"
"$llvm_installer" "$version" all
rm -f "$llvm_installer"

for bin in "${bins[@]}"; do
    if ! command -v "$bin-$version" &>/dev/null; then
        echo "Warning: $bin-$version not found" >&2
        continue
    fi
    ln -fs "$(command -v "$bin-$version")" "/usr/bin/$bin"
done

echo "LLVM $version installed:"
llvm-config --version
