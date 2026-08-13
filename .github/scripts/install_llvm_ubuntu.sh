#!/usr/bin/env bash
#
# CI/Docker provisioning ONLY. This installs apt.llvm.org packages system-wide and
# force-overwrites the /usr/bin/{clang,llvm-config,lld,ld.lld,FileCheck} symlinks, so it must
# not be run on a developer machine: install your distribution's llvm-22 package instead
# (Homebrew `llvm@22` on macOS) and put its bin directory on PATH.
set -eo pipefail

version=${1:-22}
bins=(clang llvm-config lld ld.lld FileCheck)

# apt does not retry failed downloads by default, so a single blip reaching deb.debian.org or
# apt.llvm.org aborts the image build. This also covers the apt calls inside the upstream
# installer invoked below, which this script cannot pass options to.
printf 'Acquire::Retries "3";\n' > /etc/apt/apt.conf.d/80-alethia-retries

# The official installer needs this package on Debian bookworm but not on newer distributions.
apt-get update -qq
apt-get install -y --no-install-recommends \
    lsb-release wget gnupg ca-certificates
apt-get install -y --no-install-recommends software-properties-common 2>/dev/null || true

llvm_installer=$(mktemp)
# Bound and retry this fetch ourselves. wget's default --timeout is 900s, so one stalled
# connection to apt.llvm.org used to wedge the build for a silent 15 minutes and then abort with
# a bare "exit code: 4". --timeout caps DNS, connect and read alike; the outer loop retries
# regardless of which of them wget reports, and -nv restores the error text that -q swallowed.
attempts=4
for attempt in $(seq "$attempts"); do
    if wget -nv --timeout=20 --tries=1 -O "$llvm_installer" https://apt.llvm.org/llvm.sh; then
        break
    fi
    if [[ "$attempt" -eq "$attempts" ]]; then
        echo "Error: could not download https://apt.llvm.org/llvm.sh ($attempts attempts)" >&2
        exit 1
    fi
    echo "Retrying https://apt.llvm.org/llvm.sh in $((attempt * 5))s" >&2
    sleep $((attempt * 5))
done
# Pin the upstream installer so CI and Docker builds fail loudly when it changes; review the new
# script before updating this hash.
echo "03878e08f47b66cc95bc4b544b0db3c6d9ce8d60e6cf2492ae357984330a9eae  $llvm_installer" | sha256sum -c -
chmod +x "$llvm_installer"
# Fetch the signing key with the same bounded GET used for the installer. The upstream script
# otherwise probes this URL with `wget --method=HEAD` before downloading it; that probe repeatedly
# timed out on the arm64 runner even while GETs from apt.llvm.org succeeded. Install the key only
# after a complete download so a failed attempt cannot make upstream skip the key setup.
llvm_key_path=/etc/apt/trusted.gpg.d/apt.llvm.org.asc
if [[ ! -f "$llvm_key_path" ]]; then
    llvm_key_download=$(mktemp)
    for key_attempt in $(seq "$attempts"); do
        if wget -nv --timeout=20 --tries=1 -O "$llvm_key_download" \
            https://apt.llvm.org/llvm-snapshot.gpg.key; then
            break
        fi
        if [[ "$key_attempt" -eq "$attempts" ]]; then
            echo "Error: could not download the apt.llvm.org signing key ($attempts attempts)" >&2
            exit 1
        fi
        echo "Retrying the apt.llvm.org signing key in $((key_attempt * 5))s" >&2
        sleep $((key_attempt * 5))
    done
    install -m 0644 "$llvm_key_download" "$llvm_key_path"
    rm -f "$llvm_key_download"
fi
# Retry the installer too. Before touching apt it preflights apt.llvm.org with a single
# `wget --method=HEAD` (its check_url), and reports *any* failure of that probe -- including a
# connection blip -- as a fatal "Distribution 'debian' in version '13 (trixie)' is not supported
# by this script.", exit 2. Neither the download loop above nor Acquire::Retries covers that raw
# wget, so one unlucky HEAD request aborted an arm64 image build even though the repository was
# up. The probe runs before any state is written, so re-running the installer is the fix.
install_attempts=3
for install_attempt in $(seq "$install_attempts"); do
    if "$llvm_installer" "$version" all; then
        break
    fi
    if [[ "$install_attempt" -eq "$install_attempts" ]]; then
        echo "Error: the apt.llvm.org installer failed $install_attempts times." >&2
        echo "Note: an 'is not supported by this script' error above can also mean apt.llvm.org" \
             "was unreachable rather than that the distribution is genuinely unsupported." >&2
        exit 1
    fi
    # On distributions that get the deb822 layout the installer appends its stanza with `tee -a`,
    # so an attempt that failed after writing it would leave apt with a duplicate on the next pass.
    # Dropping the file is safe here because the retry immediately below rewrites it.
    rm -f /etc/apt/sources.list.d/*apt_llvm_org*.sources
    echo "Retrying the apt.llvm.org installer in $((install_attempt * 10))s" >&2
    sleep $((install_attempt * 10))
done
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
