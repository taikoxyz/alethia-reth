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
# Pin the upstream installer so CI and Docker builds fail loudly when it changes; review the new
# script before updating this hash.
echo "9474ecd78b52aba6e923976b1e9773f5613027cc7e237b9956986cb536e02a36  $llvm_installer" | sha256sum -c -
chmod +x "$llvm_installer"
"$llvm_installer" "$version" all
rm -f "$llvm_installer"

for bin in "${bins[@]}"; do
    if ! command -v "$bin-$version" &>/dev/null; then
        echo "Error: $bin-$version not found after install" >&2
        exit 1
    fi
    ln -fs "$(command -v "$bin-$version")" "/usr/bin/$bin"
done

echo "LLVM $version installed:"
llvm-config --version
