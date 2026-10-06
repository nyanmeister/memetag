#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --version ]]; then echo 'memetag-package 0.3.1'; exit 0; fi
if [[ ${1:-} == --help ]]; then
    echo 'Usage: tools/package.sh core|desktop|full OUTPUT_DIRECTORY [--no-build]'
    echo 'Stages a version-checked local Linux archive; never installs or publishes.'
    exit 0
fi
mode=${1:?choose core, desktop or full}
out=${2:?provide an output directory}
no_build=${3:-}
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
case "$mode" in
    core) packages=(-p memetag); programs=(memetag) ;;
    desktop) packages=(-p memetag -p memetag-gui); programs=(memetag memetag-gui) ;;
    full) packages=(--workspace); programs=(memetag memetag-gui memetag-infer) ;;
    *) echo 'Expected core, desktop or full' >&2; exit 1 ;;
esac
[[ -z "$no_build" || "$no_build" == --no-build ]] || { echo 'Unknown option' >&2; exit 1; }
if [[ -z "$no_build" ]]; then cargo build --locked --release "${packages[@]}"; fi
target=${CARGO_TARGET_DIR:-$repo/target}
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
name="memetag-$version-$mode-$(uname -m)-linux"
mkdir -p "$out"
out=$(cd "$out" && pwd)
stage=$(mktemp -d "${TMPDIR:-/tmp}/memetag-package.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
base="$stage/$name"
mkdir -p "$base/bin" "$base/share/doc/memetag" "$base/share/applications" "$base/share/icons/hicolor/scalable/apps" "$base/share/memetag/systemd"
for program in "${programs[@]}"; do
    report=$("$target/release/$program" --version)
    [[ "$report" == "$program $version "* ]] || { echo "Unexpected binary: $report" >&2; exit 1; }
    install -m755 "$target/release/$program" "$base/bin/$program"
    echo "$report" >> "$base/share/doc/memetag/VERSION"
    ldd "$base/bin/$program" >> "$base/share/doc/memetag/runtime-libraries.txt"
done
if rg -q 'not found' "$base/share/doc/memetag/runtime-libraries.txt"; then
    echo 'Unresolved runtime dependency; archive was not created' >&2; exit 1
fi
# The current ORT build is static. Refuse an incomplete archive if that changes.
if rg -q 'libonnxruntime.*=>' "$base/share/doc/memetag/runtime-libraries.txt"; then
    echo 'Dynamic ONNX runtime detected: packaging its library and licence needs an explicit update' >&2; exit 1
fi
install -m644 README.md docs/installation.md docs/releasing.md docs/network-sources.md docs/concurrency.md "$base/share/doc/memetag/"
if [[ -f LICENSE ]]; then install -m644 LICENSE "$base/share/doc/memetag/"; fi
host=$(rustc -vV | sed -n 's/^host: //p')
cargo metadata --locked --offline --format-version 1 --filter-platform "$host" > "$stage/metadata.json"
jq -r '.packages[] | [.name, .version, (.license // "UNDECLARED")] | @tsv' "$stage/metadata.json" | sort -u > "$base/share/doc/memetag/dependency-licenses.tsv"
while IFS=$'\t' read -r dependency dependency_version manifest; do
    source_dir=$(dirname "$manifest")
    while IFS= read -r notice; do
        relative=${notice#"$source_dir/"}
        install -Dm644 "$notice" "$base/share/doc/memetag/licenses/$dependency-$dependency_version/$relative"
    done < <(rg --files --hidden --no-ignore "$source_dir" -g '*LICENSE*' -g '*LICENCE*' -g '*COPYING*' -g '*NOTICE*' -g '*OFL*' -g '!target' || true)
    fallback="packaging/licenses/$dependency-$dependency_version"
    if [[ -d "$fallback" ]]; then
        mkdir -p "$base/share/doc/memetag/licenses/$dependency-$dependency_version"
        cp -R "$fallback"/. "$base/share/doc/memetag/licenses/$dependency-$dependency_version/"
    fi
    if [[ ! -d "$base/share/doc/memetag/licenses/$dependency-$dependency_version" ]]; then
        echo "Missing dependency notices: $dependency-$dependency_version" >&2
        exit 1
    fi
done < <(jq -r '.packages[] | select(.source != null) | [.name,.version,.manifest_path] | @tsv' "$stage/metadata.json")
# ONNX runtime is a downloaded static dependency, outside Cargo's source packages.
if [[ "$mode" == full ]]; then
    install -Dm644 packaging/licenses/onnxruntime-1.28.0/LICENSE "$base/share/doc/memetag/licenses/onnxruntime/LICENSE"
    install -Dm644 packaging/licenses/onnxruntime-1.28.0/ThirdPartyNotices.txt "$base/share/doc/memetag/licenses/onnxruntime/ThirdPartyNotices.txt"

fi
install -m644 config/config.toml.example "$base/share/memetag/"
install -m644 systemd/* "$base/share/memetag/systemd/"
if [[ "$mode" != core ]]; then
    install -m644 packaging/memetag.desktop "$base/share/applications/"
    install -m644 packaging/memetag.svg "$base/share/icons/hicolor/scalable/apps/"
fi
(cd "$base" && sha256sum bin/* > SHA256SUMS)
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
